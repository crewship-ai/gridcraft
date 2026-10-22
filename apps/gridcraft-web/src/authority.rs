//! Host-authority bridge (`?host=authority&parent_origin=<origin>`).
//!
//! Unlike [`crate::host`] (same-origin parent, whole files, "saved" as soon as the bytes are
//! handed over), here the embedding page is the authority over the document and may live on a
//! different origin:
//!
//! - Handshake: the editor posts `gridcraft:hello` to `parent_origin` only; the parent answers
//!   with `gridcraft:connect` and transfers a `MessagePort`. The connect is accepted only when
//!   `event.origin == parent_origin` and `event.source` is the parent window. All further traffic
//!   is JSON strings over that port (no wildcard target origins anywhere).
//! - Editor → host: `mutation {mutation_id, base_rev, ops:[{op:"cell.set", sheet, cell, input}]}`
//!   for every local change, detected by diffing cell inputs against the last state accounted
//!   for; `resume {last_rev, pending:[mutation…]}` after the host reports it is back online.
//! - Host → editor: `snapshot {rev, cells}`, `committed {rev, mutation_id, client_id, ops}` (in
//!   rev order), `ack {mutation_id, rev}`, `rejected {mutation_id, code, current_rev, cells}`,
//!   `offline`, `online`.
//! - "Saved" only when every local mutation has a host ack (the host acks after it made the
//!   commit durable). A rejection reverts the optimistic local change to the host's value and
//!   keeps the lost input visible as "Not saved".
//! - Remote commits are applied through the engine without touching the cell editor, the
//!   selection or the undo/redo stacks (`Session::apply_remote`): undo/redo replay remote ops
//!   onto restored snapshots, so a local Undo can't revert someone else's commit.
//!
//! Limits: sheet 0 only, cell inputs only (no styles, structure, sheets), history rebasing
//! by replaying the op on every snapshot (O(history) recalcs per remote op).

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;
use std::sync::Arc;

use gridcraft_engine::model::Workbook;
use gridcraft_engine::remote::RemoteOp;
use gridcraft_engine::{DocState, Session};
use gridcraft_ui_egui::SheetApp;
use serde_json::{Value as Json, json};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::JsValue;
use wasm_bindgen::closure::Closure;

const SHEET: usize = 0;

#[derive(Clone, Debug)]
struct Op {
    cell: String,
    input: String,
}

#[derive(Clone, Debug)]
struct Pending {
    mutation_id: String,
    base_rev: u64,
    ops: Vec<Op>,
}

#[derive(Clone, Debug, PartialEq)]
enum Status {
    Connecting,
    Saved,
    Saving,
    Offline,
    NotSaved(String),
}

#[derive(Default)]
struct Shared {
    port: Option<web_sys::MessagePort>,
    inbound: VecDeque<Json>,
}

pub struct Authority {
    window: web_sys::Window,
    parent: web_sys::Window,
    parent_origin: String,
    client_id: String,
    debug: bool,
    shared: Rc<RefCell<Shared>>,
    loaded: bool,
    confirmed_rev: u64,
    /// The host's committed inputs (sheet 0), A1 → input.
    confirmed: BTreeMap<String, String>,
    /// The workbook whose cell inputs are accounted for (confirmed or pending).
    synced: Option<Arc<Workbook>>,
    pending: Vec<Pending>,
    next_seq: u64,
    online: bool,
    rejected: Option<String>,
    last_debug: String,
    /// The cell being edited and the host rev when its editor opened: the commit's precondition
    /// is "this cell is unchanged since I started typing", not "since I pressed Enter".
    edit_start: Option<(String, u64)>,
}

impl Authority {
    /// `None` when not framed or `parent_origin` is missing.
    pub fn new(parent_origin: String, client_id: String, debug: bool) -> Option<Self> {
        let window = web_sys::window()?;
        let parent = window.parent().ok().flatten()?;
        if js_sys::Object::is(&parent, &window) || parent_origin.is_empty() || parent_origin == "*" {
            return None;
        }
        Some(Self {
            window,
            parent,
            parent_origin,
            client_id,
            debug,
            shared: Rc::default(),
            loaded: false,
            confirmed_rev: 0,
            confirmed: BTreeMap::new(),
            synced: None,
            pending: vec![],
            next_seq: 1,
            online: false,
            rejected: None,
            last_debug: String::new(),
            edit_start: None,
        })
    }

    /// Listen for the host's `gridcraft:connect` (with a `MessagePort`) and say hello.
    pub fn connect(&self, ctx: egui::Context) {
        let parent = self.parent.clone();
        let origin = self.parent_origin.clone();
        let shared = self.shared.clone();
        let on_window_message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
            // Explicit origin and source checks: only the expected parent may hand us a port.
            if event.origin() != origin || !event.source().is_some_and(|s| js_sys::Object::is(&s, &parent)) {
                return;
            }
            if event.data().as_string().as_deref() != Some("gridcraft:connect") {
                return;
            }
            let Some(port) = event.ports().get(0).dyn_into::<web_sys::MessagePort>().ok() else { return };
            let inbox = shared.clone();
            let ctx2 = ctx.clone();
            let on_port_message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |e: web_sys::MessageEvent| {
                let Some(text) = e.data().as_string() else { return };
                if let Ok(msg) = serde_json::from_str::<Json>(&text) {
                    inbox.borrow_mut().inbound.push_back(msg);
                    ctx2.request_repaint();
                }
            });
            port.set_onmessage(Some(on_port_message.as_ref().unchecked_ref()));
            on_port_message.forget();
            port.start();
            shared.borrow_mut().port = Some(port);
            ctx.request_repaint();
        });
        if self.window.add_event_listener_with_callback("message", on_window_message.as_ref().unchecked_ref()).is_ok() {
            on_window_message.forget();
        }
        let hello = js_sys::Object::new();
        let _ = js_sys::Reflect::set(&hello, &"type".into(), &"gridcraft:hello".into());
        let _ = js_sys::Reflect::set(&hello, &"version".into(), &env!("CARGO_PKG_VERSION").into());
        let _ = js_sys::Reflect::set(&hello, &"client_id".into(), &self.client_id.as_str().into());
        if let Err(e) = self.parent.post_message(&hello, &self.parent_origin) {
            log::error!("hello to the host failed: {e:?}");
        }
    }

    fn post(&self, msg: &Json) {
        if let Some(port) = &self.shared.borrow().port
            && let Err(e) = port.post_message(&JsValue::from_str(&msg.to_string()))
        {
            log::error!("post to the host failed: {e:?}");
        }
    }

    fn status(&self) -> Status {
        if let Some(why) = &self.rejected {
            return Status::NotSaved(why.clone());
        }
        if !self.loaded {
            return Status::Connecting;
        }
        if !self.online {
            return Status::Offline;
        }
        if self.pending.is_empty() { Status::Saved } else { Status::Saving }
    }

    /// After each frame: apply what the host sent, send local changes, update "saved".
    pub fn sync(&mut self, app: &mut SheetApp, ctx: &egui::Context) {
        if self.loaded {
            self.send_local_changes(app);
        }
        match &app.editor {
            Some(ed) if self.edit_start.as_ref().is_none_or(|(c, _)| *c != ed.cell.a1()) => {
                self.edit_start = Some((ed.cell.a1(), self.confirmed_rev));
            }
            None => self.edit_start = None,
            _ => {}
        }
        loop {
            let next = self.shared.borrow_mut().inbound.pop_front();
            let Some(msg) = next else { break };
            self.handle(app, &msg);
        }
        let status = self.status();
        if let Some(d) = app.session.active_mut() {
            // The unsaved-changes flag follows host acks, never "handed over".
            if status == Status::Saved {
                d.saved = d.wb.clone();
            } else if !d.is_dirty() {
                d.saved = Arc::new((*d.wb).clone());
            }
        }
        self.draw_status(ctx, &status);
        if self.debug {
            self.post_debug(app, &status);
        }
    }

    fn handle(&mut self, app: &mut SheetApp, msg: &Json) {
        match msg.get("type").and_then(Json::as_str).unwrap_or("") {
            "snapshot" => {
                let rev = msg.get("rev").and_then(Json::as_u64).unwrap_or(0);
                let cells = cells_of(msg.get("cells"));
                self.load_snapshot(app, rev, cells);
                self.online = true;
            }
            "committed" => {
                let rev = msg.get("rev").and_then(Json::as_u64).unwrap_or(0);
                if rev <= self.confirmed_rev {
                    return; // replayed twice
                }
                if rev != self.confirmed_rev + 1 {
                    // A gap: ask the host to replay from what we have.
                    log::warn!("rev gap: have {}, got {rev}", self.confirmed_rev);
                    self.resume();
                    return;
                }
                let mutation_id = msg.get("mutation_id").and_then(Json::as_str).unwrap_or("");
                let ops = ops_of(msg.get("ops"));
                if let Some(i) = self.pending.iter().position(|p| p.mutation_id == mutation_id) {
                    // Our own mutation (its ack was lost, e.g. across a reconnect): an ack.
                    let p = self.pending.remove(i);
                    for op in p.ops {
                        self.confirmed.insert(op.cell, op.input);
                    }
                    self.confirmed_rev = rev;
                    return;
                }
                let pending_cells: Vec<String> = self.pending.iter().flat_map(|p| p.ops.iter().map(|o| o.cell.clone())).collect();
                let mut to_apply = vec![];
                for op in ops {
                    self.confirmed.insert(op.cell.clone(), op.input.clone());
                    // A cell with an unacknowledged local change keeps showing it until the host
                    // decides (it will reject ours if it conflicts).
                    if !pending_cells.contains(&op.cell) {
                        to_apply.push(op);
                    }
                }
                self.confirmed_rev = rev;
                apply_remote(app, rev, &to_apply);
                self.synced = app.session.active().map(|d| d.wb.clone());
            }
            "ack" => {
                let mutation_id = msg.get("mutation_id").and_then(Json::as_str).unwrap_or("");
                let rev = msg.get("rev").and_then(Json::as_u64).unwrap_or(0);
                if let Some(i) = self.pending.iter().position(|p| p.mutation_id == mutation_id) {
                    let p = self.pending.remove(i);
                    for op in p.ops {
                        self.confirmed.insert(op.cell, op.input);
                    }
                    if rev == self.confirmed_rev + 1 {
                        self.confirmed_rev = rev;
                    } else if rev > self.confirmed_rev {
                        self.resume();
                    }
                }
            }
            "rejected" => {
                let mutation_id = msg.get("mutation_id").and_then(Json::as_str).unwrap_or("");
                let code = msg.get("code").and_then(Json::as_str).unwrap_or("rejected").to_string();
                let Some(i) = self.pending.iter().position(|p| p.mutation_id == mutation_id) else { return };
                let p = self.pending.remove(i);
                // The host's current values for the rejected cells.
                for (cell, input) in cells_of(msg.get("cells")) {
                    self.confirmed.insert(cell, input);
                }
                let still_pending: Vec<String> = self.pending.iter().flat_map(|q| q.ops.iter().map(|o| o.cell.clone())).collect();
                let revert: Vec<Op> = p
                    .ops
                    .iter()
                    .filter(|o| !still_pending.contains(&o.cell))
                    .map(|o| Op { cell: o.cell.clone(), input: self.confirmed.get(&o.cell).cloned().unwrap_or_default() })
                    .collect();
                apply_remote(app, self.confirmed_rev, &revert);
                self.synced = app.session.active().map(|d| d.wb.clone());
                let lost: Vec<String> = p.ops.iter().map(|o| format!("{} = \"{}\"", o.cell, o.input)).collect();
                self.rejected = Some(format!("{code}: {}", lost.join(", ")));
            }
            "offline" => self.online = false,
            "online" => {
                self.online = true;
                self.resume();
            }
            "dismiss" => self.rejected = None,
            other => log::warn!("unknown host message {other}"),
        }
    }

    /// Ask the host to replay from our last rev and re-send unacknowledged mutations (same ids:
    /// the host deduplicates).
    fn resume(&self) {
        let pending: Vec<Json> = self.pending.iter().map(mutation_json).collect();
        self.post(&json!({"type": "resume", "last_rev": self.confirmed_rev, "pending": pending}));
    }

    fn load_snapshot(&mut self, app: &mut SheetApp, rev: u64, cells: Vec<(String, String)>) {
        let mut tmp = Session::new();
        tmp.add_document(DocState::new(Workbook::new(), None, "Workbook".into()));
        for (cell, input) in &cells {
            if let Err(e) = tmp.execute_headless("cell.set", json!({"sheet": SHEET, "cell": cell, "input": input})) {
                log::error!("snapshot cell {cell}: {e}");
            }
        }
        let Some(mut doc) = tmp.active().cloned() else { return };
        doc.undo.clear();
        doc.redo.clear();
        doc.saved = doc.wb.clone();
        doc.title = "Workbook".into();
        while !app.session.documents().is_empty() {
            app.session.close_document(0);
        }
        app.session.add_document(doc);
        self.synced = app.session.active().map(|d| d.wb.clone());
        self.confirmed = cells.into_iter().collect();
        self.confirmed_rev = rev;
        self.pending.clear();
        self.loaded = true;
    }

    fn send_local_changes(&mut self, app: &SheetApp) {
        let Some(d) = app.session.active() else { return };
        let Some(synced) = &self.synced else { return };
        if Arc::ptr_eq(synced, &d.wb) {
            return;
        }
        let ops = diff_inputs(synced, &d.wb);
        self.synced = Some(d.wb.clone());
        if ops.is_empty() {
            return;
        }
        let base_rev = match &self.edit_start {
            Some((cell, rev)) if ops.iter().any(|o| o.cell == *cell) => (*rev).min(self.confirmed_rev),
            _ => self.confirmed_rev,
        };
        let p = Pending { mutation_id: format!("{}-{}", self.client_id, self.next_seq), base_rev, ops };
        self.next_seq += 1;
        // A new local edit after a rejection: the rejection notice has been seen.
        self.rejected = None;
        self.post(&mutation_json(&p));
        self.pending.push(p);
    }

    fn draw_status(&self, ctx: &egui::Context, status: &Status) {
        let (text, color) = match status {
            Status::Connecting => ("Connecting…".to_string(), egui::Color32::GRAY),
            Status::Saved => (format!("Saved · rev {}", self.confirmed_rev), egui::Color32::from_rgb(0x1e, 0x7b, 0x34)),
            Status::Saving => (format!("Saving… ({} pending)", self.pending.len()), egui::Color32::from_rgb(0xb2, 0x6b, 0x00)),
            Status::Offline => (format!("Offline · not saved ({} pending)", self.pending.len()), egui::Color32::from_rgb(0xb2, 0x6b, 0x00)),
            Status::NotSaved(why) => (format!("Not saved · {why}"), egui::Color32::from_rgb(0xc0, 0x1c, 0x1c)),
        };
        egui::Area::new(egui::Id::new("host_authority_status")).anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-8.0, -30.0)).interactable(false).show(
            ctx,
            |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.add(egui::Label::new(egui::RichText::new(text).color(color)).wrap_mode(egui::TextWrapMode::Extend));
                });
            },
        );
    }

    /// Test-only state report (`&debug=1`) so a headless test can assert what is on screen.
    fn post_debug(&mut self, app: &SheetApp, status: &Status) {
        let Some(d) = app.session.active() else { return };
        let mut cells = BTreeMap::new();
        if let Some(sh) = d.wb.sheets.get(SHEET) {
            for (c, _) in sh.cells.iter() {
                let t = sh.input_text(c);
                if !t.is_empty() {
                    cells.insert(c.a1(), t);
                }
            }
        }
        let editor = app.editor.as_ref().map(|e| json!({"cell": e.cell.a1(), "text": e.text, "caret": e.caret}));
        let state = json!({
            "type": "debug",
            "status": format!("{status:?}"),
            "rev": self.confirmed_rev,
            "pending": self.pending.len(),
            "cells": cells,
            "editor": editor,
            "selection": {"active": d.selection.active.a1(), "ranges": d.selection.ranges.iter().map(|r| r.a1()).collect::<Vec<_>>()},
            "undo": d.undo.iter().map(|h| h.label.clone()).collect::<Vec<_>>(),
            "redo": d.redo.len(),
            "dirty": d.is_dirty(),
        });
        let text = state.to_string();
        if text != self.last_debug {
            self.post(&state);
            self.last_debug = text;
        }
    }
}

fn mutation_json(p: &Pending) -> Json {
    json!({
        "type": "mutation",
        "mutation_id": p.mutation_id,
        "base_rev": p.base_rev,
        "ops": p.ops.iter().map(|o| json!({"op": "cell.set", "sheet": SHEET, "cell": o.cell, "input": o.input})).collect::<Vec<_>>(),
    })
}

fn ops_of(v: Option<&Json>) -> Vec<Op> {
    v.and_then(Json::as_array)
        .map(|a| {
            a.iter()
                .filter(|o| o.get("op").and_then(Json::as_str) == Some("cell.set"))
                .filter_map(|o| {
                    Some(Op { cell: o.get("cell")?.as_str()?.to_string(), input: o.get("input").and_then(Json::as_str).unwrap_or("").to_string() })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn cells_of(v: Option<&Json>) -> Vec<(String, String)> {
    v.and_then(Json::as_object).map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string())).collect()).unwrap_or_default()
}

/// Cells (sheet 0) whose input differs between two workbooks.
fn diff_inputs(old: &Workbook, new: &Workbook) -> Vec<Op> {
    let (Some(a), Some(b)) = (old.sheets.get(SHEET), new.sheets.get(SHEET)) else { return vec![] };
    if Arc::ptr_eq(a, b) {
        return vec![];
    }
    let mut keys = std::collections::BTreeSet::new();
    keys.extend(a.cells.iter().map(|(c, _)| c));
    keys.extend(b.cells.iter().map(|(c, _)| c));
    keys.into_iter()
        .filter_map(|c| {
            let (x, y) = (a.input_text(c), b.input_text(c));
            (x != y).then(|| Op { cell: c.a1(), input: y })
        })
        .collect()
}

/// Apply host-committed ops (or a reconcile to the host's values) through the engine's remote
/// path: no undo step, redo kept, no journal, selection and the open cell editor untouched; a
/// later local undo replays them instead of reverting them.
fn apply_remote(app: &mut SheetApp, rev: u64, ops: &[Op]) {
    if ops.is_empty() {
        return;
    }
    let Some(sheet) = app.session.active().and_then(|d| d.wb.sheets.get(SHEET)).map(|s| s.name.clone()) else { return };
    let ops: Vec<RemoteOp> =
        ops.iter().filter_map(|o| RemoteOp::set(&sheet, &o.cell, &o.input).map_err(|e| log::error!("remote {}: {e}", o.cell)).ok()).collect();
    if let Err(e) = app.session.apply_remote(rev, &ops) {
        log::error!("remote ops at rev {rev}: {e}");
    }
    app.after_engine();
}

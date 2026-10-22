//! Host-authority bridge, protocol 1 (`?host=authority&parent_origin=<origin>`).
//!
//! Unlike [`crate::host`] (same-origin parent, whole files, "saved" as soon as the bytes are
//! handed over), here the embedding page owns the document and its revision log, and may live on
//! another origin. The editor may run in a sandboxed frame with an opaque origin
//! (`sandbox allow-scripts` without `allow-same-origin`): it needs no cookies or storage, and the
//! parent sees its messages with origin `"null"`.
//!
//! Handshake (window messages, then a private `MessagePort`):
//! - editor → parent, posted to `parent_origin` only:
//!   `{type:"gridcraft:hello", protocol:1, version, bundle:{wasm,js}|null}`. `bundle` holds the
//!   sha256 digests the build wrote into `<meta name="gridcraft-bundle">` (null in dev builds).
//! - parent → editor: `{type:"gridcraft:connect", protocol:1}` with the port transferred. The
//!   editor accepts it only when `event.origin == parent_origin` and `event.source` is the parent
//!   window, and only for protocol 1.
//!
//! On the port (JSON text):
//! - editor → host: `mutation {seq, base_rev, ops}` with `ops: [{op:"cell.set", sheet, cell,
//!   input}]` (sheet by name; an empty input clears). `seq` is monotonic per editor instance and
//!   stays the same on retries; the host builds the idempotency key from it and its session.
//!   `resume {last_rev, pending:[mutation…]}` after the host reports it is back online or after
//!   a gap. `notice {code, command}` when a local command was refused (see below).
//! - host → editor: `snapshot {rev, sheets:[{name, cells:{A1: input}}]}`,
//!   `committed {rev, own?, seq?, ops}` in rev order, `ack {seq, rev}`,
//!   `rejected {seq, code, current_rev, current:[op…]}`, `offline`, `online`, `dismiss`.
//!
//! Local changes come from the engine's commands (`Session::record_local_ops`), one mutation per
//! command: typing, paste, fill, clear, undo and redo. Commands whose effect cell ops can't carry
//! (inserting rows, sheets, formatting) are taken back right away and reported, so the editor
//! never shows a state the host doesn't have.
//!
//! "Saved" only when every mutation has an ack (sent after the host made the commit durable).
//! A rejection reverts the optimistic change to the host's value and shows "Not saved".
//! Host commits apply through `Session::apply_remote`: not undoable by the local user, the
//! selection and an open cell editor are untouched.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;
use std::sync::Arc;

use gridcraft_engine::DocState;
use gridcraft_engine::model::{Sheet, Workbook};
use gridcraft_engine::remote::{LocalEdit, RemoteOp};
use gridcraft_ui_egui::SheetApp;
use serde_json::{Value as Json, json};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::JsValue;
use wasm_bindgen::closure::Closure;

pub const PROTOCOL: u64 = 1;

#[derive(Clone, Debug)]
struct Pending {
    seq: u64,
    base_rev: u64,
    ops: Vec<RemoteOp>,
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

type CellKey = (String, String);

pub struct Authority {
    window: web_sys::Window,
    parent: web_sys::Window,
    parent_origin: String,
    debug: bool,
    shared: Rc<RefCell<Shared>>,
    loaded: bool,
    confirmed_rev: u64,
    /// The host's committed inputs, (sheet, A1) → input.
    confirmed: BTreeMap<CellKey, String>,
    pending: Vec<Pending>,
    next_seq: u64,
    online: bool,
    rejected: Option<String>,
    /// A refused local command, shown until the next local edit.
    notice: Option<String>,
    last_debug: String,
    /// The cell being edited and the host rev when its editor opened: the commit's precondition
    /// is "this cell is unchanged since I started typing", not "since I pressed Enter".
    edit_start: Option<(CellKey, u64)>,
}

impl Authority {
    /// `None` when not framed or `parent_origin` is missing or a wildcard.
    pub fn new(parent_origin: String, debug: bool) -> Option<Self> {
        let window = web_sys::window()?;
        let parent = window.parent().ok().flatten()?;
        if js_sys::Object::is(&parent, &window) || parent_origin.is_empty() || parent_origin == "*" || parent_origin == "null" {
            return None;
        }
        Some(Self {
            window,
            parent,
            parent_origin,
            debug,
            shared: Rc::default(),
            loaded: false,
            confirmed_rev: 0,
            confirmed: BTreeMap::new(),
            pending: vec![],
            next_seq: 1,
            online: false,
            rejected: None,
            notice: None,
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
            let data = event.data();
            let field = |k: &str| js_sys::Reflect::get(&data, &k.into()).ok();
            if field("type").and_then(|v| v.as_string()).as_deref() != Some("gridcraft:connect") {
                return;
            }
            let protocol = field("protocol").and_then(|v| v.as_f64());
            if protocol != Some(PROTOCOL as f64) {
                log::error!("host speaks protocol {protocol:?}, this editor {PROTOCOL}: not connecting");
                return;
            }
            if shared.borrow().port.is_some() {
                return; // one connection per editor instance
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
        let hello = json!({
            "type": "gridcraft:hello",
            "protocol": PROTOCOL,
            "version": env!("CARGO_PKG_VERSION"),
            "bundle": bundle_digests(&self.window),
        });
        let hello = js_sys::JSON::parse(&hello.to_string()).unwrap_or(JsValue::NULL);
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
        app.session.record_local_ops = true;
        if self.loaded {
            self.send_local_changes(app);
        } else {
            // Nothing local counts before the host's snapshot.
            let _ = app.session.take_local_ops();
        }
        let editing = app.editor.as_ref().and_then(|ed| {
            let sheet = app.session.active()?.wb.sheets.get(ed.sheet)?.name.clone();
            Some((sheet, ed.cell.a1()))
        });
        match editing {
            Some(key) if self.edit_start.as_ref().is_none_or(|(k, _)| *k != key) => self.edit_start = Some((key, self.confirmed_rev)),
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
                self.load_snapshot(app, rev, msg);
                self.online = true;
            }
            "committed" => {
                let rev = msg.get("rev").and_then(Json::as_u64).unwrap_or(0);
                if rev <= self.confirmed_rev {
                    return; // replayed twice
                }
                if rev != self.confirmed_rev + 1 {
                    log::warn!("rev gap: have {}, got {rev}", self.confirmed_rev);
                    self.resume();
                    return;
                }
                let ops = ops_of(msg.get("ops"));
                let own = msg.get("own").and_then(Json::as_bool).unwrap_or(false);
                let seq = msg.get("seq").and_then(Json::as_u64);
                if own && let Some(i) = self.pending.iter().position(|p| Some(p.seq) == seq) {
                    // Our own mutation (its ack was lost, e.g. across a reconnect): an ack.
                    let p = self.pending.remove(i);
                    self.confirm(&p.ops);
                    self.confirmed_rev = rev;
                    return;
                }
                let pending_cells: Vec<CellKey> = self.pending.iter().flat_map(|p| p.ops.iter().map(key)).collect();
                let mut to_apply = vec![];
                for op in ops {
                    self.confirmed.insert(key(&op), op.input.clone());
                    // A cell with an unacknowledged local change keeps showing it until the host
                    // decides (it rejects ours if it conflicts).
                    if !pending_cells.contains(&key(&op)) {
                        to_apply.push(op);
                    }
                }
                self.confirmed_rev = rev;
                apply_remote(app, rev, &to_apply);
            }
            "ack" => {
                let seq = msg.get("seq").and_then(Json::as_u64);
                let rev = msg.get("rev").and_then(Json::as_u64).unwrap_or(0);
                if let Some(i) = self.pending.iter().position(|p| Some(p.seq) == seq) {
                    let p = self.pending.remove(i);
                    self.confirm(&p.ops);
                    if rev == self.confirmed_rev + 1 {
                        self.confirmed_rev = rev;
                    } else if rev > self.confirmed_rev {
                        self.resume();
                    }
                }
            }
            "rejected" => {
                let seq = msg.get("seq").and_then(Json::as_u64);
                let code = msg.get("code").and_then(Json::as_str).unwrap_or("rejected").to_string();
                let Some(i) = self.pending.iter().position(|p| Some(p.seq) == seq) else { return };
                let p = self.pending.remove(i);
                // The host's current values for the rejected cells.
                for op in ops_of(msg.get("current")) {
                    self.confirmed.insert(key(&op), op.input);
                }
                let still_pending: Vec<CellKey> = self.pending.iter().flat_map(|q| q.ops.iter().map(key)).collect();
                let revert: Vec<RemoteOp> = p
                    .ops
                    .iter()
                    .filter(|o| !still_pending.contains(&key(o)))
                    .map(|o| RemoteOp { input: self.confirmed.get(&key(o)).cloned().unwrap_or_default(), ..o.clone() })
                    .collect();
                apply_remote(app, self.confirmed_rev, &revert);
                let lost: Vec<String> = p.ops.iter().map(|o| format!("{} = \"{}\"", o.cell.a1(), o.input)).collect();
                self.rejected = Some(format!("{code}: {}", lost.join(", ")));
            }
            "offline" => self.online = false,
            "online" => {
                self.online = true;
                self.resume();
            }
            "dismiss" => {
                self.rejected = None;
                self.notice = None;
            }
            other => log::warn!("unknown host message {other}"),
        }
    }

    fn confirm(&mut self, ops: &[RemoteOp]) {
        for op in ops {
            self.confirmed.insert(key(op), op.input.clone());
        }
    }

    /// Ask the host to replay from our last rev and re-send unacknowledged mutations (same seq:
    /// the host deduplicates).
    fn resume(&self) {
        let pending: Vec<Json> = self.pending.iter().map(mutation_json).collect();
        self.post(&json!({"type": "resume", "last_rev": self.confirmed_rev, "pending": pending}));
    }

    fn load_snapshot(&mut self, app: &mut SheetApp, rev: u64, msg: &Json) {
        // `sheets: [{name, cells}]`; a bare `cells` map is one sheet.
        let sheets: Vec<(String, Json)> = match msg.get("sheets").and_then(Json::as_array) {
            Some(list) => {
                list.iter().filter_map(|s| Some((s.get("name")?.as_str()?.to_string(), s.get("cells").cloned().unwrap_or(Json::Null)))).collect()
            }
            None => vec![("Sheet1".to_string(), msg.get("cells").cloned().unwrap_or(Json::Null))],
        };
        let mut wb = Workbook::new();
        wb.sheets.clear();
        for (name, _) in &sheets {
            wb.sheets.push(Arc::new(Sheet::new(name.clone())));
        }
        if wb.sheets.is_empty() {
            wb = Workbook::new();
        }
        let ops: Vec<RemoteOp> = sheets
            .iter()
            .flat_map(|(name, cells)| {
                cells
                    .as_object()
                    .into_iter()
                    .flat_map(|m| m.iter())
                    .filter_map(|(cell, input)| RemoteOp::set(name, cell, input.as_str().unwrap_or_default()).ok())
                    .collect::<Vec<_>>()
            })
            .collect();
        let mut doc = DocState::new(wb, None, "Workbook".into());
        if let Err(e) = doc.apply_remote(rev, &ops) {
            log::error!("snapshot at rev {rev}: {e}");
        }
        doc.saved = doc.wb.clone();
        while !app.session.documents().is_empty() {
            app.session.close_document(0);
        }
        app.session.add_document(doc);
        let _ = app.session.take_local_ops();
        self.confirmed = ops.into_iter().map(|o| (key(&o), o.input)).collect();
        self.confirmed_rev = rev;
        self.pending.clear();
        self.loaded = true;
    }

    fn send_local_changes(&mut self, app: &mut SheetApp) {
        for edit in app.session.take_local_ops() {
            if !edit.complete && edit.ops.is_empty() {
                self.refuse(app, &edit);
                continue;
            }
            if edit.ops.is_empty() {
                continue;
            }
            if !edit.complete {
                // Contents are shared; the formatting part of the same command stays local.
                self.notice = Some(format!("{}: only cell contents are shared", edit.command));
            }
            let base_rev = match &self.edit_start {
                Some((k, rev)) if edit.ops.iter().any(|o| key(o) == *k) => (*rev).min(self.confirmed_rev),
                _ => self.confirmed_rev,
            };
            let p = Pending { seq: self.next_seq, base_rev, ops: edit.ops };
            self.next_seq += 1;
            // A new local edit after a rejection: the rejection notice has been seen.
            self.rejected = None;
            self.post(&mutation_json(&p));
            self.pending.push(p);
        }
    }

    /// A command whose effect cell ops can't carry: take it back so the editor keeps showing
    /// only what the host has, and say so.
    fn refuse(&mut self, app: &mut SheetApp, edit: &LocalEdit) {
        if edit.undo_step && app.session.execute("edit.undo", json!({})).is_ok() {
            if let Some(d) = app.session.active_mut() {
                d.redo.pop();
            }
            let _ = app.session.take_local_ops();
            app.after_engine();
            let what = if edit.structural { "inserting or deleting rows, columns or sheets" } else { "formatting" };
            self.notice = Some(format!("{} isn't shared yet ({what}): undone", edit.command));
            self.post(&json!({"type": "notice", "code": "unsupported_command", "command": edit.command}));
        }
    }

    fn draw_status(&self, ctx: &egui::Context, status: &Status) {
        let (mut text, color) = match status {
            Status::Connecting => ("Connecting…".to_string(), egui::Color32::GRAY),
            Status::Saved => (format!("Saved · rev {}", self.confirmed_rev), egui::Color32::from_rgb(0x1e, 0x7b, 0x34)),
            Status::Saving => (format!("Saving… ({} pending)", self.pending.len()), egui::Color32::from_rgb(0xb2, 0x6b, 0x00)),
            Status::Offline => (format!("Offline · not saved ({} pending)", self.pending.len()), egui::Color32::from_rgb(0xb2, 0x6b, 0x00)),
            Status::NotSaved(why) => (format!("Not saved · {why}"), egui::Color32::from_rgb(0xc0, 0x1c, 0x1c)),
        };
        if let Some(n) = &self.notice {
            text = format!("{text} · {n}");
        }
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
        let mut sheets = BTreeMap::new();
        for sh in &d.wb.sheets {
            let mut cells = BTreeMap::new();
            for (c, _) in sh.cells.iter() {
                let t = sh.input_text(c);
                if !t.is_empty() {
                    cells.insert(c.a1(), t);
                }
            }
            sheets.insert(sh.name.clone(), cells);
        }
        let first = d.wb.sheets.first().and_then(|s| sheets.get(&s.name)).cloned().unwrap_or_default();
        let editor = app.editor.as_ref().map(|e| json!({"cell": e.cell.a1(), "text": e.text, "caret": e.caret}));
        let state = json!({
            "type": "debug",
            "status": format!("{status:?}"),
            "notice": self.notice,
            "rev": self.confirmed_rev,
            "pending": self.pending.len(),
            "cells": first,
            "sheets": sheets,
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

fn key(op: &RemoteOp) -> CellKey {
    (op.sheet.clone(), op.cell.a1())
}

fn mutation_json(p: &Pending) -> Json {
    let ops: Vec<Json> = p.ops.iter().map(|o| json!({"op": "cell.set", "sheet": o.sheet, "cell": o.cell.a1(), "input": o.input})).collect();
    json!({"type": "mutation", "seq": p.seq, "base_rev": p.base_rev, "ops": ops})
}

fn ops_of(v: Option<&Json>) -> Vec<RemoteOp> {
    v.and_then(Json::as_array)
        .map(|a| a.iter().filter_map(|o| RemoteOp::from_json(o).map_err(|e| log::warn!("host op ignored: {e}")).ok()).collect())
        .unwrap_or_default()
}

/// The digests the release build wrote into `<meta name="gridcraft-bundle"
/// content="wasm=sha256:<hex> js=sha256:<hex>">`; `null` when absent (dev builds).
fn bundle_digests(window: &web_sys::Window) -> Json {
    let content =
        window.document().and_then(|d| d.query_selector("meta[name=\"gridcraft-bundle\"]").ok().flatten()).and_then(|m| m.get_attribute("content"));
    let Some(content) = content else { return Json::Null };
    let mut out = serde_json::Map::new();
    for part in content.split_whitespace() {
        if let Some((k, v)) = part.split_once('=')
            && (k == "wasm" || k == "js")
            && v.strip_prefix("sha256:").is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            out.insert(k.to_string(), Json::String(v.to_string()));
        }
    }
    if out.contains_key("wasm") && out.contains_key("js") { Json::Object(out) } else { Json::Null }
}

/// Apply host-committed ops (or a reconcile to the host's values) through the engine's remote
/// path: no undo step, redo kept, no journal, selection and the open cell editor untouched; a
/// later local undo replays them instead of reverting them.
fn apply_remote(app: &mut SheetApp, rev: u64, ops: &[RemoteOp]) {
    if ops.is_empty() {
        return;
    }
    if let Err(e) = app.session.apply_remote(rev, ops) {
        log::error!("remote ops at rev {rev}: {e}");
    }
    app.after_engine();
}

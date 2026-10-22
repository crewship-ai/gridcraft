//! Host-authority bridge (`?host=authority&parent_origin=<origin>`): the browser glue around
//! [`crate::protocol`], which defines protocol 1 and the sync state.
//!
//! Unlike [`crate::host`] (same-origin parent, whole files, "saved" as soon as the bytes are
//! handed over), here the embedding page owns the document and its revision log, and may live on
//! another origin. The editor is meant to run in a sandboxed frame with an opaque origin
//! (`sandbox allow-scripts`, no `allow-same-origin`, loaded credentialless): it uses no cookies
//! or storage, and the parent sees its messages with origin `"null"`.
//!
//! - Hello goes to `parent_origin` only, from the window's `load` event or later (a host hears
//!   the hello only after the frame's first `load`, so the port reaches only the document whose
//!   load it saw).
//! - The port is accepted once, only from the parent window and `parent_origin`. After that,
//!   nothing is ever sent on window messages: every message travels on the port.
//! - Local changes come from the engine's commands (`Session::record_local_ops`): typing, paste,
//!   fill, clear, undo and redo. Commands whose effect cell ops can't carry (rows, sheets,
//!   formatting) are taken back right away, so the editor never shows a state the host doesn't
//!   have.
//! - Host commits apply through `Session::apply_remote`: not undoable by the local user, the
//!   redo stack, the selection and an open cell editor are untouched.
//! - "Saved" only when every mutation has an ack (sent once the host made the commit durable).
//!   A rejection shows the host's value again and "Not saved".
//!
//! Test hook: with `&debug=1` in the URL, or `globalThis.__gridcraftDebug === true` set before
//! the page's scripts run (a test harness can inject it; the bridge never sees it), the editor
//! writes its state as JSON into a hidden `<pre id="gridcraft-debug">` of its own document.
//! Nothing of it travels to the host.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;
use std::sync::Arc;

use gridcraft_engine::DocState;
use gridcraft_ui_egui::SheetApp;
use serde_json::{Value as Json, json};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::JsValue;
use wasm_bindgen::closure::Closure;

use crate::protocol::{self, Effect, Status, Sync};

#[derive(Default)]
struct Shared {
    port: Option<web_sys::MessagePort>,
    inbound: VecDeque<String>,
}

pub struct Authority {
    window: web_sys::Window,
    parent: web_sys::Window,
    parent_origin: String,
    debug: bool,
    shared: Rc<RefCell<Shared>>,
    sync: Sync,
    last_debug: String,
}

impl Authority {
    /// `None` when not framed or `parent_origin` is missing, a wildcard or opaque.
    pub fn new(parent_origin: String, debug: bool) -> Option<Self> {
        let window = web_sys::window()?;
        let parent = window.parent().ok().flatten()?;
        if js_sys::Object::is(&parent, &window) || parent_origin.is_empty() || parent_origin == "*" || parent_origin == "null" {
            return None;
        }
        let injected = js_sys::Reflect::get(&window, &"__gridcraftDebug".into()).ok().and_then(|v| v.as_bool()) == Some(true);
        Some(Self { window, parent, parent_origin, debug: debug || injected, shared: Rc::default(), sync: Sync::new(), last_debug: String::new() })
    }

    /// Listen for the host's `gridcraft:connect` (with a `MessagePort`), then say hello once the
    /// window has loaded.
    pub fn connect(&self, ctx: egui::Context) {
        let parent = self.parent.clone();
        let origin = self.parent_origin.clone();
        let shared = self.shared.clone();
        let on_window_message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
            // Explicit origin and source checks: only the expected parent may hand us a port.
            if event.origin() != origin || !event.source().is_some_and(|s| js_sys::Object::is(&s, &parent)) {
                return;
            }
            if shared.borrow().port.is_some() {
                return; // one connection per editor instance
            }
            let data = js_sys::JSON::stringify(&event.data()).ok().and_then(|s| s.as_string()).and_then(|s| serde_json::from_str::<Json>(&s).ok());
            if !data.as_ref().is_some_and(protocol::is_connect) {
                if data.as_ref().and_then(|d| d.get("type")).and_then(Json::as_str) == Some("gridcraft:connect") {
                    log::error!("the host speaks another bridge protocol than {}: not connecting", protocol::PROTOCOL);
                }
                return;
            }
            let Some(port) = event.ports().get(0).dyn_into::<web_sys::MessagePort>().ok() else { return };
            let inbox = shared.clone();
            let ctx2 = ctx.clone();
            let on_port_message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |e: web_sys::MessageEvent| {
                if let Some(text) = e.data().as_string() {
                    inbox.borrow_mut().inbound.push_back(text);
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

        let bundle = self.window.location().pathname().ok().and_then(|p| protocol::bundle_digest_from_path(&p));
        if bundle.is_none() {
            log::warn!("not served from a pinned bundle path (/b/<digest>/index.html): the hello names no bundle");
        }
        let hello = protocol::hello(bundle.as_deref()).to_string();
        let parent = self.parent.clone();
        let origin = self.parent_origin.clone();
        // Always in a task of its own, after the one that fired `load`.
        let say_hello = Closure::once_into_js(move || {
            let msg = js_sys::JSON::parse(&hello).unwrap_or(JsValue::NULL);
            if let Err(e) = parent.post_message(&msg, &origin) {
                log::error!("hello to the host failed: {e:?}");
            }
        });
        let complete = self.window.document().is_some_and(|d| d.ready_state() == "complete");
        if complete {
            let _ = self.window.set_timeout_with_callback_and_timeout_and_arguments_0(say_hello.unchecked_ref(), 0);
        } else {
            let window = self.window.clone();
            let on_load = Closure::once_into_js(move || {
                let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(say_hello.unchecked_ref(), 0);
            });
            let options = web_sys::AddEventListenerOptions::new();
            options.set_once(true);
            let _ = self.window.add_event_listener_with_callback_and_add_event_listener_options("load", on_load.unchecked_ref(), &options);
        }
    }

    fn post(&self, msg: &Json) {
        if let Some(port) = &self.shared.borrow().port
            && let Err(e) = port.post_message(&JsValue::from_str(&msg.to_string()))
        {
            log::error!("post to the host failed: {e:?}");
        }
    }

    /// After each frame: apply what the host sent, send local changes, update "saved".
    pub fn sync(&mut self, app: &mut SheetApp, ctx: &egui::Context) {
        app.session.record_local_ops = true;
        if self.sync.loaded {
            for edit in app.session.take_local_ops() {
                let effects = self.sync.local_edit(edit);
                self.run(app, effects);
            }
        } else {
            // Nothing local counts before the host's snapshot.
            let _ = app.session.take_local_ops();
        }
        let editing = app.editor.as_ref().and_then(|ed| {
            let sheet = app.session.active()?.wb.sheets.get(ed.sheet)?.name.clone();
            Some((sheet, ed.cell.a1()))
        });
        self.sync.editing(editing);
        loop {
            let next = self.shared.borrow_mut().inbound.pop_front();
            let Some(text) = next else { break };
            match protocol::parse_host(&text) {
                Ok(msg) => {
                    let effects = self.sync.on_host(msg);
                    self.run(app, effects);
                }
                Err(e) => log::warn!("host message ignored: {e}"),
            }
        }
        let status = self.sync.status();
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
            self.write_debug(app, &status);
        }
    }

    fn run(&mut self, app: &mut SheetApp, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Post(msg) => self.post(&msg),
                Effect::Load { rev, snapshot } => self.load(app, rev, &snapshot),
                Effect::Apply { rev, ops } => {
                    if let Err(e) = app.session.apply_remote(rev, &ops) {
                        log::error!("remote ops at rev {rev}: {e}");
                    }
                    app.after_engine();
                }
                Effect::Undo => {
                    if app.session.execute("edit.undo", json!({})).is_ok() {
                        if let Some(d) = app.session.active_mut() {
                            d.redo.pop();
                        }
                        let _ = app.session.take_local_ops();
                        app.after_engine();
                    }
                }
            }
        }
    }

    /// Replaces the open workbook with the host's snapshot; the view's selection is kept where
    /// it still fits.
    fn load(&mut self, app: &mut SheetApp, rev: u64, snapshot: &Json) {
        let wb = match protocol::workbook_from_snapshot(snapshot) {
            Ok(wb) => wb,
            Err(e) => {
                log::error!("snapshot at rev {rev}: {e}");
                self.sync.rejected = Some(format!("the workbook could not be read: {e}"));
                return;
            }
        };
        let inputs = protocol::inputs_of(&wb);
        let previous = app.session.active().map(|d| (d.sheet_index(), d.selection.clone()));
        let mut doc = DocState::new(wb, None, "Workbook".into());
        if let Some((sheet, selection)) = previous
            && doc.sheet_index() == sheet
        {
            doc.selection = selection;
        }
        doc.saved = doc.wb.clone();
        while !app.session.documents().is_empty() {
            app.session.close_document(0);
        }
        app.session.add_document(doc);
        let _ = app.session.take_local_ops();
        app.after_engine();
        self.sync.loaded(rev, inputs);
    }

    fn draw_status(&self, ctx: &egui::Context, status: &Status) {
        let (mut text, color) = match status {
            Status::Connecting => ("Connecting…".to_string(), egui::Color32::GRAY),
            Status::Saved => (format!("Saved · rev {}", self.sync.confirmed_rev), egui::Color32::from_rgb(0x1e, 0x7b, 0x34)),
            Status::Saving => (format!("Saving… ({} pending)", self.sync.pending.len()), egui::Color32::from_rgb(0xb2, 0x6b, 0x00)),
            Status::Offline => (format!("Offline · not saved ({} pending)", self.sync.pending.len()), egui::Color32::from_rgb(0xb2, 0x6b, 0x00)),
            Status::NotSaved(why) => (format!("Not saved · {why}"), egui::Color32::from_rgb(0xc0, 0x1c, 0x1c)),
        };
        if let Some(n) = &self.sync.notice {
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

    /// Test-only state report into the editor's own document (see the module docs).
    fn write_debug(&mut self, app: &SheetApp, status: &Status) {
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
            "status": format!("{status:?}"),
            "notice": self.sync.notice,
            "rev": self.sync.confirmed_rev,
            "pending": self.sync.pending.len(),
            "cells": first,
            "sheets": sheets,
            "editor": editor,
            "selection": {"active": d.selection.active.a1(), "ranges": d.selection.ranges.iter().map(|r| r.a1()).collect::<Vec<_>>()},
            "undo": d.undo.iter().map(|h| h.label.clone()).collect::<Vec<_>>(),
            "redo": d.redo.len(),
            "dirty": d.is_dirty(),
        });
        let text = state.to_string();
        if text == self.last_debug {
            return;
        }
        let Some(doc) = self.window.document() else { return };
        let el = match doc.get_element_by_id("gridcraft-debug") {
            Some(el) => el,
            None => {
                let Ok(el) = doc.create_element("pre") else { return };
                el.set_id("gridcraft-debug");
                let _ = el.set_attribute("hidden", "");
                if let Some(body) = doc.body() {
                    let _ = body.append_child(&el);
                }
                el
            }
        };
        el.set_text_content(Some(&text));
        self.last_debug = text;
    }
}

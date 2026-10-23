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
//! - The open cell editor is reported as a `draft` (its address only, never its text) whenever
//!   it opens, moves to another cell, commits or is cancelled, so the host can warn about another
//!   writer's change to that cell.
//! - "Saved" only when every mutation has an ack (sent once the host made the commit durable).
//!   A rejection shows the host's value again and "Not saved".
//! - A host `snapshot` after the first (a reload) keeps the open cell editor, the selection and,
//!   when only cell contents changed, the undo history; unconfirmed mutations the host did not
//!   list in `discarded` stay on screen and are sent again unchanged (`document::load_snapshot`).
//!
//! Test hook: with `&debug=1` in the URL, or `globalThis.__gridcraftDebug === true` set before
//! the page's scripts run (a test harness can inject it; the bridge never sees it), the editor
//! writes its state as JSON into a hidden `<pre id="gridcraft-debug">` of its own document.
//! Nothing of it travels to the host.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;

use gridcraft_ui_egui::SheetApp;
use serde_json::{Value as Json, json};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::JsValue;
use wasm_bindgen::closure::Closure;

use crate::document;
use crate::protocol::{self, Status, Sync};

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
            log::warn!("not served from a pinned bundle path (…/b/<digest>/index.html): the hello names no bundle");
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
        let inbound: Vec<String> = self.shared.borrow_mut().inbound.drain(..).collect();
        for msg in document::step(&mut self.sync, app, inbound) {
            self.post(&msg);
        }
        let status = self.sync.status();
        self.draw_status(ctx, &status);
        if self.debug {
            self.write_debug(app, &status);
        }
    }

    fn draw_status(&mut self, ctx: &egui::Context, status: &Status) {
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
        // Discarded edits stay on screen until the person dismisses them.
        let discarded = self.sync.discarded_notice();
        let mut dismiss = false;
        egui::Area::new(egui::Id::new("host_authority_status"))
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-8.0, -30.0))
            .interactable(discarded.is_some())
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    if let Some(d) = &discarded {
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::Label::new(egui::RichText::new(d).color(egui::Color32::from_rgb(0xc0, 0x1c, 0x1c)))
                                    .wrap_mode(egui::TextWrapMode::Extend),
                            );
                            dismiss = ui.small_button("Dismiss").clicked();
                        });
                    }
                    ui.add(egui::Label::new(egui::RichText::new(text).color(color)).wrap_mode(egui::TextWrapMode::Extend));
                });
            });
        if dismiss {
            self.sync.dismiss_discarded();
        }
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
            "notice": self.sync.notices(),
            "discarded": self.sync.discarded_notice(),
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

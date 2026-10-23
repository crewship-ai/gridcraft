//! The document side of the host-authority bridge: one frame's sync between the open workbook
//! and [`Sync`], and what a host `snapshot` does to the workbook and the editor around it. No
//! browser here (`authority` is the glue: it moves the port's strings and draws the status), so
//! it is tested natively with the real [`SheetApp`].
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use gridcraft_engine::DocState;
use gridcraft_engine::core::CellRef;
use gridcraft_engine::model::Workbook;
use gridcraft_engine::remote::RemoteOp;
use gridcraft_ui_egui::SheetApp;
use serde_json::{Value as Json, json};

use crate::protocol::{self, CellKey, Effect, Status, Sync};

/// After each frame: take the local commands, apply what the host sent (`inbound`, port
/// messages in arrival order), report the open drafts and keep the unsaved-changes flag on host
/// acks. Returns the messages to post on the port, in order.
pub fn step(sync: &mut Sync, app: &mut SheetApp, inbound: Vec<String>) -> Vec<Json> {
    let mut out = vec![];
    app.session.record_local_ops = true;
    if sync.loaded {
        for edit in app.session.take_local_ops() {
            let effects = sync.local_edit(edit);
            run(sync, app, effects, &mut out);
        }
    } else {
        // Nothing local counts before the host's snapshot.
        let _ = app.session.take_local_ops();
    }
    let editing = editing(app);
    sync.editing(editing.clone());
    for text in inbound {
        match protocol::parse_host(&text) {
            Ok(msg) => {
                let effects = sync.on_host(msg);
                run(sync, app, effects, &mut out);
            }
            Err(e) => log::warn!("host message ignored: {e}"),
        }
    }
    // Which cell has an open, uncommitted editor (its address, never its text): the host hears
    // of each change once, after any commit this frame sent.
    let effects = sync.drafts(editing);
    run(sync, app, effects, &mut out);
    if let Some(d) = app.session.active_mut() {
        // The unsaved-changes flag follows host acks, never "handed over".
        if sync.status() == Status::Saved {
            d.saved = d.wb.clone();
        } else if !d.is_dirty() {
            d.saved = Arc::new((*d.wb).clone());
        }
    }
    out
}

/// The cell with an open cell editor, by sheet name and A1.
pub fn editing(app: &SheetApp) -> Option<CellKey> {
    let ed = app.editor.as_ref()?;
    let sheet = app.session.active()?.wb.sheets.get(ed.sheet)?.name.clone();
    Some((sheet, ed.cell.a1()))
}

fn run(sync: &mut Sync, app: &mut SheetApp, effects: Vec<Effect>, out: &mut Vec<Json>) {
    for effect in effects {
        match effect {
            Effect::Post(msg) => out.push(msg),
            Effect::Load { rev, snapshot, replay } => match load_snapshot(app, rev, &snapshot, &replay) {
                Ok(inputs) => sync.loaded(rev, inputs),
                Err(e) => {
                    log::error!("snapshot at rev {rev}: {e}");
                    sync.rejected = Some(format!("the workbook could not be read: {e}"));
                }
            },
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

/// Makes the host's snapshot the document, with `replay` (the editor's unconfirmed edits the host
/// did not discard) on top. Returns the snapshot's own cell inputs: what the host has confirmed.
///
/// Whatever the editor holds around the document stays: an open cell editor keeps its text, caret
/// and cell even when the snapshot changed that cell (the draft is the person's; the host keeps
/// hearing of it as a `draft`), and the selection stays.
///
/// A snapshot of the same session differs from the open document only in cell contents (cell ops
/// are the only edits a host-authority session shares; an import is a new generation and a new
/// frame). Then the difference is applied as a remote op batch ([`DocState::apply_remote`]): the
/// undo and redo history stay, and undoing a local edit afterwards keeps the snapshot's cells as
/// it keeps any other writer's. Anything else (other sheets, names, settings, an apply that would
/// not reproduce the snapshot exactly) replaces the document, and its history goes with it: undo
/// steps are whole workbooks of content the host replaced, and restoring one would write that
/// content back over the host's.
pub fn load_snapshot(app: &mut SheetApp, rev: u64, snapshot: &Json, replay: &[RemoteOp]) -> Result<BTreeMap<CellKey, String>, String> {
    let wb = protocol::workbook_from_snapshot(snapshot)?;
    let inputs = protocol::inputs_of(&wb);
    let mut target = inputs.clone();
    for op in replay {
        if op.input.is_empty() {
            target.remove(&protocol::key(op));
        } else {
            target.insert(protocol::key(op), op.input.clone());
        }
    }
    let replayed: BTreeSet<CellKey> = replay.iter().map(protocol::key).collect();
    if !rebase(app, rev, &wb, &target, &replayed) {
        replace(app, rev, wb, replay);
    }
    let _ = app.session.take_local_ops();
    app.after_engine();
    Ok(inputs)
}

/// Brings the open document to `target` (cell inputs) as one remote batch, keeping its history.
/// False when the document differs from `wb` in more than cell contents, or the batch would not
/// reproduce `target` exactly, or afterwards a cell outside `replayed` (the editor's own
/// unconfirmed cells) or a row looks different from the snapshot's (a number format a local edit
/// added, a hidden row, a row style): then the caller replaces the document.
fn rebase(app: &mut SheetApp, rev: u64, wb: &Workbook, target: &BTreeMap<CellKey, String>, replayed: &BTreeSet<CellKey>) -> bool {
    let Some(d) = app.session.active_mut() else { return false };
    if shape(&d.wb) != shape(wb) {
        return false;
    }
    let current = protocol::inputs_of(&d.wb);
    let cells: BTreeSet<&CellKey> = current.keys().chain(target.keys()).collect();
    let mut ops = vec![];
    for k in cells {
        let want = target.get(k);
        if current.get(k) == want {
            continue;
        }
        let Some(cell) = CellRef::parse(&k.1) else { return false };
        ops.push(RemoteOp { sheet: k.0.clone(), cell, input: want.cloned().unwrap_or_default() });
    }
    // `Ok(false)`: a revision below the last one applied; the document is untouched.
    if !matches!(d.apply_remote(rev, &ops), Ok(true)) {
        return false;
    }
    protocol::inputs_of(&d.wb) == *target && looks(&d.wb, replayed) == looks(wb, replayed)
}

/// What cell ops do not carry but a cell or row shows: the default style, each cell's style
/// (resolved, so two style tables compare) unless it is the default or the cell is in `skip`,
/// and each row's hidden, outline and style state and any height the user set (automatic heights
/// follow content).
fn looks(wb: &Workbook, skip: &BTreeSet<CellKey>) -> Json {
    let default = wb.styles.get(gridcraft_engine::model::StyleId::DEFAULT);
    let style = |id| {
        let s = wb.styles.get(id);
        (s != default).then(|| serde_json::to_value(s).unwrap_or(Json::Null))
    };
    let sheets: Vec<Json> = wb
        .sheets
        .iter()
        .map(|sh| {
            let cells: BTreeMap<String, Json> = sh
                .cells
                .iter()
                .filter(|(c, _)| !skip.contains(&(sh.name.clone(), c.a1())))
                .filter_map(|(c, cell)| Some((c.a1(), style(cell.style)?)))
                .collect();
            let rows: BTreeMap<u32, Json> = sh
                .rows
                .iter()
                .map(|(r, l)| (*r, json!([l.hidden, l.outline, l.collapsed, l.custom.then_some(l.size).flatten(), l.style.and_then(style)])))
                .filter(|(_, v)| *v != json!([false, 0, false, null, null]))
                .collect();
            json!({"name": sh.name, "cells": cells, "rows": rows})
        })
        .collect();
    json!({"default": serde_json::to_value(default).unwrap_or(Json::Null), "sheets": sheets})
}

/// The workbook without what cell ops change or what is the view's own: cells, row heights
/// (they follow content), the style table (cell ops add number formats), the active sheet and
/// each sheet's scroll position and zoom.
fn shape(wb: &Workbook) -> Json {
    let mut v = serde_json::to_value(wb).unwrap_or(Json::Null);
    if let Some(o) = v.as_object_mut() {
        o.remove("styles");
        o.remove("active_sheet");
    }
    if let Some(sheets) = v.get_mut("sheets").and_then(Json::as_array_mut) {
        for sh in sheets.iter_mut().filter_map(Json::as_object_mut) {
            for k in ["cells", "rows", "view_active", "view_top_left", "zoom"] {
                sh.remove(k);
            }
        }
    }
    v
}

/// A new document from the snapshot, on the sheet the person was on (by name) with their
/// selection; `replay` on top as a remote batch. An open cell editor follows its sheet by name,
/// and is closed only when that sheet is gone.
fn replace(app: &mut SheetApp, rev: u64, mut wb: Workbook, replay: &[RemoteOp]) {
    let previous = app.session.active().and_then(|d| Some((d.wb.active()?.name.clone(), d.selection.clone())));
    let editing = app.editor.as_ref().and_then(|ed| Some(app.session.active()?.wb.sheets.get(ed.sheet)?.name.clone()));
    let mut keep_selection = None;
    if let Some((name, selection)) = previous
        && let Some(i) = wb.sheet_index(&name)
    {
        wb.active_sheet = i;
        keep_selection = Some(selection);
    }
    let mut doc = DocState::new(wb, None, "Workbook".into());
    if let Some(selection) = keep_selection {
        doc.selection = selection;
    }
    doc.saved = doc.wb.clone();
    let replay: Vec<RemoteOp> = replay.iter().filter(|op| doc.wb.sheet_index(&op.sheet).is_some()).cloned().collect();
    if !replay.is_empty()
        && let Err(e) = doc.apply_remote(rev, &replay)
    {
        log::error!("unconfirmed edits on the snapshot at rev {rev}: {e}");
    }
    match editing.map(|name| doc.wb.sheet_index(&name)) {
        Some(Some(i)) => {
            if let Some(ed) = app.editor.as_mut() {
                ed.sheet = i;
            }
        }
        Some(None) => app.cancel_edit(),
        None => {}
    }
    while !app.session.documents().is_empty() {
        app.session.close_document(0);
    }
    app.session.add_document(doc);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    //! F7 (editor contract, 23. 10.): a host `snapshot` while a cell editor is open and an edit
    //! is unconfirmed. Driven frame by frame the way the browser runs it: egui input, the app's
    //! UI, then [`step`] with the port's messages.
    use super::*;
    use egui::{Context, Event, Key, Modifiers, RawInput, Rect, pos2, vec2};
    use gridcraft_engine::Session;

    struct Editor {
        ctx: Context,
        app: SheetApp,
        sync: Sync,
        /// Everything posted on the port, in order.
        posted: Vec<Json>,
    }

    impl Editor {
        fn new() -> Self {
            let mut session = Session::new();
            session.new_workbook();
            let mut e = Editor { ctx: Context::default(), app: SheetApp::new(session, Default::default()), sync: Sync::new(), posted: vec![] };
            e.frames(4);
            e
        }
        /// One frame: input, UI, then the bridge step with what the host sent meanwhile.
        fn frame(&mut self, events: Vec<Event>, inbound: Vec<Json>) {
            let mut raw = RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1200.0, 800.0))), events, ..Default::default() };
            self.app.raw_input_hook(&mut raw);
            let app = &mut self.app;
            let mut out = self.ctx.run_ui(raw, |ui| {
                let ctx = ui.ctx().clone();
                app.logic(&ctx);
                app.ui(ui);
            });
            out.textures_delta.clear();
            let inbound = inbound.iter().map(Json::to_string).collect();
            self.posted.extend(step(&mut self.sync, &mut self.app, inbound));
        }
        fn frames(&mut self, n: usize) {
            for _ in 0..n {
                self.frame(vec![], vec![]);
            }
        }
        fn host(&mut self, msg: Json) {
            self.frame(vec![], vec![msg]);
            self.frames(2);
        }
        fn keys(&mut self, k: Key) {
            let ev = |pressed| Event::Key { key: k, physical_key: Some(k), pressed, repeat: false, modifiers: Modifiers::NONE };
            self.frame(vec![ev(true), ev(false)], vec![]);
            self.frames(2);
        }
        fn type_text(&mut self, s: &str) {
            self.frame(s.chars().map(|c| Event::Text(c.to_string())).collect(), vec![]);
            self.frames(2);
        }
        fn select(&mut self, a1: &str) {
            self.app.session.run("selection.set", json!({"cell": a1})).unwrap();
            self.frames(2);
        }
        /// Types `text` into `a1` and presses Enter: one local edit, one mutation.
        fn set(&mut self, a1: &str, text: &str) {
            self.select(a1);
            self.type_text(text);
            self.keys(Key::Enter);
        }
        fn input(&self, a1: &str) -> String {
            self.app.session.active().unwrap().wb.sheet(0).unwrap().input_text(CellRef::parse(a1).unwrap())
        }
        fn undo_len(&self) -> usize {
            self.app.session.active().unwrap().undo.len()
        }
        /// (cell, text, caret) of the open cell editor.
        fn editor(&self) -> Option<(String, String, usize)> {
            self.app.editor.as_ref().map(|ed| (ed.cell.a1(), ed.text.clone(), ed.caret))
        }
        fn active(&self) -> String {
            self.app.session.active().unwrap().selection.active.a1()
        }
        fn since(&self, n: usize) -> Vec<Json> {
            self.posted[n..].to_vec()
        }
    }

    /// A host workbook: Sheet1 with these inputs.
    fn workbook(cells: &[(&str, &str)]) -> Json {
        let mut s = Session::new();
        s.new_workbook();
        for (a1, v) in cells {
            s.run("cell.set", json!({"cell": a1, "input": v})).unwrap();
        }
        serde_json::to_value(&*s.active().unwrap().wb).unwrap()
    }
    fn snapshot(rev: u64, cells: &[(&str, &str)], discarded: &[u64]) -> Json {
        json!({"type": "snapshot", "protocol": 1, "rev": rev, "generation": 1, "snapshot": workbook(cells), "discarded": discarded})
    }
    fn of_type(msgs: &[Json], t: &str) -> Vec<Json> {
        msgs.iter().filter(|m| m["type"] == t).cloned().collect()
    }
    fn draft_cells(m: &Json) -> Vec<String> {
        m["cells"].as_array().unwrap().iter().map(|c| format!("{}!{}", c["sheet"].as_str().unwrap(), c["cell"].as_str().unwrap())).collect()
    }

    /// Loaded at rev 1 with B2 = 10; B3 = "1" typed and sent (seq 1, unconfirmed); B5 open in a
    /// cell editor with "draft", caret at 3.
    fn typing_with_an_unconfirmed_edit() -> Editor {
        let mut e = Editor::new();
        e.host(snapshot(1, &[("B2", "10")], &[]));
        assert_eq!(e.sync.status(), Status::Saved);
        e.set("B3", "1");
        let m = of_type(&e.posted, "mutation");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0]["seq"], 1);
        e.select("B5");
        e.type_text("draft");
        {
            let ed = e.app.editor.as_mut().expect("editing B5");
            ed.caret = 3;
            ed.sync_caret = true;
        }
        e.frames(2);
        assert_eq!(e.editor(), Some(("B5".into(), "draft".into(), 3)));
        assert_eq!(draft_cells(of_type(&e.posted, "draft").last().unwrap()), ["Sheet1!B5"]);
        assert_eq!(e.sync.pending.len(), 1);
        e
    }

    #[test]
    fn a_snapshot_keeps_the_open_editor_and_resends_the_unconfirmed_edit() {
        let mut e = typing_with_an_unconfirmed_edit();
        let first = of_type(&e.posted, "mutation")[0].to_string();
        let undo = e.undo_len();
        let n = e.posted.len();
        // Somebody else's C1 is in the new head; our B3 is not (the host never had it).
        e.host(snapshot(3, &[("B2", "10"), ("C1", "theirs")], &[]));
        // (a) the cell editor, its text, caret and the selection are untouched.
        assert_eq!(e.editor(), Some(("B5".into(), "draft".into(), 3)));
        assert_eq!(e.active(), "B5");
        // ...and the host still hears of it (its draft set lives with the session; no change, no message).
        assert!(of_type(&e.since(n), "draft").is_empty(), "{:?}", e.since(n));
        // (b) the edit not in `discarded` is sent again byte for byte and still shown and pending.
        let again = of_type(&e.since(n), "mutation").iter().map(|m| m.to_string()).collect::<Vec<_>>();
        assert_eq!(again, [first]);
        assert_eq!(e.sync.pending.iter().map(|p| p.seq).collect::<Vec<_>>(), [1]);
        assert_eq!((e.input("B3").as_str(), e.input("C1").as_str(), e.input("B2").as_str()), ("1", "theirs", "10"));
        assert_eq!(e.sync.status(), Status::Saving);
        assert_eq!(e.sync.notice, None);
        // (c) the undo history stays.
        assert_eq!(e.undo_len(), undo);

        // Typing goes on at the caret; Enter commits the draft against the snapshot's revision
        // (the snapshot did not change B5) and the editor closes with an empty draft.
        e.type_text("X");
        e.keys(Key::Enter);
        assert_eq!(e.input("B5"), "draXft");
        let m = of_type(&e.since(n), "mutation");
        assert_eq!(m.len(), 2);
        assert_eq!((m[1]["seq"].as_u64(), m[1]["base_rev"].as_u64(), m[1]["ops"][0]["input"].as_str()), (Some(2), Some(3), Some("draXft")));
        assert_eq!(draft_cells(of_type(&e.posted, "draft").last().unwrap()), Vec::<String>::new());
        // Both acks: saved at the host's head.
        e.host(json!({"type": "ack", "protocol": 1, "seq": 1, "rev": 4}));
        e.host(json!({"type": "ack", "protocol": 1, "seq": 2, "rev": 5}));
        assert_eq!((e.sync.status(), e.sync.confirmed_rev), (Status::Saved, 5));
        // Undo takes back the last local edit only: the other writer's C1 stays.
        e.app.session.run("edit.undo", json!({})).unwrap();
        e.frames(2);
        assert_eq!((e.input("B5").as_str(), e.input("B3").as_str(), e.input("C1").as_str()), ("", "1", "theirs"));
        let last = of_type(&e.posted, "mutation").last().cloned().unwrap();
        assert_eq!((last["seq"].as_u64(), last["ops"][0]["cell"].as_str(), last["ops"][0]["input"].as_str()), (Some(3), Some("B5"), Some("")));
    }

    #[test]
    fn a_snapshot_that_changes_the_edited_cell_keeps_the_draft() {
        let mut e = typing_with_an_unconfirmed_edit();
        let n = e.posted.len();
        e.host(snapshot(3, &[("B2", "10"), ("B5", "theirs")], &[]));
        assert_eq!(e.editor(), Some(("B5".into(), "draft".into(), 3)));
        assert!(of_type(&e.since(n), "draft").is_empty(), "the host still has B5 as a draft");
        e.keys(Key::Enter);
        // The precondition is still "unchanged since I started typing" (rev 1): the host refuses
        // it as a conflict with the other writer's B5 instead of overwriting it silently.
        let m = of_type(&e.since(n), "mutation");
        let mine = m.iter().find(|m| m["ops"][0]["cell"] == "B5").unwrap();
        assert_eq!((mine["base_rev"].as_u64(), mine["ops"][0]["input"].as_str()), (Some(1), Some("draft")));
        assert_eq!(e.input("B5"), "draft");
    }

    #[test]
    fn a_snapshot_drops_exactly_the_discarded_edits() {
        let mut e = typing_with_an_unconfirmed_edit();
        // A second unconfirmed edit (seq 2) after the cell editor is closed.
        e.keys(Key::Escape);
        e.set("B4", "2");
        let sent: Vec<String> = of_type(&e.posted, "mutation").iter().map(|m| m.to_string()).collect();
        assert_eq!(sent.len(), 2);
        let n = e.posted.len();
        e.host(snapshot(3, &[("B2", "10"), ("B3", "host")], &[1]));
        // seq 1 is gone: not sent again, the host's value is shown, and the person is told.
        let again: Vec<String> = of_type(&e.since(n), "mutation").iter().map(|m| m.to_string()).collect();
        assert_eq!(again, [sent[1].clone()]);
        assert_eq!(e.sync.pending.iter().map(|p| p.seq).collect::<Vec<_>>(), [2]);
        assert_eq!((e.input("B3").as_str(), e.input("B4").as_str()), ("host", "2"));
        assert_eq!(e.sync.discarded_notice().as_deref(), Some("the workbook was reloaded: 1 unsaved change(s) discarded: Sheet1!B3"));

        // A late answer for the dropped edit changes nothing.
        e.host(json!({"type": "ack", "protocol": 1, "seq": 1, "rev": 4}));
        assert_eq!(e.sync.pending.len(), 1);
        e.host(json!({"type": "ack", "protocol": 1, "seq": 2, "rev": 4}));
        assert_eq!((e.sync.status(), e.input("B3").as_str(), e.input("B4").as_str()), (Status::Saved, "host", "2"));
        // The notice stays through later edits until the person dismisses it.
        e.set("B6", "6");
        assert!(e.sync.discarded_notice().is_some());
        e.sync.dismiss_discarded();
        assert_eq!(e.sync.discarded_notice(), None);
    }

    #[test]
    fn a_snapshot_of_another_shape_replaces_the_document_but_keeps_editor_and_edits() {
        let mut e = typing_with_an_unconfirmed_edit();
        let n = e.posted.len();
        // Another sheet layout (not reachable through cell ops): the document is replaced and its
        // undo history with it, the open editor and the unconfirmed edit are not.
        let mut wb = workbook(&[("B2", "10")]);
        let mut second = wb["sheets"][0].clone();
        second["name"] = json!("Sheet2");
        wb["sheets"].as_array_mut().unwrap().push(second);
        e.host(json!({"type": "snapshot", "protocol": 1, "rev": 3, "generation": 1, "snapshot": wb, "discarded": []}));
        assert_eq!(e.app.session.active().unwrap().wb.sheets.len(), 2);
        assert_eq!(e.editor(), Some(("B5".into(), "draft".into(), 3)));
        assert_eq!(e.input("B3"), "1");
        assert_eq!(of_type(&e.since(n), "mutation").len(), 1);
        assert_eq!(e.undo_len(), 0);
    }

    // Review G2-F7: R1 (typing start vs. a snapshot), L1 (a discarded cell looks like the host's)
    // and the paths checked without findings (undo of a discarded edit, two snapshots in a row,
    // a multi-cell edit across a snapshot).

    fn committed(rev: u64, cell: &str, input: &str) -> Json {
        json!({"type": "committed", "protocol": 1, "rev": rev, "actor_label": "Other", "ops": [{"op": "cell.set", "sheet": "Sheet1", "cell": cell, "input": input}]})
    }

    /// Another writer changes the edited cell while the person types (a `committed`), then a
    /// snapshot that does not change it again arrives. The commit must still be "unchanged since I
    /// started typing" (base below the other writer's rev 2), not "since the snapshot".
    #[test]
    fn a_commit_by_another_writer_then_a_snapshot_keeps_the_typing_start() {
        let mut e = Editor::new();
        e.host(snapshot(1, &[("B2", "10")], &[]));
        e.select("B5");
        e.type_text("mine");
        assert!(e.app.editor.is_some());
        e.host(committed(2, "B5", "theirs"));
        e.host(snapshot(3, &[("B2", "10"), ("B5", "theirs")], &[]));
        assert_eq!(e.editor().map(|x| x.1), Some("mine".to_string()));
        let n = e.posted.len();
        e.keys(Key::Enter);
        let m = of_type(&e.since(n), "mutation");
        assert_eq!(m.len(), 1, "{m:?}");
        assert!(
            m[0]["base_rev"].as_u64().unwrap() < 2,
            "base_rev {} lets the server overwrite the other writer's B5 (rev 2) without a conflict",
            m[0]["base_rev"]
        );
    }

    /// The same without the snapshot: base_rev is the typing start (1).
    #[test]
    fn a_commit_by_another_writer_keeps_the_typing_start() {
        let mut e = Editor::new();
        e.host(snapshot(1, &[("B2", "10")], &[]));
        e.select("B5");
        e.type_text("mine");
        e.host(committed(2, "B5", "theirs"));
        let n = e.posted.len();
        e.keys(Key::Enter);
        let m = of_type(&e.since(n), "mutation");
        assert_eq!(m[0]["base_rev"].as_u64(), Some(1));
    }

    /// Undo of an edit the host discarded writes nothing over the host's value.
    #[test]
    fn undo_after_a_discarded_edit_writes_nothing() {
        let mut e = Editor::new();
        e.host(snapshot(1, &[("B2", "10")], &[]));
        e.set("B3", "1");
        e.host(snapshot(3, &[("B2", "10"), ("B3", "host")], &[1]));
        assert_eq!(e.input("B3"), "host");
        let n = e.posted.len();
        e.app.session.run("edit.undo", json!({})).unwrap();
        e.frames(2);
        assert_eq!(e.input("B3"), "host");
        assert!(of_type(&e.since(n), "mutation").is_empty());
    }

    /// Two snapshots in one frame: the first keeps seq 1 (sent again), the second discards it.
    #[test]
    fn two_snapshots_in_a_row_keep_then_drop() {
        let mut e = Editor::new();
        e.host(snapshot(1, &[("B2", "10")], &[]));
        e.set("B3", "1");
        let n = e.posted.len();
        e.frame(vec![], vec![snapshot(3, &[("B2", "10")], &[]), snapshot(4, &[("B2", "10"), ("B3", "host")], &[1])]);
        e.frames(2);
        assert_eq!(of_type(&e.since(n), "mutation").len(), 1);
        assert!(e.sync.pending.is_empty());
        assert_eq!(e.input("B3"), "host");
        assert!(e.sync.discarded_notice().unwrap_or_default().contains("discarded: Sheet1!B3"), "{:?}", e.sync.discarded_notice());
        assert_eq!(e.sync.status(), Status::Saved);
        // A late ack for the dropped seq changes nothing.
        e.host(json!({"type": "ack", "protocol": 1, "seq": 1, "rev": 5}));
        assert_eq!(e.input("B3"), "host");
    }

    /// A multi-cell edit (fill down, 3 cells, one mutation) survives a snapshot, is sent again
    /// byte for byte, and undo afterwards takes back exactly its cells.
    #[test]
    fn a_multi_cell_edit_survives_a_snapshot_and_undoes() {
        let mut e = Editor::new();
        e.host(snapshot(1, &[("A1", "1")], &[]));
        e.app.session.run("selection.set", json!({"range": "A1:A4"})).unwrap();
        e.frames(1);
        e.app.session.run("edit.fillDown", json!({})).unwrap();
        e.frames(2);
        let sent = of_type(&e.posted, "mutation");
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["ops"].as_array().unwrap().len(), 3);
        let n = e.posted.len();
        e.host(snapshot(3, &[("A1", "1"), ("C1", "theirs")], &[]));
        let again = of_type(&e.since(n), "mutation");
        assert_eq!(again.iter().map(Json::to_string).collect::<Vec<_>>(), [sent[0].to_string()]);
        assert_eq!((e.input("A2").as_str(), e.input("A4").as_str(), e.input("C1").as_str()), ("1", "1", "theirs"));
        let n = e.posted.len();
        e.app.session.run("edit.undo", json!({})).unwrap();
        e.frames(2);
        let undo = of_type(&e.since(n), "mutation");
        assert_eq!(undo.len(), 1);
        assert_eq!(undo[0]["ops"].as_array().unwrap().len(), 3);
        assert_eq!((e.input("A2").as_str(), e.input("C1").as_str()), ("", "theirs"));
    }

    /// After a discarded edit of a formatted input, the rebased cell looks like the host's
    /// (not only the same input): compare the cell JSON with a fresh load of the same snapshot.
    #[test]
    fn a_discarded_percent_shows_the_hosts_cell() {
        let mut e = Editor::new();
        e.host(snapshot(1, &[("B2", "10")], &[]));
        e.set("B3", "50%");
        let snap = snapshot(3, &[("B2", "10"), ("B3", "5")], &[1]);
        let fresh = protocol::workbook_from_snapshot(&snap["snapshot"]).unwrap();
        e.host(snap);
        let d = e.app.session.active().unwrap();
        let cell_json = |wb: &Workbook| serde_json::to_value(wb).unwrap()["sheets"][0]["cells"].to_string();
        assert_eq!(cell_json(&d.wb), cell_json(&fresh), "the editor's B3 differs from the host's snapshot B3");
        assert_eq!(e.input("B3"), "5");
    }

    /// A kept (resent) formatted edit keeps its own look, and the reload keeps the undo history:
    /// only cells outside the editor's unconfirmed ones are compared.
    #[test]
    fn a_kept_percent_keeps_its_format_and_the_history() {
        let mut e = Editor::new();
        e.host(snapshot(1, &[("B2", "10")], &[]));
        e.set("B3", "50%");
        let undo = e.undo_len();
        let style = |e: &Editor| e.app.session.active().unwrap().wb.sheet(0).unwrap().cells.iter().find(|(c, _)| c.a1() == "B3").unwrap().1.style;
        let (before, input) = (style(&e), e.input("B3"));
        e.host(snapshot(3, &[("B2", "10"), ("C1", "theirs")], &[]));
        assert_eq!((e.input("B3"), e.input("C1").as_str()), (input, "theirs"));
        assert_eq!(e.undo_len(), undo);
        assert_ne!(style(&e), gridcraft_engine::model::StyleId::DEFAULT);
        assert_eq!(style(&e), before);
    }

    /// A snapshot that hides a row (not expressible as a cell op) replaces the document: the
    /// editor shows the host's row, not its own.
    #[test]
    fn a_snapshot_with_a_hidden_row_replaces_the_document() {
        let mut e = Editor::new();
        e.host(snapshot(1, &[("B2", "10")], &[]));
        e.set("B3", "1");
        let mut s = snapshot(3, &[("B2", "10")], &[]);
        let wb = &mut s["snapshot"];
        let mut sheet: gridcraft_engine::model::Sheet = serde_json::from_value(wb["sheets"][0].clone()).unwrap();
        sheet.rows.entry(3).or_default().hidden = true;
        wb["sheets"][0] = serde_json::to_value(&sheet).unwrap();
        e.host(s);
        let d = e.app.session.active().unwrap();
        assert!(d.wb.sheet(0).unwrap().rows.get(&3).is_some_and(|l| l.hidden), "row 4 hidden as the host has it");
        assert_eq!(e.input("B3"), "1", "the unconfirmed edit stays");
        assert_eq!(e.undo_len(), 0);
    }
}

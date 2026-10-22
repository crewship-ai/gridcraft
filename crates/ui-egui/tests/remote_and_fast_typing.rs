//! Remote host ops keep the open cell editor and the selection; keystrokes that arrive in one
//! frame (a slow frame on a software renderer, or a fast typist) are all delivered, in order.
use egui::{Context, Event, Key, Modifiers, RawInput, Rect, pos2, vec2};
use gridcraft_engine::Session;
use gridcraft_engine::core::CellRef;
use gridcraft_engine::remote::RemoteOp;
use gridcraft_ui_egui::SheetApp;

/// One frame the way the web and native runners drive it: the app's raw input hook, then the UI.
fn frame_with(ctx: &Context, app: &mut SheetApp, events: Vec<Event>) {
    let mut raw = RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1200.0, 800.0))), events, ..Default::default() };
    app.raw_input_hook(&mut raw);
    let mut out = ctx.run_ui(raw, |ui| {
        let ctx = ui.ctx().clone();
        app.logic(&ctx);
        app.ui(ui);
    });
    out.textures_delta.clear();
}

fn frames(ctx: &Context, app: &mut SheetApp, n: usize) {
    for _ in 0..n {
        frame_with(ctx, app, vec![]);
    }
}

fn app() -> (Context, SheetApp) {
    let ctx = Context::default();
    let mut session = Session::new();
    session.new_workbook();
    let mut app = SheetApp::new(session, Default::default());
    frames(&ctx, &mut app, 4);
    (ctx, app)
}

fn key(k: Key) -> [Event; 2] {
    let ev = |pressed| Event::Key { key: k, physical_key: Some(k), pressed, repeat: false, modifiers: Modifiers::NONE };
    [ev(true), ev(false)]
}

fn typed(s: &str) -> Vec<Event> {
    s.chars().map(|c| Event::Text(c.to_string())).collect()
}

fn input(app: &SheetApp, a1: &str) -> String {
    let d = app.session.active().unwrap();
    d.wb.sheet(0).unwrap().input_text(CellRef::parse(a1).unwrap())
}

fn active(app: &SheetApp) -> String {
    app.session.active().unwrap().selection.active.a1()
}

#[test]
fn remote_op_keeps_open_editor_caret_selection_and_undo() {
    let (ctx, mut app) = app();
    app.session.run("cell.set", serde_json::json!({"cell": "B3", "input": "21"})).unwrap();
    app.session.run("selection.set", serde_json::json!({"cell": "B5"})).unwrap();
    frames(&ctx, &mut app, 2);
    frame_with(&ctx, &mut app, key(Key::F2).to_vec());
    frame_with(&ctx, &mut app, typed("draft"));
    frames(&ctx, &mut app, 2);
    {
        let ed = app.editor.as_mut().expect("editing B5");
        assert_eq!(ed.text, "draft");
        ed.caret = 3;
        ed.sync_caret = true;
    }
    frames(&ctx, &mut app, 2);
    let undo_len = app.session.active().unwrap().undo.len();

    app.session.apply_remote(3, &[RemoteOp::set("Sheet1", "B2", "99").unwrap()]).unwrap();
    app.after_engine();
    frames(&ctx, &mut app, 3);

    assert_eq!(input(&app, "B2"), "99");
    let ed = app.editor.as_ref().expect("the editor survives a remote op");
    assert_eq!((ed.text.as_str(), ed.caret, ed.cell.a1()), ("draft", 3, "B5".to_string()));
    assert_eq!(active(&app), "B5");
    assert_eq!(app.session.active().unwrap().undo.len(), undo_len);

    // Typing goes on at the caret, and undoing the local edit leaves the remote op.
    frame_with(&ctx, &mut app, typed("X"));
    frame_with(&ctx, &mut app, key(Key::Enter).to_vec());
    frames(&ctx, &mut app, 2);
    assert_eq!(input(&app, "B5"), "draXft");
    app.session.run("edit.undo", serde_json::json!({})).unwrap();
    assert_eq!((input(&app, "B5").as_str(), input(&app, "B2").as_str()), ("", "99"));
}

#[test]
fn a_word_typed_within_one_frame_is_not_lost() {
    let (ctx, mut app) = app();
    // Everything between two slow frames arrives as one batch.
    let mut batch = typed("draft");
    batch.extend(key(Key::Enter));
    batch.extend(typed("next"));
    batch.extend(key(Key::Tab));
    frame_with(&ctx, &mut app, batch);
    frames(&ctx, &mut app, 6);
    assert_eq!(input(&app, "A1"), "draft");
    assert_eq!(input(&app, "A2"), "next");
    assert_eq!(active(&app), "B2");
}

#[test]
fn f2_then_typing_in_the_same_frame_appends() {
    let (ctx, mut app) = app();
    app.session.run("cell.set", serde_json::json!({"cell": "A1", "input": "ab"})).unwrap();
    frames(&ctx, &mut app, 2);
    let mut batch = key(Key::F2).to_vec();
    batch.extend(typed("cd"));
    batch.extend(key(Key::Enter));
    frame_with(&ctx, &mut app, batch);
    frames(&ctx, &mut app, 6);
    assert_eq!(input(&app, "A1"), "abcd");
    assert_eq!(active(&app), "A2");
}

#[test]
fn typing_at_full_speed_one_key_per_frame_still_works() {
    let (ctx, mut app) = app();
    for e in typed("full") {
        frame_with(&ctx, &mut app, vec![e]);
    }
    frame_with(&ctx, &mut app, key(Key::Enter).to_vec());
    frames(&ctx, &mut app, 2);
    assert_eq!(input(&app, "A1"), "full");
    assert_eq!(active(&app), "A2");
}

#[test]
fn remote_op_on_the_cell_being_edited_keeps_the_draft_and_redo() {
    let (ctx, mut app) = app();
    app.session.run("cell.set", serde_json::json!({"cell": "C1", "input": "mine"})).unwrap();
    app.session.run("edit.undo", serde_json::json!({})).unwrap();
    app.session.run("selection.set", serde_json::json!({"cell": "B2"})).unwrap();
    frames(&ctx, &mut app, 2);
    frame_with(&ctx, &mut app, typed("500"));
    frames(&ctx, &mut app, 2);
    let redo_len = app.session.active().unwrap().redo.len();
    assert_eq!(redo_len, 1);

    // Somebody else commits the same cell while it is open in the editor.
    app.session.apply_remote(2, &[RemoteOp::set("Sheet1", "B2", "7").unwrap()]).unwrap();
    app.after_engine();
    frames(&ctx, &mut app, 3);

    assert_eq!(input(&app, "B2"), "7", "the document shows the committed value underneath");
    let ed = app.editor.as_ref().expect("the editor stays open");
    assert_eq!((ed.text.as_str(), ed.cell.a1()), ("500", "B2".to_string()), "the draft is the user's until they commit or cancel");
    assert_eq!(active(&app), "B2");
    assert_eq!(app.session.active().unwrap().redo.len(), redo_len, "a remote op keeps the redo stack");

    // Escape leaves the committed value; redo brings back only the local edit.
    frame_with(&ctx, &mut app, key(Key::Escape).to_vec());
    frames(&ctx, &mut app, 2);
    assert!(app.editor.is_none());
    assert_eq!(input(&app, "B2"), "7");
    app.session.run("edit.redo", serde_json::json!({})).unwrap();
    assert_eq!((input(&app, "C1").as_str(), input(&app, "B2").as_str()), ("mine", "7"));
}

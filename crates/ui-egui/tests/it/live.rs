//! Live co-editing in the window: a WordCraft window joins a room through the in-memory relay
//! (`wordcraft-live`'s `test-util`) next to Bob, a second editor with no window.

use egui::accesskit::Role;
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use serde_json::json;
use wordcraft_doc::Document;
use wordcraft_engine::Session;
use wordcraft_live::LiveEditing;
use wordcraft_live::testing::{MemHub, MemRelay, ROOM};
use wordcraft_ui_egui::live::Access;
use wordcraft_ui_egui::{Services, WordApp};

fn file() -> Vec<u8> {
    wordcraft_engine::io::save_bytes("plan.docx", &Document::from_text("Hello world\nSecond")).unwrap()
}

/// Bob: a session joined to the room, with no window.
struct Bob {
    s: Session,
    le: LiveEditing<MemHub>,
}

impl Bob {
    fn join(relay: &MemRelay) -> Bob {
        let mut s = Session::new(Document::new());
        let le = LiveEditing::start(relay.connect("bob", "Bob", true), ROOM, &file(), "plan.docx", &mut s).unwrap();
        Bob { s, le }
    }

    fn text(&self) -> String {
        self.s.doc.body.iter().filter_map(|b| b.as_para()).map(|p| p.text.clone()).collect::<Vec<_>>().join("\n")
    }
}

/// Alice's window, joined to the room with `access` over a connection that may edit or not.
fn alice(relay: &MemRelay, access: Access, may_edit: bool) -> (Harness<'static, WordApp>, MemHub) {
    let mut app = WordApp::new(Session::new(Document::new()), Services::default());
    app.ui.language = "en".into();
    let hub = relay.connect("alice", "Alice", may_edit);
    app.start_live(Box::new(hub.clone()), ROOM, &file(), "plan.docx", access).unwrap();
    let h = Harness::builder().with_size(egui::vec2(1440.0, 900.0)).build_ui_state(
        |ui, app: &mut WordApp| {
            let ctx = ui.ctx().clone();
            app.logic(&ctx);
            app.ui(ui);
        },
        app,
    );
    (h, hub)
}

/// Run Alice's frames and Bob's pump until the relay has nothing left to deliver.
fn settle(relay: &MemRelay, h: &mut Harness<'static, WordApp>, bob: &mut Bob) {
    for _ in 0..40 {
        h.step();
        bob.le.pump(&mut bob.s);
        if relay.quiet() {
            break;
        }
    }
    for _ in 0..3 {
        h.step();
    }
}

fn alice_text(h: &mut Harness<'static, WordApp>) -> String {
    h.state().session.doc.body.iter().filter_map(|b| b.as_para()).map(|p| p.text.clone()).collect::<Vec<_>>().join("\n")
}

/// Typing in Alice's window, as the keyboard delivers it.
fn type_text(h: &mut Harness<'static, WordApp>, text: &str) {
    h.event(egui::Event::Text(text.into()));
    h.step();
    h.step();
}

fn has_label(h: &Harness<'static, WordApp>, label: &str) -> bool {
    h.query_all_by_label(label).next().is_some()
}

#[test]
fn remote_caret_is_drawn_with_name() {
    let relay = MemRelay::new();
    let mut bob = Bob::join(&relay);
    let (mut h, _hub) = alice(&relay, Access::Edit, true);
    settle(&relay, &mut h, &mut bob);
    assert!(!has_label(&h, "Bob's cursor"), "no caret before Bob has put one anywhere");
    let first = bob.s.doc.body[0].as_para().unwrap().pid;
    bob.le.send_cursor(first, 5, None);
    bob.le.pump(&mut bob.s);
    settle(&relay, &mut h, &mut bob);
    let caret = h.get_by_role_and_label(Role::Caret, "Bob's cursor");
    let bounds = caret.accesskit_node().bounding_box().expect("the caret is placed on screen");
    assert!(bounds.height() > 0.0, "it spans a line");
    assert!(has_label(&h, "Bob"), "the presence bar names Bob");
    // And the other way: Alice's caret moves, and Bob gets it once the caret limit (five a
    // second, on the real clock) lets it out.
    h.state_mut().session.sel = wordcraft_engine::Selection::caret(wordcraft_doc::Pos::body(1, 3));
    let second = bob.s.doc.body[1].as_para().unwrap().pid;
    let mut seen = None;
    for _ in 0..100 {
        settle(&relay, &mut h, &mut bob);
        seen = bob.le.pump(&mut bob.s).into_iter().find(|p| p.display == "Alice").and_then(|p| p.caret);
        if seen == Some((second, 3)) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(seen, Some((second, 3)), "Alice's caret reached Bob");
}

#[test]
fn read_ticket_blocks_typing() {
    let relay = MemRelay::new();
    let mut bob = Bob::join(&relay);
    let (mut h, _hub) = alice(&relay, Access::Read, false);
    settle(&relay, &mut h, &mut bob);
    assert!(has_label(&h, "Read-only"), "the status line says the file is read-only");
    let (before, bob_before) = (alice_text(&mut h), bob.text());
    type_text(&mut h, "X");
    assert!(h.state_mut().run("text.insert", json!({"text": "Y"})).is_err(), "the command is refused too");
    settle(&relay, &mut h, &mut bob);
    assert_eq!(alice_text(&mut h), before, "nothing was typed into the read-only document");
    assert_eq!(bob.text(), bob_before, "and nothing reached the room");
    // It still follows the editors.
    bob.s.sel = wordcraft_engine::Selection::caret(wordcraft_doc::Pos::body(0, 0));
    bob.le.run(&mut bob.s, "text.insert", &json!({"text": "Bob: "})).unwrap();
    settle(&relay, &mut h, &mut bob);
    assert_ne!(alice_text(&mut h), before, "Bob's edit reached the read-only window");
}

#[test]
fn status_line_says_reconnecting_when_hub_drops() {
    let relay = MemRelay::new();
    let mut bob = Bob::join(&relay);
    let (mut h, hub) = alice(&relay, Access::Edit, true);
    settle(&relay, &mut h, &mut bob);
    assert!(!has_label(&h, "Reconnecting…"), "connected: not reconnecting");
    relay.drop_link(&hub);
    for _ in 0..3 {
        h.step();
    }
    assert!(has_label(&h, "Reconnecting…"), "the status line says it is reconnecting");
    relay.restore_link(&hub);
    settle(&relay, &mut h, &mut bob);
    assert!(!has_label(&h, "Reconnecting…"), "and stops once the link is back");
}

#[test]
fn non_shareable_command_is_disabled_while_live() {
    let relay = MemRelay::new();
    let mut bob = Bob::join(&relay);
    let (mut h, _hub) = alice(&relay, Access::Edit, true);
    settle(&relay, &mut h, &mut bob);
    let app = h.state();
    assert!(wordcraft_ui_egui::widgets::enabled(app, "format.bold"), "a shared edit stays enabled");
    assert!(!wordcraft_ui_egui::widgets::enabled(app, "insert.bookmark"), "a bookmark can't be shared: greyed out");
    assert!(!wordcraft_ui_egui::widgets::enabled(app, "edit.paste"), "nor can a paste");
    assert!(!wordcraft_ui_egui::widgets::enabled(app, "file.save"), "the document service saves, not this window");
    let bold = h.get_by_role_and_label(Role::Button, "Bold");
    assert!(!bold.accesskit_node().is_disabled(), "the ribbon's Bold button is enabled");
    let before = h.state().session.doc.clone();
    let refused = h.state_mut().run("insert.bookmark", json!({"name": "here"}));
    assert!(refused.is_err(), "running it anyway is refused");
    assert!(h.state().session.doc.same_content(&before), "and changes nothing");
}

#[test]
fn broken_status_blocks_typing() {
    let relay = MemRelay::new();
    let mut bob = Bob::join(&relay);
    // An edit ticket the relay treats as read-only: the first edit breaks the link.
    let (mut h, _hub) = alice(&relay, Access::Edit, false);
    settle(&relay, &mut h, &mut bob);
    let before = alice_text(&mut h);
    type_text(&mut h, "X");
    assert_ne!(alice_text(&mut h), before, "typing reaches the document while live");
    settle(&relay, &mut h, &mut bob);
    assert!(has_label(&h, "Live editing ended. Reopen the file."), "the status line says live editing ended");
    let broken = alice_text(&mut h);
    assert!(h.state_mut().run("text.insert", json!({"text": "Z"})).is_err(), "the window refuses the edit itself");
    type_text(&mut h, "Y");
    settle(&relay, &mut h, &mut bob);
    assert_eq!(alice_text(&mut h), broken, "typing is blocked once the link is broken");
}

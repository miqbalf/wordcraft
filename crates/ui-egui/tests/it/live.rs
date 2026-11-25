//! Live co-editing in the window: a WordCraft window joins a room through the in-memory relay
//! (`wordcraft-live`'s `test-util`) next to Bob, a second editor with no window.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use craft_collab::{ClientMsg, Hub};
use egui::accesskit::Role;
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use serde_json::{Value, json};
use wordcraft_doc::Document;
use wordcraft_engine::Session;
use wordcraft_live::LiveEditing;
use wordcraft_live::testing::{MemHub, MemRelay, ROOM};
use wordcraft_ui_egui::live::{Access, COPY_DOWNLOADED, ENDED, ENDED_UNSAVED, LiveReport, LiveStatus, NEWER, NOT_SAVING, SAVED_FOR_YOU};
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
        Bob::join_as(relay, "bob", "Bob")
    }

    /// Another editor, or with `craft_office` the document service that saves the file.
    fn join_as(relay: &MemRelay, uid: &str, display: &str) -> Bob {
        let mut s = Session::new(Document::new());
        let le = LiveEditing::start(relay.connect(uid, display, true), ROOM, &file(), "plan.docx", &mut s).unwrap();
        Bob { s, le }
    }

    fn text(&self) -> String {
        self.s.doc.body.iter().filter_map(|b| b.as_para()).map(|p| p.text.clone()).collect::<Vec<_>>().join("\n")
    }
}

/// Alice's window, joined to the room with `access` over a connection that may edit or not.
fn alice(relay: &MemRelay, access: Access, may_edit: bool) -> (Harness<'static, WordApp>, MemHub) {
    alice_with(relay, access, may_edit, Services::default())
}

/// [`alice`] with the host's `services`.
fn alice_with(relay: &MemRelay, access: Access, may_edit: bool, services: Services) -> (Harness<'static, WordApp>, MemHub) {
    alice_set_up(relay, access, may_edit, services, |_| {})
}

/// [`alice_with`], with `set_up` run on the linked window before its first frame.
fn alice_set_up(
    relay: &MemRelay,
    access: Access,
    may_edit: bool,
    services: Services,
    set_up: impl FnOnce(&mut WordApp),
) -> (Harness<'static, WordApp>, MemHub) {
    let mut app = WordApp::new(Session::new(Document::new()), services);
    app.ui.language = "en".into();
    let hub = relay.connect("alice", "Alice", may_edit);
    app.start_live(Box::new(hub.clone()), ROOM, &file(), "plan.docx", access).unwrap();
    set_up(&mut app);
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
    settle_all(relay, h, &mut [bob]);
}

/// [`settle`] with several editors besides Alice.
fn settle_all(relay: &MemRelay, h: &mut Harness<'static, WordApp>, others: &mut [&mut Bob]) {
    for _ in 0..40 {
        h.step();
        for o in others.iter_mut() {
            o.le.pump(&mut o.s);
        }
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

/// The host's `live-state` reports, as they are made.
fn recorded_reports() -> (Services, Rc<RefCell<Vec<LiveReport>>>) {
    let (services, reports, _) = recorded();
    (services, reports)
}

/// The host's `live-state` reports and its unsaved-changes flags (the leave guard), as made.
fn recorded() -> (Services, Rc<RefCell<Vec<LiveReport>>>, Rc<RefCell<Vec<bool>>>) {
    let reports = Rc::new(RefCell::new(Vec::new()));
    let dirty = Rc::new(RefCell::new(Vec::new()));
    let (seen, guard) = (Rc::clone(&reports), Rc::clone(&dirty));
    let services = Services {
        live_state: Some(Box::new(move |r: &LiveReport| seen.borrow_mut().push(r.clone()))),
        on_dirty: Some(Box::new(move |d| guard.borrow_mut().push(d))),
        ..Services::default()
    };
    (services, reports, dirty)
}

fn status(h: &Harness<'static, WordApp>) -> LiveStatus {
    h.state().live_link().map(|l| l.status().clone()).unwrap()
}

#[test]
fn live_edits_no_save_holds_yet_keep_the_leave_guard_on() {
    let relay = MemRelay::new();
    let mut bob = Bob::join(&relay);
    let mut service = Bob::join_as(&relay, "craft_office", "Craft Office");
    let (services, _, guard) = recorded();
    let (mut h, _hub) = alice_with(&relay, Access::Edit, true, services);
    settle_all(&relay, &mut h, &mut [&mut bob, &mut service]);
    assert_eq!(guard.borrow().last(), Some(&false), "nothing unsaved after joining");
    type_text(&mut h, "X");
    settle_all(&relay, &mut h, &mut [&mut bob, &mut service]);
    assert!(alice_text(&mut h).contains('X'), "the edit was made");
    assert!(h.state().session.dirty, "no save holds it yet");
    assert_eq!(guard.borrow().last(), Some(&true), "leaving now would lose it: the guard is on while live");
    // The service saves through the edit: this window's copy is clean again.
    service.le.announce_saved(relay.seq(), "etag-1").unwrap();
    settle_all(&relay, &mut h, &mut [&mut bob, &mut service]);
    assert!(matches!(status(&h), LiveStatus::Live { saved_at: Some(_) }));
    assert!(!h.state().session.dirty, "the save holds this window's edits");
    assert_eq!(guard.borrow().last(), Some(&false), "and the guard is off again");
}

#[test]
fn an_edit_after_the_saved_checkpoint_stays_unsaved() {
    let relay = MemRelay::new();
    let mut bob = Bob::join(&relay);
    let mut service = Bob::join_as(&relay, "craft_office", "Craft Office");
    let (mut h, _hub) = alice(&relay, Access::Edit, true);
    settle_all(&relay, &mut h, &mut [&mut bob, &mut service]);
    // The service snapshots the room here, then Alice types before its save is announced.
    let checkpoint = relay.seq();
    type_text(&mut h, "X");
    settle_all(&relay, &mut h, &mut [&mut bob, &mut service]);
    assert!(relay.seq() > checkpoint, "the edit was ordered after the checkpoint");
    service.le.announce_saved(checkpoint, "etag-1").unwrap();
    settle_all(&relay, &mut h, &mut [&mut bob, &mut service]);
    assert!(matches!(status(&h), LiveStatus::Live { saved_at: Some(_) }), "the save was heard");
    assert!(h.state().session.dirty, "but it does not hold the edit");
    service.le.announce_saved(relay.seq(), "etag-2").unwrap();
    settle_all(&relay, &mut h, &mut [&mut bob, &mut service]);
    assert!(!h.state().session.dirty, "the next save does");
}

#[test]
fn broken_link_with_an_unsaved_edit_keeps_it_for_a_copy() {
    let relay = MemRelay::new();
    let mut bob = Bob::join(&relay);
    let (services, _, guard) = recorded();
    // An edit ticket the relay treats as read-only: the first edit breaks the link.
    let (mut h, _hub) = alice_with(&relay, Access::Edit, false, services);
    settle(&relay, &mut h, &mut bob);
    let before = alice_text(&mut h);
    type_text(&mut h, "X");
    assert_ne!(alice_text(&mut h), before, "typing reaches the document while live");
    settle(&relay, &mut h, &mut bob);
    assert!(matches!(status(&h), LiveStatus::Broken(_)));
    assert!(has_label(&h, ENDED_UNSAVED), "the status line says to download a copy");
    assert_eq!(guard.borrow().last(), Some(&true), "the leave guard is on");
    assert!(wordcraft_ui_egui::widgets::enabled(h.state(), "file.saveAs"), "Save As keeps the edit");
    assert!(!wordcraft_ui_egui::widgets::enabled(h.state(), "file.save"), "but nothing saves over the shared file");
    assert!(h.state_mut().run("file.save", json!({})).is_err());
    let broken = alice_text(&mut h);
    assert!(h.state_mut().run("text.insert", json!({"text": "Z"})).is_err(), "the window refuses the edit itself");
    type_text(&mut h, "Y");
    settle(&relay, &mut h, &mut bob);
    assert_eq!(alice_text(&mut h), broken, "typing is blocked once the link is broken");
}

#[test]
fn broken_link_with_every_edit_saved_shows_saved() {
    let relay = MemRelay::new();
    let mut bob = Bob::join(&relay);
    let mut service = Bob::join_as(&relay, "craft_office", "Craft Office");
    let (services, _, guard) = recorded();
    let (mut h, hub) = alice_with(&relay, Access::Edit, true, services);
    settle_all(&relay, &mut h, &mut [&mut bob, &mut service]);
    type_text(&mut h, "X");
    settle_all(&relay, &mut h, &mut [&mut bob, &mut service]);
    service.le.announce_saved(relay.seq(), "etag-1").unwrap();
    settle_all(&relay, &mut h, &mut [&mut bob, &mut service]);
    relay.fall_behind(&hub);
    settle_all(&relay, &mut h, &mut [&mut bob, &mut service]);
    assert!(matches!(status(&h), LiveStatus::Broken(_)));
    assert!(!h.state().session.dirty, "the save holds every edit");
    assert!(has_label(&h, ENDED), "reopening loses nothing");
    assert_eq!(guard.borrow().last(), Some(&false), "the leave guard stays off");
}

#[test]
fn the_host_hears_each_state_at_once_and_a_seconds_changes_once() {
    let relay = MemRelay::new();
    let mut bob = Bob::join(&relay);
    let (services, reports) = recorded_reports();
    let now = Rc::new(Cell::new(0.0));
    let clock = Rc::clone(&now);
    // The report clock is the test's from the first frame on.
    let (mut h, hub) = alice_set_up(&relay, Access::Edit, true, services, |app| app.live_set_clock(Box::new(move || clock.get())));
    settle(&relay, &mut h, &mut bob);
    assert_eq!(reports.borrow().last().map(|r| r.state), Some("live"));
    assert!(reports.borrow().last().is_some_and(|r| r.peers.iter().any(|(_, name)| name == "Bob")), "with Bob");
    let n = reports.borrow().len();
    for _ in 0..20 {
        h.step();
    }
    assert_eq!(reports.borrow().len(), n, "nothing changed: nothing told");
    // Two roster changes within the second: held, then told once.
    now.set(100.0);
    let mut carol = Bob::join_as(&relay, "carol", "Carol");
    settle_all(&relay, &mut h, &mut [&mut bob, &mut carol]);
    now.set(500.0);
    let mut dave = Bob::join_as(&relay, "dave", "Dave");
    settle_all(&relay, &mut h, &mut [&mut bob, &mut carol, &mut dave]);
    assert_eq!(reports.borrow().len(), n, "held within the second");
    now.set(1100.0);
    h.step();
    assert_eq!(reports.borrow().len(), n + 1, "one report for both");
    let names: Vec<String> = reports.borrow().last().map(|r| r.peers.iter().map(|(_, name)| name.clone()).collect()).unwrap_or_default();
    assert!(names.contains(&"Carol".to_string()) && names.contains(&"Dave".to_string()), "{names:?}");
    // A new state goes out at once, however soon after the last report.
    relay.drop_link(&hub);
    h.step();
    assert_eq!(reports.borrow().len(), n + 2);
    assert_eq!(reports.borrow().last().map(|r| r.state), Some("reconnecting"));
}

#[test]
fn a_fresh_ticket_goes_to_the_hub() {
    let relay = MemRelay::new();
    let mut bob = Bob::join(&relay);
    let (mut h, hub) = alice(&relay, Access::Edit, true);
    settle(&relay, &mut h, &mut bob);
    assert!(relay.tokens(&hub).is_empty());
    assert!(h.state_mut().live_refresh_ticket("ticket-2".into()), "a link takes it");
    assert_eq!(relay.tokens(&hub), vec!["ticket-2".to_string()], "the hub has it for its next connect");
}

#[test]
fn a_refused_connection_says_why_until_a_fresh_ticket_joins() {
    let relay = MemRelay::new();
    let mut bob = Bob::join(&relay);
    let (services, reports) = recorded_reports();
    let (mut h, hub) = alice_with(&relay, Access::Edit, true, services);
    settle(&relay, &mut h, &mut bob);
    relay.refuse(&hub, "auth", "Your access to this file expired.");
    settle(&relay, &mut h, &mut bob);
    assert!(has_label(&h, "Your access to this file expired."), "the status line says why");
    let last = reports.borrow().last().cloned().unwrap();
    assert_eq!((last.state, last.reason.as_deref()), ("refused", Some("Your access to this file expired.")), "and so does the host");
    assert!(h.state_mut().run("text.insert", json!({"text": "Z"})).is_err(), "no edits meanwhile");
    // The page's refresh: the hub connects again with the new ticket and the relay joins it.
    assert!(h.state_mut().live_refresh_ticket("ticket-2".into()));
    relay.restore_link(&hub);
    settle(&relay, &mut h, &mut bob);
    assert!(has_label(&h, "Live"), "live again");
    assert_eq!(reports.borrow().last().map(|r| r.state), Some("live"));
}

#[test]
fn the_save_refusal_stays_on_the_status_bar() {
    let relay = MemRelay::new();
    let mut bob = Bob::join(&relay);
    let (mut h, _hub) = alice(&relay, Access::Edit, true);
    settle(&relay, &mut h, &mut bob);
    h.state_mut().status("an ordinary message");
    let ordinary = h.state().status_msg.as_ref().map(|(_, at)| *at).unwrap();
    assert_eq!(h.state_mut().run("file.save", json!({})), Err(SAVED_FOR_YOU.to_string()));
    let (text, at) = h.state().status_msg.clone().unwrap();
    assert_eq!(text, SAVED_FOR_YOU);
    assert!(at - ordinary > 1000.0, "it is a long message, not the usual six seconds");
}

#[test]
fn a_newer_room_refuses_every_edit() {
    let mut app = WordApp::new(Session::new(Document::from_text("Hello")), Services::default());
    app.ui.language = "en".into();
    app.live_newer();
    let mut h = Harness::builder().with_size(egui::vec2(1440.0, 900.0)).build_ui_state(
        |ui, app: &mut WordApp| {
            let ctx = ui.ctx().clone();
            app.logic(&ctx);
            app.ui(ui);
        },
        app,
    );
    h.step();
    assert!(has_label(&h, NEWER), "the status line says to update");
    assert!(h.state_mut().run("text.insert", json!({"text": "Z"})).is_err());
    assert!(!wordcraft_ui_egui::widgets::enabled(h.state(), "format.bold"), "edits are greyed out");
}

#[test]
fn save_as_after_the_link_broke_downloads_a_copy_and_stays_unsaved() {
    let relay = MemRelay::new();
    let mut bob = Bob::join(&relay);
    let (mut services, _, guard) = recorded();
    let copies = Rc::new(RefCell::new(Vec::<(String, usize)>::new()));
    let hosted = Rc::new(Cell::new(0));
    let downloads = Rc::new(Cell::new(0));
    let (c, hs, d) = (Rc::clone(&copies), Rc::clone(&hosted), Rc::clone(&downloads));
    services.download_copy = Some(Box::new(move |name: &str, _mime: &str, bytes: &[u8]| {
        c.borrow_mut().push((name.to_owned(), bytes.len()));
        Ok(())
    }));
    // A framing page (`?host=parent`) that would get `wordcraft:save`, and the app's own save
    // download: neither may be used for the copy.
    services.host_save = Some(Box::new(move |_: &str, _: &[u8], _: bool| {
        hs.set(hs.get() + 1);
        Ok(())
    }));
    services.download = Some(Box::new(move |_: &str, _: &[u8]| {
        d.set(d.get() + 1);
        Ok(())
    }));
    // An edit ticket the relay treats as read-only: the first edit breaks the link.
    let (mut h, _hub) = alice_with(&relay, Access::Edit, false, services);
    settle(&relay, &mut h, &mut bob);
    type_text(&mut h, "X");
    settle(&relay, &mut h, &mut bob);
    assert!(matches!(status(&h), LiveStatus::Broken(_)));
    assert!(h.state().session.dirty, "the edit is in no save");
    // F12 opens the Save As list like everywhere else; nothing downloads yet.
    assert_eq!(h.state_mut().run("file.saveAs", json!({})).map(|r| r["open"].clone()), Ok(json!("saveAs")));
    assert!(copies.borrow().is_empty(), "the list opens, no copy yet");
    // Each format of the list makes the "my copy" download through the one Save As function.
    for (id, ext) in [("docx", ".docx"), ("pdf", ".pdf")] {
        let r = h.state_mut().save_as_copy(id);
        assert!(r.is_ok(), "{id}: {r:?}");
        let (name, bytes) = copies.borrow().last().cloned().unwrap();
        assert!(name.starts_with("plan (my copy ") && name.ends_with(&format!("){ext}")), "{name}");
        assert!(bytes > 0);
    }
    assert_eq!(copies.borrow().len(), 2, "one copy each");
    assert_eq!((hosted.get(), downloads.get()), (0, 0), "no wordcraft:save to the page, no save download");
    assert_eq!(h.state().status_msg.as_ref().map(|(m, _)| m.as_str()), Some(COPY_DOWNLOADED));
    for _ in 0..3 {
        h.step();
    }
    assert!(h.state().session.dirty, "a download can't be confirmed: still unsaved");
    assert_eq!(guard.borrow().last(), Some(&true), "and the leave guard stays on");
}

/// The window's link has ended; a control client asks for a copy of another open document.
/// That copy keeps its own document's name, not the window's "(my copy …)" one.
#[test]
fn a_background_documents_copy_keeps_its_own_name_after_the_link_broke() {
    let relay = MemRelay::new();
    let mut bob = Bob::join(&relay);
    let (mut services, _, _guard) = recorded();
    let copies = Rc::new(RefCell::new(Vec::<String>::new()));
    let c = Rc::clone(&copies);
    services.download_copy = Some(Box::new(move |name: &str, _mime: &str, _bytes: &[u8]| {
        c.borrow_mut().push(name.to_owned());
        Ok(())
    }));
    let (mut h, _hub) = alice_with(&relay, Access::Edit, false, services);
    settle(&relay, &mut h, &mut bob);
    type_text(&mut h, "X");
    settle(&relay, &mut h, &mut bob);
    assert!(matches!(status(&h), LiveStatus::Broken(_)));
    let live = h.state().session.document_id();
    let other = h.state_mut().session.add_document(wordcraft_doc::Document::from_text("Other"));
    h.state_mut().session.switch_document(live).unwrap();
    let ctx = egui::Context::default();
    let (req, _reply) = wordcraft_ui_egui::ControlRequest::new("engine.execute", json!({"command": "file.saveAs", "params": {}, "doc": other}));
    match wordcraft_ui_egui::control::handle(h.state_mut(), &ctx, &req) {
        wordcraft_ui_egui::control::Outcome::Done(v) => assert_eq!(v["ok"], true, "{v}"),
        wordcraft_ui_egui::control::Outcome::Screenshot { .. } => panic!("not a reply"),
    }
    let name = copies.borrow().last().cloned().unwrap();
    assert!(!name.contains("my copy") && name.ends_with(".docx"), "{name}");
    assert_ne!(h.state().status_msg.as_ref().map(|(m, _)| m.as_str()), Some(COPY_DOWNLOADED));
    assert_eq!(h.state().session.document_id(), live, "the window keeps its document");
    // The window's own document still gets its "my copy".
    h.state_mut().save_as_copy("docx").unwrap();
    let name = copies.borrow().last().cloned().unwrap();
    assert!(name.starts_with("plan (my copy "), "{name}");
}

/// The document service in the room sends the editors `kind` with `payload`.
fn service_says(service: &mut Bob, kind: &str, payload: Value) {
    service.le.hub_mut().send(ClientMsg::Ephemeral { doc: ROOM.into(), kind: kind.into(), payload }).unwrap();
}

#[test]
fn the_service_ending_the_session_ends_the_link_and_allows_a_copy() {
    let relay = MemRelay::new();
    let mut bob = Bob::join(&relay);
    let mut service = Bob::join_as(&relay, "craft_office", "Craft Office");
    let (services, reports) = recorded_reports();
    let (mut h, _hub) = alice_with(&relay, Access::Edit, true, services);
    settle_all(&relay, &mut h, &mut [&mut bob, &mut service]);
    type_text(&mut h, "X");
    settle_all(&relay, &mut h, &mut [&mut bob, &mut service]);
    assert!(wordcraft_ui_egui::widgets::enabled(h.state(), "file.saveAs"), "Save As is a copy: it works while the service saves");
    let reason = "Saving has stopped: your changes are no longer being saved to the file. Download a copy to keep your work.";
    service_says(&mut service, "ended", json!({ "reason": reason }));
    settle_all(&relay, &mut h, &mut [&mut bob, &mut service]);
    assert_eq!(status(&h), LiveStatus::Broken(reason.into()));
    assert!(wordcraft_ui_egui::widgets::enabled(h.state(), "file.saveAs"), "Save As downloads a copy now");
    assert!(has_label(&h, ENDED_UNSAVED), "the status bar says to keep a copy, not that it is saved");
    let last = reports.borrow().last().cloned().unwrap();
    assert_eq!((last.state, last.reason.as_deref()), ("broken", Some(reason)), "the host hears it ended, apart from a refusal");
}

#[test]
fn a_service_notice_reaches_the_status_bar() {
    let relay = MemRelay::new();
    let mut service = Bob::join_as(&relay, "craft_office", "Craft Office");
    let (mut h, _hub) = alice(&relay, Access::Edit, true);
    settle(&relay, &mut h, &mut service);
    let text = "Someone changed this file outside the live session. Their version was kept as Plan (conflict 2026-11-21 1403).docx.";
    service_says(&mut service, "notice", json!({ "text": text }));
    settle(&relay, &mut h, &mut service);
    assert_eq!(h.state().status_msg.as_ref().map(|(m, _)| m.as_str()), Some(text));
    assert!(matches!(status(&h), LiveStatus::Live { .. }), "a notice does not end the link");
}

#[test]
fn the_host_ending_the_link_allows_a_copy() {
    let relay = MemRelay::new();
    let mut bob = Bob::join(&relay);
    let (services, reports) = recorded_reports();
    let (mut h, _hub) = alice_with(&relay, Access::Edit, true, services);
    settle(&relay, &mut h, &mut bob);
    // The page sent another room (the session was restarted) or a ticket with less access.
    let reason = "The live session was restarted. Reopen the file.";
    assert!(h.state_mut().live_end(reason));
    settle(&relay, &mut h, &mut bob);
    assert_eq!(status(&h), LiveStatus::Broken(reason.into()));
    assert!(wordcraft_ui_egui::widgets::enabled(h.state(), "file.saveAs"));
    assert!(h.state_mut().run("text.insert", json!({"text": "Z"})).is_err(), "no edits once ended");
    assert_eq!(reports.borrow().last().map(|r| r.state), Some("broken"));
}

#[test]
fn a_page_that_keeps_no_saves_gets_a_download_and_the_document_stays_unsaved() {
    // `?host=parent`: the framing page stores nothing WordCraft saves, so Save is a download.
    let copies = Rc::new(RefCell::new(Vec::<String>::new()));
    let posted = Rc::new(Cell::new(0));
    let (c, p, q) = (Rc::clone(&copies), Rc::clone(&posted), Rc::clone(&posted));
    let services = Services {
        saves_are_copies: true,
        download_copy: Some(Box::new(move |name: &str, _: &str, _: &[u8]| {
            c.borrow_mut().push(name.to_owned());
            Ok(())
        })),
        download: Some(Box::new(move |_: &str, _: &[u8]| {
            p.set(p.get() + 1);
            Ok(())
        })),
        host_save: Some(Box::new(move |_: &str, _: &[u8], _: bool| {
            q.set(q.get() + 1);
            Ok(())
        })),
        ..Services::default()
    };
    let mut session = Session::new(Document::from_text("Hello"));
    session.path = Some("plan.docx".into());
    let mut app = WordApp::new(session, services);
    app.ui.language = "en".into();
    assert!(app.run("text.insert", json!({"text": "X"})).is_ok());
    assert!(app.session.dirty);
    for (id, params) in [("file.save", json!({})), ("file.saveAs", json!({"path": "plan.odt"})), ("file.exportPdf", json!({}))] {
        let r = app.run(id, params);
        assert!(r.is_ok(), "{id}: {r:?}");
    }
    assert_eq!(copies.borrow().len(), 3, "each one downloads: {:?}", copies.borrow());
    assert_eq!(posted.get(), 0, "nothing goes to the page or the save picker");
    assert!(app.session.dirty, "a download can't be confirmed: the document stays unsaved");
}

#[test]
fn a_room_with_no_document_service_says_nothing_is_saved_and_allows_a_copy() {
    // The worker never joined (refused at its cap, gave up, or could not start), or it left.
    let relay = MemRelay::new();
    let mut bob = Bob::join(&relay);
    let (mut h, _hub) = alice(&relay, Access::Edit, true);
    let now = std::sync::Arc::new(std::sync::Mutex::new(0.0_f64));
    let clock = std::sync::Arc::clone(&now);
    h.state_mut().live_set_session_clock(Box::new(move || *clock.lock().unwrap()));
    settle(&relay, &mut h, &mut bob);
    assert!(matches!(status(&h), LiveStatus::Live { .. }));
    assert!(!has_label(&h, NOT_SAVING), "the service may still be on its way");
    assert!(wordcraft_ui_egui::widgets::enabled(h.state(), "file.saveAs"), "Save As is a copy: always allowed");
    *now.lock().unwrap() = 16_000.0;
    settle(&relay, &mut h, &mut bob);
    assert!(wordcraft_ui_egui::widgets::enabled(h.state(), "file.saveAs"), "nothing saves the edits: a copy keeps them");
    assert!(has_label(&h, NOT_SAVING), "the status bar says the changes are not being saved");
    // The service joins: it saves again.
    let service_hub = relay.connect("craft_office", "Craft Office", true);
    let mut s = Session::new(Document::new());
    let le = LiveEditing::start(service_hub.clone(), ROOM, &file(), "plan.docx", &mut s).unwrap();
    let mut service = Bob { s, le };
    settle_all(&relay, &mut h, &mut [&mut bob, &mut service]);
    assert!(wordcraft_ui_egui::widgets::enabled(h.state(), "file.saveAs"));
    assert!(!has_label(&h, NOT_SAVING));
    // It leaves: counted again from then.
    relay.drop_link(&service_hub);
    *now.lock().unwrap() = 20_000.0;
    settle(&relay, &mut h, &mut bob);
    assert!(!has_label(&h, NOT_SAVING), "within the wait");
    *now.lock().unwrap() = 36_000.0;
    settle(&relay, &mut h, &mut bob);
    assert!(wordcraft_ui_egui::widgets::enabled(h.state(), "file.saveAs"));
    assert!(has_label(&h, NOT_SAVING));
}

/// A hosted frame (`?host=parent`, a page that stores saves): its copies, its saves to the page
/// and its save-picker downloads, as made.
fn hosted() -> (Services, Rc<RefCell<Vec<String>>>, Rc<Cell<usize>>) {
    let copies = Rc::new(RefCell::new(Vec::<String>::new()));
    let other = Rc::new(Cell::new(0));
    let (c, p, d) = (Rc::clone(&copies), Rc::clone(&other), Rc::clone(&other));
    let services = Services {
        hosted: true,
        download_copy: Some(Box::new(move |name: &str, _: &str, _: &[u8]| {
            c.borrow_mut().push(name.to_owned());
            Ok(())
        })),
        host_save: Some(Box::new(move |_: &str, _: &[u8], _: bool| {
            p.set(p.get() + 1);
            Ok(())
        })),
        download: Some(Box::new(move |_: &str, _: &[u8]| {
            d.set(d.get() + 1);
            Ok(())
        })),
        pick_save: Some(Box::new(|name: &str| Some(name.to_owned()))),
        ..Services::default()
    };
    (services, copies, other)
}

#[test]
fn hosted_copies_download_in_their_format_while_live_and_after_the_link_ends() {
    let relay = MemRelay::new();
    let mut bob = Bob::join(&relay);
    let mut service = Bob::join_as(&relay, "craft_office", "Craft Office");
    let (services, copies, other) = hosted();
    let (mut h, _hub) = alice_with(&relay, Access::Edit, true, services);
    settle_all(&relay, &mut h, &mut [&mut bob, &mut service]);
    assert!(matches!(status(&h), LiveStatus::Live { .. }), "the service saves the file");
    assert!(wordcraft_ui_egui::widgets::enabled(h.state(), "file.saveAs"), "a copy writes nothing shared");
    for (id, params) in [("file.saveAs", json!({"path": "plan.pdf"})), ("file.exportPng", json!({}))] {
        let r = h.state_mut().run(id, params);
        assert!(r.is_ok(), "{id}: {r:?}");
    }
    // Save As without a format opens the Save As list, with no refusal.
    assert_eq!(h.state_mut().run("file.saveAs", json!({})).map(|r| r["open"].clone()), Ok(json!("saveAs")));
    assert_eq!(h.state_mut().run("file.save", json!({})), Err(SAVED_FOR_YOU.to_string()), "Save stays the service's");
    assert_eq!(*copies.borrow(), ["plan.pdf".to_string(), "plan.png".to_string()]);
    assert!(matches!(status(&h), LiveStatus::Live { .. }), "copies don't touch the link");

    assert!(h.state_mut().live_end("The live session was restarted. Reopen the file."));
    settle_all(&relay, &mut h, &mut [&mut bob, &mut service]);
    h.state_mut().run("file.saveAs", json!({"path": "plan.odt"})).unwrap();
    assert_eq!(copies.borrow().last().map(String::as_str), Some("plan.odt"), "the chosen format, not a .docx");
    // F12 still opens the list; a format picked from it downloads a "my copy" in that format.
    assert_eq!(h.state_mut().run("file.saveAs", json!({})).map(|r| r["open"].clone()), Ok(json!("saveAs")));
    h.state_mut().save_as_copy("pdf").unwrap();
    let last = copies.borrow().last().cloned().unwrap();
    assert!(last.starts_with("plan (my copy ") && last.ends_with(").pdf"), "{last}");
    assert_eq!(other.get(), 0, "nothing went to the page or the save picker");
}

/// Live, framed by the page (hosted) or a plain browser page, the Save As page lists every
/// format with no refusal, and a click downloads that copy; nothing goes to the page.
#[test]
fn hosted_save_as_page_while_live_offers_the_export_formats_and_no_refusal() {
    for framed in [true, false] {
        save_as_page_while_live(framed);
    }
}

fn save_as_page_while_live(framed: bool) {
    let relay = MemRelay::new();
    let mut bob = Bob::join(&relay);
    let mut service = Bob::join_as(&relay, "craft_office", "Craft Office");
    let (mut services, copies, other) = hosted();
    services.hosted = framed;
    if !framed {
        services.host_save = None;
    }
    let (mut h, _hub) = alice_with(&relay, Access::Edit, true, services);
    settle_all(&relay, &mut h, &mut [&mut bob, &mut service]);
    h.state_mut().run("ui.backstage", json!({"value": true, "page": "saveAs"})).unwrap();
    h.step();
    h.step();
    assert!(matches!(status(&h), LiveStatus::Live { .. }), "the service saves the file");
    assert!(!has_label(&h, SAVED_FOR_YOU), "no refusal on the page");
    assert!(!has_label(&h, "Browse…"));
    for label in ["Word document (*.docx)", "OpenDocument Text (*.odt)", "PDF document (*.pdf)", "Word template (*.dotx)"] {
        assert!(has_label(&h, label), "{label} is offered");
    }
    h.get_by_label("OpenDocument Text (*.odt)").click();
    h.step();
    h.step();
    assert_eq!(*copies.borrow(), ["plan.odt".to_string()], "the chosen format downloads");
    assert_eq!(other.get(), 0);
}

/// Hosted and live, another document shown in the window ends the link for the page's file.
/// Shown again, that file still belongs to the room: Save stays refused (no plain save to a page
/// in live mode), and a copy still downloads.
#[test]
fn hosted_live_file_shown_again_after_another_document_still_refuses_save() {
    let relay = MemRelay::new();
    let mut bob = Bob::join(&relay);
    let mut service = Bob::join_as(&relay, "craft_office", "Craft Office");
    let (services, copies, other) = hosted();
    let (mut h, _hub) = alice_with(&relay, Access::Edit, true, services);
    settle_all(&relay, &mut h, &mut [&mut bob, &mut service]);
    let live = h.state().session.document_id();
    h.state_mut().session.add_document(Document::from_text("Letters"));
    settle_all(&relay, &mut h, &mut [&mut bob, &mut service]);
    assert!(h.state().live_link().is_none(), "the link stopped");
    h.state_mut().session.switch_document(live).unwrap();
    h.step();
    assert!(h.state_mut().run("file.save", json!({})).is_err(), "Save stays refused for the room's file");
    assert_eq!(other.get(), 0, "nothing went to the page");
    h.state_mut().run("file.saveAs", json!({"path": "plan.odt"})).unwrap();
    assert_eq!(copies.borrow().last().map(String::as_str), Some("plan.odt"), "a copy still downloads");
}

/// A hosted frame whose saves to the page are counted.
fn counting_host() -> (Services, Rc<Cell<usize>>) {
    let posted = Rc::new(Cell::new(0));
    let p = Rc::clone(&posted);
    let services = Services {
        hosted: true,
        host_save: Some(Box::new(move |_: &str, _: &[u8], _: bool| {
            p.set(p.get() + 1);
            Ok(())
        })),
        ..Default::default()
    };
    (services, posted)
}

const LOCKED_TIP: &str = "Saved to Nextcloud automatically";
const HOSTED_TIP: &str = "Save (Ctrl+S) stores this file in Nextcloud";

/// Hosted and live (a .docx in Nextcloud, a saver in the room): the AutoSave switch shows on and
/// locked with the service's tooltip, and clicking it adds no save path: nothing is posted to
/// the page, and nothing changes.
#[test]
fn hosted_live_autosave_shows_on_and_locked() {
    let relay = MemRelay::new();
    let mut bob = Bob::join(&relay);
    let (services, posted) = counting_host();
    let (mut h, _hub) = alice_with(&relay, Access::Edit, true, services);
    settle(&relay, &mut h, &mut bob);
    let app = h.state();
    assert!(app.saved_by_service(), "the live worker saves the file");
    assert_eq!(app.autosave_tip(), LOCKED_TIP);
    assert_eq!(app.autosave_block(), Some(wordcraft_ui_egui::AutoSaveBlock::Hosted));
    assert!(!app.autosaves(), "this window writes no copy of its own");
    h.get_by_label("AutoSave").click();
    h.run_steps(3);
    assert!(h.state().saved_by_service(), "locked: a click changes nothing");
    assert!(!h.state().autosaves());
    assert_eq!(posted.get(), 0, "no save message reached the page");
    assert_eq!(h.state().autosave_tip(), LOCKED_TIP);
}

/// The lock is the service saving, not just a link: read-only, broken and saver-less live
/// sessions fall back to the Hosted reason, and a live room in a window that is not hosted never
/// says "Nextcloud".
#[test]
fn live_but_not_saved_by_the_service_is_not_locked_on() {
    // Read-only ticket.
    let relay = MemRelay::new();
    let mut bob = Bob::join(&relay);
    let (services, posted) = counting_host();
    let (mut h, _hub) = alice_with(&relay, Access::Read, false, services);
    settle(&relay, &mut h, &mut bob);
    assert_eq!(status(&h), LiveStatus::ReadOnly);
    assert!(!h.state().saved_by_service(), "read-only");
    assert_eq!(h.state().autosave_tip(), HOSTED_TIP);
    h.get_by_label("AutoSave").click();
    h.run_steps(3);
    assert_eq!(posted.get(), 0);
    assert_eq!(h.state().autosave_tip(), HOSTED_TIP);

    // A broken link (an edit ticket the relay treats as read-only).
    let relay = MemRelay::new();
    let mut bob = Bob::join(&relay);
    let (services, _) = counting_host();
    let (mut h, _hub) = alice_with(&relay, Access::Edit, false, services);
    settle(&relay, &mut h, &mut bob);
    type_text(&mut h, "X");
    settle(&relay, &mut h, &mut bob);
    assert!(matches!(status(&h), LiveStatus::Broken(_)));
    assert!(!h.state().saved_by_service(), "broken");
    assert_eq!(h.state().autosave_tip(), HOSTED_TIP);

    // No document service in the room for 15 s.
    let relay = MemRelay::new();
    let mut bob = Bob::join(&relay);
    let (services, _) = counting_host();
    let (mut h, _hub) = alice_with(&relay, Access::Edit, true, services);
    let now = std::sync::Arc::new(std::sync::Mutex::new(0.0_f64));
    let clock = std::sync::Arc::clone(&now);
    h.state_mut().live_set_session_clock(Box::new(move || *clock.lock().unwrap()));
    settle(&relay, &mut h, &mut bob);
    assert!(h.state().saved_by_service(), "the service may still be on its way");
    *now.lock().unwrap() = 16_000.0;
    settle(&relay, &mut h, &mut bob);
    assert!(!h.state().saved_by_service(), "no saver");
    assert_eq!(h.state().autosave_tip(), HOSTED_TIP);

    // Live but not hosted: never the Nextcloud text.
    let relay = MemRelay::new();
    let mut bob = Bob::join(&relay);
    let (mut h, _hub) = alice(&relay, Access::Edit, true);
    settle(&relay, &mut h, &mut bob);
    assert!(!h.state().saved_by_service(), "not hosted");
    assert!(!h.state().autosave_tip().contains("Nextcloud"), "{}", h.state().autosave_tip());
}

/// A live file that is not a .docx is not locked on either.
#[test]
fn hosted_live_non_docx_is_not_locked_on() {
    let relay = MemRelay::new();
    let mut bob = Bob::join(&relay);
    let (services, _) = counting_host();
    let (mut h, _hub) = alice_with(&relay, Access::Edit, true, services);
    settle(&relay, &mut h, &mut bob);
    h.state_mut().session.path = Some("plan.odt".into());
    assert!(!h.state().saved_by_service());
    assert_eq!(h.state().autosave_tip(), HOSTED_TIP);
}

#[test]
fn a_window_its_hub_wakes_pumps_on_a_slow_safety_tick_and_still_takes_edits_in() {
    let relay = MemRelay::new();
    let mut bob = Bob::join(&relay);
    let (mut h, _hub) = alice(&relay, Access::Edit, true);
    assert_eq!(h.state().live_pump_ms(), Some(100), "a polled hub: a frame every 100 ms");
    h.state_mut().live_woken();
    assert_eq!(h.state().live_pump_ms(), Some(1000), "a waking hub: a safety tick of at most 1 s");
    settle(&relay, &mut h, &mut bob);
    bob.s.sel = wordcraft_engine::Selection::caret(wordcraft_doc::Pos::body(0, 0));
    bob.le.run(&mut bob.s, "text.insert", &json!({"text": "Bob "})).unwrap();
    settle(&relay, &mut h, &mut bob);
    assert!(alice_text(&mut h).contains("Bob "), "{}", alice_text(&mut h));
}

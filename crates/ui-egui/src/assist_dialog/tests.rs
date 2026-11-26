//! The Improve and Summarize modal against a fake page that records what it is sent, in a
//! kittest window.

use std::cell::RefCell;
use std::rc::Rc;

use egui::accesskit::Role;
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT as _, Queryable as _};
use serde_json::json;
use wordcraft_doc::{Document, Path, Pos, StoryRef};
use wordcraft_engine::Session;

use super::*;
use crate::Services;
use crate::assist::{AiTask, AssistHost, AssistIn, AssistInbox, AssistOut};

type Sent = Rc<RefCell<Vec<AssistOut>>>;

/// A window whose page offers AI help when `ai`, with the page's messages recorded.
fn app_on(text: &str, ai: bool) -> (WordApp, Sent) {
    let sent: Sent = Rc::default();
    let log = sent.clone();
    let host = AssistHost {
        send: Box::new(move |out| {
            log.borrow_mut().push(out.clone());
            Ok(())
        }),
        inbox: AssistInbox::default(),
    };
    host.inbox.borrow_mut().push(AssistIn::Enabled { proof: false, ai });
    let mut app = WordApp::new(Session::new(Document::from_text(text)), Services { assist: Some(host), ..Services::default() });
    app.ui.language = "en".into();
    app.assist_frame_at(&egui::Context::default(), 0.0);
    (app, sent)
}

fn harness(app: WordApp) -> Harness<'static, WordApp> {
    let mut h = Harness::builder().with_size(egui::vec2(1440.0, 900.0)).build_ui_state(
        |ui, app: &mut WordApp| {
            let ctx = ui.ctx().clone();
            app.logic(&ctx);
            app.ui(ui);
        },
        app,
    );
    h.run_steps(4);
    h
}

/// The AI requests sent so far: id, task and text.
fn asked(sent: &Sent) -> Vec<(String, AiTask, String)> {
    sent.borrow()
        .iter()
        .filter_map(|o| match o {
            AssistOut::Ai { id, task, text, .. } => Some((id.clone(), *task, text.clone())),
            AssistOut::ProofCheck { .. } => None,
        })
        .collect()
}

/// The page answers.
fn page_says(h: &mut Harness<'static, WordApp>, m: AssistIn) {
    if let Some(host) = &h.state().services.assist {
        host.inbox.borrow_mut().push(m);
    }
    h.run_steps(4);
}

fn body(app: &WordApp) -> String {
    app.session.doc.plain_text(StoryRef::Body)
}

fn open(app: &WordApp) -> bool {
    app.assist.dialog.is_some()
}

/// The modal's area on screen.
fn modal_rect(h: &Harness<'static, WordApp>) -> Option<egui::Rect> {
    h.ctx.memory(|m| m.area_rect(egui::Id::unique(MODAL_ID)))
}

/// Nodes inside the modal whose label is exactly `label` (trimmed) and whose role is `role`.
fn in_modal<'h>(h: &'h Harness<'static, WordApp>, label: &'h str, role: Role) -> Vec<egui_kittest::Node<'h>> {
    let Some(r) = modal_rect(h) else { return Vec::new() };
    h.query_all_by_label_contains(label)
        .filter(|n| n.accesskit_node().role() == role && n.accesskit_node().label().is_some_and(|l| l.trim() == label) && r.contains_rect(n.rect()))
        .collect()
}

fn button<'h>(h: &'h Harness<'static, WordApp>, label: &'h str) -> Option<egui_kittest::Node<'h>> {
    in_modal(h, label, Role::Button).into_iter().next()
}

/// Whether text `text` is shown inside the modal (a label).
fn says(h: &Harness<'static, WordApp>, text: &str) -> bool {
    let Some(r) = modal_rect(h) else { return false };
    h.query_all_by_label_contains(text).any(|n| r.contains_rect(n.rect()))
}

/// The value of the read-only box labelled `label`.
fn box_value(h: &Harness<'static, WordApp>, label: &str) -> Option<String> {
    in_modal(h, label, Role::MultilineTextInput).into_iter().next().and_then(|n| n.value())
}

fn menu_has(h: &Harness<'static, WordApp>, label: &str) -> bool {
    h.query_all_by_label_contains(label)
        .any(|n| n.accesskit_node().role() == Role::Button && n.accesskit_node().label().is_some_and(|l| l.trim_end() == label))
}

fn select(app: &mut WordApp, (b0, o0): (usize, usize), (b1, o1): (usize, usize)) {
    let at = |b, off| Pos { story: StoryRef::Body, path: Path::top(b), off };
    app.session.sel = wordcraft_engine::Selection { anchor: at(b0, o0), focus: at(b1, o1) };
}

#[test]
fn right_click_offers_improve_only_when_ai_is_on() {
    let menu = |mut app: WordApp| {
        app.run("select.text", json!({"text": "cat"})).unwrap();
        let mut h = harness(app);
        h.state_mut().run("ui.contextMenu", json!({})).unwrap();
        h.run_steps(2);
        assert!(h.state().canvas.context_menu_open);
        h
    };
    let (on, _) = app_on("The cat sat on the mat.", true);
    let h = menu(on);
    assert!(menu_has(&h, "Improve with AI"), "the page offers AI help");
    h.get_by_label_contains("Improve with AI").click();
    let mut h = h;
    h.run_steps(3);
    assert!(open(h.state()), "the item opens the modal");

    let (off, sent) = app_on("The cat sat on the mat.", false);
    let h = menu(off);
    assert!(!menu_has(&h, "Improve with AI"), "the page turned AI help off");
    assert!(h.query_by_label("Cut").is_some(), "the rest of the menu is there");
    assert!(asked(&sent).is_empty());

    let desktop = WordApp::new(Session::new(Document::from_text("The cat sat on the mat.")), Services::default());
    let h = menu(desktop);
    assert!(!menu_has(&h, "Improve with AI"), "no host page: no AI help");
}

#[test]
fn review_tab_shows_summarize_only_when_ai_is_on() {
    for ai in [true, false] {
        let (mut app, _) = app_on("Some text.", ai);
        app.run("ui.tab", json!({"tab": "Review"})).unwrap();
        let h = harness(app);
        let shown = h.query_all_by_label("Summarize Document").any(|n| n.accesskit_node().role() == Role::Button);
        assert_eq!(shown, ai, "Summarize Document is on the Review tab only when AI help is on (ai: {ai})");
        assert_eq!(crate::widgets::enabled(h.state(), "ui.summarize"), ai);
        assert_eq!(crate::widgets::enabled(h.state(), "ui.improve"), ai);
    }
    let mut desktop = WordApp::new(Session::new(Document::from_text("Some text.")), Services::default());
    assert!(desktop.run("ui.summarize", json!({})).is_err(), "refused without a host page");
    assert!(!open(&desktop));
}

#[test]
fn improve_shows_spinner_then_original_and_proposal_and_replace_applies_once() {
    let (app, sent) = app_on("Teh cat sat.", true);
    let mut h = harness(app);
    h.state_mut().run("ui.improve", json!({})).unwrap();
    h.run_steps(3);
    let asks = asked(&sent);
    assert_eq!(asks.len(), 1, "one request");
    let (id, task, text) = asks[0].clone();
    assert_eq!((task, text.as_str()), (AiTask::Improve, "Teh cat sat."));
    assert!(says(&h, "Improve with AI"), "titled");
    assert!(says(&h, ASKING), "the spinner says it is asking");
    assert_eq!(box_value(&h, "Original").as_deref(), Some("Teh cat sat."));
    assert!(box_value(&h, "Proposal").is_none(), "no proposal yet");

    // A reply wrapped at a fixed width: the box shows it as it will be applied (M2).
    page_says(&mut h, AssistIn::AiResult { id: id.clone(), text: "The cat\nsat.".into() });
    assert!(!says(&h, ASKING), "the spinner goes when the answer comes");
    assert_eq!(box_value(&h, "Original").as_deref(), Some("Teh cat sat."));
    assert_eq!(box_value(&h, "Proposal").as_deref(), Some("The cat sat."));
    assert_eq!(body(h.state()).trim(), "Teh cat sat.", "nothing changes before Replace");

    button(&h, "Replace").expect("Replace").click();
    h.run_steps(3);
    assert!(!open(h.state()), "Replace closes the modal");
    assert_eq!(body(h.state()).trim(), "The cat sat.");
    // The same answer again changes nothing more, and one undo puts the text back.
    page_says(&mut h, AssistIn::AiResult { id, text: "The dog sat.".into() });
    assert_eq!(body(h.state()).trim(), "The cat sat.");
    assert_eq!(asked(&sent).len(), 1);
    h.state_mut().run("edit.undo", json!({})).unwrap();
    assert_eq!(body(h.state()).trim(), "Teh cat sat.", "one undo step");
}

#[test]
fn cancel_then_late_answer_changes_nothing() {
    let (app, sent) = app_on("Teh cat sat.", true);
    let mut h = harness(app);
    h.state_mut().run("ui.improve", json!({})).unwrap();
    h.run_steps(3);
    let (id, _, _) = asked(&sent).remove(0);
    let rev = h.state().session.rev();
    button(&h, "Cancel").expect("Cancel").click();
    h.run_steps(3);
    assert!(!open(h.state()));
    page_says(&mut h, AssistIn::AiResult { id: id.clone(), text: "The cat sat.".into() });
    assert!(!open(h.state()), "a late answer opens nothing");
    assert_eq!(body(h.state()).trim(), "Teh cat sat.");
    assert_eq!(h.state().session.rev(), rev, "the document did not change");
    assert_eq!(h.state_mut().assist_take_answer(&id), None, "and the answer is not kept");
}

#[test]
fn refusal_shows_in_the_modal_and_sends_nothing() {
    let (mut app, sent) = app_on("Some text here", true);
    select(&mut app, (0, 4), (0, 4));
    app.run("references.footnote", json!({"text": "note"})).unwrap();
    select(&mut app, (0, 0), (0, 0));
    app.run("select.all", json!({})).unwrap();
    let mut h = harness(app);
    h.state_mut().run("ui.improve", json!({})).unwrap();
    h.run_steps(3);
    assert!(open(h.state()), "the modal opens with the refusal");
    assert!(says(&h, wordcraft_engine::cmd::review::HAS_OBJECTS), "the refusal is shown");
    assert!(!says(&h, ASKING));
    assert!(asked(&sent).is_empty(), "nothing is sent");
    let replace = button(&h, "Replace");
    assert!(replace.is_none_or(|b| b.accesskit_node().is_disabled()), "nothing to replace");
    assert!(button(&h, "Cancel").expect("Cancel").is_focused(), "the keyboard is in the modal (M3)");
    h.key_press(egui::Key::Escape);
    h.run_steps(3);
    assert!(!open(h.state()));
}

#[test]
fn error_codes_show_their_sentences() {
    let cases = [
        ("busy", BUSY),
        ("unavailable", UNAVAILABLE),
        ("too_large", TOO_LARGE),
        ("disabled", DISABLED),
        ("bad_request", BAD_REQUEST),
        ("something_else", UNAVAILABLE),
    ];
    assert_eq!(BUSY, "Too many requests right now. Try again in a minute.");
    assert_eq!(UNAVAILABLE, "The AI service is not available right now. Try again later.");
    assert_eq!(TOO_LARGE, "This text is too long for the AI service.");
    assert_eq!(DISABLED, "AI writing help is turned off on this server.");
    assert_eq!(BAD_REQUEST, "The AI service could not read this request.");
    for (code, sentence) in cases {
        for command in ["ui.improve", "ui.summarize"] {
            let (app, sent) = app_on("Teh cat sat.", true);
            let mut h = harness(app);
            h.state_mut().run(command, json!({})).unwrap();
            h.run_steps(3);
            let (id, _, _) = asked(&sent).remove(0);
            page_says(&mut h, AssistIn::AiFailed { id, error: code.into() });
            assert!(open(h.state()), "the error stays in the modal");
            assert!(says(&h, sentence), "{command} {code}: {sentence}");
            assert!(!says(&h, ASKING));
            assert_eq!(body(h.state()).trim(), "Teh cat sat.");
        }
    }
}

#[test]
fn summarize_copy_and_insert_at_cursor() {
    let (mut app, sent) = app_on("Intro.\nEnd.", true);
    let paras = app.session.doc.paragraph_count();
    let before = body(&app);
    let mut h = {
        select(&mut app, (0, 0), (0, 0));
        harness(app)
    };
    h.state_mut().run("ui.summarize", json!({})).unwrap();
    h.run_steps(3);
    let (id, task, text) = asked(&sent).remove(0);
    assert_eq!(task, AiTask::Summarize);
    assert!(text.contains("Intro.") && text.contains("End."), "the document is sent");
    assert!(says(&h, "Summary"), "titled");
    assert!(!says(&h, TRUNCATED), "the whole document was sent");
    let summary = "First point.\nSecond point.";
    page_says(&mut h, AssistIn::AiResult { id, text: summary.into() });
    assert_eq!(box_value(&h, "Summary").as_deref(), Some(summary));

    button(&h, "Copy").expect("Copy").click();
    h.step();
    let copied = h.output().platform_output.commands.iter().any(|c| matches!(c, egui::OutputCommand::CopyText(t) if t == summary));
    assert!(copied, "Copy puts the summary on the clipboard");
    h.run_steps(2);
    assert!(h.state().status_msg.as_ref().is_some_and(|(m, _)| m == "Copied"), "{:?}", h.state().status_msg);
    assert!(open(h.state()), "Copy leaves the modal open");
    assert_eq!(body(h.state()), before);

    // Keyboard order: Copy, Insert at Cursor, Close.
    button(&h, "Copy").expect("Copy").focus();
    h.run_steps(2);
    h.key_press(egui::Key::Tab);
    h.run_steps(2);
    assert!(button(&h, "Insert at Cursor").expect("Insert at Cursor").is_focused(), "Tab moves from Copy to Insert at Cursor");
    h.key_press(egui::Key::Tab);
    h.run_steps(2);
    assert!(button(&h, "Close").expect("Close").is_focused(), "then to Close");

    // The caret at the start of "Intro.": the lines go in as whole paragraphs after it (M1).
    button(&h, "Insert at Cursor").expect("Insert at Cursor").click();
    h.run_steps(3);
    assert!(!open(h.state()), "Insert at Cursor closes the modal");
    let doc = &h.state().session.doc;
    assert_eq!(doc.paragraph_count(), paras + 2, "two lines: two paragraphs more");
    let texts: Vec<String> = doc.body.iter().filter_map(|b| b.as_para()).map(|p| p.text.clone()).collect();
    assert_eq!(texts, ["Intro.", "First point.", "Second point.", "End."], "no line glued to the text around the caret");
    h.state_mut().run("edit.undo", json!({})).unwrap();
    assert_eq!(body(h.state()), before, "one undo removes them");

    // A document over the limit was cut: the modal says so.
    let cut = AssistDialog::summary(&json!({"text": "x", "language": "auto", "truncated": true}), "ai-x-1".into());
    assert_eq!(cut.note(), Some(TRUNCATED));
    assert_eq!(TRUNCATED, "Only the first 200 KB of the document was summarized.");
}

#[test]
fn modal_is_keyboard_reachable_and_labelled() {
    let (app, sent) = app_on("Teh cat sat.", true);
    let mut h = harness(app);
    h.state_mut().run("ui.improve", json!({})).unwrap();
    h.run_steps(3);
    let (id, _, _) = asked(&sent).remove(0);
    assert!(h.state().modal_open(), "the rest of the window waits");
    let dialog = h.query_all_by_label("Improve with AI").any(|n| n.accesskit_node().role() == Role::Dialog);
    assert!(dialog, "the modal is a dialog named by its title (M4)");
    assert!(button(&h, "Cancel").expect("Cancel").is_focused(), "while it waits, the keyboard is on Cancel (M3)");
    page_says(&mut h, AssistIn::AiResult { id, text: "The cat sat.".into() });
    // Every control has a name and a role.
    for label in ["Replace", "Cancel"] {
        assert!(button(&h, label).is_some(), "{label} is a labelled button");
    }
    for label in ["Original", "Proposal"] {
        assert!(!in_modal(&h, label, Role::MultilineTextInput).is_empty(), "the {label} box is labelled");
    }
    assert!(button(&h, "Replace").expect("Replace").is_focused(), "focus goes to Replace when the answer comes");
    // Both boxes are read-only.
    in_modal(&h, "Proposal", Role::MultilineTextInput)[0].focus();
    h.run_steps(2);
    h.event(egui::Event::Text("zzz".into()));
    h.run_steps(2);
    assert_eq!(box_value(&h, "Proposal").as_deref(), Some("The cat sat."), "the proposal can't be typed into");
    button(&h, "Replace").expect("Replace").focus();
    h.run_steps(2);
    h.key_press(egui::Key::Tab);
    h.run_steps(2);
    assert!(button(&h, "Cancel").expect("Cancel").is_focused(), "Tab moves from Replace to Cancel");
    h.key_press(egui::Key::Escape);
    h.run_steps(3);
    assert!(!open(h.state()), "Esc is Cancel");
    assert_eq!(body(h.state()).trim(), "Teh cat sat.");
}

/// A co-editor (or the user, through a command) changed the selected text while the AI worked:
/// Replace is refused in the modal and changes nothing.
#[test]
fn a_stale_selection_is_refused_in_the_modal() {
    let (app, sent) = app_on("Teh cat sat.", true);
    let mut h = harness(app);
    h.state_mut().run("ui.improve", json!({})).unwrap();
    h.run_steps(3);
    let (id, _, _) = asked(&sent).remove(0);
    page_says(&mut h, AssistIn::AiResult { id, text: "The cat sat.".into() });
    select(h.state_mut(), (0, 4), (0, 4));
    h.state_mut().session.run("text.insert", &json!({"text": "big "})).unwrap();
    button(&h, "Replace").expect("Replace").click();
    h.run_steps(3);
    assert!(open(h.state()), "the refusal stays in the modal");
    assert!(says(&h, wordcraft_engine::cmd::review::TEXT_CHANGED));
    assert_eq!(body(h.state()).trim(), "Teh big cat sat.", "nothing was replaced");
}

/// M8: an edit above the selection moves it, but the same text is found again.
#[test]
fn an_edit_above_the_selection_does_not_refuse_replace() {
    let (mut app, sent) = app_on("Intro.\nTeh cat sat.", true);
    select(&mut app, (1, 0), (1, 0));
    let mut h = harness(app);
    h.state_mut().run("ui.improve", json!({})).unwrap();
    h.run_steps(3);
    let (id, _, text) = asked(&sent).remove(0);
    assert_eq!(text, "Teh cat sat.");
    page_says(&mut h, AssistIn::AiResult { id, text: "The cat sat.".into() });
    // A new paragraph above moves the selected paragraph down.
    select(h.state_mut(), (0, 0), (0, 0));
    h.state_mut().session.run("text.insert", &json!({"text": "Zero."})).unwrap();
    h.state_mut().session.run("text.newParagraph", &json!({})).unwrap();
    button(&h, "Replace").expect("Replace").click();
    h.run_steps(3);
    assert!(!open(h.state()));
    let texts: Vec<String> = h.state().session.doc.body.iter().filter_map(|b| b.as_para()).map(|p| p.text.clone()).collect();
    assert_eq!(texts, ["Zero.", "Intro.", "The cat sat."]);
}

/// M7: with AI help off, the command (a keytip, a script) says why, on the status bar too.
#[test]
fn ai_off_refusal_is_said() {
    let (mut app, _) = app_on("Some text.", false);
    for id in ["ui.summarize", "ui.improve"] {
        app.status_msg = None;
        let why = app.run(id, json!({})).unwrap_err();
        assert!(!why.is_empty());
        assert_eq!(app.status_msg.as_ref().map(|(m, _)| m.as_str()), Some(why.as_str()), "{id} says it on the status bar");
        assert!(!open(&app));
    }
}

/// M5: once the answer is there, a click beside the modal does not throw it away.
#[test]
fn a_click_beside_a_ready_modal_keeps_the_answer() {
    let (app, sent) = app_on("Some text.", true);
    let mut h = harness(app);
    h.state_mut().run("ui.summarize", json!({})).unwrap();
    h.run_steps(3);
    let (id, _, _) = asked(&sent).remove(0);
    page_says(&mut h, AssistIn::AiResult { id, text: "A point.".into() });
    let r = modal_rect(&h).expect("the modal");
    let beside = egui::pos2(r.min.x - 40.0, r.min.y - 40.0);
    assert!(!r.contains(beside));
    h.event(egui::Event::PointerMoved(beside));
    h.event(egui::Event::PointerButton { pos: beside, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE });
    h.step();
    h.event(egui::Event::PointerButton { pos: beside, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE });
    h.run_steps(3);
    assert!(open(h.state()), "the summary is still there");
    assert_eq!(box_value(&h, "Summary").as_deref(), Some("A point."));
}

/// Fix 2, F3: a new document shown while the modal is open closes it, so the old proposal can
/// never be applied to the new document, and its late answer changes nothing.
#[test]
fn the_modal_closes_when_another_document_is_shown() {
    let (app, sent) = app_on("Teh cat sat.", true);
    let mut h = harness(app);
    h.state_mut().run("ui.improve", json!({})).unwrap();
    h.run_steps(3);
    let (id, _, _) = asked(&sent).remove(0);
    assert!(open(h.state()));
    let first = h.state().session.document_id();
    h.state_mut().run("file.new", json!({})).unwrap();
    h.run_steps(3);
    assert_ne!(h.state().session.document_id(), first, "another document is shown");
    assert!(!open(h.state()), "the modal closed with its document");
    page_says(&mut h, AssistIn::AiResult { id: id.clone(), text: "The cat sat.".into() });
    assert!(!open(h.state()));
    assert_eq!(h.state_mut().assist_take_answer(&id), None, "the answer is not kept");
    assert!(body(h.state()).trim().is_empty(), "nothing went into the new document");
}

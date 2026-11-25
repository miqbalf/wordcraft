//! The pump against a fake page that records what it is sent. `assist_frame_at` takes the time,
//! so no test waits.

use std::cell::RefCell;
use std::rc::Rc;

use serde_json::json;
use wordcraft_engine::Session;
use wordcraft_engine::proof_remote::{BUSY_BACKOFF_MS, IDLE_MS, ProofReply, RemoteKind, RemoteMatch};

use super::*;
use crate::Services;

type Sent = Rc<RefCell<Vec<AssistOut>>>;

fn fake() -> (AssistHost, Sent) {
    let sent: Sent = Rc::default();
    let log = sent.clone();
    let host = AssistHost {
        send: Box::new(move |out| {
            log.borrow_mut().push(out.clone());
            Ok(())
        }),
        inbox: AssistInbox::default(),
    };
    (host, sent)
}

fn app_with(host: Option<AssistHost>, text: &str) -> WordApp {
    let services = Services { assist: host, ..Services::default() };
    let mut app = WordApp::new(Session::new(wordcraft_doc::Document::new()), services);
    app.run("text.insert", json!({"text": text})).expect("insert");
    app
}

fn tell(app: &mut WordApp, m: AssistIn) {
    if let Some(a) = &app.services.assist {
        a.inbox.borrow_mut().push(m);
    }
}

fn frame(app: &mut WordApp, at: f64) {
    let ctx = egui::Context::default();
    app.assist_frame_at(&ctx, at);
}

fn checks(sent: &Sent) -> Vec<(String, String)> {
    sent.borrow()
        .iter()
        .filter_map(|o| match o {
            AssistOut::ProofCheck { id, text, .. } => Some((id.clone(), text.clone())),
            AssistOut::Ai { .. } => None,
        })
        .collect()
}

fn remote_rules(app: &mut WordApp) -> Vec<String> {
    let issues = app.run("review.issues", json!({})).expect("issues");
    issues.as_array().expect("array").iter().filter(|i| i["source"] == "remote").map(|i| i["rule"].as_str().unwrap_or("").to_string()).collect()
}

fn on_word(text: &str, word: &str) -> ProofReply {
    let at = text.find(word).expect("word");
    ProofReply {
        matches: vec![RemoteMatch {
            offset: text[..at].chars().count(),
            length: word.chars().count(),
            kind: RemoteKind::Grammar,
            message: "remote".into(),
            replacements: vec!["fix".into()],
            rule: "R_ONE".into(),
        }],
        ..ProofReply::default()
    }
}

#[test]
fn enabled_proof_sends_a_check_after_idle_and_applies_the_result() {
    let (host, sent) = fake();
    let mut app = app_with(Some(host), "The cat sat on the mat today.");
    tell(&mut app, AssistIn::Enabled { proof: true, ai: false });
    frame(&mut app, 0.0);
    assert!(checks(&sent).is_empty(), "not before the idle wait");
    frame(&mut app, IDLE_MS);
    let sent_now = checks(&sent);
    assert_eq!(sent_now.len(), 1, "one check");
    let (id, text) = sent_now[0].clone();
    assert!(text.contains("mat"));
    assert!(remote_rules(&mut app).is_empty());
    tell(&mut app, AssistIn::ProofResult { id, reply: on_word(&text, "mat") });
    frame(&mut app, IDLE_MS + 100.0);
    assert_eq!(remote_rules(&mut app), vec!["R_ONE"]);
}

#[test]
fn proof_failed_busy_backs_off() {
    let (host, sent) = fake();
    let mut app = app_with(Some(host), "The cat sat on the mat today.");
    tell(&mut app, AssistIn::Enabled { proof: true, ai: false });
    frame(&mut app, 0.0);
    frame(&mut app, IDLE_MS);
    let (id, _) = checks(&sent).remove(0);
    tell(&mut app, AssistIn::ProofFailed { id, error: "busy".into() });
    frame(&mut app, IDLE_MS + 10.0);
    // Changed text waits out the backoff.
    app.run("text.insert", json!({"text": " More."})).expect("insert");
    frame(&mut app, IDLE_MS + 5_000.0);
    frame(&mut app, IDLE_MS + BUSY_BACKOFF_MS - 1_000.0);
    assert_eq!(checks(&sent).len(), 1, "nothing during the backoff");
    frame(&mut app, IDLE_MS + BUSY_BACKOFF_MS + 5_000.0);
    assert_eq!(checks(&sent).len(), 2, "sent again after it");
}

#[test]
fn assist_off_or_desktop_sends_nothing() {
    // Desktop: no host at all, even with the engine's remote source on.
    let mut desktop = app_with(None, "The cat sat on the mat today.");
    desktop.session.proof_remote.set_enabled(true);
    frame(&mut desktop, 0.0);
    frame(&mut desktop, 10_000.0);
    assert!(!desktop.assist_ai_on());
    assert!(desktop.assist_ask(AiTask::Improve, "auto".into(), "Hi.".into()).is_err());

    // A host that never said `proof`, and one that said it is off.
    let (host, sent) = fake();
    let mut app = app_with(Some(host), "The cat sat on the mat today.");
    frame(&mut app, 0.0);
    frame(&mut app, 10_000.0);
    tell(&mut app, AssistIn::Enabled { proof: false, ai: false });
    frame(&mut app, 20_000.0);
    frame(&mut app, 40_000.0);
    assert!(!app.assist_ai_on());
    assert!(app.assist_ask(AiTask::Summarize, "auto".into(), "Hi.".into()).is_err());
    assert!(sent.borrow().is_empty(), "{:?}", sent.borrow());
}

#[test]
fn a_late_ai_answer_for_a_cancelled_request_is_dropped() {
    let (host, sent) = fake();
    let mut app = app_with(Some(host), "x");
    tell(&mut app, AssistIn::Enabled { proof: false, ai: true });
    frame(&mut app, 0.0);
    assert!(app.assist_ai_on());
    let first = app.assist_ask(AiTask::Improve, "auto".into(), "Teh cat.".into()).expect("first");
    let second = app.assist_ask(AiTask::Improve, "auto".into(), "Teh dog.".into()).expect("second");
    assert_ne!(first, second);
    assert_eq!(sent.borrow().len(), 2);
    tell(&mut app, AssistIn::AiResult { id: first.clone(), text: "The cat.".into() });
    frame(&mut app, 100.0);
    assert_eq!(app.assist_take_answer(&first), None, "the cancelled one is dropped");
    assert_eq!(app.assist_take_answer(&second), None, "and does not answer the new one");
    tell(&mut app, AssistIn::AiResult { id: second.clone(), text: "The dog.".into() });
    frame(&mut app, 200.0);
    assert_eq!(app.assist_take_answer(&second), Some(Ok("The dog.".to_string())));
    assert_eq!(app.assist_take_answer(&second), None, "taken once");
    // A failure comes back as its code; a stray answer after it is dropped.
    let third = app.assist_ask(AiTask::Summarize, "auto".into(), "Long.".into()).expect("third");
    tell(&mut app, AssistIn::AiFailed { id: third.clone(), error: "too_large".into() });
    tell(&mut app, AssistIn::AiResult { id: third.clone(), text: "late".into() });
    frame(&mut app, 300.0);
    assert_eq!(app.assist_take_answer(&third), Some(Err("too_large".to_string())));
}

//! The entry points a live co-editing layer uses: the edit observer, the remote apply and the
//! undo hook ([`Session::set_observer`], [`Session::apply_remote`], [`Session::live_undo_hook`]).
use std::sync::{Arc, Mutex};

use serde_json::json;
use wordcraft_doc::{Block, Document, Paragraph, Pos, StoryRef};
use wordcraft_engine::{EditObserver, Selection, Session};

/// One `edited` call: the command and the documents it was given.
#[derive(Clone)]
struct Seen {
    id: String,
    before: Document,
    after: Document,
}

type Calls = Arc<Mutex<Vec<Seen>>>;

/// Records every `edited` call.
struct Log(Calls);

impl EditObserver for Log {
    fn edited(&mut self, before: &Document, after: &Document, command: &str) {
        if let Ok(mut v) = self.0.lock() {
            v.push(Seen { id: command.to_string(), before: before.clone(), after: after.clone() });
        }
    }
}

fn observed(s: &mut Session) -> Calls {
    let log = Arc::new(Mutex::new(Vec::new()));
    s.set_observer(Some(Box::new(Log(log.clone()))));
    log
}

fn calls(log: &Calls) -> Vec<Seen> {
    log.lock().map(|v| v.clone()).unwrap_or_default()
}

fn ids(log: &Calls) -> Vec<String> {
    calls(log).into_iter().map(|c| c.id).collect()
}

/// Gives every body paragraph a shared id, 1, 2, 3… in order.
fn share(s: &mut Session) {
    for (i, path) in s.doc.para_paths(StoryRef::Body).into_iter().enumerate() {
        s.doc.para_mut(StoryRef::Body, &path).unwrap().pid = i as u64 + 1;
    }
}

/// Runs `id` and checks it was reported exactly once, with the real documents before and after.
fn reported_once(s: &mut Session, log: &Calls, id: &str, params: serde_json::Value) {
    let n = calls(log).len();
    let before = s.doc.clone();
    s.run(id, &params).unwrap();
    let seen = calls(log);
    assert_eq!(seen.len(), n + 1, "{id} is reported once: {:?}", ids(log));
    let last = &seen[n];
    assert_eq!(last.id, id);
    assert!(last.before.same_content(&before), "{id}: `before` is the document before it ran");
    assert!(last.after.same_content(&s.doc), "{id}: `after` is the document it left");
    assert!(!last.before.same_content(&last.after));
}

#[test]
fn observer_sees_every_mutating_command() {
    let mut s = Session::new(Document::new());
    let log = observed(&mut s);

    // Backspace at the start of an empty document and a selection change change nothing.
    s.run("text.backspace", &json!({})).unwrap();
    s.run("select.all", &json!({})).unwrap();
    s.run("select.collapse", &json!({})).unwrap();
    assert!(calls(&log).is_empty(), "no content change, no call: {:?}", ids(&log));

    reported_once(&mut s, &log, "text.insert", json!({"text": "Hello world"}));
    reported_once(&mut s, &log, "text.newParagraph", json!({}));
    s.run("select.all", &json!({})).unwrap();
    reported_once(&mut s, &log, "format.bold", json!({}));
    s.run("select.collapse", &json!({})).unwrap();
    let n = calls(&log).len();
    s.run_ime("text.insert", &json!({"text": "x"})).unwrap();
    assert_eq!(ids(&log)[n..], ["text.insert"], "an input method's commit is reported once");

    // Uninstalled: nothing more is seen.
    s.set_observer(None);
    s.run("text.insert", &json!({"text": "y"})).unwrap();
    assert_eq!(calls(&log).len(), n + 1);
}

#[test]
fn repeat_is_reported_once() {
    let mut s = Session::new(Document::new());
    let log = observed(&mut s);
    s.run("text.insert", &json!({"text": "ab"})).unwrap();
    // Repeat (F4) runs the last command again inside itself.
    reported_once(&mut s, &log, "edit.repeat", json!({}));
}

#[test]
fn a_macro_is_reported_once() {
    let mut s = Session::new(Document::new());
    let log = observed(&mut s);
    // A macro's steps run inside the macro command.
    s.macros.insert("twice".into(), vec![("text.insert".into(), json!({"text": "c"})), ("text.insert".into(), json!({"text": "d"}))]);
    reported_once(&mut s, &log, "tools.macros", json!({"run": "twice"}));
}

#[test]
fn snapshot_undo_and_redo_are_reported() {
    let mut s = Session::new(Document::new());
    let log = observed(&mut s);
    s.run("text.insert", &json!({"text": "ab"})).unwrap();
    // The session's own Undo and Redo change the document too.
    reported_once(&mut s, &log, "edit.undo", json!({}));
    reported_once(&mut s, &log, "edit.redo", json!({}));
}

#[test]
fn a_panicking_observer_stays_installed() {
    struct Flaky(Arc<Mutex<usize>>);
    impl EditObserver for Flaky {
        fn edited(&mut self, _: &Document, _: &Document, _: &str) {
            let mut n = self.0.lock().unwrap();
            *n += 1;
            if *n == 1 {
                drop(n);
                panic!("observer bug");
            }
        }
    }
    let count = Arc::new(Mutex::new(0));
    let mut s = Session::new(Document::new());
    s.set_observer(Some(Box::new(Flaky(count.clone()))));
    assert!(s.run("text.insert", &json!({"text": "a"})).is_ok(), "the edit stands");
    s.run("text.insert", &json!({"text": "b"})).unwrap();
    assert_eq!(*count.lock().unwrap(), 2, "the observer still hears edits after it panicked");
}

#[test]
fn the_live_hooks_follow_their_document() {
    let mut s = Session::new(Document::new());
    let shared = s.document_id();
    let log = observed(&mut s);
    let asked = Arc::new(Mutex::new(0));
    let seen = asked.clone();
    s.live_undo_hook(Some(Box::new(move |_| {
        *seen.lock().unwrap() += 1;
        true
    })));

    let other = s.add_document(Document::new());
    s.run("text.insert", &json!({"text": "elsewhere"})).unwrap();
    s.run("edit.undo", &json!({})).unwrap();
    assert!(calls(&log).is_empty(), "another document's edits are not the shared document's");
    assert_eq!(*asked.lock().unwrap(), 0, "another document's Undo is its own");
    assert!(!s.doc.plain_text(StoryRef::Body).contains("elsewhere"), "and it undid its typing");
    assert!(s.apply_remote(|_| {}).is_err(), "a peer's edit never lands in another document");

    s.activate(shared).unwrap();
    assert!(s.is_open(other));
    s.run("text.insert", &json!({"text": "here"})).unwrap();
    assert_eq!(ids(&log), ["text.insert"]);
    assert!(s.apply_remote(|_| {}).is_ok());
}

#[test]
fn session_stays_send() {
    fn send<T: Send>() {}
    send::<Session>();
}

#[test]
fn apply_remote_adds_no_undo_step() {
    let mut s = Session::new(Document::new());
    s.run("text.insert", &json!({"text": "Local"})).unwrap();
    let depth = s.undo_depth();
    let dirty = s.dirty;
    s.dirty = false;
    let log = observed(&mut s);

    s.apply_remote(|d| {
        if let Ok(body) = d.story_mut(StoryRef::Body) {
            body.push(Arc::new(Block::Para(Paragraph::with_text("Remote", Default::default()))));
        }
    })
    .unwrap();

    assert!(s.doc.plain_text(StoryRef::Body).contains("Remote"), "the remote edit is in the document");
    assert!(depth > 0);
    assert_eq!(s.undo_depth(), 0, "a remote edit adds no step, and the older ones are gone");
    assert!(!s.dirty, "a remote edit alone does not mark the document for autosave (was {dirty})");
    assert!(calls(&log).is_empty(), "a remote edit is not echoed back to the observer");

    // An older snapshot can never come back over the peer's edit.
    s.run("edit.undo", &json!({})).unwrap();
    assert!(s.doc.plain_text(StoryRef::Body).contains("Remote"), "Undo keeps the remote edit");
    s.run("edit.redo", &json!({})).unwrap();
    assert!(s.doc.plain_text(StoryRef::Body).contains("Remote"), "Redo keeps the remote edit");
}

#[test]
fn apply_remote_keeps_caret_on_same_pid() {
    let mut s = Session::new(Document::from_text("Alpha\nBravo\nCharlie\n"));
    share(&mut s);
    let bravo = s.doc.para_paths(StoryRef::Body)[1].clone();
    s.sel = Selection::caret(Pos::new(StoryRef::Body, bravo.clone(), 3));

    // A peer inserts two paragraphs above the caret's.
    s.apply_remote(|d| {
        if let Ok(body) = d.story_mut(StoryRef::Body) {
            for t in ["One", "Two"] {
                let mut p = Paragraph::with_text(t, Default::default());
                p.pid = 100 + t.len() as u64;
                body.insert(0, Arc::new(Block::Para(p)));
            }
        }
    })
    .unwrap();

    let at = s.sel.focus.clone();
    assert!(s.sel.is_collapsed());
    assert_ne!(at.path, bravo, "the caret's paragraph moved down");
    let p = s.doc.para_at(&at).unwrap();
    assert_eq!(p.pid, 2, "the caret is still in the same shared paragraph");
    assert_eq!(&p.text[..at.off], "Bra", "at the same offset");

    // That paragraph goes: the caret lands at the start of the one now at its index, never panics.
    let index = s.doc.para_paths(StoryRef::Body).iter().position(|q| *q == at.path).unwrap();
    s.apply_remote(|d| {
        if let Ok(body) = d.story_mut(StoryRef::Body) {
            body.truncate(index);
        }
    })
    .unwrap();
    let at = s.sel.focus.clone();
    let paths = s.doc.para_paths(StoryRef::Body);
    let now = paths.iter().position(|q| *q == at.path).unwrap();
    assert_eq!(now, index.min(paths.len() - 1), "the caret is on the paragraph now at its index");
    assert_eq!(at.off, 0);
}

#[test]
fn live_undo_hook_owns_undo_while_live() {
    let mut s = Session::new(Document::new());
    s.run("text.insert", &json!({"text": "Kept"})).unwrap();
    let asked = Arc::new(Mutex::new(Vec::new()));
    let seen = asked.clone();
    s.live_undo_hook(Some(Box::new(move |redo| {
        if let Ok(mut v) = seen.lock() {
            v.push(redo);
        }
        false
    })));

    s.run("edit.undo", &json!({})).unwrap();
    s.run("edit.redo", &json!({})).unwrap();
    assert_eq!(asked.lock().map(|v| v.clone()).unwrap_or_default(), [false, true]);
    assert!(s.doc.plain_text(StoryRef::Body).contains("Kept"), "the snapshot undo did not run");

    s.live_undo_hook(None);
    s.run("edit.undo", &json!({})).unwrap();
    assert!(!s.doc.plain_text(StoryRef::Body).contains("Kept"), "without the hook, undo is the snapshot undo again");
}

#[test]
fn apply_remote_relayouts_paragraphs_edited_in_place() {
    let mut s = Session::new(Document::from_text("Alpha\nBravo\n"));
    let path = s.doc.para_paths(StoryRef::Body)[1].clone();
    let rev = s.doc.para(StoryRef::Body, &path).unwrap().rev;
    s.apply_remote(|d| {
        if let Ok(body) = d.story_mut(StoryRef::Body)
            && let Some(b) = body.get_mut(1)
            && let Block::Para(p) = Arc::make_mut(b)
        {
            // A direct edit, without the paragraph's own methods.
            p.text.push('!');
            if let Some(r) = p.runs.last_mut() {
                r.len += 1;
            }
        }
    })
    .unwrap();
    let p = s.doc.para(StoryRef::Body, &path).unwrap();
    assert_eq!(p.text, "Bravo!");
    assert_ne!(p.rev, rev, "the edited paragraph's layout is not the cached one");
}

#[test]
fn undo_actions_go_to_the_live_hook() {
    let mut s = Session::new(Document::new());
    let asked = Arc::new(Mutex::new(0));
    let seen = asked.clone();
    s.live_undo_hook(Some(Box::new(move |_| {
        let mut n = seen.lock().unwrap();
        *n += 1;
        *n < 3
    })));
    // No snapshot step at all: the hook decides.
    assert!(s.can_undo() && s.can_redo());
    s.run("edit.undoTo", &json!({"count": 5})).unwrap();
    assert_eq!(*asked.lock().unwrap(), 3, "asked until it had nothing more to undo");
}

#[test]
fn removing_one_hook_does_not_move_the_binding() {
    let mut s = Session::new(Document::new());
    let shared = s.document_id();
    let log = observed(&mut s);
    s.live_undo_hook(Some(Box::new(|_| true)));
    s.add_document(Document::new());
    // The undo hook goes while another document is active; the observer stays on the first.
    s.live_undo_hook(None);
    s.run("text.insert", &json!({"text": "elsewhere"})).unwrap();
    assert!(calls(&log).is_empty(), "the other document's edit is not reported: {:?}", ids(&log));
    assert!(s.apply_remote(|_| {}).is_err());
    s.activate(shared).unwrap();
    s.run("text.insert", &json!({"text": "here"})).unwrap();
    assert_eq!(ids(&log), ["text.insert"]);
}

#[test]
fn assigning_pids_is_not_an_edit() {
    let mut s = Session::new(Document::from_text("Alpha\nBravo\n"));
    s.run("text.insert", &json!({"text": "x"})).unwrap();
    let depth = s.undo_depth();
    s.dirty = false;
    let log = observed(&mut s);
    s.assign_pids(|d| {
        for (i, path) in d.para_paths(StoryRef::Body).into_iter().enumerate() {
            if let Ok(p) = d.para_mut(StoryRef::Body, &path) {
                p.pid = i as u64 + 1;
            }
        }
    })
    .unwrap();
    assert!(s.doc.para_paths(StoryRef::Body).iter().all(|p| s.doc.para(StoryRef::Body, p).is_some_and(|q| q.pid != 0)));
    assert_eq!(s.undo_depth(), depth, "the undo history stays");
    assert!(!s.dirty);
    assert!(calls(&log).is_empty(), "ids are not reported as an edit");

    // Anything but ids is refused, and the document is as it was.
    let text = s.doc.plain_text(StoryRef::Body);
    assert!(s.assign_pids(|d| d.body.truncate(1)).is_err());
    assert_eq!(s.doc.plain_text(StoryRef::Body), text);
}

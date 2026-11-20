//! The entry points a live co-editing layer uses: the edit observer, the remote apply and the
//! undo hook ([`Session::set_observer`], [`Session::apply_remote`], [`Session::live_undo_hook`]).
use std::sync::{Arc, Mutex};

use serde_json::json;
use wordcraft_doc::{Block, Document, Paragraph, Pos, StoryRef};
use wordcraft_engine::{EditObserver, Selection, Session};

/// Records the command id of every `edited` call, and whether `before` and `after` differed.
struct Log(Arc<Mutex<Vec<(String, bool)>>>);

impl EditObserver for Log {
    fn edited(&mut self, before: &Document, after: &Document, command: &str) {
        if let Ok(mut v) = self.0.lock() {
            v.push((command.to_string(), !before.same_content(after)));
        }
    }
}

fn observed(s: &mut Session) -> Arc<Mutex<Vec<(String, bool)>>> {
    let log = Arc::new(Mutex::new(Vec::new()));
    s.set_observer(Some(Box::new(Log(log.clone()))));
    log
}

fn calls(log: &Arc<Mutex<Vec<(String, bool)>>>) -> Vec<(String, bool)> {
    log.lock().map(|v| v.clone()).unwrap_or_default()
}

/// Gives every body paragraph a shared id, 1, 2, 3… in order.
fn share(s: &mut Session) {
    for (i, path) in s.doc.para_paths(StoryRef::Body).into_iter().enumerate() {
        s.doc.para_mut(StoryRef::Body, &path).unwrap().pid = i as u64 + 1;
    }
}

#[test]
fn observer_sees_every_mutating_command() {
    let mut s = Session::new(Document::new());
    let log = observed(&mut s);

    // Backspace at the start of an empty document changes nothing: no call.
    s.run("text.backspace", &json!({})).unwrap();
    assert!(calls(&log).is_empty(), "a no-op is not an edit: {:?}", calls(&log));

    s.run("text.insert", &json!({"text": "Hello world"})).unwrap();
    s.run("text.newParagraph", &json!({})).unwrap();
    s.run("select.all", &json!({})).unwrap();
    s.run("format.bold", &json!({})).unwrap();
    s.run("select.collapse", &json!({})).unwrap();
    s.run_ime("text.insert", &json!({"text": "x"})).unwrap();

    let seen = calls(&log);
    let ids: Vec<&str> = seen.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(ids, ["text.insert", "text.newParagraph", "format.bold", "text.insert"], "once per mutating command");
    assert!(seen.iter().all(|(_, changed)| *changed), "each call sees the document before and after: {seen:?}");

    // Uninstalled: nothing more is seen.
    s.set_observer(None);
    s.run("text.insert", &json!({"text": "y"})).unwrap();
    assert_eq!(calls(&log).len(), seen.len());
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
    assert_eq!(s.undo_depth(), depth, "a remote edit is not the local user's undo step");
    assert!(!s.dirty, "a remote edit alone does not mark the document for autosave (was {dirty})");
    assert!(calls(&log).is_empty(), "a remote edit is not echoed back to the observer");

    // Undo takes the local edit back and leaves the remote one.
    s.run("edit.undo", &json!({})).unwrap();
    assert!(!s.doc.plain_text(StoryRef::Body).contains("Local"));
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
    assert!(s.doc.para_at(&at).is_some(), "the caret is on a paragraph that exists");
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

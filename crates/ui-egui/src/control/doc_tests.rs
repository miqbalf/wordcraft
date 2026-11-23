//! The control channel's `doc`: methods addressed to an open document that isn't the active one.

use serde_json::{Value, json};
use wordcraft_engine::Session;

use super::{ControlRequest, Outcome, handle};
use crate::{Services, WordApp};

fn app() -> WordApp {
    WordApp::new(Session::new(wordcraft_doc::Document::new()), Services::default())
}

/// Send one request the way the control channel does and return the reply.
fn send(a: &mut WordApp, method: &str, params: Value) -> Value {
    let ctx = egui::Context::default();
    let (req, _reply) = ControlRequest::new(method, params);
    match handle(a, &ctx, &req) {
        Outcome::Done(v) => v,
        Outcome::Screenshot { .. } => json!({"ok": false, "error": "not a reply"}),
    }
}

fn exec(a: &mut WordApp, command: &str, params: Value) -> Value {
    send(a, "engine.execute", json!({"command": command, "params": params}))
}

fn body(a: &WordApp) -> String {
    a.session.doc.plain_text(wordcraft_doc::StoryRef::Body)
}

/// Two documents, the first typed in and parked behind the second: (first, second).
fn two_documents(a: &mut WordApp) -> (u64, u64) {
    let first = a.session.document_id();
    assert_eq!(exec(a, "text.insert", json!({"text": "First"}))["ok"], true);
    assert_eq!(exec(a, "file.new", json!({}))["ok"], true);
    let second = a.session.document_id();
    assert_ne!(first, second);
    assert_eq!(exec(a, "text.insert", json!({"text": "Second"}))["ok"], true);
    (first, second)
}

/// W1: `doc` sends a command, an inspection or a render to a document in the background; the
/// window's document stays active and unchanged, and an id that names no open document is an
/// error.
#[test]
fn methods_address_an_open_document_by_id() {
    let mut a = app();
    let (first, second) = two_documents(&mut a);

    let r = send(&mut a, "engine.execute", json!({"command": "text.insert", "params": {"text": " more"}, "doc": first}));
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(a.session.document_id(), second);
    assert_eq!(body(&a).trim(), "Second");
    let r = send(&mut a, "document.inspect", json!({"doc": first}));
    assert_eq!(r["result"]["blocks"][0]["text"], "First more", "{r}");
    let r = send(&mut a, "ui.render", json!({"doc": first, "scale": 0.2}));
    assert!(r["result"]["png"].as_str().is_some_and(|p| !p.is_empty()), "{r}");
    // The active document's own id works like no `doc` at all.
    let r = send(&mut a, "engine.execute", json!({"command": "text.insert", "params": {"text": "!"}, "doc": second}));
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(body(&a).trim(), "Second!");

    let r = send(&mut a, "ui.inspect", json!({}));
    assert_eq!(r["result"]["document"], second, "{r}");
    let ids: Vec<u64> = r["result"]["documents"].as_array().into_iter().flatten().filter_map(|d| d["id"].as_u64()).collect();
    assert_eq!(ids, vec![first, second]);

    // Window commands need the document on screen.
    let r = send(&mut a, "engine.execute", json!({"command": "ui.tab", "params": {"value": "insert"}, "doc": first}));
    assert_eq!(r["ok"], false, "{r}");

    for bad in [json!(u64::MAX), json!(-1), json!("1"), json!(first.to_string()), json!(1.5), json!(1e300), json!({}), json!([first])] {
        for (method, params) in [
            ("engine.execute", json!({"command": "text.insert", "params": {"text": "x"}, "doc": bad})),
            ("document.inspect", json!({"doc": bad})),
            ("ui.render", json!({"doc": bad})),
        ] {
            let r = send(&mut a, method, params);
            assert_eq!(r["ok"], false, "{method} with doc {bad}: {r}");
        }
    }
    assert_eq!(a.session.document_id(), second);
    assert_eq!(body(&a).trim(), "Second!");
    let r = send(&mut a, "document.inspect", json!({"doc": first}));
    assert_eq!(r["result"]["blocks"][0]["text"], "First more", "{r}");
}

/// W1: `file.close` addressed to a document in the background refuses its unsaved changes
/// unless forced (only a boolean true), as it does for the window's document (#1268), and closes
/// only that document.
#[test]
fn closing_an_addressed_document_keeps_unsaved_work_unless_forced() {
    let mut a = app();
    let (first, second) = two_documents(&mut a);

    for params in [json!({}), json!({"force": "true"}), json!({"force": 1})] {
        let r = send(&mut a, "engine.execute", json!({"command": "file.close", "params": params, "doc": first}));
        assert_eq!(r["ok"], false, "{r}");
        let e = r["error"].as_str().unwrap_or("");
        assert!(e.contains("unsaved") && e.contains("force"), "{e}");
        assert!(a.session.is_open(first));
    }

    let r = send(&mut a, "engine.execute", json!({"command": "file.close", "params": {"force": true}, "doc": first}));
    assert_eq!(r["ok"], true, "{r}");
    assert!(!a.session.is_open(first));
    assert_eq!(a.session.document_id(), second);
    assert!(a.has_document() && !a.quit_requested);
    assert_eq!(body(&a).trim(), "Second");
    let r = send(&mut a, "engine.execute", json!({"command": "text.insert", "params": {"text": "x"}, "doc": first}));
    assert_eq!(r["ok"], false, "a closed document can't be addressed: {r}");

    // A document without unsaved changes closes at once.
    assert_eq!(exec(&mut a, "file.new", json!({}))["ok"], true);
    let clean = a.session.document_id();
    assert_eq!(exec(&mut a, "file.new", json!({}))["ok"], true);
    let active = a.session.document_id();
    let r = send(&mut a, "engine.execute", json!({"command": "file.close", "doc": clean}));
    assert_eq!(r["ok"], true, "{r}");
    assert!(!a.session.is_open(clean) && a.session.is_open(second));
    assert_eq!(a.session.document_id(), active);
}

/// The text of a downloaded or written copy.
fn copy_text(name: &str, bytes: &[u8]) -> String {
    wordcraft_engine::io::open_bytes(name, bytes).unwrap().plain_text(wordcraft_doc::StoryRef::Body)
}

type Copies = std::rc::Rc<std::cell::RefCell<Vec<(String, Vec<u8>)>>>;

/// Save As = Export on a document in the background, in a browser: `file.saveAs` without a path
/// and the exports download that document's copy through the one Save As function, never
/// Save's dialog and never the active document. Both documents stay as they were.
#[test]
fn save_as_on_an_addressed_document_downloads_its_copy_in_a_browser() {
    let mut a = app();
    let copies = Copies::default();
    let c = copies.clone();
    a.services.download_copy = Some(Box::new(move |name: &str, _: &str, bytes: &[u8]| {
        c.borrow_mut().push((name.to_string(), bytes.to_vec()));
        Ok(())
    }));
    let saved = std::rc::Rc::new(std::cell::Cell::new(0));
    let s = saved.clone();
    a.services.download = Some(Box::new(move |_: &str, _: &[u8]| {
        s.set(s.get() + 1);
        Ok(())
    }));
    let (first, second) = two_documents(&mut a);
    for (command, ext) in [("file.saveAs", ".docx"), ("file.exportPdf", ".pdf"), ("file.exportPng", ".png"), ("file.saveTemplate", ".dotx")] {
        let r = send(&mut a, "engine.execute", json!({"command": command, "params": {}, "doc": first}));
        assert_eq!(r["ok"], true, "{command}: {r}");
        let (name, _) = copies.borrow().last().cloned().unwrap();
        assert!(name.ends_with(ext), "{command}: {name}");
    }
    let (name, bytes) = copies.borrow()[0].clone();
    assert_eq!(copy_text(&name, &bytes).trim(), "First", "the addressed document's copy");
    assert_eq!(saved.get(), 0, "no save download");
    assert!(a.file_dialog.is_none() && !a.ui.backstage, "no dialog, no Save As page");
    assert_eq!(a.session.document_id(), second, "the window keeps its document");
    assert!(a.session.document_dirty(first) && a.session.document_dirty(second), "a copy is not a save");
}

/// The same on the desktop: the Save dialog asks for the addressed document's copy, which is
/// written there; neither document moves to the picked file.
#[test]
fn save_as_on_an_addressed_document_writes_its_copy_on_the_desktop() {
    let dir = std::env::temp_dir().join(format!("wordcraft-doc-save-as-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut a = app();
    let folder = dir.clone();
    a.services.pick_save = Some(Box::new(move |name: &str| Some(folder.join(name).to_string_lossy().into_owned())));
    let (first, second) = two_documents(&mut a);
    let r = send(&mut a, "engine.execute", json!({"command": "file.saveAs", "params": {}, "doc": first}));
    assert_eq!(r["ok"], true, "{r}");
    let written: Vec<_> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).map(|e| e.path()).collect();
    assert_eq!(written.len(), 1, "{written:?}");
    let name = written[0].file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.ends_with(".docx"), "{name}");
    assert_eq!(copy_text(&name, &std::fs::read(&written[0]).unwrap()).trim(), "First");
    let r = send(&mut a, "engine.execute", json!({"command": "file.exportPdf", "params": {}, "doc": first}));
    assert_eq!(r["ok"], true, "{r}");
    assert!(std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).any(|e| e.path().extension().is_some_and(|x| x == "pdf")));
    assert_eq!(a.session.document_id(), second);
    assert!(a.session.path.is_none(), "the active document isn't moved to the copy");
    assert!(a.session.with_document(first, |s| s.path.is_none()).unwrap(), "nor the addressed one");
    assert!(a.session.document_dirty(first));
    let _ = std::fs::remove_dir_all(&dir);
}

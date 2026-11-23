//! `document.bytes`: the active document as a file's bytes in the reply, for a host page that
//! makes a copy in another format (Craft Office's Convert to .docx). It changes nothing about the
//! document.

use serde_json::{Value, json};
use wordcraft_engine::Session;

use super::{ControlRequest, Outcome, handle};
use crate::{Services, WordApp};

fn app() -> WordApp {
    WordApp::new(Session::new(wordcraft_doc::Document::new()), Services::default())
}

fn send(a: &mut WordApp, method: &str, params: Value) -> Value {
    let ctx = egui::Context::default();
    let (req, _reply) = ControlRequest::new(method, params);
    match handle(a, &ctx, &req) {
        Outcome::Done(v) => v,
        Outcome::Screenshot { .. } => json!({"ok": false, "error": "not a reply"}),
    }
}

fn decode(v: &Value) -> Vec<u8> {
    wordcraft_engine::cmd::insert::base64_decode(v["result"]["data"].as_str().expect("data")).expect("base64")
}

/// An .odt opened and edited comes back as a .docx that holds the edit; the document keeps its
/// file, its unsaved changes and its save stamps.
#[test]
fn document_bytes_is_a_docx_copy_that_changes_nothing() {
    let mut a = app();
    a.session.path = Some(std::path::PathBuf::from("Notes.odt"));
    a.run("text.insert", json!({"text": "Convert me"})).unwrap();
    assert!(a.session.dirty);
    let revision = a.session.doc.core.revision;
    let modified = a.session.doc.core.modified.clone();

    let v = send(&mut a, "document.bytes", json!({"name": "Notes.docx"}));
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["result"]["name"], "Notes.docx", "{v}");
    let bytes = decode(&v);
    assert_eq!(&bytes[..2], b"PK", "a Word package");
    assert_eq!(v["result"]["size"], bytes.len(), "{v}");
    let back = wordcraft_engine::io::open_bytes("Notes.docx", &bytes).expect("opens as .docx");
    assert!(back.plain_text(wordcraft_doc::StoryRef::Body).contains("Convert me"));

    assert_eq!(a.session.path.as_deref(), Some(std::path::Path::new("Notes.odt")), "same file");
    assert!(a.session.dirty, "still unsaved");
    assert_eq!(a.session.doc.core.revision, revision, "no save stamp");
    assert_eq!(a.session.doc.core.modified, modified, "no save stamp");
}

/// The name's extension picks the format, as a save does.
#[test]
fn document_bytes_follows_the_extension() {
    let mut a = app();
    a.run("text.insert", json!({"text": "Plain"})).unwrap();
    let v = send(&mut a, "document.bytes", json!({"name": "a.rtf"}));
    assert_eq!(v["ok"], true, "{v}");
    assert!(decode(&v).starts_with(b"{\\rtf"), "{v}");
    let v = send(&mut a, "document.bytes", json!({"name": "a.nope"}));
    assert_eq!(v["ok"], false, "{v}");
}

/// No name, no document, or a password-protected document: refused, with no bytes.
#[test]
fn document_bytes_refusals() {
    let mut a = app();
    for p in [json!({}), json!({"name": ""}), json!({"name": "  "}), json!({"name": 5})] {
        let v = send(&mut a, "document.bytes", p.clone());
        assert_eq!(v["ok"], false, "{p}: {v}");
        assert!(v["error"].as_str().is_some_and(|e| e.contains("name")), "{p}: {v}");
    }

    a.session.password = Some(wordcraft_engine::io::Password::new("synthetic-password"));
    let v = send(&mut a, "document.bytes", json!({"name": "a.docx"}));
    assert_eq!(v["ok"], false, "{v}");
    assert!(v["error"].as_str().is_some_and(|e| e.contains("password")), "{v}");
    assert!(v.get("result").is_none(), "{v}");

    let mut a = app();
    a.start_screen();
    let v = send(&mut a, "document.bytes", json!({"name": "a.docx"}));
    assert_eq!(v["ok"], false, "{v}");
    assert!(v["error"].as_str().is_some_and(|e| e.contains("no document is open")), "{v}");
}

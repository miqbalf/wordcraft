//! Copy, cut and paste while editing live (LP1): through the pump, like typing.

use wordcraft_doc::{Block, Document, InlineObject};
use wordcraft_live::PASTED_AS_TEXT;

use super::*;

/// A one-pixel PNG, for a picture to copy.
const PIXEL: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

fn select(e: &mut Ed, from: (usize, usize), to: (usize, usize)) {
    e.s.sel = Selection { anchor: Pos::body(from.0, from.1), focus: Pos::body(to.0, to.1) };
}

/// The body the editor's shared copy holds, read back as a document.
fn shared_body(e: &Ed) -> Document {
    let live = e.le.live().doc().expect("live");
    document_from_state(&e.s.doc, &live.state()).unwrap()
}

/// Both editors hold the same document, and each window shows what its shared copy holds: the
/// drift check has nothing to repair.
fn assert_converged(a: &Ed, b: &Ed, what: &str) {
    assert_same(a, b, what);
    for e in [a, b] {
        let shared = shared_body(e);
        assert_eq!(texts(&shared), texts(&e.s.doc), "{what}: the window shows its shared copy");
        assert!(shared.body.iter().zip(&e.s.doc.body).all(|(x, y)| x == y), "{what}: the shared copy's formatting is the window's");
        assert_eq!(pids(&shared), pids(&e.s.doc), "{what}: under the same ids");
        assert_eq!(e.le.drift(&e.s), None, "{what}: the drift check finds nothing");
        assert_eq!(e.le.repairs(), 0, "{what}: and never had to repair");
    }
}

fn two(paras: &[&str]) -> (Shared, Ed, Ed) {
    let relay = relay(None);
    let bytes = file(paras);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    let mut b = Ed::open(&relay, "bob", Access::Edit, &bytes);
    settle(&relay, &mut [&mut a, &mut b]);
    (relay, a, b)
}

fn objects(d: &Document) -> usize {
    d.body.iter().filter_map(|b| b.as_para()).map(|p| p.objects.len()).sum()
}

fn tables(d: &Document) -> usize {
    d.body.iter().filter(|b| b.as_table().is_some()).count()
}

fn bold_text(d: &Document) -> String {
    let mut out = String::new();
    for p in d.body.iter().filter_map(|b| b.as_para()) {
        for (r, props) in p.run_ranges() {
            if props.bold == Some(true) {
                out.push_str(p.text.get(r).unwrap_or(""));
            }
        }
    }
    out
}

#[test]
fn copy_and_paste_are_allowed_live() {
    let (_relay, a, _b) = two(&["Hello"]);
    for id in ["edit.copy", "edit.cut", "edit.paste", "edit.pasteText", "edit.pasteMerge"] {
        assert!(a.le.allows(id), "{id} is allowed while live");
    }
}

#[test]
fn copying_while_live_changes_nothing_and_fills_the_clipboard() {
    let (relay, mut a, mut b) = two(&["Hello world", "Second"]);
    select(&mut a, (0, 0), (0, 5));
    assert!(a.run("edit.copy", json!({})));
    assert_eq!(a.s.clipboard.as_ref().map(|f| f.plain_text()).as_deref(), Some("Hello"));
    assert!(a.le.take_notices().is_empty(), "no refusal");
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(texts(&b.s.doc), ["Hello world", "Second"]);
    assert_converged(&a, &b, "after copy");
}

/// Our own copy, formatted and across paragraphs, pasted in the middle of a paragraph.
#[test]
fn pasting_formatted_paragraphs_reaches_the_other_editor() {
    let (relay, mut a, mut b) = two(&["Hello world", "Second line", "Third"]);
    select(&mut a, (0, 0), (0, 5));
    assert!(a.run("format.bold", json!({})));
    settle(&relay, &mut [&mut a, &mut b]);
    // "Hello world\nSecond" with "Hello" bold.
    select(&mut a, (0, 0), (1, 6));
    assert!(a.run("edit.copy", json!({})));
    a.caret(2, 2);
    assert!(a.run("edit.paste", json!({})), "the paste ran");
    assert!(a.le.take_notices().is_empty(), "nothing to tell: everything pasted is shared");
    assert_eq!(texts(&a.s.doc), ["Hello world", "Second line", "ThHello world", "Secondird"]);
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(texts(&b.s.doc), ["Hello world", "Second line", "ThHello world", "Secondird"], "Bob has the pasted text");
    assert_eq!(bold_text(&b.s.doc), "HelloHello", "and its bold");
    assert_converged(&a, &b, "after a paste of several paragraphs");
}

/// Another program's formatted copy (HTML), with paragraph formatting.
#[test]
fn pasting_html_from_another_program_reaches_the_other_editor() {
    let (relay, mut a, mut b) = two(&["Hello", "World"]);
    a.end_of(0);
    let html = r#"<p style="text-align:center"><b>Big</b> news</p><p><i>More</i> text</p>"#;
    assert!(a.run("edit.paste", json!({"text": "Big news\nMore text", "html": html})));
    assert!(a.le.take_notices().is_empty(), "nothing to tell");
    settle(&relay, &mut [&mut a, &mut b]);
    let t = texts(&b.s.doc);
    assert_eq!(t.first().map(String::as_str), Some("HelloBig news"), "{t:?}");
    assert!(t.iter().any(|p| p.starts_with("More text")), "{t:?}");
    assert_eq!(bold_text(&b.s.doc), "Big");
    assert_converged(&a, &b, "after an HTML paste");
}

/// Plain text pasted within one paragraph, and Keep Text Only across lines.
#[test]
fn pasting_plain_text_reaches_the_other_editor() {
    let (relay, mut a, mut b) = two(&["Hello", "World"]);
    a.end_of(0);
    assert!(a.run("edit.paste", json!({"text": " there", "privateClipboard": false})));
    a.end_of(1);
    assert!(a.run("edit.pasteText", json!({"text": "\nand more\nlines"})));
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(texts(&b.s.doc), ["Hello there", "World", "and more", "lines"]);
    assert_converged(&a, &b, "after plain pastes");
}

/// A Session not live, holding a copy of a picture, a table and the text around them.
fn copy_with_objects() -> wordcraft_doc::edit::Fragment {
    let mut s = Session::new(Document::from_text("Before"));
    s.sel = Selection::caret(Pos::body(0, 6));
    s.run("insert.picture", &json!({"data": PIXEL})).unwrap();
    // The new picture is selected: type after it.
    s.run("select.collapse", &json!({"end": true})).unwrap();
    s.run("text.newParagraph", &json!({})).unwrap();
    s.run("insert.table", &json!({"rows": 1, "cols": 2})).unwrap();
    s.run("text.insert", &json!({"text": "Cell"})).unwrap();
    // Everything, from the first paragraph to the one after the table (Select All inside a
    // table selects the table).
    let last = s.doc.body.len() - 1;
    let end = s.doc.body[last].as_para().map_or(0, |p| p.text.len());
    s.sel = Selection { anchor: Pos::body(0, 0), focus: Pos::body(last, end) };
    s.run("edit.copy", &json!({})).unwrap();
    let f = s.clipboard.clone().unwrap();
    let pictures = f.blocks.iter().filter_map(|b| b.as_para()).flat_map(|p| &p.objects).filter(|o| matches!(o, InlineObject::Image { .. })).count();
    assert_eq!(pictures, 1, "the copy holds the picture");
    assert!(f.blocks.iter().any(|b| matches!(b, Block::Table(_))), "and the table");
    f
}

#[test]
fn pasting_a_picture_and_a_table_arrives_as_text_with_a_notice() {
    let (relay, mut a, mut b) = two(&["Hello", "World"]);
    a.s.clipboard = Some(copy_with_objects());
    a.end_of(0);
    assert!(a.run("edit.paste", json!({})), "the paste ran");
    let said = a.le.take_notices();
    assert_eq!(said, [PASTED_AS_TEXT], "one notice");
    assert_eq!(PASTED_AS_TEXT, "Pasted as text: pictures and tables can't be shared while others are editing.");
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(objects(&b.s.doc), 0, "no picture reached Bob");
    assert_eq!(tables(&b.s.doc), 0, "nor a table");
    let all = texts(&b.s.doc).join("\n");
    assert!(all.contains("HelloBefore"), "the text around them did: {all:?}");
    assert!(all.contains("Cell"), "and the table's text: {all:?}");
    assert!(!all.contains(wordcraft_doc::para::OBJ), "{all:?}");
    assert_eq!(a.s.doc.media.len(), b.s.doc.media.len(), "no picture data came in");
    assert_converged(&a, &b, "after a paste made text");
}

#[test]
fn cut_converges() {
    let (relay, mut a, mut b) = two(&["Hello world", "Second line", "Third"]);
    select(&mut a, (0, 5), (1, 6));
    assert!(a.run("edit.cut", json!({})));
    assert!(a.le.take_notices().is_empty(), "the cut was shared");
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(texts(&b.s.doc), ["Hello line", "Third"]);
    assert_converged(&a, &b, "after a cut");
    // And pasted back somewhere else.
    a.end_of(1);
    assert!(a.run("edit.paste", json!({})));
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(texts(&b.s.doc), ["Hello line", "Third world", "Second"]);
    assert_converged(&a, &b, "after pasting the cut");
}

#[test]
fn undo_of_a_paste_restores_both_editors() {
    let (relay, mut a, mut b) = two(&["One", "Two"]);
    let start = a.s.doc.clone();
    a.end_of(0);
    assert!(a.run("edit.paste", json!({"text": " pasted\nnew paragraph\nend", "privateClipboard": false})));
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(texts(&b.s.doc), ["One pasted", "new paragraph", "end", "Two"]);
    assert!(a.run("edit.undo", json!({})), "one Undo");
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(texts(&a.s.doc), ["One", "Two"], "takes the whole paste back");
    assert!(start.same_content(&b.s.doc), "for Bob too: {:?}", texts(&b.s.doc));
    assert_converged(&a, &b, "after Undo");
    assert!(a.run("edit.redo", json!({})));
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(texts(&b.s.doc), ["One pasted", "new paragraph", "end", "Two"], "Redo brings it back");
    assert_converged(&a, &b, "after Redo");
}

#[test]
fn undo_of_a_cut_restores_both_editors() {
    let (relay, mut a, mut b) = two(&["One", "Two", "Three"]);
    select(&mut a, (0, 1), (2, 2));
    assert!(a.run("edit.cut", json!({})));
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(texts(&b.s.doc), ["Oree"]);
    assert!(a.run("edit.undo", json!({})));
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(texts(&b.s.doc), ["One", "Two", "Three"]);
    assert_converged(&a, &b, "after undoing the cut");
}

/// A paste the live layer still cannot share (here: Track Changes is on in the file, so the
/// paste would add a tracked change) is one clear message, with nothing applied anywhere.
#[test]
fn a_paste_that_cannot_be_shared_applies_nothing() {
    let mut doc = plain(&["Hello", "World"]);
    doc.settings.track_changes = true;
    let bytes = save_bytes("plan.docx", &doc).unwrap();
    let relay = relay(None);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    let mut b = Ed::open(&relay, "bob", Access::Edit, &bytes);
    settle(&relay, &mut [&mut a, &mut b]);
    let before = a.s.doc.clone();
    a.end_of(0);
    let _ = a.run("edit.paste", json!({"text": " one\ntwo", "privateClipboard": false}));
    let said = a.le.take_notices();
    assert_eq!(said.len(), 1, "one message: {said:?}");
    assert!(said[0].contains("nothing was pasted"), "{said:?}");
    assert!(before.same_content(&a.s.doc), "nothing applied: {:?}", texts(&a.s.doc));
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(texts(&b.s.doc), ["Hello", "World"]);
    assert_converged(&a, &b, "after a refused paste");
}

/// Pasted text that sits in a paragraph style the document has keeps it; one the document lacks
/// would add a style definition, so it pastes in the style it lands in, with the notice.
#[test]
fn a_new_style_is_not_brought_in() {
    let (relay, mut a, mut b) = two(&["Hello", "World"]);
    let styles = a.s.doc.styles.clone();
    a.end_of(1);
    let html = r#"<style>p.Fancy { color: red }</style><h1>Title</h1><p class="Fancy">Body</p>"#;
    assert!(a.run("edit.paste", json!({"text": "Title\nBody", "html": html})));
    assert!(a.s.doc.styles == styles, "no style definition came in");
    settle(&relay, &mut [&mut a, &mut b]);
    assert!(texts(&b.s.doc).join("\n").contains("Title"), "{:?}", texts(&b.s.doc));
    assert_converged(&a, &b, "after pasting styled HTML");
}

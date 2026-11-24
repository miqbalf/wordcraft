//! Copy, cut and paste while editing live (LP1): through the pump, like typing.

use wordcraft_doc::edit::Fragment;
use wordcraft_doc::{Block, Document, InlineObject, Paragraph, StoryRef};
use wordcraft_live::{PASTED_AS_TEXT, PASTED_WITHOUT_FORMATTING, paste_notice};

/// craft-live's notice for an Undo that would remove another editor's text.
const UNDO_TAKES_OTHERS: &str = "Someone else has typed in what this Undo would remove, so it wasn't undone.";

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
    // Undo takes the whole paste back, on both.
    assert!(a.run("edit.undo", json!({})));
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(texts(&b.s.doc), ["Hello", "World"]);
    assert_converged(&a, &b, "after undoing a paste made text");
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
    assert!(a.run("edit.redo", json!({})));
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(texts(&b.s.doc), ["Oree"], "Redo cuts it again");
    assert_converged(&a, &b, "after redoing the cut");
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
    assert_eq!(said[0], "Tracked changes are off during live editing, so nothing was pasted.");
    assert!(before.same_content(&a.s.doc), "nothing applied: {:?}", texts(&a.s.doc));
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(texts(&b.s.doc), ["Hello", "World"]);
    assert_converged(&a, &b, "after a refused paste");
}

/// A fragment of the given paragraphs, as a copy holds them.
fn fragment(paras: Vec<Paragraph>) -> Fragment {
    Fragment { blocks: paras.into_iter().map(Block::Para).collect(), parts: Default::default(), media: Default::default() }
}

/// Pasted text in a paragraph style the document lacks would add a style definition, so it
/// pastes in the style it lands in, and the notice says formatting was left out.
#[test]
fn a_new_style_is_not_brought_in() {
    let (relay, mut a, mut b) = two(&["Hello", "World"]);
    let styles = a.s.doc.styles.clone();
    a.s.clipboard =
        Some(fragment(vec![Paragraph::with_text("Title", Default::default()).styled("Fancy"), Paragraph::with_text("Body", Default::default())]));
    a.end_of(1);
    assert!(a.run("edit.paste", json!({})));
    assert_eq!(a.le.take_notices(), [PASTED_WITHOUT_FORMATTING]);
    assert!(a.s.doc.styles == styles, "no style definition came in");
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(texts(&b.s.doc), ["Hello", "WorldTitle", "Body"]);
    assert!(b.s.doc.styles == styles, "nor on Bob's side");
    assert_converged(&a, &b, "after pasting a missing style");
    // Another program's HTML with a class the document lacks: the text comes in, no style does.
    let html = r#"<style>p.Fancy { color: red }</style><h1>Head</h1><p class="Fancy">More</p>"#;
    a.end_of(2);
    assert!(a.run("edit.paste", json!({"text": "Head\nMore", "html": html})));
    let said = a.le.take_notices();
    assert!(said.is_empty() || said == [PASTED_WITHOUT_FORMATTING], "at most the formatting notice: {said:?}");
    assert!(a.s.doc.styles == styles, "no style definition came in");
    settle(&relay, &mut [&mut a, &mut b]);
    let all = texts(&b.s.doc).join("\n");
    assert!(all.contains("Head") && all.contains("More"), "{all:?}");
    assert_converged(&a, &b, "after pasting styled HTML");
}

/// The notice names what was taken out, and only that (review I1).
#[test]
fn the_notice_names_what_was_taken_out() {
    let say = |lost: Value, pasted: bool| {
        let mut r = json!({"lost": lost});
        if !pasted {
            r["pasted"] = json!(false);
        }
        paste_notice(&r)
    };
    let f = false;
    let lost = |p: bool, t: bool, fi: bool, n: bool, fm: bool| json!({"pictures": p, "tables": t, "fields": fi, "notes": n, "formatting": fm});
    assert_eq!(paste_notice(&json!({})), None, "nothing taken out");
    assert_eq!(say(lost(f, f, f, f, f), true), None, "only bookmarks or comment anchors");
    assert_eq!(say(lost(true, f, f, f, f), true).as_deref(), Some("Pasted as text: pictures can't be shared while others are editing."));
    assert_eq!(say(lost(f, true, f, f, f), true).as_deref(), Some("Pasted as text: tables can't be shared while others are editing."));
    assert_eq!(say(lost(true, true, f, f, f), true).as_deref(), Some(PASTED_AS_TEXT));
    assert_eq!(say(lost(f, f, true, true, f), true).as_deref(), Some("Pasted as text: fields and notes can't be shared while others are editing."));
    assert_eq!(say(lost(f, f, f, f, true), true).as_deref(), Some(PASTED_WITHOUT_FORMATTING));
    assert_eq!(
        say(lost(true, true, f, f, true), true).as_deref(),
        Some("Pasted as text: pictures, tables and some formatting can't be shared while others are editing.")
    );
    assert_eq!(say(lost(true, f, f, f, f), false).as_deref(), Some("Pictures can't be shared while others are editing, so nothing was pasted."));
    assert_eq!(
        say(lost(true, true, f, f, f), false).as_deref(),
        Some("Pictures and tables can't be shared while others are editing, so nothing was pasted.")
    );
}

/// A copy of text with a bookmark in it (Word files are full of hidden ones) pastes with no
/// notice: nothing the user can see was left out (review I1).
#[test]
fn a_bookmark_alone_pastes_without_a_notice() {
    let mut s = Session::new(Document::from_text("Marked words"));
    s.sel = Selection { anchor: Pos::body(0, 0), focus: Pos::body(0, 6) };
    s.run("insert.bookmark", &json!({"name": "here"})).unwrap();
    s.sel = Selection { anchor: Pos::body(0, 0), focus: Pos::body(0, s.doc.body[0].as_para().unwrap().text.len()) };
    s.run("edit.copy", &json!({})).unwrap();
    let f = s.clipboard.clone().unwrap();
    assert!(f.blocks.iter().filter_map(|b| b.as_para()).any(|p| !p.objects.is_empty()), "the copy holds the bookmark");
    let (relay, mut a, mut b) = two(&["Hello"]);
    a.s.clipboard = Some(f);
    a.end_of(0);
    assert!(a.run("edit.paste", json!({})));
    assert_eq!(a.le.take_notices(), Vec::<String>::new(), "no notice");
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(texts(&b.s.doc), ["HelloMarked words"]);
    assert_converged(&a, &b, "after pasting a bookmark");
}

/// A Session not live holding a copy of one picture and nothing else.
fn copy_of_a_picture() -> Fragment {
    let mut s = Session::new(Document::from_text(""));
    s.run("insert.picture", &json!({"data": PIXEL})).unwrap();
    s.sel = Selection { anchor: Pos::body(0, 0), focus: Pos::body(0, s.doc.body[0].as_para().unwrap().text.len()) };
    s.run("edit.copy", &json!({})).unwrap();
    s.clipboard.clone().unwrap()
}

/// A picture alone leaves nothing to paste: nothing is applied, and the notice says why
/// (review I2).
#[test]
fn a_picture_alone_pastes_nothing_and_says_so() {
    let (relay, mut a, mut b) = two(&["Hello", "World"]);
    a.s.clipboard = Some(copy_of_a_picture());
    let before = a.s.doc.clone();
    // At a caret, and over a selection, which stays.
    for sel in [((0, 5), (0, 5)), ((0, 1), (1, 2))] {
        select(&mut a, sel.0, sel.1);
        let kept = a.s.sel.clone();
        assert!(a.run("edit.paste", json!({})));
        assert_eq!(a.le.take_notices(), ["Pictures can't be shared while others are editing, so nothing was pasted."]);
        assert!(before.same_content(&a.s.doc), "nothing applied: {:?}", texts(&a.s.doc));
        assert_eq!(a.s.sel, kept, "the selection stays");
    }
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(texts(&b.s.doc), ["Hello", "World"]);
    assert_converged(&a, &b, "after a paste of a picture alone");
}

/// A table pastes as tab-separated rows, a row to a paragraph (review I3), as a spreadsheet's
/// own text would.
#[test]
fn a_table_pastes_as_tab_separated_rows() {
    let (relay, mut a, mut b) = two(&["Hello", "World"]);
    let html =
        "<table><tr><td>a</td><td><b>b</b></td><td>c</td></tr><tr><td>d</td><td>e</td><td>f</td></tr><tr><td>g</td><td></td><td>i</td></tr></table>";
    a.end_of(1);
    assert!(a.run("edit.paste", json!({"text": "a\tb\tc\nd\te\tf\ng\t\ti", "html": html})));
    assert_eq!(a.le.take_notices(), ["Pasted as text: tables can't be shared while others are editing."]);
    settle(&relay, &mut [&mut a, &mut b]);
    let t = texts(&b.s.doc);
    for row in ["a\tb\tc", "d\te\tf", "g\t\ti"] {
        assert!(t.iter().any(|p| p.ends_with(row) || p == row), "{row:?} is a paragraph on Bob: {t:?}");
    }
    assert_eq!(tables(&b.s.doc), 0);
    assert_eq!(bold_text(&b.s.doc), "b", "the cells keep their formatting");
    assert_converged(&a, &b, "after pasting a table");
}

/// A cell's own paragraphs join with line breaks inside its row.
#[test]
fn a_cell_of_several_paragraphs_stays_in_its_row() {
    let (relay, mut a, mut b) = two(&["Hello"]);
    let html = "<table><tr><td><p>one</p><p>two</p></td><td>x</td></tr></table>";
    a.end_of(0);
    assert!(a.run("edit.paste", json!({"text": "one two\tx", "html": html})));
    settle(&relay, &mut [&mut a, &mut b]);
    let all = texts(&b.s.doc).join("|");
    assert!(all.contains("one\ntwo\tx"), "{all:?}");
    assert_converged(&a, &b, "after pasting a cell of two paragraphs");
}

/// Bob types in the paragraph Alice pastes into, and in the one after it, before either has the
/// other's edit (review I4, first).
#[test]
fn bob_typing_while_alice_pastes_converges() {
    let (relay, mut a, mut b) = two(&["One two", "Three", "Four"]);
    a.s.clipboard = Some(fragment(vec![
        Paragraph::with_text("Alpha", wordcraft_doc::CharProps { bold: Some(true), ..Default::default() }),
        Paragraph::with_text("Beta", Default::default()),
        Paragraph::with_text("Gamma", Default::default()),
    ]));
    a.caret(0, 3);
    b.caret(0, 7);
    // Alice's paste goes out; Bob has not seen it when he types.
    assert!(a.run("edit.paste", json!({})));
    assert!(b.run("text.insert", json!({"text": " bob1"})));
    b.caret(1, 0);
    assert!(b.run("text.insert", json!({"text": "bob2 "})));
    settle(&relay, &mut [&mut a, &mut b]);
    let all = texts(&b.s.doc).join("\n");
    for word in ["Alpha", "Beta", "Gamma", "bob1", "bob2"] {
        assert!(all.contains(word), "{word} is there: {all:?}");
    }
    assert_converged(&a, &b, "after a paste racing typing");
    // And the other way round: Bob's typing goes out first.
    b.caret(2, 2);
    assert!(b.run("text.insert", json!({"text": "BOB"})));
    a.end_of(0);
    assert!(a.run("edit.paste", json!({"text": "x\ny\nz", "privateClipboard": false})));
    settle(&relay, &mut [&mut a, &mut b]);
    let all = texts(&a.s.doc).join("\n");
    assert!(all.contains("BOB") && all.contains("x\ny\nz"), "{all:?}");
    assert_converged(&a, &b, "after typing racing a paste");
}

/// Alice undoes a paste after Bob typed inside its text, in a paragraph that was there before:
/// the undo takes back only Alice's characters, so Bob's typing stays (review I4).
#[test]
fn undo_of_a_paste_keeps_bobs_typing_in_an_old_paragraph() {
    let (relay, mut a, mut b) = two(&["One", "Two"]);
    a.end_of(0);
    assert!(a.run("edit.paste", json!({"text": " pasted\nnew paragraph\nend", "privateClipboard": false})));
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(texts(&b.s.doc), ["One pasted", "new paragraph", "end", "Two"]);
    b.caret(0, 6);
    assert!(b.run("text.insert", json!({"text": "BOB"})));
    settle(&relay, &mut [&mut a, &mut b]);
    assert!(a.run("edit.undo", json!({})));
    assert!(a.le.take_notices().is_empty(), "the undo ran");
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(texts(&b.s.doc), ["OneBOB", "Two"], "Bob's typing stays; Alice's paste went");
    assert_converged(&a, &b, "after undoing a paste Bob typed into");
}

/// Undo never removes another person's text: Bob typed in a paragraph Alice's paste made, which
/// her Undo would remove whole, so it is refused with a message and nothing changes; the step
/// stays, and once Bob's typing is gone again the Undo works (review round 2, item 1).
#[test]
fn undo_of_a_paste_bob_typed_in_is_refused() {
    let (relay, mut a, mut b) = two(&["One", "Two"]);
    a.end_of(0);
    assert!(a.run("edit.paste", json!({"text": " pasted\nnew paragraph\nend", "privateClipboard": false})));
    settle(&relay, &mut [&mut a, &mut b]);
    b.caret(1, 3);
    assert!(b.run("text.insert", json!({"text": "NEW"})));
    settle(&relay, &mut [&mut a, &mut b]);
    let want = ["One pasted", "newNEW paragraph", "end", "Two"];
    let _ = a.run("edit.undo", json!({}));
    assert_eq!(a.le.take_notices(), [UNDO_TAKES_OTHERS]);
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(texts(&a.s.doc), want, "nothing was undone");
    assert_eq!(texts(&b.s.doc), want, "Bob keeps his typing");
    assert_converged(&a, &b, "after a refused undo");
    // Bob takes his typing back: Alice's Undo, still there, now runs.
    select(&mut b, (1, 3), (1, 6));
    assert!(b.run("text.delete", json!({})));
    settle(&relay, &mut [&mut a, &mut b]);
    assert!(a.run("edit.undo", json!({})));
    assert!(a.le.take_notices().is_empty());
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(texts(&b.s.doc), ["One", "Two"]);
    assert_converged(&a, &b, "after the undo ran");
}

/// The same for Enter: Bob typed in the paragraph Alice's Enter made, so undoing the Enter is
/// refused and Bob's text stays (review round 2, item 1).
#[test]
fn undo_of_enter_is_refused_when_bob_typed_in_the_new_paragraph() {
    let (relay, mut a, mut b) = two(&["Hello world"]);
    a.caret(0, 5);
    assert!(a.run("text.newParagraph", json!({})));
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(texts(&b.s.doc), ["Hello", " world"]);
    b.end_of(1);
    assert!(b.run("text.insert", json!({"text": " BOB"})));
    settle(&relay, &mut [&mut a, &mut b]);
    let _ = a.run("edit.undo", json!({}));
    assert_eq!(a.le.take_notices(), [UNDO_TAKES_OTHERS]);
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(texts(&b.s.doc), ["Hello", " world BOB"]);
    assert_converged(&a, &b, "after a refused undo of Enter");
}

/// The file's bytes: a paragraph, a 2x2 table, a paragraph.
fn file_with_a_table() -> Vec<u8> {
    let mut s = Session::new(Document::from_text("Before"));
    s.sel = Selection::caret(Pos::body(0, 6));
    s.run("insert.table", &json!({"rows": 2, "cols": 2})).unwrap();
    save_bytes("plan.docx", &s.doc).unwrap()
}

/// While live, a cut that would change a table (the whole table, a cell range, or a selection
/// across one) is refused with a message and changes nothing; Copy still works (review M2).
#[test]
fn a_cut_of_a_table_is_refused() {
    let bytes = file_with_a_table();
    let relay = relay(None);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    let mut b = Ed::open(&relay, "bob", Access::Edit, &bytes);
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(tables(&a.s.doc), 1);
    let at = a.s.doc.body.iter().position(|b| b.as_table().is_some()).unwrap();
    let before = a.s.doc.clone();
    let last = a.s.doc.body.len() - 1;
    // Across the table.
    select(&mut a, (0, 2), (last, 0));
    let err = a.le.run(&mut a.s, "edit.cut", &json!({})).unwrap_err().to_string();
    assert!(err.contains("Tables can't be changed while others are editing. Copy still works."), "{err}");
    assert!(before.same_content(&a.s.doc), "nothing cut");
    // The whole table.
    a.s.sel = Selection::caret(Pos { story: StoryRef::Body, path: wordcraft_doc::Path(vec![at as u32, 0, 0, 0]), off: 0 });
    assert!(a.run("table.selectTable", json!({})));
    let err = a.le.run(&mut a.s, "edit.cut", &json!({})).unwrap_err().to_string();
    assert!(err.contains("Tables can't be changed"), "{err}");
    assert!(before.same_content(&a.s.doc), "nothing cut");
    // Copy still works.
    assert!(a.run("edit.copy", json!({})));
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(tables(&b.s.doc), 1, "Bob keeps the table");
    assert_converged(&a, &b, "after a refused cut of a table");
}

/// Tracked changes are off while live, so a paste brings no tracked marks, even ones naming a
/// revision the document holds; text a tracked change deleted does not come (review M3).
#[test]
fn a_paste_brings_no_tracked_changes() {
    let mut s = Session::new(Document::from_text("Hello"));
    s.doc.settings.track_changes = true;
    s.sel = Selection::caret(Pos::body(0, 5));
    s.run("text.insert", &json!({"text": " tracked"})).unwrap();
    s.doc.settings.track_changes = false;
    assert!(!s.doc.revisions.is_empty(), "the file holds a revision");
    let bytes = save_bytes("plan.docx", &s.doc).unwrap();
    let relay = relay(None);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    let mut b = Ed::open(&relay, "bob", Access::Edit, &bytes);
    settle(&relay, &mut [&mut a, &mut b]);
    let ins = wordcraft_doc::CharProps { ins: Some(0), ..Default::default() };
    let del = wordcraft_doc::CharProps { del: Some(0), ..Default::default() };
    let mut p = Paragraph::with_text("Kept", ins);
    let _ = p.insert_text(p.text.len(), "Gone", &del);
    a.s.clipboard = Some(fragment(vec![p]));
    a.end_of(0);
    assert!(a.run("edit.paste", json!({})));
    assert_eq!(a.le.take_notices(), [PASTED_WITHOUT_FORMATTING]);
    settle(&relay, &mut [&mut a, &mut b]);
    let para = b.s.doc.body[0].as_para().unwrap().clone();
    assert!(para.text.ends_with("Kept"), "{:?}", para.text);
    assert!(!para.text.contains("Gone"), "deleted text does not come");
    let at = para.text.len() - 4;
    assert!(para.run_ranges().filter(|(r, _)| r.start >= at).all(|(_, c)| c.ins.is_none() && c.del.is_none()), "no tracked mark came");
    assert_converged(&a, &b, "after a paste with tracked marks");
}

/// A paragraph that held only a picture leaves no empty paragraph behind (review N3).
#[test]
fn a_picture_only_paragraph_leaves_no_empty_paragraph() {
    let (relay, mut a, mut b) = two(&["Hello", "World"]);
    let html = format!("<p>Before</p><p><img src=\"data:image/png;base64,{PIXEL}\"></p><p>After</p>");
    a.end_of(1);
    assert!(a.run("edit.paste", json!({"text": "Before\n\nAfter", "html": html})));
    assert_eq!(a.le.take_notices(), ["Pasted as text: pictures can't be shared while others are editing."]);
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(texts(&b.s.doc), ["Hello", "WorldBefore", "After"]);
    assert_converged(&a, &b, "after pasting around a picture");
}

/// A paste over a selection across paragraphs: one delete and insert, shared as one edit.
#[test]
fn pasting_over_a_selection_converges() {
    let (relay, mut a, mut b) = two(&["Hello world", "Second line", "Third"]);
    select(&mut a, (0, 6), (1, 6));
    assert!(a.run("edit.paste", json!({"text": "there\nnew", "privateClipboard": false})));
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(texts(&b.s.doc), ["Hello there", "new line", "Third"]);
    assert_converged(&a, &b, "after pasting over a selection");
    assert!(a.run("edit.undo", json!({})));
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(texts(&b.s.doc), ["Hello world", "Second line", "Third"]);
    assert_converged(&a, &b, "after undoing it");
}

/// The file's bytes: `text`, then a picture.
fn file_with_a_picture(text: &str) -> Vec<u8> {
    let mut s = Session::new(Document::from_text(text));
    s.sel = Selection::caret(Pos::body(0, text.len()));
    s.run("insert.picture", &json!({"data": PIXEL})).unwrap();
    save_bytes("plan.docx", &s.doc).unwrap()
}

/// A paste over a selection holding a picture would remove it: refused whole, saying why
/// (review M4).
#[test]
fn pasting_over_a_picture_is_refused_and_says_why() {
    let bytes = file_with_a_picture("Before");
    let relay = relay(None);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    let mut b = Ed::open(&relay, "bob", Access::Edit, &bytes);
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(objects(&a.s.doc), 1);
    let before = a.s.doc.clone();
    let end = a.s.doc.body[0].as_para().unwrap().text.len();
    select(&mut a, (0, 0), (0, end));
    let _ = a.run("edit.paste", json!({"text": "new", "privateClipboard": false}));
    assert_eq!(
        a.le.take_notices(),
        ["The selection has a picture or other object, which can't be removed while others are editing, so nothing was pasted."]
    );
    assert!(before.same_content(&a.s.doc), "nothing applied");
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(objects(&b.s.doc), 1, "Bob keeps the picture");
    assert_converged(&a, &b, "after a refused paste over a picture");
}

/// The file's bytes: one body paragraph, a header and a footnote.
fn file_with_a_header_and_a_note() -> Vec<u8> {
    let mut s = Session::new(Document::from_text("Body text"));
    s.sel = Selection::caret(Pos::body(0, 4));
    s.run("references.footnote", &json!({"text": "Note gamma"})).unwrap();
    s.sel = Selection::caret(Pos::body(0, 0));
    s.run("insert.header", &json!({"text": "Head alpha"})).unwrap();
    save_bytes("plan.docx", &s.doc).unwrap()
}

/// A paste into a header or a footnote changes what is not shared: refused whole, with a
/// message, nothing applied on either editor (review I4).
#[test]
fn a_paste_into_a_header_or_a_note_is_refused_whole() {
    let bytes = file_with_a_header_and_a_note();
    let relay = relay(None);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    let mut b = Ed::open(&relay, "bob", Access::Edit, &bytes);
    settle(&relay, &mut [&mut a, &mut b]);
    let stories: Vec<u32> = a.s.doc.parts.keys().copied().collect();
    assert!(stories.len() >= 2, "a header and a note: {stories:?}");
    for id in stories {
        let Some(path) = a.s.doc.para_paths(StoryRef::Part(id)).into_iter().next() else { continue };
        let before = a.s.doc.clone();
        a.s.sel = Selection::caret(Pos::new(StoryRef::Part(id), path, 0));
        let _ = a.run("edit.paste", json!({"text": "pasted", "privateClipboard": false}));
        let said = a.le.take_notices();
        assert_eq!(said, ["Headers, footers, notes and text boxes can't be changed while others are editing, so nothing was pasted."], "story {id}");
        assert!(before.same_content(&a.s.doc), "nothing applied in story {id}");
    }
    settle(&relay, &mut [&mut a, &mut b]);
    assert_converged(&a, &b, "after refused pastes into a header and a note");
    assert!(b.s.doc.same_content(&a.s.doc));
}

/// Paste: Merge Formatting and Paste Special run live (review I4).
#[test]
fn paste_merge_and_paste_special_run_live() {
    let (relay, mut a, mut b) = two(&["Hello", "World"]);
    let html = "<p><b>Merged</b> text</p>";
    a.end_of(0);
    assert!(a.run("edit.pasteMerge", json!({"text": "Merged text", "html": html})));
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(texts(&b.s.doc)[0], "HelloMerged text");
    assert_eq!(bold_text(&b.s.doc), "Merged", "Merge Formatting keeps the emphasis");
    // Paste Special, each format.
    select(&mut a, (1, 0), (1, 5));
    assert!(a.run("edit.copy", json!({})));
    for (fmt, params, want) in [
        ("formatted", json!({"as": "formatted"}), "World"),
        ("html", json!({"as": "html", "html": "<p><i>Html</i></p>"}), "Html"),
        ("rtf", json!({"as": "rtf", "rtf": r"{\rtf1\ansi {\b Rtf} words\par}"}), "Rtf words"),
        ("text", json!({"as": "text", "text": "Plain"}), "Plain"),
    ] {
        a.end_of(1);
        assert!(a.run("edit.pasteSpecial", params), "{fmt}");
        assert!(a.le.take_notices().is_empty(), "{fmt}: nothing to tell");
        settle(&relay, &mut [&mut a, &mut b]);
        assert!(texts(&b.s.doc).join("\n").contains(want), "{fmt}: {:?}", texts(&b.s.doc));
        assert_converged(&a, &b, fmt);
    }
    // A picture is not offered, and pasting one as a picture pastes nothing, saying so.
    let before = a.s.doc.clone();
    let offered = a.le.run(&mut a.s, "edit.pasteSpecial", &json!({"picture": PIXEL, "text": "x"})).unwrap();
    let ids: Vec<String> = offered["formats"].as_array().unwrap().iter().map(|f| f["id"].as_str().unwrap().to_owned()).collect();
    assert!(!ids.contains(&"picture".to_owned()), "{ids:?}");
    assert!(a.run("edit.pasteSpecial", json!({"as": "picture", "picture": PIXEL})));
    assert_eq!(a.le.take_notices(), ["Pictures can't be shared while others are editing, so nothing was pasted."]);
    assert!(before.same_content(&a.s.doc));
    settle(&relay, &mut [&mut a, &mut b]);
    assert_converged(&a, &b, "after Paste Special");
}

/// A section break pasted while live would bring its own page setup: it is left out (review M1).
#[test]
fn a_section_break_is_not_pasted() {
    let (relay, mut a, mut b) = two(&["Hello", "World"]);
    let mut first = Paragraph::with_text("Ends a section", Default::default());
    first.section = Some(Box::default());
    a.s.clipboard = Some(fragment(vec![first, Paragraph::with_text("After", Default::default())]));
    a.end_of(0);
    assert!(a.run("edit.paste", json!({})));
    assert_eq!(a.le.take_notices(), [PASTED_WITHOUT_FORMATTING]);
    settle(&relay, &mut [&mut a, &mut b]);
    assert!(b.s.doc.body.iter().filter_map(|b| b.as_para()).all(|p| p.section.is_none()), "no section break reached Bob");
    assert_converged(&a, &b, "after pasting a section break");
}

/// The text inside a pasted text box comes in as text (review M6).
#[test]
fn text_box_text_comes_in_as_text() {
    let mut s = Session::new(Document::from_text("Lead"));
    s.sel = Selection::caret(Pos::body(0, 4));
    s.run("insert.textBox", &json!({"text": "Box words"})).unwrap();
    s.sel = Selection { anchor: Pos::body(0, 0), focus: Pos::body(0, s.doc.body[0].as_para().unwrap().text.len()) };
    s.run("edit.copy", &json!({})).unwrap();
    let f = s.clipboard.clone().unwrap();
    assert!(!f.parts.is_empty(), "the copy holds the text box's story");
    let (relay, mut a, mut b) = two(&["Hello"]);
    a.s.clipboard = Some(f);
    a.end_of(0);
    assert!(a.run("edit.paste", json!({})));
    assert_eq!(a.le.take_notices(), ["Pasted as text: pictures can't be shared while others are editing."]);
    settle(&relay, &mut [&mut a, &mut b]);
    let all = texts(&b.s.doc).join("\n");
    assert!(all.contains("Box words"), "{all:?}");
    assert_eq!(objects(&b.s.doc), 0);
    assert_converged(&a, &b, "after pasting a text box");
}

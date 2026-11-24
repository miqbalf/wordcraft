use std::sync::Arc;

use serde_json::{Value, json};
use wordcraft_doc::para::InlineObject;
use wordcraft_doc::props::CharProps;
use wordcraft_doc::{Document, Paragraph, Path, Pos, RevisionKind, StoryRef, para_block};

use super::*;
use crate::Selection;

/// A session over plain paragraphs, without proofing marks.
fn session(paras: &[&str]) -> Session {
    let mut doc = Document::new();
    doc.body = paras.iter().map(|t| para_block(Paragraph::with_text(t, CharProps::default()))).collect();
    let mut s = Session::new(doc);
    s.view.proofing = false;
    s
}

fn para(s: &Session, i: usize) -> &Paragraph {
    s.doc.body[i].as_para().expect("a paragraph")
}

fn select(s: &mut Session, (b0, o0): (usize, usize), (b1, o1): (usize, usize)) {
    s.sel = Selection { anchor: Pos::body(b0, o0), focus: Pos::body(b1, o1) };
}

fn format(s: &mut Session, block: usize, from: usize, to: usize, f: &dyn Fn(&mut CharProps)) {
    s.doc.para_mut(StoryRef::Body, &Path::top(block)).unwrap().format(from, to, f).unwrap();
}

/// The source for the selection, then the proposal applied to it.
fn improve(s: &mut Session, proposal: &str) -> Result<Value, CmdError> {
    let src = s.run("review.improveSource", &json!({}))?;
    s.run("review.improveApply", &json!({"anchor": src["anchor"], "focus": src["focus"], "original": src["text"], "proposal": proposal}))
}

fn failed(r: Result<Value, CmdError>) -> String {
    match r {
        Err(CmdError::Failed(m)) => m,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// Each character of a paragraph with its formatting.
fn chars(p: &Paragraph) -> Vec<(char, CharProps)> {
    p.text.char_indices().map(|(i, c)| (c, p.props_of_char(i).clone())).collect()
}

fn find(p: &Paragraph, word: &str) -> usize {
    p.text.find(word).unwrap_or_else(|| panic!("{word:?} not in {:?}", p.text))
}

#[test]
fn improve_replaces_only_changed_words() {
    let mut s = session(&["she go to school yesterday", "Untouched paragraph."]);
    format(&mut s, 0, 0, 3, &|c| c.italic = Some(true));
    format(&mut s, 0, 17, 26, &|c| c.bold = Some(true));
    select(&mut s, (0, 2), (0, 2));
    let before = s.doc.clone();
    let src = s.run("review.improveSource", &json!({})).unwrap();
    assert_eq!(src["text"], "she go to school yesterday", "a caret means its whole paragraph");
    assert!(src["language"].is_string());
    improve(&mut s, "she went to school yesterday").unwrap();

    let (old, new) = (before.body[0].as_para().unwrap(), para(&s, 0));
    assert_eq!(new.text, "she went to school yesterday");
    let (oc, nc) = (chars(old), chars(new));
    let (go, went) = (find(old, "go"), find(new, "went"));
    assert_eq!(oc[..go], nc[..went], "the words before the change keep their formatting");
    assert_eq!(oc[go + 2..], nc[went + 4..], "the words after the change keep their formatting");
    assert!(new.run_ranges().all(|(_, c)| c.ins.is_none() && c.del.is_none()), "nothing tracked with Track Changes off");
    assert!(Arc::ptr_eq(&before.body[1], &s.doc.body[1]), "the other paragraph is the same block");
}

#[test]
fn untouched_words_keep_bold_and_link() {
    let mut s = session(&["a bold word and a link here"]);
    format(&mut s, 0, 2, 6, &|c| c.bold = Some(true));
    format(&mut s, 0, 18, 22, &|c| c.link = Some("https://example.com".into()));
    select(&mut s, (0, 0), (0, 27));
    improve(&mut s, "a bold new word and a link now here").unwrap();

    let p = para(&s, 0);
    assert_eq!(p.text, "a bold new word and a link now here");
    assert_eq!(p.props_of_char(find(p, "bold")).bold, Some(true), "the kept bold word is still bold");
    assert!(p.props_of_char(find(p, "link")).link.is_some(), "the kept link is still a link");
    assert_eq!(p.props_of_char(find(p, "new")).bold, Some(true), "a word inserted after the bold word is bold");
    assert!(p.props_of_char(find(p, "now")).link.is_none(), "a word inserted after the link is not linked");
}

#[test]
fn partial_selection_changes_only_the_selected_text() {
    let mut s = session(&["alpha thsi beta"]);
    select(&mut s, (0, 3), (0, 13));
    let src = s.run("review.improveSource", &json!({})).unwrap();
    assert_eq!(src["text"], para(&s, 0).text[3..13], "only the selected part is sent");
    improve(&mut s, "ha this be").unwrap();
    assert_eq!(para(&s, 0).text, "alpha this beta", "the text outside the selection is kept");
}

#[test]
fn one_undo_restores_the_paragraph() {
    let mut s = session(&["she go to scool yesterday ok"]);
    format(&mut s, 0, 0, 3, &|c| c.bold = Some(true));
    let before = s.doc.clone();
    improve(&mut s, "she went to school yesterday").unwrap();
    assert!(!s.doc.same_content(&before));
    s.run("edit.undo", &json!({})).unwrap();
    assert!(s.doc.same_content(&before), "one undo puts every hunk back: {:?}", para(&s, 0).text);
}

#[test]
fn track_changes_records_insertions_and_deletions() {
    let mut s = session(&["she go to school"]);
    s.run("review.trackChanges", &json!({"value": true})).unwrap();
    improve(&mut s, "she went to school").unwrap();
    let p = para(&s, 0);
    let rev = |i: u32| s.doc.revisions[i as usize].clone();
    let deleted: Vec<(String, RevisionKind, String)> =
        p.run_ranges().filter_map(|(r, c)| c.del.map(|d| (p.text[r].to_string(), rev(d).kind, rev(d).author))).collect();
    let inserted: Vec<(String, RevisionKind, String)> =
        p.run_ranges().filter_map(|(r, c)| c.ins.map(|i| (p.text[r].to_string(), rev(i).kind, rev(i).author))).collect();
    assert_eq!(deleted, [("go".to_string(), RevisionKind::Delete, s.author.clone())]);
    assert_eq!(inserted, [("went".to_string(), RevisionKind::Insert, s.author.clone())]);
    assert_eq!(p.final_text(), "she went to school", "accepted, it reads as the proposal");
}

#[test]
fn multi_paragraph_selection_is_improved_per_paragraph() {
    let mut s = session(&["one fsh", "two fish", "red fsh"]);
    let ptrs: Vec<*const wordcraft_doc::Block> = s.doc.body.iter().map(Arc::as_ptr).collect();
    select(&mut s, (0, 0), (2, 7));
    let src = s.run("review.improveSource", &json!({})).unwrap();
    assert_eq!(src["text"], "one fsh\n\ntwo fish\n\nred fsh");
    improve(&mut s, "one fish\r\n\r\ntwo fish\n \nred fish\n").unwrap();
    let texts: Vec<&str> = (0..3).map(|i| para(&s, i).text.as_str()).collect();
    assert_eq!(texts, ["one fish", "two fish", "red fish"], "each paragraph changed in place");
    assert_eq!(s.doc.body[1].as_ref() as *const _, ptrs[1], "the unchanged paragraph is the same block");

    // An empty paragraph in the range is not sent and stays.
    let mut s = session(&["one fsh", "", "red fsh"]);
    select(&mut s, (0, 0), (2, 7));
    improve(&mut s, "one fish\n\nred fish").unwrap();
    let texts: Vec<&str> = (0..3).map(|i| para(&s, i).text.as_str()).collect();
    assert_eq!(texts, ["one fish", "", "red fish"]);
}

#[test]
fn paragraph_count_change_is_refused_and_changes_nothing() {
    let mut s = session(&["one fsh", "two fsh"]);
    select(&mut s, (0, 0), (1, 7));
    let before = s.doc.clone();
    for proposal in ["one fish two fish", "one fish\n\ntwo\n\nfish", "one fish\n\n\n"] {
        assert_eq!(failed(improve(&mut s, proposal)), PARAGRAPHS_CHANGED, "{proposal:?}");
        assert!(s.doc.same_content(&before), "{proposal:?} changed nothing");
    }
}

#[test]
fn objects_and_breaks_are_refused() {
    let picture = |s: &mut Session| {
        let img = InlineObject::Image {
            media: "pic".into(),
            w: 10.0,
            h: 10.0,
            alt: String::new(),
            float: Default::default(),
            crop: [0.0; 4],
            fx: Default::default(),
            ole: None,
        };
        s.doc.para_mut(StoryRef::Body, &Path::top(0)).unwrap().insert_object(4, img, &CharProps::default()).unwrap();
    };
    let cases: [(&str, &dyn Fn(&mut Session)); 6] = [
        ("footnote", &|s| {
            select(s, (0, 4), (0, 4));
            s.run("references.footnote", &json!({"text": "note"})).unwrap();
        }),
        ("comment", &|s| {
            select(s, (0, 0), (0, 4));
            s.run("review.newComment", &json!({"text": "c"})).unwrap();
        }),
        ("field", &|s| {
            select(s, (0, 4), (0, 4));
            s.run("insert.field", &json!({"instr": "AUTHOR", "result": "x"})).unwrap();
        }),
        ("picture", &picture),
        ("bookmark", &|s| {
            select(s, (0, 0), (0, 4));
            s.run("insert.bookmark", &json!({"name": "here"})).unwrap();
        }),
        ("page break", &|s| {
            select(s, (0, 4), (0, 4));
            s.run("text.pageBreak", &json!({})).unwrap();
        }),
    ];
    for (what, setup) in cases {
        let mut s = session(&["Some text here"]);
        setup(&mut s);
        // Back in the body (a note leaves the caret in its story), then all of it.
        select(&mut s, (0, 0), (0, 0));
        s.run("select.all", &json!({})).unwrap();
        assert_eq!(failed(s.run("review.improveSource", &json!({}))), HAS_OBJECTS, "{what}");
    }
}

#[test]
fn existing_tracked_changes_are_refused() {
    for deletion in [false, true] {
        let mut s = session(&["Some text here"]);
        s.run("review.trackChanges", &json!({"value": true})).unwrap();
        if deletion {
            select(&mut s, (0, 0), (0, 5));
            s.run("text.delete", &json!({})).unwrap();
        } else {
            select(&mut s, (0, 0), (0, 0));
            s.run("text.insert", &json!({"text": "x"})).unwrap();
        }
        s.run("review.trackChanges", &json!({"value": false})).unwrap();
        s.run("select.all", &json!({})).unwrap();
        assert_eq!(failed(s.run("review.improveSource", &json!({}))), HAS_CHANGES, "deletion: {deletion}");
    }
}

#[test]
fn other_ranges_are_refused() {
    // Across table cells.
    let mut s = session(&[""]);
    s.run("insert.table", &json!({"rows": 1, "cols": 2})).unwrap();
    let t = s.doc.body.iter().position(|b| b.as_table().is_some()).expect("a table");
    let cell = |c: u32| Path(vec![t as u32, 0, c, 0]);
    for c in 0..2 {
        s.doc.para_mut(StoryRef::Body, &cell(c)).unwrap().insert_text(0, "cell text", &CharProps::default()).unwrap();
    }
    s.sel = Selection { anchor: Pos::new(StoryRef::Body, cell(0), 2), focus: Pos::new(StoryRef::Body, cell(1), 4) };
    assert_eq!(failed(s.run("review.improveSource", &json!({}))), ACROSS_CELLS);
    // Within one cell it works.
    s.sel = Selection { anchor: Pos::new(StoryRef::Body, cell(1), 0), focus: Pos::new(StoryRef::Body, cell(1), 9) };
    assert_eq!(s.run("review.improveSource", &json!({})).unwrap()["text"], "cell text");

    // White space only, and too long.
    let mut s = session(&["   \t "]);
    assert_eq!(failed(s.run("review.improveSource", &json!({}))), NO_TEXT);
    let long = "word ".repeat(IMPROVE_MAX_BYTES / 5 + 1);
    let mut s = session(&[long.as_str()]);
    assert_eq!(failed(s.run("review.improveSource", &json!({}))), TOO_LONG);
}

#[test]
fn changed_text_is_refused_at_apply() {
    let mut s = session(&["she go to school"]);
    let src = s.run("review.improveSource", &json!({})).unwrap();
    // Typing inside the range while the AI works.
    select(&mut s, (0, 4), (0, 4));
    s.run("text.insert", &json!({"text": "really "})).unwrap();
    let typed = s.doc.clone();
    let r = s.run(
        "review.improveApply",
        &json!({"anchor": src["anchor"], "focus": src["focus"], "original": src["text"], "proposal": "she went to school"}),
    );
    assert_eq!(failed(r), TEXT_CHANGED);
    assert!(s.doc.same_content(&typed), "nothing was replaced");
}

#[test]
fn summary_source_caps_at_204800_bytes_on_a_char_boundary() {
    let mut s = session(&[""]);
    assert_eq!(failed(s.run("review.summarySource", &json!({}))), EMPTY_DOCUMENT);

    let mut s = session(&["Short one.", "Two."]);
    let r = s.run("review.summarySource", &json!({})).unwrap();
    assert_eq!((r["text"].as_str(), r["truncated"].as_bool()), (Some(s.doc.final_text(StoryRef::Body).as_str()), Some(false)));

    // Two-byte characters after one byte: the cap falls inside a character.
    let big = format!("a{}", "é".repeat(SUMMARIZE_MAX_BYTES / 2 + 10));
    let mut s = session(&[big.as_str()]);
    let r = s.run("review.summarySource", &json!({})).unwrap();
    let text = r["text"].as_str().unwrap();
    assert!(text.len() <= SUMMARIZE_MAX_BYTES && text.len() + 4 > SUMMARIZE_MAX_BYTES, "{}", text.len());
    assert!(big.starts_with(text), "a prefix of the body");
    assert_eq!(r["truncated"], true);
}

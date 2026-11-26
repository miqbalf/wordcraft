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
    format(&mut s, 0, 4, 6, &|c| c.underline = Some(wordcraft_doc::props::Underline::Single));
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
    assert!(nc[went..went + 4].iter().all(|(_, c)| *c == oc[go].1), "the new word takes the formatting of the word it replaces");
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
    improve(&mut s, "ha these be").unwrap();
    assert_eq!(para(&s, 0).text, "alpha these beta", "the text outside the selection is kept");
    let (a, b) = s.sel.ordered();
    assert_eq!(&para(&s, 0).text[a.off..b.off], "ha these be", "the improved text is left selected");
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

#[test]
fn a_replaced_word_keeps_its_own_bold_and_link() {
    let mut s = session(&["The deadline is Friday, see the documnet now"]);
    format(&mut s, 0, 4, 12, &|c| c.bold = Some(true));
    let link = s.doc.body[0].as_para().unwrap().text.find("documnet").unwrap();
    format(&mut s, 0, link, link + 8, &|c| c.link = Some("https://example.com/doc".into()));
    select(&mut s, (0, 0), (0, 0));
    improve(&mut s, "The due date is Friday, see the document now").unwrap();
    let p = para(&s, 0);
    let (due, doc) = (find(p, "due date"), find(p, "document"));
    assert!((due..due + 8).all(|i| p.props_of_char(i).bold == Some(true)), "a rephrased bold word stays bold: {:?}", p.runs);
    let linked: Vec<Option<String>> = (doc..doc + 8).map(|i| p.props_of_char(i).link.clone()).collect();
    assert!(linked.iter().all(|l| l.as_deref() == Some("https://example.com/doc")), "the corrected word stays in its link: {linked:?}");
    assert!(p.props_of_char(find(p, " now")).link.is_none(), "the link does not grow past its word");
}

#[test]
fn inserted_words_skip_note_anchors_and_symbols_before_them() {
    let setups: [(&str, &dyn Fn(&mut Session)); 2] = [
        ("footnote", &|s| {
            select(s, (0, 16), (0, 16));
            s.run("references.footnote", &json!({"text": "Source."})).unwrap();
        }),
        ("symbol", &|s| {
            select(s, (0, 16), (0, 16));
            s.run("insert.symbol", &json!({"char": "\u{F0E0}", "font": "Wingdings"})).unwrap();
        }),
    ];
    for (what, setup) in setups {
        let mut s = session(&["It was measured. The tree go fast."]);
        setup(&mut s);
        let text = para(&s, 0).text.clone();
        let start = text.find("The").unwrap();
        let plain = para(&s, 0).props_of_char(start).clone();
        select(&mut s, (0, start), (0, text.len()));
        improve(&mut s, "Indeed The tree goes fast.").unwrap();
        let p = para(&s, 0);
        let at = find(p, "Indeed");
        let got = p.props_of_char(at);
        assert!(!matches!(got.style.as_deref(), Some("FootnoteReference" | "EndnoteReference")), "{what}: not a note number: {got:?}");
        assert!(got.sym_font.is_none() && got.font == plain.font, "{what}: not in the symbol's font: {got:?}");
        assert_eq!(*got, plain, "{what}: formatted as the text around it");
    }
}

#[test]
fn a_blank_line_inside_a_paragraph_is_refused_at_the_source() {
    let mut s = session(&["line one\n\nline two"]);
    assert_eq!(failed(s.run("review.improveSource", &json!({}))), BLANK_LINE);
    let mut s = session(&["line one\nline two"]);
    assert_eq!(s.run("review.improveSource", &json!({})).unwrap()["text"], "line one\nline two", "one line break is allowed");
}

#[test]
fn single_newlines_in_a_reply_are_spaces_unless_the_paragraph_had_line_breaks() {
    let mut s = session(&["she go to school"]);
    improve(&mut s, "she went to\nschool").unwrap();
    assert_eq!(para(&s, 0).text, "she went to school", "a wrapped reply is not a line break");
    let mut s = session(&["one fsh\ntwo fsh"]);
    improve(&mut s, "one fish\ntwo fish").unwrap();
    assert_eq!(para(&s, 0).text, "one fish\ntwo fish", "the paragraph's own line break stays");
}

#[test]
fn objects_and_control_characters_in_a_reply_are_left_out() {
    let mut s = session(&["she go to school"]);
    improve(&mut s, "she went\u{000B} to\u{FFFC}\u{000C} school\r").unwrap();
    let p = para(&s, 0);
    assert_eq!(p.text, "she went to school");
    assert!(p.objects.is_empty() && s.doc.body.len() == 1, "no object, break or new paragraph");
    let mut s = session(&["she go to school"]);
    improve(&mut s, "she went\rto school").unwrap();
    assert_eq!(para(&s, 0).text, "she went to school", "a lone carriage return is a line end, then a space");
}

#[test]
fn language_is_w2_text_language_english_and_indonesian_counting_together() {
    // French has the most bytes of any one language (12), but English and Indonesian together
    // (16) are `auto`, which then has the most.
    let mut s = session(&["bonjour mon hello, apa kabar"]);
    format(&mut s, 0, 0, 12, &|c| c.lang = Some("fr-FR".into()));
    format(&mut s, 0, 12, 19, &|c| c.lang = Some("en-US".into()));
    format(&mut s, 0, 19, 28, &|c| c.lang = Some("id-ID".into()));
    select(&mut s, (0, 0), (0, 0));
    assert_eq!(s.run("review.improveSource", &json!({})).unwrap()["language"], "auto");
    assert_eq!(s.run("review.summarySource", &json!({})).unwrap()["language"], "auto");
    // French alone is sent as itself.
    format(&mut s, 0, 0, 28, &|c| c.lang = Some("fr-FR".into()));
    assert_eq!(s.run("review.improveSource", &json!({})).unwrap()["language"], "fr-FR");
    assert_eq!(s.run("review.summarySource", &json!({})).unwrap()["language"], "fr-FR");
}

/// W4 ruling (F2): replaced text takes the first replaced character's formatting with its link,
/// white space included, so a changed space inside a link keeps one link.
#[test]
fn a_whitespace_change_inside_a_link_keeps_one_link() {
    let mut s = session(&["click\there now"]);
    format(&mut s, 0, 0, 10, &|c| c.link = Some("https://example.com".into()));
    improve(&mut s, "click here now").unwrap();
    let p = para(&s, 0);
    assert_eq!(p.text, "click here now");
    let end = find(p, "here") + 4;
    for i in 0..end {
        assert!(p.props_of_char(i).link.is_some(), "byte {i} of \"click here\" is still in the link");
    }
    assert!(p.props_of_char(find(p, "now")).link.is_none());
}

/// W4 ruling (F1): a replaced character in a legacy symbol font does not give that font to the
/// new words; the first replaced word character does, and with none the insertion rule applies.
#[test]
fn a_replaced_symbol_font_character_does_not_give_its_font() {
    let symbol = |c: &mut CharProps| c.font = Some("Wingdings".into());
    // The symbol starts a replaced word: the word's next characters give the formatting.
    let mut s = session(&["xDone now"]);
    format(&mut s, 0, 0, 1, &symbol);
    format(&mut s, 0, 1, 5, &|c| c.bold = Some(true));
    improve(&mut s, "Finished now").unwrap();
    let p = para(&s, 0);
    assert_eq!(p.text, "Finished now");
    let f = p.props_of_char(0);
    assert_ne!(f.font.as_deref(), Some("Wingdings"), "no symbol font");
    assert_eq!(f.bold, Some(true), "the first replaced word character's formatting");
    // Only a symbol is replaced: the new word is not in the symbol font either.
    let mut s = session(&["x Done"]);
    format(&mut s, 0, 0, 1, &symbol);
    improve(&mut s, "Ok Done").unwrap();
    let p = para(&s, 0);
    assert_eq!(p.text, "Ok Done");
    assert_ne!(p.props_of_char(0).font.as_deref(), Some("Wingdings"));
}

/// W6 shows the proposal as it will be applied: a reply wrapped at a fixed width reads as one
/// line, and a reply with another paragraph count is refused before Replace.
#[test]
fn improve_preview_is_the_text_as_it_will_be_applied() {
    let mut s = session(&["Teh cat sat on teh mat."]);
    let src = s.run("review.improveSource", &json!({})).unwrap();
    let preview = |s: &mut Session, proposal: &str| {
        s.run("review.improvePreview", &json!({"anchor": src["anchor"], "focus": src["focus"], "proposal": proposal}))
    };
    let shown = preview(&mut s, "The cat sat\non the mat.\u{7}").unwrap();
    let before = s.doc.clone();
    assert_eq!(shown["text"], "The cat sat on the mat.");
    assert!(s.doc.same_content(&before), "a preview changes nothing");
    improve(&mut s, "The cat sat\non the mat.\u{7}").unwrap();
    assert_eq!(para(&s, 0).text, shown["text"].as_str().unwrap_or(""), "what was shown is what was applied");
    let mut s = session(&["Teh cat sat."]);
    let src = s.run("review.improveSource", &json!({})).unwrap();
    let r = s.run("review.improvePreview", &json!({"anchor": src["anchor"], "focus": src["focus"], "proposal": "The cat.\n\nIt sat."}));
    assert_eq!(failed(r), PARAGRAPHS_CHANGED);
}

/// M8: a co-editor's edit above the range moves it; the same text is found again and replaced.
#[test]
fn replace_after_an_edit_above_finds_the_text_again() {
    let mut s = session(&["Intro.", "Teh cat sat."]);
    select(&mut s, (1, 0), (1, 0));
    let src = s.run("review.improveSource", &json!({})).unwrap();
    // A paragraph above, and words typed earlier in a paragraph above.
    select(&mut s, (0, 0), (0, 0));
    s.run("text.insert", &json!({"text": "Zero.", "paragraphs": true})).unwrap();
    s.run("text.newParagraph", &json!({})).unwrap();
    let apply = |s: &mut Session| {
        s.run("review.improveApply", &json!({"anchor": src["anchor"], "focus": src["focus"], "original": src["text"], "proposal": "The cat sat."}))
    };
    apply(&mut s).unwrap();
    let texts: Vec<String> = s.doc.body.iter().filter_map(|b| b.as_para()).map(|p| p.text.clone()).collect();
    assert_eq!(texts, ["Zero.", "Intro.", "The cat sat."]);
    // Inside the same paragraph, before the range.
    let mut s = session(&["Teh cat sat."]);
    select(&mut s, (0, 4), (0, 12));
    let src = s.run("review.improveSource", &json!({})).unwrap();
    select(&mut s, (0, 0), (0, 0));
    s.run("text.insert", &json!({"text": "So "})).unwrap();
    s.run("review.improveApply", &json!({"anchor": src["anchor"], "focus": src["focus"], "original": src["text"], "proposal": "cat sits."})).unwrap();
    assert_eq!(para(&s, 0).text, "So Teh cat sits.");
}

/// M8: the text is found again only when it is there once; otherwise nothing is replaced.
#[test]
fn a_moved_range_whose_text_is_there_twice_is_refused() {
    let mut s = session(&["Intro.", "Teh cat sat."]);
    select(&mut s, (1, 0), (1, 0));
    let src = s.run("review.improveSource", &json!({})).unwrap();
    select(&mut s, (0, 0), (0, 0));
    s.run("text.insert", &json!({"text": "Teh cat sat.", "paragraphs": true})).unwrap();
    s.run("text.newParagraph", &json!({})).unwrap();
    let before = s.doc.clone();
    let r =
        s.run("review.improveApply", &json!({"anchor": src["anchor"], "focus": src["focus"], "original": src["text"], "proposal": "The cat sat."}));
    assert_eq!(failed(r), TEXT_CHANGED);
    assert!(s.doc.same_content(&before));
}

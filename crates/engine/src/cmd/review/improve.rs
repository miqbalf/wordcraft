//! Improve with AI and Summarize Document: the text the host sends to the AI, and the AI's
//! proposal put back as a word-level diff (spec decision 6).
//!
//! Improve changes only the words the AI changed, so untouched words keep their character
//! formatting and links; inserted words take the formatting of the word before them. The whole
//! replacement runs inside one command: one undo step, one live edit, and tracked insertions and
//! deletions when Track Changes is on.

use serde_json::{Value, json};
use wordcraft_doc::para::{COLUMN_BREAK, OBJ, PAGE_BREAK};
use wordcraft_doc::props::CharProps;
use wordcraft_doc::{Paragraph, Path, Pos, StoryRef};
use wordcraft_layout::para::ProofRun;

use crate::cmd::tools::{Op, lcs_ops};
use crate::cmd::{delete_selection, parse_pos, pos_json, sel_result, type_text};
use crate::proof_remote::text_language;
use crate::{CmdError, CmdResult, Selection, Session, p};

/// The most text Improve sends (UTF-8 bytes of `text`).
pub const IMPROVE_MAX_BYTES: usize = 8192;
/// The most text Summarize sends (UTF-8 bytes of `text`).
pub const SUMMARIZE_MAX_BYTES: usize = 204_800;

/// Refusals, in English (the UI translates them).
pub const NO_TEXT: &str = "There is no text here to improve.";
pub const HAS_OBJECTS: &str = "Improve works on plain text only; this text holds objects such as notes, comments, fields or pictures.";
pub const HAS_CHANGES: &str = "Accept or reject the tracked changes in this text first.";
pub const ACROSS_CELLS: &str = "Select text within one cell or outside a table.";
pub const TOO_LONG: &str = "This text is too long to improve. Select less text.";
pub const TEXT_CHANGED: &str = "The text changed while the AI was working, so nothing was replaced.";
pub const PARAGRAPHS_CHANGED: &str = "The proposal changed the number of paragraphs, so nothing was replaced.";
pub const EMPTY_DOCUMENT: &str = "The document is empty.";
pub const BLANK_LINE: &str = "Improve can't keep an empty line inside a paragraph. Select text without it.";

/// Paragraphs are sent, and come back, separated by a blank line.
const SEPARATOR: &str = "\n\n";

/// Comparison table cells the word diff of one paragraph may fill.
const DIFF_BUDGET: u64 = 1_000_000;

/// One paragraph of the range: the bytes Improve sends and replaces (the selected part without
/// its leading and trailing white space, which stay as they are).
struct Part {
    path: Path,
    start: usize,
    end: usize,
}

/// What Improve works on: the range, its paragraphs and the text sent.
struct Source {
    story: StoryRef,
    start: Pos,
    end: Pos,
    parts: Vec<Part>,
    text: String,
    language: String,
}

/// The range Improve works on: the selection, or the caret's whole paragraph.
fn range(s: &Session, anchor: &Pos, focus: &Pos) -> (Pos, Pos) {
    let (a, b) = Selection { anchor: anchor.clone(), focus: focus.clone() }.ordered();
    if a != b {
        return (a, b);
    }
    let len = s.doc.para_at(&a).map(Paragraph::len).unwrap_or(0);
    (Pos { off: 0, ..a.clone() }, Pos { off: len, ..a })
}

/// The source of `anchor..focus`, or the refusal.
fn source(s: &Session, anchor: &Pos, focus: &Pos) -> Result<Source, &'static str> {
    let (a, b) = range(s, anchor, focus);
    if a.story != b.story {
        return Err(ACROSS_CELLS);
    }
    let paths = s.doc.para_paths_in(&a, &b);
    let Some(first) = paths.first() else { return Err(NO_TEXT) };
    // One container: the story's own paragraphs, or one table cell's.
    let parent = first.parent();
    if paths.iter().any(|q| q.parent() != parent) || a.path.parent() != parent || b.path.parent() != parent {
        return Err(ACROSS_CELLS);
    }
    let mut parts = Vec::new();
    let mut runs: Vec<ProofRun> = Vec::new();
    let last = paths.len().saturating_sub(1);
    for (k, path) in paths.iter().enumerate() {
        let Some(para) = s.doc.para(a.story, path) else { return Err(NO_TEXT) };
        let from = if *path == a.path { para.clamp(a.off) } else { 0 };
        let to = if *path == b.path { para.clamp(b.off) } else { para.len() };
        let selected = para.text.get(from..to.max(from)).unwrap_or("");
        if selected.contains([OBJ, PAGE_BREAK, COLUMN_BREAK]) {
            return Err(HAS_OBJECTS);
        }
        let tracked = para.run_ranges().any(|(r, c)| r.start < to && r.end > from && (c.ins.is_some() || c.del.is_some()));
        // The paragraph marks inside the range: all but the last paragraph's.
        let mark = k < last && (para.mark.ins.is_some() || para.mark.del.is_some());
        if tracked || mark {
            return Err(HAS_CHANGES);
        }
        let start = from + (selected.len() - selected.trim_start().len());
        let end = to - (selected.len() - selected.trim_end().len());
        if start >= end {
            // White space only (an empty paragraph between two others): not sent, left alone.
            continue;
        }
        // An empty line inside the paragraph would read as the paragraph separator, so no reply
        // could keep the paragraph count: refused before the AI is asked.
        if has_blank_line(para.text.get(start..end).unwrap_or("")) {
            return Err(BLANK_LINE);
        }
        // Each run cut to the bytes sent; `text_language` weighs them.
        for run in wordcraft_layout::para::proof_runs(&s.doc, para, None) {
            let range = run.range.start.max(start)..run.range.end.min(end);
            if range.start < range.end {
                runs.push(ProofRun { range, ..run });
            }
        }
        parts.push(Part { path: path.clone(), start, end });
    }
    if parts.is_empty() {
        return Err(NO_TEXT);
    }
    let mut text = String::new();
    for (k, part) in parts.iter().enumerate() {
        if k > 0 {
            text.push_str(SEPARATOR);
        }
        text.push_str(s.doc.para(a.story, &part.path).and_then(|q| q.text.get(part.start..part.end)).unwrap_or(""));
    }
    if text.len() > IMPROVE_MAX_BYTES {
        return Err(TOO_LONG);
    }
    Ok(Source { story: a.story, start: a, end: b, parts, text, language: language(&runs) })
}

/// The language sent for text made of `runs`: remote proofing's rule
/// ([`text_language`]), so the AI and the proofing service get the same language; `auto` when
/// no checked run has one.
fn language(runs: &[ProofRun]) -> String {
    text_language(runs).unwrap_or_else(|| "auto".to_string())
}

/// `review.improveSource`: the text Improve sends for the selection.
pub(super) fn improve_source(s: &mut Session, _: &Value) -> CmdResult {
    let src = source(s, &s.sel.anchor.clone(), &s.sel.focus.clone()).map_err(|m| CmdError::Failed(m.into()))?;
    Ok(json!({"text": src.text, "language": src.language, "anchor": pos_json(&src.start), "focus": pos_json(&src.end)}))
}

/// Whether `text` holds an empty line: a line end, then only white space, then another.
fn has_blank_line(text: &str) -> bool {
    let lines: Vec<&str> = text.split('\n').collect();
    lines.get(1..lines.len().saturating_sub(1)).is_some_and(|inner| inner.iter().any(|l| l.trim().is_empty()))
}

/// The proposal's paragraphs, each as its lines: line ends normalised, the whole trimmed, split
/// on blank lines. Characters that would split a paragraph or stand for an object or a break are
/// left out.
fn proposal_paragraphs(proposal: &str) -> Vec<Vec<String>> {
    let text = proposal.replace("\r\n", "\n").replace(['\r', '\u{2028}', '\u{2029}'], "\n");
    let clean: String = text.chars().filter(|c| *c == '\n' || *c == '\t' || (!c.is_control() && *c != OBJ)).collect();
    let mut out: Vec<Vec<String>> = Vec::new();
    let mut cur: Vec<String> = Vec::new();
    for line in clean.trim().split('\n') {
        if line.trim().is_empty() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(line.to_string());
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// One paragraph of the proposal as text for a part: its lines joined by a line break when the
/// part had line breaks of its own, else by a space (a reply wrapped at a fixed width is not a
/// line break), then trimmed.
fn paragraph_text(lines: &[String], line_breaks: bool) -> String {
    if line_breaks { lines.join("\n").trim().to_string() } else { lines.iter().map(|l| l.trim()).collect::<Vec<_>>().join(" ").trim().to_string() }
}

/// A word and the white space after it, as byte ranges of the text (punctuation stays with its
/// word).
struct Token {
    start: usize,
    word_end: usize,
    end: usize,
}

fn tokens(text: &str) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::new();
    let mut in_space = true;
    for (i, c) in text.char_indices() {
        let space = c.is_whitespace();
        if !space && in_space {
            if let Some(t) = out.last_mut() {
                t.end = i;
            }
            out.push(Token { start: i, word_end: text.len(), end: text.len() });
        } else if space
            && !in_space
            && let Some(t) = out.last_mut()
        {
            t.word_end = i;
        }
        in_space = space;
    }
    if let Some(t) = out.last_mut() {
        t.end = text.len();
    }
    // Leading white space (none in a trimmed part) belongs to no word: give it its own token.
    if out.first().is_some_and(|t| t.start > 0) || (out.is_empty() && !text.is_empty()) {
        let end = out.first().map_or(text.len(), |t| t.start);
        out.insert(0, Token { start: 0, word_end: 0, end });
    }
    out
}

/// The changes between `old` and `new`: (old byte range, new text), in order, not overlapping.
///
/// Words are compared by their text; a word kept with different white space after it changes
/// only that white space. Neighbouring changes are merged, and white space both sides end with
/// is left out, so a replaced word keeps the space after it.
fn hunks(old: &str, new: &str) -> Vec<(usize, usize, String)> {
    let (ot, nt) = (tokens(old), tokens(new));
    let word = |text: &str, t: &Token| text.get(t.start..t.word_end).unwrap_or("").to_string();
    let ow: Vec<String> = ot.iter().map(|t| word(old, t)).collect();
    let nw: Vec<String> = nt.iter().map(|t| word(new, t)).collect();
    let mut budget = DIFF_BUDGET;
    let ops = lcs_ops(&ow, &nw, &mut budget);
    let mut out: Vec<(usize, usize, String)> = Vec::new();
    let mut push = |x: usize, y: usize, text: &str| match out.last_mut() {
        Some(last) if last.1 == x => {
            last.1 = y;
            last.2.push_str(text);
        }
        _ => out.push((x, y, text.to_string())),
    };
    let (mut i, mut j) = (0usize, 0usize);
    for op in ops {
        let at = ot.get(i).map_or(old.len(), |t| t.start);
        match op {
            Op::Same => {
                if let (Some(a), Some(b)) = (ot.get(i), nt.get(j)) {
                    let (ws_old, ws_new) = (old.get(a.word_end..a.end).unwrap_or(""), new.get(b.word_end..b.end).unwrap_or(""));
                    if ws_old != ws_new {
                        push(a.word_end, a.end, ws_new);
                    }
                }
                i += 1;
                j += 1;
            }
            Op::Del => {
                if let Some(a) = ot.get(i) {
                    push(a.start, a.end, "");
                }
                i += 1;
            }
            Op::Ins => {
                if let Some(b) = nt.get(j) {
                    push(at, at, new.get(b.start..b.end).unwrap_or(""));
                }
                j += 1;
            }
        }
    }
    for h in &mut out {
        // White space both the old and the new text end with stays.
        while let (Some(o), Some(n)) = (old.get(h.0..h.1).and_then(|t| t.chars().last()), h.2.chars().last())
            && o == n
            && o.is_whitespace()
        {
            h.1 -= o.len_utf8();
            h.2.pop();
        }
    }
    out.retain(|h| h.0 != h.1 || !h.2.is_empty());
    out
}

/// Formatting as the editor gives typed text: never a note number's style, a symbol's own font
/// or a revision mark.
fn typed(mut c: CharProps) -> CharProps {
    if matches!(c.style.as_deref(), Some("FootnoteReference" | "EndnoteReference")) {
        c.style = None;
    }
    c.ins = None;
    c.del = None;
    c.fmt_change = None;
    c.rtl = None;
    c.without_symbol()
}

/// Whether the character at `i` is part of a word whose formatting new words may take: not
/// white space, an object (a note anchor, a field…) or a symbol.
fn is_word_char(p: &Paragraph, i: usize, c: char) -> bool {
    if c.is_whitespace() || c == OBJ {
        return false;
    }
    let props = p.props_of_char(i);
    let symbol = props.sym_font.as_deref().is_some_and(|f| !f.trim().is_empty())
        || props.font.as_deref().is_some_and(wordcraft_doc::symbol_fonts::is_symbol_font);
    !symbol && !matches!(props.style.as_deref(), Some("FootnoteReference" | "EndnoteReference"))
}

/// The formatting of the text replacing `x..y`.
///
/// Text that replaces words takes the formatting of the first word it replaces, link included,
/// as typing over a selection does: a rephrased bold word stays bold, a corrected word in a link
/// stays in it. Inserted words take the formatting of the word before them (past white space,
/// objects and symbols), without its link; at the paragraph's start, that of the text after.
fn props_for(p: &Paragraph, x: usize, y: usize) -> CharProps {
    let replaced = p.text.get(x..y).unwrap_or("");
    if let Some((i, _)) = replaced.char_indices().find(|(_, c)| !c.is_whitespace()) {
        return typed(p.props_of_char(x + i).clone());
    }
    let before = p.text.get(..x).unwrap_or("");
    let word = before.char_indices().rev().find(|(i, c)| is_word_char(p, *i, *c)).map(|(i, _)| i);
    let props = match word {
        Some(i) => p.props_of_char(i),
        None if x < p.len() => p.props_of_char(x),
        None => p.props_at(x),
    };
    typed(props.unlinked())
}

/// `review.improveApply`: replace the range's text with the proposal, word by word.
pub(super) fn improve_apply(s: &mut Session, v: &Value) -> CmdResult {
    let pos = |k: &str| v.get(k).and_then(parse_pos).ok_or_else(|| CmdError::Params(format!("`{k}` (Pos) is required")));
    let (anchor, focus) = (pos("anchor")?, pos("focus")?);
    let original = p::req_str(v, "original")?;
    let proposal = p::req_str(v, "proposal")?;
    let changed = || CmdError::Failed(TEXT_CHANGED.into());
    let valid = |x: &Pos| s.doc.para_at(x).is_some_and(|q| x.off <= q.len() && q.text.is_char_boundary(x.off));
    if !valid(&anchor) || !valid(&focus) {
        return Err(changed());
    }
    let src = source(s, &anchor, &focus).map_err(|_| changed())?;
    if src.text != original {
        return Err(changed());
    }
    let pieces = proposal_paragraphs(proposal);
    if pieces.len() != src.parts.len() {
        return Err(CmdError::Failed(PARAGRAPHS_CHANGED.into()));
    }
    let paras: Vec<String> = src
        .parts
        .iter()
        .zip(&pieces)
        .map(|(part, lines)| {
            let own = s.doc.para(src.story, &part.path).and_then(|q| q.text.get(part.start..part.end)).is_some_and(|t| t.contains('\n'));
            paragraph_text(lines, own)
        })
        .collect();
    if paras.iter().any(String::is_empty) {
        return Err(CmdError::Failed(PARAGRAPHS_CHANGED.into()));
    }
    let end_len = s.doc.para_at(&src.end).map(Paragraph::len).unwrap_or(0);
    s.goal_x = None;
    for (part, new) in src.parts.iter().zip(&paras).rev() {
        let at = |off: usize| Pos { story: src.story, path: part.path.clone(), off };
        let old = s.doc.para(src.story, &part.path).and_then(|q| q.text.get(part.start..part.end)).unwrap_or("").to_string();
        for (x, y, text) in hunks(&old, new).into_iter().rev() {
            let (x, y) = (part.start + x, part.start + y);
            let props = s.doc.para(src.story, &part.path).map(|q| props_for(q, x, y)).unwrap_or_default();
            s.sel = Selection { anchor: at(x), focus: at(y) };
            s.pending = Some(props);
            if text.is_empty() {
                delete_selection(s)?;
            } else {
                type_text(s, &text)?;
            }
        }
    }
    s.pending = None;
    // The improved range is left selected.
    let grown = s.doc.para_at(&src.end).map(Paragraph::len).unwrap_or(0);
    let end_off = (src.end.off + grown).saturating_sub(end_len);
    let end_off = s.doc.para_at(&src.end).map_or(end_off, |q| q.clamp(end_off));
    s.sel = Selection { anchor: src.start, focus: Pos { off: end_off, ..src.end } };
    sel_result(s)
}

/// `review.summarySource`: the body's text, as it reads with every change accepted, cut at a
/// character boundary to [`SUMMARIZE_MAX_BYTES`].
pub(super) fn summary_source(s: &mut Session, _: &Value) -> CmdResult {
    let mut text = s.doc.final_text(StoryRef::Body);
    if text.trim().is_empty() {
        return Err(CmdError::Failed(EMPTY_DOCUMENT.into()));
    }
    let truncated = text.len() > SUMMARIZE_MAX_BYTES;
    if truncated {
        let mut cut = SUMMARIZE_MAX_BYTES;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
    }
    let mut runs: Vec<ProofRun> = Vec::new();
    for path in s.doc.para_paths(StoryRef::Body) {
        let Some(para) = s.doc.para(StoryRef::Body, &path) else { continue };
        runs.extend(wordcraft_layout::para::proof_runs(&s.doc, para, None));
    }
    Ok(json!({"text": text, "language": language(&runs), "truncated": truncated}))
}

#[cfg(test)]
mod tests;

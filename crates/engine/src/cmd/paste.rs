//! Paste Special: paste the clipboard in a chosen format (formatted, RTF, HTML, picture,
//! unformatted), and the formatted content Paste and Merge Formatting take from other programs.
//!
//! The clipboard can hold several representations of the same content. WordCraft's own copy keeps
//! a rich fragment; the system clipboard gives text, and callers (agents, the CLI, a front end
//! with a richer clipboard) may pass `html`, `rtf` or a `picture` too. Text that is itself RTF or
//! HTML source can also be pasted as that format. Paste keeps the source formatting of another
//! program's HTML or RTF; Merge Formatting keeps only its emphasis (#687).
//! Plain text has no formatting to keep, so Paste inserts it as Keep Text Only does.

use std::sync::Arc;

use serde_json::{Value, json};
use wordcraft_doc::edit::Fragment;
use wordcraft_doc::{Block, Document, Paragraph};

use super::{delete_selection, sel_result};
use crate::{CmdError, CmdResult, CommandSpec, Selection, Session, p};

/// Largest clipboard payload Paste Special accepts, in bytes.
pub const MAX_PASTE_BYTES: usize = 16 << 20;
/// Largest clipboard picture Paste Special accepts (base64), in bytes.
pub const MAX_PICTURE_BYTES: usize = 96 << 20;
/// Nesting depth we follow when adjusting pasted tables.
const MAX_DEPTH: usize = 64;

/// The formats, in the order the dialog lists them: (id, label).
pub const FORMATS: [(&str, &str); 5] = [
    ("formatted", "Formatted Text"),
    ("rtf", "Rich Text Format (RTF)"),
    ("html", "HTML Format"),
    ("picture", "Picture"),
    ("text", "Unformatted Text"),
];

pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec::new("edit.pasteSpecial", "Paste Special", "Home › Clipboard › Paste", paste_special)
            .key("Mod+Alt+V")
            .params(r#"{"as"?: "formatted"|"rtf"|"html"|"picture"|"text", "text"?: string, "html"?: string, "rtf"?: string, "picture"?: base64, "privateClipboard"?: bool}"#),
    ]
}

/// Clipboard text from the params (system clipboard), else WordCraft's own copy.
fn clip_text(s: &Session, v: &Value) -> Option<String> {
    match p::str(v, "text") {
        Some(t) => Some(t.replace("\r\n", "\n")),
        None => s.clipboard.as_ref().map(Fragment::plain_text),
    }
}

fn looks_like_rtf(t: &str) -> bool {
    t.trim_start().starts_with("{\\rtf")
}

fn looks_like_html(t: &str) -> bool {
    let t = t.trim_start();
    t.starts_with('<') && (t.contains("</") || t.contains("/>"))
}

/// Whether the system clipboard still holds this window's copy. A front end that can check says
/// so in `privateClipboard`; callers that cannot (an agent, the CLI) omit it, and the session's
/// copy is used as before.
fn owns_clipboard(v: &Value) -> bool {
    p::bool(v, "privateClipboard").unwrap_or(true)
}

/// The source for a format, if the clipboard has it.
fn source(s: &Session, v: &Value, fmt: &str) -> Option<String> {
    let non_empty = |t: String| if t.is_empty() { None } else { Some(t) };
    match fmt {
        "formatted" if owns_clipboard(v) => {
            let f = s.clipboard.as_ref()?;
            // System clipboard text that isn't our own copy means someone else copied since.
            match p::str(v, "text") {
                Some(t) if f.clipboard_text() != t.replace("\r\n", "\n") => None,
                _ => Some(String::new()),
            }
        }
        "formatted" => None,
        "rtf" => p::str(v, "rtf").map(str::to_string).or_else(|| clip_text(s, v).filter(|t| looks_like_rtf(t))).and_then(non_empty),
        "html" => p::str(v, "html").map(str::to_string).or_else(|| clip_text(s, v).filter(|t| looks_like_html(t))).and_then(non_empty),
        "picture" => p::str(v, "picture").map(str::to_string).and_then(non_empty),
        "text" => clip_text(s, v).and_then(non_empty),
        _ => None,
    }
}

/// Formats the clipboard can be pasted as, best first.
pub fn available(s: &Session, v: &Value) -> Vec<&'static str> {
    // While live a picture can't be shared, so the dialog doesn't offer it.
    FORMATS.iter().map(|(id, _)| *id).filter(|id| !(*id == "picture" && s.editing_live())).filter(|id| source(s, v, id).is_some()).collect()
}

fn formats_json(ids: &[&str]) -> Value {
    json!(ids.iter().map(|id| json!({"id": id, "label": FORMATS.iter().find(|(f, _)| f == id).map(|(_, l)| *l).unwrap_or(*id)})).collect::<Vec<_>>())
}

fn paste_special(s: &mut Session, v: &Value) -> CmdResult {
    for k in ["text", "html", "rtf"] {
        if p::str(v, k).is_some_and(|t| t.len() > MAX_PASTE_BYTES) {
            return Err(CmdError::Params(format!("`{k}` is larger than {} MB", MAX_PASTE_BYTES >> 20)));
        }
    }
    if p::str(v, "picture").is_some_and(|t| t.len() > MAX_PICTURE_BYTES) {
        return Err(CmdError::Params(format!("`picture` is larger than {} MB", MAX_PICTURE_BYTES >> 20)));
    }
    let ids = available(s, v);
    let Some(fmt) = p::str(v, "as") else {
        // No format chosen: list them and ask a front end (if any) to show the dialog.
        let mut req = json!({"open": "pasteSpecial", "formats": ids});
        for k in ["text", "html", "rtf", "picture"] {
            if let Some(t) = p::str(v, k) {
                req[k] = json!(t);
            }
        }
        // Keep the ownership result so the dialog's list is the same as this one.
        if let Some(own) = p::bool(v, "privateClipboard") {
            req["privateClipboard"] = json!(own);
        }
        // Only the newest dialog request matters; older ones would pin their payloads.
        s.ui_requests.retain(|r| r["open"] != "pasteSpecial");
        s.push_ui_request(req);
        return Ok(json!({"formats": formats_json(&ids)}));
    };
    let fmt = match fmt.to_ascii_lowercase().as_str() {
        "formatted" | "keep" | "wordcraft" => "formatted",
        "rtf" => "rtf",
        "html" => "html",
        "picture" | "png" | "image" => "picture",
        "text" | "unformatted" | "plain" => "text",
        other => return Err(CmdError::Params(format!("unknown format `{other}` (formatted, rtf, html, picture or text)"))),
    };
    let Some(src) = source(s, v, fmt) else {
        return Err(CmdError::Failed(if ids.is_empty() {
            "the clipboard is empty".into()
        } else {
            format!("the clipboard has no {fmt} content (available: {})", ids.join(", "))
        }));
    };
    let frag = match fmt {
        "text" => {
            super::paste_text(s, &src.replace("\r\n", "\r").replace('\n', "\r"))?;
            let mut r = sel_result(s)?;
            r["pastedAs"] = json!(fmt);
            return Ok(r);
        }
        // A picture is not shared while live: nothing is pasted, and the result says why.
        "picture" if s.editing_live() => {
            let mut r = pasted_result(s, Lost { pictures: true, nothing_left: true, ..Lost::default() })?;
            r["pastedAs"] = json!(fmt);
            return Ok(r);
        }
        "picture" => {
            let mut r = super::insert::picture(s, &json!({"data": src}))?;
            r["pastedAs"] = json!(fmt);
            return Ok(r);
        }
        "formatted" => s.clipboard.clone().ok_or_else(|| CmdError::Failed("the clipboard is empty".into()))?,
        "html" => adopt(s, import_html(&src)),
        _ => adopt(s, wordcraft_formats::rtf::import(src.as_bytes()).map_err(|e| CmdError::Failed(format!("bad RTF: {e}")))?),
    };
    let lost = insert_pasted(s, frag)?;
    let mut r = pasted_result(s, lost)?;
    r["pastedAs"] = json!(fmt);
    Ok(r)
}

/// Indent each side of a pasted `<blockquote>` whose page gives it no margins, points.
const BLOCKQUOTE_INDENT: f32 = 36.0;

/// Another program's HTML clipboard payload as a document. A `<blockquote>` there is how
/// LibreOffice (and many web pages) write a paragraph indented on both sides, not a quotation
/// (#1270): it pastes as a Normal paragraph indented by the page's `blockquote` margins, else half
/// an inch each side, rather than in the (centred, italic) Quote style. Opening an .html file keeps
/// `<blockquote>` as Quote, so HTML round-trips.
fn import_html(html: &str) -> Document {
    let mut doc = wordcraft_formats::html::import(html_fragment(html));
    let (left, right) = blockquote_margins(html);
    for b in &mut doc.body {
        each_para(Arc::make_mut(b), 0, &mut |p| {
            if p.props.style.as_deref() == Some("Quote") {
                p.props.style = None;
                p.props.indent_left.get_or_insert(left);
                p.props.indent_right.get_or_insert(right);
                p.touch();
            }
        });
    }
    doc
}

/// The left and right margins (points) the payload's `<style>` sheets give `blockquote` — LibreOffice
/// writes `blockquote { margin-left: 1cm; margin-right: 1cm }` — else [`BLOCKQUOTE_INDENT`].
fn blockquote_margins(html: &str) -> (f32, f32) {
    let len = |v: &str| wordcraft_formats::html::css_len_pt(v).map(|x| x.clamp(0.0, 1584.0));
    let (mut left, mut right) = (BLOCKQUOTE_INDENT, BLOCKQUOTE_INDENT);
    let lower = html.to_ascii_lowercase();
    let mut rest = lower.as_str();
    while let Some(at) = rest.find("<style") {
        let tag = rest.get(at..).unwrap_or("");
        let sheet = tag.find('>').and_then(|j| tag.get(j + 1..)).unwrap_or("");
        let end = sheet.find("</style").unwrap_or(sheet.len());
        for rule in sheet.get(..end).unwrap_or("").split('}') {
            let Some((sel, decls)) = rule.split_once('{') else { continue };
            if !sel.split(',').any(|s| s.trim().trim_start_matches("<!--").trim() == "blockquote") {
                continue;
            }
            for d in decls.split(';') {
                let Some((name, v)) = d.split_once(':') else { continue };
                match name.trim() {
                    "margin-left" => left = len(v).unwrap_or(left),
                    "margin-right" => right = len(v).unwrap_or(right),
                    "margin" => {
                        let vals: Vec<&str> = v.split_whitespace().take(4).collect();
                        let (r, l) = match vals.as_slice() {
                            [a] => (*a, *a),
                            [_, h] | [_, h, _] => (*h, *h),
                            [_, r, _, l] => (*r, *l),
                            _ => continue,
                        };
                        right = len(r).unwrap_or(right);
                        left = len(l).unwrap_or(left);
                    }
                    _ => {}
                }
            }
        }
        rest = sheet.get(end..).unwrap_or("");
    }
    (left, right)
}

/// The HTML itself from a Windows "HTML Format" (CF_HTML) clipboard payload — a `Version:` header
/// giving byte offsets — or the text unchanged. Hostile offsets fall back to the text after the
/// header.
pub fn html_fragment(html: &str) -> &str {
    if !html.starts_with("Version:") {
        return html;
    }
    let offset = |key: &str| -> Option<usize> {
        let at = html.find(key)?.checked_add(key.len())?;
        let digits: &str = html.get(at..)?.split(['\r', '\n']).next()?;
        digits.trim().parse().ok()
    };
    if let (Some(a), Some(b)) = (offset("StartFragment:"), offset("EndFragment:"))
        && let Some(frag) = html.get(a..b)
    {
        return frag;
    }
    html.find('<').and_then(|i| html.get(i..)).unwrap_or("")
}

/// Whether an imported document has anything to paste: text, a table, or a picture it carries.
fn has_content(d: &Document) -> bool {
    d.body.iter().any(|b| match &**b {
        Block::Para(p) => !p.text.trim().is_empty() || (!p.objects.is_empty() && !d.media.is_empty()),
        Block::Table(_) => true,
    })
}

/// Another program's formatted copy, from the system clipboard's HTML, else its RTF (#687), as a
/// fragment of this document. `None` when neither is given, either is larger than
/// [`MAX_PASTE_BYTES`], or it holds nothing to paste (the caller pastes the text then).
pub fn rich_fragment(s: &mut Session, v: &Value) -> Option<Fragment> {
    let fits = |k: &str| p::str(v, k).filter(|t| !t.trim().is_empty() && t.len() <= MAX_PASTE_BYTES);
    let doc = fits("html").map(import_html);
    let doc = match doc.filter(has_content) {
        Some(d) => d,
        None => {
            let rtf = fits("rtf").filter(|t| looks_like_rtf(t))?;
            wordcraft_formats::rtf::import(rtf.as_bytes()).ok().filter(has_content)?
        }
    };
    Some(adopt(s, doc))
}

/// What a Paste brings in.
pub enum Pasted {
    /// WordCraft's own copy, or another program's HTML or RTF: it keeps the formatting it carries.
    Fragment(Fragment),
    /// The system clipboard's text, lines separated by `\n`.
    Text(String),
}

/// What Paste inserts: WordCraft's own copy while the system clipboard still holds it, else
/// another program's formatted copy (HTML, RTF), else the system clipboard's text. A front end
/// that knows another program owns the clipboard passes `privateClipboard: false`, which keeps
/// the fragment out even when the text matches.
pub fn paste_content(s: &mut Session, v: &Value) -> Result<Pasted, CmdError> {
    let text = p::str(v, "text").map(|t| t.replace("\r\n", "\n"));
    if owns_clipboard(v)
        && let Some(f) = &s.clipboard
        && text.as_ref().is_none_or(|t| f.clipboard_text() == *t)
        && (text.is_some() || (p::str(v, "html").is_none() && p::str(v, "rtf").is_none()))
    {
        return Ok(Pasted::Fragment(f.clone()));
    }
    if let Some(f) = rich_fragment(s, v) {
        return Ok(Pasted::Fragment(f));
    }
    match text {
        Some(t) if !t.is_empty() => Ok(Pasted::Text(t)),
        _ => Err(CmdError::Failed("the clipboard is empty".into())),
    }
}

/// [`paste_content`] as a fragment. Text becomes paragraphs with no formatting of their own.
pub fn paste_fragment(s: &mut Session, v: &Value) -> Result<Fragment, CmdError> {
    Ok(match paste_content(s, v)? {
        Pasted::Fragment(f) => f,
        Pasted::Text(t) => Fragment::from_text(&t),
    })
}

/// Paste plain text the way Keep Text Only does, which is what Word does with text that carries
/// no formatting: the text takes the formatting before the caret, and each line after the first
/// starts a paragraph formatted like the one the text lands in (direction, style, alignment,
/// indents, list). Empty lines become empty paragraphs.
pub fn insert_text(s: &mut Session, text: &str) -> Result<(), CmdError> {
    // Only our own cut, pasted straight after it, becomes a move (see `insert`).
    s.pending_move = None;
    super::paste_text(s, &text.replace("\r\n", "\r").replace('\n', "\r"))
}

/// Merge Formatting (#687): the pasted text takes the formatting where it lands — the paragraph's
/// and the caret's — keeping only its emphasis (bold, italic, underline, strikethrough).
pub fn merge_formatting(s: &Session, frag: &mut Fragment) {
    let mut base = s.typing_props();
    base.ins = None;
    base.del = None;
    base.fmt_change = None;
    let (start, _) = s.sel.ordered();
    let para = s.doc.para_at(&start).map(|p| p.props.clone()).unwrap_or_default();
    for b in &mut frag.blocks {
        each_para(b, 0, &mut |p| {
            p.props = para.clone();
            for run in &mut p.runs {
                let src = std::mem::replace(&mut run.props, base.clone());
                if src.bold == Some(true) {
                    run.props.bold = Some(true);
                }
                if src.italic == Some(true) {
                    run.props.italic = Some(true);
                }
                if src.underline.is_some_and(|u| u != wordcraft_doc::props::Underline::None) {
                    run.props.underline = src.underline;
                }
                if src.strike == Some(true) {
                    run.props.strike = Some(true);
                }
                run.props.link = src.link;
                // Tracked changes are not formatting: text deleted in the source stays deleted.
                run.props.ins = src.ins;
                run.props.del = src.del;
            }
            p.touch();
        });
    }
}

/// A paste: [`insert`], with the fragment first kept to what live editing shares
/// ([`live_text`]) while editing live ([`Session::editing_live`]). Returns what that took out;
/// when nothing is left ([`Lost::nothing_left`]) nothing is inserted and the selection stays.
pub fn insert_pasted(s: &mut Session, frag: Fragment) -> Result<Lost, CmdError> {
    if !s.editing_live() {
        insert(s, frag)?;
        return Ok(Lost::default());
    }
    let (frag, lost) = live_text(&s.doc, frag);
    if !lost.nothing_left {
        insert(s, frag)?;
    }
    Ok(lost)
}

/// A paste's result: the selection, and while live what [`live_text`] took out (`lost`) and
/// `pasted: false` when nothing was left to paste.
pub fn pasted_result(s: &Session, lost: Lost) -> CmdResult {
    let mut r = sel_result(s)?;
    if lost.any() {
        r["lost"] = lost.to_json();
    }
    if lost.nothing_left {
        r["pasted"] = json!(false);
    }
    Ok(r)
}

/// Replace the selection with `frag`, marking it as an insertion under Track Changes.
pub fn insert(s: &mut Session, mut frag: Fragment) -> Result<(), CmdError> {
    super::controls::prepare_edit(s)?;
    let at = delete_selection(s)?;
    let track = s.doc.settings.track_changes;
    let mut rid = None;
    if track {
        // What the last command cut, pasted again: the two ends of a move.
        let r = match super::moves::take_paste(s, &frag) {
            Some(r) => r,
            None => super::new_revision(s, wordcraft_doc::RevisionKind::Insert),
        };
        rid = Some(r);
        for b in &mut frag.blocks {
            each_para(b, 0, &mut |p| {
                for run in &mut p.runs {
                    // Text deleted in the source stays deleted: inserted, then deleted.
                    run.props.ins = Some(r);
                    run.props.fmt_change = None;
                }
                p.mark.ins = None;
                p.mark.del = None;
            });
        }
    }
    let end = s.doc.insert_fragment(&at, &frag)?;
    // The paragraph marks the paste added are inserted too, so rejecting it joins the
    // paragraphs again. The last paragraph ends with the original mark and keeps it.
    if let Some(r) = rid
        && at.story == end.story
        && at.path.parent() == end.path.parent()
    {
        for i in at.path.last()..end.path.last() {
            if let Ok(p) = s.doc.para_mut(at.story, &at.path.with_last(i)) {
                p.mark.ins = Some(r);
                p.mark.del = None;
                p.touch();
            }
        }
    }
    s.sel = Selection::caret(end);
    Ok(())
}

/// What a live paste took out ([`live_text`]), by what a notice can name. Bookmarks, comment
/// anchors and permission marks are not counted: nothing visible goes with them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Lost {
    /// Pictures, charts, shapes, text boxes (their text is kept) and groups.
    pub pictures: bool,
    /// Tables (their text is kept, a row to a line).
    pub tables: bool,
    /// Fields and equations (their result or text is kept).
    pub fields: bool,
    /// Footnote and endnote marks.
    pub notes: bool,
    /// Formatting only: a paragraph or character style or a list the document lacks, a section
    /// break, a tracked change, a content control, a phonetic guide.
    pub formatting: bool,
    /// Something visible was taken out and no text is left: nothing is pasted.
    pub nothing_left: bool,
}

impl Lost {
    /// Whether anything was taken out that the user may miss.
    pub fn any(&self) -> bool {
        self.pictures || self.tables || self.fields || self.notes || self.formatting
    }

    /// Whether content (not only formatting) was taken out.
    pub fn content(&self) -> bool {
        self.pictures || self.tables || self.fields || self.notes
    }

    /// For a command's result: `{"pictures": bool, …}`.
    pub fn to_json(&self) -> Value {
        json!({
            "pictures": self.pictures,
            "tables": self.tables,
            "fields": self.fields,
            "notes": self.notes,
            "formatting": self.formatting,
        })
    }
}

/// What of a pasted fragment live co-editing can share, and what was taken out.
///
/// Live editing shares the body's paragraphs: their text, character formatting and paragraph
/// formatting. A paste must not bring what lives outside the body or is not shared, so here:
/// - a table becomes one paragraph a row, its cells' text joined by tabs and a cell's own
///   paragraphs by line breaks (a nested table the same way, inside its cell);
/// - an inline object becomes its text: a field its result, an equation its linear text, a
///   text box the text inside it, a picture, shape, note mark, bookmark or comment anchor none;
/// - a paragraph or character style `doc` lacks, a list `doc` lacks, a section break, and
///   every tracked mark are dropped (text a tracked change deleted goes); a paragraph that held
///   only objects leaves no empty paragraph;
/// - media and text box stories stay behind.
///
/// What is left only refers to what `doc` already has. [`Lost`] records each kind taken out,
/// as it is taken out.
pub fn live_text(doc: &Document, frag: Fragment) -> (Fragment, Lost) {
    let mut lost = Lost::default();
    let Fragment { blocks: source, parts, .. } = frag;
    let mut blocks = Vec::with_capacity(source.len());
    for b in source {
        match b {
            Block::Para(_) => blocks.push(b),
            Block::Table(t) => {
                lost.tables = true;
                blocks.extend(table_rows(t, 0).into_iter().map(Block::Para));
            }
        }
    }
    // A paragraph that held only objects (a picture alone) and is empty once they are text
    // leaves nothing behind, not an empty paragraph.
    let only_objects = |b: &Block| b.as_para().is_some_and(|p| !p.text.is_empty() && p.text.chars().all(|c| c == wordcraft_doc::para::OBJ));
    blocks = blocks
        .into_iter()
        .filter_map(|mut b| {
            let was_objects = only_objects(&b);
            if let Block::Para(p) = &mut b {
                keep_shared(doc, &parts, p, &mut lost);
                if was_objects && p.text.is_empty() {
                    return None;
                }
            }
            Some(b)
        })
        .collect();
    let text_left = blocks.iter().any(|b| b.as_para().is_some_and(|p| !p.text.is_empty()));
    lost.nothing_left = lost.content() && !text_left;
    (Fragment { blocks, parts: Default::default(), media: Default::default() }, lost)
}

/// A table's rows as paragraphs (see [`live_text`]). Each row takes the formatting of its first
/// cell paragraph. Past [`MAX_DEPTH`] nested tables are dropped.
fn table_rows(t: wordcraft_doc::Table, depth: usize) -> Vec<Paragraph> {
    if depth > MAX_DEPTH {
        return Vec::new();
    }
    t.rows
        .into_iter()
        .map(|row| {
            let mut line: Option<Paragraph> = None;
            for (i, cell) in row.cells.into_iter().enumerate() {
                if i > 0 {
                    append_text(&mut line, "\t");
                }
                let paras = cell.blocks.into_iter().flat_map(|cb| match Arc::unwrap_or_clone(cb) {
                    Block::Para(p) => vec![p],
                    Block::Table(inner) => table_rows(inner, depth + 1),
                });
                for (j, p) in paras.enumerate() {
                    if j > 0 {
                        append_text(&mut line, &wordcraft_doc::para::LINE_BREAK.to_string());
                    }
                    match &mut line {
                        Some(l) => l.append(Paragraph { section: None, ..p }),
                        None => line = Some(p),
                    }
                }
            }
            line.unwrap_or_default()
        })
        .collect()
}

/// Add `text` (a tab or a line break between cells) at the end of `line` (a new paragraph if
/// there is none), in plain formatting: a bold cell's bold does not run on into the separator.
fn append_text(line: &mut Option<Paragraph>, text: &str) {
    let l = line.get_or_insert_default();
    let end = l.text.len();
    let _ = l.insert_text(end, text, &Default::default());
}

/// The text of a text box story: its paragraphs' text, joined by line breaks.
fn story_text(parts: &std::collections::BTreeMap<u32, wordcraft_doc::Part>, id: u32) -> String {
    let mut lines = Vec::new();
    if let Some(part) = parts.get(&id) {
        for b in part.blocks.iter() {
            let mut b = (**b).clone();
            each_para(&mut b, 0, &mut |p| lines.push(p.plain_text()));
        }
    }
    lines.join(&wordcraft_doc::para::LINE_BREAK.to_string())
}

/// What an inline object becomes when pasted live: its text, and what kind of loss it is.
fn object_text(o: &wordcraft_doc::InlineObject, parts: &std::collections::BTreeMap<u32, wordcraft_doc::Part>, lost: &mut Lost) -> String {
    use wordcraft_doc::InlineObject as O;
    match o {
        O::Image { .. } | O::Graphic { .. } | O::Shape { .. } | O::Group { .. } => {
            lost.pictures = true;
            o.text_boxes()
                .into_iter()
                .map(|id| story_text(parts, id))
                .filter(|t| !t.is_empty())
                .collect::<Vec<_>>()
                .join(&wordcraft_doc::para::LINE_BREAK.to_string())
        }
        O::Field { .. } | O::FieldStart { .. } | O::FieldEnd | O::Equation { .. } => {
            lost.fields = true;
            o.plain_text().to_owned()
        }
        O::NoteRef { .. } => {
            lost.notes = true;
            String::new()
        }
        O::BookmarkStart { .. } | O::BookmarkEnd { .. } | O::CommentStart { .. } | O::CommentEnd { .. } | O::PermStart { .. } | O::PermEnd { .. } => {
            String::new()
        }
        _ => {
            lost.formatting = true;
            o.plain_text().to_owned()
        }
    }
}

/// [`live_text`] for one paragraph.
fn keep_shared(doc: &Document, parts: &std::collections::BTreeMap<u32, wordcraft_doc::Part>, p: &mut Paragraph, lost: &mut Lost) {
    use wordcraft_doc::para::OBJ;
    let mut changed = false;
    // Tracked changes are off while live: no tracked mark comes in, even one naming a revision
    // the document holds (it would be another author's), and text a tracked change deleted goes.
    let gone: Vec<std::ops::Range<usize>> = p.run_ranges().filter(|(_, c)| c.del.is_some()).map(|(r, _)| r).collect();
    for r in gone.into_iter().rev() {
        if p.delete(r.start, r.end).is_ok() {
            changed = true;
            lost.formatting = true;
        }
    }
    // Objects become their text, in the formatting they had.
    let objects: Vec<(usize, String)> = p.objects_with_offsets().map(|(off, o)| (off, object_text(o, parts, lost))).collect();
    for (off, text) in objects.into_iter().rev() {
        let props = p.props_of_char(off).clone();
        if p.delete(off, off + OBJ.len_utf8()).is_ok() {
            changed = true;
            let _ = p.insert_text(off, &text, &props);
        }
    }
    let style_ok = |id: &Option<String>| id.as_deref().is_none_or(|id| doc.styles.get(id).is_some());
    let mut taken = |cond: bool, lost: &mut Lost| {
        if cond {
            changed = true;
            lost.formatting = true;
        }
        cond
    };
    if taken(!style_ok(&p.props.style), lost) {
        p.props.style = None;
    }
    if taken(p.props.numbering.as_ref().is_some_and(|n| n.num != 0 && doc.numbering.num(n.num).is_none()), lost) {
        p.props.numbering = None;
    }
    // A section break would bring its own page setup, which is locked while live.
    if taken(p.section.is_some(), lost) {
        p.section = None;
    }
    if taken(p.props.num_change.is_some(), lost) {
        p.props.num_change = None;
    }
    if taken(p.props.fmt_change.is_some(), lost) {
        p.props.fmt_change = None;
    }
    for props in p.runs.iter_mut().map(|r| &mut r.props).chain(std::iter::once(&mut p.mark)) {
        if taken(!style_ok(&props.style), lost) {
            props.style = None;
        }
        if taken(props.ins.is_some(), lost) {
            props.ins = None;
        }
        if taken(props.del.is_some(), lost) {
            props.del = None;
        }
        if taken(props.fmt_change.is_some(), lost) {
            props.fmt_change = None;
        }
    }
    if changed {
        p.normalize();
        p.touch();
    }
}

/// Every paragraph in a block, tables included (bounded depth).
pub(crate) fn each_para(b: &mut Block, depth: usize, f: &mut dyn FnMut(&mut Paragraph)) {
    if depth > MAX_DEPTH {
        return;
    }
    match b {
        Block::Para(p) => f(p),
        Block::Table(t) => {
            for row in &mut t.rows {
                for cell in &mut row.cells {
                    for cb in &mut cell.blocks {
                        each_para(Arc::make_mut(cb), depth + 1, f);
                    }
                }
            }
        }
    }
}

/// Section breaks brought in from another document keep their page, but their header and
/// footer ids are that document's: clear them (and the different first page that went with
/// them) so the sections link to the previous section's headers and footers, as in Word.
pub(crate) fn unlink_sections(blocks: &mut [Block]) {
    for b in blocks {
        each_para(b, 0, &mut |p| {
            if let Some(sec) = p.section.as_deref_mut() {
                sec.headers = Default::default();
                sec.footers = Default::default();
                sec.title_page = false;
            }
        });
    }
}

/// Turn an imported document into a fragment of the session's document: bring over the media,
/// lists and styles it refers to (renumbered so they can't collide with ours).
fn adopt(s: &mut Session, src: Document) -> Fragment {
    // While editing live the document's styles, lists and media must not change: adopt into a
    // copy, and `insert` keeps to what the document has (`live_text`).
    let mut frag = if s.editing_live() { s.doc.clone().adopt(src) } else { s.doc.adopt(src) };
    // Pasted pictures fit our text width, like Insert › Pictures (layout keeps their size).
    let max_w = crate::io::text_width(&s.doc);
    for b in &mut frag.blocks {
        crate::io::fit_block_pictures(b, max_w);
    }
    unlink_sections(&mut frag.blocks);
    frag
}

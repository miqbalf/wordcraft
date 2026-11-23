//! Body blocks as shared blocks, and back.
//!
//! * A paragraph is one block. Its text is the block's text, U+FFFC and all. Its runs are the
//!   text's formatting attributes: a run's [`CharProps`] as JSON, one key per field that is set.
//!   `props`, `mark`, `objects` and `section` are the block attributes `"p"`, `"m"`, `"o"` and
//!   `"s"` (serde JSON), each left out when empty. The block id is [`Paragraph::pid`].
//! * Every other body block (in this model, a table) is one opaque block whose only attribute is
//!   `"opaque"`: the whole [`Block`] as serde JSON. Its id lives in [`crate::LiveMap`]'s table
//!   list, because a table has no id field of its own.

use std::collections::BTreeSet;
use std::sync::Arc;

use craft_collab::{Attributes, SeedBlock};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use wordcraft_doc::{Block, CharProps, Document, Paragraph, Run, SectionProps, Style, StyleKind};

pub(crate) const PROPS: &str = "p";
pub(crate) const MARK: &str = "m";
pub(crate) const OBJECTS: &str = "o";
pub(crate) const SECTION: &str = "s";
pub(crate) const OPAQUE: &str = "opaque";

/// The display name of the paragraph style a recovered paragraph gets.
///
/// It is document data, like the names of the built-in styles, not interface text: every peer
/// must write the same style into the file, so it is not translated. (This crate sits below the
/// interface and has no translation system.)
pub const RECOVERED_STYLE: &str = "Recovered text";

/// The style id of [`RECOVERED_STYLE`], which paragraphs name in `props.style`.
pub const RECOVERED_STYLE_ID: &str = "RecoveredText";

/// `v` as JSON object entries, nulls dropped.
pub(crate) fn object<T: Serialize>(v: &T) -> Attributes {
    match serde_json::to_value(v) {
        Ok(Value::Object(m)) => m.into_iter().filter(|(_, v)| !v.is_null()).collect(),
        _ => Attributes::new(),
    }
}

fn put<T: Serialize>(attrs: &mut Attributes, key: &str, v: &T) {
    match serde_json::to_value(v) {
        Ok(json) => {
            attrs.insert(key.to_owned(), json);
        }
        Err(e) => log::warn!("live: block attribute {key} did not serialise: {e}"),
    }
}

/// The largest integer yrs writes as an integer; it writes any other (exactly) as a float.
const YRS_INT_MAX: f64 = 2_147_483_647.0;

/// `v` with every whole float outside yrs's integer range back as an integer.
///
/// yrs encodes an integer outside `±0x7FFFFFFF` as an exact float, and every replica that
/// decodes it (another peer, a late joiner, the saver's checkpoint) reads it back as a float,
/// which an integer field refuses: the whole attribute would read as its default there, and that
/// peer's document would drift from the author's. A float that size is never one of ours, so it
/// is turned back into the integer it was.
fn whole_numbers(v: &mut Value) {
    match v {
        Value::Number(n) => {
            if let Some(f) = n.as_f64().filter(|f| n.is_f64() && f.fract() == 0.0 && f.abs() > YRS_INT_MAX) {
                if (0.0..18_446_744_073_709_551_616.0).contains(&f) {
                    *v = Value::from(f as u64);
                } else if (-9_223_372_036_854_775_808.0..0.0).contains(&f) {
                    *v = Value::from(f as i64);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(whole_numbers),
        Value::Object(m) => m.values_mut().for_each(whole_numbers),
        _ => {}
    }
}

fn parse<T: DeserializeOwned>(v: &Value, what: &str) -> Option<T> {
    let mut v = v.clone();
    whole_numbers(&mut v);
    match serde_json::from_value(v) {
        Ok(t) => Some(t),
        Err(e) => {
            log::warn!("live: shared {what} did not read back: {e}");
            None
        }
    }
}

fn get<T: DeserializeOwned + Default>(attrs: &Attributes, key: &str) -> T {
    attrs.get(key).and_then(|v| parse(v, key)).unwrap_or_default()
}

/// The block attributes of a paragraph.
pub(crate) fn para_attrs(p: &Paragraph) -> Attributes {
    let mut attrs = Attributes::new();
    if !p.props.is_empty() {
        put(&mut attrs, PROPS, &p.props);
    }
    if !p.mark.is_empty() {
        put(&mut attrs, MARK, &p.mark);
    }
    if !p.objects.is_empty() {
        put(&mut attrs, OBJECTS, &p.objects);
    }
    if let Some(s) = &p.section {
        put(&mut attrs, SECTION, s);
    }
    attrs
}

/// A paragraph's runs as `(chars, attributes)`.
pub(crate) fn para_runs(p: &Paragraph) -> Vec<(usize, Attributes)> {
    let mut at = 0usize;
    p.runs
        .iter()
        .map(|r| {
            let end = at.saturating_add(r.len).min(p.text.len());
            let chars = p.text.get(at..end).map_or(0, |t| t.chars().count());
            at = end;
            (chars, object(&r.props))
        })
        .collect()
}

pub(crate) fn para_seed(id: u64, p: &Paragraph) -> SeedBlock {
    SeedBlock { id, text: p.text.clone(), attrs: para_attrs(p), runs: para_runs(p) }
}

pub(crate) fn opaque_attrs(b: &Block) -> Attributes {
    let mut attrs = Attributes::new();
    put(&mut attrs, OPAQUE, b);
    attrs
}

/// One body block as a shared block with id `id`.
pub(crate) fn block_seed(id: u64, b: &Block) -> SeedBlock {
    match b {
        Block::Para(p) => para_seed(id, p),
        Block::Table(_) => SeedBlock { id, attrs: opaque_attrs(b), ..SeedBlock::default() },
    }
}

/// Byte runs over `text` from `(chars, attributes)` runs. No runs is unformatted text.
fn byte_runs(text: &str, runs: &[(usize, Attributes)]) -> Vec<Run> {
    if runs.is_empty() {
        return if text.is_empty() { Vec::new() } else { vec![Run { len: text.len(), props: CharProps::default() }] };
    }
    let bounds: Vec<usize> = text.char_indices().map(|(i, _)| i).chain(std::iter::once(text.len())).collect();
    let last = bounds.len().saturating_sub(1);
    let mut chars = 0usize;
    runs.iter()
        .map(|(n, attrs)| {
            let start = bounds.get(chars.min(last)).copied().unwrap_or(0);
            chars = chars.saturating_add(*n);
            let end = bounds.get(chars.min(last)).copied().unwrap_or(text.len());
            let props = parse(&Value::Object(attrs.clone()), "run").unwrap_or_default();
            Run { len: end.saturating_sub(start), props }
        })
        .collect()
}

/// A shared block as a paragraph with id `seed.id`.
pub(crate) fn seed_para(seed: &SeedBlock) -> Paragraph {
    let mut p = Paragraph::new();
    p.text = seed.text.clone();
    p.runs = byte_runs(&seed.text, &seed.runs);
    p.props = get(&seed.attrs, PROPS);
    p.mark = get(&seed.attrs, MARK);
    p.objects = get(&seed.attrs, OBJECTS);
    p.section = seed.attrs.get(SECTION).and_then(|v| parse::<SectionProps>(v, SECTION)).map(Box::new);
    p.normalize();
    p.pid = seed.id;
    p
}

/// A shared block as a body block. An opaque block that does not read back becomes its text as
/// a paragraph, the same on every replica.
pub(crate) fn seed_block(seed: &SeedBlock) -> Arc<Block> {
    if let Some(b) = seed.attrs.get(OPAQUE).and_then(|v| parse::<Block>(v, OPAQUE)) {
        return Arc::new(b);
    }
    Arc::new(Block::Para(seed_para(seed)))
}

/// The id each body block is shared under, for the file every peer opened.
///
/// A paragraph keeps a nonzero id it already has, if no earlier block has it; every other block
/// gets `(0 << 32) | (index + 1)`, or when that is taken the next free id past every id in use.
/// A pure function of the document, so every peer that opens the same file agrees.
pub fn seed_ids(doc: &Document) -> Vec<u64> {
    let mut kept = BTreeSet::new();
    let own: Vec<Option<u64>> = doc.body.iter().map(|b| b.as_para().map(|p| p.pid).filter(|pid| *pid != 0 && kept.insert(*pid))).collect();
    let mut spare = kept.iter().next_back().copied().unwrap_or(0).max(doc.body.len() as u64) + 1;
    let mut used = kept;
    own.iter()
        .enumerate()
        .map(|(i, own)| match own {
            Some(pid) => *pid,
            None => {
                let mut id = i as u64 + 1;
                if used.contains(&id) {
                    while used.contains(&spare) {
                        spare += 1;
                    }
                    id = spare;
                }
                used.insert(id);
                id
            }
        })
        .collect()
}

/// The body as shared blocks, under the ids [`seed_ids`] gives. Every peer seeds its
/// [`LiveDoc`](craft_collab::LiveDoc) from the same file with this.
pub fn seed_blocks(doc: &Document) -> Vec<SeedBlock> {
    doc.body.iter().zip(seed_ids(doc)).map(|(b, id)| block_seed(id, b)).collect()
}

/// Write the ids [`seed_ids`] gives into the body's paragraphs.
///
/// Call order when a document is shared: open the file, write the ids with
/// `session.assign_pids(assign_seed_pids)` (not an edit), then [`seed_blocks`] from the
/// session's document, `LiveDoc::seeded`, `set_client`, [`LiveMap::new`](crate::LiveMap::new),
/// and only then install the observer.
pub fn assign_seed_pids(doc: &mut Document) {
    let ids = seed_ids(doc);
    for (b, id) in doc.body.iter_mut().zip(ids) {
        if b.as_para().is_some_and(|p| p.pid != id)
            && let Block::Para(p) = Arc::make_mut(b)
        {
            p.pid = id;
        }
    }
}

/// Write `(body index, id)` pairs into the body's paragraphs (see
/// [`LocalEdit::pids`](crate::LocalEdit::pids)).
pub fn write_pids(doc: &mut Document, pids: &[(usize, u64)]) {
    for (i, id) in pids {
        if let Some(b) = doc.body.get_mut(*i)
            && b.as_para().is_some_and(|p| p.pid != *id)
            && let Block::Para(p) = Arc::make_mut(b)
        {
            p.pid = *id;
        }
    }
}

/// Add the [`RECOVERED_STYLE`] paragraph style when the document lacks it.
pub(crate) fn ensure_recovered_style(doc: &mut Document) {
    if doc.styles.get(RECOVERED_STYLE_ID).is_some() {
        return;
    }
    let based_on = doc.styles.get("Normal").map(|s| s.id.clone());
    Arc::make_mut(&mut doc.styles).styles.push(Style {
        id: RECOVERED_STYLE_ID.to_owned(),
        name: RECOVERED_STYLE.to_owned(),
        kind: StyleKind::Paragraph,
        based_on,
        ..Style::default()
    });
}

/// `attrs` with the paragraph style set to [`RECOVERED_STYLE_ID`].
pub(crate) fn recovered_attrs(attrs: &Attributes) -> Attributes {
    let mut props: wordcraft_doc::ParaProps = get(attrs, PROPS);
    props.style = Some(RECOVERED_STYLE_ID.to_owned());
    let mut out = attrs.clone();
    put(&mut out, PROPS, &props);
    out
}

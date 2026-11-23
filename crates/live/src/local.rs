//! Local edits: one reported command diffed into replica steps.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use craft_collab::LiveDoc;
use wordcraft_doc::{Block, Document, InlineObject, Paragraph};

use crate::ops::Ops;
use crate::{LiveMap, LocalEdit, MapError, Moved, Refusal, diff, map};

/// The ids of the blocks of a body: paragraphs by pid, tables by the side list.
struct Ids<'a> {
    paras: HashMap<u64, &'a Arc<Block>>,
    tables: HashMap<u64, &'a Arc<Block>>,
}

impl<'a> Ids<'a> {
    fn of(doc: &'a Document, tables: &[u64]) -> Ids<'a> {
        let paras = doc.body.iter().filter_map(|b| Some((b.as_para().map(|p| p.pid).filter(|pid| *pid != 0)?, b))).collect();
        let tables = tables.iter().copied().zip(doc.body.iter().filter(|b| b.as_table().is_some())).collect();
        Ids { paras, tables }
    }

    fn para(&self, id: &u64) -> Option<&'a Paragraph> {
        self.paras.get(id).and_then(|b| b.as_para())
    }
}

/// Whether two bodies hold the very same blocks.
pub(crate) fn same_body(a: &Document, b: &Document) -> bool {
    a.body.len() == b.body.len() && a.body.iter().zip(&b.body).all(|(x, y)| Arc::ptr_eq(x, y))
}

/// The first part of the document outside the body that differs, by name: live editing shares
/// only the body ([`Document::changed_outside_body`] lists every field).
pub(crate) fn outside_body_change(a: &Document, b: &Document) -> Option<&'static str> {
    a.changed_outside_body(b)
}

/// How deep tables are looked into for inline objects.
const TABLE_DEPTH: usize = 32;

/// The inline objects of block `b`, those in its table cells too.
fn block_objects<'a>(b: &'a Block, depth: usize, out: &mut Vec<&'a InlineObject>) {
    match b {
        Block::Para(p) => out.extend(&p.objects),
        Block::Table(t) if depth < TABLE_DEPTH => {
            for cb in t.rows.iter().flat_map(|r| &r.cells).flat_map(|c| &c.blocks) {
                block_objects(cb, depth + 1, out);
            }
        }
        Block::Table(_) => {}
    }
}

/// The inline objects of the body blocks not in `other`, in order, table cells included.
fn objects_off<'a>(doc: &'a Document, other: &Document) -> Vec<&'a InlineObject> {
    let there: HashSet<*const Block> = other.body.iter().map(Arc::as_ptr).collect();
    let mut out = Vec::new();
    for b in doc.body.iter().filter(|b| !there.contains(&Arc::as_ptr(b))) {
        block_objects(b, 0, &mut out);
    }
    out
}

/// Whether `now` holds only objects `was` held: some were removed, none added.
fn only_removed(was: &[&InlineObject], now: &[&InlineObject]) -> bool {
    let mut left = was.to_vec();
    now.iter().all(|o| match left.iter().position(|w| w == o) {
        Some(i) => {
            left.swap_remove(i);
            true
        }
        None => false,
    })
}

/// Whether table `b` reads back the same from its shared form: its JSON, as the shared document
/// holds it, parsed again. A value JSON or the shared document can't hold (a width that is not a
/// number, an integer past `i64::MAX`) would leave the other editors with something else.
fn survives_sharing(b: &Block) -> bool {
    let Ok(json) = serde_json::to_value(b) else { return false };
    let back = craft_collab::replica::to_json(&craft_collab::replica::to_any(&json));
    serde_json::from_value::<Block>(back).is_ok_and(|r| r == *b)
}

/// Refuse an edit that changes what live editing does not share: anything outside the body, the
/// body's inline objects, table cells' included (moving them between paragraphs, as Enter and
/// Delete do, is fine), or a new or changed table that would not read back from its shared form.
pub(crate) fn check_shareable(before: &Document, after: &Document, command: &str) -> Result<(), MapError> {
    let (was, now) = match in_place(before, after) {
        Some(changed) => {
            let at = |d| changed.iter().filter_map(|i| objects_at(d, *i)).flatten().collect::<Vec<&InlineObject>>();
            (at(before), at(after))
        }
        None => (objects_off(before, after), objects_off(after, before)),
    };
    let reason = if was == now {
        let there: HashSet<*const Block> = before.body.iter().map(Arc::as_ptr).collect();
        let fresh_tables = after.body.iter().filter(|b| b.as_table().is_some() && !there.contains(&Arc::as_ptr(b)));
        match outside_body_change(before, after) {
            Some(part) => Some(Refusal::OutsideBody(part)),
            None => fresh_tables.into_iter().any(|b| !survives_sharing(b)).then_some(Refusal::NotEncodable),
        }
    } else if only_removed(&was, &now) {
        Some(Refusal::ObjectRemoved)
    } else {
        Some(Refusal::InlineObject)
    };
    match reason {
        Some(reason) => Err(MapError::NotShareable { command: command.to_owned(), reason }),
        None => Ok(()),
    }
}

fn objects_at(d: &Document, i: usize) -> Option<&[InlineObject]> {
    d.body.get(i).and_then(|b| b.as_para()).map(|p| p.objects.as_slice())
}

/// The body indices of the paragraphs an edit changed in place, when that is all it did: the
/// same blocks, in the same order, each changed one a paragraph with the same shared id.
fn in_place(before: &Document, after: &Document) -> Option<Vec<usize>> {
    if before.body.len() != after.body.len() {
        return None;
    }
    let mut changed = Vec::new();
    for (i, (a, b)) in before.body.iter().zip(&after.body).enumerate() {
        if Arc::ptr_eq(a, b) {
            continue;
        }
        match (a.as_para(), b.as_para()) {
            (Some(p), Some(q)) if p.pid != 0 && p.pid == q.pid => changed.push(i),
            _ => return None,
        }
    }
    Some(changed)
}

impl LiveMap {
    /// A fresh id `(space << 32) | next`, unused in `live` and not in `taken`.
    pub(crate) fn mint(&mut self, live: &LiveDoc, taken: &HashSet<u64>) -> u64 {
        loop {
            self.next = self.next.wrapping_add(1).max(1);
            let id = (u64::from(self.space) << 32) | u64::from(self.next);
            if !taken.contains(&id) && live.block(id).is_none() {
                return id;
            }
        }
    }

    /// [`LiveMap::local_updates`] once a space is known.
    pub(crate) fn diff(&mut self, live: &mut LiveDoc, before: &Document, after: &Document, command: &str) -> Result<LocalEdit, MapError> {
        check_shareable(before, after, command)?;
        if same_body(before, after) {
            return Ok(LocalEdit::default());
        }
        if let Some(changed) = in_place(before, after) {
            // Typing and formatting: no block came or went, so the order is not needed.
            let mut ops = Ops::in_place(live);
            for p in changed.iter().filter_map(|i| after.body.get(*i)?.as_para()) {
                if let Some(have) = ops.live.block(p.pid) {
                    ops.reconcile(&have, &map::para_seed(p.pid, p));
                }
            }
            return Ok(ops.out);
        }
        let old = Ids::of(before, &self.tables);
        let mut ops = Ops::new(live);
        let (want, pids) = self.want_ids(&ops, &old, after);
        ops.out.pids = pids;
        let mut dirty = HashSet::new();
        self.remove_gone(&mut ops, &old, after, &want, &mut dirty);
        let want = self.fix_order(&mut ops, &old, after, want);
        let fresh = Self::insert_new(&mut ops, &old, after, &want, &mut dirty);
        Self::update_kept(&mut ops, &old, after, &want, &fresh, &dirty);
        self.tables = after.body.iter().zip(&want).filter(|(b, _)| b.as_table().is_some()).map(|(_, id)| *id).collect();
        Ok(ops.out)
    }

    /// The id each block of `after` should have, and the paragraphs that need a new one.
    fn want_ids(&mut self, ops: &Ops, old: &Ids, after: &Document) -> (Vec<u64>, Vec<(usize, u64)>) {
        let mut tables = self.match_tables(old, after).into_iter();
        let mut taken = HashSet::new();
        let mut pids = Vec::new();
        let mut want = Vec::with_capacity(after.body.len());
        for (i, b) in after.body.iter().enumerate() {
            let own = match &**b {
                Block::Para(p) => Some(p.pid),
                Block::Table(_) => tables.next().flatten(),
            };
            let id = match own.filter(|id| *id != 0 && ops.at(*id).is_some() && !taken.contains(id)) {
                Some(id) => id,
                None => {
                    let id = self.mint(ops.live, &taken);
                    if b.as_para().is_some() {
                        pids.push((i, id));
                    }
                    id
                }
            };
            taken.insert(id);
            want.push(id);
        }
        (want, pids)
    }

    /// The id of each table of `after`: `before`'s tables by position when there are as many,
    /// otherwise each matched in order to the next equal one. `None` is a new table.
    fn match_tables(&self, old: &Ids, after: &Document) -> Vec<Option<u64>> {
        let olds: Vec<(u64, &Arc<Block>)> = self.tables.iter().filter_map(|id| old.tables.get(id).map(|b| (*id, *b))).collect();
        let news: Vec<&Arc<Block>> = after.body.iter().filter(|b| b.as_table().is_some()).collect();
        if olds.len() == news.len() {
            return olds.iter().map(|(id, _)| Some(*id)).collect();
        }
        let mut from = 0;
        news.iter()
            .map(|n| {
                let found = olds.iter().skip(from).position(|(_, o)| Arc::ptr_eq(o, n) || o == n)?;
                from += found + 1;
                olds.get(from - 1).map(|(id, _)| *id)
            })
            .collect()
    }

    /// Take out the blocks `after` no longer has: each run of removed blocks after a kept
    /// paragraph is removed, its last one joined to that paragraph when the paragraph now ends
    /// with that one's text.
    fn remove_gone(&mut self, ops: &mut Ops, old: &Ids, after: &Document, want: &[u64], dirty: &mut HashSet<u64>) {
        let at: HashMap<u64, usize> = want.iter().enumerate().map(|(i, id)| (*id, i)).collect();
        // (the kept block before, the removed blocks after it), in order.
        let mut groups: Vec<(Option<u64>, Vec<u64>)> = Vec::new();
        let mut prev = None;
        let mut open = false;
        for id in &ops.order {
            if at.contains_key(id) {
                prev = Some(*id);
                open = false;
            } else if let Some((_, gone)) = groups.last_mut().filter(|_| open) {
                gone.push(*id);
            } else {
                groups.push((prev, vec![*id]));
                open = true;
            }
        }
        for (prev, gone) in groups {
            let Some((last, rest)) = gone.split_last() else { continue };
            for s in rest {
                ops.remove(*s);
                self.removed.insert(*s, prev);
            }
            let into = prev.and_then(|p| Some((p, old.para(&p)?, after.body.get(*at.get(&p)?)?.as_para()?)));
            match (into, old.para(last)) {
                (Some((p, _, now)), Some(s)) if diff::common_suffix(&now.text, &s.text) > 0 => {
                    ops.merge(p, *last);
                    dirty.insert(p);
                }
                _ => ops.remove(*last),
            }
            self.removed.insert(*last, prev);
        }
    }

    /// Remove and re-add, under new ids, the kept blocks that moved: those off one longest run
    /// of blocks still in `after`'s order. A moved paragraph is remembered as moved, not
    /// removed, so typing that lands in its old block is forwarded to it.
    fn fix_order(&mut self, ops: &mut Ops, old: &Ids, after: &Document, mut want: Vec<u64>) -> Vec<u64> {
        let at: HashMap<u64, usize> = want.iter().enumerate().map(|(i, id)| (*id, i)).collect();
        let seq: Vec<usize> = ops.order.iter().filter_map(|id| at.get(id).copied()).collect();
        if seq.windows(2).all(|w| w.first() < w.get(1)) {
            return want;
        }
        let keep = diff::increasing(&seq);
        let mut taken: HashSet<u64> = want.iter().copied().collect();
        for (i, k) in seq.iter().zip(keep) {
            let Some(was) = want.get(*i).copied().filter(|_| !k) else { continue };
            ops.remove(was);
            let id = self.mint(ops.live, &taken);
            taken.insert(id);
            if let Some(w) = want.get_mut(*i) {
                *w = id;
            }
            if let Some(p) = after.body.get(*i).and_then(|b| b.as_para()) {
                ops.out.pids.push((*i, id));
                let from_text = old.para(&was).map_or_else(|| p.text.clone(), |q| q.text.clone());
                self.moved.insert(was, Moved { to: id, from_text, to_text: p.text.clone() });
            }
        }
        want
    }

    /// Add the blocks the replica lacks, in `after`'s order: a paragraph right after a kept one
    /// whose old tail it ends with is split off it. Returns the ids inserted whole.
    fn insert_new(ops: &mut Ops, old: &Ids, after: &Document, want: &[u64], dirty: &mut HashSet<u64>) -> HashSet<u64> {
        let mut fresh = HashSet::new();
        for (i, id) in want.iter().enumerate() {
            if ops.at(*id).is_some() {
                continue;
            }
            let Some(block) = after.body.get(i) else { continue };
            let prev = i.checked_sub(1).and_then(|j| Some((j, *want.get(j)?)));
            if let (Some((j, p)), Some(new)) = (prev, block.as_para())
                && let Some(k) = Self::split_point(ops, old, after, j, p, new, dirty)
            {
                ops.split(p, k, *id);
                dirty.insert(p);
                dirty.insert(*id);
                continue;
            }
            let at = prev.and_then(|(_, p)| ops.at(p)).map_or(0, |j| j + 1);
            ops.insert(at, &map::block_seed(*id, block));
            fresh.insert(*id);
        }
        fresh
    }

    /// Where to split paragraph `p` (at `after` index `i`) so its tail becomes `new`: when `p`
    /// is an old paragraph not split or joined yet in this edit and `new` ends with the text
    /// past what `p` kept.
    fn split_point(ops: &Ops, old: &Ids, after: &Document, i: usize, p: u64, new: &Paragraph, dirty: &HashSet<u64>) -> Option<usize> {
        let head = after.body.get(i)?.as_para()?;
        let was = old.para(&p)?;
        if dirty.contains(&p) || ops.at(p).is_none() {
            return None;
        }
        let k = diff::common_prefix(&was.text, &head.text);
        let tail: String = was.text.chars().skip(k).collect();
        new.text.ends_with(&tail).then_some(k)
    }

    /// Bring every kept block that changed to its content in `after`. A block still the same
    /// `Arc` as in `before` is skipped without looking inside it.
    fn update_kept(ops: &mut Ops, old: &Ids, after: &Document, want: &[u64], fresh: &HashSet<u64>, dirty: &HashSet<u64>) {
        for (b, id) in after.body.iter().zip(want) {
            if fresh.contains(id) {
                continue;
            }
            let was = match &**b {
                Block::Para(_) => old.paras.get(id),
                Block::Table(_) => old.tables.get(id),
            };
            let changed = dirty.contains(id) || was.is_none_or(|w| !Arc::ptr_eq(w, b) && ***w != **b);
            if !changed {
                continue;
            }
            match &**b {
                Block::Para(p) => {
                    if let Some(have) = ops.live.block(*id) {
                        ops.reconcile(&have, &map::para_seed(*id, p));
                    }
                }
                Block::Table(_) => {
                    let r = ops.live.set_attrs(*id, &map::opaque_attrs(b));
                    ops.sent(r);
                }
            }
        }
    }
}

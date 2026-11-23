//! Local edits: one reported command diffed into replica steps.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use craft_collab::LiveDoc;
use wordcraft_doc::{Block, Document, InlineObject, Paragraph};

use crate::ops::Ops;
use craft_collab::SeedBlock;

use crate::{LiveMap, LocalEdit, MapError, Moved, Part, Refusal, diff, map};

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

/// The inline objects of the body blocks not in `other`, in order.
fn objects_off<'a>(doc: &'a Document, other: &Document) -> Vec<&'a InlineObject> {
    let there: HashSet<*const Block> = other.body.iter().map(Arc::as_ptr).collect();
    doc.body.iter().filter(|b| !there.contains(&Arc::as_ptr(b))).filter_map(|b| b.as_para()).flat_map(|p| &p.objects).collect()
}

/// Refuse an edit that changes what live editing does not share: anything outside the body, or
/// the body's inline objects (moving them between paragraphs, as Enter and Delete do, is fine).
pub(crate) fn check_shareable(before: &Document, after: &Document, command: &str) -> Result<(), MapError> {
    let objects_kept = match in_place(before, after) {
        Some(changed) => changed.iter().all(|i| objects_at(before, *i) == objects_at(after, *i)),
        None => objects_off(before, after) == objects_off(after, before),
    };
    let reason = if objects_kept { outside_body_change(before, after).map(Refusal::OutsideBody) } else { Some(Refusal::InlineObject) };
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
        let mut want = self.fix_order(&mut ops, &old, after, want);
        let fresh = self.insert_new(&mut ops, &old, after, &mut want, &mut dirty);
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
            let src = ops.live.block(was);
            ops.remove(was);
            let id = self.mint(ops.live, &taken);
            taken.insert(id);
            if let Some(w) = want.get_mut(*i) {
                *w = id;
            }
            if let Some(p) = after.body.get(*i).and_then(|b| b.as_para()) {
                ops.out.pids.push((*i, id));
                let (from_text, from_runs, from_attrs) = match src {
                    Some(b) => (b.text, b.runs, b.attrs),
                    None => (old.para(&was).map_or_else(|| p.text.clone(), |q| q.text.clone()), Vec::new(), map::para_attrs(p)),
                };
                let parts = vec![Part { start: 0, to: id, to_text: p.text.clone() }];
                self.moved.insert(was, Moved { from_text, from_runs, from_attrs, parts });
            }
        }
        want
    }

    /// Add the blocks the replica lacks, in `after`'s order: a paragraph right after a kept one
    /// whose old tail it ends with is split off it. Returns the ids inserted whole.
    ///
    /// A split inside the text is a move of the old paragraph into a new head (whose id replaces
    /// the old one in `want`) and the new tail, so edits that race it can be forwarded (see the
    /// [crate documentation](crate)); a split at the end moves no text and keeps the id.
    fn insert_new(&mut self, ops: &mut Ops, old: &Ids, after: &Document, want: &mut [u64], dirty: &mut HashSet<u64>) -> HashSet<u64> {
        let mut fresh = HashSet::new();
        for i in 0..want.len() {
            let Some(id) = want.get(i).copied() else { continue };
            if ops.at(id).is_some() {
                continue;
            }
            let Some(block) = after.body.get(i) else { continue };
            let prev = i.checked_sub(1).and_then(|j| Some((j, *want.get(j)?)));
            if let (Some((j, p)), Some(new)) = (prev, block.as_para())
                && let Some(k) = Self::split_point(ops, old, after, j, p, new, dirty)
            {
                let moves_text = old.para(&p).is_some_and(|w| k < w.text.chars().count());
                let taken: HashSet<u64> = want.iter().copied().collect();
                let moved = if moves_text { self.split_as_move(ops, p, k, id, &taken) } else { None };
                match moved {
                    Some(head) => {
                        if let Some(w) = want.get_mut(j) {
                            *w = head;
                        }
                        ops.out.pids.push((j, head));
                        dirty.insert(head);
                    }
                    None => {
                        ops.split(p, k, id);
                        dirty.insert(p);
                    }
                }
                dirty.insert(id);
                continue;
            }
            let at = prev.and_then(|(_, p)| ops.at(p)).map_or(0, |j| j + 1);
            ops.insert(at, &map::block_seed(id, block));
            fresh.insert(id);
        }
        fresh
    }

    /// Split block `p` at char `k` by moving it: take it out of the order (its text stays, as
    /// the old block) and add its head under a fresh id and its tail as `tail`. Returns the head's
    /// id; `None` when `p` could not be taken out, and nothing was done.
    fn split_as_move(&mut self, ops: &mut Ops, p: u64, k: usize, tail: u64, taken: &HashSet<u64>) -> Option<u64> {
        let src = ops.live.block(p)?;
        let at = ops.at(p)?;
        let n = src.text.chars().count();
        if k > n {
            return None;
        }
        ops.remove(p);
        if ops.at(p).is_some() {
            return None;
        }
        let head = self.mint(ops.live, taken);
        let part = |id: u64, from: usize, to: usize| SeedBlock {
            id,
            text: src.text.chars().skip(from).take(to - from).collect(),
            attrs: src.attrs.clone(),
            runs: diff::slice_runs(&src.runs, from, to),
        };
        let (h, t) = (part(head, 0, k), part(tail, k, n));
        ops.insert(at, &h);
        ops.insert(at + 1, &t);
        let parts = vec![Part { start: 0, to: head, to_text: h.text }, Part { start: k, to: tail, to_text: t.text }];
        self.moved.insert(p, Moved { from_text: src.text, from_runs: src.runs, from_attrs: src.attrs, parts });
        Some(head)
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

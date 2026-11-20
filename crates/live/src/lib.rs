//! Live co-editing for WordCraft: the body of a [`Document`] mapped to and from the shared block
//! document of `craft-collab` ([`LiveDoc`]), so that sessions editing the same file converge.
//!
//! # The mapping
//!
//! [`map`] holds it: a body paragraph is one block whose id is its
//! [`Paragraph::pid`](wordcraft_doc::Paragraph::pid), its text the block's text and its runs the
//! text's formatting; any other body block (a table) is one opaque block. Only the body is
//! shared. Headers, notes, styles and the rest are not, and live editing inside a table is not
//! either: a table edit replaces the whole table, and the last writer wins.
//!
//! # The flow
//!
//! 1. Every peer opens the same file, writes the seed ids with
//!    `session.assign_pids(assign_seed_pids)`, seeds a [`LiveDoc`] from [`seed_blocks`], takes
//!    its client space with `set_client`, and makes a [`LiveMap::new`].
//! 2. Each local command the session's observer reports goes through
//!    [`LiveMap::local_updates`]. The updates go to the relay; the new paragraph ids go back into
//!    the session with [`share_pids`] before the next command or remote edit.
//! 3. Each update from the relay goes through [`LiveMap::receive`], and the changes it reports
//!    into the session with `session.apply_remote(|d| map.apply_changes(d, &live, &changes))`.
//!    The updates `receive` returns (recovered text) go to the relay too.
//!
//! # Recovered text
//!
//! A paragraph this peer removed (by deleting it or joining it to the one before) while another
//! peer was still typing in it stays in the shared document as a tombstone, and the typing lands
//! there. When an update brings text into a tombstone this peer removed, [`LiveMap::receive`]
//! inserts a paragraph in the [`RECOVERED_STYLE`] after the nearest paragraph that survives, with
//! the tombstone's text, so the typing is not lost. Only the peer whose removal orphaned the
//! typing does this, so the paragraph is inserted once. Two peers removing the same paragraph
//! at the same time would both insert one; that is accepted for phase 1.

mod diff;
pub mod map;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use craft_collab::{BlockChange, LiveDoc, LiveError, SeedBlock};
use wordcraft_doc::{Block, Document, Paragraph};
use wordcraft_engine::{CmdError, Session};

pub use map::{RECOVERED_STYLE, RECOVERED_STYLE_ID, assign_seed_pids, seed_blocks, seed_ids, write_pids};

/// Why a mapping call was refused. Nothing was changed when one is returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MapError {
    /// The replica refused the call.
    #[error("shared document: {0}")]
    Live(#[from] LiveError),
    /// No client space yet, so no new paragraph id can be minted.
    #[error("no client space yet")]
    NoSpace,
}

/// What one local command sends.
#[derive(Clone, Debug, Default)]
pub struct LocalEdit {
    /// The updates for the relay, in order.
    pub updates: Vec<Vec<u8>>,
    /// `(body index in the document after the command, id)` for each paragraph that needs a new
    /// id. Write them with [`share_pids`] (or [`write_pids`] inside `Session::assign_pids`)
    /// before the next command or remote edit.
    pub pids: Vec<(usize, u64)>,
    /// Steps the replica refused. The updates before and after them still stand and must be
    /// sent; a nonempty list means the replica and the session may differ, so the caller should
    /// reload from a checkpoint.
    pub failed: Vec<LiveError>,
}

/// What one remote update changed, and what this peer sends in answer.
#[derive(Clone, Debug, Default)]
pub struct Remote {
    /// For [`LiveMap::apply_changes`].
    pub changes: Vec<BlockChange>,
    /// Updates for the relay: the recovered text this peer inserted.
    pub updates: Vec<Vec<u8>>,
}

/// Write a [`LocalEdit`]'s new paragraph ids into the session. Not an edit: no undo step, not
/// reported to the observer.
pub fn share_pids(session: &mut Session, edit: &LocalEdit) -> Result<(), CmdError> {
    if edit.pids.is_empty() {
        return Ok(());
    }
    session.assign_pids(|d| write_pids(d, &edit.pids))
}

/// One peer's side of the mapping: the client space and id counter new paragraphs take ids
/// from, the ids of the body's tables, and the paragraphs it removed (for recovered text).
#[derive(Clone, Debug)]
pub struct LiveMap {
    space: u32,
    next: u32,
    /// The ids of the body's tables, in body order.
    tables: Vec<u64>,
    /// Blocks this peer took out of the order, with the block before each then.
    removed: BTreeMap<u64, Option<u64>>,
    /// Tombstone id to the recovered paragraph made for it.
    recovered: BTreeMap<u64, u64>,
}

/// The ids of the blocks of a body: paragraphs by pid, tables by the side list.
struct Ids<'a> {
    paras: HashMap<u64, &'a Paragraph>,
    tables: HashMap<u64, &'a Arc<Block>>,
}

impl<'a> Ids<'a> {
    fn of(doc: &'a Document, tables: &[u64]) -> Ids<'a> {
        let paras = doc.body.iter().filter_map(|b| b.as_para()).filter(|p| p.pid != 0).map(|p| (p.pid, p)).collect();
        let tables = tables.iter().copied().zip(doc.body.iter().filter(|b| b.as_table().is_some())).collect();
        Ids { paras, tables }
    }
}

/// The replica plus a mirror of its order, and what the edit sent so far.
struct Ops<'a> {
    live: &'a mut LiveDoc,
    order: Vec<u64>,
    out: LocalEdit,
}

impl Ops<'_> {
    fn sent(&mut self, r: Result<Vec<u8>, LiveError>) -> bool {
        match r {
            Ok(u) => {
                self.out.updates.push(u);
                true
            }
            Err(e) => {
                log::warn!("live: the shared document refused a step: {e}");
                self.out.failed.push(e);
                false
            }
        }
    }

    fn at(&self, id: u64) -> Option<usize> {
        self.order.iter().position(|o| *o == id)
    }

    fn remove(&mut self, id: u64) {
        let r = self.live.remove_block(id);
        if self.sent(r) {
            self.order.retain(|o| *o != id);
        }
    }

    fn merge(&mut self, first: u64, second: u64) {
        let r = self.live.merge(first, second);
        if self.sent(r) {
            self.order.retain(|o| *o != second);
        }
    }

    fn split(&mut self, id: u64, at: usize, new: u64) {
        let r = self.live.split(id, at, new);
        if self.sent(r)
            && let Some(i) = self.at(id)
        {
            self.order.insert(i + 1, new);
        }
    }

    fn insert(&mut self, at: usize, block: &SeedBlock) {
        let r = self.live.insert_block(at, block);
        if self.sent(r) {
            self.order.insert(at.min(self.order.len()), block.id);
        }
    }

    /// Bring block `id` to `want`'s text, runs and attributes, `have` being what it holds.
    fn reconcile(&mut self, have: &SeedBlock, want: &SeedBlock) {
        let id = have.id;
        let mut runs = have.runs.clone();
        if let Some(e) = diff::text_edit(&have.text, &want.text) {
            if e.del > 0 {
                let r = self.live.delete_text(id, e.at, e.del);
                self.sent(r);
            }
            if !e.ins.is_empty() {
                let r = self.live.insert_text(id, e.at, &e.ins, &diff::attrs_at(&want.runs, e.at));
                self.sent(r);
            }
            runs = self.live.block(id).map(|b| b.runs).unwrap_or_default();
        }
        for (at, len, attrs) in diff::format_edits(&runs, &want.runs, want.text.chars().count()) {
            let r = self.live.format(id, at, len, &attrs);
            self.sent(r);
        }
        if have.attrs != want.attrs {
            let r = self.live.set_attrs(id, &want.attrs);
            self.sent(r);
        }
    }
}

/// Whether two bodies hold the very same blocks.
fn same_body(a: &Document, b: &Document) -> bool {
    a.body.len() == b.body.len() && a.body.iter().zip(&b.body).all(|(x, y)| Arc::ptr_eq(x, y))
}

impl LiveMap {
    /// The map for `doc`, the session's document right after seeding (its ids written), as
    /// client `space` (0 until the relay grants one; see [`LiveMap::set_space`]).
    pub fn new(doc: &Document, space: u32) -> LiveMap {
        let ids = seed_ids(doc);
        let tables = doc.body.iter().zip(ids).filter(|(b, _)| b.as_table().is_some()).map(|(_, id)| id).collect();
        LiveMap { space, next: 0, tables, removed: BTreeMap::new(), recovered: BTreeMap::new() }
    }

    /// Mint new ids in `space` from now on.
    pub fn set_space(&mut self, space: u32) {
        if space != self.space {
            self.space = space;
            self.next = 0;
        }
    }

    /// A fresh id `(space << 32) | next`, unused in `live` and not in `taken`.
    fn mint(&mut self, live: &LiveDoc, taken: &HashSet<u64>) -> u64 {
        loop {
            self.next = self.next.wrapping_add(1).max(1);
            let id = (u64::from(self.space) << 32) | u64::from(self.next);
            if !taken.contains(&id) && live.block(id).is_none() {
                return id;
            }
        }
    }

    /// The updates that take `live` from `before` to `after`: one reported command.
    ///
    /// Paragraphs are matched by id. A paragraph equal in both sends nothing; a changed one sends
    /// a minimal text change (common prefix and suffix), format calls over the changed runs and
    /// its attributes. A new paragraph right after one whose tail it took is a `split`; a removed
    /// paragraph whose tail the one before it took is a `merge`; other new and removed blocks are
    /// inserted and removed, and a block that moved is removed and inserted under a new id.
    /// Paragraphs that need an id (new ones, split tails, moved ones) get `(space << 32) | next`,
    /// returned in [`LocalEdit::pids`] for the caller to write back.
    ///
    /// `live` must hold what `before` holds: the ids of the last edit written back, every remote
    /// change applied.
    pub fn local_updates(&mut self, live: &mut LiveDoc, before: &Document, after: &Document) -> Result<LocalEdit, MapError> {
        if same_body(before, after) {
            return Ok(LocalEdit::default());
        }
        if self.space == 0 {
            return Err(MapError::NoSpace);
        }
        let old = Ids::of(before, &self.tables);
        let order = live.order();
        let (want, pids) = self.want_ids(live, &order, &old, after);
        let mut ops = Ops { live, order, out: LocalEdit { pids, ..LocalEdit::default() } };
        let mut dirty = HashSet::new();
        self.remove_gone(&mut ops, &old, after, &want, &mut dirty);
        let want = self.fix_order(&mut ops, after, want);
        let fresh = Self::insert_new(&mut ops, &old, after, &want, &mut dirty);
        Self::update_kept(&mut ops, &old, after, &want, &fresh, &dirty);
        self.tables = after.body.iter().zip(&want).filter(|(b, _)| b.as_table().is_some()).map(|(_, id)| *id).collect();
        Ok(ops.out)
    }

    /// The id each block of `after` should have, and the paragraphs that need a new one.
    fn want_ids(&mut self, live: &LiveDoc, order: &[u64], old: &Ids, after: &Document) -> (Vec<u64>, Vec<(usize, u64)>) {
        let live_ids: HashSet<u64> = order.iter().copied().collect();
        let mut tables = self.match_tables(old, after).into_iter();
        let mut taken = HashSet::new();
        let mut pids = Vec::new();
        let mut want = Vec::with_capacity(after.body.len());
        for (i, b) in after.body.iter().enumerate() {
            let own = match &**b {
                Block::Para(p) => Some(p.pid),
                Block::Table(_) => tables.next().flatten(),
            };
            let id = match own.filter(|id| *id != 0 && live_ids.contains(id) && !taken.contains(id)) {
                Some(id) => id,
                None => {
                    let id = self.mint(live, &taken);
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
        let mut prev: Option<u64> = None;
        let mut gone: Vec<u64> = Vec::new();
        for id in ops.order.clone().into_iter().chain(std::iter::once(u64::MAX)) {
            if id != u64::MAX && !at.contains_key(&id) {
                gone.push(id);
                continue;
            }
            if let Some((last, rest)) = gone.split_last() {
                for s in rest {
                    ops.remove(*s);
                    self.removed.insert(*s, prev);
                }
                let into = prev.filter(|p| old.paras.contains_key(p)).and_then(|p| Some((p, after.body.get(*at.get(&p)?)?.as_para()?)));
                match (into, old.paras.get(last)) {
                    (Some((p, now)), Some(s)) if diff::common_suffix(&now.text, &s.text) > 0 => {
                        ops.merge(p, *last);
                        dirty.insert(p);
                    }
                    _ => ops.remove(*last),
                }
                self.removed.insert(*last, prev);
                gone.clear();
            }
            prev = Some(id);
        }
    }

    /// Remove and re-add, under new ids, the kept blocks that moved: those off one longest run
    /// of blocks still in `after`'s order.
    fn fix_order(&mut self, ops: &mut Ops, after: &Document, mut want: Vec<u64>) -> Vec<u64> {
        let at: HashMap<u64, usize> = want.iter().enumerate().map(|(i, id)| (*id, i)).collect();
        let seq: Vec<usize> = ops.order.iter().filter_map(|id| at.get(id).copied()).collect();
        if seq.windows(2).all(|w| w[0] < w[1]) {
            return want;
        }
        let keep = diff::increasing(&seq);
        let mut taken: HashSet<u64> = want.iter().copied().collect();
        for (i, k) in seq.iter().zip(keep) {
            if k {
                continue;
            }
            let Some(old) = want.get(*i).copied() else { continue };
            let prev = ops.at(old).and_then(|j| j.checked_sub(1)).and_then(|j| ops.order.get(j).copied());
            ops.remove(old);
            self.removed.insert(old, prev);
            let id = self.mint(ops.live, &taken);
            taken.insert(id);
            if let Some(w) = want.get_mut(*i) {
                *w = id;
            }
            if after.body.get(*i).is_some_and(|b| b.as_para().is_some()) {
                ops.out.pids.push((*i, id));
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
            let prev = i.checked_sub(1).and_then(|j| want.get(j).copied());
            if let (Some(p), Some(new)) = (prev, block.as_para())
                && let Some(k) = Self::split_point(ops, old, after, i - 1, p, new, dirty)
            {
                ops.split(p, k, *id);
                dirty.insert(p);
                dirty.insert(*id);
                continue;
            }
            let at = prev.and_then(|p| ops.at(p)).map_or(0, |j| j + 1);
            ops.insert(at, &map::block_seed(*id, block));
            fresh.insert(*id);
        }
        fresh
    }

    /// Where to split paragraph `p` (at `after` index `i`) so its tail becomes `new`: when `p`
    /// is an old paragraph not split yet in this edit and `new` ends with the text past what
    /// `p` kept.
    fn split_point(ops: &Ops, old: &Ids, after: &Document, i: usize, p: u64, new: &Paragraph, dirty: &HashSet<u64>) -> Option<usize> {
        let head = after.body.get(i)?.as_para()?;
        let was = old.paras.get(&p)?;
        if dirty.contains(&p) {
            return None;
        }
        let k = diff::common_prefix(&was.text, &head.text);
        let tail: String = was.text.chars().skip(k).collect();
        (ops.at(p).is_some() && new.text.ends_with(&tail)).then_some(k)
    }

    /// Bring every kept block that changed to its content in `after`.
    fn update_kept(ops: &mut Ops, old: &Ids, after: &Document, want: &[u64], fresh: &HashSet<u64>, dirty: &HashSet<u64>) {
        for (b, id) in after.body.iter().zip(want) {
            if fresh.contains(id) {
                continue;
            }
            match &**b {
                Block::Para(p) => {
                    let changed = dirty.contains(id) || old.paras.get(id).is_none_or(|was| *was != p);
                    if changed && let Some(have) = ops.live.block(*id) {
                        ops.reconcile(&have, &map::para_seed(*id, p));
                    }
                }
                Block::Table(_) => {
                    if old.tables.get(id).is_none_or(|was| !Arc::ptr_eq(was, b) && ***was != **b) {
                        let r = ops.live.set_attrs(*id, &map::opaque_attrs(b));
                        ops.sent(r);
                    }
                }
            }
        }
    }

    /// Merge an update from the relay into `live`, and say what changed for
    /// [`LiveMap::apply_changes`]. Text typed into a paragraph this peer removed is recovered
    /// (see the [crate documentation](crate)): the recovering inserts are in the changes and in
    /// [`Remote::updates`], which go to the relay.
    pub fn receive(&mut self, live: &mut LiveDoc, update: &[u8]) -> Result<Remote, MapError> {
        let mut changes = live.apply(update)?;
        let order: HashSet<u64> = live.order().into_iter().collect();
        let mut orphans = BTreeSet::new();
        for c in &changes {
            if let BlockChange::Text { id } = c
                && !order.contains(id)
                && self.removed.contains_key(id)
            {
                orphans.insert(*id);
            }
        }
        let mut updates = Vec::new();
        for id in orphans {
            self.recover(live, id, &mut changes, &mut updates);
        }
        Ok(Remote { changes, updates })
    }

    /// Put tombstone `gone`'s text in a recovered paragraph, or bring the one made for it
    /// earlier up to date.
    fn recover(&mut self, live: &mut LiveDoc, gone: u64, changes: &mut Vec<BlockChange>, updates: &mut Vec<Vec<u8>>) {
        let Some(src) = live.block(gone) else { return };
        let order = live.order();
        let mut ops = Ops { live, order, out: LocalEdit::default() };
        if let Some(r) = self.recovered.get(&gone).copied() {
            if ops.at(r).is_some()
                && let Some(have) = ops.live.block(r)
            {
                let want = SeedBlock { id: r, attrs: have.attrs.clone(), ..src };
                ops.reconcile(&have, &want);
                changes.push(BlockChange::Text { id: r });
            }
        } else if self.space != 0 {
            let id = self.mint(ops.live, &HashSet::new());
            let at = self.neighbour(&ops, gone).map_or(0, |j| j + 1);
            let block = SeedBlock { id, attrs: map::recovered_attrs(&src.attrs), ..src };
            ops.insert(at, &block);
            if ops.at(id).is_some() {
                self.recovered.insert(gone, id);
                changes.push(BlockChange::Inserted { at, id });
            }
        } else {
            log::warn!("live: text typed into a removed paragraph was not recovered: no client space yet");
        }
        updates.extend(ops.out.updates);
    }

    /// Where the nearest block before removed block `gone` that survives is now.
    fn neighbour(&self, ops: &Ops, gone: u64) -> Option<usize> {
        let mut cur = self.removed.get(&gone).copied().flatten();
        for _ in 0..=self.removed.len() {
            let id = cur?;
            if let Some(j) = ops.at(id) {
                return Some(j);
            }
            cur = self.removed.get(&id).copied().flatten();
        }
        None
    }

    /// Rebuild the body of `doc` from `live` after `changes`: in `live`'s order, every block a
    /// change names read from `live`, every other one kept as it is.
    ///
    /// Paragraphs are found by id, so `doc`'s new paragraphs must have theirs (see
    /// [`LocalEdit::pids`]); a paragraph without one is not in `live` and is dropped.
    pub fn apply_changes(&mut self, doc: &mut Document, live: &LiveDoc, changes: &[BlockChange]) {
        if changes.is_empty() {
            return;
        }
        let touched: HashSet<u64> = changes
            .iter()
            .filter_map(|c| match c {
                BlockChange::Inserted { id, .. } | BlockChange::Text { id } | BlockChange::Attrs { id } => Some(*id),
                BlockChange::Removed { .. } => None,
            })
            .collect();
        let mut kept: HashMap<u64, Arc<Block>> =
            doc.body.iter().filter(|b| b.as_para().is_some_and(|p| p.pid != 0)).filter_map(|b| Some((b.as_para()?.pid, b.clone()))).collect();
        kept.extend(self.tables.iter().copied().zip(doc.body.iter().filter(|b| b.as_table().is_some()).cloned()));
        let mut body = Vec::new();
        let mut tables = Vec::new();
        let mut recovered = false;
        for id in live.order() {
            let block = match kept.remove(&id).filter(|_| !touched.contains(&id)) {
                Some(b) => b,
                None => {
                    let Some(seed) = live.block(id) else { continue };
                    let b = map::seed_block(&seed);
                    recovered |= b.as_para().is_some_and(|p| p.props.style.as_deref() == Some(RECOVERED_STYLE_ID));
                    b
                }
            };
            if block.as_table().is_some() {
                tables.push(id);
            }
            body.push(block);
        }
        doc.body = body;
        self.tables = tables;
        if recovered {
            map::ensure_recovered_style(doc);
        }
    }
}

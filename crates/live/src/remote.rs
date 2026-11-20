//! Remote updates: merged into the replica, orphaned typing recovered or forwarded, and the
//! session's body rebuilt from what changed.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use craft_collab::{BlockChange, LiveDoc, SeedBlock};
use wordcraft_doc::{Block, Document};

use crate::ops::Ops;
use crate::{LiveMap, MapError, RECOVERED_STYLE_ID, REMEMBERED, Remote, diff, map};

impl LiveMap {
    /// Merge an update from the relay into `live`, and say what changed for
    /// [`LiveMap::apply_changes`]. Text typed into a paragraph this peer removed is recovered,
    /// and text typed into one it moved is forwarded to the moved paragraph (see the
    /// [crate documentation](crate)); those steps are in the changes and in
    /// [`Remote::updates`], which go to the relay.
    pub fn receive(&mut self, live: &mut LiveDoc, update: &[u8]) -> Result<Remote, MapError> {
        let changes = live.apply(update)?;
        Ok(self.received(live, changes))
    }

    /// [`LiveMap::receive`] for updates already merged into `live` (as `craft_collab::Live`
    /// merges them and reports `Work::Doc`): `changes` is what they changed. Recovers and
    /// forwards orphaned typing the same way.
    pub fn received(&mut self, live: &mut LiveDoc, mut changes: Vec<BlockChange>) -> Remote {
        // Only blocks this peer removed or moved can be orphans; the order is read only then.
        let mut orphans: BTreeSet<u64> = changes
            .iter()
            .filter_map(|c| match c {
                BlockChange::Text { id } if self.moved.contains(id) || self.removed.contains(id) => Some(*id),
                _ => None,
            })
            .collect();
        if !orphans.is_empty() {
            let order: HashSet<u64> = live.order().into_iter().collect();
            orphans.retain(|id| !order.contains(id));
        }
        let mut updates = Vec::new();
        for id in orphans {
            if self.moved.contains(&id) {
                self.forward(live, id, &mut changes, &mut updates);
            } else if self.removed.contains(&id) {
                self.recover(live, id, &mut changes, &mut updates);
            }
        }
        Remote { changes, updates }
    }

    /// Put tombstone `gone`'s text in a recovered paragraph, or bring the one made for it
    /// earlier up to date.
    fn recover(&mut self, live: &mut LiveDoc, gone: u64, changes: &mut Vec<BlockChange>, updates: &mut Vec<Vec<u8>>) {
        let Some(src) = live.block(gone) else { return };
        let mut ops = Ops::new(live);
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

    /// Carry typing that landed in `old`, a block this peer moved, over to the paragraph it
    /// moved to: the edit between the old block's text as last forwarded and its text now,
    /// placed in the moved paragraph by following that one's own edits since.
    fn forward(&mut self, live: &mut LiveDoc, old: u64, changes: &mut Vec<BlockChange>, updates: &mut Vec<Vec<u8>>) {
        let (Some(tomb), Some(m)) = (live.block(old), self.moved.get(&old).cloned()) else { return };
        let mut ops = Ops::new(live);
        let mut to = m.to;
        for _ in 0..REMEMBERED {
            match self.moved.get(&to).filter(|_| ops.at(to).is_none()) {
                Some(next) => to = next.to,
                None => break,
            }
        }
        let Some(have) = ops.live.block(to).filter(|_| ops.at(to).is_some()) else {
            log::warn!("live: text typed into a moved paragraph was not forwarded: it is gone");
            return;
        };
        if let Some(e) = diff::text_edit(&m.from_text, &tomb.text) {
            let len = have.text.chars().count();
            let at = diff::follow(&m.to_text, &have.text, e.at).min(len);
            let end = diff::follow(&m.to_text, &have.text, e.at + e.del).clamp(at, len);
            let attrs = diff::attrs_at(&tomb.runs, e.at);
            ops.edit_text(to, &diff::TextEdit { at, del: end - at, ins: e.ins }, &attrs);
            changes.push(BlockChange::Text { id: to });
        }
        let to_text = ops.live.block(to).map(|b| b.text).unwrap_or_default();
        if let Some(entry) = self.moved.get_mut(&old) {
            entry.from_text = tomb.text;
            entry.to_text = to_text;
        }
        updates.extend(ops.out.updates);
    }

    /// When `changes` only edit blocks `doc` has, replace just those, without reading the order.
    /// Returns whether that was so.
    fn replace_in_place(&mut self, doc: &mut Document, live: &LiveDoc, changes: &[BlockChange]) -> bool {
        let mut ids = HashSet::new();
        for c in changes {
            match c {
                BlockChange::Text { id } | BlockChange::Attrs { id } => ids.insert(*id),
                _ => return false,
            };
        }
        let mut at = Vec::new();
        let mut k = 0;
        for (i, b) in doc.body.iter().enumerate() {
            let id = match &**b {
                Block::Para(p) => p.pid,
                Block::Table(_) => {
                    k += 1;
                    self.tables.get(k - 1).copied().unwrap_or(0)
                }
            };
            if id != 0 && ids.contains(&id) {
                at.push((i, id));
            }
        }
        if at.len() != ids.len() {
            return false;
        }
        let mut recovered = false;
        for (i, id) in at {
            let Some(seed) = live.block(id) else { return false };
            let b = map::seed_block(&seed);
            recovered |= b.as_para().is_some_and(|p| p.props.style.as_deref() == Some(RECOVERED_STYLE_ID));
            if let Some(slot) = doc.body.get_mut(i) {
                *slot = b;
            }
        }
        if recovered {
            map::ensure_recovered_style(doc);
        }
        true
    }

    /// Where the nearest block before removed block `gone` that survives is now.
    fn neighbour(&self, ops: &Ops, gone: u64) -> Option<usize> {
        let mut cur = self.removed.get(&gone).copied().flatten();
        for _ in 0..=REMEMBERED {
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
    /// [`LocalEdit::pids`](crate::LocalEdit::pids)); a paragraph without one is not in `live`
    /// and is dropped.
    pub fn apply_changes(&mut self, doc: &mut Document, live: &LiveDoc, changes: &[BlockChange]) {
        if changes.is_empty() {
            return;
        }
        if self.replace_in_place(doc, live, changes) {
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
            doc.body.iter().filter_map(|b| Some((b.as_para().map(|p| p.pid).filter(|pid| *pid != 0)?, b.clone()))).collect();
        kept.extend(self.tables.iter().copied().zip(doc.body.iter().filter(|b| b.as_table().is_some()).cloned()));
        let mut body: Vec<Arc<Block>> = Vec::with_capacity(doc.body.len());
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

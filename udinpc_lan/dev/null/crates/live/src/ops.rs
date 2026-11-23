//! Steps on the replica, with a mirror of its order indexed by id.

use std::collections::HashMap;

use craft_collab::{LiveDoc, LiveError, SeedBlock};

use crate::{LocalEdit, diff};

/// The replica plus a mirror of its order, and what the edit sent so far.
pub(crate) struct Ops<'a> {
    pub live: &'a mut LiveDoc,
    pub order: Vec<u64>,
    index: HashMap<u64, usize>,
    pub out: LocalEdit,
}

impl<'a> Ops<'a> {
    pub fn new(live: &'a mut LiveDoc) -> Ops<'a> {
        let order = live.order();
        let index = order.iter().enumerate().map(|(i, id)| (*id, i)).collect();
        Ops { live, order, index, out: LocalEdit::default() }
    }

    /// Steps that only edit blocks in place, which need no order.
    pub fn in_place(live: &'a mut LiveDoc) -> Ops<'a> {
        Ops { live, order: Vec::new(), index: HashMap::new(), out: LocalEdit::default() }
    }

    pub fn sent(&mut self, r: Result<Vec<u8>, LiveError>) -> bool {
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

    /// Where `id` is in the order.
    pub fn at(&self, id: u64) -> Option<usize> {
        self.index.get(&id).copied()
    }

    fn reindex(&mut self, from: usize) {
        for (i, id) in self.order.iter().enumerate().skip(from) {
            self.index.insert(*id, i);
        }
    }

    fn take_out(&mut self, id: u64) {
        if let Some(i) = self.index.remove(&id) {
            if i < self.order.len() {
                self.order.remove(i);
            }
            self.reindex(i);
        }
    }

    fn put(&mut self, at: usize, id: u64) {
        let at = at.min(self.order.len());
        self.order.insert(at, id);
        self.reindex(at);
    }

    pub fn remove(&mut self, id: u64) {
        let r = self.live.remove_block(id);
        if self.sent(r) {
            self.take_out(id);
        }
    }

    pub fn merge(&mut self, first: u64, second: u64) {
        let r = self.live.merge(first, second);
        if self.sent(r) {
            self.take_out(second);
        }
    }

    pub fn split(&mut self, id: u64, at: usize, new: u64) {
        let r = self.live.split(id, at, new);
        if self.sent(r)
            && let Some(i) = self.at(id)
        {
            self.put(i + 1, new);
        }
    }

    pub fn insert(&mut self, at: usize, block: &SeedBlock) {
        let r = self.live.insert_block(at, block);
        if self.sent(r) {
            self.put(at, block.id);
        }
    }

    /// Apply one text edit to block `id`.
    pub fn edit_text(&mut self, id: u64, e: &diff::TextEdit, attrs: &craft_collab::Attributes) {
        if e.del > 0 {
            let r = self.live.delete_text(id, e.at, e.del);
            self.sent(r);
        }
        if !e.ins.is_empty() {
            let r = self.live.insert_text(id, e.at, &e.ins, attrs);
            self.sent(r);
        }
    }

    /// Bring block `id` to `want`'s text, runs and attributes, `have` being what it holds.
    pub fn reconcile(&mut self, have: &SeedBlock, want: &SeedBlock) {
        let id = have.id;
        let mut runs = have.runs.clone();
        if let Some(e) = diff::text_edit(&have.text, &want.text) {
            self.edit_text(id, &e, &diff::attrs_at(&want.runs, e.at));
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

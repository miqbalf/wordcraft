//! Shared paragraph ids ([`Paragraph::pid`]): keeping them unique in a document and
//! following which paragraphs an edit replaced.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::{Block, Blocks, Document, Paragraph, StoryRef};

/// Every paragraph in `b` (tables descended) as never shared: content copied into a document
/// is a new paragraph, not the one it was copied from.
/// A block shared with other copies is copied only when it holds an id.
pub(crate) fn unshare_block(b: &mut Block) {
    match b {
        Block::Para(p) => p.pid = 0,
        Block::Table(t) => {
            for inner in t.rows.iter_mut().flat_map(|r| &mut r.cells).flat_map(|c| &mut c.blocks) {
                unshare_shared(inner);
            }
        }
    }
}

/// [`unshare_block`] for a shared block: it is copied only when it holds an id.
pub(crate) fn unshare_shared(b: &mut Arc<Block>) {
    let mut any = false;
    for_each_para(b, &mut |p| any |= p.pid != 0);
    if any {
        unshare_block(Arc::make_mut(b));
    }
}

fn for_each_para_mut(b: &mut Block, f: &mut dyn FnMut(&mut Paragraph)) {
    match b {
        Block::Para(p) => f(p),
        Block::Table(t) => {
            for row in &mut t.rows {
                for cell in &mut row.cells {
                    for inner in &mut cell.blocks {
                        for_each_para_mut(Arc::make_mut(inner), f);
                    }
                }
            }
        }
    }
}

fn for_each_para(b: &Block, f: &mut dyn FnMut(&Paragraph)) {
    match b {
        Block::Para(p) => f(p),
        Block::Table(t) => {
            for inner in t.rows.iter().flat_map(|r| &r.cells).flat_map(|c| &c.blocks) {
                for_each_para(inner, f);
            }
        }
    }
}

impl Document {
    /// The stories' block lists: the body first, then the parts in id order.
    fn stories_mut(&mut self) -> impl Iterator<Item = &mut Blocks> {
        std::iter::once(&mut self.body).chain(self.parts.values_mut().map(|p| &mut p.blocks))
    }

    fn stories(&self) -> impl Iterator<Item = (StoryRef, &Blocks)> {
        std::iter::once((StoryRef::Body, &self.body)).chain(self.parts.iter().map(|(id, p)| (StoryRef::Part(*id), &p.blocks)))
    }

    /// Bump the revision of every paragraph in a top-level block that is not the same shared
    /// block as one of `before`'s, so caches keyed on it see an edit made without
    /// [`Paragraph::touch`]. Returns how many blocks it touched.
    pub fn touch_changed_since(&mut self, before: &Document) -> usize {
        let kept: HashSet<*const Block> = before.stories().flat_map(|(_, bl)| bl.iter().map(Arc::as_ptr)).collect();
        let mut touched = 0;
        for story in self.stories_mut() {
            for b in story.iter_mut().filter(|b| !kept.contains(&Arc::as_ptr(b))) {
                for_each_para_mut(Arc::make_mut(b), &mut Paragraph::touch);
                touched += 1;
            }
        }
        touched
    }

    /// Make the shared paragraph ids unique again: when two paragraphs have the same nonzero
    /// id, the one in a block `before` already had (the same shared block) keeps it, else the
    /// first in document order, and the others become never shared (`0`). Returns how many ids
    /// were cleared.
    pub fn unshare_duplicate_pids(&mut self, before: &Document) -> usize {
        // Each id's places: (story, paragraph index in the story, in a block `before` had).
        let kept: HashSet<*const Block> = before.stories().flat_map(|(_, bl)| bl.iter().map(Arc::as_ptr)).collect();
        let mut places: HashMap<u64, Vec<(StoryRef, usize, bool)>> = HashMap::new();
        for (story, blocks) in self.stories() {
            let mut i = 0usize;
            for b in blocks {
                let old = kept.contains(&Arc::as_ptr(b));
                for_each_para(b, &mut |p| {
                    if p.pid != 0 {
                        places.entry(p.pid).or_default().push((story, i, old));
                    }
                    i += 1;
                });
            }
        }
        // The (story, index) that keeps each duplicated id.
        let keep: HashMap<u64, (StoryRef, usize)> = places
            .into_iter()
            .filter(|(_, at)| at.len() > 1)
            .filter_map(|(pid, at)| at.iter().find(|a| a.2).or_else(|| at.first()).map(|a| (pid, (a.0, a.1))))
            .collect();
        if keep.is_empty() {
            return 0;
        }
        let mut cleared = 0;
        let ids: Vec<StoryRef> = std::iter::once(StoryRef::Body).chain(self.parts.keys().map(|id| StoryRef::Part(*id))).collect();
        for (story, blocks) in ids.into_iter().zip(self.stories_mut()) {
            let mut i = 0usize;
            for b in blocks.iter_mut() {
                let mut hit = false;
                for_each_para(b, &mut |p| hit |= keep.contains_key(&p.pid));
                if !hit {
                    for_each_para(b, &mut |_| i += 1);
                    continue;
                }
                for_each_para_mut(Arc::make_mut(b), &mut |p| {
                    if keep.get(&p.pid).is_some_and(|k| *k != (story, i)) {
                        p.pid = 0;
                        cleared += 1;
                    }
                    i += 1;
                });
            }
        }
        cleared
    }
}

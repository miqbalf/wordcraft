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

    /// Every paragraph's id, story by story, in document order (tables descended).
    fn pid_lists(&self) -> Vec<(StoryRef, Vec<u64>)> {
        self.stories()
            .map(|(story, blocks)| {
                let mut ids = Vec::new();
                for b in blocks {
                    for_each_para(b, &mut |p| ids.push(p.pid));
                }
                (story, ids)
            })
            .collect()
    }

    /// Whether any paragraph in the document has id `pid`.
    fn has_pid(&self, pid: u64) -> bool {
        let mut found = false;
        for (_, blocks) in self.stories() {
            for b in blocks {
                for_each_para(b, &mut |p| found |= p.pid == pid);
                if found {
                    return true;
                }
            }
        }
        false
    }

    /// A block about to go into the document: its paragraphs whose ids are in the document
    /// already are copies, so they become never shared. A block that holds no id costs nothing.
    pub(crate) fn unshare_copies(&self, b: &mut Block) {
        let mut ids = Vec::new();
        for_each_para(b, &mut |p| {
            if p.pid != 0 {
                ids.push(p.pid);
            }
        });
        let copied: HashSet<u64> = ids.into_iter().filter(|pid| self.has_pid(*pid)).collect();
        if !copied.is_empty() {
            for_each_para_mut(b, &mut |p| {
                if copied.contains(&p.pid) {
                    p.pid = 0;
                }
            });
        }
    }

    /// Make the shared paragraph ids unique again after an edit. When paragraphs share a
    /// nonzero id, the one that sits where the id was in `before` keeps it: the same story
    /// and the same neighbours (the nearest shared paragraphs with other ids before and after
    /// it), then the same paragraph index, then the first in document order. The others become
    /// never shared (`0`). Returns how many ids were cleared.
    ///
    /// Two copies right next to each other with nothing else between them look the same from
    /// where they sit; the index then decides.
    pub fn unshare_duplicate_pids(&mut self, before: &Document) -> usize {
        // (story, index, previous other id, next other id) of the paragraph at `i`.
        fn place(story: StoryRef, ids: &[u64], i: usize) -> (StoryRef, usize, Option<u64>, Option<u64>) {
            let me = ids.get(i).copied().unwrap_or(0);
            let other = |q: &&u64| **q != 0 && **q != me;
            let prev = ids.get(..i).and_then(|h| h.iter().rev().find(other)).copied();
            let next = ids.get(i + 1..).and_then(|t| t.iter().find(other)).copied();
            (story, i, prev, next)
        }
        let now = self.pid_lists();
        let mut at: HashMap<u64, Vec<(StoryRef, usize)>> = HashMap::new();
        for (story, ids) in &now {
            for (i, pid) in ids.iter().enumerate().filter(|(_, p)| **p != 0) {
                at.entry(*pid).or_default().push((*story, i));
            }
        }
        at.retain(|_, v| v.len() > 1);
        if at.is_empty() {
            return 0;
        }
        let was: HashMap<u64, (StoryRef, usize, Option<u64>, Option<u64>)> = before
            .pid_lists()
            .iter()
            .flat_map(|(story, ids)| {
                ids.iter().enumerate().filter(|(_, p)| at.contains_key(p)).map(|(i, p)| (*p, place(*story, ids, i))).collect::<Vec<_>>()
            })
            .collect();
        let ids_of = |story: StoryRef| now.iter().find(|(s, _)| *s == story).map(|(_, ids)| ids.as_slice()).unwrap_or(&[]);
        // The (story, index) that keeps each duplicated id.
        let keep: HashMap<u64, (StoryRef, usize)> = at
            .iter()
            .filter_map(|(pid, places)| {
                let score = |(story, i): &(StoryRef, usize)| match was.get(pid) {
                    Some(old) if old.0 == *story => {
                        let here = place(*story, ids_of(*story), *i);
                        (u8::from(here.2 == old.2) + u8::from(here.3 == old.3), u8::from(here.1 == old.1))
                    }
                    _ => (0, 0),
                };
                // The best place; the first one among equals.
                let best = places.iter().rev().max_by_key(|p| score(p))?;
                Some((*pid, *best))
            })
            .collect();
        let mut cleared = 0;
        let stories: Vec<StoryRef> = std::iter::once(StoryRef::Body).chain(self.parts.keys().map(|id| StoryRef::Part(*id))).collect();
        for (story, blocks) in stories.into_iter().zip(self.stories_mut()) {
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

//! The safety net: the session's body checked against the body the shared copy holds.
//!
//! The mapping keeps the two equal edit by edit; nothing else would notice if a bug let them
//! drift apart, and the window would then show (and the user edit) a document that is not the
//! one the room saves. [`LiveMap::drift`] compares the two document models, block by block (never
//! the replica's bytes, whose encoding is not canonical), and [`LiveEditing`](crate::LiveEditing)
//! asks it every few seconds, rebuilding the body from the shared copy when they differ.

use std::sync::Arc;

use craft_collab::LiveDoc;
use wordcraft_doc::{Block, Document, Paragraph};

use crate::{LiveMap, map};

/// The body the shared copy holds as every peer shows it: its blocks in order, plus the empty
/// paragraph the session adds when a body would have none or end in a table.
fn shared_body(live: &LiveDoc) -> Vec<Arc<Block>> {
    let mut body: Vec<Arc<Block>> = live.order().into_iter().filter_map(|id| live.block(id)).map(|seed| map::seed_block(&seed)).collect();
    if !body.iter().any(|b| b.as_para().is_some()) || body.last().is_some_and(|b| b.as_table().is_some()) {
        body.push(Arc::new(Block::Para(Paragraph::new())));
    }
    body
}

/// Whether paragraph `have` (the session's) holds what `want` (read from the shared copy) does,
/// its id included. Run boundaries the shared copy merges are not a difference.
fn same_para(have: &Paragraph, want: &Paragraph) -> bool {
    if have.pid != want.pid || have.text != want.text {
        return false;
    }
    if have == want {
        return true;
    }
    let mut have = have.clone();
    have.normalize();
    have == *want
}

impl LiveMap {
    /// Where the body of `doc` (the session's document) differs from the body `live` holds, in
    /// words for the log; `None` when they are the same. Call it only with every local edit
    /// shared and every remote one applied.
    pub fn drift(&self, doc: &Document, live: &LiveDoc) -> Option<String> {
        let want = shared_body(live);
        if want.len() != doc.body.len() {
            return Some(format!("{} blocks here, {} in the shared copy", doc.body.len(), want.len()));
        }
        for (i, (have, want)) in doc.body.iter().zip(&want).enumerate() {
            let same = Arc::ptr_eq(have, want)
                || match (&**have, &**want) {
                    (Block::Para(p), Block::Para(q)) => same_para(p, q),
                    (a, b) => a == b,
                };
            if !same {
                return Some(format!("block {i} differs"));
            }
        }
        None
    }
}

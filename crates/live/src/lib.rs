//! Live co-editing for WordCraft: the body of a [`Document`] mapped to and from the shared block
//! document of `craft-collab` ([`LiveDoc`]), so that sessions editing the same file converge.
//!
//! # The mapping
//!
//! [`map`] holds it: a body paragraph is one block whose id is its
//! [`Paragraph::pid`](wordcraft_doc::Paragraph::pid), its text the block's text and its runs the
//! text's formatting; any other body block (a table) is one opaque block. Only the body is
//! shared. Headers, notes, styles, numbering, media, comments and the rest are not, and live
//! editing inside a table is not either: a table edit replaces the whole table, and the last
//! writer wins. Commands that would change what is not shared are listed by [`is_shareable`];
//! [`LiveMap::local_updates`] refuses such an edit with [`MapError::NotShareable`] as a safety
//! net. A delete across a section break whose following section is the last one changes the
//! document's last section setup (`last_section`), which is outside the body, so it is refused;
//! one whose following section ends at a paragraph changes that paragraph's `"s"` attribute,
//! which is shared.
//!
//! # The flow
//!
//! 1. Every peer opens the same file, writes the seed ids with
//!    `session.assign_pids(assign_seed_pids)`, seeds a [`LiveDoc`] from [`seed_blocks`], takes
//!    its client space with `set_client`, and makes a [`LiveMap::new`].
//! 2. Each local command the session's observer reports goes through
//!    [`LiveMap::local_updates`]. The updates go to the relay; the new paragraph ids go back into
//!    the session with [`share_pids`] before the next command or remote edit. When it is
//!    refused ([`MapError::refused`]: [`MapError::NotShareable`] or [`MapError::NoSpace`])
//!    nothing was sent: put the session back to `before` with
//!    `session.apply_remote(|d| *d = before.clone())` and tell the user why ([`Refusal`]).
//!
//!    The engine reports one edit per top-level command, however many steps it runs inside
//!    (a macro, Repeat), so restoring that edit's `before` restores the state before the
//!    command.
//! 3. Each update from the relay goes through [`LiveMap::receive`], and the changes it reports
//!    into the session with `session.apply_remote(|d| map.apply_changes(d, &live, &changes))`.
//!    The updates `receive` returns (recovered and forwarded text) go to the relay too.
//!
//! # The session pump
//!
//! [`LiveEditing`] does all of the above over a `craft_collab::Live` link, once per frame, and
//! tracks the live status, the other editors and their carets; see the [`session`] module. The
//! session state machine underneath is craft-live's, shared by every Craft app: this crate is
//! WordCraft's side of it (the mapping, the engine hooks and the commands allowed live).
//!
//! # Before the relay grants a space
//!
//! Without a client space no paragraph id can be minted, so the app takes no input until the
//! space is granted ([`LiveEditing::editable`] says when). That is the rule; the backstop is that an edit reported
//! anyway is refused with [`MapError::NoSpace`], nothing is held, and the caller puts `before`
//! back like any other refusal. [`LiveMap::grant`] then only sets the space.
//!
//! # A peer joining late
//!
//! A peer that starts from a checkpoint instead of the file does not seed: it builds the replica
//! with `LiveDoc::from_state(&snapshot)` and `set_client`, opens the file for everything that is
//! not shared (styles, settings…), clears its body, makes a [`LiveMap::new`] for that empty
//! body, and fills the body with [`LiveMap::apply_changes`] over a `BlockChange::Inserted` for
//! every id in `live.order()`. Only then does it install the observer.
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
//!
//! A paragraph this peer moved is re-added under a new id (the shared order cannot move a
//! block). It is not a removal: typing that lands in the old block is forwarded into the moved
//! paragraph instead, again by this peer only.
//!
//! The peer remembers the last [`REMEMBERED`] removals and moves; a race against an older one
//! is not recovered.

mod diff;
mod local;
pub mod map;
mod ops;
mod remote;
pub mod session;
mod shareable;
/// An in-memory relay for host tests (craft-live's); feature `test-util`, never in a release build.
#[cfg(feature = "test-util")]
pub use craft_live::testing;

use std::collections::{BTreeMap, VecDeque};

use craft_collab::{BlockChange, LiveDoc, LiveError};
use wordcraft_doc::Document;
use wordcraft_engine::{CmdError, Session};

pub use map::{RECOVERED_STYLE, RECOVERED_STYLE_ID, assign_seed_pids, seed_blocks, seed_ids, write_pids};
pub use session::{
    Access, ENDED_KIND, LiveEditing, LiveFail, LiveStatus, NOTICE_KIND, Peer, SAVED_KIND, SERVICE_UID, SaveEvent, Tap, Word, document_from_state,
};
pub use shareable::{SHAREABLE, is_shareable};

/// How many removals and moves a peer remembers for recovering and forwarding text.
pub const REMEMBERED: usize = 4096;

/// Why live editing refused a command's edit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// It added, removed or changed an inline object (a picture, field, bookmark, note or
    /// comment mark…). Moving one between paragraphs, as Enter and Delete do, is fine.
    InlineObject,
    /// It changed a part of the document outside the body, named here: `styles`, `numbering`,
    /// `last_section`, `settings`… Only the body is shared.
    OutsideBody(&'static str),
}

/// Why a mapping call was refused. Nothing was sent when one is returned.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MapError {
    /// The replica refused the call.
    #[error("shared document: {0}")]
    Live(#[from] LiveError),
    /// No client space yet, so no new paragraph id can be minted. The edit is refused for good
    /// and nothing is held: put `before` back.
    #[error("{command} was made before the relay granted a client space")]
    NoSpace { command: String },
    /// The command changed something live editing does not share. Put `before` back.
    #[error("{command} is not available while editing live: {reason:?}")]
    NotShareable { command: String, reason: Refusal },
}

impl MapError {
    /// The command whose edit was refused, when the caller must put the document back to the
    /// edit's `before`.
    pub fn refused(&self) -> Option<&str> {
        match self {
            MapError::NoSpace { command } | MapError::NotShareable { command, .. } => Some(command),
            MapError::Live(_) => None,
        }
    }
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
    /// Updates for the relay: the text this peer recovered or forwarded.
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

/// A map that forgets its oldest entries past [`REMEMBERED`].
#[derive(Clone, Debug)]
struct Bounded<V> {
    map: BTreeMap<u64, V>,
    age: VecDeque<u64>,
}

impl<V> Default for Bounded<V> {
    fn default() -> Self {
        Bounded { map: BTreeMap::new(), age: VecDeque::new() }
    }
}

impl<V> Bounded<V> {
    fn insert(&mut self, k: u64, v: V) {
        if self.map.insert(k, v).is_none() {
            self.age.push_back(k);
        }
        while self.age.len() > REMEMBERED {
            if let Some(old) = self.age.pop_front() {
                self.map.remove(&old);
            }
        }
    }
    fn get(&self, k: &u64) -> Option<&V> {
        self.map.get(k)
    }
    fn get_mut(&mut self, k: &u64) -> Option<&mut V> {
        self.map.get_mut(k)
    }
    fn contains(&self, k: &u64) -> bool {
        self.map.contains_key(k)
    }
}

/// A block this peer moved: re-added as `to`, and the two texts as last forwarded.
#[derive(Clone, Debug)]
struct Moved {
    to: u64,
    /// The old block's text when it was last forwarded from.
    from_text: String,
    /// The new block's text then.
    to_text: String,
}

/// One peer's side of the mapping: the client space and id counter new paragraphs take ids
/// from, the ids of the body's tables, and the paragraphs it removed and moved (for recovered
/// and forwarded text).
#[derive(Clone, Debug)]
pub struct LiveMap {
    space: u32,
    next: u32,
    /// The ids of the body's tables, in body order.
    tables: Vec<u64>,
    /// Blocks this peer took out of the order, with the block before each then.
    removed: Bounded<Option<u64>>,
    /// Blocks this peer moved.
    moved: Bounded<Moved>,
    /// Tombstone id to the recovered paragraph made for it.
    recovered: Bounded<u64>,
}

impl LiveMap {
    /// The map for `doc`, the session's document right after seeding (its ids written), as
    /// client `space` (0 until the relay grants one; see [`LiveMap::grant`]).
    pub fn new(doc: &Document, space: u32) -> LiveMap {
        let ids = seed_ids(doc);
        let tables = doc.body.iter().zip(ids).filter(|(b, _)| b.as_table().is_some()).map(|(_, id)| id).collect();
        LiveMap { space, next: 0, tables, removed: Bounded::default(), moved: Bounded::default(), recovered: Bounded::default() }
    }

    /// Mint new ids in `space` from now on.
    pub fn set_space(&mut self, space: u32) {
        if space != self.space {
            self.space = space;
            self.next = 0;
        }
    }

    /// The relay granted `space` (`live` already has it as its client). Edits made before
    /// were refused, so there is nothing to send. Returns whether the session's `doc` and the
    /// replica hold the same body, a check made in debug builds only (release says `true`);
    /// a `false` is logged and means the app let an edit through without putting it back.
    pub fn grant(&mut self, live: &LiveDoc, doc: &Document, space: u32) -> bool {
        self.set_space(space);
        if !cfg!(debug_assertions) {
            return true;
        }
        let mut base = doc.clone();
        base.body.clear();
        let mut probe = LiveMap { tables: Vec::new(), ..self.clone() };
        let all: Vec<BlockChange> = live.order().into_iter().enumerate().map(|(at, id)| BlockChange::Inserted { at, id }).collect();
        probe.apply_changes(&mut base, live, &all);
        let same = base.body.len() == doc.body.len() && base.body.iter().zip(&doc.body).all(|(x, y)| x == y);
        if !same {
            log::warn!("live: the session and the shared document differ at the space grant");
        }
        same
    }

    /// The updates that take `live` from `before` to `after`: one reported command, `command`
    /// its id.
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
    /// change applied. An edit that changes what is not shared is refused with
    /// [`MapError::NotShareable`], one made without a space with [`MapError::NoSpace`]; either
    /// way nothing is sent and the caller puts `before` back.
    pub fn local_updates(&mut self, live: &mut LiveDoc, before: &Document, after: &Document, command: &str) -> Result<LocalEdit, MapError> {
        if local::same_body(before, after) && local::outside_body_change(before, after).is_none() {
            return Ok(LocalEdit::default());
        }
        if self.space == 0 {
            return Err(MapError::NoSpace { command: command.to_owned() });
        }
        self.diff(live, before, after, command)
    }
}

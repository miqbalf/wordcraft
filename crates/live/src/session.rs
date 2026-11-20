//! The live session pump: one WordCraft [`Session`] kept in step with the other editors of the
//! same file through craft-collab's [`Live`] over any [`Hub`].
//!
//! # For a host (the browser app, a desktop window)
//!
//! 1. Connect a hub with the ticket (the relay admits the peer from it; never pass
//!    `Session::applied()` as the hub's `applied`: reconnects use the hub's own automatic
//!    reconnect with its original auth), then [`LiveEditing::start`] (or
//!    [`LiveEditing::start_as`] with [`Access::Read`] for a read-only ticket). It opens the
//!    file into the session, gives the paragraphs their seed ids, seeds the shared document and
//!    installs the edit observer and the live undo hook.
//! 2. Every frame, call [`LiveEditing::pump`]. It sends the local edits made since the last
//!    pump, applies the other editors' edits, and returns who else is here with their carets.
//!    Show [`LiveEditing::status`] and [`LiveEditing::take_notices`].
//! 3. Run document-changing commands through [`LiveEditing::run`], or call
//!    [`LiveEditing::flush`] after each `Session::run` of one. Each edit is then shared before
//!    the next command, so new paragraph ids are written back in time. Edits that reach the
//!    pump in a batch (several commands between pumps) are shared as one edit, and an edit made
//!    after an Undo in the same batch is dropped with a notice.
//! 4. Grey out what [`LiveEditing::allows`] refuses, and take no typing while
//!    [`LiveEditing::editable`] is false. An edit that arrives anyway is put back and the user
//!    is told why: the pump is the backstop, not the gate.
//! 5. Send the caret with [`LiveEditing::send_cursor`] whenever it moves; the pump holds it to
//!    at most five a second ([`CURSOR_INTERVAL_MS`]). In a browser, give it the page's clock
//!    with [`LiveEditing::set_clock`]; without a clock it counts frames instead.
//!
//! # The status
//!
//! - [`LiveStatus::Connecting`] from the start until the relay has granted a client space, the
//!   join's catch-up has arrived whole (the ordered replay up to `Joined.replay_until`, or the
//!   last `text_ops` piece of a fallback) and the replica is ready. No edits: the catch-up may
//!   still replace the replica with the room's checkpoint. A catch-up still arriving after 10 s
//!   is told to the user once; after 20 s it ends in [`LiveStatus::Broken`] (reopen).
//! - [`LiveStatus::Live`] while connected, joined and ready: the only state that takes edits.
//!   `saved_at` is the ETag of the newest file the document service saved (the `saved`
//!   marker from the service uid [`SERVICE_UID`], `{seq, etag}`, `seq` never going back). It
//!   is kept across reconnects and peers leaving.
//! - [`LiveStatus::Reconnecting`] when the link drops after it had been live, until the hub's
//!   rejoin is answered and its catch-up has arrived. No edits.
//! - [`LiveStatus::ReadOnly`] for a read-only ticket, from the start and for good (it follows
//!   the other editors and shows its caret, and never sends an edit).
//! - [`LiveStatus::Refused`] when the relay grants no client space, so nothing can be shared.
//! - [`LiveStatus::Broken`] when the replica can no longer be trusted: the relay refused an
//!   edit this replica already holds, an update this replica holds could not be sent, the
//!   catch-up could not be rebuilt (`Work::TextIncomplete`), the relay could no longer catch
//!   this link up (it fell too far behind), a checkpoint did not restore, or yrs failed on an
//!   update. It is final: the message tells the user to reopen the file. A read-only ticket
//!   started for editing ends here too, when the relay refuses its first edit ("read-only
//!   access"): start a read-only ticket with [`Access::Read`].
//!
//! # For the document service (the saver)
//!
//! The worker connects with its service ticket (uid [`SERVICE_UID`]) and pumps like any host.
//! To save: [`LiveEditing::checkpoint`] returns the state it snapshotted; wait for
//! [`SaveEvent::Checkpointed`] from [`LiveEditing::take_save_events`], build the document from
//! those bytes with [`document_from_state`], write the file, then [`LiveEditing::announce_saved`]
//! with the sequence and the file's new ETag.
//!
//! - One checkpoint at a time: drain [`LiveEditing::take_save_events`] (pumping) until its
//!   outcome arrives before calling [`LiveEditing::checkpoint`] again.
//! - [`SaveEvent::Raced`]: throw the state bytes of that checkpoint away (they may miss a
//!   remote edit) and checkpoint again.
//! - [`SaveEvent::Refused`], or `checkpoint()` failing with `craft_collab::Error::Refused`: a
//!   retry will not help (read-only, too large); stop until something changes.
//! - `checkpoint()` failing with any other error (not ready, still catching up, one in flight,
//!   the hub failed): nothing was sent; keep pumping and try again later.
//!
//! Recovered and forwarded text (see the crate documentation) is made outside the replica's
//! undo history, so the user's Undo never takes it back.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use craft_collab::{BlockChange, ClientMsg, Hub, Live, LiveDoc, LiveError, ServerMsg, Work};
use serde_json::{Value, json};
use wordcraft_doc::Document;
use wordcraft_engine::{CmdError, EditObserver, Registry, Session};

use crate::{LiveMap, MapError, Refusal, Remote, assign_seed_pids, is_shareable, seed_blocks, share_pids};

/// The uid of the document service, the only peer whose `saved` marker counts.
pub const SERVICE_UID: &str = "craft_office";
/// The ephemeral kind carrying a caret: `{pid, at, end}`, `end` `[pid, at]` or null.
pub const CURSOR_KIND: &str = "cursor";
/// The ephemeral kind the document service announces a save with: `{seq, etag}`.
pub const SAVED_KIND: &str = "saved";
/// The least time between two carets sent: at most five a second.
pub const CURSOR_INTERVAL_MS: f64 = 200.0;
/// Without a clock, the frames between two carets sent (five a second at 60 frames).
const CURSOR_FRAMES: u32 = 12;
/// The largest `saved` payload the relay keeps.
const SAVED_MAX_BYTES: usize = 1024;

const BROKEN: &str = "This document fell out of step with the other editors. Reopen the file to keep editing.";
const READ_ONLY_BROKEN: &str = "You can only read this file. Reopen it to keep following the other editors.";
const NO_SPACE: &str = "The live editing server cannot share edits right now. Reopen the file later.";
const CONNECTING: &str = "Still connecting to the other editors. You can edit in a moment.";
const RECONNECTING: &str = "Reconnecting to the other editors. Editing is paused until then.";
const READ_ONLY: &str = "This file is read-only for you.";
const PARTIAL: &str = "Part of your last edit could not be shared and was taken back.";
const BEHIND: &str = "This window fell too far behind the other editors. Reopen the file to catch up.";
const UNSENT: &str = "Your last edit could not be sent to the other editors.";
const CATCHING_UP: &str = "Still catching up with the other editors. Editing starts as soon as that is done.";
const CATCH_UP_FAILED: &str = "Catching up with the other editors is taking too long. Reopen the file to try again.";
/// How long a join's catch-up may take before the user is told; twice that and it gives up.
const CATCH_UP_LATE_MS: f64 = 10_000.0;
/// The same without a clock, in frames.
const CATCH_UP_LATE_FRAMES: u32 = 600;
const AFTER_UNDO: &str = "An edit made right after Undo was not kept. Please make it again.";

/// What live editing is doing now; see the [module documentation](self).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LiveStatus {
    Connecting,
    Live { saved_at: Option<String> },
    ReadOnly,
    Reconnecting,
    Refused(String),
    Broken(String),
}

/// What the ticket allows. The relay enforces it; this side never offers an edit it refuses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Edit,
    Read,
}

/// Another editor in the document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    /// The relay's peer id for one connection (`<uid>~<hex>`).
    pub id: String,
    pub display: String,
    /// The same for every connection of the same person, for a stable badge colour.
    pub color_seed: u64,
    /// `(paragraph id, char offset)`, kept on the same characters through edits.
    pub caret: Option<(u64, usize)>,
}

/// A checkpoint's outcome, for the saver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SaveEvent {
    /// Ordered at `seq` and acknowledged: write the file from the checkpoint's state now.
    Checkpointed { seq: u64 },
    /// Dropped for a reason that can pass: checkpoint again.
    Raced,
    /// Refused for a reason a retry does not change, in the relay's words.
    Refused { reason: String },
}

/// Why live editing could not start.
#[derive(Debug, thiserror::Error)]
pub enum LiveFail {
    #[error("the file did not open: {0}")]
    Open(String),
    #[error("the paragraphs could not be given shared ids: {0}")]
    Ids(CmdError),
    #[error("the shared document could not be seeded: {0}")]
    Seed(LiveError),
}

/// What the hub's join messages said, read as they pass.
#[derive(Default)]
struct Seen {
    joins: u64,
    space: Option<u32>,
    /// The join's ordered replay ends at this sequence, not yet seen.
    replay_until: Option<u64>,
    /// The join's catch-up comes as text ops, whose last piece has not arrived.
    text_ops_owed: bool,
    /// The relay could not catch this link up (`snapshot`: fell too far behind).
    replaced: bool,
}

impl Seen {
    /// Whether the last join's catch-up is still arriving. Until it has, the replica may
    /// still be replaced by the room's snapshot, and an edit made meanwhile would break it.
    fn catching_up(&self) -> bool {
        self.replay_until.is_some() || self.text_ops_owed
    }

    fn saw(&mut self, m: &ServerMsg) {
        match m {
            ServerMsg::Joined { seq, space, replay_until, fallback, .. } => {
                self.joins += 1;
                self.space = Some(*space);
                // An older relay does not mark its replay; there nothing can be waited for.
                self.replay_until = replay_until.filter(|until| until > seq);
                self.text_ops_owed = *fallback;
            }
            ServerMsg::Op { seq, .. } if self.replay_until.is_some_and(|until| *seq >= until) => self.replay_until = None,
            ServerMsg::TextOps { more: false, .. } => self.text_ops_owed = false,
            ServerMsg::Snapshot { .. } => self.replaced = true,
            _ => {}
        }
    }
}

/// The host's hub, watched for the relay's join answer (the granted space, where the catch-up
/// ends) and for a relay that gave up catching it up, which [`Live`]
/// keeps to itself.
pub struct Tap<H> {
    hub: H,
    seen: Arc<Mutex<Seen>>,
}

impl<H: Hub> Hub for Tap<H> {
    fn send(&mut self, msg: ClientMsg) -> craft_collab::Result<()> {
        self.hub.send(msg)
    }
    fn poll(&mut self) -> Vec<ServerMsg> {
        let msgs = self.hub.poll();
        let mut seen = lock(&self.seen);
        for m in &msgs {
            seen.saw(m);
        }
        msgs
    }
    fn connected(&self) -> bool {
        self.hub.connected()
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A local change waiting for the pump.
enum Local {
    /// `(before, after, command)`, boxed: two documents are large.
    Edit(Box<(Document, Document, String)>),
    History {
        steps: usize,
        redo: bool,
    },
}

/// Shared with the observer and the undo hook, which run inside `Session::run`.
#[derive(Default)]
struct Shared {
    queue: Vec<Local>,
    /// Replica undo steps per shared edit, newest last.
    undo: Vec<usize>,
    redo: Vec<usize>,
}

struct Observer(Arc<Mutex<Shared>>);

impl EditObserver for Observer {
    fn edited(&mut self, before: &Document, after: &Document, command: &str) {
        lock(&self.0).queue.push(Local::Edit(Box::new((before.clone(), after.clone(), command.to_owned()))));
    }
}

/// The live undo hook: takes the step off its stack now (so the answer is honest) and leaves
/// the replica's undo to the next flush.
fn history_hook(shared: Arc<Mutex<Shared>>) -> Box<dyn FnMut(bool) -> bool + Send> {
    Box::new(move |redo| {
        let mut s = lock(&shared);
        let step = if redo { s.redo.pop() } else { s.undo.pop() };
        match step {
            Some(steps) => {
                s.queue.push(Local::History { steps, redo });
                true
            }
            None => false,
        }
    })
}

/// A peer's caret and the text of its paragraph when last followed.
struct Caret {
    pid: u64,
    at: usize,
    text: String,
}

/// A join's catch-up that has not finished arriving.
struct CatchUp {
    since: Option<f64>,
    frames: u32,
    told: bool,
}

#[derive(Default)]
struct CursorOut {
    pending: Option<Value>,
    last: Option<Value>,
    sent_at: Option<f64>,
    frames: u32,
}

type Clock = Box<dyn FnMut() -> f64 + Send>;

#[cfg(not(target_arch = "wasm32"))]
fn default_clock() -> Option<Clock> {
    let start = std::time::Instant::now();
    Some(Box::new(move || start.elapsed().as_secs_f64() * 1000.0))
}

/// `std::time` has no clock on the web; the host gives one with [`LiveEditing::set_clock`].
#[cfg(target_arch = "wasm32")]
fn default_clock() -> Option<Clock> {
    None
}

/// One document edited live: the session's side of a [`Live`] link.
pub struct LiveEditing<H: Hub> {
    live: Live<Tap<H>>,
    seen: Arc<Mutex<Seen>>,
    map: LiveMap,
    shared: Arc<Mutex<Shared>>,
    registry: Arc<Registry>,
    access: Access,
    status: LiveStatus,
    space: u32,
    joins: u64,
    need_join: bool,
    been_live: bool,
    broken: Option<String>,
    refused: Option<String>,
    saved: Option<(u64, String)>,
    service_uid: String,
    carets: HashMap<String, Caret>,
    notices: Vec<String>,
    save_events: Vec<SaveEvent>,
    cursor: CursorOut,
    clock: Option<Clock>,
    catch_up: Option<CatchUp>,
}

impl<H: Hub> LiveEditing<H> {
    /// Start editing `file_bytes` (named `file_name`, for its format) live in room `room`,
    /// with an edit ticket. See the [module documentation](self).
    pub fn start(hub: H, room: &str, file_bytes: &[u8], file_name: &str, session: &mut Session) -> Result<Self, LiveFail> {
        Self::start_as(hub, room, file_bytes, file_name, session, Access::Edit)
    }

    /// [`LiveEditing::start`] with the ticket's access.
    pub fn start_as(hub: H, room: &str, file_bytes: &[u8], file_name: &str, session: &mut Session, access: Access) -> Result<Self, LiveFail> {
        let doc = wordcraft_engine::io::open_bytes(file_name, file_bytes).map_err(LiveFail::Open)?;
        // Hooks left from an earlier link would keep their binding to the old document.
        session.set_observer(None);
        session.live_undo_hook(None);
        session.set_document(doc);
        session.assign_pids(assign_seed_pids).map_err(LiveFail::Ids)?;
        let seen = Arc::new(Mutex::new(Seen::default()));
        let mut live = Live::new(Tap { hub, seen: seen.clone() }, room);
        // Nothing has been polled yet, so nothing is held and nothing changes.
        live.seed(&seed_blocks(&session.doc)).map_err(LiveFail::Seed)?;
        let map = LiveMap::new(&session.doc, 0);
        let shared = Arc::new(Mutex::new(Shared::default()));
        session.set_observer(Some(Box::new(Observer(shared.clone()))));
        session.live_undo_hook(Some(history_hook(shared.clone())));
        Ok(LiveEditing {
            live,
            seen,
            map,
            shared,
            registry: session.registry.clone(),
            access,
            status: if access == Access::Read { LiveStatus::ReadOnly } else { LiveStatus::Connecting },
            space: 0,
            joins: 0,
            need_join: true,
            been_live: false,
            broken: None,
            refused: None,
            saved: None,
            service_uid: SERVICE_UID.to_owned(),
            carets: HashMap::new(),
            notices: Vec::new(),
            save_events: Vec::new(),
            cursor: CursorOut::default(),
            clock: default_clock(),
            catch_up: None,
        })
    }

    /// One frame: share the local edits, take in the other editors' edits, carets and saves,
    /// and say who else is here.
    pub fn pump(&mut self, session: &mut Session) -> Vec<Peer> {
        self.flush(session);
        let works = self.live.pump();
        self.note_join();
        for work in works {
            self.act(work, session);
        }
        self.take_live_notices();
        self.update_status();
        self.cursor.frames = self.cursor.frames.saturating_add(1);
        self.send_due_cursor();
        self.roster()
    }

    /// Run a command on the session, refused when live editing does not allow it now, and
    /// share its edit right away.
    pub fn run(&mut self, session: &mut Session, id: &str, params: &Value) -> Result<Value, CmdError> {
        if !self.allows(id) {
            let why = self.refusal_for(id);
            self.notices.push(why.clone());
            return Err(CmdError::Disabled(why));
        }
        let result = session.run(id, params);
        self.flush(session);
        result
    }

    /// Share the edits the session made since the last flush, and run the Undo and Redo it
    /// asked for. Refused edits are put back, with a notice.
    pub fn flush(&mut self, session: &mut Session) {
        let queue = std::mem::take(&mut lock(&self.shared).queue);
        let mut group = Vec::new();
        let mut stale = false;
        let mut dropped = false;
        for item in queue {
            match item {
                Local::Edit(edit) if !stale => group.push(*edit),
                Local::Edit(_) => dropped = true,
                Local::History { steps, redo } => {
                    self.share(session, std::mem::take(&mut group));
                    self.history(session, steps, redo);
                    stale = true;
                }
            }
        }
        self.share(session, group);
        if dropped {
            // The session holds those edits and the room does not: take them back.
            self.resync(session);
            self.notices.push(AFTER_UNDO.to_owned());
        }
    }

    /// Tell the room where this editor's caret is: paragraph `pid`, char `char_at`, and the
    /// other end of the selection. Held to at most five a second; the newest goes out when due.
    pub fn send_cursor(&mut self, pid: u64, char_at: usize, sel_end: Option<(u64, usize)>) {
        let payload = json!({"pid": pid, "at": char_at, "end": sel_end.map(|(p, a)| json!([p, a]))});
        if self.cursor.last.as_ref() == Some(&payload) {
            self.cursor.pending = None;
            return;
        }
        self.cursor.pending = Some(payload);
        self.send_due_cursor();
    }

    pub fn status(&self) -> &LiveStatus {
        &self.status
    }

    /// Whether the host may take edits now: live, connected, with an edit ticket.
    pub fn editable(&self) -> bool {
        matches!(self.status, LiveStatus::Live { .. }) && self.access == Access::Edit && self.live.connected() && self.live.ready_to_type()
    }

    /// Whether command `command_id` may run now: a command that changes nothing always may;
    /// one that edits only while [`editable`](Self::editable) and only when its edit is
    /// shared ([`is_shareable`]).
    pub fn allows(&self, command_id: &str) -> bool {
        match self.registry.get(command_id) {
            Some(spec) if !spec.mutates => true,
            _ => self.editable() && is_shareable(command_id),
        }
    }

    /// Lines for the user since the last call, in plain English (untranslated).
    pub fn take_notices(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notices)
    }

    /// Checkpoint outcomes since the last call, for the saver.
    pub fn take_save_events(&mut self) -> Vec<SaveEvent> {
        std::mem::take(&mut self.save_events)
    }

    /// Checkpoint the shared document (the saver only): returns the state snapshotted. See
    /// the [module documentation](self) and `Live::checkpoint`.
    pub fn checkpoint(&mut self) -> craft_collab::Result<Vec<u8>> {
        self.live.checkpoint()
    }

    /// Announce a saved file (the saver only): `seq` the checkpoint's sequence, `etag` the
    /// file's new ETag. Every editor's status shows it.
    pub fn announce_saved(&mut self, seq: u64, etag: &str) -> craft_collab::Result<()> {
        self.live.send_ephemeral(SAVED_KIND, json!({"seq": seq, "etag": etag}))?;
        let me = self.service_uid.clone();
        self.take_saved(&me, &json!({"seq": seq, "etag": etag}));
        Ok(())
    }

    /// The uid whose `saved` markers count, [`SERVICE_UID`] unless the relay names another.
    pub fn set_service_uid(&mut self, uid: impl Into<String>) {
        self.service_uid = uid.into();
    }

    /// The clock the caret limit runs on, in milliseconds. Native builds have one; a browser
    /// host passes the page's.
    pub fn set_clock(&mut self, clock: Box<dyn FnMut() -> f64 + Send>) {
        self.clock = Some(clock);
    }

    /// The link, for reading its state.
    pub fn live(&self) -> &Live<Tap<H>> {
        &self.live
    }

    /// The host's hub.
    pub fn hub(&self) -> &H {
        &self.live.hub().hub
    }

    /// The host's hub, to steer its transport (a fresh ticket, for one).
    pub fn hub_mut(&mut self) -> &mut H {
        &mut self.live.hub_mut().hub
    }

    /// Stop editing live: the session keeps the document and takes back its own undo.
    pub fn stop(self, session: &mut Session) {
        session.set_observer(None);
        session.live_undo_hook(None);
    }

    fn note_join(&mut self) {
        let (joins, space) = {
            let s = lock(&self.seen);
            (s.joins, s.space)
        };
        if joins == self.joins {
            return;
        }
        self.joins = joins;
        self.need_join = false;
        match space {
            Some(space) if space > 0 => {
                self.refused = None;
                if space != self.space {
                    self.space = space;
                    self.map.set_space(space);
                    // The replica took a new client id, which starts its undo history again.
                    let mut s = lock(&self.shared);
                    s.undo.clear();
                    s.redo.clear();
                }
            }
            _ => self.refused = Some(NO_SPACE.to_owned()),
        }
    }

    fn act(&mut self, work: Work, session: &mut Session) {
        match work {
            Work::Doc { changes } => self.take_remote(session, changes),
            Work::Reset => self.reset(session),
            Work::TextIncomplete { .. } => self.break_with(BROKEN),
            Work::Ephemeral { origin, kind, payload } => self.ephemeral(&origin, &kind, &payload, session),
            Work::Checkpointed { seq } => self.save_events.push(SaveEvent::Checkpointed { seq }),
            Work::CheckpointRaced => self.save_events.push(SaveEvent::Raced),
            Work::CheckpointRefused { reason } => self.save_events.push(SaveEvent::Refused { reason }),
            Work::SeedWanted { .. } => log::debug!("live: the shared document was seeded at the start"),
            Work::Command { command, .. } => log::warn!("live: ignoring the locked command {command} from another editor"),
            Work::Frame { .. } => {}
            // The relay refused the ticket or the protocol and closed the link, and the hub has
            // stopped reconnecting until the host gives it a new ticket (`Hub::set_token`
            // through `live().hub_mut()`). Refused until a later join is granted a space.
            Work::ConnectionRefused { code, reason } => {
                log::warn!("live: the relay refused the connection ({code})");
                self.refused = Some(reason);
            }
        }
    }

    /// Remote edits `Live` already merged: recover orphaned typing, then the session.
    fn take_remote(&mut self, session: &mut Session, changes: Vec<BlockChange>) {
        let fallback = changes.clone();
        let map = &mut self.map;
        let remote = match edit_live(&mut self.live, |d| {
            // Recovered and forwarded text is the room's, not an edit of this user's: their
            // Undo must never take it back.
            let r = d.untracked(|d| map.received(d, changes));
            let updates = r.updates.clone();
            (r, updates)
        }) {
            Some((remote, sent)) => {
                if !sent {
                    self.unsent();
                }
                remote
            }
            None => Remote { changes: fallback, updates: Vec::new() },
        };
        self.apply_to_session(session, &remote.changes);
    }

    fn apply_to_session(&mut self, session: &mut Session, changes: &[BlockChange]) {
        let Some(doc) = self.live.doc() else { return };
        let map = &mut self.map;
        let landed = session.apply_remote(|d| map.apply_changes(d, doc, changes));
        if let Err(e) = landed {
            log::warn!("live: a remote edit did not land: {e}");
            self.break_with(BROKEN);
        }
        self.follow_carets(session);
    }

    /// The whole body again from the replica.
    fn resync(&mut self, session: &mut Session) {
        let Some(doc) = self.live.doc() else { return };
        let all: Vec<BlockChange> = doc.order().into_iter().enumerate().map(|(at, id)| BlockChange::Inserted { at, id }).collect();
        self.apply_to_session(session, &all);
    }

    /// The replica was replaced by the room's checkpoint.
    fn reset(&mut self, session: &mut Session) {
        let mut base = session.doc.clone();
        base.body.clear();
        self.map = LiveMap::new(&base, self.space);
        let mut s = lock(&self.shared);
        s.undo.clear();
        s.redo.clear();
        drop(s);
        self.resync(session);
    }

    /// Share one batch of reported edits as one.
    fn share(&mut self, session: &mut Session, group: Vec<(Document, Document, String)>) {
        let command = group.iter().map(|(_, _, c)| c).find(|c| !is_shareable(c)).or_else(|| group.last().map(|(_, _, c)| c)).cloned();
        let (Some(command), Some(after)) = (command, group.last().map(|(_, a, _)| a.clone())) else { return };
        let Some(before) = group.into_iter().next().map(|(b, _, _)| b) else { return };
        if !self.editable() {
            self.put_back(session, before);
            let why = self.not_editable();
            self.notices.push(why);
            return;
        }
        let map = &mut self.map;
        let result = edit_live(&mut self.live, |d| match map.local_updates(d, &before, &after, &command) {
            Ok(edit) => {
                let updates = edit.updates.clone();
                (Ok(edit), updates)
            }
            Err(e) => (Err(e), Vec::new()),
        });
        match result {
            Some((Ok(edit), sent)) => {
                if !sent {
                    self.unsent();
                }
                if !edit.updates.is_empty() {
                    let mut s = lock(&self.shared);
                    s.undo.push(edit.updates.len());
                    s.redo.clear();
                }
                if let Err(e) = share_pids(session, &edit) {
                    log::warn!("live: new paragraph ids were not written back: {e}");
                    self.resync(session);
                } else if !edit.failed.is_empty() {
                    self.resync(session);
                    self.notices.push(PARTIAL.to_owned());
                }
                self.follow_carets(session);
            }
            Some((Err(e), _)) if e.refused().is_some() => {
                self.put_back(session, before);
                self.notices.push(refusal_text(&e).to_owned());
            }
            Some((Err(e), _)) => {
                log::warn!("live: the shared document refused an edit: {e}");
                self.resync(session);
                self.notices.push(PARTIAL.to_owned());
            }
            None => {
                self.put_back(session, before);
                self.break_with(BROKEN);
            }
        }
    }

    /// An update this replica holds never reached the relay, so the room will not have it:
    /// carrying on would let this window drift from the others for good.
    fn unsent(&mut self) {
        self.notices.push(UNSENT.to_owned());
        self.break_with(BROKEN);
    }

    fn put_back(&mut self, session: &mut Session, before: Document) {
        if let Err(e) = session.apply_remote(|d| *d = before) {
            log::warn!("live: a refused edit could not be put back: {e}");
            self.break_with(BROKEN);
        }
    }

    /// Undo or redo `steps` replica steps (one shared edit), then the session from the replica.
    fn history(&mut self, session: &mut Session, steps: usize, redo: bool) {
        if !self.editable() {
            let mut s = lock(&self.shared);
            if redo {
                s.redo.push(steps)
            } else {
                s.undo.push(steps)
            }
            drop(s);
            let why = self.not_editable();
            self.notices.push(why);
            return;
        }
        let mut done = 0;
        for _ in 0..steps {
            let step = if redo { self.live.redo() } else { self.live.undo() };
            match step {
                Ok(true) => done += 1,
                Ok(false) => break,
                Err(e @ (craft_collab::Error::Hub(_) | craft_collab::Error::Refused(_))) => {
                    // The step landed in the replica, but the room will never see it.
                    log::warn!("live: an undo step was not sent: {e}");
                    done += 1;
                    self.unsent();
                    break;
                }
                Err(e) => {
                    log::warn!("live: undo did not go through: {e}");
                    break;
                }
            }
        }
        if done > 0 {
            let mut s = lock(&self.shared);
            if redo {
                s.undo.push(done)
            } else {
                s.redo.push(done)
            }
            drop(s);
            self.resync(session);
        }
    }

    fn ephemeral(&mut self, origin: &str, kind: &str, payload: &Value, session: &Session) {
        match kind {
            SAVED_KIND => self.take_saved(origin, payload),
            CURSOR_KIND => {
                let caret = payload.get("pid").and_then(Value::as_u64).zip(payload.get("at").and_then(Value::as_u64));
                let text = caret.and_then(|(pid, _)| para_text(&session.doc, pid));
                match (caret, text) {
                    (Some((pid, at)), Some(text)) => {
                        let at = usize::try_from(at).unwrap_or(usize::MAX).min(text.chars().count());
                        self.carets.insert(origin.to_owned(), Caret { pid, at, text });
                    }
                    _ => {
                        self.carets.remove(origin);
                    }
                }
            }
            _ => {}
        }
    }

    /// A `saved` marker counts only from the service uid, shaped `{seq, etag}` within the
    /// relay's size, and with `seq` not going back.
    fn take_saved(&mut self, origin: &str, payload: &Value) {
        if uid_of(origin) != self.service_uid {
            log::debug!("live: ignoring a saved marker from a peer that is not the service");
            return;
        }
        let seq = payload.get("seq").and_then(Value::as_u64);
        let etag = payload.get("etag").and_then(Value::as_str);
        let small = serde_json::to_vec(payload).is_ok_and(|v| v.len() <= SAVED_MAX_BYTES);
        if let (Some(seq), Some(etag), true) = (seq, etag, small)
            && self.saved.as_ref().is_none_or(|(had, _)| seq >= *had)
        {
            self.saved = Some((seq, etag.to_owned()));
        }
    }

    /// Keep each peer's caret on the same characters: followed through its paragraph's edit,
    /// clamped to it, and dropped with it.
    fn follow_carets(&mut self, session: &Session) {
        if self.carets.is_empty() {
            return;
        }
        let paras: HashMap<u64, &str> =
            session.doc.body.iter().filter_map(|b| b.as_para()).filter(|p| p.pid != 0).map(|p| (p.pid, p.text.as_str())).collect();
        self.carets.retain(|_, c| match paras.get(&c.pid) {
            Some(text) => {
                if *text != c.text {
                    c.at = follow(&c.text, text, c.at);
                    (*text).clone_into(&mut c.text);
                }
                c.at = c.at.min(text.chars().count());
                true
            }
            None => false,
        });
    }

    fn take_live_notices(&mut self) {
        for n in std::mem::take(&mut self.live.notices) {
            if n.starts_with("read-only access") {
                if self.access == Access::Edit {
                    self.broken = Some(READ_ONLY_BROKEN.to_owned());
                }
                continue;
            }
            self.notices.push(sentence(&n));
        }
    }

    fn update_status(&mut self) {
        let connected = self.live.connected();
        if !connected {
            self.need_join = true;
        }
        let (catching_up, replaced) = {
            let s = lock(&self.seen);
            (s.catching_up(), s.replaced)
        };
        if replaced {
            self.break_with(BEHIND);
        }
        let counting = catching_up && !self.need_join && self.broken.is_none() && self.refused.is_none();
        self.wait_for_catch_up(counting);
        if self.broken.is_none() && !self.need_join && self.space != 0 && !self.live.ready_to_type() {
            // Joined with a space, yet the replica takes no edits: Live gave up on it.
            self.broken = Some(BROKEN.to_owned());
        }
        self.status = if let Some(m) = &self.broken {
            LiveStatus::Broken(m.clone())
        } else if let Some(m) = &self.refused {
            LiveStatus::Refused(m.clone())
        } else if self.access == Access::Read {
            LiveStatus::ReadOnly
        } else if connected && !self.need_join && self.space != 0 && !catching_up {
            self.been_live = true;
            LiveStatus::Live { saved_at: self.saved.as_ref().map(|(_, e)| e.clone()) }
        } else if self.been_live {
            LiveStatus::Reconnecting
        } else {
            LiveStatus::Connecting
        };
    }

    /// Count how long the join's catch-up has been arriving: say so once when it is late, and
    /// give up (reopen) when it is twice as late.
    fn wait_for_catch_up(&mut self, waiting: bool) {
        if !waiting {
            self.catch_up = None;
            return;
        }
        let now = self.clock.as_mut().map(|c| c());
        let since = self.catch_up.get_or_insert(CatchUp { since: now, frames: 0, told: false });
        since.frames = since.frames.saturating_add(1);
        // How many late periods have passed.
        let periods = match (now, since.since) {
            (Some(now), Some(t)) => (now - t) / CATCH_UP_LATE_MS,
            _ => f64::from(since.frames) / f64::from(CATCH_UP_LATE_FRAMES),
        };
        if periods >= 2.0 {
            self.catch_up = None;
            self.break_with(CATCH_UP_FAILED);
        } else if periods >= 1.0 && !std::mem::replace(&mut since.told, true) {
            self.notices.push(CATCHING_UP.to_owned());
        }
    }

    fn break_with(&mut self, msg: &str) {
        if self.broken.is_none() {
            self.broken = Some(msg.to_owned());
        }
    }

    fn not_editable(&self) -> String {
        match &self.status {
            LiveStatus::Connecting => CONNECTING.to_owned(),
            LiveStatus::Live { .. } | LiveStatus::Reconnecting => RECONNECTING.to_owned(),
            LiveStatus::ReadOnly => READ_ONLY.to_owned(),
            LiveStatus::Refused(m) | LiveStatus::Broken(m) => m.clone(),
        }
    }

    fn refusal_for(&self, id: &str) -> String {
        if self.editable() { command_text(id).to_owned() } else { self.not_editable() }
    }

    fn send_due_cursor(&mut self) {
        let Some(payload) = self.cursor.pending.take() else { return };
        let now = self.clock.as_mut().map(|c| c());
        let due = match now {
            Some(now) => self.cursor.sent_at.is_none_or(|t| now - t >= CURSOR_INTERVAL_MS),
            None => self.cursor.last.is_none() || self.cursor.frames >= CURSOR_FRAMES,
        };
        if !due || !self.live.connected() {
            self.cursor.pending = Some(payload);
            return;
        }
        if let Err(e) = self.live.send_ephemeral(CURSOR_KIND, payload.clone()) {
            log::debug!("live: the caret did not go out: {e}");
            self.cursor.pending = Some(payload);
            return;
        }
        self.cursor.sent_at = now;
        self.cursor.frames = 0;
        self.cursor.last = Some(payload);
    }

    /// The other editors, without the document service, with their carets.
    fn roster(&mut self) -> Vec<Peer> {
        let roster = &self.live.roster;
        self.carets.retain(|id, _| roster.iter().any(|(p, _)| p == id));
        roster
            .iter()
            .filter(|(id, _)| uid_of(id) != self.service_uid)
            .map(|(id, display)| Peer {
                id: id.clone(),
                display: display.clone(),
                color_seed: fnv(uid_of(id)),
                caret: self.carets.get(id).map(|c| (c.pid, c.at)),
            })
            .collect()
    }
}

/// Make an edit on the replica with `f`, which returns its result and the updates it made,
/// and send those. `Live` reaches its replica only through `Live::edit`, one update per call,
/// so the first goes out from inside and the rest after. `None` when the replica takes no
/// edit (broken, damaged); otherwise whether every update went out.
fn edit_live<H: Hub, R>(live: &mut Live<Tap<H>>, f: impl FnOnce(&mut LiveDoc) -> (R, Vec<Vec<u8>>)) -> Option<(R, bool)> {
    let mut out = None;
    let mut rest = Vec::new();
    let mut nothing = false;
    let first = live.edit(|d| {
        let (r, updates) = f(d);
        out = Some(r);
        let mut updates = updates.into_iter();
        let first = updates.next();
        rest = updates.collect();
        nothing = first.is_none();
        // Nothing to send: an error sends nothing and leaves the link as it was.
        first.ok_or(LiveError::OutOfRange)
    });
    let r = out?;
    let mut sent = nothing || first.is_ok();
    if let (Err(e), false) = (&first, nothing) {
        log::warn!("live: an update did not go out: {e}");
    }
    for update in rest {
        if let Err(e) = live.edit(move |_| Ok(update)) {
            log::warn!("live: an update did not go out: {e}");
            sent = false;
        }
    }
    Some((r, sent))
}

/// The plain-English reason a refused edit gives the user.
fn refusal_text(e: &MapError) -> &'static str {
    match e {
        MapError::NoSpace { .. } => CONNECTING,
        MapError::NotShareable { command, reason: Refusal::InlineObject } => {
            if is_image(command) {
                "Images can't be added while others are editing."
            } else {
                "This edit touches a link or bookmark and can't be shared."
            }
        }
        MapError::NotShareable { reason: Refusal::OutsideBody(part), .. } => match *part {
            "last_section" => "Page setup is locked during live editing.",
            "styles" => "Styles can't be changed while others are editing.",
            "numbering" => "Bullets and numbering are locked during live editing.",
            "comments" | "revisions" => "Comments and tracked changes are off during live editing.",
            "media" => "Images can't be added while others are editing.",
            "settings" => "Document settings are locked during live editing.",
            "parts" => "Headers and footers are locked during live editing.",
            _ => "This change can't be shared while others are editing.",
        },
        MapError::Live(_) => PARTIAL,
    }
}

/// The plain-English reason a command is not offered while live.
fn command_text(id: &str) -> &'static str {
    let has = |words: &[&str]| words.iter().any(|w| id.contains(w));
    if is_image(id) {
        "Images can't be added while others are editing."
    } else if id.starts_with("layout.") || has(&["section", "margin", "orientation", "pageSetup"]) {
        "Page setup is locked during live editing."
    } else if has(&["bookmark", "link", "field", "crossRef"]) {
        "This edit touches a link or bookmark and can't be shared."
    } else if id.starts_with("table.") {
        "Tables can't be changed while others are editing."
    } else if id.starts_with("list.") || has(&["bullet", "numbering"]) {
        "Bullets and numbering are locked during live editing."
    } else if id.starts_with("review.") || has(&["comment"]) {
        "Comments and tracked changes are off during live editing."
    } else if id == "edit.paste" || id == "edit.cut" || id.starts_with("edit.paste") {
        "Paste and cut are off during live editing."
    } else {
        "This command isn't available while others are editing."
    }
}

fn is_image(id: &str) -> bool {
    ["picture", "image", "shape", "chart", "media"].iter().any(|w| id.contains(w))
}

/// A notice from the link as a sentence.
fn sentence(n: &str) -> String {
    let mut chars = n.chars();
    let mut s: String = chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default();
    if !s.ends_with(['.', '!', '?']) {
        s.push('.');
    }
    s
}

/// The uid part of a relay peer id (`<uid>~<hex>`).
fn uid_of(peer: &str) -> &str {
    peer.split('~').next().unwrap_or(peer)
}

/// FNV-1a, for a colour seed that is the same on every build and platform.
fn fnv(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

/// The text of body paragraph `pid`.
fn para_text(doc: &Document, pid: u64) -> Option<String> {
    doc.body.iter().filter_map(|b| b.as_para()).find(|p| p.pid == pid).map(|p| p.text.clone())
}

/// Where char `at` of `old` is in `new`, taking the edit as what lies outside their common
/// prefix and suffix: before it stays, after it shifts, inside it goes to its start.
fn follow(old: &str, new: &str, at: usize) -> usize {
    let prefix = old.chars().zip(new.chars()).take_while(|(a, b)| a == b).count();
    let (old_len, new_len) = (old.chars().count(), new.chars().count());
    let room = old_len.min(new_len).saturating_sub(prefix);
    let suffix = old.chars().rev().zip(new.chars().rev()).take(room).take_while(|(a, b)| a == b).count();
    if at <= prefix {
        at
    } else if at >= old_len.saturating_sub(suffix) {
        (at + new_len).saturating_sub(old_len)
    } else {
        prefix
    }
}

/// The document a checkpoint's state holds, with everything that is not shared (styles,
/// settings…) taken from `base` (the session's document). For the saver, which writes the file
/// from this once the checkpoint is acknowledged.
pub fn document_from_state(base: &Document, state: &[u8]) -> Result<Document, LiveError> {
    let live = LiveDoc::from_state(state)?;
    let mut doc = base.clone();
    doc.body.clear();
    let mut map = LiveMap::new(&doc, 0);
    let all: Vec<BlockChange> = live.order().into_iter().enumerate().map(|(at, id)| BlockChange::Inserted { at, id }).collect();
    map.apply_changes(&mut doc, &live, &all);
    doc.ensure_nonempty();
    Ok(doc)
}

//! The live session pump: one WordCraft [`Session`] kept in step with the other editors of the
//! same file through craft-collab's [`Live`] over any [`Hub`].
//!
//! The session state machine (the status, the catch-up, history and saver waits, the document
//! service's `saved`, `notice` and `ended` messages, the caret limit and the roster, the saver
//! side) is craft-live's, shared by every Craft app; see [`craft_live`] for the status rules.
//! What is WordCraft's is here: opening the file into the engine session, the paragraph mapping
//! ([`LiveMap`]) behind craft-live's [`LiveApp`], the engine's observer and live undo hook, the
//! commands live editing allows, and the reasons a refused edit gives the user.
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
//! # For the document service (the saver)
//!
//! The worker connects with its service ticket (uid [`SERVICE_UID`]) and pumps like any host.
//! To save: [`LiveEditing::checkpoint`] returns the state it snapshotted; wait for
//! [`SaveEvent::Checkpointed`] from [`LiveEditing::take_save_events`], build the document from
//! those bytes with [`document_from_state`], write the file, then [`LiveEditing::announce_saved`]
//! with the sequence and the file's new ETag. The rules are craft-live's.
//!
//! Recovered and forwarded text (see the crate documentation) is made outside the replica's
//! undo history, so the user's Undo never takes it back.

use std::collections::HashMap;
use std::sync::Arc;

use craft_collab::{BlockChange, Hub, Live, LiveDoc, LiveError};
pub use craft_live::{Access, CURSOR_INTERVAL_MS, CURSOR_KIND, ENDED_KIND, LiveStatus, NOTICE_KIND, SAVED_KIND, SERVICE_UID, SaveEvent, Tap};
use craft_live::{CONNECTING, Landed, LiveApp, LiveKind, LocalError, PARTIAL};
use serde_json::{Value, json};
use wordcraft_doc::Document;
use wordcraft_engine::{CmdError, EditObserver, Registry, Session};

use crate::{LiveMap, LocalEdit, MapError, Refusal, assign_seed_pids, is_shareable, seed_blocks, share_pids};

/// Another editor in the document, its caret `(paragraph id, char offset)`.
pub type Peer = craft_live::Peer<(u64, usize)>;

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

/// WordCraft's live session types, for craft-live.
pub struct Word;

impl LiveKind for Word {
    type Replica = LiveDoc;
    /// `(before, after, command)`, boxed: two documents are large.
    type Edit = Box<(Document, Document, String)>;
    type Caret = Caret;
    type Pos = (u64, usize);
    fn pos(caret: &Caret) -> (u64, usize) {
        (caret.pid, caret.at)
    }
}

/// A peer's caret and the text of its paragraph when last followed.
pub struct Caret {
    pid: u64,
    at: usize,
    text: String,
}

struct Observer(craft_live::EditQueue<Box<(Document, Document, String)>>);

impl EditObserver for Observer {
    fn edited(&mut self, before: &Document, after: &Document, command: &str) {
        self.0.push(Box::new((before.clone(), after.clone(), command.to_owned())));
    }
}

/// One call's view of WordCraft for craft-live: the engine session and the paragraph mapping.
struct App<'a> {
    session: &'a mut Session,
    map: &'a mut LiveMap,
    /// The last batch's mapping, for its paragraph ids once its updates are sent.
    pending: &'a mut Option<LocalEdit>,
}

impl LiveApp<Word> for App<'_> {
    fn local(&mut self, live: &mut LiveDoc, edits: &[Box<(Document, Document, String)>]) -> Result<Vec<Vec<u8>>, LocalError> {
        let command = edits.iter().map(|e| &e.2).find(|c| !is_shareable(c)).or_else(|| edits.last().map(|e| &e.2)).cloned();
        let (Some(command), Some(last), Some(first)) = (command, edits.last(), edits.first()) else { return Ok(Vec::new()) };
        match self.map.local_updates(live, &first.0, &last.1, &command) {
            Ok(edit) => {
                let updates = edit.updates.clone();
                *self.pending = Some(edit);
                Ok(updates)
            }
            Err(e) if e.refused().is_some() => Err(LocalError::Refused(refusal_text(&e).to_owned())),
            Err(e) => Err(LocalError::Failed(e.to_string())),
        }
    }

    fn landed(&mut self) -> Landed {
        let Some(edit) = self.pending.take() else { return Landed::Done };
        if let Err(e) = share_pids(self.session, &edit) {
            log::warn!("live: new paragraph ids were not written back: {e}");
            Landed::Resync
        } else if !edit.failed.is_empty() {
            Landed::Partial
        } else {
            Landed::Done
        }
    }

    fn put_back(&mut self, edits: Vec<Box<(Document, Document, String)>>) -> Result<(), String> {
        let Some(edit) = edits.into_iter().next() else { return Ok(()) };
        let (before, _, _) = *edit;
        self.session.apply_remote(|d| *d = before).map_err(|e| e.to_string())
    }

    fn received(&mut self, live: &mut LiveDoc, changes: Vec<BlockChange>) -> (Vec<BlockChange>, Vec<Vec<u8>>) {
        let map = &mut *self.map;
        // Recovered and forwarded text is the room's, not an edit of this user's: their Undo
        // must never take it back.
        let r = live.untracked(|d| map.received(d, changes));
        (r.changes, r.updates)
    }

    fn apply(&mut self, live: &LiveDoc, changes: &[BlockChange]) -> Result<(), String> {
        let map = &mut *self.map;
        self.session.apply_remote(|d| map.apply_changes(d, live, changes)).map_err(|e| e.to_string())
    }

    fn resync(&mut self, live: &LiveDoc) -> Result<(), String> {
        let all: Vec<BlockChange> = live.order().into_iter().enumerate().map(|(at, id)| BlockChange::Inserted { at, id }).collect();
        self.apply(live, &all)
    }

    fn reset(&mut self, space: u32) {
        let mut base = self.session.doc.clone();
        base.body.clear();
        *self.map = LiveMap::new(&base, space);
    }

    fn set_space(&mut self, space: u32) {
        self.map.set_space(space);
    }

    fn caret(&self, payload: &Value) -> Option<Caret> {
        let (pid, at) = payload.get("pid").and_then(Value::as_u64).zip(payload.get("at").and_then(Value::as_u64))?;
        let text = para_text(&self.session.doc, pid)?;
        let at = usize::try_from(at).unwrap_or(usize::MAX).min(text.chars().count());
        Some(Caret { pid, at, text })
    }

    fn follow(&self, carets: &mut HashMap<String, Caret>) {
        let paras: HashMap<u64, &str> =
            self.session.doc.body.iter().filter_map(|b| b.as_para()).filter(|p| p.pid != 0).map(|p| (p.pid, p.text.as_str())).collect();
        carets.retain(|_, c| match paras.get(&c.pid) {
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
}

/// One document edited live: the session's side of a [`Live`] link.
pub struct LiveEditing<H: Hub> {
    core: craft_live::LiveEditing<H, Word>,
    map: LiveMap,
    pending: Option<LocalEdit>,
    registry: Arc<Registry>,
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
        let core = craft_live::LiveEditing::start(hub, room, seed_blocks(&session.doc).as_slice(), access).map_err(LiveFail::Seed)?;
        let map = LiveMap::new(&session.doc, 0);
        session.set_observer(Some(Box::new(Observer(core.queue()))));
        session.live_undo_hook(Some(core.queue().history_hook()));
        Ok(LiveEditing { core, map, pending: None, registry: session.registry.clone() })
    }

    /// One frame: share the local edits, take in the other editors' edits, carets and saves,
    /// and say who else is here.
    pub fn pump(&mut self, session: &mut Session) -> Vec<Peer> {
        self.core.pump(&mut App { session, map: &mut self.map, pending: &mut self.pending })
    }

    /// Run a command on the session, refused when live editing does not allow it now, and
    /// share its edit right away.
    pub fn run(&mut self, session: &mut Session, id: &str, params: &Value) -> Result<Value, CmdError> {
        if !self.allows(id) {
            let why = self.refusal_for(id);
            self.core.push_notice(why.clone());
            return Err(CmdError::Disabled(why));
        }
        let result = session.run(id, params);
        self.flush(session);
        result
    }

    /// Share the edits the session made since the last flush, and run the Undo and Redo it
    /// asked for. Refused edits are put back, with a notice.
    pub fn flush(&mut self, session: &mut Session) {
        self.core.flush(&mut App { session, map: &mut self.map, pending: &mut self.pending });
    }

    /// Tell the room where this editor's caret is: paragraph `pid`, char `char_at`, and the
    /// other end of the selection. Held to at most five a second; the newest goes out when due.
    pub fn send_cursor(&mut self, pid: u64, char_at: usize, sel_end: Option<(u64, usize)>) {
        self.core.send_presence(json!({"pid": pid, "at": char_at, "end": sel_end.map(|(p, a)| json!([p, a]))}));
    }

    pub fn status(&self) -> &LiveStatus {
        self.core.status()
    }

    /// The highest relay sequence the replica has applied in order: an op that arrived ahead
    /// of a gap is not counted until the ops before it are in. An op can take a sequence and
    /// change nothing (a merged resend): the saver uses this to mark the file saved up to there
    /// when the document still equals it.
    pub fn newest_seq(&self) -> u64 {
        self.core.newest_seq()
    }

    /// Whether the host may take edits now: live, connected, with an edit ticket.
    pub fn editable(&self) -> bool {
        self.core.editable()
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

    /// Whether the newest save announced holds every local edit this replica made: each one
    /// the relay ordered is at or before the save's sequence, none is waiting to be ordered or
    /// sent again, and none was kept back from the room. False means the document has edits
    /// only this window holds, which reopening the file would lose.
    pub fn edits_saved(&self) -> bool {
        self.core.edits_saved()
    }

    /// Lines for the user since the last call, in plain English (untranslated).
    pub fn take_notices(&mut self) -> Vec<String> {
        self.core.take_notices()
    }

    /// Whether the link is live but no document service has been in the room for 15 s (it never
    /// joined, or it left): nothing saves this window's edits, so the window keeps them itself.
    /// Clears as soon as the service is in the room again.
    pub fn saver_missing(&self) -> bool {
        self.core.saver_missing()
    }

    /// Whether a join since the last call found the room had lost its history (the relay
    /// restarted with nothing to replay). The saver, holding a settled document, checkpoints
    /// it at once, so the room and every editor that waits for it have a snapshot again.
    pub fn take_history_lost(&mut self) -> bool {
        self.core.take_history_lost()
    }

    /// Checkpoint outcomes since the last call, for the saver.
    pub fn take_save_events(&mut self) -> Vec<SaveEvent> {
        self.core.take_save_events()
    }

    /// Checkpoint the shared document (the saver only): returns the state snapshotted. See
    /// the [module documentation](self) and `Live::checkpoint`.
    pub fn checkpoint(&mut self) -> craft_collab::Result<Vec<u8>> {
        self.core.checkpoint()
    }

    /// Announce a saved file (the saver only): `seq` the checkpoint's sequence, `etag` the
    /// file's new ETag. Every editor's status shows it.
    pub fn announce_saved(&mut self, seq: u64, etag: &str) -> craft_collab::Result<()> {
        self.core.announce_saved(seq, etag)
    }

    /// The newest save announced: its sequence and the file's ETag.
    pub fn saved(&self) -> Option<(u64, &str)> {
        self.core.saved()
    }

    /// Announce that the service leaves the room (the saver only): the newest save again, marked
    /// `closed`. The relay keeps it after the service has left, so an editor that rejoins the
    /// room later learns that nothing saves it any more.
    pub fn announce_closed(&mut self, seq: u64, etag: &str) -> craft_collab::Result<()> {
        self.core.announce_closed(seq, etag)
    }

    /// End the link from outside (the host learned the session is over: another room, less
    /// access): [`LiveStatus::Broken`] with `reason`, at once and for good.
    pub fn end(&mut self, reason: &str) {
        self.core.end(reason);
    }

    /// The uid whose `saved` markers count, [`SERVICE_UID`] unless the relay names another.
    pub fn set_service_uid(&mut self, uid: impl Into<String>) {
        self.core.set_service_uid(uid);
    }

    /// The clock the caret limit runs on, in milliseconds. Native builds have one; a browser
    /// host passes the page's.
    pub fn set_clock(&mut self, clock: Box<dyn FnMut() -> f64 + Send>) {
        self.core.set_clock(clock);
    }

    /// The link, for reading its state.
    pub fn live(&self) -> &Live<Tap<H>> {
        self.core.live()
    }

    /// The host's hub.
    pub fn hub(&self) -> &H {
        self.core.hub()
    }

    /// The host's hub, to steer its transport (a fresh ticket, for one).
    pub fn hub_mut(&mut self) -> &mut H {
        self.core.hub_mut()
    }

    /// The shared session underneath, for code that drives any Craft app's session (the
    /// document service's worker loop).
    pub fn core(&self) -> &craft_live::LiveEditing<H, Word> {
        &self.core
    }

    /// The shared session underneath, to act on it (checkpoint, announce, steer the hub).
    pub fn core_mut(&mut self) -> &mut craft_live::LiveEditing<H, Word> {
        &mut self.core
    }

    /// Stop editing live: the session keeps the document and takes back its own undo.
    pub fn stop(self, session: &mut Session) {
        session.set_observer(None);
        session.live_undo_hook(None);
    }

    fn refusal_for(&self, id: &str) -> String {
        if self.editable() { command_text(id).to_owned() } else { self.core.not_editable() }
    }
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

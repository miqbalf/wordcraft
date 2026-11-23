//! `wordcraft-cli live-worker`: the document service of live co-editing.
//!
//! The worker loop (the notify endpoint, room tasks, service tickets, checkpoint-then-write
//! saves with `If-Match`, conflict copies, heartbeats, closing empty rooms) is craft-live-worker's,
//! shared by every Craft app; see [`craft_live_worker`]. What is WordCraft's is here: only `.docx`
//! files are served, a file is opened into an engine [`Session`] and a [`LiveEditing`], the
//! shared content is the body, and a checkpoint's state is written back as the file through the
//! engine's own writer, with everything that is not shared taken from the file as opened.
//!
//! Tickets, tokens and secrets are never logged.

use std::future::Future;
use std::sync::Arc;

use craft_collab::Hub;
pub use craft_live_worker::{
    ALONE_MS, CONFLICT_TEXT, Clock, Config, DEFAULT_MAX_ROOMS, DocumentService, EMPTY_GRACE_MS, ENDED_KIND, FileError, FileGot, Flow, Grant,
    HEARTBEAT_MS, HubTicket, LiveDocument, MAX_UNSAVED_MS, NOTICE_KIND, Nextcloud, Notified, OpenError, QUIET_MS, RoomFiles, Rooms, SAVE_GIVE_UP_MS,
    Secret, Spawn, SystemClock, TICKET_REFRESH_SECS, TICKET_SECS, listen, router, service_ticket, worker_signature,
};
use wordcraft_doc::{Blocks, Document};
use wordcraft_engine::Session;
use wordcraft_engine::io::save_bytes;
use wordcraft_live::{LiveEditing, Word, document_from_state};

/// One live room served by the worker, over any hub. Drive it with [`Room::step`].
pub type Room<H> = craft_live_worker::Room<H, WordService>;

/// WordCraft's document service: Word documents (`.docx`) only. Phase 1 shares the body, and the
/// file is written back in its own format, which for anything else could lose what it holds.
#[derive(Clone, Copy, Debug, Default)]
pub struct WordService;

/// A Word document open in a live session in the worker.
pub struct WordDoc<H: Hub> {
    le: LiveEditing<H>,
    session: Session,
}

impl DocumentService for WordService {
    type Doc<H: Hub> = WordDoc<H>;

    fn accepts(&self, file_name: &str) -> bool {
        is_docx(file_name)
    }

    fn refusal(&self, file_name: &str) -> String {
        format!("only .docx files are edited live, not {file_name}")
    }

    fn open<H: Hub>(&self, hub: H, room: &str, file_name: &str, bytes: &[u8]) -> Result<WordDoc<H>, String> {
        let mut session = Session::new(Document::new());
        let le = LiveEditing::start(hub, room, bytes, file_name, &mut session).map_err(|e| e.to_string())?;
        Ok(WordDoc { le, session })
    }
}

impl<H: Hub> LiveDocument<H> for WordDoc<H> {
    type Kind = Word;
    /// Only the body is shared, so only the body is compared.
    type Snapshot = Blocks;

    fn live(&self) -> &craft_live::LiveEditing<H, Word> {
        self.le.core()
    }

    fn live_mut(&mut self) -> &mut craft_live::LiveEditing<H, Word> {
        self.le.core_mut()
    }

    fn pump(&mut self) -> usize {
        self.le.pump(&mut self.session).len()
    }

    fn snapshot(&self) -> Blocks {
        self.session.doc.body.clone()
    }

    /// The very same blocks (by address: copy-on-write blocks that were not changed keep theirs).
    fn unchanged(&self, a: &Blocks, b: &Blocks) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| Arc::ptr_eq(x, y))
    }

    /// Equal blocks: by address first, by value where that differs.
    fn same(&self, a: &Blocks, b: &Blocks) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| Arc::ptr_eq(x, y) || x == y)
    }

    fn save(&self, state: &[u8], name: &str) -> Result<Vec<u8>, String> {
        document_from_state(&self.session.doc, state).map_err(|e| e.to_string()).and_then(|doc| save_bytes(name, &doc))
    }
}

/// Whether `name` is a Word document (`.docx`), the only format edited live in phase 1.
fn is_docx(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(stem, ext)| !stem.is_empty() && ext.eq_ignore_ascii_case("docx"))
}

/// Serve one granted room until it closes or its grant ends (craft-live-worker's
/// [`serve_grant`](craft_live_worker::serve_grant) with WordCraft's service).
pub async fn serve_grant<H, C, F>(relay_secret: &Secret, files: RoomFiles, clock: Arc<dyn Clock>, connect: C)
where
    H: Hub,
    C: FnMut(String) -> F,
    F: Future<Output = Result<H, String>>,
{
    craft_live_worker::serve_grant(Arc::new(WordService), relay_secret, files, clock, connect).await;
}

/// Run the worker until it fails to listen.
pub fn serve(cfg: Config) -> Result<(), String> {
    craft_live_worker::serve(cfg, WordService)
}

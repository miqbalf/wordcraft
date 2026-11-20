//! `wordcraft-cli live-worker`: the document service of live co-editing.
//!
//! The Nextcloud app tells the worker about every edit grant (`POST /rooms {room, fileId,
//! token}`, signed `X-Craft-Worker: hex HMAC_SHA256(worker secret, "notify:" room ":" token)`).
//! For each room the worker runs one task that:
//!
//! 1. reads the file through the app's worker routes (signed `hex HMAC(worker secret, room ":"
//!    token)`, the token being the newest one a notify brought),
//! 2. mints its own relay ticket (uid [`SERVICE_UID`], read and write) with the relay secret and
//!    joins the room as the service peer over craft-collab's native hub,
//! 3. keeps a [`LiveEditing`] session in step with the editors and saves the shared document
//!    back: [`QUIET_MS`] after the last edit, and at least every [`MAX_UNSAVED_MS`] while
//!    edits keep coming. A save is a relay checkpoint first; the file is written only once the
//!    relay has acknowledged it ([`SaveEvent::Checkpointed`]), from the checkpoint's own state,
//!    with `If-Match` on the file's ETag. Then every editor is told (`saved {seq, etag}`).
//! 4. heartbeats the room every [`HEARTBEAT_MS`], and once no editor has been in the room for
//!    [`EMPTY_GRACE_MS`], saves a last time, closes the room (which releases the app lock) and
//!    leaves.
//!
//! When the file changed outside the session (`412`), the version found there is copied aside
//! as `"{stem} (conflict {YYYY-MM-DD HHMM}).{ext}"` (` 2` … ` 9` added when the name is
//! taken), the editors get a `notice`, and the room goes on saving over that version's ETag.
//!
//! Tickets, tokens and secrets are never logged.

use std::collections::HashMap;
use std::fmt;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use craft_collab::native::NativeHub;
use craft_collab::{ClientMsg, Hub, ServerMsg};
use hmac::digest::KeyInit;
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::json;
use sha2::Sha256;
use tokio::sync::watch;
use wordcraft_doc::{Blocks, Document};
use wordcraft_engine::Session;
use wordcraft_engine::io::save_bytes;
use wordcraft_live::{LiveEditing, LiveStatus, SERVICE_UID, SaveEvent, document_from_state};

/// The ephemeral kind the worker tells the editors something with: `{text}`.
pub const NOTICE_KIND: &str = "notice";
/// Save this long after the last edit.
pub const QUIET_MS: u64 = 5_000;
/// Save at least this often while edits keep coming.
pub const MAX_UNSAVED_MS: u64 = 60_000;
/// Tell the app the room is still served this often (it closes rooms silent for 120 s).
pub const HEARTBEAT_MS: u64 = 30_000;
/// A room with no editor for this long gets its last save and is closed.
pub const EMPTY_GRACE_MS: u64 = 10_000;
/// A room no editor ever joined is given up after this long.
pub const ALONE_MS: u64 = 120_000;
/// How long a service ticket is good for.
pub const TICKET_SECS: i64 = 600;
/// A ticket this old is replaced, so a reconnect never offers one close to expiry.
pub const TICKET_REFRESH_SECS: i64 = 480;
/// The text the editors get when the file changed outside the session (the copy's name follows).
pub const CONFLICT_TEXT: &str = "Someone changed this file outside the live session. Your session was saved as";

/// After a failed checkpoint or a raced one, wait this long before trying again.
const RETRY_MS: u64 = 2_000;
/// After a failed write, wait this long before checkpointing again.
const PUT_RETRY_MS: u64 = 5_000;
/// A closing room whose last save does not get through is closed anyway after this long.
const CLOSE_GIVE_UP_MS: u64 = 60_000;
/// A relay link down this long is replaced by a fresh connection with a fresh ticket.
const RELINK_MS: u64 = 5_000;
/// The first connection to the relay is given up after this long.
const CONNECT_GIVE_UP_MS: u64 = 120_000;
const TICK: Duration = Duration::from_millis(100);
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const MIN_SECRET_BYTES: usize = 32;
const CONFLICT_NAMES: u32 = 9;
/// The app's worker routes, under the Nextcloud URL.
const APP_PATH: &str = "/index.php/apps/craft_office/api/v1";
const NOTIFY_MAX_BYTES: usize = 16 * 1024;
const SERVICE_DISPLAY: &str = "Document service";

// ---- clock -----------------------------------------------------------------------------------

/// Time for the worker: a monotonic clock for the debounce and the heartbeat, and the wall clock
/// for ticket expiry and conflict names. Injected, so the timing can be tested.
pub trait Clock: Send + Sync {
    /// Milliseconds on a monotonic clock.
    fn now_ms(&self) -> u64;
    /// Milliseconds since the Unix epoch.
    fn unix_ms(&self) -> i64;
}

/// The real clock.
pub struct SystemClock {
    start: Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        SystemClock { start: Instant::now() }
    }
}

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
    fn unix_ms(&self) -> i64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
    }
}

// ---- secrets, signatures, tickets ------------------------------------------------------------

/// A shared secret from the environment. Its `Debug` shows nothing of it.
pub struct Secret(Vec<u8>);

impl Secret {
    /// The secret in `value` (environment variable `name`), trimmed; refused when missing or
    /// shorter than 32 bytes.
    pub fn new(name: &str, value: Option<&str>) -> Result<Secret, String> {
        let v = value.map(str::trim).unwrap_or("");
        if v.is_empty() {
            return Err(format!("{name} is not set"));
        }
        if v.len() < MIN_SECRET_BYTES {
            return Err(format!("{name} must be at least {MIN_SECRET_BYTES} bytes (use 64 hex characters)"));
        }
        Ok(Secret(v.as_bytes().to_vec()))
    }

    fn mac(&self, msg: &[u8]) -> Result<Hmac<Sha256>, String> {
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(&self.0).map_err(|e| format!("hmac: {e}"))?;
        mac.update(msg);
        Ok(mac)
    }

    fn sign(&self, msg: &[u8]) -> Result<Vec<u8>, String> {
        Ok(self.mac(msg)?.finalize().into_bytes().to_vec())
    }

    /// Whether `hex_sig` is this secret's signature of `msg`, compared in constant time.
    fn verifies(&self, msg: &[u8], hex_sig: &str) -> bool {
        let Some(sig) = unhex(hex_sig.trim()) else { return false };
        self.mac(msg).is_ok_and(|mac| mac.verify_slice(&sig).is_ok())
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(..)")
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from(DIGITS[usize::from(b >> 4)]));
        s.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    s
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| s.get(i..i + 2).and_then(|p| u8::from_str_radix(p, 16).ok())).collect()
}

/// The signature the app checks on the worker's file and room calls: `hex HMAC(secret, room ":"
/// token)`.
pub fn worker_signature(secret: &Secret, room: &str, token: &str) -> Result<String, String> {
    Ok(hex(&secret.sign(format!("{room}:{token}").as_bytes())?))
}

/// A relay ticket for the service peer in `room`: `base64url(claims).base64url(HMAC_SHA256)`,
/// uid [`SERVICE_UID`], read and write, [`TICKET_SECS`] from `unix_secs`.
pub fn service_ticket(relay_secret: &Secret, room: &str, unix_secs: i64) -> Result<String, String> {
    let mut nonce = [0u8; 4];
    getrandom::fill(&mut nonce).map_err(|e| format!("no randomness for the peer id: {e}"))?;
    let claims = json!({
        "rm": room,
        "uid": SERVICE_UID,
        "dsp": SERVICE_DISPLAY,
        "perm": 3,
        "exp": unix_secs + TICKET_SECS,
        "pid": format!("{SERVICE_UID}~{}", hex(&nonce)),
    });
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).map_err(|e| e.to_string())?);
    let sig = URL_SAFE_NO_PAD.encode(relay_secret.sign(payload.as_bytes())?);
    Ok(format!("{payload}.{sig}"))
}

/// Whether `room` is a room id the app hands out (`<instance>:<fileid>:<gen>`): safe in a URL
/// path and as a relay room.
fn room_ok(room: &str) -> bool {
    !room.is_empty()
        && room.len() <= 200
        && room.contains(':')
        && !room.starts_with(':')
        && !room.ends_with(':')
        && room.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '-' | '_' | '.'))
}

fn token_ok(token: &str) -> bool {
    !token.is_empty() && token.len() <= 256 && token.chars().all(|c| c.is_ascii_alphanumeric())
}

// ---- configuration ---------------------------------------------------------------------------

/// What `live-worker` runs with.
#[derive(Debug)]
pub struct Config {
    pub listen: SocketAddr,
    /// The relay as it is published, base path included (`https://host/craft`).
    pub relay: String,
    /// The Nextcloud base URL.
    pub nextcloud: String,
    pub relay_secret: Arc<Secret>,
    pub worker_secret: Arc<Secret>,
}

impl Config {
    /// Check the command line and the secrets (from `CRAFT_RELAY_SECRET` and
    /// `CRAFT_WORKER_SECRET`). Plain `http://` URLs only with `insecure_dev`.
    pub fn new(
        listen: &str,
        relay: &str,
        nextcloud: &str,
        insecure_dev: bool,
        relay_secret: Option<&str>,
        worker_secret: Option<&str>,
    ) -> Result<Config, String> {
        let listen: SocketAddr = listen.parse().map_err(|_| format!("--listen needs HOST:PORT, got `{listen}`"))?;
        Ok(Config {
            listen,
            relay: checked_url("--relay", relay, insecure_dev)?,
            nextcloud: checked_url("--nextcloud", nextcloud, insecure_dev)?,
            relay_secret: Arc::new(Secret::new("CRAFT_RELAY_SECRET", relay_secret)?),
            worker_secret: Arc::new(Secret::new("CRAFT_WORKER_SECRET", worker_secret)?),
        })
    }
}

fn checked_url(what: &str, url: &str, insecure_dev: bool) -> Result<String, String> {
    let url = url.trim().trim_end_matches('/');
    let host = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://").filter(|_| insecure_dev));
    match host {
        Some(h) if !h.is_empty() => Ok(url.to_owned()),
        _ if url.starts_with("http://") => Err(format!("{what} must be https:// (http:// only with --insecure-dev, for a dev stack)")),
        _ => Err(format!("{what} needs an https:// URL, got `{url}`")),
    }
}

// ---- the Nextcloud app's worker routes ------------------------------------------------------

/// Why a file call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileError {
    /// `412`: the file is not the version the worker last saw.
    Stale,
    /// `409`: a copy by that name exists.
    Exists,
    /// `403`: the grant is gone or the last editor lost access.
    Forbidden,
    Http(String),
}

impl fmt::Display for FileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FileError::Stale => f.write_str("the file changed since it was read"),
            FileError::Exists => f.write_str("that name is taken"),
            FileError::Forbidden => f.write_str("the server no longer allows this room to write the file"),
            FileError::Http(e) => f.write_str(e),
        }
    }
}

/// The Nextcloud the worker serves.
pub struct Nextcloud {
    base: String,
    secret: Arc<Secret>,
    http: reqwest::Client,
}

impl Nextcloud {
    pub fn new(base: &str, secret: Arc<Secret>) -> Result<Nextcloud, String> {
        // No redirects: the signature header would follow one to wherever it points.
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| format!("http client: {e}"))?;
        Ok(Nextcloud { base: base.trim_end_matches('/').to_owned(), secret, http })
    }
}

/// The file as read.
pub struct FileGot {
    pub bytes: Vec<u8>,
    pub name: String,
    pub etag: String,
}

/// One room's file and room calls, signed with the newest grant token.
pub struct RoomFiles {
    nc: Arc<Nextcloud>,
    room: String,
    token: watch::Receiver<String>,
}

impl RoomFiles {
    pub fn new(nc: Arc<Nextcloud>, room: String, token: watch::Receiver<String>) -> RoomFiles {
        RoomFiles { nc, room, token }
    }

    pub fn room(&self) -> &str {
        &self.room
    }

    fn url(&self, route: &str, tail: &str) -> String {
        format!("{}{APP_PATH}/{route}/{}{tail}", self.nc.base, self.room)
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<reqwest::Response, FileError> {
        let token = self.token.borrow().clone();
        let sig = worker_signature(&self.nc.secret, &self.room, &token).map_err(FileError::Http)?;
        let resp = req.header("X-Craft-Worker", sig).send().await.map_err(|e| FileError::Http(e.without_url().to_string()))?;
        match resp.status().as_u16() {
            200..=299 => Ok(resp),
            412 => Err(FileError::Stale),
            409 => Err(FileError::Exists),
            403 => Err(FileError::Forbidden),
            s => Err(FileError::Http(format!("the server answered {s}"))),
        }
    }

    pub async fn get(&self) -> Result<FileGot, FileError> {
        let resp = self.send(self.nc.http.get(self.url("file", ""))).await?;
        let header = |h: &str| resp.headers().get(h).and_then(|v| v.to_str().ok()).map(str::to_owned);
        let etag =
            header("ETag").map(|e| bare_etag(&e)).filter(|e| !e.is_empty()).ok_or_else(|| FileError::Http("the file came without an ETag".into()))?;
        let name = file_name_of(header("Content-Disposition").as_deref()).unwrap_or_else(|| "document.docx".to_owned());
        let bytes = resp.bytes().await.map_err(|e| FileError::Http(e.without_url().to_string()))?.to_vec();
        Ok(FileGot { bytes, name, etag })
    }

    /// Write the file over the version `etag`; the new ETag.
    pub async fn put(&self, bytes: Vec<u8>, etag: &str) -> Result<String, FileError> {
        let req = self.nc.http.put(self.url("file", "")).header("If-Match", format!("\"{etag}\"")).body(bytes);
        let resp = self.send(req).await?;
        let body: serde_json::Value = serde_json::from_slice(&resp.bytes().await.map_err(|e| FileError::Http(e.without_url().to_string()))?)
            .map_err(|_| FileError::Http("the save answer was not JSON".into()))?;
        body.get("etag").and_then(|e| e.as_str()).map(bare_etag).ok_or_else(|| FileError::Http("the save answer had no etag".into()))
    }

    /// Copy the file as it is now to `name` beside it.
    pub async fn copy(&self, name: &str) -> Result<String, FileError> {
        let body = serde_json::to_vec(&json!({ "name": name })).map_err(|e| FileError::Http(e.to_string()))?;
        let req = self.nc.http.post(self.url("file", "/copy")).header("Content-Type", "application/json").body(body);
        let resp = self.send(req).await?;
        let v: serde_json::Value =
            serde_json::from_slice(&resp.bytes().await.map_err(|e| FileError::Http(e.without_url().to_string()))?).unwrap_or_default();
        Ok(v.get("name").and_then(|n| n.as_str()).unwrap_or(name).to_owned())
    }

    pub async fn heartbeat(&self) -> Result<(), FileError> {
        self.send(self.nc.http.post(self.url("room", "/heartbeat"))).await.map(drop)
    }

    pub async fn close(&self) -> Result<(), FileError> {
        self.send(self.nc.http.post(self.url("room", "/close"))).await.map(drop)
    }
}

fn bare_etag(e: &str) -> String {
    e.trim().trim_start_matches("W/").trim_matches('"').to_owned()
}

/// The file name a `Content-Disposition` header gives, `filename*=` (RFC 5987) first.
fn file_name_of(header: Option<&str>) -> Option<String> {
    let header = header?;
    let parts = || header.split(';').map(str::trim);
    let extended = parts().find_map(|p| p.strip_prefix("filename*=")).and_then(|v| {
        let v = v.trim_matches('"');
        percent_decode(v.split_once("''").map_or(v, |(_, rest)| rest))
    });
    let name = extended.or_else(|| parts().find_map(|p| p.strip_prefix("filename=")).map(|v| v.trim_matches('"').to_owned()))?;
    let name = name.rsplit(['/', '\\']).next().unwrap_or("").to_owned();
    (!name.is_empty() && name != "." && name != "..").then_some(name)
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        if b == b'%' {
            let pair = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(pair, 16).ok()?);
            i += 3;
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// `"{stem} (conflict {YYYY-MM-DD HHMM}).{ext}"`, with ` {n}` after the parenthesis for `n > 1`.
fn conflict_name(file_name: &str, unix_ms: i64, n: u32) -> String {
    let when = chrono::DateTime::from_timestamp_millis(unix_ms).map_or_else(String::new, |t| t.format("%Y-%m-%d %H%M").to_string());
    let (stem, ext) = match file_name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, Some(ext)),
        _ => (file_name, None),
    };
    let nth = if n > 1 { format!(" {n}") } else { String::new() };
    match ext {
        Some(ext) => format!("{stem} (conflict {when}){nth}.{ext}"),
        None => format!("{stem} (conflict {when}){nth}"),
    }
}

// ---- one room --------------------------------------------------------------------------------

/// What a room's task does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    Continue,
    /// The room is closed (or gone); leave it.
    Done,
}

/// A checkpoint sent and not answered yet: its state, and when its unsaved edits began.
struct Inflight {
    state: Vec<u8>,
    dirty_since: u64,
}

/// Why a room failed to start.
#[derive(Debug)]
pub enum OpenError {
    File(FileError),
    Live(String),
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpenError::File(e) => write!(f, "the file could not be read: {e}"),
            OpenError::Live(e) => write!(f, "live editing did not start: {e}"),
        }
    }
}

/// One live room served by the worker, over any hub. Drive it with [`Room::step`].
pub struct Room<H: Hub> {
    le: LiveEditing<H>,
    session: Session,
    files: RoomFiles,
    clock: Arc<dyn Clock>,
    doc_id: String,
    file_name: String,
    etag: String,
    /// The body as last looked at, to tell an edit by its blocks' addresses.
    seen_body: Blocks,
    /// When the oldest edit not yet in a save request arrived.
    dirty_since: Option<u64>,
    last_edit: u64,
    inflight: Option<Inflight>,
    retry_at: u64,
    /// Saving stopped for good, and why.
    stopped: Option<String>,
    next_heartbeat: u64,
    opened_at: u64,
    seen_editor: bool,
    empty_since: Option<u64>,
}

impl<H: Hub> Room<H> {
    /// Read the file and join the room on `hub` as the service peer.
    pub async fn open(hub: H, files: RoomFiles, clock: Arc<dyn Clock>) -> Result<Room<H>, OpenError> {
        let got = files.get().await.map_err(OpenError::File)?;
        let doc_id = format!("doc:{}", files.room());
        let mut session = Session::new(Document::new());
        let mut le = LiveEditing::start(hub, &doc_id, &got.bytes, &got.name, &mut session).map_err(|e| OpenError::Live(e.to_string()))?;
        let c = clock.clone();
        le.set_clock(Box::new(move || c.now_ms() as f64));
        let now = clock.now_ms();
        Ok(Room {
            seen_body: session.doc.body.clone(),
            le,
            session,
            files,
            clock,
            doc_id,
            file_name: got.name,
            etag: got.etag,
            dirty_since: None,
            last_edit: now,
            inflight: None,
            retry_at: 0,
            stopped: None,
            next_heartbeat: now + HEARTBEAT_MS,
            opened_at: now,
            seen_editor: false,
            empty_since: None,
        })
    }

    /// The ETag of the file as last read or written.
    pub fn etag(&self) -> &str {
        &self.etag
    }

    /// Why saving stopped for good, if it did.
    pub fn saving_stopped(&self) -> Option<&str> {
        self.stopped.as_deref()
    }

    pub fn hub(&self) -> &H {
        self.le.hub()
    }

    pub fn hub_mut(&mut self) -> &mut H {
        self.le.hub_mut()
    }

    /// One turn: take in the editors' edits, act on checkpoint answers (writing the file),
    /// heartbeat, start a save when one is due, and close the room once it has stayed empty.
    pub async fn step(&mut self) -> Flow {
        let now = self.clock.now_ms();
        let editors = self.le.pump(&mut self.session);
        for n in self.le.take_notices() {
            say(&self.doc_id, format_args!("live: {n}"));
        }
        self.note_edits(now);
        if let LiveStatus::Broken(why) = self.le.status().clone() {
            self.stop(&why);
        }
        self.answers(now).await;

        if now >= self.next_heartbeat {
            self.next_heartbeat = now + HEARTBEAT_MS;
            match self.files.heartbeat().await {
                Ok(()) => {}
                Err(FileError::Forbidden) => {
                    say(&self.doc_id, "the app ended this room's grant; leaving");
                    self.tell("This live session was ended on the server. Changes from now on are not saved; reopen the file.");
                    return Flow::Done;
                }
                Err(e) => say(&self.doc_id, format_args!("heartbeat failed: {e}")),
            }
        }

        if editors.is_empty() {
            if self.seen_editor || now.saturating_sub(self.opened_at) >= ALONE_MS {
                self.empty_since.get_or_insert(now);
            }
        } else {
            self.seen_editor = true;
            self.empty_since = None;
        }
        let closing_for = self.empty_since.map(|t| now.saturating_sub(t)).filter(|d| *d >= EMPTY_GRACE_MS);

        self.maybe_checkpoint(now, closing_for.is_some());

        if let Some(d) = closing_for {
            let settled = self.inflight.is_none() && (self.dirty_since.is_none() || self.stopped.is_some());
            if settled || d >= EMPTY_GRACE_MS + CLOSE_GIVE_UP_MS {
                if !settled {
                    say(&self.doc_id, "the last save did not get through; closing anyway");
                }
                if let Err(e) = self.files.close().await {
                    say(&self.doc_id, format_args!("closing the room failed: {e}"));
                }
                say(&self.doc_id, "no editors left; room closed");
                return Flow::Done;
            }
        }
        Flow::Continue
    }

    /// Mark the document dirty when the editors changed it. Changes before the session is live
    /// (the join's catch-up) are not edits.
    fn note_edits(&mut self, now: u64) {
        let body = &self.session.doc.body;
        let changed = body.len() != self.seen_body.len() || body.iter().zip(&self.seen_body).any(|(a, b)| !Arc::ptr_eq(a, b));
        if !changed {
            return;
        }
        self.seen_body = body.clone();
        if matches!(self.le.status(), LiveStatus::Live { .. }) {
            self.last_edit = now;
            self.dirty_since.get_or_insert(now);
        }
    }

    fn maybe_checkpoint(&mut self, now: u64, closing: bool) {
        let Some(since) = self.dirty_since else { return };
        if self.stopped.is_some() || self.inflight.is_some() || now < self.retry_at {
            return;
        }
        let due = closing || now.saturating_sub(self.last_edit) >= QUIET_MS || now.saturating_sub(since) >= MAX_UNSAVED_MS;
        if !due {
            return;
        }
        // Outcomes of an earlier checkpoint were all taken in `answers`; none is pending.
        match self.le.checkpoint() {
            Ok(state) => {
                self.inflight = Some(Inflight { state, dirty_since: since });
                self.dirty_since = None;
            }
            Err(craft_collab::Error::Refused(why)) => self.stop(&why),
            Err(e) => {
                log::debug!("live-worker: checkpoint not sent ({e}); retrying");
                self.retry_at = now + RETRY_MS;
            }
        }
    }

    /// Checkpoint outcomes: write the file for an acknowledged one, retry a raced one, stop on a
    /// refused one.
    async fn answers(&mut self, now: u64) {
        for event in self.le.take_save_events() {
            let Some(inflight) = self.inflight.take() else { continue };
            match event {
                SaveEvent::Checkpointed { seq } => self.write(inflight, seq, now).await,
                SaveEvent::Raced => {
                    self.unsaved(inflight.dirty_since);
                    self.retry_at = now + RETRY_MS;
                }
                SaveEvent::Refused { reason } => {
                    self.unsaved(inflight.dirty_since);
                    self.stop(&reason);
                }
            }
        }
    }

    /// The edits since `since` are not saved after all.
    fn unsaved(&mut self, since: u64) {
        self.dirty_since = Some(self.dirty_since.map_or(since, |d| d.min(since)));
    }

    /// Write the acknowledged checkpoint `inflight` (ordered at `seq`) to the file.
    async fn write(&mut self, inflight: Inflight, seq: u64, now: u64) {
        let bytes = match document_from_state(&self.session.doc, &inflight.state)
            .map_err(|e| e.to_string())
            .and_then(|doc| save_bytes(&self.file_name, &doc))
        {
            Ok(b) => b,
            Err(e) => {
                self.unsaved(inflight.dirty_since);
                self.stop(&format!("the document could not be written as {}: {e}", self.file_name));
                return;
            }
        };
        match self.put(bytes).await {
            Ok(etag) => {
                if let Err(e) = self.le.announce_saved(seq, &etag) {
                    say(&self.doc_id, format_args!("the saved marker did not go out: {e}"));
                }
                self.etag = etag;
            }
            Err(e) => {
                say(&self.doc_id, format_args!("saving failed: {e}"));
                self.unsaved(inflight.dirty_since);
                if e == FileError::Forbidden {
                    self.stop("the server no longer lets this session write the file");
                } else {
                    self.retry_at = now + PUT_RETRY_MS;
                }
            }
        }
    }

    /// PUT over the known version; when it changed outside, copy that version aside and write
    /// over it.
    async fn put(&mut self, bytes: Vec<u8>) -> Result<String, FileError> {
        match self.files.put(bytes.clone(), &self.etag).await {
            Err(FileError::Stale) => {}
            other => return other,
        }
        let outside = self.files.get().await?;
        let name = copy_aside(&self.files, &self.file_name, self.clock.unix_ms()).await?;
        say(&self.doc_id, "the file changed outside the session; that version was copied aside");
        self.tell(&format!("{CONFLICT_TEXT} {name}."));
        self.etag = outside.etag;
        self.files.put(bytes, &self.etag).await
    }

    /// Stop saving for good: tell the editors, keep the room open.
    fn stop(&mut self, why: &str) {
        if self.stopped.is_some() {
            return;
        }
        say(&self.doc_id, format_args!("saving stopped: {why}"));
        self.tell(&format!("This live session can no longer be saved to the file ({why}). Keep a copy of your work."));
        self.stopped = Some(why.to_owned());
    }

    /// A `notice` for every editor.
    fn tell(&mut self, text: &str) {
        let msg = ClientMsg::Ephemeral { doc: self.doc_id.clone(), kind: NOTICE_KIND.to_owned(), payload: json!({ "text": text }) };
        if let Err(e) = self.le.hub_mut().send(msg) {
            say(&self.doc_id, format_args!("a notice did not go out: {e}"));
        }
    }
}

/// Copy the file as it is now aside under the first free conflict name. A free function, so a
/// room's task never holds a shared borrow of the room (not `Sync`) across an await.
async fn copy_aside(files: &RoomFiles, file_name: &str, unix_ms: i64) -> Result<String, FileError> {
    for n in 1..=CONFLICT_NAMES {
        match files.copy(&conflict_name(file_name, unix_ms, n)).await {
            Err(FileError::Exists) => {}
            other => return other,
        }
    }
    Err(FileError::Http("no free name for the conflict copy".into()))
}

fn say(room: &str, msg: impl fmt::Display) {
    eprintln!("live-worker {room}: {msg}");
}

// ---- notifications ---------------------------------------------------------------------------

/// A room the app granted, as its notify said.
pub struct Grant {
    pub room: String,
    pub file_id: u64,
    /// The newest grant token; a later notify for the same room replaces it.
    pub token: watch::Receiver<String>,
    id: u64,
}

/// The rooms being served, so a second notify for a room updates its token rather than
/// starting a second task.
#[derive(Default)]
pub struct Rooms {
    live: Mutex<HashMap<String, (u64, watch::Sender<String>)>>,
    next: AtomicU64,
}

impl Rooms {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, (u64, watch::Sender<String>)>> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A new grant to serve, or `None` when the room is served already (its token is updated).
    pub fn notify(&self, room: &str, file_id: u64, token: &str) -> Option<Grant> {
        let mut live = self.lock();
        if let Some((_, tx)) = live.get(room) {
            tx.send_replace(token.to_owned());
            return None;
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = watch::channel(token.to_owned());
        live.insert(room.to_owned(), (id, tx));
        Some(Grant { room: room.to_owned(), file_id, token: rx, id })
    }

    /// The task serving `grant` ended.
    pub fn finished(&self, grant: &Grant) {
        let mut live = self.lock();
        if live.get(&grant.room).is_some_and(|(id, _)| *id == grant.id) {
            live.remove(&grant.room);
        }
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Starts the task for a new grant.
pub type Spawn = Arc<dyn Fn(Grant) + Send + Sync>;

#[derive(Clone)]
struct NotifyState {
    secret: Arc<Secret>,
    rooms: Arc<Rooms>,
    spawn: Spawn,
}

#[derive(Deserialize)]
struct NotifyBody {
    room: String,
    #[serde(rename = "fileId")]
    file_id: u64,
    token: String,
}

/// The worker's HTTP routes: `POST /rooms` (the app's signed notify) and `GET /healthz`.
pub fn router(worker_secret: Arc<Secret>, rooms: Arc<Rooms>, spawn: Spawn) -> Router {
    Router::new()
        .route("/rooms", post(notify))
        .route("/healthz", get(|| async { "ok" }))
        .layer(DefaultBodyLimit::max(NOTIFY_MAX_BYTES))
        .with_state(NotifyState { secret: worker_secret, rooms, spawn })
}

async fn notify(State(st): State<NotifyState>, headers: HeaderMap, body: Bytes) -> (StatusCode, &'static str) {
    let Ok(n) = serde_json::from_slice::<NotifyBody>(&body) else {
        return (StatusCode::BAD_REQUEST, "bad request");
    };
    if !room_ok(&n.room) || !token_ok(&n.token) {
        return (StatusCode::BAD_REQUEST, "bad request");
    }
    let sig = headers.get("X-Craft-Worker").and_then(|v| v.to_str().ok()).unwrap_or("");
    if !st.secret.verifies(format!("notify:{}:{}", n.room, n.token).as_bytes(), sig) {
        return (StatusCode::FORBIDDEN, "forbidden");
    }
    if let Some(grant) = st.rooms.notify(&n.room, n.file_id, &n.token) {
        say(&n.room, "granted; joining");
        (st.spawn)(grant);
    }
    (StatusCode::ACCEPTED, "ok")
}

// ---- the service ------------------------------------------------------------------------------

/// The relay link of a room: a native hub that can be replaced by a fresh connection.
#[derive(Default)]
pub struct RelayLink {
    hub: Option<NativeHub>,
}

impl Hub for RelayLink {
    fn send(&mut self, msg: ClientMsg) -> craft_collab::Result<()> {
        match self.hub.as_mut() {
            Some(h) => h.send(msg),
            None => Err(craft_collab::Error::Hub("not connected to the relay".into())),
        }
    }
    fn poll(&mut self) -> Vec<ServerMsg> {
        self.hub.as_mut().map(Hub::poll).unwrap_or_default()
    }
    fn connected(&self) -> bool {
        self.hub.as_ref().is_some_and(Hub::connected)
    }
}

/// The current service ticket of a room, re-minted before it gets old.
struct Ticket {
    ticket: String,
    minted: i64,
}

impl Ticket {
    fn fresh(cfg: &Config, room: &str, clock: &dyn Clock) -> Result<Ticket, String> {
        let now = clock.unix_ms() / 1000;
        Ok(Ticket { ticket: service_ticket(&cfg.relay_secret, room, now)?, minted: now })
    }

    fn refresh(&mut self, cfg: &Config, room: &str, clock: &dyn Clock) {
        if clock.unix_ms() / 1000 - self.minted >= TICKET_REFRESH_SECS {
            match Ticket::fresh(cfg, room, clock) {
                Ok(t) => *self = t,
                Err(e) => say(room, format_args!("ticket: {e}")),
            }
        }
    }
}

async fn connect(cfg: &Config, room: &str, ticket: &mut Ticket, clock: &dyn Clock) -> Option<NativeHub> {
    ticket.refresh(cfg, room, clock);
    match NativeHub::connect(&cfg.relay, &ticket.ticket, None).await {
        Ok(h) => Some(h),
        Err(e) => {
            say(room, format_args!("relay connection failed: {e}"));
            None
        }
    }
}

async fn run_room(cfg: Arc<Config>, nc: Arc<Nextcloud>, rooms: Arc<Rooms>, grant: Grant) {
    serve_room(&cfg, nc, &grant).await;
    rooms.finished(&grant);
}

async fn serve_room(cfg: &Config, nc: Arc<Nextcloud>, grant: &Grant) {
    let room = grant.room.as_str();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::default());
    let mut ticket = match Ticket::fresh(cfg, room, clock.as_ref()) {
        Ok(t) => t,
        Err(e) => return say(room, format_args!("ticket: {e}")),
    };
    let start = clock.now_ms();
    let hub = loop {
        if let Some(h) = connect(cfg, room, &mut ticket, clock.as_ref()).await {
            break h;
        }
        if clock.now_ms().saturating_sub(start) >= CONNECT_GIVE_UP_MS {
            return say(room, "the relay stayed unreachable; giving up");
        }
        tokio::time::sleep(Duration::from_millis(RELINK_MS)).await;
    };
    let files = RoomFiles::new(nc, grant.room.clone(), grant.token.clone());
    let mut w = match Room::open(RelayLink { hub: Some(hub) }, files, clock.clone()).await {
        Ok(w) => w,
        Err(e) => return say(room, e),
    };
    let mut down_since: Option<u64> = None;
    let mut next_try = 0;
    loop {
        if w.step().await == Flow::Done {
            return;
        }
        ticket.refresh(cfg, room, clock.as_ref());
        let now = clock.now_ms();
        if w.hub().connected() {
            down_since = None;
        } else if now.saturating_sub(*down_since.get_or_insert(now)) >= RELINK_MS && now >= next_try {
            // The hub's own reconnect reuses the first ticket and never says it is back, so a
            // link down this long is replaced with a fresh connection and a fresh ticket.
            next_try = now + RELINK_MS;
            if let Some(h) = connect(cfg, room, &mut ticket, clock.as_ref()).await {
                w.hub_mut().hub = Some(h);
                down_since = None;
            }
        }
        tokio::time::sleep(TICK).await;
    }
}

struct StderrLog;

impl log::Log for StderrLog {
    fn enabled(&self, m: &log::Metadata<'_>) -> bool {
        m.level() <= log::Level::Warn
    }
    fn log(&self, r: &log::Record<'_>) {
        if self.enabled(r.metadata()) {
            eprintln!("live-worker {} {}: {}", r.level(), r.target(), r.args());
        }
    }
    fn flush(&self) {}
}

static LOG: StderrLog = StderrLog;

/// Run the worker until it fails to listen.
pub fn serve(cfg: Config) -> Result<(), String> {
    if log::set_logger(&LOG).is_ok() {
        log::set_max_level(log::LevelFilter::Warn);
    }
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().map_err(|e| format!("runtime: {e}"))?;
    rt.block_on(async move {
        let cfg = Arc::new(cfg);
        let nc = Arc::new(Nextcloud::new(&cfg.nextcloud, cfg.worker_secret.clone())?);
        let rooms = Arc::new(Rooms::default());
        let spawn: Spawn = {
            let (cfg, nc, rooms) = (cfg.clone(), nc.clone(), rooms.clone());
            Arc::new(move |grant: Grant| {
                tokio::spawn(run_room(cfg.clone(), nc.clone(), rooms.clone(), grant));
            })
        };
        let listener = tokio::net::TcpListener::bind(cfg.listen).await.map_err(|e| format!("cannot listen on {}: {e}", cfg.listen))?;
        eprintln!("live-worker: listening on {}, relay {}, nextcloud {}", cfg.listen, cfg.relay, cfg.nextcloud);
        axum::serve(listener, router(cfg.worker_secret.clone(), rooms, spawn)).await.map_err(|e| format!("server: {e}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflict_names() {
        // 2026-11-21 14:03 UTC
        let at = 1_795_269_780_000;
        assert_eq!(conflict_name("Plan.docx", at, 1), "Plan (conflict 2026-11-21 1403).docx");
        assert_eq!(conflict_name("Plan.docx", at, 3), "Plan (conflict 2026-11-21 1403) 3.docx");
        assert_eq!(conflict_name("README", at, 1), "README (conflict 2026-11-21 1403)");
    }

    #[test]
    fn content_disposition_names() {
        assert_eq!(file_name_of(Some("attachment; filename=\"Plan.docx\"")).as_deref(), Some("Plan.docx"));
        assert_eq!(file_name_of(Some("attachment; filename=\"x\"; filename*=UTF-8''Pl%C3%A4n.docx")).as_deref(), Some("Plän.docx"));
        assert_eq!(file_name_of(Some("attachment; filename=\"../../etc/passwd\"")).as_deref(), Some("passwd"));
        assert_eq!(file_name_of(Some("attachment")), None);
    }
}

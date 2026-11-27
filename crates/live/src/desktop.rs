//! Desktop joins: a desktop window edits the same live session as the web app, over a native
//! socket (feature `native-hub`).
//!
//! The launcher asks the server for a ticket, writes it to a session file and starts the app with
//! `--live-session <file>` and a local working copy of the file. This module reads that file
//! ([`read`]), connects to the relay without blocking the window ([`connect`]), and follows the
//! launcher's ticket refreshes ([`SessionWatch`]). Everything after the connection is the same
//! [`crate::LiveEditing`] the web app runs; nothing on the wire differs.
//!
//! # The session file
//!
//! JSON, written by the launcher (mode 0600):
//!
//! ```json
//! {"v":1,"server":"https://cloud.example","fileId":123,"path":"/Docs/a.docx",
//!  "ticket":"…","relayUrl":"wss://cloud.example/craft","room":"…",
//!  "expiresAt":"2026-11-27T12:00:00Z","readOnly":false,"displayName":"…"}
//! ```
//!
//! `saver` may be added (the ticket answer's; `false` when no document service saves the room
//! right now). The relay URL may be a path on the server (`/craft`). Unknown fields are ignored. A file of another version `v` is refused with
//! [`SessionError::Version`], so the window opens the working copy without live editing and says
//! why. The relay must be on the server's host (a relative `relayUrl` is), as the web app
//! requires of the page's host, so the ticket goes nowhere else. The ticket is never logged:
//! [`LiveSessionFile`]'s `Debug` leaves it out.
//!
//! # Refresh
//!
//! The launcher rewrites the same file (a new file renamed over it) with a new ticket before the
//! old one expires. [`SessionWatch::poll`] looks at the file's modification time every
//! [`WATCH_INTERVAL`] and reports a new ticket ([`SessionChange::Ticket`], for
//! `Hub::set_token`), or the end of the link: another room means the session was restarted, and
//! a read-only file after an editable one means the access dropped.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use craft_collab::native::NativeHub;
use craft_collab::{ClientMsg, Handover, Hub, Reopen, ServerMsg};
use serde::Deserialize;
pub use tokio::runtime::{Handle, Runtime};
use tokio::sync::oneshot;

use crate::Access;

/// The session file version this build reads.
pub const SESSION_VERSION: u64 = 1;

/// How often [`SessionWatch::poll`] looks at the file.
pub const WATCH_INTERVAL: Duration = Duration::from_secs(2);

/// The end when the session file names another room: the session was restarted. The same words
/// as the web app's, so they share a translation.
pub const RESTARTED: &str = "The live session was restarted. Reopen the file.";

/// The end when the refreshed session file gives less access.
pub const ACCESS_CHANGED: &str = "Your access changed. Reopen the file.";

/// The first wait before trying an unreachable relay again; it doubles up to [`RETRY_MAX`].
const RETRY_FIRST: Duration = Duration::from_millis(250);
/// The longest wait between tries.
const RETRY_MAX: Duration = Duration::from_secs(5);

/// The session file the launcher wrote, checked.
#[derive(Clone, PartialEq, Eq)]
pub struct LiveSessionFile {
    /// The Nextcloud server, `https://host`.
    pub server: String,
    /// The file's path on the server, for its name.
    pub path: Option<String>,
    ticket: String,
    /// The relay as the hub connects to it: absolute, HTTP(S), on the server's host.
    pub relay_url: String,
    /// The live room.
    pub room: String,
    /// The ticket only lets this window read.
    pub read_only: bool,
    /// False when the server said no document service saves the room right now (the ticket's
    /// `saver`); absent means true.
    pub saver: bool,
}

impl std::fmt::Debug for LiveSessionFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveSessionFile")
            .field("server", &self.server)
            .field("path", &self.path)
            .field("relay_url", &self.relay_url)
            .field("room", &self.room)
            .field("read_only", &self.read_only)
            .field("saver", &self.saver)
            .finish_non_exhaustive()
    }
}

impl LiveSessionFile {
    /// The ticket the relay admits this window with. Never log it.
    pub fn ticket(&self) -> &str {
        &self.ticket
    }

    /// The access to start with.
    pub fn access(&self) -> Access {
        if self.read_only { Access::Read } else { Access::Edit }
    }

    /// The name the document is shown and opened under (its format comes from it): the server
    /// path's last part, or else the working copy's.
    pub fn file_name(&self, working_copy: &Path) -> String {
        self.path
            .as_deref()
            .and_then(|p| p.rsplit('/').next())
            .filter(|n| !n.is_empty())
            .map(str::to_owned)
            .or_else(|| working_copy.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "document.docx".to_owned())
    }
}

/// Why a session file was not used.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionError {
    /// It could not be read or is not the expected JSON.
    #[error("the live session file could not be read: {0}")]
    Unreadable(String),
    /// A version this build does not read: a newer launcher.
    #[error("the live session file is version {0}, which this WordCraft can't read")]
    Version(u64),
    /// A field is missing or not allowed.
    #[error("the live session file is not usable: {0}")]
    Invalid(&'static str),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Raw {
    v: Option<u64>,
    #[serde(default)]
    server: String,
    path: Option<String>,
    #[serde(default)]
    ticket: String,
    #[serde(default)]
    relay_url: String,
    #[serde(default)]
    room: String,
    #[serde(default)]
    read_only: bool,
    saver: Option<bool>,
}

/// Read and check the session file at `path`.
pub fn read(path: &Path) -> Result<LiveSessionFile, SessionError> {
    let bytes = std::fs::read(path).map_err(|e| SessionError::Unreadable(e.to_string()))?;
    parse(&bytes)
}

/// Check a session file's bytes.
pub fn parse(bytes: &[u8]) -> Result<LiveSessionFile, SessionError> {
    // serde_json's message names a position, never the text, so no ticket ends up in it.
    let raw: Raw = serde_json::from_slice(bytes).map_err(|e| SessionError::Unreadable(e.to_string()))?;
    match raw.v {
        Some(SESSION_VERSION) => {}
        Some(v) => return Err(SessionError::Version(v)),
        None => return Err(SessionError::Invalid("it has no version")),
    }
    if raw.ticket.is_empty() {
        return Err(SessionError::Invalid("it has no ticket"));
    }
    if raw.room.is_empty() {
        return Err(SessionError::Invalid("it names no room"));
    }
    let relay_url = relay_url(&raw.server, &raw.relay_url)?;
    Ok(LiveSessionFile {
        server: raw.server,
        path: raw.path,
        ticket: raw.ticket,
        relay_url,
        room: raw.room,
        read_only: raw.read_only,
        saver: raw.saver != Some(false),
    })
}

/// `scheme://authority` and the rest of an absolute URL.
fn split_url(url: &str) -> Option<(&str, &str, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    (!authority.is_empty() && !authority.contains('@')).then_some((scheme, authority, tail))
}

/// The relay URL the hub connects to: `relay` (absolute, or a path on `server`) with a WebSocket
/// scheme as its HTTP(S) twin, on the server's own host and port only.
pub fn relay_url(server: &str, relay: &str) -> Result<String, SessionError> {
    let (server_scheme, server_host, _) = split_url(server).ok_or(SessionError::Invalid("the server is not a URL"))?;
    if !matches!(server_scheme, "https" | "http") {
        return Err(SessionError::Invalid("the server is not an HTTP(S) address"));
    }
    if relay.is_empty() {
        return Err(SessionError::Invalid("it names no relay"));
    }
    if relay.starts_with('/') && !relay.starts_with("//") {
        return Ok(format!("{server_scheme}://{server_host}{relay}"));
    }
    let (scheme, host, tail) = split_url(relay).ok_or(SessionError::Invalid("the relay is not a URL"))?;
    let scheme = match scheme {
        "wss" | "https" => "https",
        "ws" | "http" => "http",
        _ => return Err(SessionError::Invalid("the relay is not a WebSocket or HTTP(S) address")),
    };
    if !host.eq_ignore_ascii_case(server_host) {
        return Err(SessionError::Invalid("the relay is not on the server's host"));
    }
    Ok(format!("{scheme}://{host}{tail}"))
}

/// What a newer session file says.
#[derive(Clone, PartialEq, Eq)]
pub enum SessionChange {
    /// A fresh ticket for the room: give it to the hub (`Hub::set_token`). Never log it.
    Ticket(String),
    /// The link ends, with this English reason ([`RESTARTED`], [`ACCESS_CHANGED`]).
    Ended(&'static str),
}

impl std::fmt::Debug for SessionChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionChange::Ticket(_) => f.write_str("Ticket(..)"),
            SessionChange::Ended(why) => f.debug_tuple("Ended").field(why).finish(),
        }
    }
}

/// Follows the launcher's rewrites of the session file.
pub struct SessionWatch {
    path: PathBuf,
    current: LiveSessionFile,
    stamp: Option<(SystemTime, u64)>,
    next: Option<Instant>,
    ended: bool,
}

fn stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

impl SessionWatch {
    /// Watch `path`, whose content the window joined with is `current`.
    pub fn new(path: &Path, current: LiveSessionFile) -> SessionWatch {
        SessionWatch { path: path.to_owned(), current, stamp: stamp(path), next: None, ended: false }
    }

    /// Look at the file, at most once every [`WATCH_INTERVAL`] (`now` is the caller's clock).
    /// A file that is gone or can't be read changes nothing; after an end, nothing more is said.
    pub fn poll(&mut self, now: Instant) -> Option<SessionChange> {
        if self.ended || self.next.is_some_and(|next| now < next) {
            return None;
        }
        self.next = Some(now + WATCH_INTERVAL);
        let stamp = stamp(&self.path)?;
        if self.stamp == Some(stamp) {
            return None;
        }
        self.stamp = Some(stamp);
        let new = match read(&self.path) {
            Ok(new) => new,
            Err(e) => {
                log::warn!("live: the refreshed session file was not used: {e}");
                return None;
            }
        };
        let change = if new.room != self.current.room {
            Some(SessionChange::Ended(RESTARTED))
        } else if new.read_only && !self.current.read_only {
            Some(SessionChange::Ended(ACCESS_CHANGED))
        } else if new.ticket != self.current.ticket {
            Some(SessionChange::Ticket(new.ticket.clone()))
        } else {
            None
        };
        self.ended = matches!(change, Some(SessionChange::Ended(_)));
        self.current = new;
        change
    }
}

/// A multi-threaded runtime for the relay link, with one worker: the socket is the only async
/// thing a desktop window owns.
pub fn runtime() -> std::io::Result<Runtime> {
    tokio::runtime::Builder::new_multi_thread().worker_threads(1).thread_name("wordcraft-live").enable_all().build()
}

/// The link while it is being opened, then the native hub.
enum Link {
    Opening(oneshot::Receiver<(NativeHub, String)>),
    Open(NativeHub),
    Closed,
}

/// A [`NativeHub`] that is opened in the background, so the window never waits for the relay.
/// Until the relay answers, it is not connected, takes nothing in and drops what is sent (as a
/// native hub whose link is down does); an unreachable relay is tried again, with the newest
/// ticket, from [`RETRY_FIRST`] up to every [`RETRY_MAX`]. Once open it is that hub.
pub struct DesktopHub {
    link: Link,
    token: Arc<Mutex<String>>,
    space: Option<u32>,
}

impl DesktopHub {
    /// Start opening a link to the relay at `url` with `ticket` on `rt`. `applied` is the
    /// handshake's resume sequence ([`NativeHub::connect`]).
    pub fn open(rt: &Handle, url: &str, ticket: &str, applied: Option<u64>) -> DesktopHub {
        let token = Arc::new(Mutex::new(ticket.to_owned()));
        let (tx, rx) = oneshot::channel();
        let (url, shared) = (url.to_owned(), token.clone());
        rt.spawn(async move {
            let mut wait = RETRY_FIRST;
            loop {
                if tx.is_closed() {
                    return;
                }
                let ticket = shared.lock().map(|t| t.clone()).unwrap_or_default();
                match NativeHub::connect(&url, &ticket, applied).await {
                    Ok(hub) => {
                        // The window gave the hub up meanwhile: dropping it ends the link.
                        let _ = tx.send((hub, ticket));
                        return;
                    }
                    Err(e) => log::warn!("live: the relay could not be reached ({e}); trying again in {wait:?}"),
                }
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(RETRY_MAX);
            }
        });
        DesktopHub { link: Link::Opening(rx), token, space: None }
    }

    /// Whether the relay has answered.
    pub fn opened(&self) -> bool {
        matches!(self.link, Link::Open(_))
    }

    /// Take the link once the relay answered, bringing it up to date.
    fn settle(&mut self) {
        let Link::Opening(rx) = &mut self.link else { return };
        match rx.try_recv() {
            Ok((mut hub, used)) => {
                if let Some(space) = self.space {
                    hub.set_space(space);
                }
                let newest = self.token.lock().map(|t| t.clone()).unwrap_or_default();
                if newest != used {
                    hub.set_token(newest);
                }
                self.link = Link::Open(hub);
            }
            Err(oneshot::error::TryRecvError::Empty) => {}
            Err(oneshot::error::TryRecvError::Closed) => {
                log::warn!("live: the relay link task ended before it connected");
                self.link = Link::Closed;
            }
        }
    }
}

impl Hub for DesktopHub {
    fn send(&mut self, msg: ClientMsg) -> craft_collab::Result<()> {
        self.settle();
        match &mut self.link {
            Link::Open(hub) => hub.send(msg),
            // Dropped, as by a native hub whose link is down: the live session sends what was
            // not confirmed again once it has joined.
            Link::Opening(_) => Ok(()),
            Link::Closed => Err(craft_collab::Error::Hub("the relay link is closed".to_owned())),
        }
    }

    fn poll(&mut self) -> Vec<ServerMsg> {
        self.settle();
        match &mut self.link {
            Link::Open(hub) => hub.poll(),
            Link::Opening(_) | Link::Closed => Vec::new(),
        }
    }

    fn connected(&self) -> bool {
        matches!(&self.link, Link::Open(hub) if hub.connected())
    }

    fn set_token(&mut self, token: String) {
        if let Ok(mut current) = self.token.lock() {
            current.clone_from(&token);
        }
        if let Link::Open(hub) = &mut self.link {
            hub.set_token(token);
        }
    }

    fn set_space(&mut self, space: u32) {
        self.space = Some(space);
        if let Link::Open(hub) = &mut self.link {
            hub.set_space(space);
        }
    }

    fn close(&mut self) {
        // Dropping the hub ends its socket task; dropping the receiver ends an opening one.
        self.link = Link::Closed;
    }
}

/// The hub a desktop window joins `session`'s room with: a [`DesktopHub`] that opens a second
/// link when given a fresh ticket while connected, and switches to it once it has joined
/// ([`Handover`], as the web app does), so a ticket's expiry never drops the link.
pub fn connect(rt: &Handle, session: &LiveSessionFile) -> Handover<DesktopHub> {
    let first = DesktopHub::open(rt, &session.relay_url, session.ticket(), None);
    let (rt, url) = (rt.clone(), session.relay_url.clone());
    Handover::new(first, move |r: &Reopen<'_>| Ok(DesktopHub::open(&rt, &url, r.ticket, r.applied)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(fields: &str) -> Vec<u8> {
        format!(r#"{{"v":1,"server":"https://cloud.example","fileId":7,"path":"/Docs/Plan.docx","ticket":"secret.sig","room":"oc:7","expiresAt":"2026-11-27T12:00:00Z","displayName":"Ann"{fields}}}"#).into_bytes()
    }

    #[test]
    fn a_session_file_names_the_relay_on_the_servers_host() {
        let s = parse(&file(r#","relayUrl":"wss://cloud.example/craft","newField":[1]"#)).unwrap();
        assert_eq!(s.relay_url, "https://cloud.example/craft", "a WebSocket address connects as its HTTP(S) twin");
        assert_eq!(s.access(), Access::Edit);
        assert_eq!(s.file_name(Path::new("/tmp/x/copy.docx")), "Plan.docx");
        let relative = parse(&file(r#","relayUrl":"/craft","readOnly":true"#)).unwrap();
        assert_eq!(relative.relay_url, "https://cloud.example/craft", "a path is on the server");
        assert_eq!(relative.access(), Access::Read);
        assert!(s.saver && !parse(&file(r#","relayUrl":"/craft","saver":false"#)).unwrap().saver, "only a `saver: false` says so");
    }

    #[test]
    fn the_ticket_goes_to_no_other_host() {
        for relay in [
            "wss://relay.evil.example/craft",
            "wss://cloud.example:8443/craft",
            "//evil.example/craft",
            "ftp://cloud.example/",
            "wss://x@cloud.example/",
        ] {
            let got = parse(&file(&format!(r#","relayUrl":"{relay}""#)));
            assert!(matches!(got, Err(SessionError::Invalid(_))), "{relay}: {got:?}");
        }
        assert!(relay_url("https://Cloud.Example", "wss://cloud.example/craft").is_ok(), "hosts compare without case");
    }

    #[test]
    fn another_version_is_refused_and_the_ticket_is_never_shown() {
        let newer = String::from_utf8(file(r#","relayUrl":"/craft""#)).unwrap().replace(r#""v":1"#, r#""v":2"#);
        assert_eq!(parse(newer.as_bytes()), Err(SessionError::Version(2)));
        let s = parse(&file(r#","relayUrl":"/craft""#)).unwrap();
        assert!(!format!("{s:?}").contains("secret"), "Debug leaves the ticket out");
        assert!(!format!("{:?}", SessionChange::Ticket("secret".into())).contains("secret"));
        let broken = parse(br#"{"v":1,"ticket":"secret.sig""#).unwrap_err();
        assert!(!broken.to_string().contains("secret"), "{broken}");
    }

    /// Rewrite the file as the launcher does, a new file renamed over it, a second later.
    fn rewrite(path: &Path, bytes: &[u8], at: SystemTime) {
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, bytes).unwrap();
        std::fs::File::options().write(true).open(&tmp).unwrap().set_modified(at).unwrap();
        std::fs::rename(&tmp, path).unwrap();
    }

    #[test]
    fn a_rewritten_file_brings_a_fresh_ticket_and_a_new_room_ends_the_link() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.json");
        let first = file(r#","relayUrl":"/craft""#);
        std::fs::write(&path, &first).unwrap();
        let mut w = SessionWatch::new(&path, parse(&first).unwrap());
        let t0 = Instant::now();
        assert_eq!(w.poll(t0), None, "nothing changed");
        let fresh = String::from_utf8(first.clone()).unwrap().replace("secret.sig", "fresh.sig");
        rewrite(&path, fresh.as_bytes(), SystemTime::now() + Duration::from_secs(5));
        assert_eq!(w.poll(t0 + Duration::from_millis(10)), None, "not looked at again before the interval");
        assert_eq!(w.poll(t0 + WATCH_INTERVAL), Some(SessionChange::Ticket("fresh.sig".into())));
        assert_eq!(w.poll(t0 + WATCH_INTERVAL * 2), None, "the same file again");

        let readonly = fresh.replace(r#""displayName""#, r#""readOnly":true,"displayName""#);
        rewrite(&path, readonly.as_bytes(), SystemTime::now() + Duration::from_secs(10));
        assert_eq!(w.poll(t0 + WATCH_INTERVAL * 3), Some(SessionChange::Ended(ACCESS_CHANGED)));

        let mut w = SessionWatch::new(&path, parse(fresh.as_bytes()).unwrap());
        rewrite(&path, fresh.replace("oc:7", "oc:7:b").as_bytes(), SystemTime::now() + Duration::from_secs(15));
        assert_eq!(w.poll(t0), Some(SessionChange::Ended(RESTARTED)));
        rewrite(&path, fresh.as_bytes(), SystemTime::now() + Duration::from_secs(20));
        assert_eq!(w.poll(t0 + WATCH_INTERVAL), None, "nothing more after the end");
    }
}

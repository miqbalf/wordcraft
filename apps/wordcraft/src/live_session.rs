//! `--live-session <file>`: the udinCraft launcher opened a cloud file for live editing. The
//! window joins the file's live room with the launcher's ticket (`wordcraft_live::desktop`),
//! starting from the working copy named on the command line, and takes the launcher's ticket
//! refreshes from the same file. From then on it is the same live window as the web app's: the
//! status bar says "Live · saved 12:04", "Reconnecting…" or "Read-only", and Save says the file is
//! saved for everyone (Save As and the exports stay local copies).
//!
//! When live editing can't be had, the working copy opens as an ordinary local document and the
//! status bar says why. That is when the session file can't be used (unreadable, another
//! version, a relay off the server's host), live editing does not start, or the room is never
//! joined: the relay refuses the ticket or ends the link first, or does not answer within
//! [`JOIN_WAIT`]. The window then first writes `<session file>.fallback` (mode 0600,
//! `{"v":1,"reason":"…"}`, an English reason with no secret), before the copy is shown, so the
//! launcher switches to its check-out flow and uploads the saves. Once written it stays, even if
//! the relay answers later: the file is edited as a copy until it is opened again.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use wordcraft_live::desktop::{self, Runtime, SessionChange, SessionWatch};
use wordcraft_ui_egui::live::LiveStatus;
use wordcraft_ui_egui::{WordApp, i18n, tl};

/// The status line when the working copy opened without live editing and the launcher was told
/// to upload it; `{error}` says why.
pub const NOT_LIVE: &str =
    "Live editing isn't available ({error}). You're editing a copy on this computer, which the launcher uploads when you save.";
/// The same when the launcher could not be told: the saves stay on this computer.
pub const NOT_LIVE_LOCAL: &str = "Live editing isn't available ({error}). This is a copy on this computer: changes you save stay here.";
/// The reason when the command line named no working copy.
pub const NO_COPY: &str = "no file was given to edit";
/// The reason when the relay did not let the window join within [`JOIN_WAIT`].
pub const NO_ANSWER: &str = "the live editing service did not answer";
/// How long the window waits for the room before it edits the copy instead.
pub const JOIN_WAIT: Duration = Duration::from_secs(20);

/// A running desktop join: the relay link's runtime and the session file's watch.
pub struct LiveSession {
    /// Kept for the link's socket task; dropped with the window.
    _runtime: Runtime,
    watch: SessionWatch,
    session: PathBuf,
    copy: PathBuf,
    started: Instant,
    /// The room was joined once: from then on a lost link is the live window's to show.
    joined: bool,
    /// Fell back to the copy: nothing more to do.
    done: bool,
}

impl LiveSession {
    /// Join the room the session file at `session` names, starting from `working_copy`. `None`
    /// when the window is not live; it then shows the working copy, if it opened, and why.
    pub fn start(app: &mut WordApp, session: &Path, working_copy: Option<&Path>) -> Option<LiveSession> {
        let Some(copy) = working_copy else {
            not_live(app, session, None, NO_COPY);
            return None;
        };
        match Self::join(app, session, copy) {
            Ok(live) => Some(live),
            Err(error) => {
                log::warn!("live editing did not start: {error}");
                not_live(app, session, Some(copy), &error);
                None
            }
        }
    }

    fn join(app: &mut WordApp, session: &Path, copy: &Path) -> Result<LiveSession, String> {
        let file = desktop::read(session).map_err(|e| e.to_string())?;
        let bytes = wordcraft_engine::io::read_document(copy)?;
        let runtime = desktop::runtime().map_err(|e| e.to_string())?;
        let hub = desktop::connect(runtime.handle(), &file);
        app.start_live(Box::new(hub), &file.room, &bytes, &file.file_name(copy), file.access())?;
        log::info!("live: joining {:?}", file);
        if !file.saver {
            // The room's own state says so too once it is joined (the status bar's line); the
            // server already knows, so say it from the start.
            app.status_long(tl!(wordcraft_ui_egui::live::NOT_SAVING));
        }
        Ok(LiveSession {
            _runtime: runtime,
            watch: SessionWatch::new(session, file),
            session: session.to_owned(),
            copy: copy.to_owned(),
            started: Instant::now(),
            joined: false,
            done: false,
        })
    }

    /// Once a frame (`now` is the caller's clock): pass a refreshed ticket to the link, or end it
    /// when the session changed; until the room is joined, fall back to the copy when it can't be.
    pub fn frame(&mut self, app: &mut WordApp, now: Instant) {
        if self.done {
            return;
        }
        if !self.joined
            && let Some(reason) = self.not_joining(app, now)
        {
            log::warn!("live: the room was not joined: {reason}");
            self.done = true;
            let live_document = app.session.document_id();
            app.live_stop();
            if not_live(app, &self.session, Some(&self.copy), &reason)
                && app.session.document_id() != live_document
                && let Err(e) = app.session.close_document(live_document)
            {
                log::warn!("live: the room's document stayed open: {e}");
            }
            return;
        }
        match self.watch.poll(now) {
            Some(SessionChange::Ticket(ticket)) => {
                app.live_refresh_ticket(ticket);
            }
            Some(SessionChange::Ended(reason)) => {
                log::warn!("live: the session ended: {reason}");
                app.live_end(tl!(reason));
            }
            None => {}
        }
    }

    /// Why the room won't be joined, once that is known; notes a join.
    fn not_joining(&mut self, app: &WordApp, now: Instant) -> Option<String> {
        let Some(link) = app.live_link() else { return Some(NO_ANSWER.to_owned()) };
        match link.status() {
            LiveStatus::Live { .. } | LiveStatus::ReadOnly => {
                self.joined = true;
                None
            }
            LiveStatus::Refused(why) | LiveStatus::Broken(why) => Some(why.clone()),
            LiveStatus::Connecting | LiveStatus::Reconnecting => (now.duration_since(self.started) >= JOIN_WAIT).then(|| NO_ANSWER.to_owned()),
        }
    }
}

/// Tell the launcher this window edits the copy, not the room (see the module documentation),
/// then open the working copy as a local document, if there is one, and say why it is not live.
/// `reason` is in English. False when the copy did not open.
fn not_live(app: &mut WordApp, session: &Path, copy: Option<&Path>, reason: &str) -> bool {
    let told = match signal_fallback(session, reason) {
        Ok(()) => true,
        Err(e) => {
            log::error!("live: the launcher could not be told to upload the copy: {e}");
            false
        }
    };
    if let Some(copy) = copy {
        let name = copy.to_string_lossy();
        if let Err(e) = app.open_from_platform(&name, None) {
            app.report_open_failure(&name, &e);
            return false;
        }
    }
    let line = if told { tl!(NOT_LIVE) } else { tl!(NOT_LIVE_LOCAL) };
    app.status_long(i18n::fmt(line, &[("error", tl!(reason))]));
    copy.is_some()
}

/// The launcher's fallback signal beside the session file.
pub fn fallback_path(session: &Path) -> PathBuf {
    let mut path = session.as_os_str().to_owned();
    path.push(".fallback");
    PathBuf::from(path)
}

/// Write `<session>.fallback`, mode 0600, whole or not at all (a new file renamed into place).
fn signal_fallback(session: &Path, reason: &str) -> std::io::Result<()> {
    let target = fallback_path(session);
    let mut tmp = target.clone().into_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let body = serde_json::to_vec(&serde_json::json!({ "v": 1, "reason": reason }))?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&tmp)?;
    std::io::Write::write_all(&mut file, &body)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, &target)
}

/// Take the working copy out of the files named on the command line: the first one.
pub fn take_working_copy(files: &mut Vec<String>, other_files: &mut Vec<PathBuf>) -> Option<PathBuf> {
    if !files.is_empty() {
        return Some(PathBuf::from(files.remove(0)));
    }
    (!other_files.is_empty()).then(|| other_files.remove(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wordcraft_engine::Session;
    use wordcraft_ui_egui::Services;

    #[test]
    fn a_session_file_of_another_version_opens_the_working_copy_and_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let copy = dir.path().join("Plan.txt");
        std::fs::write(&copy, "Local words").unwrap();
        let session = dir.path().join("live.json");
        let newer = r#"{"v":2,"server":"https://cloud.example","ticket":"t.s","relayUrl":"/craft","room":"oc:1"}"#;
        std::fs::write(&session, newer).unwrap();
        let mut app = WordApp::new(Session::new(wordcraft_doc::Document::new()), Services::default());
        assert!(LiveSession::start(&mut app, &session, Some(&copy)).is_none(), "not live");
        assert!(app.live_link().is_none());
        let text: String = app.session.doc.body.iter().filter_map(|b| b.as_para()).map(|p| p.text.clone()).collect();
        assert!(text.contains("Local words"), "the working copy is shown: {text:?}");
        let status = app.status_msg.as_ref().map(|(s, _)| s.clone()).unwrap_or_default();
        assert!(status.contains("version 2") && status.contains("uploads"), "the status bar says why: {status}");
        assert_eq!(fallback_reason(&session).as_deref().map(|r| r.contains("version 2")), Some(true), "the launcher is told");
    }

    /// The launcher's fallback signal: `{"v":1,"reason":…}`, mode 0600.
    fn fallback_reason(session: &Path) -> Option<String> {
        let path = fallback_path(session);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).ok()?.permissions().mode() & 0o777;
            assert_eq!(mode & 0o077, 0, "only the user reads it: {mode:o}");
        }
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
        assert_eq!(v["v"], 1);
        v["reason"].as_str().map(str::to_owned)
    }

    #[test]
    fn a_relay_that_never_answers_falls_back_to_the_copy_and_tells_the_launcher() {
        let dir = tempfile::tempdir().unwrap();
        let copy = dir.path().join("Plan.txt");
        std::fs::write(&copy, "Local words").unwrap();
        let session = dir.path().join("live.json");
        // Nothing listens on port 9 (discard): the hub keeps trying and never joins.
        let file = r#"{"v":1,"server":"http://127.0.0.1:9","ticket":"t.s","relayUrl":"/craft","room":"oc:1","readOnly":false}"#;
        std::fs::write(&session, file).unwrap();
        let mut app = WordApp::new(Session::new(wordcraft_doc::Document::new()), Services::default());
        let mut live = LiveSession::start(&mut app, &session, Some(&copy)).expect("the join starts");
        assert!(app.live_link().is_some(), "joining");
        live.frame(&mut app, Instant::now());
        assert!(fallback_reason(&session).is_none(), "not before the wait is over");
        let room_document = app.session.document_id();
        live.frame(&mut app, Instant::now() + JOIN_WAIT);
        assert_eq!(fallback_reason(&session).as_deref(), Some(NO_ANSWER), "the launcher is told");
        assert!(app.live_link().is_none(), "the link stopped");
        assert!(!app.session.is_open(room_document), "the room's document is gone");
        let text: String = app.session.doc.body.iter().filter_map(|b| b.as_para()).map(|p| p.text.clone()).collect();
        assert!(text.contains("Local words"), "the working copy is shown: {text:?}");
        let status = app.status_msg.as_ref().map(|(s, _)| s.clone()).unwrap_or_default();
        assert!(status.contains(NO_ANSWER), "the status bar says why: {status}");
    }
}

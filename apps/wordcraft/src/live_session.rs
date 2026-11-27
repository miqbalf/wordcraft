//! `--live-session <file>`: the udinCraft launcher opened a cloud file for live editing. The
//! window joins the file's live room with the launcher's ticket (`wordcraft_live::desktop`),
//! starting from the working copy named on the command line, and takes the launcher's ticket
//! refreshes from the same file. From then on it is the same live window as the web app's: the
//! status bar says "Live · saved 12:04", "Reconnecting…" or "Read-only", and Save says the file is
//! saved for everyone (Save As and the exports stay local copies).
//!
//! When the session file can't be used (unreadable, another version, a relay off the server's
//! host) or live editing does not start, the working copy opens as an ordinary local document
//! and the status bar says so: its saves stay on this computer.

use std::path::{Path, PathBuf};
use std::time::Instant;

use wordcraft_live::desktop::{self, Runtime, SessionChange, SessionWatch};
use wordcraft_ui_egui::{WordApp, i18n, tl};

/// The status line when the working copy opened without live editing; `{error}` says why.
pub const NOT_LIVE: &str = "Live editing isn't available here ({error}). This is a copy on this computer: changes you save stay here.";
/// The reason when the command line named no working copy.
pub const NO_COPY: &str = "no file was given to edit";

/// A running desktop join: the relay link's runtime and the session file's watch.
pub struct LiveSession {
    /// Kept for the link's socket task; dropped with the window.
    _runtime: Runtime,
    watch: SessionWatch,
}

impl LiveSession {
    /// Join the room the session file at `session` names, starting from `working_copy`. `None`
    /// when the window is not live; it then shows the working copy, if it opened, and why.
    pub fn start(app: &mut WordApp, session: &Path, working_copy: Option<&Path>) -> Option<LiveSession> {
        let Some(copy) = working_copy else {
            not_live(app, None, tl!(NO_COPY));
            return None;
        };
        match Self::join(app, session, copy) {
            Ok(live) => Some(live),
            Err(error) => {
                log::warn!("live editing did not start: {error}");
                not_live(app, Some(copy), &error);
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
        Ok(LiveSession { _runtime: runtime, watch: SessionWatch::new(session, file) })
    }

    /// Once a frame: pass a refreshed ticket to the link, or end it when the session changed.
    pub fn frame(&mut self, app: &mut WordApp) {
        match self.watch.poll(Instant::now()) {
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
}

/// Open the working copy as a local document, if there is one, and say why it is not live.
fn not_live(app: &mut WordApp, copy: Option<&Path>, error: &str) {
    if let Some(copy) = copy {
        let name = copy.to_string_lossy();
        if let Err(e) = app.open_from_platform(&name, None) {
            app.report_open_failure(&name, &e);
            return;
        }
    }
    app.status_long(i18n::fmt(tl!(NOT_LIVE), &[("error", error)]));
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
        assert!(status.contains("version 2") && status.contains("stay here"), "the status bar says why: {status}");
    }
}

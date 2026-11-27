//! Grammar check and AI writing help on the desktop (DG1): the same craft_office assist as the web
//! app, called over HTTPS with the udinCraft launcher's short-lived assist token instead of a
//! browser session. The app never sees the launcher's app password.
//!
//! The token comes from a file the launcher writes (mode 0600) and renews while it runs:
//! - the live session file (`--live-session`), under `"assist"`, for a cloud file opened live;
//! - otherwise the launcher's account-wide `assist/assist.json` in its data folder, for every
//!   other file (a cloud check-out, or a file on this computer: the owner's "always on when
//!   signed in"). Both read `{"server": "https://…", "assist": {"token", "expiresAt", "proof", "ai"}}`.
//!
//! With no usable token (no file, expired, refused by the server) the window says to sign in
//! through the launcher ([`AssistIn::SignedOut`]) and sends nothing. Requests go only to the
//! file's `server`, which must be https (http only on this computer). Neither the token nor the
//! text is ever logged; a failure logs its code.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use wordcraft_ui_egui::assist::{AiTask, AssistHost, AssistIn, AssistInbox, AssistOut, wire};

/// The token routes, under the server's base URL. `index.php` works with and without pretty URLs.
pub const PROOF_PATH: &str = "/index.php/apps/craft_office/assist/v1/proof/check";
pub const IMPROVE_PATH: &str = "/index.php/apps/craft_office/assist/v1/ai/improve";
pub const SUMMARIZE_PATH: &str = "/index.php/apps/craft_office/assist/v1/ai/summarize";
/// The header the token goes in (never the URL, so it stays out of access logs).
pub const TOKEN_HEADER: &str = "X-Craft-Assist-Token";
/// How often the token file is read again: the launcher renews it well before it expires.
const RELOAD: Duration = Duration::from_secs(2);
/// Largest answer read: a summary is at most 200 KB of text, escaped.
const MAX_ANSWER_BYTES: u64 = 1024 * 1024;
/// The page's timeouts (`assist.js`): a little over the server's own to its services.
const PROOF_TIMEOUT: Duration = Duration::from_secs(40);
const IMPROVE_TIMEOUT: Duration = Duration::from_secs(95);
const SUMMARIZE_TIMEOUT: Duration = Duration::from_secs(125);

/// The launcher's account-wide assist file, in its data folder: the per-user local data folder
/// (`~/Library/Application Support`, `%LOCALAPPDATA%`, `$XDG_DATA_HOME` or `~/.local/share`, as the
/// launcher's `dirs::data_local_dir`) and the launcher's own folder name in it.
pub fn account_file() -> Option<PathBuf> {
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let (data, dir) = if cfg!(target_os = "macos") {
        (env("HOME")?.join("Library/Application Support"), "ArtCraft Launcher")
    } else if cfg!(windows) {
        (env("LOCALAPPDATA")?, "ArtCraft Launcher")
    } else {
        (env("XDG_DATA_HOME").filter(|p| p.is_absolute()).or_else(|| env("HOME").map(|h| h.join(".local/share")))?, "artcraft-launcher")
    };
    Some(data.join(dir).join("assist").join("assist.json"))
}

/// A usable token and where to use it. No `Debug`: the token is a secret.
#[derive(Clone, PartialEq)]
pub struct Creds {
    pub server: String,
    pub token: String,
    pub proof: bool,
    pub ai: bool,
}

/// The token in `json` (a session file or the account file), if it is one this app can use at
/// `now`: a server on https (http only on this computer), a token, and an expiry still ahead.
pub fn creds_from(json: &str, now: chrono::DateTime<chrono::Utc>) -> Option<Creds> {
    let v: Value = serde_json::from_str(json).ok()?;
    let server = v.get("server")?.as_str()?.trim_end_matches('/').to_string();
    if !server_allowed(&server) {
        return None;
    }
    let a = v.get("assist")?;
    let token = a.get("token")?.as_str()?.to_string();
    if token.is_empty() || token.len() > 4096 || !token.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)) {
        return None;
    }
    let expires = chrono::DateTime::parse_from_rfc3339(a.get("expiresAt")?.as_str()?).ok()?;
    if expires.with_timezone(&chrono::Utc) <= now {
        return None;
    }
    let flag = |k: &str| a.get(k).and_then(Value::as_bool).unwrap_or(false);
    Some(Creds { server, token, proof: flag("proof"), ai: flag("ai") })
}

/// https with a host, or http to this computer (a test server, the dev stack). No user info,
/// query or fragment.
pub fn server_allowed(server: &str) -> bool {
    let (rest, local_only) = if let Some(r) = server.strip_prefix("https://") {
        (r, false)
    } else if let Some(r) = server.strip_prefix("http://") {
        (r, true)
    } else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or_default();
    if authority.is_empty() || authority.contains('@') || server.contains(['?', '#', ' ', '\\']) {
        return false;
    }
    let host = if let Some(v6) = authority.strip_prefix('[') {
        v6.split(']').next().unwrap_or_default()
    } else {
        authority.split(':').next().unwrap_or_default()
    };
    !host.is_empty() && (!local_only || matches!(host, "localhost" | "127.0.0.1" | "::1"))
}

/// What the window was last told.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Told {
    On { proof: bool, ai: bool },
    SignedOut,
}

/// The shared state of the host: the token in use and the ones the server refused.
#[derive(Default)]
struct Shared {
    creds: Option<Creds>,
    /// Tokens the server answered 401 to: never used again, even if the file still holds them.
    refused: Vec<String>,
}

/// The desktop assist host: give [`AssistNet::host`] to the app's services, and call
/// [`AssistNet::frame`] once a frame.
pub struct AssistNet {
    files: Vec<PathBuf>,
    shared: Arc<Mutex<Shared>>,
    inbox: AssistInbox,
    rx: Receiver<AssistIn>,
    tx: Sender<AssistIn>,
    ctx: egui::Context,
    told: Option<Told>,
    last_read: Option<Instant>,
}

impl AssistNet {
    /// A host that reads its token from the first of `files` that holds a usable one.
    pub fn new(files: Vec<PathBuf>, ctx: egui::Context) -> AssistNet {
        let (tx, rx) = channel();
        AssistNet { files, shared: Arc::default(), inbox: AssistInbox::default(), rx, tx, ctx, told: None, last_read: None }
    }

    /// The [`AssistHost`] for the app's services: its `send` posts to the server on a thread of
    /// its own and the answer comes back through [`AssistNet::frame`].
    pub fn host(&self) -> AssistHost {
        let shared = self.shared.clone();
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        AssistHost {
            send: Box::new(move |out: &AssistOut| {
                let creds = shared.lock().unwrap_or_else(PoisonError::into_inner).creds.clone().ok_or_else(|| wire::SIGNED_OUT.to_string())?;
                let (shared, tx, ctx, out) = (shared.clone(), tx.clone(), ctx.clone(), out.clone());
                std::thread::Builder::new()
                    .name("assist".into())
                    .spawn(move || {
                        let answer = ask(&creds, &out);
                        if matches!(&answer, AssistIn::ProofFailed { error, .. } | AssistIn::AiFailed { error, .. } if error == wire::SIGNED_OUT) {
                            let mut s = shared.lock().unwrap_or_else(PoisonError::into_inner);
                            s.refused.push(creds.token.clone());
                            s.creds = None;
                        }
                        // The window may have closed: nothing waits then.
                        let _ = tx.send(answer);
                        ctx.request_repaint();
                    })
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            }),
            inbox: self.inbox.clone(),
        }
    }

    /// Once a frame: re-reads the token file every [`RELOAD`], tells the window when assist
    /// turns on, changes or signs out, and hands it the answers that came.
    pub fn frame(&mut self, now: Instant) {
        if self.last_read.is_none_or(|t| now.duration_since(t) >= RELOAD) {
            self.last_read = Some(now);
            self.reload(chrono::Utc::now());
            self.ctx.request_repaint_after(RELOAD);
        }
        let mut inbox = self.inbox.borrow_mut();
        inbox.extend(self.rx.try_iter());
        let creds = self.shared.lock().unwrap_or_else(PoisonError::into_inner).creds.clone();
        let told = creds.map_or(Told::SignedOut, |c| Told::On { proof: c.proof, ai: c.ai });
        if self.told != Some(told) {
            self.told = Some(told);
            inbox.push(match told {
                Told::On { proof, ai } => AssistIn::Enabled { proof, ai },
                Told::SignedOut => AssistIn::SignedOut,
            });
        }
    }

    fn reload(&mut self, now: chrono::DateTime<chrono::Utc>) {
        let found = self.files.iter().find_map(|f| read(f, now));
        let mut s = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
        s.creds = found.filter(|c| !s.refused.contains(&c.token));
        // Only the latest refusals matter: the launcher renews into a new token.
        let keep = s.refused.len().saturating_sub(8);
        s.refused.drain(..keep);
    }
}

fn read(file: &Path, now: chrono::DateTime<chrono::Utc>) -> Option<Creds> {
    // A token file is small; a large one is not the launcher's.
    let meta = std::fs::metadata(file).ok()?;
    if meta.len() > 64 * 1024 {
        return None;
    }
    creds_from(&std::fs::read_to_string(file).ok()?, now)
}

/// One request to the server, answered as the page would answer it.
pub fn ask(creds: &Creds, out: &AssistOut) -> AssistIn {
    let (id, path, timeout, language, text) = match out {
        AssistOut::ProofCheck { id, language, text } => (id, PROOF_PATH, PROOF_TIMEOUT, language, text),
        AssistOut::Ai { id, task: AiTask::Improve, language, text } => (id, IMPROVE_PATH, IMPROVE_TIMEOUT, language, text),
        AssistOut::Ai { id, task: AiTask::Summarize, language, text } => (id, SUMMARIZE_PATH, SUMMARIZE_TIMEOUT, language, text),
    };
    let proof = matches!(out, AssistOut::ProofCheck { .. });
    let fail = |error: &str| {
        if error != wire::SIGNED_OUT {
            log::warn!("assist: request failed: {error}");
        }
        if proof {
            AssistIn::ProofFailed { id: id.clone(), error: error.to_string() }
        } else {
            AssistIn::AiFailed { id: id.clone(), error: error.to_string() }
        }
    };
    let body = match post(&format!("{}{path}", creds.server), &creds.token, timeout, &json!({"language": language, "text": text}).to_string()) {
        Ok((200, body)) => body,
        Ok((status, _)) => return fail(wire::code_for_status(status)),
        Err(()) => return fail(wire::UNAVAILABLE),
    };
    let Ok(v) = serde_json::from_str::<Value>(&body) else { return fail(wire::UNAVAILABLE) };
    if proof {
        match wire::proof_reply(&v) {
            Some(reply) => AssistIn::ProofResult { id: id.clone(), reply },
            None => fail(wire::UNAVAILABLE),
        }
    } else {
        match wire::ai_text(&v) {
            Ok(text) => AssistIn::AiResult { id: id.clone(), text },
            Err(code) => fail(code),
        }
    }
}

/// POSTs `body` as JSON with the token header; the status and, for a 200, the body. No redirect
/// is followed (the token must not travel to another address). Errors carry nothing: the caller
/// logs only a code.
fn post(url: &str, token: &str, timeout: Duration, body: &str) -> Result<(u16, String), ()> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(timeout)).http_status_as_error(false).max_redirects(0).build().into();
    let mut res = agent
        .post(url)
        .header(TOKEN_HEADER, token)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .send(body)
        .map_err(|_| ())?;
    let status = res.status().as_u16();
    if status != 200 {
        return Ok((status, String::new()));
    }
    let text = res.body_mut().with_config().limit(MAX_ANSWER_BYTES).read_to_string().map_err(|_| ())?;
    Ok((status, text))
}

#[cfg(test)]
mod tests;

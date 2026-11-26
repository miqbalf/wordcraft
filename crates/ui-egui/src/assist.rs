//! Grammar check and AI writing help through the page that hosts WordCraft (`wordcraft:proof-check`,
//! `wordcraft:ai`): the types of the messages, the per-frame pump and the state of the one AI
//! request that waits. The page holds the keys and the routes; the desktop app sets no
//! [`AssistHost`], so everything here stays off there (Ruling R10).

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use egui::Context;
use wordcraft_engine::proof_remote::{ProofFailure, ProofReply};

use super::WordApp;

/// What an AI request asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AiTask {
    Improve,
    Summarize,
}

/// A message to the page.
#[derive(Clone, Debug, PartialEq)]
pub enum AssistOut {
    ProofCheck { id: String, language: String, text: String },
    Ai { id: String, task: AiTask, language: String, text: String },
}

/// A message from the page, already checked by the host's parser.
#[derive(Clone, Debug, PartialEq)]
pub enum AssistIn {
    Enabled { proof: bool, ai: bool },
    ProofResult { id: String, reply: ProofReply },
    ProofFailed { id: String, error: String },
    AiResult { id: String, text: String },
    AiFailed { id: String, error: String },
}

/// What the page's messages leave for the next frame.
pub type AssistInbox = Rc<RefCell<Vec<AssistIn>>>;

/// The host's side: how to post a message, and where its answers wait.
pub struct AssistHost {
    pub send: Box<dyn Fn(&AssistOut) -> Result<(), String>>,
    pub inbox: AssistInbox,
}

/// What the page says it can do, and the AI request that waits.
#[derive(Default)]
pub struct AssistState {
    pub proof: bool,
    pub ai: bool,
    /// The last AI request id handed out.
    next_ai: u64,
    /// Tells this page load's ids from an earlier one's: a reloaded app must never take the old
    /// page's answer for its own request.
    session: Option<String>,
    /// The AI request whose answer is wanted; any other answer is a cancelled or late one.
    waiting: Option<String>,
    /// The answer to the waiting request, until it is taken.
    answer: Option<(String, Result<String, String>)>,
    /// The Improve with AI or Summary modal, while it is open.
    pub(crate) dialog: Option<crate::assist_dialog::AssistDialog>,
}

impl WordApp {
    /// One frame of the pump at `now` ms: see [`WordApp::assist_frame`].
    pub(crate) fn assist_frame_at(&mut self, ctx: &Context, now: f64) {
        let Some(host) = &self.services.assist else { return };
        let inbox = host.inbox.clone();
        let incoming: Vec<AssistIn> = std::mem::take(&mut *inbox.borrow_mut());
        for message in incoming {
            self.assist_take(message, now);
        }
        if !self.session.proof_remote.enabled() {
            return;
        }
        let poll = self.session.proof_poll(now);
        for request in poll.send {
            let out = AssistOut::ProofCheck { id: request.id.clone(), language: request.language, text: request.text };
            let sent = match &self.services.assist {
                Some(host) => (host.send)(&out),
                None => Err("no host".to_string()),
            };
            if let Err(e) = sent {
                log::warn!("grammar check not sent: {e}");
                self.session.proof_receive(&request.id, Err(ProofFailure::Unavailable), now);
            }
        }
        if let Some(wake) = poll.wake_in_ms {
            ctx.request_repaint_after(Duration::from_secs_f64(wake.max(0.0) / 1000.0));
        }
    }

    /// One message from the page.
    fn assist_take(&mut self, message: AssistIn, now: f64) {
        match message {
            AssistIn::Enabled { proof, ai } => {
                self.assist.proof = proof;
                self.assist.ai = ai;
                let was = self.session.proof_remote.enabled();
                self.session.proof_remote.set_enabled(proof);
                if was && !proof {
                    // Turning it off drops the remote results; a poll rebuilds the overlays
                    // without them (it sends nothing while off).
                    self.session.proof_poll(now);
                }
                if !ai {
                    self.assist.waiting = None;
                    self.assist.answer = None;
                }
            }
            AssistIn::ProofResult { id, reply } => self.session.proof_receive(&id, Ok(reply), now),
            AssistIn::ProofFailed { id, error } => self.session.proof_receive(&id, Err(ProofFailure::from_code(&error)), now),
            AssistIn::AiResult { id, text } => self.assist_answered(id, Ok(text)),
            AssistIn::AiFailed { id, error } => self.assist_answered(id, Err(error)),
        }
    }

    /// Keeps an AI answer only when it is for the request that waits.
    fn assist_answered(&mut self, id: String, answer: Result<String, String>) {
        if self.assist.waiting.as_deref() == Some(id.as_str()) {
            self.assist.waiting = None;
            self.assist.answer = Some((id, answer));
        }
    }

    /// Drains the page's answers, lets the remote proof source send what is due and asks to be
    /// woken when more will be.
    pub(crate) fn assist_frame(&mut self, ctx: &Context) {
        self.assist_frame_at(ctx, super::clock::now_ms());
    }

    /// The page offers AI writing help (and this is not the desktop app).
    pub fn assist_ai_on(&self) -> bool {
        self.services.assist.is_some() && self.assist.ai
    }

    /// Sends an AI request; returns its id. A newer request replaces the one that waits: the
    /// older one's answer is dropped when it comes.
    pub fn assist_ask(&mut self, task: AiTask, language: String, text: String) -> Result<String, String> {
        if !self.assist_ai_on() {
            return Err("AI help is not available here".to_string());
        }
        // The request that waited is replaced, even when the new one never leaves.
        self.assist.waiting = None;
        self.assist.answer = None;
        let session = self
            .assist
            .session
            .get_or_insert_with(|| {
                let ms = web_time::SystemTime::now().duration_since(web_time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
                format!("{ms:x}")
            })
            .clone();
        let Some(host) = &self.services.assist else { return Err("AI help is not available here".to_string()) };
        let id = format!("ai-{session}-{}", self.assist.next_ai + 1);
        (host.send)(&AssistOut::Ai { id: id.clone(), task, language, text })?;
        self.assist.next_ai += 1;
        self.assist.waiting = Some(id.clone());
        Ok(id)
    }

    /// The answer to request `id`, once it came (the error is the page's code). Taken once.
    pub fn assist_take_answer(&mut self, id: &str) -> Option<Result<String, String>> {
        match self.assist.answer.take() {
            Some((got, answer)) if got == id => Some(answer),
            other => {
                self.assist.answer = other;
                None
            }
        }
    }
}

#[cfg(test)]
mod tests;

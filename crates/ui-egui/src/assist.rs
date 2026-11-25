//! Grammar check and AI writing help through the page that hosts WordCraft (`wordcraft:proof-check`,
//! `wordcraft:ai`): the types of the messages, the per-frame pump and the state of the one AI
//! request that waits. The page holds the keys and the routes; the desktop app sets no
//! [`AssistHost`], so everything here stays off there (Ruling R10).

use std::cell::RefCell;
use std::rc::Rc;

use egui::Context;
use wordcraft_engine::proof_remote::ProofReply;

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
}

impl WordApp {
    /// One frame of the pump at `now` ms: see [`WordApp::assist_frame`].
    pub(crate) fn assist_frame_at(&mut self, _ctx: &Context, _now: f64) {}

    /// Drains the page's answers, lets the remote proof source send what is due and asks to be
    /// woken when more will be.
    pub(crate) fn assist_frame(&mut self, ctx: &Context) {
        self.assist_frame_at(ctx, super::clock::now_ms());
    }

    /// The page offers AI writing help (and this is not the desktop app).
    pub fn assist_ai_on(&self) -> bool {
        false
    }

    /// Sends an AI request; returns its id. A newer request replaces the one that waits.
    pub(crate) fn assist_ask(&mut self, _task: AiTask, _language: String, _text: String) -> Result<String, String> {
        Err(String::new())
    }

    /// The answer to request `id`, once it came (the error is the page's code).
    pub(crate) fn assist_take_answer(&mut self, _id: &str) -> Option<Result<String, String>> {
        None
    }
}

#[cfg(test)]
mod tests;

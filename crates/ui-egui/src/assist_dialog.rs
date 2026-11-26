//! Improve with AI and Summarize Document (stub for the red tests).

use serde_json::Value;

use crate::WordApp;

pub(crate) const MODAL_ID: &str = "wordcraft.assistDialog";
pub(crate) const ASKING: &str = "";
pub(crate) const BUSY: &str = "";
pub(crate) const UNAVAILABLE: &str = "";
pub(crate) const TOO_LARGE: &str = "";
pub(crate) const DISABLED: &str = "";
pub(crate) const BAD_REQUEST: &str = "";
pub(crate) const TRUNCATED: &str = "";

pub(crate) struct AssistDialog;

impl AssistDialog {
    pub(crate) fn summary(_: &Value, _: String) -> AssistDialog {
        AssistDialog
    }
    pub(crate) fn note(&self) -> Option<&'static str> {
        None
    }
}

pub(crate) fn show(_: &mut WordApp, _: &egui::Context) {}

#[cfg(test)]
mod tests;

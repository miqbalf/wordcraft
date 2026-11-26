//! Improve with AI and Summarize Document: the modal that asks the host page's AI, shows what it
//! answers, and changes the document only when the user says so (spec decision 6).
//!
//! `ui.improve` sends the selection (`review.improveSource`), `ui.summarize` the document
//! (`review.summarySource`). A refusal of either source opens the modal with the refusal and sends
//! nothing. Replace runs `review.improveApply` (one undo step); Insert at Cursor runs
//! `text.insert` with each line a paragraph. Cancel, Close and Esc drop the request, so its
//! answer, when it comes, changes nothing.

use egui::{Context, Id, Ui};
use serde_json::{Value, json};

use crate::WordApp;
use crate::assist::AiTask;

/// The modal's egui id (its area's).
pub(crate) const MODAL_ID: &str = "wordcraft.assistDialog";

pub(crate) const IMPROVE_TITLE: &str = "Improve with AI";
pub(crate) const SUMMARY_TITLE: &str = "Summary";
pub(crate) const ASKING: &str = "Asking the AI…";
pub(crate) const BUSY: &str = "Too many requests right now. Try again in a minute.";
pub(crate) const UNAVAILABLE: &str = "The AI service is not available right now. Try again later.";
pub(crate) const TOO_LARGE: &str = "This text is too long for the AI service.";
pub(crate) const DISABLED: &str = "AI writing help is turned off on this server.";
pub(crate) const BAD_REQUEST: &str = "The AI service could not read this request.";
pub(crate) const TRUNCATED: &str = "Only the first 200 KB of the document was summarized.";
const OFF: &str = "AI writing help is not available here.";

/// The sentence for the page's error `code`; any code the app does not know is "unavailable".
pub(crate) fn error_sentence(code: &str) -> &'static str {
    match code {
        "busy" => BUSY,
        "too_large" => TOO_LARGE,
        "disabled" => DISABLED,
        "bad_request" => BAD_REQUEST,
        _ => UNAVAILABLE,
    }
}

/// The range Improve replaces, as `review.improveSource` gave it.
struct Source {
    anchor: Value,
    focus: Value,
    original: String,
}

enum Kind {
    /// Improve: the source, or `None` when the selection was refused before asking.
    Improve(Option<Source>),
    Summary {
        truncated: bool,
    },
}

enum Phase {
    /// The request with this id waits for its answer.
    Waiting(String),
    /// The AI's text.
    Ready(String),
    /// Why there is nothing to show (English, translated when drawn).
    Failed(String),
}

/// The open modal.
pub(crate) struct AssistDialog {
    kind: Kind,
    phase: Phase,
    /// The answer just came: the first button takes the keyboard on the next frame.
    focus_answer: bool,
}

impl AssistDialog {
    /// A Summary modal for `review.summarySource`'s answer `source`, waiting for request `id`.
    pub(crate) fn summary(source: &Value, id: String) -> AssistDialog {
        let truncated = source.get("truncated").and_then(Value::as_bool).unwrap_or(false);
        AssistDialog { kind: Kind::Summary { truncated }, phase: Phase::Waiting(id), focus_answer: false }
    }

    /// The note under the summary: the document was cut to the limit.
    pub(crate) fn note(&self) -> Option<&'static str> {
        matches!(self.kind, Kind::Summary { truncated: true }).then_some(TRUNCATED)
    }

    fn failed(kind: Kind, why: impl Into<String>) -> AssistDialog {
        AssistDialog { kind, phase: Phase::Failed(why.into()), focus_answer: false }
    }
}

impl WordApp {
    /// `ui.improve`: ask the AI to improve the selection (or the caret's paragraph).
    pub(crate) fn assist_improve(&mut self) -> Result<Value, String> {
        if !self.assist_ai_on() {
            return Err(OFF.to_string());
        }
        let source = match self.session.run("review.improveSource", &json!({})) {
            Ok(v) => v,
            Err(e) => {
                let why = e.to_string();
                self.assist.dialog = Some(AssistDialog::failed(Kind::Improve(None), why.clone()));
                return Ok(json!({"dialog": "improve", "refused": why}));
            }
        };
        let text = source.get("text").and_then(Value::as_str).unwrap_or_default().to_string();
        let language = source.get("language").and_then(Value::as_str).unwrap_or("auto").to_string();
        let src = || Source {
            anchor: source.get("anchor").cloned().unwrap_or(Value::Null),
            focus: source.get("focus").cloned().unwrap_or(Value::Null),
            original: text.clone(),
        };
        let (ready, failed) = (Kind::Improve(Some(src())), Kind::Improve(Some(src())));
        let waiting = |id| AssistDialog { kind: ready, phase: Phase::Waiting(id), focus_answer: false };
        self.assist_open(AiTask::Improve, language, text.clone(), "improve", waiting, failed)
    }

    /// `ui.summarize`: ask the AI to summarize the document.
    pub(crate) fn assist_summarize(&mut self) -> Result<Value, String> {
        if !self.assist_ai_on() {
            return Err(OFF.to_string());
        }
        let source = match self.session.run("review.summarySource", &json!({})) {
            Ok(v) => v,
            Err(e) => {
                let why = e.to_string();
                self.assist.dialog = Some(AssistDialog::failed(Kind::Summary { truncated: false }, why.clone()));
                return Ok(json!({"dialog": "summary", "refused": why}));
            }
        };
        let text = source.get("text").and_then(Value::as_str).unwrap_or_default().to_string();
        let language = source.get("language").and_then(Value::as_str).unwrap_or("auto").to_string();
        let truncated = source.get("truncated").and_then(Value::as_bool).unwrap_or(false);
        let waiting = |id| AssistDialog::summary(&source, id);
        self.assist_open(AiTask::Summarize, language, text, "summary", waiting, Kind::Summary { truncated })
    }

    /// Sends the request and opens the modal waiting for it (or showing why it was not sent).
    fn assist_open(
        &mut self,
        task: AiTask,
        language: String,
        text: String,
        name: &str,
        waiting: impl FnOnce(String) -> AssistDialog,
        kind: Kind,
    ) -> Result<Value, String> {
        match self.assist_ask(task, language, text) {
            Ok(id) => {
                self.assist.dialog = Some(waiting(id.clone()));
                Ok(json!({"dialog": name, "id": id}))
            }
            Err(e) => {
                log::warn!("AI request not sent: {e}");
                self.assist.dialog = Some(AssistDialog::failed(kind, UNAVAILABLE));
                Ok(json!({"dialog": name, "error": "unavailable"}))
            }
        }
    }
}

/// What the user chose this frame.
enum Choice {
    Replace,
    Copy,
    Insert,
    Close,
}

/// Takes the answer the modal waits for, if it came.
fn poll(app: &mut WordApp, d: &mut AssistDialog) {
    let Phase::Waiting(id) = &d.phase else { return };
    let Some(answer) = app.assist_take_answer(id) else { return };
    d.phase = match answer {
        Ok(text) => {
            d.focus_answer = true;
            Phase::Ready(text)
        }
        Err(code) => Phase::Failed(error_sentence(&code).to_string()),
    };
}

/// Draws the modal while it is open and does what its buttons say.
pub(crate) fn show(app: &mut WordApp, ctx: &Context) {
    let Some(mut d) = app.assist.dialog.take() else { return };
    poll(app, &mut d);
    let resp = egui::Modal::new(Id::unique(MODAL_ID)).show(ctx, |ui| body(ui, &mut d));
    // egui's Modal blocks the window behind it, but not the document's keyboard.
    crate::dialogs::release_keyboard(app, ctx);
    let closing = resp.should_close();
    let mut choice = resp.inner;
    if choice.is_none() && closing {
        choice = Some(Choice::Close);
    }
    match choice {
        None => app.assist.dialog = Some(d),
        Some(Choice::Close) => close(app),
        Some(Choice::Copy) => {
            if let Phase::Ready(text) = &d.phase {
                ctx.copy_text(text.clone());
                app.status(tl!("Copied"));
            }
            app.assist.dialog = Some(d);
        }
        Some(Choice::Replace) => {
            let applied = match (&d.kind, &d.phase) {
                (Kind::Improve(Some(src)), Phase::Ready(proposal)) => Some(
                    app.run("review.improveApply", json!({"anchor": src.anchor, "focus": src.focus, "original": src.original, "proposal": proposal})),
                ),
                _ => None,
            };
            match applied {
                Some(Ok(_)) => close(app),
                // The refusal stays in the modal; nothing was changed.
                Some(Err(e)) => {
                    d.phase = Phase::Failed(e);
                    app.assist.dialog = Some(d);
                }
                None => app.assist.dialog = Some(d),
            }
        }
        Some(Choice::Insert) => {
            let inserted = match &d.phase {
                Phase::Ready(text) => Some(app.run("text.insert", json!({"text": text, "paragraphs": true}))),
                _ => None,
            };
            match inserted {
                Some(Ok(_)) => close(app),
                Some(Err(e)) => {
                    d.phase = Phase::Failed(e);
                    app.assist.dialog = Some(d);
                }
                None => app.assist.dialog = Some(d),
            }
        }
    }
}

/// Closes the modal: the request it waited for is dropped, so a late answer changes nothing.
fn close(app: &mut WordApp) {
    app.assist.dialog = None;
    app.assist_forget();
    app.canvas.want_focus = true;
}

/// A read-only box of `text`, named `label` for assistive technology.
fn read_only(ui: &mut Ui, label: &str, text: &str, salt: &str) {
    let caption = ui.label(egui::RichText::new(label).strong());
    let mut shown: &str = text;
    let resp = egui::ScrollArea::vertical()
        .id_salt(salt)
        .max_height(160.0)
        .show(ui, |ui| ui.add(egui::TextEdit::multiline(&mut shown).id_salt(salt).desired_rows(4).desired_width(f32::INFINITY)))
        .inner;
    let resp = resp.labelled_by(caption.id);
    ui.ctx().accesskit_node_builder(resp.id, |node| {
        node.set_label(label.to_string());
        node.set_read_only();
    });
}

/// A labelled button, `on` or greyed out.
fn button(ui: &mut Ui, label: &str, on: bool, primary: bool) -> egui::Response {
    let b = if primary && on { crate::widgets::primary_button(ui, label) } else { egui::Button::new(label.to_string()) };
    ui.add_enabled(on, b)
}

/// The modal's contents; `Some` when a button was pressed.
fn body(ui: &mut Ui, d: &mut AssistDialog) -> Option<Choice> {
    ui.set_width(520.0);
    let title = match d.kind {
        Kind::Improve(_) => IMPROVE_TITLE,
        Kind::Summary { .. } => SUMMARY_TITLE,
    };
    ui.label(egui::RichText::new(tl!(title)).font(crate::theme::semibold(14.0)));
    ui.add_space(6.0);
    if let Kind::Improve(Some(src)) = &d.kind {
        read_only(ui, tl!("Original"), &src.original, "assist.original");
        ui.add_space(4.0);
    }
    // The waiting and error lines are a live region: the change is announced.
    let status = match &d.phase {
        Phase::Waiting(_) => Some(
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(tl!(ASKING))
            })
            .inner,
        ),
        Phase::Failed(why) => Some(ui.label(tl!(why.as_str()))),
        Phase::Ready(text) => {
            match d.kind {
                Kind::Improve(_) => read_only(ui, tl!("Proposal"), text, "assist.proposal"),
                Kind::Summary { .. } => read_only(ui, tl!(SUMMARY_TITLE), text, "assist.summary"),
            }
            None
        }
    };
    if let Some(status) = status {
        ui.ctx().accesskit_node_builder(status.id, |node| node.set_live(egui::accesskit::Live::Polite));
    }
    if let Some(note) = d.note() {
        ui.label(egui::RichText::new(tl!(note)).small().weak());
    }
    let ready = matches!(d.phase, Phase::Ready(_));
    let focus = std::mem::take(&mut d.focus_answer);
    let mut out = None;
    ui.add_space(8.0);
    ui.horizontal(|ui| match d.kind {
        Kind::Improve(_) => {
            let replace = button(ui, tl!("Replace"), ready, true);
            if focus {
                replace.request_focus();
            }
            if replace.clicked() {
                out = Some(Choice::Replace);
            }
            if button(ui, tl!("Cancel"), true, false).clicked() {
                out = Some(Choice::Close);
            }
        }
        Kind::Summary { .. } => {
            let copy = button(ui, tl!("Copy"), ready, true);
            if focus {
                copy.request_focus();
            }
            if copy.clicked() {
                out = Some(Choice::Copy);
            }
            if button(ui, tl!("Insert at Cursor"), ready, false).clicked() {
                out = Some(Choice::Insert);
            }
            if button(ui, tl!("Close"), true, false).clicked() {
                out = Some(Choice::Close);
            }
        }
    });
    out
}

#[cfg(test)]
mod tests;

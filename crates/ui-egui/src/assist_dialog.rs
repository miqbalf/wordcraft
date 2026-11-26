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
const READ_ONLY: &str = "Improve with AI changes the text, and this document can't be edited here.";

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
    /// The text as it will be put in (`shown`), and the AI's answer that Replace sends.
    Ready { shown: String, answer: String },
    /// Why there is nothing to show (English, translated when drawn).
    Failed(String),
}

/// The open modal.
pub(crate) struct AssistDialog {
    kind: Kind,
    phase: Phase,
    /// The document it was opened for: it closes when another one is shown.
    document: u64,
    /// The phase changed: a button takes the keyboard on the next frame (the first one once the
    /// answer is there, else Cancel or Close).
    focus: bool,
}

impl AssistDialog {
    fn new(kind: Kind, phase: Phase, document: u64) -> AssistDialog {
        AssistDialog { kind, phase, document, focus: true }
    }

    /// A Summary modal for `review.summarySource`'s answer `source`, waiting for request `id`.
    #[cfg(test)]
    pub(crate) fn summary(source: &Value, id: String) -> AssistDialog {
        let truncated = source.get("truncated").and_then(Value::as_bool).unwrap_or(false);
        AssistDialog::new(Kind::Summary { truncated }, Phase::Waiting(id), 0)
    }

    /// The note under the summary: the document was cut to the limit.
    pub(crate) fn note(&self) -> Option<&'static str> {
        matches!(self.kind, Kind::Summary { truncated: true }).then_some(TRUNCATED)
    }
}

impl WordApp {
    /// The refusal of `ui.improve` or `ui.summarize` before anything is sent, also said on the
    /// status bar (a keytip or a script started it, with nothing else to show it).
    fn assist_refused(&mut self, why: &str) -> Result<Value, String> {
        let why = tl!(why).to_string();
        self.status(why.clone());
        Err(why)
    }

    /// `ui.improve`: ask the AI to improve the selection (or the caret's paragraph). Only where
    /// Replace could apply, so a viewer sends nothing and uses none of the AI's rate limit.
    pub(crate) fn assist_improve(&mut self) -> Result<Value, String> {
        if !self.assist_ai_on() {
            return self.assist_refused(OFF);
        }
        if !crate::widgets::enabled(self, "review.improveApply") {
            return self.assist_refused(READ_ONLY);
        }
        let document = self.session.document_id();
        let source = match self.session.run("review.improveSource", &json!({})) {
            Ok(v) => v,
            Err(e) => {
                let why = e.to_string();
                self.assist.dialog = Some(AssistDialog::new(Kind::Improve(None), Phase::Failed(why.clone()), document));
                return Ok(json!({"dialog": "improve", "refused": why}));
            }
        };
        let text = source.get("text").and_then(Value::as_str).unwrap_or_default().to_string();
        let language = source.get("language").and_then(Value::as_str).unwrap_or("auto").to_string();
        let kind = Kind::Improve(Some(Source {
            anchor: source.get("anchor").cloned().unwrap_or(Value::Null),
            focus: source.get("focus").cloned().unwrap_or(Value::Null),
            original: text.clone(),
        }));
        self.assist_open(kind, AiTask::Improve, language, text, "improve")
    }

    /// `ui.summarize`: ask the AI to summarize the document.
    pub(crate) fn assist_summarize(&mut self) -> Result<Value, String> {
        if !self.assist_ai_on() {
            return self.assist_refused(OFF);
        }
        let document = self.session.document_id();
        let source = match self.session.run("review.summarySource", &json!({})) {
            Ok(v) => v,
            Err(e) => {
                let why = e.to_string();
                self.assist.dialog = Some(AssistDialog::new(Kind::Summary { truncated: false }, Phase::Failed(why.clone()), document));
                return Ok(json!({"dialog": "summary", "refused": why}));
            }
        };
        let text = source.get("text").and_then(Value::as_str).unwrap_or_default().to_string();
        let language = source.get("language").and_then(Value::as_str).unwrap_or("auto").to_string();
        let truncated = source.get("truncated").and_then(Value::as_bool).unwrap_or(false);
        self.assist_open(Kind::Summary { truncated }, AiTask::Summarize, language, text, "summary")
    }

    /// Sends the request and opens the modal waiting for it (or showing why it was not sent).
    fn assist_open(&mut self, kind: Kind, task: AiTask, language: String, text: String, name: &str) -> Result<Value, String> {
        let document = self.session.document_id();
        match self.assist_ask(task, language, text) {
            Ok(id) => {
                self.assist.dialog = Some(AssistDialog::new(kind, Phase::Waiting(id.clone()), document));
                Ok(json!({"dialog": name, "id": id}))
            }
            Err(e) => {
                log::warn!("AI request not sent: {e}");
                self.assist.dialog = Some(AssistDialog::new(kind, Phase::Failed(UNAVAILABLE.into()), document));
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

/// What the buttons may do now.
#[derive(Clone, Copy)]
struct Can {
    replace: bool,
    insert: bool,
}

/// Takes the answer the modal waits for, if it came. Improve's answer is shown as
/// `review.improvePreview` says Replace will put it in, or as the refusal Replace would give.
fn poll(app: &mut WordApp, d: &mut AssistDialog) {
    let Phase::Waiting(id) = &d.phase else { return };
    let Some(answer) = app.assist_take_answer(id) else { return };
    d.focus = true;
    d.phase = match (answer, &d.kind) {
        (Ok(answer), Kind::Improve(Some(src))) => {
            let params = json!({"anchor": src.anchor, "focus": src.focus, "original": src.original, "proposal": answer});
            match app.session.run("review.improvePreview", &params) {
                Ok(v) => Phase::Ready { shown: v.get("text").and_then(Value::as_str).unwrap_or_default().to_string(), answer },
                Err(e) => Phase::Failed(e.to_string()),
            }
        }
        (Ok(answer), _) => Phase::Ready { shown: answer.clone(), answer },
        (Err(code), _) => Phase::Failed(error_sentence(&code).to_string()),
    };
}

/// Draws the modal while it is open and does what its buttons say.
pub(crate) fn show(app: &mut WordApp, ctx: &Context) {
    let Some(mut d) = app.assist.dialog.take() else { return };
    // Opened for a document that is no longer shown: whatever it would do belongs to that one.
    if d.document != app.session.document_id() {
        close(app);
        return;
    }
    poll(app, &mut d);
    let can = Can { replace: crate::widgets::enabled(app, "review.improveApply"), insert: crate::widgets::enabled(app, "text.insert") };
    let resp = egui::Modal::new(Id::unique(MODAL_ID)).show(ctx, |ui| body(ui, &mut d, can));
    let title = match d.kind {
        Kind::Improve(_) => IMPROVE_TITLE,
        Kind::Summary { .. } => SUMMARY_TITLE,
    };
    ctx.accesskit_node_builder(resp.response.id, |node| {
        node.set_role(egui::accesskit::Role::Dialog);
        node.set_label(tl!(title).to_string());
    });
    // egui's Modal blocks the window behind it, but not the document's keyboard.
    crate::dialogs::release_keyboard(app, ctx);
    let ready = matches!(d.phase, Phase::Ready { .. });
    // A click beside the modal does not throw away an answer that came (Esc and the buttons do).
    let beside = resp.backdrop_response.clicked();
    let closing = resp.should_close() && !(ready && beside);
    let mut choice = resp.inner;
    if choice.is_none() && closing {
        choice = Some(Choice::Close);
    }
    match choice {
        None => app.assist.dialog = Some(d),
        Some(Choice::Close) => close(app),
        Some(Choice::Copy) => {
            if let Phase::Ready { shown, .. } = &d.phase {
                ctx.copy_text(shown.clone());
                app.status(tl!("Copied"));
            }
            app.assist.dialog = Some(d);
        }
        Some(Choice::Replace) => {
            let applied = match (&d.kind, &d.phase) {
                (Kind::Improve(Some(src)), Phase::Ready { answer, .. }) => Some(
                    app.run("review.improveApply", json!({"anchor": src.anchor, "focus": src.focus, "original": src.original, "proposal": answer})),
                ),
                _ => None,
            };
            finish(app, d, applied);
        }
        Some(Choice::Insert) => {
            let inserted = match &d.phase {
                Phase::Ready { shown, .. } => Some(insert_paragraphs(app, shown)),
                _ => None,
            };
            finish(app, d, inserted);
        }
    }
}

/// After Replace or Insert at Cursor: closed when it applied; a refusal stays in the modal and
/// nothing was changed.
fn finish(app: &mut WordApp, mut d: AssistDialog, done: Option<Result<Value, String>>) {
    match done {
        Some(Ok(_)) => close(app),
        Some(Err(e)) => {
            d.phase = Phase::Failed(e);
            d.focus = true;
            app.assist.dialog = Some(d);
        }
        None => app.assist.dialog = Some(d),
    }
}

/// Insert at Cursor: the lines as whole paragraphs after the caret's paragraph (in it when it is
/// empty), so no line is glued to the text around the caret and a selection is left alone. One
/// command, so one undo step.
fn insert_paragraphs(app: &mut WordApp, text: &str) -> Result<Value, String> {
    let focus = app.session.sel.focus.clone();
    let len = app.session.doc.para_at(&focus).map_or(0, wordcraft_doc::Paragraph::len);
    app.session.sel = wordcraft_engine::Selection::caret(wordcraft_doc::Pos { off: len, ..focus });
    let text = if len > 0 { format!("\n{text}") } else { text.to_string() };
    app.run("text.insert", json!({"text": text, "paragraphs": true}))
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
fn body(ui: &mut Ui, d: &mut AssistDialog, can: Can) -> Option<Choice> {
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
        Phase::Ready { shown, .. } => {
            match d.kind {
                Kind::Improve(_) => read_only(ui, tl!("Proposal"), shown, "assist.proposal"),
                Kind::Summary { .. } => read_only(ui, tl!(SUMMARY_TITLE), shown, "assist.summary"),
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
    let ready = matches!(d.phase, Phase::Ready { .. });
    let focus = std::mem::take(&mut d.focus);
    let mut out = None;
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        let (first, close) = match d.kind {
            Kind::Improve(_) => {
                let replace = button(ui, tl!("Replace"), ready && can.replace, true);
                if replace.clicked() {
                    out = Some(Choice::Replace);
                }
                let cancel = button(ui, tl!("Cancel"), true, false);
                (replace, cancel)
            }
            Kind::Summary { .. } => {
                let copy = button(ui, tl!("Copy"), ready, true);
                if copy.clicked() {
                    out = Some(Choice::Copy);
                }
                if button(ui, tl!("Insert at Cursor"), ready && can.insert, false).clicked() {
                    out = Some(Choice::Insert);
                }
                let close = button(ui, tl!("Close"), true, false);
                (copy, close)
            }
        };
        if close.clicked() {
            out = Some(Choice::Close);
        }
        if focus {
            if first.enabled() { first.request_focus() } else { close.request_focus() }
        }
    });
    out
}

#[cfg(test)]
mod tests;

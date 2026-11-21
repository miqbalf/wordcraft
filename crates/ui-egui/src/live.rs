//! Live co-editing in the window: the document is edited with other people through a relay
//! (`wordcraft-live`'s [`LiveEditing`]). The host connects the hub and hands it over with
//! [`WordApp::start_live`]; from then on this module
//!
//! - pumps the link every frame and keeps the frame clock running while live, because the
//!   relay's messages arrive between frames and do not wake the window;
//! - is the command gate: [`WordApp::execute`], which every user, shortcut, typing, IME, menu,
//!   ribbon and script command goes through, asks [`WordApp::live_gate`] first and runs the
//!   engine through [`WordApp::live_session_run`], so each edit is shared before the next
//!   command (an edit queued after an Undo would otherwise be dropped). Refused commands are
//!   greyed out through [`crate::widgets::enabled`];
//! - shows the state on the status bar, the other editors as a presence bar, and their carets on
//!   the page with their names, each caret an accessibility node "<Name>'s cursor";
//! - sends this editor's caret when it moves (the link holds it to five a second);
//! - tells the host (`Services::live_state`) at once when the state changes, and at most once a
//!   second when only the save time or the roster changed.
//!
//! A file whose live session speaks a newer protocol ([`WordApp::live_newer`]) opens nothing
//! and refuses every edit.

use craft_collab::{ClientMsg, Hub, ServerMsg};
use egui::{Color32, Pos2, Rect, Sense, Stroke, Ui, pos2, vec2};
use serde_json::Value;
use wordcraft_doc::{Pos, StoryRef};
use wordcraft_live::LiveEditing;
pub use wordcraft_live::{Access, LiveStatus, Peer};

use crate::{WordApp, i18n, now_ms};

/// The status line when the room speaks a protocol this build does not.
pub const NEWER: &str = "This file is being edited with a newer WordCraft. Update to join.";
/// The status line once the link is broken for good ([`LiveStatus::Broken`]).
pub const ENDED: &str = "Live editing ended. Reopen the file.";
/// The notice when the link breaks: [`ENDED`] and why.
pub const ENDED_BECAUSE: &str = "Live editing ended. Reopen the file. {reason}";
/// The status line once the link is broken with edits no save holds: Save As keeps them.
pub const ENDED_UNSAVED: &str = "Live editing ended. Download a copy to keep your changes, then reopen the file.";
/// [`ENDED_UNSAVED`] and why, as the notice.
pub const ENDED_UNSAVED_BECAUSE: &str = "Live editing ended. Download a copy to keep your changes, then reopen the file. {reason}";
/// Save and Save As while live: the document service writes the file for everyone.
pub const SAVED_FOR_YOU: &str = "While the file is edited live, it is saved for everyone automatically.";
/// How often the host hears the state when nothing changed, in milliseconds.
const REPORT_MS: f64 = 1000.0;
/// How often a live window draws a frame to take in the relay's messages, in milliseconds.
const PUMP_MS: u64 = 100;

/// Any hub, so the window does not care which transport the host connected.
pub struct DynHub(pub Box<dyn Hub>);

impl Hub for DynHub {
    fn send(&mut self, msg: ClientMsg) -> craft_collab::Result<()> {
        self.0.send(msg)
    }

    fn poll(&mut self) -> Vec<ServerMsg> {
        self.0.poll()
    }

    fn connected(&self) -> bool {
        self.0.connected()
    }

    fn set_token(&mut self, token: String) {
        self.0.set_token(token);
    }
}

/// What the host is told about the session (`live-state`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveReport {
    /// `connecting`, `live`, `readonly`, `reconnecting` or `refused`.
    pub state: &'static str,
    /// Why, for `refused`: the user's message, in English.
    pub reason: Option<String>,
    /// When this window learned of the newest save, in Unix milliseconds.
    pub saved_at_ms: Option<i64>,
    /// The other editors: `(peer id, display name)`.
    pub peers: Vec<(String, String)>,
}

/// The window's live session.
pub enum LiveMode {
    /// Linked to the room.
    Linked(Box<LiveLink>),
    /// The room speaks a newer protocol: nothing is shared and nothing may be edited.
    Newer,
}

/// A running link and what the window keeps about it.
pub struct LiveLink {
    editing: LiveEditing<DynHub>,
    /// The document the link edits; another document shown in its place ends the link.
    document: u64,
    peers: Vec<Peer>,
    /// The caret last sent: `(pid, char)` and the selection's other end.
    sent_caret: Option<((u64, usize), Option<(u64, usize)>)>,
    /// The newest save's ETag, and the Unix seconds when this window learned of it.
    saved: Option<(String, i64)>,
    reported: Option<(LiveReport, f64)>,
    /// The broken state's detail has been put on the status line.
    told_broken: bool,
    /// Milliseconds for the report throttle: the frame clock unless set
    /// ([`WordApp::live_set_clock`]).
    clock: Box<dyn Fn() -> f64>,
}

impl LiveLink {
    /// The link's own state.
    pub fn status(&self) -> &LiveStatus {
        self.editing.status()
    }

    /// Who else is here, as of the last frame.
    pub fn peers(&self) -> &[Peer] {
        &self.peers
    }

    /// Whether the hub is connected now.
    pub fn connected(&self) -> bool {
        self.editing.hub().connected()
    }

    /// The editing link itself, for reading its state.
    pub fn editing(&self) -> &LiveEditing<DynHub> {
        &self.editing
    }
}

fn unix_secs() -> i64 {
    web_time::SystemTime::now().duration_since(web_time::UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// A body position as the link names it: the paragraph's id and the offset in chars.
fn caret_of(doc: &wordcraft_doc::Document, pos: &Pos) -> Option<(u64, usize)> {
    if pos.story != StoryRef::Body || pos.path.depth() != 0 {
        return None;
    }
    let para = doc.body.get(pos.path.last())?.as_para()?;
    let off = pos.off.min(para.text.len());
    let chars = para.text.get(..off).map_or_else(|| para.text.chars().count(), |s| s.chars().count());
    (para.pid != 0).then_some((para.pid, chars))
}

/// The body's paragraphs by id: `pid` to block index.
fn blocks_by_pid(doc: &wordcraft_doc::Document) -> std::collections::HashMap<u64, usize> {
    doc.body.iter().enumerate().filter_map(|(i, b)| b.as_para().filter(|p| p.pid != 0).map(|p| (p.pid, i))).collect()
}

/// A caret the link names, back as a body position (bytes), if its paragraph is here. `blocks`
/// is [`blocks_by_pid`] of `doc`.
fn pos_of(doc: &wordcraft_doc::Document, blocks: &std::collections::HashMap<u64, usize>, pid: u64, chars: usize) -> Option<Pos> {
    let block = *blocks.get(&pid)?;
    let para = doc.body.get(block)?.as_para()?;
    let off = para.text.char_indices().nth(chars).map_or(para.text.len(), |(i, _)| i);
    Some(Pos::body(block, off))
}

impl WordApp {
    /// Edit `bytes` (the file `file_name`) live in `room`, over `hub`. The document replaces the
    /// one shown; with [`Access::Read`] nothing is ever sent.
    pub fn start_live(&mut self, hub: Box<dyn Hub>, room: &str, bytes: &[u8], file_name: &str, access: Access) -> Result<(), String> {
        if let Some(LiveMode::Linked(old)) = self.live.take() {
            old.editing.stop(&mut self.session);
        }
        let before = self.session.document_id();
        let mut editing = LiveEditing::start_as(DynHub(hub), room, bytes, file_name, &mut self.session, access).map_err(|e| e.to_string())?;
        let start = web_time::Instant::now();
        editing.set_clock(Box::new(move || start.elapsed().as_secs_f64() * 1000.0));
        self.session.path = Some(std::path::PathBuf::from(file_name));
        self.session.dirty = false;
        self.ui.backstage = false;
        if self.session.document_id() != before {
            self.sync_document();
        }
        let document = self.session.document_id();
        self.live = Some(LiveMode::Linked(Box::new(LiveLink {
            editing,
            document,
            peers: Vec::new(),
            sent_caret: None,
            saved: None,
            reported: None,
            told_broken: false,
            clock: Box::new(now_ms),
        })));
        Ok(())
    }

    /// The room speaks a protocol newer than this build's: say so, and take no edits.
    pub fn live_newer(&mut self) {
        if let Some(LiveMode::Linked(old)) = self.live.take() {
            old.editing.stop(&mut self.session);
        }
        self.live = Some(LiveMode::Newer);
        // Nothing was sent to open: show the empty page, whose status bar says why.
        self.no_document = None;
        self.ui.backstage = false;
        self.status_long(tl!(NEWER));
    }

    /// The running link, if any.
    pub fn live_link(&self) -> Option<&LiveLink> {
        match &self.live {
            Some(LiveMode::Linked(link)) => Some(link),
            _ => None,
        }
    }

    /// A fresh ticket from the host: the hub uses it for its next (re)connect, and a hub the
    /// relay refused (an expired ticket) connects again with it. False when nothing is linked.
    pub fn live_refresh_ticket(&mut self, ticket: String) -> bool {
        match &mut self.live {
            Some(LiveMode::Linked(link)) => {
                link.editing.hub_mut().set_token(ticket);
                true
            }
            _ => false,
        }
    }

    /// The clock, in milliseconds, that holds the host's reports to one a second (tests).
    pub fn live_set_clock(&mut self, clock: Box<dyn Fn() -> f64>) {
        if let Some(LiveMode::Linked(link)) = &mut self.live {
            link.clock = clock;
        }
    }

    /// Whether the document service saves this window's document: linked to a room and
    /// connecting, live or reconnecting, so the window has no unsaved changes of its own. Once
    /// the link is broken or refused, edits no save holds are this window's to keep.
    pub(crate) fn live_saves(&self) -> bool {
        self.live_link().is_some_and(|link| matches!(link.status(), LiveStatus::Connecting | LiveStatus::Live { .. } | LiveStatus::Reconnecting))
    }

    /// Whether live editing lets command `id` run now; true when not live.
    pub(crate) fn live_allows(&self, id: &str) -> bool {
        match &self.live {
            None => true,
            // Saving over the file would race the document service's saves. A copy is refused
            // while the service saves, and is how edits are kept once the link has ended.
            Some(LiveMode::Linked(link)) => match id {
                "file.save" => false,
                "file.saveAs" => !self.live_saves(),
                _ => link.editing.allows(id),
            },
            Some(LiveMode::Newer) => self.session.registry.get(id).is_none_or(|spec| !spec.mutates),
        }
    }

    /// The gate in [`WordApp::execute`]: the refusal of an engine command live editing does not
    /// allow now, with the reason on the status bar. `None` lets it run.
    pub(crate) fn live_gate(&mut self, id: &str, params: &Value) -> Option<Result<Value, String>> {
        if self.session.registry.get(id).is_none() || self.live_allows(id) {
            return None;
        }
        let (saves, dirty) = (self.live_saves(), self.session.dirty);
        let why = match &mut self.live {
            // Refuses without running, with the reason as a notice for the next frame.
            Some(LiveMode::Linked(link)) if !saves_the_file(id) => {
                return Some(link.editing.run(&mut self.session, id, params).map_err(|e| e.to_string()));
            }
            Some(LiveMode::Linked(_)) if saves => tl!(SAVED_FOR_YOU),
            Some(LiveMode::Linked(_)) if dirty => tl!(ENDED_UNSAVED),
            Some(LiveMode::Linked(_)) => tl!(ENDED),
            _ => tl!(NEWER),
        };
        self.status_long(why);
        let refusal = Err(why.to_string());
        Some(refusal)
    }

    /// Run an engine command, through the link while live so its edit is shared before the next
    /// one. An input method's commands run as its transaction and are shared straight after.
    pub(crate) fn live_session_run(&mut self, id: &str, params: &Value, ime: bool) -> Result<Value, String> {
        match &mut self.live {
            Some(LiveMode::Linked(link)) if ime => {
                let r = self.session.run_ime(id, params);
                link.editing.flush(&mut self.session);
                r.map_err(|e| e.to_string())
            }
            Some(LiveMode::Linked(link)) => link.editing.run(&mut self.session, id, params).map_err(|e| e.to_string()),
            _ if ime => self.session.run_ime(id, params).map_err(|e| e.to_string()),
            _ => self.session.run(id, params).map_err(|e| e.to_string()),
        }
    }

    /// One frame of the link: pump it, show its notices, send the caret, tell the host.
    pub(crate) fn live_tick(&mut self, ctx: &egui::Context) {
        let Some(LiveMode::Linked(link)) = &mut self.live else { return };
        if self.session.document_id() != link.document {
            // Another document is shown: this one is no longer edited live.
            if let Some(LiveMode::Linked(old)) = self.live.take() {
                old.editing.stop(&mut self.session);
            }
            return;
        }
        let document = self.session.document_id();
        link.peers = link.editing.pump(&mut self.session);
        // The room's catch-up can replace the whole document: it is still the one edited live
        // (the window follows it on its own, as after any command).
        if self.session.document_id() != document {
            link.document = self.session.document_id();
        }
        let mut notices: Vec<String> = link.editing.take_notices().iter().map(|n| tl!(n.as_str()).to_string()).collect();
        if let LiveStatus::Live { saved_at: Some(etag) } = link.editing.status()
            && link.saved.as_ref().is_none_or(|(e, _)| e != etag)
        {
            link.saved = Some((etag.clone(), unix_secs()));
        }
        // Clean once the newest save holds every edit this window made; an edit the relay
        // ordered after the save's checkpoint, or has not ordered yet, keeps it dirty.
        if self.session.dirty && link.editing.edits_saved() {
            self.session.dirty = false;
        }
        if let LiveStatus::Broken(detail) = link.editing.status()
            && !link.told_broken
        {
            link.told_broken = true;
            let notice = if self.session.dirty { ENDED_UNSAVED_BECAUSE } else { ENDED_BECAUSE };
            notices.push(i18n::fmt(tl!(notice), &[("reason", tl!(detail.as_str()))]));
        }
        // The caret, when it moved.
        let focus = caret_of(&self.session.doc, &self.session.sel.focus);
        let anchor = if self.session.sel.is_collapsed() { None } else { caret_of(&self.session.doc, &self.session.sel.anchor) };
        if let Some(at) = focus {
            let caret = (at, anchor);
            if link.sent_caret != Some(caret) {
                link.editing.send_cursor(at.0, at.1, anchor);
                link.sent_caret = Some(caret);
            }
        }
        let report = report_of(link);
        let now = (link.clock)();
        let due = match &link.reported {
            None => true,
            // A new state at once; a new save or roster at most once a second.
            Some((last, at)) => last.state != report.state || last.reason != report.reason || (*last != report && now - at >= REPORT_MS),
        };
        if due {
            if let Some(tell) = &self.services.live_state {
                tell(&report);
            }
            link.reported = Some((report, now));
        }
        for n in notices {
            self.status_long(n);
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(PUMP_MS));
    }

    /// The status bar's live item: its text and, for a broken link, the detail for its tooltip.
    pub(crate) fn live_status_line(&self) -> Option<(String, Option<String>)> {
        let link = match &self.live {
            None => return None,
            Some(LiveMode::Newer) => return Some((tl!(NEWER).to_string(), None)),
            Some(LiveMode::Linked(link)) => link,
        };
        Some(match link.editing.status() {
            LiveStatus::Connecting => (tl!("Connecting…").to_string(), None),
            LiveStatus::Reconnecting => (tl!("Reconnecting…").to_string(), None),
            LiveStatus::ReadOnly => (tl!("Read-only").to_string(), None),
            LiveStatus::Live { .. } => match &link.saved {
                Some((_, secs)) => (i18n::fmt(tl!("Live · saved {time}"), &[("time", &crate::local_time::clock_time(*secs))]), None),
                None => (tl!("Live").to_string(), None),
            },
            LiveStatus::Refused(why) => (tl!(why.as_str()).to_string(), None),
            LiveStatus::Broken(why) => {
                let line = if self.session.dirty { ENDED_UNSAVED } else { ENDED };
                (tl!(line).to_string(), Some(tl!(why.as_str()).to_string()))
            }
        })
    }

    /// The other editors as a row of badges, on the status bar.
    pub(crate) fn live_presence_bar(&self, ui: &mut Ui) {
        let Some(link) = self.live_link() else { return };
        if link.peers.is_empty() {
            return;
        }
        let labels = craft_ui::presence::PresenceLabels {
            alone: String::new(),
            with_one: tl!("Editing with {name}").to_string(),
            with_many: tl!("Editing with {count} others").to_string(),
            and_more: tl!("{count} more").to_string(),
            who: tl!("Who is here").to_string(),
        };
        let mut bar = craft_ui::presence::PresenceBar::new().with_labels(labels).badges_only();
        bar.set(link.peers.iter().map(|p| craft_ui::presence::Peer::new(p.id.clone(), p.display.clone())).collect());
        bar.show(ui);
    }
}

/// Save and Save As, which write this window's copy over the file.
fn saves_the_file(id: &str) -> bool {
    matches!(id, "file.save" | "file.saveAs")
}

fn report_of(link: &LiveLink) -> LiveReport {
    let (state, reason) = match link.editing.status() {
        LiveStatus::Connecting => ("connecting", None),
        LiveStatus::Live { .. } => ("live", None),
        LiveStatus::ReadOnly => ("readonly", None),
        LiveStatus::Reconnecting => ("reconnecting", None),
        LiveStatus::Refused(why) | LiveStatus::Broken(why) => ("refused", Some(why.clone())),
    };
    LiveReport {
        state,
        reason,
        saved_at_ms: link.saved.as_ref().map(|(_, s)| s.saturating_mul(1000)),
        peers: link.peers.iter().map(|p| (p.id.clone(), p.display.clone())).collect(),
    }
}

/// The other editors' carets on the pages: a bar in each one's colour with the name above it,
/// and an accessibility node "<Name>'s cursor". `rects` are the pages on screen.
pub(crate) fn paint_carets(app: &mut WordApp, ui: &mut Ui, painter: &egui::Painter, rects: &[Rect], scale: f32) {
    let Some(link) = app.live_link() else { return };
    let ids: Vec<String> = link.peers.iter().map(|p| p.id.clone()).collect();
    let colours = craft_ui::presence::colours_for(&ids);
    let blocks = blocks_by_pid(&app.session.doc);
    let shown: Vec<(String, String, Color32, Pos)> = link
        .peers
        .iter()
        .zip(colours)
        .filter_map(|(p, colour)| {
            let (pid, at) = p.caret?;
            Some((p.id.clone(), p.display.clone(), colour, pos_of(&app.session.doc, &blocks, pid, at)?))
        })
        .collect();
    if shown.is_empty() {
        return;
    }
    let layout = app.session.layout();
    let hint = app.session.page_hint;
    for (id, name, colour, pos) in shown {
        let Some(c) = layout.caret_on(&pos, hint) else { continue };
        let Some(pr) = rects.get(c.page) else { continue };
        let s = layout.pages.get(c.page).map_or(scale, |page| crate::canvas::page_screen_scale(*pr, page, scale));
        let x = (pr.min.x + c.x * s).round() + 0.5;
        let y0 = pr.min.y + c.top * s;
        let y1 = y0 + c.height.max(4.0) * s;
        painter.line_segment([pos2(x, y0), pos2(x, y1)], Stroke::new(2.0, colour));
        let font = egui::FontId::proportional(11.0);
        let galley = painter.layout_no_wrap(name.clone(), font, Color32::WHITE);
        let tag = Rect::from_min_size(Pos2::new(x - 1.0, y0 - galley.size().y - 2.0), galley.size() + vec2(6.0, 2.0));
        painter.rect_filled(tag, 2.0, colour);
        painter.galley(tag.min + vec2(3.0, 1.0), galley, Color32::WHITE);
        let hit = Rect::from_min_max(pos2(x - 1.0, tag.min.y), pos2(x + 1.0, y1)).union(tag);
        let label = i18n::fmt_plain(tl!("{name}'s cursor"), &[("name", &name)]);
        let resp = ui.interact(hit, ui.scope_id().with(("live_caret", id)), Sense::hover());
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Caret, true, &label));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caret_offsets_are_chars_both_ways() {
        let mut doc = wordcraft_doc::Document::from_text("héllo");
        if let Some(wordcraft_doc::Block::Para(p)) = doc.body.get_mut(0).map(std::sync::Arc::make_mut) {
            p.pid = 7;
        }
        let pos = Pos::body(0, "hé".len());
        let at = caret_of(&doc, &pos);
        assert_eq!(at, Some((7, 2)), "two chars in, not three bytes");
        assert_eq!(pos_of(&doc, &blocks_by_pid(&doc), 7, 2), Some(pos), "and back to the same byte");
    }
}

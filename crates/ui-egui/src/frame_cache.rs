//! Draw a part of the window from last frame's shapes when nothing it shows can have changed.
//!
//! egui lays every widget out again each frame. For the ribbon and the status bar that was 17%
//! and 5% of the browser's CPU while typing, and most of a live window's idle frames (PERF1). Most
//! frames change neither: the caret blinks or glides, a peer's caret moves, the page scrolls.
//! [`show`] runs the part's code on a frame that may change it and records what it painted; on
//! the other frames it paints the recording again and runs nothing.
//!
//! A frame may change the part, and so runs its code, when:
//! - the caller's `key` changed (what the part shows: the document's revision, the selection, the
//!   tab, the zoom, the language, …);
//! - the frame or the frame before had input that can run a command (a key, text, a click, a
//!   paste): the app handles keys late in a frame, so their effect shows a frame later;
//! - the pointer is over the part, or was last frame (hover highlights, tooltips);
//! - a popup or menu is open, or a widget inside the part has the keyboard focus;
//! - a screen reader asked for the accessibility tree (a replay has no widgets to describe);
//! - the part's rectangle or layer moved;
//! - the recording is older than [`MAX_AGE`], a bound on how long anything the key misses can
//!   stay stale;
//! - the last two runs painted different shapes: the part is still settling or animating (a tab
//!   sliding in, a group re-planned for the width), which asks for frames the replay would not.
//!
//! A replay registers no widgets, so it takes no clicks: that is why the pointer over the part,
//! or any click, makes it run for real.

use std::sync::Arc;

use egui::epaint::ClippedShape;
use egui::{Id, LayerId, Rect, Ui};

/// The oldest recording [`show`] replays, in seconds.
pub const MAX_AGE: f64 = 1.0;

/// What a part painted on the last frame it ran.
#[derive(Clone)]
struct Recording {
    key: u64,
    rect: Rect,
    layer: LayerId,
    shapes: Arc<Vec<ClippedShape>>,
    /// egui's clock when it was made.
    at: f64,
    /// The pointer was over the part, or a widget in it had the focus: run it again next frame.
    busy: bool,
    /// It painted the same shapes as the run before it.
    stable: bool,
}

/// Per-part bookkeeping kept between frames.
#[derive(Clone, Default)]
struct State {
    rec: Option<Recording>,
    /// The last frame number that had input which can run a command.
    input_frame: Option<u64>,
    /// How many frames ran the part's code, and how many replayed it (tests, tools).
    built: u64,
    replayed: u64,
}

fn enabled_id() -> Id {
    Id::unique("frame_cache_enabled")
}

/// Turn replays on or off for this context. On by default in the browser, where the frames cost
/// the most; off elsewhere, where a part's code runs every frame as before.
pub fn set_enabled(ctx: &egui::Context, on: bool) {
    ctx.data_mut(|d| d.insert_temp(enabled_id(), on));
}

/// Whether [`show`] replays settled parts in this context.
pub fn enabled(ctx: &egui::Context) -> bool {
    ctx.data(|d| d.get_temp::<bool>(enabled_id())).unwrap_or(cfg!(target_arch = "wasm32"))
}

/// How often a part ran and how often it was replayed (tests and tools).
pub fn counts(ctx: &egui::Context, id: Id) -> (u64, u64) {
    ctx.data(|d| d.get_temp::<State>(id)).map_or((0, 0), |s| (s.built, s.replayed))
}

/// Forget a part's recording: it runs on the next frame.
pub fn invalidate(ctx: &egui::Context, id: Id) {
    ctx.data_mut(|d| {
        if let Some(s) = d.get_temp_mut_or_default::<State>(id).rec.as_mut() {
            s.busy = true;
        }
    });
}

/// Input that can run a command or change what a part shows.
fn commanding(ctx: &egui::Context) -> bool {
    ctx.input(|i| {
        i.events.iter().any(|e| {
            matches!(
                e,
                egui::Event::Key { .. }
                    | egui::Event::Text(_)
                    | egui::Event::Paste(_)
                    | egui::Event::Copy
                    | egui::Event::Cut
                    | egui::Event::Ime(_)
                    | egui::Event::PointerButton { .. }
            )
        }) || i.pointer.any_down()
    })
}

/// Run `build` in `ui` on a frame that may change what it shows, and otherwise paint what it
/// painted last time. `id` names the part; `key` fingerprints what it shows.
pub fn show(ui: &mut Ui, id: Id, key: u64, build: impl FnOnce(&mut Ui)) {
    let ctx = ui.ctx().clone();
    if !enabled(&ctx) {
        build(ui);
        return;
    }
    let rect = ui.max_rect();
    let layer = ui.layer_id();
    let (now, frame) = (ctx.input(|i| i.time), ctx.cumulative_frame_nr());
    let mut state = ctx.data(|d| d.get_temp::<State>(id)).unwrap_or_default();
    if commanding(&ctx) {
        state.input_frame = Some(frame);
    }
    let recent_input = state.input_frame.is_some_and(|f| frame.saturating_sub(f) <= 1);
    let hovered = ctx.pointer_hover_pos().is_some_and(|p| rect.contains(p));
    let screen_reader = ctx.accesskit_node_builder(ui.scope_id(), |_| ()).is_some();
    let replay = !recent_input
        && !hovered
        && !screen_reader
        && !ctx.any_popup_open()
        && state
            .rec
            .as_ref()
            .is_some_and(|r| !r.busy && r.stable && r.key == key && r.rect == rect && r.layer == layer && (0.0..MAX_AGE).contains(&(now - r.at)));
    if replay && let Some(rec) = &state.rec {
        ctx.graphics_mut(|g| {
            let list = g.entry(layer);
            for s in rec.shapes.iter() {
                list.add(s.clip_rect, s.shape.clone());
            }
        });
        ui.expand_to_include_rect(rect);
        state.replayed += 1;
        ctx.data_mut(|d| d.insert_temp(id, state));
        return;
    }
    let start = ctx.graphics(|g| g.get(layer).map_or(0, |l| l.next_idx().0));
    build(ui);
    let shapes: Vec<ClippedShape> = ctx.graphics(|g| g.get(layer).map(|l| l.all_entries().skip(start).cloned().collect()).unwrap_or_default());
    let focus_inside = ctx.memory(|m| m.focused()).and_then(|f| ctx.read_response(f)).is_some_and(|r| rect.intersects(r.rect));
    let stable = state.rec.as_ref().is_some_and(|r| r.rect == rect && r.layer == layer && *r.shapes == shapes);
    state.rec = Some(Recording { key, rect, layer, shapes: Arc::new(shapes), at: now, busy: hovered || focus_inside, stable });
    state.built += 1;
    ctx.data_mut(|d| d.insert_temp(id, state));
}

/// A fingerprint of `value` for a key.
pub fn hash(value: impl std::hash::Hash) -> u64 {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests;

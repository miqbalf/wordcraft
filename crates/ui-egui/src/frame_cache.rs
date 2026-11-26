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
//! - the fonts changed, or egui cleared its font atlas: recorded text points into the old atlas
//!   (a probe galley laid out every frame is a new one then);
//! - the recording is [`MAX_AGE`] old or older. The window is longer than the longest period of
//!   the frames that come anyway (the caret blinks every 0.53 s), so a blinking window replays at
//!   most one frame in three and its bars run about once a second. A replay asks for a frame only
//!   as a backstop, [`BACKSTOP_GRACE`] after [`MAX_AGE`], for a window that would otherwise draw no
//!   further frame: a frame that comes first clears that request, and a run asks for none, so
//!   anything the key misses is shown at most [`MAX_AGE`] + [`BACKSTOP_GRACE`] late;
//! - the last two runs painted different shapes: the part is still settling or animating (a tab
//!   sliding in, a group re-planned for the width), which asks for frames the replay would not.
//!
//! A replay registers no widgets, so it takes no clicks: that is why the pointer over the part,
//! or any click, makes it run for real. That is too late for a press that arrives with the pointer
//! in one frame (a tap, a scripted click, a fast flick): egui hit-tests it against the widgets of
//! the frame before, and that frame was a replay. [`hold_presses`] (called from the app's
//! `raw_input_hook`) holds such a press, and what follows it, for one frame and lets the pointer
//! move through: the part runs on that frame, and the press then finds its widgets (LI3).

use std::sync::Arc;

use egui::epaint::{ClippedShape, Galley};
use egui::{Id, LayerId, Rect, Ui};

/// The oldest recording [`show`] replays, in seconds.
pub const MAX_AGE: f64 = 1.0;

/// How long past [`MAX_AGE`] a replay waits for a frame to come anyway before it asks for one: more
/// than the caret's blink period, so in a window that blinks the request is always cleared first.
pub const BACKSTOP_GRACE: f64 = 0.25;

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
    /// The font probe when it was made ([`fonts_probe`]).
    fonts: Arc<Galley>,
}

/// The parts replayed on a frame, with the frame's number and the part's rectangle.
#[derive(Clone, Default)]
struct Replayed(std::collections::HashMap<Id, (u64, Rect)>);

/// Input held back by [`hold_presses`]: the cumulative frame number when it was held, and the events.
#[derive(Clone)]
struct Held {
    at: u64,
    events: Vec<egui::Event>,
}

fn replayed_id() -> Id {
    Id::unique("frame_cache_replayed")
}

fn held_id() -> Id {
    Id::unique("frame_cache_held")
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

/// A short text laid out through egui's galley cache. The cache hands back the same galley each
/// frame until the fonts change or the font atlas is cleared, which empty the cache: then the
/// recorded text, whose glyphs point into the old atlas, must not be replayed.
fn fonts_probe(ctx: &egui::Context) -> Arc<Galley> {
    ctx.fonts_mut(|f| f.layout_no_wrap("frame_cache fonts probe".to_owned(), egui::FontId::default(), egui::Color32::WHITE))
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
    let fonts = fonts_probe(&ctx);
    let replay = !recent_input
        && !hovered
        && !screen_reader
        && !ctx.any_popup_open()
        && state.rec.as_ref().is_some_and(|r| {
            !r.busy
                && r.stable
                && r.key == key
                && r.rect == rect
                && r.layer == layer
                && Arc::ptr_eq(&r.fonts, &fonts)
                && (0.0..MAX_AGE).contains(&(now - r.at))
        });
    if replay && let Some(rec) = &state.rec {
        ctx.graphics_mut(|g| {
            let list = g.entry(layer);
            for s in rec.shapes.iter() {
                list.add(s.clip_rect, s.shape.clone());
            }
        });
        ui.expand_to_include_rect(rect);
        // The backstop: run for real once the recording is MAX_AGE old even if no frame comes. A
        // frame that comes first runs the part (it is MAX_AGE old by then) and the browser runner
        // drops this request when it draws; the grace keeps the request behind a caret's edges.
        let wait = (rec.at + MAX_AGE + BACKSTOP_GRACE - now).max(0.0);
        ctx.request_repaint_after(std::time::Duration::from_secs_f64(wait));
        state.replayed += 1;
        ctx.data_mut(|d| {
            d.get_temp_mut_or_default::<Replayed>(replayed_id()).0.insert(id, (frame, rect));
            d.insert_temp(id, state);
        });
        return;
    }
    let start = ctx.graphics(|g| g.get(layer).map_or(0, |l| l.next_idx().0));
    build(ui);
    let shapes: Vec<ClippedShape> = ctx.graphics(|g| g.get(layer).map(|l| l.all_entries().skip(start).cloned().collect()).unwrap_or_default());
    let focus_inside = ctx.memory(|m| m.focused()).and_then(|f| ctx.read_response(f)).is_some_and(|r| rect.intersects(r.rect));
    let stable = state.rec.as_ref().is_some_and(|r| r.rect == rect && r.layer == layer && *r.shapes == shapes);
    state.rec = Some(Recording { key, rect, layer, shapes: Arc::new(shapes), at: now, busy: hovered || focus_inside, stable, fonts });
    state.built += 1;
    let held = ctx.data_mut(|d| {
        d.get_temp_mut_or_default::<Replayed>(replayed_id()).0.remove(&id);
        d.insert_temp(id, state);
        d.get_temp::<Held>(held_id()).is_some()
    });
    if held {
        // A press waits for this frame's widgets: the next frame lets it go.
        ctx.request_repaint();
    }
}

/// Hold a press that lands on a part which was replayed last frame, and what follows it, for one
/// frame. Call it from the app's `raw_input_hook` with the frame's input, before anything reads it.
///
/// egui hit-tests a press against the widgets of the frame before, and a replay registers none, so
/// a press that arrives with the pointer (a tap, a scripted click, a fast flick) would find no
/// button. The press and the events after it wait; a pointer move to the press's position takes
/// their place, so the part runs on this frame ([`show`] runs a part the pointer is over) and
/// registers its widgets. The next frame gets the held events first, in their order, and asks for
/// it now. A press that lands anywhere else, or on a part that ran last frame, passes at once.
pub fn hold_presses(ctx: &egui::Context, raw: &mut egui::RawInput) {
    if !enabled(ctx) || raw.viewport_id != egui::ViewportId::ROOT {
        return;
    }
    let frame = ctx.cumulative_frame_nr();
    if let Some(held) = ctx.data(|d| d.get_temp::<Held>(held_id())) {
        if frame <= held.at {
            // The same input offered again (a tick that drew nothing): keep holding.
            return;
        }
        ctx.data_mut(|d| d.remove::<Held>(held_id()));
        raw.events.splice(0..0, held.events);
        return;
    }
    let rects: Vec<Rect> = ctx.data(|d| d.get_temp::<Replayed>(replayed_id())).map_or_else(Vec::new, |r| {
        r.0.values().filter(|(at, _)| at + 1 == frame).map(|(_, rect)| *rect).collect()
    });
    if rects.is_empty() {
        return;
    }
    let landing = raw.events.iter().enumerate().find_map(|(i, e)| match e {
        egui::Event::PointerButton { pos, pressed: true, .. } | egui::Event::Touch { pos, phase: egui::TouchPhase::Start, .. }
            if rects.iter().any(|r| r.contains(*pos)) =>
        {
            Some((i, *pos))
        }
        _ => None,
    });
    let Some((i, pos)) = landing else { return };
    let events = raw.events.split_off(i);
    raw.events.push(egui::Event::PointerMoved(pos));
    ctx.data_mut(|d| d.insert_temp(held_id(), Held { at: frame, events }));
    ctx.request_repaint();
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

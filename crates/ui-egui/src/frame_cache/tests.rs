use std::cell::Cell;

use egui::{Context, Event, Id, RawInput, pos2, vec2};

use super::{MAX_AGE, counts, set_enabled, show};

/// One frame at `time` with `events`: a panel at the top holding the part, which paints a label
/// (`text`) and counts its runs in `ran`. Returns the frame's shape count.
fn frame(ctx: &Context, time: f64, events: Vec<Event>, key: u64, text: &str, ran: &Cell<u32>) -> usize {
    let input =
        RawInput { time: Some(time), screen_rect: Some(egui::Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0))), events, ..Default::default() };
    let out = ctx.run_ui(input, |ui| {
        egui::Panel::top("part").exact_size(40.0).show(ui, |ui| {
            show(ui, Id::unique("part"), key, |ui| {
                ran.set(ran.get() + 1);
                ui.label(text);
            });
        });
    });
    let shapes = out.shapes.len();
    out.drop_without_applying_deltas();
    shapes
}

/// A context with replays on (they are off by default outside the browser).
fn on() -> Context {
    let ctx = Context::default();
    set_enabled(&ctx, true);
    ctx
}

fn id() -> Id {
    Id::unique("part")
}

#[test]
fn a_settled_part_is_replayed_with_the_same_shapes_and_its_code_not_run() {
    let (ctx, ran) = (on(), Cell::new(0));
    let first = frame(&ctx, 1.0, vec![], 7, "Home", &ran);
    let second = frame(&ctx, 1.1, vec![], 7, "Home", &ran);
    assert_eq!(ran.get(), 2, "it runs until two runs painted the same");
    let third = frame(&ctx, 1.2, vec![], 7, "Home", &ran);
    let fourth = frame(&ctx, 1.3, vec![], 7, "Home", &ran);
    assert_eq!(ran.get(), 2, "then it is replayed");
    assert_eq!((first, second, third, fourth), (first, first, first, first), "a replay paints what a run painted");
    assert_eq!(counts(&ctx, id()), (2, 2));
}

#[test]
fn a_new_key_runs_the_part() {
    let (ctx, ran) = (on(), Cell::new(0));
    for t in [1.0, 1.1, 1.2] {
        frame(&ctx, t, vec![], 7, "Home", &ran);
    }
    assert_eq!(ran.get(), 2);
    frame(&ctx, 1.3, vec![], 8, "Home", &ran);
    assert_eq!(ran.get(), 3);
}

#[test]
fn changing_shapes_keep_the_part_running() {
    let (ctx, ran) = (on(), Cell::new(0));
    for (i, t) in [1.0, 1.1, 1.2, 1.3].into_iter().enumerate() {
        frame(&ctx, t, vec![], 7, &format!("frame {i}"), &ran);
    }
    assert_eq!(ran.get(), 4, "an animating part is never replayed");
}

#[test]
fn input_runs_the_part_that_frame_and_the_next() {
    let (ctx, ran) = (on(), Cell::new(0));
    for t in [1.0, 1.1, 1.2] {
        frame(&ctx, t, vec![], 7, "Home", &ran);
    }
    assert_eq!(ran.get(), 2);
    let key = Event::Key { key: egui::Key::B, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::COMMAND };
    frame(&ctx, 1.3, vec![key], 7, "Home", &ran);
    frame(&ctx, 1.4, vec![], 7, "Home", &ran);
    assert_eq!(ran.get(), 4, "a key's command shows a frame later");
    frame(&ctx, 1.5, vec![], 7, "Home", &ran);
    assert_eq!(ran.get(), 4);
    frame(&ctx, 1.6, vec![Event::Text("x".into())], 7, "Home", &ran);
    assert_eq!(ran.get(), 5);
}

#[test]
fn the_pointer_over_the_part_runs_it_and_once_after_it_left() {
    let (ctx, ran) = (on(), Cell::new(0));
    for t in [1.0, 1.1, 1.2] {
        frame(&ctx, t, vec![], 7, "Home", &ran);
    }
    assert_eq!(ran.get(), 2);
    frame(&ctx, 1.3, vec![Event::PointerMoved(pos2(20.0, 10.0))], 7, "Home", &ran);
    frame(&ctx, 1.4, vec![], 7, "Home", &ran);
    assert_eq!(ran.get(), 4, "hovered: its widgets must take the pointer");
    frame(&ctx, 1.5, vec![Event::PointerMoved(pos2(400.0, 400.0))], 7, "Home", &ran);
    assert_eq!(ran.get(), 5, "the frame the pointer left clears the hover highlight");
    frame(&ctx, 1.6, vec![], 7, "Home", &ran);
    assert_eq!(ran.get(), 5);
}

#[test]
fn a_recording_older_than_max_age_runs_the_part() {
    let (ctx, ran) = (on(), Cell::new(0));
    for t in [1.0, 1.1, 1.2] {
        frame(&ctx, t, vec![], 7, "Home", &ran);
    }
    assert_eq!(ran.get(), 2);
    frame(&ctx, 1.1 + MAX_AGE + 0.01, vec![], 7, "Home", &ran);
    assert_eq!(ran.get(), 3);
}

#[test]
fn a_disabled_cache_runs_the_part_every_frame() {
    let (ctx, ran) = (Context::default(), Cell::new(0));
    set_enabled(&ctx, false);
    for t in [1.0, 1.1, 1.2, 1.3] {
        frame(&ctx, t, vec![], 7, "Home", &ran);
    }
    assert_eq!(ran.get(), 4);
    assert_eq!(counts(&ctx, id()), (0, 0), "a disabled cache keeps no books");
}

/// The delay until the next frame that a frame at `time` asked for.
fn repaint_delay(ctx: &Context, time: f64, ran: &Cell<u32>) -> std::time::Duration {
    let input = RawInput { time: Some(time), screen_rect: Some(egui::Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0))), ..Default::default() };
    let out = ctx.run_ui(input, |ui| {
        egui::Panel::top("part").exact_size(40.0).show(ui, |ui| {
            show(ui, id(), 7, |ui| {
                ran.set(ran.get() + 1);
                ui.label("Home");
            });
        });
    });
    let delay = out.viewport_output.get(&egui::ViewportId::ROOT).map(|v| v.repaint_delay).unwrap_or(std::time::Duration::MAX);
    out.drop_without_applying_deltas();
    delay
}

#[test]
fn a_replay_asks_for_a_frame_when_its_recording_is_max_age_old() {
    let (ctx, ran) = (on(), Cell::new(0));
    frame(&ctx, 1.0, vec![], 7, "Home", &ran);
    frame(&ctx, 1.2, vec![], 7, "Home", &ran);
    assert_eq!(ran.get(), 2);
    let delay = repaint_delay(&ctx, 1.5, &ran);
    assert_eq!(ran.get(), 2, "replayed");
    // Recorded at 1.2: the next frame is due at 2.2, 0.7 s after this one.
    assert!(delay <= std::time::Duration::from_secs_f64(MAX_AGE - 0.3 + 0.01), "{delay:?}");
    assert!(delay >= std::time::Duration::from_secs_f64(MAX_AGE - 0.3 - 0.01), "{delay:?}");
}

#[test]
fn new_fonts_run_the_part_again() {
    let (ctx, ran) = (on(), Cell::new(0));
    for t in [1.0, 1.1, 1.2] {
        frame(&ctx, t, vec![], 7, "Home", &ran);
    }
    assert_eq!(ran.get(), 2, "settled and replayed");
    // A font arrives (a CJK fallback, a host font) with no input: the recorded glyphs point into
    // the old atlas.
    let mut fonts = egui::FontDefinitions::default();
    fonts.families.entry(egui::FontFamily::Proportional).or_default().reverse();
    ctx.set_fonts(fonts);
    frame(&ctx, 1.3, vec![], 7, "Home", &ran);
    assert_eq!(ran.get(), 3, "the part ran with the new fonts");
}

#[test]
fn a_recording_half_max_age_old_runs_on_the_next_frame() {
    let (ctx, ran) = (on(), Cell::new(0));
    for t in [1.0, 1.1, 1.2] {
        frame(&ctx, t, vec![], 7, "Home", &ran);
    }
    assert_eq!(ran.get(), 2, "settled and replayed");
    // Recorded at 1.1: a frame that comes anyway once it is MAX_AGE / 2 old runs the part, so the
    // refresh rides on that frame and the backstop frame at MAX_AGE is not needed.
    frame(&ctx, 1.1 + MAX_AGE / 2.0 + 0.01, vec![], 7, "Home", &ran);
    assert_eq!(ran.get(), 3);
}

#[test]
fn a_blinking_caret_gets_no_extra_frames_from_the_cache() {
    // The caret draws a frame at each blink edge, every 0.53 s, and asks for the next edge. The
    // cache must not ask for a frame before that one: it would add about one frame a second to
    // every idle window (PERF2 fix round 1 review, N1).
    const EDGE: f64 = 0.53;
    let (ctx, ran) = (on(), Cell::new(0));
    let mut extra = vec![];
    for i in 0..12 {
        let t = 1.0 + f64::from(i) * EDGE;
        let delay = repaint_delay(&ctx, t, &ran);
        if delay < std::time::Duration::from_secs_f64(EDGE - 0.01) {
            extra.push((t, delay));
        }
    }
    assert!(extra.is_empty(), "frames asked for before the next caret edge: {extra:?}");
}

use std::cell::Cell;

use egui::{Context, Event, Id, RawInput, pos2, vec2};

use super::{BACKSTOP_GRACE, MAX_AGE, counts, hold_presses, set_enabled, show};

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
    // Recorded at 1.2: the backstop is due MAX_AGE + BACKSTOP_GRACE after that, 0.3 s from now.
    let want = MAX_AGE + BACKSTOP_GRACE - 0.3;
    assert!(delay <= std::time::Duration::from_secs_f64(want + 0.01), "{delay:?}");
    assert!(delay >= std::time::Duration::from_secs_f64(want - 0.01), "{delay:?}");
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
fn a_recording_older_than_the_caret_blink_is_still_replayed() {
    let (ctx, ran) = (on(), Cell::new(0));
    for t in [1.0, 1.1, 1.2] {
        frame(&ctx, t, vec![], 7, "Home", &ran);
    }
    assert_eq!(ran.get(), 2, "settled and replayed");
    // Recorded at 1.1: the next caret edge comes 0.53 s later. It must replay, or the bars are
    // rebuilt at every edge of an idle window (PERF2 fix round 2 review, F1).
    frame(&ctx, 1.1 + 0.53, vec![], 7, "Home", &ran);
    assert_eq!(ran.get(), 2, "replayed");
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
        // A new context asks for frames of its own while it lays out its first two frames.
        if i >= 2 && delay < std::time::Duration::from_secs_f64(EDGE - 0.01) {
            extra.push((t, delay));
        }
    }
    assert!(extra.is_empty(), "frames asked for before the next caret edge: {extra:?}");
    // And the bars do not run at every edge: with the cache off this part runs 12 times
    // (PERF2 fix round 2 review, F2).
    assert!(ran.get() <= 7, "the part ran {} times over 12 caret edges", ran.get());
}

// ---- LI3: a press that arrives with the pointer must still click a replayed part ----

/// One frame through [`hold_presses`], as the app's `raw_input_hook` runs it: a top panel holding a
/// cached part with a "Copy" button. `clicks` counts its clicks, `button` its rectangle.
fn hooked(ctx: &Context, time: f64, events: Vec<Event>, clicks: &Cell<u32>, button: &Cell<egui::Rect>, ran: &Cell<u32>) {
    let mut input =
        RawInput { time: Some(time), screen_rect: Some(egui::Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0))), events, ..Default::default() };
    hold_presses(ctx, &mut input);
    let out = ctx.run_ui(input, |ui| {
        egui::Panel::top("part").exact_size(40.0).show(ui, |ui| {
            show(ui, id(), 7, |ui| {
                ran.set(ran.get() + 1);
                let r = ui.button("Copy");
                button.set(r.rect);
                if r.clicked() {
                    clicks.set(clicks.get() + 1);
                }
            });
        });
    });
    out.drop_without_applying_deltas();
}

type Rig = (Context, Cell<u32>, Cell<egui::Rect>, Cell<u32>, f64);

/// A context whose part has settled and is replayed, and where its button is.
fn replaying() -> Rig {
    let (ctx, clicks, button, ran) = (on(), Cell::new(0), Cell::new(egui::Rect::NOTHING), Cell::new(0));
    // The pointer starts in the page, away from the part.
    let mut t = 1.0;
    hooked(&ctx, t, vec![Event::PointerMoved(pos2(400.0, 300.0))], &clicks, &button, &ran);
    for _ in 0..4 {
        t += 0.05;
        hooked(&ctx, t, vec![], &clicks, &button, &ran);
    }
    assert!(counts(&ctx, id()).1 >= 2, "the part is replayed before the click: {:?}", counts(&ctx, id()));
    (ctx, clicks, button, ran, t)
}

fn press(pos: egui::Pos2) -> Event {
    Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, modifiers: Default::default() }
}

fn release(pos: egui::Pos2) -> Event {
    Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, modifiers: Default::default() }
}

#[test]
fn a_click_that_arrives_with_the_pointer_still_clicks_a_replayed_button() {
    let (ctx, clicks, button, ran, mut t) = replaying();
    let at = button.get().center();
    t += 0.05;
    hooked(&ctx, t, vec![Event::PointerMoved(at), press(at), release(at)], &clicks, &button, &ran);
    for _ in 0..2 {
        t += 0.05;
        hooked(&ctx, t, vec![], &clicks, &button, &ran);
    }
    assert_eq!(clicks.get(), 1, "the click was lost");
}

#[test]
fn a_tap_that_arrives_with_the_finger_still_clicks_a_replayed_button() {
    let (ctx, clicks, button, ran, mut t) = replaying();
    let at = button.get().center();
    let touch = |phase| Event::Touch { device_id: egui::TouchDeviceId(1), id: egui::TouchId(1), phase, pos: at, force: None };
    t += 0.05;
    let events = vec![touch(egui::TouchPhase::Start), press(at), touch(egui::TouchPhase::End), release(at), Event::PointerGone];
    hooked(&ctx, t, events, &clicks, &button, &ran);
    for _ in 0..2 {
        t += 0.05;
        hooked(&ctx, t, vec![], &clicks, &button, &ran);
    }
    assert_eq!(clicks.get(), 1, "the tap was lost");
}

#[test]
fn a_press_with_the_pointer_already_over_the_button_is_not_delayed() {
    let (ctx, clicks, button, ran, mut t) = replaying();
    let at = button.get().center();
    t += 0.05;
    hooked(&ctx, t, vec![Event::PointerMoved(at)], &clicks, &button, &ran);
    t += 0.05;
    hooked(&ctx, t, vec![press(at), release(at)], &clicks, &button, &ran);
    t += 0.05;
    hooked(&ctx, t, vec![], &clicks, &button, &ran);
    assert_eq!(clicks.get(), 1, "a hovered part took the click as before");
}

#[test]
fn a_press_outside_every_replayed_part_is_passed_on_at_once() {
    let (ctx, ..) = replaying();
    let away = pos2(500.0, 400.0);
    let mut input = RawInput { events: vec![Event::PointerMoved(away), press(away), release(away)], ..Default::default() };
    hold_presses(&ctx, &mut input);
    assert_eq!(input.events, vec![Event::PointerMoved(away), press(away), release(away)]);
}

#[test]
fn a_held_press_keeps_the_order_of_the_events_and_is_let_go_one_frame_later() {
    let (ctx, clicks, button, ran, mut t) = replaying();
    let at = button.get().center();
    let (before, key) = (Event::Text("a".into()), Event::Text("b".into()));
    let mut input = RawInput { events: vec![before.clone(), press(at), release(at), key.clone()], ..Default::default() };
    hold_presses(&ctx, &mut input);
    assert_eq!(input.events, vec![before, Event::PointerMoved(at)], "the pointer moves through, the press waits");
    t += 0.05;
    hooked(&ctx, t, input.events, &clicks, &button, &ran);
    let mut next = RawInput::default();
    hold_presses(&ctx, &mut next);
    assert_eq!(next.events, vec![press(at), release(at), key], "the held events come back in order");
    let mut later = RawInput::default();
    hold_presses(&ctx, &mut later);
    assert!(later.events.is_empty(), "and only once");
}

#[test]
fn quiet_frames_and_pointer_moves_elsewhere_still_replay_with_the_hook_on() {
    let (ctx, clicks, button, ran, mut t) = replaying();
    let before = ran.get();
    for _ in 0..6 {
        t += 0.05;
        hooked(&ctx, t, vec![Event::PointerMoved(pos2(400.0, 300.0))], &clicks, &button, &ran);
    }
    assert_eq!(ran.get(), before, "no frame ran the part");
}

/// Frames of a top panel holding the part (`inside`: a text field in it that has the keyboard, or
/// a label) over a page whose canvas has the keyboard and is as tall as the window, so its
/// rectangle reaches the part's.
fn focus_frame(ctx: &Context, time: f64, inside: bool, ran: &Cell<u32>) {
    let input = RawInput { time: Some(time), screen_rect: Some(egui::Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0))), ..Default::default() };
    let out = ctx.run_ui(input, |ui| {
        egui::Panel::top("part").exact_size(40.0).show(ui, |ui| {
            show(ui, id(), 7, |ui| {
                ran.set(ran.get() + 1);
                if inside {
                    let mut text = String::from("x");
                    let r = ui.add(egui::TextEdit::singleline(&mut text).id(Id::new("field")));
                    r.request_focus();
                } else {
                    ui.label("Home");
                }
            });
        });
        if !inside {
            let page = ui.max_rect();
            let r = ui.interact(page.union(egui::Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0))), Id::new("page"), egui::Sense::focusable_noninteractive());
            r.request_focus();
        }
    });
    out.drop_without_applying_deltas();
}

#[test]
fn a_focused_widget_outside_the_part_does_not_keep_it_running() {
    let (ctx, ran) = (on(), Cell::new(0));
    for i in 0..4 {
        focus_frame(&ctx, 1.0 + f64::from(i) * 0.1, false, &ran);
    }
    let before = ran.get();
    for i in 4..10 {
        focus_frame(&ctx, 1.0 + f64::from(i) * 0.1, false, &ran);
    }
    assert_eq!(ran.get(), before, "the page's keyboard focus overlaps the bar but is not in it (PERF2 fix round 3)");
}

#[test]
fn a_focused_widget_inside_the_part_keeps_it_running() {
    let (ctx, ran) = (on(), Cell::new(0));
    for i in 0..10 {
        focus_frame(&ctx, 1.0 + f64::from(i) * 0.1, true, &ran);
    }
    assert_eq!(ran.get(), 10, "a text field with the keyboard in the part runs it every frame");
}

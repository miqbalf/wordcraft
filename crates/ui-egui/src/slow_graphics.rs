//! A one-time notice when the browser draws without graphics acceleration (PERF2).
//!
//! A browser with its GPU switched off (or blocked) falls back to a software renderer:
//! SwiftShader, llvmpipe, or one that names itself "Software". Every frame then costs many times
//! more, and typing feels slow for a reason the person can fix in the browser's settings. The host
//! hands the renderer's name to [`renderer`] once it knows it; [`show`] then puts a small notice
//! at the bottom of the window until it is dismissed.

use egui::{Align2, Context, Id, vec2};

/// The notice, in English (the lookup key).
pub const SLOW_GRAPHICS: &str = "Graphics acceleration is off in this browser, so editing may be slow.";

/// Where the notice stands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    /// The renderer is not known yet.
    #[default]
    Unknown,
    /// A software renderer: the notice shows.
    Shown,
    /// A hardware renderer, or the notice was dismissed: nothing shows again.
    Done,
}

fn id() -> Id {
    Id::unique("slow_graphics_notice")
}

/// Whether a renderer's name is a software renderer's (Windows' WARP is the "Microsoft Basic
/// Render Driver").
pub fn is_software(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    ["swiftshader", "llvmpipe", "software", "basic render driver"].iter().any(|s| name.contains(s))
}

/// The browser draws with `name` (`cpu`: the graphics API itself says it is a CPU device). Only
/// the first call counts, so the notice shows at most once.
pub fn renderer(ctx: &Context, name: &str, cpu: bool) {
    ctx.data_mut(|d| {
        let state = d.get_temp_mut_or_default::<State>(id());
        if *state == State::Unknown {
            *state = if cpu || is_software(name) { State::Shown } else { State::Done };
        }
    });
}

/// Whether the notice is showing.
pub fn showing(ctx: &Context) -> bool {
    ctx.data(|d| d.get_temp::<State>(id())) == Some(State::Shown)
}

/// Close the notice for good.
pub fn dismiss(ctx: &Context) {
    ctx.data_mut(|d| d.insert_temp(id(), State::Done));
}

/// Draw the notice, if it is showing, above the status bar.
pub fn show(ctx: &Context) {
    if !showing(ctx) {
        return;
    }
    egui::Area::new(id()).order(egui::Order::Foreground).anchor(Align2::CENTER_BOTTOM, vec2(0.0, -36.0)).show(ctx, |ui| {
        egui::Frame::popup(ui.style()).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(crate::tl!(SLOW_GRAPHICS));
                if ui.button(crate::tl!("Dismiss")).clicked() {
                    dismiss(ui.ctx());
                }
            });
        });
    });
}

#[cfg(test)]
mod tests;

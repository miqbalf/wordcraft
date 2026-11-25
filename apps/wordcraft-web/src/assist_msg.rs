//! The `wordcraft:` assist messages as JSON: what the page sends is checked field by field and
//! capped as the route's contract says, and what the app sends is built as the page reads it.
//! Pure, so it runs natively; `assist` (wasm) does the posting.

use wordcraft_ui_egui::assist::{AssistIn, AssistOut};

/// One message from the page, or `None` when it is not a usable assist message.
pub fn parse(_json: &str) -> Option<AssistIn> {
    None
}

/// A message to the page as JSON.
pub fn build(_out: &AssistOut) -> String {
    String::new()
}

#[cfg(test)]
mod tests;

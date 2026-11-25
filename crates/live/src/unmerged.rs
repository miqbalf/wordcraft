//! The unmerged copy: local edits the room does not have, kept so the person loses nothing.
//!
//! craft-live hands such edits over ([`craft_live::LiveApp::unmerged`]) before it drops them: a
//! replica given up or replaced by the room's snapshot, an edit the relay refused for good, or a
//! link that broke with edits still waiting. WordCraft also keeps one when too much waits to be
//! sent ([`FULL_TEXT`]): new edits are refused then, and the copy holds everything typed so far.
//!
//! A copy is only ever offered: the host shows it until the person saves it, and never saves it
//! on its own. Each copy is the `.docx` of the room's file with the kept edits applied; a kept
//! state that cannot be built into a document is offered as it is, a recovery file (`.yrs`).

use wordcraft_doc::Document;

use crate::session::document_from_state;

/// Told when a copy is ready because edits could not be shared.
pub const UNMERGED_TEXT: &str = "Some of your changes could not be shared with the other editors. A copy with them is ready to save.";

/// Told when too much waits to be sent: new edits are refused, and a copy holds what was typed.
pub const FULL_TEXT: &str =
    "Too many changes are waiting to be sent to the other editors. A copy with them is ready to save; editing resumes once they are sent.";

/// One copy of kept edits, for the host to offer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnmergedCopy {
    /// The `.docx` document, or for a recovery file the replica's state as it was kept.
    pub bytes: Vec<u8>,
    /// A recovery file (`.yrs`), not a document.
    pub recovery: bool,
}

impl UnmergedCopy {
    /// The file extension the copy is saved with.
    pub fn ext(&self) -> &'static str {
        if self.recovery { "yrs" } else { "docx" }
    }
}

/// The copies `states` build into on `base` (the room's file, for what is not shared): one
/// document with every state merged when that builds, else one per state that builds and a
/// recovery file for each one that does not. Nothing kept is dropped, a panic included.
pub(crate) fn build(base: &Document, states: &[Vec<u8>]) -> Vec<UnmergedCopy> {
    if states.is_empty() {
        return Vec::new();
    }
    if let Ok(bytes) = merged_bytes(base, states) {
        return vec![UnmergedCopy { bytes, recovery: false }];
    }
    states
        .iter()
        .map(|s| match copy_bytes(base, s) {
            Ok(bytes) => UnmergedCopy { bytes, recovery: false },
            Err(e) => {
                log::warn!("wordcraft-live: a kept state does not build into a document ({e}); it is offered as a recovery file");
                UnmergedCopy { bytes: s.clone(), recovery: true }
            }
        })
        .collect()
}

/// The `.docx` bytes of `base` with every state applied (updates of one document, so their merge
/// holds each one's edits).
fn merged_bytes(base: &Document, states: &[Vec<u8>]) -> Result<Vec<u8>, String> {
    match states {
        [one] => copy_bytes(base, one),
        _ => {
            let parts: Vec<&[u8]> = states.iter().map(Vec::as_slice).collect();
            let merged = std::panic::catch_unwind(|| craft_collab::yrs::merge_updates_v1(&parts))
                .map_err(|_| "merging the states panicked".to_owned())?
                .map_err(|e| e.to_string())?;
            copy_bytes(base, &merged)
        }
    }
}

/// The `.docx` bytes of `base` with `state` applied. A panic while building is an error here.
fn copy_bytes(base: &Document, state: &[u8]) -> Result<Vec<u8>, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let doc = document_from_state(base, state).map_err(|e| e.to_string())?;
        wordcraft_engine::io::save_bytes("copy.docx", &doc)
    }))
    .unwrap_or_else(|_| Err("building the document panicked".to_owned()))
}

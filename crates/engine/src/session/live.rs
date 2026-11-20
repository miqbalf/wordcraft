//! Entry points for a live co-editing layer: it watches local edits through an
//! [`EditObserver`], applies a peer's edits with [`Session::apply_remote`] and takes over Undo
//! and Redo with [`Session::live_undo_hook`].

use super::doc_state::Selection;
use super::{CmdError, Document, Pos, Session, StoryRef};

/// Watches the local edits of a session ([`Session::set_observer`]).
///
/// `edited` is called once for each top-level command that changed the document (typing,
/// a paragraph split, formatting, an input method's commit…), after it ran; a command that
/// changed nothing, a failed command and the commands a command runs inside it are not
/// reported. `before` is the document the command started from and `after` the one it left.
/// [`Session::apply_remote`] is not reported, so a peer's edit is never echoed back.
pub trait EditObserver: Send {
    fn edited(&mut self, before: &Document, after: &Document, command: &str);
}

/// The live undo hook: called with `redo` false for Undo and true for Redo.
pub(crate) type LiveUndo = Box<dyn FnMut(bool) -> bool>;

/// Where a selection end is, by what a peer's edit keeps: the paragraph's shared id, its
/// index among the story's paragraphs and the offset in characters.
struct Anchor {
    story: StoryRef,
    pid: u64,
    index: usize,
    chars: usize,
    pos: Pos,
}

impl Anchor {
    fn of(doc: &Document, pos: &Pos) -> Anchor {
        let index = doc.para_paths(pos.story).iter().position(|p| *p == pos.path).unwrap_or(0);
        let (pid, chars) = match doc.para_at(pos) {
            Some(p) => (p.pid, p.text.get(..pos.off).map_or(0, |t| t.chars().count())),
            None => (0, 0),
        };
        Anchor { story: pos.story, pid, index, chars, pos: pos.clone() }
    }

    /// The same place in `doc`: the same character of the paragraph with the same shared id;
    /// when that paragraph is gone, the start of the paragraph now at the same index. An
    /// unshared paragraph (id 0) keeps its path. The result is clamped to the document.
    fn find(&self, doc: &Document) -> Pos {
        if self.pid == 0 {
            return doc.clamp(&self.pos);
        }
        let paths = doc.para_paths(self.story);
        let found = paths.iter().find_map(|path| doc.para(self.story, path).filter(|p| p.pid == self.pid).map(|p| (path, p)));
        let pos = match found {
            Some((path, p)) => {
                let off = p.text.char_indices().nth(self.chars).map_or(p.text.len(), |(i, _)| i);
                Pos::new(self.story, path.clone(), off)
            }
            None => match paths.get(self.index.min(paths.len().saturating_sub(1))) {
                Some(path) => Pos::new(self.story, path.clone(), 0),
                None => doc.start_of(StoryRef::Body),
            },
        };
        doc.clamp(&pos)
    }
}

impl Session {
    /// Install (or with `None`, remove) the observer that sees every local edit
    /// ([`EditObserver`]). Without one, a command pays one branch for it.
    pub fn set_observer(&mut self, o: Option<Box<dyn EditObserver>>) {
        self.observer = o;
    }

    /// Install (or remove) the live undo hook. While it is set, Undo and Redo call it (with
    /// `true` for Redo) instead of putting back the session's own snapshots: the live layer
    /// owns undo. What it returns is whether it undid something; either way the snapshot undo
    /// does not run.
    pub fn live_undo_hook(&mut self, h: Option<Box<dyn FnMut(bool) -> bool>>) {
        self.live_undo = h;
    }

    /// Whether the live undo hook took this Undo (or Redo): `Some` with its answer when set.
    pub(super) fn run_live_undo(&mut self, redo: bool) -> Option<bool> {
        self.live_undo.as_mut().map(|h| h(redo))
    }

    /// Apply a peer's edit to the document. It adds no undo step and is not reported to the
    /// observer. The selection stays on the same characters of the same shared paragraphs
    /// (see [`Paragraph::pid`](wordcraft_doc::Paragraph::pid)); a selection end whose
    /// paragraph is gone goes to the start of the paragraph now at its index. The layout
    /// follows on the next query, as after a command. A remote edit alone does not set
    /// [`Session::dirty`]: the document has no unsaved local changes because of it.
    ///
    /// `f` should change paragraphs through their own methods (or call
    /// [`Paragraph::touch`](wordcraft_doc::Paragraph::touch)) so the layout cache sees them. If
    /// `f` panics the document is left as it was and the panic is returned as an error.
    pub fn apply_remote(&mut self, f: impl FnOnce(&mut Document)) -> Result<(), CmdError> {
        let anchor = Anchor::of(&self.doc, &self.sel.anchor);
        let focus = Anchor::of(&self.doc, &self.sel.focus);
        // Cheap: the document's blocks are shared, so this copies a pointer per block.
        let before = self.doc.clone();
        let applied = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&mut self.doc)));
        if let Err(p) = applied {
            self.doc = before;
            let msg = p.downcast_ref::<&str>().map(ToString::to_string).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_default();
            return Err(CmdError::Failed(format!("remote edit panicked: {msg}")));
        }
        self.doc.ensure_nonempty();
        self.sel = Selection { anchor: anchor.find(&self.doc), focus: focus.find(&self.doc) };
        // Selections that name places the peer's edit may have moved are dropped.
        self.column = None;
        self.widened = None;
        self.also_selected.clear();
        self.member = None;
        // The next local keystroke starts its own undo step rather than one that spans the
        // peer's edit.
        self.typing_open = false;
        self.clamp_selection();
        let dirty = self.dirty;
        self.touch();
        self.dirty = dirty;
        // The input method has to see the document changed under it.
        self.ime_state_seen = Some(self.current_ime_caret_state());
        self.reset_ime_state_revision();
        self.pending_ime_application_change = true;
        Ok(())
    }
}

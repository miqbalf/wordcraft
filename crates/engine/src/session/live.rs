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
pub(crate) type LiveUndo = Box<dyn FnMut(bool) -> bool + Send>;

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
    /// Bind the live hooks to the active document, or unbind them once none is installed.
    fn bind_live(&mut self) {
        self.live_doc = (self.observer.is_some() || self.live_undo.is_some()).then_some(self.document_id);
    }

    /// Whether the live hooks are bound to the active document.
    fn live_here(&self) -> bool {
        self.live_doc == Some(self.document_id)
    }

    /// Install (or with `None`, remove) the observer that sees every local edit
    /// ([`EditObserver`]). It is bound to the active document: while another document is
    /// active, or after [`Session::set_document`] replaced this one, it hears nothing. Without
    /// one, a command pays one branch for it.
    pub fn set_observer(&mut self, o: Option<Box<dyn EditObserver>>) {
        self.observer = o;
        self.bind_live();
    }

    /// Install (or remove) the live undo hook, bound to the active document like the observer.
    /// While it is set and that document is active, Undo, Redo and Undo Actions call it (with
    /// `true` for Redo) instead of putting back the session's own snapshots: the live layer
    /// owns undo. What it returns is whether it undid something; either way the snapshot undo
    /// does not run. Undo and Redo are then always enabled, and the snapshot stack's depth and
    /// protection check do not apply: the hook decides.
    pub fn live_undo_hook(&mut self, h: Option<Box<dyn FnMut(bool) -> bool + Send>>) {
        self.live_undo = h;
        self.bind_live();
    }

    /// Whether the live undo hook owns Undo and Redo now.
    pub(crate) fn live_undo_active(&self) -> bool {
        self.live_undo.is_some() && self.live_here()
    }

    /// Whether the live undo hook took this Undo (or Redo): `Some` with its answer when it owns
    /// undo now.
    pub(super) fn run_live_undo(&mut self, redo: bool) -> Option<bool> {
        if !self.live_here() {
            return None;
        }
        self.live_undo.as_mut().map(|h| h(redo))
    }

    /// Apply a peer's edit to the document the live hooks are bound to (an error while another
    /// document is active). It is not reported to the observer, and it adds no undo step: it
    /// clears the session's own undo and redo snapshots instead, since putting one of them back
    /// would erase the peer's edit. The selection stays on the same characters of the same
    /// shared paragraphs (see [`Paragraph::pid`](wordcraft_doc::Paragraph::pid)); a selection
    /// end whose paragraph is gone goes to the start of the paragraph now at its index. Every
    /// block `f` replaced has its paragraphs touched, so the layout follows on the next query.
    /// A remote edit alone does not set [`Session::dirty`]. If `f` panics the document is left
    /// as it was and the panic is returned as an error.
    pub fn apply_remote(&mut self, f: impl FnOnce(&mut Document)) -> Result<(), CmdError> {
        if self.live_doc.is_some() && !self.live_here() {
            return Err(CmdError::Disabled("remote edit: the shared document is not the active one".into()));
        }
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
        self.doc.touch_changed_since(&before);
        drop(before);
        self.doc.ensure_nonempty();
        self.sel = Selection { anchor: anchor.find(&self.doc), focus: focus.find(&self.doc) };
        // Selections that name places the peer's edit may have moved are dropped.
        self.column = None;
        self.widened = None;
        self.also_selected.clear();
        self.member = None;
        // An old snapshot put back by Undo or Redo would erase the peer's edit.
        self.reset_history();
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

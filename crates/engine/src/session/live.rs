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
    /// The paragraph's text then, to follow the characters through the peer's edit.
    text: String,
    pos: Pos,
}

/// Where char `at` of `old` is in `new`: the edit between them is taken as the part outside
/// their common prefix and suffix. A position before it stays, one after it moves by what the
/// edit added or removed, and one inside text the edit replaced goes to the start of the edit.
fn follow(old: &str, new: &str, at: usize) -> usize {
    if old == new {
        return at;
    }
    let prefix = old.chars().zip(new.chars()).take_while(|(a, b)| a == b).count();
    let (old_len, new_len) = (old.chars().count(), new.chars().count());
    let room = old_len.min(new_len).saturating_sub(prefix);
    let suffix = old.chars().rev().zip(new.chars().rev()).take(room).take_while(|(a, b)| a == b).count();
    let old_end = old_len.saturating_sub(suffix);
    if at <= prefix {
        at
    } else if at >= old_end {
        (at + new_len).saturating_sub(old_len)
    } else {
        prefix
    }
}

/// How alike two texts are: the length of their common prefix plus their common suffix.
fn likeness(a: &str, b: &str) -> usize {
    let prefix = a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count();
    let suffix = a.chars().rev().zip(b.chars().rev()).take_while(|(x, y)| x == y).count();
    prefix + suffix
}

impl Anchor {
    fn of(doc: &Document, pos: &Pos) -> Anchor {
        let index = doc.para_paths(pos.story).iter().position(|p| *p == pos.path).unwrap_or(0);
        let (pid, chars, text) = match doc.para_at(pos) {
            Some(p) => (p.pid, p.text.get(..pos.off).map_or(0, |t| t.chars().count()), p.text.clone()),
            None => (0, 0, String::new()),
        };
        Anchor { story: pos.story, pid, index, chars, text, pos: pos.clone() }
    }

    /// The same character in the paragraphs from index `index` on whose texts, joined, start
    /// like the old paragraph's text up to it: the head and the tail of the paragraph split
    /// under it. `None` when they do not.
    ///
    /// When those paragraphs end before the character (this window typed there after the peer
    /// split the paragraph, and the peer has not passed that text on yet), the end of the last
    /// of them, with that text and the id of that paragraph: the caret waits there for it.
    fn in_split(&self, doc: &Document, paths: &[super::Path], index: usize) -> Option<(Pos, Option<(u64, String)>)> {
        let mut old = self.text.chars();
        let mut before = 0;
        let mut last: Option<(&super::Path, usize, u64)> = None;
        for path in paths.get(index..)? {
            let para = doc.para(self.story, path);
            let text = para.map_or("", |p| p.text.as_str());
            let n = text.chars().count();
            // The paragraph's text first: when it runs out, no char of the old text is used up.
            let same = text.chars().zip(old.by_ref()).take_while(|(a, b)| a == b).count();
            if self.chars <= before + same {
                let off = text.char_indices().nth(self.chars - before).map_or(text.len(), |(i, _)| i);
                return Some((Pos::new(self.story, path.clone(), off), None));
            }
            if same < n {
                if same > 0 {
                    return None;
                }
                break;
            }
            before += n;
            last = Some((path, text.len(), para.map_or(0, |p| p.pid)));
        }
        let (path, end, pid) = last.filter(|_| before > 0)?;
        let awaited: String = self.text.chars().skip(before).take(self.chars - before).collect();
        Some((Pos::new(self.story, path.clone(), end), Some((pid, awaited))))
    }

    /// The same place in `doc`: the same character of the paragraph with the same shared id
    /// (followed through the edit to its text, see [`follow`]); when two paragraphs have that
    /// id, the one whose text is most like the old one; when that paragraph is gone, the same
    /// character in the paragraphs from its index on when their texts, joined, start like the
    /// old text up to it (the head and tail of a split), otherwise the start of the paragraph now
    /// at its index. An unshared paragraph (id 0) keeps its path. The
    /// result is clamped to the document.
    fn find(&self, doc: &Document) -> (Pos, Option<(u64, String)>) {
        if self.pid == 0 {
            return (doc.clamp(&self.pos), None);
        }
        let paths = doc.para_paths(self.story);
        let found = paths
            .iter()
            .filter_map(|path| doc.para(self.story, path).filter(|p| p.pid == self.pid).map(|p| (path, p)))
            .rev()
            .max_by_key(|(_, p)| likeness(&p.text, &self.text));
        let mut awaits = None;
        let pos = match found {
            Some((path, p)) => {
                let chars = follow(&self.text, &p.text, self.chars);
                let off = p.text.char_indices().nth(chars).map_or(p.text.len(), |(i, _)| i);
                Pos::new(self.story, path.clone(), off)
            }
            None => {
                let index = self.index.min(paths.len().saturating_sub(1));
                match paths.get(index) {
                    Some(path) => match self.in_split(doc, &paths, index) {
                        Some((pos, waits)) => {
                            awaits = waits;
                            pos
                        }
                        None => Pos::new(self.story, path.clone(), 0),
                    },
                    None => doc.start_of(StoryRef::Body),
                }
            }
        };
        (doc.clamp(&pos), awaits)
    }
}

impl Session {
    /// Bind the live hooks to the active document when the first of them is installed, and
    /// unbind them once both are gone. Installing or removing one while the other stays never
    /// moves the binding, so it cannot follow the user to another document.
    fn bind_live(&mut self) {
        if self.observer.is_none() && self.live_undo.is_none() {
            self.live_doc = None;
        } else if self.live_doc.is_none() {
            self.live_doc = Some(self.document_id);
        }
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

    /// Give paragraphs their shared ids ([`Paragraph::pid`](wordcraft_doc::Paragraph::pid))
    /// with `f`, which must change nothing else. It is not an edit: not reported to the
    /// observer, no undo step, no unsaved changes, and the undo history stays. When `f` changed
    /// anything but ids, or panicked, the document is put back and the result is an error.
    pub fn assign_pids(&mut self, f: impl FnOnce(&mut Document)) -> Result<(), CmdError> {
        let before = self.doc.clone();
        let applied = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&mut self.doc)));
        // Ids are not content, so the same content means only ids changed.
        if applied.is_err() || !before.same_content(&self.doc) {
            self.doc = before;
            return Err(CmdError::Failed("assign ids: only paragraph ids may change".into()));
        }
        Ok(())
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
        let ((a, _), (f, awaits)) = (anchor.find(&self.doc), focus.find(&self.doc));
        let mut sel = Selection { anchor: a, focus: f };
        // A caret left waiting at the end of a split paragraph's part (see `Anchor::in_split`)
        // for this window's own text: when that text has come right after it, it goes past it,
        // where this window typed it.
        // It keeps waiting through other edits while it stays in that paragraph.
        let mut still = None;
        if let Some((pid, text)) = self.live_awaits.take().filter(|_| sel.anchor == sel.focus) {
            let here = self.doc.para_at(&sel.focus).filter(|p| p.pid == pid).map(|p| p.text.get(sel.focus.off..).unwrap_or(""));
            let was = focus.text.get(focus.pos.off..).unwrap_or("");
            if here.is_some_and(|t| t.starts_with(&text)) && !was.starts_with(&text) {
                sel.focus.off += text.len();
                sel.anchor = sel.focus.clone();
            } else if here.is_some() {
                still = Some((pid, text));
            }
        }
        if sel.anchor == sel.focus {
            self.live_awaits = awaits.or(still);
        }
        self.sel = sel;
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

//! Which editing commands live editing shares in phase 1.

/// The editing commands whose effect live editing shares: typing and deleting text, splitting
/// and joining paragraphs, character formatting, paragraph styles and paragraph formatting,
/// moving paragraphs, cut and paste, and Undo and Redo (which the live layer owns while live).
///
/// A paste may bring what is not shared (a picture, a new style or list definition): its edit is
/// refused like any other, and [`LiveEditing::run`](crate::LiveEditing::run) pastes its text
/// instead ([`PASTE_FALLS_BACK`]). A cut that would remove an object is refused like Delete.
///
/// Left out, because what they change is not shared in phase 1: inline objects (pictures,
/// shapes, fields, equations, bookmarks, note and comment marks), media, numbering definitions
/// (bullets and numbering), notes, comments, tracked changes, headers and footers, character
/// styles (they may add a style definition), the other paste commands (Paste Special pastes
/// pictures and other formats; Paste All and the Spike paste several pieces), and any table
/// edit.
pub const SHAREABLE: &[&str] = &[
    // Text.
    "text.insert",
    "text.delete",
    "text.backspace",
    "text.deleteWordBack",
    "text.deleteWordForward",
    "text.deleteSentenceBack",
    "text.newParagraph",
    "text.lineBreak",
    "text.pageBreak",
    "text.columnBreak",
    "text.tab",
    "text.nbsp",
    "text.nbHyphen",
    "text.optionalHyphen",
    "text.emDash",
    "text.ellipsis",
    "text.copyright",
    "text.registered",
    "text.trademark",
    // Character formatting.
    "format.bold",
    "format.italic",
    "format.underline",
    "format.doubleUnderline",
    "format.wordUnderline",
    "format.strikethrough",
    "format.doubleStrikethrough",
    "format.subscript",
    "format.superscript",
    "format.allCaps",
    "format.smallCaps",
    "format.hidden",
    "format.outline",
    "format.shadow",
    "format.emboss",
    "format.engrave",
    "format.font",
    "format.size",
    "format.growFont",
    "format.shrinkFont",
    "format.color",
    "format.underlineColor",
    "format.highlight",
    "format.shading",
    "format.changeCase",
    "format.clear",
    "format.resetChar",
    "format.spacing",
    "format.scale",
    "format.position",
    // Paragraph styles and formatting.
    "para.style",
    "para.normal",
    "para.alignLeft",
    "para.alignCenter",
    "para.alignRight",
    "para.justify",
    "para.distribute",
    "para.align",
    "para.rtl",
    "para.ltr",
    "para.indent",
    "para.outdent",
    "para.hangingIndent",
    "para.removeHanging",
    "para.indents",
    "para.lineSpacing",
    "para.single",
    "para.double",
    "para.oneAndHalf",
    "para.spacing",
    "para.addSpaceBefore",
    "para.removeSpaceAfter",
    "para.reset",
    "para.keepNext",
    "para.keepLines",
    "para.pageBreakBefore",
    "para.widowControl",
    "para.shading",
    "para.borders",
    "para.outlineLevel",
    "para.moveUp",
    "para.moveDown",
    // Cut and paste.
    "edit.cut",
    "edit.paste",
    "edit.pasteText",
    "edit.pasteMerge",
    "edit.pasteClipboardItem",
    // History, owned by the live layer while live.
    "edit.undo",
    "edit.redo",
];

/// Whether live editing shares what the editing command `command_id` changes ([`SHAREABLE`]).
///
/// It classifies commands that change the document. Commands that change nothing (moving the
/// caret, selecting, the view, `CommandSpec::mutates` false) need no check. The app disables
/// every other editing command while live. [`LiveMap::local_updates`](crate::LiveMap::local_updates)
/// still refuses an edit that changes what is not shared, as a safety net.
pub fn is_shareable(command_id: &str) -> bool {
    SHAREABLE.contains(&command_id)
}

/// The paste commands that, when what they paste is not shared, paste its text instead.
pub const PASTE_FALLS_BACK: &[&str] = &["edit.paste", "edit.pasteMerge", "edit.pasteClipboardItem"];

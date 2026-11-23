//! Save As = Export (`craft_share::export`): one list of every format WordCraft writes, and one
//! function every Save As and Export entry point calls ([`WordApp::save_as_copy`]).
//!
//! Save As is always a copy: it never changes the document's file, name, unsaved changes,
//! password or save stamps. In a browser (plain or framed by a page such as Nextcloud) the copy
//! downloads to the user's computer and nothing goes to the page; on the desktop the system Save
//! dialog asks where to write it. Plain Save is not here: it writes the document's own file (the
//! page's file when hosted), as before.

use craft_share::export::{Delivery, ExportFormat, ExportList, Runtime};
use serde_json::{Value, json};

use crate::WordApp;
use crate::file_dialogs::{AfterPick, FileDialogRequest};

/// The export id of a Word 2003 XML Document: an `.xml` file saved with `format: "wordml2003"`.
pub(crate) const WORD_2003: &str = "wordml2003";

/// Every format WordCraft writes, in the order File › Save As and File › Export show them. The id
/// is the engine's format (`xml` is a Word XML Document, `wordml2003` a Word 2003 XML Document);
/// the label is the translation key.
pub const SAVE_AS: ExportList = ExportList::new(&[
    ExportFormat::new("pdf", "PDF document (*.pdf)", "pdf", "application/pdf"),
    ExportFormat::new("docx", "Word document (*.docx)", "docx", "application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
    ExportFormat::new("xml", "Word XML Document (*.xml)", "xml", "application/xml"),
    ExportFormat::new(WORD_2003, "Word 2003 XML Document (*.xml)", "xml", "application/xml"),
    ExportFormat::new("odt", "OpenDocument Text (*.odt)", "odt", "application/vnd.oasis.opendocument.text"),
    ExportFormat::new("ott", "OpenDocument Text Template (*.ott)", "ott", "application/vnd.oasis.opendocument.text-template"),
    ExportFormat::new("fodt", "Flat OpenDocument Text (*.fodt)", "fodt", "application/vnd.oasis.opendocument.text-flat-xml"),
    ExportFormat::new("rtf", "Rich Text Format (*.rtf)", "rtf", "application/rtf"),
    ExportFormat::new("html", "Web page (*.html)", "html", "text/html"),
    ExportFormat::new("md", "Markdown (*.md)", "md", "text/markdown"),
    ExportFormat::new("tex", "LaTeX (*.tex)", "tex", "application/x-tex"),
    ExportFormat::new("txt", "Plain text (*.txt)", "txt", "text/plain"),
    ExportFormat::new("png", "Page image (*.png)", "png", "image/png"),
    ExportFormat::new("dotx", "Word template (*.dotx)", "dotx", "application/vnd.openxmlformats-officedocument.wordprocessingml.template"),
])
.replacing(&["doc", "dot", "htm", "docm", "dotm"]);

/// The path an export to format `id` is written to, given the picked one (the built-in browser's
/// typed name, a system dialog's pick): kept when the format accepts its extension, else the
/// format's extension is added (`notes.docx` as PDF is `notes.docx.pdf`). Folders are kept. An
/// unknown id or a name that is no name leaves it as it is.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn export_name(name: &str, id: &str) -> String {
    SAVE_AS.picked_path(name, id).map_or_else(|_| name.to_string(), |p| p.path)
}

/// The file name in the built-in browser's name box once the type changes to format `id`: a
/// known document extension gives way to the format's (`notes.docx` as plain text is
/// `notes.txt`), any other is kept and the format's added.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn retyped_name(name: &str, id: &str) -> String {
    SAVE_AS.file_name(name, "Document", id).unwrap_or_else(|_| name.to_string())
}

/// The extension format `id` writes (`pdf` for an unknown id).
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn export_ext(id: &str) -> &'static str {
    SAVE_AS.get(id).map_or("pdf", |f| f.ext)
}

/// The engine's `format` for an `.xml` copy (Word XML or Word 2003 XML); `None` otherwise.
fn engine_format(id: &str) -> Option<&'static str> {
    match id {
        "xml" => Some(wordcraft_engine::cmd::file::FORMAT_FLAT_OPC),
        WORD_2003 => Some(WORD_2003),
        _ => None,
    }
}

/// The MIME type of a downloaded copy named `name`: its format's, else the shared table's.
pub fn mime_of(name: &str) -> &'static str {
    let ext = std::path::Path::new(name).extension().and_then(|e| e.to_str()).unwrap_or("");
    SAVE_AS.for_ext(ext).map_or_else(|| craft_share::export::mime_for_ext(ext), |f| f.mime)
}

/// The commands a browser page hides ([`WordApp::browser_hides`]).
const BROWSER_HIDDEN: [&str; 6] = ["ui.checkForUpdates", "view.newWindow", "view.arrangeAll", "view.sideBySide", "view.syncScroll", "insert.object"];

/// A Save As or export with a path (a script, an agent, the control channel) writes a copy there:
/// the document is never re-pointed to it. Other commands pass through as they are.
pub(crate) fn as_copy(id: &str, mut params: Value) -> Value {
    if matches!(id, "file.saveAs" | "file.exportPdf" | "file.exportPng")
        && params.get("path").and_then(Value::as_str).is_some()
        && let Some(map) = params.as_object_mut()
    {
        map.insert("copy".into(), json!(true));
    }
    params
}

impl WordApp {
    /// Whether this window is a browser page (one with a download hook, as the web shell and
    /// the browser tests give it): the desktop-only commands (Check for Updates, New Window,
    /// Zotero, Insert Object, Language packs) are hidden there.
    pub fn in_browser(&self) -> bool {
        cfg!(target_arch = "wasm32") || self.services.download_copy.is_some() || self.services.download.is_some()
    }

    /// Whether a browser page hides command `id` (WA F6): Check for Updates (the page is always
    /// the current version), New Window, Arrange All and Side by Side (a page is one window),
    /// Insert › Object (it reads a file by its path) and the Zotero tab's commands (the Zotero
    /// desktop app isn't there).
    pub fn browser_hides(&self, id: &str) -> bool {
        self.in_browser() && (BROWSER_HIDDEN.contains(&id) || id.starts_with("ui.zotero."))
    }

    /// Whether a plain Save must first ask where (the Save As page then offers Save's own
    /// Browse…): a desktop document with no file yet, or only an import-only Word 97–2003 one.
    pub(crate) fn needs_a_file_to_save(&self) -> bool {
        self.runtime() == Runtime::Desktop && (self.session.path.is_none() || self.legacy_word_path())
    }

    /// The Save As and export commands in [`WordApp::execute`]: Save As without a path opens the
    /// Save As list (File › Save As), Export PDF / PNG and Save as Template without one make that
    /// copy ([`Self::save_as_copy`]), and in a browser Save as Template with a name downloads it.
    /// `None`: the command goes on (with a path, as a copy: [`as_copy`]).
    pub(crate) fn save_as_command(&mut self, id: &str, params: &Value) -> Option<Result<Value, String>> {
        let path = params.get("path").and_then(Value::as_str);
        match (id, path) {
            ("file.saveAs", None) => {
                self.ui.backstage = true;
                crate::backstage::go_to_page(self, "saveAs");
                Some(Ok(json!({"open": "saveAs"})))
            }
            ("file.exportPdf", None) => Some(self.save_as_copy("pdf")),
            ("file.exportPng", None) => Some(self.save_as_copy("png")),
            ("file.saveTemplate", None) => Some(self.save_as_copy("dotx")),
            ("file.saveTemplate", Some(name)) if self.runtime() != Runtime::Desktop => {
                // A .dotm keeps its name, as the engine's Save as Template does; anything else is a .dotx.
                let name = if wordcraft_engine::cmd::user_templates::is_template_name(name) { name.to_string() } else { format!("{name}.dotx") };
                Some(self.web_copy(&json!({"path": name, "copy": true})))
            }
            _ => None,
        }
    }

    /// Browser: download a copy (`params` as `file.saveAs`'s, with `copy`) through the browser
    /// download, never the page: the document stays as it was.
    pub(crate) fn web_copy(&mut self, params: &Value) -> Result<Value, String> {
        self.web_save("file.saveAs", params)
    }

    /// Where Save As copies go: a browser window (one with a download hook) downloads them,
    /// framed by a page that keeps the files ([`crate::Services::hosted`]) or not; the desktop
    /// asks with its Save dialog.
    pub fn runtime(&self) -> Runtime {
        if self.in_browser() { Runtime::web(self.services.hosted || self.services.host_save.is_some()) } else { Runtime::Desktop }
    }

    /// Save As = Export: a copy of the document in format `id` ([`SAVE_AS`]). Every Save As and
    /// Export entry point comes here: the File › Save As and Export pages, their keytips, F12 and
    /// Cmd+Shift+S (through the Save As page), Export PDF / PNG and Save as Template from the
    /// command search or the control channel without a path, and Save as Word Template.
    ///
    /// In a browser the copy downloads at once; on the desktop the Save dialog asks where and
    /// [`Self::write_copy`] writes it. The document stays as it was either way, live or not.
    pub fn save_as_copy(&mut self, id: &str) -> Result<Value, String> {
        let plan = match SAVE_AS.plan(self.runtime(), "", "Document", id) {
            Ok(plan) => plan,
            Err(e) => return Err(self.fail_export(e.to_string())),
        };
        // Named after the document's title (`/` in a title is part of it, not a folder).
        let file_name = match SAVE_AS.file_name_from_title(&self.title_stem(), "Document", id) {
            Ok(name) => name,
            Err(e) => return Err(self.fail_export(e.to_string())),
        };
        match plan.delivery {
            Delivery::Download => {
                let mut params = json!({"path": file_name, "copy": true});
                if let Some(format) = engine_format(id) {
                    params["format"] = json!(format);
                }
                let r = self.web_copy(&params);
                match &r {
                    Ok(_) => self.clear_export_error(),
                    Err(e) => {
                        self.fail_export(e.clone());
                    }
                }
                r
            }
            Delivery::SaveDialog => {
                // Save as Word Template starts in the user templates folder (#2852).
                let dir = if id == "dotx" { self.templates_dir_to_save() } else { None };
                let req = FileDialogRequest::Save { name: file_name, dir };
                match self.ask_file(req, AfterPick::Export { ext: id.to_string() }) {
                    crate::file_dialogs::Asked::Done(r) => r,
                    crate::file_dialogs::Asked::Pending => Ok(json!({"pending": "saveAs"})),
                    crate::file_dialogs::Asked::Busy => Ok(json!({"saved": false})),
                }
            }
        }
    }

    /// Desktop: write the copy in format `id` at the path the Save dialog (or the built-in file
    /// browser) picked. The format's extension is made sure of; a name the dialog never confirmed
    /// that is already taken is refused instead of written over.
    pub(crate) fn write_copy(&mut self, id: &str, picked: &str) -> Result<Value, String> {
        let p = SAVE_AS.picked_path(picked, id).map_err(|e| self.fail_export(e.to_string()))?;
        if p.renamed && std::path::Path::new(&p.path).exists() {
            return Err(self.fail_export(format!("{} already exists; choose another name", p.path)));
        }
        let r = if id == "png" {
            self.run("file.exportPng", json!({"path": p.path}))
        } else {
            let mut params = json!({"path": p.path, "copy": true});
            if let Some(format) = engine_format(id) {
                params["format"] = json!(format);
            }
            self.run("file.saveAs", params)
        };
        match &r {
            Ok(_) => {
                self.clear_export_error();
                self.status(crate::i18n::fmt(tl!("Exported {path}"), &[("path", &crate::i18n::isolate_path(&p.path))]));
            }
            Err(e) => {
                self.fail_export(e.clone());
            }
        }
        r
    }

    /// Desktop: the user templates folder, made if missing, where Save as Word Template starts.
    #[cfg(not(target_arch = "wasm32"))]
    fn templates_dir_to_save(&self) -> Option<String> {
        let dir = self.user_templates_dir()?;
        if !dir.is_dir()
            && let Err(e) = std::fs::create_dir_all(&dir)
        {
            log::warn!("couldn't make the templates folder {}: {e}", dir.display());
        }
        dir.is_dir().then(|| dir.to_string_lossy().into_owned())
    }

    #[cfg(target_arch = "wasm32")]
    fn templates_dir_to_save(&self) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_format_list_is_valid() {
        assert_eq!(SAVE_AS.check(), Ok(()));
    }

    /// Every writer the engine has is on the list, and the list offers nothing the engine can't
    /// write: a new writer must be offered by Save As, and a format on the list must save.
    #[test]
    fn the_list_covers_every_writer() {
        let mut listed: Vec<&str> = SAVE_AS.formats().iter().map(|f| f.ext).collect();
        listed.sort_unstable();
        listed.dedup();
        // `.docm` and `.dotm` are the opened file's own formats (Save keeps its macros), never a
        // copy's; `json` is the engine's own debug dump.
        let mut writers: Vec<&str> = wordcraft_engine::io::SAVE_EXTS.iter().copied().filter(|e| !matches!(*e, "docm" | "dotm" | "json")).collect();
        writers.sort_unstable();
        writers.dedup();
        assert_eq!(listed, writers);
        let doc = wordcraft_doc::Document::from_text("copy");
        for f in SAVE_AS.formats() {
            let name = format!("copy.{}", f.ext);
            let bytes = wordcraft_engine::io::save_bytes(&name, &doc);
            assert!(bytes.is_ok_and(|b| !b.is_empty()), "{} writes", f.id);
        }
    }
}

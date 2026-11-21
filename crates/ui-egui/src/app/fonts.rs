use super::{QUIT, WordApp, i18n, json, theme};

impl WordApp {
    /// Web: a host page offered fonts; they are being fetched. Until `false` is passed (all
    /// arrived or failed), the missing-fonts box doesn't come up by itself.
    pub fn host_fonts_coming(&mut self, coming: bool) {
        self.host_fonts_coming = coming;
    }

    /// Web: install a font file a host page offered (`file` is its file name, which must name a
    /// font) like one the user added with Add Font: it joins the font database, is kept through
    /// [`crate::Services::store_font`], and the document lays out with it. `Ok(false)`: it was
    /// already available (nothing changed, nothing said). A box that is open for missing fonts is
    /// checked again, so it drops the fonts that are now present and closes when none are left.
    pub fn install_host_font(&mut self, file: &str, bytes: &[u8]) -> Result<bool, String> {
        let params = json!({"name": file, "data": wordcraft_engine::cmd::insert::base64_encode(bytes)});
        // The engine says "Added …" on the status line, which a quiet background install must not.
        let status = self.session.status.clone();
        let result = self.session.run("font.add", &params).map_err(|e| e.to_string());
        self.session.status = status;
        match result {
            Ok(added) => {
                self.keep_web_font("font.add", &params, &added);
                self.refresh_font_box();
                Ok(true)
            }
            Err(e) if e.contains("already available") => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// The missing-fonts box, if open, lists what is still missing now.
    fn refresh_font_box(&mut self) {
        let Some(crate::dialogs::Dialog::FontDownload { dont_show, .. }) = &self.dialog else { return };
        let dont_show = *dont_show;
        let fonts = crate::font_download::rows(self);
        self.dialog = (!fonts.is_empty()).then_some(crate::dialogs::Dialog::FontDownload { fonts, dont_show });
    }
    /// Fonts were added or removed (a command, the web shell loading stored fonts): lay out
    /// again and refresh the font lists and their previews.
    pub(crate) fn watch_fonts(&mut self, ctx: &egui::Context) {
        let db = wordcraft_fonts::FontDb::global();
        let generation = db.generation();
        if generation == self.font_generation {
            return;
        }
        self.font_generation = generation;
        self.session.relayout();
        self.previews.fonts_changed();
        ctx.request_repaint();
    }

    /// The window caption is painted with the semibold interface face (Inter). A file name in a
    /// script those faces lack would otherwise be drawn as `?`. Borrow the document fonts'
    /// fallback faces, as IME composition does (#565). A CJK name uses the CJK UI font instead:
    /// those collections are too large to add here.
    pub(super) fn ensure_caption_fonts(&mut self) {
        let stem = self.title_stem();
        if stem.chars().any(wordcraft_fonts::is_cjk) {
            self.want_system_cjk = true;
        }
        let new = theme::faces_for_preedit(&stem, &self.canvas.ime_faces, theme::caption_char_covered, |c| {
            wordcraft_fonts::FontDb::global().fallback_for(c, u32::MAX)
        });
        if !new.is_empty() {
            self.canvas.ime_faces.extend(new);
            // Fonts are already installed for this frame's language; load the new faces too.
            self.styled = false;
        }
    }

    /// Comment balloons and the Comments pane draw comment text and author names with the
    /// interface faces, which have no Arabic, Hebrew, Thai …: each such letter was drawn as a box.
    /// Borrow the document fonts' fallback faces for them, as the caption does. Looked at once per
    /// document revision; CJK text uses the CJK UI font instead.
    pub(super) fn ensure_comment_fonts(&mut self) {
        let key = (self.session.document_id(), self.session.rev());
        if self.comment_fonts_for == Some(key) {
            return;
        }
        self.comment_fonts_for = Some(key);
        let doc = &self.session.doc;
        let mut chars = std::collections::BTreeSet::new();
        for c in doc.comments.values() {
            chars.extend(c.author.chars());
            chars.extend(doc.plain_text(wordcraft_doc::StoryRef::Part(c.part)).chars());
        }
        if chars.iter().any(|c| wordcraft_fonts::is_cjk(*c)) {
            self.want_system_cjk = true;
        }
        let text: String = chars.into_iter().collect();
        let new = theme::faces_for_preedit(&text, &self.canvas.ime_faces, theme::caption_char_covered, |c| {
            wordcraft_fonts::FontDb::global().fallback_for(c, u32::MAX)
        });
        if !new.is_empty() {
            self.canvas.ime_faces.extend(new);
            self.styled = false;
        }
    }

    /// File › Recover draws the recovered documents' names with the interface faces too: a
    /// Chinese name was a row of boxes, an Arabic or Hebrew one likewise. The same treatment as
    /// the caption (and as the Recent list, `ensure_recent_fonts`): CJK loads the CJK UI font,
    /// other scripts borrow the document fallback faces. Looked at again only when the names change.
    pub(super) fn ensure_recover_fonts(&mut self) {
        use std::hash::{Hash, Hasher};
        let docs = self.session.recovery_documents();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        docs.iter().for_each(|d| d.name.hash(&mut hasher));
        let key = hasher.finish();
        if self.recover_fonts_for == Some(key) {
            return;
        }
        self.recover_fonts_for = Some(key);
        let chars: std::collections::BTreeSet<char> = docs.iter().flat_map(|d| d.name.chars()).collect();
        if chars.iter().any(|c| wordcraft_fonts::is_cjk(*c)) {
            self.want_system_cjk = true;
        }
        let text: String = chars.into_iter().collect();
        let new = theme::faces_for_preedit(&text, &self.canvas.ime_faces, theme::caption_char_covered, |c| {
            wordcraft_fonts::FontDb::global().fallback_for(c, u32::MAX)
        });
        if !new.is_empty() {
            self.canvas.ime_faces.extend(new);
            self.styled = false;
        }
    }

    /// Recent documents in Backstage (and the Start screen) show file names and paths with the
    /// interface faces. Borrow document fallback faces for characters in non-Latin scripts (#2067),
    /// as the caption and comments do.
    pub(super) fn ensure_recent_fonts(&mut self) {
        if self.recent_fonts_for.as_slice() == self.ui.recent.as_slice() {
            return;
        }
        self.recent_fonts_for = self.ui.recent.clone();
        for p in &self.ui.recent {
            if p.chars().any(wordcraft_fonts::is_cjk) {
                self.want_system_cjk = true;
            }
            let new = theme::faces_for_preedit(p, &self.canvas.ime_faces, theme::caption_char_covered, |c| {
                wordcraft_fonts::FontDb::global().fallback_for(c, u32::MAX)
            });
            if !new.is_empty() {
                for face in new {
                    if !self.canvas.ime_faces.iter().any(|f| f.id() == face.id()) {
                        self.canvas.ime_faces.push(face);
                    }
                }
                self.styled = false;
            }
        }
    }

    /// The Find box and the Navigation pane's Results list draw the typed query and document
    /// excerpts around each match with the interface faces, which have no Georgian, Armenian,
    /// Thai … (#3248): each such letter was drawn as a box. Borrow the document fonts' fallback
    /// faces for them, as comments and the caption do. Looked at once per query/result-count/
    /// revision change (an edit can change a result's surrounding text — adding Georgian beside
    /// an existing Latin match — without changing the query or how many matches there are, #3257
    /// review); CJK text uses the CJK UI font instead.
    pub(super) fn ensure_find_fonts(&mut self) {
        let key = (self.session.find.query.clone(), self.session.find.results.len(), self.session.rev());
        if self.find_fonts_for.as_ref() == Some(&key) {
            return;
        }
        self.find_fonts_for = Some(key);
        let mut text = self.session.find.query.clone();
        for (a, b) in self.session.find.results.iter().take(500) {
            let Some(p) = self.session.doc.para_at(a) else { continue };
            let from = p.clamp(a.off.saturating_sub(30));
            let to = p.clamp((b.off + 40).min(p.len()));
            if let Some(s) = p.text.get(from..to) {
                text.push_str(s);
            }
        }
        if text.chars().any(wordcraft_fonts::is_cjk) {
            self.want_system_cjk = true;
        }
        let new = theme::faces_for_preedit(&text, &self.canvas.ime_faces, theme::caption_char_covered, |c| {
            wordcraft_fonts::FontDb::global().fallback_for(c, u32::MAX)
        });
        if !new.is_empty() {
            self.canvas.ime_faces.extend(new);
            self.styled = false;
        }
    }

    /// Install the interface fonts when the language, the CJK fallback, or a caption fallback
    /// face has changed since the last install.
    pub(super) fn install_ui_fonts(&mut self, ctx: &egui::Context, lang: i18n::Lang) {
        let system_cjk = self.fonts_system_cjk || self.want_system_cjk || lang.uses_cjk();
        // An Arabic interface, or a screen that shows Arabic names (language picker / packs) in
        // another interface language, asks for an installed Arabic font when none is embedded.
        let system_arabic = lang.rtl() || self.want_system_arabic;
        if !self.styled
            || lang.prefers_hans() != self.fonts_hans
            || lang.prefers_korean() != self.fonts_korean
            || lang.prefers_hant() != self.fonts_hant
            || system_cjk != self.fonts_system_cjk
            || system_arabic != self.fonts_system_arabic
        {
            self.fonts_hans = lang.prefers_hans();
            self.fonts_korean = lang.prefers_korean();
            self.fonts_hant = lang.prefers_hant();
            self.fonts_system_cjk = system_cjk;
            self.fonts_system_arabic = system_arabic;
            self.apply_ui_fonts(ctx);
            // Mod with -, = and 0 are Word shortcuts (optional hyphen, subscript, paragraph spacing);
            // egui's keyboard zoom would also scale the whole window on them. Zoom is View › Zoom.
            // egui quits on Command-Q, which is Ctrl+Q off the Mac: Word's Reset Paragraph
            // Formatting there (#1218). Keep ⌘Q, which only the Mac sends. egui would close only the
            // window it was pressed in, so with native windows the app quits itself
            // ([`WordApp::window_logic_before`]).
            let quit = (!self.windows.native()).then_some(QUIT).into_iter().collect();
            ctx.options_mut(|o| {
                o.zoom_with_keyboard = false;
                o.quit_shortcuts = quit;
            });
            self.styled = true;
        }
    }

    /// Install the interface fonts [`Self::install_ui_fonts`] chose, with every window's borrowed
    /// faces and each window's composition face. The fonts are the app's, one set for every
    /// window: fonts made from one window's faces would draw another window's caption or
    /// comments as boxes, and its composition in the wrong face.
    pub(crate) fn apply_ui_fonts(&self, ctx: &egui::Context) {
        let mut fonts = theme::font_definitions_language_arabic(
            self.fonts_hans,
            self.fonts_korean,
            self.fonts_hant,
            self.fonts_system_cjk,
            self.fonts_system_arabic,
        );
        // Every window's borrowed faces: the fonts are the app's.
        let mut faces = self.canvas.ime_faces.clone();
        for w in self.other_windows() {
            for face in &w.canvas.ime_faces {
                if !faces.iter().any(|f| f.id() == face.id()) {
                    faces.push(face.clone());
                }
            }
        }
        theme::add_fallback_faces(&mut fonts, &faces);
        theme::set_composition_face(&mut fonts, self.windows.shown(), self.canvas.ime_face);
        for (window, w) in self.other_windows_by_key() {
            theme::set_composition_face(&mut fonts, window, w.canvas.ime_face);
        }
        ctx.set_fonts(fonts);
    }
}

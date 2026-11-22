//! Host modes: a page that runs WordCraft (as an `<iframe>`, or in its own document) opens
//! documents in it and stores what it saves instead of the browser downloading. The messages and
//! the two transports are `craft-web-host`'s (storytold/craft-libs), the same contract as the
//! other Craft apps' bridges, with the `wordcraft:` prefix. This module is WordCraft's part of it.
//!
//! - `?host=parent` (an `<iframe>`, such as a Nextcloud app) talks to the same-origin parent over
//!   `postMessage`; `data-host="page"` on the canvas (the page's own document, no iframe) uses
//!   DOM `CustomEvent`s on the canvas instead. `?host=parent` wins when both are present. With no
//!   parent, or a parent on another origin, there is no bridge: the normal app with downloads.
//! - `wordcraft:ready` once the app is running and listening; send a file now. WordCraft reads
//!   bytes, so it says `file: false` and a host sends `bytes`.
//! - `wordcraft:open` (from the host): `{ name, bytes }`. It opens like a document picked with
//!   File › Open or dropped on the window; a file over [`MAX_OPEN_BYTES`] is refused before it is
//!   copied.
//! - `wordcraft:save` (to the host): `{ name, saveAs, bytes }` for every file WordCraft would
//!   otherwise download. The host stores it and reports failures itself. `saveAs` says whether
//!   the file becomes the document's file:
//!   - Save: `false`. It writes the opened file under its own name and format, as the desktop
//!     app does. A name the parent never sent (an untitled document, an imported .doc saved as
//!     .docx) is a new file: `true`, as for Save As.
//!   - Save As, and File › Export to another document format (which runs Save As): `true` for a
//!     format that keeps everything (.docx and the other Word packages, .odt, .rtf). The document
//!     is now that file; the next Save sends its name with `false`.
//!   - Save As or Export to a format that doesn't (PDF, Markdown, plain text), Export as PDF and
//!     Export Page as PNG: `false`. They are copies; the document's file stays the same.
//! - Share (`?host=parent` only): `wordcraft:ready` also says `shareButton: true`, so the page
//!   can hide a Share button of its own. When the page's `wordcraft:host` says `share: true`, the
//!   ribbon's Share posts `wordcraft:share {}` to the page, which opens its sharing for the open
//!   file, instead of opening File › Export. Without `share: true` Share exports as before.
//! - `wordcraft:dirty` whenever unsaved changes appear or go away, `wordcraft:failed` when the
//!   app couldn't start.
//!
//! In both host modes the host sends the document to edit, so no sample opens, and `?author=`
//! names the user for comments and tracked changes. With `data-host="page"` saves are handed to
//! the host rather than downloaded, and the host guards leaving the page. A framing page
//! (`?host=parent`) stores no saves unless its `wordcraft:host` says `saves: true`: without it,
//! Save, Save As and Export download a copy in the browser and never mark the document saved, and
//! the frame guards leaving the page itself (`beforeunload`), with unsaved changes as
//! `wordcraft:dirty` also tells the page.
//!
//! With `saves: true` (and the document not live: a live session saves its own file) every save
//! goes out as `wordcraft:save` with an `id`. The document is marked saved only on
//! `wordcraft:saved {id, name?}` (a `name` is the file's name from then on: a conflict copy or
//! Save As), and on `wordcraft:save-failed {id, error}` it stays unsaved with "Couldn't save to
//! Nextcloud: {error}". `wordcraft:fonts {fonts:[{name,url}]}` lists same-origin font files; each
//! is fetched and installed like a font the user added, and the missing-font box checks again.
//!
//! # Live editing (`craft-live/1`, `?host=parent` only)
//!
//! The parent page may instead send the unprefixed `live` message: `{ticket, relayUrl, room,
//! fileUrl, fileName, proto}` (`craft-web-host`'s `live` module, which checks every field and
//! refuses another `proto`). WordCraft posts the unprefixed `{type:"ready"}` once it listens.
//!
//! - The first `live` fetches `fileUrl` with the user's cookies, only from this page's own
//!   origin over HTTP(S) ([`crate::live_ticket::same_origin`]), then connects to the relay on
//!   the page's own host only ([`crate::live_ticket::same_host`]; a relative `/craft` is) over
//!   the page's `socket.io-client` (`craft-collab`'s `WebHub`, which derives the Socket.IO path
//!   `/craft/socket.io/` from `relayUrl`) and opens the file live. A read ticket (`perm` without
//!   the update bit) joins read-only and never sends an edit. If the fetch, the connect or the
//!   start fails, the window says why and the next `live` tries again
//!   ([`crate::live_ticket::Joining`]).
//! - A later `live` for the same room is the page's ticket refresh (every 480 s). It goes to the
//!   hub (`Hub::set_token`) for its next reconnect; a hub the relay refused (an expired
//!   ticket) connects again with it. One that comes while the file is fetched waits for the join.
//!   One whose ticket no longer has the update bit ends the link ("Your access changed").
//! - A `live` for another room after the join means the session was restarted (the room closed
//!   and the page opened the file again): the link ends ("The live session was restarted").
//!   Either way the window is `broken` and Save As downloads a copy.
//! - Another `proto` shows "This file is being edited with a newer WordCraft. Update to join."
//!   and nothing can be edited.
//! - `live-state` `{state, reason, savedAt, peers}` goes to the parent on every change of
//!   state and at most once a second otherwise. `state` is `broken` once the link ended for good
//!   (reopen the file), apart from `refused` (nothing can be shared now). `savedAt` is when this
//!   window learned of the newest save (ISO 8601, to the second), not when the file was written.
//!
//! Tickets are never logged.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::live_ticket::{Joining, Next};
use craft_web_host::Mode;
use craft_web_host::PendingSaves;
use craft_web_host::live::{LiveError, LiveMessage, LiveState, Peer as HostPeer};
use craft_web_host::web::{FileData, Host, Message, fetch_font};
use wasm_bindgen::JsCast as _;
use wordcraft_ui_egui::async_saves::{AsyncSaves, SaveOutcome};
use wordcraft_ui_egui::live::LiveReport;
use wordcraft_ui_egui::{Inbox, Services, WordApp};

/// The app id in the host messages (`wordcraft:ready`).
const APP: &str = "wordcraft";

/// The name a document gets when the host sent none (the crate says `document` for a blank name).
const DEFAULT_NAME: &str = "Document.docx";

/// The largest document taken from the host page, refused before it is copied.
const MAX_OPEN_BYTES: u64 = 512 << 20;

/// The longest user name taken from `?author=`.
const MAX_AUTHOR_CHARS: usize = 200;

/// What the host's messages leave for the next frame.
pub enum HostEvent {
    /// The host stores saves ([`Bridge::apply_saves`]).
    Saves,
    /// Fonts are being fetched (true) or all arrived or failed (false).
    FontsComing(bool),
    /// A fetched font: its file name and bytes.
    Font(String, Vec<u8>),
    /// The host shares files itself ([`Bridge::apply_share`]).
    Share,
    /// The host opened this file in the frame (the name it is shared under).
    Opened(String),
}

pub type HostEvents = Rc<RefCell<Vec<HostEvent>>>;

/// The saves sent to a host that answers each, by the ticket the app waits on.
#[derive(Default)]
struct SentSaves {
    ids: PendingSaves,
    tickets: Vec<(String, u64)>,
}

#[derive(Clone)]
pub struct Bridge {
    host: Rc<Host>,
    /// The host said `saves: true`.
    saves: Rc<Cell<bool>>,
    /// The host said `share: true`.
    share: Rc<Cell<bool>>,
    sent: Rc<RefCell<SentSaves>>,
}

impl Bridge {
    /// The bridge for this page, or `None` when WordCraft isn't hosted: no `?host=parent` and no
    /// `data-host="page"` on the canvas, an unframed `?host=parent` page (it stays the normal app
    /// with downloads), or a parent on another origin.
    pub fn detect(canvas: &web_sys::HtmlCanvasElement) -> Option<Self> {
        Some(Self { host: Rc::new(Host::connect(APP, canvas)?), saves: Rc::default(), share: Rc::default(), sent: Rc::default() })
    }

    /// The host mode of this page.
    pub fn mode(&self) -> Mode {
        self.host.mode()
    }

    /// Hear the files the host sends and report unsaved changes to it (and to `dirty`, the flag
    /// the window's own leave guard reads). A page (`data-host="page"`) gets saved and exported
    /// files instead of downloads; under a framing page (`?host=parent`), which stores none, they
    /// download in the browser as copies.
    pub fn connect(
        &self,
        services: &mut Services,
        inbox: Inbox,
        ctx: egui::Context,
        dirty: Rc<std::cell::Cell<bool>>,
        saves: AsyncSaves,
        events: HostEvents,
    ) {
        if self.host.mode() == Mode::Parent {
            listen_share(self.share.clone(), events.clone(), ctx.clone());
        }
        self.listen(inbox, ctx, saves, events);
        if self.host.mode() == Mode::Parent {
            services.saves_are_copies = true;
            services.host_save = None;
            services.download = Some(Box::new(|name: &str, bytes: &[u8]| {
                let window = web_sys::window().ok_or("no window")?;
                crate::web::trigger_anchor_download(&window, name, bytes)
            }));
        } else {
            let bridge = self.clone();
            services.host_save = Some(Box::new(move |name: &str, bytes: &[u8], save_as: bool| bridge.post_save(name, bytes, save_as)));
            // Anything else that would download goes to the host too.
            let bridge = self.clone();
            services.download = Some(Box::new(move |name: &str, bytes: &[u8]| bridge.post_save(name, bytes, false)));
        }
        // The browser's save picker is never used, so no save finishes later.
        services.async_saves = None;
        // The crate does not repeat a state it already reported.
        let host = self.host.clone();
        services.on_dirty = Some(Box::new(move |d: bool| {
            dirty.set(d);
            host.post_dirty(d);
        }));
        let host = self.host.clone();
        services.live_state = Some(Box::new(move |report: &LiveReport| {
            if let Err(e) = host.post_live_state(&live_state(report)) {
                log::warn!("live-state not delivered: {e}");
            }
        }));
    }

    /// The host said it stores saves: from now on Save, Save As and Export go to it with an id and
    /// the document is marked saved only when it answers. Called each frame; does nothing before
    /// the host's `host` message, in page mode (which stores saves already) or twice.
    pub fn apply_saves(&self, services: &mut Services, saves: &AsyncSaves) {
        if !self.saves.get() || self.host.mode() != Mode::Parent || services.host_confirms_saves {
            return;
        }
        services.saves_are_copies = false;
        services.host_confirms_saves = true;
        services.async_saves = Some(saves.clone());
        let (bridge, saves) = (self.clone(), saves.clone());
        services.host_save = Some(Box::new(move |name: &str, bytes: &[u8], save_as: bool| {
            let ticket = saves.start();
            let id = bridge.sent.borrow_mut().ids.start();
            bridge.sent.borrow_mut().tickets.push((id.clone(), ticket));
            let name = std::path::Path::new(name).file_name().map_or_else(|| name.to_string(), |n| n.to_string_lossy().to_string());
            let posted = bridge.host.post_save(&name, bytes, Some(&id), save_as);
            if let Err(e) = &posted {
                // No answer will come.
                bridge.sent.borrow_mut().ids.cancel(&id);
                bridge.sent.borrow_mut().tickets.retain(|(i, _)| *i != id);
                saves.finish(ticket, SaveOutcome::Failed(e.clone()));
            }
            posted
        }));
    }

    /// The host said it shares files: from now on the ribbon's Share posts `wordcraft:share` to it.
    /// Called each frame; does nothing before the host's `host` message, outside parent mode or
    /// twice.
    pub fn apply_share(&self, services: &mut wordcraft_ui_egui::Services) {
        if !self.share.get() || self.host.mode() != Mode::Parent || services.host_share.is_some() {
            return;
        }
        services.host_share = Some(Box::new(|| post_to_parent(&[("type", "wordcraft:share".into())])));
    }

    /// Hear the parent's `live` messages: the first fetches the file and joins, later ones are
    /// ticket refreshes. They wait in `events` for the next frame ([`Live::frame`]).
    pub fn listen_live(&self, events: LiveEvents, joining: Rc<RefCell<Joining>>, ctx: egui::Context) {
        self.host.listen_live(move |message| match message {
            Err(LiveError::UnsupportedProto(_)) => {
                events.borrow_mut().push(LiveEvent::Newer);
                ctx.request_repaint();
            }
            // The error never quotes the message, so no ticket reaches the log.
            Err(e) => log::warn!("live message refused: {e}"),
            Ok(message) => {
                let access = crate::live_ticket::ticket_access(message.ticket());
                let next = joining.borrow_mut().message(&message.room, access);
                match next {
                    Next::Restarted | Next::AccessDropped => {
                        let reason = next.end_reason().unwrap_or(crate::live_ticket::RESTARTED);
                        log::warn!("the live session ended: {reason}");
                        events.borrow_mut().push(LiveEvent::End(reason));
                        ctx.request_repaint();
                    }
                    Next::Refresh => {
                        events.borrow_mut().push(LiveEvent::Ticket(message.ticket().to_owned()));
                        ctx.request_repaint();
                    }
                    Next::Open => {
                        let (events, ctx, joining) = (events.clone(), ctx.clone(), joining.clone());
                        wasm_bindgen_futures::spawn_local(async move {
                            let event = match fetch_file(&message.file_url).await {
                                Ok(bytes) => LiveEvent::Open(message, bytes),
                                Err(e) => {
                                    // The page's next `live` (its refresh) tries again.
                                    joining.borrow_mut().failed();
                                    LiveEvent::Failed(e)
                                }
                            };
                            events.borrow_mut().push(event);
                            ctx.request_repaint();
                        });
                    }
                }
            }
        });
    }

    /// `?author=`: the host's name for the user signs comments and tracked changes.
    pub fn set_author(app: &mut WordApp, author: &str) {
        let author: String = author.trim().chars().take(MAX_AUTHOR_CHARS).collect();
        if !author.is_empty() {
            app.session.author = author;
        }
    }

    pub fn post_ready(&self) {
        if self.host.mode() == Mode::Parent {
            // `craft-web-host`'s `ready`, plus `shareButton`: the ribbon has its own Share button.
            let _ = post_to_parent(&[
                ("type", "wordcraft:ready".into()),
                ("version", craft_web_host::VERSION.into()),
                ("appVersion", env!("CARGO_PKG_VERSION").into()),
                ("file", false.into()),
                ("shareButton", true.into()),
            ]);
            // A live host waits for the unprefixed `ready` before it sends `live`.
            let _ = self.host.post_live_ready();
        } else {
            let _ = self.host.post_ready(env!("CARGO_PKG_VERSION"), false);
        }
    }

    /// The app couldn't start (no WebGPU or WebGL2, for example): let the host say so.
    pub fn post_failed(&self, error: &str) {
        let _ = self.host.post_failed(error, None);
    }

    fn post_save(&self, path: &str, bytes: &[u8], save_as: bool) -> Result<(), String> {
        let name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string());
        self.host.post_save(&name, bytes, None, save_as)
    }

    /// Hear the host's messages: `open` queues the file like a dropped one, `host` says whether it
    /// stores saves, `saved` and `save-failed` answer a save, `fonts` lists fonts to fetch.
    fn listen(&self, inbox: Inbox, ctx: egui::Context, saves: AsyncSaves, events: HostEvents) {
        let bridge = self.clone();
        self.host.listen(move |message| match message {
            Message::Open { name, data } => {
                if let Some(name) = deliver_open(&inbox, &ctx, &name, &data) {
                    events.borrow_mut().push(HostEvent::Opened(name));
                }
            }
            Message::Host { saves: stores, .. } => {
                if stores {
                    bridge.saves.set(true);
                    events.borrow_mut().push(HostEvent::Saves);
                    ctx.request_repaint();
                }
            }
            Message::Saved { id, name } => {
                if let Some(ticket) = bridge.answered(&id) {
                    saves.finish(ticket, name.map_or(SaveOutcome::Saved, SaveOutcome::SavedAs));
                    ctx.request_repaint();
                }
            }
            Message::SaveFailed { id, error } => {
                if let Some(ticket) = bridge.answered(&id) {
                    saves.finish(ticket, SaveOutcome::Failed(error));
                    ctx.request_repaint();
                }
            }
            Message::Fonts { fonts } => fetch_fonts(fonts, events.clone(), ctx.clone()),
            Message::AttachPdf { .. } | Message::HandoffDone { .. } => {}
        });
    }

    /// The ticket of the save `id` names, once: an unknown or repeated answer has none.
    fn answered(&self, id: &str) -> Option<u64> {
        let mut sent = self.sent.borrow_mut();
        if !sent.ids.finish(id) {
            return None;
        }
        let at = sent.tickets.iter().position(|(i, _)| i == id)?;
        Some(sent.tickets.remove(at).1)
    }
}

/// The framing parent and this page's origin, when the parent is on the same origin (the only one
/// heard and answered, as `craft-web-host` does).
fn same_origin_parent() -> Option<(web_sys::Window, web_sys::Window, String)> {
    let window = web_sys::window()?;
    let parent = window.parent().ok().flatten()?;
    if js_sys::Object::is(&parent, &window) {
        return None;
    }
    let origin = window.location().origin().ok()?;
    // Reading another origin's location throws.
    (parent.location().origin().ok()? == origin).then_some((window, parent, origin))
}

/// Post `fields` as one object to the same-origin parent, addressed to this origin only.
fn post_to_parent(fields: &[(&str, wasm_bindgen::JsValue)]) -> Result<(), String> {
    let (_, parent, origin) = same_origin_parent().ok_or("no same-origin parent")?;
    let message = js_sys::Object::new();
    for (key, value) in fields {
        js_sys::Reflect::set(&message, &(*key).into(), value).map_err(|_| "couldn't build the message".to_string())?;
    }
    parent.post_message(&message, &origin).map_err(|e| e.as_string().unwrap_or_else(|| format!("{e:?}")))
}

/// Hear `share: true` in the same-origin parent's `wordcraft:host` (`craft-web-host` reads the
/// other fields of that message, not this one).
fn listen_share(share: Rc<Cell<bool>>, events: HostEvents, ctx: egui::Context) {
    let Some((window, parent, origin)) = same_origin_parent() else { return };
    let on_message = wasm_bindgen::closure::Closure::<dyn Fn(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
        if event.origin() != origin || !event.source().is_some_and(|s| js_sys::Object::is(&s, &parent)) {
            return;
        }
        let data = event.data();
        let field = |key: &str| js_sys::Reflect::get(&data, &key.into()).unwrap_or(wasm_bindgen::JsValue::UNDEFINED);
        if field("type").as_string().as_deref() == Some("wordcraft:host") && field("share").as_bool() == Some(true) && !share.get() {
            share.set(true);
            events.borrow_mut().push(HostEvent::Share);
            ctx.request_repaint();
        }
    });
    if window.add_event_listener_with_callback("message", on_message.as_ref().unchecked_ref()).is_ok() {
        on_message.forget();
    }
}

/// The file name a font is installed under: the last part of its URL (it has the extension), else
/// the host's name for it.
fn font_file_name(name: &str, url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or_default();
    let last = path.rsplit('/').next().unwrap_or_default();
    let decoded = js_sys::decode_uri_component(last).ok().and_then(|s| s.as_string()).unwrap_or_else(|| last.to_string());
    if wordcraft_fonts::is_font_file_name(&decoded.to_ascii_lowercase()) { decoded } else { name.to_string() }
}

/// Fetch each font a host offered (one at a time, same-origin only, size-capped by the bridge) and
/// leave its bytes for the next frame; a failure is logged and skipped.
fn fetch_fonts(fonts: Vec<craft_web_host::FontRef>, events: HostEvents, ctx: egui::Context) {
    if fonts.is_empty() {
        return;
    }
    events.borrow_mut().push(HostEvent::FontsComing(true));
    wasm_bindgen_futures::spawn_local(async move {
        for font in fonts {
            match fetch_font(&font).await {
                Ok(bytes) => events.borrow_mut().push(HostEvent::Font(font_file_name(&font.name, &font.url), bytes)),
                Err(e) => log::warn!("host font {} not fetched: {e}", font.name),
            }
            ctx.request_repaint();
        }
        events.borrow_mut().push(HostEvent::FontsComing(false));
        ctx.request_repaint();
    });
}

/// The file an open message carries, pushed onto the inbox for the next frame, like a dropped
/// file. A host's file is untrusted input: an oversized one is refused before it is copied into
/// the app's memory (the data's own length, not the host's claim). The crate has cleaned the name
/// (and cut it to 255 characters); a blank one comes as `document` and becomes `Document.docx`.
/// Returns the name it is opened under, or `None` when it was refused.
fn deliver_open(inbox: &Inbox, ctx: &egui::Context, name: &str, data: &FileData) -> Option<String> {
    let name = if name == "document" { DEFAULT_NAME } else { name }.to_string();
    let FileData::Bytes(array) = data else {
        log::warn!("host file {name} came as a File; WordCraft reads bytes only; not opened");
        return None;
    };
    if data.size() > MAX_OPEN_BYTES as f64 {
        log::warn!("host file is {} bytes, over the {} MiB host open limit; not opened", data.size(), MAX_OPEN_BYTES >> 20);
        return None;
    }
    inbox.lock().unwrap_or_else(|e| e.into_inner()).push((name.clone(), array.to_vec()));
    ctx.request_repaint();
    Some(name)
}

/// What the parent's `live` messages brought, for the next frame.
pub enum LiveEvent {
    /// The file, fetched: join with this message's ticket.
    Open(LiveMessage, Vec<u8>),
    /// A later `live` for the room: a fresh ticket.
    Ticket(String),
    /// The file could not be fetched.
    Failed(String),
    /// The room speaks a newer protocol.
    Newer,
    /// The session ended (another room, less access), for this reason.
    End(&'static str),
}

pub type LiveEvents = Rc<RefCell<Vec<LiveEvent>>>;

/// The web app's side of a live session: when it joins, and a ticket that came during the join.
#[derive(Default)]
pub struct Live {
    joining: Rc<RefCell<Joining>>,
    /// A fresh ticket that arrived while the file was fetched, for the hub once joined.
    pending: Option<String>,
}

impl Live {
    /// The join state the `live` listener shares ([`Bridge::listen_live`]).
    pub fn joining(&self) -> Rc<RefCell<Joining>> {
        Rc::clone(&self.joining)
    }

    /// Act on what arrived since the last frame.
    pub fn frame(&mut self, app: &mut WordApp, events: &LiveEvents) {
        let arrived = std::mem::take(&mut *events.borrow_mut());
        for event in arrived {
            match event {
                LiveEvent::Newer => app.live_newer(),
                LiveEvent::End(reason) => {
                    app.live_end(wordcraft_ui_egui::tl!(reason));
                }
                LiveEvent::Failed(e) => {
                    log::error!("live file not fetched: {e}");
                    not_opened(app, &e);
                }
                LiveEvent::Open(message, bytes) => self.open(app, &message, &bytes),
                // The hub uses it for its next connect, and starts again with it after the relay
                // refused an expired one.
                LiveEvent::Ticket(ticket) => {
                    if !app.live_refresh_ticket(ticket.clone()) {
                        self.pending = Some(ticket);
                    }
                }
            }
        }
    }

    fn open(&mut self, app: &mut WordApp, message: &LiveMessage, bytes: &[u8]) {
        let access = crate::live_ticket::ticket_access(message.ticket());
        let started = relay_on_page_host(&message.relay_url)
            .and_then(|()| craft_collab::web::WebHub::connect(&message.relay_url, message.ticket(), None).map_err(|e| e.to_string()))
            .and_then(|hub| app.start_live(Box::new(hub), &message.room, bytes, &message.file_name, access));
        match started {
            Ok(()) => {
                app.services.host_file = Some(message.file_name.clone());
                self.joining.borrow_mut().joined(&message.room, access);
                if let Some(ticket) = self.pending.take() {
                    app.live_refresh_ticket(ticket);
                }
            }
            Err(e) => {
                // The page's next `live` (its refresh) tries again.
                self.joining.borrow_mut().failed();
                self.pending = None;
                log::error!("live editing did not start: {e}");
                not_opened(app, &e);
            }
        }
    }
}

fn not_opened(app: &mut WordApp, error: &str) {
    app.status_long(wordcraft_ui_egui::i18n::fmt(wordcraft_ui_egui::tl!("Couldn't open the file for live editing: {error}"), &[("error", error)]));
}

/// The relay URL must be on the page's own host (a relative `/craft` is), so the ticket goes
/// nowhere else.
fn relay_on_page_host(relay_url: &str) -> Result<(), String> {
    let window = web_sys::window().ok_or("no window")?;
    let location = window.location();
    let page = location.href().map_err(|_| "no page address")?;
    let page_host = location.host().map_err(|_| "no page host")?;
    let url = web_sys::Url::new_with_base(relay_url, &page).map_err(|_| "the relay address is not a URL")?;
    if crate::live_ticket::same_host(&url.protocol(), &url.host(), &page_host) { Ok(()) } else { Err("the relay is not on this site".into()) }
}

/// The file to edit live, fetched with the user's cookies from this page's own origin only.
async fn fetch_file(file_url: &str) -> Result<Vec<u8>, String> {
    let window = web_sys::window().ok_or("no window")?;
    let location = window.location();
    let page_origin = location.origin().map_err(|_| "no page origin")?;
    let page = location.href().map_err(|_| "no page address")?;
    let url = web_sys::Url::new_with_base(file_url, &page).map_err(|_| "the file address is not a URL")?;
    if !crate::live_ticket::same_origin(&url.protocol(), &url.origin(), &page_origin) {
        return Err("the file is not on this site".into());
    }
    let init = web_sys::RequestInit::new();
    init.set_method("GET");
    init.set_mode(web_sys::RequestMode::SameOrigin);
    init.set_credentials(web_sys::RequestCredentials::SameOrigin);
    let response = wasm_bindgen_futures::JsFuture::from(window.fetch_with_str_and_init(&url.href(), &init))
        .await
        .map_err(|_| "the file could not be fetched")?;
    let response: web_sys::Response = response.dyn_into().map_err(|_| "the fetch gave no response")?;
    if !response.ok() {
        return Err(format!("the server answered {}", response.status()));
    }
    let too_big = || format!("the file is over the {} MiB limit", MAX_OPEN_BYTES >> 20);
    if let Ok(Some(length)) = response.headers().get("content-length")
        && length.trim().parse::<u64>().is_ok_and(|n| n > MAX_OPEN_BYTES)
    {
        return Err(too_big());
    }
    let buffer = response.array_buffer().map_err(|_| "the file could not be read")?;
    let buffer = wasm_bindgen_futures::JsFuture::from(buffer).await.map_err(|_| "the file could not be read")?;
    let array = js_sys::Uint8Array::new(&buffer);
    if u64::from(array.length()) > MAX_OPEN_BYTES {
        return Err(too_big());
    }
    Ok(array.to_vec())
}

/// The window's report as the host message.
fn live_state(report: &LiveReport) -> LiveState {
    let state = crate::live_ticket::live_status(report.state);
    let saved_at = report.saved_at_ms.and_then(|ms| js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms as f64)).to_iso_string().as_string());
    LiveState {
        state,
        reason: report.reason.clone(),
        saved_at,
        peers: report.peers.iter().map(|(id, display)| HostPeer { id: id.clone(), display: display.clone() }).collect(),
    }
}

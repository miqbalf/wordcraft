//! The assist messages over `postMessage` (wasm): the `message` listener and the poster for
//! `wordcraft:proof-check` and `wordcraft:ai`. The parent and origin checks are those of
//! `craft_web_host::Host::connect`, and the listener is guarded like `Host::listen_live`: only
//! the framing parent on this origin is heard or answered. Nothing is logged but a refusal's
//! reason, never a message.

use wasm_bindgen::JsCast as _;
use wasm_bindgen::closure::Closure;
use web_sys::js_sys;
use wordcraft_ui_egui::assist::{AssistInbox, AssistOut};

use crate::assist_msg::{build, parse, usable_origin, wants};

/// The poster to the framing parent, with its answers delivered to `inbox` (and `ctx` woken), or
/// `None` on an unframed page or a parent on another origin.
pub fn connect(inbox: AssistInbox, ctx: egui::Context) -> Option<Box<dyn Fn(&AssistOut) -> Result<(), String>>> {
    let window = web_sys::window()?;
    let parent = window.parent().ok().flatten()?;
    if js_sys::Object::is(&parent, &window) {
        return None;
    }
    let origin = window.location().origin().ok()?;
    // An opaque origin (a sandboxed frame) cannot be posted to: the feature stays off.
    if !usable_origin(&origin) {
        return None;
    }
    // Reading another origin's location throws, which is `None` too.
    if parent.location().origin().ok()? != origin {
        return None;
    }
    let (from, expect) = (parent.clone(), origin.clone());
    let on_message = Closure::<dyn Fn(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
        if event.origin() != expect || !event.source().is_some_and(|s| js_sys::Object::is(&s, &from)) {
            return;
        }
        let data = event.data();
        let Some(kind) = js_sys::Reflect::get(&data, &"type".into()).ok().and_then(|t| t.as_string()) else { return };
        // Nothing else is serialised: `wordcraft:open` carries a whole document.
        if !wants(&kind) {
            return;
        }
        let Some(json) = js_sys::JSON::stringify(&data).ok().and_then(|s| s.as_string()) else { return };
        if let Some(message) = parse(&json) {
            inbox.borrow_mut().push(message);
            ctx.request_repaint();
        }
    });
    window.add_event_listener_with_callback("message", on_message.as_ref().unchecked_ref()).ok()?;
    on_message.forget();
    Some(Box::new(move |out: &AssistOut| {
        let message = js_sys::JSON::parse(&build(out)).map_err(|_| "couldn't build the message".to_string())?;
        parent.post_message(&message, &origin).map_err(|e| e.as_string().unwrap_or_else(|| format!("{e:?}")))
    }))
}

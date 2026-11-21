//! What the web app reads off a live ticket, a file URL and a relay URL before it joins a room,
//! and when it joins. Pure, so it is tested natively; `host` (wasm32) does the browser part.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use craft_web_host::Mode;
use craft_web_host::live::LiveStatus;
use wordcraft_ui_egui::live::Access;

/// The window's end when the page sends a `live` message for another room after the join.
pub const RESTARTED: &str = "The live session was restarted. Reopen the file.";
/// The window's end when the page's refreshed ticket for the room gives less access.
pub const ACCESS_CHANGED: &str = "Your access changed. Reopen the file.";

/// The relay's permission bit for changing the file (OCS numbering: read 1, update 2).
const PERMISSION_UPDATE: u64 = 2;

/// The access a ticket grants, read from its payload (`payload.signature`, base64url JSON with
/// `perm`). Only the interface uses it, to start a read-only ticket read-only; the relay checks
/// the signature and enforces the permission. A ticket that can't be read is taken as an edit
/// ticket and left to the relay: one it can't verify either is refused at the join (the window
/// says why), and one without the update bit has its first edit refused, which ends live
/// editing with the relay's reason.
pub fn ticket_access(ticket: &str) -> Access {
    let perm = ticket
        .split('.')
        .next()
        .and_then(|payload| URL_SAFE_NO_PAD.decode(payload).ok())
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|claims| claims.get("perm").and_then(serde_json::Value::as_u64));
    match perm {
        Some(perm) if perm & PERMISSION_UPDATE == 0 => Access::Read,
        _ => Access::Edit,
    }
}

/// Whether a resolved file URL may be fetched with the user's cookies: plain HTTP(S) on the
/// page's own origin only, so a forged `fileUrl` cannot point the session's credentials anywhere
/// else.
pub fn same_origin(protocol: &str, origin: &str, page_origin: &str) -> bool {
    matches!(protocol, "http:" | "https:") && !origin.is_empty() && origin == page_origin
}

/// Whether a resolved relay URL may be connected to with the user's ticket: HTTP(S) or
/// WebSocket on the page's own host only (a relative `/craft` resolves to it), so a forged
/// `relayUrl` cannot send the ticket anywhere else.
pub fn same_host(protocol: &str, host: &str, page_host: &str) -> bool {
    matches!(protocol, "http:" | "https:" | "ws:" | "wss:") && !host.is_empty() && host.eq_ignore_ascii_case(page_host)
}

/// When the window joins a room, as the parent's `live` messages arrive: the first one fetches
/// the file and joins; later ones for the joined room are fresh tickets. A fetch or a join that
/// failed lets the next message try again.
#[derive(Debug, Default, PartialEq, Eq)]
pub enum Joining {
    #[default]
    Idle,
    /// Fetching the file, then joining.
    Opening,
    /// Joined this room, with this access.
    Joined(String, Access),
}

/// What to do with a `live` message.
#[derive(Debug, PartialEq, Eq)]
pub enum Next {
    /// Fetch the file and join.
    Open,
    /// Hand the ticket to the hub (or keep it for the join in progress).
    Refresh,
    /// A message for another room while still joined to one: the session was restarted (the
    /// room was closed and opened again), so the link ends ([`RESTARTED`]).
    Restarted,
    /// A ticket for the room that no longer lets this window edit: the link ends
    /// ([`ACCESS_CHANGED`]).
    AccessDropped,
}

impl Next {
    /// Why the link ends, for a message that ends it.
    pub fn end_reason(&self) -> Option<&'static str> {
        match self {
            Next::Restarted => Some(RESTARTED),
            Next::AccessDropped => Some(ACCESS_CHANGED),
            Next::Open | Next::Refresh => None,
        }
    }
}

impl Joining {
    /// A `live` message for `room`, with a ticket granting `access`, arrived.
    pub fn message(&mut self, room: &str, access: Access) -> Next {
        match self {
            Joining::Idle => {
                *self = Joining::Opening;
                Next::Open
            }
            Joining::Opening => Next::Refresh,
            Joining::Joined(joined, _) if joined != room => Next::Restarted,
            Joining::Joined(_, Access::Edit) if access == Access::Read => Next::AccessDropped,
            Joining::Joined(..) => Next::Refresh,
        }
    }

    /// The fetch or the join failed: the next message opens again.
    pub fn failed(&mut self) {
        *self = Joining::Idle;
    }

    /// The window joined `room` with `access`.
    pub fn joined(&mut self, room: &str, access: Access) {
        *self = Joining::Joined(room.to_owned(), access);
    }
}

/// The `live-state` value for the window's report state ([`wordcraft_ui_egui::live::LiveReport`]).
pub fn live_status(state: &str) -> LiveStatus {
    match state {
        "connecting" => LiveStatus::Connecting,
        "live" => LiveStatus::Live,
        "readonly" => LiveStatus::Readonly,
        "reconnecting" => LiveStatus::Reconnecting,
        "broken" => LiveStatus::Broken,
        _ => LiveStatus::Refused,
    }
}

/// Whether this window guards leaving the page itself (`beforeunload`): when it is not hosted,
/// and framed by a page (`?host=parent`), whose own guard cannot see the frame's unsaved changes.
/// A page that runs WordCraft in its own document (`data-host="page"`) guards it.
pub fn guards_own_leaving(mode: Option<Mode>) -> bool {
    mode != Some(Mode::Page)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket(claims: &str) -> String {
        format!("{}.c2lnbmF0dXJl", URL_SAFE_NO_PAD.encode(claims))
    }

    #[test]
    fn a_read_ticket_starts_read_only() {
        let read = ticket(r#"{"rm":"oc:156","uid":"bob","dsp":"bob","perm":1,"exp":1795198977,"pid":"bob~bd68c9c8"}"#);
        let edit = ticket(r#"{"rm":"oc:155","uid":"bob","dsp":"bob","perm":3,"exp":1795198977,"pid":"bob~7260a2f2"}"#);
        assert_eq!(ticket_access(&read), Access::Read);
        assert_eq!(ticket_access(&edit), Access::Edit);
        assert_eq!(ticket_access("not a ticket"), Access::Edit, "the relay decides what it can't be read for");
    }

    #[test]
    fn only_the_pages_own_origin_is_fetched() {
        let page = "https://cloud.example";
        assert!(same_origin("https:", "https://cloud.example", page));
        assert!(!same_origin("https:", "https://evil.example", page));
        assert!(!same_origin("http:", "http://cloud.example", page), "another scheme is another origin");
        assert!(!same_origin("data:", "null", "null"), "no data or blob URLs");
    }

    #[test]
    fn only_the_pages_own_host_gets_the_ticket() {
        let page = "cloud.example";
        assert!(same_host("wss:", "cloud.example", page));
        assert!(same_host("https:", "Cloud.Example", page), "hosts compare without case");
        assert!(!same_host("wss:", "relay.evil.example", page));
        assert!(!same_host("wss:", "cloud.example:8443", page), "another port is another host");
        assert!(!same_host("javascript:", "", page));
    }

    #[test]
    fn a_failed_start_is_tried_again() {
        let mut j = Joining::default();
        assert_eq!(j.message("oc:1", Access::Edit), Next::Open);
        assert_eq!(j.message("oc:1", Access::Edit), Next::Refresh, "a ticket while the file is fetched");
        j.failed();
        assert_eq!(j.message("oc:1", Access::Edit), Next::Open, "the next message opens again");
        j.joined("oc:1", Access::Edit);
        assert_eq!(j.message("oc:1", Access::Edit), Next::Refresh, "joined: a fresh ticket");
    }

    #[test]
    fn another_room_after_the_join_ends_the_link() {
        // The worker closed the room while this window was away; the page reopened the file
        // and now sends the new room's ticket.
        let mut j = Joining::default();
        assert_eq!(j.message("oc:155:aaa", Access::Edit), Next::Open);
        j.joined("oc:155:aaa", Access::Edit);
        let next = j.message("oc:155:bbb", Access::Edit);
        assert_eq!(next, Next::Restarted);
        assert_eq!(next.end_reason(), Some(RESTARTED));
    }

    #[test]
    fn less_access_after_the_join_ends_the_link() {
        let mut j = Joining::default();
        j.message("oc:1", Access::Edit);
        j.joined("oc:1", Access::Edit);
        let next = j.message("oc:1", Access::Read);
        assert_eq!(next, Next::AccessDropped, "edit to read (perm 3 to 1)");
        assert_eq!(next.end_reason(), Some(ACCESS_CHANGED));
        let mut r = Joining::default();
        r.message("oc:1", Access::Read);
        r.joined("oc:1", Access::Read);
        assert_eq!(r.message("oc:1", Access::Read), Next::Refresh, "a read ticket refreshed stays");
    }

    #[test]
    fn a_broken_window_is_reported_apart_from_a_refused_one() {
        assert_eq!(live_status("broken"), LiveStatus::Broken);
        assert_eq!(live_status("refused"), LiveStatus::Refused);
        assert_eq!(live_status("live"), LiveStatus::Live);
    }

    #[test]
    fn a_framed_window_guards_leaving_itself() {
        assert!(guards_own_leaving(Some(Mode::Parent)), "the framing page can't see the frame's unsaved changes");
        assert!(guards_own_leaving(None), "the plain app");
        assert!(!guards_own_leaving(Some(Mode::Page)), "a page running WordCraft in its document guards it");
    }
}

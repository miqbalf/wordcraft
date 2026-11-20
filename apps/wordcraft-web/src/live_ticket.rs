//! What the web app reads off a live ticket and a file URL before it joins a room. Pure, so it
//! is tested natively; `host` (wasm32) does the browser part.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use wordcraft_ui_egui::live::Access;

/// The relay's permission bit for changing the file (OCS numbering: read 1, update 2).
const PERMISSION_UPDATE: u64 = 2;

/// The access a ticket grants, read from its payload (`payload.signature`, base64url JSON with
/// `perm`). Only the interface uses it, to start a read-only ticket read-only; the relay checks
/// the signature and enforces the permission. A ticket that can't be read is taken as an edit
/// ticket: the relay then refuses its first edit, and the window says it is read-only.
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
}

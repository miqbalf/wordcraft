use serde_json::json;
use wordcraft_engine::proof_remote::{OffsetUnit, RemoteKind};

use super::*;

/// The desktop reads the route's 200 body itself: the same caps and kinds as the page's bridge.
#[test]
fn a_route_body_is_read_with_the_contract_caps() {
    let one = json!({"offset": 4, "length": 2, "kind": "grammar", "message": "m".repeat(900), "replacements": ["a", "b", "c", "d", "e", "f", "x".repeat(201)], "rule": "R"});
    let style = json!({"offset": 0, "length": 1, "kind": "style", "message": "s", "replacements": []});
    let many: Vec<_> = std::iter::repeat_n(one.clone(), MAX_MATCHES + 3).chain([style]).collect();
    let reply = proof_reply(&json!({"matches": many, "indonesian": [[0, 5], [1], "x"]})).unwrap_or_else(|| panic!("a body"));
    assert!(reply.matches.len() <= MAX_MATCHES);
    assert!(reply.matches.iter().all(|m| m.kind == RemoteKind::Grammar), "style never passes");
    let first = &reply.matches[0];
    assert!(first.message.chars().count() <= MAX_MESSAGE_CHARS);
    assert!(first.replacements.len() <= MAX_REPLACEMENTS && first.replacements.iter().all(|r| r.chars().count() <= MAX_REPLACEMENT_CHARS));
    assert_eq!(reply.indonesian, vec![(0, 5)], "only well-formed spans");
    assert_eq!(reply.unit, OffsetUnit::CodePoints);
    assert!(proof_reply(&json!({"matches": []})).is_none(), "no indonesian: not a body");
    assert!(proof_reply(&json!({"error": "busy"})).is_none());
}

/// A status the desktop meets is the code the page would have passed on, and the token routes'
/// 401 is its own code, so the window can say to sign in instead of "unavailable".
#[test]
fn statuses_become_the_pages_codes_and_401_is_signed_out() {
    for (status, code) in [(413, "too_large"), (429, "busy"), (403, "disabled"), (400, "bad_request"), (401, SIGNED_OUT)] {
        assert_eq!(code_for_status(status), code);
    }
    for status in [500, 502, 503, 404, 302, 0] {
        assert_eq!(code_for_status(status), UNAVAILABLE, "{status}");
    }
    assert_eq!(known_code("<b>boom</b>"), UNAVAILABLE);
}

#[test]
fn an_ai_body_over_the_cap_or_without_text_fails() {
    assert_eq!(ai_text(&json!({"text": "went"})), Ok("went".to_string()));
    assert_eq!(ai_text(&json!({"text": "x".repeat(MAX_AI_TEXT_BYTES + 1)})), Err("too_large"));
    assert_eq!(ai_text(&json!({"text": 3})), Err(UNAVAILABLE));
}

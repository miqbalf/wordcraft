use serde_json::{Value, json};
use wordcraft_engine::proof_remote::{OffsetUnit, ProofReply, RemoteKind};
use wordcraft_ui_egui::assist::{AiTask, AssistIn, AssistOut};

use super::*;

fn p(v: Value) -> Option<AssistIn> {
    parse(&v.to_string())
}

fn reply(v: Value) -> ProofReply {
    match p(v) {
        Some(AssistIn::ProofResult { reply, .. }) => reply,
        other => panic!("not a proof result: {other:?}"),
    }
}

fn one_match(extra: Value) -> Value {
    let mut m = json!({"offset": 4, "length": 3, "kind": "grammar", "message": "m", "replacements": ["a"], "rule": "R"});
    if let (Some(m), Some(extra)) = (m.as_object_mut(), extra.as_object()) {
        for (k, v) in extra {
            m.insert(k.clone(), v.clone());
        }
    }
    m
}

#[test]
fn parses_each_incoming_type_and_refuses_bad_ids_and_wrong_types() {
    assert_eq!(p(json!({"type": "wordcraft:assist", "proof": true, "ai": false})), Some(AssistIn::Enabled { proof: true, ai: false }));
    assert_eq!(
        p(json!({"type": "wordcraft:ai-result", "id": "ai-1", "text": "Hi"})),
        Some(AssistIn::AiResult { id: "ai-1".into(), text: "Hi".into() })
    );
    assert_eq!(
        p(json!({"type": "wordcraft:ai-failed", "id": "ai_1", "error": "busy"})),
        Some(AssistIn::AiFailed { id: "ai_1".into(), error: "busy".into() })
    );
    assert_eq!(
        p(json!({"type": "wordcraft:proof-failed", "id": "proof-9", "error": "unavailable"})),
        Some(AssistIn::ProofFailed { id: "proof-9".into(), error: "unavailable".into() })
    );
    let r = reply(json!({"type": "wordcraft:proof-result", "id": "proof-2", "matches": [one_match(json!({}))], "indonesian": [[0, 5]]}));
    assert_eq!(r.matches.len(), 1);
    assert_eq!((r.matches[0].offset, r.matches[0].length, r.matches[0].kind), (4, 3, RemoteKind::Grammar));
    assert_eq!(r.matches[0].rule, "R");
    assert_eq!(r.indonesian, vec![(0, 5)]);

    // Ids: 1 to 64 of [A-Za-z0-9_-].
    for bad in ["", "a b", "a/b", "é", &"x".repeat(65)] {
        assert_eq!(p(json!({"type": "wordcraft:ai-result", "id": bad, "text": "t"})), None, "id {bad:?}");
    }
    assert!(p(json!({"type": "wordcraft:ai-result", "id": "x".repeat(64), "text": "t"})).is_some());
    assert_eq!(p(json!({"type": "wordcraft:ai-result", "text": "t"})), None, "no id");
    assert_eq!(p(json!({"type": "wordcraft:ai-result", "id": 7, "text": "t"})), None, "id of the wrong type");
    // Wrong field types, unknown types, other prefixes, and what is not an object.
    assert_eq!(p(json!({"type": "wordcraft:ai-result", "id": "a", "text": 5})), None);
    assert_eq!(p(json!({"type": "wordcraft:assist", "proof": "yes", "ai": false})), None);
    assert_eq!(p(json!({"type": "wordcraft:assist", "proof": true})), None);
    // M5: a proof-result with a good id and a bad body fails that request at once, so it does
    // not wait out the reply timeout and the 60 s backoff.
    let failed = Some(AssistIn::ProofFailed { id: "a".into(), error: "unavailable".into() });
    assert_eq!(p(json!({"type": "wordcraft:proof-result", "id": "a", "matches": {}, "indonesian": []})), failed);
    assert_eq!(p(json!({"type": "wordcraft:proof-result", "id": "a", "matches": [], "indonesian": 3})), failed);
    assert_eq!(p(json!({"type": "wordcraft:proof-result", "id": "bad id", "matches": {}, "indonesian": []})), None, "no usable id: nobody to fail");
    assert_eq!(p(json!({"type": "wordcraft:proof-failed", "id": "a", "error": 4})), None);
    assert_eq!(p(json!({"type": "wordcraft:open", "id": "a"})), None);
    assert_eq!(p(json!({"type": "pdfcraft:ai-result", "id": "a", "text": "t"})), None);
    assert_eq!(p(json!({"id": "a"})), None);
    assert_eq!(parse("not json"), None);
    assert_eq!(parse("[1,2]"), None);
    // A match with an unknown kind or a bad number is skipped; the others stay. Style never passes.
    let r = reply(json!({"type": "wordcraft:proof-result", "id": "a", "indonesian": [], "matches": [
        one_match(json!({"kind": "style"})), one_match(json!({"offset": -1})), one_match(json!({"length": "3"})),
        one_match(json!({"message": 5})), one_match(json!({"kind": "spelling"}))]}));
    assert_eq!(r.matches.len(), 1);
    assert_eq!(r.matches[0].kind, RemoteKind::Spelling);
    // A bad span in `indonesian` is skipped.
    let r = reply(json!({"type": "wordcraft:proof-result", "id": "a", "matches": [], "indonesian": [[1, 2], [1], ["a", 2], [-1, 2], [3, 4]]}));
    assert_eq!(r.indonesian, vec![(1, 2), (3, 4)]);
}

#[test]
fn caps_500_matches_5_replacements_and_message_length() {
    let many: Vec<Value> = (0..600).map(|i| one_match(json!({"offset": i}))).collect();
    let r = reply(json!({"type": "wordcraft:proof-result", "id": "a", "matches": many, "indonesian": []}));
    assert_eq!(r.matches.len(), 500);
    assert_eq!(r.matches[499].offset, 499, "the first 500 stay");

    let long = "é".repeat(900);
    let m = one_match(json!({"message": long, "rule": "r".repeat(400),
        "replacements": ["1", "2", "3", "4", "5", "6", "7", "x".repeat(201), "y".repeat(200)]}));
    let r = reply(json!({"type": "wordcraft:proof-result", "id": "a", "matches": [m], "indonesian": []}));
    assert_eq!(r.matches[0].message.chars().count(), 500, "cut at 500 characters, not bytes");
    assert_eq!(r.matches[0].replacements, vec!["1", "2", "3", "4", "5"]);
    assert!(r.matches[0].rule.chars().count() <= 200);
    // A replacement over 200 characters is left out, and the next one takes its place.
    let m = one_match(json!({"replacements": ["x".repeat(201), "y".repeat(200), 7, "z"]}));
    let r = reply(json!({"type": "wordcraft:proof-result", "id": "a", "matches": [m], "indonesian": []}));
    assert_eq!(r.matches[0].replacements, vec!["y".repeat(200), "z".to_string()]);
    // The spans are capped too.
    let spans: Vec<Value> = (0..2500).map(|i| json!([i, 1])).collect();
    let r = reply(json!({"type": "wordcraft:proof-result", "id": "a", "matches": [], "indonesian": spans}));
    assert_eq!(r.indonesian.len(), 2000);
}

#[test]
fn the_offset_unit_is_the_contracts_code_points_unless_the_message_names_one() {
    // The route hands back code points (craft_office converts LanguageTool's UTF-16 itself);
    // the unit is set here, never taken from the engine's default.
    let msg = |unit: Value| {
        let mut v = json!({"type": "wordcraft:proof-result", "id": "a", "matches": [one_match(json!({}))], "indonesian": []});
        if !unit.is_null() {
            v["unit"] = unit;
        }
        v
    };
    assert_eq!(reply(msg(json!(null))).unit, OffsetUnit::CodePoints);
    assert_eq!(reply(msg(json!("codepoints"))).unit, OffsetUnit::CodePoints);
    assert_eq!(reply(msg(json!("utf16"))).unit, OffsetUnit::Utf16);
    let failed = Some(AssistIn::ProofFailed { id: "a".into(), error: "unavailable".into() });
    assert_eq!(p(msg(json!("utf8"))), failed, "an unknown unit is refused, not guessed");
    assert_eq!(p(msg(json!(16))), failed);
}

#[test]
fn builds_proof_check_and_ai_with_the_contract_fields() {
    let v: Value =
        serde_json::from_str(&build(&AssistOut::ProofCheck {
            id: "proof-1".into(), language: "auto".into(), text: "Teh \"cat\"\n\nsat 😀.".into()
        }))
        .expect("json");
    assert_eq!(v, json!({"type": "wordcraft:proof-check", "id": "proof-1", "language": "auto", "text": "Teh \"cat\"\n\nsat 😀."}));
    let v: Value =
        serde_json::from_str(&build(&AssistOut::Ai { id: "ai-2".into(), task: AiTask::Improve, language: "fr-FR".into(), text: "Bonjour".into() }))
            .expect("json");
    assert_eq!(v, json!({"type": "wordcraft:ai", "id": "ai-2", "task": "improve", "language": "fr-FR", "text": "Bonjour"}));
    let v: Value =
        serde_json::from_str(&build(&AssistOut::Ai { id: "ai-3".into(), task: AiTask::Summarize, language: "auto".into(), text: "T".into() }))
            .expect("json");
    assert_eq!(v["task"], "summarize");
    assert_eq!(v.as_object().map(|o| o.len()), Some(5), "no other field");
}

/// n-2: the same emoji text, read in each unit, lands on the same word in the engine.
#[test]
fn an_emoji_reply_lands_on_the_right_word_in_each_unit() {
    use wordcraft_engine::Session;
    let text = "😀 Teh cat";
    // "Teh" starts after the emoji and a space: code point 2, UTF-16 unit 3.
    for (unit, offset) in [("codepoints", 2), ("utf16", 3)] {
        let mut s = Session::new(wordcraft_doc::Document::new());
        s.proof_remote.set_enabled(true);
        s.run("text.insert", &json!({"text": text})).expect("insert");
        let mut at = 0.0;
        let req = loop {
            if let Some(r) = s.proof_poll(at).send.into_iter().next() {
                break r;
            }
            at += 250.0;
            assert!(at < 10_000.0, "nothing was sent");
        };
        assert_eq!(req.text, text);
        let json = json!({"type": "wordcraft:proof-result", "id": req.id, "unit": unit, "indonesian": [],
            "matches": [one_match(json!({"offset": offset, "length": 3}))]});
        let Some(AssistIn::ProofResult { id, reply }) = parse(&json.to_string()) else { panic!("parse") };
        s.proof_receive(&id, Ok(reply), at + 10.0);
        let issues = s.run("review.issues", &json!({})).expect("issues");
        let hit: Vec<&str> = issues.as_array().into_iter().flatten().filter(|i| i["source"] == "remote").filter_map(|i| i["text"].as_str()).collect();
        assert_eq!(hit, vec!["Teh"], "unit {unit}");
    }
}

/// I1: the AI answer is bounded like the longest request the page accepts (summarize, 200 KB).
#[test]
fn ai_result_text_is_capped_at_the_pages_summarize_limit() {
    let at = |n: usize| p(json!({"type": "wordcraft:ai-result", "id": "a", "text": "x".repeat(n)}));
    assert!(matches!(at(204_800), Some(AssistIn::AiResult { .. })));
    // Over the cap the request fails as too large, so the modal that waits for it stops waiting.
    let too_large = Some(AssistIn::AiFailed { id: "a".into(), error: "too_large".into() });
    assert_eq!(at(204_801), too_large);
    // The cap counts UTF-8 bytes, not characters.
    assert_eq!(p(json!({"type": "wordcraft:ai-result", "id": "a", "text": "é".repeat(102_401)})), too_large);
    // With no usable id there is nothing to fail.
    assert_eq!(p(json!({"type": "wordcraft:ai-result", "id": "a b", "text": "x".repeat(204_801)})), None);
}

/// M4: a host error code is one the app knows; anything else is "unavailable".
#[test]
fn host_error_codes_are_an_allow_list() {
    for code in ["busy", "unavailable", "too_large", "disabled", "bad_request"] {
        assert_eq!(
            p(json!({"type": "wordcraft:ai-failed", "id": "a", "error": code})),
            Some(AssistIn::AiFailed { id: "a".into(), error: code.into() })
        );
    }
    assert_eq!(
        p(json!({"type": "wordcraft:ai-failed", "id": "a", "error": "<b>boom</b>"})),
        Some(AssistIn::AiFailed { id: "a".into(), error: "unavailable".into() })
    );
    assert_eq!(
        p(json!({"type": "wordcraft:proof-failed", "id": "a", "error": "weird"})),
        Some(AssistIn::ProofFailed { id: "a".into(), error: "unavailable".into() })
    );
}

/// M1: only the assist types are read from the page's messages at all.
#[test]
fn only_assist_types_are_wanted() {
    for t in ["wordcraft:assist", "wordcraft:proof-result", "wordcraft:proof-failed", "wordcraft:ai-result", "wordcraft:ai-failed"] {
        assert!(wants(t), "{t}");
    }
    for t in ["wordcraft:open", "wordcraft:host", "wordcraft:saved", "wordcraft:", "live", "pdfcraft:ai-result", ""] {
        assert!(!wants(t), "{t}");
    }
}

/// M3: an opaque origin ("null", a sandboxed frame) cannot be posted to, so the feature is off
/// there rather than failing every send.
#[test]
fn an_opaque_origin_is_not_usable() {
    assert!(usable_origin("https://cloud.example.org"));
    assert!(usable_origin("http://localhost:8080"));
    assert!(!usable_origin("null"));
    assert!(!usable_origin(""));
}

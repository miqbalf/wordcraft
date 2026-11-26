//! The `wordcraft:` assist messages as JSON: what the page sends is checked field by field and
//! capped as the route's contract says, and what the app sends is built as the page reads it.
//! Pure, so it runs natively; `assist` (wasm) does the posting.

use serde_json::{Value, json};
use wordcraft_engine::proof_remote::{OffsetUnit, ProofReply, RemoteKind, RemoteMatch};
use wordcraft_ui_egui::assist::{AiTask, AssistIn, AssistOut};

/// The route's limits (the engine applies the same ones again).
const MAX_MATCHES: usize = 500;
const MAX_MESSAGE_CHARS: usize = 500;
const MAX_REPLACEMENTS: usize = 5;
const MAX_REPLACEMENT_CHARS: usize = 200;
const MAX_RULE_CHARS: usize = 200;
const MAX_SPANS: usize = 2000;
/// The longest `ai-result` text in UTF-8 bytes: the page's summarize limit (`LIMITS.summarize`).
const MAX_AI_TEXT_BYTES: usize = 204_800;

/// Whether `kind` (a message's `type`) is one the app reads; the listener checks it before it
/// serialises anything.
pub fn wants(kind: &str) -> bool {
    matches!(kind.strip_prefix("wordcraft:"), Some("assist" | "proof-result" | "proof-failed" | "ai-result" | "ai-failed"))
}

/// Whether messages can be posted to `origin`: not an opaque one.
pub fn usable_origin(origin: &str) -> bool {
    !origin.is_empty() && origin != "null"
}

/// One message from the page, or `None` when it is not a usable assist message: another type or
/// prefix, a field of the wrong type, or an id outside `[A-Za-z0-9_-]{1,64}`.
pub fn parse(json: &str) -> Option<AssistIn> {
    let v: Value = serde_json::from_str(json).ok()?;
    let kind = v.get("type")?.as_str()?.strip_prefix("wordcraft:")?;
    match kind {
        "assist" => Some(AssistIn::Enabled { proof: v.get("proof")?.as_bool()?, ai: v.get("ai")?.as_bool()? }),
        // A good id with a bad body fails that request now, instead of leaving it to time out.
        "proof-result" => Some(match reply(&v) {
            Some(reply) => AssistIn::ProofResult { id: id(&v)?, reply },
            None => AssistIn::ProofFailed { id: id(&v)?, error: UNAVAILABLE.to_string() },
        }),
        "proof-failed" => Some(AssistIn::ProofFailed { id: id(&v)?, error: error(&v)? }),
        "ai-result" => {
            let text = v.get("text")?.as_str().filter(|t| t.len() <= MAX_AI_TEXT_BYTES)?;
            Some(AssistIn::AiResult { id: id(&v)?, text: text.to_string() })
        }
        "ai-failed" => Some(AssistIn::AiFailed { id: id(&v)?, error: error(&v)? }),
        _ => None,
    }
}

/// A message to the page as JSON: the `wordcraft:proof-check` or `wordcraft:ai` object.
pub fn build(out: &AssistOut) -> String {
    match out {
        AssistOut::ProofCheck { id, language, text } => json!({"type": "wordcraft:proof-check", "id": id, "language": language, "text": text}),
        AssistOut::Ai { id, task, language, text } => {
            let task = match task {
                AiTask::Improve => "improve",
                AiTask::Summarize => "summarize",
            };
            json!({"type": "wordcraft:ai", "id": id, "task": task, "language": language, "text": text})
        }
    }
    .to_string()
}

fn id(v: &Value) -> Option<String> {
    let id = v.get("id")?.as_str()?;
    (!id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')).then(|| id.to_string())
}

const UNAVAILABLE: &str = "unavailable";

/// The error code: one of the page's, and "unavailable" for any other string.
fn error(v: &Value) -> Option<String> {
    let code = v.get("error")?.as_str()?;
    Some(if matches!(code, "busy" | "too_large" | "disabled" | "bad_request") { code } else { UNAVAILABLE }.to_string())
}

/// A non-negative whole number.
fn count(v: &Value) -> Option<usize> {
    usize::try_from(v.as_u64()?).ok()
}

/// The reply's offset unit. The route counts code points (craft_office turns LanguageTool's
/// UTF-16 offsets into them, so every match of a reply has one unit); a message that names
/// another unit says so, and one that names an unknown unit is refused.
fn unit(v: &Value) -> Option<OffsetUnit> {
    match v.get("unit") {
        None => Some(OffsetUnit::CodePoints),
        Some(u) => match u.as_str()? {
            "codepoints" => Some(OffsetUnit::CodePoints),
            "utf16" => Some(OffsetUnit::Utf16),
            _ => None,
        },
    }
}

fn reply(v: &Value) -> Option<ProofReply> {
    let matches = v.get("matches")?.as_array()?.iter().filter_map(remote_match).take(MAX_MATCHES).collect();
    let indonesian = v
        .get("indonesian")?
        .as_array()?
        .iter()
        .filter_map(|s| {
            let s = s.as_array()?;
            (s.len() == 2).then_some(())?;
            Some((count(s.first()?)?, count(s.get(1)?)?))
        })
        .take(MAX_SPANS)
        .collect();
    Some(ProofReply { matches, indonesian, unit: unit(v)? })
}

/// One match, or `None` for one that is not LanguageTool's or Hermes's spelling or grammar
/// (style never passes) or has a bad field.
fn remote_match(m: &Value) -> Option<RemoteMatch> {
    let kind = match m.get("kind")?.as_str()? {
        "spelling" => RemoteKind::Spelling,
        "grammar" => RemoteKind::Grammar,
        _ => return None,
    };
    let replacements = m
        .get("replacements")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .filter(|r| r.chars().count() <= MAX_REPLACEMENT_CHARS)
        .take(MAX_REPLACEMENTS)
        .map(str::to_string)
        .collect();
    Some(RemoteMatch {
        offset: count(m.get("offset")?)?,
        length: count(m.get("length")?)?,
        kind,
        message: m.get("message")?.as_str()?.chars().take(MAX_MESSAGE_CHARS).collect(),
        replacements,
        rule: m.get("rule").and_then(Value::as_str).unwrap_or_default().chars().take(MAX_RULE_CHARS).collect(),
    })
}

#[cfg(test)]
mod tests;

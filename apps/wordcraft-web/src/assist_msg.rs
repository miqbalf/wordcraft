//! The `wordcraft:` assist messages as JSON: what the page sends is checked field by field and
//! capped as the route's contract says, and what the app sends is built as the page reads it.
//! Pure, so it runs natively; `assist` (wasm) does the posting.

use serde_json::{Value, json};
use wordcraft_engine::proof_remote::ProofReply;
use wordcraft_ui_egui::assist::{AiTask, AssistIn, AssistOut, wire};

/// The longest `ai-result` text in UTF-8 bytes: the page's summarize limit (`LIMITS.summarize`).
const MAX_AI_TEXT_BYTES: usize = wire::MAX_AI_TEXT_BYTES;

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
            let text = v.get("text")?.as_str()?;
            // Over the cap the request fails, so whatever waits for it stops waiting.
            if text.len() > MAX_AI_TEXT_BYTES {
                return Some(AssistIn::AiFailed { id: id(&v)?, error: "too_large".to_string() });
            }
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

const UNAVAILABLE: &str = wire::UNAVAILABLE;

/// The error code: one of the page's, and "unavailable" for any other string.
fn error(v: &Value) -> Option<String> {
    Some(wire::known_code(v.get("error")?.as_str()?).to_string())
}

/// A `proof-result`'s body: the route's 200 body, read as the desktop reads it (`wire`).
fn reply(v: &Value) -> Option<ProofReply> {
    wire::proof_reply(v)
}

#[cfg(test)]
mod tests;

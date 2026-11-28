//! The craft_office assist routes' answers as JSON, shared by both hosts: the web page bridge
//! (`wordcraft-web`'s `assist_msg`, whose `proof-result` carries the route's 200 body) and the
//! desktop app's own HTTP client (`apps/wordcraft`'s `assist_net`), so the two never read the
//! routes' contract differently. Pure, so it runs natively.

use serde_json::Value;
use wordcraft_engine::proof_remote::{OffsetUnit, ProofReply, RemoteKind, RemoteMatch};

/// The route's limits (the engine applies the same ones again).
pub const MAX_MATCHES: usize = 500;
pub const MAX_MESSAGE_CHARS: usize = 500;
pub const MAX_REPLACEMENTS: usize = 5;
pub const MAX_REPLACEMENT_CHARS: usize = 200;
pub const MAX_RULE_CHARS: usize = 200;
pub const MAX_SPANS: usize = 2000;
/// The longest AI answer text in UTF-8 bytes: the summarize limit (`LIMITS.summarize`).
pub const MAX_AI_TEXT_BYTES: usize = 204_800;

/// The code every failure the app does not know becomes.
pub const UNAVAILABLE: &str = "unavailable";
/// The desktop's code for a missing, expired or refused assist token (the token routes' 401).
pub const SIGNED_OUT: &str = "signed_out";

/// An error code as the hosts pass it on: one the app knows, and "unavailable" for any other.
pub fn known_code(code: &str) -> &'static str {
    match code {
        "busy" => "busy",
        "too_large" => "too_large",
        "disabled" => "disabled",
        "bad_request" => "bad_request",
        _ => UNAVAILABLE,
    }
}

/// The code for an HTTP status of a route, as the page maps it (`assist.js`): 413 too_large,
/// 429 busy, 403 disabled, 400 bad_request, 401 signed_out (the desktop's token routes), and
/// anything else unavailable.
pub fn code_for_status(status: u16) -> &'static str {
    match status {
        413 => "too_large",
        429 => "busy",
        403 => "disabled",
        400 => "bad_request",
        401 => SIGNED_OUT,
        _ => UNAVAILABLE,
    }
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

/// The proof route's 200 body (`{matches, indonesian}`), capped as the contract says, or `None`
/// when it is not one.
pub fn proof_reply(v: &Value) -> Option<ProofReply> {
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

/// The AI routes' 200 body (`{text}`): the text, or the code to fail with (`too_large` over
/// [`MAX_AI_TEXT_BYTES`], `unavailable` for a body that is not one).
pub fn ai_text(v: &Value) -> Result<String, &'static str> {
    let text = v.get("text").and_then(Value::as_str).ok_or(UNAVAILABLE)?;
    if text.len() > MAX_AI_TEXT_BYTES {
        return Err("too_large");
    }
    Ok(text.to_string())
}

#[cfg(test)]
mod tests;

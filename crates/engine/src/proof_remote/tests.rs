//! The remote proof source driven by hand: `now_ms` is set by each test, and replies come back
//! when a test says so.

use wordcraft_doc::props::CharProps;
use wordcraft_doc::{Document, Path, StoryRef};
use wordcraft_proof::{IssueKind, ProofLang};

use super::*;

fn doc(paras: &[&str]) -> Document {
    Document::from_text(&paras.join("\n"))
}

fn on() -> RemoteProof {
    let mut rp = RemoteProof::default();
    rp.set_enabled(true);
    rp
}

fn para(d: &Document, k: usize) -> &wordcraft_doc::Paragraph {
    d.para(StoryRef::Body, &Path::top(k)).expect("paragraph")
}

fn rev(d: &Document, k: usize) -> u64 {
    para(d, k).rev
}

/// Replace paragraph `k`'s text, as typing would (a new paragraph revision).
fn edit(d: &mut Document, k: usize, text: &str) {
    let p = d.para_mut(StoryRef::Body, &Path::top(k)).expect("paragraph");
    let n = p.len();
    p.delete(0, n).expect("delete");
    p.insert_text(0, text, &CharProps::default()).expect("insert");
}

/// Format bytes `a..b` of paragraph `k`.
fn format(d: &mut Document, k: usize, a: usize, b: usize, f: &dyn Fn(&mut CharProps)) {
    d.para_mut(StoryRef::Body, &Path::top(k)).expect("paragraph").format(a, b, f).expect("format");
}

fn lang(d: &mut Document, k: usize, tag: &str) {
    let n = para(d, k).len();
    let tag = tag.to_string();
    format(d, k, 0, n, &move |c| c.lang = Some(tag.clone()));
}

/// The code-point offset of `word` in `text`.
fn cp(text: &str, word: &str) -> usize {
    let at = text.find(word).unwrap_or_else(|| panic!("{word:?} not in {text:?}"));
    text[..at].chars().count()
}

fn m(offset: usize, length: usize, kind: RemoteKind, rule: &str) -> RemoteMatch {
    RemoteMatch { offset, length, kind, message: format!("remote {rule}"), replacements: vec!["fix".into()], rule: rule.into() }
}

/// A match on `word` in the request.
fn on_word(req: &ProofRequest, word: &str, kind: RemoteKind, rule: &str) -> RemoteMatch {
    m(cp(&req.text, word), word.chars().count(), kind, rule)
}

fn ok(matches: Vec<RemoteMatch>) -> Result<ProofReply, ProofFailure> {
    Ok(ProofReply { matches, ..Default::default() })
}

/// The UTF-16 offset of `word` in `text`.
fn u16_at(text: &str, word: &str) -> usize {
    let at = text.find(word).unwrap_or_else(|| panic!("{word:?} not in {text:?}"));
    text[..at].encode_utf16().count()
}

/// The one request a poll sends.
fn one(p: Poll) -> ProofRequest {
    assert_eq!(p.send.len(), 1, "one request: {p:?}");
    p.send.into_iter().next().expect("request")
}

/// The remote issues on paragraph `k` as (start, end, kind, rule).
fn remote(rp: &RemoteProof, d: &Document, k: usize) -> Vec<(usize, usize, IssueKind, String)> {
    let Some(o) = rp.overlays(0) else { return Vec::new() };
    let Some((_, o)) = o.get(rev(d, k)) else { return Vec::new() };
    o.remote.iter().map(|r| (r.issue.start, r.issue.end, r.issue.kind, r.rule.clone())).collect()
}

/// The first request: the document is seen at 0 ms and sent once it has been idle for
/// [`IDLE_MS`].
fn first(rp: &mut RemoteProof, d: &Document) -> ProofRequest {
    assert!(rp.poll(d, 1, 0.0).send.is_empty(), "not before the idle wait");
    one(rp.poll(d, 1, IDLE_MS))
}

/// Polls every 250 ms from `t` until a poll sends or `until` passes; answers nothing.
fn first_send(rp: &mut RemoteProof, d: &Document, doc_rev: u64, mut t: f64, until: f64) -> Option<(f64, ProofRequest)> {
    while t <= until {
        let p = rp.poll(d, doc_rev, t);
        if let Some(r) = p.send.into_iter().next() {
            return Some((t, r));
        }
        t += 250.0;
    }
    None
}

/// Every request until none is left, each answered at once with no matches.
fn drain(rp: &mut RemoteProof, d: &Document, doc_rev: u64, t: &mut f64) -> Vec<ProofRequest> {
    let mut out = Vec::new();
    while let Some((at, r)) = first_send(rp, d, doc_rev, *t, *t + 200_000.0) {
        rp.receive(&r.id, ok(Vec::new()), at);
        out.push(r);
        *t = at;
    }
    out
}

#[test]
fn nothing_is_sent_before_1500ms_idle_and_typing_restarts_the_wait() {
    let mut rp = on();
    let mut d = doc(&["Hello wrold."]);
    let p = rp.poll(&d, 1, 0.0);
    assert!(p.send.is_empty());
    assert!(p.wake_in_ms.is_some_and(|w| w > 0.0 && w <= IDLE_MS), "{p:?}");
    assert!(rp.poll(&d, 1, 1000.0).send.is_empty());
    edit(&mut d, 0, "Hello wrold again.");
    assert!(rp.poll(&d, 2, 1200.0).send.is_empty(), "typing restarts the wait");
    assert!(rp.poll(&d, 2, 1200.0 + IDLE_MS - 1.0).send.is_empty());
    let r = one(rp.poll(&d, 2, 1200.0 + IDLE_MS));
    assert_eq!(r.text, "Hello wrold again.");
}

#[test]
fn at_most_one_request_in_flight_and_one_per_2000ms() {
    let mut rp = on();
    let mut d = doc(&["Hello there my friend.", "Guten Tag mein Freund."]);
    lang(&mut d, 1, "de");
    let first = first(&mut rp, &d);
    assert!(rp.poll(&d, 1, IDLE_MS + 10_000.0).send.is_empty(), "one in flight at a time");
    let t = IDLE_MS + 10_000.0;
    rp.receive(&first.id, ok(Vec::new()), t);
    // The gap counts from the last send, which was long ago: the next may go now.
    let second = one(rp.poll(&d, 1, t));
    assert_ne!(second.text, first.text);
    rp.receive(&second.id, ok(Vec::new()), t + 1.0);
    // Make two more batches' worth of work and check the gap between sends.
    edit(&mut d, 0, "Hello again my friend.");
    edit(&mut d, 1, "Guten Abend mein Freund.");
    lang(&mut d, 1, "de");
    let t2 = t + 5000.0;
    assert!(rp.poll(&d, 2, t2).send.is_empty(), "idle wait");
    let a = one(rp.poll(&d, 2, t2 + IDLE_MS));
    rp.receive(&a.id, ok(Vec::new()), t2 + IDLE_MS + 1.0);
    let p = rp.poll(&d, 2, t2 + IDLE_MS + 1.0);
    assert!(p.send.is_empty(), "no second request within 2000 ms");
    assert!(p.wake_in_ms.is_some_and(|w| w > 0.0 && w <= MIN_GAP_MS), "{p:?}");
    assert!(rp.poll(&d, 2, t2 + IDLE_MS + MIN_GAP_MS - 1.0).send.is_empty());
    one(rp.poll(&d, 2, t2 + IDLE_MS + MIN_GAP_MS));
}

#[test]
fn only_changed_paragraphs_are_sent_after_the_first_check() {
    let mut rp = on();
    let mut d = doc(&["First paragraph here.", "Second paragraph here.", "Third paragraph here."]);
    let r = first(&mut rp, &d);
    assert!(["First", "Second", "Third"].iter().all(|w| r.text.contains(w)), "{r:?}");
    rp.receive(&r.id, ok(Vec::new()), IDLE_MS);
    edit(&mut d, 1, "Second paragraph changed.");
    let mut t = 5000.0;
    let sent = drain(&mut rp, &d, 2, &mut t);
    assert_eq!(sent.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(), vec!["Second paragraph changed."]);
}

#[test]
fn same_text_is_never_sent_twice() {
    let mut rp = on();
    let mut d = doc(&["Teh cat sat."]);
    let r = first(&mut rp, &d);
    rp.receive(&r.id, ok(vec![on_word(&r, "Teh", RemoteKind::Spelling, "SPELL")]), IDLE_MS);
    rp.poll(&d, 1, IDLE_MS);
    assert_eq!(remote(&rp, &d, 0), vec![(0, 3, IssueKind::Spelling, "SPELL".to_string())]);
    edit(&mut d, 0, "The cat sat.");
    let mut t = 5000.0;
    assert_eq!(drain(&mut rp, &d, 2, &mut t).len(), 1);
    // Undo back to the checked text: a new revision with the old text.
    edit(&mut d, 0, "Teh cat sat.");
    let mut t = t + 1000.0;
    assert!(drain(&mut rp, &d, 3, &mut t).is_empty(), "the cached result is used");
    assert_eq!(remote(&rp, &d, 0), vec![(0, 3, IssueKind::Spelling, "SPELL".to_string())], "overlay restored");
}

#[test]
fn batch_stays_under_8000_chars_and_20kb_and_holds_one_language() {
    let ascii = "word ".repeat(140); // 700 characters
    let emoji = "😀 ".repeat(300); // 600 characters, 1500 bytes
    let huge = "x".repeat(MAX_BATCH_CHARS + 1);
    let mut texts: Vec<String> = Vec::new();
    for i in 0..20 {
        texts.push(format!("{i} {ascii}"));
        texts.push(format!("{i} {emoji}"));
    }
    texts.insert(7, "Guten Tag mein Freund.".to_string());
    texts.insert(9, huge.clone());
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let mut d = doc(&refs);
    lang(&mut d, 7, "de");
    let mut rp = on();
    let mut t = 0.0;
    let sent = drain(&mut rp, &d, 1, &mut t);
    assert!(sent.len() > 1);
    for r in &sent {
        assert!(r.text.chars().count() <= MAX_BATCH_CHARS, "{} characters", r.text.chars().count());
        assert!(r.text.len() <= MAX_BATCH_BYTES, "{} bytes", r.text.len());
        assert_eq!(r.text.contains("Guten Tag"), r.language == "de", "one language per batch: {} {:?}", r.language, &r.text[..20]);
    }
    for (k, text) in texts.iter().enumerate() {
        let n = sent.iter().filter(|r| r.text.split("\n\n").any(|part| part == text)).count();
        if text == &huge {
            assert_eq!(n, 0, "a paragraph over the limit is not sent");
        } else {
            assert_eq!(n, 1, "paragraph {k} sent once");
        }
    }
}

#[test]
fn english_untagged_and_indonesian_runs_send_auto_an_explicit_german_run_sends_de() {
    let mut d = doc(&["Untagged text here.", "British colour here.", "Kami akan mengirimkan.", "Guten Tag mein Freund.", "Bonjour mon ami."]);
    lang(&mut d, 1, "en-GB");
    lang(&mut d, 2, "id-ID");
    lang(&mut d, 3, "de-DE");
    lang(&mut d, 4, "fr-FR");
    let mut rp = on();
    let mut t = 0.0;
    let sent = drain(&mut rp, &d, 1, &mut t);
    let language_of = |needle: &str| sent.iter().find(|r| r.text.contains(needle)).map(|r| r.language.clone());
    assert_eq!(language_of("Untagged").as_deref(), Some("auto"));
    assert_eq!(language_of("British").as_deref(), Some("auto"));
    assert_eq!(language_of("Kami").as_deref(), Some("auto"));
    assert_eq!(language_of("Guten").as_deref(), Some("de"));
    assert_eq!(language_of("Bonjour").as_deref(), Some("fr-FR"));
    assert_eq!(run_language(ProofLang::EnCa).as_deref(), Some("auto"));
    assert_eq!(run_language(ProofLang::Other), None);
    assert_eq!(run_language(ProofLang::from_tag(Some("sr-latn-rs"))).as_deref(), Some("sr-Latn-RS"));
}

#[test]
fn paragraph_language_is_the_one_with_the_most_checked_bytes() {
    // The longest run is English, but the German runs together hold more bytes.
    let text = "Guten Tag. Hello there everyone. Gute Nacht und gut.";
    let mut d = doc(&[text]);
    let hello = text.find("Hello").expect("Hello");
    let gute = text.find("Gute Nacht").expect("Gute");
    format(&mut d, 0, 0, hello, &|c| c.lang = Some("de".into()));
    format(&mut d, 0, gute, text.len(), &|c| c.lang = Some("de".into()));
    assert!(gute - hello > hello.max(text.len() - gute), "the English run is the longest");
    let mut rp = on();
    let mut t = 0.0;
    let sent = drain(&mut rp, &d, 1, &mut t);
    assert_eq!(sent.iter().map(|r| r.language.as_str()).collect::<Vec<_>>(), vec!["de"]);
}

#[test]
fn do_not_check_and_x_none_runs_are_not_sent() {
    let mut d = doc(&["Do not check this one.", "No language for this one.", "Check this one."]);
    let n = para(&d, 0).len();
    format(&mut d, 0, 0, n, &|c| c.no_proof = Some(true));
    lang(&mut d, 1, "x-none");
    let mut rp = on();
    let mut t = 0.0;
    let sent = drain(&mut rp, &d, 1, &mut t);
    assert_eq!(sent.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(), vec!["Check this one."]);
    assert_eq!(rp.poll(&d, 1, t + 100_000.0).wake_in_ms, None, "nothing waits");
}

#[test]
fn matches_map_to_paragraph_bytes_and_a_match_across_the_separator_is_dropped() {
    let mut rp = on();
    let d = doc(&["Alpha wrold.", "Beta tset."]);
    let r = first(&mut rp, &d);
    let across = m(cp(&r.text, "wrold"), cp(&r.text, "Beta") + 4 - cp(&r.text, "wrold"), RemoteKind::Grammar, "ACROSS");
    let matches = vec![on_word(&r, "wrold", RemoteKind::Spelling, "S1"), on_word(&r, "tset", RemoteKind::Grammar, "G1"), across];
    rp.receive(&r.id, ok(matches), IDLE_MS);
    let (p0, p1) = (&para(&d, 0).text, &para(&d, 1).text);
    let w = p0.find("wrold").expect("wrold");
    let s = p1.find("tset").expect("tset");
    assert_eq!(remote(&rp, &d, 0), vec![(w, w + 5, IssueKind::Spelling, "S1".to_string())]);
    assert_eq!(remote(&rp, &d, 1), vec![(s, s + 4, IssueKind::Grammar, "G1".to_string())]);
}

#[test]
fn untrusted_matches_outside_the_text_or_empty_are_dropped() {
    let mut rp = on();
    let d = doc(&["Alpha wrold."]);
    let r = first(&mut rp, &d);
    let n = r.text.chars().count();
    let matches = vec![
        m(0, 0, RemoteKind::Spelling, "EMPTY"),
        m(n, 1, RemoteKind::Spelling, "PAST_END"),
        m(usize::MAX, 2, RemoteKind::Spelling, "OVERFLOW"),
        m(2, usize::MAX, RemoteKind::Spelling, "OVERFLOW2"),
        RemoteMatch { message: "x".repeat(10_000), replacements: vec!["y".repeat(10_000); 50], ..on_word(&r, "wrold", RemoteKind::Spelling, "BIG") },
    ];
    let reply = Ok(ProofReply { matches, indonesian: vec![(usize::MAX, 5), (0, 0), (3, usize::MAX)], ..Default::default() });
    rp.receive(&r.id, reply, IDLE_MS);
    let o = rp.overlays(0).expect("overlays");
    let (_, o) = o.get(rev(&d, 0)).expect("overlay");
    assert_eq!(o.remote.len(), 1, "{:?}", o.remote);
    let i = &o.remote[0].issue;
    assert!(i.message.chars().count() < 10_000 && i.suggestions.len() < 50, "capped");
    assert!(o.quiet_spelling.is_empty(), "{:?}", o.quiet_spelling);
}

#[test]
fn offsets_past_an_emoji_land_on_the_right_bytes() {
    let mut rp = on();
    let d = doc(&["😀 Teh 👍🏽 cat wrold."]);
    let r = first(&mut rp, &d);
    rp.receive(&r.id, ok(vec![on_word(&r, "Teh", RemoteKind::Spelling, "A"), on_word(&r, "wrold", RemoteKind::Spelling, "B")]), IDLE_MS);
    let text = &para(&d, 0).text;
    let (t, w) = (text.find("Teh").expect("Teh"), text.find("wrold").expect("wrold"));
    let got = remote(&rp, &d, 0);
    assert_eq!(got.iter().map(|g| &text[g.0..g.1]).collect::<Vec<_>>(), vec!["Teh", "wrold"]);
    assert_eq!(got.iter().map(|g| g.0).collect::<Vec<_>>(), vec![t, w]);
}

#[test]
fn tracked_deletion_text_is_not_sent_and_offsets_skip_it() {
    let mut d = doc(&["The quick brown fox wrold."]);
    let q = para(&d, 0).text.find("quick ").expect("quick");
    format(&mut d, 0, q, q + 6, &|c| c.del = Some(1));
    let mut rp = on();
    let r = first(&mut rp, &d);
    assert_eq!(r.text, "The brown fox wrold.");
    // "The brown" in the request runs over the deleted text in the paragraph: dropped.
    let over = m(0, cp(&r.text, "brown") + 5, RemoteKind::Grammar, "OVER");
    rp.receive(&r.id, ok(vec![on_word(&r, "wrold", RemoteKind::Spelling, "W"), over]), IDLE_MS);
    let text = &para(&d, 0).text;
    let w = text.find("wrold").expect("wrold");
    assert_eq!(remote(&rp, &d, 0), vec![(w, w + 5, IssueKind::Spelling, "W".to_string())]);
}

#[test]
fn reply_for_a_changed_paragraph_is_not_applied_and_the_new_text_is_queued() {
    let mut rp = on();
    let mut d = doc(&["Teh cat sat."]);
    let r = first(&mut rp, &d);
    let old_rev = rev(&d, 0);
    edit(&mut d, 0, "Teh dog sat.");
    assert!(rp.poll(&d, 2, IDLE_MS + 100.0).send.is_empty(), "still in flight");
    rp.receive(&r.id, ok(vec![on_word(&r, "cat", RemoteKind::Spelling, "CAT")]), IDLE_MS + 200.0);
    assert!(remote(&rp, &d, 0).is_empty(), "the reply was for other text");
    assert!(rp.overlays(0).and_then(|o| o.get(old_rev).map(|_| ())).is_none(), "nor for the old revision");
    let mut t = IDLE_MS + 200.0;
    let sent = drain(&mut rp, &d, 2, &mut t);
    assert_eq!(sent.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(), vec!["Teh dog sat."]);
    // The result is kept under the text it was for: typing it again needs no request.
    edit(&mut d, 0, "Teh cat sat.");
    let mut t = t + 1000.0;
    assert!(drain(&mut rp, &d, 3, &mut t).is_empty());
    let c = para(&d, 0).text.find("cat").expect("cat");
    assert_eq!(remote(&rp, &d, 0), vec![(c, c + 3, IssueKind::Spelling, "CAT".to_string())]);
}

#[test]
fn indonesian_spans_become_quiet_spelling_for_their_paragraph() {
    let mut rp = on();
    let d = doc(&["Kami akan mengirimkan. Teh cat.", "Hello there.", "Saya pergi."]);
    let r = first(&mut rp, &d);
    let kami = (cp(&r.text, "Kami"), "Kami akan mengirimkan.".chars().count());
    // From "Saya" back across the separator into "there.": cut at the separator.
    let saya = cp(&r.text, "Saya");
    let across = (cp(&r.text, "there."), saya + 4 - cp(&r.text, "there."));
    let empty = (cp(&r.text, "Teh"), 0);
    rp.receive(&r.id, Ok(ProofReply { matches: Vec::new(), indonesian: vec![kami, across, empty], ..Default::default() }), IDLE_MS);
    let o = rp.overlays(0).expect("overlays");
    let quiet = |k: usize| o.get(rev(&d, k)).map(|(_, o)| o.quiet_spelling.clone()).unwrap_or_default();
    let p0 = &para(&d, 0).text;
    assert_eq!(quiet(0), vec![0..p0.find(" Teh").expect("Teh")]);
    let p1 = &para(&d, 1).text;
    assert_eq!(quiet(1), vec![p1.find("there.").expect("there")..p1.len()]);
    assert_eq!(quiet(2), vec![0..4]);
    assert!(o.get(rev(&d, 0)).is_some_and(|(_, o)| o.quiet_spelling.iter().all(|q| !q.is_empty())), "no empty range");
}

#[test]
fn a_paragraph_with_nothing_remote_gets_no_overlay() {
    let mut rp = on();
    let d = doc(&["Teh cat.", "Fine text."]);
    let r = first(&mut rp, &d);
    rp.receive(&r.id, ok(vec![on_word(&r, "Teh", RemoteKind::Spelling, "S")]), IDLE_MS);
    let o = rp.overlays(0).expect("overlays");
    assert!(o.get(rev(&d, 0)).is_some());
    assert!(o.get(rev(&d, 1)).is_none(), "no empty overlay");
    let mut clean = on();
    let d = doc(&["Fine text."]);
    let r = first(&mut clean, &d);
    clean.receive(&r.id, ok(Vec::new()), IDLE_MS);
    assert!(clean.overlays(0).is_none_or(|o| o.get(rev(&d, 0)).is_none()));
}

#[test]
fn busy_and_down_back_off_then_resend() {
    let d = doc(&["Teh cat sat."]);
    for (failure, wait) in [(Some(ProofFailure::Busy), BUSY_BACKOFF_MS), (Some(ProofFailure::Unavailable), DOWN_BACKOFF_MS), (None, DOWN_BACKOFF_MS)]
    {
        let mut rp = on();
        let r = first(&mut rp, &d);
        // A missing reply counts once REPLY_TIMEOUT_MS has passed.
        let failed_at = match failure {
            Some(f) => {
                rp.receive(&r.id, Err(f), IDLE_MS + 500.0);
                IDLE_MS + 500.0
            }
            None => {
                assert!(rp.poll(&d, 1, IDLE_MS + REPLY_TIMEOUT_MS - 1.0).send.is_empty(), "waiting for the reply");
                IDLE_MS + REPLY_TIMEOUT_MS
            }
        };
        let p = rp.poll(&d, 1, failed_at);
        assert!(p.send.is_empty(), "{failure:?}");
        assert!(p.wake_in_ms.is_some_and(|w| w > MIN_GAP_MS), "{failure:?}: {p:?}");
        assert!(rp.poll(&d, 1, failed_at + wait - 1.0).send.is_empty(), "{failure:?}: backing off");
        let again = one(rp.poll(&d, 1, failed_at + wait));
        assert_eq!(again.text, r.text, "{failure:?}: resent");
        // A late reply to the timed-out request is not taken for the new one.
        rp.receive(&r.id, ok(vec![on_word(&r, "Teh", RemoteKind::Spelling, "LATE")]), failed_at + wait + 1.0);
        if failure.is_none() {
            assert!(remote(&rp, &d, 0).is_empty());
        }
    }
}

#[test]
fn too_large_and_bad_request_mark_their_paragraphs_checked() {
    for f in [ProofFailure::TooLarge, ProofFailure::BadRequest] {
        let mut rp = on();
        let d = doc(&["Teh cat sat."]);
        let r = first(&mut rp, &d);
        rp.receive(&r.id, Err(f), IDLE_MS);
        let mut t = IDLE_MS;
        assert!(drain(&mut rp, &d, 1, &mut t).is_empty(), "{f:?}");
        assert!(remote(&rp, &d, 0).is_empty());
    }
    assert_eq!(ProofFailure::from_code("too_large"), ProofFailure::TooLarge);
    assert_eq!(ProofFailure::from_code("busy"), ProofFailure::Busy);
    assert_eq!(ProofFailure::from_code("disabled"), ProofFailure::Disabled);
    assert_eq!(ProofFailure::from_code("bad_request"), ProofFailure::BadRequest);
    assert_eq!(ProofFailure::from_code("what"), ProofFailure::Unavailable);
}

#[test]
fn disabled_reply_stops_all_requests() {
    let mut rp = on();
    let mut d = doc(&["Teh cat sat."]);
    let r = first(&mut rp, &d);
    rp.receive(&r.id, Err(ProofFailure::Disabled), IDLE_MS);
    assert!(!rp.enabled());
    // Underlines from before the service was turned off go with it.
    let mut was_on = on();
    let mut two = doc(&["Teh cat sat.", "Fine text."]);
    let r = first(&mut was_on, &two);
    was_on.receive(&r.id, ok(vec![on_word(&r, "Teh", RemoteKind::Spelling, "S")]), IDLE_MS);
    assert!(!remote(&was_on, &two, 0).is_empty());
    edit(&mut two, 1, "Fine text, edited.");
    let (at, r) = first_send(&mut was_on, &two, 2, IDLE_MS + 5000.0, IDLE_MS + 20_000.0).expect("the edit is sent");
    let g = was_on.generation();
    was_on.receive(&r.id, Err(ProofFailure::Disabled), at);
    assert!(was_on.generation() > g);
    assert!(remote(&was_on, &two, 0).is_empty(), "no remote underline once the service is off");
    edit(&mut d, 0, "Another text.");
    let p = rp.poll(&d, 2, 1_000_000.0);
    assert!(p.send.is_empty());
    assert_eq!(p.wake_in_ms, None);
    let mut off = RemoteProof::default();
    assert!(!off.enabled(), "off until the host turns it on");
    assert!(off.poll(&d, 1, 1_000_000.0).send.is_empty());
}

#[test]
fn large_document_sweep_is_paced_and_capped_but_edits_still_go() {
    // Each paragraph fills a batch on its own: 80 batches.
    let texts: Vec<String> = (0..80).map(|i| format!("{i} {}", "word ".repeat(1000))).collect();
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let mut d = doc(&refs);
    let mut rp = on();
    let mut times = Vec::new();
    let mut t = 0.0;
    while t < 400_000.0 {
        let p = rp.poll(&d, 1, t);
        for r in p.send {
            times.push(t);
            rp.receive(&r.id, ok(Vec::new()), t);
        }
        t += 250.0;
    }
    assert_eq!(times.len(), MAX_SWEEP_BATCHES as usize);
    assert!(times.windows(2).all(|w| w[1] - w[0] >= MIN_GAP_MS), "{times:?}");
    assert_eq!(rp.poll(&d, 1, t).wake_in_ms, None, "the rest of the sweep waits for nothing");
    edit(&mut d, 79, "An edit after the sweep.");
    let (_, r) = first_send(&mut rp, &d, 2, t, t + 10_000.0).expect("the edit is sent");
    assert_eq!(r.text, "An edit after the sweep.");
}

#[test]
fn cache_keeps_at_most_2000_entries() {
    let texts: Vec<String> = (0..CACHE_ENTRIES + 100).map(|i| format!("p{i}")).collect();
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let d = doc(&refs);
    let mut rp = on();
    let mut t = 0.0;
    let sent = drain(&mut rp, &d, 1, &mut t);
    assert!(sent.iter().map(|r| r.text.split("\n\n").count()).sum::<usize>() > CACHE_ENTRIES);
    assert!(rp.cache.len() <= CACHE_ENTRIES, "{}", rp.cache.len());
    assert!(drain(&mut rp, &d, 1, &mut t).is_empty(), "checked paragraphs are not sent again");
}

#[test]
fn ignores_reach_the_overlays_and_rules_stay_with_their_document() {
    let mut rp = on();
    let d = doc(&["Teh cat sat."]);
    let r = first(&mut rp, &d);
    rp.receive(&r.id, ok(vec![on_word(&r, "Teh", RemoteKind::Spelling, "S")]), IDLE_MS);
    let g = rp.generation();
    rp.ignore_occurrence(0, para(&d, 0), 0, 3, "S");
    rp.ignore_rule(0, "R1");
    rp.ignore_word(0, "Teh");
    assert!(rp.generation() > g);
    let o = rp.overlays(0).expect("overlays");
    assert!(o.get(rev(&d, 0)).is_some_and(|(_, o)| o.ignored.iter().any(|i| (i.start, i.end, i.rule.as_str()) == (0, 3, "S"))));
    assert!(o.ignored_rules().contains("R1"));
    assert!(o.words().contains("Teh"));
    let other = rp.overlays(7);
    assert!(other.is_none_or(|o| o.ignored_rules().is_empty() && !o.words().contains("Teh")), "another document's are its own");
}

#[test]
fn utf16_and_code_point_offsets_land_on_the_same_bytes() {
    let d = doc(&["😀 Teh 👍🏽 cat wrold."]);
    let text = para(&d, 0).text.clone();
    let mut got = Vec::new();
    for unit in [OffsetUnit::CodePoints, OffsetUnit::Utf16] {
        let mut rp = on();
        let r = first(&mut rp, &d);
        let at = |w: &str| if unit == OffsetUnit::Utf16 { u16_at(&r.text, w) } else { cp(&r.text, w) };
        let len = |w: &str| if unit == OffsetUnit::Utf16 { w.encode_utf16().count() } else { w.chars().count() };
        let matches = vec![
            m(at("Teh"), len("Teh"), RemoteKind::Spelling, "A"),
            m(at("wrold"), len("wrold"), RemoteKind::Spelling, "B"),
            // Covers "👍🏽 " whole in either unit.
            m(at("👍🏽"), len("👍🏽 "), RemoteKind::Grammar, "C"),
        ];
        rp.receive(&r.id, Ok(ProofReply { matches, indonesian: vec![(at("cat"), len("cat"))], unit }), IDLE_MS);
        let o = rp.overlays(0).expect("overlays");
        let (_, o) = o.get(rev(&d, 0)).expect("overlay");
        let mut found: Vec<(usize, usize)> = o.remote.iter().map(|r| (r.issue.start, r.issue.end)).collect();
        found.sort_unstable();
        let words: Vec<&str> = found.iter().map(|&(a, b)| &text[a..b]).collect();
        assert_eq!(words, vec!["Teh", "👍🏽 ", "wrold"], "{unit:?}");
        assert_eq!(o.quiet_spelling.iter().map(|q| &text[q.clone()]).collect::<Vec<_>>(), vec!["cat"], "{unit:?}");
        got.push(found);
    }
    assert_eq!(got[0], got[1]);
}

#[test]
fn a_utf16_offset_inside_a_surrogate_pair_or_past_the_end_is_dropped() {
    let d = doc(&["😀 Teh."]);
    let mut rp = on();
    let r = first(&mut rp, &d);
    let n = r.text.encode_utf16().count();
    let matches = vec![
        m(1, 3, RemoteKind::Spelling, "MID_PAIR_START"),
        m(0, 1, RemoteKind::Spelling, "MID_PAIR_END"),
        m(n, 1, RemoteKind::Spelling, "PAST_END"),
        m(n - 1, usize::MAX, RemoteKind::Spelling, "OVERFLOW"),
        m(u16_at(&r.text, "Teh"), 3, RemoteKind::Spelling, "OK"),
    ];
    let reply = Ok(ProofReply { matches, indonesian: vec![(1, 2), (n, 4)], unit: OffsetUnit::Utf16 });
    rp.receive(&r.id, reply, IDLE_MS);
    let o = rp.overlays(0).expect("overlays");
    let (_, o) = o.get(rev(&d, 0)).expect("overlay");
    assert_eq!(o.remote.iter().map(|r| r.rule.as_str()).collect::<Vec<_>>(), vec!["OK"]);
    assert!(o.quiet_spelling.is_empty(), "{:?}", o.quiet_spelling);
}

#[test]
fn combining_marks_are_code_points_in_both_units() {
    let d = doc(&["Cafe\u{301} au lait wrold."]);
    let text = para(&d, 0).text.clone();
    for unit in [OffsetUnit::CodePoints, OffsetUnit::Utf16] {
        let mut rp = on();
        let r = first(&mut rp, &d);
        let (at, len) = if unit == OffsetUnit::Utf16 { (u16_at(&r.text, "wrold"), 5) } else { (cp(&r.text, "wrold"), 5) };
        let accent = if unit == OffsetUnit::Utf16 { u16_at(&r.text, "Cafe") } else { cp(&r.text, "Cafe") };
        let matches = vec![m(at, len, RemoteKind::Spelling, "W"), m(accent, 5, RemoteKind::Grammar, "ACCENT")];
        rp.receive(&r.id, Ok(ProofReply { matches, indonesian: Vec::new(), unit }), IDLE_MS);
        let o = rp.overlays(0).expect("overlays");
        let (_, o) = o.get(rev(&d, 0)).expect("overlay");
        let mut found: Vec<(usize, usize)> = o.remote.iter().map(|r| (r.issue.start, r.issue.end)).collect();
        found.sort_unstable();
        let words: Vec<&str> = found.iter().map(|&(a, b)| &text[a..b]).collect();
        assert_eq!(words, vec!["Cafe\u{301}", "wrold"], "{unit:?}");
    }
}

#[test]
fn a_document_switch_keeps_each_document_s_results() {
    let mut rp = on();
    let a = doc(&["Teh cat sat."]);
    let r = first(&mut rp, &a);
    rp.receive(&r.id, ok(vec![on_word(&r, "Teh", RemoteKind::Spelling, "S")]), IDLE_MS);
    assert!(rp.set_document(2));
    let b = doc(&["Another document."]);
    let mut t = 10_000.0;
    drain(&mut rp, &b, 1, &mut t);
    assert!(rp.set_document(0));
    // Back in A, before any poll: its underline is there at once.
    assert_eq!(remote(&rp, &a, 0), vec![(0, 3, IssueKind::Spelling, "S".to_string())]);
}

#[test]
fn a_too_large_batch_is_retried_one_paragraph_at_a_time() {
    let mut rp = on();
    let d = doc(&["First paragraph here.", "Second paragraph here."]);
    let r = first(&mut rp, &d);
    assert!(r.text.contains("First") && r.text.contains("Second"));
    rp.receive(&r.id, Err(ProofFailure::TooLarge), IDLE_MS);
    let mut t = IDLE_MS;
    let mut sent = Vec::new();
    while let Some((at, r)) = first_send(&mut rp, &d, 1, t, t + 200_000.0) {
        let matches = if r.text.starts_with("Second") { vec![on_word(&r, "paragraph", RemoteKind::Grammar, "G")] } else { Vec::new() };
        rp.receive(&r.id, ok(matches), at);
        sent.push(r.text);
        t = at;
    }
    assert_eq!(sent, vec!["First paragraph here.", "Second paragraph here."], "each on its own");
    assert_eq!(remote(&rp, &d, 1).len(), 1, "the retried paragraph is checked");
}

#[test]
fn ignored_occurrences_reach_paragraphs_never_checked() {
    // The desktop app never polls: an ignore still reaches the paragraph through its text.
    let rp = {
        let mut rp = RemoteProof::default();
        let d = doc(&["Teh cat sat."]);
        rp.ignore_occurrence(5, para(&d, 0), 0, 3, "builtin:spelling");
        rp
    };
    let d = doc(&["Unrelated.", "Teh cat sat."]);
    let o = rp.overlays_in(5, &d).expect("overlays");
    assert!(o.get(rev(&d, 1)).is_some_and(|(_, o)| o.ignored.len() == 1));
    assert!(o.get(rev(&d, 0)).is_none());
    assert!(rp.overlays_in(6, &d).is_none(), "not in another document");
}

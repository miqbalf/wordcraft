//! The remote proof source: the body paragraphs whose text changed are sent, once the typing
//! stops, to the host's grammar route in batches, and the matches that come back are laid over
//! the built-in checks as [`ProofOverlays`].
//!
//! - Nothing is sent before [`IDLE_MS`] without a change, at most one request is in flight and
//!   requests are at least [`MIN_GAP_MS`] apart.
//! - A result is cached by the hash of the language and the text sent, so the same text is
//!   never sent twice (an undo back to checked text gets its result again at once).
//! - A reply touches only paragraphs whose text still hashes as it did when it was sent.
//! - The host's matches are not trusted: any that do not fit the text sent are dropped.
//! - The viewer's ignores are kept here, per document.
//!
//! The host (the web page) does the sending: [`RemoteProof::poll`] says what to send and when to
//! poll again, [`RemoteProof::receive`] takes the answer.

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::sync::Arc;

use wordcraft_doc::{Document, Paragraph, StoryRef};
use wordcraft_layout::para::{IgnoredIssue, ProofOverlay, ProofOverlays, ProofRun, RemoteIssue, proof_runs, proof_text};
use wordcraft_proof::{Issue, IssueKind, ProofLang};

/// Idle time after the last change before anything is sent.
pub const IDLE_MS: f64 = 1500.0;
/// Least time between two requests.
pub const MIN_GAP_MS: f64 = 2000.0;
/// Most characters in one request.
pub const MAX_BATCH_CHARS: usize = 8000;
/// Most UTF-8 bytes in one request (the route's cap).
pub const MAX_BATCH_BYTES: usize = 20 * 1024;
/// A request with no answer after this long counts as failed ([`ProofFailure::Unavailable`]).
pub const REPLY_TIMEOUT_MS: f64 = 45_000.0;
/// Wait after [`ProofFailure::Busy`].
pub const BUSY_BACKOFF_MS: f64 = 30_000.0;
/// Wait after [`ProofFailure::Unavailable`] or a missing answer.
pub const DOWN_BACKOFF_MS: f64 = 60_000.0;
/// Most results kept in the cache (least recently used go first).
pub const CACHE_ENTRIES: usize = 2000;
/// Most batches the check of the paragraphs already there when a document opens may send.
pub const MAX_SWEEP_BATCHES: u32 = 60;

/// The batch separator: a blank line.
const SEPARATOR: &str = "\n\n";
/// What a reply may hold (the route's limits): the rest is left out.
const MAX_MATCHES: usize = 500;
const MAX_MESSAGE_CHARS: usize = 500;
const MAX_REPLACEMENTS: usize = 5;
const MAX_REPLACEMENT_CHARS: usize = 200;
const MAX_RULE_CHARS: usize = 200;
const MAX_SPANS: usize = 2000;

/// One request for the host: `{id, language, text}`.
#[derive(Clone, Debug, PartialEq)]
pub struct ProofRequest {
    pub id: String,
    pub language: String,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteKind {
    Spelling,
    Grammar,
}

/// One match from the host. `offset` and `length` count code points of the request's text.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteMatch {
    pub offset: usize,
    pub length: usize,
    pub kind: RemoteKind,
    pub message: String,
    pub replacements: Vec<String>,
    pub rule: String,
}

/// What a reply's offsets and lengths count in the request's text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OffsetUnit {
    /// Unicode code points (the route's contract).
    #[default]
    CodePoints,
    /// UTF-16 code units (Java and JavaScript string indices).
    Utf16,
}

/// The host's answer: the matches, and the sentences (offset and length) it routed to
/// Indonesian, both counted in `unit`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProofReply {
    pub matches: Vec<RemoteMatch>,
    pub indonesian: Vec<(usize, usize)>,
    pub unit: OffsetUnit,
}

/// Why a request failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProofFailure {
    Busy,
    Unavailable,
    TooLarge,
    Disabled,
    BadRequest,
}

impl ProofFailure {
    /// The failure a host error code names; an unknown code is [`ProofFailure::Unavailable`].
    pub fn from_code(code: &str) -> Self {
        match code {
            "busy" => ProofFailure::Busy,
            "too_large" => ProofFailure::TooLarge,
            "disabled" => ProofFailure::Disabled,
            "bad_request" => ProofFailure::BadRequest,
            _ => ProofFailure::Unavailable,
        }
    }
}

/// What a poll asks of the host: the requests to send now, and how long until polling again
/// can send something (`None`: nothing waits).
#[derive(Debug, Default)]
pub struct Poll {
    pub send: Vec<ProofRequest>,
    pub wake_in_ms: Option<f64>,
}

/// The `language` sent for a run's proofing language (Ruling R5): English and Indonesian are
/// `auto` (the service tells them apart by sentence), a language without proofing tools is not
/// sent, and any other is its tag in BCP 47 case (`fr-fr` → `fr-FR`).
pub fn run_language(lang: ProofLang) -> Option<String> {
    match lang {
        ProofLang::En | ProofLang::EnGb | ProofLang::EnAu | ProofLang::EnCa | ProofLang::Id => Some("auto".to_string()),
        ProofLang::Other => None,
        other => Some(bcp47(&other.tag())).filter(|t| !t.is_empty()),
    }
}

/// The language a text made of `runs` is sent in: the one with the most checked bytes, as
/// [`run_language`] writes it; `None` when no checked run has one.
pub fn text_language(runs: &[ProofRun]) -> Option<String> {
    let mut bytes: Vec<(String, usize)> = Vec::new();
    for r in runs.iter().filter(|r| !r.skip && !r.range.is_empty()) {
        let Some(l) = run_language(r.lang) else { continue };
        match bytes.iter_mut().find(|(k, _)| *k == l) {
            Some((_, n)) => *n += r.range.len(),
            None => bytes.push((l, r.range.len())),
        }
    }
    let mut best: Option<(String, usize)> = None;
    for (l, n) in bytes {
        if best.as_ref().is_none_or(|(_, b)| n > *b) {
            best = Some((l, n));
        }
    }
    best.map(|(l, _)| l)
}

/// A lowercase locale (`fr-fr`, `sr-latn`) in BCP 47 case (`fr-FR`, `sr-Latn`).
fn bcp47(tag: &str) -> String {
    tag.split('-')
        .enumerate()
        .map(|(i, sub)| {
            let alpha = sub.chars().all(|c| c.is_ascii_alphabetic());
            if i > 0 && alpha && sub.len() == 2 {
                sub.to_ascii_uppercase()
            } else if i > 0 && alpha && sub.len() == 4 {
                let mut c = sub.chars();
                c.next().map(|f| f.to_ascii_uppercase().to_string() + &c.as_str().to_ascii_lowercase()).unwrap_or_default()
            } else {
                sub.to_ascii_lowercase()
            }
        })
        .collect::<Vec<_>>()
        .join("-")
}

/// A match in the text of one paragraph as sent, in code points.
#[derive(Clone, Debug)]
struct Found {
    start: usize,
    end: usize,
    kind: RemoteKind,
    message: String,
    replacements: Vec<String>,
    rule: String,
}

/// The result for one text as sent (code points), as cached.
#[derive(Debug, Default)]
struct Checked {
    found: Vec<Found>,
    quiet: Vec<Range<usize>>,
}

/// A paragraph's result placed on its bytes.
#[derive(Debug, Default)]
struct Placed {
    remote: Vec<RemoteIssue>,
    quiet: Vec<Range<usize>>,
}

/// What is sent for a paragraph that waits for its check.
#[derive(Debug)]
struct Pending {
    language: String,
    text: String,
    /// Each sent character's start byte and length in the paragraph.
    chars: Vec<(usize, usize)>,
    /// The runs' byte ranges and languages, for each issue's language.
    langs: Vec<(Range<usize>, ProofLang)>,
}

/// A body paragraph revision as the last scan saw it.
#[derive(Debug)]
struct Para {
    rev: u64,
    /// The hash of its language and text as sent; `None` when nothing is sent.
    hash: Option<u64>,
    /// The hash of its text, for ignored occurrences.
    text_key: u64,
    /// Appeared after the document's first scan: sent before the sweep.
    edited: bool,
    /// Waiting for its check (`None` once checked or when nothing is sent).
    pending: Option<Box<Pending>>,
}

/// One paragraph text in the request in flight.
#[derive(Debug)]
struct Slice {
    hash: u64,
    /// Its first code point in the request.
    start: usize,
    /// Its length in code points.
    len: usize,
}

#[derive(Debug)]
struct InFlight {
    id: String,
    sent_at: f64,
    slices: Vec<Slice>,
    chars: usize,
    /// The UTF-16 offset of each character outside the Basic Multilingual Plane, in order: what
    /// turns a UTF-16 offset into a code point.
    astral: Vec<usize>,
    sweep: bool,
}

/// What is kept for a document while another is followed, so switching back shows its results
/// at once and its sweep cap goes on where it stopped.
#[derive(Debug, Default)]
struct Followed {
    scanned: Option<u64>,
    paras: Vec<Para>,
    placed: HashMap<u64, Placed>,
    sweep_batches: u32,
}

/// The viewer's ignores in one document (decision 5).
#[derive(Debug, Default)]
struct Ignores {
    rules: BTreeSet<String>,
    words: BTreeSet<String>,
    /// By the paragraph's text hash.
    occurrences: HashMap<u64, Vec<IgnoredIssue>>,
}

/// The remote proof source of a session: see the module documentation. It follows one document
/// at a time ([`RemoteProof::set_document`]); results are cached by text for all of them.
#[derive(Default)]
pub struct RemoteProof {
    enabled: bool,
    document: u64,
    /// The document revision the paragraphs were last scanned at.
    scanned: Option<u64>,
    /// The body paragraphs in document order, as last scanned.
    paras: Vec<Para>,
    /// Results placed on live paragraph revisions (only those with something to show).
    placed: HashMap<u64, Placed>,
    in_flight: Option<InFlight>,
    /// The other documents followed before, by id.
    parked: HashMap<u64, Followed>,
    /// Texts a batch was too large with: they are sent on their own.
    solo: HashSet<u64>,
    last_change: f64,
    last_sent: Option<f64>,
    backoff_until: f64,
    sweep_batches: u32,
    next_id: u64,
    /// Results by text hash, with the tick they were last used at.
    cache: HashMap<u64, (u64, Arc<Checked>)>,
    /// The cache's hashes by the tick they were last used at, oldest first.
    order: BTreeMap<u64, u64>,
    tick: u64,
    ignores: HashMap<u64, Ignores>,
    /// The user dictionary's words, read when its generation changes.
    dictionary: (Option<u64>, BTreeSet<String>),
    generation: u64,
    /// The generation [`RemoteProof::take_change`] last reported.
    taken: u64,
}

impl RemoteProof {
    /// Turns remote checks on (the host has a proof route) or off.
    /// Turning it off drops the remote results shown (they come back from the cache when it
    /// is turned on again); the ignores stay.
    pub fn set_enabled(&mut self, on: bool) {
        if !on && self.enabled {
            self.in_flight = None;
            self.scanned = None;
            self.paras.clear();
            self.placed.clear();
            self.parked.clear();
            self.generation += 1;
        }
        if on && !self.enabled {
            self.scanned = None;
        }
        self.enabled = on;
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Follows document `document` from now on: whether that is another document than before.
    /// The document left is parked with its results; one followed before comes back with its
    /// own, and with any result that came for it meanwhile.
    pub fn set_document(&mut self, document: u64) -> bool {
        if document == self.document {
            return false;
        }
        let left = Followed {
            scanned: self.scanned.take(),
            paras: std::mem::take(&mut self.paras),
            placed: std::mem::take(&mut self.placed),
            sweep_batches: self.sweep_batches,
        };
        self.parked.insert(self.document, left);
        let back = self.parked.remove(&document).unwrap_or_default();
        self.document = document;
        self.scanned = back.scanned;
        self.paras = back.paras;
        self.placed = back.placed;
        self.sweep_batches = back.sweep_batches;
        let waiting: HashSet<u64> =
            self.paras.iter().filter(|p| p.pending.is_some()).filter_map(|p| p.hash).filter(|h| self.cache.contains_key(h)).collect();
        if !waiting.is_empty() {
            self.apply_cached(waiting);
        }
        self.generation += 1;
        true
    }

    /// Drops what is kept for document `document`, closed or replaced: its parked results and
    /// its ignores. The document followed is kept.
    pub fn forget(&mut self, document: u64) {
        if document != self.document {
            self.parked.remove(&document);
            self.ignores.remove(&document);
        }
    }

    /// What to send at `now_ms` for document `doc` at revision `doc_rev`.
    pub fn poll(&mut self, doc: &Document, doc_rev: u64, now_ms: f64) -> Poll {
        if !self.enabled {
            return Poll::default();
        }
        self.read_dictionary();
        if self.scanned != Some(doc_rev) {
            self.scan(doc, now_ms);
            self.scanned = Some(doc_rev);
        }
        if let Some(f) = &self.in_flight
            && now_ms - f.sent_at >= REPLY_TIMEOUT_MS
        {
            let id = f.id.clone();
            self.receive(&id, Err(ProofFailure::Unavailable), now_ms);
        }
        let mut send = Vec::new();
        if self.in_flight.is_none()
            && now_ms >= self.ready_at()
            && let Some(r) = self.batch(now_ms)
        {
            send.push(r);
        }
        Poll { send, wake_in_ms: self.wake(now_ms) }
    }

    /// The host's answer to request `id`. An answer to any other request than the one in flight
    /// is ignored.
    pub fn receive(&mut self, id: &str, reply: Result<ProofReply, ProofFailure>, now_ms: f64) {
        if self.in_flight.as_ref().is_none_or(|f| f.id != id) {
            return;
        }
        let Some(f) = self.in_flight.take() else { return };
        match reply {
            Ok(reply) => {
                for s in &f.slices {
                    self.solo.remove(&s.hash);
                }
                let results = split_reply(&f, reply);
                for (s, checked) in f.slices.iter().zip(results) {
                    self.cache_put(s.hash, Arc::new(checked));
                }
                self.apply_cached(f.slices.iter().map(|s| s.hash).collect());
            }
            Err(failure) => {
                if f.sweep {
                    self.sweep_batches = self.sweep_batches.saturating_sub(1);
                }
                match failure {
                    ProofFailure::Busy => self.backoff_until = now_ms + BUSY_BACKOFF_MS,
                    ProofFailure::Unavailable => self.backoff_until = now_ms + DOWN_BACKOFF_MS,
                    ProofFailure::TooLarge if f.slices.len() > 1 => {
                        // Sent again one by one; only a text too large on its own is given up.
                        self.solo.extend(f.slices.iter().map(|s| s.hash));
                    }
                    ProofFailure::TooLarge | ProofFailure::BadRequest => {
                        for s in &f.slices {
                            self.cache_put(s.hash, Arc::new(Checked::default()));
                        }
                        self.apply_cached(f.slices.iter().map(|s| s.hash).collect());
                    }
                    ProofFailure::Disabled => self.set_enabled(false),
                }
            }
        }
    }

    /// The overlays of document `document`: the remote results of its live paragraphs (when it
    /// is the document followed), the ignored occurrences of the paragraphs scanned, its ignored
    /// rules, and its ignored words with the user dictionary's. `None` when there is nothing at
    /// all.
    pub fn overlays(&self, document: u64) -> Option<Arc<ProofOverlays>> {
        let mut by_rev = self.placed_overlays(document);
        if let Some(occurrences) = self.occurrences(document)
            && document == self.document
        {
            for p in &self.paras {
                if let Some(list) = occurrences.get(&p.text_key) {
                    by_rev.entry(p.rev).or_default().ignored = list.clone();
                }
            }
        }
        self.finish(document, by_rev)
    }

    /// [`RemoteProof::overlays`] with the ignored occurrences found in every paragraph of `doc`
    /// (every story), checked or not: what a session without remote checks (the desktop app)
    /// needs for Ignore on the built-in issues.
    pub fn overlays_in(&self, document: u64, doc: &Document) -> Option<Arc<ProofOverlays>> {
        let mut by_rev = self.placed_overlays(document);
        if let Some(occurrences) = self.occurrences(document) {
            for story in std::iter::once(StoryRef::Body).chain(doc.parts.keys().map(|k| StoryRef::Part(*k))) {
                for path in doc.para_paths(story) {
                    if let Some(p) = doc.para(story, &path)
                        && let Some(list) = occurrences.get(&text_key(p))
                    {
                        by_rev.entry(p.rev).or_default().ignored = list.clone();
                    }
                }
            }
        }
        self.finish(document, by_rev)
    }

    /// Whether document `document` has an issue ignored by its text ([`RemoteProof::ignore_occurrence`]).
    pub(crate) fn has_ignored_occurrences(&self, document: u64) -> bool {
        self.occurrences(document).is_some()
    }

    fn occurrences(&self, document: u64) -> Option<&HashMap<u64, Vec<IgnoredIssue>>> {
        self.ignores.get(&document).map(|i| &i.occurrences).filter(|o| !o.is_empty())
    }

    /// The remote results placed on the paragraphs of `document`, if it is the one followed.
    fn placed_overlays(&self, document: u64) -> HashMap<u64, ProofOverlay> {
        if document != self.document {
            return HashMap::new();
        }
        self.placed
            .iter()
            .map(|(rev, x)| (*rev, ProofOverlay { remote: x.remote.clone(), quiet_spelling: x.quiet.clone(), ignored: Vec::new() }))
            .collect()
    }

    fn finish(&self, document: u64, by_rev: HashMap<u64, ProofOverlay>) -> Option<Arc<ProofOverlays>> {
        let by_rev: HashMap<u64, Arc<ProofOverlay>> = by_rev
            .into_iter()
            .filter(|(_, o)| !(o.remote.is_empty() && o.quiet_spelling.is_empty() && o.ignored.is_empty()))
            .map(|(rev, o)| (rev, Arc::new(o)))
            .collect();
        let ignores = self.ignores.get(&document);
        let rules = ignores.map(|i| i.rules.clone()).unwrap_or_default();
        let mut words = ignores.map(|i| i.words.clone()).unwrap_or_default();
        words.extend(self.dictionary.1.iter().cloned());
        if by_rev.is_empty() && rules.is_empty() && words.is_empty() {
            return None;
        }
        Some(Arc::new(ProofOverlays::new(by_rev, rules, words)))
    }

    /// A number that changes whenever [`RemoteProof::overlays`] may give something new.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether [`RemoteProof::overlays`] may have changed since the last call: what the session
    /// asks before it rebuilds its overlays.
    pub fn take_change(&mut self) -> bool {
        let changed = self.taken != self.generation;
        self.taken = self.generation;
        changed
    }

    /// Ignore the issue at bytes `start..end` with rule `rule` in paragraph `p` of document
    /// `document`, and wherever the same text comes back there. The key is the paragraph's text
    /// hash, so paragraphs with identical text share one ignore.
    pub fn ignore_occurrence(&mut self, document: u64, p: &Paragraph, start: usize, end: usize, rule: &str) {
        let list = self.ignores.entry(document).or_default().occurrences.entry(text_key(p)).or_default();
        let g = IgnoredIssue { start, end, rule: rule.to_string() };
        if !list.contains(&g) {
            list.push(g);
        }
        self.generation += 1;
    }

    /// Ignore rule `rule` in document `document`.
    pub fn ignore_rule(&mut self, document: u64, rule: &str) {
        self.ignores.entry(document).or_default().rules.insert(rule.to_string());
        self.generation += 1;
    }

    /// Hide remote spelling issues on `word` in document `document`.
    pub fn ignore_word(&mut self, document: u64, word: &str) {
        self.ignores.entry(document).or_default().words.insert(word.to_string());
        self.generation += 1;
    }

    pub(crate) fn read_dictionary(&mut self) {
        let g = wordcraft_proof::dictionary_generation();
        if self.dictionary.0 != Some(g) {
            let words: BTreeSet<String> = wordcraft_proof::user_dictionary().into_iter().collect();
            if words != self.dictionary.1 {
                self.generation += 1;
            }
            self.dictionary = (Some(g), words);
        }
    }

    /// Looks at the body paragraphs: a new revision gets a cached result if there is one, or
    /// waits for its check.
    fn scan(&mut self, doc: &Document, now_ms: f64) {
        let first = self.scanned.is_none() && self.paras.is_empty();
        let mut old: HashMap<u64, Para> = std::mem::take(&mut self.paras).into_iter().map(|p| (p.rev, p)).collect();
        let mut new_hashes = Vec::new();
        let mut changed = false;
        for path in doc.para_paths(StoryRef::Body) {
            let Some(p) = doc.para(StoryRef::Body, &path) else { continue };
            if let Some(known) = old.remove(&p.rev) {
                self.paras.push(known);
                continue;
            }
            changed = true;
            let para = describe(doc, p, !first);
            if self.ignores.get(&self.document).is_some_and(|i| i.occurrences.contains_key(&para.text_key)) {
                self.generation += 1;
            }
            if let (Some(h), true) = (para.hash, para.pending.is_some()) {
                new_hashes.push(h);
            }
            self.paras.push(para);
        }
        if !old.is_empty() {
            changed = true;
            for rev in old.keys() {
                if self.placed.remove(rev).is_some() {
                    self.generation += 1;
                }
            }
        }
        if changed {
            self.last_change = now_ms;
        }
        let cached: HashSet<u64> = new_hashes.into_iter().filter(|h| self.cache.contains_key(h)).collect();
        if !cached.is_empty() {
            self.apply_cached(cached);
        }
    }

    /// Gives each waiting paragraph whose hash is in `hashes` its cached result.
    fn apply_cached(&mut self, hashes: HashSet<u64>) {
        let mut any = false;
        for i in 0..self.paras.len() {
            let Some(h) = self.paras.get(i).and_then(|p| p.hash).filter(|h| hashes.contains(h)) else { continue };
            let Some(checked) = self.cache_get(h) else { continue };
            let Some(p) = self.paras.get_mut(i) else { continue };
            let Some(pending) = p.pending.take() else { continue };
            let placed = place(&checked, &pending);
            if !(placed.remote.is_empty() && placed.quiet.is_empty()) {
                self.placed.insert(p.rev, placed);
                any = true;
            }
        }
        if any {
            self.generation += 1;
        }
    }

    fn cache_get(&mut self, hash: u64) -> Option<Arc<Checked>> {
        self.tick += 1;
        let (t, c) = self.cache.get_mut(&hash)?;
        self.order.remove(t);
        *t = self.tick;
        self.order.insert(self.tick, hash);
        Some(c.clone())
    }

    fn cache_put(&mut self, hash: u64, checked: Arc<Checked>) {
        self.tick += 1;
        if let Some((old, _)) = self.cache.insert(hash, (self.tick, checked)) {
            self.order.remove(&old);
        }
        self.order.insert(self.tick, hash);
        while self.cache.len() > CACHE_ENTRIES {
            let Some((_, oldest)) = self.order.pop_first() else { break };
            self.cache.remove(&oldest);
        }
    }

    /// When the next request may go: idle, gap and backoff all past.
    fn ready_at(&self) -> f64 {
        let gap = self.last_sent.map_or(f64::NEG_INFINITY, |t| t + MIN_GAP_MS);
        (self.last_change + IDLE_MS).max(gap).max(self.backoff_until)
    }

    /// The paragraphs that may go next, edited ones first, each in document order.
    fn candidates(&self) -> Vec<usize> {
        let sweep_open = self.sweep_batches < MAX_SWEEP_BATCHES;
        let flying: HashSet<u64> = self.in_flight.iter().flat_map(|f| f.slices.iter().map(|s| s.hash)).collect();
        let waiting = |p: &Para| p.pending.is_some() && p.hash.is_some_and(|h| !flying.contains(&h));
        let edited = self.paras.iter().enumerate().filter(|(_, p)| p.edited && waiting(p)).map(|(i, _)| i);
        let sweep = self.paras.iter().enumerate().filter(|(_, p)| sweep_open && !p.edited && waiting(p)).map(|(i, _)| i);
        edited.chain(sweep).collect()
    }

    /// The next request, if any paragraph waits: one language, within the size limits. A
    /// paragraph over a limit on its own is marked checked with nothing found.
    fn batch(&mut self, now_ms: f64) -> Option<ProofRequest> {
        let mut language: Option<String> = None;
        let mut text = String::new();
        let mut chars = 0usize;
        let mut slices: Vec<Slice> = Vec::new();
        let mut sweep = false;
        let mut too_large = HashSet::new();
        let mut astral: Vec<usize> = Vec::new();
        for i in self.candidates() {
            let Some(p) = self.paras.get(i) else { continue };
            let (Some(hash), Some(pending)) = (p.hash, p.pending.as_deref()) else { continue };
            let n = pending.chars.len();
            if n > MAX_BATCH_CHARS || pending.text.len() > MAX_BATCH_BYTES {
                too_large.insert(hash);
                continue;
            }
            if language.as_ref().is_some_and(|l| *l != pending.language) || slices.iter().any(|s| s.hash == hash) {
                continue;
            }
            let alone = self.solo.contains(&hash);
            if alone && !slices.is_empty() {
                continue;
            }
            let sep = if slices.is_empty() { 0 } else { SEPARATOR.len() };
            if chars + sep + n > MAX_BATCH_CHARS || text.len() + sep + pending.text.len() > MAX_BATCH_BYTES {
                continue;
            }
            if sep > 0 {
                text.push_str(SEPARATOR);
                chars += sep;
            }
            slices.push(Slice { hash, start: chars, len: n });
            for (k, c) in pending.text.chars().enumerate() {
                if c.len_utf16() == 2 {
                    astral.push(chars + k + astral.len());
                }
            }
            text.push_str(&pending.text);
            chars += n;
            sweep |= !p.edited;
            language.get_or_insert_with(|| pending.language.clone());
            if alone {
                break;
            }
        }
        for h in &too_large {
            self.cache_put(*h, Arc::new(Checked::default()));
        }
        if !too_large.is_empty() {
            self.apply_cached(too_large);
        }
        let language = language?;
        self.next_id += 1;
        let id = format!("proof-{}", self.next_id);
        if sweep {
            self.sweep_batches += 1;
        }
        self.last_sent = Some(now_ms);
        self.in_flight = Some(InFlight { id: id.clone(), sent_at: now_ms, slices, chars, astral, sweep });
        Some(ProofRequest { id, language, text })
    }

    /// How long until a poll can send something, while anything waits.
    fn wake(&self, now_ms: f64) -> Option<f64> {
        if let Some(f) = &self.in_flight {
            return Some((f.sent_at + REPLY_TIMEOUT_MS - now_ms).max(0.0));
        }
        if self.candidates().is_empty() {
            return None;
        }
        Some((self.ready_at() - now_ms).max(0.0))
    }
}

fn hash_of(value: &impl Hash) -> u64 {
    let mut h = DefaultHasher::new();
    value.hash(&mut h);
    h.finish()
}

/// The key ignored occurrences are kept under: the paragraph's text hash.
fn text_key(p: &Paragraph) -> u64 {
    hash_of(&p.text)
}

/// A paragraph revision as sent: its language, and its text less tracked deletions and objects.
fn describe(doc: &Document, p: &Paragraph, edited: bool) -> Para {
    let runs = proof_runs(doc, p, None);
    let language = text_language(&runs);
    let mut text = String::new();
    let mut chars = Vec::new();
    for (i, c) in proof_text(p).char_indices().filter(|(_, c)| *c != '\u{1}') {
        text.push(c);
        chars.push((i, c.len_utf8()));
    }
    let pending = language.filter(|_| !text.trim().is_empty()).map(|language| {
        let langs = runs.iter().map(|r| (r.range.clone(), r.lang)).collect();
        Box::new(Pending { language, text, chars, langs })
    });
    let hash = pending.as_ref().map(|q| hash_of(&(&q.language, &q.text)));
    Para { rev: p.rev, hash, text_key: text_key(p), edited, pending }
}

/// Cuts a reply into one result per slice, in the slice's own code points. Matches and spans
/// that do not fit the request are dropped; a match must lie wholly inside one slice.
fn split_reply(f: &InFlight, reply: ProofReply) -> Vec<Checked> {
    let mut out: Vec<Checked> = f.slices.iter().map(|_| Checked::default()).collect();
    let unit = reply.unit;
    for m in reply.matches.into_iter().take(MAX_MATCHES) {
        let Some((offset, end)) = code_points(f, unit, m.offset, m.length) else { continue };
        let m = RemoteMatch { offset, ..m };
        let Some(k) = f.slices.iter().position(|s| s.start <= m.offset && end <= s.start + s.len) else { continue };
        let (Some(s), Some(c)) = (f.slices.get(k), out.get_mut(k)) else { continue };
        c.found.push(Found {
            start: m.offset - s.start,
            end: end - s.start,
            kind: m.kind,
            message: m.message.chars().take(MAX_MESSAGE_CHARS).collect(),
            replacements: m.replacements.into_iter().filter(|r| r.chars().count() <= MAX_REPLACEMENT_CHARS).take(MAX_REPLACEMENTS).collect(),
            rule: m.rule.chars().take(MAX_RULE_CHARS).collect(),
        });
    }
    for (offset, length) in reply.indonesian.into_iter().take(MAX_SPANS) {
        let Some((offset, end)) = code_points(f, unit, offset, length) else { continue };
        for (s, c) in f.slices.iter().zip(out.iter_mut()) {
            let (a, b) = (offset.max(s.start), end.min(s.start + s.len));
            if a < b {
                c.quiet.push(a - s.start..b - s.start);
            }
        }
    }
    out
}

/// A reply's `offset` and `length` in `unit` as a code-point range of the request's text;
/// `None` when it is empty, runs past the end or (in UTF-16) starts or ends inside a surrogate
/// pair.
fn code_points(f: &InFlight, unit: OffsetUnit, offset: usize, length: usize) -> Option<(usize, usize)> {
    let end = offset.checked_add(length).filter(|_| length > 0)?;
    let (a, b) = match unit {
        OffsetUnit::CodePoints => (offset, end),
        OffsetUnit::Utf16 => (utf16_to_code_point(f, offset)?, utf16_to_code_point(f, end)?),
    };
    (a < b && b <= f.chars).then_some((a, b))
}

/// The code point at UTF-16 offset `u` of the request's text; `None` inside a surrogate pair.
fn utf16_to_code_point(f: &InFlight, u: usize) -> Option<usize> {
    let before = f.astral.partition_point(|&s| s < u);
    if before > 0 && f.astral.get(before - 1).is_some_and(|&s| s + 1 == u) {
        return None;
    }
    u.checked_sub(before)
}

/// A result placed on a paragraph's bytes. A match over a character left out of the text sent
/// (a tracked deletion, an object) is dropped.
fn place(c: &Checked, p: &Pending) -> Placed {
    let span = |a: usize, b: usize| -> Option<Range<usize>> {
        let first = p.chars.get(a)?;
        let last = p.chars.get(b.checked_sub(1)?)?;
        (a < b).then_some(first.0..last.0 + last.1)
    };
    let mut out = Placed::default();
    for f in &c.found {
        let Some(range) = span(f.start, f.end) else { continue };
        let bytes: usize = p.chars.get(f.start..f.end).map_or(0, |cs| cs.iter().map(|x| x.1).sum());
        if bytes != range.len() {
            continue;
        }
        let lang = p.langs.iter().find(|(r, _)| r.start <= range.start && range.start < r.end).map(|(_, l)| *l).unwrap_or_default();
        let kind = match f.kind {
            RemoteKind::Spelling => IssueKind::Spelling,
            RemoteKind::Grammar => IssueKind::Grammar,
        };
        let issue = Issue { start: range.start, end: range.end, kind, message: f.message.clone(), suggestions: f.replacements.clone(), lang };
        out.remote.push(RemoteIssue { issue, rule: f.rule.clone() });
    }
    for q in &c.quiet {
        if let Some(range) = span(q.start, q.end).filter(|r| !r.is_empty()) {
            out.quiet.push(range);
        }
    }
    out
}

#[cfg(test)]
mod tests;

//! Proofing results from outside the built-in checker, per paragraph revision: a remote
//! checker's issues, the sentences whose English spelling marks it asks to hide (text it routed
//! to another language) and the issues the viewer ignored. [`merged_issues`] joins them with the
//! built-in issues ([`paragraph_issues`]) into the one list the squiggles, F7, the proofing pane
//! and the right-click menu all read.

use std::collections::{BTreeSet, HashMap};
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::sync::Arc;

use wordcraft_doc::Paragraph;
use wordcraft_proof::{Issue, IssueKind, ProofLang};

use super::proof::{ProofRun, overlaps, paragraph_issues, skipped_ranges};

/// An issue a remote checker found, with the checker's rule id. Its byte offsets are in
/// [`Paragraph::text`]; its suggestions are the checker's own.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteIssue {
    pub issue: Issue,
    pub rule: String,
}

/// One issue the viewer ignored ("Ignore"): its byte range and rule.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct IgnoredIssue {
    pub start: usize,
    pub end: usize,
    pub rule: String,
}

/// What a paragraph revision gets beyond the built-in checks.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProofOverlay {
    pub remote: Vec<RemoteIssue>,
    /// Byte ranges whose English spelling marks are hidden (sentences checked as another
    /// language).
    pub quiet_spelling: Vec<Range<usize>>,
    pub ignored: Vec<IgnoredIssue>,
}

impl ProofOverlay {
    /// A hash of everything the overlay holds.
    fn hash_key(&self) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.remote.len().hash(&mut h);
        for r in &self.remote {
            hash_issue(&r.issue, &mut h);
            r.rule.hash(&mut h);
        }
        for q in &self.quiet_spelling {
            (q.start, q.end).hash(&mut h);
        }
        self.ignored.hash(&mut h);
        h.finish()
    }
}

fn hash_issue(i: &Issue, h: &mut impl Hasher) {
    (i.start, i.end, i.kind == IssueKind::Grammar).hash(h);
    i.message.hash(h);
    i.suggestions.hash(h);
    i.lang.hash(h);
}

/// The overlays of a document's paragraphs, by paragraph revision ([`Paragraph::rev`]), with
/// the rules and words ignored across the document. Its keys tell the layout cache what changed:
/// a paragraph's overlay hash lays out only that paragraph again, [`ProofOverlays::rules_key`]
/// every paragraph.
#[derive(Clone, Debug, Default)]
pub struct ProofOverlays {
    by_rev: HashMap<u64, (u64, Arc<ProofOverlay>)>,
    ignored_rules: BTreeSet<String>,
    words: BTreeSet<String>,
    rules_key: u64,
    key: u64,
}

impl ProofOverlays {
    pub fn new(by_rev: HashMap<u64, Arc<ProofOverlay>>, ignored_rules: BTreeSet<String>, words: BTreeSet<String>) -> Self {
        let by_rev: HashMap<u64, (u64, Arc<ProofOverlay>)> = by_rev.into_iter().map(|(rev, o)| (rev, (o.hash_key(), o))).collect();
        // 0 with nothing ignored, as with no overlays at all, so the first overlays a
        // document gets lay out only the paragraphs they cover.
        let rules_key = if ignored_rules.is_empty() && words.is_empty() {
            0
        } else {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            ignored_rules.hash(&mut h);
            words.hash(&mut h);
            h.finish()
        };
        let key = {
            let mut revs: Vec<(u64, u64)> = by_rev.iter().map(|(rev, (k, _))| (*rev, *k)).collect();
            revs.sort_unstable();
            let mut h = std::collections::hash_map::DefaultHasher::new();
            rules_key.hash(&mut h);
            revs.hash(&mut h);
            h.finish()
        };
        ProofOverlays { by_rev, ignored_rules, words, rules_key, key }
    }

    /// The overlay of paragraph revision `rev`, with its hash.
    pub fn get(&self, rev: u64) -> Option<(u64, &ProofOverlay)> {
        self.by_rev.get(&rev).map(|(k, o)| (*k, o.as_ref()))
    }

    /// A hash of the ignored rules and words, which apply to every paragraph: 0 when neither has
    /// any.
    pub fn rules_key(&self) -> u64 {
        self.rules_key
    }

    /// A hash of [`ProofOverlays::rules_key`] and every overlay's hash.
    pub fn key(&self) -> u64 {
        self.key
    }

    /// Rules ignored in the whole document ("Ignore Rule").
    pub fn ignored_rules(&self) -> &BTreeSet<String> {
        &self.ignored_rules
    }

    /// Words whose remote spelling issues are hidden (ignored or in the user dictionary).
    pub fn words(&self) -> &BTreeSet<String> {
        &self.words
    }
}

/// An issue in the merged list: from the built-in checker or a remote one (`remote`), with its
/// rule id ([`builtin_rule`] for a built-in one).
#[derive(Clone, Debug, PartialEq)]
pub struct ProofIssue {
    pub issue: Issue,
    pub rule: String,
    pub remote: bool,
}

/// The rule id of a built-in issue: the built-in checker has no rule ids, so a grammar rule is
/// known by its message.
pub fn builtin_rule(i: &Issue) -> String {
    match i.kind {
        IssueKind::Spelling => "builtin:spelling".to_string(),
        IssueKind::Grammar => format!("builtin:grammar:{}", i.message),
    }
}

fn is_english(lang: ProofLang) -> bool {
    matches!(lang, ProofLang::En | ProofLang::EnGb | ProofLang::EnAu | ProofLang::EnCa)
}

/// The paragraph's issues, built-in and remote, as one list sorted by start:
/// 1. the built-in issues ([`paragraph_issues`]), less English spelling overlapping a
///    `quiet_spelling` range;
/// 2. the remote issues, less those overlapping a run left unchecked, grammar unless `grammar`,
///    spelling of a word in [`ProofOverlays::words`], and any whose range isn't in the text;
/// 3. a built-in issue with a remote one of the same range and kind is left out: the remote one
///    is shown, once;
/// 4. less issues ignored there (same range and rule) or by rule ([`ProofOverlays::ignored_rules`]).
pub fn merged_issues(
    p: &Paragraph,
    runs: &[ProofRun],
    spelling: &wordcraft_proof::Options,
    grammar: bool,
    list_item: bool,
    overlay: Option<&ProofOverlay>,
    overlays: Option<&ProofOverlays>,
) -> Vec<ProofIssue> {
    let builtin = paragraph_issues(p, runs, spelling, grammar, list_item);
    let quiet: &[Range<usize>] = overlay.map_or(&[], |o| &o.quiet_spelling);
    let builtin = builtin
        .into_iter()
        .filter(|i| !(i.kind == IssueKind::Spelling && is_english(i.lang) && quiet.iter().any(|q| i.start < q.end && q.start < i.end)));
    let mut remote: Vec<ProofIssue> = Vec::new();
    if let Some(o) = overlay
        && !o.remote.is_empty()
    {
        let skipped = skipped_ranges(runs);
        let words = overlays.map(ProofOverlays::words);
        for r in &o.remote {
            let i = &r.issue;
            let Some(word) = p.text.get(i.start..i.end).filter(|_| i.start < i.end) else { continue };
            if overlaps(&skipped, i.start, i.end) || (i.kind == IssueKind::Grammar && !grammar) {
                continue;
            }
            if i.kind == IssueKind::Spelling && words.is_some_and(|w| w.contains(word) || w.contains(&word.to_lowercase())) {
                continue;
            }
            if remote.iter().any(|m| same_place(&m.issue, i)) {
                continue;
            }
            remote.push(ProofIssue { issue: i.clone(), rule: r.rule.clone(), remote: true });
        }
    }
    let mut out: Vec<ProofIssue> = builtin
        .filter(|i| !remote.iter().any(|r| same_place(&r.issue, i)))
        .map(|i| ProofIssue { rule: builtin_rule(&i), issue: i, remote: false })
        .collect();
    out.append(&mut remote);
    let ignored: &[IgnoredIssue] = overlay.map_or(&[], |o| &o.ignored);
    out.retain(|m| {
        !ignored.iter().any(|g| g.start == m.issue.start && g.end == m.issue.end && g.rule == m.rule)
            && !overlays.is_some_and(|o| o.ignored_rules.contains(&m.rule))
    });
    out.sort_by_key(|m| m.issue.start);
    out
}

/// Same byte range and kind.
fn same_place(a: &Issue, b: &Issue) -> bool {
    a.start == b.start && a.end == b.end && a.kind == b.kind
}

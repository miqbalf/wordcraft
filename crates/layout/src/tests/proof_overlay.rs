//! Remote proofing results and ignores laid over the built-in checks ([`crate::para::merged_issues`]):
//! what is drawn, what is left out, and what a change to them lays out again.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use wordcraft_doc::{Document, Path, Pos, StoryRef};
use wordcraft_proof::{Issue, IssueKind};

use crate::para::{IgnoredIssue, ProofOverlay, ProofOverlays, RemoteIssue, merged_issues, paragraph_issues, proof_runs};
use crate::{DocLayout, LayoutCache, LayoutOptions, Placed, display, layout};

fn rev(d: &Document, k: usize) -> u64 {
    d.para(StoryRef::Body, &Path::top(k)).map(|p| p.rev).unwrap_or_default()
}

fn remote(start: usize, end: usize, kind: IssueKind, rule: &str) -> RemoteIssue {
    let issue = Issue { start, end, kind, message: format!("remote {rule}"), suggestions: vec!["fix".into()], lang: Default::default() };
    RemoteIssue { issue, rule: rule.into() }
}

fn overlays(by_rev: Vec<(u64, ProofOverlay)>, rules: &[&str]) -> Option<Arc<ProofOverlays>> {
    let by_rev: HashMap<u64, Arc<ProofOverlay>> = by_rev.into_iter().map(|(r, o)| (r, Arc::new(o))).collect();
    Some(Arc::new(ProofOverlays::new(by_rev, rules.iter().map(|r| r.to_string()).collect(), BTreeSet::new())))
}

fn builtin(d: &Document, k: usize) -> Vec<Issue> {
    let Some(p) = d.para(StoryRef::Body, &Path::top(k)) else { return Vec::new() };
    paragraph_issues(p, &proof_runs(d, p, None), &Default::default(), true, false)
}

/// The squiggles of paragraph `k` as (start, end, grammar).
fn squiggles(l: &DocLayout, k: usize) -> Vec<(usize, usize, bool)> {
    l.pages[0]
        .items
        .iter()
        .filter_map(|i| match i {
            Placed::Lines { para, path, .. } if *path == Path::top(k) => Some(para.issues.clone()),
            _ => None,
        })
        .flatten()
        .collect()
}

fn proofed() -> LayoutOptions {
    LayoutOptions { proofing: true, ..LayoutOptions::default() }
}

fn doubles(d: &Document, l: &DocLayout) -> usize {
    display::page_display(d, &l.pages[0], &display::DisplayOptions::default())
        .iter()
        .filter(|i| matches!(i, display::Draw::Line { stroke: display::Stroke::Double, .. }))
        .count()
}

#[test]
fn remote_issue_draws_a_squiggle_and_a_same_range_builtin_is_shown_once() {
    crate::test_fonts();
    let text = "this is a test.";
    let d = Document::from_text(text);
    let grammar = builtin(&d, 0).into_iter().find(|i| i.kind == IssueKind::Grammar).expect("a built-in grammar issue");
    let mut c = LayoutCache::new();
    let plain = layout(&d, &mut c, &proofed());
    let before = doubles(&d, &plain);
    assert!(before >= 1);
    // A remote issue on the built-in one's range, and one on "test" that nothing else marks.
    let test = text.find("test").unwrap_or_default();
    let o = ProofOverlay {
        remote: vec![remote(grammar.start, grammar.end, IssueKind::Grammar, "R_SAME"), remote(test, test + 4, IssueKind::Grammar, "R_NEW")],
        ..Default::default()
    };
    let l = layout(&d, &mut c, &LayoutOptions { proof_overlays: overlays(vec![(rev(&d, 0), o)], &[]), ..proofed() });
    assert_eq!(doubles(&d, &l), before + 1, "the shared range is drawn once and the new one is drawn");
    let marks = squiggles(&l, 0);
    assert_eq!(marks.iter().filter(|m| m.0 == grammar.start && m.1 == grammar.end).count(), 1, "{marks:?}");
    assert!(marks.contains(&(test, test + 4, true)), "{marks:?}");
}

#[test]
fn quiet_spelling_hides_english_spelling_only_inside_the_range() {
    crate::test_fonts();
    let text = "Kami akan mengirimkan. Teh cat.";
    let d = Document::from_text(text);
    let quiet = 0..text.find('.').unwrap_or_default() + 1;
    let teh = text.find("Teh").unwrap_or_default();
    let spelling = |marks: &[(usize, usize, bool)]| marks.iter().filter(|m| !m.2).map(|m| (m.0, m.1)).collect::<Vec<_>>();
    let mut c = LayoutCache::new();
    let plain = spelling(&squiggles(&layout(&d, &mut c, &proofed()), 0));
    assert!(plain.iter().any(|m| m.0 < quiet.end), "untagged Indonesian is marked as English: {plain:?}");
    let o = ProofOverlay { quiet_spelling: vec![quiet.clone()], ..Default::default() };
    let l = layout(&d, &mut c, &LayoutOptions { proof_overlays: overlays(vec![(rev(&d, 0), o)], &[]), ..proofed() });
    let marks = spelling(&squiggles(&l, 0));
    assert!(marks.iter().all(|m| m.0 >= quiet.end), "{marks:?}");
    assert_eq!(marks.iter().filter(|m| m.0 == teh).count(), 1, "{marks:?}");
}

#[test]
fn ignored_occurrence_and_ignored_rule_drop_builtin_and_remote_issues() {
    crate::test_fonts();
    let text = "this is a tset here and there.";
    let d = Document::from_text(text);
    let Some(p) = d.para(StoryRef::Body, &Path::top(0)) else { panic!("no paragraph") };
    let runs = proof_runs(&d, p, None);
    let tset = text.find("tset").unwrap_or_default();
    let here = text.find("here").unwrap_or_default();
    let there = text.find("there").unwrap_or_default();
    let grammar = builtin(&d, 0).into_iter().find(|i| i.kind == IssueKind::Grammar).expect("a built-in grammar issue");
    let base = ProofOverlay {
        remote: vec![remote(here, here + 4, IssueKind::Grammar, "R_ONE"), remote(there, there + 5, IssueKind::Grammar, "R_TWO")],
        ..Default::default()
    };
    let starts = |o: &ProofOverlay, rules: &[&str]| -> Vec<usize> {
        let all = overlays(vec![(p.rev, o.clone())], rules);
        merged_issues(p, &runs, &Default::default(), true, false, Some(o), all.as_deref()).iter().map(|m| m.issue.start).collect()
    };
    let shown = starts(&base, &[]);
    for s in [grammar.start, tset, here, there] {
        assert!(shown.contains(&s), "{s} in {shown:?}");
    }
    // This occurrence of the built-in spelling issue and of the remote one, by range and rule.
    let mut o = base.clone();
    o.ignored = vec![
        IgnoredIssue { start: tset, end: tset + 4, rule: "builtin:spelling".into() },
        IgnoredIssue { start: here, end: here + 4, rule: "R_ONE".into() },
        // The right range with the wrong rule ignores nothing.
        IgnoredIssue { start: there, end: there + 5, rule: "R_ONE".into() },
    ];
    let shown = starts(&o, &[]);
    assert!(!shown.contains(&tset) && !shown.contains(&here), "{shown:?}");
    assert!(shown.contains(&there) && shown.contains(&grammar.start), "{shown:?}");
    // Whole rules: a remote one and the built-in grammar rule.
    let builtin_rule = crate::para::builtin_rule(&grammar);
    let shown = starts(&base, &["R_TWO", builtin_rule.as_str()]);
    assert!(!shown.contains(&there) && !shown.contains(&grammar.start), "{shown:?}");
    assert!(shown.contains(&here) && shown.contains(&tset), "{shown:?}");
}

#[test]
fn remote_issue_over_an_unchecked_run_is_dropped() {
    crate::test_fonts();
    let text = "alpha beta gamma delta";
    let mut d = Document::from_text(text);
    let beta = text.find("beta").unwrap_or_default();
    let delta = text.find("delta").unwrap_or_default();
    d.format_range(&Pos::body(0, beta), &Pos::body(0, beta + 4), &|c| c.no_proof = Some(true)).unwrap();
    let o = ProofOverlay {
        remote: vec![remote(beta, beta + 4, IssueKind::Grammar, "R_A"), remote(delta, delta + 5, IssueKind::Grammar, "R_B")],
        ..Default::default()
    };
    let mut c = LayoutCache::new();
    let l = layout(&d, &mut c, &LayoutOptions { proof_overlays: overlays(vec![(rev(&d, 0), o)], &[]), ..proofed() });
    let marks = squiggles(&l, 0);
    assert!(!marks.iter().any(|m| m.0 < beta + 4 && beta < m.1), "{marks:?}");
    assert!(marks.contains(&(delta, delta + 5, true)), "{marks:?}");
}

#[test]
fn overlay_change_relayouts_only_its_paragraph() {
    crate::test_fonts();
    let d = Document::from_text("first paragraph here\nsecond paragraph here");
    let mut c = LayoutCache::new();
    let plain = layout(&d, &mut c, &proofed());
    let (misses, hits) = (c.misses, c.hits);
    let o = ProofOverlay { remote: vec![remote(7, 16, IssueKind::Grammar, "R_X")], ..Default::default() };
    let l = layout(&d, &mut c, &LayoutOptions { proof_overlays: overlays(vec![(rev(&d, 1), o)], &[]), ..proofed() });
    assert_eq!(c.misses, misses + 1, "only the paragraph with the overlay is laid out again");
    assert!(c.hits > hits, "the first paragraph comes from the cache");
    assert_eq!(squiggles(&l, 0), squiggles(&plain, 0));
    assert_ne!(squiggles(&l, 1), squiggles(&plain, 1));
}

#[test]
fn ignored_rules_change_invalidates_every_paragraph() {
    crate::test_fonts();
    let d = Document::from_text("first paragraph here\nsecond paragraph here\nthird one");
    let mut c = LayoutCache::new();
    let o = || ProofOverlay { remote: vec![remote(7, 16, IssueKind::Grammar, "R_X")], ..Default::default() };
    layout(&d, &mut c, &LayoutOptions { proof_overlays: overlays(vec![(rev(&d, 1), o())], &[]), ..proofed() });
    let misses = c.misses;
    let l = layout(&d, &mut c, &LayoutOptions { proof_overlays: overlays(vec![(rev(&d, 1), o())], &["R_X"]), ..proofed() });
    assert_eq!(c.misses, misses + d.body.len() as u64, "every paragraph is laid out again");
    assert!(!squiggles(&l, 1).contains(&(7, 16, true)), "the ignored rule is gone");
}

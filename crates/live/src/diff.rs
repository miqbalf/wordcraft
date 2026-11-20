//! Small diffs: a block's text by common prefix and suffix, its runs by constant segments, and
//! the longest run of blocks that kept their order.

use craft_collab::Attributes;
use serde_json::Value;

/// One text change: at char `at`, delete `del` chars and insert `ins`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct TextEdit {
    pub at: usize,
    pub del: usize,
    pub ins: String,
}

/// The change from `have` to `want`, outside their common prefix and suffix. `None` when equal.
pub(crate) fn text_edit(have: &str, want: &str) -> Option<TextEdit> {
    if have == want {
        return None;
    }
    let a: Vec<char> = have.chars().collect();
    let b: Vec<char> = want.chars().collect();
    let prefix = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let room = a.len().min(b.len()) - prefix;
    let suffix = a.iter().rev().zip(b.iter().rev()).take(room).take_while(|(x, y)| x == y).count();
    Some(TextEdit { at: prefix, del: a.len() - prefix - suffix, ins: b[prefix..b.len() - suffix].iter().collect() })
}

/// Chars `a` and `b` share at the front.
pub(crate) fn common_prefix(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count()
}

/// Chars `a` and `b` share at the end.
pub(crate) fn common_suffix(a: &str, b: &str) -> usize {
    a.chars().rev().zip(b.chars().rev()).take_while(|(x, y)| x == y).count()
}

/// Runs covering `len` chars: no runs is one unformatted run.
fn cover(runs: &[(usize, Attributes)], len: usize) -> Vec<(usize, Attributes)> {
    let mut out: Vec<(usize, Attributes)> = runs.iter().filter(|(n, _)| *n > 0).cloned().collect();
    let covered: usize = out.iter().map(|(n, _)| n).sum();
    if covered < len {
        out.push((len - covered, Attributes::new()));
    }
    out
}

/// The run attributes at char `at` (those of the char before it at the end).
pub(crate) fn attrs_at(runs: &[(usize, Attributes)], at: usize) -> Attributes {
    let mut start = 0;
    let mut last = Attributes::new();
    for (n, a) in runs {
        if at < start + n {
            return a.clone();
        }
        start += n;
        last = a.clone();
    }
    last
}

/// What turns attributes `have` into `want`: `want`, plus a null for each key only `have` sets.
fn patch(have: &Attributes, want: &Attributes) -> Attributes {
    let mut out = want.clone();
    for k in have.keys().filter(|k| !want.contains_key(*k)) {
        out.insert(k.clone(), Value::Null);
    }
    out
}

/// `(at, len, attributes)` format calls that turn runs `have` into `want` over `len` chars.
pub(crate) fn format_edits(have: &[(usize, Attributes)], want: &[(usize, Attributes)], len: usize) -> Vec<(usize, usize, Attributes)> {
    let (have, want) = (cover(have, len), cover(want, len));
    let (mut hi, mut wi, mut hleft, mut wleft) = (0, 0, have.first().map_or(0, |r| r.0), want.first().map_or(0, |r| r.0));
    let mut at = 0;
    let mut out: Vec<(usize, usize, Attributes)> = Vec::new();
    while let (Some((_, h)), Some((_, w))) = (have.get(hi), want.get(wi)) {
        let n = hleft.min(wleft);
        if h != w && n > 0 {
            let p = patch(h, w);
            match out.last_mut() {
                Some((s, l, q)) if *s + *l == at && *q == p => *l += n,
                _ => out.push((at, n, p)),
            }
        }
        at += n;
        hleft -= n;
        wleft -= n;
        if hleft == 0 {
            hi += 1;
            hleft = have.get(hi).map_or(0, |r| r.0);
        }
        if wleft == 0 {
            wi += 1;
            wleft = want.get(wi).map_or(0, |r| r.0);
        }
    }
    out
}

/// Which entries of `seq` lie on one longest strictly increasing subsequence.
pub(crate) fn increasing(seq: &[usize]) -> Vec<bool> {
    // tails[k]: index in seq of the smallest tail of an increasing run of length k + 1.
    let mut tails: Vec<usize> = Vec::new();
    let mut prev: Vec<Option<usize>> = vec![None; seq.len()];
    for (i, v) in seq.iter().enumerate() {
        let k = tails.partition_point(|t| seq.get(*t).is_some_and(|x| x < v));
        prev[i] = k.checked_sub(1).and_then(|j| tails.get(j).copied());
        if k == tails.len() {
            tails.push(i);
        } else {
            tails[k] = i;
        }
    }
    let mut keep = vec![false; seq.len()];
    let mut cur = tails.last().copied();
    while let Some(i) = cur {
        keep[i] = true;
        cur = prev[i];
    }
    keep
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_edit_is_the_middle() {
        assert_eq!(text_edit("hello", "help"), Some(TextEdit { at: 3, del: 2, ins: "p".into() }));
        assert_eq!(text_edit("aaa", "aaaa"), Some(TextEdit { at: 3, del: 0, ins: "a".into() }));
        assert_eq!(text_edit("é😀x", "é😀"), Some(TextEdit { at: 2, del: 1, ins: String::new() }));
        assert_eq!(text_edit("same", "same"), None);
    }

    #[test]
    fn increasing_keeps_the_longest_order() {
        let keep = increasing(&[0, 3, 1, 2, 4]);
        assert_eq!(keep, [true, false, true, true, true]);
    }
}

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
    let room = a.len().min(b.len()).saturating_sub(prefix);
    let suffix = a.iter().rev().zip(b.iter().rev()).take(room).take_while(|(x, y)| x == y).count();
    let del = a.len().saturating_sub(prefix + suffix);
    let ins = b.iter().skip(prefix).take(b.len().saturating_sub(prefix + suffix)).collect();
    Some(TextEdit { at: prefix, del, ins })
}

/// The separate edits that turn `have` into `want`, in order, each `at` in `have`'s chars: a
/// shortest char diff of the stretch [`text_edit`] finds, so two edits apart stay apart. That
/// stretch is one edit when it is too long to diff (over about a million char pairs).
pub(crate) fn text_edits(have: &str, want: &str) -> Vec<TextEdit> {
    let Some(one) = text_edit(have, want) else { return Vec::new() };
    let x: Vec<char> = have.chars().skip(one.at).take(one.del).collect();
    let y: Vec<char> = one.ins.chars().collect();
    let (n, m) = (x.len(), y.len());
    if n == 0 || m == 0 || n.saturating_mul(m) > 1 << 20 {
        return vec![one];
    }
    // lcs[i * w + j]: the longest common subsequence of x[i..] and y[j..].
    let w = m + 1;
    let mut lcs = vec![0u32; (n + 1) * w];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i * w + j] = if x[i] == y[j] { lcs[(i + 1) * w + j + 1] + 1 } else { lcs[(i + 1) * w + j].max(lcs[i * w + j + 1]) };
        }
    }
    let (mut out, mut cur, mut i, mut j) = (Vec::new(), None::<TextEdit>, 0, 0);
    while i < n || j < m {
        if i < n && j < m && x[i] == y[j] {
            out.extend(cur.take());
            i += 1;
            j += 1;
            continue;
        }
        let e = cur.get_or_insert_with(|| TextEdit { at: one.at + i, del: 0, ins: String::new() });
        if j < m && (i == n || lcs[i * w + j + 1] >= lcs[(i + 1) * w + j]) {
            e.ins.push(y[j]);
            j += 1;
        } else {
            e.del += 1;
            i += 1;
        }
    }
    out.extend(cur);
    out
}

/// Where char `at` of the old text is after `edits` (from [`text_edits`]): moved by what the
/// edits before it added or removed; inside deleted text it goes to the start of that edit.
pub(crate) fn follow_edits(edits: &[TextEdit], at: usize) -> usize {
    let mut shift = 0isize;
    for e in edits {
        if at <= e.at {
            break;
        }
        if at < e.at + e.del {
            return e.at.saturating_add_signed(shift);
        }
        shift += e.ins.chars().count() as isize - e.del as isize;
    }
    at.saturating_add_signed(shift)
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

/// The runs over chars `from..to` of runs `runs`, in the canonical form (none when no char of
/// the stretch is formatted).
pub(crate) fn slice_runs(runs: &[(usize, Attributes)], from: usize, to: usize) -> Vec<(usize, Attributes)> {
    let mut out: Vec<(usize, Attributes)> = Vec::new();
    let mut start = 0;
    for (n, a) in runs {
        let (lo, hi) = (start.max(from), (start + n).min(to));
        start += n;
        if lo >= hi {
            continue;
        }
        match out.last_mut() {
            Some((m, b)) if b == a => *m += hi - lo,
            _ => out.push((hi - lo, a.clone())),
        }
    }
    if out.iter().all(|(_, a)| a.is_empty()) {
        out.clear();
    }
    out
}

/// What turns attributes `have` into `want`: `want`, plus a null for each key only `have` sets.
fn patch(have: &Attributes, want: &Attributes) -> Attributes {
    let mut out = want.clone();
    for k in have.keys().filter(|k| !want.contains_key(*k)) {
        out.insert(k.clone(), Value::Null);
    }
    out
}

/// Only the attributes that differ from `have` to `want`: `want`'s value, or a null where
/// `want` drops it. `None` when they are the same.
pub(crate) fn delta(have: &Attributes, want: &Attributes) -> Option<Attributes> {
    let mut out: Attributes = want.iter().filter(|(k, v)| have.get(*k) != Some(*v)).map(|(k, v)| (k.clone(), v.clone())).collect();
    out.extend(have.keys().filter(|k| !want.contains_key(*k)).map(|k| (k.clone(), Value::Null)));
    (!out.is_empty()).then_some(out)
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
    let mut prev: Vec<Option<usize>> = Vec::with_capacity(seq.len());
    for (i, v) in seq.iter().enumerate() {
        let k = tails.partition_point(|t| seq.get(*t).is_some_and(|x| x < v));
        prev.push(k.checked_sub(1).and_then(|j| tails.get(j).copied()));
        match tails.get_mut(k) {
            Some(t) => *t = i,
            None => tails.push(i),
        }
    }
    let mut keep = vec![false; seq.len()];
    let mut cur = tails.last().copied();
    while let Some(i) = cur {
        if let Some(k) = keep.get_mut(i) {
            *k = true;
        }
        cur = prev.get(i).copied().flatten();
    }
    keep
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_edits_keep_separate_edits_apart() {
        let e = text_edits("Alpha beta", "Apha beta!");
        assert_eq!(e, [TextEdit { at: 1, del: 1, ins: String::new() }, TextEdit { at: 10, del: 0, ins: "!".into() }]);
        assert_eq!(follow_edits(&e, 5), 4);
        assert_eq!(follow_edits(&e, 2), 1);
        assert_eq!(follow_edits(&e, 10), 9);
        assert_eq!(text_edits("same", "same"), []);
        assert_eq!(text_edits("abc", "xyz"), [TextEdit { at: 0, del: 3, ins: "xyz".into() }]);
    }

    #[test]
    fn text_edit_is_the_middle() {
        assert_eq!(text_edit("hello", "help"), Some(TextEdit { at: 3, del: 2, ins: "p".into() }));
        assert_eq!(text_edit("aaa", "aaaa"), Some(TextEdit { at: 3, del: 0, ins: "a".into() }));
        assert_eq!(text_edit("é😀x", "é😀"), Some(TextEdit { at: 2, del: 1, ins: String::new() }));
        assert_eq!(text_edit("same", "same"), None);
    }

    #[test]
    fn follow_moves_with_the_text_around_it() {
        // "abc|def": typing before the caret moves it, after it does not.
        assert_eq!(follow_edits(&text_edits("abcdef", "XYabcdef"), 3), 5);
        assert_eq!(follow_edits(&text_edits("abcdef", "abcdefZ"), 3), 3);
        // Deleting around it puts it where the deletion was.
        assert_eq!(follow_edits(&text_edits("abcdef", "af"), 3), 1);
    }

    #[test]
    fn slice_runs_cuts_and_drops_plain() {
        let bold: Attributes = [("bold".to_owned(), Value::Bool(true))].into_iter().collect();
        let runs = vec![(3, Attributes::new()), (4, bold.clone()), (2, Attributes::new())];
        assert_eq!(slice_runs(&runs, 2, 8), vec![(1, Attributes::new()), (4, bold), (1, Attributes::new())]);
        assert!(slice_runs(&runs, 0, 3).is_empty(), "a stretch with no formatting has no runs");
    }

    #[test]
    fn increasing_keeps_the_longest_order() {
        let keep = increasing(&[0, 3, 1, 2, 4]);
        assert_eq!(keep, [true, false, true, true, true]);
    }
}

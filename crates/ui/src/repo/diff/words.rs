//! Word-level diff between a deleted line and the added line it was replaced by.

use std::ops::Range;

/// Token products above this fall back to trimming the common prefix / suffix.
const LCS_LIMIT: usize = 40_000;

/// Changed byte ranges in `old` and `new`. Empty when the lines barely share anything (then
/// highlighting words only adds noise).
pub fn changed(old: &str, new: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    if old == new {
        return (Vec::new(), Vec::new());
    }
    let (a, b) = (tokens(old), tokens(new));
    let keep = if a.len() * b.len() <= LCS_LIMIT {
        lcs(old, &a, new, &b)
    } else {
        prefix_suffix(old, &a, new, &b)
    };
    let (keep_a, keep_b) = keep;
    let unchanged: usize = a
        .iter()
        .zip(&keep_a)
        .filter(|(t, k)| **k && !old[(*t).clone()].trim().is_empty())
        .map(|(t, _)| t.len())
        .sum();
    let total = old.trim().len().max(new.trim().len()).max(1);
    if unchanged * 4 < total {
        return (Vec::new(), Vec::new());
    }
    (ranges(&a, &keep_a), ranges(&b, &keep_b))
}

/// Words (alphanumerics, `_`, any non-ASCII byte), whitespace runs, single punctuation.
/// Never splits a UTF-8 sequence, so every range is on a char boundary.
fn tokens(text: &str) -> Vec<Range<usize>> {
    #[derive(PartialEq)]
    enum Class {
        Word,
        Space,
        Punct,
    }
    let class = |b: u8| {
        if b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80 {
            Class::Word
        } else if b.is_ascii_whitespace() {
            Class::Space
        } else {
            Class::Punct
        }
    };
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    while start < bytes.len() {
        let c = class(bytes[start]);
        let mut end = start + 1;
        if c != Class::Punct {
            while end < bytes.len() && class(bytes[end]) == c {
                end += 1;
            }
        }
        out.push(start..end);
        start = end;
    }
    out
}

/// Which tokens each side keeps (part of the longest common subsequence).
fn lcs(old: &str, a: &[Range<usize>], new: &str, b: &[Range<usize>]) -> (Vec<bool>, Vec<bool>) {
    let (n, m) = (a.len(), b.len());
    let eq = |i: usize, j: usize| old[a[i].clone()] == new[b[j].clone()];
    // table[i][j] = LCS length of a[i..] and b[j..].
    let mut table = vec![0u32; (n + 1) * (m + 1)];
    let at = |i: usize, j: usize| i * (m + 1) + j;
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[at(i, j)] = if eq(i, j) {
                table[at(i + 1, j + 1)] + 1
            } else {
                table[at(i + 1, j)].max(table[at(i, j + 1)])
            };
        }
    }
    let (mut keep_a, mut keep_b) = (vec![false; n], vec![false; m]);
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if eq(i, j) {
            keep_a[i] = true;
            keep_b[j] = true;
            i += 1;
            j += 1;
        } else if table[at(i + 1, j)] >= table[at(i, j + 1)] {
            i += 1;
        } else {
            j += 1;
        }
    }
    (keep_a, keep_b)
}

fn prefix_suffix(
    old: &str,
    a: &[Range<usize>],
    new: &str,
    b: &[Range<usize>],
) -> (Vec<bool>, Vec<bool>) {
    let eq = |i: usize, j: usize| old[a[i].clone()] == new[b[j].clone()];
    let prefix = (0..a.len().min(b.len())).take_while(|&k| eq(k, k)).count();
    let suffix = (0..a.len().min(b.len()) - prefix)
        .take_while(|&k| eq(a.len() - 1 - k, b.len() - 1 - k))
        .count();
    let keep = |len: usize| {
        (0..len)
            .map(|k| k < prefix || k >= len - suffix)
            .collect::<Vec<_>>()
    };
    (keep(a.len()), keep(b.len()))
}

/// Merges adjacent changed tokens into ranges.
fn ranges(tokens: &[Range<usize>], keep: &[bool]) -> Vec<Range<usize>> {
    let mut out: Vec<Range<usize>> = Vec::new();
    for (t, _) in tokens.iter().zip(keep).filter(|(_, k)| !**k) {
        match out.last_mut() {
            Some(last) if last.end == t.start => last.end = t.end,
            _ => out.push(t.clone()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_word_changed() {
        let (a, b) = changed(
            "let kind = Kind::Renamed,",
            "let kind = Kind::RenamedOrCopied {",
        );
        assert_eq!(a, vec![17..25]);
        // `RenamedOrCopied {` (the `,` went away with it).
        assert_eq!(b, vec![17..34]);
    }

    #[test]
    fn unrelated_lines_get_no_emphasis() {
        assert_eq!(
            changed("foo(bar)", "completely different"),
            (vec![], vec![])
        );
        assert_eq!(changed("same", "same"), (vec![], vec![]));
    }

    #[test]
    fn non_ascii_stays_on_char_boundaries() {
        let (a, b) = changed("naïve café", "naïve cafés");
        assert!(a.iter().all(|r| "naïve café".is_char_boundary(r.start)));
        assert_eq!(b.len(), 1);
    }

    #[test]
    fn long_lines_fall_back() {
        let old = "x ".repeat(300) + "a";
        let new = "x ".repeat(300) + "b";
        let (a, b) = changed(&old, &new);
        assert_eq!(a, vec![600..601]);
        assert_eq!(b, vec![600..601]);
    }
}

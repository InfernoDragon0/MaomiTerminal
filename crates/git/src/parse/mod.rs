//! Parsers for git's machine-readable output. Pure functions over bytes: no I/O, no panics on
//! malformed or huge input (fuzzed, see `fuzz/` at the repo root).
//!
//! Each module exports the exact arguments its parser expects (`ARGS` / `FORMAT`), so
//! [`crate::CliRepo`] and the parser can never drift apart.

pub mod diff;
pub mod log;
pub mod refs;
pub mod status;

/// Malformed git output. Carries the byte offset where parsing gave up.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("cannot parse git {what} output at byte {offset}: {reason}")]
pub struct ParseError {
    pub what: &'static str,
    pub offset: usize,
    pub reason: String,
}

impl ParseError {
    pub fn new(what: &'static str, offset: usize, reason: impl Into<String>) -> Self {
        Self {
            what,
            offset,
            reason: reason.into(),
        }
    }
}

pub type ParseResult<T> = Result<T, ParseError>;

/// Splits `input` into `sep`-terminated records, yielding `(offset, record)`. A last record
/// without a terminator is kept; nothing is yielded for the empty tail after a final `sep`.
fn records(input: &[u8], sep: u8) -> impl Iterator<Item = (usize, &[u8])> {
    let mut pos = 0;
    std::iter::from_fn(move || {
        let rest = input.get(pos..).filter(|rest| !rest.is_empty())?;
        let start = pos;
        let len = memchr::memchr(sep, rest).unwrap_or(rest.len());
        pos = start + len + 1;
        Some((start, &rest[..len]))
    })
}

/// Unsigned decimal without sign or whitespace; `None` on overflow or any other byte.
fn parse_uint<T: TryFrom<u64>>(digits: &[u8]) -> Option<T> {
    if digits.is_empty() {
        return None;
    }
    let n = digits.iter().try_fold(0u64, |n, &d| {
        if !d.is_ascii_digit() {
            return None;
        }
        n.checked_mul(10)?.checked_add(u64::from(d - b'0'))
    })?;
    T::try_from(n).ok()
}

/// Signed decimal with an optional leading `-`.
fn parse_i64(text: &[u8]) -> Option<i64> {
    match text.strip_prefix(b"-") {
        Some(digits) => {
            let n: u64 = parse_uint(digits)?;
            0i64.checked_sub_unsigned(n)
        }
        None => parse_uint(text),
    }
}

/// Full hex object id (SHA-1 or SHA-256).
fn parse_oid(hex: &[u8]) -> Option<String> {
    let valid = matches!(hex.len(), 40 | 64) && hex.iter().all(u8::is_ascii_hexdigit);
    valid.then(|| String::from_utf8_lossy(hex).into_owned())
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The same entry points as the cargo-fuzz targets in `fuzz/fuzz_targets`, fed garbage on stable:
/// none may panic, whatever the input.
#[cfg(test)]
mod garbage_tests {
    use super::{diff, log, refs, status};

    fn every_parser(data: &[u8]) {
        let _ = log::parse(data);
        let (first, second) = data.split_at(data.len() / 3);
        let mut parser = log::LogParser::new();
        let _ = parser.push(first);
        let _ = parser.push(second);
        let _ = parser.finish();
        let _ = log::co_authors(data);
        let _ = log::short_description(data, 72);
        let _ = diff::parse(data);
        let _ = diff::whole_files(data);
        let _ = status::parse(data);
        let _ = refs::parse_refs(data);
        let _ = refs::parse_stashes(data);
        let _ = refs::parse_remotes(data);
        let _ = refs::parse_ahead_behind(data);
    }

    #[test]
    fn edge_inputs_do_not_panic() {
        let long = vec![b'a'; 100_000];
        let inputs: [&[u8]; 12] = [
            b"",
            b"\0",
            b"\0\0\0",
            b"\n",
            b"\xff\xfe\x80\xc3",
            b"1 ",
            b"1 .M N... 100644 100644 100644 ",
            b"diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,99999999999999999999 +1 @@\n",
            b"diff --git a/x b/x\nBinary files a/x and b/x differ\n",
            b"@@ -0,0 +0,0 @@\n+\n",
            b"-1\t-1\t",
            &long,
        ];
        for input in inputs {
            every_parser(input);
        }
    }

    #[test]
    fn random_token_soup_does_not_panic() {
        const TOKENS: [&[u8]; 20] = [
            b"\0",
            b"\n",
            b"\t",
            b" ",
            b"diff --git a/f b/f\n",
            b"--- a/f\n",
            b"+++ b/f\n",
            b"@@ -1,2 +3,4 @@ ctx\n",
            b"\\ No newline at end of file\n",
            b"+added\n",
            b"-removed\n",
            b" context\n",
            b"1 .M N... 100644 100644 100644 ",
            b"2 R. ",
            b"? ",
            b"refs/heads/main",
            b"0123456789abcdef0123456789abcdef01234567",
            b"Co-authored-by: A <a@b>",
            b"99999999999999999999",
            b"\xff\xc3",
        ];
        // xorshift64: deterministic, so a failure reproduces.
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..2_000 {
            let mut input = Vec::new();
            for _ in 0..(next() % 24) {
                input.extend_from_slice(TOKENS[(next() % TOKENS.len() as u64) as usize]);
            }
            every_parser(&input);
        }
    }
}

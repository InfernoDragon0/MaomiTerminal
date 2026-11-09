//! Parsers return `Ok` or `Err` on any input, never panic. Dependency-free pseudo-fuzzing over
//! real fixtures and token soup; the cargo-fuzz targets in `fuzz/` go deeper.

use maomi_git::parse::{diff, log, refs, status};

/// xorshift64*: deterministic, so a failure reproduces.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

const TOKENS: &[&[u8]] = &[
    b"\0",
    b"\n",
    b"\r\n",
    b"\x1f",
    b" ",
    b"\t",
    b"\"",
    b"\\",
    b"\\303",
    b"0",
    b"1",
    b"9",
    b"-",
    b"+",
    b",",
    b"@@ -",
    b" +",
    b" @@",
    b"@@ -1 +1 @@\n",
    b"diff --git a/x b/x\n",
    b"diff --cc x\n",
    b"--- a/x\n",
    b"+++ b/x\n",
    b"--- /dev/null\n",
    b"new file mode 100644\n",
    b"old mode 100644\n",
    b"rename from \"a\\tb\"\n",
    b"similarity index 90%\n",
    b"\\ No newline at end of file\n",
    b"Binary files a/x and b/x differ\n",
    b"# branch.ab +1 -2",
    b"# branch.oid ",
    b"# stash ",
    b"1 .M N... ",
    b"2 R. N... ",
    b"u UU N... ",
    b"? ",
    b"! ",
    b"R100 ",
    b"100644 ",
    b"4294967296",
    b"99999999999999999999",
    b"HEAD -> refs/heads/x",
    b"tag: refs/tags/v",
    b", ",
    b"stash@{",
    b"}",
    b"ahead 1, behind 2",
    b"gone",
    b" (fetch)",
    b" (push)",
    b"0123456789abcdef0123456789abcdef01234567",
    b"1696766400 +0200",
    "é".as_bytes(),
    b"\xff",
];

const SEEDS: &[&[u8]] = &[
    include_bytes!("fixtures/status-work.bin"),
    include_bytes!("fixtures/status-ignored.bin"),
    include_bytes!("fixtures/status-conflict.bin"),
    include_bytes!("fixtures/status-tracking.bin"),
    include_bytes!("fixtures/log-all.bin"),
    include_bytes!("fixtures/log-detached.bin"),
    include_bytes!("fixtures/diff-worktree.bin"),
    include_bytes!("fixtures/diff-cached.bin"),
    include_bytes!("fixtures/diff-quoted.bin"),
    include_bytes!("fixtures/diff-conflict.bin"),
    include_bytes!("fixtures/diff-empty.bin"),
    include_bytes!("fixtures/refs.bin"),
    include_bytes!("fixtures/stashes.bin"),
    include_bytes!("fixtures/remotes.bin"),
    include_bytes!("fixtures/ahead-behind.bin"),
];

fn mutate(rng: &mut Rng, input: &mut Vec<u8>) {
    for _ in 0..=rng.below(4) {
        let at = rng.below(input.len() + 1);
        match rng.below(6) {
            0 => input.truncate(at),
            1 if at < input.len() => input[at] = rng.next() as u8,
            2 => input.insert(at, *b"\0\n\x1f \"\\@-+".get(rng.below(10)).unwrap_or(&0)),
            3 => {
                let end = (at + rng.below(64)).min(input.len());
                input.drain(at..end);
            }
            4 => {
                let token = TOKENS[rng.below(TOKENS.len())];
                input.splice(at..at, token.iter().copied());
            }
            _ => {
                let end = (at + rng.below(256)).min(input.len());
                let copy = input[at..end].to_vec();
                let to = rng.below(input.len() + 1);
                input.splice(to..to, copy);
            }
        }
    }
}

/// Feeds every truncation of every seed, then `iterations` mutated seeds and token soups.
fn hammer(seed: u64, iterations: usize, mut parse: impl FnMut(&[u8])) {
    for input in SEEDS {
        for len in 0..=input.len() {
            parse(&input[..len]);
        }
    }
    let mut rng = Rng(seed);
    for _ in 0..iterations {
        let input = if rng.below(3) == 0 {
            let mut soup = Vec::new();
            for _ in 0..rng.below(64) {
                soup.extend_from_slice(TOKENS[rng.below(TOKENS.len())]);
            }
            soup
        } else {
            let mut input = SEEDS[rng.below(SEEDS.len())].to_vec();
            mutate(&mut rng, &mut input);
            input
        };
        parse(&input);
    }
}

#[test]
fn status_never_panics() {
    hammer(0x5eed_0001, 20_000, |input| {
        let _ = status::parse(input);
    });
}

#[test]
fn log_never_panics() {
    let mut rng = Rng(0x5eed_0002);
    hammer(0x5eed_0003, 20_000, |input| {
        let whole = log::parse(input);
        let mut parser = log::LogParser::new();
        let mut commits = Vec::new();
        let mut rest = input;
        while !rest.is_empty() {
            let (chunk, tail) = rest.split_at(rng.below(rest.len()) + 1);
            match parser.push(chunk) {
                Ok(batch) => commits.extend(batch),
                Err(_) => return assert!(whole.is_err()),
            }
            rest = tail;
        }
        match parser.finish() {
            Ok(batch) => {
                commits.extend(batch);
                assert_eq!(Ok(commits), whole);
            }
            Err(_) => assert!(whole.is_err()),
        }
    });
}

#[test]
fn diff_never_panics() {
    hammer(0x5eed_0004, 20_000, |input| {
        let _ = diff::parse(input);
    });
}

#[test]
fn refs_never_panic() {
    hammer(0x5eed_0005, 20_000, |input| {
        let _ = refs::parse_refs(input);
        let _ = refs::parse_stashes(input);
        let _ = refs::parse_remotes(input);
        let _ = refs::parse_ahead_behind(input);
    });
}

//! Conflict markers in a worktree file: `merge` style (`<<<<<<<` / `=======` / `>>>>>>>`) and
//! `diff3` / `zdiff3` (with a `|||||||` base section). Lines keep their own endings, so a
//! resolved file is byte-identical outside the blocks. Broken or unterminated markers stay
//! plain text.

/// Default `conflict-marker-size`.
pub const MARKER_SIZE: usize = 7;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// Text both sides agree on.
    Common(Vec<u8>),
    Conflict(Block),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub ours: Vec<u8>,
    /// Only with `diff3` / `zdiff3` markers.
    pub base: Option<Vec<u8>>,
    pub theirs: Vec<u8>,
    /// Text after the markers, e.g. `HEAD`, `feature`, `parent of 1a2b3c (msg)`.
    pub ours_label: String,
    pub base_label: Option<String>,
    pub theirs_label: String,
}

/// How one block is resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    Ours,
    Theirs,
    /// Ours, then theirs.
    Both,
    /// Theirs, then ours.
    BothReversed,
    Base,
    /// Hand-written text.
    Custom(Vec<u8>),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConflictText {
    pub segments: Vec<Segment>,
}

impl ConflictText {
    pub fn parse(text: &[u8]) -> Self {
        parse_with(text, MARKER_SIZE)
    }

    pub fn blocks(&self) -> impl Iterator<Item = &Block> {
        self.segments.iter().filter_map(|s| match s {
            Segment::Conflict(b) => Some(b),
            Segment::Common(_) => None,
        })
    }

    pub fn conflict_count(&self) -> usize {
        self.blocks().count()
    }

    /// The file with `choices[i]` applied to the i-th block; blocks without a choice keep
    /// their markers.
    pub fn resolve(&self, choices: &[Option<Choice>]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut i = 0;
        for segment in &self.segments {
            match segment {
                Segment::Common(text) => out.extend_from_slice(text),
                Segment::Conflict(block) => {
                    match choices.get(i).cloned().flatten() {
                        Some(choice) => block.apply(&choice, &mut out),
                        None => block.write_markers(&mut out),
                    }
                    i += 1;
                }
            }
        }
        out
    }
}

impl Block {
    fn apply(&self, choice: &Choice, out: &mut Vec<u8>) {
        match choice {
            Choice::Ours => out.extend_from_slice(&self.ours),
            Choice::Theirs => out.extend_from_slice(&self.theirs),
            Choice::Both => join(out, &self.ours, &self.theirs),
            Choice::BothReversed => join(out, &self.theirs, &self.ours),
            Choice::Base => out.extend_from_slice(self.base.as_deref().unwrap_or_default()),
            Choice::Custom(text) => out.extend_from_slice(text),
        }
    }

    fn write_markers(&self, out: &mut Vec<u8>) {
        let eol: &[u8] = if [&self.ours, &self.theirs]
            .iter()
            .any(|t| t.ends_with(b"\r\n"))
        {
            b"\r\n"
        } else {
            b"\n"
        };
        let marker = |out: &mut Vec<u8>, c: u8, label: Option<&str>| {
            out.extend(std::iter::repeat_n(c, MARKER_SIZE));
            if let Some(label) = label.filter(|l| !l.is_empty()) {
                out.push(b' ');
                out.extend_from_slice(label.as_bytes());
            }
            out.extend_from_slice(eol);
        };
        marker(out, b'<', Some(&self.ours_label));
        out.extend_from_slice(&self.ours);
        if let Some(base) = &self.base {
            marker(out, b'|', self.base_label.as_deref());
            out.extend_from_slice(base);
        }
        marker(out, b'=', None);
        out.extend_from_slice(&self.theirs);
        marker(out, b'>', Some(&self.theirs_label));
    }
}

/// `a` then `b`, with a line break between them when `a` lacks one (last line of a file).
fn join(out: &mut Vec<u8>, a: &[u8], b: &[u8]) {
    out.extend_from_slice(a);
    if !a.is_empty() && !b.is_empty() && !a.ends_with(b"\n") {
        out.push(b'\n');
    }
    out.extend_from_slice(b);
}

/// Does `text` still contain a conflict block?
pub fn has_markers(text: &[u8]) -> bool {
    ConflictText::parse(text).conflict_count() > 0
}

/// `line` is `size` × `c` followed by end of line or a space (and a label).
fn marker(line: &[u8], c: u8, size: usize) -> Option<&[u8]> {
    let rest = line.strip_prefix(&*vec![c; size])?;
    let body = trim_eol(rest);
    match body.first() {
        None => Some(body),
        Some(b' ') => Some(&body[1..]),
        Some(_) => None,
    }
}

fn trim_eol(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

fn label(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_string()
}

enum State {
    Outside,
    Ours,
    Base,
    Theirs,
}

pub fn parse_with(text: &[u8], size: usize) -> ConflictText {
    let mut segments = Vec::new();
    let mut common = Vec::new();
    // Raw text of the block being read, emitted as plain text if it never closes.
    let mut raw = Vec::new();
    let mut block = Block {
        ours: Vec::new(),
        base: None,
        theirs: Vec::new(),
        ours_label: String::new(),
        base_label: None,
        theirs_label: String::new(),
    };
    let mut state = State::Outside;
    for line in text.split_inclusive(|&b| b == b'\n') {
        match state {
            State::Outside => {
                if let Some(l) = marker(line, b'<', size) {
                    block.ours_label = label(l);
                    raw.extend_from_slice(line);
                    state = State::Ours;
                } else {
                    common.extend_from_slice(line);
                }
                continue;
            }
            State::Ours | State::Base | State::Theirs => raw.extend_from_slice(line),
        }
        match state {
            State::Ours => {
                if let Some(l) = marker(line, b'|', size) {
                    block.base = Some(Vec::new());
                    block.base_label = Some(label(l));
                    state = State::Base;
                } else if marker(line, b'=', size).is_some_and(<[u8]>::is_empty) {
                    state = State::Theirs;
                } else {
                    block.ours.extend_from_slice(line);
                }
            }
            State::Base => {
                if marker(line, b'=', size).is_some_and(<[u8]>::is_empty) {
                    state = State::Theirs;
                } else if let Some(base) = &mut block.base {
                    base.extend_from_slice(line);
                }
            }
            State::Theirs => {
                if let Some(l) = marker(line, b'>', size) {
                    block.theirs_label = label(l);
                    if !common.is_empty() {
                        segments.push(Segment::Common(std::mem::take(&mut common)));
                    }
                    let done = std::mem::replace(
                        &mut block,
                        Block {
                            ours: Vec::new(),
                            base: None,
                            theirs: Vec::new(),
                            ours_label: String::new(),
                            base_label: None,
                            theirs_label: String::new(),
                        },
                    );
                    segments.push(Segment::Conflict(done));
                    raw.clear();
                    state = State::Outside;
                } else {
                    block.theirs.extend_from_slice(line);
                }
            }
            State::Outside => unreachable!(),
        }
    }
    // Unterminated block: plain text after all.
    common.extend_from_slice(&raw);
    if !common.is_empty() {
        segments.push(Segment::Common(common));
    }
    ConflictText { segments }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MERGE: &[u8] = b"a\n<<<<<<< HEAD\nours 1\nours 2\n=======\ntheirs\n>>>>>>> feature\nz\n";
    const DIFF3: &[u8] = b"<<<<<<< ours\r\nmine\r\n||||||| base\r\norig\r\n=======\r\nyours\r\n>>>>>>> theirs\r\ntail";

    #[test]
    fn merge_style() {
        let text = ConflictText::parse(MERGE);
        assert_eq!(text.conflict_count(), 1);
        let block = text.blocks().next().unwrap();
        assert_eq!(block.ours, b"ours 1\nours 2\n");
        assert_eq!(block.theirs, b"theirs\n");
        assert_eq!(block.base, None);
        assert_eq!(
            (block.ours_label.as_str(), block.theirs_label.as_str()),
            ("HEAD", "feature")
        );
        assert_eq!(
            text.resolve(&[Some(Choice::Ours)]),
            b"a\nours 1\nours 2\nz\n"
        );
        assert_eq!(text.resolve(&[Some(Choice::Theirs)]), b"a\ntheirs\nz\n");
        assert_eq!(
            text.resolve(&[Some(Choice::BothReversed)]),
            b"a\ntheirs\nours 1\nours 2\nz\n"
        );
        assert_eq!(
            text.resolve(&[Some(Choice::Custom(b"x\n".to_vec()))]),
            b"a\nx\nz\n"
        );
        // Unresolved blocks are written back as they were.
        assert_eq!(text.resolve(&[]), MERGE);
    }

    #[test]
    fn diff3_crlf_and_no_final_newline() {
        let text = ConflictText::parse(DIFF3);
        let block = text.blocks().next().unwrap();
        assert_eq!(block.base.as_deref(), Some(&b"orig\r\n"[..]));
        assert_eq!(block.base_label.as_deref(), Some("base"));
        assert_eq!(text.resolve(&[Some(Choice::Base)]), b"orig\r\ntail");
        assert_eq!(
            text.resolve(&[Some(Choice::Both)]),
            b"mine\r\nyours\r\ntail"
        );
        assert_eq!(text.resolve(&[None]), DIFF3);
    }

    #[test]
    fn several_blocks_and_empty_sides() {
        let src = b"<<<<<<< a\n=======\nadded\n>>>>>>> b\nmid\n<<<<<<< a\nx\n=======\n>>>>>>> b\n";
        let text = ConflictText::parse(src);
        assert_eq!(text.conflict_count(), 2);
        assert_eq!(
            text.resolve(&[Some(Choice::Theirs), Some(Choice::Ours)]),
            b"added\nmid\nx\n"
        );
        assert_eq!(
            text.resolve(&[Some(Choice::Both), None]),
            b"added\nmid\n<<<<<<< a\nx\n=======\n>>>>>>> b\n"
        );
    }

    #[test]
    fn last_line_without_newline_joins_cleanly() {
        let mut out = Vec::new();
        join(&mut out, b"a", b"b");
        assert_eq!(out, b"a\nb");
    }

    #[test]
    fn not_markers() {
        for src in [
            &b"<<<<<<<< eight\n=======\n>>>>>>> b\n"[..],
            b"<<<<<<<x\n=======\n>>>>>>> b\n",
            b"<<<<<<< open\nnever closed\n=======\n",
            b"plain\n======= \n",
        ] {
            let text = ConflictText::parse(src);
            assert_eq!(text.conflict_count(), 0, "{}", String::from_utf8_lossy(src));
            assert_eq!(text.resolve(&[]), src, "round trip");
        }
        assert!(has_markers(MERGE));
        assert!(!has_markers(b"fine\n"));
    }
}

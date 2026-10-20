//! Picks shell-integration OSC sequences out of the raw PTY stream.
//!
//! alacritty_terminal drops OSC 7, 9;9, 133 and 633, so they are scanned here before the bytes
//! reach the parser. The bytes still go to the parser unchanged.

use std::path::PathBuf;

/// Longest OSC payload kept; longer ones (images, big OSC 52 copies) are skipped.
const MAX_PAYLOAD: usize = 4096;

/// A shell-integration mark found in the output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ShellMark {
    Cwd(PathBuf),
    Prompt(PromptMark),
}

/// FinalTerm / OSC 133 semantic prompt marks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptMark {
    /// `A`: the prompt starts drawing.
    PromptStart,
    /// `B`: the prompt is drawn; the user types the command.
    CommandStart,
    /// `C`: the command was submitted and runs.
    CommandExecuted,
    /// `D[;code]`: the command finished.
    CommandFinished(Option<i32>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    Esc,
    Osc,
    /// ESC seen inside an OSC: `\` ends it (ST), anything else aborts it.
    OscEsc,
    /// Payload over [`MAX_PAYLOAD`]: wait for the terminator without storing.
    Skip,
    SkipEsc,
}

#[derive(Debug)]
pub(crate) struct OscScanner {
    state: State,
    payload: Vec<u8>,
}

impl Default for OscScanner {
    fn default() -> Self {
        Self {
            state: State::Ground,
            payload: Vec::new(),
        }
    }
}

impl OscScanner {
    /// Scans `bytes` (any chunking) and appends the marks found to `out`.
    pub(crate) fn feed(&mut self, bytes: &[u8], out: &mut Vec<ShellMark>) {
        let mut i = 0;
        while i < bytes.len() {
            if self.state == State::Ground {
                // Most output has no escapes; jump to the next ESC.
                match bytes[i..].iter().position(|&b| b == 0x1b) {
                    Some(at) => i += at,
                    None => return,
                }
            }
            let b = bytes[i];
            i += 1;
            self.state = match self.state {
                State::Ground => State::Esc,
                State::Esc => match b {
                    b']' => {
                        self.payload.clear();
                        State::Osc
                    }
                    0x1b => State::Esc,
                    _ => State::Ground,
                },
                State::Osc => match b {
                    0x07 => {
                        self.finish(out);
                        State::Ground
                    }
                    0x1b => State::OscEsc,
                    // CAN / SUB cancel the sequence.
                    0x18 | 0x1a => State::Ground,
                    _ if self.payload.len() >= MAX_PAYLOAD => State::Skip,
                    _ => {
                        self.payload.push(b);
                        State::Osc
                    }
                },
                State::OscEsc => match b {
                    b'\\' => {
                        self.finish(out);
                        State::Ground
                    }
                    b']' => {
                        self.payload.clear();
                        State::Osc
                    }
                    0x1b => State::Esc,
                    _ => State::Ground,
                },
                State::Skip => match b {
                    0x07 | 0x18 | 0x1a => State::Ground,
                    0x1b => State::SkipEsc,
                    _ => State::Skip,
                },
                State::SkipEsc => match b {
                    b'\\' => State::Ground,
                    b']' => {
                        self.payload.clear();
                        State::Osc
                    }
                    0x1b => State::Esc,
                    _ => State::Ground,
                },
            };
        }
    }

    fn finish(&mut self, out: &mut Vec<ShellMark>) {
        if let Some(mark) = parse(&String::from_utf8_lossy(&self.payload)) {
            out.push(mark);
        }
        self.payload.clear();
    }
}

fn parse(payload: &str) -> Option<ShellMark> {
    let (code, rest) = payload.split_once(';')?;
    match code {
        "7" => parse_file_url(rest).map(ShellMark::Cwd),
        "9" => {
            let path = rest.strip_prefix("9;")?;
            let path = path.trim_matches('"');
            (!path.is_empty()).then(|| ShellMark::Cwd(PathBuf::from(path)))
        }
        "133" => parse_prompt_mark(rest).map(ShellMark::Prompt),
        // VS Code's variant: same A-D marks plus `P;Cwd=<path>`.
        "633" => {
            if let Some(cwd) = rest.strip_prefix("P;Cwd=") {
                return (!cwd.is_empty()).then(|| ShellMark::Cwd(PathBuf::from(cwd)));
            }
            parse_prompt_mark(rest).map(ShellMark::Prompt)
        }
        _ => None,
    }
}

fn parse_prompt_mark(rest: &str) -> Option<PromptMark> {
    let mut parts = rest.split(';');
    Some(match parts.next()? {
        "A" => PromptMark::PromptStart,
        "B" => PromptMark::CommandStart,
        "C" => PromptMark::CommandExecuted,
        "D" => PromptMark::CommandFinished(parts.next().and_then(|c| c.parse().ok())),
        _ => return None,
    })
}

/// `file://host/path` → path. The host is ignored; remote cwds are the SSH card's concern.
fn parse_file_url(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    let path = &rest[rest.find('/')?..];
    let path = percent_decode(path);
    // Windows shells send `/C:/Users/x`.
    let bytes = path.as_bytes();
    if bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
        return Some(PathBuf::from(&path[1..]));
    }
    Some(PathBuf::from(path))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = bytes.get(i + 1..i + 3)
            && let Ok(byte) = u8::from_str_radix(std::str::from_utf8(hex).unwrap_or("zz"), 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(chunks: &[&[u8]]) -> Vec<ShellMark> {
        let mut scanner = OscScanner::default();
        let mut out = Vec::new();
        for chunk in chunks {
            scanner.feed(chunk, &mut out);
        }
        out
    }

    #[test]
    fn osc7_with_bel_and_st() {
        let marks = scan(&[b"hi\x1b]7;file://box/home/me/my%20dir\x07 \x1b]7;file:///tmp\x1b\\"]);
        assert_eq!(
            marks,
            vec![
                ShellMark::Cwd(PathBuf::from("/home/me/my dir")),
                ShellMark::Cwd(PathBuf::from("/tmp")),
            ]
        );
    }

    #[test]
    fn osc7_windows_drive() {
        let marks = scan(&[b"\x1b]7;file://pc/C:/Users/me\x07"]);
        assert_eq!(marks, vec![ShellMark::Cwd(PathBuf::from("C:/Users/me"))]);
    }

    #[test]
    fn osc9_9_quoted() {
        let marks = scan(&[b"\x1b]9;9;\"C:\\Program Files\"\x07"]);
        assert_eq!(
            marks,
            vec![ShellMark::Cwd(PathBuf::from(r"C:\Program Files"))]
        );
    }

    #[test]
    fn split_across_chunks() {
        let marks = scan(&[b"out\x1b", b"]13", b"3;D;", b"2\x1b", b"\\more"]);
        assert_eq!(
            marks,
            vec![ShellMark::Prompt(PromptMark::CommandFinished(Some(2)))]
        );
    }

    #[test]
    fn prompt_marks() {
        let marks = scan(&[b"\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07\x1b]633;D\x07"]);
        assert_eq!(
            marks,
            vec![
                ShellMark::Prompt(PromptMark::PromptStart),
                ShellMark::Prompt(PromptMark::CommandStart),
                ShellMark::Prompt(PromptMark::CommandExecuted),
                ShellMark::Prompt(PromptMark::CommandFinished(None)),
            ]
        );
    }

    #[test]
    fn ignores_other_sequences_and_oversized_payloads() {
        let big = format!("\x1b]52;c;{}\x07", "A".repeat(MAX_PAYLOAD * 2));
        let marks = scan(&[
            b"\x1b[31mred\x1b[0m\x1b]0;title\x07",
            big.as_bytes(),
            b"\x1b]133;A\x07",
        ]);
        assert_eq!(marks, vec![ShellMark::Prompt(PromptMark::PromptStart)]);
    }
}

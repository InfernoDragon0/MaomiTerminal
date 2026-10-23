//! Picks shell-integration OSC sequences and the win32-input-mode switch out of the raw PTY stream.
//!
//! alacritty_terminal drops OSC 7, 9;9, 133 and 633 and does not know `CSI ? 9001 h/l` (ConPTY's
//! request for win32-input-mode keys), so they are scanned here before the bytes reach the
//! parser. The bytes still go to the parser unchanged.

use std::path::PathBuf;

/// Longest OSC payload kept; longer ones (images, big OSC 52 copies) are skipped.
const MAX_PAYLOAD: usize = 4096;
/// Longest CSI parameter list kept; the sequences wanted here are far shorter.
const MAX_CSI: usize = 32;

/// A mark found in the output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ShellMark {
    Cwd(PathBuf),
    Prompt(PromptMark),
    /// `CSI ? 9001 h` (true) or `l` (false): the program wants win32-input-mode keys.
    Win32InputMode(bool),
    /// A desktop notification (`OSC 9 ; text` or `OSC 777 ; notify ; title ; body`), e.g. an
    /// agent asking for permission.
    Notify(String),
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
    /// Inside `ESC [`: parameter and intermediate bytes go to `payload` until the final byte.
    Csi,
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
                    b'[' => {
                        self.payload.clear();
                        State::Csi
                    }
                    0x1b => State::Esc,
                    _ => State::Ground,
                },
                State::Csi => match b {
                    0x20..=0x3f if self.payload.len() < MAX_CSI => {
                        self.payload.push(b);
                        State::Csi
                    }
                    0x40..=0x7e => {
                        match (self.payload.as_slice(), b) {
                            (b"?9001", b'h') => out.push(ShellMark::Win32InputMode(true)),
                            (b"?9001", b'l') => out.push(ShellMark::Win32InputMode(false)),
                            _ => {}
                        }
                        self.payload.clear();
                        State::Ground
                    }
                    0x1b => State::Esc,
                    // Controls inside a CSI are rare; dropping the sequence is safe.
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
            if let Some(path) = rest.strip_prefix("9;") {
                let path = path.trim_matches('"');
                return (!path.is_empty()).then(|| ShellMark::Cwd(PathBuf::from(path)));
            }
            // ConEmu's other OSC 9 commands start with a number (`4;1;50` is progress); any
            // other text is iTerm2's notification.
            let command = rest.split(';').next().unwrap_or_default();
            let is_conemu = !command.is_empty() && command.bytes().all(|b| b.is_ascii_digit());
            (!is_conemu && !rest.trim().is_empty()).then(|| ShellMark::Notify(rest.to_owned()))
        }
        "777" => {
            let body = rest.strip_prefix("notify;")?;
            Some(ShellMark::Notify(body.replacen(';', ": ", 1)))
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
    if cfg!(windows)
        && let Some(path) = msys_drive_path(&path)
    {
        return Some(path);
    }
    Some(PathBuf::from(path))
}

/// Git Bash (MSYS) reports `/c/Users/x` for `C:\Users\x`.
fn msys_drive_path(path: &str) -> Option<PathBuf> {
    let rest = path.strip_prefix('/')?;
    let mut chars = rest.chars();
    let drive = chars.next().filter(char::is_ascii_alphabetic)?;
    let tail = chars.as_str();
    if !(tail.is_empty() || tail.starts_with('/')) {
        return None;
    }
    let tail = if tail.is_empty() { "/" } else { tail };
    Some(PathBuf::from(format!(
        "{}:{tail}",
        drive.to_ascii_uppercase()
    )))
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
    fn notifications() {
        let marks = scan(&[
            b"\x1b]9;Claude needs your permission to use Bash\x07",
            b"\x1b]9;4;1;50\x07\x1b]777;notify;Claude;Waiting\x1b\\",
        ]);
        assert_eq!(
            marks,
            vec![
                ShellMark::Notify("Claude needs your permission to use Bash".into()),
                ShellMark::Notify("Claude: Waiting".into()),
            ]
        );
    }

    #[test]
    fn msys_drive_paths() {
        assert_eq!(
            msys_drive_path("/c/Users/me"),
            Some(PathBuf::from("C:/Users/me"))
        );
        assert_eq!(msys_drive_path("/d"), Some(PathBuf::from("D:/")));
        assert_eq!(msys_drive_path("/home/me"), None);
        assert_eq!(msys_drive_path("/tmp"), None);
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
    fn win32_input_mode_set_and_reset() {
        let marks = scan(&[b"a\x1b[?9001hb\x1b[?1004h\x1b[31m\x1b[?9001lc"]);
        assert_eq!(
            marks,
            vec![
                ShellMark::Win32InputMode(true),
                ShellMark::Win32InputMode(false)
            ]
        );
    }

    #[test]
    fn win32_input_mode_split_across_chunks() {
        let marks = scan(&[b"x\x1b", b"[?90", b"01", b"h", b"\x1b[?9001", b"l"]);
        assert_eq!(
            marks,
            vec![
                ShellMark::Win32InputMode(true),
                ShellMark::Win32InputMode(false)
            ]
        );
    }

    #[test]
    fn similar_csi_sequences_are_ignored() {
        let marks = scan(&[b"\x1b[9001h\x1b[?90011h\x1b[?9001;1h\x1b[?9001m\x1b[?9001\x18h"]);
        assert_eq!(marks, vec![]);
    }

    #[test]
    fn csi_does_not_break_osc_scanning() {
        let marks = scan(&[b"\x1b[?9001h\x1b[2J\x1b]133;A\x07"]);
        assert_eq!(
            marks,
            vec![
                ShellMark::Win32InputMode(true),
                ShellMark::Prompt(PromptMark::PromptStart)
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

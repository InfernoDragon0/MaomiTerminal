//! Kitty graphics protocol commands (`ESC _ G <keys> ; <payload> ESC \`), picked out of the raw
//! PTY stream.
//!
//! vte ignores APC strings, so they are scanned here before the bytes reach the parser, the same
//! way `osc.rs` picks out shell-integration OSCs. Chunked transfers (`m=1` … `m=0`) are joined
//! into one command. The payload stays base64 text; the image store decodes it.

/// Longest key list kept (`a=T,U=1,f=100,…`); real ones are well under 100 bytes.
const MAX_KEYS: usize = 512;
/// Most base64 bytes kept for one command, all chunks together. A larger image is cut off and
/// flagged `truncated`, so the store can refuse it without holding the rest.
pub(crate) const MAX_PAYLOAD: usize = 48 << 20;

/// One graphics command, chunks joined. Keys the store does not use are dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KittyCommand {
    /// `a`: `t` transmit, `T` transmit and display, `q` query, `p` place, `d` delete, …
    pub action: u8,
    /// `t`: `d` direct (base64 in the payload), `f` file, `t` temp file, `s` shared memory.
    pub medium: u8,
    /// `f`: 24 RGB, 32 RGBA, 100 PNG.
    pub format: u32,
    /// `o`: `z` when the data is zlib-compressed.
    pub compression: Option<u8>,
    /// `s`, `v`: pixel width and height for raw RGB/RGBA.
    pub width: u32,
    pub height: u32,
    /// `S`, `O`: bytes to read from a file and the offset to start at.
    pub size: u32,
    pub offset: u32,
    /// `i`: image id, 0 when absent.
    pub id: u32,
    /// `I`: image number (the terminal picks the id), 0 when absent.
    pub number: u32,
    /// `p`: placement id.
    pub placement: u32,
    /// `U=1`: a virtual placement shown through Unicode placeholder cells.
    pub virtual_placement: bool,
    /// `c`, `r`: the placement's size in cells.
    pub columns: u32,
    pub rows: u32,
    /// `q`: 0 answers everything, 1 errors only, 2 nothing.
    pub quiet: u8,
    /// `d`: what a delete targets (`a`, `i`, `I`, …; upper case also frees the data).
    pub delete: u8,
    /// Base64 text (or a base64 file path for `t=f`), all chunks joined.
    pub payload: Vec<u8>,
    /// The payload passed [`MAX_PAYLOAD`] and was cut off.
    pub truncated: bool,
}

impl Default for KittyCommand {
    fn default() -> Self {
        Self {
            action: b't',
            medium: b'd',
            format: 32,
            compression: None,
            width: 0,
            height: 0,
            size: 0,
            offset: 0,
            id: 0,
            number: 0,
            placement: 0,
            virtual_placement: false,
            columns: 0,
            rows: 0,
            quiet: 0,
            delete: b'a',
            payload: Vec::new(),
            truncated: false,
        }
    }
}

/// Keys of one escape sequence, before chunks are joined.
struct Parsed {
    command: KittyCommand,
    /// `m=1`: more chunks follow.
    more: bool,
    /// The sequence named `a` or `i`, so it starts a new command rather than continuing one.
    starts_new: bool,
}

fn parse_keys(keys: &[u8]) -> Parsed {
    let mut command = KittyCommand::default();
    let mut more = false;
    let mut starts_new = false;
    for pair in keys.split(|&b| b == b',') {
        let [key, b'=', value @ ..] = pair else {
            continue;
        };
        let first = value.first().copied().unwrap_or_default();
        let number = || std::str::from_utf8(value).ok()?.parse::<u32>().ok();
        match key {
            b'a' => {
                command.action = first;
                starts_new = true;
            }
            b't' => command.medium = first,
            b'o' => command.compression = Some(first),
            b'd' => command.delete = first,
            b'm' => more = value == b"1",
            b'U' => command.virtual_placement = value == b"1",
            b'q' => command.quiet = number().map_or(0, |q| q.min(2) as u8),
            _ => {
                let Some(n) = number() else { continue };
                match key {
                    b'f' => command.format = n,
                    b's' => command.width = n,
                    b'v' => command.height = n,
                    b'S' => command.size = n,
                    b'O' => command.offset = n,
                    b'i' => {
                        command.id = n;
                        starts_new = true;
                    }
                    b'I' => command.number = n,
                    b'p' => command.placement = n,
                    b'c' => command.columns = n,
                    b'r' => command.rows = n,
                    _ => {}
                }
            }
        }
    }
    Parsed {
        command,
        more,
        starts_new,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    #[default]
    Ground,
    Esc,
    /// `ESC _` seen: `G` makes it a graphics command; anything else is another APC to skip.
    ApcStart,
    Keys,
    Payload,
    /// ESC inside the string: `\` ends it (ST), anything else aborts it.
    StringEsc,
    /// An APC that is not graphics, or keys too long: wait for ST without storing.
    Skip,
    SkipEsc,
}

#[derive(Debug, Default)]
pub(crate) struct KittyScanner {
    state: State,
    keys: Vec<u8>,
    payload: Vec<u8>,
    /// This sequence's payload passed the cap.
    overflow: bool,
    /// A chunked transfer waiting for its `m=0` chunk.
    pending: Option<KittyCommand>,
}

impl KittyScanner {
    /// Scans `bytes` (any chunking) and appends the finished commands to `out`.
    pub(crate) fn feed(&mut self, bytes: &[u8], out: &mut Vec<KittyCommand>) {
        let mut i = 0;
        while i < bytes.len() {
            match self.state {
                // Most output has no escapes; jump to the next ESC.
                State::Ground => match bytes[i..].iter().position(|&b| b == 0x1b) {
                    Some(at) => i += at,
                    None => return,
                },
                // Copy payload runs in one go: images are megabytes of base64.
                State::Payload => {
                    let run = bytes[i..]
                        .iter()
                        .position(|&b| matches!(b, 0x1b | 0x18 | 0x1a))
                        .unwrap_or(bytes.len() - i);
                    self.push_payload(&bytes[i..i + run]);
                    i += run;
                    if i == bytes.len() {
                        return;
                    }
                }
                _ => {}
            }
            let b = bytes[i];
            i += 1;
            self.state = match self.state {
                State::Ground => State::Esc,
                State::Esc => match b {
                    b'_' => State::ApcStart,
                    0x1b => State::Esc,
                    _ => State::Ground,
                },
                State::ApcStart => match b {
                    b'G' => {
                        self.keys.clear();
                        self.payload.clear();
                        self.overflow = false;
                        State::Keys
                    }
                    0x1b => State::SkipEsc,
                    _ => State::Skip,
                },
                State::Keys => match b {
                    b';' => State::Payload,
                    0x1b => State::StringEsc,
                    0x18 | 0x1a => State::Ground,
                    _ if self.keys.len() >= MAX_KEYS => State::Skip,
                    _ => {
                        self.keys.push(b);
                        State::Keys
                    }
                },
                State::Payload => match b {
                    0x1b => State::StringEsc,
                    // CAN / SUB cancel the sequence.
                    _ => State::Ground,
                },
                State::StringEsc => match b {
                    b'\\' => {
                        self.finish(out);
                        State::Ground
                    }
                    b'_' => State::ApcStart,
                    0x1b => State::Esc,
                    _ => State::Ground,
                },
                State::Skip => match b {
                    0x18 | 0x1a => State::Ground,
                    0x1b => State::SkipEsc,
                    _ => State::Skip,
                },
                State::SkipEsc => match b {
                    b'\\' => State::Ground,
                    b'_' => State::ApcStart,
                    0x1b => State::Esc,
                    _ => State::Ground,
                },
            };
        }
    }

    fn push_payload(&mut self, run: &[u8]) {
        let held = self.payload.len() + self.pending.as_ref().map_or(0, |p| p.payload.len());
        let room = MAX_PAYLOAD.saturating_sub(held);
        if run.len() > room {
            self.overflow = true;
        }
        self.payload.extend_from_slice(&run[..run.len().min(room)]);
    }

    fn finish(&mut self, out: &mut Vec<KittyCommand>) {
        let parsed = parse_keys(&self.keys);
        let payload = std::mem::take(&mut self.payload);
        let overflow = std::mem::take(&mut self.overflow);
        // A continuation chunk carries only `m` (and maybe `q`); one naming `a` or `i` starts
        // over, dropping a transfer the program never finished.
        let mut command = match self.pending.take() {
            Some(pending) if !parsed.starts_new => pending,
            _ => parsed.command,
        };
        command.payload.extend_from_slice(&payload);
        command.truncated |= overflow;
        if parsed.more {
            self.pending = Some(command);
        } else {
            out.push(command);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(chunks: &[&[u8]]) -> Vec<KittyCommand> {
        let mut scanner = KittyScanner::default();
        let mut out = Vec::new();
        for chunk in chunks {
            scanner.feed(chunk, &mut out);
        }
        out
    }

    #[test]
    fn claude_code_transmit_with_placeholders() {
        let out = scan(&[b"x\x1b_Ga=T,U=1,q=2,f=100,i=31,c=40,r=12;iVBORw0KGgo=\x1b\\y"]);
        assert_eq!(
            out,
            vec![KittyCommand {
                action: b'T',
                format: 100,
                id: 31,
                virtual_placement: true,
                columns: 40,
                rows: 12,
                quiet: 2,
                payload: b"iVBORw0KGgo=".to_vec(),
                ..KittyCommand::default()
            }]
        );
    }

    #[test]
    fn query_and_delete() {
        let out = scan(&[b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b_Ga=d,d=I,i=7,q=2\x1b\\"]);
        assert_eq!(out.len(), 2);
        assert_eq!(
            (out[0].action, out[0].format, out[0].width, out[0].height),
            (b'q', 24, 1, 1)
        );
        assert_eq!((out[1].action, out[1].delete, out[1].id), (b'd', b'I', 7));
        assert!(out[1].payload.is_empty());
    }

    #[test]
    fn chunks_join_across_sequences_and_reads() {
        let out = scan(&[
            b"\x1b_Ga=T,U=1,f=100,i=5,m=1;AAAA",
            b"BBBB\x1b",
            b"\\\x1b_Gm=1;CC",
            b"CC\x1b\\text\x1b_Gm=0,q=2;DDDD\x1b\\",
        ]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, 5);
        assert_eq!(out[0].payload, b"AAAABBBBCCCCDDDD");
        assert!(!out[0].truncated);
    }

    #[test]
    fn new_command_drops_an_unfinished_transfer() {
        let out = scan(&[b"\x1b_Gi=1,m=1;AAAA\x1b\\\x1b_Ga=d,d=I,i=2\x1b\\"]);
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].action, out[0].id), (b'd', 2));
    }

    #[test]
    fn other_apcs_and_escapes_are_ignored() {
        let out = scan(&[b"\x1b_Xnot graphics\x1b\\\x1b[31m\x1b]0;t\x07\x1b_Ga=q,i=3;\x1b\\"]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, 3);
    }

    #[test]
    fn cancelled_sequence_is_dropped() {
        let out = scan(&[b"\x1b_Gi=1;AA\x18AA\x1b\\\x1b_Gi=2;\x1b\\"]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, 2);
    }

    #[test]
    fn oversized_payload_is_truncated() {
        let big = vec![b'A'; MAX_PAYLOAD + 10];
        let mut seq = b"\x1b_Gi=9;".to_vec();
        seq.extend_from_slice(&big);
        seq.extend_from_slice(b"\x1b\\");
        let out = scan(&[&seq]);
        assert_eq!(out.len(), 1);
        assert!(out[0].truncated);
        assert_eq!(out[0].payload.len(), MAX_PAYLOAD);
    }
}

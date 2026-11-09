//! git's `--progress` lines on stderr (`Receiving objects:  45% (450/1000), 1.2 MiB | ...`).

/// One progress update. `percent` / `total` are absent for counting phases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    /// `Receiving objects`, `Resolving deltas`, `remote: Counting objects`, ...
    pub phase: String,
    pub percent: Option<u8>,
    pub done: Option<u64>,
    pub total: Option<u64>,
    /// The raw line, trimmed.
    pub line: String,
}

/// Parses one line (git separates updates with `\r` and finished phases with `\n`).
pub fn parse_line(line: &str) -> Option<Progress> {
    let line = line.trim();
    let (phase, rest) = line.rsplit_once(": ")?;
    let rest = rest.trim_start();
    let (percent, rest) = match rest.split_once('%') {
        Some((p, rest)) => (p.trim().parse::<u8>().ok().filter(|p| *p <= 100), rest),
        None => (None, rest),
    };
    // `(450/1000)` after a percentage, or a bare count `100` / `100, done.` while counting.
    let (done, total) = match rest.split_once('(').and_then(|(_, r)| r.split_once(')')) {
        Some((counts, _)) => {
            let (d, t) = counts.split_once('/')?;
            (d.trim().parse().ok(), t.trim().parse().ok())
        }
        None if percent.is_none() => {
            let count = rest.split(|c: char| !c.is_ascii_digit()).next()?;
            (Some(count.parse().ok()?), None)
        }
        None => (None, None),
    };
    Some(Progress {
        phase: phase.to_string(),
        percent,
        done,
        total,
        line: line.to_string(),
    })
}

/// Splits raw stderr into lines on `\r` and `\n`, keeping an incomplete tail between calls.
#[derive(Debug, Default)]
pub(crate) struct LineSplitter {
    partial: Vec<u8>,
}

impl LineSplitter {
    pub fn push(&mut self, chunk: &[u8], mut on_line: impl FnMut(&str)) {
        for &b in chunk {
            if b == b'\r' || b == b'\n' {
                if !self.partial.is_empty() {
                    on_line(&String::from_utf8_lossy(&self.partial));
                    self.partial.clear();
                }
            } else if self.partial.len() < 4096 {
                self.partial.push(b);
            }
        }
    }

    pub fn finish(&mut self, on_line: impl FnMut(&str)) {
        self.push(b"\n", on_line);
    }
}

/// One `GIT_LFS_PROGRESS` line (`download 3/12 1048576/2097152 path`) as `LFS download 3/12 · name 50%`.
pub fn describe_lfs(line: &str) -> Option<String> {
    let mut parts = line.trim().splitn(4, ' ');
    let direction = parts.next()?;
    let files = parts.next()?;
    let bytes = parts.next()?;
    let name = parts.next().unwrap_or_default();
    let name = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let (done, total) = bytes.split_once('/')?;
    let (done, total): (u64, u64) = (done.parse().ok()?, total.parse().ok()?);
    let percent = (done * 100).checked_div(total).unwrap_or(100);
    Some(format!("LFS {direction} {files} \u{b7} {name} {percent}%"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lfs_lines() {
        assert_eq!(
            describe_lfs("download 3/12 1048576/2097152 Content/Maps/Map.umap\n").as_deref(),
            Some("LFS download 3/12 \u{b7} Map.umap 50%")
        );
        assert_eq!(
            describe_lfs("upload 1/1 0/0 a.bin").as_deref(),
            Some("LFS upload 1/1 \u{b7} a.bin 100%")
        );
        assert_eq!(describe_lfs("garbage"), None);
    }

    #[test]
    fn parses_progress_lines() {
        let p = parse_line("Receiving objects:  45% (450/1000), 1.20 MiB | 2.00 MiB/s").unwrap();
        assert_eq!(
            (p.phase.as_str(), p.percent, p.done, p.total),
            ("Receiving objects", Some(45), Some(450), Some(1000))
        );
        let p = parse_line("remote: Enumerating objects: 1234, done.").unwrap();
        assert_eq!(
            (p.phase.as_str(), p.percent, p.done, p.total),
            ("remote: Enumerating objects", None, Some(1234), None)
        );
        let p = parse_line("Resolving deltas: 100% (5/5), done.").unwrap();
        assert_eq!((p.percent, p.done, p.total), (Some(100), Some(5), Some(5)));
        assert!(parse_line("Cloning into 'x'...").is_none());
        assert!(parse_line("warning: something odd").is_none());
    }

    #[test]
    fn splits_on_cr_and_lf_across_chunks() {
        let mut lines = Vec::new();
        let mut s = LineSplitter::default();
        s.push(b"Rec 1%\rRec", |l| lines.push(l.to_string()));
        s.push(b" 2%\r\nDone", |l| lines.push(l.to_string()));
        s.finish(|l| lines.push(l.to_string()));
        assert_eq!(lines, ["Rec 1%", "Rec 2%", "Done"]);
    }
}

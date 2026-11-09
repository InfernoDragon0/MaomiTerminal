//! Secret redaction for everything shown or logged: command lines, stderr tails, errors.
//!
//! Two layers: URL userinfo (`https://user:token@host` -> `https://***@host`) is always hidden,
//! and every secret registered with [`register`] (tokens loaded from the keychain) is replaced
//! wherever it appears. The registry is process-wide; secrets are wiped on [`unregister`] and
//! never logged.

use std::borrow::Cow;
use std::sync::{Mutex, MutexGuard, OnceLock};

use zeroize::Zeroizing;

/// What a hidden value becomes.
pub const MASK: &str = "\u{2022}\u{2022}\u{2022}\u{2022}";

/// Secrets shorter than this are not registered (they would mask ordinary words).
const MIN_SECRET_LEN: usize = 8;

fn registry() -> MutexGuard<'static, Vec<Zeroizing<String>>> {
    static SECRETS: OnceLock<Mutex<Vec<Zeroizing<String>>>> = OnceLock::new();
    SECRETS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Hides `secret` from all later [`redact`] output.
pub fn register(secret: &str) {
    if secret.len() < MIN_SECRET_LEN {
        return;
    }
    let mut secrets = registry();
    if !secrets.iter().any(|s| s.as_str() == secret) {
        secrets.push(Zeroizing::new(secret.to_string()));
        // Longest first, so a secret containing another is masked whole.
        secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    }
}

/// Forgets `secret` (sign out). The stored copy is zeroed.
pub fn unregister(secret: &str) {
    registry().retain(|s| s.as_str() != secret);
}

/// `text` with URL userinfo and registered secrets masked.
pub fn redact(text: &str) -> Cow<'_, str> {
    let mut out = Cow::Borrowed(text);
    if let Some(masked) = mask_userinfo(&out) {
        out = Cow::Owned(masked);
    }
    for secret in registry().iter() {
        if out.contains(secret.as_str()) {
            out = Cow::Owned(out.replace(secret.as_str(), MASK));
        }
    }
    out
}

/// Masks `user[:password]@` after every `scheme://` (no allocation when there is none).
fn mask_userinfo(text: &str) -> Option<String> {
    let mut out: Option<String> = None;
    let mut done = 0;
    let mut search = 0;
    while let Some(found) = text[search..].find("://") {
        let auth_start = search + found + 3;
        search = auth_start;
        let rest = &text[auth_start..];
        // The authority ends at the first `/`, `?`, `#` or whitespace.
        let end = rest
            .find(|c: char| {
                matches!(c, '/' | '?' | '#') || c.is_whitespace() || c == '"' || c == '\''
            })
            .unwrap_or(rest.len());
        let Some(at) = rest[..end].rfind('@') else {
            continue;
        };
        let buf = out.get_or_insert_with(String::new);
        buf.push_str(&text[done..auth_start]);
        buf.push_str("***");
        done = auth_start + at;
        search = done;
    }
    out.map(|mut buf| {
        buf.push_str(&text[done..]);
        buf
    })
}

/// An [`io::Write`](std::io::Write) that redacts whole lines before passing them on: the log
/// writer, so a secret in any log event is masked, not just at the call sites that redact.
pub struct RedactingWriter<W: std::io::Write> {
    inner: W,
    line: Vec<u8>,
}

impl<W: std::io::Write> RedactingWriter<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            line: Vec::new(),
        }
    }

    fn emit(&mut self, upto: usize) -> std::io::Result<()> {
        let line: Vec<u8> = self.line.drain(..upto).collect();
        let text = String::from_utf8_lossy(&line);
        self.inner.write_all(redact(&text).as_bytes())
    }
}

impl<W: std::io::Write> std::io::Write for RedactingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Secrets never span lines, so a whole line is enough context.
        self.line.extend_from_slice(buf);
        while let Some(nl) = self.line.iter().position(|&b| b == b'\n') {
            self.emit(nl + 1)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if !self.line.is_empty() {
            self.emit(self.line.len())?;
        }
        self.inner.flush()
    }
}

impl<W: std::io::Write> Drop for RedactingWriter<W> {
    fn drop(&mut self) {
        _ = std::io::Write::flush(self);
    }
}

/// A crash report (thread, message, location, backtrace when `RUST_BACKTRACE` asks), redacted.
pub fn panic_report(info: &std::panic::PanicHookInfo<'_>) -> String {
    let message = info
        .payload()
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| info.payload().downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "Box<dyn Any>".into());
    let thread = std::thread::current();
    let mut report = format!(
        "thread '{}' panicked at {}:\n{message}",
        thread.name().unwrap_or("<unnamed>"),
        info.location()
            .map_or_else(|| "<unknown>".into(), |l| l.to_string()),
    );
    let backtrace = std::backtrace::Backtrace::capture();
    if backtrace.status() == std::backtrace::BacktraceStatus::Captured {
        report.push_str(&format!("\nstack backtrace:\n{backtrace}"));
    }
    redact(&report).into_owned()
}

/// Replaces the default panic hook (which prints the raw message) with [`panic_report`].
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| eprintln!("{}", panic_report(info))));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn userinfo_is_masked() {
        assert_eq!(
            redact("fatal: https://bob:hunter2@github.com/o/r.git failed"),
            "fatal: https://***@github.com/o/r.git failed"
        );
        assert_eq!(
            redact("https://token@dev.azure.com/x"),
            "https://***@dev.azure.com/x"
        );
        // Not userinfo: `@` after the authority, or none at all.
        assert_eq!(
            redact("https://github.com/o/r@v1"),
            "https://github.com/o/r@v1"
        );
        assert_eq!(redact("git@github.com:o/r.git"), "git@github.com:o/r.git");
        assert_eq!(
            redact("a https://u:p@h1/x and http://v@h2"),
            "a https://***@h1/x and http://***@h2"
        );
        assert!(matches!(redact("plain text"), Cow::Borrowed(_)));
    }

    #[test]
    fn registered_secrets_are_masked_everywhere() {
        let canary = "ghp_CANARY0123456789abcdef";
        register(canary);
        assert_eq!(
            redact(&format!("remote: token {canary} rejected")),
            format!("remote: token {MASK} rejected")
        );
        // Short values are ignored rather than masking common words.
        register("abc");
        assert_eq!(redact("abc"), "abc");
        unregister(canary);
        assert!(redact(canary).contains(canary));
    }
}

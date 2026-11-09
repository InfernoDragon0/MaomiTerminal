//! Redaction canary for logs and crash reports. Its own test binary: it swaps the process-wide
//! panic hook.

use std::io::Write as _;
use std::sync::Mutex;

use maomi_git::redact::{self, RedactingWriter};

const CANARY: &str = "CANARY-redaction-0123456789abcdef";

#[test]
fn canary_never_reaches_logs_or_crash_reports() {
    redact::register(CANARY);

    // Log writer: events arrive in pieces; a secret split across writes is still masked.
    let mut out = Vec::new();
    {
        let mut log = RedactingWriter::new(&mut out);
        let half = CANARY.len() / 2;
        write!(log, "INFO push: token {}", &CANARY[..half]).unwrap();
        writeln!(log, "{} for https://me:pw@example.com", &CANARY[half..]).unwrap();
        write!(log, "trailing {CANARY}").unwrap();
    }
    let log = String::from_utf8(out).unwrap();
    assert!(!log.contains(CANARY), "{log}");
    assert!(!log.contains(":pw@"), "{log}");
    assert!(log.contains("INFO push: token") && log.contains("trailing"));
    assert_eq!(log.lines().count(), 2);

    // Crash report: the hook's text, for both &str and String payloads.
    static REPORTS: Mutex<Vec<String>> = Mutex::new(Vec::new());
    std::panic::set_hook(Box::new(|info| {
        REPORTS.lock().unwrap().push(redact::panic_report(info))
    }));
    _ = std::panic::catch_unwind(|| panic!("clone failed: https://x:{CANARY}@host"));
    _ = std::panic::catch_unwind(|| std::panic::panic_any(format!("token {CANARY}")));
    let _ = std::panic::take_hook();
    let reports = REPORTS.lock().unwrap();
    assert_eq!(reports.len(), 2);
    for report in reports.iter() {
        assert!(!report.contains(CANARY), "{report}");
        assert!(report.contains("panicked at") && report.contains("redaction.rs"));
    }
}

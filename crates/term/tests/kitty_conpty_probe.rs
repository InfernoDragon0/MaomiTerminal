//! Temporary spike: does ConPTY pass Kitty graphics APCs and placeholder cells through?
//! Run with MAOMI_RECORD=<dir> and MAOMI_KITTY_PAYLOAD=<file>, then read the recording.

use std::time::{Duration, Instant};

use maomi_term::{PtySession, SessionOptions, Shell};

#[test]
#[ignore = "spike"]
fn kitty_through_conpty() {
    let payload = std::env::var("MAOMI_KITTY_PAYLOAD").expect("MAOMI_KITTY_PAYLOAD");
    let session = PtySession::spawn(SessionOptions {
        shell: Some(Shell::with_args("cmd.exe", vec!["/Q".into()])),
        cwd: Some(std::env::temp_dir()),
        shell_integration: false,
        ..SessionOptions::default()
    })
    .expect("spawn");
    session.write(format!("python -c \"import sys;sys.stdout.buffer.write(open(r'{payload}','rb').read());sys.stdout.flush()\"\r"));
    let deadline = Instant::now() + Duration::from_secs(20);
    while !session.snapshot().text().contains("END") {
        assert!(Instant::now() < deadline, "{}", session.snapshot().text());
        std::thread::sleep(Duration::from_millis(50));
    }
    // The a=q in the payload is answered into the PTY input; see where ConPTY puts it.
    std::thread::sleep(Duration::from_millis(1500));
    println!("SCREEN:\n{}", session.snapshot().text());
}

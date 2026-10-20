//! Real-PTY tests: spawn a shell, type into it, read the grid back.

use std::time::{Duration, Instant};

use maomi_term::{PromptState, PtySession, SessionOptions, Shell, TermEvent, TermSize};

const TIMEOUT: Duration = Duration::from_secs(20);

/// A plain shell without integration, so output is exactly what we type.
fn plain_shell() -> Shell {
    if cfg!(windows) {
        Shell::with_args("cmd.exe", vec!["/Q".into()])
    } else {
        Shell::with_args("/bin/sh", Vec::new())
    }
}

fn spawn(shell: Shell, integration: bool) -> PtySession {
    PtySession::spawn(SessionOptions {
        shell: Some(shell),
        cwd: Some(std::env::temp_dir()),
        shell_integration: integration,
        ..SessionOptions::default()
    })
    .expect("spawn shell")
}

fn line(cmd: &str) -> String {
    format!("{cmd}{}", if cfg!(windows) { "\r" } else { "\n" })
}

/// Polls the grid until `pred` holds or the timeout hits; returns the last text.
fn wait_for_text(session: &PtySession, pred: impl Fn(&str) -> bool) -> String {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let text = session.snapshot().text();
        if pred(&text) {
            return text;
        }
        assert!(Instant::now() < deadline, "timed out; screen:\n{text}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn wait_for_event(session: &PtySession, pred: impl Fn(&TermEvent) -> bool) -> TermEvent {
    let events = session.events();
    let deadline = Instant::now() + TIMEOUT;
    loop {
        while let Ok(event) = events.try_recv() {
            if pred(&event) {
                return event;
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for event; screen:\n{}",
            session.snapshot().text()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn echo_round_trip() {
    let session = spawn(plain_shell(), false);
    session.write(line("echo maomi-round-trip"));
    // The typed command echoes once and its output once.
    wait_for_text(&session, |t| t.matches("maomi-round-trip").count() >= 2);
}

#[test]
fn resize_reaches_the_shell() {
    let session = spawn(plain_shell(), false);
    session.resize(TermSize::new(101, 31));
    assert_eq!(session.snapshot().columns, 101);
    assert_eq!(session.snapshot().screen_lines, 31);
    if cfg!(windows) {
        session.write(line("mode con"));
        wait_for_text(&session, |t| t.contains("101"));
    } else {
        session.write(line("stty size"));
        wait_for_text(&session, |t| t.contains("31 101"));
    }
}

#[test]
fn exit_is_reported_after_output() {
    let session = spawn(plain_shell(), false);
    session.write(line("echo bye-bye && exit 3"));
    let event = wait_for_event(&session, |e| matches!(e, TermEvent::Exited(_)));
    assert_eq!(event, TermEvent::Exited(Some(3)));
    assert!(!session.is_running());
    assert!(session.snapshot().text().contains("bye-bye"));
}

#[test]
fn kill_ends_the_session() {
    let session = spawn(plain_shell(), false);
    session.kill();
    wait_for_event(&session, |e| matches!(e, TermEvent::Exited(_)));
}

#[test]
fn wakeup_is_coalesced_until_snapshot() {
    let session = spawn(plain_shell(), false);
    let events = session.events();
    session.write(line("echo a && echo b && echo c"));
    wait_for_event(&session, |e| *e == TermEvent::Wakeup);
    std::thread::sleep(Duration::from_millis(300));
    let extra = std::iter::from_fn(|| events.try_recv().ok())
        .filter(|e| *e == TermEvent::Wakeup)
        .count();
    assert_eq!(extra, 0, "no second Wakeup before a snapshot");
    session.snapshot();
    session.write(line("echo d"));
    wait_for_event(&session, |e| *e == TermEvent::Wakeup);
}

/// Shell integration: the cwd and prompt marks survive the PTY (ConPTY on Windows).
#[test]
fn integration_reports_cwd_and_prompt() {
    let shell = if cfg!(windows) {
        match maomi_term::available_shells()
            .into_iter()
            .find(|s| s.kind.is_powershell())
        {
            Some(shell) => shell,
            None => return,
        }
    } else {
        match which_shell("bash") {
            Some(shell) => shell,
            None => return,
        }
    };
    let session = spawn(shell, true);
    wait_for_event(&session, |e| {
        *e == TermEvent::PromptChanged(PromptState::Prompt)
    });

    let target = std::env::temp_dir().join("maomi-cwd-test");
    std::fs::create_dir_all(&target).unwrap();
    session.write(line(&format!("cd '{}'", target.display())));
    wait_for_event(&session, |e| match e {
        TermEvent::CwdChanged(path) => same_dir(path, &target),
        _ => false,
    });
    assert_eq!(session.at_prompt(), Some(true));
}

/// Without integration, `poll_cwd` reads the shell process's cwd from the OS.
#[test]
fn poll_cwd_falls_back_to_the_process() {
    let session = spawn(plain_shell(), false);
    let target = std::env::temp_dir().join("maomi-poll-cwd-test");
    std::fs::create_dir_all(&target).unwrap();
    let cd = if cfg!(windows) { "cd /d" } else { "cd" };
    session.write(line(&format!(
        "{cd} \"{}\" && echo moved",
        target.display()
    )));
    wait_for_text(&session, |t| t.matches("moved").count() >= 2);
    let deadline = Instant::now() + TIMEOUT;
    while !session.poll_cwd().is_some_and(|p| same_dir(&p, &target)) {
        assert!(Instant::now() < deadline, "cwd stayed {:?}", session.cwd());
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(unix)]
fn which_shell(name: &str) -> Option<Shell> {
    ["/bin", "/usr/bin", "/usr/local/bin"]
        .iter()
        .map(|dir| std::path::Path::new(dir).join(name))
        .find(|p| p.is_file())
        .map(|p| Shell::with_args(p, Vec::new()))
}

#[cfg(windows)]
fn which_shell(_: &str) -> Option<Shell> {
    None
}

fn same_dir(a: &std::path::Path, b: &std::path::Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

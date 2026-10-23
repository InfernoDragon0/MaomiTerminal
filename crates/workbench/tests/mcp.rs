//! Process tests for the board client against `fixtures/fake-mcp.js`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use maomi_workbench::{BoardClient, Column, McpError};
use serde_json::{Value, json};

fn node_available() -> bool {
    Command::new("node")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Skips the test (with a note) when node is not installed.
macro_rules! need_node {
    () => {
        if !node_available() {
            eprintln!("skipped: node is not on PATH");
            return;
        }
    };
}

struct Fixture {
    dir: PathBuf,
    script: PathBuf,
    log: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("maomi-mcp-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake-mcp.js");
        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-mcp.js"),
            &script,
        )
        .unwrap();
        Self {
            log: dir.join("log.txt"),
            dir,
            script,
        }
    }

    fn client(&self) -> BoardClient {
        BoardClient::with_command(
            "node",
            [self.script.as_os_str(), self.log.as_os_str()],
            Some(self.script.clone()),
        )
    }

    fn log_lines(&self) -> Vec<String> {
        fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn spawns(&self) -> usize {
        self.log_lines()
            .iter()
            .filter(|l| l.starts_with("spawn "))
            .count()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn handshake_then_calls_share_one_child() {
    need_node!();
    let fx = Fixture::new("share");
    let client = fx.client();
    assert_eq!(client.call_tool("ping", &json!({})).unwrap(), "ok ping");
    let first = client.call_tool("pid", &json!({})).unwrap();
    let second = client.call_tool("pid", &json!({})).unwrap();
    assert_eq!(first, second);
    assert_eq!(fx.spawns(), 1);

    let log = fx.log_lines();
    let init = log.iter().find(|l| l.starts_with("initialize ")).unwrap();
    let info: Value = serde_json::from_str(init.strip_prefix("initialize ").unwrap()).unwrap();
    assert_eq!(info["name"], "MaomiTerminal");
    assert!(log.iter().any(|l| l == "initialized"));
}

#[test]
fn typed_helpers_send_the_built_arguments() {
    need_node!();
    let fx = Fixture::new("typed");
    let client = fx.client();
    client
        .move_card("proj", "p-1", "m-1", &Column::Review, Some(0), 5)
        .unwrap();
    client
        .set_task_done("proj", "p-1", "m-1", "t-1", true, true, 5)
        .unwrap();
    client.review("proj", "p-1", "m-1", true, None).unwrap();

    let calls: Vec<Value> = fx
        .log_lines()
        .iter()
        .filter_map(|l| l.strip_prefix("call "))
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0]["name"], "board");
    assert_eq!(calls[0]["arguments"]["actor"], "user");
    assert_eq!(calls[0]["arguments"]["args"]["column"], "review");
    assert_eq!(calls[1]["name"], "update_task");
    assert_eq!(calls[1]["arguments"]["approved"], true);
    assert_eq!(calls[2]["arguments"]["args"]["decision"], "approve");
}

#[test]
fn rev_conflict_and_tool_errors_are_told_apart() {
    need_node!();
    let fx = Fixture::new("errors");
    let client = fx.client();
    match client.call_tool("conflict", &json!({})) {
        Err(McpError::Conflict(text)) => assert!(text.contains("it is now rev 9")),
        other => panic!("expected Conflict, got {other:?}"),
    }
    match client.call_tool("tool_error", &json!({})) {
        Err(McpError::Tool(text)) => assert_eq!(text, "card m-1 has no open review"),
        other => panic!("expected Tool, got {other:?}"),
    }
    assert!(matches!(
        client.call_tool("rpc_error", &json!({})),
        Err(McpError::Protocol(_))
    ));
    // Errors that came back as replies keep the child.
    assert_eq!(fx.spawns(), 1);
}

#[test]
fn timeout_kills_the_child_and_the_next_call_starts_a_new_one() {
    need_node!();
    let fx = Fixture::new("timeout");
    let client = fx.client().with_timeout(Duration::from_secs(3));
    assert_eq!(client.call_tool("ping", &json!({})).unwrap(), "ok ping");
    assert!(matches!(
        client.call_tool("hang", &json!({})),
        Err(McpError::Timeout)
    ));
    assert_eq!(client.call_tool("ping", &json!({})).unwrap(), "ok ping");
    assert_eq!(fx.spawns(), 2);
}

#[test]
fn a_child_that_exits_mid_call_is_not_retried_but_replaced_next_time() {
    need_node!();
    let fx = Fixture::new("exit");
    let client = fx.client();
    assert!(matches!(
        client.call_tool("exit", &json!({})),
        Err(McpError::Exited)
    ));
    // The request reached the server once; it must not be sent again.
    let sent = fx
        .log_lines()
        .iter()
        .filter(|l| l.starts_with("call "))
        .count();
    assert_eq!(sent, 1);
    assert_eq!(client.call_tool("ping", &json!({})).unwrap(), "ok ping");
    assert_eq!(fx.spawns(), 2);
}

#[test]
fn a_newer_script_restarts_the_child() {
    need_node!();
    let fx = Fixture::new("mtime");
    let client = fx.client();
    let before = client.call_tool("pid", &json!({})).unwrap();
    assert_eq!(client.call_tool("pid", &json!({})).unwrap(), before);

    let file = fs::OpenOptions::new().write(true).open(&fx.script).unwrap();
    file.set_modified(SystemTime::now() + Duration::from_secs(60))
        .unwrap();
    drop(file);

    let after = client.call_tool("pid", &json!({})).unwrap();
    assert_ne!(before, after);
    assert_eq!(fx.spawns(), 2);
}

#[test]
fn a_script_deleted_while_running_is_reported() {
    need_node!();
    let fx = Fixture::new("gone");
    let client = fx.client();
    client.call_tool("ping", &json!({})).unwrap();
    fs::remove_file(&fx.script).unwrap();
    assert!(matches!(
        client.call_tool("ping", &json!({})),
        Err(McpError::ScriptMissing(_))
    ));
}

#[test]
fn dropping_the_client_stops_the_child() {
    need_node!();
    let fx = Fixture::new("drop");
    let client = fx.client();
    let pid: u32 = client
        .call_tool("pid", &json!({}))
        .unwrap()
        .parse()
        .unwrap();
    drop(client);
    std::thread::sleep(Duration::from_millis(500));
    assert!(!process_alive(pid), "child {pid} still running");
}

fn process_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        let out = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).contains(&pid.to_string())
    }
    #[cfg(not(windows))]
    {
        Path::new(&format!("/proc/{pid}")).exists()
    }
}

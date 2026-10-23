//! Client for the Workbench board server, the only way MaomiTerminal changes a board.
//!
//! The Workbench VS Code extension installs `maomi-board-mcp.js`, an MCP server that owns every
//! board write (validation, rev checks, activity log). This module starts it as a child process
//! (`node <script>`) and speaks newline-delimited JSON-RPC 2.0 over its stdio. The API is blocking:
//! call it from a background thread.
//!
//! Writes act as the person (actor `"user"`) and carry the plan rev that was last read, so the
//! server refuses them when a Claude session changed the plan in the meantime.

use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};

use crate::plan::Column;
use crate::store::workbench_home;

/// Time allowed for one call, including starting the server.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);
const PROTOCOL_VERSION: &str = "2025-06-18";
/// The board server writes as this actor when the person acts (its `USER_ACTOR`).
const USER_ACTOR: &str = "user";
/// Start of the server's rev-mismatch text, `Plan <id> changed since your base_rev; ...`.
const CONFLICT_MARKER: &str = "changed since your base_rev";
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Why a board call failed. The texts are meant to be shown to the user as they are.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("Node.js was not found. Install Node.js so the board server can run.")]
    NodeMissing,
    #[error(
        "The board server script is missing at {}. Open the Maomi Workbench extension in VS Code to install it.",
        .0.display()
    )]
    ScriptMissing(PathBuf),
    #[error("Could not start the board server: {0}")]
    Spawn(#[source] io::Error),
    #[error("Lost the connection to the board server: {0}")]
    Io(#[source] io::Error),
    #[error("The board server stopped before it answered.")]
    Exited,
    #[error("The board server sent an unexpected reply: {0}")]
    Protocol(String),
    #[error("The board server did not answer in time.")]
    Timeout,
    /// The plan changed after it was read (`base_rev` no longer matches). Reload and try again.
    #[error("The plan changed since it was loaded. Reload it and try again. ({0})")]
    Conflict(String),
    /// The server refused the change; the text says why.
    #[error("{0}")]
    Tool(String),
}

/// One `tools/call`: the tool's name and its `arguments`.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub name: &'static str,
    pub arguments: Value,
}

/// Moves a card to `column`, at 0-based position `index` when given.
///
/// Goes through the `board` tool: the `actor` has to sit next to `op`/`args`, because the server
/// copies only the top-level one into the op's arguments.
pub fn move_card_call(
    project: &str,
    plan: &str,
    card: &str,
    column: &Column,
    index: Option<usize>,
    base_rev: i64,
) -> ToolCall {
    let mut args = json!({
        "project": project,
        "plan": plan,
        "milestone": card,
        "column": column.as_str(),
        "base_rev": base_rev,
    });
    if let Some(index) = index {
        args["index"] = json!(index);
    }
    board_call("move_milestone", args)
}

/// Ticks or unticks a task. `approval` marks an approval task, which the server only lets someone
/// tick when `approved: true` relays the user's approval, so that is sent along with a tick.
pub fn set_task_done_call(
    project: &str,
    plan: &str,
    card: &str,
    task_id: &str,
    done: bool,
    approval: bool,
    base_rev: i64,
) -> ToolCall {
    let mut arguments = json!({
        "actor": USER_ACTOR,
        "project": project,
        "plan": plan,
        "milestone": card,
        "task": task_id,
        "done": done,
        "base_rev": base_rev,
    });
    if approval && done {
        arguments["approved"] = json!(true);
    }
    ToolCall {
        name: "update_task",
        arguments,
    }
}

/// Relays the user's review decision on a card: approve it or send it back with `feedback`.
///
/// The server requires feedback text, so a default is filled in. `relay_review_feedback` takes no
/// `base_rev` (it only appends a note or resolves the open review), so none is sent.
pub fn review_call(
    project: &str,
    plan: &str,
    card: &str,
    approve: bool,
    feedback: Option<&str>,
) -> ToolCall {
    let feedback = feedback
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .unwrap_or(if approve { "Approved." } else { "Sent back." });
    board_call(
        "relay_review_feedback",
        json!({
            "project": project,
            "plan": plan,
            "milestone": card,
            "feedback": feedback,
            "decision": if approve { "approve" } else { "send_back" },
        }),
    )
}

/// Answers an open question of a plan's handoff (the user's own answer, so no relaying needed).
pub fn answer_question_call(
    project: &str,
    plan: &str,
    handoff: &str,
    question: &str,
    text: &str,
) -> ToolCall {
    board_call(
        "answer_question",
        json!({
            "project": project,
            "plan": plan,
            "handoff": handoff,
            "question": question,
            "text": text,
        }),
    )
}

fn board_call(op: &str, args: Value) -> ToolCall {
    ToolCall {
        name: "board",
        arguments: json!({ "op": op, "args": args, "actor": USER_ACTOR }),
    }
}

/// A running board server and the pipes to it.
struct Running {
    child: Child,
    stdin: ChildStdin,
    /// Lines from the child's stdout; disconnects when the child's stdout closes.
    lines: Receiver<String>,
    /// The script's state when this child started, to notice a newer install.
    stamp: Option<Stamp>,
}

impl Running {
    fn send(&mut self, message: &Value) -> io::Result<()> {
        let mut line = message.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes())?;
        self.stdin.flush()
    }

    /// Waits for the reply to request `id`, skipping anything else the server prints.
    fn reply(&self, id: u64, deadline: Instant) -> Result<Value, McpError> {
        loop {
            let left = deadline
                .checked_duration_since(Instant::now())
                .ok_or(McpError::Timeout)?;
            match self.lines.recv_timeout(left) {
                Ok(line) => match serde_json::from_str::<Value>(&line) {
                    Ok(v) if v.get("id").and_then(Value::as_u64) == Some(id) => return Ok(v),
                    Ok(_) => {}
                    Err(_) => tracing::debug!(%line, "board server printed a non-JSON line"),
                },
                Err(RecvTimeoutError::Timeout) => return Err(McpError::Timeout),
                Err(RecvTimeoutError::Disconnected) => return Err(McpError::Exited),
            }
        }
    }

    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        // Killing a child that already exited fails; that is fine.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Default)]
struct Inner {
    running: Option<Running>,
    next_id: u64,
}

/// Size and modification time of a file; any change means a newer install.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp(SystemTime, u64);

fn stamp(path: &Path) -> Option<Stamp> {
    let meta = path.metadata().ok()?;
    Some(Stamp(meta.modified().ok()?, meta.len()))
}

/// Connection to the board server. Starts the child on first use, runs one request at a time and
/// can be shared between threads (wrap it in an `Arc`).
pub struct BoardClient {
    program: OsString,
    args: Vec<OsString>,
    /// Script whose changes restart the child; it must exist.
    watch: Option<PathBuf>,
    timeout: Duration,
    inner: Mutex<Inner>,
}

impl BoardClient {
    /// Client for `node <script>`.
    pub fn new(script: PathBuf) -> Self {
        Self::with_command("node", [script.clone().into_os_string()], Some(script))
    }

    /// The script the Workbench extension installs, `<workbench home>/bridge/maomi-board-mcp.js`.
    pub fn default_script() -> Option<PathBuf> {
        Some(workbench_home()?.join("bridge").join("maomi-board-mcp.js"))
    }

    /// Client for any command that speaks the same protocol (used by tests). `watch` is the file
    /// whose change restarts the child; a missing file fails calls with [`McpError::ScriptMissing`].
    pub fn with_command<I, S>(program: impl Into<OsString>, args: I, watch: Option<PathBuf>) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            watch,
            timeout: DEFAULT_TIMEOUT,
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Replaces the per-call time limit (20 seconds by default).
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Moves a card to a column. Returns the server's confirmation text.
    pub fn move_card(
        &self,
        project: &str,
        plan: &str,
        card: &str,
        column: &Column,
        index: Option<usize>,
        base_rev: i64,
    ) -> Result<String, McpError> {
        self.call(&move_card_call(
            project, plan, card, column, index, base_rev,
        ))
    }

    /// Ticks or unticks a task of a card. See [`set_task_done_call`].
    #[allow(clippy::too_many_arguments)]
    pub fn set_task_done(
        &self,
        project: &str,
        plan: &str,
        card: &str,
        task_id: &str,
        done: bool,
        approval: bool,
        base_rev: i64,
    ) -> Result<String, McpError> {
        self.call(&set_task_done_call(
            project, plan, card, task_id, done, approval, base_rev,
        ))
    }

    /// Approves a card in review or sends it back. See [`review_call`].
    pub fn review(
        &self,
        project: &str,
        plan: &str,
        card: &str,
        approve: bool,
        feedback: Option<&str>,
    ) -> Result<String, McpError> {
        self.call(&review_call(project, plan, card, approve, feedback))
    }

    pub fn answer_question(
        &self,
        project: &str,
        plan: &str,
        handoff: &str,
        question: &str,
        text: &str,
    ) -> Result<String, McpError> {
        self.call(&answer_question_call(
            project, plan, handoff, question, text,
        ))
    }

    /// Runs a built call.
    pub fn call(&self, call: &ToolCall) -> Result<String, McpError> {
        self.call_tool(call.name, &call.arguments)
    }

    /// Sends one `tools/call` and returns the tool's text.
    ///
    /// A request that reached the server is never sent twice: the write may have happened.
    /// Only a failure to start the server, or a pipe that was already dead, is retried.
    pub fn call_tool(&self, name: &str, arguments: &Value) -> Result<String, McpError> {
        let deadline = Instant::now() + self.timeout;
        let mut inner = self.lock();
        let mut retried = false;
        loop {
            self.ensure_running(&mut inner, deadline)?;
            inner.next_id += 1;
            let id = inner.next_id;
            let Some(running) = inner.running.as_mut() else {
                return Err(McpError::Exited);
            };
            let request = json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": { "name": name, "arguments": arguments },
            });
            if let Err(err) = running.send(&request) {
                inner.running = None;
                if err.kind() == io::ErrorKind::BrokenPipe && !retried {
                    retried = true;
                    continue;
                }
                return Err(McpError::Io(err));
            }
            let reply = running.reply(id, deadline);
            return match reply {
                Ok(reply) => tool_text(&reply),
                Err(err) => {
                    // A server that timed out or died is not trusted for the next call.
                    inner.running = None;
                    Err(err)
                }
            };
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Makes `inner.running` a live child that runs the current script.
    fn ensure_running(&self, inner: &mut Inner, deadline: Instant) -> Result<(), McpError> {
        let current = match &self.watch {
            Some(path) => Some(stamp(path).ok_or_else(|| McpError::ScriptMissing(path.clone()))?),
            None => None,
        };
        if let Some(running) = inner.running.as_mut() {
            if running.alive() && running.stamp == current {
                return Ok(());
            }
            tracing::debug!("restarting the board server");
            inner.running = None;
        }
        // A child that dies during the handshake has not seen a write yet, so one retry is safe.
        let mut attempt = self.start(current, &mut inner.next_id, deadline);
        if matches!(attempt, Err(McpError::Exited | McpError::Io(_))) {
            attempt = self.start(current, &mut inner.next_id, deadline);
        }
        inner.running = Some(attempt?);
        Ok(())
    }

    fn start(
        &self,
        stamp: Option<Stamp>,
        next_id: &mut u64,
        deadline: Instant,
    ) -> Result<Running, McpError> {
        let mut command = Command::new(&self.program);
        command
            .args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = command.spawn().map_err(|err| match err.kind() {
            io::ErrorKind::NotFound => McpError::NodeMissing,
            _ => McpError::Spawn(err),
        })?;
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(McpError::Spawn(io::Error::other("no pipes to the child")));
        };

        // The server logs to stderr; read it so a full pipe never stalls the child.
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                tracing::debug!(target: "maomi_workbench::board_server", "{line}");
            }
        });
        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });

        let mut running = Running {
            child,
            stdin,
            lines,
            stamp,
        };
        *next_id += 1;
        let id = *next_id;
        running
            .send(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "initialize",
                "params": {
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "MaomiTerminal", "version": env!("CARGO_PKG_VERSION") },
                },
            }))
            .map_err(McpError::Io)?;
        let reply = running.reply(id, deadline)?;
        if let Some(error) = reply.get("error") {
            return Err(McpError::Protocol(rpc_error_text(error)));
        }
        running
            .send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .map_err(McpError::Io)?;
        Ok(running)
    }
}

fn rpc_error_text(error: &Value) -> String {
    error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("unknown error")
        .to_owned()
}

/// Reads a `tools/call` reply: the joined text parts, or the right error for `isError`.
fn tool_text(reply: &Value) -> Result<String, McpError> {
    if let Some(error) = reply.get("error") {
        return Err(McpError::Protocol(rpc_error_text(error)));
    }
    let result = reply
        .get("result")
        .ok_or_else(|| McpError::Protocol("the reply has no result".into()))?;
    let text = result
        .get("content")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return Err(if text.contains(CONFLICT_MARKER) {
            McpError::Conflict(text)
        } else {
            McpError::Tool(text)
        });
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn move_card_puts_actor_next_to_op_not_inside_args() {
        let call = move_card_call("proj", "p-1", "m-2", &Column::InProgress, Some(3), 7);
        assert_eq!(call.name, "board");
        assert_eq!(
            call.arguments,
            json!({
                "op": "move_milestone",
                "actor": "user",
                "args": {
                    "project": "proj",
                    "plan": "p-1",
                    "milestone": "m-2",
                    "column": "in-progress",
                    "index": 3,
                    "base_rev": 7,
                },
            })
        );
        assert!(call.arguments["args"].get("actor").is_none());
    }

    #[test]
    fn move_card_omits_index_when_none() {
        let call = move_card_call("proj", "p", "m", &Column::Done, None, 1);
        assert!(call.arguments["args"].get("index").is_none());
        assert_eq!(call.arguments["args"]["column"], "done");
    }

    #[test]
    fn column_strings_match_the_server() {
        for (column, text) in [
            (Column::Backlog, "backlog"),
            (Column::InProgress, "in-progress"),
            (Column::Review, "review"),
            (Column::Blocked, "blocked"),
            (Column::Done, "done"),
        ] {
            let call = move_card_call("p", "p", "m", &column, None, 1);
            assert_eq!(call.arguments["args"]["column"], text);
        }
    }

    #[test]
    fn task_tick_is_a_direct_call_with_top_level_actor() {
        let call = set_task_done_call("proj", "p-1", "m-2", "t-9", true, false, 4);
        assert_eq!(call.name, "update_task");
        assert_eq!(
            call.arguments,
            json!({
                "actor": "user",
                "project": "proj",
                "plan": "p-1",
                "milestone": "m-2",
                "task": "t-9",
                "done": true,
                "base_rev": 4,
            })
        );
    }

    #[test]
    fn approved_is_sent_only_when_ticking_an_approval_task() {
        let ticked = set_task_done_call("p", "p", "m", "t", true, true, 1);
        assert_eq!(ticked.arguments["approved"], true);
        let unticked = set_task_done_call("p", "p", "m", "t", false, true, 1);
        assert!(unticked.arguments.get("approved").is_none());
        let plain = set_task_done_call("p", "p", "m", "t", true, false, 1);
        assert!(plain.arguments.get("approved").is_none());
    }

    #[test]
    fn review_maps_decision_and_has_no_base_rev() {
        let sent_back = review_call("proj", "p-1", "m-2", false, Some("  fix the title "));
        assert_eq!(sent_back.name, "board");
        assert_eq!(
            sent_back.arguments,
            json!({
                "op": "relay_review_feedback",
                "actor": "user",
                "args": {
                    "project": "proj",
                    "plan": "p-1",
                    "milestone": "m-2",
                    "feedback": "fix the title",
                    "decision": "send_back",
                },
            })
        );
        let approved = review_call("proj", "p-1", "m-2", true, None);
        assert_eq!(approved.arguments["args"]["decision"], "approve");
        assert_eq!(approved.arguments["args"]["feedback"], "Approved.");
        assert!(approved.arguments["args"].get("base_rev").is_none());
    }

    #[test]
    fn tool_text_joins_parts() {
        let reply = json!({"result": {"content": [
            {"type": "text", "text": "a"}, {"type": "text", "text": "b"}]}});
        assert_eq!(tool_text(&reply).unwrap(), "a\nb");
    }

    #[test]
    fn tool_text_tells_a_rev_conflict_from_other_errors() {
        let conflict = json!({"result": {"isError": true, "content": [{"type": "text", "text":
            "Plan p-1 changed since your base_rev; it is now rev 9. Re-read and retry."}]}});
        assert!(matches!(tool_text(&conflict), Err(McpError::Conflict(t)) if t.contains("rev 9")));
        let other =
            json!({"result": {"isError": true, "content": [{"type": "text", "text": "nope"}]}});
        assert!(matches!(tool_text(&other), Err(McpError::Tool(t)) if t == "nope"));
    }

    #[test]
    fn tool_text_reports_rpc_errors() {
        let reply = json!({"error": {"code": -32602, "message": "Unknown tool: x"}});
        assert!(matches!(tool_text(&reply), Err(McpError::Protocol(t)) if t == "Unknown tool: x"));
    }

    #[test]
    fn missing_script_is_reported_before_spawning() {
        let client = BoardClient::new(PathBuf::from("Z:/definitely/not/here.js"));
        assert!(matches!(
            client.call_tool("open_plan", &json!({})),
            Err(McpError::ScriptMissing(_))
        ));
    }

    #[test]
    fn missing_program_is_node_missing() {
        let client = BoardClient::with_command("maomi-no-such-program-xyz", ["x"], None);
        assert!(matches!(
            client.call_tool("open_plan", &json!({})),
            Err(McpError::NodeMissing)
        ));
    }
}

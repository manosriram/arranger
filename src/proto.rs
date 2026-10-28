//! Client <-> daemon messages: u32 little-endian length, then a postcard body.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::env;
use std::io::{self, Read};
use std::path::PathBuf;

pub const MAX_FRAME: usize = 16 << 20;
/// Bump whenever Request/Reply change shape. A daemon left running from an older build
/// can't decode newer messages, so clients check this before anything else.
pub const VERSION: u32 = 26;

#[derive(Serialize, Deserialize, Debug)]
pub enum Request {
    Ping,
    /// `prompt` is passed to the agent as its first message; `model` as `--model` (None: the agent's default).
    /// `headless` runs it over stdio instead of in a terminal (claude only, see `headless`).
    /// `effort`: its reasoning effort ("" for the agent's default). `headless`: in a chat, not a terminal.
    New { agent: String, repo: PathBuf, base: Option<String>, prompt: Option<String>, model: Option<String>, effort: String, headless: bool },
    List { files: bool },
    Kill { id: u64 },
    Events { id: u64 },
    /// From `arranger hook`; no reply.
    Event { task: u64, event: Event },
    /// A permission ask from `arranger hook`; replied to with `Decision` once a human answers.
    Ask { task: u64, event: Event },
    Answer { id: u64, allow: bool, message: String },
    /// Turns the connection into a stream: Output/Exited down, Input/Resize up.
    /// `native`: the viewer answers permission asks in the agent's own UI (terminal attach),
    /// rather than through Answer (GUI).
    Attach { id: u64, cols: u16, rows: u16, native: bool },
    /// Like Attach, but to the task's shell in its worktree (the Terminal tab), started if needed.
    Shell { id: u64, cols: u16, rows: u16 },
    /// Subscribes to `Tasks` pushes whenever any task changes.
    Watch,
    Input(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    /// Sent first on every client connection; see `VERSION`.
    Version,
    Review { id: u64 },
    /// Undo one hunk (identified by its header) in the task's worktree.
    Revert { id: u64, path: String, header: String },
    /// Type `text` into the agent and submit it, as a new prompt.
    Prompt { id: u64, text: String },
    /// Stop the agent's current turn, like Esc in its terminal.
    Interrupt { id: u64 },
    /// A headless task's conversation: its last `limit` items, then `ChatPatch`es on this
    /// connection as it goes on.
    Chat { id: u64, limit: usize },
    /// Up to `limit` items before item `end`, for scrolling back; replied to with `Chat`.
    ChatPage { id: u64, end: usize, limit: usize },
    /// Commit everything in the worktree, then maybe merge or open a PR. Replied to with JobDone.
    Commit { id: u64, message: String, ship: Ship, force: bool },
    /// The task's recorded PTY output (zstd), for replaying a finished task's chat.
    Log { id: u64 },
    /// Start an ended task's agent again in its worktree, resuming its last session, and send it
    /// `prompt` if not empty.
    Revive { id: u64, prompt: String },
    /// Undo the merge under way in the task's worktree.
    AbortMerge { id: u64 },
    /// Settle every conflict with one side: the task's branch (`ours`) or its base.
    TakeSide { id: u64, ours: bool },
    /// Repository paths for the new-task prompt’s @ picker.
    Paths { repo: PathBuf },
    /// A new task on its own worktree, made from task `id`'s current state (uncommitted edits
    /// too), whose agent carries on from a copy of `id`'s session, sent `prompt` if not empty.
    /// `model`: None keeps `id`'s. Replied to with Created.
    Clone { id: u64, prompt: String, model: Option<String> },
    /// Stop through the protected socket; never signal a PID read from a stale file.
    /// Append new variants so an old hook cannot be reinterpreted as a different operation.
    Shutdown,
    /// Switch a headless task's permission mode (one of `headless::MODES`); kept for its revives.
    SetMode { id: u64, mode: String },
    /// Move a task's setup along: see `SetupAction`.
    Setup { id: u64, action: SetupAction },
    /// The task's setup output so far (raw, with colour); replied to with Log.
    SetupLog { id: u64 },
    /// Run these checks in the task's worktree, one after another. Replied to with Ok.
    RunChecks { id: u64, checks: Vec<CheckSpec> },
    /// Stop the task's running checks.
    StopChecks { id: u64 },
    /// The task's check runs, newest first; replied to with Checks.
    Checks { id: u64 },
    /// Lines `start..start + limit` of a check's output; replied to with Console.
    Console { check: u64, start: usize, limit: usize },
    /// Run the planned checks waiting on approval; `always`: and don't ask about these
    /// commands again in this repository.
    ApproveChecks { id: u64, always: bool },
    /// Stop requiring a check that failed before: it no longer runs again by itself.
    DropCheck { id: u64, command: String },
    /// Go on despite the latest run's failures. Replied to with Ok.
    AcceptChecks { id: u64 },
    /// What to tell the agent about its latest run's failures; replied to with Text.
    CheckReport { id: u64 },
    GenerateSummary { id: u64 },
    /// A daemon setting ("auto_fix": rounds of failures sent to the agent by themselves), set
    /// to `value` if given; replied to with Text (its value now).
    Setting { key: String, value: Option<String> },
}

/// A check to run: a shell command, and why it's worth running.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct CheckSpec {
    pub name: String,
    pub command: String,
    pub why: String,
}

/// One go at a task's checks, on the worktree as it was (`tree`).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CheckRun {
    pub id: u64,
    /// One-based run number within this task, independent of the database ID.
    pub number: u64,
    /// Unix seconds; `ended` is 0 while it runs.
    pub started: u64,
    pub ended: u64,
    /// The worktree's tree hash when it started, uncommitted edits included.
    pub tree: String,
    /// "planning", "approval", "running", "passed", "failed", "stopped", "stale" (the agent
    /// started another turn), "skipped" (nothing needed checking), "unverified" (code changed and
    /// no check covers it) or "error" (it couldn't be planned).
    pub state: String,
    /// Arranger started it after the agent's turn (not you).
    pub auto: bool,
    /// Why it has no checks, or what went wrong.
    pub note: String,
    /// It failed, and you chose to go on anyway.
    pub accepted: bool,
    pub checks: Vec<CheckInfo>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CheckInfo {
    pub id: u64,
    pub name: String,
    pub command: String,
    pub why: String,
    /// "pending", "running", "passed", "failed" or "stopped".
    pub state: String,
    /// Its exit status; None if it didn't exit by itself (or hasn't yet).
    pub exit: Option<i32>,
    pub started: u64,
    pub ended: u64,
    /// "planned" (by the check planner), "sticky" (it failed before and must pass), or "manual".
    pub origin: String,
    /// What arranger saw: files it changed (and were put back), or why it was stopped.
    pub note: String,
}

/// Where a task's latest check run is, for its pipeline strip.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct ChecksSummary {
    /// "" (never run), or the latest run's state (`CheckRun::state`).
    pub state: String,
    pub passed: u32,
    pub failed: u32,
    pub total: u32,
    /// The run, and when it started (unix seconds).
    pub run: u64,
    pub started: u64,
    /// It failed and you went on anyway.
    pub accepted: bool,
}

/// A line of a check's output: runs of text in one style, and whether it looks like an error.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ConsoleLine {
    pub spans: Vec<(String, Style)>,
    pub error: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub enum Color {
    /// The 256-colour palette; 0-15 are the terminal's own.
    Idx(u8),
    Rgb(u8, u8, u8),
}

/// What to do with a task's setup (`TaskInfo::setup`).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub enum SetupAction {
    /// Run the planned setup, and remember the plan as approved for this repository.
    Approve,
    /// Start the agent without setting up.
    Skip,
    /// Run the same plan again (after a failure).
    Retry,
    /// Forget the plan and have the agent look at the repository again.
    Replan,
}

/// What happens after the commit.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub enum Ship {
    Commit,
    /// Into the task's base branch.
    Merge,
    /// Push and open a PR against the base branch.
    Pr,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum Reply {
    Pong,
    Ok,
    Created { id: u64, state: String },
    Tasks(Vec<TaskInfo>),
    /// (unix ms, event)
    Events(Vec<(u64, Event)>),
    /// `allow: None` means no decision: the agent shows its own prompt.
    Decision { allow: Option<bool>, message: String },
    Output(Vec<u8>),
    Exited,
    Error(String),
    Version(u32),
    Review(Box<Review>),
    JobDone { ok: bool, output: String },
    Log(Vec<u8>),
    Paths(Vec<String>),
    /// Items `start..start + items.len()` of a conversation `total` long.
    Chat { start: usize, total: usize, items: Vec<ChatItem> },
    /// Item `index` changed, or (at the end) was added.
    ChatPatch { index: usize, item: ChatItem },
    /// To a headless task's Prompt that only the agent's terminal can take: open one in the task's
    /// worktree with `command` typed, for the user to run.
    Terminal { command: String },
    Checks(Vec<CheckRun>),
    Text(String),
    /// Lines `start..start + lines.len()` of `total` so far; `done`: the check has ended and
    /// all its output is in.
    Console { check: u64, start: usize, total: usize, done: bool, lines: Vec<ConsoleLine> },
}

/// One entry in a headless task's conversation.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum ChatItem {
    /// What the user sent.
    User { text: String },
    /// Markdown; it grows while it streams.
    Assistant { text: String },
    Thinking { text: String },
    /// A tool call and, once it's back, its result. `diff` is what an edit changes.
    Tool { id: String, name: String, kind: ToolKind, target: String, input: String, output: String, ok: Option<bool>, diff: Vec<DiffLine> },
    /// The agent's plan: (step, "pending" | "in_progress" | "completed").
    Todos { items: Vec<(String, String)> },
    Notice { text: String, error: bool },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TaskInfo {
    pub id: u64,
    pub agent: String,
    pub branch: String,
    pub worktree: String,
    pub state: String,
    /// What the agent is doing right now, e.g. "editing src/pty.rs".
    pub action: String,
    pub files: Vec<String>,
    /// First prompt, or "" before there is one.
    pub title: String,
    /// Unix seconds.
    pub created: u64,
    /// The event that put the task in needs-you, while it's there.
    pub need: Option<Event>,
    /// A permission ask is waiting on Answer.
    pub asking: bool,
    /// Unix seconds when `state`/`action` last changed (live tasks only, else 0).
    pub since: u64,
    /// First line of the agent's last reply, or "".
    pub summary: String,
    /// How its work last left Review: "committed", "merged" or "PR"; "" if it hasn't.
    pub shipped: String,
    /// Unix seconds of its last event, state change or ship.
    pub updated: u64,
    /// Its `--model` ("" for the agent's default).
    #[serde(default)]
    pub model: String,
    /// The source worktree's task ID, if this task was cloned.
    #[serde(default)]
    pub cloned_from: Option<u64>,
    /// Runs over stdio, with no terminal to attach to.
    #[serde(default)]
    pub headless: bool,
    /// Its reasoning effort ("" for the agent's default).
    #[serde(default)]
    pub effort: String,
    /// A headless agent's permission mode ("" if it has none arranger can switch).
    #[serde(default)]
    pub mode: String,
    /// Where its setup is: "" (none: from before setup existed), "planning", "approval",
    /// "running", "ok", "failed" or "skipped".
    #[serde(default)]
    pub setup: String,
    /// The setup plan while it waits for approval, why it failed, or what it's running.
    #[serde(default)]
    pub setup_detail: String,
    /// Its latest check run.
    #[serde(default)]
    pub checks: ChecksSummary,
    pub summary_state: String,
    /// Which machine the GUI got this task from (an index into its machine list); not sent.
    #[serde(skip)]
    pub machine: usize,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkSummary {
    pub tree: String,
    pub title: String,
    pub overview: String,
    pub files: Vec<(String, String)>,
    pub risks: Vec<String>,
    pub verified_by: Vec<String>,
}

/// A task's changes against the commit its worktree branched from.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Review {
    pub tree: String,
    /// passed, accepted, skipped, stale, unverified, failed, running, or approval.
    pub verification: String,
    pub work_summary: Option<WorkSummary>,
    /// missing, running, ready, stale, or an error message.
    pub summary_state: String,
    pub branch: String,
    pub base: String,
    /// The agent's last reply.
    pub summary: String,
    /// Arranger checks for this tree: ok is None while pending or running.
    pub checks: Vec<(String, Option<bool>)>,
    pub files: Vec<FileDiff>,
    /// Files a merge left with conflict markers in the worktree.
    pub conflicts: Vec<String>,
    /// A merge is under way in the worktree (conflicts, or resolved and not yet committed).
    pub merging: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FileDiff {
    pub path: String,
    /// 'A' added, 'M' modified, 'D' deleted.
    pub status: char,
    pub added: u32,
    pub removed: u32,
    pub binary: bool,
    pub hunks: Vec<Hunk>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Hunk {
    /// `@@ -a,b +c,d @@  scope`, also the hunk's identity for Revert.
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct DiffLine {
    /// ' ', '+' or '-'.
    pub kind: char,
    /// 1-based; 0 when the line doesn't exist on that side.
    pub old: u32,
    pub new: u32,
    /// Includes its line terminator, if any.
    pub text: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub enum ToolKind {
    Edit,
    Read,
    Bash,
    Search,
    Web,
    Other,
}

/// One normalised agent action; every adapter maps into these.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum Event {
    Prompt { text: String },
    ToolStart { kind: ToolKind, target: String },
    ToolEnd { ok: bool },
    PermissionRequest { tool: String, detail: String },
    Question {
        text: String,
        #[serde(default)]
        options: Vec<String>,
    },
    /// OSC 9/777 terminal notification.
    Attention { text: String },
    TurnDone,
    /// The agent's closing reply for a turn.
    Summary { text: String },
    Error { message: String },
    /// Session totals so far.
    Usage { tokens: u64, cost: Option<f64> },
}

impl Event {
    pub fn describe(&self) -> String {
        match self {
            Event::Prompt { text } => format!("prompt: {text}"),
            Event::ToolStart { kind, target } => {
                let verb = match kind {
                    ToolKind::Edit => "editing",
                    ToolKind::Read => "reading",
                    ToolKind::Bash => "running",
                    ToolKind::Search => "searching",
                    ToolKind::Web => "fetching",
                    ToolKind::Other => "using",
                };
                format!("{verb} {target}")
            }
            Event::ToolEnd { ok: true } => "tool done".into(),
            Event::ToolEnd { ok: false } => "tool failed".into(),
            Event::PermissionRequest { tool, detail } => format!("wants {tool}: {detail}"),
            Event::Question { text, .. } => format!("asks: {text}"),
            Event::Attention { text } => format!("notice: {text}"),
            Event::TurnDone => "done".into(),
            Event::Summary { text } => format!("summary: {text}"),
            Event::Error { message } => format!("error: {message}"),
            Event::Usage { tokens, cost: Some(cost) } => format!("{tokens} tokens, ${cost:.4}"),
            Event::Usage { tokens, cost: None } => format!("{tokens} tokens"),
        }
    }
}

pub fn frame<T: Serialize>(msg: &T) -> Vec<u8> {
    let body = postcard::to_stdvec(msg).expect("postcard serialize");
    let mut out = (body.len() as u32).to_le_bytes().to_vec();
    out.extend(body);
    out
}

/// A reply that cannot fit must produce an error the client can read, rather than a disconnect.
pub fn reply_frame(msg: &Reply) -> Vec<u8> {
    let bytes = frame(msg);
    if bytes.len() > MAX_FRAME {
        frame(&Reply::Error("reply exceeds the 16 MiB limit; request a smaller page or use git for large diffs".into()))
    } else {
        bytes
    }
}

/// Pops one complete message off the front of `buf`, if there is one.
pub fn unframe<T: DeserializeOwned>(buf: &mut Vec<u8>) -> Option<io::Result<T>> {
    let len = u32::from_le_bytes(buf.get(..4)?.try_into().unwrap()) as usize;
    if len > MAX_FRAME {
        return Some(Err(io::Error::other("frame too large")));
    }
    let body = buf.get(4..4 + len)?;
    let msg = postcard::from_bytes(body).map_err(io::Error::other);
    buf.drain(..4 + len);
    Some(msg)
}

pub fn read_msg<T: DeserializeOwned>(r: &mut impl Read) -> io::Result<T> {
    let mut len = [0; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::other("frame too large"));
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body)?;
    postcard::from_bytes(&body).map_err(io::Error::other)
}

/// `$ARRANGER_HOME` overrides both paths (used by bench-mem).
pub fn socket_path() -> PathBuf {
    if let Some(home) = env::var_os("ARRANGER_HOME") {
        return PathBuf::from(home).join("arranger.sock");
    }
    let dir = env::var_os("XDG_RUNTIME_DIR")
        .unwrap_or("/tmp".into());
    PathBuf::from(dir).join(format!("arranger-{}", rustix::process::geteuid().as_raw())).join("arranger.sock")
}

pub fn data_dir() -> PathBuf {
    if let Some(home) = env::var_os("ARRANGER_HOME") {
        return home.into();
    }
    let home = PathBuf::from(env::var_os("HOME").expect("HOME is not set"));
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/arranger")
    } else {
        env::var_os("XDG_DATA_HOME")
            .map_or(home.join(".local/share"), PathBuf::from)
            .join("arranger")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unframe_waits_for_whole_frames() {
        let mut buf = frame(&Request::Kill { id: 7 });
        buf.extend(frame(&Request::Ping));
        let second = buf.split_off(buf.len() - 2);
        assert!(matches!(unframe(&mut buf), Some(Ok(Request::Kill { id: 7 }))));
        buf.extend(&second[..1]);
        assert!(unframe::<Request>(&mut buf).is_none());
        buf.extend(&second[1..]);
        assert!(matches!(unframe(&mut buf), Some(Ok(Request::Ping))));
        assert!(buf.is_empty());
    }
}

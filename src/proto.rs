//! Client <-> daemon messages: u32 little-endian length, then a postcard body.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::env;
use std::io::{self, Read};
use std::path::PathBuf;

const MAX_FRAME: usize = 64 << 20;
/// Bump whenever Request/Reply change shape. A daemon left running from an older build
/// can't decode newer messages, so clients check this before anything else.
pub const VERSION: u32 = 11;

#[derive(Serialize, Deserialize, Debug)]
pub enum Request {
    Ping,
    /// `prompt` is passed to the agent as its first message; `model` as `--model` (None: the agent's default).
    New { agent: String, repo: PathBuf, base: Option<String>, prompt: Option<String>, model: Option<String> },
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
    /// Commit everything in the worktree, then maybe merge or open a PR. Replied to with JobDone.
    Commit { id: u64, message: String, ship: Ship },
    /// The task's recorded PTY output (zstd), for replaying a finished task's chat.
    Log { id: u64 },
    /// Start an ended task's agent again in its worktree, resuming its last session, and send it
    /// `prompt` if not empty.
    Revive { id: u64, prompt: String },
    /// Undo the merge under way in the task's worktree.
    AbortMerge { id: u64 },
    /// Settle every conflict with one side: the task's branch (`ours`) or its base.
    TakeSide { id: u64, ours: bool },
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
    Review(Review),
    JobDone { ok: bool, output: String },
    Log(Vec<u8>),
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
    /// Which machine the GUI got this task from (an index into its machine list); not sent.
    #[serde(skip)]
    pub machine: usize,
}

/// A task's changes against the commit its worktree branched from.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Review {
    pub branch: String,
    pub base: String,
    /// The agent's last reply.
    pub summary: String,
    /// Check-like commands the agent ran (test, build, lint…): ok is None while running.
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

#[derive(Serialize, Deserialize, Debug, Clone)]
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
        .or_else(|| env::var_os("TMPDIR"))
        .unwrap_or("/tmp".into());
    PathBuf::from(dir).join("arranger.sock")
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

//! Client <-> daemon messages: u32 little-endian length, then a postcard body.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::env;
use std::io::{self, Read};
use std::path::PathBuf;

const MAX_FRAME: usize = 64 << 20;

#[derive(Serialize, Deserialize, Debug)]
pub enum Request {
    Ping,
    /// `prompt` is passed to the agent as its first message.
    New { agent: String, repo: PathBuf, base: Option<String>, prompt: Option<String> },
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
    /// Subscribes to `Tasks` pushes whenever any task changes.
    Watch,
    Input(Vec<u8>),
    Resize { cols: u16, rows: u16 },
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

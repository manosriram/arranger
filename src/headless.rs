//! Headless agents: each CLI over stdio in its own structured protocol, instead of its TUI in a
//! PTY. No terminal emulation, no hooks: the stream carries prompts, tool calls, permission asks
//! and turn ends.
//!
//! - claude: stream-json and its control protocol (what the Agent SDK wraps), `claude.rs`
//! - codex: `codex app-server` JSON-RPC (what its IDE extension uses), `codex.rs`
//! - pi: `pi --mode rpc`, `pi.rs`
//! - gemini: `gemini --acp`, the Agent Client Protocol, `acp.rs`
//!
//! Each launch keeps the agent as close to its interactive self as its protocol allows;
//! `scripts/parity.py` checks claude's and codex's API requests match.

mod acp;
mod claude;
mod codex;
mod pi;

use crate::adapter;
use crate::proto::Event;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::fs;
use std::io;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};

pub(crate) use acp::tool_kind as acp_tool_kind;
pub(crate) use codex::command as codex_command;

const MAX_LINE: usize = 16 << 20;

/// Agents that can run headless.
pub fn supported(agent: &str) -> bool {
    matches!(adapter::kind(agent), "claude" | "codex" | "pi" | "gemini")
}

/// Agents headless by default: those whose parity check passes.
pub fn default_on(agent: &str) -> bool {
    matches!(adapter::kind(agent), "claude" | "codex")
}

/// One agent's protocol. It reads parsed stdout lines and queues what it writes in `out`.
pub trait Transport {
    /// A line from its stdout: records what it says in `events`. False keeps the line out of
    /// the chat log (a resumed session replaying history the log already has).
    fn line(&mut self, v: Value, events: &mut Vec<Event>) -> bool;
    /// A message from the user: the answer to a pending question, else a prompt, queued while a
    /// turn runs. Returns events to record.
    fn prompt(&mut self, text: String) -> Vec<Event>;
    /// Allow or deny the pending permission ask.
    fn answer(&mut self, allow: bool, message: &str) -> Result<(), String>;
    /// Stops the running turn: denies what it waits on, drops queued prompts, interrupts.
    fn cancel(&mut self);
    fn core(&self) -> &Core;
    fn core_mut(&mut self) -> &mut Core;
    /// A message it can't take headless (a command only its terminal has): the command line
    /// that opens it there.
    fn terminal(&self, _text: &str) -> Option<String> {
        None
    }
    /// Whether its stream repeats the user's prompts; if not, the daemon logs them for the chat.
    fn echoes(&self) -> bool {
        true
    }
}

/// What every transport keeps: the turn, the queues and the bytes for stdin.
#[derive(Default)]
pub struct Core {
    pub cwd: String,
    /// The agent's session (thread, session file) for revive and clone; saved by the daemon.
    pub session: String,
    ready: bool,
    busy: bool,
    next_id: u64,
    /// The turn's latest assistant text, for its summary.
    last_text: String,
    prompts: VecDeque<String>,
    asks: VecDeque<Ask>,
    /// Bytes for the CLI's stdin, kept across partial nonblocking writes by the daemon.
    pub output: Vec<u8>,
}

impl Core {
    fn new(cwd: &Path) -> Self {
        Core { cwd: cwd.display().to_string(), next_id: 10, ..Core::default() }
    }

    fn send(&mut self, value: Value) {
        self.output.extend(serde_json::to_vec(&value).unwrap());
        self.output.push(b'\n');
    }

    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// The next queued prompt, if it can start now; marks the turn started.
    fn next_prompt(&mut self) -> Option<String> {
        if !self.ready || self.busy {
            return None;
        }
        let text = self.prompts.pop_front()?;
        self.busy = true;
        self.last_text.clear();
        Some(text)
    }

    /// Notes an ask, announcing it if it's the first waiting.
    fn ask(&mut self, ask: Ask, events: &mut Vec<Event>) {
        if self.asks.is_empty() {
            events.push(ask.need());
        }
        self.asks.push_back(ask);
    }

    /// The turn ended: its summary, then done.
    fn done(&mut self, error: Option<String>, events: &mut Vec<Event>) {
        self.busy = false;
        self.asks.clear();
        if !self.last_text.is_empty() {
            events.push(Event::Summary { text: std::mem::take(&mut self.last_text) });
        }
        match error {
            Some(message) => events.push(Event::Error { message }),
            None => events.push(Event::TurnDone),
        }
    }
}

/// A permission ask, or questions being answered one message each.
#[derive(Clone)]
struct Ask {
    id: Value,
    /// What asks: the protocol's method (codex, ACP, pi) or tool name (claude).
    method: String,
    params: Value,
    /// Its questions as (key, question, options); empty for a permission ask.
    questions: Vec<(String, String, Vec<String>)>,
    question: usize,
    answers: serde_json::Map<String, Value>,
    /// For a permission ask: (tool, what it would do).
    permission: (String, String),
}

impl Ask {
    fn permission(id: Value, method: &str, params: Value, tool: &str, detail: String) -> Ask {
        Ask { id, method: method.into(), params, questions: Vec::new(), question: 0, answers: Default::default(), permission: (tool.into(), detail) }
    }

    fn questions(id: Value, method: &str, params: Value, questions: Vec<(String, String, Vec<String>)>) -> Ask {
        Ask { id, method: method.into(), params, questions, question: 0, answers: Default::default(), permission: Default::default() }
    }

    fn is_question(&self) -> bool {
        !self.questions.is_empty()
    }

    fn need(&self) -> Event {
        match self.questions.get(self.question) {
            Some((_, text, options)) => Event::Question { text: text.clone(), options: options.clone() },
            None => Event::PermissionRequest { tool: self.permission.0.clone(), detail: self.permission.1.clone() },
        }
    }

    /// Takes one answer; true once every question has one.
    fn take(&mut self, text: String) -> bool {
        let key = self.questions[self.question].0.clone();
        self.answers.insert(key, json!(text));
        self.question += 1;
        self.question >= self.questions.len()
    }
}

/// What the daemon holds per headless task: its transport, fed whole lines.
pub struct Agent {
    t: Box<dyn Transport>,
    input: Vec<u8>,
}

impl Deref for Agent {
    type Target = dyn Transport;
    fn deref(&self) -> &Self::Target {
        &*self.t
    }
}

impl DerefMut for Agent {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut *self.t
    }
}

impl Agent {
    pub fn busy(&self) -> bool {
        self.t.core().busy
    }

    pub fn asking(&self) -> bool {
        !self.t.core().asks.is_empty()
    }

    pub fn pending_need(&self) -> Option<Event> {
        self.t.core().asks.front().map(Ask::need)
    }

    pub fn session(&self) -> &str {
        &self.t.core().session
    }

    pub fn output(&mut self) -> &mut Vec<u8> {
        &mut self.t.core_mut().output
    }

    /// Stdout from the CLI: the events it means, and the lines that belong in its chat log.
    pub fn feed(&mut self, bytes: &[u8]) -> (Vec<Event>, Vec<u8>) {
        self.input.extend_from_slice(bytes);
        let (mut events, mut kept) = (Vec::new(), Vec::new());
        while let Some(end) = self.input.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.input.drain(..=end).collect();
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            match serde_json::from_slice::<Value>(&line) {
                Ok(v) => {
                    if self.t.line(v, &mut events) {
                        kept.extend_from_slice(&line);
                    }
                }
                Err(_) => events.push(Event::Error { message: "the agent wrote a line that isn't JSON; see its stderr log".into() }),
            }
        }
        if self.input.len() > MAX_LINE {
            self.input.clear();
            events.push(Event::Error { message: "the agent wrote a line over the size limit".into() });
        }
        (events, kept)
    }
}

/// How to start task's agent headless: `program` is what the task runs (`claude`, a path to it);
/// `session` the one to carry on ("" for the latest in `cwd`, where the agent has that);
/// `fork` carries it on in a new session (a cloned task). Returns its argv and transport.
pub fn start(program: &str, cwd: &Path, model: &str, effort: &str, session: Option<&str>, fork: bool) -> (Vec<String>, Agent) {
    let mut argv = vec![program.to_string()];
    let t: Box<dyn Transport> = match adapter::kind(program) {
        "codex" => {
            argv.extend(codex::argv());
            Box::new(codex::Codex::new(cwd, model, effort, session, fork))
        }
        "pi" => {
            argv.extend(pi::argv(model, effort, session, fork));
            Box::new(pi::Pi::new(cwd))
        }
        "gemini" => {
            argv.extend(acp::gemini_argv(model));
            Box::new(acp::Acp::new(cwd, session))
        }
        _ => {
            argv.extend(claude::argv(model, effort, "", session, fork, cwd));
            Box::new(claude::Claude::new(cwd))
        }
    };
    (argv, Agent { t, input: Vec::new() })
}

/// Where the daemon keeps task `task`'s headless session.
pub fn session_file(data: &Path, task: u64) -> PathBuf {
    data.join(format!("adapters/session-{task}"))
}

/// Gives task `to.0` (in worktree `to.1`) a copy of task `from.0`'s session (in `from.1`), for
/// `start` with `fork` to pick up.
pub fn copy_session(agent: &str, data: &Path, from: (u64, &Path), to: (u64, &Path)) -> io::Result<()> {
    let session = fs::read_to_string(session_file(data, from.0))?;
    let session = session.trim();
    if session.is_empty() {
        return Err(io::Error::other(format!("it has no {} session yet", adapter::kind(agent))));
    }
    match adapter::kind(agent) {
        // claude finds sessions under the directory they started in.
        "claude" => {
            let name = format!("{session}.jsonl");
            let (src, dst) = (adapter::claude_sessions(from.1).join(&name), adapter::claude_sessions(to.1));
            fs::create_dir_all(&dst)?;
            fs::copy(src, dst.join(name))?;
        }
        // codex's threads are global, pi's a file `--fork` copies: the id or path is enough.
        "codex" | "pi" => {}
        kind => return Err(io::Error::other(format!("{kind} can't carry a session to another worktree"))),
    }
    fs::write(session_file(data, to.0), session)
}

fn string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        _ => v.to_string(),
    }
}

fn error_message(v: &Value, agent: &str) -> String {
    match v {
        Value::String(s) if !s.is_empty() => s.clone(),
        Value::Array(a) if !a.is_empty() => a.iter().map(|e| error_message(e, agent)).collect::<Vec<_>>().join("; "),
        _ if v["message"].is_string() => string(&v["message"]),
        Value::Null | Value::String(_) | Value::Array(_) => format!("{agent} stopped on an error"),
        _ => v.to_string(),
    }
}

/// What a permission ask would do, for its card: "run make test".
fn detail(tool: &str, input: &Value, cwd: &str) -> String {
    adapter::tool_start(tool, input, cwd).first().map(Event::describe).unwrap_or_default()
}

#[cfg(test)]
mod testing {
    use super::*;

    pub fn receive(a: &mut Agent, v: Value) -> Vec<Event> {
        let mut bytes = serde_json::to_vec(&v).unwrap();
        bytes.push(b'\n');
        a.feed(&bytes).0
    }

    pub fn sent(a: &mut Agent) -> Vec<Value> {
        let bytes = std::mem::take(a.output());
        bytes.split(|b| *b == b'\n').filter(|s| !s.is_empty()).map(|s| serde_json::from_slice(s).unwrap()).collect()
    }
}

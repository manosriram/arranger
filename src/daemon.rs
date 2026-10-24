//! `arranger serve`: one thread, one mio loop owning every PTY, client and task.

use crate::adapter::{self, Osc};
use crate::background;
use crate::proto::{self, Event, Reply, Request, Ship, TaskInfo};
use crate::pty::{self, Pty};
use crate::review;
use alacritty_terminal::term::TermMode;
use crate::screen::Screen;
use mio::net::{UnixListener, UnixStream};
use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token};
use rusqlite::{Connection, OptionalExtension, params};
use rustix::io::Errno;
use rustix::process::{Pid, Signal};
use signal_hook::consts::{SIGCHLD, SIGINT, SIGTERM};
use std::collections::HashMap;
use std::fs::{self, Permissions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const LISTENER: Token = Token(0);
const CHILD: Token = Token(1);
const STOP: Token = Token(2);
/// Task PTYs use `Token(TASK + id)`; client connections count up from 3.
const TASK: usize = 1 << 40;
/// Commit/PR jobs use `Token(JOB + n)`.
const JOB: usize = 1 << 39;
const LOG_CHUNK: usize = 64 * 1024;
const MAX_OUTBUF: usize = 16 << 20;

struct Task {
    pty: Pty,
    background: Option<background::Agent>,
    screen: Screen,
    /// Raw output not yet compressed into the on-disk log.
    log: Vec<u8>,
    viewers: Vec<Token>,
    osc: Osc,
    activity: Activity,
    /// What put the task in needs-you.
    need: Option<Event>,
    /// Live action line, e.g. "editing src/pty.rs".
    action: String,
    /// Unix seconds when `activity`/`action` last changed.
    since: u64,
    /// Prompts sent mid-turn that the agent is holding until it picks them up.
    queued: usize,
    /// Ignore late status hooks from a cancelled turn until a new prompt starts.
    interrupted: bool,
}

/// What a running agent is up to, derived from its events.
#[derive(Clone, Copy, PartialEq)]
enum Activity {
    Running,
    Retrying,
    NeedsYou,
    Done,
}

impl Activity {
    fn name(self) -> &'static str {
        match self {
            Activity::Running => "running",
            Activity::Retrying => "retrying",
            Activity::NeedsYou => "needs-you",
            Activity::Done => "done",
        }
    }
}

struct Conn {
    stream: UnixStream,
    inbuf: Vec<u8>,
    outbuf: Vec<u8>,
    attached: Option<u64>,
    /// Attached from a terminal: asks are answered in the agent's own UI.
    native: bool,
    watching: bool,
    conversation: Option<u64>,
    writable: bool,
}

/// A commit/PR script running in a task's worktree; its output goes back to `conn`.
struct Job {
    child: std::process::Child,
    out: std::io::PipeReader,
    buf: Vec<u8>,
    conn: Token,
    task: u64,
    /// The worktree's HEAD when the job started.
    head: String,
    ship: Ship,
}

struct Daemon {
    poll: Poll,
    listener: UnixListener,
    conns: HashMap<Token, Conn>,
    tasks: HashMap<u64, Task>,
    /// Hook connections blocked on a human decision: (task, hook connection), oldest first.
    asks: Vec<(u64, Token)>,
    db: Connection,
    zstd: zstd::bulk::Compressor<'static>,
    data: PathBuf,
    exe: PathBuf,
    sigchld: StdUnixStream,
    next_conn: usize,
    max_live: usize,
    /// Some task changed; push the list to watchers after this loop turn.
    dirty: bool,
    jobs: HashMap<usize, Job>,
    next_job: usize,
}

pub fn run() -> io::Result<()> {
    let data = proto::data_dir();
    fs::create_dir_all(data.join("logs"))?;
    fs::create_dir_all(data.join("adapters"))?;
    let sock = proto::socket_path();
    if StdUnixStream::connect(&sock).is_ok() {
        return Err(io::Error::other("daemon already running"));
    }
    let _ = fs::remove_file(&sock);
    let mut listener = UnixListener::bind(&sock)?;
    fs::set_permissions(&sock, Permissions::from_mode(0o600))?;
    fs::write(data.join("daemon.pid"), std::process::id().to_string())?;
    // Don't keep the starting client's directory: if it's deleted later (a removed worktree, a temp
    // dir), anything that resolves paths against the cwd fails, e.g. gix::discover in `create`.
    let data = fs::canonicalize(&data)?;
    std::env::set_current_dir(&data)?;

    let db = Connection::open(data.join("arranger.db")).map_err(io::Error::other)?;
    // Chat streams record an event per token; WAL keeps each insert from forcing a journal sync.
    db.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;").map_err(io::Error::other)?;
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS tasks (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            agent TEXT NOT NULL, repo TEXT NOT NULL, branch TEXT NOT NULL,
            worktree TEXT NOT NULL, state TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS events (
            id INTEGER PRIMARY KEY, task INTEGER NOT NULL, ts INTEGER NOT NULL, event TEXT NOT NULL);
         CREATE INDEX IF NOT EXISTS events_task ON events (task);
         CREATE INDEX IF NOT EXISTS events_task_ts ON events (task, ts);
         UPDATE tasks SET state = 'dead' WHERE state = 'running';
         DELETE FROM tasks WHERE state = 'new';",
    )
    .map_err(io::Error::other)?;
    // Columns added after Phase 1; errors just mean they're already there.
    for col in ["created INTEGER NOT NULL DEFAULT 0", "title TEXT NOT NULL DEFAULT ''", "prompt TEXT NOT NULL DEFAULT ''",
        "base TEXT NOT NULL DEFAULT ''",
        "base_commit TEXT NOT NULL DEFAULT ''",
        "summary TEXT NOT NULL DEFAULT ''",
        "reviewed TEXT NOT NULL DEFAULT ''",
        "shipped TEXT NOT NULL DEFAULT ''",
        "model TEXT NOT NULL DEFAULT ''",
        "updated INTEGER NOT NULL DEFAULT 0",
    ] {
        let _ = db.execute(&format!("ALTER TABLE tasks ADD COLUMN {col}"), []);
    }

    let poll = Poll::new()?;
    poll.registry().register(&mut listener, LISTENER, Interest::READABLE)?;
    let sigchld = signal_pipe(&[SIGCHLD])?;
    let stop = signal_pipe(&[SIGTERM, SIGINT])?;
    poll.registry().register(&mut SourceFd(&sigchld.as_raw_fd()), CHILD, Interest::READABLE)?;
    poll.registry().register(&mut SourceFd(&stop.as_raw_fd()), STOP, Interest::READABLE)?;

    let mut d = Daemon {
        poll,
        listener,
        conns: HashMap::new(),
        tasks: HashMap::new(),
        asks: Vec::new(),
        db,
        zstd: zstd::bulk::Compressor::new(3)?,
        data,
        exe: std::env::current_exe()?,
        sigchld,
        next_conn: 3,
        max_live: std::env::var("ARRANGER_MAX_AGENTS").ok().and_then(|v| v.parse().ok()).unwrap_or(20),
        dirty: false,
        jobs: HashMap::new(),
        next_job: 0,
    };
    d.start_queued();

    let mut events = Events::with_capacity(256);
    loop {
        if let Err(e) = d.poll.poll(&mut events, None) {
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        for ev in &events {
            match ev.token() {
                LISTENER => d.accept(),
                CHILD => d.reap(),
                STOP => {
                    d.shutdown(&sock);
                    return Ok(());
                }
                Token(t) if t >= TASK => {
                    let id = (t - TASK) as u64;
                    if ev.is_writable() { d.flush_agent(id); }
                    if ev.is_readable() { d.read_task(id); }
                },
                Token(t) if t >= JOB => d.read_job(t - JOB),
                t => d.conn_event(t, ev.is_readable(), ev.is_writable()),
            }
        }
        if d.dirty {
            d.dirty = false;
            d.push_tasks();
        }
    }
}

fn signal_pipe(signals: &[i32]) -> io::Result<StdUnixStream> {
    let (read, write) = StdUnixStream::pair()?;
    read.set_nonblocking(true)?;
    write.set_nonblocking(true)?;
    for &sig in signals {
        signal_hook::low_level::pipe::register(sig, write.try_clone()?)?;
    }
    Ok(read)
}

fn git(cmd: &mut Command) -> Result<String, String> {
    let out = cmd.output().map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn git_in(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir);
    cmd
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Logged with every resize so a replay knows the size the output was drawn for. It is the
/// xterm "resize window" sequence, so `zstd -dc log | cat` in a terminal stays faithful too.
fn size_mark(cols: u16, rows: u16) -> Vec<u8> {
    format!("\x1b[8;{rows};{cols}t").into_bytes()
}

/// Concatenated zstd frames are a valid zstd file, so each chunk is compressed on its own.
fn flush_log(data: &Path, id: u64, log: &mut Vec<u8>, zstd: &mut zstd::bulk::Compressor) {
    if log.is_empty() {
        return;
    }
    let path = data.join(format!("logs/{id}.zst"));
    let res = zstd.compress(log).and_then(|frame| {
        fs::OpenOptions::new().create(true).append(true).open(&path)?.write_all(&frame)
    });
    if let Err(e) = res {
        eprintln!("log {}: {e}", path.display());
    }
    log.clear();
}

// ponytail: drops input when the PTY buffer is full; queue it if big pastes into a busy agent get lost
fn write_pty(master: &OwnedFd, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        match rustix::io::write(master, bytes) {
            Ok(n) => bytes = &bytes[n..],
            Err(Errno::INTR) => {}
            Err(_) => return,
        }
    }
}

/// The latest result of each check-like command (tests, builds, linters) the agent ran.
fn checks(events: &[(u64, Event)]) -> Vec<(String, Option<bool>)> {
    const WORDS: &[&str] = &["test", "build", "check", "lint", "clippy", "fmt", "tsc", "eslint", "vet", "bench", "make"];
    let mut out: Vec<(String, Option<bool>)> = Vec::new();
    let mut running: Option<String> = None;
    for (_, e) in events {
        match e {
            Event::ToolStart { kind: proto::ToolKind::Bash, target } => {
                let lower = target.to_lowercase();
                running = WORDS.iter().any(|w| lower.contains(w)).then(|| target.clone());
                if let Some(cmd) = &running {
                    out.retain(|(c, _)| c != cmd);
                    out.push((cmd.clone(), None));
                }
            }
            Event::ToolEnd { ok } => {
                if let Some(cmd) = running.take() {
                    if let Some(c) = out.iter_mut().find(|(c, _)| *c == cmd) {
                        c.1 = Some(*ok);
                    }
                }
            }
            _ => {}
        }
    }
    let skip = out.len().saturating_sub(6);
    out.split_off(skip)
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn e2s(e: impl std::fmt::Display) -> String {
    e.to_string()
}

impl Daemon {
    fn accept(&mut self) {
        while let Ok((mut stream, _)) = self.listener.accept() {
            let t = Token(self.next_conn);
            self.next_conn += 1;
            if self.poll.registry().register(&mut stream, t, Interest::READABLE).is_ok() {
                let conn = Conn {
                    stream,
                    inbuf: Vec::new(),
                    outbuf: Vec::new(),
                    attached: None,
                    native: false,
                    watching: false,
                    conversation: None,
                    writable: false,
                };
                self.conns.insert(t, conn);
            }
        }
    }

    fn conn_event(&mut self, t: Token, readable: bool, writable: bool) {
        if writable {
            self.flush(t);
        }
        if !readable {
            return;
        }
        let mut buf = [0u8; 16 * 1024];
        let mut closed = false;
        while let Some(c) = self.conns.get_mut(&t) {
            match c.stream.read(&mut buf) {
                Ok(0) => {
                    closed = true;
                    break;
                }
                Ok(n) => c.inbuf.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    closed = true;
                    break;
                }
            }
        }
        while let Some(msg) = self.conns.get_mut(&t).and_then(|c| proto::unframe(&mut c.inbuf)) {
            match msg {
                Ok(req) => self.handle(t, req),
                Err(_) => {
                    closed = true;
                    break;
                }
            }
        }
        if closed {
            self.close(t);
        }
    }

    fn handle(&mut self, t: Token, req: Request) {
        let attached = self.conns.get(&t).and_then(|c| c.attached);
        let reply = match req {
            Request::Ping => Reply::Pong,
            Request::Version => Reply::Version(proto::VERSION),
            Request::List { files } => match self.list(files) {
                Ok(tasks) => Reply::Tasks(tasks),
                Err(e) => Reply::Error(e),
            },
            Request::New { agent, repo, base, prompt, model } => match self.create(&agent, &repo, base.as_deref(), prompt.as_deref().unwrap_or(""), model.as_deref().unwrap_or("")) {
                Ok(id) => Reply::Created { id, state: self.state(id) },
                Err(e) => Reply::Error(e),
            },
            Request::Log { id } => match fs::read(self.data.join(format!("logs/{id}.zst"))) {
                Ok(bytes) => Reply::Log(bytes),
                Err(e) => Reply::Error(format!("no log for task {id}: {e}")),
            },
            Request::Review { id } => match self.review(id) {
                Ok(r) => Reply::Review(r),
                Err(e) => Reply::Error(e),
            },
            Request::Revive { id, prompt } => match self.revive(id, &prompt) {
                Ok(()) => Reply::Ok,
                Err(e) => Reply::Error(e),
            },
            Request::AbortMerge { id } => match self.task_paths(id).and_then(|(wt, _)| git(git_in(Path::new(&wt)).args(["merge", "--abort"]))) {
                Ok(_) => Reply::Ok,
                Err(e) => Reply::Error(e),
            },
            Request::TakeSide { id, ours } => {
                let side = if ours { "--ours" } else { "--theirs" };
                let taken = self.task_paths(id).and_then(|(wt, _)| {
                    let (files, _) = review::conflicts(Path::new(&wt))?;
                    if files.is_empty() {
                        return Ok(());
                    }
                    git(git_in(Path::new(&wt)).args(["checkout", side, "--"]).args(&files))?;
                    git(git_in(Path::new(&wt)).arg("add").arg("--").args(&files)).map(drop)
                });
                match taken {
                    Ok(()) => Reply::Ok,
                    Err(e) => Reply::Error(e),
                }
            }
            Request::Revert { id, path, header } => {
                let reverted = self.task_paths(id).and_then(|(wt, base)| review::revert(Path::new(&wt), &base, &path, &header));
                match reverted {
                    Ok(()) => Reply::Ok,
                    Err(e) => Reply::Error(e),
                }
            }
            Request::Conversation { id } => {
                match self.events(id) {
                    Ok(events) => {
                        if let Some(c) = self.conns.get_mut(&t) { c.conversation = Some(id); }
                        Reply::Events(events)
                    }
                    Err(e) => Reply::Error(e),
                }
            }
            Request::Cancel { id } => {
                if let Some(task) = self.tasks.get_mut(&id) {
                    if let Some(agent) = &mut task.background { agent.cancel(); }
                    else { write_pty(&task.pty.master, b"\x03"); }
                    self.flush_agent(id);
                    self.interrupt(id);
                    Reply::Ok
                } else { Reply::Error(format!("task {id} isn't running")) }
            }
            Request::Prompt { id, text } => match self.prompt(id, &text) {
                Ok(()) => Reply::Ok,
                Err(e) => Reply::Error(e),
            },
            Request::Commit { id, message, ship } => match self.commit(t, id, &message, ship) {
                Ok(()) => return, // JobDone comes when the script finishes
                Err(e) => Reply::Error(e),
            },
            Request::Kill { id } => match self.kill(id) {
                Ok(()) => Reply::Ok,
                Err(e) => Reply::Error(e),
            },
            Request::Events { id } => match self.events(id) {
                Ok(events) => Reply::Events(events),
                Err(e) => Reply::Error(e),
            },
            Request::Event { task, event } => {
                self.record(task, event);
                return;
            }
            Request::Ask { task, event } => {
                self.record(task, event);
                if !self.has_native_viewer(task) {
                    self.asks.push((task, t));
                    return;
                }
                Reply::Decision { allow: None, message: String::new() }
            }
            Request::Watch => {
                if let Some(c) = self.conns.get_mut(&t) {
                    c.watching = true;
                }
                match self.list(false) {
                    Ok(tasks) => Reply::Tasks(tasks),
                    Err(e) => Reply::Error(e),
                }
            }
            Request::Answer { id, allow, message } if self.tasks.get(&id).and_then(|t| t.background.as_ref()).is_some_and(|a| a.asking()) => {
                let result = self.tasks.get_mut(&id).unwrap().background.as_mut().unwrap().answer(allow, &message);
                self.flush_agent(id);
                self.refresh_agent_need(id);
                match result { Ok(()) => Reply::Ok, Err(e) => Reply::Error(e) }
            }
            Request::Answer { id, allow, message } => match self.asks.iter().position(|a| a.0 == id) {
                Some(i) => {
                    let (_, hook) = self.asks.remove(i);
                    self.send(hook, &proto::frame(&Reply::Decision { allow: Some(allow), message }));
                    self.resume(id);
                    self.dirty = true;
                    Reply::Ok
                }
                None => Reply::Error(format!("task {id} isn't waiting on a permission ask")),
            },
            Request::Attach { id, cols, rows, native } => {
                self.detach(t);
                if native {
                    self.release_asks(id);
                }
                match self.tasks.get_mut(&id) {
                    Some(task) if task.background.is_some() => Reply::Error("This agent runs in the background; open its conversation in arranger-gui or use arranger events".into()),
                    Some(task) => {
                        task.viewers.push(t);
                        self.resize(id, cols, rows);
                        if let Some(c) = self.conns.get_mut(&t) {
                            c.attached = Some(id);
                            c.native = native;
                        }
                        Reply::Output(self.tasks[&id].screen.snapshot())
                    }
                    None => Reply::Error(format!("task {id} is not running")),
                }
            }
            Request::Input(bytes) => {
                if let Some(id) = attached {
                    if let Some(task) = self.tasks.get(&id) {
                        write_pty(&task.pty.master, &bytes);
                    }
                    if is_interrupt(&bytes) {
                        self.interrupt(id);
                    } else {
                        self.resume(id);
                    }
                }
                return;
            }
            Request::Resize { cols, rows } => {
                if let Some(id) = attached {
                    self.resize(id, cols, rows);
                }
                return;
            }
        };
        self.send(t, &proto::frame(&reply));
    }

    fn prompt(&mut self, id: u64, text: &str) -> Result<(), String> {
        if !self.tasks.contains_key(&id) { return self.revive(id, text); }
        let task = self.tasks.get_mut(&id).unwrap();
        if let Some(agent) = &mut task.background {
            agent.prompt(text.into())?;
            self.record(id, Event::Prompt { text: text.into() });
            self.flush_agent(id);
            self.refresh_agent_need(id);
        } else {
            let mut bytes = if task.screen.mode().contains(TermMode::BRACKETED_PASTE) {
                format!("\x1b[200~{text}\x1b[201~").into_bytes()
            } else { text.replace('\n', "\r").into_bytes() };
            bytes.push(b'\r');
            write_pty(&task.pty.master, &bytes);
            if task.activity == Activity::Running {
                task.queued += 1; task.action = queued_label(task.queued); task.since = now(); self.dirty = true;
            }
            self.resume(id);
        }
        Ok(())
    }

    fn refresh_agent_need(&mut self, id: u64) {
        if let Some(need) = self.tasks.get(&id).and_then(|t| t.background.as_ref()).and_then(|a| a.pending_need()) {
            self.record(id, need);
        } else { self.resume(id); }
        self.dirty = true;
    }

    fn flush_agent(&mut self, id: u64) {
        let Some(task) = self.tasks.get_mut(&id) else { return };
        let Some(agent) = &mut task.background else { return };
        while !agent.output.is_empty() {
            match rustix::io::write(&task.pty.master, &agent.output) {
                Ok(0) => break,
                Ok(n) => { agent.output.drain(..n); }
                Err(Errno::INTR) => continue,
                Err(Errno::AGAIN) => break,
                Err(_) => { agent.output.clear(); break; }
            }
        }
        let interest = if agent.output.is_empty() { Interest::READABLE } else { Interest::READABLE | Interest::WRITABLE };
        let _ = self.poll.registry().reregister(&mut SourceFd(&task.pty.master.as_raw_fd()), Token(TASK + id as usize), interest);
    }

    fn send(&mut self, t: Token, frame: &[u8]) {
        let Some(c) = self.conns.get_mut(&t) else { return };
        c.outbuf.extend_from_slice(frame);
        if c.outbuf.len() > MAX_OUTBUF {
            // ponytail: a viewer this far behind is dropped; it can reattach for a fresh snapshot
            self.close(t);
            return;
        }
        self.flush(t);
    }

    fn flush(&mut self, t: Token) {
        let Some(c) = self.conns.get_mut(&t) else { return };
        while !c.outbuf.is_empty() {
            match c.stream.write(&c.outbuf) {
                Ok(n) => {
                    c.outbuf.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    self.close(t);
                    return;
                }
            }
        }
        let want = !c.outbuf.is_empty();
        if want != c.writable {
            c.writable = want;
            let interest = if want { Interest::READABLE | Interest::WRITABLE } else { Interest::READABLE };
            let _ = self.poll.registry().reregister(&mut c.stream, t, interest);
        }
    }

    fn detach(&mut self, t: Token) {
        if let Some(id) = self.conns.get_mut(&t).and_then(|c| c.attached.take()) {
            if let Some(task) = self.tasks.get_mut(&id) {
                task.viewers.retain(|&v| v != t);
            }
        }
    }

    fn close(&mut self, t: Token) {
        self.detach(t);
        let before = self.asks.len();
        self.asks.retain(|a| a.1 != t);
        self.dirty |= self.asks.len() != before;
        self.conns.remove(&t);
    }

    fn resize(&mut self, id: u64, cols: u16, rows: u16) {
        // alacritty panics below its minimum size (e.g. a 0x0 pty from `script`)
        let (cols, rows) = (cols.max(2), rows.max(1));
        if let Some(task) = self.tasks.get_mut(&id) {
            if task.background.is_some() { return; }
            let _ = pty::resize(&task.pty.master, cols, rows);
            task.screen.resize(cols, rows);
            task.log.extend(size_mark(cols, rows));
        }
    }

    fn read_task(&mut self, id: u64) {
        let mut buf = [0u8; 64 * 1024];
        loop {
            let Some(task) = self.tasks.get_mut(&id) else { return };
            match rustix::io::read(&task.pty.master, &mut buf) {
                Ok(0) => return,
                Ok(n) => self.output(id, &buf[..n]),
                Err(Errno::INTR) => {}
                Err(_) => return, // EAGAIN, or EIO once the agent is gone (SIGCHLD cleans up)
            }
        }
    }

    fn output(&mut self, id: u64, bytes: &[u8]) {
        if let Some(agent) = self.tasks.get_mut(&id).and_then(|t| t.background.as_mut()) {
            let before = agent.session.clone();
            let events = agent.feed(bytes);
            if agent.session != before && !agent.session.is_empty() {
                let _ = fs::write(self.data.join(format!("adapters/session-{id}")), &agent.session);
                // Older terminal sessions use this path too.
                let _ = fs::write(adapter::codex_session(&self.data, id), &agent.session);
            }
            for event in events { self.record(id, event); }
            if let Some(need) = self.tasks.get(&id).and_then(|t| t.background.as_ref()).and_then(|a| a.pending_need()) {
                if self.tasks[&id].need.as_ref() != Some(&need) { self.record(id, need); }
            }
            self.flush_agent(id);
            return;
        }
        let task = self.tasks.get_mut(&id).unwrap();
        task.log.extend_from_slice(bytes);
        if task.log.len() >= LOG_CHUNK {
            flush_log(&self.data, id, &mut task.log, &mut self.zstd);
        }
        let notices = task.osc.feed(bytes);
        if let Some(name) = task.osc.title.take() {
            let _ = self.db.execute("UPDATE tasks SET title = ?1 WHERE id = ?2", params![name, id]);
            self.dirty = true;
        }
        let replies = task.screen.feed(bytes);
        if task.viewers.is_empty() {
            // Attached viewers' own terminals answer queries; answer only when nobody is.
            write_pty(&task.pty.master, &replies);
        }
        let frame = proto::frame(&Reply::Output(bytes.to_vec()));
        for v in task.viewers.clone() {
            self.send(v, &frame);
        }
        for text in notices {
            self.record(id, Event::Attention { text });
        }
    }

    /// Stores an event and updates the task's derived state and action line.
    fn record(&mut self, id: u64, event: Event) {
        // Streamed text leaves the task list unchanged; pushing it per token floods every viewer.
        self.dirty |= !matches!(event, Event::Message { .. } | Event::Usage { .. } | Event::ToolOutput { .. });
        let ts = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        let json = serde_json::to_string(&event).unwrap();
        if let Err(e) = self.db.execute("INSERT INTO events (task, ts, event) VALUES (?1, ?2, ?3)", params![id, ts, json]) {
            eprintln!("task {id}: {e}");
        }
        let frame = proto::frame(&Reply::ChatEvent(ts, event.clone()));
        let viewers: Vec<_> = self.conns.iter().filter(|(_, c)| c.conversation == Some(id)).map(|(&t, _)| t).collect();
        for viewer in viewers { self.send(viewer, &frame); }
        let Some(task) = self.tasks.get_mut(&id) else { return };
        if !accept_after_interrupt(&mut task.interrupted, &event) {
            return;
        }
        let (activity, action) = match &event {
            Event::Summary { text } => {
                let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
                let line: String = line.chars().take(200).collect();
                let _ = self.db.execute("UPDATE tasks SET summary = ?1 WHERE id = ?2", params![line, id]);
                return;
            }
            Event::Message { text, append: true, .. } if task.activity == Activity::Retrying && !text.is_empty() => {
                self.dirty = true;
                (Activity::Running, "working".into())
            }
            Event::Usage { .. } | Event::Message { .. } | Event::ToolOutput { .. } => return,
            // Completion/cleanup events do not erase a provider failure. Only real work or a new
            // prompt can recover it; the process may stay alive while the model is unavailable.
            Event::TurnDone | Event::ToolEnd { .. } if matches!(task.need, Some(Event::Error { .. })) => return,
            Event::Retrying { .. } => (Activity::Retrying, event.describe()),
            // Agents with hooks also ping the terminal when idle; that's not a new ask.
            Event::Attention { .. } if task.activity == Activity::Done => return,
            Event::PermissionRequest { .. } | Event::Question { .. } | Event::Attention { .. } | Event::Error { .. } => {
                (Activity::NeedsYou, event.describe())
            }
            Event::TurnDone => (Activity::Done, event.describe()),
            Event::ToolStart { .. } | Event::TurnStarted => (Activity::Running, event.describe()),
            Event::Prompt { .. } | Event::ToolEnd { .. } => (Activity::Running, "working".into()),
        };
        match &event {
            Event::Prompt { .. } => task.queued = task.queued.saturating_sub(1),
            // Some agents fold queued prompts into the running turn without a prompt event.
            Event::TurnDone => task.queued = 0,
            _ => {}
        }
        let action = if activity == Activity::Running && task.queued > 0 { queued_label(task.queued) } else { action };
        task.need = (activity == Activity::NeedsYou).then(|| event.clone());
        if (task.activity, &task.action) != (activity, &action) {
            task.since = now();
        }
        task.activity = activity;
        task.action = action;
        if let Event::Prompt { text } = &event {
            let _ = self.db.execute("UPDATE tasks SET title = ?1 WHERE id = ?2 AND title = ''", params![text, id]);
        }
    }

    fn has_native_viewer(&self, id: u64) -> bool {
        self.tasks.get(&id).is_some_and(|t| t.viewers.iter().any(|v| self.conns.get(v).is_some_and(|c| c.native)))
    }

    fn push_tasks(&mut self) {
        let watchers: Vec<Token> = self.conns.iter().filter(|(_, c)| c.watching).map(|(&t, _)| t).collect();
        if watchers.is_empty() {
            return;
        }
        let Ok(tasks) = self.list(false) else { return };
        let frame = proto::frame(&Reply::Tasks(tasks));
        for t in watchers {
            self.send(t, &frame);
        }
    }

    /// A human answered (approve/deny or typing): back to running unless more asks wait.
    fn resume(&mut self, id: u64) {
        let waiting = self.asks.iter().any(|a| a.0 == id) || self.tasks.get(&id).and_then(|t| t.background.as_ref()).is_some_and(|a| a.asking());
        if let Some(task) = self.tasks.get_mut(&id) {
            if task.activity == Activity::NeedsYou && !waiting {
                task.activity = Activity::Running;
                task.action = "working".into();
                task.need = None;
                task.since = now();
                self.dirty = true;
            }
        }
    }

    /// The viewer hit Esc or Ctrl-C mid-turn; agents skip their stop hook when interrupted.
    fn interrupt(&mut self, id: u64) {
        if let Some(task) = self.tasks.get_mut(&id) {
            task.interrupted = true;
            if task.activity != Activity::Done {
                task.activity = Activity::Done;
                task.action = "interrupted".into();
                task.need = None;
                task.queued = 0;
                task.since = now();
                self.dirty = true;
            }
        }
    }

    /// Hands pending asks back to the agent undecided, so it shows its own prompt to the viewer.
    fn release_asks(&mut self, id: u64) {
        let frame = proto::frame(&Reply::Decision { allow: None, message: String::new() });
        let (mine, rest): (Vec<_>, Vec<_>) = self.asks.drain(..).partition(|a| a.0 == id);
        self.asks = rest;
        self.dirty |= !mine.is_empty();
        for (_, hook) in mine {
            self.send(hook, &frame);
        }
    }

    fn reap(&mut self) {
        let mut buf = [0u8; 64];
        while matches!((&self.sigchld).read(&mut buf), Ok(n) if n > 0) {}
        let exited: Vec<u64> = self
            .tasks
            .iter_mut()
            .filter_map(|(&id, t)| matches!(t.pty.child.try_wait(), Ok(Some(_))).then_some(id))
            .collect();
        for id in exited {
            self.read_task(id);
            let failed = self.tasks.get_mut(&id).and_then(|t| t.pty.child.try_wait().ok().flatten()).is_some_and(|s| !s.success());
            if failed {
                let path = self.data.join(format!("logs/{id}.stderr"));
                let diagnostic = fs::File::open(path).ok().and_then(|mut f| {
                    use std::io::{Seek, SeekFrom};
                    let len = f.metadata().ok()?.len();
                    let _ = f.seek(SeekFrom::Start(len.saturating_sub(4096)));
                    let mut bytes = Vec::new(); f.take(4096).read_to_end(&mut bytes).ok()?;
                    Some(String::from_utf8_lossy(&bytes).trim().to_string())
                }).unwrap_or_default();
                self.record(id, Event::Error { message: if diagnostic.is_empty() { "Agent exited unsuccessfully".into() } else { diagnostic } });
            }
            self.finish(id, if failed { "failed" } else { "exited" });
        }
        self.start_queued();
    }

    fn finish(&mut self, id: u64, state: &str) {
        let Some(mut task) = self.tasks.remove(&id) else { return };
        flush_log(&self.data, id, &mut task.log, &mut self.zstd);
        let frame = proto::frame(&Reply::Exited);
        for v in task.viewers {
            if let Some(c) = self.conns.get_mut(&v) {
                c.attached = None;
            }
            self.send(v, &frame);
        }
        self.set_state(id, state);
    }

    fn create(&mut self, agent: &str, dir: &Path, base: Option<&str>, prompt: &str, model: &str) -> Result<u64, String> {
        // A repo typed on another machine (`~/src/api`): the daemon's own home.
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        let dir = dir.strip_prefix("~").map_or(dir.to_path_buf(), |rest| home.join(rest));
        let repo = gix::discover(&dir).map_err(e2s)?;
        let top = repo.workdir().ok_or("not a git work tree")?.to_path_buf();
        let base_name = base.map_or_else(|| review::current_branch(&top), str::to_string);
        let base_commit = review::resolve(&top, base.unwrap_or("HEAD"))?;
        let repo_name = top.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        self.db
            .execute(
                "INSERT INTO tasks (agent, repo, branch, worktree, state, created, title, prompt, model)
                 VALUES (?1, ?2, '', '', 'new', ?3, ?4, ?4, ?5)",
                params![agent, top.to_string_lossy(), now_secs(), prompt, model],
            )
            .map_err(e2s)?;
        let id = self.db.last_insert_rowid() as u64;
        let branch = format!("arranger/{id}");
        let wt = self.data.join("worktrees").join(repo_name).join(id.to_string());
        // ponytail: git runs inside the event loop; move it off-loop if large repos stall other agents
        let added = git(git_in(&top).args(["worktree", "add", "-b", &branch]).arg(&wt).arg(&base_commit));
        if let Err(e) = added {
            let _ = self.db.execute("DELETE FROM tasks WHERE id = ?1", [id]);
            return Err(e);
        }
        self.db
            .execute(
                "UPDATE tasks SET branch = ?1, worktree = ?2, state = 'queued', base = ?3, base_commit = ?4 WHERE id = ?5",
                params![branch, wt.to_string_lossy(), base_name, base_commit, id],
            )
            .map_err(e2s)?;
        self.start_queued();
        Ok(id)
    }

    fn start_queued(&mut self) {
        while self.tasks.len() < self.max_live {
            let next = self
                .db
                .query_row("SELECT id FROM tasks WHERE state = 'queued' ORDER BY id LIMIT 1", [], |r| r.get(0))
                .optional();
            let Ok(Some(id)) = next else { break };
            if let Err(e) = self.start(id) {
                eprintln!("task {id}: {e}");
                self.record(id, Event::Error { message: e });
                self.set_state(id, "failed");
            }
        }
    }

    fn start(&mut self, id: u64) -> Result<(), String> {
        self.spawn(id, None)
    }

    /// Brings an ended task's agent back in its worktree, resuming its last session.
    fn revive(&mut self, id: u64, prompt: &str) -> Result<(), String> {
        if self.tasks.contains_key(&id) {
            return Err(format!("task {id} is still running"));
        }
        let state: String = self.db.query_row("SELECT state FROM tasks WHERE id = ?1", [id], |r| r.get(0)).map_err(e2s)?;
        if !matches!(state.as_str(), "exited" | "dead" | "failed") {
            return Err(format!("task {id} is {state}, not ended"));
        }
        self.task_paths(id)?; // its worktree must still be there
        self.spawn(id, Some(prompt))
    }

    /// Launches task `id`'s agent: its first prompt, or with `resume` its last session plus that prompt.
    fn spawn(&mut self, id: u64, resume: Option<&str>) -> Result<(), String> {
        let (agent, repo, wt, prompt, model): (String, String, String, String, String) = self
            .db
            .query_row("SELECT agent, repo, worktree, prompt, model FROM tasks WHERE id = ?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .map_err(e2s)?;
        let prompt = resume.unwrap_or(&prompt);
        let session = resume.map(|_| fs::read_to_string(adapter::codex_session(&self.data, id)).unwrap_or_default());
        let headless = background::supported(&agent);
        let (argv, env) = if headless {
            (background::argv(&agent, &model, resume.is_some()), vec![("ARRANGER_BIN".into(), self.exe.display().to_string())])
        } else { adapter::launch(&agent, prompt, &model, session.as_deref(), &self.exe, &self.data).map_err(e2s)? };
        let saved = if resume.is_some() {
            fs::read_to_string(self.data.join(format!("adapters/session-{id}"))).unwrap_or_else(|_| session.clone().unwrap_or_default())
        } else { String::new() };
        let mut background = headless.then(|| background::Agent::new(&agent, Path::new(&wt), &model, saved));
        if let Some(a) = &mut background { if !prompt.is_empty() { a.prompt(prompt.into())?; } }
        let mut cmd = if Path::new(&wt).join(".arranger/setup").exists() {
            let mut sh = Command::new("sh");
            sh.args(["-c", "./.arranger/setup >&2 && exec \"$0\" \"$@\""]).args(&argv);
            sh
        } else {
            let mut cmd = Command::new(&argv[0]);
            cmd.args(&argv[1..]);
            cmd
        };
        for key in adapter::PARENT_SESSION_ENV {
            cmd.env_remove(key);
        }
        cmd.current_dir(&wt)
            .envs(env)
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env("ARRANGER_TASK", id.to_string())
            .env("ARRANGER_REPO", &repo)
            .env("PORT", (20000 + id % 1000 * 10).to_string()); // PORT..PORT+9 is this task's
        let (cols, rows) = (80, 24);
        let pty = if headless {
            let stderr = fs::File::options().create(true).append(true).open(self.data.join(format!("logs/{id}.stderr"))).map_err(e2s)?;
            pty::spawn_background(cmd, stderr)
        } else { pty::spawn(cmd, cols, rows) }.map_err(|e| format!("{agent}: {e}"))?;
        let token = Token(TASK + id as usize);
        self.poll
            .registry()
            .register(&mut SourceFd(&pty.master.as_raw_fd()), token, Interest::READABLE)
            .map_err(e2s)?;
        let task = Task {
            pty,
            background,
            screen: Screen::new(cols, rows),
            log: size_mark(cols, rows),
            viewers: Vec::new(),
            osc: Osc::named(agent == "claude"), // ponytail: claude only; other agents' titles are status, not names
            activity: if headless && prompt.is_empty() { Activity::Done } else { Activity::Running },
            need: None,
            action: String::new(),
            since: now(),
            queued: 0,
            interrupted: false,
        };
        self.tasks.insert(id, task);
        if headless && !prompt.is_empty() { self.record(id, Event::Prompt { text: prompt.into() }); }
        self.flush_agent(id);
        self.set_state(id, "running");
        Ok(())
    }

    fn kill(&mut self, id: u64) -> Result<(), String> {
        let (repo, wt): (String, String) = self
            .db
            .query_row("SELECT repo, worktree FROM tasks WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()
            .map_err(e2s)?
            .ok_or(format!("no task {id}"))?;
        if let Some(task) = self.tasks.get_mut(&id) {
            let _ = rustix::process::kill_process_group(Pid::from_child(&task.pty.child), Signal::KILL);
            let _ = task.pty.child.wait();
            self.finish(id, "killed");
        }
        if !wt.is_empty() && Path::new(&wt).exists() {
            git(git_in(Path::new(&repo)).args(["worktree", "remove", "--force"]).arg(&wt))?;
        }
        let _ = fs::remove_file(self.data.join(format!("logs/{id}.zst")));
        self.db.execute("DELETE FROM tasks WHERE id = ?1", [id]).map_err(e2s)?;
        self.db.execute("DELETE FROM events WHERE task = ?1", [id]).map_err(e2s)?;
        self.dirty = true;
        self.start_queued();
        Ok(())
    }

    fn list(&self, with_files: bool) -> Result<Vec<TaskInfo>, String> {
        let mut stmt =
            self.db.prepare(
                "SELECT id, agent, branch, worktree, state, title, created, summary, shipped,
                 MAX(created, updated, COALESCE((SELECT MAX(ts) FROM events WHERE task = tasks.id) / 1000, 0))
                 FROM tasks ORDER BY id",
            ).map_err(e2s)?;
        let rows = stmt.query_map([], |r| {
            Ok(TaskInfo {
                id: r.get(0)?,
                agent: r.get(1)?,
                branch: r.get(2)?,
                worktree: r.get(3)?,
                state: r.get(4)?,
                action: String::new(),
                files: Vec::new(),
                title: r.get(5)?,
                created: r.get(6)?,
                need: None,
                asking: false,
                since: 0,
                summary: r.get(7)?,
                shipped: r.get(8)?,
                updated: r.get(9)?,
                machine: 0,
            })
        });
        let mut tasks: Vec<TaskInfo> = rows.and_then(Iterator::collect).map_err(e2s)?;
        for info in &mut tasks {
            if let Some(task) = self.tasks.get(&info.id) {
                info.state = task.activity.name().into();
                info.action = task.action.clone();
                info.need = task.need.clone();
                info.asking = self.asks.iter().any(|a| a.0 == info.id) || task.background.as_ref().is_some_and(|a| a.asking());
                info.since = task.since;
            }
            if with_files {
                info.files = self.files(info.id).map_err(e2s)?;
            }
        }
        Ok(tasks)
    }

    /// Distinct files a task's edit events touched.
    fn files(&self, id: u64) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self.db.prepare(
            "SELECT DISTINCT json_extract(event, '$.ToolStart.target') FROM events
             WHERE task = ?1 AND json_extract(event, '$.ToolStart.kind') = 'Edit' ORDER BY 1",
        )?;
        stmt.query_map([id], |r| r.get(0))?.collect()
    }

    /// (worktree, commit the review diffs against). Tasks from before base_commit was stored fall
    /// back to the fork point.
    fn task_paths(&self, id: u64) -> Result<(String, String), String> {
        let (repo, wt, base_commit, reviewed, base_branch): (String, String, String, String, String) = self
            .db
            .query_row("SELECT repo, worktree, base_commit, reviewed, base FROM tasks WHERE id = ?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .optional()
            .map_err(e2s)?
            .ok_or(format!("no task {id}"))?;
        if !Path::new(&wt).exists() {
            return Err(format!("task {id} has no worktree"));
        }
        // What you committed from Review is done with; the diff shows only what came after.
        let base = if !reviewed.is_empty() {
            reviewed
        } else if base_commit.is_empty() {
            review::fork_point(Path::new(&wt), Path::new(&repo))?
        } else {
            base_commit
        };
        let base = review::diff_base(Path::new(&wt), &base, if base_branch == "HEAD" { "" } else { &base_branch });
        Ok((wt, base))
    }

    fn review(&self, id: u64) -> Result<proto::Review, String> {
        let (wt, base_commit) = self.task_paths(id)?;
        let (branch, base): (String, String) = self
            .db
            .query_row("SELECT branch, base FROM tasks WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(e2s)?;
        let events = self.events(id)?;
        let summary = events.iter().rev().find_map(|(_, e)| match e {
            Event::Summary { text } => Some(text.clone()),
            _ => None,
        });
        let (conflicts, merging) = review::conflicts(Path::new(&wt))?;
        Ok(proto::Review {
            conflicts,
            merging,
            branch,
            base: if base.is_empty() { "HEAD".into() } else { base },
            summary: summary.unwrap_or_default(),
            checks: checks(&events),
            files: review::diff(Path::new(&wt), &base_commit, None)?,
        })
    }

    fn commit(&mut self, conn: Token, id: u64, message: &str, ship: Ship) -> Result<(), String> {
        let (wt, _) = self.task_paths(id)?;
        let (title, base): (String, String) = self
            .db
            .query_row("SELECT title, base FROM tasks WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(e2s)?;
        let message = if message.trim().is_empty() { title.lines().next().unwrap_or("arranger task").to_string() } else { message.to_string() };
        let summary = self.review(id).map(|r| r.summary).unwrap_or_default();
        let base = if base == "HEAD" { String::new() } else { base };
        let (out, write) = std::io::pipe().map_err(e2s)?;
        let child = Command::new("sh")
            .args(["-c", review::COMMIT_SCRIPT, "arranger-commit", &message, match ship { Ship::Commit => "", Ship::Merge => "merge", Ship::Pr => "pr" }, &summary, &base])
            .current_dir(&wt)
            .stdin(std::process::Stdio::null())
            .stdout(write.try_clone().map_err(e2s)?)
            .stderr(write)
            .spawn()
            .map_err(e2s)?; // the Command, and with it our copies of the write end, drop here
        rustix::fs::fcntl_setfl(&out, rustix::fs::OFlags::NONBLOCK).map_err(e2s)?;
        let n = self.next_job;
        self.next_job += 1;
        self.poll.registry().register(&mut SourceFd(&out.as_raw_fd()), Token(JOB + n), Interest::READABLE).map_err(e2s)?;
        let head = review::resolve(Path::new(&wt), "HEAD").unwrap_or_default();
        self.jobs.insert(n, Job { child, out, buf: Vec::new(), conn, task: id, head, ship });
        Ok(())
    }

    fn read_job(&mut self, n: usize) {
        let Some(job) = self.jobs.get_mut(&n) else { return };
        let mut buf = [0u8; 8192];
        loop {
            match (&job.out).read(&mut buf) {
                Ok(0) => break,
                Ok(k) => job.buf.extend_from_slice(&buf[..k]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        // EOF: the script closed its output, so it's exiting.
        let mut job = self.jobs.remove(&n).unwrap();
        let ok = job.child.wait().is_ok_and(|s| s.success());
        let output = String::from_utf8_lossy(&job.buf).trim().to_string();
        // Review now starts from what was committed. A failed merge or PR may still have committed
        // first, so a moved HEAD counts too.
        if let Ok(head) = self.task_paths(job.task).and_then(|(wt, _)| review::resolve(Path::new(&wt), "HEAD")) {
            // Not mid-merge, though: those changes aren't settled yet.
            let merging = self.task_paths(job.task).is_ok_and(|(wt, _)| review::conflicts(Path::new(&wt)).is_ok_and(|c| c.1));
            if (ok || head != job.head) && !merging {
                let _ = self.db.execute("UPDATE tasks SET reviewed = ?1 WHERE id = ?2", params![head, job.task]);
            }
        }
        if ok {
            let shipped = match job.ship {
                Ship::Commit => "committed",
                Ship::Merge => "merged",
                Ship::Pr => "PR",
            };
            let _ = self.db.execute("UPDATE tasks SET shipped = ?1, updated = ?2 WHERE id = ?3", params![shipped, now(), job.task]);
            self.dirty = true;
        }
        self.send(job.conn, &proto::frame(&Reply::JobDone { ok, output }));
    }

    fn events(&self, id: u64) -> Result<Vec<(u64, Event)>, String> {
        let mut stmt = self.db.prepare("SELECT ts, event FROM events WHERE task = ?1 ORDER BY id").map_err(e2s)?;
        let rows = stmt.query_map([id], |r| Ok((r.get::<_, u64>(0)?, r.get::<_, String>(1)?))).map_err(e2s)?;
        let mut events = Vec::new();
        for row in rows {
            let (ts, json) = row.map_err(e2s)?;
            events.push((ts, serde_json::from_str(&json).map_err(e2s)?));
        }
        Ok(events)
    }

    fn state(&self, id: u64) -> String {
        self.db.query_row("SELECT state FROM tasks WHERE id = ?1", [id], |r| r.get(0)).unwrap_or_default()
    }

    fn set_state(&mut self, id: u64, state: &str) {
        self.dirty = true;
        if let Err(e) = self.db.execute("UPDATE tasks SET state = ?1, updated = ?2 WHERE id = ?3", params![state, now(), id]) {
            eprintln!("task {id}: {e}");
        }
    }

    /// Agents get SIGHUP when their PTY masters close as the process exits.
    fn shutdown(&mut self, sock: &Path) {
        for (&id, task) in &mut self.tasks {
            if task.background.is_some() {
                let _ = rustix::process::kill_process_group(Pid::from_child(&task.pty.child), Signal::HUP);
            }
            flush_log(&self.data, id, &mut task.log, &mut self.zstd);
        }
        let _ = fs::remove_file(sock);
        let _ = fs::remove_file(self.data.join("daemon.pid"));
    }
}

/// Esc, Vim's double Esc, or Ctrl-C (not the start of a terminal escape sequence).
fn is_interrupt(bytes: &[u8]) -> bool {
    matches!(bytes, b"\x1b" | b"\x1b\x1b" | b"\x03")
}

/// Keep recording cancelled-turn hooks, but only a new prompt can restart its status.
/// Summary and usage hooks may still update metadata without changing activity.
fn accept_after_interrupt(interrupted: &mut bool, event: &Event) -> bool {
    if matches!(event, Event::Prompt { .. }) {
        *interrupted = false;
    }
    !*interrupted || matches!(event, Event::Summary { .. } | Event::Usage { .. })
}

fn queued_label(n: usize) -> String {
    if n == 1 { "prompt queued".into() } else { format!("{n} prompts queued") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::ToolKind;

    #[test]
    fn interrupted_turn_ignores_late_hooks_until_next_prompt() {
        let mut interrupted = true;
        for event in [
            Event::ToolEnd { ok: false },
            Event::ToolStart { kind: ToolKind::Bash, target: "late tool".into() },
            Event::TurnDone,
            Event::Attention { text: "idle".into() },
        ] {
            assert!(!accept_after_interrupt(&mut interrupted, &event));
            assert!(interrupted);
        }
        assert!(accept_after_interrupt(&mut interrupted, &Event::Summary { text: "cancelled".into() }));
        assert!(interrupted);
        assert!(accept_after_interrupt(&mut interrupted, &Event::Prompt { text: "try again".into() }));
        assert!(!interrupted);
        assert!(accept_after_interrupt(&mut interrupted, &Event::ToolEnd { ok: true }));
    }

    #[test]
    fn checks_keep_the_latest_result() {
        let bash = |cmd: &str| (0, Event::ToolStart { kind: ToolKind::Bash, target: cmd.into() });
        let end = |ok| (0, Event::ToolEnd { ok });
        let events = [bash("cargo test"), end(false), bash("ls"), end(true), bash("cargo build"), end(true), bash("cargo test"), end(true), bash("npm run lint")];
        let want: Vec<(String, Option<bool>)> = vec![("cargo build".into(), Some(true)), ("cargo test".into(), Some(true)), ("npm run lint".into(), None)];
        assert_eq!(checks(&events), want);
    }

    #[test]
    fn interrupt_keys() {
        assert!(is_interrupt(b"\x1b"));
        assert!(is_interrupt(b"\x1b\x1b"));
        assert!(is_interrupt(b"\x03"));
        assert!(!is_interrupt(b"\x1b[A"));
        assert!(!is_interrupt(b"\x1b\x1b[A"));
        assert!(!is_interrupt(b"a"));
    }
}

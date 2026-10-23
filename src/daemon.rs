//! `arranger serve`: one thread, one mio loop owning every PTY, client and task.

use crate::adapter::{self, Osc};
use crate::proto::{self, Event, Reply, Request, TaskInfo};
use crate::pty::{self, Pty};
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
const LOG_CHUNK: usize = 64 * 1024;
const MAX_OUTBUF: usize = 16 << 20;

struct Task {
    pty: Pty,
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
}

/// What a running agent is up to, derived from its events.
#[derive(Clone, Copy, PartialEq)]
enum Activity {
    Running,
    NeedsYou,
    Done,
}

impl Activity {
    fn name(self) -> &'static str {
        match self {
            Activity::Running => "running",
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
    writable: bool,
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

    let db = Connection::open(data.join("arranger.db")).map_err(io::Error::other)?;
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS tasks (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            agent TEXT NOT NULL, repo TEXT NOT NULL, branch TEXT NOT NULL,
            worktree TEXT NOT NULL, state TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS events (
            id INTEGER PRIMARY KEY, task INTEGER NOT NULL, ts INTEGER NOT NULL, event TEXT NOT NULL);
         CREATE INDEX IF NOT EXISTS events_task ON events (task);
         UPDATE tasks SET state = 'dead' WHERE state = 'running';
         DELETE FROM tasks WHERE state = 'new';",
    )
    .map_err(io::Error::other)?;
    // Columns added after Phase 1; errors just mean they're already there.
    for col in ["created INTEGER NOT NULL DEFAULT 0", "title TEXT NOT NULL DEFAULT ''", "prompt TEXT NOT NULL DEFAULT ''"] {
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
                Token(t) if t >= TASK => d.read_task((t - TASK) as u64),
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
            Request::List { files } => match self.list(files) {
                Ok(tasks) => Reply::Tasks(tasks),
                Err(e) => Reply::Error(e),
            },
            Request::New { agent, repo, base, prompt } => match self.create(&agent, &repo, base.as_deref(), prompt.as_deref().unwrap_or("")) {
                Ok(id) => Reply::Created { id, state: self.state(id) },
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
                    self.resume(id);
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
            let _ = pty::resize(&task.pty.master, cols, rows);
            task.screen.resize(cols, rows);
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
        let task = self.tasks.get_mut(&id).unwrap();
        task.log.extend_from_slice(bytes);
        if task.log.len() >= LOG_CHUNK {
            flush_log(&self.data, id, &mut task.log, &mut self.zstd);
        }
        let notices = task.osc.feed(bytes);
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
        let Some(task) = self.tasks.get_mut(&id) else { return };
        self.dirty = true;
        let ts = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        let json = serde_json::to_string(&event).unwrap();
        if let Err(e) = self.db.execute("INSERT INTO events (task, ts, event) VALUES (?1, ?2, ?3)", params![id, ts, json]) {
            eprintln!("task {id}: {e}");
        }
        let (activity, action) = match &event {
            Event::Usage { .. } => return,
            // Agents with hooks also ping the terminal when idle; that's not a new ask.
            Event::Attention { .. } if task.activity == Activity::Done => return,
            Event::PermissionRequest { .. } | Event::Question { .. } | Event::Attention { .. } | Event::Error { .. } => {
                (Activity::NeedsYou, event.describe())
            }
            Event::TurnDone => (Activity::Done, event.describe()),
            Event::ToolStart { .. } => (Activity::Running, event.describe()),
            Event::Prompt { .. } | Event::ToolEnd { .. } => (Activity::Running, "working".into()),
        };
        task.need = (activity == Activity::NeedsYou).then(|| event.clone());
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
        let waiting = self.asks.iter().any(|a| a.0 == id);
        if let Some(task) = self.tasks.get_mut(&id) {
            if task.activity == Activity::NeedsYou && !waiting {
                task.activity = Activity::Running;
                task.action = "working".into();
                task.need = None;
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
            self.finish(id, "exited");
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

    fn create(&mut self, agent: &str, dir: &Path, base: Option<&str>, prompt: &str) -> Result<u64, String> {
        let top = PathBuf::from(git(git_in(dir).args(["rev-parse", "--show-toplevel"]))?);
        let repo_name = top.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        self.db
            .execute(
                "INSERT INTO tasks (agent, repo, branch, worktree, state, created, title, prompt)
                 VALUES (?1, ?2, '', '', 'new', ?3, ?4, ?4)",
                params![agent, top.to_string_lossy(), now_secs(), prompt],
            )
            .map_err(e2s)?;
        let id = self.db.last_insert_rowid() as u64;
        let branch = format!("arranger/{id}");
        let wt = self.data.join("worktrees").join(repo_name).join(id.to_string());
        // ponytail: git runs inside the event loop; move it off-loop if large repos stall other agents
        let added = git(git_in(&top).args(["worktree", "add", "-b", &branch]).arg(&wt).arg(base.unwrap_or("HEAD")));
        if let Err(e) = added {
            let _ = self.db.execute("DELETE FROM tasks WHERE id = ?1", [id]);
            return Err(e);
        }
        self.db
            .execute(
                "UPDATE tasks SET branch = ?1, worktree = ?2, state = 'queued' WHERE id = ?3",
                params![branch, wt.to_string_lossy(), id],
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
                self.set_state(id, "failed");
            }
        }
    }

    fn start(&mut self, id: u64) -> Result<(), String> {
        let (agent, repo, wt, prompt): (String, String, String, String) = self
            .db
            .query_row("SELECT agent, repo, worktree, prompt FROM tasks WHERE id = ?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .map_err(e2s)?;
        let (argv, env) = adapter::launch(&agent, &prompt, &self.exe, &self.data).map_err(e2s)?;
        let mut cmd = if Path::new(&wt).join(".arranger/setup").exists() {
            let mut sh = Command::new("sh");
            sh.args(["-c", "./.arranger/setup && exec \"$0\" \"$@\""]).args(&argv);
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
        let pty = pty::spawn(cmd, cols, rows).map_err(|e| format!("{agent}: {e}"))?;
        let token = Token(TASK + id as usize);
        self.poll
            .registry()
            .register(&mut SourceFd(&pty.master.as_raw_fd()), token, Interest::READABLE)
            .map_err(e2s)?;
        let task = Task {
            pty,
            screen: Screen::new(cols, rows),
            log: Vec::new(),
            viewers: Vec::new(),
            osc: Osc::default(),
            activity: Activity::Running,
            need: None,
            action: String::new(),
        };
        self.tasks.insert(id, task);
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
            self.db.prepare("SELECT id, agent, branch, worktree, state, title, created FROM tasks ORDER BY id").map_err(e2s)?;
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
            })
        });
        let mut tasks: Vec<TaskInfo> = rows.and_then(Iterator::collect).map_err(e2s)?;
        for info in &mut tasks {
            if let Some(task) = self.tasks.get(&info.id) {
                info.state = task.activity.name().into();
                info.action = task.action.clone();
                info.need = task.need.clone();
                info.asking = self.asks.iter().any(|a| a.0 == info.id);
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
        if let Err(e) = self.db.execute("UPDATE tasks SET state = ?1 WHERE id = ?2", params![state, id]) {
            eprintln!("task {id}: {e}");
        }
    }

    /// Agents get SIGHUP when their PTY masters close as the process exits.
    fn shutdown(&mut self, sock: &Path) {
        for (&id, task) in &mut self.tasks {
            flush_log(&self.data, id, &mut task.log, &mut self.zstd);
        }
        let _ = fs::remove_file(sock);
    }
}

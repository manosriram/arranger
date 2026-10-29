//! `arranger serve`: one thread, one mio loop owning every PTY, client and task.

use crate::adapter::{self, Osc};
use crate::chat::Chat;
use crate::review::tree_hash;
use std::os::unix::process::CommandExt;
use crate::headless::{self, Agent};
use crate::proto::{self, Event, PlanLimit, Reply, Request, Ship, TaskInfo};
use crate::pty::{self, Pty};
use crate::review;
use crate::security;
use crate::setup::{self, Plan};
use crate::side;
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
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const LISTENER: Token = Token(0);
// Number the full task history before limiting the displayed runs.

const CHILD: Token = Token(1);
const STOP: Token = Token(2);
/// Task PTYs use `Token(TASK + id)`; client connections count up from 3.
const TASK: usize = 1 << 40;
/// Task shells (the Terminal tab) use `Token(SHELL + id)`.
const SHELL: usize = 1 << 41;
/// Commit/PR jobs use `Token(JOB + n)`.
const JOB: usize = 1 << 39;
const LOG_CHUNK: usize = 64 * 1024;
const MAX_OUTBUF: usize = 16 << 20;
/// Chat patches go out at most this often: a streaming reply is many small deltas.
const CHAT_EVERY: Duration = Duration::from_millis(33);

struct Task {
    /// For a headless task, its stdio socket rather than a terminal.
    pty: Pty,
    headless: Option<Agent>,
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

/// A shell in a task's worktree; it lives until it exits or the task is deleted.
struct Shell {
    pty: Pty,
    screen: Screen,
    viewers: Vec<Token>,
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
    /// `attached` is the task's shell, not its agent.
    shell: bool,
    watching: bool,
    /// Subscribed to this task's chat.
    chat: Option<u64>,
    writable: bool,
    received: bool,
    last_input: Instant,
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
    /// Merge preparation ends before the final fast-forward.
    merge: Option<MergePending>,
}

struct Daemon {
    poll: Poll,
    listener: UnixListener,
    conns: HashMap<Token, Conn>,
    tasks: HashMap<u64, Task>,
    shells: HashMap<u64, Shell>,
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
    stopping: bool,
    /// Headless tasks' conversations, loaded from their logs when first asked for.
    chats: HashMap<u64, Chat>,
    /// When changed chat items next go out to their viewers.
    chat_due: Option<Instant>,
    /// Setup plans being made: one side-run per repository state, and the tasks waiting on it.
    plans: Vec<PlanRun>,
    /// Setup commands running, by task, and when they're out of time.
    setups: HashMap<u64, (std::process::Child, Instant)>,
    next_side: u64,
    summary_due: HashMap<u64, Instant>,
    summaries: HashMap<u64, SummaryRun>,
    /// What the agents last said about their plans' usage, one per (agent, window).
    limits: Vec<PlanLimit>,
    /// `limits` changed since watchers last heard.
    limits_dirty: bool,
    usage_refresh: std::sync::mpsc::SyncSender<()>,
}

struct SummaryRun {
    tree: String,
    side: side::Run,
    deadline: Instant,
}

#[derive(Clone)]
struct MergePending {
    conn: Token,
    tree: String,
    message: String,
    base: String,
}

struct PlanRun {
    repo: String,
    key: String,
    /// The ignored files offered to it: its plan may copy only these.
    candidates: Vec<String>,
    run: side::Run,
    waiting: Vec<u64>,
    deadline: Instant,
}

const PLAN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const SETUP_TIMEOUT: Duration = Duration::from_secs(20 * 60);

pub fn run() -> io::Result<()> {
    let data = proto::data_dir();
    // Set before spawning threads or children. All daemon-created state is private by default.
    rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o077));
    security::private_dir(&data)?;
    security::private_dir(&data.join("logs"))?;
    security::private_dir(&data.join("adapters"))?;
    let sock = proto::socket_path();
    security::private_dir(sock.parent().ok_or_else(|| io::Error::other("socket has no directory"))?)?;
    // Retain the lock for the daemon's lifetime. Concurrent starts must never unlink a live socket.
    let _lock = security::private_file(&sock.with_extension("lock"), false)?;
    rustix::fs::flock(&_lock, rustix::fs::FlockOperation::NonBlockingLockExclusive)
        .map_err(|_| io::Error::other("daemon already running or starting"))?;
    if StdUnixStream::connect(&sock).is_ok() {
        return Err(io::Error::other("daemon already running"));
    }
    match security::check_socket(&sock) {
        Ok(()) => fs::remove_file(&sock)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut listener = UnixListener::bind(&sock)?;
    fs::set_permissions(&sock, Permissions::from_mode(0o600))?;
    security::write_private(&data.join("daemon.pid"), std::process::id().to_string().as_bytes())?;
    // Don't keep the starting client's directory: if it's deleted later (a removed worktree, a temp
    // dir), anything that resolves paths against the cwd fails, e.g. gix::discover in `create`.
    let data = fs::canonicalize(&data)?;
    std::env::set_current_dir(&data)?;

    security::private_file(&data.join("arranger.db"), false)?;
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
    for col in ["created INTEGER NOT NULL DEFAULT 0", "title TEXT NOT NULL DEFAULT ''", "prompt TEXT NOT NULL DEFAULT ''",
        "base TEXT NOT NULL DEFAULT ''",
        "base_commit TEXT NOT NULL DEFAULT ''",
        "summary TEXT NOT NULL DEFAULT ''",
        "reviewed TEXT NOT NULL DEFAULT ''",
        "shipped TEXT NOT NULL DEFAULT ''",
        "model TEXT NOT NULL DEFAULT ''",
        "updated INTEGER NOT NULL DEFAULT 0",
        "cloned_from INTEGER",
        "headless INTEGER NOT NULL DEFAULT 0",
        "effort TEXT NOT NULL DEFAULT ''",
        "mode TEXT NOT NULL DEFAULT ''",
        "setup TEXT NOT NULL DEFAULT ''",
        "setup_detail TEXT NOT NULL DEFAULT ''",
        "start_ask TEXT NOT NULL DEFAULT ''",
        "setup_key TEXT NOT NULL DEFAULT ''",
        "tokens INTEGER NOT NULL DEFAULT 0",
        "cost REAL",
    ] {
        let _ = db.execute(&format!("ALTER TABLE tasks ADD COLUMN {col}"), []);
    }
    // Setup plans per repository and the state of its manifests (`setup::fingerprint`);
    // `approved`: you approved it, so it runs without asking.
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS setup_plans (
            repo TEXT NOT NULL, key TEXT NOT NULL, plan TEXT NOT NULL, approved INTEGER NOT NULL DEFAULT 0,
            created INTEGER NOT NULL, PRIMARY KEY (repo, key));
         UPDATE tasks SET setup = 'failed', setup_detail = 'Interrupted: the daemon stopped while it ran'
            WHERE state = 'setup' AND setup IN ('planning', 'running');
         CREATE TABLE IF NOT EXISTS summaries (task INTEGER NOT NULL, tree TEXT NOT NULL, state TEXT NOT NULL,
            json TEXT NOT NULL DEFAULT '', error TEXT NOT NULL DEFAULT '', updated INTEGER NOT NULL, PRIMARY KEY(task, tree));
         UPDATE summaries SET state = 'error', error = 'Interrupted by daemon restart' WHERE state = 'running';
         CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    )
    .map_err(io::Error::other)?;
    let poll = Poll::new()?;
    poll.registry().register(&mut listener, LISTENER, Interest::READABLE)?;
    let sigchld = signal_pipe(&[SIGCHLD])?;
    let stop = signal_pipe(&[SIGTERM, SIGINT])?;
    poll.registry().register(&mut SourceFd(&sigchld.as_raw_fd()), CHILD, Interest::READABLE)?;
    poll.registry().register(&mut SourceFd(&stop.as_raw_fd()), STOP, Interest::READABLE)?;

    let (usage, usage_refresh) = crate::usage::watch();
    let mut d = Daemon {
        poll,
        listener,
        conns: HashMap::new(),
        tasks: HashMap::new(),
        shells: HashMap::new(),
        asks: Vec::new(),
        db,
        zstd: zstd::bulk::Compressor::new(3)?,
        data,
        exe: std::env::current_exe()?,
        sigchld,
        next_conn: 3,
        max_live: std::env::var("ARRANGER_MAX_AGENTS").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(20).clamp(1, 64),
        dirty: false,
        jobs: HashMap::new(),
        next_job: 0,
        stopping: false,
        chats: HashMap::new(),
        chat_due: None,
        plans: Vec::new(),
        setups: HashMap::new(),
        next_side: 0,
        summary_due: HashMap::new(),
        summaries: HashMap::new(),
        limits: Vec::new(),
        limits_dirty: false,
        usage_refresh,
    };
    d.limits = d.db.query_row("SELECT value FROM settings WHERE key = 'plan_limits'", [], |r| r.get::<_, String>(0)).ok()
        .and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default();
    d.start_queued();

    let mut events = Events::with_capacity(256);
    loop {
        let timeout = Some(d.chat_due.map_or(Duration::from_secs(1), |due| due.saturating_duration_since(Instant::now()).min(Duration::from_secs(1))));
        if let Err(e) = d.poll.poll(&mut events, timeout) {
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        for ev in &events {
            if d.stopping { break; }
            match ev.token() {
                LISTENER => d.accept(),
                CHILD => d.reap(),
                STOP => {
                    d.shutdown(&sock);
                    return Ok(());
                }
                Token(t) if t >= SHELL => d.read_shell((t - SHELL) as u64),
                Token(t) if t >= TASK => {
                    let id = (t - TASK) as u64;
                    if ev.is_writable() {
                        d.flush_agent(id);
                    }
                    if ev.is_readable() || ev.is_read_closed() {
                        d.read_task(id);
                    }
                }
                Token(t) if t >= JOB => d.read_job(t - JOB),
                t => d.conn_event(t, ev.is_readable(), ev.is_writable()),
            }
        }
        if d.stopping {
            d.shutdown(&sock);
            return Ok(());
        }
        d.time_out_setups();
        d.auto_summaries();
        d.reap_summaries();
        for limits in usage.try_iter().filter(|limits| !limits.is_empty()) {
            // /usage is a complete account snapshot, including billing-mode changes.
            d.limits.retain(|l| l.agent != "claude" || limits.iter().any(|new| new.window == l.window));
            for limit in limits { d.set_limit(limit); }
        }
        let expired: Vec<Token> = d.conns.iter().filter(|(_, c)| (!c.received || !c.inbuf.is_empty()) && c.last_input.elapsed() > Duration::from_secs(30)).map(|(&t, _)| t).collect();
        for t in expired { d.close(t); }
        if d.dirty {
            d.dirty = false;
            d.push_tasks();
        }
        if d.chat_due.is_some_and(|due| due <= Instant::now()) {
            d.push_chats();
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

/// Copies the uncommitted edits in worktree `from`, tracked and untracked, into worktree `to`,
/// checked out at the same commit.
fn carry_changes(from: &Path, to: &Path) -> Result<(), String> {
    let diff = git_in(from).args(["diff", "--binary", "HEAD"]).output().map_err(|e| format!("git: {e}"))?;
    if !diff.status.success() {
        return Err(String::from_utf8_lossy(&diff.stderr).trim().to_string());
    }
    if !diff.stdout.is_empty() {
        let mut apply = git_in(to)
            .args(["apply", "--whitespace=nowarn", "-"])
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("git: {e}"))?;
        let wrote = apply.stdin.take().unwrap().write_all(&diff.stdout);
        let out = apply.wait_with_output().map_err(|e| format!("git: {e}"))?;
        if !out.status.success() {
            return Err(format!("can't carry over uncommitted edits: {}", String::from_utf8_lossy(&out.stderr).trim()));
        }
        wrote.map_err(|e| format!("git: {e}"))?;
    }
    let untracked = git_in(from).args(["ls-files", "--others", "--exclude-standard", "-z"]).output().map_err(e2s)?;
    if !untracked.status.success() { return Err(String::from_utf8_lossy(&untracked.stderr).into_owned()); }
    use std::os::unix::ffi::OsStrExt;
    for path in untracked.stdout.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        let path = Path::new(std::ffi::OsStr::from_bytes(path));
        security::copy_worktree_file(from, to, path).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(())
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

/// Task `id`'s output log: what its terminal drew, or a headless task's JSON lines.
fn log_path(data: &Path, id: u64, headless: bool) -> PathBuf {
    data.join(format!("logs/{id}.{}", if headless { "jsonl.zst" } else { "zst" }))
}

/// Concatenated zstd frames are a valid zstd file, so each chunk is compressed on its own.
fn flush_log(path: &Path, log: &mut Vec<u8>, zstd: &mut zstd::bulk::Compressor) {
    if log.is_empty() {
        return;
    }
    let res = zstd.compress(log).and_then(|frame| {
        security::private_file(path, true)?.write_all(&frame)
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

/// PORT..PORT+9 is task `id`'s, for its agent and its setup.
fn task_port(id: u64) -> u64 {
    20000 + id % 1000 * 10
}

/// Up to the last `limit` bytes of a file.
fn read_tail(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    use std::io::Seek;
    let mut f = fs::File::open(path)?;
    let len = f.metadata()?.len();
    f.seek(io::SeekFrom::Start(len.saturating_sub(limit)))?;
    let mut buf = Vec::new();
    f.take(limit).read_to_end(&mut buf)?;
    Ok(buf)
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
            if self.conns.len() >= 128 { continue; }
            let t = Token(self.next_conn);
            self.next_conn += 1;
            if self.poll.registry().register(&mut stream, t, Interest::READABLE).is_ok() {
                let conn = Conn {
                    stream,
                    inbuf: Vec::new(),
                    outbuf: Vec::new(),
                    attached: None,
                    native: false,
                    shell: false,
                    watching: false,
                    chat: None,
                    writable: false,
                    received: false,
                    last_input: Instant::now(),
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
        let mut bad_input = false;
        while let Some(c) = self.conns.get_mut(&t) {
            match c.stream.read(&mut buf) {
                Ok(0) => {
                    closed = true;
                    break;
                }
                Ok(n) => {
                    c.last_input = Instant::now();
                    // Check before allocation; a writer must not fill memory before parsing begins.
                    if c.inbuf.len().saturating_add(n) > (1 << 20) {
                        closed = true;
                        bad_input = true;
                        break;
                    }
                    c.inbuf.extend_from_slice(&buf[..n]);
                    if c.inbuf.len() >= 4 && u32::from_le_bytes(c.inbuf[..4].try_into().unwrap()) > (1 << 20) {
                        closed = true;
                        bad_input = true;
                        break;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    closed = true;
                    break;
                }
            }
        }
        while !bad_input && let Some(msg) = self.conns.get_mut(&t).and_then(|c| proto::unframe(&mut c.inbuf)) {
            match msg {
                Ok(req) => {
                    if let Some(c) = self.conns.get_mut(&t) { c.received = true; }
                    self.handle(t, req);
                    if self.stopping { break; }
                },
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
        let on_shell = self.conns.get(&t).is_some_and(|c| c.shell);
        let reply = match req {
            Request::Paths { repo } => {
                let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
                let repo = repo.strip_prefix("~").map_or(repo.clone(), |rest| home.join(rest));
                match git_in(&repo).args(["ls-files", "--cached", "--others", "--exclude-standard", "-z"]).output() {
                    Ok(out) if out.status.success() => {
                        let mut paths: Vec<String> = out.stdout.split(|b| *b == 0).filter(|p| !p.is_empty())
                            .map(|p| String::from_utf8_lossy(p).into_owned()).collect();
                        paths.sort();
                        paths.dedup();
                        Reply::Paths(paths)
                    }
                    Ok(out) => Reply::Error(String::from_utf8_lossy(&out.stderr).trim().into()),
                    Err(e) => Reply::Error(format!("git: {e}")),
                }
            },
            Request::RefreshUsage => {
                let _ = self.usage_refresh.try_send(());
                self.dirty = true;
                self.limits_dirty = true;
                Reply::Ok
            }
            Request::Ping => Reply::Pong,
            Request::Shutdown => {
                self.stopping = true;
                Reply::Ok
            },
            Request::Chat { id, limit } => match self.chat(id) {
                Ok(chat) => {
                    let total = chat.items.len();
                    let start = total.saturating_sub(limit);
                    let items = chat.items[start..].to_vec();
                    if let Some(c) = self.conns.get_mut(&t) {
                        c.chat = Some(id);
                    }
                    Reply::Chat { start, total, items }
                }
                Err(e) => Reply::Error(e),
            },
            Request::ChatPage { id, end, limit } => match self.chat(id) {
                Ok(chat) => {
                    let total = chat.items.len();
                    let end = end.min(total);
                    let start = end.saturating_sub(limit);
                    Reply::Chat { start, total, items: chat.items[start..end].to_vec() }
                }
                Err(e) => Reply::Error(e),
            },
            Request::Version => Reply::Version(proto::VERSION),
            Request::List { files } => match self.list(files) {
                Ok(tasks) => Reply::Tasks(tasks),
                Err(e) => Reply::Error(e),
            },
            Request::New { agent, repo, base, prompt, model, effort, headless } => match self.create(&agent, &repo, base.as_deref(), prompt.as_deref().unwrap_or(""), model.as_deref().unwrap_or(""), &effort, headless) {
                Ok(id) => Reply::Created { id, state: self.state(id) },
                Err(e) => Reply::Error(e),
            },
            Request::Log { id } => match fs::File::open(self.data.join(format!("logs/{id}.zst"))).and_then(|f| security::read_limited(f, MAX_OUTBUF - 1024)) {
                Ok(bytes) => Reply::Log(bytes),
                Err(e) => Reply::Error(format!("no log for task {id}: {e}")),
            },
            Request::Review { id } => match self.review(id) {
                Ok(r) => Reply::Review(Box::new(r)),
                Err(e) => Reply::Error(e),
            },
            Request::Revive { id, prompt } => match self.revive(id, &prompt) {
                Ok(()) => Reply::Ok,
                Err(e) => Reply::Error(e),
            },
            Request::Clone { id, prompt, model } => match self.clone_task(id, &prompt, model.as_deref()) {
                Ok(id) => Reply::Created { id, state: self.state(id) },
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
            Request::Prompt { id, text } => {
                match self.tasks.get_mut(&id) {
                    Some(task) if task.headless.is_some() => {
                        let agent = task.headless.as_mut().unwrap();
                        let (busy, asking) = (agent.busy(), agent.asking());
                        // Only its terminal has this command: the viewer opens one with it typed.
                        if let Some(command) = agent.terminal(&text).filter(|_| !asking) {
                            self.send(t, &proto::reply_frame(&Reply::Terminal { command }));
                            return;
                        }
                        let events = agent.prompt(text);
                        // Not an answer to a question, and a turn is running: claude gets it next.
                        if busy && !asking && events.is_empty() {
                            task.queued += 1;
                            task.action = queued_label(task.queued);
                            task.since = now();
                            self.dirty = true;
                        }
                        self.agent_events(id, events);
                        Reply::Ok
                    }
                    Some(task) => {
                        let mut bytes = security::paste_text(&text, task.screen.mode().contains(TermMode::BRACKETED_PASTE)).into_bytes();
                        bytes.push(b'\r');
                        write_pty(&task.pty.master, &bytes);
                        // Busy agents hold the prompt until their turn ends.
                        if task.activity == Activity::Running {
                            task.queued += 1;
                            task.action = queued_label(task.queued);
                            task.since = now();
                            self.dirty = true;
                        }
                        self.resume(id);
                        Reply::Ok
                    }
                    None => Reply::Error(format!("task {id} isn't running")),
                }
            },
            Request::Interrupt { id } => match self.tasks.get_mut(&id) {
                Some(task) => {
                    match &mut task.headless {
                        Some(agent) => agent.cancel(),
                        None => write_pty(&task.pty.master, b"\x1b"),
                    }
                    self.stop_turn(id);
                    Reply::Ok
                }
                None => Reply::Error(format!("task {id} isn't running")),
            },
            Request::SetMode { id, mode } => match self.tasks.get_mut(&id).and_then(|t| t.headless.as_mut()) {
                Some(agent) => match agent.set_mode(&mode) {
                    Ok(()) => {
                        self.mode_changed(id);
                        self.flush_agent(id);
                        Reply::Ok
                    }
                    Err(e) => Reply::Error(e),
                },
                None => Reply::Error(format!("task {id} isn't running in a chat")),
            },
            Request::Commit { id, message, ship, force } => match self.commit(t, id, &message, ship, force) {
                Ok(()) => return, // JobDone comes when the script finishes
                Err(e) => Reply::Error(e),
            },
            Request::Setup { id, action } => match self.setup_action(id, action) {
                Ok(()) => Reply::Ok,
                Err(e) => Reply::Error(e),
            },
            Request::SetupLog { id } => match read_tail(&self.data.join(format!("logs/{id}.setup")), 4 << 20) {
                Ok(bytes) => Reply::Log(bytes),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Reply::Log(Vec::new()),
                Err(e) => Reply::Error(format!("no setup log for task {id}: {e}")),
            },
            // Retain retired wire variants so older hooks keep their operation numbers.
            Request::RunChecks { .. } | Request::StopChecks { .. } | Request::Checks { .. }
            | Request::Console { .. } | Request::CheckReport { .. } | Request::AcceptChecks { .. }
            | Request::Setting { .. } => Reply::Error("checks have been removed".into()),
            Request::GenerateSummary { id } => self.start_summary(id).map(|()| Reply::Ok).unwrap_or_else(Reply::Error),
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
                if !self.tasks.contains_key(&task) || self.asks.iter().any(|a| a.1 == t) {
                    self.send(t, &proto::reply_frame(&Reply::Decision { allow: Some(false), message: "no live task or another ask is already pending".into() }));
                    return;
                }
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
                    Ok(tasks) => {
                        self.send(t, &proto::reply_frame(&Reply::Tasks(tasks)));
                        Reply::Limits(self.limits.clone())
                    }
                    Err(e) => Reply::Error(e),
                }
            }
            Request::Answer { id, allow, message } if self.tasks.get(&id).is_some_and(|t| t.headless.as_ref().is_some_and(Agent::asking)) => {
                let result = self.tasks.get_mut(&id).unwrap().headless.as_mut().unwrap().answer(allow, &message);
                self.agent_events(id, Vec::new());
                match result {
                    Ok(()) => Reply::Ok,
                    Err(e) => Reply::Error(e),
                }
            }
            Request::Answer { id, allow, message } => match self.asks.iter().position(|a| a.0 == id) {
                Some(i) => {
                    let (_, hook) = self.asks.remove(i);
                    self.send(hook, &proto::reply_frame(&Reply::Decision { allow: Some(allow), message }));
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
                    // Nothing to draw. Stay attached so the viewer hears when it exits; Esc still interrupts.
                    Some(task) if task.headless.is_some() => {
                        task.viewers.push(t);
                        if let Some(c) = self.conns.get_mut(&t) {
                            (c.attached, c.native) = (Some(id), false);
                        }
                        let text = format!("Task {id} runs headless, without a terminal.\r\n\r\n  arranger events {id}    what it's doing\r\n  arranger send {id} …    message it\r\n  arranger approve {id}   allow what it asks (or deny)\r\n");
                        Reply::Output(text.into_bytes())
                    }
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
            Request::Shell { id, cols, rows } => {
                self.detach(t);
                match self.open_shell(id) {
                    Ok(()) => {
                        self.shells.get_mut(&id).unwrap().viewers.push(t);
                        self.resize_shell(id, cols, rows);
                        if let Some(c) = self.conns.get_mut(&t) {
                            (c.attached, c.shell, c.native) = (Some(id), true, false);
                        }
                        Reply::Output(self.shells[&id].screen.snapshot())
                    }
                    Err(e) => Reply::Error(e),
                }
            }
            Request::Input(bytes) if on_shell => {
                if let Some(shell) = attached.and_then(|id| self.shells.get(&id)) {
                    write_pty(&shell.pty.master, &bytes);
                }
                return;
            }
            Request::Resize { cols, rows } if on_shell => {
                if let Some(id) = attached {
                    self.resize_shell(id, cols, rows);
                }
                return;
            }
            Request::Input(bytes) if attached.is_some_and(|id| self.tasks.get(&id).is_some_and(|t| t.headless.is_some())) => {
                if is_interrupt(&bytes) {
                    let id = attached.unwrap();
                    self.tasks.get_mut(&id).unwrap().headless.as_mut().unwrap().cancel();
                    self.stop_turn(id);
                }
                return;
            }
            Request::Input(bytes) => {
                if let Some(id) = attached {
                    if let Some(task) = self.tasks.get(&id) {
                        write_pty(&task.pty.master, &bytes);
                    }
                    if is_interrupt(&bytes) {
                        self.interrupt(id);
                    } else if !is_menu_move(&bytes) {
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
        self.send(t, &proto::reply_frame(&reply));
    }

    fn send(&mut self, t: Token, frame: &[u8]) {
        let Some(c) = self.conns.get_mut(&t) else { return };
        if c.outbuf.len().saturating_add(frame.len()) > MAX_OUTBUF {
            // ponytail: a viewer this far behind is dropped; it can reattach for a fresh snapshot
            self.close(t);
            return;
        }
        c.outbuf.extend_from_slice(frame);
        self.flush(t);
    }

    fn flush(&mut self, t: Token) {
        let Some(c) = self.conns.get_mut(&t) else { return };
        while !c.outbuf.is_empty() {
            match c.stream.write(&c.outbuf) {
                Ok(0) => {
                    self.close(t);
                    return;
                }
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
        let Some(c) = self.conns.get_mut(&t) else { return };
        let (id, shell) = (c.attached.take(), std::mem::take(&mut c.shell));
        let Some(id) = id else { return };
        let viewers = if shell { self.shells.get_mut(&id).map(|s| &mut s.viewers) } else { self.tasks.get_mut(&id).map(|t| &mut t.viewers) };
        if let Some(viewers) = viewers {
            viewers.retain(|&v| v != t);
        }
    }

    /// Starts task `id`'s shell in its worktree, unless it's already running.
    fn open_shell(&mut self, id: u64) -> Result<(), String> {
        if self.shells.contains_key(&id) {
            return Ok(());
        }
        let (wt, _) = self.task_paths(id)?;
        let repo: String = self.db.query_row("SELECT repo FROM tasks WHERE id = ?1", [id], |r| r.get(0)).map_err(e2s)?;
        let mut cmd = crate::shell::command(Path::new(&wt), Path::new(&repo), &self.data.join("shell")).map_err(e2s)?;
        for key in adapter::PARENT_SESSION_ENV {
            cmd.env_remove(key);
        }
        cmd.env("TERM", "xterm-256color").env("COLORTERM", "truecolor").env("ARRANGER_TASK", id.to_string());
        let (cols, rows) = (80, 24);
        let pty = pty::spawn(cmd, cols, rows).map_err(|e| format!("shell: {e}"))?;
        self.poll
            .registry()
            .register(&mut SourceFd(&pty.master.as_raw_fd()), Token(SHELL + id as usize), Interest::READABLE)
            .map_err(e2s)?;
        self.shells.insert(id, Shell { pty, screen: Screen::new(cols, rows), viewers: Vec::new() });
        Ok(())
    }

    fn resize_shell(&mut self, id: u64, cols: u16, rows: u16) {
        let (cols, rows) = crate::screen::bounded_size(cols, rows);
        if let Some(shell) = self.shells.get_mut(&id) {
            let _ = pty::resize(&shell.pty.master, cols, rows);
            shell.screen.resize(cols, rows);
        }
    }

    fn read_shell(&mut self, id: u64) {
        let mut buf = [0u8; 64 * 1024];
        loop {
            let Some(shell) = self.shells.get_mut(&id) else { return };
            match rustix::io::read(&shell.pty.master, &mut buf) {
                Ok(0) => return,
                Ok(n) => {
                    let replies = shell.screen.feed(&buf[..n]);
                    if shell.viewers.is_empty() {
                        write_pty(&shell.pty.master, &replies);
                    }
                    let frame = proto::reply_frame(&Reply::Output(buf[..n].to_vec()));
                    for v in shell.viewers.clone() {
                        self.send(v, &frame);
                    }
                }
                Err(Errno::INTR) => {}
                Err(_) => return,
            }
        }
    }

    /// Ends task `id`'s shell (already exited, or killed here) and tells its viewers.
    fn close_shell(&mut self, id: u64) {
        let Some(mut shell) = self.shells.remove(&id) else { return };
        let _ = rustix::process::kill_process_group(Pid::from_child(&shell.pty.child), Signal::KILL);
        let _ = shell.pty.child.wait();
        let _ = self.poll.registry().deregister(&mut SourceFd(&shell.pty.master.as_raw_fd()));
        let frame = proto::reply_frame(&Reply::Exited);
        for v in shell.viewers {
            if let Some(c) = self.conns.get_mut(&v) {
                (c.attached, c.shell) = (None, false);
            }
            self.send(v, &frame);
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
        let (cols, rows) = crate::screen::bounded_size(cols, rows);
        if let Some(task) = self.tasks.get_mut(&id).filter(|t| t.headless.is_none()) {
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
        let task = self.tasks.get_mut(&id).unwrap();
        if let Some(agent) = &mut task.headless {
            let (session, mode) = (agent.session().to_string(), agent.mode().to_string());
            let (events, lines) = agent.feed(bytes);
            if agent.failed() {
                let _ = rustix::process::kill_process_group(Pid::from_child(&task.pty.child), Signal::KILL);
            }
            if agent.session() != session {
                let _ = security::write_private(&headless::session_file(&self.data, id), agent.session().as_bytes());
            }
            let mode_changed = agent.mode() != mode;
            task.log.extend_from_slice(&lines);
            if task.log.len() >= LOG_CHUNK {
                flush_log(&log_path(&self.data, id, true), &mut task.log, &mut self.zstd);
            }
            if let Some(chat) = self.chats.get_mut(&id).filter(|_| !lines.is_empty()) {
                chat.feed(&lines);
                self.chat_due.get_or_insert_with(|| Instant::now() + CHAT_EVERY);
            }
            if mode_changed {
                self.mode_changed(id);
            }
            self.agent_events(id, events);
            return;
        }
        task.log.extend_from_slice(bytes);
        if task.log.len() >= LOG_CHUNK {
            flush_log(&log_path(&self.data, id, false), &mut task.log, &mut self.zstd);
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
        let frame = proto::reply_frame(&Reply::Output(bytes.to_vec()));
        for v in task.viewers.clone() {
            self.send(v, &frame);
        }
        for text in notices {
            self.record(id, Event::Attention { text });
        }
    }

    /// Stores an event and updates the task's derived state and action line.
    fn record(&mut self, id: u64, event: Event) {
        // Not this task's doing: the whole plan's.
        if let Event::Limit(limit) = event {
            if self.tasks.contains_key(&id) {
                self.set_limit(limit);
            }
            return;
        }
        // New work invalidates any summary in progress.
        if matches!(event, Event::Prompt { .. }) && self.tasks.contains_key(&id) {
            self.cancel_summary(id);
            self.summary_due.remove(&id);
        }
        let Some(task) = self.tasks.get_mut(&id) else { return };
        self.dirty = true;
        let ts = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        let json = serde_json::to_string(&event).unwrap();
        if let Err(e) = self.db.execute("INSERT INTO events (task, ts, event) VALUES (?1, ?2, ?3)", params![id, ts, json]) {
            eprintln!("task {id}: {e}");
        }
        if !accept_after_interrupt(&mut task.interrupted, &event) {
            return;
        }
        if matches!(event, Event::TurnDone) {
            self.summary_due.insert(id, Instant::now() + Duration::from_secs(2));
        }
        let (activity, action) = match &event {
            Event::Summary { text } => {
                let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
                let line: String = line.chars().take(200).collect();
                let _ = self.db.execute("UPDATE tasks SET summary = ?1 WHERE id = ?2", params![line, id]);
                return;
            }
            Event::Usage { tokens, cost } => {
                let _ = self.db.execute("UPDATE tasks SET tokens = ?1, cost = ?2 WHERE id = ?3", params![tokens, cost, id]);
                return;
            }
            Event::Limit(_) => return,
            // Agents with hooks also ping the terminal when idle; that's not a new ask.
            Event::Attention { .. } if task.activity == Activity::Done => return,
            Event::PermissionRequest { .. } | Event::Question { .. } | Event::Attention { .. } | Event::Error { .. } => {
                (Activity::NeedsYou, event.describe())
            }
            Event::TurnDone => (Activity::Done, event.describe()),
            Event::ToolStart { .. } => (Activity::Running, event.describe()),
            Event::Retrying { .. } => (Activity::Running, event.describe()),
            Event::Prompt { .. } | Event::ToolEnd { .. } => (Activity::Running, "working".into()),
        };
        match &event {
            Event::Prompt { .. } => task.queued = task.queued.saturating_sub(1),
            // Some agents fold queued prompts into the running turn without a prompt event.
            Event::TurnDone => task.queued = 0,
            _ => {}
        }
        let action = if activity == Activity::Running && task.queued > 0 && !matches!(event, Event::Retrying { .. }) { queued_label(task.queued) } else { action };
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

    /// Task `id`'s conversation, read from its log the first time.
    fn chat(&mut self, id: u64) -> Result<&mut Chat, String> {
        if !self.chats.contains_key(&id) {
            let (agent, wt, headless): (String, String, bool) = self
                .db
                .query_row("SELECT agent, worktree, headless FROM tasks WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .optional()
                .map_err(e2s)?
                .ok_or(format!("no task {id}"))?;
            if !headless {
                return Err(format!("task {id} runs in a terminal; it has no chat"));
            }
            let mut chat = Chat::new(&agent, &wt);
            if let Ok(file) = fs::File::open(log_path(&self.data, id, true)) {
                chat.feed(&security::decode_log(file).map_err(e2s)?);
            }
            if let Some(task) = self.tasks.get(&id) {
                chat.feed(&task.log); // not flushed to the file yet
            }
            chat.take_changed();
            self.chats.insert(id, chat);
        }
        Ok(self.chats.get_mut(&id).unwrap())
    }

    /// Sends each chat's changed items to the connections watching it.
    fn push_chats(&mut self) {
        self.chat_due = None;
        let mut frames: Vec<(u64, Vec<u8>)> = Vec::new();
        for (&id, chat) in &mut self.chats {
            for index in chat.take_changed() {
                frames.push((id, proto::reply_frame(&Reply::ChatPatch { index, item: chat.items[index].clone() })));
            }
        }
        for (id, frame) in frames {
            let viewers: Vec<Token> = self.conns.iter().filter(|(_, c)| c.chat == Some(id)).map(|(&t, _)| t).collect();
            for v in viewers {
                self.send(v, &frame);
            }
        }
    }

    /// A note from arranger itself in a headless task's chat, logged like claude's output so it
    /// survives a reload.
    fn chat_notice(&mut self, id: u64, text: &str, error: bool) {
        self.chat_line(id, serde_json::json!({"type":"arranger_notice","text":text,"error":error}));
    }

    /// One of arranger's own lines in a headless task's chat log.
    fn chat_line(&mut self, id: u64, v: serde_json::Value) {
        let mut line = serde_json::to_vec(&v).unwrap();
        line.push(b'\n');
        if let Some(task) = self.tasks.get_mut(&id) {
            task.log.extend_from_slice(&line);
            if task.log.len() >= LOG_CHUNK {
                flush_log(&log_path(&self.data, id, true), &mut task.log, &mut self.zstd);
            }
        }
        if let Some(chat) = self.chats.get_mut(&id) {
            chat.feed(&line);
            self.chat_due.get_or_insert_with(|| Instant::now() + CHAT_EVERY);
        }
    }

    /// Records what a headless agent reported, keeps needs-you in step with what it waits on, and
    /// sends claude whatever it has for it.
    fn agent_events(&mut self, id: u64, events: Vec<Event>) {
        let echoes = self.tasks.get(&id).and_then(|t| t.headless.as_ref()).is_none_or(|a| a.echoes());
        for event in events {
            if let Event::Prompt { text } = &event
                && !echoes {
                self.chat_line(id, serde_json::json!({"type":"arranger_user","text":text}));
            }
            self.record(id, event);
        }
        let Some(task) = self.tasks.get(&id) else { return };
        match task.headless.as_ref().and_then(Agent::pending_need) {
            Some(need) if task.need.as_ref() != Some(&need) => self.record(id, need),
            Some(_) => {}
            // Its ask was answered; an error stays until something new happens.
            None if matches!(task.need, Some(Event::PermissionRequest { .. } | Event::Question { .. })) => self.resume(id),
            None => {}
        }
        self.flush_agent(id);
    }

    /// Keeps a headless agent's new permission mode for its revives and clones, and tells watchers.
    fn mode_changed(&mut self, id: u64) {
        let Some(agent) = self.tasks.get(&id).and_then(|t| t.headless.as_ref()) else { return };
        let _ = self.db.execute("UPDATE tasks SET mode = ?1 WHERE id = ?2", params![agent.mode(), id]);
        self.dirty = true;
    }

    /// Writes what's pending for a headless agent's stdin, watching for room when it's full.
    fn flush_agent(&mut self, id: u64) {
        let Some(task) = self.tasks.get_mut(&id) else { return };
        let Some(agent) = &mut task.headless else { return };
        let output = agent.output();
        while !output.is_empty() {
            match rustix::io::write(&task.pty.master, &*output) {
                Ok(n) => {
                    output.drain(..n);
                }
                Err(Errno::INTR) => {}
                Err(Errno::AGAIN) => break,
                Err(_) => {
                    output.clear(); // it's gone; SIGCHLD cleans up
                    break;
                }
            }
        }
        let interest = if output.is_empty() { Interest::READABLE } else { Interest::READABLE | Interest::WRITABLE };
        let _ = self.poll.registry().reregister(&mut SourceFd(&task.pty.master.as_raw_fd()), Token(TASK + id as usize), interest);
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
        let frame = proto::reply_frame(&Reply::Tasks(tasks));
        let limits = std::mem::take(&mut self.limits_dirty).then(|| proto::reply_frame(&Reply::Limits(self.limits.clone())));
        for t in watchers {
            self.send(t, &frame);
            if let Some(limits) = &limits {
                self.send(t, limits);
            }
        }
    }

    /// Keeps an agent's newest word on one of its plan's windows, for every watcher and restart.
    fn set_limit(&mut self, mut limit: PlanLimit) {
        limit.seen = now();
        match self.limits.iter_mut().find(|l| l.agent == limit.agent && l.window == limit.window) {
            Some(l) => {
                // Rate-limit events often contain status but omit the percentage.
                if limit.used.is_none() { limit.used = l.used; }
                if limit.resets == 0 { limit.resets = l.resets; }
                *l = limit;
            }
            None => self.limits.push(limit),
        }
        let json = serde_json::to_string(&self.limits).unwrap_or_default();
        let _ = self.db.execute("INSERT INTO settings (key, value) VALUES ('plan_limits', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value", [json]);
        self.limits_dirty = true;
        self.dirty = true;
    }

    /// A human answered (approve/deny or typing): back to running unless more asks wait.
    fn resume(&mut self, id: u64) {
        let waiting = self.asks.iter().any(|a| a.0 == id) || self.tasks.get(&id).and_then(|t| t.headless.as_ref()).is_some_and(Agent::asking);
        if let Some(task) = self.tasks.get_mut(&id)
            && task.activity == Activity::NeedsYou && !waiting {
            task.activity = Activity::Running;
            task.action = "working".into();
            task.need = None;
            task.since = now();
            self.dirty = true;
        }
    }

    /// After asking an agent to stop its turn: tells a headless one now, and marks it stopped.
    fn stop_turn(&mut self, id: u64) {
        let busy = self.tasks.get(&id).is_some_and(|t| t.activity != Activity::Done);
        if busy && self.tasks.get(&id).is_some_and(|t| t.headless.is_some()) {
            self.chat_notice(id, "Interrupted", false);
        }
        self.flush_agent(id);
        self.interrupt(id);
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
        let frame = proto::reply_frame(&Reply::Decision { allow: None, message: String::new() });
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
            let task = &mut self.tasks.get_mut(&id).unwrap();
            let failed = task.headless.is_some() && task.pty.child.try_wait().ok().flatten().is_some_and(|s| !s.success());
            if failed {
                // No terminal showed what went wrong; its stderr log did.
                let stderr = fs::read(self.data.join(format!("logs/{id}.stderr"))).unwrap_or_default();
                let tail = String::from_utf8_lossy(&stderr[stderr.len().saturating_sub(2048)..]).trim().to_string();
                let message = if tail.is_empty() { "the agent exited with an error".to_string() } else { tail };
                self.chat_notice(id, &message, true);
                self.record(id, Event::Error { message });
            }
            self.finish(id, if failed { "failed" } else { "exited" });
        }
        self.reap_setups();
        let exited: Vec<u64> = self.shells.iter_mut().filter_map(|(&id, s)| matches!(s.pty.child.try_wait(), Ok(Some(_))).then_some(id)).collect();
        for id in exited {
            self.read_shell(id);
            self.close_shell(id);
        }
        self.start_queued();
    }

    fn finish(&mut self, id: u64, state: &str) {
        if state == "exited" { self.summary_due.insert(id, Instant::now() + Duration::from_secs(2)); }
        let Some(mut task) = self.tasks.remove(&id) else { return };
        flush_log(&log_path(&self.data, id, task.headless.is_some()), &mut task.log, &mut self.zstd);
        let frame = proto::reply_frame(&Reply::Exited);
        for v in task.viewers {
            if let Some(c) = self.conns.get_mut(&v) {
                c.attached = None;
            }
            self.send(v, &frame);
        }
        self.set_state(id, state);
    }

    fn create(&mut self, agent: &str, dir: &Path, base: Option<&str>, prompt: &str, model: &str, effort: &str, headless: bool) -> Result<u64, String> {
        if headless && !headless::supported(agent) {
            return Err(format!("{} can't run headless", adapter::kind(agent)));
        }
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
                "INSERT INTO tasks (agent, repo, branch, worktree, state, created, title, prompt, model, effort, headless)
                 VALUES (?1, ?2, '', '', 'new', ?3, ?4, ?4, ?5, ?6, ?7)",
                params![agent, top.to_string_lossy(), now_secs(), prompt, model, effort, headless],
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
                "UPDATE tasks SET branch = ?1, worktree = ?2, state = 'setup', base = ?3, base_commit = ?4 WHERE id = ?5",
                params![branch, wt.to_string_lossy(), base_name, base_commit, id],
            )
            .map_err(e2s)?;
        self.begin_setup(id);
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
        self.spawn(id, None, false)
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
        self.spawn(id, Some(prompt), false)
    }

    /// A new task on its own worktree and branch, made from task `src`'s HEAD plus its
    /// uncommitted edits, whose agent carries on from a copy of `src`'s session.
    /// Its first message is `prompt` if not empty, and it's named after it; `model` None keeps `src`'s.
    fn clone_task(&mut self, src: u64, ask: &str, new_model: Option<&str>) -> Result<u64, String> {
        let row: Option<(String, String, String, String, String, String, String, String, String, String, bool)> = self
            .db
            .query_row(
                "SELECT agent, repo, worktree, base, base_commit, reviewed, title, prompt, model, effort, headless FROM tasks WHERE id = ?1",
                [src],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?)),
            )
            .optional()
            .map_err(e2s)?;
        let (agent, repo, from, base, base_commit, reviewed, title, prompt, model, effort, headless) = row.ok_or(format!("no task {src}"))?;
        let ask = ask.trim();
        let title = if ask.is_empty() { title } else { ask.to_string() };
        let model = new_model.map_or(model, str::to_string);
        let from = PathBuf::from(from);
        if !from.exists() {
            return Err(format!("task {src} has no worktree"));
        }
        let head = review::resolve(&from, "HEAD")?;
        self.db
            .execute(
                "INSERT INTO tasks (agent, repo, branch, worktree, state, created, title, prompt, model, effort, cloned_from, headless, mode)
                 VALUES (?1, ?2, '', '', 'new', ?3, ?4, ?5, ?6, ?7, ?8, ?9, (SELECT mode FROM tasks WHERE id = ?8))",
                params![agent, repo, now_secs(), title, prompt, model, effort, src, headless],
            )
            .map_err(e2s)?;
        let id = self.db.last_insert_rowid() as u64;
        let branch = format!("arranger/{id}");
        let repo_name = Path::new(&repo).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let wt = self.data.join("worktrees").join(repo_name).join(id.to_string());
        let made = git(git_in(Path::new(&repo)).args(["worktree", "add", "-b", &branch]).arg(&wt).arg(&head)).and_then(|_| {
            carry_changes(&from, &wt)?;
            let copied = if headless {
                headless::copy_session(&agent, &self.data, (src, &from), (id, &wt))
            } else {
                adapter::copy_session(&agent, &self.data, (src, &from), (id, &wt))
            };
            copied.map_err(|e| format!("can't clone task {src}'s session: {e}"))
        });
        if let Err(e) = made {
            if wt.exists() {
                let _ = git(git_in(Path::new(&repo)).args(["worktree", "remove", "--force"]).arg(&wt));
                let _ = git(git_in(Path::new(&repo)).args(["branch", "-D", &branch]));
            }
            let _ = self.db.execute("DELETE FROM tasks WHERE id = ?1", [id]);
            return Err(e);
        }
        self.db
            .execute(
                "UPDATE tasks SET branch = ?1, worktree = ?2, state = 'setup', base = ?3, base_commit = ?4, reviewed = ?5, start_ask = ?6 WHERE id = ?7",
                params![branch, wt.to_string_lossy(), base, base_commit, reviewed, ask, id],
            )
            .map_err(e2s)?;
        self.dirty = true;
        // Its edits came along, its ignored files (dependencies, .env) didn't.
        self.begin_setup(id);
        Ok(id)
    }

    /// Launches task `id`'s agent: its first prompt, or with `resume` its last session plus that prompt.
    /// `fork` carries on in a new session of its own (a cloned task's first start).
    fn spawn(&mut self, id: u64, resume: Option<&str>, fork: bool) -> Result<(), String> {
        let (agent, repo, wt, prompt, model, effort, headless, mode): (String, String, String, String, String, String, bool, String) = self
            .db
            .query_row("SELECT agent, repo, worktree, prompt, model, effort, headless, mode FROM tasks WHERE id = ?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?))
            })
            .map_err(e2s)?;
        let prompt = resume.unwrap_or(&prompt);
        let (argv, env, mut stdio) = if headless {
            // Its session, from the last run; "" (none yet) resumes the latest here.
            let session = resume.map(|_| fs::read_to_string(headless::session_file(&self.data, id)).unwrap_or_default());
            let (argv, agent) = headless::start(&agent, Path::new(&wt), &model, &effort, &mode, session.as_deref().map(str::trim), fork);
            (argv, Vec::new(), Some(agent))
        } else {
            let session = resume.map(|_| fs::read_to_string(adapter::codex_session(&self.data, id)).unwrap_or_default());
            let (argv, env) = adapter::launch(&agent, prompt, &model, &effort, session.as_deref(), fork, &self.exe, &self.data).map_err(e2s)?;
            (argv, env, None)
        };
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        for key in adapter::PARENT_SESSION_ENV {
            cmd.env_remove(key);
        }
        cmd.current_dir(&wt)
            .envs(env)
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env("ARRANGER_TASK", id.to_string())
            .env("ARRANGER_REPO", &repo)
            .env("PORT", task_port(id).to_string());
        let (cols, rows) = (80, 24);
        let pty = if headless {
            let stderr = security::private_file(&self.data.join(format!("logs/{id}.stderr")), true).map_err(e2s)?;
            pty::spawn_stdio(cmd, stderr)
        } else {
            pty::spawn(cmd, cols, rows)
        }
        .map_err(|e| format!("{agent}: {e}"))?;
        // It announces the prompt once the agent has started.
        if let Some(a) = stdio.as_mut().filter(|_| !prompt.trim().is_empty()) {
            a.prompt(prompt.to_string());
        }
        // Resumed with nothing to say (a clone or revive without a prompt), it sits waiting: no hook fires to say so.
        let idle = (resume.is_some() || headless) && prompt.trim().is_empty();
        let token = Token(TASK + id as usize);
        self.poll
            .registry()
            .register(&mut SourceFd(&pty.master.as_raw_fd()), token, Interest::READABLE)
            .map_err(e2s)?;
        let task = Task {
            pty,
            headless: stdio,
            screen: Screen::new(cols, rows),
            log: if headless { Vec::new() } else { size_mark(cols, rows) }, // a headless log is JSON lines
            viewers: Vec::new(),
            osc: Osc::named(agent == "claude"), // ponytail: claude only; other agents' titles are status, not names
            activity: if idle { Activity::Done } else { Activity::Running },
            need: None,
            action: if idle { "idle".into() } else { String::new() },
            since: now(),
            queued: 0,
            interrupted: false,
        };
        self.tasks.insert(id, task);
        if headless {
            self.chat(id)?;
        }
        self.flush_agent(id);
        self.set_state(id, "running");
        Ok(())
    }

    /// Starts task `id`'s setup: its repository's plan, reused or made now, then copy and run.
    /// The task is in state `setup` until it ends; then its agent starts.
    fn begin_setup(&mut self, id: u64) {
        let row = self.db.query_row("SELECT agent, repo, base_commit, model FROM tasks WHERE id = ?1", [id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))
        });
        let Ok((agent, repo, commit, model)) = row else { return };
        let candidates = setup::candidates(Path::new(&repo));
        let commit = if commit.is_empty() { "HEAD".to_string() } else { commit };
        let key = match setup::fingerprint(Path::new(&repo), &commit, &candidates) {
            Ok(key) => key,
            Err(e) => return self.setup_stage(id, "failed", &format!("Couldn't read the repository: {e}")),
        };
        let _ = self.db.execute("UPDATE tasks SET setup_key = ?1 WHERE id = ?2", params![key, id]);
        if let Some((plan, approved)) = self.stored_plan(&repo, &key) {
            return self.planned(id, &plan, approved);
        }
        if let Some(p) = self.plans.iter_mut().find(|p| p.repo == repo && p.key == key) {
            p.waiting.push(id);
            return self.setup_stage(id, "planning", "");
        }
        let Some(backend) = side::backend(&agent) else {
            self.setup_stage(id, "skipped", "No claude or codex to plan the setup with");
            return self.setup_done(id);
        };
        // Use the task's model when its own agent runs the setup.
        let same = matches!((&backend, adapter::kind(&agent)), (side::Backend::Claude, "claude") | (side::Backend::Codex, "codex"));
        let name = format!("setup-{}", self.next_side);
        self.next_side += 1;
        match side::start(backend, Path::new(&repo), &setup::prompt(&candidates), &setup::schema(), if same { &model } else { "" }, &self.data.join("side"), &name) {
            Ok(run) => {
                self.plans.push(PlanRun { repo, key, candidates, run, waiting: vec![id], deadline: Instant::now() + PLAN_TIMEOUT });
                self.setup_stage(id, "planning", "");
            }
            Err(e) => self.setup_stage(id, "failed", &format!("Couldn't start setup: {e}")),
        }
    }

    fn stored_plan(&self, repo: &str, key: &str) -> Option<(Plan, bool)> {
        let row: Option<(String, bool)> = self
            .db
            .query_row("SELECT plan, approved FROM setup_plans WHERE repo = ?1 AND key = ?2", params![repo, key], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()
            .ok()
            .flatten();
        row.and_then(|(plan, approved)| Some((serde_json::from_str(&plan).ok()?, approved)))
    }

    fn setup_stage(&mut self, id: u64, stage: &str, detail: &str) {
        let _ = self.db.execute("UPDATE tasks SET setup = ?1, setup_detail = ?2, updated = ?3 WHERE id = ?4", params![stage, detail, now(), id]);
        self.dirty = true;
    }

    /// Task `id` has a plan: nothing to do, run it, or ask first.
    fn planned(&mut self, id: u64, plan: &Plan, approved: bool) {
        if plan.is_empty() {
            self.setup_stage(id, "ok", &plan.note);
            self.setup_done(id);
        } else if approved || plan.allowed() {
            self.run_setup(id, plan);
        } else {
            self.setup_stage(id, "approval", &plan.describe());
        }
    }

    fn run_setup(&mut self, id: u64, plan: &Plan) {
        let Ok((repo, wt)) = self.db.query_row("SELECT repo, worktree FROM tasks WHERE id = ?1", [id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))) else { return };
        if let Err(e) = setup::copy(Path::new(&repo), Path::new(&wt), &plan.copy) {
            return self.setup_stage(id, "failed", &e);
        }
        if plan.commands.is_empty() {
            self.setup_stage(id, "ok", &plan.note);
            return self.setup_done(id);
        }
        let spawned = security::private_file(&self.data.join(format!("logs/{id}.setup")), false).and_then(|log| {
            log.set_len(0)?;
            use std::os::unix::process::CommandExt;
            Command::new("sh")
                .args(["-c", setup::SCRIPT, "arranger-setup"])
                .args(plan.commands.iter().map(|c| &c.run))
                .current_dir(&wt)
                .stdin(Stdio::null())
                .stdout(log.try_clone()?)
                .stderr(log)
                .env("ARRANGER_TASK", id.to_string())
                .env("ARRANGER_ROOT", &repo)
                .env("ARRANGER_WORKTREE", &wt)
                .env("PORT", task_port(id).to_string())
                .env("FORCE_COLOR", "1")
                .env("CLICOLOR_FORCE", "1")
                .process_group(0)
                .spawn()
        });
        match spawned {
            Ok(child) => {
                self.setups.insert(id, (child, Instant::now() + SETUP_TIMEOUT));
                self.setup_stage(id, "running", &plan.describe());
            }
            Err(e) => self.setup_stage(id, "failed", &format!("Couldn't start the setup: {e}")),
        }
    }

    /// Setup is over (done or skipped): a new task queues for its agent, a clone starts its fork.
    fn setup_done(&mut self, id: u64) {
        let Ok((cloned, ask)) = self.db.query_row("SELECT cloned_from, start_ask FROM tasks WHERE id = ?1", [id], |r| Ok((r.get::<_, Option<u64>>(0)?, r.get::<_, String>(1)?))) else { return };
        if cloned.is_some() {
            // Left ended if it can't start: Revive tries again.
            self.set_state(id, "exited");
            if let Err(e) = self.spawn(id, Some(&ask), true) {
                eprintln!("task {id}: {e}");
            }
        } else {
            self.set_state(id, "queued");
            self.start_queued();
        }
    }

    fn reap_setups(&mut self) {
        let mut i = 0;
        while i < self.plans.len() {
            match self.plans[i].run.child.try_wait() {
                Ok(Some(status)) => {
                    let p = self.plans.remove(i);
                    self.plan_made(p, status.success());
                }
                Ok(None) => i += 1,
                Err(_) => {
                    let p = self.plans.remove(i);
                    self.plan_made(p, false);
                }
            }
        }
        let exited: Vec<(u64, bool)> = self
            .setups
            .iter_mut()
            .filter_map(|(&id, (child, _))| match child.try_wait() {
                Ok(Some(status)) => Some((id, status.success())),
                Ok(None) => None,
                Err(_) => Some((id, false)),
            })
            .collect();
        for (id, ok) in exited {
            self.setups.remove(&id);
            if ok {
                self.setup_stage(id, "ok", "");
                self.setup_done(id);
            } else {
                let log = read_tail(&self.data.join(format!("logs/{id}.setup")), 16 << 10).unwrap_or_default();
                let tail = setup::tail(&log, 4);
                self.setup_stage(id, "failed", if tail.is_empty() { "The setup failed" } else { &tail });
            }
        }
    }

    fn plan_made(&mut self, p: PlanRun, ok: bool) {
        let made = p.run.finish(ok).and_then(|(answer, _cost)| setup::validate(answer, &p.candidates));
        let waiting: Vec<u64> = p.waiting.into_iter().filter(|&id| self.setup_of(id).as_deref() == Some("planning")).collect();
        match made {
            Ok(plan) => {
                let json = serde_json::to_string(&plan).unwrap();
                let _ = self.db.execute(
                    "INSERT OR REPLACE INTO setup_plans (repo, key, plan, approved, created) VALUES (?1, ?2, ?3, 0, ?4)",
                    params![p.repo, p.key, json, now_secs()],
                );
                for id in waiting {
                    self.planned(id, &plan, false);
                }
            }
            Err(e) => {
                for id in waiting {
                    self.setup_stage(id, "failed", &format!("Couldn't plan the setup: {e}"));
                }
            }
        }
    }

    /// The setup stage of a task that's still setting up.
    fn setup_of(&self, id: u64) -> Option<String> {
        self.db.query_row("SELECT setup FROM tasks WHERE id = ?1 AND state = 'setup'", [id], |r| r.get(0)).optional().ok().flatten()
    }

    /// Kills plans and setups that ran out of time; reaping then records the failure.
    fn time_out_setups(&mut self) {
        let now = Instant::now();
        let far = now + Duration::from_secs(3600);
        for p in &mut self.plans {
            if p.deadline <= now {
                let _ = rustix::process::kill_process_group(Pid::from_child(&p.run.child), Signal::KILL);
                p.deadline = far;
            }
        }
        for (&id, (child, deadline)) in &mut self.setups {
            if *deadline <= now {
                if let Ok(mut log) = security::private_file(&self.data.join(format!("logs/{id}.setup")), true) {
                    let _ = writeln!(log, "\n\x1b[31mstopped after {} minutes\x1b[0m", SETUP_TIMEOUT.as_secs() / 60);
                }
                let _ = rustix::process::kill_process_group(Pid::from_child(child), Signal::KILL);
                *deadline = far;
            }
        }
    }

    /// Stops task `id`'s setup if it's running, and takes it off any plan it waits on.
    fn stop_setup(&mut self, id: u64) {
        if let Some((mut child, _)) = self.setups.remove(&id) {
            let _ = rustix::process::kill_process_group(Pid::from_child(&child), Signal::KILL);
            let _ = child.wait();
        }
        for p in &mut self.plans {
            p.waiting.retain(|&w| w != id);
        }
        // A shared setup run is useful only while at least one task still needs its answer.
        self.plans.retain(|p| !p.waiting.is_empty());
    }

    fn setup_action(&mut self, id: u64, action: proto::SetupAction) -> Result<(), String> {
        use proto::SetupAction::*;
        let stage = self.setup_of(id).ok_or(format!("task {id} isn't setting up"))?;
        let (repo, key): (String, String) = self.db.query_row("SELECT repo, setup_key FROM tasks WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?))).map_err(e2s)?;
        match (action, stage.as_str()) {
            (Skip, _) => {
                self.stop_setup(id);
                self.setup_stage(id, "skipped", "");
                self.setup_done(id);
            }
            (Approve, "approval") => {
                let (plan, _) = self.stored_plan(&repo, &key).ok_or("its setup plan is gone; replan it")?;
                self.db.execute("UPDATE setup_plans SET approved = 1 WHERE repo = ?1 AND key = ?2", params![repo, key]).map_err(e2s)?;
                // Every task waiting on this plan goes ahead.
                let mut stmt = self.db.prepare("SELECT id FROM tasks WHERE state = 'setup' AND setup = 'approval' AND repo = ?1 AND setup_key = ?2").map_err(e2s)?;
                let ids: Vec<u64> = stmt.query_map(params![repo, key], |r| r.get(0)).map_err(e2s)?.filter_map(Result::ok).collect();
                drop(stmt);
                for id in ids {
                    self.run_setup(id, &plan);
                }
            }
            (Retry, "failed") => match self.stored_plan(&repo, &key) {
                Some((plan, approved)) => self.planned(id, &plan, approved),
                None => self.begin_setup(id),
            },
            (Replan, "approval" | "failed") => {
                self.db.execute("DELETE FROM setup_plans WHERE repo = ?1 AND key = ?2", params![repo, key]).map_err(e2s)?;
                self.begin_setup(id);
            }
            (action, stage) => return Err(format!("can't {} a setup that is {stage}", format!("{action:?}").to_lowercase())),
        }
        Ok(())
    }

    /// The last command list explicitly supplied by the user, never a generated list.
    /// The task's worktree, once setup has finished.
    fn task_worktree(&self, id: u64) -> Result<String, String> {
        let (state, wt): (String, String) = self
            .db
            .query_row("SELECT state, worktree FROM tasks WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()
            .map_err(e2s)?
            .ok_or(format!("no task {id}"))?;
        if state == "setup" {
            return Err("its worktree is still being set up".into());
        }
        if wt.is_empty() || !Path::new(&wt).exists() {
            return Err(format!("task {id} has no worktree"));
        }
        Ok(wt)
    }

    fn auto_summaries(&mut self) {
        let due: Vec<u64> = self.summary_due.iter().filter(|(_, at)| **at <= Instant::now()).map(|(&id, _)| id).collect();
        for id in due {
            self.summary_due.remove(&id);
            if self.tasks.get(&id).map_or_else(|| self.db.query_row("SELECT state FROM tasks WHERE id = ?1", [id], |r| r.get::<_, String>(0)).is_ok_and(|s| s == "exited"), |t| t.activity == Activity::Done && t.need.is_none() && t.queued == 0 && !t.headless.as_ref().is_some_and(Agent::asking))
                && !self.asks.iter().any(|a| a.0 == id) {
                self.schedule_summary(id);
            }
        }
    }

    fn schedule_summary(&mut self, id: u64) {
        if self.summaries.contains_key(&id) { return; }
        if let Ok(tree) = self.task_worktree(id).and_then(|wt| tree_hash(Path::new(&wt)))
            && self.db.query_row("SELECT state FROM summaries WHERE task = ?1 AND tree = ?2", params![id, tree], |r| r.get::<_, String>(0)).is_ok_and(|state| state == "ready") {
            return;
        }
        if let Err(error) = self.start_summary(id)
            && let Ok(tree) = self.task_worktree(id).and_then(|wt| tree_hash(Path::new(&wt)))
        {
            let _ = self.db.execute("INSERT INTO summaries (task, tree, state, error, updated) VALUES (?1, ?2, 'error', ?3, ?4) ON CONFLICT(task, tree) DO UPDATE SET state = 'error', error = excluded.error, updated = excluded.updated", params![id, tree, error, now_secs()]);
            self.dirty = true;
        }
    }

    fn start_summary(&mut self, id: u64) -> Result<(), String> {
        if self.summaries.contains_key(&id) { return Err("summary is already running".into()); }
        let wt = self.task_worktree(id)?;
        let tree = tree_hash(Path::new(&wt))?;
        let (agent, model, base, headless): (String, String, String, bool) = self.db.query_row(
            "SELECT agent, model, base_commit, headless FROM tasks WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).map_err(e2s)?;
        let backend = side::backend(&agent).ok_or("summary generation is disabled or no agent is installed")?;
        let own = matches!((&backend, adapter::kind(&agent)), (side::Backend::Claude, "claude") | (side::Backend::Codex, "codex"));
        let session = if headless {
            fs::read_to_string(headless::session_file(&self.data, id)).unwrap_or_default()
        } else if adapter::kind(&agent) == "codex" {
            fs::read_to_string(adapter::codex_session(&self.data, id)).unwrap_or_default()
        } else if adapter::kind(&agent) == "claude" {
            fs::read_dir(adapter::claude_sessions(Path::new(&wt))).ok().into_iter().flatten().filter_map(Result::ok)
                .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
                .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
                .and_then(|e| e.path().file_stem().map(|s| s.to_string_lossy().into_owned())).unwrap_or_default()
        } else { String::new() };
        let mut diff = git(git_in(Path::new(&wt)).args(["diff", if base.is_empty() { "HEAD" } else { &base }, &tree]))?;
        if diff.len() > 100 << 10 {
            let cut = (0..=100 << 10).rev().find(|&i| diff.is_char_boundary(i)).unwrap_or(0);
            diff.truncate(cut);
            diff.push_str("\n(diff truncated; read the worktree for more)");
        }
        let context = review::chat_summary(&self.events(id)?);
        let prompt = format!("Summarize the completed work for human review and a commit message. Return JSON matching the schema. Read only; do not edit files, run checks, or send messages. Describe this exact tree {tree}, not intended future work. Keep the overview short: one concise paragraph describing only what the changes do to the existing codebase. Do not include headings, lists, per-file breakdowns, risks, or verification details in the overview. Include a short title and return empty arrays for files and risks. Do not claim verification.\nConversation context:\n{context}\nDiff:\n{diff}");
        let name = format!("summary-{}", self.next_side);
        self.next_side += 1;
        let side = side::start_summary(backend, Path::new(&wt), &prompt, if own { &model } else { "" }, &self.data.join("side"), &name, &agent, if own { session.trim() } else { "" }, &self.exe).map_err(e2s)?;
        self.db.execute("INSERT INTO summaries (task, tree, state, updated) VALUES (?1, ?2, 'running', ?3) ON CONFLICT(task, tree) DO UPDATE SET state = 'running', error = '', updated = excluded.updated", params![id, tree, now_secs()]).map_err(e2s)?;
        self.summaries.insert(id, SummaryRun { tree, side, deadline: Instant::now() + PLAN_TIMEOUT });
        self.dirty = true;
        Ok(())
    }

    fn reap_summaries(&mut self) {
        let done: Vec<(u64, bool)> = self.summaries.iter_mut().filter_map(|(&id, run)| {
            if Instant::now() >= run.deadline {
                let _ = rustix::process::kill_process_group(Pid::from_child(&run.side.child), Signal::KILL);
                return Some((id, false));
            }
            match run.side.child.try_wait() { Ok(Some(s)) => Some((id, s.success())), Ok(None) => None, Err(_) => Some((id, false)) }
        }).collect();
        for (id, ok) in done {
            let run = self.summaries.remove(&id).unwrap();
            let tree = run.tree.clone();
            let result = run.side.finish(ok).and_then(|(v, _)| crate::summary::parse(v, tree.clone(), Vec::new()));
            let current = self.task_worktree(id).and_then(|wt| tree_hash(Path::new(&wt)));
            let result = result.and_then(|s| if current.as_deref() == Ok(tree.as_str()) { Ok(s) } else { Err("Work changed while the summary was generated; regenerate it".into()) });
            let (state, json, error) = match result {
                Ok(summary) => ("ready", serde_json::to_string(&summary).unwrap(), String::new()),
                Err(e) => ("error", String::new(), e),
            };
            let _ = self.db.execute("UPDATE summaries SET state = ?1, json = ?2, error = ?3, updated = ?4 WHERE task = ?5 AND tree = ?6", params![state, json, error, now_secs(), id, tree]);
            self.dirty = true;
        }
    }

    fn cancel_summary(&mut self, id: u64) {
        if let Some(mut run) = self.summaries.remove(&id) {
            let _ = rustix::process::kill_process_group(Pid::from_child(&run.side.child), Signal::KILL);
            let _ = run.side.child.wait();
            let _ = run.side.finish(false);
            let _ = self.db.execute("UPDATE summaries SET state = 'error', error = 'Interrupted by new work' WHERE task = ?1 AND state = 'running'", [id]);
        }
    }

    fn kill(&mut self, id: u64) -> Result<(), String> {
        for n in self.jobs.iter().filter(|(_, j)| j.task == id).map(|(&n, _)| n).collect::<Vec<_>>() {
            if let Some(mut job) = self.jobs.remove(&n) {
                let _ = self.poll.registry().deregister(&mut SourceFd(&job.out.as_raw_fd()));
                let _ = rustix::process::kill_process_group(Pid::from_child(&job.child), Signal::KILL);
                let _ = job.child.wait();
                self.send(job.conn, &proto::reply_frame(&Reply::JobDone { ok: false, output: "Commit cancelled: task deleted".into() }));
            }
        }
        self.cancel_summary(id);
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
        self.close_shell(id);
        self.stop_setup(id);
        self.summary_due.remove(&id);
        let _ = self.db.execute("DELETE FROM summaries WHERE task = ?1", [id]);
        if !wt.is_empty() && Path::new(&wt).exists() {
            git(git_in(Path::new(&repo)).args(["worktree", "remove", "--force"]).arg(&wt))?;
        }
        for log in [format!("logs/{id}.zst"), format!("logs/{id}.jsonl.zst"), format!("logs/{id}.stderr"), format!("logs/{id}.setup")] {
            let _ = fs::remove_file(self.data.join(log));
        }
        let _ = fs::remove_file(headless::session_file(&self.data, id));
        self.chats.remove(&id);
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
                 MAX(created, updated, COALESCE((SELECT MAX(ts) FROM events WHERE task = tasks.id) / 1000, 0)), model, cloned_from, headless, effort, mode, setup, setup_detail, tokens, cost
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
                model: r.get(10)?,
                cloned_from: r.get(11)?,
                headless: r.get(12)?,
                effort: r.get(13)?,
                mode: r.get(14)?,
                setup: r.get(15)?,
                setup_detail: r.get(16)?,
                checks: proto::ChecksSummary::default(),
                summary_state: String::new(),
                tokens: r.get(17)?,
                cost: r.get(18)?,
                machine: 0,
            })
        });
        let mut tasks: Vec<TaskInfo> = rows.and_then(Iterator::collect).map_err(e2s)?;
        drop(stmt);
        for info in &mut tasks {
            info.summary_state = self.db.query_row("SELECT state FROM summaries WHERE task = ?1 ORDER BY updated DESC LIMIT 1", [info.id], |r| r.get(0)).optional().map_err(e2s)?.unwrap_or_default();
            if self.tasks.get(&info.id).is_some_and(|t| t.activity != Activity::Done) && !info.summary_state.is_empty() {
                info.summary_state = "stale".into();
            }
        }
        for info in &mut tasks {
            if info.state == "setup" {
                let needs_you = matches!(info.setup.as_str(), "approval" | "failed");
                info.state = if needs_you { "setup-needs-you" } else { "setting-up" }.into();
                info.action = match info.setup.as_str() {
                    "planning" => "planning setup",
                    "approval" => "setup waits for approval",
                    "failed" => "setup failed",
                    _ => "setting up",
                }
                .into();
            }
            if let Some(task) = self.tasks.get(&info.id) {
                info.state = task.activity.name().into();
                info.action = task.action.clone();
                info.need = task.need.clone();
                info.asking = self.asks.iter().any(|a| a.0 == info.id) || task.headless.as_ref().is_some_and(Agent::asking);
                info.since = task.since;
                if let Some(agent) = &task.headless {
                    info.mode = agent.mode().to_string();
                }
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
        let tree = tree_hash(Path::new(&wt))?;
        let verification = String::new();
        let stored: Option<(String, String, String, String)> = self.db.query_row(
            "SELECT tree, state, json, error FROM summaries WHERE task = ?1 ORDER BY (tree = ?2) DESC, updated DESC LIMIT 1", params![id, tree], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional().map_err(e2s)?;
        let (work_summary, summary_state) = match stored {
            Some((old, state, json, error)) => (serde_json::from_str::<proto::WorkSummary>(&json).ok(), if old != tree { "stale".into() } else if state == "error" { error } else { state }),
            None => (None, "missing".into()),
        };
        let summary = work_summary.as_ref().map(crate::summary::message).unwrap_or_else(|| review::chat_summary(&events));
        let (conflicts, merging) = review::conflicts(Path::new(&wt))?;
        Ok(proto::Review {
            tree: tree.clone(), verification, work_summary, summary_state,
            conflicts,
            merging,
            branch,
            base: if base.is_empty() { "HEAD".into() } else { base },
            summary,
            checks: Vec::new(),
            files: review::diff(Path::new(&wt), &base_commit, None)?,
        })
    }

    fn commit(&mut self, conn: Token, id: u64, message: &str, ship: Ship, _force: bool) -> Result<(), String> {
        if self.jobs.values().any(|j| j.task == id) {
            return Err("a commit or merge is already running".into());
        }
        if ship == Ship::Merge && (self.tasks.get(&id).is_some_and(|t| t.activity != Activity::Done || t.queued > 0)) {
            return Err("wait for the agent to finish before merging".into());
        }
        let data = self.review(id)?;
        let (title, base): (String, String) = self.db.query_row("SELECT title, base FROM tasks WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?))).map_err(e2s)?;
        let message = if !message.trim().is_empty() { message.to_string() }
            else if let Some(s) = data.work_summary.as_ref().filter(|s| s.tree == data.tree && data.summary_state == "ready") { crate::summary::message(s) }
            else { title.lines().next().unwrap_or("arranger task").to_string() };
        let base = if base == "HEAD" { String::new() } else { base };
        if ship != Ship::Commit {
            if base.is_empty() { return Err("no base branch selected".into()); }
            let (wt, _) = self.task_paths(id)?;
            git(git_in(Path::new(&wt)).args(["check-ref-format", "--branch", &base]))?;
            git(git_in(Path::new(&wt)).args(["show-ref", "--verify", &format!("refs/heads/{base}")]))?;
        }
        if ship == Ship::Merge {
            self.cancel_summary(id);
            let pending = MergePending { conn, tree: data.tree.clone(), message: message.clone(), base: base.clone() };
            return self.spawn_commit(conn, id, &message, ship, "prepare", &data.summary, &base, &data.tree, Some(pending));
        }
        self.spawn_commit(conn, id, &message, ship, if ship == Ship::Pr { "pr" } else { "" }, &data.summary, &base, &data.tree, None)
    }

    fn spawn_commit(&mut self, conn: Token, id: u64, message: &str, ship: Ship, mode: &str, summary: &str, base: &str, tree: &str, merge: Option<MergePending>) -> Result<(), String> {
        let (wt, _) = self.task_paths(id)?;
        let (out, write) = std::io::pipe().map_err(e2s)?;
        let child = Command::new("sh")
            .args(["-c", review::COMMIT_SCRIPT, "arranger-commit", message, mode, summary, base, tree])
            .current_dir(&wt).stdin(Stdio::null()).stdout(write.try_clone().map_err(e2s)?).stderr(write)
            .process_group(0).spawn().map_err(e2s)?;
        rustix::fs::fcntl_setfl(&out, rustix::fs::OFlags::NONBLOCK).map_err(e2s)?;
        let n = self.next_job;
        self.next_job += 1;
        self.poll.registry().register(&mut SourceFd(&out.as_raw_fd()), Token(JOB + n), Interest::READABLE).map_err(e2s)?;
        let head = review::resolve(Path::new(&wt), "HEAD").unwrap_or_default();
        self.jobs.insert(n, Job { child, out, buf: Vec::new(), conn, task: id, head, ship, merge });
        Ok(())
    }

    fn prepared_merge(&mut self, id: u64, mut pending: MergePending) -> Result<(), String> {
        let wt = self.task_worktree(id)?;
        let tree = tree_hash(Path::new(&wt))?;
        pending.tree = tree;
        self.finish_merge(id, pending)
    }

    fn finish_merge(&mut self, id: u64, pending: MergePending) -> Result<(), String> {
        let wt = self.task_worktree(id)?;
        if tree_hash(Path::new(&wt))? != pending.tree { return Err("worktree changed before merge; review and retry".into()); }
        self.spawn_commit(pending.conn, id, &pending.message, Ship::Merge, "merge", "", &pending.base, &pending.tree, None)
    }

    fn read_job(&mut self, n: usize) {
        let Some(job) = self.jobs.get_mut(&n) else { return };
        let mut buf = [0u8; 8192];
        loop {
            match (&job.out).read(&mut buf) {
                Ok(0) => break,
                Ok(k) => {
                    let keep = k.min((1usize << 20).saturating_sub(job.buf.len()));
                    job.buf.extend_from_slice(&buf[..keep]);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        // EOF: the script closed its output, so it's exiting.
        let mut job = self.jobs.remove(&n).unwrap();
        let _ = self.poll.registry().deregister(&mut SourceFd(&job.out.as_raw_fd()));
        let ok = job.child.wait().is_ok_and(|s| s.success());
        let output = String::from_utf8_lossy(&job.buf).trim().to_string();
        if ok && let Some(pending) = job.merge.take() {
            if let Err(output) = self.prepared_merge(job.task, pending) {
                self.send(job.conn, &proto::reply_frame(&Reply::JobDone { ok: false, output }));
            }
            return;
        }
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
        self.send(job.conn, &proto::reply_frame(&Reply::JobDone { ok, output }));
    }

    fn events(&self, id: u64) -> Result<Vec<(u64, Event)>, String> {
        let mut stmt = self.db.prepare("SELECT ts, event FROM events WHERE task = ?1 ORDER BY id").map_err(e2s)?;
        let rows = stmt.query_map([id], |r| Ok((r.get::<_, u64>(0)?, r.get::<_, String>(1)?))).map_err(e2s)?;
        let mut events = Vec::new();
        let mut bytes = 0usize;
        for row in rows {
            let (ts, json) = row.map_err(e2s)?;
            bytes = bytes.saturating_add(json.len());
            if bytes > (8 << 20) || events.len() >= 100_000 { return Err("event history exceeds the in-memory limit".into()); }
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

    /// Drop agent owners explicitly: kill their process groups and reap them before returning.
    fn shutdown(&mut self, sock: &Path) {
        for id in self.summaries.keys().copied().collect::<Vec<_>>() { self.cancel_summary(id); }
        for id in self.setups.keys().copied().collect::<Vec<_>>() { self.stop_setup(id); }
        self.plans.clear();
        for job in self.jobs.values_mut() {
            let _ = rustix::process::kill_process_group(Pid::from_child(&job.child), Signal::KILL);
            let _ = job.child.wait();
        }
        for (id, mut task) in self.tasks.drain() {
            flush_log(&log_path(&self.data, id, task.headless.is_some()), &mut task.log, &mut self.zstd);
        }
        self.shells.clear();
        let _ = fs::remove_file(sock);
        let _ = fs::remove_file(self.data.join("daemon.pid"));
    }
}

/// Esc, Vim's double Esc, or Ctrl-C (not the start of a terminal escape sequence).
fn is_interrupt(bytes: &[u8]) -> bool {
    matches!(bytes, b"\x1b" | b"\x1b\x1b" | b"\x03")
}

/// ↑/↓ (either cursor mode): moving through an agent's menu isn't answering it yet.
fn is_menu_move(bytes: &[u8]) -> bool {
    matches!(bytes, b"\x1b[A" | b"\x1b[B" | b"\x1bOA" | b"\x1bOB")
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
    fn interrupt_keys() {
        assert!(is_interrupt(b"\x1b"));
        assert!(is_interrupt(b"\x1b\x1b"));
        assert!(is_interrupt(b"\x03"));
        assert!(!is_interrupt(b"\x1b[A"));
        assert!(!is_interrupt(b"\x1b\x1b[A"));
        assert!(!is_interrupt(b"a"));
        assert!(is_menu_move(b"\x1bOB"));
        assert!(!is_menu_move(b"\r"));
    }
}

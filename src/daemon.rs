//! `arranger serve`: one thread, one mio loop owning every PTY, client and task.

use crate::adapter::{self, Osc};
use crate::chat::Chat;
use crate::checks;
use std::os::unix::process::CommandExt;
use crate::console::Console;
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
const CHECK_RUNS_QUERY: &str = "SELECT id, started, ended, tree, state, auto, note, accepted,
    ROW_NUMBER() OVER (ORDER BY id) AS number
    FROM check_runs WHERE task = ?1 ORDER BY id DESC LIMIT 30";
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
    /// Merge preparation ends before checks and the final fast-forward.
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
    /// Check runs under way, by task.
    passes: HashMap<u64, Pass>,
    /// Check output parsed so far, by check, and how many bytes of its log that took.
    consoles: HashMap<u64, (Console, u64)>,
    /// Check planners under way, by task.
    check_plans: HashMap<u64, CheckPlan>,
    /// Tasks whose turn just ended, and when to see whether it needs checks.
    auto_due: HashMap<u64, Instant>,
    summaries: HashMap<u64, SummaryRun>,
    merges: HashMap<u64, MergePending>,
    /// What the agents last said about their plans' usage, one per (agent, window).
    limits: Vec<PlanLimit>,
    /// `limits` changed since watchers last heard.
    limits_dirty: bool,
}

struct SummaryRun {
    tree: String,
    verified_by: Vec<String>,
    side: side::Run,
    deadline: Instant,
}

#[derive(Clone)]
struct MergePending {
    conn: Token,
    tree: String,
    message: String,
    base: String,
    force: bool,
    checks: Vec<proto::CheckSpec>,
}

/// A side-run planning a task's checks after its turn.
struct CheckPlan {
    run: u64,
    side: side::Run,
    deadline: Instant,
    repo: String,
    /// Files changed since the work last passed: whether no checks means unverified.
    changed: Vec<String>,
}

/// How long after a turn ends its checks are planned: claude may carry on with a queued prompt.
const AUTO_AFTER: Duration = Duration::from_secs(2);

/// A task's check run: the checks still to go, and the one running.
struct Pass {
    run: u64,
    queue: std::collections::VecDeque<u64>,
    current: Option<Running>,
}

struct Running {
    check: u64,
    child: std::process::Child,
    deadline: Instant,
    /// Why arranger stopped it (out of time, too much output), if it did.
    cut: Option<&'static str>,
}

const CHECK_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const CHECK_LOG_MAX: u64 = 32 << 20;

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
    security::private_dir(&data.join("logs/checks"))?;
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
         CREATE TABLE IF NOT EXISTS check_runs (
            id INTEGER PRIMARY KEY AUTOINCREMENT, task INTEGER NOT NULL, started INTEGER NOT NULL,
            ended INTEGER NOT NULL DEFAULT 0, tree TEXT NOT NULL, state TEXT NOT NULL);
         CREATE INDEX IF NOT EXISTS check_runs_task ON check_runs (task);
         CREATE TABLE IF NOT EXISTS checks (
            id INTEGER PRIMARY KEY AUTOINCREMENT, run INTEGER NOT NULL, task INTEGER NOT NULL, pos INTEGER NOT NULL,
            name TEXT NOT NULL, command TEXT NOT NULL, why TEXT NOT NULL, state TEXT NOT NULL, exit INTEGER,
            started INTEGER NOT NULL DEFAULT 0, ended INTEGER NOT NULL DEFAULT 0);
         CREATE INDEX IF NOT EXISTS checks_run ON checks (run);
         CREATE TABLE IF NOT EXISTS summaries (task INTEGER NOT NULL, tree TEXT NOT NULL, state TEXT NOT NULL,
            json TEXT NOT NULL DEFAULT '', error TEXT NOT NULL DEFAULT '', updated INTEGER NOT NULL, PRIMARY KEY(task, tree));
         UPDATE summaries SET state = 'error', error = 'Interrupted by daemon restart' WHERE state = 'running';
         CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS approved_checks (repo TEXT NOT NULL, command TEXT NOT NULL, PRIMARY KEY (repo, command));
         CREATE TABLE IF NOT EXISTS dropped_checks (task INTEGER NOT NULL, command TEXT NOT NULL, PRIMARY KEY (task, command));",
    )
    .map_err(io::Error::other)?;
    for col in ["tasks ADD COLUMN fix_rounds INTEGER NOT NULL DEFAULT 0", "check_runs ADD COLUMN accepted INTEGER NOT NULL DEFAULT 0", "check_runs ADD COLUMN auto INTEGER NOT NULL DEFAULT 0", "check_runs ADD COLUMN note TEXT NOT NULL DEFAULT ''",
        "checks ADD COLUMN origin TEXT NOT NULL DEFAULT 'manual'", "checks ADD COLUMN note TEXT NOT NULL DEFAULT ''"] {
        let _ = db.execute(&format!("ALTER TABLE {col}"), []);
    }
    // Checks that were running or being planned when the daemon stopped. Approvals still wait.
    db.execute_batch(
        "UPDATE checks SET state = 'stopped', ended = CAST(strftime('%s', 'now') AS INTEGER) WHERE state = 'running'
            OR (state = 'pending' AND run IN (SELECT id FROM check_runs WHERE state = 'running'));
         UPDATE check_runs SET state = 'stopped', ended = CAST(strftime('%s', 'now') AS INTEGER) WHERE state = 'running';
         UPDATE check_runs SET state = 'error', note = 'Interrupted: the daemon stopped while it planned', ended = CAST(strftime('%s', 'now') AS INTEGER)
            WHERE state = 'planning';",
    )
    .map_err(io::Error::other)?;

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
        passes: HashMap::new(),
        consoles: HashMap::new(),
        check_plans: HashMap::new(),
        auto_due: HashMap::new(),
        summaries: HashMap::new(),
        merges: HashMap::new(),
        limits: Vec::new(),
        limits_dirty: false,
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
        d.time_out_checks();
        d.auto_checks();
        d.reap_summaries();
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
                let _ = self.db.execute("UPDATE tasks SET fix_rounds = 0 WHERE id = ?1", [id]);
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
            Request::RunChecks { id, checks } => match self.run_checks(id, checks) {
                Ok(_) => Reply::Ok,
                Err(e) => Reply::Error(e),
            },
            Request::StopChecks { id } => match self.stop_checks(id) {
                Ok(()) => Reply::Ok,
                Err(e) => Reply::Error(e),
            },
            Request::Checks { id } => match self.check_runs(id) {
                Ok(runs) => Reply::Checks(runs),
                Err(e) => Reply::Error(e),
            },
            Request::Console { check, start, limit } => self.console(check, start, limit).unwrap_or_else(Reply::Error),
            Request::ApproveChecks { id, always } => match self.approve_checks(id, always) {
                Ok(()) => Reply::Ok,
                Err(e) => Reply::Error(e),
            },
            Request::GenerateSummary { id } => self.start_summary(id).map(|()| Reply::Ok).unwrap_or_else(Reply::Error),
            Request::CheckReport { id } => self.check_report(id).map(Reply::Text).unwrap_or_else(Reply::Error),
            Request::AcceptChecks { id } => self.accept_checks(id).map(|()| Reply::Ok).unwrap_or_else(Reply::Error),
            Request::Setting { key, value } => self.setting(&key, value.as_deref()).map(Reply::Text).unwrap_or_else(Reply::Error),
            Request::DropCheck { id, command } => match self.db.execute("INSERT OR IGNORE INTO dropped_checks (task, command) VALUES (?1, ?2)", params![id, command]) {
                Ok(_) => Reply::Ok,
                Err(e) => Reply::Error(e2s(e)),
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
        // Another turn: checks of the last one no longer judge the work, and mustn't run under it.
        if matches!(event, Event::Prompt { .. }) && self.tasks.contains_key(&id) {
            self.cancel_summary(id);
            self.auto_due.remove(&id);
            self.end_checks(id, "stale");
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
            self.auto_due.insert(id, Instant::now() + AUTO_AFTER);
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
            Some(l) => *l = limit,
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
        self.reap_checks();
        self.reap_check_plans();
        let exited: Vec<u64> = self.shells.iter_mut().filter_map(|(&id, s)| matches!(s.pty.child.try_wait(), Ok(Some(_))).then_some(id)).collect();
        for id in exited {
            self.read_shell(id);
            self.close_shell(id);
        }
        self.start_queued();
    }

    fn finish(&mut self, id: u64, state: &str) {
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
        let Some(planner) = side::planner(&agent) else {
            self.setup_stage(id, "skipped", "No claude or codex to plan the setup with");
            return self.setup_done(id);
        };
        // Its own model when the planner is its own agent.
        let same = matches!((&planner, adapter::kind(&agent)), (side::Planner::Claude, "claude") | (side::Planner::Codex, "codex"));
        let name = format!("setup-{}", self.next_side);
        self.next_side += 1;
        match side::start(planner, Path::new(&repo), &setup::prompt(&candidates), &setup::schema(), if same { &model } else { "" }, &self.data.join("side"), &name) {
            Ok(run) => {
                self.plans.push(PlanRun { repo, key, candidates, run, waiting: vec![id], deadline: Instant::now() + PLAN_TIMEOUT });
                self.setup_stage(id, "planning", "");
            }
            Err(e) => self.setup_stage(id, "failed", &format!("Couldn't start the planner: {e}")),
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

    /// Runs `specs` on task `id`'s worktree as it is now, at your asking.
    fn run_checks(&mut self, id: u64, specs: Vec<proto::CheckSpec>) -> Result<u64, String> {
        if self.passes.contains_key(&id) || self.check_plans.contains_key(&id) || self.jobs.values().any(|j| j.task == id) || self.merges.contains_key(&id) {
            return Err("checks are already running; stop them first".into());
        }
        if specs.is_empty() {
            self.plan_checks(id, true)?;
            return self.check_runs(id)?.first().map(|r| r.id).ok_or("no check planner is available".into());
        }
        let wt = self.checkable_worktree(id)?;
        let specs = checks::validate(specs)?;
        let tree = checks::tree_hash(Path::new(&wt))?;
        // Whatever waited on approval is replaced by this.
        self.db.execute("UPDATE check_runs SET state = 'stopped', ended = ?1 WHERE task = ?2 AND state = 'approval'", params![now_secs(), id]).map_err(e2s)?;
        self.cancel_summary(id);
        let run = self.new_run(id, &tree, "running", false)?;
        let specs: Vec<(proto::CheckSpec, &str)> = specs.into_iter().map(|s| (s, "manual")).collect();
        self.add_checks(run, id, &specs)?;
        self.start_run(id, run);
        Ok(run)
    }

    /// The task's worktree, if checks can run in it now.
    fn checkable_worktree(&self, id: u64) -> Result<String, String> {
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

    fn new_run(&mut self, task: u64, tree: &str, state: &str, auto: bool) -> Result<u64, String> {
        self.db
            .execute("INSERT INTO check_runs (task, started, tree, state, auto) VALUES (?1, ?2, ?3, ?4, ?5)", params![task, now_secs(), tree, state, auto])
            .map_err(e2s)?;
        self.dirty = true;
        Ok(self.db.last_insert_rowid() as u64)
    }

    fn add_checks(&mut self, run: u64, task: u64, specs: &[(proto::CheckSpec, &str)]) -> Result<(), String> {
        for (pos, (s, origin)) in specs.iter().enumerate() {
            self.db
                .execute(
                    "INSERT INTO checks (run, task, pos, name, command, why, state, origin) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7)",
                    params![run, task, pos, s.name, s.command, s.why, origin],
                )
                .map_err(e2s)?;
        }
        Ok(())
    }

    /// Runs the run's pending checks, from the worktree as it is now.
    fn start_run(&mut self, task: u64, run: u64) {
        let queue = self
            .db
            .prepare("SELECT id FROM checks WHERE run = ?1 AND state = 'pending' ORDER BY pos")
            .and_then(|mut s| s.query_map([run], |r| r.get(0))?.collect())
            .unwrap_or_default();
        // The tree it judges is the tree now, which checks mustn't change.
        if let Ok(tree) = self.checkable_worktree(task).and_then(|wt| checks::tree_hash(Path::new(&wt))) {
            let _ = self.db.execute("UPDATE check_runs SET state = 'running', tree = ?1 WHERE id = ?2", params![tree, run]);
        }
        self.passes.insert(task, Pass { run, queue, current: None });
        self.next_check(task);
    }

    fn check_log(&self, check: u64) -> PathBuf {
        self.data.join(format!("logs/checks/{check}.log"))
    }

    /// Starts the run's next check, or ends the run when none is left.
    fn next_check(&mut self, task: u64) {
        self.dirty = true;
        loop {
            let Some(check) = self.passes.get_mut(&task).and_then(|p| p.queue.pop_front()) else {
                return self.end_run(task);
            };
            match self.spawn_check(task, check) {
                Ok(child) => {
                    let _ = self.db.execute("UPDATE checks SET state = 'running', started = ?1 WHERE id = ?2", params![now_secs(), check]);
                    if let Some(p) = self.passes.get_mut(&task) {
                        p.current = Some(Running { check, child, deadline: Instant::now() + CHECK_TIMEOUT, cut: None });
                    }
                    return;
                }
                Err(e) => {
                    if let Ok(mut log) = security::private_file(&self.check_log(check), true) {
                        let _ = writeln!(log, "\x1b[31mcouldn't start: {e}\x1b[0m");
                    }
                    let _ = self.db.execute("UPDATE checks SET state = 'failed', started = ?1, ended = ?1 WHERE id = ?2", params![now_secs(), check]);
                }
            }
        }
    }

    fn spawn_check(&self, task: u64, check: u64) -> Result<std::process::Child, String> {
        use std::os::unix::process::CommandExt;
        let (command, wt, repo): (String, String, String) = self
            .db
            .query_row("SELECT c.command, t.worktree, t.repo FROM checks c JOIN tasks t ON t.id = c.task WHERE c.id = ?1", [check], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map_err(e2s)?;
        let log = security::private_file(&self.check_log(check), false).map_err(e2s)?;
        log.set_len(0).map_err(e2s)?;
        Command::new("sh")
            .args(["-c", &command])
            .current_dir(&wt)
            .stdin(Stdio::null())
            .stdout(log.try_clone().map_err(e2s)?)
            .stderr(log)
            .env("ARRANGER_TASK", task.to_string())
            .env("ARRANGER_ROOT", &repo)
            .env("ARRANGER_WORKTREE", &wt)
            .env("PORT", task_port(task).to_string())
            // Colour, though it's not a terminal: the console keeps it.
            .env("TERM", "xterm-256color")
            .env("FORCE_COLOR", "1")
            .env("CLICOLOR_FORCE", "1")
            .env("CARGO_TERM_COLOR", "always")
            .process_group(0)
            .spawn()
            .map_err(e2s)
    }

    /// The run is over: failed if a check failed, stopped if one was stopped, else passed.
    fn end_run(&mut self, task: u64) {
        let Some(pass) = self.passes.remove(&task) else { return };
        let count = |state: &str| -> u32 {
            self.db.query_row("SELECT COUNT(*) FROM checks WHERE run = ?1 AND state = ?2", params![pass.run, state], |r| r.get(0)).unwrap_or(0)
        };
        let state = if count("failed") > 0 { "failed" } else if count("stopped") > 0 { "stopped" } else { "passed" };
        let _ = self.db.execute("UPDATE check_runs SET state = ?1, ended = ?2 WHERE id = ?3", params![state, now_secs(), pass.run]);
        self.dirty = true;
        if state == "failed" {
            let _ = self.db.execute("UPDATE summaries SET state = 'stale' WHERE task = ?1 AND tree = (SELECT tree FROM check_runs WHERE id = ?2)", params![task, pass.run]);
        }
        if state == "passed" { self.schedule_summary(task); }
        if let Some(pending) = self.merges.remove(&task) {
            let result = if state == "passed" { self.finish_merge(task, pending.clone()) } else { Err("Checks after merging the base failed; base branch was not updated".into()) };
            if let Err(output) = result { self.send(pending.conn, &proto::reply_frame(&Reply::JobDone { ok: false, output })); }
            return;
        }
        if state == "failed" {
            if let Err(e) = self.auto_fix(task) {
                eprintln!("task {task}: auto-fix: {e}");
            }
        } else if state == "passed" {
            let _ = self.db.execute("UPDATE tasks SET fix_rounds = 0 WHERE id = ?1", [task]);
        }
    }

    fn reap_checks(&mut self) {
        let exited: Vec<(u64, Option<i32>, bool, Option<&'static str>)> = self
            .passes
            .iter_mut()
            .filter_map(|(&task, p)| {
                let r = p.current.as_mut()?;
                match r.child.try_wait() {
                    Ok(Some(status)) => Some((task, status.code(), status.success(), r.cut)),
                    Ok(None) => None,
                    Err(_) => Some((task, None, false, r.cut)),
                }
            })
            .collect();
        for (task, code, ok, cut) in exited {
            let Some(r) = self.passes.get_mut(&task).and_then(|p| p.current.take()) else { continue };
            let run = self.passes[&task].run;
            // Leaving new files behind (a lockfile, a snapshot) is untidy; changing the work is failing.
            let changed = self.guard_tree(task, run, r.check);
            let state = if ok && cut.is_none() && changed.altered.is_empty() { "passed" } else { "failed" };
            let note = match (cut, changed.altered.is_empty(), changed.added.is_empty()) {
                (Some(why), ..) => format!("stopped: {why}"),
                (None, false, _) => format!("changed {} (put back)", changed.altered.join(", ")),
                (None, true, false) => format!("left {} behind (removed)", changed.added.join(", ")),
                _ => String::new(),
            };
            let _ = self.db.execute(
                "UPDATE checks SET state = ?1, exit = ?2, ended = ?3, note = ?4 WHERE id = ?5",
                params![state, if cut.is_some() { None } else { code }, now_secs(), note, r.check],
            );
            self.next_check(task);
        }
    }

    /// Checks report; they mustn't change the work they judge. If this one changed the worktree
    /// (while the agent wasn't working in it), its changes are undone; returns the files.
    fn guard_tree(&mut self, task: u64, run: u64, check: u64) -> checks::Changes {
        if self.tasks.get(&task).is_some_and(|t| t.activity == Activity::Running) {
            return Default::default();
        }
        let Ok(wt) = self.checkable_worktree(task) else { return Default::default() };
        let Ok(tree): Result<String, _> = self.db.query_row("SELECT tree FROM check_runs WHERE id = ?1", [run], |r| r.get(0)) else { return Default::default() };
        if checks::tree_hash(Path::new(&wt)).is_ok_and(|now| now == tree) {
            return Default::default();
        }
        match checks::restore_tree(Path::new(&wt), &tree) {
            Ok(changed) => {
                if let Ok(mut log) = security::private_file(&self.check_log(check), true) {
                    if !changed.altered.is_empty() {
                        let _ = writeln!(log, "\n\x1b[31mthis check changed files, which were put back: {}\x1b[0m", changed.altered.join(", "));
                    }
                    if !changed.added.is_empty() {
                        let _ = writeln!(log, "\n\x1b[33mthis check left new files, which were removed: {}\x1b[0m", changed.added.join(", "));
                    }
                }
                changed
            }
            Err(e) => checks::Changes { added: Vec::new(), altered: vec![format!("the worktree (couldn't put it back: {e})")] },
        }
    }

    /// Stops checks that ran out of time or wrote too much; reaping records them as failed.
    fn time_out_checks(&mut self) {
        let now = Instant::now();
        let mut cuts = Vec::new();
        for p in self.passes.values_mut() {
            let Some(r) = p.current.as_mut().filter(|r| r.cut.is_none()) else { continue };
            let path = self.data.join(format!("logs/checks/{}.log", r.check));
            let too_long = fs::metadata(&path).is_ok_and(|m| m.len() > CHECK_LOG_MAX);
            if r.deadline <= now || too_long {
                let why = if too_long { "its output passed 32 MiB" } else { "it ran for 10 minutes" };
                r.cut = Some(why);
                let _ = rustix::process::kill_process_group(Pid::from_child(&r.child), Signal::KILL);
                cuts.push((path, why));
            }
        }
        for (path, why) in cuts {
            if let Ok(mut log) = security::private_file(&path, true) {
                let _ = writeln!(log, "\n\x1b[31mstopped: {why}\x1b[0m");
            }
        }
        let late: Vec<u64> = self.check_plans.iter().filter(|(_, p)| p.deadline <= now).map(|(&t, _)| t).collect();
        for task in late {
            if let Some(p) = self.check_plans.get_mut(&task) {
                let _ = rustix::process::kill_process_group(Pid::from_child(&p.side.child), Signal::KILL);
                p.deadline = now + Duration::from_secs(3600);
            }
        }
    }

    fn stop_checks(&mut self, task: u64) -> Result<(), String> {
        if self.end_checks(task, "stopped") {
            Ok(())
        } else {
            Err(format!("task {task} has no checks running or waiting"))
        }
    }

    /// Ends whatever the task's checks are doing (planning, waiting on approval, running) as
    /// `state`: "stopped" by you, or "stale" when the agent starts another turn. False if none.
    fn end_checks(&mut self, task: u64, state: &str) -> bool {
        let now = now_secs();
        if let Some(pending) = self.merges.remove(&task) {
            self.send(pending.conn, &proto::reply_frame(&Reply::JobDone { ok: false, output: "Merge checks were interrupted; base branch was not updated".into() }));
        }
        let mut ended = false;
        if let Some(mut plan) = self.check_plans.remove(&task) {
            let _ = rustix::process::kill_process_group(Pid::from_child(&plan.side.child), Signal::KILL);
            let _ = plan.side.child.wait();
            let _ = plan.side.finish(false);
            let _ = self.db.execute("UPDATE check_runs SET state = ?1, ended = ?2 WHERE id = ?3", params![state, now, plan.run]);
            ended = true;
        }
        if let Some(mut pass) = self.passes.remove(&task) {
            if let Some(mut r) = pass.current.take() {
                let _ = rustix::process::kill_process_group(Pid::from_child(&r.child), Signal::KILL);
                let _ = r.child.wait();
                if let Ok(mut log) = security::private_file(&self.check_log(r.check), true) {
                    let _ = writeln!(log, "\n\x1b[33m{}\x1b[0m", if state == "stale" { "stopped: the agent started another turn" } else { "stopped" });
                }
            }
            let _ = self.db.execute("UPDATE checks SET state = 'stopped', ended = ?1 WHERE run = ?2 AND state IN ('pending', 'running')", params![now, pass.run]);
            let _ = self.db.execute("UPDATE check_runs SET state = ?1, ended = ?2 WHERE id = ?3", params![state, now, pass.run]);
            ended = true;
        }
        let waiting = self.db.execute("UPDATE check_runs SET state = ?1, ended = ?2 WHERE task = ?3 AND state = 'approval'", params![state, now, task]).unwrap_or(0);
        if waiting > 0 {
            let _ = self.db.execute("UPDATE checks SET state = 'stopped' WHERE task = ?1 AND state = 'pending'", [task]);
            ended = true;
        }
        self.dirty |= ended;
        ended
    }

    /// After a turn: plans checks for what it changed, unless nothing changed since the last
    /// checks, or nobody can plan them.
    fn plan_checks(&mut self, id: u64, force: bool) -> Result<(), String> {
        if self.passes.contains_key(&id) || self.check_plans.contains_key(&id) || self.merges.contains_key(&id) || self.jobs.values().any(|j| j.task == id) {
            return Err("checks or a commit are already running".into());
        }
        let (agent, repo, model, base_commit): (String, String, String, String) = self
            .db
            .query_row("SELECT agent, repo, model, base_commit FROM tasks WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .map_err(e2s)?;
        let Some(planner) = side::planner(&agent) else { return if force { Err("check planning is disabled or no planner is installed; supply check commands explicitly".into()) } else { Ok(()) } };
        let wt = self.checkable_worktree(id)?;
        let wtp = Path::new(&wt);
        let tree = checks::tree_hash(wtp)?;
        let last: Option<(String, String, bool)> = self
            .db
            .query_row("SELECT tree, state, accepted FROM check_runs WHERE task = ?1 AND state NOT IN ('stale', 'stopped', 'error') ORDER BY id DESC LIMIT 1", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()
            .map_err(e2s)?;
        let base = if base_commit.is_empty() { "HEAD".to_string() } else { base_commit };
        let base_tree = git(git_in(wtp).args(["rev-parse", &format!("{base}^{{tree}}")]))?.trim().to_string();
        if !force && last.as_ref().map_or(base_tree == tree, |(old, state, accepted)| old == &tree && (state != "failed" || *accepted)) {
            return Ok(());
        }
        // What changed since the work last passed (or since its base).
        let passed: Option<String> = self
            .db
            .query_row("SELECT tree FROM check_runs WHERE task = ?1 AND state = 'passed' ORDER BY id DESC LIMIT 1", [id], |r| r.get(0))
            .optional()
            .map_err(e2s)?;
        let from = passed.unwrap_or(base_tree);
        let stat = git(git_in(wtp).args(["diff", "--stat", &from, &tree])).unwrap_or_default();
        let mut diff = git(git_in(wtp).args(["diff", &from, &tree])).unwrap_or_default();
        if diff.len() > 100 << 10 {
            let cut = (0..=100 << 10).rev().find(|&i| diff.is_char_boundary(i)).unwrap_or(0);
            diff.truncate(cut);
            diff.push_str("\n… (the diff goes on; read the files for the rest)");
        }
        let changed = checks::changed_files(wtp, &from, &tree).unwrap_or_default();
        let manifests: Vec<String> = git(git_in(wtp).args(["ls-files"]))
            .unwrap_or_default()
            .lines()
            .filter(|p| p.matches('/').count() <= 2 && setup::MANIFESTS.contains(&p.rsplit('/').next().unwrap_or(p)))
            .take(40)
            .map(str::to_string)
            .collect();
        let prompts = turn_prompts(&self.events(id).unwrap_or_default());
        let previous: Vec<(String, String)> = self
            .db
            .prepare("SELECT command, state, exit FROM checks WHERE run = (SELECT MAX(id) FROM check_runs WHERE task = ?1 AND state NOT IN ('planning', 'approval')) ORDER BY pos")
            .and_then(|mut s| {
                s.query_map([id], |r| {
                    let (state, exit): (String, Option<i32>) = (r.get(1)?, r.get(2)?);
                    Ok((r.get(0)?, exit.map_or(state.clone(), |e| format!("{state} (exit {e})"))))
                })?
                .collect()
            })
            .unwrap_or_default();
        let sticky = self.sticky_checks(id);
        let prompt = checks::plan_prompt(&checks::Trip { prompts: &prompts, stat: &stat, diff: &diff, manifests: &manifests, previous: &previous, sticky: &sticky });
        let same = matches!((&planner, adapter::kind(&agent)), (side::Planner::Claude, "claude") | (side::Planner::Codex, "codex"));
        let name = format!("checks-{}", self.next_side);
        self.next_side += 1;
        let run = self.new_run(id, &tree, "planning", true)?;
        match side::start(planner, wtp, &prompt, &checks::plan_schema(), if same { &model } else { "" }, &self.data.join("side"), &name) {
            Ok(side) => {
                self.check_plans.insert(id, CheckPlan { run, side, deadline: Instant::now() + PLAN_TIMEOUT, repo, changed });
            }
            Err(e) => {
                let _ = self.db.execute("UPDATE check_runs SET state = 'error', note = ?1, ended = ?2 WHERE id = ?3", params![format!("Couldn't start the planner: {e}"), now_secs(), run]);
            }
        }
        Ok(())
    }

    /// Checks that failed in an earlier run and haven't passed since (unless you dropped them):
    /// they run again whatever the planner says.
    fn sticky_checks(&self, id: u64) -> Vec<proto::CheckSpec> {
        let rows: Vec<(String, String, String, String)> = self
            .db
            .prepare("SELECT name, command, why, state FROM checks WHERE task = ?1 AND state IN ('passed', 'failed') ORDER BY run, pos")
            .and_then(|mut s| s.query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect())
            .unwrap_or_default();
        let dropped: Vec<String> = self
            .db
            .prepare("SELECT command FROM dropped_checks WHERE task = ?1")
            .and_then(|mut s| s.query_map([id], |r| r.get(0))?.collect())
            .unwrap_or_default();
        let mut failing: Vec<proto::CheckSpec> = Vec::new();
        for (name, command, why, state) in rows {
            failing.retain(|c| c.command != command);
            if state == "failed" && !dropped.contains(&command) {
                failing.push(proto::CheckSpec { name, command, why });
            }
        }
        failing
    }

    fn reap_check_plans(&mut self) {
        let done: Vec<(u64, bool)> = self
            .check_plans
            .iter_mut()
            .filter_map(|(&task, p)| match p.side.child.try_wait() {
                Ok(Some(status)) => Some((task, status.success())),
                Ok(None) => None,
                Err(_) => Some((task, false)),
            })
            .collect();
        for (task, ok) in done {
            let plan = self.check_plans.remove(&task).unwrap();
            self.check_plan_made(task, plan, ok);
        }
    }

    /// The planner answered: its checks plus the ones still failing, run now or after approval.
    fn check_plan_made(&mut self, task: u64, plan: CheckPlan, ok: bool) {
        let (run, repo, changed) = (plan.run, plan.repo.clone(), plan.changed.clone());
        let answer = plan.side.finish(ok).and_then(|(answer, _cost)| checks::plan_from(answer));
        let sticky = self.sticky_checks(task);
        let (planned, note) = match answer {
            Ok((planned, reason)) => (planned, reason),
            Err(e) if !sticky.is_empty() => (Vec::new(), format!("Couldn't plan checks ({e}); running the ones still failing")),
            Err(e) => {
                let _ = self.db.execute("UPDATE check_runs SET state = 'error', note = ?1, ended = ?2 WHERE id = ?3", params![format!("Couldn't plan checks: {e}"), now_secs(), run]);
                self.dirty = true;
                return;
            }
        };
        let mut specs: Vec<(proto::CheckSpec, &str)> = planned.into_iter().map(|s| (s, "planned")).collect();
        for s in sticky {
            if !specs.iter().any(|(p, _)| p.command == s.command) {
                specs.push((s, "sticky"));
            }
        }
        specs.truncate(checks::MAX_CHECKS);
        if specs.is_empty() {
            let (state, note) = if checks::code_changed(&changed) {
                ("unverified", if note.is_empty() { "Code changed and no check covers it".to_string() } else { note })
            } else {
                ("skipped", if note.is_empty() { "Nothing here needs checking".to_string() } else { note })
            };
            let _ = self.db.execute("UPDATE check_runs SET state = ?1, note = ?2, ended = ?3 WHERE id = ?4", params![state, note, now_secs(), run]);
            self.dirty = true;
            if state == "skipped" { self.schedule_summary(task); }
            return;
        }
        let _ = self.db.execute("UPDATE check_runs SET note = ?1 WHERE id = ?2", params![note, run]);
        if let Err(e) = self.add_checks(run, task, &specs) {
            let _ = self.db.execute("UPDATE check_runs SET state = 'error', note = ?1 WHERE id = ?2", params![e, run]);
            return;
        }
        let wt = self.checkable_worktree(task).unwrap_or_default();
        let asking = specs.iter().filter(|(s, _)| !self.check_allowed(&repo, Path::new(&wt), &s.command)).count();
        if asking == 0 {
            self.start_run(task, run);
        } else {
            let _ = self.db.execute("UPDATE check_runs SET state = 'approval' WHERE id = ?1", [run]);
            self.dirty = true;
        }
    }

    fn check_allowed(&self, repo: &str, wt: &Path, command: &str) -> bool {
        checks::allowed(command, wt)
            || self.db.query_row("SELECT 1 FROM approved_checks WHERE repo = ?1 AND command = ?2", params![repo, command], |_| Ok(())).is_ok()
    }

    fn approve_checks(&mut self, id: u64, always: bool) -> Result<(), String> {
        let run: u64 = self
            .db
            .query_row("SELECT id FROM check_runs WHERE task = ?1 AND state = 'approval' ORDER BY id DESC LIMIT 1", [id], |r| r.get(0))
            .optional()
            .map_err(e2s)?
            .ok_or(format!("task {id} has no checks waiting for approval"))?;
        if self.passes.contains_key(&id) || self.check_plans.contains_key(&id) {
            return Err("checks are already running; stop them first".into());
        }
        if always {
            let repo: String = self.db.query_row("SELECT repo FROM tasks WHERE id = ?1", [id], |r| r.get(0)).map_err(e2s)?;
            let commands: Vec<String> = self.db.prepare("SELECT command FROM checks WHERE run = ?1").and_then(|mut s| s.query_map([run], |r| r.get(0))?.collect()).map_err(e2s)?;
            for c in commands {
                self.db.execute("INSERT OR IGNORE INTO approved_checks (repo, command) VALUES (?1, ?2)", params![repo, c]).map_err(e2s)?;
            }
        }
        self.start_run(id, run);
        Ok(())
    }

    /// After the agent's turn: once it's idle a moment later, checks for what it changed.
    fn auto_checks(&mut self) {
        let now = Instant::now();
        let due: Vec<u64> = self.auto_due.iter().filter(|(_, at)| **at <= now).map(|(&id, _)| id).collect();
        for id in due {
            self.auto_due.remove(&id);
            let idle = self.tasks.get(&id).is_some_and(|t| t.activity == Activity::Done && t.need.is_none() && t.queued == 0 && !t.headless.as_ref().is_some_and(Agent::asking))
                && !self.asks.iter().any(|a| a.0 == id);
            if !idle || self.passes.contains_key(&id) || self.check_plans.contains_key(&id) {
                continue;
            }
            if let Err(e) = self.plan_checks(id, false) {
                eprintln!("task {id}: checks: {e}");
            }
        }
    }

    fn verification(&self, id: u64, tree: &str) -> Result<String, String> {
        let Some(run) = self.check_runs(id)?.into_iter().next() else { return Ok("unverified".into()) };
        if run.tree != tree { return Ok("stale".into()); }
        Ok(if run.accepted && run.state == "failed" { "accepted".into() } else { run.state })
    }

    fn schedule_summary(&mut self, id: u64) {
        if self.summaries.contains_key(&id) { return; }
        if let Err(error) = self.start_summary(id)
            && let Ok(tree) = self.checkable_worktree(id).and_then(|wt| checks::tree_hash(Path::new(&wt)))
        {
            let _ = self.db.execute("INSERT INTO summaries (task, tree, state, error, updated) VALUES (?1, ?2, 'error', ?3, ?4) ON CONFLICT(task, tree) DO UPDATE SET state = 'error', error = excluded.error, updated = excluded.updated", params![id, tree, error, now_secs()]);
            self.dirty = true;
        }
    }

    fn start_summary(&mut self, id: u64) -> Result<(), String> {
        if self.summaries.contains_key(&id) { return Err("summary is already running".into()); }
        let wt = self.checkable_worktree(id)?;
        let tree = checks::tree_hash(Path::new(&wt))?;
        if !matches!(self.verification(id, &tree)?.as_str(), "passed" | "accepted" | "skipped") {
            return Err("run checks or accept their failures before generating a summary".into());
        }
        let (agent, model, base, headless): (String, String, String, bool) = self.db.query_row(
            "SELECT agent, model, base_commit, headless FROM tasks WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).map_err(e2s)?;
        let planner = side::planner(&agent).ok_or("summary generation is disabled or no agent is installed")?;
        let own = matches!((&planner, adapter::kind(&agent)), (side::Planner::Claude, "claude") | (side::Planner::Codex, "codex"));
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
        let run = self.check_runs(id)?.into_iter().next().ok_or("no check run")?;
        let verified_by = run.checks.iter().filter(|c| c.state == "passed").map(|c| c.command.clone()).collect::<Vec<_>>();
        let context = review::chat_summary(&self.events(id)?);
        let prompt = format!("Summarize the completed work for human review and a commit message. Return JSON matching the schema. Read only; do not edit files, run checks, or send messages. Describe this exact tree {tree}, not intended future work. Keep the overview short: one concise paragraph describing only what the changes do to the existing codebase. Do not include headings, lists, per-file breakdowns, risks, or verification details in the overview. Include a short title and return empty arrays for files and risks. Do not claim verification beyond these recorded results: {}. Check state: {}.\nConversation context:\n{context}\nDiff:\n{diff}", verified_by.join(", "), run.state);
        let name = format!("summary-{}", self.next_side);
        self.next_side += 1;
        let side = side::start_summary(planner, Path::new(&wt), &prompt, if own { &model } else { "" }, &self.data.join("side"), &name, &agent, if own { session.trim() } else { "" }, &self.exe).map_err(e2s)?;
        self.db.execute("INSERT INTO summaries (task, tree, state, updated) VALUES (?1, ?2, 'running', ?3) ON CONFLICT(task, tree) DO UPDATE SET state = 'running', error = '', updated = excluded.updated", params![id, tree, now_secs()]).map_err(e2s)?;
        self.summaries.insert(id, SummaryRun { tree, verified_by, side, deadline: Instant::now() + PLAN_TIMEOUT });
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
            let result = run.side.finish(ok).and_then(|(v, _)| crate::summary::parse(v, tree.clone(), run.verified_by));
            let current = self.checkable_worktree(id).and_then(|wt| checks::tree_hash(Path::new(&wt)));
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

    fn setting(&self, key: &str, value: Option<&str>) -> Result<String, String> {
        if key != "auto_fix" {
            return Err(format!("unknown setting: {key}"));
        }
        if let Some(value) = value {
            let rounds: u32 = value.parse().map_err(|_| "auto_fix must be an integer from 0 to 10")?;
            if rounds > 10 { return Err("auto_fix must be an integer from 0 to 10".into()); }
            self.db.execute("INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value", params![key, rounds.to_string()]).map_err(e2s)?;
        }
        Ok(self.db.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0)).optional().map_err(e2s)?.unwrap_or_else(|| "0".into()))
    }

    fn failed_run(&self, id: u64) -> Result<proto::CheckRun, String> {
        let run = self.check_runs(id)?.into_iter().next().ok_or("no check run")?;
        if run.state != "failed" { return Err("the latest checks have not failed".into()); }
        Ok(run)
    }

    fn accept_checks(&mut self, id: u64) -> Result<(), String> {
        let run = self.failed_run(id)?;
        if self.tasks.get(&id).is_some_and(|t| t.activity != Activity::Done || t.queued > 0) {
            return Err("wait for the agent to finish before accepting checks".into());
        }
        let wt = self.checkable_worktree(id)?;
        if checks::tree_hash(Path::new(&wt))? != run.tree {
            return Err("the worktree changed; run checks again before accepting".into());
        }
        self.db.execute("UPDATE check_runs SET accepted = 1 WHERE id = ?1", [run.id]).map_err(e2s)?;
        self.auto_due.remove(&id);
        self.dirty = true;
        self.schedule_summary(id);
        Ok(())
    }

    fn check_report(&self, id: u64) -> Result<String, String> {
        let run = self.failed_run(id)?;
        let mut failures = Vec::new();
        for check in run.checks.into_iter().filter(|c| c.state == "failed") {
            let mut console = Console::default();
            let bytes = match read_tail(&self.check_log(check.id), 32 << 20) {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == io::ErrorKind::NotFound => b"Check output is unavailable.\n".to_vec(),
                Err(e) => return Err(e2s(e)),
            };
            console.feed(&bytes);
            let lines = console.lines(0, console.len()).into_iter().map(|line| {
                (line.spans.into_iter().map(|span| span.0).collect(), line.error)
            }).collect();
            failures.push(checks::Failure { name: check.name, command: check.command, exit: check.exit, note: check.note, lines });
        }
        Ok(checks::failure_report(&failures))
    }

    fn auto_fix(&mut self, id: u64) -> Result<(), String> {
        let limit: u32 = self.setting("auto_fix", None)?.parse().map_err(e2s)?;
        let used: u32 = self.db.query_row("SELECT fix_rounds FROM tasks WHERE id = ?1", [id], |r| r.get(0)).map_err(e2s)?;
        if used >= limit { return Ok(()); }
        let Some(task) = self.tasks.get(&id) else { return Ok(()) };
        if task.activity != Activity::Done || task.need.is_some() || task.queued > 0 || task.interrupted || task.headless.as_ref().is_some_and(Agent::asking) {
            return Ok(());
        }
        let run = self.failed_run(id)?;
        let wt = self.checkable_worktree(id)?;
        if run.accepted || checks::tree_hash(Path::new(&wt))? != run.tree { return Ok(()); }
        let text = self.check_report(id)?;
        self.db.execute("UPDATE tasks SET fix_rounds = fix_rounds + 1 WHERE id = ?1", [id]).map_err(e2s)?;
        let task = self.tasks.get_mut(&id).unwrap();
        if let Some(agent) = task.headless.as_mut() {
            let events = agent.prompt(text);
            self.agent_events(id, events);
        } else {
            let mut bytes = security::paste_text(&text, task.screen.mode().contains(TermMode::BRACKETED_PASTE)).into_bytes();
            bytes.push(b'\r');
            write_pty(&task.pty.master, &bytes);
            task.activity = Activity::Running;
            task.action = "fixing failed checks".into();
            task.since = now();
            self.dirty = true;
        }
        Ok(())
    }

    /// Task `id`'s check runs, newest first.
    fn check_runs(&self, id: u64) -> Result<Vec<proto::CheckRun>, String> {
        let mut stmt = self.db.prepare(CHECK_RUNS_QUERY).map_err(e2s)?;
        let mut runs: Vec<proto::CheckRun> = stmt
            .query_map([id], |r| {
                Ok(proto::CheckRun { id: r.get(0)?, number: r.get(8)?, started: r.get(1)?, ended: r.get(2)?, tree: r.get(3)?, state: r.get(4)?, auto: r.get(5)?, note: r.get(6)?, accepted: r.get(7)?, checks: Vec::new() })
            })
            .and_then(Iterator::collect)
            .map_err(e2s)?;
        let mut stmt = self.db.prepare("SELECT id, name, command, why, state, exit, started, ended, origin, note FROM checks WHERE run = ?1 ORDER BY pos").map_err(e2s)?;
        for run in &mut runs {
            run.checks = stmt
                .query_map([run.id], |r| {
                    Ok(proto::CheckInfo {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        command: r.get(2)?,
                        why: r.get(3)?,
                        state: r.get(4)?,
                        exit: r.get(5)?,
                        started: r.get(6)?,
                        ended: r.get(7)?,
                        origin: r.get(8)?,
                        note: r.get(9)?,
                    })
                })
                .and_then(Iterator::collect)
                .map_err(e2s)?;
        }
        Ok(runs)
    }

    /// Lines of a check's output: what its log has gained since last asked is parsed on.
    fn console(&mut self, check: u64, start: usize, limit: usize) -> Result<Reply, String> {
        use std::io::Seek;
        let state: String = self
            .db
            .query_row("SELECT state FROM checks WHERE id = ?1", [check], |r| r.get(0))
            .optional()
            .map_err(e2s)?
            .ok_or(format!("no check {check}"))?;
        let path = self.check_log(check);
        let entry = self.consoles.entry(check).or_default();
        let len = match fs::File::open(&path) {
            Ok(mut f) => {
                let len = f.metadata().map_err(e2s)?.len();
                if len > entry.1 {
                    f.seek(io::SeekFrom::Start(entry.1)).map_err(e2s)?;
                    let mut buf = Vec::new();
                    (&mut f).take(8 << 20).read_to_end(&mut buf).map_err(e2s)?;
                    entry.0.feed(&buf);
                    entry.1 += buf.len() as u64;
                }
                len
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => 0,
            Err(e) => return Err(e2s(e)),
        };
        let done = !matches!(state.as_str(), "pending" | "running") && entry.1 >= len;
        let reply = Reply::Console { check, start, total: entry.0.len(), done, lines: entry.0.lines(start, limit.min(5000)) };
        // Keep the running ones and this one; the rest parse again if asked for.
        if self.consoles.len() > 16 {
            let running: Vec<u64> = self.passes.values().filter_map(|p| p.current.as_ref().map(|r| r.check)).collect();
            self.consoles.retain(|c, _| *c == check || running.contains(c));
        }
        Ok(reply)
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
        let _ = self.stop_checks(id);
        let checks: Vec<u64> = self.db.prepare("SELECT id FROM checks WHERE task = ?1").and_then(|mut s| s.query_map([id], |r| r.get(0))?.collect()).unwrap_or_default();
        for c in checks {
            let _ = fs::remove_file(self.check_log(c));
            self.consoles.remove(&c);
        }
        let _ = self.db.execute("DELETE FROM checks WHERE task = ?1", [id]);
        let _ = self.db.execute("DELETE FROM check_runs WHERE task = ?1", [id]);
        let _ = self.db.execute("DELETE FROM summaries WHERE task = ?1", [id]);
        let _ = self.db.execute("DELETE FROM dropped_checks WHERE task = ?1", [id]);
        self.auto_due.remove(&id);
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
        // Each task's latest check run.
        let mut stmt = self
            .db
            .prepare(
                "SELECT r.task, r.state, COALESCE(SUM(c.state = 'passed'), 0), COALESCE(SUM(c.state = 'failed'), 0), COUNT(c.id), r.id, r.started, r.accepted
                 FROM check_runs r LEFT JOIN checks c ON c.run = r.id
                 WHERE r.id IN (SELECT MAX(id) FROM check_runs GROUP BY task) GROUP BY r.id",
            )
            .map_err(e2s)?;
        let summaries: HashMap<u64, proto::ChecksSummary> = stmt
            .query_map([], |r| Ok((r.get(0)?, proto::ChecksSummary { state: r.get(1)?, passed: r.get(2)?, failed: r.get(3)?, total: r.get(4)?, run: r.get(5)?, started: r.get(6)?, accepted: r.get(7)? })))
            .and_then(Iterator::collect)
            .map_err(e2s)?;
        for info in &mut tasks {
            let state: Option<String> = self.db.query_row("SELECT CASE WHEN s.tree = r.tree THEN s.state ELSE 'stale' END FROM summaries s LEFT JOIN check_runs r ON r.id = (SELECT MAX(id) FROM check_runs WHERE task = ?1) WHERE s.task = ?1 ORDER BY (s.tree = r.tree) DESC, s.updated DESC LIMIT 1", [info.id], |r| r.get(0)).optional().map_err(e2s)?;
            info.summary_state = if self.tasks.get(&info.id).is_some_and(|t| t.activity == Activity::Running) && state.is_some() { "stale".into() } else { state.unwrap_or_default() };
            if let Some(s) = summaries.get(&info.id) {
                info.checks = s.clone();
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
        for info in &mut tasks {
            // Planned checks waiting on you put an idle agent in needs-you.
            if info.checks.state == "approval" && info.state == "done" {
                info.state = "needs-you".into();
                info.need = Some(Event::Attention { text: "Approve its checks".into() });
                info.action = "checks wait for approval".into();
            } else if info.checks.state == "failed" && !info.checks.accepted && info.state == "done" {
                info.state = "needs-you".into();
                info.action = format!("Checks failed ({}/{})", info.checks.failed, info.checks.total);
                info.need = Some(Event::Attention { text: info.action.clone() });
            } else if matches!(info.checks.state.as_str(), "planning" | "running") && info.state == "done" {
                info.action = if info.checks.state == "planning" { "planning checks" } else { "running checks" }.into();
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
        let tree = checks::tree_hash(Path::new(&wt))?;
        let verification = self.verification(id, &tree)?;
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
            checks: self.check_runs(id)?.first().filter(|r| r.tree == tree).map(|r| r.checks.iter().map(|c| (c.command.clone(), match c.state.as_str() { "passed" => Some(true), "failed" | "stopped" => Some(false), _ => None })).collect()).unwrap_or_default(),
            files: review::diff(Path::new(&wt), &base_commit, None)?,
        })
    }

    fn commit(&mut self, conn: Token, id: u64, message: &str, ship: Ship, force: bool) -> Result<(), String> {
        if self.jobs.values().any(|j| j.task == id) || self.merges.contains_key(&id) {
            return Err("a commit or merge is already running".into());
        }
        if ship == Ship::Merge && (self.tasks.get(&id).is_some_and(|t| t.activity != Activity::Done || t.queued > 0)
            || self.passes.contains_key(&id) || self.check_plans.contains_key(&id)) {
            return Err("wait for the agent and checks to finish before merging".into());
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
            if !force && !matches!(data.verification.as_str(), "passed" | "accepted" | "skipped") {
                let _ = self.plan_checks(id, true);
                return Err("this tree has not passed checks; review/run checks and retry, or use --force to merge anyway".into());
            }
            let checks = self.check_runs(id)?.first().map(|r| r.checks.iter().map(|c| proto::CheckSpec { name: c.name.clone(), command: c.command.clone(), why: c.why.clone() }).collect()).unwrap_or_default();
            self.cancel_summary(id);
            let pending = MergePending { conn, tree: data.tree.clone(), message: message.clone(), base: base.clone(), force, checks };
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
        let wt = self.checkable_worktree(id)?;
        let tree = checks::tree_hash(Path::new(&wt))?;
        if tree == pending.tree || pending.force {
            pending.tree = tree;
            return self.finish_merge(id, pending);
        }
        // The base changed the tree: rerun exactly the checks already approved, with no planner.
        if pending.checks.is_empty() && !checks::code_changed(&checks::changed_files(Path::new(&wt), &pending.tree, &tree)?) {
            let run = self.new_run(id, &tree, "skipped", false)?;
            self.db.execute("UPDATE check_runs SET ended = ?1, note = 'Base changed only non-code files; no checks needed' WHERE id = ?2", params![now_secs(), run]).map_err(e2s)?;
            pending.tree = tree;
            self.schedule_summary(id);
            return self.finish_merge(id, pending);
        }
        if pending.checks.is_empty() {
            return Err("merging the base changed the tree and there are no previous checks; run checks before retrying".into());
        }
        let run = self.new_run(id, &tree, "running", false)?;
        let specs = pending.checks.iter().cloned().map(|c| (c, "merge")).collect::<Vec<_>>();
        self.add_checks(run, id, &specs)?;
        pending.tree = tree;
        self.merges.insert(id, pending);
        self.start_run(id, run);
        Ok(())
    }

    fn finish_merge(&mut self, id: u64, pending: MergePending) -> Result<(), String> {
        let wt = self.checkable_worktree(id)?;
        if checks::tree_hash(Path::new(&wt))? != pending.tree { return Err("worktree changed before merge; run checks again".into()); }
        if !pending.force && !matches!(self.verification(id, &pending.tree)?.as_str(), "passed" | "accepted" | "skipped") {
            return Err("the merged tree has not passed checks".into());
        }
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

    /// Agents get SIGHUP when their PTY masters close as the process exits; headless ones, which
    /// have no terminal, get it from us.
    fn shutdown(&mut self, sock: &Path) {
        for id in self.summaries.keys().copied().collect::<Vec<_>>() { self.cancel_summary(id); }
        let checking: Vec<u64> = self.passes.keys().chain(self.check_plans.keys()).copied().collect();
        for id in checking { self.end_checks(id, "stopped"); }
        for job in self.jobs.values_mut() {
            let _ = rustix::process::kill_process_group(Pid::from_child(&job.child), Signal::KILL);
            let _ = job.child.wait();
        }
        for (&id, task) in &mut self.tasks {
            if task.headless.is_some() {
                let _ = rustix::process::kill_process_group(Pid::from_child(&task.pty.child), Signal::HUP);
            }
            flush_log(&log_path(&self.data, id, task.headless.is_some()), &mut task.log, &mut self.zstd);
        }
        let _ = fs::remove_file(sock);
        let _ = fs::remove_file(self.data.join("daemon.pid"));
    }
}

/// What the agent was asked in its latest turn: the prompts after the turn before it ended.
fn turn_prompts(events: &[(u64, Event)]) -> Vec<String> {
    let mut out = Vec::new();
    for (_, e) in events.iter().rev() {
        match e {
            // The turn that just ended.
            Event::TurnDone if out.is_empty() => {}
            Event::TurnDone => break,
            Event::Prompt { text } => out.push(text.clone()),
            _ => {}
        }
    }
    out.reverse();
    out
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
    fn check_run_numbers_start_fresh_for_each_task() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE check_runs (
            id INTEGER PRIMARY KEY, task INTEGER, started INTEGER DEFAULT 0,
            ended INTEGER DEFAULT 0, tree TEXT DEFAULT '', state TEXT DEFAULT 'passed',
            auto INTEGER DEFAULT 0, note TEXT DEFAULT '', accepted INTEGER DEFAULT 0)").unwrap();
        for task in [1, 2, 1, 3, 2] {
            db.execute("INSERT INTO check_runs (task) VALUES (?1)", [task]).unwrap();
        }
        let numbers = |task| {
            db.prepare(CHECK_RUNS_QUERY).unwrap()
                .query_map([task], |r| Ok((r.get::<_, u64>(0)?, r.get::<_, u64>(8)?)))
                .unwrap().collect::<Result<Vec<_>, _>>().unwrap()
        };
        assert_eq!(numbers(1), [(3, 2), (1, 1)]);
        assert_eq!(numbers(2), [(5, 2), (2, 1)]);
        assert_eq!(numbers(3), [(4, 1)]);
        assert!(numbers(4).is_empty());
        for _ in 0..31 {
            db.execute("INSERT INTO check_runs (task) VALUES (1)", []).unwrap();
        }
        let runs = numbers(1);
        assert_eq!(runs.len(), 30);
        assert_eq!(runs.first().unwrap().1, 33);
        assert_eq!(runs.last().unwrap().1, 4);
    }

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
    fn a_turns_prompts_are_the_ones_since_the_turn_before() {
        let p = |t: &str| (0, Event::Prompt { text: t.into() });
        let events = vec![p("first"), (0, Event::TurnDone), p("second"), (0, Event::ToolEnd { ok: true }), p("and also"), (0, Event::TurnDone)];
        assert_eq!(turn_prompts(&events), ["second", "and also"]);
        assert_eq!(turn_prompts(&events[..2]), ["first"]);
        assert!(turn_prompts(&[]).is_empty());
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

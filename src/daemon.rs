//! `arranger serve`: one thread, one mio loop owning every PTY, client and task.

use crate::proto::{self, Reply, Request, TaskInfo};
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
}

struct Conn {
    stream: UnixStream,
    inbuf: Vec<u8>,
    outbuf: Vec<u8>,
    attached: Option<u64>,
    writable: bool,
}

struct Daemon {
    poll: Poll,
    listener: UnixListener,
    conns: HashMap<Token, Conn>,
    tasks: HashMap<u64, Task>,
    db: Connection,
    zstd: zstd::bulk::Compressor<'static>,
    data: PathBuf,
    sigchld: StdUnixStream,
    next_conn: usize,
    max_live: usize,
}

pub fn run() -> io::Result<()> {
    let data = proto::data_dir();
    fs::create_dir_all(data.join("logs"))?;
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
         UPDATE tasks SET state = 'dead' WHERE state = 'running';
         DELETE FROM tasks WHERE state = 'new';",
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
        db,
        zstd: zstd::bulk::Compressor::new(3)?,
        data,
        sigchld,
        next_conn: 3,
        max_live: std::env::var("ARRANGER_MAX_AGENTS").ok().and_then(|v| v.parse().ok()).unwrap_or(20),
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

fn e2s(e: impl std::fmt::Display) -> String {
    e.to_string()
}

impl Daemon {
    fn accept(&mut self) {
        while let Ok((mut stream, _)) = self.listener.accept() {
            let t = Token(self.next_conn);
            self.next_conn += 1;
            if self.poll.registry().register(&mut stream, t, Interest::READABLE).is_ok() {
                let conn = Conn { stream, inbuf: Vec::new(), outbuf: Vec::new(), attached: None, writable: false };
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
            Request::List => match self.list() {
                Ok(tasks) => Reply::Tasks(tasks),
                Err(e) => Reply::Error(e),
            },
            Request::New { agent, repo, base } => match self.create(&agent, &repo, base.as_deref()) {
                Ok(id) => Reply::Created { id, state: self.state(id) },
                Err(e) => Reply::Error(e),
            },
            Request::Kill { id } => match self.kill(id) {
                Ok(()) => Reply::Ok,
                Err(e) => Reply::Error(e),
            },
            Request::Attach { id, cols, rows } => {
                self.detach(t);
                match self.tasks.get_mut(&id) {
                    Some(task) => {
                        task.viewers.push(t);
                        self.resize(id, cols, rows);
                        if let Some(c) = self.conns.get_mut(&t) {
                            c.attached = Some(id);
                        }
                        Reply::Output(self.tasks[&id].screen.snapshot())
                    }
                    None => Reply::Error(format!("task {id} is not running")),
                }
            }
            Request::Input(bytes) => {
                if let Some(task) = attached.and_then(|id| self.tasks.get(&id)) {
                    write_pty(&task.pty.master, &bytes);
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
        let replies = task.screen.feed(bytes);
        if task.viewers.is_empty() {
            // Attached viewers' own terminals answer queries; answer only when nobody is.
            write_pty(&task.pty.master, &replies);
            return;
        }
        let frame = proto::frame(&Reply::Output(bytes.to_vec()));
        for v in task.viewers.clone() {
            self.send(v, &frame);
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

    fn create(&mut self, agent: &str, dir: &Path, base: Option<&str>) -> Result<u64, String> {
        let top = PathBuf::from(git(git_in(dir).args(["rev-parse", "--show-toplevel"]))?);
        let repo_name = top.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        self.db
            .execute(
                "INSERT INTO tasks (agent, repo, branch, worktree, state) VALUES (?1, ?2, '', '', 'new')",
                params![agent, top.to_string_lossy()],
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
        let (agent, repo, wt): (String, String, String) = self
            .db
            .query_row("SELECT agent, repo, worktree FROM tasks WHERE id = ?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .map_err(e2s)?;
        let mut cmd = if Path::new(&wt).join(".arranger/setup").exists() {
            let mut sh = Command::new("sh");
            sh.args(["-c", "./.arranger/setup && exec \"$0\"", &agent]);
            sh
        } else {
            Command::new(&agent)
        };
        cmd.current_dir(&wt)
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
        let task = Task { pty, screen: Screen::new(cols, rows), log: Vec::new(), viewers: Vec::new() };
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
        self.start_queued();
        Ok(())
    }

    fn list(&self) -> Result<Vec<TaskInfo>, String> {
        let mut stmt =
            self.db.prepare("SELECT id, agent, branch, worktree, state FROM tasks ORDER BY id").map_err(e2s)?;
        let rows = stmt.query_map([], |r| {
            Ok(TaskInfo { id: r.get(0)?, agent: r.get(1)?, branch: r.get(2)?, worktree: r.get(3)?, state: r.get(4)? })
        });
        rows.and_then(Iterator::collect).map_err(e2s)
    }

    fn state(&self, id: u64) -> String {
        self.db.query_row("SELECT state FROM tasks WHERE id = ?1", [id], |r| r.get(0)).unwrap_or_default()
    }

    fn set_state(&self, id: u64, state: &str) {
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

mod daemon;
mod proto;
mod pty;
mod screen;

use proto::{Reply, Request};
use rustix::termios::{self, OptionalActions};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{env, thread};

const USAGE: &str = "usage: arranger [ls | new <agent> [--branch <base>] | attach <id> | kill <id> | ping | serve]";
/// Ctrl-\, raw and in kitty keyboard encoding.
const DETACH: &[&[u8]] = &[b"\x1c", b"\x1b[92;5u"];

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args[..] {
        ["serve"] => daemon::run(),
        [] | ["ls"] => ls(),
        ["ping"] => request(&Request::Ping).map(|_| println!("pong")),
        ["new", agent] => new(agent, None),
        ["new", agent, "--branch", base] => new(agent, Some(base)),
        ["attach", id] => parse_id(id).and_then(attach),
        ["kill", id] => parse_id(id).and_then(|id| request(&Request::Kill { id }).map(drop)),
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("arranger: {e}");
        std::process::exit(1);
    }
}

fn parse_id(s: &str) -> io::Result<u64> {
    s.parse().map_err(|_| io::Error::other(format!("bad task id: {s}")))
}

fn unexpected(reply: Reply) -> io::Error {
    io::Error::other(format!("unexpected reply: {reply:?}"))
}

/// Connects to the daemon, starting it detached if nothing is listening.
fn connect() -> io::Result<UnixStream> {
    let sock = proto::socket_path();
    if let Ok(s) = UnixStream::connect(&sock) {
        return Ok(s);
    }
    let data = proto::data_dir();
    fs::create_dir_all(&data)?;
    let log = File::options().create(true).append(true).open(data.join("daemon.log"))?;
    let mut cmd = Command::new(env::current_exe()?);
    cmd.arg("serve").stdin(Stdio::null()).stdout(Stdio::null()).stderr(log);
    unsafe {
        cmd.pre_exec(|| Ok(rustix::process::setsid().map(drop)?));
    }
    cmd.spawn()?;
    for _ in 0..100 {
        thread::sleep(Duration::from_millis(20));
        if let Ok(s) = UnixStream::connect(&sock) {
            return Ok(s);
        }
    }
    Err(io::Error::other(format!("daemon did not start, see {}", data.join("daemon.log").display())))
}

fn request(req: &Request) -> io::Result<Reply> {
    let mut s = connect()?;
    s.write_all(&proto::frame(req))?;
    match proto::read_msg(&mut s)? {
        Reply::Error(e) => Err(io::Error::other(e)),
        reply => Ok(reply),
    }
}

fn ls() -> io::Result<()> {
    let reply = request(&Request::List)?;
    let Reply::Tasks(tasks) = reply else { return Err(unexpected(reply)) };
    println!("{:<4} {:<8} {:<10} {:<14} WORKTREE", "ID", "STATE", "AGENT", "BRANCH");
    for t in tasks {
        println!("{:<4} {:<8} {:<10} {:<14} {}", t.id, t.state, t.agent, t.branch, t.worktree);
    }
    Ok(())
}

fn new(agent: &str, base: Option<&str>) -> io::Result<()> {
    let req = Request::New { agent: agent.into(), repo: env::current_dir()?, base: base.map(Into::into) };
    match request(&req)? {
        Reply::Created { id, state } => Ok(println!("{id} {state}")),
        reply => Err(unexpected(reply)),
    }
}

fn tty_size(tty: &File) -> io::Result<(u16, u16)> {
    let ws = termios::tcgetwinsize(tty)?;
    Ok((ws.ws_col, ws.ws_row))
}

fn attach(id: u64) -> io::Result<()> {
    let tty = File::options().read(true).write(true).open("/dev/tty")?;
    let (cols, rows) = tty_size(&tty)?;
    let mut sock = connect()?;
    sock.write_all(&proto::frame(&Request::Attach { id, cols, rows }))?;
    let snapshot = match proto::read_msg(&mut sock)? {
        Reply::Output(bytes) => bytes,
        Reply::Error(e) => return Err(io::Error::other(e)),
        reply => return Err(unexpected(reply)),
    };

    let saved = termios::tcgetattr(&tty)?;
    let mut raw = saved.clone();
    raw.make_raw();
    termios::tcsetattr(&tty, OptionalActions::Now, &raw)?;
    let result = stream(&tty, &mut sock, &snapshot);
    termios::tcsetattr(&tty, OptionalActions::Now, &saved)?;
    // Undo modes the agent may have turned on in this terminal.
    let mut out = io::stdout();
    out.write_all(b"\x1b[0m\x1b[?1049l\x1b[?25h\x1b[?1l\x1b>\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1006l\x1b[?2004l\x1b[<99u\r\n")?;
    match result? {
        true => println!("[exited]"),
        false => println!("[detached from {id}]"),
    }
    Ok(())
}

fn send(writer: &Mutex<UnixStream>, req: &Request) -> io::Result<()> {
    writer.lock().unwrap().write_all(&proto::frame(req))
}

/// Pumps tty <-> daemon until detach (false) or the task exits (true).
/// Blocking threads, not poll(): macOS poll() doesn't support tty devices.
fn stream(tty: &File, sock: &mut UnixStream, snapshot: &[u8]) -> io::Result<bool> {
    let writer = Arc::new(Mutex::new(sock.try_clone()?));
    let detached = Arc::new(AtomicBool::new(false));

    let (mut keys, w, done, sock2) = (tty.try_clone()?, writer.clone(), detached.clone(), sock.try_clone()?);
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            let n = keys.read(&mut buf).unwrap_or(0);
            let input = &buf[..n];
            let cut = DETACH.iter().filter_map(|k| input.windows(k.len()).position(|w| w == *k)).min();
            let send_now = &input[..cut.unwrap_or(n)];
            if !send_now.is_empty() && send(&w, &Request::Input(send_now.to_vec())).is_err() {
                return;
            }
            if cut.is_some() || n == 0 {
                done.store(true, Ordering::SeqCst);
                let _ = sock2.shutdown(Shutdown::Both);
                return;
            }
        }
    });

    let (winch, winch_w) = UnixStream::pair()?;
    signal_hook::low_level::pipe::register(signal_hook::consts::SIGWINCH, winch_w)?;
    let size_tty = tty.try_clone()?;
    thread::spawn(move || {
        let mut buf = [0u8; 64];
        while matches!((&winch).read(&mut buf), Ok(n) if n > 0) {
            let Ok((cols, rows)) = tty_size(&size_tty) else { return };
            if send(&writer, &Request::Resize { cols, rows }).is_err() {
                return;
            }
        }
    });

    let mut out = io::stdout().lock();
    out.write_all(snapshot)?;
    out.flush()?;
    let mut inbuf = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = match sock.read(&mut buf) {
            Ok(n) if n > 0 => n,
            _ if detached.load(Ordering::SeqCst) => return Ok(false),
            Ok(_) => return Err(io::Error::other("daemon went away")),
            Err(e) => return Err(e),
        };
        inbuf.extend_from_slice(&buf[..n]);
        while let Some(msg) = proto::unframe(&mut inbuf) {
            match msg? {
                Reply::Output(bytes) => out.write_all(&bytes)?,
                Reply::Exited => return Ok(true),
                reply => return Err(unexpected(reply)),
            }
        }
        out.flush()?;
    }
}

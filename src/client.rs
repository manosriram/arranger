//! Talking to the daemon from any client: CLI, GUI or hook.

use crate::proto::{self, Reply, Request};
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;
use std::{env, thread};

/// Connects to the local daemon (starting it if needed) and checks it speaks this build's protocol.
pub fn connect() -> io::Result<UnixStream> {
    connect_at(None)
}

/// Connects to the daemon at `sock` (a remote machine's forwarded socket), or the local one when None.
pub fn connect_at(sock: Option<&Path>) -> io::Result<UnixStream> {
    let mut s = match sock {
        Some(path) => UnixStream::connect(path)?,
        None => connect_or_start()?,
    };
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    s.write_all(&proto::frame(&Request::Version))?;
    match proto::read_msg(&mut s) {
        Ok(Reply::Version(v)) if v == proto::VERSION => {}
        // Older daemons don't know `Version` and drop the connection.
        _ => {
            return Err(io::Error::other(
                "the running daemon is from a different arranger build; restart it with `arranger stop` (this stops its agents)",
            ));
        }
    }
    s.set_read_timeout(None)?;
    Ok(s)
}

/// Stops the daemon with SIGTERM (it flushes logs; its agents get SIGHUP) and waits for it to go.
pub fn stop() -> io::Result<()> {
    let pid_file = proto::data_dir().join("daemon.pid");
    let pid: i32 = fs::read_to_string(&pid_file)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .ok_or_else(|| io::Error::other(format!("no {}; is the daemon running?", pid_file.display())))?;
    let pid = rustix::process::Pid::from_raw(pid).ok_or_else(|| io::Error::other("bad pid file"))?;
    rustix::process::kill_process(pid, rustix::process::Signal::TERM)?;
    for _ in 0..150 {
        if UnixStream::connect(proto::socket_path()).is_err() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(20));
    }
    Err(io::Error::other("daemon didn't stop within 3s"))
}

/// Connects to the daemon, starting it detached if nothing is listening.
fn connect_or_start() -> io::Result<UnixStream> {
    let sock = proto::socket_path();
    if let Ok(s) = UnixStream::connect(&sock) {
        return Ok(s);
    }
    let data = proto::data_dir();
    fs::create_dir_all(&data)?;
    let log = File::options().create(true).append(true).open(data.join("daemon.log"))?;
    // The daemon is always `arranger`, even when the GUI is the caller (which `arranger` names).
    let cli = env::var_os("ARRANGER_CLI").map_or_else(|| env::current_exe().map(|e| e.with_file_name("arranger")), |p| Ok(p.into()))?;
    let mut cmd = Command::new(cli);
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

pub fn request(req: &Request) -> io::Result<Reply> {
    request_at(None, req)
}

pub fn request_at(sock: Option<&Path>, req: &Request) -> io::Result<Reply> {
    let mut s = connect_at(sock)?;
    s.write_all(&proto::frame(req))?;
    match proto::read_msg(&mut s)? {
        Reply::Error(e) => Err(io::Error::other(e)),
        reply => Ok(reply),
    }
}

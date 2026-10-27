//! Talking to the daemon from any client: CLI, GUI or hook.

use crate::proto::{self, Reply, Request};
use crate::security;
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
        Some(path) => {
            security::check_socket(path)?;
            UnixStream::connect(path)?
        }
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

/// Stops the daemon over its protected socket and waits for it to go.
pub fn stop() -> io::Result<()> {
    let path = proto::socket_path();
    // Shutdown's wire format stays stable across protocol versions. In particular,
    // don't use connect_at: its version check would prevent stopping an old daemon
    // after an upgrade, which is exactly when users need this command.
    security::check_socket(&path)?;
    let mut stream = UnixStream::connect(&path)?;
    stream.write_all(&proto::frame(&Request::Shutdown))?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    match proto::read_msg(&mut stream)? {
        Reply::Ok => {}
        reply => return Err(io::Error::other(format!("unexpected shutdown reply: {reply:?}"))),
    }
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
    let data = proto::data_dir();
    security::private_dir(&data)?;
    security::private_dir(sock.parent().ok_or_else(|| io::Error::other("socket has no directory"))?)?;
    match security::check_socket(&sock) {
        Ok(()) => if let Ok(s) = UnixStream::connect(&sock) { return Ok(s); },
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let log = security::private_file(&data.join("daemon.log"), true)?;
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
        if security::check_socket(&sock).is_ok() && let Ok(s) = UnixStream::connect(&sock) {
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

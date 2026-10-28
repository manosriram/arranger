use rustix::fs::{self, Mode, OFlags};
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
use rustix::termios::{Winsize, tcsetwinsize};
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};

pub struct Pty {
    /// Non-blocking master side.
    pub master: OwnedFd,
    pub child: Child,
    owner: Option<std::io::PipeWriter>,
}

/// Wrap a command before configuring its stdio or pre_exec hooks. The pipe's only writer
/// belongs to the caller: dropping it (even on daemon SIGKILL) kills this process group.
/// The watcher stays in the group after the command exits, so its PGID cannot be recycled
/// before cleanup. It never inherits the agent's input/output sockets or terminal.
pub(crate) fn owned(cmd: Command) -> io::Result<(Command, std::io::PipeWriter)> {
    let (read, owner) = std::io::pipe()?;
    let mut wrapped = Command::new("/bin/sh");
    wrapped.args(["-c", "parent=$$; (trap '' HUP INT TERM; read -r ignored <&3; kill -KILL -\"$parent\") </dev/null >/dev/null 2>&1 & exec 3<&-; exec \"$@\"", "arranger-agent"])
        .arg(cmd.get_program()).args(cmd.get_args());
    if let Some(dir) = cmd.get_current_dir() { wrapped.current_dir(dir); }
    for (key, value) in cmd.get_envs() {
        if let Some(value) = value { wrapped.env(key, value); } else { wrapped.env_remove(key); }
    }
    unsafe {
        wrapped.pre_exec(move || {
            // Unlike rustix::dup2's OwnedFd target, raw dup2 also accepts a closed fd 3.
            // Stdio setup may just have closed it after duplicating it onto stdin/stderr.
            unsafe extern "C" { fn dup2(old: i32, new: i32) -> i32; }
            if dup2(read.as_raw_fd(), 3) == -1 {
                return Err(io::Error::last_os_error());
            }
            rustix::io::fcntl_setfd(BorrowedFd::borrow_raw(3), rustix::io::FdFlags::empty())?;
            Ok(())
        });
    }
    Ok((wrapped, owner))
}

impl Drop for Pty {
    fn drop(&mut self) {
        // Closing the lease also kills helpers left behind by a normally exited agent.
        self.owner.take();
        let _ = self.child.wait();
    }
}

/// Runs `cmd` as a session leader with a fresh PTY as its controlling terminal.
pub fn spawn(cmd: Command, cols: u16, rows: u16) -> io::Result<Pty> {
    let (mut cmd, owner) = owned(cmd)?;
    let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY)?;
    rustix::io::fcntl_setfd(&master, rustix::io::FdFlags::CLOEXEC)?;
    grantpt(&master)?;
    unlockpt(&master)?;
    let slave = fs::open(
        ptsname(&master, Vec::new())?,
        OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    resize(&master, cols, rows)?;
    cmd.stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    unsafe {
        cmd.pre_exec(|| {
            rustix::process::setsid()?;
            rustix::process::ioctl_tiocsctty(BorrowedFd::borrow_raw(0))?;
            Ok(())
        });
    }
    fs::fcntl_setfl(&master, OFlags::NONBLOCK)?;
    let child = cmd.spawn()?;
    Ok(Pty { master, child, owner: Some(owner) })
    // `cmd` drops here, closing our copies of the slave so EOF reaches the master.
}

/// Full-duplex stdio over a socket pair, no terminal, for headless agents; stderr goes to `stderr`.
/// Its own session, like `spawn`, so signals reach the whole process group.
pub fn spawn_stdio(cmd: Command, stderr: std::fs::File) -> io::Result<Pty> {
    let (mut cmd, owner) = owned(cmd)?;
    let (parent, child) = std::os::unix::net::UnixStream::pair()?;
    cmd.stdin(Stdio::from(OwnedFd::from(child.try_clone()?))).stdout(Stdio::from(OwnedFd::from(child))).stderr(stderr);
    unsafe {
        cmd.pre_exec(|| Ok(rustix::process::setsid().map(drop)?));
    }
    parent.set_nonblocking(true)?;
    let child = cmd.spawn()?;
    Ok(Pty { master: parent.into(), child, owner: Some(owner) })
}

pub fn resize(master: &OwnedFd, cols: u16, rows: u16) -> io::Result<()> {
    let size = Winsize { ws_col: cols, ws_row: rows, ws_xpixel: 0, ws_ypixel: 0 };
    Ok(tcsetwinsize(master, size)?)
}

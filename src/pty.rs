use rustix::fs::{self, Mode, OFlags};
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
use rustix::termios::{Winsize, tcsetwinsize};
use std::io;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};

pub struct Pty {
    /// Non-blocking master side.
    pub master: OwnedFd,
    pub child: Child,
}

/// Runs `cmd` as a session leader with a fresh PTY as its controlling terminal.
pub fn spawn(mut cmd: Command, cols: u16, rows: u16) -> io::Result<Pty> {
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
    let child = cmd.spawn()?;
    fs::fcntl_setfl(&master, OFlags::NONBLOCK)?;
    Ok(Pty { master, child })
    // `cmd` drops here, closing our copies of the slave so EOF reaches the master.
}

pub fn resize(master: &OwnedFd, cols: u16, rows: u16) -> io::Result<()> {
    let size = Winsize { ws_col: cols, ws_row: rows, ws_xpixel: 0, ws_ypixel: 0 };
    Ok(tcsetwinsize(master, size)?)
}

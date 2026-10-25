//! Remote machines: a daemon on another host, reached through the user's own `ssh` (their keys,
//! agent and ~/.ssh/config). `arranger endpoint` on the host starts its daemon if needed and prints
//! where its socket is; `ssh -L` forwards that socket to a local one, which clients then use exactly
//! like the local daemon's.

use crate::proto;
use crate::security;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Read};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Machine {
    pub name: String,
    /// `user@host` or a Host from ~/.ssh/config.
    pub ssh: String,
    /// Repo new tasks start in, as a path on that machine (`~/src/api`).
    #[serde(default)]
    pub repo: String,
}

fn config() -> PathBuf {
    proto::data_dir().join("machines.json")
}

pub fn load() -> Vec<Machine> {
    fs::read(config()).ok().and_then(|b| serde_json::from_slice::<Vec<Machine>>(&b).ok()).unwrap_or_default()
        .into_iter().filter(|m| validate_name(&m.name).is_ok() && validate_target(&m.ssh).is_ok()).collect()
}

pub fn save(list: &[Machine]) -> io::Result<()> {
    for m in list { validate_name(&m.name)?; validate_target(&m.ssh)?; }
    security::private_dir(&proto::data_dir())?;
    security::write_private(&config(), &serde_json::to_vec_pretty(list)?)
}

pub fn validate_name(name: &str) -> io::Result<()> {
    if name.is_empty() || name.len() > 32 || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return Err(io::Error::other("machine names must be 1–32 ASCII letters, digits, hyphens or underscores"));
    }
    Ok(())
}

pub fn validate_target(target: &str) -> io::Result<()> {
    if target.is_empty() || target.starts_with('-') || target.len() > 255 || !target.bytes().all(|b| b.is_ascii_alphanumeric() || b"@._-:[]+".contains(&b)) {
        return Err(io::Error::other("expected an SSH host or user@host, without options or shell characters"));
    }
    Ok(())
}

fn validate_forward_path(path: &Path) -> io::Result<()> {
    if !path.is_absolute() || path.to_str().is_none_or(|s| s.chars().any(|c| c.is_control() || c == ':' || c == '\\')) {
        return Err(io::Error::other("SSH socket paths must be absolute and contain no forwarding delimiters or control characters"));
    }
    Ok(())
}

/// Where a machine's daemon socket is forwarded to on this one.
pub fn local_socket(name: &str) -> PathBuf {
    use std::hash::{Hash, Hasher};
    // Bounded length matters on macOS, whose default temporary directory is already long.
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut hash);
    proto::socket_path().with_file_name(format!("m-{}-{:016x}.sock", std::process::id(), hash.finish()))
}

/// `ssh` that never prompts (keys only), requires a known host key, fails fast and
/// notices a dead link within ~45s.
fn ssh(target: &str) -> io::Result<Command> {
    validate_target(target)?;
    let mut c = Command::new("ssh");
    for opt in ["BatchMode=yes", "StrictHostKeyChecking=yes", "ForwardAgent=no", "ForwardX11=no", "StreamLocalBindMask=0177", "ConnectTimeout=10", "ServerAliveInterval=15", "ServerAliveCountMax=3"] {
        c.args(["-o", opt]);
    }
    Ok(c)
}

fn last_line(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    text.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("ssh failed").trim().to_string()
}

fn drain_bounded(reader: &mut impl Read, bytes: &mut Vec<u8>) -> io::Result<()> {
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                if bytes.len() + n > (64 << 10) { return Err(io::Error::other("SSH endpoint output exceeds 64 KiB")); }
                bytes.extend_from_slice(&buf[..n]);
            },
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {},
            Err(e) => return Err(e),
        }
    }
}

fn endpoint_output(cmd: &mut Command) -> io::Result<std::process::Output> {
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let result = (|| {
        let (out, err) = (child.stdout.as_mut().unwrap(), child.stderr.as_mut().unwrap());
        rustix::fs::fcntl_setfl(out, rustix::fs::OFlags::NONBLOCK)?;
        rustix::fs::fcntl_setfl(err, rustix::fs::OFlags::NONBLOCK)?;
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            drain_bounded(child.stdout.as_mut().unwrap(), &mut stdout)?;
            drain_bounded(child.stderr.as_mut().unwrap(), &mut stderr)?;
            if let Some(status) = child.try_wait()? {
                // Drain bytes written between the last read and process exit.
                drain_bounded(child.stdout.as_mut().unwrap(), &mut stdout)?;
                drain_bounded(child.stderr.as_mut().unwrap(), &mut stderr)?;
                return Ok(std::process::Output { status, stdout, stderr });
            }
            if Instant::now() >= deadline { return Err(io::Error::new(io::ErrorKind::TimedOut, "SSH endpoint timed out after 30s")); }
            thread::sleep(Duration::from_millis(20));
        }
    })();
    if result.is_err() { let _ = child.kill(); let _ = child.wait(); }
    result
}

/// Runs `arranger endpoint` on the machine, which starts its daemon if needed. Returns the
/// daemon's socket path there.
pub fn endpoint(target: &str) -> io::Result<PathBuf> {
    // Non-interactive ssh shells often miss the PATH additions where arranger is installed.
    let cmd = r#"PATH="$HOME/.local/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH" arranger endpoint"#;
    let out = endpoint_output(ssh(target)?.args(["--", target, cmd]))?;
    if !out.status.success() {
        let err = last_line(&out.stderr);
        if out.status.code() == Some(127) || err.contains("not found") {
            return Err(io::Error::other("arranger isn't installed there (see the README's install line)"));
        }
        return Err(io::Error::other(err));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let (version, sock) = text.trim().split_once(' ').ok_or_else(|| io::Error::other("unexpected reply from `arranger endpoint`"))?;
    if version != proto::VERSION.to_string() {
        return Err(io::Error::other(format!(
            "it runs arranger protocol {version}, this app {}; install the same release on both",
            proto::VERSION
        )));
    }
    let path = PathBuf::from(sock);
    validate_forward_path(&path)?;
    Ok(path)
}

/// Forwards the machine's daemon socket at `remote` to `local`. The tunnel lives as long as the
/// returned ssh process.
pub fn tunnel(target: &str, remote: &Path, local: &Path) -> io::Result<Child> {
    validate_target(target)?;
    validate_forward_path(remote)?;
    validate_forward_path(local)?;
    security::private_dir(local.parent().ok_or_else(|| io::Error::other("socket has no directory"))?)?;
    match security::check_socket(local) {
        Ok(()) => fs::remove_file(local)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut child = ssh(target)?
        .args(["-N", "-o", "ExitOnForwardFailure=yes", "-L"])
        .arg(format!("{}:{}", local.display(), remote.display()))
        .args(["--", target])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    for _ in 0..150 {
        if security::check_socket(local).is_ok() && UnixStream::connect(local).is_ok() {
            return Ok(child);
        }
        if child.try_wait()?.is_some() {
            let mut err = Vec::new();
            if let Some(e) = child.stderr.take() { err = security::read_limited(e, 64 << 10)?; }
            return Err(io::Error::other(last_line(&err)));
        }
        thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    Err(io::Error::other("the ssh tunnel didn't come up within 15s"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_socket_names_are_path_safe() {
        let sock = local_socket("gpu box/1");
        assert_eq!(sock.parent(), proto::socket_path().parent());
        assert!(sock.file_name().unwrap().len() < 40);
        assert_ne!(sock, local_socket("gpu_box_1"));
    }

    #[test]
    fn rejects_ssh_option_injection_and_ambiguous_forwarding() {
        for host in ["", "-oProxyCommand=evil", "host\n-oProxyCommand=evil", "user@host;id", "$(id)"] {
            assert!(validate_target(host).is_err());
        }
        for host in ["gpu-box", "user@example.com", "user@[::1]"] { assert!(validate_target(host).is_ok()); }
        for path in ["relative.sock", "/tmp/sock:22:host", "/tmp/sock\n"] {
            assert!(validate_forward_path(Path::new(path)).is_err());
        }
        assert!(validate_name("gpu box/1").is_err());
    }

    #[test]
    fn endpoint_capture_is_bounded() {
        let output = endpoint_output(Command::new("sh").args(["-c", "printf '19 /tmp/test.sock\\n'; printf diagnostic >&2"])).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"19 /tmp/test.sock\n");
        assert_eq!(output.stderr, b"diagnostic");
        let mut bytes = Vec::new();
        assert!(drain_bounded(&mut std::io::repeat(b'x'), &mut bytes).is_err());
        assert!(bytes.len() <= (64 << 10));
    }
}

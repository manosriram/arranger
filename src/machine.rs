//! Remote machines: a daemon on another host, reached through the user's own `ssh` (their keys,
//! agent and ~/.ssh/config). `arranger endpoint` on the host starts its daemon if needed and prints
//! where its socket is; `ssh -L` forwards that socket to a local one, which clients then use exactly
//! like the local daemon's.

use crate::proto;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Read};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

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
    fs::read(config()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn save(list: &[Machine]) -> io::Result<()> {
    fs::create_dir_all(proto::data_dir())?;
    fs::write(config(), serde_json::to_vec_pretty(list)?)
}

/// Where a machine's daemon socket is forwarded to on this one.
pub fn local_socket(name: &str) -> PathBuf {
    let safe: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
    proto::socket_path().with_file_name(format!("arranger-{safe}.sock"))
}

/// `ssh` that never prompts (keys only), trusts a new host key on first use, fails fast and
/// notices a dead link within ~45s.
fn ssh(target: &str) -> Command {
    let mut c = Command::new("ssh");
    for opt in ["BatchMode=yes", "StrictHostKeyChecking=accept-new", "ConnectTimeout=10", "ServerAliveInterval=15", "ServerAliveCountMax=3"] {
        c.args(["-o", opt]);
    }
    c.arg(target);
    c
}

fn last_line(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    text.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("ssh failed").trim().to_string()
}

/// Runs `arranger endpoint` on the machine, which starts its daemon if needed. Returns the
/// daemon's socket path there.
pub fn endpoint(target: &str) -> io::Result<PathBuf> {
    // Non-interactive ssh shells often miss the PATH additions where arranger is installed.
    let cmd = r#"PATH="$HOME/.local/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH" arranger endpoint"#;
    let out = ssh(target).arg(cmd).stdin(Stdio::null()).output()?;
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
    Ok(sock.into())
}

/// Forwards the machine's daemon socket at `remote` to `local`. The tunnel lives as long as the
/// returned ssh process.
pub fn tunnel(target: &str, remote: &Path, local: &Path) -> io::Result<Child> {
    let _ = fs::remove_file(local);
    let mut child = ssh(target)
        .args(["-N", "-o", "ExitOnForwardFailure=yes", "-L"])
        .arg(format!("{}:{}", local.display(), remote.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    for _ in 0..150 {
        if UnixStream::connect(local).is_ok() {
            return Ok(child);
        }
        if child.try_wait()?.is_some() {
            let mut err = Vec::new();
            child.stderr.take().map(|mut e| e.read_to_end(&mut err));
            return Err(io::Error::other(last_line(&err)));
        }
        thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    Err(io::Error::other("the ssh tunnel didn't come up within 15s"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_socket_names_are_path_safe() {
        assert!(local_socket("gpu box/1").ends_with("arranger-gpu_box_1.sock"));
    }
}

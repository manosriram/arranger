//! Client <-> daemon messages: u32 little-endian length, then a postcard body.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::env;
use std::io::{self, Read};
use std::path::PathBuf;

const MAX_FRAME: usize = 64 << 20;

#[derive(Serialize, Deserialize, Debug)]
pub enum Request {
    Ping,
    New { agent: String, repo: PathBuf, base: Option<String> },
    List,
    Kill { id: u64 },
    /// Turns the connection into a stream: Output/Exited down, Input/Resize up.
    Attach { id: u64, cols: u16, rows: u16 },
    Input(Vec<u8>),
    Resize { cols: u16, rows: u16 },
}

#[derive(Serialize, Deserialize, Debug)]
pub enum Reply {
    Pong,
    Ok,
    Created { id: u64, state: String },
    Tasks(Vec<TaskInfo>),
    Output(Vec<u8>),
    Exited,
    Error(String),
}

#[derive(Serialize, Deserialize, Debug)]
pub struct TaskInfo {
    pub id: u64,
    pub agent: String,
    pub branch: String,
    pub worktree: String,
    pub state: String,
}

pub fn frame<T: Serialize>(msg: &T) -> Vec<u8> {
    let body = postcard::to_stdvec(msg).expect("postcard serialize");
    let mut out = (body.len() as u32).to_le_bytes().to_vec();
    out.extend(body);
    out
}

/// Pops one complete message off the front of `buf`, if there is one.
pub fn unframe<T: DeserializeOwned>(buf: &mut Vec<u8>) -> Option<io::Result<T>> {
    let len = u32::from_le_bytes(buf.get(..4)?.try_into().unwrap()) as usize;
    if len > MAX_FRAME {
        return Some(Err(io::Error::other("frame too large")));
    }
    let body = buf.get(4..4 + len)?;
    let msg = postcard::from_bytes(body).map_err(io::Error::other);
    buf.drain(..4 + len);
    Some(msg)
}

pub fn read_msg<T: DeserializeOwned>(r: &mut impl Read) -> io::Result<T> {
    let mut len = [0; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::other("frame too large"));
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body)?;
    postcard::from_bytes(&body).map_err(io::Error::other)
}

/// `$ARRANGER_HOME` overrides both paths (used by bench-mem).
pub fn socket_path() -> PathBuf {
    if let Some(home) = env::var_os("ARRANGER_HOME") {
        return PathBuf::from(home).join("arranger.sock");
    }
    let dir = env::var_os("XDG_RUNTIME_DIR")
        .or_else(|| env::var_os("TMPDIR"))
        .unwrap_or("/tmp".into());
    PathBuf::from(dir).join("arranger.sock")
}

pub fn data_dir() -> PathBuf {
    if let Some(home) = env::var_os("ARRANGER_HOME") {
        return home.into();
    }
    let home = PathBuf::from(env::var_os("HOME").expect("HOME is not set"));
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/arranger")
    } else {
        env::var_os("XDG_DATA_HOME")
            .map_or(home.join(".local/share"), PathBuf::from)
            .join("arranger")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unframe_waits_for_whole_frames() {
        let mut buf = frame(&Request::Kill { id: 7 });
        buf.extend(frame(&Request::Ping));
        let second = buf.split_off(buf.len() - 2);
        assert!(matches!(unframe(&mut buf), Some(Ok(Request::Kill { id: 7 }))));
        buf.extend(&second[..1]);
        assert!(unframe::<Request>(&mut buf).is_none());
        buf.extend(&second[1..]);
        assert!(matches!(unframe(&mut buf), Some(Ok(Request::Ping))));
        assert!(buf.is_empty());
    }
}

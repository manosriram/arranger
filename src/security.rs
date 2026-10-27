//! Local trust boundary: private directories, no-follow file opens and bounded decoding.
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path};

/// Create a user-owned directory without following a symlink at its final component.
/// Existing application directories are tightened to protect data from older releases.
pub fn private_dir(path: &Path) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(io::Error::other("arranger paths must be absolute"));
    }
    match DirBuilder::new().recursive(true).mode(0o700).create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    check_ancestors(path)?;
    let file = OpenOptions::new().read(true).custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::DIRECTORY).bits() as i32).open(path)?;
    let meta = file.metadata()?;
    if !meta.is_dir() || meta.uid() != rustix::process::geteuid().as_raw() {
        return Err(io::Error::other(format!("{} must be a user-owned directory, not a symlink", path.display())));
    }
    file.set_permissions(fs::Permissions::from_mode(0o700))
}

fn check_ancestors(path: &Path) -> io::Result<()> {
    let uid = rustix::process::geteuid().as_raw();
    let check = |path: &Path| -> io::Result<()> {
        for ancestor in path.ancestors().skip(1) {
            let meta = fs::symlink_metadata(ancestor)?;
            if (meta.uid() != 0 && meta.uid() != uid) || (!meta.file_type().is_symlink() && meta.mode() & 0o022 != 0 && meta.mode() & 0o1000 == 0) {
                return Err(io::Error::other(format!("{} is an unsafe parent for private arranger state", ancestor.display())));
            }
        }
        Ok(())
    };
    check(path)?;
    check(&fs::canonicalize(path)?)
}

/// Reject public, foreign-owned or symlinked socket directories before connecting.
pub fn check_socket_dir(path: &Path) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| io::Error::other("socket has no directory"))?;
    check_ancestors(parent)?;
    let meta = fs::symlink_metadata(parent)?;
    if !meta.is_dir() || meta.uid() != rustix::process::geteuid().as_raw() || meta.mode() & 0o077 != 0 {
        return Err(io::Error::other("socket directory must be private and owned by the current user"));
    }
    Ok(())
}

pub fn check_socket(path: &Path) -> io::Result<()> {
    check_socket_dir(path)?;
    let meta = fs::symlink_metadata(path)?;
    if !meta.file_type().is_socket() || meta.uid() != rustix::process::geteuid().as_raw() || meta.mode() & 0o077 != 0 {
        return Err(io::Error::other("refusing an unsafe arranger socket"));
    }
    Ok(())
}

/// Open sensitive application files without following symlinks or modifying hard-link targets.
pub fn private_file(path: &Path, append: bool) -> io::Result<File> {
    let file = OpenOptions::new().read(true).write(true).create(true).append(append)
        .mode(0o600).custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32).open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.uid() != rustix::process::geteuid().as_raw() || meta.nlink() != 1 {
        return Err(io::Error::other("refusing an unsafe arranger data file"));
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut file = private_file(path, false)?;
    file.set_len(0)?;
    file.write_all(bytes)
}

pub fn read_limited(reader: impl Read, limit: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(io::Error::other("data exceeds the size limit"));
    }
    Ok(bytes)
}

pub const MAX_LOG: usize = 64 << 20;
pub const MAX_JSON_LINE: usize = 1 << 20;
/// An agent's own stdout records: a file it read (an image, a PDF) comes back inline as base64.
pub const MAX_AGENT_LINE: usize = 32 << 20;

/// Enforce the limit while assembling fragments, and discard an entire oversized record.
/// Parsing a suffix as a fresh JSON message could otherwise forge protocol events.
pub struct JsonLines {
    input: Vec<u8>,
    discarding: bool,
    limit: usize,
}

impl Default for JsonLines {
    fn default() -> Self {
        JsonLines::new(MAX_JSON_LINE)
    }
}

impl JsonLines {
    pub fn new(limit: usize) -> Self {
        JsonLines { input: Vec::new(), discarding: false, limit }
    }

    pub fn feed(&mut self, bytes: &[u8]) -> (Vec<Vec<u8>>, bool) {
        let mut lines = Vec::new();
        let mut oversized = false;
        for part in bytes.split_inclusive(|b| *b == b'\n') {
            let complete = part.last() == Some(&b'\n');
            if self.discarding {
                if complete { self.discarding = false; }
                continue;
            }
            if self.input.len().saturating_add(part.len()) > self.limit {
                self.input.clear();
                self.discarding = !complete;
                oversized = true;
                continue;
            }
            self.input.extend_from_slice(part);
            if complete { lines.push(std::mem::take(&mut self.input)); }
        }
        (lines, oversized)
    }
}

pub fn decode_log(reader: impl Read) -> io::Result<Vec<u8>> {
    read_limited(zstd::stream::read::Decoder::new(reader)?, MAX_LOG)
}

/// Clipboard and prompt text must not terminate bracketed paste or inject terminal keystrokes.
pub fn paste_text(text: &str, bracketed: bool) -> String {
    let clean: String = text.chars().filter_map(|c| match c {
        '\n' | '\r' | '\t' if !bracketed => Some(' '),
        '\r' => Some('\n'),
        '\n' | '\t' => Some(c),
        c if c.is_control() => None,
        c => Some(c),
    }).collect();
    if bracketed { format!("\x1b[200~{clean}\x1b[201~") } else { clean }
}

/// Walk with directory handles so even a replaced intermediate directory cannot redirect a read.
/// Symlinks are displayed as their link text, never followed into credentials or special files.
pub fn worktree_file(root: &Path, path: &Path, limit: usize) -> io::Result<Vec<u8>> {
    use rustix::fs::{open, openat, readlinkat, Mode, OFlags};
    relative_path(path)?;
    let mut parts = path.components().peekable();
    let mut dir = open(root, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::empty())?;
    while let Some(part) = parts.next() {
        if parts.peek().is_some() {
            dir = openat(&dir, part.as_os_str(), OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::empty())?;
        } else {
            // readlinkat returns EINVAL for a regular file, and never follows the final component.
            if let Ok(link) = readlinkat(&dir, part.as_os_str(), Vec::new()) {
                return Ok(link.into_bytes());
            }
            let file = File::from(openat(&dir, part.as_os_str(), OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC, Mode::empty())?);
            if !file.metadata()?.is_file() { return Err(io::Error::other("refusing to read a special file")); }
            return read_limited(file, limit);
        }
    }
    Err(io::Error::other("empty worktree path"))
}

/// Copy untracked files with anchored directories; preserve links without reading their targets.
pub fn copy_worktree_file(from: &Path, to: &Path, path: &Path) -> io::Result<()> {
    use rustix::fs::{mkdirat, open, openat, readlinkat, symlinkat, Mode, OFlags};
    relative_path(path)?;
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let (mut src, mut dst) = (open(from, flags, Mode::empty())?, open(to, flags, Mode::empty())?);
    let mut parts = path.components().peekable();
    while let Some(part) = parts.next() {
        let name = part.as_os_str();
        if parts.peek().is_some() {
            src = openat(&src, name, flags, Mode::empty())?;
            match mkdirat(&dst, name, Mode::from_raw_mode(0o700)) {
                Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                Err(e) => return Err(e.into()),
            }
            dst = openat(&dst, name, flags, Mode::empty())?;
        } else {
            if let Ok(link) = readlinkat(&src, name, Vec::new()) {
                symlinkat(&link, &dst, name)?;
                return Ok(());
            }
            let mut input = File::from(openat(&src, name, OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC, Mode::empty())?);
            let meta = input.metadata()?;
            if !meta.is_file() { return Err(io::Error::other("cannot clone a special file")); }
            let mut output = File::from(openat(&dst, name, OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::from_raw_mode((meta.mode() & 0o777) as _))?);
            io::copy(&mut input, &mut output)?;
            return Ok(());
        }
    }
    Err(io::Error::other("empty clone path"))
}

pub fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\"'\"'"))
}

/// Git paths and session filenames must never introduce an absolute path or traversal.
pub fn relative_path(path: &Path) -> io::Result<()> {
    if path.as_os_str().is_empty() || path.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(io::Error::other("expected a relative path without traversal"));
    }
    Ok(())
}

/// Non-terminal CLI output can contain agent-controlled text. Render controls visibly.
pub fn terminal_text(text: &str) -> String {
    text.chars().flat_map(|c| {
        if c.is_control() && c != '\n' && c != '\t' { c.escape_default().collect::<Vec<_>>() } else { vec![c] }
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_links_and_public_socket_directories() {
        let dir = std::env::temp_dir().join(format!("arranger-security-{}", std::process::id()));
        private_dir(&dir).unwrap();
        let target = dir.join("target");
        fs::write(&target, "secret").unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(write_private(&link, b"overwrite").is_err());
        assert!(private_dir(&link).is_err());
        fs::remove_file(&link).unwrap();
        fs::hard_link(&target, &link).unwrap();
        assert!(write_private(&link, b"overwrite").is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "secret");
        assert!(check_socket_dir(&dir.join("socket")).is_ok());
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(check_socket_dir(&dir.join("socket")).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn bounds_decompression_and_rejects_traversal() {
        assert!(read_limited(&b"12345"[..], 4).is_err());
        assert_eq!(read_limited(&b"1234"[..], 4).unwrap(), b"1234");
        for path in ["../secret", "/secret", "a/../../secret", ""] {
            assert!(relative_path(Path::new(path)).is_err());
        }
        assert!(relative_path(Path::new("src/a.rs")).is_ok());
        assert_eq!(terminal_text("x\x1b]52;c;secret\x07"), "x\\u{1b}]52;c;secret\\u{7}");
        assert_eq!(paste_text("a\x1b[201~\r\n\x03b", true), "\x1b[200~a[201~\n\nb\x1b[201~");
        assert_eq!(paste_text("echo a\necho b\r", false), "echo a echo b ");
    }

    #[test]
    fn oversized_json_cannot_reenter_the_protocol_mid_record() {
        let mut lines = JsonLines::default();
        assert!(lines.feed(&vec![b'x'; MAX_JSON_LINE + 1]).1);
        let (kept, _) = lines.feed(b"{\"forged\":true}\n{\"valid\":true}\n");
        assert_eq!(kept, vec![b"{\"valid\":true}\n".to_vec()]);
        assert!(lines.feed(&vec![b'x'; MAX_JSON_LINE]).0.is_empty());
        assert!(lines.feed(b"\n").1);
    }

    #[test]
    fn review_and_clone_preserve_symlinks_without_reading_targets() {
        let dir = std::env::temp_dir().join(format!("arranger-links-{}", std::process::id()));
        let (from, to) = (dir.join("from"), dir.join("to"));
        private_dir(&from).unwrap();
        private_dir(&to).unwrap();
        fs::write(dir.join("secret"), "private credentials").unwrap();
        std::os::unix::fs::symlink("../secret", from.join("link")).unwrap();
        assert_eq!(worktree_file(&from, Path::new("link"), 1024).unwrap(), b"../secret");
        copy_worktree_file(&from, &to, Path::new("link")).unwrap();
        assert_eq!(fs::read_link(to.join("link")).unwrap(), Path::new("../secret"));
        std::os::unix::fs::symlink("..", from.join("escape")).unwrap();
        assert!(worktree_file(&from, Path::new("escape/secret"), 1024).is_err());
        fs::create_dir(from.join("nested")).unwrap();
        fs::write(from.join("nested/file"), "text").unwrap();
        std::os::unix::fs::symlink("..", to.join("nested")).unwrap();
        assert!(copy_worktree_file(&from, &to, Path::new("nested/file")).is_err());
        assert!(!dir.join("file").exists());
        fs::remove_dir_all(dir).unwrap();
    }
}

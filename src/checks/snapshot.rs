//! Disposable check worktrees. Only Git objects from the captured tree are checked out;
//! neither the source index nor ignored build output is copied.
//!
//! The caller must supply an absent destination in its private verification run directory.
//! This is isolation from accidental edits, not a sandbox for hostile check commands.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug)]
pub struct Snapshot {
    source: PathBuf,
    path: PathBuf,
    common: PathBuf,
    admin: PathBuf,
    gitfile: Vec<u8>,
}

/// Materialize an exact tree returned by `checks::tree_hash`, without moving a branch.
/// The parent directory must already exist and be owned by the daemon. Existing
/// destinations (including empty directories and symlinks) are never adopted.
pub fn create(source: &Path, destination: &Path, tree: &str) -> Result<Snapshot, String> {
    if !matches!(tree.len(), 40 | 64) || !tree.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("snapshot requires a full Git tree object ID".into());
    }
    let source = fs::canonicalize(source).map_err(|e| format!("snapshot source: {e}"))?;
    let root = PathBuf::from(output_text(git(&source).args(["rev-parse", "--show-toplevel"]))?);
    if fs::canonicalize(root).map_err(|e| e.to_string())? != source {
        return Err("snapshot source must be a worktree root".into());
    }
    let common = common_dir(&source)?;
    if output_text(git(&source).args(["cat-file", "-t", tree]))? != "tree" {
        return Err("snapshot object is not a Git tree".into());
    }
    let entries = output(git(&source).args(["ls-tree", "-r", "-z", tree]), None)?;
    if entries.split(|b| *b == 0).any(|entry| entry.starts_with(b"160000 ")) {
        return Err("snapshot cannot faithfully materialize submodules".into());
    }
    // Also catch LFS pointer blobs whose attributes have been removed or changed.
    let pointers = git(&source)
        .args(["grep", "-I", "-l", "-e", "^version https://git-lfs.github.com/spec/v1$", tree, "--"])
        .output().map_err(|e| format!("git: {e}"))?;
    match pointers.status.code() {
        Some(0) => return Err("snapshot cannot faithfully materialize Git LFS pointers".into()),
        Some(1) => {}
        _ => return Err(format!("checking snapshot for Git LFS: {}", String::from_utf8_lossy(&pointers.stderr).trim())),
    }

    let name = destination.file_name().ok_or("snapshot destination needs a run directory name")?;
    let parent = fs::canonicalize(destination.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")))
        .map_err(|e| format!("snapshot destination parent: {e}"))?;
    let path = parent.join(name);
    if path.starts_with(&source) || source.starts_with(&path) || path.starts_with(&common) {
        return Err("snapshot destination must be outside the source and Git directories".into());
    }
    let commit = output_text(git(&source).args(["commit-tree", tree, "-m", "arranger verification snapshot"]))?;
    fs::create_dir(&path).map_err(|e| format!("reserve snapshot destination: {e}"))?;
    // No checkout means no post-checkout hook, filters or submodule recursion here.
    let added = output(git(&source).args(["worktree", "add", "--detach", "--no-checkout"]).arg(&path).arg(&commit), None);
    if let Err(error) = added {
        // Git normally rolls back its registration on failure. Never recursively
        // delete a directory whose ownership/registration we could not establish.
        if let Ok(snapshot) = owned(&source, &path, &common) {
            let _ = snapshot.remove();
        } else {
            let _ = fs::remove_dir(&path);
        }
        return Err(format!("create snapshot worktree: {error}"));
    }
    let snapshot = match owned(&source, &path, &common) {
        Ok(snapshot) => snapshot,
        Err(error) => return Err(format!("snapshot ownership could not be established (left for inspection): {error}")),
    };
    // From here any error drops an owned worktree, including failed checkouts.
    let marker = owner_record(&snapshot.source, &snapshot.path, &snapshot.common, &snapshot.admin, &snapshot.gitfile);
    fs::OpenOptions::new().write(true).create_new(true).open(snapshot.admin.join("arranger-snapshot-owner"))
        .and_then(|mut file| file.write_all(&marker)).map_err(|e| format!("record snapshot ownership: {e}"))?;
    output(git(&snapshot.path).args(["read-tree", tree]), None)?;
    let mut names = Vec::new();
    for entry in entries.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        let tab = entry.iter().position(|b| *b == b'\t').ok_or("invalid Git tree listing")?;
        names.extend_from_slice(&entry[tab + 1..]);
        names.push(0);
    }
    let attrs = output(git(&snapshot.path).args(["check-attr", "--cached", "-z", "--stdin", "filter"]), Some(&names))?;
    for fields in attrs.split(|b| *b == 0).collect::<Vec<_>>().chunks(3) {
        if fields.len() == 3 && !matches!(fields[2], b"unspecified" | b"unset") {
            return Err(format!("snapshot cannot faithfully materialize checkout filters (including Git LFS): {}", String::from_utf8_lossy(fields[2])));
        }
    }
    output(git(&snapshot.path).args(["checkout-index", "--all", "--force"]), None)?;
    snapshot.verify_contents(tree)?;
    Ok(snapshot)
}

/// Recover a snapshot after daemon restart. Only worktrees bearing the exact
/// ownership record written by `create` for this source and destination qualify.
/// Missing paths/linkage, symlinked paths, ordinary worktrees and old snapshots
/// without an ownership record are refused, never adopted or recursively deleted.
pub fn remove_abandoned(source: &Path, path: &Path) -> Result<(), String> {
    let source = unaliased_path(source)?;
    let path = unaliased_path(path)?;
    let root = PathBuf::from(output_text(git(&source).args(["rev-parse", "--show-toplevel"]))?);
    if fs::canonicalize(root).map_err(|e| e.to_string())? != source {
        return Err("snapshot source must be a worktree root".into());
    }
    let common = common_dir(&source)?;
    if path.starts_with(&source) || source.starts_with(&path) || path.starts_with(&common) || common.starts_with(&path) {
        return Err("refusing abandoned cleanup of source or Git directories".into());
    }
    // Validate the durable marker before constructing a handle: its Drop may
    // remove a worktree, so merely finding a valid .git file is not ownership.
    let (gitfile, admin) = registration(&path, &common)?;
    let marker = admin.join("arranger-snapshot-owner");
    if !fs::symlink_metadata(&marker).map_err(|e| format!("snapshot ownership record: {e}"))?.file_type().is_file()
        || fs::read(&marker).map_err(|e| e.to_string())? != owner_record(&source, &path, &common, &admin, &gitfile)
    {
        return Err("abandoned snapshot ownership record does not match; refusing cleanup".into());
    }
    owned(&source, &path, &common)?.remove()
}

fn unaliased_path(path: &Path) -> Result<PathBuf, String> {
    let absolute = if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir().map_err(|e| e.to_string())?.join(path) };
    let canonical = fs::canonicalize(&absolute).map_err(|e| format!("snapshot cleanup path: {e}"))?;
    if canonical != absolute {
        return Err("snapshot cleanup refuses symlinked or aliased paths".into());
    }
    Ok(canonical)
}

fn owner_record(source: &Path, path: &Path, common: &Path, admin: &Path, gitfile: &[u8]) -> Vec<u8> {
    let mut record = format!("arranger snapshot v1\nsource: {source:?}\npath: {path:?}\ncommon: {common:?}\nadmin: {admin:?}\ngitfile: ").into_bytes();
    record.extend_from_slice(gitfile);
    record
}

impl Snapshot {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Fail closed if either the disposable checkout or the source being judged
    /// has changed/disappeared. Nothing is restored in either worktree.
    pub fn verify(&self, tree: &str) -> Result<(), String> {
        self.verify_contents(tree)?;
        if common_dir(&self.source)? != self.common {
            return Err("snapshot source repository changed".into());
        }
        let actual = super::tree_hash(&self.source).map_err(|e| format!("snapshot source unavailable: {e}"))?;
        if actual != tree {
            return Err(format!("snapshot source changed: expected {tree}, found {actual}"));
        }
        Ok(())
    }

    fn verify_contents(&self, tree: &str) -> Result<(), String> {
        self.check_owned()?;
        let actual = super::tree_hash(&self.path).map_err(|e| format!("snapshot unavailable: {e}"))?;
        if actual != tree {
            return Err(format!("snapshot changed: expected {tree}, found {actual}"));
        }
        Ok(())
    }

    /// Remove this owned worktree and its registration, including ignored build
    /// output. Refuse cleanup if its Git linkage or canonical path was replaced.
    /// Repeated removal succeeds; removing the source is never attempted.
    pub fn remove(&self) -> Result<(), String> {
        if !self.path.try_exists().map_err(|e| e.to_string())? && !self.admin.try_exists().map_err(|e| e.to_string())? {
            return Ok(());
        }
        self.check_owned()?;
        output(git_dir(&self.common).args(["worktree", "remove", "--force", "--force"]).arg(&self.path), None)?;
        Ok(())
    }

    fn check_owned(&self) -> Result<(), String> {
        if fs::canonicalize(&self.path).map_err(|e| format!("snapshot path: {e}"))? != self.path
            || fs::symlink_metadata(self.path.join(".git")).map_err(|e| e.to_string())?.file_type().is_symlink()
            || fs::read(self.path.join(".git")).map_err(|e| e.to_string())? != self.gitfile
            || fs::canonicalize(&self.admin).map_err(|e| e.to_string())? != self.admin
            || fs::symlink_metadata(self.admin.join("gitdir")).map_err(|e| e.to_string())?.file_type().is_symlink()
            || fs::read_to_string(self.admin.join("gitdir")).map_err(|e| e.to_string())?.trim_end() != self.path.join(".git").to_str().ok_or("non-UTF-8 snapshot path")?
            || fs::symlink_metadata(self.admin.join("commondir")).map_err(|e| e.to_string())?.file_type().is_symlink()
        {
            return Err("snapshot ownership changed; refusing cleanup or verification".into());
        }
        let common = fs::read_to_string(self.admin.join("commondir")).map_err(|e| e.to_string())?;
        if fs::canonicalize(self.admin.join(common.trim_end())).map_err(|e| e.to_string())? != self.common {
            return Err("snapshot common directory changed".into());
        }
        Ok(())
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        let _ = self.remove();
    }
}

fn owned(source: &Path, path: &Path, common: &Path) -> Result<Snapshot, String> {
    let (gitfile, admin) = registration(path, common)?;
    let snapshot = Snapshot { source: source.into(), path: path.into(), common: common.into(), admin, gitfile };
    // Drop repeats this guard before attempting any removal, including on error.
    snapshot.check_owned()?;
    Ok(snapshot)
}

fn registration(path: &Path, common: &Path) -> Result<(Vec<u8>, PathBuf), String> {
    if !fs::symlink_metadata(path.join(".git")).map_err(|e| e.to_string())?.file_type().is_file() {
        return Err("snapshot .git must be a regular linkage file".into());
    }
    let gitfile = fs::read(path.join(".git")).map_err(|e| e.to_string())?;
    let text = std::str::from_utf8(&gitfile).map_err(|e| e.to_string())?;
    let admin = fs::canonicalize(text.trim_end().strip_prefix("gitdir: ").ok_or("invalid snapshot .git file")?).map_err(|e| e.to_string())?;
    if admin.parent() != Some(common.join("worktrees").as_path()) {
        return Err("snapshot registration is not a linked worktree".into());
    }
    Ok((gitfile, admin))
}

fn common_dir(source: &Path) -> Result<PathBuf, String> {
    let dir = output_text(git(source).args(["rev-parse", "--path-format=absolute", "--git-common-dir"]))?;
    fs::canonicalize(dir).map_err(|e| format!("snapshot Git directory: {e}"))
}

fn git(source: &Path) -> Command {
    let mut command = command();
    command.arg("-C").arg(source);
    command
}

fn git_dir(common: &Path) -> Command {
    let mut command = command();
    command.arg("--git-dir").arg(common);
    command
}

fn command() -> Command {
    let mut command = Command::new("git");
    command.args(["-c", "core.hooksPath=/dev/null", "-c", "commit.gpgSign=false", "-c", "core.autocrlf=false", "-c", "submodule.recurse=false"]);
    for key in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR"] {
        command.env_remove(key);
    }
    for key in ["GIT_AUTHOR_NAME", "GIT_COMMITTER_NAME"] {
        command.env(key, "arranger verification");
    }
    for key in ["GIT_AUTHOR_EMAIL", "GIT_COMMITTER_EMAIL"] {
        command.env(key, "verification@arranger.invalid");
    }
    command
}

fn output_text(command: &mut Command) -> Result<String, String> {
    String::from_utf8(output(command, None)?).map(|s| s.trim().to_string()).map_err(|e| e.to_string())
}

fn output(command: &mut Command, input: Option<&[u8]>) -> Result<Vec<u8>, String> {
    command.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|e| format!("git: {e}"))?;
    // Drain stdout concurrently with writing stdin, even for very large trees.
    let result = std::thread::scope(|scope| {
        let writer = input.map(|bytes| {
            let mut stdin = child.stdin.take().expect("piped Git stdin");
            scope.spawn(move || stdin.write_all(bytes))
        });
        let result = child.wait_with_output().map_err(|e| format!("git: {e}"));
        if let Some(writer) = writer {
            let written = writer.join().map_err(|_| "Git input writer panicked".to_string())?;
            // Prefer Git's diagnostic if it exited before reading all its input.
            if result.as_ref().is_ok_and(|out| out.status.success()) {
                written.map_err(|e| format!("git input: {e}"))?;
            }
        }
        result
    })?;
    if !result.status.success() {
        return Err(format!("git: {}", String::from_utf8_lossy(&result.stderr).trim()));
    }
    Ok(result.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Repo(PathBuf);

    impl Repo {
        fn new() -> Self {
            let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            let root = std::env::temp_dir().join(format!("arranger-snapshot-{}-{nanos}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            fs::create_dir_all(root.join("source")).unwrap();
            fs::create_dir_all(root.join("data/verification")).unwrap();
            let repo = Self(root);
            repo.git(&["init", "-q", "-b", "main"]);
            repo.write("tracked", "original\n");
            repo.write(".gitignore", "target/\n");
            repo.git(&["add", "-A"]);
            repo.git(&["commit", "-qm", "initial"]);
            repo
        }

        fn source(&self) -> PathBuf { self.0.join("source") }
        fn destination(&self) -> PathBuf { self.0.join("data/verification/run") }
        fn write(&self, path: &str, text: &str) { fs::write(self.source().join(path), text).unwrap(); }
        fn git(&self, args: &[&str]) -> String { output_text(git(&self.source()).args(args)).unwrap() }
        fn tree(&self) -> String { super::super::tree_hash(&self.source()).unwrap() }
    }

    impl Drop for Repo {
        fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
    }

    #[test]
    fn captures_dirty_files_without_touching_source_index_or_branch() {
        let repo = Repo::new();
        repo.write("tracked", "staged\n");
        repo.git(&["add", "tracked"]);
        repo.write("tracked", "unstaged\n");
        repo.write("untracked", "new\n");
        fs::create_dir(repo.source().join("target")).unwrap();
        repo.write("target/output", "ignored build output");
        let index = fs::read(repo.source().join(".git/index")).unwrap();
        let head = repo.git(&["rev-parse", "HEAD"]);
        let tree = repo.tree();
        let snapshot = create(&repo.source(), &repo.destination(), &tree).unwrap();
        snapshot.verify(&tree).unwrap();
        assert_eq!(fs::read_to_string(snapshot.path().join("tracked")).unwrap(), "unstaged\n");
        assert_eq!(fs::read_to_string(snapshot.path().join("untracked")).unwrap(), "new\n");
        assert!(!snapshot.path().join("target").exists());
        assert_eq!(fs::read(repo.source().join(".git/index")).unwrap(), index);
        assert_eq!(repo.git(&["rev-parse", "HEAD"]), head);
        assert_eq!(repo.git(&["symbolic-ref", "HEAD"]), "refs/heads/main");
        assert!(!git(snapshot.path()).args(["symbolic-ref", "-q", "HEAD"]).status().unwrap().success());
        snapshot.remove().unwrap();
        snapshot.remove().unwrap();
        assert!(!repo.destination().exists());
        assert_eq!(repo.git(&["worktree", "list", "--porcelain"]).matches("worktree ").count(), 1);
        assert_eq!(fs::read(repo.source().join(".git/index")).unwrap(), index);
    }

    #[test]
    fn uses_captured_objects_even_when_source_changes_and_fails_closed() {
        let repo = Repo::new();
        repo.write("untracked", "captured\n");
        let tree = repo.tree();
        repo.write("tracked", "later edit\n");
        fs::remove_file(repo.source().join("untracked")).unwrap();
        let snapshot = create(&repo.source(), &repo.destination(), &tree).unwrap();
        assert_eq!(fs::read_to_string(snapshot.path().join("tracked")).unwrap(), "original\n");
        assert_eq!(fs::read_to_string(snapshot.path().join("untracked")).unwrap(), "captured\n");
        assert!(snapshot.verify(&tree).unwrap_err().contains("source changed"));
        assert_eq!(fs::read_to_string(repo.source().join("tracked")).unwrap(), "later edit\n");
        fs::remove_dir_all(repo.source()).unwrap();
        assert!(snapshot.verify(&tree).is_err());
    }

    #[test]
    fn detects_mutation_and_drop_cleans_build_outputs() {
        let repo = Repo::new();
        let tree = repo.tree();
        {
            let snapshot = create(&repo.source(), &repo.destination(), &tree).unwrap();
            fs::write(snapshot.path().join("tracked"), "check mutated this\n").unwrap();
            assert!(snapshot.verify(&tree).unwrap_err().contains("snapshot changed"));
            fs::create_dir(snapshot.path().join("target")).unwrap();
            fs::write(snapshot.path().join("target/output"), "build").unwrap();
        }
        assert!(!repo.destination().exists());
        assert_eq!(repo.tree(), tree);
    }

    #[test]
    fn rejects_existing_destinations_and_unsafe_cleanup() {
        let repo = Repo::new();
        let tree = repo.tree();
        fs::create_dir(repo.destination()).unwrap();
        fs::write(repo.destination().join("keep"), "not ours").unwrap();
        assert!(create(&repo.source(), &repo.destination(), &tree).is_err());
        assert!(repo.destination().join("keep").exists());
        fs::remove_dir_all(repo.destination()).unwrap();
        let snapshot = create(&repo.source(), &repo.destination(), &tree).unwrap();
        let original = fs::read(snapshot.path().join(".git")).unwrap();
        fs::write(snapshot.path().join(".git"), "gitdir: /not/ours\n").unwrap();
        assert!(snapshot.remove().is_err());
        assert!(snapshot.verify(&tree).is_err());
        fs::write(snapshot.path().join(".git"), original).unwrap();
        snapshot.remove().unwrap();
    }

    #[test]
    fn rejects_submodules_lfs_and_filters_without_leaking_worktrees() {
        let repo = Repo::new();
        let head = repo.git(&["rev-parse", "HEAD"]);
        repo.git(&["update-index", "--add", "--cacheinfo", &format!("160000,{head},sub")]);
        // write-tree keeps the gitlink; tree_hash would inspect the absent submodule.
        let tree = repo.git(&["write-tree"]);
        assert!(create(&repo.source(), &repo.destination(), &tree).err().unwrap().contains("submodules"));
        repo.git(&["reset", "-q", "HEAD"]);
        repo.write("pointer", "version https://git-lfs.github.com/spec/v1\noid sha256:abc\nsize 1\n");
        assert!(create(&repo.source(), &repo.destination(), &repo.tree()).err().unwrap().contains("LFS"));
        fs::remove_file(repo.source().join("pointer")).unwrap();
        repo.write(".gitattributes", "tracked filter=lfs\n");
        // Construct the captured tree without invoking an installed source clean filter.
        repo.git(&["add", ".gitattributes"]);
        let tree = repo.git(&["write-tree"]);
        assert!(create(&repo.source(), &repo.destination(), &tree).err().unwrap().contains("filters"));
        assert!(!repo.destination().exists());
        assert_eq!(repo.git(&["worktree", "list", "--porcelain"]).matches("worktree ").count(), 1);
    }

    #[test]
    fn checkout_hooks_are_not_run() {
        let repo = Repo::new();
        repo.git(&["config", "user.name", ""]);
        repo.git(&["config", "user.email", ""]);
        repo.git(&["config", "user.useConfigOnly", "true"]);
        let marker = repo.0.join("hook-ran");
        let hook = repo.source().join(".git/hooks/post-checkout");
        fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let snapshot = create(&repo.source(), &repo.destination(), &repo.tree()).unwrap();
        assert!(!marker.exists());
        snapshot.remove().unwrap();
    }

    #[test]
    fn linked_source_can_disappear_without_preventing_owned_cleanup() {
        let repo = Repo::new();
        let linked = repo.0.join("linked-source");
        output(git(&repo.source()).args(["worktree", "add", "--detach"]).arg(&linked), None).unwrap();
        fs::write(linked.join("untracked"), "linked tree\n").unwrap();
        let tree = super::super::tree_hash(&linked).unwrap();
        let snapshot = create(&linked, &repo.destination(), &tree).unwrap();
        snapshot.verify(&tree).unwrap();
        fs::remove_dir_all(&linked).unwrap();
        assert!(snapshot.verify(&tree).is_err());
        snapshot.remove().unwrap();
        assert!(!repo.destination().exists());
        assert_eq!(fs::read_to_string(repo.source().join("tracked")).unwrap(), "original\n");
    }

    #[test]
    fn abandoned_snapshot_is_recovered_after_restart_without_touching_source() {
        let repo = Repo::new();
        let snapshot = create(&repo.source(), &repo.destination(), &repo.tree()).unwrap();
        let admin = snapshot.admin.clone();
        fs::create_dir(snapshot.path().join("target")).unwrap();
        fs::write(snapshot.path().join("target/output"), "build output").unwrap();
        std::mem::forget(snapshot); // A daemon restart loses the live cleanup guard.
        repo.write("tracked", "user's later edits\n");
        let index = fs::read(repo.source().join(".git/index")).unwrap();
        remove_abandoned(&repo.source(), &repo.destination()).unwrap();
        assert!(!repo.destination().exists());
        assert!(!admin.exists());
        assert_eq!(repo.git(&["worktree", "list", "--porcelain"]).matches("worktree ").count(), 1);
        assert_eq!(fs::read_to_string(repo.source().join("tracked")).unwrap(), "user's later edits\n");
        assert_eq!(fs::read(repo.source().join(".git/index")).unwrap(), index);
        assert!(remove_abandoned(&repo.source(), &repo.destination()).is_err(), "missing linkage is not adopted");
    }

    #[test]
    fn abandoned_cleanup_rejects_source_arbitrary_directories_and_normal_worktrees() {
        let repo = Repo::new();
        assert!(remove_abandoned(&repo.source(), &repo.source()).is_err());
        assert!(repo.source().join("tracked").exists());
        fs::create_dir(repo.destination()).unwrap();
        fs::write(repo.destination().join("keep"), "not arranger's").unwrap();
        assert!(remove_abandoned(&repo.source(), &repo.destination()).is_err());
        assert!(repo.destination().join("keep").exists());
        let normal = repo.0.join("data/verification/normal");
        output(git(&repo.source()).args(["worktree", "add", "--detach"]).arg(&normal), None).unwrap();
        assert!(remove_abandoned(&repo.source(), &normal).is_err(), "a Git worktree alone is not ownership");
        assert!(normal.join("tracked").exists());
        assert!(remove_abandoned(&normal, &normal).is_err(), "linked task source must not be deleted either");
    }

    #[test]
    fn abandoned_cleanup_refuses_missing_or_replaced_linkage_and_ownership_record() {
        let repo = Repo::new();
        let snapshot = create(&repo.source(), &repo.destination(), &repo.tree()).unwrap();
        let admin = snapshot.admin.clone();
        let gitfile = snapshot.gitfile.clone();
        let marker_path = admin.join("arranger-snapshot-owner");
        let marker = fs::read(&marker_path).unwrap();
        std::mem::forget(snapshot);
        fs::remove_file(repo.destination().join(".git")).unwrap();
        assert!(remove_abandoned(&repo.source(), &repo.destination()).is_err());
        fs::write(repo.destination().join(".git"), "gitdir: /not/ours\n").unwrap();
        assert!(remove_abandoned(&repo.source(), &repo.destination()).is_err());
        fs::write(repo.destination().join(".git"), gitfile).unwrap();
        fs::write(&marker_path, "replaced ownership record").unwrap();
        assert!(remove_abandoned(&repo.source(), &repo.destination()).is_err());
        fs::remove_file(&marker_path).unwrap();
        assert!(remove_abandoned(&repo.source(), &repo.destination()).is_err());
        fs::write(&marker_path, marker).unwrap();
        fs::write(admin.join("gitdir"), repo.source().join(".git").to_str().unwrap()).unwrap();
        assert!(remove_abandoned(&repo.source(), &repo.destination()).is_err());
        assert!(repo.destination().join("tracked").exists());
        fs::write(admin.join("gitdir"), format!("{}\n", repo.destination().join(".git").display())).unwrap();
        remove_abandoned(&repo.source(), &repo.destination()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn abandoned_cleanup_rejects_symlinked_paths_and_linkage() {
        use std::os::unix::fs::symlink;
        let repo = Repo::new();
        let snapshot = create(&repo.source(), &repo.destination(), &repo.tree()).unwrap();
        let gitfile = snapshot.gitfile.clone();
        std::mem::forget(snapshot);
        let alias = repo.0.join("alias");
        symlink(repo.destination(), &alias).unwrap();
        assert!(remove_abandoned(&repo.source(), &alias).is_err());
        fs::remove_file(&alias).unwrap();
        symlink(repo.0.join("data"), &alias).unwrap();
        assert!(remove_abandoned(&repo.source(), &alias.join("verification/run")).is_err());
        let link_target = repo.0.join("gitfile-copy");
        fs::write(&link_target, &gitfile).unwrap();
        fs::remove_file(repo.destination().join(".git")).unwrap();
        symlink(&link_target, repo.destination().join(".git")).unwrap();
        assert!(remove_abandoned(&repo.source(), &repo.destination()).is_err());
        assert!(repo.destination().join("tracked").exists());
        fs::remove_file(repo.destination().join(".git")).unwrap();
        fs::write(repo.destination().join(".git"), gitfile).unwrap();
        remove_abandoned(&repo.source(), &repo.destination()).unwrap();
    }
}

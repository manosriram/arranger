//! Checks: commands arranger runs in a task's worktree to see whether its work holds up (tests,
//! builds, linters), one after another, each with its own output. A run is keyed on the
//! worktree's tree hash, so it's known exactly which state of the work it judged.

use crate::proto::CheckSpec;
use std::path::Path;
use std::process::Command;

/// Most checks a run takes.
pub const MAX_CHECKS: usize = 20;

/// The worktree's contents as a git tree, uncommitted and untracked (not ignored) files
/// included, without touching its index: a copy of the index takes the changes.
pub fn tree_hash(worktree: &Path) -> Result<String, String> {
    let git = |index: Option<&Path>, args: &[&str]| {
        let mut c = Command::new("git");
        c.arg("-C").arg(worktree).args(args);
        if let Some(i) = index {
            c.env("GIT_INDEX_FILE", i);
        }
        let out = c.output().map_err(|e| format!("git: {e}"))?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let real = git(None, &["rev-parse", "--path-format=absolute", "--git-path", "index"])?;
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let copy = std::env::temp_dir().join(format!("arranger-index-{}-{nanos}", std::process::id()));
    // Its stat cache spares hashing every file again; a fresh one starts from HEAD.
    if std::fs::copy(&real, &copy).is_err() {
        git(Some(&copy), &["read-tree", "HEAD"])?;
    }
    let tree = git(Some(&copy), &["add", "-A"]).and_then(|_| git(Some(&copy), &["write-tree"]));
    let _ = std::fs::remove_file(&copy);
    tree
}

/// Trims and checks the specs: one-line commands of sane length, a name for each.
pub fn validate(specs: Vec<CheckSpec>) -> Result<Vec<CheckSpec>, String> {
    if specs.is_empty() {
        return Err("no checks to run".into());
    }
    if specs.len() > MAX_CHECKS {
        return Err(format!("more than {MAX_CHECKS} checks"));
    }
    specs
        .into_iter()
        .map(|mut s| {
            s.command = s.command.trim().to_string();
            if s.command.is_empty() {
                return Err("a check has no command".to_string());
            }
            if s.command.contains('\n') || s.command.len() > 500 {
                return Err(format!("a check's command is too long or spans lines: {}", s.command.chars().take(80).collect::<String>()));
            }
            s.name = s.name.lines().next().unwrap_or("").trim().chars().take(80).collect();
            if s.name.is_empty() {
                s.name = s.command.chars().take(80).collect();
            }
            s.why = s.why.lines().next().unwrap_or("").trim().chars().take(200).collect();
            Ok(s)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git").arg("-C").arg(dir).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn tree_hash_sees_edits_and_new_files_and_leaves_the_index_alone() {
        let dir = std::env::temp_dir().join(format!("arranger-tree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("a"), "1").unwrap();
        std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", "one"]);
        let clean = tree_hash(&dir).unwrap();
        assert_eq!(clean, git(&dir, &["rev-parse", "HEAD^{tree}"]));
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join("target/x"), "build output").unwrap();
        assert_eq!(tree_hash(&dir).unwrap(), clean, "ignored files don't count");
        std::fs::write(dir.join("a"), "2").unwrap();
        let edited = tree_hash(&dir).unwrap();
        assert_ne!(edited, clean);
        std::fs::write(dir.join("b"), "new").unwrap();
        assert_ne!(tree_hash(&dir).unwrap(), edited);
        // " M a" unstaged, trimmed by `git` above; b still untracked.
        assert_eq!(git(&dir, &["status", "--porcelain"]), "M a\n?? b", "the index is untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn specs_get_names_and_bad_ones_are_refused() {
        let spec = |name: &str, command: &str| CheckSpec { name: name.into(), command: command.into(), why: String::new() };
        let v = validate(vec![spec("", " cargo test "), spec("lint\nmore", "cargo clippy")]).unwrap();
        assert_eq!((v[0].name.as_str(), v[0].command.as_str()), ("cargo test", "cargo test"));
        assert_eq!(v[1].name, "lint");
        assert!(validate(vec![]).is_err());
        assert!(validate(vec![spec("", "  ")]).is_err());
        assert!(validate(vec![spec("", "a\nb")]).is_err());
        assert!(validate(vec![spec("", "true"); MAX_CHECKS + 1]).is_err());
    }
}

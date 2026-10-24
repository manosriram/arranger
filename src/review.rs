//! Review: a task's worktree diffed against the commit it branched from.
//! Reads go through gix, line diffs through imara-diff; writes go through the git CLI.

use crate::proto::{DiffLine, FileDiff, Hunk};
use imara_diff::{Algorithm, Diff, InternedInput};
use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

const CONTEXT: u32 = 3;

fn e2s(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// The commit `rev` (a ref or sha) points at, read in `repo_dir`.
pub fn resolve(repo_dir: &Path, rev: &str) -> Result<String, String> {
    let repo = gix::open(repo_dir).map_err(e2s)?;
    Ok(repo.rev_parse_single(rev).map_err(e2s)?.detach().to_string())
}

/// Short name of the branch checked out in `repo_dir`, or "HEAD" when detached.
pub fn current_branch(repo_dir: &Path) -> String {
    gix::open(repo_dir)
        .ok()
        .and_then(|r| r.head_name().ok().flatten())
        .map(|n| n.shorten().to_string())
        .unwrap_or_else(|| "HEAD".into())
}

/// Where `worktree`'s HEAD forked from `repo`'s HEAD (for tasks created before the base was stored).
pub fn fork_point(worktree: &Path, repo: &Path) -> Result<String, String> {
    let wt = gix::open(worktree).map_err(e2s)?;
    let main = gix::open(repo).map_err(e2s)?.head_id().map_err(e2s)?.detach();
    let head = wt.head_id().map_err(e2s)?.detach();
    let base = wt.merge_base(head, main).map_err(e2s)?.ok_or("no common history with the repo")?;
    Ok(base.detach().to_string())
}

/// Every file whose worktree content differs from `base` (a commit sha).
pub fn diff(worktree: &Path, base: &str, only: Option<&str>) -> Result<Vec<FileDiff>, String> {
    let repo = gix::open(worktree).map_err(e2s)?;
    let base_tree = repo.rev_parse_single(base).map_err(e2s)?.object().map_err(e2s)?.peel_to_tree().map_err(e2s)?;

    // Base tree vs index catches what the agent committed; index vs worktree what it hasn't.
    let status = repo
        .status(gix::progress::Discard)
        .map_err(e2s)?
        .head_tree(base_tree.id)
        .untracked_files(gix::status::UntrackedFiles::Files)
        .tree_index_track_renames(gix::status::tree_index::TrackRenames::Disabled)
        .index_worktree_rewrites(None);
    let mut paths = BTreeSet::new();
    for item in status.into_iter(None).map_err(e2s)? {
        paths.insert(item.map_err(e2s)?.location().to_string());
    }

    let mut files = Vec::new();
    for path in paths.into_iter().filter(|p| only.is_none_or(|o| o == p)) {
        let old = match base_tree.lookup_entry_by_path(&path).map_err(e2s)? {
            Some(entry) if entry.mode().is_blob() => Some(entry.object().map_err(e2s)?.data.clone()),
            _ => None,
        };
        let new = std::fs::read(worktree.join(&path)).ok();
        if old == new {
            continue;
        }
        let status = match (&old, &new) {
            (None, _) => 'A',
            (_, None) => 'D',
            _ => 'M',
        };
        let (old, new) = (old.unwrap_or_default(), new.unwrap_or_default());
        let binary = [&old, &new].iter().any(|d| d[..d.len().min(8000)].contains(&0));
        let (old, new) = (String::from_utf8_lossy(&old), String::from_utf8_lossy(&new));
        let hunks = if binary { Vec::new() } else { hunks(&old, &new) };
        let count = |k: char| hunks.iter().flat_map(|h: &Hunk| &h.lines).filter(|l| l.kind == k).count() as u32;
        files.push(FileDiff { added: count('+'), removed: count('-'), path, status, binary, hunks });
    }
    Ok(files)
}

/// Line hunks with `CONTEXT` lines around each change; nearby changes share a hunk.
pub fn hunks(old: &str, new: &str) -> Vec<Hunk> {
    let input = InternedInput::new(old, new);
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);
    let before: Vec<&str> = old.split_inclusive('\n').collect();
    let after: Vec<&str> = new.split_inclusive('\n').collect();
    let changes: Vec<imara_diff::Hunk> = diff.hunks().collect();

    let mut out = Vec::new();
    let mut i = 0;
    while i < changes.len() {
        // Merge changes whose context would touch.
        let mut j = i;
        while j + 1 < changes.len() && changes[j + 1].before.start - changes[j].before.end <= 2 * CONTEXT {
            j += 1;
        }
        let (first, last) = (&changes[i], &changes[j]);
        let start = first.before.start.saturating_sub(CONTEXT);
        let end = (last.before.end + CONTEXT).min(before.len() as u32);
        let shift = |c: &imara_diff::Hunk| c.after.start as i64 - c.before.start as i64;

        let mut lines = Vec::new();
        let context = |lines: &mut Vec<DiffLine>, range: std::ops::Range<u32>, shift: i64| {
            for o in range {
                let n = (o as i64 + shift) as u32;
                lines.push(DiffLine { kind: ' ', old: o + 1, new: n + 1, text: before[o as usize].to_string() });
            }
        };
        context(&mut lines, start..first.before.start, shift(first));
        for (k, c) in changes[i..=j].iter().enumerate() {
            for o in c.before.clone() {
                lines.push(DiffLine { kind: '-', old: o + 1, new: 0, text: before[o as usize].to_string() });
            }
            for n in c.after.clone() {
                lines.push(DiffLine { kind: '+', old: 0, new: n + 1, text: after[n as usize].to_string() });
            }
            let next = changes.get(i + k + 1).filter(|_| i + k < j).map_or(end, |n| n.before.start);
            context(&mut lines, c.before.end..next, shift(c) + c.after.len() as i64 - c.before.len() as i64);
        }

        let old_len = lines.iter().filter(|l| l.kind != '+').count() as u32;
        let new_len = lines.iter().filter(|l| l.kind != '-').count() as u32;
        let new_start = (start as i64 + shift(first)) as u32;
        let at = |s: u32, len: u32| if len == 0 { s } else { s + 1 };
        let header = format!(
            "@@ -{},{old_len} +{},{new_len} @@{}",
            at(start, old_len),
            at(new_start, new_len),
            scope(&before, start).map(|s| format!("  {s}")).unwrap_or_default()
        );
        out.push(Hunk { header, lines });
        i = j + 1;
    }
    out
}

/// The nearest line above `start` that opens a scope (starts with a letter, `_` or `$`), like git's funcname.
fn scope(lines: &[&str], start: u32) -> Option<String> {
    lines[..start as usize]
        .iter()
        .rev()
        .find(|l| l.starts_with(|c: char| c.is_alphabetic() || c == '_' || c == '$'))
        .map(|l| l.trim_end().chars().take(60).collect())
}

/// A one-hunk unified patch, for `git apply`.
fn patch(file: &FileDiff, hunk: &Hunk) -> String {
    let (a, b) = match file.status {
        'A' => ("/dev/null".to_string(), format!("b/{}", file.path)),
        'D' => (format!("a/{}", file.path), "/dev/null".to_string()),
        _ => (format!("a/{}", file.path), format!("b/{}", file.path)),
    };
    let mut p = format!("--- {a}\n+++ {b}\n{}\n", hunk.header.split("  ").next().unwrap_or(&hunk.header));
    for l in &hunk.lines {
        p.push(l.kind);
        p.push_str(&l.text);
        if !l.text.ends_with('\n') {
            p.push_str("\n\\ No newline at end of file\n");
        }
    }
    p
}

/// Undoes one hunk in the worktree. `header` must still match: the agent may have moved on.
pub fn revert(worktree: &Path, base: &str, path: &str, header: &str) -> Result<(), String> {
    let files = diff(worktree, base, Some(path))?;
    let file = files.first().ok_or("that file no longer differs from the base")?;
    let hunk = file.hunks.iter().find(|h| h.header == header).ok_or("the diff changed; refresh the review")?;
    let mut git = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["apply", "-R", "--whitespace=nowarn", "-"])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(e2s)?;
    git.stdin.take().unwrap().write_all(patch(file, hunk).as_bytes()).map_err(e2s)?;
    let out = git.wait_with_output().map_err(e2s)?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(())
}

/// The commit a review diffs against: `stored` (where the task started, or your last commit), until
/// the base branch is merged into this one (under way, or done). Then its tip, so only this branch's
/// work and any conflict resolution show, not everything the base branch gained meanwhile.
pub fn diff_base(worktree: &Path, stored: &str, base_branch: &str) -> String {
    let Ok(repo) = gix::open(worktree) else { return stored.into() };
    if let Ok(tip) = fs::read_to_string(repo.git_dir().join("MERGE_HEAD")) {
        return tip.lines().next().unwrap_or(stored).to_string();
    }
    let id = |rev: &str| repo.rev_parse_single(rev).ok().map(|i| i.detach());
    let (Some(tip), Some(head), Some(old)) = (id(base_branch).filter(|_| !base_branch.is_empty()), id("HEAD"), id(stored)) else {
        return stored.into();
    };
    let below = |a, b| repo.merge_base(a, b).ok().flatten().is_some_and(|m| m.detach() == a); // a is an ancestor of b
    if tip != old && below(tip, head) && below(old, tip) { tip.to_string() } else { stored.into() }
}

/// Files the merge under way left conflicted, and whether one is under way at all.
pub fn conflicts(worktree: &Path) -> Result<(Vec<String>, bool), String> {
    let repo = gix::open(worktree).map_err(e2s)?;
    let index = repo.index_or_empty().map_err(e2s)?;
    let mut files: Vec<String> = index
        .entries()
        .iter()
        .filter(|e| e.stage() != gix::index::entry::Stage::Unconflicted)
        .map(|e| e.path(&index).to_string())
        .collect();
    files.dedup(); // one entry per side, adjacent
    Ok((files, repo.git_dir().join("MERGE_HEAD").exists()))
}

/// Shell script for Commit: `$1` message, `$2` "merge", "pr" or "", `$3` PR body, `$4` base branch.
/// A merge goes into wherever the base is checked out; a conflict is backed out, never left half done.
pub const COMMIT_SCRIPT: &str = r#"set -e
if [ -n "$(git diff --name-only --diff-filter=U)" ]; then echo "resolve the conflicts first"; exit 1; fi
git add -A
if ! git diff --cached --quiet; then git commit -q -m "$1"; fi
if [ "$2" = pr ]; then
  git push -q -u origin HEAD
  gh pr create --title "$1" --body "$3" --base "$4"
elif [ "$2" = merge ]; then
  [ -n "$4" ] || { echo "no base branch to merge into"; exit 1; }
  branch=$(git rev-parse --abbrev-ref HEAD)
  where=$(git worktree list --porcelain | awk -v b="branch refs/heads/$4" '/^worktree /{w=substr($0,10)} $0==b{print w}')
  into_base() {
    if [ -z "$where" ]; then git fetch -q . "$branch:$4" 2>/dev/null
    else git -C "$where" merge -q --ff-only "$branch" 2>/dev/null; fi
  }
  if ! into_base; then
    # $4 moved on: bring it into this branch here, so a conflict lands in this worktree, never in $4.
    if ! git merge -q --no-edit "$4" >/dev/null 2>&1; then
      echo "conflicts with $4 in $(git diff --name-only --diff-filter=U | tr '\n' ' ')"
      exit 1
    fi
    into_base || { echo "couldn't update $4 (local changes there?); nothing was changed"; exit 1; }
  fi
  echo "merged $branch into $4"
else
  git log -1 --format='committed %h %s'
fi"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn render(hunks: &[Hunk]) -> String {
        hunks.iter().map(|h| format!("{}\n{}", h.header, h.lines.iter().map(|l| format!("{}{}", l.kind, l.text)).collect::<String>())).collect()
    }

    #[test]
    fn hunks_match_git_shape() {
        let old: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        let new = old.replace("line 2\n", "line 2\nadded\n").replace("line 15\n", "");
        let h = hunks(&old, &new);
        assert_eq!(h.len(), 2);
        assert!(render(&h).starts_with("@@ -1,5 +1,6 @@\n line 1\n line 2\n+added\n line 3\n"), "{}", render(&h));
        assert!(h[1].header.starts_with("@@ -12,7 +13,6 @@"));
        assert_eq!(h[1].lines.iter().find(|l| l.kind == '-').map(|l| l.old), Some(15));
        let new_file = hunks("", "a\nb");
        assert_eq!(new_file[0].header, "@@ -0,0 +1,2 @@");
    }

    #[test]
    fn revert_one_hunk_in_a_repo() {
        let dir = std::env::temp_dir().join(format!("arranger-review-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| assert!(Command::new("git").arg("-C").arg(&dir).args(args).output().unwrap().status.success());
        git(&["init", "-q"]);
        let old: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        std::fs::write(dir.join("f.txt"), &old).unwrap();
        git(&["add", "."]);
        git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "i"]);
        let base = resolve(&dir, "HEAD").unwrap();
        std::fs::write(dir.join("f.txt"), old.replace("line 2\n", "two\n").replace("line 18\n", "eighteen\n")).unwrap();
        std::fs::write(dir.join("new.txt"), "hello\n").unwrap();

        let files = diff(&dir, &base, None).unwrap();
        assert_eq!(files.iter().map(|f| (f.path.as_str(), f.status)).collect::<Vec<_>>(), [("f.txt", 'M'), ("new.txt", 'A')]);
        assert_eq!(files[0].hunks.len(), 2);
        revert(&dir, &base, "f.txt", &files[0].hunks[0].header).unwrap();
        revert(&dir, &base, "new.txt", &files[1].hunks[0].header).unwrap();
        let after = std::fs::read_to_string(dir.join("f.txt")).unwrap();
        assert!(after.contains("line 2\n") && after.contains("eighteen\n"));
        assert!(!dir.join("new.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A fresh repo under the temp dir with `f.txt` committed on `main`; returns (dir, git).
    fn repo(name: &str) -> (std::path::PathBuf, impl Fn(&[&str]) -> String) {
        let dir = std::env::temp_dir().join(format!("arranger-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.clone();
        let git = move |args: &[&str]| {
            let out = Command::new("git").arg("-C").arg(&d).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("f.txt"), "one\ntwo\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "i"]);
        (dir, git)
    }

    #[test]
    fn diff_sees_commits_edits_and_deletes() {
        let (dir, git) = repo("diff");
        let base = resolve(&dir, "HEAD").unwrap();
        std::fs::write(dir.join("c.txt"), "committed\n").unwrap();
        git(&["add", "c.txt"]);
        git(&["commit", "-q", "-m", "agent"]);
        std::fs::remove_file(dir.join("f.txt")).unwrap();
        std::fs::write(dir.join("bin.dat"), b"\0\x01").unwrap();
        let files = diff(&dir, &base, None).unwrap();
        let got: Vec<_> = files.iter().map(|f| (f.path.as_str(), f.status, f.binary, f.added, f.removed)).collect();
        assert_eq!(got, [("bin.dat", 'A', true, 0, 0), ("c.txt", 'A', false, 1, 0), ("f.txt", 'D', false, 0, 2)]);
        assert_eq!(diff(&dir, &base, Some("c.txt")).unwrap().len(), 1);
        // Nothing to review against the tip once it's all committed and the worktree matches.
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "rest"]);
        assert!(diff(&dir, "HEAD", None).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_conflicts_and_base() {
        let (dir, git) = repo("conflict");
        let fork = resolve(&dir, "HEAD").unwrap();
        git(&["checkout", "-q", "-b", "agent"]);
        std::fs::write(dir.join("f.txt"), "one\nagent\n").unwrap();
        git(&["commit", "-q", "-am", "agent"]);
        git(&["checkout", "-q", "main"]);
        std::fs::write(dir.join("f.txt"), "one\nmain\n").unwrap();
        git(&["commit", "-q", "-am", "main"]);
        let main = resolve(&dir, "main").unwrap();
        git(&["checkout", "-q", "agent"]);
        assert_eq!(conflicts(&dir).unwrap(), (vec![], false));
        assert_eq!(diff_base(&dir, &fork, "main"), fork); // main moved, but isn't merged in yet

        assert!(!Command::new("git").arg("-C").arg(&dir).args(["-c", "user.name=t", "-c", "user.email=t@t", "merge", "-q", "main"]).output().unwrap().status.success());
        assert_eq!(conflicts(&dir).unwrap(), (vec!["f.txt".to_string()], true));
        assert_eq!(diff_base(&dir, &fork, "main"), main); // mid-merge, review against what's coming in
        std::fs::write(dir.join("f.txt"), "one\nboth\n").unwrap();
        git(&["add", "f.txt"]);
        assert_eq!(conflicts(&dir).unwrap(), (vec![], true)); // resolved, not yet committed
        git(&["commit", "-q", "--no-edit"]);
        assert_eq!(diff_base(&dir, &fork, "main"), main); // merged: main's own changes drop out of the diff
        assert_eq!(diff(&dir, &main, None).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

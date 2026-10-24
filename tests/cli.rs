//! End to end through a real daemon: start a fake agent on its own worktree, answer its
//! permission ask, review and merge its work, then revive it.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// A throwaway `$ARRANGER_HOME`; its daemon is stopped on drop.
struct Home(PathBuf);

impl Home {
    fn arranger(&self, dir: &Path, args: &[&str]) -> (bool, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_arranger")).current_dir(dir).env("ARRANGER_HOME", &self.0).args(args).output().unwrap();
        (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr))
    }

    /// `arranger ls`'s state column for task 1, once it is one of `want`.
    fn wait_state(&self, dir: &Path, want: &[&str]) -> String {
        let start = Instant::now();
        loop {
            let (_, ls) = self.arranger(dir, &["ls"]);
            let state = ls.lines().find(|l| l.starts_with("1 ")).and_then(|l| l.split_whitespace().nth(1)).unwrap_or("").to_string();
            if want.contains(&state.as_str()) {
                return state;
            }
            assert!(start.elapsed() < Duration::from_secs(15), "task 1 never got to {want:?}:\n{ls}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = self.arranger(&self.0, &["stop"]);
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(dir).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

// The agent edits a file, asks to run a command and records the answer, then says it's done.
const AGENT: &str = r#"#!/bin/sh
echo "run $#" >> "$ARRANGER_HOME/runs"
echo hello > hello.txt
echo '{"hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"make test"}}' | "$ARRANGER_BIN" hook claude > "$ARRANGER_HOME/decision"
echo '{"hook_event_name":"Stop","last_assistant_message":"Added hello.txt"}' | "$ARRANGER_BIN" hook claude
"#;

#[test]
fn task_lifecycle() {
    let home = Home(std::env::temp_dir().join(format!("arranger-cli-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&home.0);
    let repo = home.0.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    // The daemon commits in a worktree without the helper's `git -c` flags.
    // Keep its identity in the repo so this also works on clean CI runners.
    git(&repo, &["config", "user.name", "t"]);
    git(&repo, &["config", "user.email", "t@t"]);
    std::fs::write(repo.join("README"), "hi\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let agent = home.0.join("agent.sh");
    std::fs::write(&agent, AGENT).unwrap();
    std::fs::set_permissions(&agent, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

    let (ok, out) = home.arranger(&repo, &["new", agent.to_str().unwrap(), "say", "hello"]);
    assert!(ok, "{out}");
    assert_eq!(out.split_whitespace().next(), Some("1"));
    home.wait_state(&repo, &["needs-you"]);
    let (_, ls) = home.arranger(&repo, &["ls"]);
    assert!(ls.contains("arranger/1"), "{ls}");

    // Approving answers the hook, which prints claude's allow decision.
    assert!(home.arranger(&repo, &["approve", "1"]).0);
    home.wait_state(&repo, &["done", "exited"]);
    let decision = std::fs::read_to_string(home.0.join("decision")).unwrap();
    assert!(decision.contains(r#""behavior":"allow""#), "{decision}");
    let (_, events) = home.arranger(&repo, &["events", "1"]);
    assert!(events.contains("make test") && events.contains("Added hello.txt"), "{events}");

    let (ok, diff) = home.arranger(&repo, &["diff", "1"]);
    assert!(ok && diff.contains("arranger/1 -> main") && diff.contains("A hello.txt  +1 -0"), "{diff}");

    // Merge lands the work on main, which is checked out in the repo itself.
    let (ok, out) = home.arranger(&repo, &["commit", "1", "--merge", "add hello"]);
    assert!(ok, "{out}");
    assert_eq!(git(&repo, &["show", "main:hello.txt"]), "hello");
    assert_eq!(std::fs::read_to_string(repo.join("hello.txt")).unwrap(), "hello\n");
    let (_, diff) = home.arranger(&repo, &["diff", "1"]);
    assert!(!diff.contains("hello.txt"), "merged work is still up for review:\n{diff}");

    // An ended agent can be revived on its worktree, with a new prompt.
    home.wait_state(&repo, &["exited"]);
    assert!(home.arranger(&repo, &["revive", "1", "once", "more"]).0);
    home.wait_state(&repo, &["needs-you"]);
    let (ok, out) = home.arranger(&repo, &["revive", "1"]);
    assert!(!ok && out.contains("still running"), "{out}");
    assert_eq!(std::fs::read_to_string(home.0.join("runs")).unwrap().lines().count(), 2);
    assert!(home.arranger(&repo, &["deny", "1", "not now"]).0);
    home.wait_state(&repo, &["done", "exited"]);
    assert!(std::fs::read_to_string(home.0.join("decision")).unwrap().contains("not now"));
}

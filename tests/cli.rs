//! End to end through a real daemon: start a fake agent on its own worktree, answer its
//! permission ask, review and merge its work, then revive it; clone another onto a new worktree.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// A throwaway `$ARRANGER_HOME`; its daemon is stopped on drop.
struct Home(PathBuf);

impl Home {
    fn arranger(&self, dir: &Path, args: &[&str]) -> (bool, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_arranger")).current_dir(dir).env("ARRANGER_HOME", &self.0)
            .env("CLAUDE_CONFIG_DIR", self.0.join("claude")).args(args).output().unwrap();
        (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr))
    }

    /// `arranger ls`'s state column for task 1, once it is one of `want`.
    fn wait_state(&self, dir: &Path, want: &[&str]) -> String {
        self.wait_task(dir, 1, want)
    }

    fn wait_task(&self, dir: &Path, id: u64, want: &[&str]) -> String {
        let start = Instant::now();
        loop {
            let (_, ls) = self.arranger(dir, &["ls"]);
            let state = ls.lines().find(|l| l.starts_with(&format!("{id} "))).and_then(|l| l.split_whitespace().nth(1)).unwrap_or("").to_string();
            if want.contains(&state.as_str()) {
                return state;
            }
            assert!(start.elapsed() < Duration::from_secs(15), "task {id} never got to {want:?}:\n{ls}");
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

/// The Terminal tab's shell on task 1: a `cd` out of the worktree steps back in, and on macOS
/// the main checkout can't be read nor anything written beside the worktree. Exiting ends it.
fn shell_stays_in_worktree(home: &Home, repo: &Path) {
    if cfg!(target_os = "macos") {
        // An inherited sandbox can forbid sandbox_apply entirely. Probe separately so
        // that restriction doesn't hide regressions in our actual sandbox profile.
        let probe = Command::new("/usr/bin/sandbox-exec")
            .args(["-p", "(version 1)(allow default)", "/usr/bin/true"])
            .output().unwrap();
        let stderr = String::from_utf8_lossy(&probe.stderr);
        if !probe.status.success() && stderr.contains("sandbox_apply: Operation not permitted") {
            eprintln!("skipping shell confinement check: this environment forbids applying a macOS sandbox: {stderr}");
            return;
        }
        assert!(probe.status.success(), "sandbox availability probe failed: {stderr}");
    }
    use arranger::proto::{Reply, Request, frame, unframe};
    use std::io::{Read, Write};
    let mut sock = std::os::unix::net::UnixStream::connect(home.0.join("arranger.sock")).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let typed = format!("cd ..; cat {}; echo x > ../x; echo DO''NE\r", repo.join("README").display());
    for req in [Request::Version, Request::Shell { id: 1, cols: 200, rows: 50 }, Request::Input(typed.into_bytes())] {
        sock.write_all(&frame(&req)).unwrap();
    }
    let (mut buf, mut out, mut exited) = (Vec::new(), String::new(), false);
    let mut chunk = [0u8; 8192];
    while !exited {
        let n = sock.read(&mut chunk).unwrap_or_else(|e| panic!("{e}; shell said:\n{out}"));
        assert!(n > 0, "daemon hung up; shell said:\n{out}");
        buf.extend_from_slice(&chunk[..n]);
        while let Some(reply) = unframe::<Reply>(&mut buf) {
            match reply.unwrap() {
                Reply::Output(bytes) => out.push_str(&String::from_utf8_lossy(&bytes)),
                Reply::Exited => exited = true,
                Reply::Error(e) => panic!("{e}"),
                _ => {}
            }
        }
        if out.contains("DONE\r\n") && !out.contains("exit\r") {
            sock.write_all(&frame(&Request::Input(b"exit\r".to_vec()))).unwrap();
            out.push_str("exit\r");
        }
    }
    assert!(out.contains("outside this worktree"), "{out}");
    if cfg!(target_os = "macos") {
        assert!(out.to_lowercase().matches("operation not permitted").count() >= 2, "{out}");
        assert!(!home.0.join("worktrees/repo/x").exists());
    }
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
    let base_head = git(&repo, &["rev-parse", "HEAD"]);
    let (ok, out) = home.arranger(&repo, &["run", "1", "sh", "-c", "cat hello.txt; git branch --show-current"]);
    assert!(ok, "{out}");
    assert_eq!(out, "hello\narranger/1\n");
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), base_head);
    assert!(!repo.join("hello.txt").exists());
    assert!(!home.arranger(&repo, &["run", "999999", "true"]).0);
    let failed = Command::new(env!("CARGO_BIN_EXE_arranger"))
        .env("ARRANGER_HOME", &home.0).args(["run", "1", "sh", "-c", "exit 42"]).status().unwrap();
    assert_eq!(failed.code(), Some(42));
    shell_stays_in_worktree(&home, &repo);
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

// A claude stand-in: the first run edits a tracked file, adds an untracked one and saves a
// session where claude would; every run records its args.
const CLAUDE: &str = r#"#!/bin/sh
echo "$*" >> "$ARRANGER_HOME/args-$ARRANGER_TASK"
[ -f hello.txt ] && exit 0
echo hello > hello.txt
echo changed >> README
d="$CLAUDE_CONFIG_DIR/projects/$(pwd -P | sed 's/[^a-zA-Z0-9]/-/g')"
mkdir -p "$d" && echo '{}' > "$d/s$ARRANGER_TASK.jsonl"
"#;

#[test]
fn clone_task() {
    let home = Home(std::env::temp_dir().join(format!("arranger-clone-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&home.0);
    let repo = home.0.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README"), "hi\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let bin = home.0.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let claude = bin.join("claude");
    std::fs::write(&claude, CLAUDE).unwrap();
    std::fs::set_permissions(&claude, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

    assert!(home.arranger(&repo, &["new", claude.to_str().unwrap(), "go"]).0);
    home.wait_state(&repo, &["exited"]);
    let (ok, out) = home.arranger(&repo, &["clone", "1"]);
    assert!(ok, "{out}");
    assert_eq!(out.split_whitespace().next(), Some("2"));
    home.wait_task(&repo, 2, &["exited"]);

    // Its own branch, with task 1's edits, committed or not, and a fork of its session.
    let (ok, out) = home.arranger(&repo, &["run", "2", "sh", "-c", "cat hello.txt README; git branch --show-current"]);
    assert!(ok, "{out}");
    assert_eq!(out, "hello\nhi\nchanged\narranger/2\n");
    let args = std::fs::read_to_string(home.0.join("args-2")).unwrap();
    assert!(args.contains("--continue --fork-session"), "{args}");
    let wt = std::fs::canonicalize(home.0.join("worktrees/repo/2")).unwrap();
    let name: String = wt.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    assert!(home.0.join("claude/projects").join(name).join("s1.jsonl").exists());
    assert_eq!(std::fs::read_to_string(repo.join("README")).unwrap(), "hi\n"); // task 1 is untouched
    assert!(!home.arranger(&repo, &["clone", "999999"]).0);
}

//! End to end through a real daemon: start a fake agent on its own worktree, answer its
//! permission ask, review and merge its work, then revive it; clone another onto a new worktree.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// A throwaway `$ARRANGER_HOME`; its daemon is stopped on drop.
struct Home(PathBuf);

impl Home {
    fn arranger(&self, dir: &Path, args: &[&str]) -> (bool, String) {
        // Inside an arranger task ARRANGER_CLI names that build; the daemon must be this one.
        let out = Command::new(env!("CARGO_BIN_EXE_arranger")).current_dir(dir).env("ARRANGER_HOME", &self.0).env_remove("ARRANGER_CLI")
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

#[test]
fn daemon_rejects_unsafe_inputs_and_repository_setup() {
    use arranger::proto::{self, Reply, Request};
    use std::io::{Read, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixStream;
    let home = Home(std::env::temp_dir().join(format!("arranger-sec-{}", std::process::id())));
    let repo = home.0.join("repo");
    std::fs::create_dir_all(repo.join(".arranger")).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join(".arranger/setup"), "#!/bin/sh\ntouch \"$ARRANGER_HOME/setup-ran\"\n").unwrap();
    std::fs::set_permissions(repo.join(".arranger/setup"), PermissionsExt::from_mode(0o755)).unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "fixture"]);
    let (ok, out) = home.arranger(&repo, &["new", "/usr/bin/true"]);
    assert!(ok, "{out}");
    home.wait_state(&repo, &["exited"]);
    assert!(!home.0.join("setup-ran").exists());
    for path in [&home.0, &home.0.join("logs"), &home.0.join("adapters")] {
        assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o077, 0);
    }
    let socket = home.0.join("arranger.sock");
    assert_eq!(std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777, 0o600);
    let mut bad = UnixStream::connect(&socket).unwrap();
    bad.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    bad.write_all(&(2u32 << 20).to_le_bytes()).unwrap();
    let mut byte = [0];
    assert!(matches!(bad.read(&mut byte), Ok(0)));
    assert!(home.arranger(&repo, &["ping"]).0);

    // Finished requests must still be processed if a hook closes immediately after writing.
    let mut sock = UnixStream::connect(&socket).unwrap();
    sock.write_all(&proto::frame(&Request::Ask { task: 999, event: proto::Event::PermissionRequest { tool: "Bash".into(), detail: "test".into() } })).unwrap();
    assert!(matches!(proto::read_msg::<Reply>(&mut sock).unwrap(), Reply::Decision { allow: Some(false), .. }));

    // A stale/tampered PID file must have no influence on shutdown.
    let mut unrelated = Command::new("sleep").arg("30").spawn().unwrap();
    std::fs::write(home.0.join("daemon.pid"), unrelated.id().to_string()).unwrap();
    assert!(home.arranger(&repo, &["stop"]).0);
    let survived = unrelated.try_wait().unwrap().is_none();
    let _ = unrelated.kill();
    let _ = unrelated.wait();
    assert!(survived, "shutdown signalled the PID file's unrelated process");
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

    assert!(home.arranger(&repo, &["new", "--terminal", claude.to_str().unwrap(), "go"]).0);
    home.wait_state(&repo, &["exited"]);
    let (ok, out) = home.arranger(&repo, &["clone", "1", "--model", "claude-haiku-5-5", "try", "it", "faster"]);
    assert!(ok, "{out}");
    assert_eq!(out.split_whitespace().next(), Some("2"));
    home.wait_task(&repo, 2, &["exited"]);

    // Its own branch, with task 1's edits, committed or not, and a fork of its session.
    let (ok, out) = home.arranger(&repo, &["run", "2", "sh", "-c", "cat hello.txt README; git branch --show-current"]);
    assert!(ok, "{out}");
    assert_eq!(out, "hello\nhi\nchanged\narranger/2\n");
    let args = std::fs::read_to_string(home.0.join("args-2")).unwrap();
    assert!(args.contains("--continue --fork-session") && args.contains("--model claude-haiku-5-5") && args.ends_with("try it faster\n"), "{args}");
    let wt = std::fs::canonicalize(home.0.join("worktrees/repo/2")).unwrap();
    let name: String = wt.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    assert!(home.0.join("claude/projects").join(name).join("s1.jsonl").exists());
    assert_eq!(std::fs::read_to_string(repo.join("README")).unwrap(), "hi\n"); // task 1 is untouched
    assert!(!home.arranger(&repo, &["clone", "999999"]).0);
}

// A headless claude stand-in speaking stream-json on stdio. Records its args; the prompt picks
// what it does: "ask" asks to run a command and saves the answer, "hang" works until interrupted,
// "bye" exits, "crash" fails on stderr; anything else is just answered.
const HEADLESS_CLAUDE: &str = r#"#!/usr/bin/env python3
import json, os, sys
home, task, argv = os.environ["ARRANGER_HOME"], os.environ["ARRANGER_TASK"], sys.argv[1:]
open(f"{home}/args-{task}", "a").write(" ".join(argv) + "\n")
def out(v):
    print(json.dumps(v), flush=True)
def read():
    line = sys.stdin.readline()
    return json.loads(line) if line else sys.exit(0)
resumed = argv[argv.index("--resume") + 1] if "--resume" in argv else None
session = resumed if resumed and "--fork-session" not in argv else f"s{task}"
d = os.environ["CLAUDE_CONFIG_DIR"] + "/projects/" + "".join(c if c.isalnum() else "-" for c in os.path.realpath("."))
os.makedirs(d, exist_ok=True)
open(f"{d}/{session}.jsonl", "a").close()
def result(subtype="success"):
    out({"type": "result", "subtype": subtype, "is_error": subtype != "success", "usage": {"input_tokens": 5}, "total_cost_usd": 0.001})
while True:
    m = read()
    if m["type"] == "control_request":
        out({"type": "control_response", "response": {"subtype": "success", "request_id": m["request_id"]}})
        if m["request"]["subtype"] == "initialize":
            out({"type": "system", "subtype": "init", "session_id": session, "slash_commands": ["compact"], "terminal_slash_commands": ["doctor"]})
        continue
    prompt = m["message"]["content"]
    if "--replay-user-messages" in argv:
        out(m)
    if prompt == "ask":
        out({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "make test"}}]}})
        out({"type": "control_request", "request_id": "r1", "request": {"subtype": "can_use_tool", "tool_name": "Bash", "input": {"command": "make test"}}})
        answer = read()
        open(f"{home}/decision", "w").write(json.dumps(answer))
        out({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]}})
        out({"type": "assistant", "message": {"content": [{"type": "text", "text": "Ran make test"}]}})
        result()
    elif prompt == "hang":
        while (m := read())["type"] != "control_request":
            pass
        out({"type": "control_response", "response": {"subtype": "success", "request_id": m["request_id"]}})
        result("error_during_execution")
    elif prompt == "bye":
        sys.exit(0)
    elif prompt == "crash":
        print("boom: no credentials", file=sys.stderr)
        sys.exit(3)
    else:
        out({"type": "assistant", "message": {"content": [{"type": "text", "text": f"Did: {prompt}"}]}})
        result()
"#;

#[test]
fn headless_task() {
    let home = Home(std::env::temp_dir().join(format!("arranger-headless-{}", std::process::id())));
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
    std::fs::write(&claude, HEADLESS_CLAUDE).unwrap();
    std::fs::set_permissions(&claude, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let claude = claude.to_str().unwrap();
    let events = || home.arranger(&repo, &["events", "1"]).1;
    let decision = || std::fs::read_to_string(home.0.join("decision")).unwrap_or_default();

    let (ok, out) = home.arranger(&repo, &["new", "--headless", "/bin/sh", "hi"]);
    assert!(!ok && out.contains("can't run headless"), "{out}");

    // Launched with the parity flags, and no system prompt of ours.
    let (ok, out) = home.arranger(&repo, &["new", "--headless", claude, "ask"]);
    assert!(ok, "{out}");
    home.wait_state(&repo, &["needs-you"]);
    let args = std::fs::read_to_string(home.0.join("args-1")).unwrap();
    for flag in ["--input-format stream-json", "--permission-prompt-tool stdio", "--setting-sources user", "--permission-mode default"] {
        assert!(args.contains(flag), "{flag} missing: {args}");
    }
    assert!(!args.contains("--system-prompt") && !args.contains("--settings"), "{args}");

    // Permission asks are answered by request id, from the CLI like any other task.
    assert!(home.arranger(&repo, &["approve", "1"]).0);
    home.wait_state(&repo, &["done"]);
    assert!(decision().contains(r#""request_id": "r1""#) && decision().contains(r#""behavior": "allow""#), "{}", decision());
    let e = events();
    assert!(e.contains("prompt: ask") && e.contains("running make test") && e.contains("summary: Ran make test"), "{e}");
    let (ok, chat) = home.arranger(&repo, &["chat", "1"]);
    assert!(ok && chat.contains("> ask") && chat.contains("✓ Bash make test") && chat.contains("Ran make test"), "{chat}");

    // Interrupting stops the turn; the next prompt starts a new one.
    assert!(home.arranger(&repo, &["send", "1", "hang"]).0);
    home.wait_state(&repo, &["running"]);
    assert!(home.arranger(&repo, &["interrupt", "1"]).0);
    home.wait_state(&repo, &["done"]);
    assert!(home.arranger(&repo, &["send", "1", "ask"]).0);
    home.wait_state(&repo, &["needs-you"]);
    assert!(home.arranger(&repo, &["deny", "1", "not now"]).0);
    home.wait_state(&repo, &["done"]);
    assert!(decision().contains(r#""behavior": "deny", "message": "not now""#), "{}", decision());
    // A command only claude's TUI has isn't sent; the CLI says how to run it.
    let (ok, out) = home.arranger(&repo, &["send", "1", "/permissions"]);
    assert!(!ok && out.contains("claude '/permissions'"), "{out}");

    // A chat view gets the conversation so far, then each change as it happens.
    {
        use arranger::proto::{ChatItem, Reply, Request, frame, unframe};
        use std::io::{Read, Write};
        let mut sock = std::os::unix::net::UnixStream::connect(home.0.join("arranger.sock")).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        for req in [Request::Version, Request::Chat { id: 1, limit: 2 }] {
            sock.write_all(&frame(&req)).unwrap();
        }
        let (mut buf, mut chunk, mut replies) = (Vec::new(), [0u8; 8192], Vec::new());
        let mut read_until = |done: &dyn Fn(&[Reply]) -> bool, replies: &mut Vec<Reply>| {
            while !done(replies) {
                let n = sock.read(&mut chunk).unwrap();
                assert!(n > 0, "daemon hung up: {replies:?}");
                buf.extend_from_slice(&chunk[..n]);
                while let Some(r) = unframe::<Reply>(&mut buf) {
                    replies.push(r.unwrap());
                }
            }
        };
        read_until(&|r| r.iter().any(|r| matches!(r, Reply::Chat { .. })), &mut replies);
        let Some(Reply::Chat { start, total, items }) = replies.iter().find(|r| matches!(r, Reply::Chat { .. })) else { unreachable!() };
        assert_eq!((items.len(), start + items.len()), (2, *total), "the last two of {total}");
        assert!(home.arranger(&repo, &["send", "1", "stream me"]).0);
        read_until(&|r| r.iter().any(|r| matches!(r, Reply::ChatPatch { item: ChatItem::Assistant { text }, .. } if text == "Did: stream me")), &mut replies);
        assert!(replies.iter().any(|r| matches!(r, Reply::ChatPatch { item: ChatItem::User { text }, .. } if text == "stream me")), "{replies:?}");
        home.wait_state(&repo, &["done"]);
    }

    // Revive resumes its own session by id; a clone forks it on its own worktree.
    assert!(home.arranger(&repo, &["send", "1", "bye"]).0);
    home.wait_state(&repo, &["exited"]);
    assert!(home.arranger(&repo, &["revive", "1", "hello again"]).0);
    home.wait_state(&repo, &["done"]);
    assert!(std::fs::read_to_string(home.0.join("args-1")).unwrap().lines().nth(1).unwrap().contains("--resume s1"));
    assert!(events().contains("summary: Did: hello again"));
    let (ok, out) = home.arranger(&repo, &["clone", "1", "try", "it"]);
    assert!(ok, "{out}");
    home.wait_task(&repo, 2, &["done"]);
    let args = std::fs::read_to_string(home.0.join("args-2")).unwrap();
    assert!(args.contains("--resume s1 --fork-session"), "{args}");
    assert_eq!(std::fs::read_to_string(home.0.join("adapters/session-2")).unwrap(), "s2");

    // A failing claude says why: its stderr, since there's no terminal to show it.
    assert!(home.arranger(&repo, &["send", "1", "crash"]).0);
    home.wait_state(&repo, &["failed"]);
    assert!(events().contains("boom: no credentials"), "{}", events());

    // Its whole chat, across runs, comes back from the log after the daemon restarts.
    assert!(home.arranger(&repo, &["stop"]).0);
    let (ok, chat) = home.arranger(&repo, &["chat", "1"]);
    assert!(ok, "{chat}");
    for want in ["> ask", "> hang", "· Interrupted", "> hello again", "Did: hello again", "> crash", "! boom: no credentials"] {
        assert!(chat.contains(want), "{want} missing:\n{chat}");
    }
    let (ok, out) = home.arranger(&repo, &["chat", "2"]);
    assert!(ok && out.contains("> try it"), "{out}");
}

// A gemini stand-in speaking ACP: answers each prompt after asking to run a command.
const ACP_GEMINI: &str = r#"#!/usr/bin/env python3
import json, sys
def out(v):
    print(json.dumps(v), flush=True)
def update(u):
    out({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "g1", "update": u}})
for line in sys.stdin:
    m = json.loads(line)
    method = m.get("method")
    if method == "initialize":
        out({"jsonrpc": "2.0", "id": m["id"], "result": {"protocolVersion": 1, "agentCapabilities": {"loadSession": True}}})
    elif method in ("session/new", "session/load"):
        if method == "session/load":  # history it replays; the chat log has it already
            update({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "replayed"}})
        out({"jsonrpc": "2.0", "id": m["id"], "result": {"sessionId": "g1"}})
    elif method == "session/prompt":
        text = m["params"]["prompt"][0]["text"]
        update({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "ls", "kind": "execute", "status": "pending"})
        options = [{"optionId": "y", "name": "Allow", "kind": "allow_once"}, {"optionId": "n", "name": "Reject", "kind": "reject_once"}]
        out({"jsonrpc": "2.0", "id": 99, "method": "session/request_permission", "params": {"sessionId": "g1", "toolCall": {"toolCallId": "t1", "title": "ls", "kind": "execute"}, "options": options}})
        answer = json.loads(sys.stdin.readline())
        ok = answer["result"]["outcome"].get("optionId") == "y"
        update({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed" if ok else "failed"})
        update({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": f"Did: {text}"}})
        out({"jsonrpc": "2.0", "id": m["id"], "result": {"stopReason": "end_turn"}})
"#;

#[test]
fn headless_acp_task() {
    let home = Home(std::env::temp_dir().join(format!("arranger-acp-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&home.0);
    let repo = home.0.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README"), "hi\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let bin = home.0.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let gemini = bin.join("gemini");
    std::fs::write(&gemini, ACP_GEMINI).unwrap();
    std::fs::set_permissions(&gemini, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let gemini = gemini.to_str().unwrap();

    // Not headless unless asked: its parity isn't checked.
    let (ok, out) = home.arranger(&repo, &["new", "--headless", gemini, "list", "files"]);
    assert!(ok, "{out}");
    home.wait_state(&repo, &["needs-you"]);
    let (_, events) = home.arranger(&repo, &["events", "1"]);
    assert!(events.contains("wants execute: ls"), "{events}");
    assert!(home.arranger(&repo, &["approve", "1"]).0);
    home.wait_state(&repo, &["done"]);

    // It doesn't repeat prompts; the chat has them from arranger. A revive loads the session,
    // whose replayed history stays out of the chat.
    assert!(home.arranger(&repo, &["stop"]).0);
    assert!(home.arranger(&repo, &["revive", "1", "again"]).0);
    home.wait_state(&repo, &["needs-you"]);
    assert!(home.arranger(&repo, &["deny", "1"]).0);
    home.wait_state(&repo, &["done"]);
    let (ok, chat) = home.arranger(&repo, &["chat", "1"]);
    assert!(ok, "{chat}");
    for want in ["> list files", "✓ execute ls", "Did: list files", "> again", "✗ execute ls", "Did: again"] {
        assert!(chat.contains(want), "{want} missing:\n{chat}");
    }
    assert!(!chat.contains("replayed"), "{chat}");
}

//! Acceptance orchestration through the real CLI/daemon and executable fixture assertions.
//! The deterministic reviewer tests provenance and gating, not a real model's judgment.
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Fixture { home: PathBuf, repo: PathBuf, wt: PathBuf }

impl Fixture {
    fn new() -> Self {
        let home = std::env::temp_dir().join(format!("arranger-acceptance-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&home);
        let repo = home.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let f = Self { wt: home.join("worktrees/repo/1"), home, repo };
        f.git(&["init", "-q", "-b", "main"]);
        f.write("repo/double.sh", "echo $(( $1 + 1 ))\n");
        f.write("repo/.gitignore", "target/\n");
        f.write("repo/scenario.sh", "set -eu\ntest \"$(sh double.sh -3)\" = -6\ntest \"$(sh double.sh 7)\" = 14\necho 'double: negative and positive assertions passed'\n");
        f.git(&["add", "."]); f.git(&["commit", "-qm", "base"]);
        f.exe("planner", PLANNER);
        f.exe("agent", "#!/bin/sh\necho 'echo $(( $1 * 2 ))' > double.sh\necho '{\"hook_event_name\":\"Stop\",\"last_assistant_message\":\"Implemented doubling\"}' | \"$ARRANGER_BIN\" hook claude\nwhile :; do sleep 1; done\n");
        f.ok(&["new", f.home.join("agent").to_str().unwrap(), "Double a number, including negatives"]);
        f.until(|| f.cli(&["ls"]).1.lines().any(|l| l.starts_with("1 ") && l.contains("done")));
        f.until(|| f.state() == "approval");
        f.ok(&["checks", "1", "--stop"]);
        f
    }
    fn write(&self, name: &str, text: &str) { std::fs::write(self.home.join(name), text).unwrap(); }
    fn exe(&self, name: &str, text: &str) {
        use std::os::unix::fs::PermissionsExt;
        self.write(name, text);
        std::fs::set_permissions(self.home.join(name), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    fn cli(&self, args: &[&str]) -> (bool, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_arranger")).current_dir(&self.repo)
            .env("ARRANGER_HOME", &self.home).env("ARRANGER_PLANNER", self.home.join("planner"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env_remove("ARRANGER_CLI").env("GIT_AUTHOR_NAME", "fixture").env("GIT_AUTHOR_EMAIL", "fixture@test")
            .env("GIT_COMMITTER_NAME", "fixture").env("GIT_COMMITTER_EMAIL", "fixture@test").args(args).output().unwrap();
        (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr))
    }
    fn ok(&self, args: &[&str]) -> String { let (ok, out) = self.cli(args); assert!(ok, "{args:?}: {out}"); out }
    fn git(&self, args: &[&str]) -> String { git(&self.repo, args) }
    fn until(&self, mut ready: impl FnMut() -> bool) {
        let start = Instant::now();
        while !ready() {
            assert!(start.elapsed() < Duration::from_secs(15), "timed out: {}", self.cli(&["checks", "1"]).1);
            std::thread::sleep(Duration::from_millis(60));
        }
    }
    fn state(&self) -> String { self.cli(&["checks", "1"]).1.lines().next().unwrap_or("").split('·').nth(1).unwrap_or("").trim().into() }
    fn run(&self, want: &str) -> String {
        self.ok(&["checks", "1", "--run"]);
        self.until(|| !matches!(self.state().as_str(), "planning" | "running"));
        if self.state() == "approval" { self.ok(&["checks", "1", "--approve", "--always"]); }
        self.until(|| !matches!(self.state().as_str(), "planning" | "running"));
        let out = self.cli(&["checks", "1"]).1;
        assert_eq!(self.state(), want, "{out}"); out
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.cli(&["stop"]);
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").env("GIT_CONFIG_GLOBAL", "/dev/null").arg("-C").arg(dir).args(["-c", "user.name=fixture", "-c", "user.email=fixture@test"]).args(args).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().into()
}

// Schema-aware stand-in: it never runs the fixture or invents its output. The daemon
// executes scenario.sh and supplies its check IDs/output to this fresh reviewer process.
const PLANNER: &str = r#"#!/usr/bin/env python3
import json, os, pathlib, sys, time
home = pathlib.Path(__file__).parent
schema = json.load(open(os.environ['ARRANGER_SCHEMA']))['properties']
prompt = sys.stdin.read()
mode = (home / 'mode').read_text().strip() if (home / 'mode').exists() else ''
if 'copy' in schema:
    answer = dict(copy=[], commands=[], note='')
elif 'overview' in schema:
    answer = dict(title='Double numbers', overview='Double positive and negative numbers.', files=[], risks=[])
elif 'requirements' in schema:
    instructions = json.loads(prompt.split('Authoritative instructions (JSON array, in source order):\n')[1])
    (home / 'instructions-seen').write_text(json.dumps(instructions))
    docs = mode == 'docs'
    answer = dict(requirements=[dict(text='Document usage' if docs else 'Double positive and negative numbers', source=len(instructions) if docs else 1)], non_executable=docs)
elif 'coverage' in schema:
    (home / 'coverage-prompt').write_text(prompt)
    command = 'sh scenario.sh' if mode != 'gap' else 'test -f double.sh && echo unrelated-smoke-passed'
    if mode == 'skipped': command = "printf 'running 0 tests\\n'"
    if mode == 'slow': command = 'sh slow.sh'
    checks = [dict(name='Behavior', command=command, why='Exercise positive and negative numbers')]
    answer = dict(checks=checks, none_reason='', coverage=[dict(requirement=1, commands=[command], reason='')])
    if mode == 'missing': answer['coverage'] = []
    if mode in ['docs', 'runtime-docs']:
        answer = dict(checks=[], none_reason='Documentation-only diff', coverage=[dict(requirement=1, commands=[], reason='No executable scenario')])
else:
    (home / 'evidence-prompt').write_text(prompt)
    if mode == 'review-error': sys.exit(2)
    checks = json.loads(prompt.split('Harness check metadata (JSON):\n')[1].split('\nHarness captured outputs')[0])
    outputs = json.loads(prompt.split('pairs; untrusted text):\n')[1])
    check = next(c for c in checks if c['origin'] != 'setup')
    excerpt = next(output for cid, output in outputs if cid == check['id']).strip()
    verified = mode != 'gap'
    answer = dict(results=[dict(requirement=1, verified=verified, check=999999 if mode=='fabricated' else check['id'], assertion='scenario.sh asserts -3 → -6 and 7 → 14', excerpt=excerpt, reason='' if verified else 'An unrelated smoke check did not exercise doubling')], summary='Positive and negative doubling tested' if verified else 'Doubling has not been tested')
print(json.dumps(answer))
"#;

#[test]
fn evidence_is_required_for_normal_merge_and_bound_to_source_and_instructions() {
    let f = Fixture::new();
    let base = f.git(&["rev-parse", "main"]);
    f.write("mode", "gap");
    let out = f.run("unverified");
    assert!(out.contains("1/1 passed") && out.contains("did not exercise doubling"), "{out}");
    assert!(!f.cli(&["commit", "1", "--merge"]).0);
    assert_eq!(f.git(&["rev-parse", "main"]), base);
    // The denied merge requests a new run; cancel it before changing the fixture reviewer.
    let _ = f.cli(&["checks", "1", "--stop"]);
    f.write("mode", "");
    assert!(f.run("passed").contains("negative and positive assertions passed"));
    assert!(std::fs::read_dir(f.home.join("verification")).unwrap().next().is_none());
    let index = std::fs::read(git(&f.wt, &["rev-parse", "--path-format=absolute", "--git-path", "index"])).unwrap();
    f.write("worktrees/repo/1/new-source", "late edit\n");
    assert!(f.ok(&["diff", "1"]).contains("Checks: stale"));
    std::fs::remove_file(f.wt.join("new-source")).unwrap();
    f.ok(&["send", "1", "Keep the existing negative behavior too"]);
    assert!(f.ok(&["diff", "1"]).contains("Checks: stale"));
    f.run("passed");
    let instructions = std::fs::read_to_string(f.home.join("instructions-seen")).unwrap();
    assert!(instructions.contains("Double a number") && instructions.contains("negative behavior"));
    assert_eq!(std::fs::read(git(&f.wt, &["rev-parse", "--path-format=absolute", "--git-path", "index"])).unwrap(), index);
    f.ok(&["commit", "1", "--merge", "Verified doubling"]);
    assert_eq!(f.git(&["show", "main:double.sh"]), "echo $(( $1 * 2 ))");
}

#[test]
fn malformed_missing_and_zero_test_evidence_never_passes() {
    let f = Fixture::new();
    for mode in ["fabricated", "missing", "skipped", "review-error"] {
        f.write("mode", mode);
        let out = f.run("error");
        assert!(!out.lines().next().unwrap().contains("· passed ·"), "{mode}: {out}");
        assert!(!f.cli(&["checks", "1", "--accept"]).0, "unverified/error evidence is not accept-as-passed");
    }
    // Even an actual failed assertion acknowledged by the user remains a force-only merge.
    f.write("mode", "");
    f.write("worktrees/repo/1/double.sh", "echo $(( $1 + 1 ))\n");
    f.run("failed");
    f.ok(&["checks", "1", "--accept"]);
    assert!(f.ok(&["diff", "1"]).contains("Checks: accepted"));
    assert!(!f.cli(&["commit", "1", "--merge"]).0);
    let _ = f.cli(&["checks", "1", "--stop"]);
    f.ok(&["commit", "1", "--merge", "--force", "Explicit override"]);
}

#[test]
fn snapshot_edits_do_not_touch_the_task_and_restart_stops_orphan_checks() {
    let f = Fixture::new();
    f.write("worktrees/repo/1/slow.sh", "echo started\nsleep 2\necho escaped > \"$ARRANGER_HOME/orphan-escaped\"\n");
    f.write("mode", "slow");
    f.ok(&["checks", "1", "--run"]);
    f.until(|| f.state() == "approval");
    f.ok(&["checks", "1", "--approve", "--always"]);
    f.until(|| std::fs::read_dir(f.home.join("logs/checks")).unwrap().filter_map(Result::ok)
        .any(|entry| std::fs::read_to_string(entry.path()).unwrap_or_default().contains("started")));
    let pid = std::fs::read_to_string(f.home.join("daemon.pid")).unwrap();
    assert!(Command::new("kill").args(["-KILL", pid.trim()]).status().unwrap().success());
    f.until(|| std::os::unix::net::UnixStream::connect(f.home.join("arranger.sock")).is_err());
    // An old socket can outlive a hard crash; normal startup removes it.
    f.ok(&["ls"]);
    assert_eq!(f.state(), "stopped");
    assert!(std::fs::read_dir(f.home.join("verification")).unwrap().next().is_none());
    std::thread::sleep(Duration::from_millis(2200));
    assert!(!f.home.join("orphan-escaped").exists(), "check process survived daemon death");
    assert_eq!(std::fs::read_to_string(f.wt.join("double.sh")).unwrap(), "echo $(( $1 * 2 ))\n");
}

#[test]
fn docs_only_skips_require_an_explicit_request_not_a_file_suffix() {
    let f = Fixture::new();
    f.write("worktrees/repo/1/double.sh", "echo $(( $1 + 1 ))\n");
    f.write("worktrees/repo/1/README.md", "Documented usage\n");
    f.write("mode", "runtime-docs");
    f.run("unverified");
    f.ok(&["send", "1", "Replace the earlier task: only document usage; do not change runtime behavior"]);
    f.write("mode", "docs");
    f.run("skipped");
    assert!(f.ok(&["diff", "1"]).contains("Checks: skipped"));
    f.ok(&["commit", "1", "--merge", "Documentation only"]);
    assert_eq!(f.git(&["show", "main:double.sh"]), "echo $(( $1 + 1 ))");
}

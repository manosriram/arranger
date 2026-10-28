//! Checks: commands arranger runs in a task's worktree to see whether its work holds up (tests,
//! builds, linters), one after another, each with its own output. A run is keyed on the
//! worktree's tree hash, so it's known exactly which state of the work it judged.

use crate::proto::CheckSpec;
use std::path::Path;
use std::process::Command;

/// Most checks a run takes.
pub const MAX_CHECKS: usize = 20;

/// Only numeric check directories are owned by the runner. Do not traverse unknown entries.
pub fn prune_artifacts(root: &Path, keep: &std::collections::HashSet<u64>) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let Some(id) = entry.file_name().to_str().and_then(|name| name.parse::<u64>().ok()) else { continue };
        if keep.contains(&id) { continue; }
        if entry.file_type()?.is_dir() { std::fs::remove_dir_all(entry.path())?; }
        else { std::fs::remove_file(entry.path())?; }
    }
    Ok(())
}

/// Passing commands do not verify a task with uncovered acceptance criteria.
pub fn run_outcome(failed: u32, stopped: u32, coverage_complete: bool) -> &'static str {
    if failed > 0 { "failed" } else if stopped > 0 { "stopped" } else if !coverage_complete { "unverified" } else { "passed" }
}

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
    static INDEX_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = INDEX_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let copy = std::env::temp_dir().join(format!("arranger-index-{}-{nanos}-{sequence}", std::process::id()));
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

/// What a check did to the worktree: files it added, and files it changed or deleted.
#[derive(Debug, Default, PartialEq)]
pub struct Changes {
    pub added: Vec<String>,
    pub altered: Vec<String>,
}

/// The worktree as `tree` again (a check changed it): files `tree` has are put back, files it
/// doesn't (and git doesn't ignore) are removed. Returns what had changed. The index is untouched.
pub fn restore_tree(worktree: &Path, tree: &str) -> Result<Changes, String> {
    let now = tree_hash(worktree)?;
    let out = Command::new("git").arg("-C").arg(worktree).args(["diff", "--name-status", "--no-renames", tree, &now]).output().map_err(|e| format!("git: {e}"))?;
    let mut changed = Changes::default();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some((status, path)) = line.split_once('\t') {
            if status == "A" { &mut changed.added } else { &mut changed.altered }.push(path.to_string());
        }
    }
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let index = std::env::temp_dir().join(format!("arranger-restore-{}-{nanos}", std::process::id()));
    let git = |args: &[&str]| {
        let out = Command::new("git").arg("-C").arg(worktree).args(args).env("GIT_INDEX_FILE", &index).output().map_err(|e| format!("git: {e}"))?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
        }
        Ok(out.stdout)
    };
    let restored = git(&["read-tree", tree]).and_then(|_| {
        for added in git(&["ls-files", "-z", "--others", "--exclude-standard"])?.split(|b| *b == 0).filter(|p| !p.is_empty()) {
            let path = Path::new(std::str::from_utf8(added).map_err(|e| e.to_string())?);
            if crate::security::relative_path(path).is_ok() {
                let _ = std::fs::remove_file(worktree.join(path));
            }
        }
        git(&["checkout-index", "-a", "-f"])
    });
    let _ = std::fs::remove_file(&index);
    restored.map(|_| changed)
}

/// Paths that differ between two trees.
pub fn changed_files(worktree: &Path, from: &str, to: &str) -> Result<Vec<String>, String> {
    let out = Command::new("git").arg("-C").arg(worktree).args(["diff", "--name-only", "--no-renames", from, to]).output().map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect())
}

/// Whether any of these paths is code (or config) rather than docs and images: a change to
/// them that no check covers is unverified.
pub fn code_changed(paths: &[String]) -> bool {
    const DOCS: &[&str] = &["md", "markdown", "mdx", "txt", "rst", "adoc", "png", "jpg", "jpeg", "gif", "svg", "webp", "ico", "pdf"];
    paths.iter().any(|p| {
        let name = p.rsplit('/').next().unwrap_or(p);
        let ext = name.rsplit_once('.').map(|(_, e)| e.to_lowercase());
        let doc = ext.as_deref().is_some_and(|e| DOCS.contains(&e)) || matches!(name, "LICENSE" | "AUTHORS" | "CODEOWNERS" | "NOTICE");
        !doc
    })
}

pub fn plan_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["checks", "none_reason", "acceptance"],
        "properties": {
            "checks": {"type": "array", "maxItems": MAX_CHECKS, "items": {
                "type": "object",
                "additionalProperties": false,
                "required": ["name", "command", "why"],
                "properties": {"name": {"type": "string"}, "command": {"type": "string", "maxLength": 500}, "why": {"type": "string"}}
            }},
            "none_reason": {"type": "string"},
            "acceptance": {"type": "array", "items": {
                "type": "object", "additionalProperties": false,
                "required": ["criterion", "commands", "gap"],
                "properties": {
                    "criterion": {"type": "string"},
                    "commands": {"type": "array", "items": {"type": "string"}},
                    "gap": {"type": "string"}
                }
            }}
        }
    })
}

/// What the check planner sees of a trip.
pub struct Trip<'a> {
    /// What the agent was asked this turn.
    pub prompts: &'a [String],
    /// `git diff --stat` and the diff since the work last passed (or since its base), cut to size.
    pub stat: &'a str,
    pub diff: &'a str,
    /// Build and dependency files in the repository, to read for its commands.
    pub manifests: &'a [String],
    /// The last run's checks: (command, how it went).
    pub previous: &'a [(String, String)],
    /// Checks still failing from before: they run whatever the plan says.
    pub sticky: &'a [CheckSpec],
}

/// Hard byte budgets include untrusted prompts, diffs, and previous command output.
pub const PLAN_PROMPT_MAX: usize = 24 * 1024;

pub fn bounded(text: &str, bytes: usize) -> String {
    if text.len() <= bytes { return text.to_string(); }
    let suffix = "\n… [truncated; read repository files for details]";
    let mut cut = bytes.saturating_sub(suffix.len());
    while !text.is_char_boundary(cut) { cut -= 1; }
    if bytes < suffix.len() { return text[..cut].to_string(); }
    format!("{}{suffix}", &text[..cut])
}

pub fn plan_prompt(trip: &Trip) -> String {
    let prompts = bounded(&trip.prompts.join("\n"), 4096);
    let manifests = bounded(&trip.manifests.join("\n"), 1024);
    let previous = bounded(&trip.previous.iter().take(MAX_CHECKS).map(|(c, s)| format!("{c}: {s}")).collect::<Vec<_>>().join("\n"), 2048);
    let sticky = bounded(&trip.sticky.iter().take(MAX_CHECKS).map(|c| c.command.as_str()).collect::<Vec<_>>().join("\n"), 2048);
    let stat = bounded(trip.stat, 1024);
    let diff = bounded(trip.diff, 12 * 1024);
    bounded(&format!(
        "Plan checks for the user's requested outcomes, not merely the implementation. Read only; do not run checks or edit files.
Arranger executes each selected command from the worktree root with a 10-minute timeout.
Derive concrete acceptance criteria from the original task and latest corrections. Read actual tests and code before
claiming coverage. Include omitted requested behavior, relevant negative/boundary cases, and affected regressions.
For each criterion give exact selected commands and any gap. Compilation/lint, test names, source-string searches,
and file-existence assertions do not prove behavior. GUI changes need relevant interaction and rendering evidence:
clicks/keys, resulting state, layout assertions or a reviewed visual baseline. Screenshots alone are not assertions.
For web, desktop, mobile and custom-rendered GUIs, discover the target project's framework and automation driver.
Use that project's runnable tests; never assume Arranger's GUI or an Arranger-specific test protocol. Report missing
interaction tests or drivers as concrete gaps for the coding agent to address.
Use the repository's existing test harness and scripts; inspect its guidance. If tests are missing, propose a short
behavioral probe only when it exercises the real code with isolated fixtures and explicit expected results. Commands
must be a single readable line of at most 500 bytes. Substantial probes belong in reviewable repository test files;
if missing, report the test that the coding agent must add as a coverage gap. Never encode a large script in a command.
Retain evidence in $ARRANGER_CHECK_ARTIFACTS outside the checkout. Isolated local apps/servers are allowed only with
bounded lifetimes and cleanup. Never drive live user data/apps. No installs, external network calls, persistent servers,
deploys, interactive commands or file-rewriting tools; use check-only flags. Prefer targeted tests, then broader checks
for shared code. Documentation examples and claims need relevant checks too. A build directly covers a build-only task.
If no meaningful check can execute, return no checks and explain why in none_reason. Otherwise leave none_reason empty.
Give each command a short name and explain what its assertions prove. Missing/uncertain evidence belongs in gap;
do not weaken acceptance criteria to fit available tests. Context is bounded; inspect files if the diff is truncated.

Task goal and latest turn (later corrections take precedence):
{prompts}

Build/dependency paths:
{manifests}

Previous checks:
{previous}

Still failing (run automatically; do not repeat):
{sticky}

Changes since last verified tree:
{stat}
{diff}"), PLAN_PROMPT_MAX)
}

/// Validated commands, a coverage note, and whether acceptance evidence is incomplete.
pub fn plan_from(answer: serde_json::Value) -> Result<(Vec<CheckSpec>, String, bool), String> {
    #[derive(serde::Deserialize)]
    struct Criterion {
        criterion: String,
        commands: Vec<String>,
        gap: String,
    }
    #[derive(serde::Deserialize)]
    struct Answer {
        checks: Vec<CheckSpec>,
        acceptance: Vec<Criterion>,
        #[serde(default)]
        none_reason: String,
    }
    let a: Answer = serde_json::from_value(answer).map_err(|e| format!("the check plan doesn't fit: {e}"))?;
    let reason = a.none_reason.lines().next().unwrap_or("").trim().chars().take(200).collect();
    let mut checks = Vec::new();
    let mut gaps = Vec::new();
    for check in a.checks {
        if check.command.trim().is_empty() { continue; }
        match validate(vec![check]) {
            Ok(mut valid) if checks.len() < MAX_CHECKS => checks.append(&mut valid),
            Ok(_) => gaps.push("Too many checks; extra checks were omitted".into()),
            Err(error) => gaps.push(format!("Skipped invalid check: {error}")),
        }
    }
    let answer_acceptance_missing = a.acceptance.is_empty();
    for c in a.acceptance {
        if c.criterion.trim().is_empty() {
            gaps.push("An acceptance criterion is empty".into());
            continue;
        }
        if c.commands.iter().any(|command| !checks.iter().any(|check| check.command == command.trim())) {
            gaps.push(format!("{}: references an unselected or rejected command", c.criterion));
        }
        if !c.gap.trim().is_empty() || c.commands.is_empty() {
            gaps.push(format!("{}: {}", c.criterion.trim(), if c.gap.trim().is_empty() { "no check covers this outcome" } else { c.gap.trim() }));
        }
    }
    if !checks.is_empty() && gaps.is_empty() && answer_acceptance_missing {
        gaps.push("No user acceptance criteria were identified".into());
    }
    let incomplete = !gaps.is_empty();
    let note = if incomplete { bounded(&format!("Unverified requirements: {}", gaps.join("; ")), 4096) } else { reason };
    Ok((checks, note, incomplete))
}

/// Feedback to the coding agent: durable tests and their actual execution, not a coverage claim.
pub fn missing_tests_report(note: &str) -> String {
    format!("Missing acceptance tests for the requested change.\n\n{}\n\nAdd the missing executable regression tests to this repository and run them against the real changed code. \
Each test must assert the user's requested observable outcome, including relevant negative/boundary cases. \
Do not remove or weaken acceptance criteria, substitute compilation/source searches for behavior, or mark gaps covered without evidence. \
For any GUI (web, desktop, mobile, or custom-rendered), inspect this project's framework and available automation; \
use or add an appropriate project-local interaction/visual test harness. Exercise actual input, resulting state, \
focus and layout where relevant; screenshots alone are not assertions. Do not assume an Arranger-specific GUI or inject \
Arranger hooks. Use isolated fixtures and bounded processes, never live user data. Keep test code in reviewable files \
and provide short commands (at most 500 bytes) that Arranger can rerun. Follow the project's dependency and permission rules. \
If a display, driver, credentials, or expected behavior is unavailable, explain the precise blocker rather than fabricating a pass. \
Arranger will independently replan and execute checks after your turn.", bounded(note, 4096))
}

/// Check-only commands that run without asking: common test, build, type and lint commands,
/// with no shell operators, and the project's own scripts whose names say they check.
pub fn allowed(command: &str, worktree: &Path) -> bool {
    if ["&&", "||", ";", "|", ">", "<", "`", "$(", "&"].iter().any(|op| command.contains(op)) {
        return false;
    }
    let words: Vec<&str> = command.split_whitespace().collect();
    let has = |flag: &str| words.contains(&flag);
    match words.as_slice() {
        ["cargo", sub, ..] => matches!(*sub, "test" | "check" | "clippy" | "build" | "nextest" | "doc" | "bench") && !has("--fix") || *sub == "fmt" && has("--check"),
        ["go", "test" | "vet" | "build", ..] => true,
        ["pytest", ..] | ["mypy", ..] | ["pyright", ..] => true,
        ["python" | "python3", "-m", "pytest" | "mypy" | "unittest", ..] => true,
        ["uv" | "poetry" | "pipenv", "run", "pytest" | "mypy" | "pyright", ..] => true,
        ["ruff", "check", ..] => !has("--fix"),
        ["ruff", "format", ..] | ["prettier", ..] | ["npx", "prettier", ..] => has("--check"),
        ["tsc", ..] | ["npx", "tsc", ..] => has("--noEmit"),
        ["eslint", ..] | ["npx", "eslint", ..] => !has("--fix"),
        ["npx", "vitest" | "jest", ..] | ["vitest" | "jest", ..] => !has("--watch") && !has("-u") && !has("--updateSnapshot"),
        ["swift", "test" | "build", ..] | ["dotnet", "test" | "build", ..] | ["mix", "test", ..] | ["bundle", "exec", "rspec" | "rake", ..] => true,
        ["npm", "test"] | ["npm", "t"] => script(worktree, "test"),
        ["npm" | "pnpm" | "yarn" | "bun", "run", name, ..] => checks_by_name(name) && script(worktree, name),
        ["pnpm" | "yarn" | "bun", "test", ..] => script(worktree, "test"),
        ["pnpm" | "yarn", name] => checks_by_name(name) && script(worktree, name),
        ["make", target] => checks_by_name(target) && recipe(worktree, &["Makefile", "makefile", "GNUmakefile"], target),
        ["just", recipe_name] => checks_by_name(recipe_name) && recipe(worktree, &["justfile", "Justfile", ".justfile"], recipe_name),
        _ => false,
    }
}

/// A script or target named for checking, not fixing, formatting or shipping.
fn checks_by_name(name: &str) -> bool {
    let n = name.to_lowercase();
    ["test", "check", "lint", "build", "typecheck", "vet", "clippy", "spec"].iter().any(|k| n.contains(k))
        && !["fix", "deploy", "publish", "release", "watch", "write", "update"].iter().any(|k| n.contains(k))
}

fn script(worktree: &Path, name: &str) -> bool {
    std::fs::read(worktree.join("package.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .is_some_and(|v| v.get("scripts").and_then(|s| s.get(name)).is_some())
}

fn recipe(worktree: &Path, files: &[&str], name: &str) -> bool {
    files.iter().filter_map(|f| std::fs::read_to_string(worktree.join(f)).ok()).any(|text| {
        text.lines().any(|l| l.strip_prefix(name).is_some_and(|rest| rest.starts_with(':') || rest.starts_with(' ') && rest.contains(':')))
    })
}

/// A failed check, for telling the agent about it.
pub struct Failure {
    pub name: String,
    pub command: String,
    pub exit: Option<i32>,
    pub note: String,
    /// Its output, one plain line each, and which lines look like errors.
    pub lines: Vec<(String, bool)>,
}

/// What to tell the agent when its checks fail: each failure's command, how it ended, and the
/// part of its output that matters (from just before the first error, else the end).
pub fn failure_report(failures: &[Failure]) -> String {
    const PER_CHECK: usize = 4000;
    let mut out = String::from(
        "These checks failed on your last change. Fix what they found (not the checks themselves), then finish your turn; they run again after it.\n",
    );
    for f in failures {
        let how = match f.exit {
            Some(code) => format!("exit {code}"),
            None => "stopped".into(),
        };
        let name = if f.name == f.command { String::new() } else { format!("{}: ", f.name) };
        out.push_str(&format!("\n✗ {name}`{}` ({how})\n", f.command));
        if !f.note.is_empty() {
            out.push_str(&format!("{}\n", f.note));
        }
        let first = f.lines.iter().position(|l| l.1);
        let (from, to) = match first {
            Some(i) => (i.saturating_sub(5), (i + 80).min(f.lines.len())),
            None => (f.lines.len().saturating_sub(60), f.lines.len()),
        };
        let mut body = String::new();
        if from > 0 {
            body.push_str(&format!("… ({from} lines above)\n"));
        }
        for (line, _) in &f.lines[from..to] {
            if body.len() + line.len() > PER_CHECK {
                body.push_str("… (cut)\n");
                break;
            }
            body.push_str(line);
            body.push('\n');
        }
        if to < f.lines.len() && !body.ends_with("(cut)\n") {
            body.push_str(&format!("… ({} lines below)\n", f.lines.len() - to));
        }
        if !body.trim().is_empty() {
            out.push_str(&format!("```\n{body}```\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn planner_context_is_bounded_for_large_unicode_inputs() {
        let huge = "user request 🦀\n".repeat(100_000);
        let prompt = plan_prompt(&Trip {
            prompts: std::slice::from_ref(&huge), stat: &huge, diff: &huge, manifests: std::slice::from_ref(&huge),
            previous: &[(huge.clone(), huge.clone())], sticky: &[],
        });
        assert!(prompt.len() <= PLAN_PROMPT_MAX);
        assert!(prompt.contains("[truncated"));
        assert_eq!(bounded("ééé", 1), "");
    }

    #[test]
    fn partial_plans_keep_valid_checks_and_report_uncovered_criteria() {
        let (checks, note, incomplete) = plan_from(json!({
            "checks": [
                {"name": "valid", "command": "cargo test dialog", "why": "dialog assertions"},
                {"name": "invalid", "command": "x".repeat(501), "why": "unreviewable"}
            ],
            "none_reason": "",
            "acceptance": [
                {"criterion": "dialog closes", "commands": [" cargo test dialog "], "gap": ""},
                {"criterion": "focus restored", "commands": ["cargo test typo"], "gap": ""}
            ]
        })).unwrap();
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].command, "cargo test dialog");
        assert!(incomplete && note.contains("Skipped invalid check") && note.contains("focus restored"));
    }

    #[test]
    fn artifact_pruning_keeps_current_evidence_and_does_not_follow_links() {
        let root = std::env::temp_dir().join(format!("arranger-prune-{}", std::process::id()));
        std::fs::create_dir_all(root.join("1")).unwrap();
        std::fs::create_dir_all(root.join("2")).unwrap();
        std::fs::create_dir_all(root.join("unrelated")).unwrap();
        let outside = root.join("unrelated/keep.txt");
        std::fs::write(&outside, "keep").unwrap();
        std::os::unix::fs::symlink(root.join("unrelated"), root.join("3")).unwrap();
        prune_artifacts(&root, &std::collections::HashSet::from([2])).unwrap();
        assert!(!root.join("1").exists() && !root.join("3").exists());
        assert!(root.join("2").is_dir() && outside.is_file());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn passing_build_does_not_verify_an_untested_user_outcome() {
        let (checks, note, incomplete) = plan_from(json!({
            "checks": [{"name": "build", "command": "cargo check", "why": "compilation only"}],
            "none_reason": "",
            "acceptance": [{"criterion": "Escape closes the dialog", "commands": [], "gap": "No interaction test exists"}]
        })).unwrap();
        assert_eq!(checks[0].command, "cargo check");
        assert!(note.contains("Escape closes the dialog") && note.contains("No interaction test exists"));
        assert!(incomplete);
        assert_eq!(run_outcome(0, 0, !incomplete), "unverified");
        assert_eq!(run_outcome(1, 0, !incomplete), "failed");
        assert_eq!(run_outcome(0, 1, !incomplete), "stopped");
    }

    #[test]
    fn acceptance_requires_selected_evidence_and_explicit_coverage() {
        let answer = json!({
            "checks": [{"name": "dialog", "command": "cargo test dialog_escape", "why": "asserts Escape closes dialog"}],
            "none_reason": "",
            "acceptance": [{"criterion": "Escape closes the dialog", "commands": ["cargo test dialog_escape"], "gap": ""}]
        });
        let (_, _, incomplete) = plan_from(answer.clone()).unwrap();
        assert!(!incomplete);
        assert_eq!(run_outcome(0, 0, !incomplete), "passed");
        let mut wrong = answer.clone();
        wrong["acceptance"][0]["commands"] = json!(["cargo test nonexistent"]);
        let (valid, note, incomplete) = plan_from(wrong).unwrap();
        assert_eq!(valid.len(), 1);
        assert!(incomplete && note.contains("unselected or rejected command"));
        let mut missing = answer.clone();
        missing.as_object_mut().unwrap().remove("acceptance");
        assert!(plan_from(missing).is_err());
        let mut empty = answer.clone();
        empty["acceptance"] = json!([]);
        assert!(plan_from(empty).unwrap().2);
        let mut partial = answer;
        partial["acceptance"][0]["gap"] = json!("Keyboard covered; visual layout still untested");
        assert!(plan_from(partial).unwrap().2);
    }

    #[test]
    fn reports_show_each_failure_from_its_first_error() {
        let mut lines: Vec<(String, bool)> = (0..200).map(|i| (format!("line {i}"), false)).collect();
        lines[150].1 = true;
        let report = failure_report(&[
            Failure { name: "unit".into(), command: "cargo test".into(), exit: Some(101), note: String::new(), lines },
            Failure { name: "fmt".into(), command: "touch x".into(), exit: Some(0), note: "changed lib.rs (put back)".into(), lines: vec![] },
        ]);
        assert!(report.contains("✗ unit: `cargo test` (exit 101)\n```\n… (145 lines above)\nline 145\n"), "{report}");
        assert!(report.contains("line 199\n```") && !report.contains("line 144\n"), "{report}");
        assert!(report.contains("✗ fmt: `touch x` (exit 0)\nchanged lib.rs (put back)\n"), "{report}");
        // No error line: the end of it.
        let tail = failure_report(&[Failure { name: "x".into(), command: "x".into(), exit: None, note: String::new(), lines: (0..100).map(|i| (format!("l{i}"), false)).collect() }]);
        assert!(tail.contains("✗ `x` (stopped)\n```\n… (40 lines above)\nl40\n") && tail.contains("l99\n```"), "{tail}");
    }

    #[test]
    fn check_only_commands_run_without_asking() {
        let dir = std::env::temp_dir().join(format!("arranger-allow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("package.json"), r#"{"scripts":{"test":"jest","lint":"eslint .","lint:fix":"eslint --fix .","deploy":"x"}}"#).unwrap();
        std::fs::write(dir.join("justfile"), "test-api filter='':\n    cargo test\ndeploy:\n    ./ship\n").unwrap();
        std::fs::write(dir.join("Makefile"), "check: build\n\tgo vet ./...\n").unwrap();
        for ok in ["cargo test -p api limiter::", "cargo clippy --all-targets -- -D warnings", "cargo fmt --check", "go test ./...", "pytest tests/test_a.py -k x",
            "npx tsc --noEmit", "npm test", "npm run lint", "pnpm lint", "just test-api", "make check", "ruff check src", "npx eslint src/a.ts"] {
            assert!(allowed(ok, &dir), "{ok}");
        }
        for ask in ["cargo fmt", "npm run deploy", "npm run lint:fix", "npm run typecheck", "just deploy", "make install", "cargo test && rm -rf /",
            "cargo test | tee x", "curl x", "eslint --fix .", "npx jest -u", "rm -rf target", "cargo test > out"] {
            assert!(!allowed(ask, &dir), "{ask}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plans_are_checked_and_docs_are_not_code() {
        let (checks, reason, _) = plan_from(json!({"checks": [{"name": "unit", "command": " cargo test ", "why": "x"}, {"name": "", "command": "", "why": ""}], "none_reason": "", "acceptance": [{"criterion": "unit behavior", "commands": ["cargo test"], "gap": ""}]})).unwrap();
        assert_eq!((checks.len(), checks[0].command.as_str(), reason.as_str()), (1, "cargo test", ""));
        let (checks, reason, _) = plan_from(json!({"checks": [], "none_reason": "docs only\nmore", "acceptance": []})).unwrap();
        assert!(checks.is_empty() && reason == "docs only");
        let (checks, note, incomplete) = plan_from(json!({"checks": [{"name": "a", "command": "a\nb", "why": ""}], "none_reason": "", "acceptance": []})).unwrap();
        assert!(checks.is_empty() && incomplete && note.contains("Skipped invalid check"));
        assert!(!code_changed(&["README.md".into(), "docs/a.png".into(), "LICENSE".into()]));
        assert!(code_changed(&["README.md".into(), "src/a.rs".into()]));
        assert!(code_changed(&["Makefile".into()]));
    }

    #[test]
    fn restoring_undoes_a_checks_edits_and_new_files() {
        let dir = std::env::temp_dir().join(format!("arranger-restore-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("a"), "1").unwrap();
        std::fs::write(dir.join("gone"), "x").unwrap();
        std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", "one"]);
        std::fs::write(dir.join("a"), "agent's edit").unwrap();
        std::fs::write(dir.join("new"), "agent's file").unwrap();
        let before = tree_hash(&dir).unwrap();
        // A "check" that formats, deletes, adds and builds.
        std::fs::write(dir.join("a"), "formatted").unwrap();
        std::fs::remove_file(dir.join("gone")).unwrap();
        std::fs::write(dir.join("snapshot"), "x").unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join("target/out"), "build").unwrap();
        let changed = restore_tree(&dir, &before).unwrap();
        assert_eq!(changed, Changes { added: vec!["snapshot".into()], altered: vec!["a".into(), "gone".into()] });
        assert_eq!(tree_hash(&dir).unwrap(), before);
        assert_eq!(std::fs::read_to_string(dir.join("a")).unwrap(), "agent's edit");
        assert_eq!(std::fs::read_to_string(dir.join("new")).unwrap(), "agent's file");
        assert!(dir.join("target/out").exists(), "ignored output stays");
        assert_eq!(git(&dir, &["status", "--porcelain"]), "M a\n?? new", "the index is untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }

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

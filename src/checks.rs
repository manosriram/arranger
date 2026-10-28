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
        "required": ["checks", "none_reason"],
        "properties": {
            "checks": {"type": "array", "items": {
                "type": "object",
                "additionalProperties": false,
                "required": ["name", "command", "why"],
                "properties": {"name": {"type": "string"}, "command": {"type": "string"}, "why": {"type": "string"}}
            }},
            "none_reason": {"type": "string"}
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

pub fn plan_prompt(trip: &Trip) -> String {
    let list = |items: Vec<String>| if items.is_empty() { "(none)".to_string() } else { items.join("\n") };
    let prompts = list(trip.prompts.iter().map(|p| format!("- {}", p.chars().take(2000).collect::<String>())).collect());
    let manifests = list(trip.manifests.iter().map(|m| format!("- {m}")).collect());
    let previous = list(trip.previous.iter().map(|(c, s)| format!("- {c}: {s}")).collect());
    let sticky = list(trip.sticky.iter().map(|c| format!("- {}", c.command)).collect());
    format!(
        "A coding agent just finished a turn in this git worktree (the current directory). You decide which checks prove its \
work holds up. You don't run them: arranger runs each command from the worktree root, one after another, and shows the \
results to a human reviewer. You may read the repository (read-only) to find the right commands.

Choose checks that cover what changed, and nothing unrelated:
- tests for the changed code: targeted ones first (a test file, module or package), then the wider suite when the change \
is broad or touches shared code;
- the compiler or type checker, and the linter the project uses, scoped to what changed when the tool allows;
- only commands that report: never formatters or linters that rewrite files (use --check variants), never installs, \
network calls, servers, deploys, or commands that wait for input;
- each should finish in under 10 minutes; use the project's own scripts (package.json, Makefile, justfile…) when it has them.
Give each a short name and say why it covers this change. If nothing needs checking (docs, comments), return no checks and \
say why in none_reason; otherwise leave none_reason empty.

What the agent was asked this turn:
{prompts}

Build and dependency files:
{manifests}

Checks from the last run and how they went:
{previous}

Still failing from before (these run anyway; don't repeat them):
{sticky}

Changes since the work last passed its checks:
{stat}

{diff}",
        stat = trip.stat,
        diff = trip.diff,
    )
}

/// The planner's answer: its checks, checked, and why there are none if there aren't.
pub fn plan_from(answer: serde_json::Value) -> Result<(Vec<CheckSpec>, String), String> {
    #[derive(serde::Deserialize)]
    struct Answer {
        checks: Vec<CheckSpec>,
        #[serde(default)]
        none_reason: String,
    }
    let a: Answer = serde_json::from_value(answer).map_err(|e| format!("the check plan doesn't fit: {e}"))?;
    let reason = a.none_reason.lines().next().unwrap_or("").trim().chars().take(200).collect();
    let mut checks = a.checks;
    checks.retain(|c| !c.command.trim().is_empty());
    checks.truncate(MAX_CHECKS);
    let checks = if checks.is_empty() { Vec::new() } else { validate(checks)? };
    Ok((checks, reason))
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
        let (checks, reason) = plan_from(json!({"checks": [{"name": "unit", "command": " cargo test ", "why": "x"}, {"name": "", "command": "", "why": ""}], "none_reason": ""})).unwrap();
        assert_eq!((checks.len(), checks[0].command.as_str(), reason.as_str()), (1, "cargo test", ""));
        let (checks, reason) = plan_from(json!({"checks": [], "none_reason": "docs only\nmore"})).unwrap();
        assert!(checks.is_empty() && reason == "docs only");
        assert!(plan_from(json!({"checks": [{"name": "a", "command": "a\nb", "why": ""}], "none_reason": ""})).is_err());
        assert_eq!(plan_schema()["required"], json!(["checks", "none_reason"]), "plans are checks and a reason, nothing more");
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

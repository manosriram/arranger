//! Side-runs: one-off, read-only runs of an agent CLI that answer with JSON matching a schema
//! (a setup plan; later check plans and summaries). Nothing they say reaches a task's chat.

use serde_json::Value;
use std::fs;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

/// Which CLI answers.
#[derive(Clone, Debug, PartialEq)]
pub enum Planner {
    Claude,
    Codex,
    /// `$ARRANGER_PLANNER`: a program that reads the prompt on stdin, finds the schema at
    /// `$ARRANGER_SCHEMA`, and prints the answer (bare, or as claude's `structured_output`).
    Program(PathBuf),
}

/// The planner for a task running `agent`: `$ARRANGER_PLANNER` if set ("off" for none), else the
/// task's own agent when it's claude or codex, else whichever of them is installed.
pub fn planner(agent: &str) -> Option<Planner> {
    match std::env::var("ARRANGER_PLANNER") {
        Ok(p) if p == "off" => return None,
        Ok(p) if !p.is_empty() => return Some(Planner::Program(p.into())),
        _ => {}
    }
    let own = match crate::adapter::kind(agent) {
        "codex" => [Planner::Codex, Planner::Claude],
        _ => [Planner::Claude, Planner::Codex],
    };
    own.into_iter().find(|p| on_path(if *p == Planner::Codex { "codex" } else { "claude" }))
}

fn on_path(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|d| d.join(name).is_file()))
}

/// A side-run under way. Its output goes to files in `dir`, read once it exits.
pub struct Run {
    pub child: Child,
    planner: Planner,
    files: [PathBuf; 4],
    owner: Option<std::io::PipeWriter>,
}

/// Starts `planner` in `cwd` on `prompt`, read-only, answering with JSON matching `schema`.
/// `name` keeps its files apart from other runs' in `dir`.
pub fn start(planner: Planner, cwd: &Path, prompt: &str, schema: &Value, model: &str, dir: &Path, name: &str) -> io::Result<Run> {
    start_inner(planner, cwd, prompt, schema, model, dir, name, None)
}

/// Fork the task session when supported. Other agents get a fresh read-only run with the diff.
pub fn start_summary(planner: Planner, cwd: &Path, prompt: &str, model: &str, dir: &Path, name: &str, agent: &str, session: &str, exe: &Path) -> io::Result<Run> {
    start_inner(planner, cwd, prompt, &crate::summary::schema(), model, dir, name, Some((agent, session, exe)))
}

fn start_inner(planner: Planner, cwd: &Path, prompt: &str, schema: &Value, model: &str, dir: &Path, name: &str, summary: Option<(&str, &str, &Path)>) -> io::Result<Run> {
    crate::security::private_dir(dir)?;
    let file = |ext: &str| dir.join(format!("{name}.{ext}"));
    let files = [file("prompt"), file("schema.json"), file("out"), file("err")];
    crate::security::write_private(&files[0], prompt.as_bytes())?;
    crate::security::write_private(&files[1], schema.to_string().as_bytes())?;
    let mut cmd = match &planner {
        Planner::Claude => {
            let binary = summary.filter(|(a, _, _)| crate::adapter::kind(a) == "claude").map_or("claude", |(a, _, _)| a);
            let mut c = Command::new(binary);
            if let Some((_, session, _)) = summary.filter(|(_, s, _)| !s.is_empty()) {
                c.args(["--resume", session, "--fork-session"]);
            }
            c.args(["-p", "--output-format", "json", "--json-schema", &schema.to_string()]);
            c.args(["--tools", "Read,Grep,Glob", "--no-session-persistence"]);
            if !model.is_empty() {
                c.args(["--model", model]);
            }
            c
        }
        Planner::Codex if summary.is_some() => {
            let (agent, session, exe) = summary.unwrap();
            let binary = if crate::adapter::kind(agent) == "codex" { agent } else { "codex" };
            let mut c = Command::new(exe);
            c.args(["internal-summary-codex", binary, session, model]);
            c
        }
        Planner::Codex => {
            let mut c = Command::new("codex");
            c.args(["exec", "--ephemeral", "--sandbox", "read-only", "--skip-git-repo-check", "--color", "never", "--output-schema"]);
            c.arg(&files[1]).arg("-o").arg(&files[2]);
            if !model.is_empty() {
                c.args(["--model", model]);
            }
            c.arg("-");
            c
        }
        Planner::Program(p) => {
            let mut c = Command::new(p);
            c.env("ARRANGER_SCHEMA", &files[1]);
            c
        }
    };
    for key in crate::adapter::PARENT_SESSION_ENV {
        cmd.env_remove(key);
    }
    let (mut cmd, owner) = crate::pty::owned(cmd)?;
    // codex writes its answer to -o and its progress to stdout: keep that out of the answer.
    let stdout = if planner == Planner::Codex && summary.is_none() { file("log") } else { files[2].clone() };
    let child = cmd
        .env("ARRANGER_SIDE_KIND", if summary.is_some() { "summary" } else { "plan" })
        .current_dir(cwd)
        .stdin(fs::File::open(&files[0])?)
        .stdout(crate::security::private_file(&stdout, true)?)
        .stderr(crate::security::private_file(&files[3], true)?)
        .process_group(0)
        .spawn()?;
    let planner = if planner == Planner::Codex && summary.is_some() { Planner::Program(PathBuf::new()) } else { planner };
    Ok(Run { child, planner, files, owner: Some(owner) })
}

impl Drop for Run {
    fn drop(&mut self) {
        self.owner.take();
        let _ = self.child.wait();
    }
}

impl Run {
    /// Once it has exited: its answer and what it cost, if it said. Removes its files.
    pub fn finish(mut self, ok: bool) -> Result<(Value, Option<f64>), String> {
        let read = |p: &Path| fs::read(p).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
        let (out, err) = (read(&self.files[2]), read(&self.files[3]));
        let _ = self.child.wait();
        for f in &self.files {
            let _ = fs::remove_file(f);
        }
        let _ = fs::remove_file(self.files[2].with_extension("log"));
        let failed = || {
            let tail: String = err.trim().chars().rev().take(400).collect::<Vec<_>>().into_iter().rev().collect();
            if tail.is_empty() { "the planner exited with an error".to_string() } else { tail }
        };
        if !ok && self.planner != Planner::Claude {
            return Err(failed());
        }
        parse(&out, self.planner == Planner::Claude).ok_or_else(|| if ok { format!("the planner's answer isn't JSON: {}", excerpt(&out)) } else { failed() })
    }
}

/// claude prints one result object: the answer is its `structured_output`, unless it errored.
/// Others print the answer itself, or wrap it the same way.
fn parse(out: &str, claude: bool) -> Option<(Value, Option<f64>)> {
    let v: Value = serde_json::from_str(out.trim()).ok().or_else(|| {
        // Prose around it: the outermost {...}.
        let (a, b) = (out.find('{')?, out.rfind('}')?);
        serde_json::from_str(out.get(a..=b)?).ok()
    })?;
    let cost = v.get("total_cost_usd").and_then(Value::as_f64);
    match v.get("structured_output") {
        Some(answer) if answer.is_object() => Some((answer.clone(), cost)),
        _ if claude => None,
        _ => Some((v, cost)),
    }
}

fn excerpt(s: &str) -> String {
    let s = s.trim();
    let short: String = s.chars().take(200).collect();
    if short.len() < s.len() { format!("{short}…") } else { short }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn claude_answers_in_structured_output() {
        let out = json!({"type":"result","is_error":false,"total_cost_usd":0.0012,"result":"{}","structured_output":{"copy":[]}}).to_string();
        assert_eq!(parse(&out, true), Some((json!({"copy":[]}), Some(0.0012))));
        // An error result has none.
        let out = json!({"type":"result","is_error":true,"result":"Not logged in"}).to_string();
        assert_eq!(parse(&out, true), None);
    }

    #[test]
    fn other_planners_answer_bare_even_with_prose_around() {
        assert_eq!(parse("{\"a\":1}\n", false), Some((json!({"a":1}), None)));
        assert_eq!(parse("Here you go:\n{\"a\":{\"b\":2}}\nDone.", false), Some((json!({"a":{"b":2}}), None)));
        assert_eq!(parse("no json", false), None);
    }

    #[test]
    fn a_program_planner_gets_prompt_and_schema() {
        let dir = std::env::temp_dir().join(format!("arranger-side-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let prog = dir.join("planner");
        fs::write(&prog, "#!/bin/sh\nread p\nprintf '{\"structured_output\":{\"said\":\"%s\",\"schema\":%s}}' \"$p\" \"$(cat \"$ARRANGER_SCHEMA\")\"\n").unwrap();
        fs::set_permissions(&prog, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let mut run = start(Planner::Program(prog), &dir, "hello\n", &json!({"type":"object"}), "", &dir.join("side"), "t").unwrap();
        let ok = run.child.wait().unwrap().success();
        let (answer, _) = run.finish(ok).unwrap();
        assert_eq!(answer, json!({"said":"hello","schema":{"type":"object"}}));
        assert!(fs::read_dir(dir.join("side")).unwrap().next().is_none(), "its files are removed");
        let _ = fs::remove_dir_all(&dir);
    }
    #[test]
    fn claude_summary_forks_without_writing_the_working_session() {
        let dir = std::env::temp_dir().join(format!("arranger-summary-claude-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let binary = dir.join("claude");
        fs::write(&binary, "#!/bin/sh\nprintf '%s\n' \"$@\" > args\ncat >/dev/null\nprintf '{\"structured_output\":{\"title\":\"Forked\"}}'\n").unwrap();
        fs::set_permissions(&binary, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let mut run = start_summary(Planner::Claude, &dir, "Summarize", "", &dir.join("side"), "summary", binary.to_str().unwrap(), "working-session", Path::new("unused")).unwrap();
        let ok = run.child.wait().unwrap().success();
        assert_eq!(run.finish(ok).unwrap().0["title"], "Forked");
        let args = fs::read_to_string(dir.join("args")).unwrap();
        assert!(args.contains("--resume\nworking-session\n--fork-session\n"), "{args}");
        assert!(args.contains("--tools\nRead,Grep,Glob\n--no-session-persistence"), "{args}");
        let _ = fs::remove_dir_all(dir);
    }

}

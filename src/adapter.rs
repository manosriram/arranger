//! Agent awareness: per-agent launch config and hook parsing into `Event`s,
//! plus OSC 9/777 detection as the generic fallback.
//!
//! claude and codex share one hook JSON shape; gemini uses the same with its own event
//! and tool names; pi gets a small extension that streams JSON lines.

use crate::proto::{Event, ToolKind};
use serde_json::{Value, json};
use std::fs;
use std::io::{self, BufRead, BufReader};
use std::path::Path;

const PI_EXTENSION: &str = include_str!("adapters/pi.ts");
/// Permission asks wait for a human; the hook is killed after this many seconds.
const ASK_TIMEOUT: u64 = 24 * 60 * 60;

/// Markers of the Claude session arranger itself may run under; agents mustn't inherit them
/// (a claude that sees them thinks it's a child session and stops saving its transcript).
pub const PARENT_SESSION_ENV: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_PID",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
];

/// Program, args and extra env that start `agent` with arranger's hooks installed, on `model`
/// ("" leaves the agent's default; claude, codex, gemini and pi all take `--model`).
/// `resume` picks up the agent's last session in this directory (a revived task) instead of a new one;
/// for codex it carries the task's session id ("" starts a new one), since its `--last` also
/// matches sessions from the main checkout and sibling worktrees.
pub fn launch(agent: &str, prompt: &str, model: &str, resume: Option<&str>, exe: &Path, data: &Path) -> io::Result<(Vec<String>, Vec<(String, String)>)> {
    let hook = |name: &str| format!("'{}' hook {name}", exe.display());
    let mut argv = vec![agent.to_string()];
    if let Some(session) = resume {
        // ponytail: gemini's and pi's flags are from their docs, untested here
        let args: &[&str] = match kind(agent) {
            "claude" | "pi" => &["--continue"],
            "codex" if !session.is_empty() => &["resume", session],
            "gemini" => &["--resume", "latest"],
            _ => &[],
        };
        argv.extend(args.iter().map(|a| a.to_string()));
    }
    let mut env = vec![("ARRANGER_BIN".to_string(), exe.display().to_string())];
    match kind(agent) {
        "claude" => {
            let h = |timeout: u64| json!([{"hooks": [{"type": "command", "command": hook("claude"), "timeout": timeout}]}]);
            let settings = json!({"hooks": {
                "UserPromptSubmit": h(10), "PreToolUse": h(10), "PostToolUse": h(10),
                "PermissionRequest": h(ASK_TIMEOUT), "Notification": h(10),
                "Stop": h(30), "StopFailure": h(10),
            }});
            argv.extend(["--settings".into(), settings.to_string()]);
        }
        "codex" => {
            argv.push("--dangerously-bypass-hook-trust".into());
            let command = serde_json::to_string(&hook("codex")).unwrap(); // a valid TOML basic string
            for (event, timeout) in
                [("UserPromptSubmit", 10), ("PreToolUse", 10), ("PostToolUse", 10), ("PermissionRequest", ASK_TIMEOUT), ("Stop", 30)]
            {
                argv.push("-c".into());
                argv.push(format!("hooks.{event}=[{{hooks=[{{type=\"command\",command={command},timeout={timeout}}}]}}]"));
            }
        }
        "gemini" => {
            // ponytail: untested, gemini isn't installed here; written from its hooks docs
            let h = json!([{"hooks": [{"type": "command", "command": hook("gemini"), "timeout": 10_000}]}]);
            let settings = json!({"hooksConfig": {"enabled": true}, "hooks": {
                "BeforeAgent": h, "BeforeTool": h, "AfterTool": h, "AfterAgent": h, "Notification": h,
            }});
            let path = data.join("adapters/gemini.json");
            fs::write(&path, settings.to_string())?;
            env.push(("GEMINI_CLI_SYSTEM_SETTINGS_PATH".into(), path.display().to_string()));
        }
        "pi" => {
            let path = data.join("adapters/pi.ts");
            fs::write(&path, PI_EXTENSION)?;
            argv.extend(["-e".into(), path.display().to_string()]);
        }
        _ => {}
    }
    if !model.is_empty() {
        argv.extend(["--model".into(), model.into()]);
    }
    // A prompt starting with `-` must not be read as a flag.
    if !prompt.is_empty() {
        match kind(agent) {
            "gemini" => argv.push(format!("--prompt-interactive={prompt}")), // its positional prompt runs non-interactively
            "pi" => argv.push(prompt.into()), // ponytail: whether pi takes `--` is untested
            _ => argv.extend(["--".into(), prompt.into()]),
        }
    }
    Ok((argv, env))
}

/// Where the hook keeps task `task`'s codex session id, for `launch` to resume it.
pub fn codex_session(data: &Path, task: u64) -> std::path::PathBuf {
    data.join(format!("adapters/codex-session-{task}"))
}

/// Adapter name from the agent command, e.g. `/usr/local/bin/claude` -> "claude".
pub fn kind(agent: &str) -> &str {
    Path::new(agent).file_name().and_then(|n| n.to_str()).unwrap_or(agent)
}

/// State a long-lived hook process keeps between events (pi sends per-message usage).
#[derive(Default)]
pub struct Totals {
    tokens: u64,
    cost: f64,
}

/// Maps one hook payload to events. `ask` is true when the agent waits for our decision.
pub fn parse(adapter: &str, v: &Value, totals: &mut Totals) -> (Vec<Event>, bool) {
    if adapter == "pi" {
        return (parse_pi(v, totals), false);
    }
    let cwd = v["cwd"].as_str().unwrap_or("");
    let name = v["hook_event_name"].as_str().unwrap_or("");
    let tool = v["tool_name"].as_str().unwrap_or("");
    let events = match name {
        "UserPromptSubmit" | "BeforeAgent" => vec![Event::Prompt { text: str(&v["prompt"]) }],
        "PreToolUse" | "BeforeTool" => tool_start(tool, &v["tool_input"], cwd),
        "PostToolUse" | "AfterTool" => vec![Event::ToolEnd { ok: !failed(&v["tool_response"]) }],
        "PermissionRequest" => {
            let detail = tool_start(tool, &v["tool_input"], cwd).first().map(Event::describe).unwrap_or_default();
            vec![Event::PermissionRequest { tool: tool.into(), detail }]
        }
        "Notification" => match v["notification_type"].as_str().unwrap_or("") {
            "elicitation_dialog" | "elicitation_url_dialog" | "agent_needs_input" => {
                vec![Event::Question { text: str(&v["message"]), options: Vec::new() }]
            }
            "ToolPermission" => vec![Event::PermissionRequest { tool: "tool".into(), detail: str(&v["message"]) }],
            _ => vec![],
        },
        "Stop" | "AfterAgent" => {
            let mut events: Vec<Event> = v["transcript_path"].as_str().and_then(transcript_usage).into_iter().collect();
            let summary = [&v["last_assistant_message"], &v["prompt_response"]].into_iter().map(str).find(|s| !s.is_empty());
            events.extend(summary.map(|text| Event::Summary { text }));
            events.push(Event::TurnDone);
            events
        }
        "StopFailure" => {
            let message = ["error", "error_type", "reason", "message"].iter().map(|k| str(&v[*k])).find(|s| !s.is_empty());
            vec![Event::Error { message: message.unwrap_or_else(|| "agent stopped on an error".into()) }]
        }
        _ => vec![],
    };
    (events, name == "PermissionRequest")
}

/// The hook's stdout for a decision, in claude/codex `PermissionRequest` form.
pub fn decision(allow: bool, message: &str) -> String {
    let decision = if allow { json!({"behavior": "allow"}) } else { json!({"behavior": "deny", "message": message}) };
    json!({"hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": decision}}).to_string()
}

fn parse_pi(v: &Value, totals: &mut Totals) -> Vec<Event> {
    match v["type"].as_str().unwrap_or("") {
        "prompt" => vec![Event::Prompt { text: str(&v["prompt"]) }],
        "tool_start" => tool_start(v["tool"].as_str().unwrap_or(""), &v["args"], &cwd()),
        "tool_end" => vec![Event::ToolEnd { ok: !v["error"].as_bool().unwrap_or(false) }],
        "usage" => {
            totals.tokens += v["usage"]["totalTokens"].as_u64().unwrap_or(0);
            totals.cost += v["usage"]["cost"]["total"].as_f64().unwrap_or(0.0);
            vec![Event::Usage { tokens: totals.tokens, cost: Some(totals.cost) }]
        }
        "done" => vec![Event::TurnDone],
        _ => vec![],
    }
}

/// Tool names across claude, codex, gemini and pi.
pub(crate) fn tool_start(tool: &str, input: &Value, cwd: &str) -> Vec<Event> {
    let path = || {
        let p = ["file_path", "notebook_path", "path"].iter().map(|k| str(&input[*k])).find(|s| !s.is_empty());
        relative(&p.unwrap_or_default(), cwd)
    };
    let (kind, target) = match tool {
        "AskUserQuestion" => {
            let q = &input["questions"][0];
            let options = q["options"].as_array().into_iter().flatten().map(|o| str(&o["label"])).collect();
            return vec![Event::Question { text: str(&q["question"]), options }];
        }
        "apply_patch" => {
            let files: Vec<Event> = patch_files(&str(&input["command"]))
                .map(|f| Event::ToolStart { kind: ToolKind::Edit, target: relative(f, cwd) })
                .collect();
            if !files.is_empty() {
                return files;
            }
            (ToolKind::Edit, "patch".into())
        }
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" | "write_file" | "replace" | "edit" | "write" => {
            (ToolKind::Edit, path())
        }
        "Read" | "read_file" | "read" | "ls" | "list_directory" => (ToolKind::Read, path()),
        "Bash" | "run_shell_command" | "bash" | "shell" | "exec_command" => (ToolKind::Bash, str(&input["command"])),
        "Grep" | "Glob" | "glob" | "grep" | "find" | "search_file_content" => (ToolKind::Search, str(&input["pattern"])),
        "WebFetch" | "web_fetch" => {
            let url = str(&input["url"]);
            (ToolKind::Web, if url.is_empty() { str(&input["prompt"]) } else { url })
        }
        "WebSearch" | "google_web_search" => (ToolKind::Web, str(&input["query"])),
        _ => (ToolKind::Other, tool.to_string()),
    };
    vec![Event::ToolStart { kind, target }]
}

/// Strings as-is, arrays (codex argv-style commands) joined with spaces.
fn str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().map(str).collect::<Vec<_>>().join(" "),
        _ => String::new(),
    }
}

fn failed(response: &Value) -> bool {
    response["is_error"].as_bool() == Some(true) || response["success"].as_bool() == Some(false)
}

fn cwd() -> String {
    std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_default()
}

fn relative(path: &str, cwd: &str) -> String {
    match path.strip_prefix(cwd) {
        Some(rest) if !cwd.is_empty() && rest.starts_with('/') => rest[1..].to_string(),
        _ => path.to_string(),
    }
}

/// Files named in an apply_patch body.
fn patch_files(patch: &str) -> impl Iterator<Item = &str> {
    patch.lines().filter_map(|l| {
        ["*** Update File: ", "*** Add File: ", "*** Delete File: "].iter().find_map(|p| l.strip_prefix(p)).map(str::trim)
    })
}

/// Session token total from a claude or codex transcript (JSONL).
fn transcript_usage(path: &str) -> Option<Event> {
    let file = fs::File::open(path).ok()?;
    let mut claude = std::collections::HashMap::new(); // message id -> tokens; messages repeat per content block
    let mut codex = None;
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        let u = &v["message"]["usage"];
        if v["type"] == "assistant" && u.is_object() {
            let tokens = ["input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens", "output_tokens"]
                .iter()
                .filter_map(|k| u[*k].as_u64())
                .sum::<u64>();
            claude.insert(str(&v["message"]["id"]), tokens);
        } else if v["payload"]["type"] == "token_count" {
            codex = v["payload"]["info"]["total_token_usage"]["total_tokens"].as_u64().or(codex);
        }
    }
    let tokens = codex.or((!claude.is_empty()).then(|| claude.values().sum()))?;
    Some(Event::Usage { tokens, cost: None })
}

/// Finds OSC 9 / OSC 777 notifications in a PTY byte stream, across read boundaries.
#[derive(Default)]
pub struct Osc {
    state: u8, // 0 ground, 1 ESC, 2 in OSC body, 3 ESC inside body
    body: Vec<u8>,
    /// Also watch the window title (OSC 0/2): Claude Code names the session there in a few words.
    pub titles: bool,
    last_title: String,
    /// A new session name, once it changes.
    pub title: Option<String>,
}

impl Osc {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut found = Vec::new();
        for &b in bytes {
            self.state = match (self.state, b) {
                (0 | 1, 0x1b) => 1,
                (1, b']') => {
                    self.body.clear();
                    2
                }
                (2, 0x07) | (3, b'\\') => {
                    found.extend(notification(&self.body));
                    if self.titles {
                        self.window_title();
                    }
                    0
                }
                (2, 0x1b) => 3,
                (2, _) => {
                    if self.body.len() < 4096 {
                        self.body.push(b);
                    }
                    2
                }
                _ => 0,
            };
        }
        found
    }
}

impl Osc {
    pub fn named(titles: bool) -> Osc {
        Osc { titles, ..Default::default() }
    }

    fn window_title(&mut self) {
        let Some(rest) = self.body.strip_prefix(b"0;").or_else(|| self.body.strip_prefix(b"2;")) else { return };
        // Claude animates a spinner glyph in front of the name.
        let name = String::from_utf8_lossy(rest).trim_start_matches(|c: char| !c.is_alphanumeric()).trim().to_string();
        if name.is_empty() || name == "Claude Code" || name == self.last_title {
            return;
        }
        self.last_title = name.clone();
        self.title = Some(name);
    }
}

fn notification(body: &[u8]) -> Option<String> {
    let text = if let Some(rest) = body.strip_prefix(b"9;") {
        if rest.starts_with(b"4;") {
            return None; // OSC 9;4 is a progress bar, not a notification
        }
        String::from_utf8_lossy(rest).into_owned()
    } else {
        String::from_utf8_lossy(body.strip_prefix(b"777;notify;")?).replace(';', ": ")
    };
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc_notifications() {
        let mut osc = Osc::default();
        assert_eq!(osc.feed(b"hi\x1b]9;Need input\x07 \x1b]0;title\x07"), ["Need input"]);
        assert_eq!(osc.feed(b"\x1b]777;notify;Codex;Appr"), Vec::<String>::new());
        assert_eq!(osc.feed(b"ove?\x1b\\\x1b]9;4;1;50\x07"), ["Codex: Approve?"]);
        assert_eq!(osc.title, None);
    }

    #[test]
    fn osc_titles() {
        let mut osc = Osc { titles: true, ..Default::default() };
        osc.feed("\x1b]0;Claude Code\x07\x1b]0;✳ Machine Stats\x07".as_bytes());
        assert_eq!(osc.title.take().as_deref(), Some("Machine Stats"));
        osc.feed("\x1b]0;⠂ Machine Stats\x07".as_bytes());
        assert_eq!(osc.title, None); // only the spinner moved
    }

    #[test]
    fn resume_args() {
        let (exe, data) = (Path::new("/bin/arranger"), Path::new("/tmp"));
        let (argv, _) = launch("claude", "fix it", "", Some(""), exe, data).unwrap();
        assert_eq!((argv[1].as_str(), argv.last().unwrap().as_str()), ("--continue", "fix it"));
        let (argv, _) = launch("codex", "", "", Some("01a1"), exe, data).unwrap();
        assert_eq!(&argv[1..3], ["resume", "01a1"]);
        let (argv, _) = launch("codex", "", "", Some(""), exe, data).unwrap();
        assert_ne!(argv[1], "resume"); // no session of its own: a new one, never another worktree's
        assert!(!launch("claude", "", "", None, exe, data).unwrap().0.contains(&"--continue".to_string()));
    }

    #[test]
    fn claude_hooks() {
        let mut t = Totals::default();
        let v = json!({"hook_event_name": "PreToolUse", "cwd": "/w", "tool_name": "Edit", "tool_input": {"file_path": "/w/src/a.rs"}});
        assert_eq!(parse("claude", &v, &mut t).0, [Event::ToolStart { kind: ToolKind::Edit, target: "src/a.rs".into() }]);
        let v = json!({"hook_event_name": "PermissionRequest", "tool_name": "Bash", "tool_input": {"command": "rm -rf x"}});
        let (events, ask) = parse("claude", &v, &mut t);
        assert!(ask);
        assert_eq!(events, [Event::PermissionRequest { tool: "Bash".into(), detail: "running rm -rf x".into() }]);
        let patch = "*** Begin Patch\n*** Update File: /w/a.rs\n@@\n*** Add File: b.rs\n*** End Patch";
        let v = json!({"hook_event_name": "PreToolUse", "cwd": "/w", "tool_name": "apply_patch", "tool_input": {"command": patch}});
        assert_eq!(parse("codex", &v, &mut t).0.len(), 2);
    }

    #[test]
    fn launch_model() {
        let (exe, data) = (Path::new("/bin/arranger"), Path::new("/tmp"));
        let (argv, _) = launch("claude", "hi", "claude-sonnet-5-5", None, exe, data).unwrap();
        assert_eq!(argv[argv.len() - 4..], ["--model", "claude-sonnet-5-5", "--", "hi"]);
        let (argv, _) = launch("claude", "", "", None, exe, data).unwrap();
        assert!(!argv.contains(&"--model".to_string()));
    }

    #[test]
    fn dash_prompt_is_not_a_flag() {
        let (exe, data) = (Path::new("/bin/arranger"), Path::new("/tmp"));
        let (argv, _) = launch("codex", "- fix it", "", None, exe, data).unwrap();
        assert_eq!(argv[argv.len() - 2..], ["--", "- fix it"]);
        let (argv, _) = launch("claude", "-x", "", Some(""), exe, data).unwrap();
        assert_eq!(argv[argv.len() - 2..], ["--", "-x"]);
    }

    #[test]
    fn questions_stops_and_failures() {
        let mut t = Totals::default();
        let q = json!({"hook_event_name": "PreToolUse", "tool_name": "AskUserQuestion", "tool_input": {"questions": [
            {"question": "Which db?", "options": [{"label": "sqlite"}, {"label": "postgres"}]}]}});
        assert_eq!(parse("claude", &q, &mut t).0, [Event::Question { text: "Which db?".into(), options: vec!["sqlite".into(), "postgres".into()] }]);
        let n = json!({"hook_event_name": "Notification", "notification_type": "agent_needs_input", "message": "waiting"});
        assert_eq!(parse("claude", &n, &mut t).0, [Event::Question { text: "waiting".into(), options: vec![] }]);
        let n = json!({"hook_event_name": "Notification", "notification_type": "idle"});
        assert!(parse("claude", &n, &mut t).0.is_empty());
        let stop = json!({"hook_event_name": "Stop", "last_assistant_message": "Done."});
        assert_eq!(parse("codex", &stop, &mut t), (vec![Event::Summary { text: "Done.".into() }, Event::TurnDone], false));
        let fail = json!({"hook_event_name": "StopFailure", "error_type": "rate_limit"});
        assert_eq!(parse("claude", &fail, &mut t).0, [Event::Error { message: "rate_limit".into() }]);
        let end = json!({"hook_event_name": "PostToolUse", "tool_response": {"success": false}});
        assert_eq!(parse("claude", &end, &mut t).0, [Event::ToolEnd { ok: false }]);
    }

    #[test]
    fn gemini_and_pi_names() {
        let mut t = Totals::default();
        let v = json!({"hook_event_name": "BeforeTool", "cwd": "/w", "tool_name": "run_shell_command", "tool_input": {"command": "ls"}});
        assert_eq!(parse("gemini", &v, &mut t).0, [Event::ToolStart { kind: ToolKind::Bash, target: "ls".into() }]);
        let v = json!({"hook_event_name": "AfterAgent", "prompt_response": "ok"});
        assert_eq!(parse("gemini", &v, &mut t).0, [Event::Summary { text: "ok".into() }, Event::TurnDone]);
        // codex sends argv-style commands
        let v = json!({"hook_event_name": "PreToolUse", "tool_name": "shell", "tool_input": {"command": ["git", "status"]}});
        assert_eq!(parse("codex", &v, &mut t).0, [Event::ToolStart { kind: ToolKind::Bash, target: "git status".into() }]);
        let usage = json!({"type": "usage", "usage": {"totalTokens": 100, "cost": {"total": 0.5}}});
        parse("pi", &usage, &mut t);
        assert_eq!(parse("pi", &usage, &mut t).0, [Event::Usage { tokens: 200, cost: Some(1.0) }]); // pi's usage adds up
        assert_eq!(parse("pi", &json!({"type": "done"}), &mut t).0, [Event::TurnDone]);
    }

    #[test]
    fn transcript_tokens() {
        let path = std::env::temp_dir().join(format!("arranger-transcript-{}.jsonl", std::process::id()));
        let msg = |id: &str| json!({"type": "assistant", "message": {"id": id, "usage": {"input_tokens": 10, "output_tokens": 5}}}).to_string();
        // the same message repeats once per content block and counts once
        fs::write(&path, [msg("a"), msg("a"), msg("b"), "not json".into()].join("\n")).unwrap();
        assert_eq!(transcript_usage(path.to_str().unwrap()), Some(Event::Usage { tokens: 30, cost: None }));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn decisions() {
        let v: Value = serde_json::from_str(&decision(false, "no")).unwrap();
        assert_eq!(v["hookSpecificOutput"]["decision"], json!({"behavior": "deny", "message": "no"}));
        let v: Value = serde_json::from_str(&decision(true, "")).unwrap();
        assert_eq!(v["hookSpecificOutput"]["decision"]["behavior"], "allow");
    }
}

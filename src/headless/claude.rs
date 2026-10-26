//! claude in stream-json, speaking its control protocol (what the Agent SDK wraps).
//!
//! The launch flags keep it as close to interactive claude as the SDK entrypoint allows;
//! `scripts/parity.py` checks the API requests match. Never send a system prompt: leaving it out
//! is what keeps Claude Code's own.

use super::{Ask, Core, MODES, Transport, detail, error_message, string};
use crate::adapter;
use crate::proto::Event;
use serde_json::{Value, json};
use std::path::Path;

/// Arguments after the program. `resume` is the session to carry on ("" for the latest in this
/// directory); `fork` carries it on in a new session (a cloned task). `mode` is the permission mode
/// ("" uses the interactive approval mode). Project settings are not trusted in chat mode.
pub fn argv(model: &str, effort: &str, mode: &str, resume: Option<&str>, fork: bool, _cwd: &Path) -> Vec<String> {
    let mut argv: Vec<String> = [
        "--output-format", "stream-json", "--verbose", "--input-format", "stream-json",
        "--include-partial-messages", "--replay-user-messages", "--permission-prompt-tool", "stdio",
        "--setting-sources", "user", "--tools", "default",
    ]
    .map(String::from)
    .into();
    argv.extend(["--permission-mode".into(), if mode.is_empty() { "default".into() } else { mode.into() }]);
    match resume {
        Some("") => argv.push("--continue".into()),
        Some(session) => argv.extend(["--resume".into(), session.into()]),
        None => {}
    }
    if fork && resume.is_some() {
        argv.push("--fork-session".into());
    }
    if !model.is_empty() {
        argv.extend(["--model".into(), model.into()]);
    }
    if !effort.is_empty() {
        argv.extend(["--effort".into(), effort.into()]);
    }
    argv
}

pub struct Claude {
    c: Core,
    /// From its init message: the slash commands it runs headless, and those only its TUI has.
    commands: Vec<String>,
    terminal_commands: Vec<String>,
    /// Its permission mode, as it last said or we last set it.
    mode: String,
    /// A mode switch it hasn't confirmed: (request id, the mode before it).
    switching: Option<(String, String)>,
}

impl Claude {
    pub fn new(cwd: &Path, mode: &str) -> Self {
        let mut c = Claude { c: Core::new(cwd), commands: Vec::new(), terminal_commands: Vec::new(), mode: mode.into(), switching: None };
        // No systemPrompt: that would replace Claude Code's.
        c.c.send(json!({"type":"control_request","request_id":"arranger-init","request":{"subtype":"initialize"}}));
        c
    }

    fn respond(&mut self, id: &Value, response: Value) {
        self.c.send(json!({"type":"control_response","response":{"subtype":"success","request_id":id,"response":response}}));
    }

    fn start_next(&mut self) -> Vec<Event> {
        let Some(text) = self.c.next_prompt() else { return Vec::new() };
        self.c.send(json!({"type":"user","message":{"role":"user","content":text}}));
        vec![Event::Prompt { text }]
    }
}

impl Transport for Claude {
    fn core(&self) -> &Core {
        &self.c
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.c
    }

    fn prompt(&mut self, text: String) -> Vec<Event> {
        if let Some(mut ask) = self.c.asks.pop_front() {
            if ask.is_question() {
                if !ask.take(text) {
                    let need = ask.need();
                    self.c.asks.push_front(ask);
                    return vec![need];
                }
                let mut input = ask.params["input"].clone();
                input["answers"] = Value::Object(std::mem::take(&mut ask.answers));
                self.respond(&ask.id, json!({"behavior":"allow","updatedInput":input}));
                return self.c.asks.front().map(Ask::need).into_iter().collect();
            }
            self.c.asks.push_front(ask);
        }
        self.c.prompts.push_back(text);
        self.start_next()
    }

    fn answer(&mut self, allow: bool, message: &str) -> Result<(), String> {
        let Some(ask) = self.c.asks.front() else { return Err("the agent isn't waiting for an answer".into()) };
        if ask.is_question() && allow {
            return Err("it's asking a question: answer with `arranger send`".into());
        }
        let ask = self.c.asks.pop_front().unwrap();
        let response = if allow && ask.method == "ExitPlanMode" && self.mode == "plan" {
            // Approving the plan leaves plan mode, as in its terminal: back to asking before edits.
            self.mode = MODES[0].into();
            json!({"behavior":"allow","updatedInput":ask.params["input"],"updatedPermissions":[{"type":"setMode","mode":MODES[0],"destination":"session"}]})
        } else if allow {
            json!({"behavior":"allow","updatedInput":ask.params["input"]})
        } else {
            json!({"behavior":"deny","message":if message.is_empty() { "The user denied this" } else { message }})
        };
        self.respond(&ask.id, response);
        Ok(())
    }

    fn cancel(&mut self) {
        self.c.prompts.clear();
        while !self.c.asks.is_empty() {
            let ask = self.c.asks.pop_front().unwrap();
            self.respond(&ask.id, json!({"behavior":"deny","message":"The user interrupted"}));
        }
        if self.c.busy {
            let id = format!("arranger-interrupt-{}", self.c.id());
            self.c.send(json!({"type":"control_request","request_id":id,"request":{"subtype":"interrupt"}}));
        }
    }

    fn mode(&self) -> &str {
        &self.mode
    }

    fn set_mode(&mut self, mode: &str) -> Result<(), String> {
        if !MODES.contains(&mode) {
            return Err(format!("unknown mode {mode:?}: one of {}", MODES.join(", ")));
        }
        // Sent after initialize, so it's taken even before claude has answered that.
        let id = format!("arranger-mode-{}", self.c.id());
        self.c.send(json!({"type":"control_request","request_id":id,"request":{"subtype":"set_permission_mode","mode":mode}}));
        let before = std::mem::replace(&mut self.mode, mode.into());
        self.switching = Some((id, before));
        Ok(())
    }

    /// Built-in commands its TUI has but stream-json doesn't (/permissions, /rewind…) and the ones
    /// it lists as terminal-only. Unknown commands go through: claude says they don't exist.
    fn terminal(&self, text: &str) -> Option<String> {
        let name = text.strip_prefix('/')?.split_whitespace().next()?;
        let known = self.commands.iter().any(|c| c == name);
        let tui = self.terminal_commands.iter().any(|c| c == name) || (!known && TUI_COMMANDS.contains(&name));
        (!self.commands.is_empty() && tui).then(|| format!("claude {}", shell_quote(text)))
    }

    fn line(&mut self, v: Value, events: &mut Vec<Event>) -> bool {
        // Subagents' messages carry their parent's tool id; their own tools still ask through us.
        let subagent = !v["parent_tool_use_id"].is_null();
        // Its init and status messages say which mode it's in (it leaves plan mode by itself, too).
        if v["type"] == "system"
            && let Some(mode) = v["permissionMode"].as_str() {
            self.mode = mode.into();
        }
        match v["type"].as_str().unwrap_or("") {
            "system" if v["subtype"] == "init" => {
                self.c.session = string(&v["session_id"]);
                let names = |k: &str| v[k].as_array().into_iter().flatten().map(string).collect();
                self.commands = names("slash_commands");
                self.terminal_commands = names("terminal_slash_commands");
            }
            "control_response" if v["response"]["request_id"] == "arranger-init" => {
                if v["response"]["subtype"] == "success" {
                    self.c.ready = true;
                    events.extend(self.start_next());
                } else {
                    events.push(Event::Error { message: format!("claude didn't start: {}", string(&v["response"]["error"])) });
                }
            }
            "control_response" if self.switching.as_ref().is_some_and(|(id, _)| v["response"]["request_id"] == id.as_str()) => {
                // Refused: the chat shows the mode it stayed in. Not an error, which would stop the turn.
                let (_, before) = self.switching.take().unwrap();
                if v["response"]["subtype"] != "success" {
                    self.mode = before;
                }
            }
            "control_request" => {
                let (id, request) = (v["request_id"].clone(), v["request"].clone());
                if request["subtype"] == "can_use_tool" {
                    let tool = request["tool_name"].as_str().unwrap_or("tool").to_string();
                    let input = &request["input"];
                    let questions: Vec<_> = (tool == "AskUserQuestion")
                        .then(|| input["questions"].as_array())
                        .flatten()
                        .into_iter()
                        .flatten()
                        .map(|q| (string(&q["question"]), string(&q["question"]), q["options"].as_array().into_iter().flatten().map(|o| string(&o["label"])).collect()))
                        .collect();
                    let ask = if questions.is_empty() {
                        let detail = detail(&tool, input, &self.c.cwd);
                        Ask::permission(id, &tool, request, &tool, detail)
                    } else {
                        Ask::questions(id, &tool, request, questions)
                    };
                    self.c.ask(ask, events);
                } else {
                    // Nothing else is registered (hooks, MCP); say so rather than leave it hanging.
                    let error = format!("arranger doesn't handle {}", string(&request["subtype"]));
                    self.c.send(json!({"type":"control_response","response":{"subtype":"error","request_id":id,"error":error}}));
                }
            }
            "control_cancel_request" => self.c.asks.retain(|a| a.id != v["request_id"]),
            "assistant" if subagent => {}
            "assistant" if v["isApiErrorMessage"] == true || v.get("error").is_some_and(|e| !e.is_null()) => {
                let text = texts(&v["message"]["content"]);
                events.push(Event::Error { message: if text.is_empty() { error_message(&v["error"], "claude") } else { text } });
            }
            "assistant" => {
                for c in v["message"]["content"].as_array().into_iter().flatten() {
                    match c["type"].as_str().unwrap_or("") {
                        "text" => self.c.last_text = string(&c["text"]),
                        "tool_use" => events.extend(adapter::tool_start(c["name"].as_str().unwrap_or("tool"), &c["input"], &self.c.cwd)),
                        _ => {}
                    }
                }
            }
            "user" if !subagent => {
                for c in v["message"]["content"].as_array().into_iter().flatten().filter(|c| c["type"] == "tool_result") {
                    events.push(Event::ToolEnd { ok: c["is_error"] != true });
                }
            }
            "result" => {
                let error = (v["is_error"] == true && v["subtype"] != "error_during_execution").then(|| {
                    let errors = v.get("errors").filter(|e| !e.is_null()).unwrap_or(&v["result"]);
                    error_message(errors, "claude")
                });
                self.c.done(error, events);
                let u = &v["usage"];
                let keys = ["input_tokens", "output_tokens", "cache_creation_input_tokens", "cache_read_input_tokens"];
                events.push(Event::Usage { tokens: keys.iter().filter_map(|k| u[k].as_u64()).sum(), cost: v["total_cost_usd"].as_f64() });
                events.extend(self.start_next());
            }
            _ => {}
        }
        true
    }
}

/// Interactive claude's own commands that stream-json leaves out of `slash_commands`.
const TUI_COMMANDS: &[&str] = &[
    "add-dir", "bashes", "bug", "doctor", "exit", "export", "feedback", "help", "hooks", "ide", "install-github-app",
    "keybindings", "login", "logout", "memory", "model", "permissions", "plugin", "plugins", "privacy-settings", "quit", "skills",
    "release-notes", "resume", "rewind", "sandbox", "status", "statusline", "terminal-setup", "theme", "upgrade", "vim",
];

/// `s` as one sh word.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn texts(content: &Value) -> String {
    content.as_array().into_iter().flatten().filter_map(|c| c["text"].as_str()).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::super::testing::{receive, sent};
    use super::super::{Agent, start};
    use super::*;
    use crate::proto::ToolKind;

    fn new() -> Agent {
        start("claude", Path::new("/repo"), "", "", "", None, false).1
    }

    fn started() -> Agent {
        let mut c = new();
        let init = sent(&mut c);
        assert_eq!(init[0]["request"], json!({"subtype":"initialize"}), "no system prompt, ever");
        receive(&mut c, json!({"type":"system","subtype":"init","session_id":"s-1","slash_commands":["compact","context","review"],"terminal_slash_commands":["doctor"]}));
        receive(&mut c, json!({"type":"control_response","response":{"subtype":"success","request_id":"arranger-init"}}));
        assert_eq!(c.session(), "s-1");
        c
    }

    fn result() -> Value {
        json!({"type":"result","subtype":"success","is_error":false,"usage":{"input_tokens":3,"output_tokens":2},"total_cost_usd":0.01})
    }

    #[test]
    fn argv_matches_the_parity_flags() {
        let a = argv("opus", "high", "plan", None, false, Path::new("/nowhere"));
        for flag in ["--input-format", "--permission-prompt-tool", "--setting-sources", "--tools", "--include-partial-messages"] {
            assert!(a.iter().any(|x| x == flag), "{flag} missing: {a:?}");
        }
        assert!(!a.iter().any(|x| x.starts_with("--system-prompt") || x == "--print"), "{a:?}");
        assert!(a.windows(2).any(|w| w == ["--permission-mode", "plan"]));
        assert!(a.windows(2).any(|w| w == ["--model", "opus"]));
        assert!(a.windows(2).any(|w| w == ["--effort", "high"]));
        let r = argv("", "", "auto", Some("s-1"), true, Path::new("/nowhere"));
        assert!(r.windows(3).any(|w| w == ["--resume", "s-1", "--fork-session"]), "{r:?}");
        assert!(!r.contains(&"--effort".into()));
        assert!(argv("", "", "auto", Some(""), false, Path::new("/nowhere")).contains(&"--continue".into()));
    }

    #[test]
    fn prompts_wait_for_initialize_and_queue_behind_a_turn() {
        let mut c = new();
        assert!(c.prompt("first".into()).is_empty());
        sent(&mut c);
        let events = receive(&mut c, json!({"type":"control_response","response":{"subtype":"success","request_id":"arranger-init"}}));
        assert_eq!(events, vec![Event::Prompt { text: "first".into() }]);
        assert_eq!(sent(&mut c)[0]["message"]["content"], "first");
        assert!(c.prompt("second".into()).is_empty(), "held while busy");
        assert!(sent(&mut c).is_empty());
        receive(&mut c, json!({"type":"assistant","message":{"content":[{"type":"text","text":"Did it."}]}}));
        let events = receive(&mut c, result());
        assert_eq!(events[0], Event::Summary { text: "Did it.".into() });
        assert_eq!(events[1], Event::TurnDone);
        assert!(matches!(events[2], Event::Usage { tokens: 5, cost: Some(_) }));
        assert_eq!(events[3], Event::Prompt { text: "second".into() });
        assert_eq!(sent(&mut c)[0]["message"]["content"], "second");
    }

    #[test]
    fn permission_asks_are_answered_by_request_id() {
        let mut c = started();
        c.prompt("go".into());
        sent(&mut c);
        let tool = json!({"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"make test"}}]}});
        assert_eq!(receive(&mut c, tool), vec![Event::ToolStart { kind: ToolKind::Bash, target: "make test".into() }]);
        let ask = |id: &str| json!({"type":"control_request","request_id":id,"request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"make test"}}});
        assert!(matches!(&receive(&mut c, ask("r1"))[0], Event::PermissionRequest { tool, detail } if tool == "Bash" && detail == "running make test"));
        assert!(receive(&mut c, ask("r2")).is_empty(), "only the first ask is announced");
        c.answer(true, "").unwrap();
        let out = sent(&mut c);
        assert_eq!(out[0]["response"]["request_id"], "r1");
        assert_eq!(out[0]["response"]["response"]["behavior"], "allow");
        assert_eq!(out[0]["response"]["response"]["updatedInput"]["command"], "make test");
        assert!(c.asking());
        c.answer(false, "no").unwrap();
        assert_eq!(sent(&mut c)[0]["response"]["response"], json!({"behavior":"deny","message":"no"}));
        assert!(c.answer(true, "").is_err());
    }

    #[test]
    fn questions_are_answered_one_message_each() {
        let mut c = started();
        c.prompt("go".into());
        sent(&mut c);
        let questions = json!([{"question":"Which?","options":[{"label":"A"},{"label":"B"}]},{"question":"Why?","options":[]}]);
        let ask = json!({"type":"control_request","request_id":"q1","request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","input":{"questions":questions}}});
        assert_eq!(receive(&mut c, ask)[0], Event::Question { text: "Which?".into(), options: vec!["A".into(), "B".into()] });
        assert!(c.answer(true, "").is_err(), "a question isn't a permission");
        assert_eq!(c.prompt("A".into()), vec![Event::Question { text: "Why?".into(), options: vec![] }]);
        assert!(c.prompt("because".into()).is_empty());
        let out = sent(&mut c);
        assert_eq!(out[0]["response"]["request_id"], "q1");
        assert_eq!(out[0]["response"]["response"]["updatedInput"]["answers"], json!({"Which?":"A","Why?":"because"}));
        assert!(!c.asking());
    }

    #[test]
    fn cancel_denies_asks_then_interrupts() {
        let mut c = started();
        c.prompt("go".into());
        c.prompt("queued".into());
        sent(&mut c);
        receive(&mut c, json!({"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{}}}));
        c.cancel();
        let out = sent(&mut c);
        assert_eq!(out[0]["response"]["response"]["behavior"], "deny");
        assert_eq!(out[1]["request"]["subtype"], "interrupt");
        let events = receive(&mut c, json!({"type":"result","subtype":"error_during_execution","is_error":true,"usage":{}}));
        assert!(events.contains(&Event::TurnDone), "an interrupted turn isn't an error: {events:?}");
        assert!(sent(&mut c).is_empty(), "queued prompts were dropped");
    }

    #[test]
    fn errors_and_unknown_requests_surface() {
        let mut c = started();
        c.prompt("go".into());
        sent(&mut c);
        receive(&mut c, json!({"type":"control_request","request_id":"x","request":{"subtype":"mcp_message"}}));
        assert_eq!(sent(&mut c)[0]["response"]["subtype"], "error");
        let events = receive(&mut c, json!({"type":"result","subtype":"error_max_turns","is_error":true,"errors":["Reached max turns"],"usage":{}}));
        assert!(events.contains(&Event::Error { message: "Reached max turns".into() }), "{events:?}");
        assert!(matches!(&c.feed(b"not json\n").0[0], Event::Error { .. }));
        let api = json!({"type":"assistant","isApiErrorMessage":true,"message":{"content":[{"type":"text","text":"Credit balance is too low"}]}});
        assert_eq!(receive(&mut c, api), vec![Event::Error { message: "Credit balance is too low".into() }]);
    }

    #[test]
    fn subagent_messages_dont_end_the_turn_or_summarize() {
        let mut c = started();
        c.prompt("go".into());
        sent(&mut c);
        receive(&mut c, json!({"type":"assistant","message":{"content":[{"type":"text","text":"Main."}]}}));
        let sub = json!({"type":"assistant","parent_tool_use_id":"t1","message":{"content":[{"type":"text","text":"Sub."},{"type":"tool_use","name":"Read","input":{"file_path":"/repo/a.rs"}}]}});
        assert!(receive(&mut c, sub).is_empty());
        assert_eq!(receive(&mut c, result())[0], Event::Summary { text: "Main.".into() });
    }

    #[test]
    fn modes_are_switched_reported_and_left_with_the_plan() {
        let mut c = started();
        assert_eq!(c.mode(), "default");
        receive(&mut c, json!({"type":"system","subtype":"status","permissionMode":"acceptEdits"}));
        assert_eq!(c.mode(), "acceptEdits", "its own reports win");
        assert!(c.set_mode("bypassPermissions").is_err());
        c.set_mode("plan").unwrap();
        let out = sent(&mut c);
        assert_eq!(out[0]["request"], json!({"subtype":"set_permission_mode","mode":"plan"}));
        assert_eq!(c.mode(), "plan");
        let id = out[0]["request_id"].clone();
        receive(&mut c, json!({"type":"control_response","response":{"subtype":"success","request_id":id}}));
        assert_eq!(c.mode(), "plan");
        c.set_mode("default").unwrap();
        let id = sent(&mut c)[0]["request_id"].clone();
        let events = receive(&mut c, json!({"type":"control_response","response":{"subtype":"error","request_id":id,"error":"no"}}));
        assert_eq!((c.mode(), events), ("plan", vec![]), "a refused switch keeps the old mode");
        c.prompt("go".into());
        sent(&mut c);
        receive(&mut c, json!({"type":"control_request","request_id":"p1","request":{"subtype":"can_use_tool","tool_name":"ExitPlanMode","input":{"plan":"x"}}}));
        c.answer(true, "").unwrap();
        let out = sent(&mut c);
        assert_eq!(out[0]["response"]["response"]["updatedPermissions"][0]["mode"], "default");
        assert_eq!(c.mode(), "default");
        c.set_mode("auto").unwrap();
        let out = sent(&mut c);
        assert_eq!(out[0]["request"], json!({"subtype":"set_permission_mode","mode":"auto"}));
        let id = out[0]["request_id"].clone();
        receive(&mut c, json!({"type":"control_response","response":{"subtype":"error","request_id":id,"error":"auto mode isn't enabled"}}));
        assert_eq!(c.mode(), "default", "an account without auto mode stays where it was");
        let cycle: Vec<_> = std::iter::successors(Some("default"), |m| Some(crate::headless::next_mode(m))).take(5).collect();
        assert_eq!(cycle, ["default", "acceptEdits", "plan", "auto", "default"]);
    }

    #[test]
    fn terminal_only_commands_open_its_terminal() {
        let c = started();
        assert_eq!(c.terminal("/permissions"), Some("claude '/permissions'".into()));
        assert_eq!(c.terminal("/doctor now"), Some("claude '/doctor now'".into()));
        assert_eq!(c.terminal("/compact keep the plan"), None);
        assert_eq!(c.terminal("/review"), None, "its own commands and skills run headless");
        assert_eq!(c.terminal("/nonsense"), None, "claude says it doesn't know it");
        assert_eq!(c.terminal("fix /permissions"), None);
        assert_eq!(new().terminal("/permissions"), None, "not before it has said what it has");
    }
}

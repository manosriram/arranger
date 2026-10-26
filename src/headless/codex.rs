//! codex over `codex app-server`: JSON-RPC on stdio, what its IDE extension speaks. A thread is
//! its session; each prompt is a turn.
//!
//! Nothing overrides its config (sandbox, approval policy, model), so it runs as `codex` would in
//! the same directory; `scripts/parity.py --agent codex` checks the requests match. What still
//! differs comes from the client's name: codex's own TUI (also an app-server client, `codex-tui`)
//! gets a `cyber` access program and no plugin-install suggestions; other clients get the reverse.

use super::{Ask, Core, Transport, error_message, string};
use crate::proto::{Event, ToolKind};
use serde_json::{Value, json};
use std::path::Path;

const INITIALIZE: u64 = 0;
const THREAD: u64 = 1;

pub fn argv() -> Vec<String> {
    vec!["app-server".into()]
}

pub struct Codex {
    c: Core,
    model: String,
    effort: String,
    /// The thread to resume or fork from; the one it's on is `c.session`.
    from: String,
    fork: bool,
    /// The running turn, for interrupting it.
    turn: String,
    /// Interrupted before its turn had an id: interrupt it as soon as it has one.
    cancelled: bool,
}

impl Codex {
    pub fn new(cwd: &Path, model: &str, effort: &str, session: Option<&str>, fork: bool) -> Self {
        let mut c = Codex {
            c: Core::new(cwd),
            model: model.into(),
            effort: effort.into(),
            from: session.unwrap_or_default().into(),
            fork,
            turn: String::new(),
            cancelled: false,
        };
        let info = json!({"name":"arranger","title":"Arranger","version":env!("CARGO_PKG_VERSION")});
        c.c.send(json!({"id":INITIALIZE,"method":"initialize","params":{"clientInfo":info}}));
        c
    }

    fn start_next(&mut self) -> Vec<Event> {
        let Some(text) = self.c.next_prompt() else { return Vec::new() };
        self.turn.clear();
        self.cancelled = false;
        let mut params = json!({"threadId":self.c.session,"input":[{"type":"text","text":text,"text_elements":[]}],"turnTrigger":"user"});
        if !self.effort.is_empty() {
            params["effort"] = json!(self.effort);
        }
        let id = self.c.id();
        self.c.send(json!({"id":id,"method":"turn/start","params":params}));
        vec![Event::Prompt { text }]
    }

    fn interrupt(&mut self) {
        let id = self.c.id();
        self.c.send(json!({"id":id,"method":"turn/interrupt","params":{"threadId":self.c.session,"turnId":self.turn}}));
    }

    fn respond(&mut self, id: &Value, result: Value) {
        self.c.send(json!({"id":id,"result":result}));
    }

    /// A request from codex: an ask to put to the user, or one arranger turns down.
    fn request(&mut self, id: Value, method: &str, p: Value, events: &mut Vec<Event>) {
        let ask = match method {
            "item/commandExecution/requestApproval" => {
                let detail = format!("run {}", string(&p["command"]));
                Ask::permission(id, method, p, "Bash", detail)
            }
            "item/fileChange/requestApproval" => {
                let reason = string(&p["reason"]);
                Ask::permission(id, method, p, "Edit", if reason.is_empty() { "edit files".into() } else { reason })
            }
            "item/permissions/requestApproval" => {
                let reason = string(&p["reason"]);
                Ask::permission(id, method, p, "Permissions", if reason.is_empty() { "more access".into() } else { reason })
            }
            "item/tool/requestUserInput" => {
                let questions = p["questions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|q| (string(&q["id"]), string(&q["question"]), q["options"].as_array().into_iter().flatten().map(|o| string(&o["label"])).collect()))
                    .collect();
                Ask::questions(id, method, p, questions)
            }
            "mcpServer/elicitation/request" => {
                self.respond(&id, json!({"action":"decline","content":null,"_meta":null}));
                events.push(Event::Error { message: "an MCP server asked for input arranger can't show; declined".into() });
                return;
            }
            _ => {
                self.c.send(json!({"id":id,"error":{"code":-32601,"message":format!("arranger doesn't handle {method}")}}));
                return;
            }
        };
        self.c.ask(ask, events);
    }

    /// What a declined or accepted ask sends back.
    fn reply(&mut self, ask: &Ask, allow: bool) {
        let result = if ask.method == "item/permissions/requestApproval" {
            json!({"permissions":if allow { ask.params["permissions"].clone() } else { json!({}) },"scope":"turn"})
        } else {
            json!({"decision":if allow { "accept" } else { "decline" }})
        };
        self.respond(&ask.id, result);
    }

    fn item(&mut self, item: &Value, started: bool, events: &mut Vec<Event>) {
        let cwd = self.c.cwd.clone();
        let relative = |p: &str| p.strip_prefix(&cwd).map(|r| r.trim_start_matches('/')).unwrap_or(p).to_string();
        match (item["type"].as_str().unwrap_or(""), started) {
            ("agentMessage", false) => {
                let text = string(&item["text"]);
                if !text.is_empty() {
                    self.c.last_text = text;
                }
            }
            ("commandExecution", true) => events.push(Event::ToolStart { kind: ToolKind::Bash, target: command(item) }),
            ("fileChange", true) => {
                for change in item["changes"].as_array().into_iter().flatten() {
                    events.push(Event::ToolStart { kind: ToolKind::Edit, target: relative(&string(&change["path"])) });
                }
            }
            ("mcpToolCall" | "dynamicToolCall", true) => events.push(Event::ToolStart { kind: ToolKind::Other, target: string(&item["tool"]) }),
            ("webSearch", true) => events.push(Event::ToolStart { kind: ToolKind::Web, target: string(&item["query"]) }),
            ("commandExecution" | "fileChange" | "mcpToolCall" | "dynamicToolCall" | "webSearch", false) => {
                events.push(Event::ToolEnd { ok: !matches!(item["status"].as_str(), Some("failed" | "declined")) });
            }
            _ => {}
        }
    }
}

/// The command a commandExecution item runs, without the `zsh -lc` it's wrapped in.
pub(crate) fn command(item: &Value) -> String {
    let actions: Vec<String> = item["commandActions"].as_array().into_iter().flatten().map(|a| string(&a["command"])).filter(|c| !c.is_empty()).collect();
    if actions.is_empty() { string(&item["command"]) } else { actions.join(" && ") }
}

/// Lines a chat is built from; the rest (rate limits, MCP startup, output deltas the completed
/// item repeats) stay out of the log.
fn chat_line(method: &str) -> bool {
    matches!(
        method,
        "item/started" | "item/completed" | "item/agentMessage/delta" | "item/reasoning/summaryTextDelta" | "item/reasoning/summaryPartAdded"
            | "turn/started" | "turn/completed" | "turn/plan/updated" | "error" | "warning" | "thread/compacted"
    )
}

impl Transport for Codex {
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
                let answers: serde_json::Map<String, Value> = ask.answers.iter().map(|(k, v)| (k.clone(), json!({"answers":[v]}))).collect();
                self.respond(&ask.id, json!({"answers":answers}));
                return self.c.asks.front().map(Ask::need).into_iter().collect();
            }
            self.c.asks.push_front(ask);
        }
        self.c.prompts.push_back(text);
        self.start_next()
    }

    fn answer(&mut self, allow: bool, _message: &str) -> Result<(), String> {
        let Some(ask) = self.c.asks.front() else { return Err("the agent isn't waiting for an answer".into()) };
        if ask.is_question() {
            return Err("it's asking a question: answer with `arranger send`".into());
        }
        let ask = self.c.asks.pop_front().unwrap();
        self.reply(&ask, allow);
        Ok(())
    }

    fn cancel(&mut self) {
        self.c.prompts.clear();
        while let Some(ask) = self.c.asks.pop_front() {
            if ask.is_question() {
                self.respond(&ask.id, json!({"answers":{}}));
            } else {
                self.reply(&ask, false);
            }
        }
        if self.c.busy {
            if self.turn.is_empty() {
                self.cancelled = true;
            } else {
                self.interrupt();
            }
        }
    }

    fn line(&mut self, v: Value, events: &mut Vec<Event>) -> bool {
        let method = v["method"].as_str().unwrap_or("").to_string();
        let p = &v["params"];
        // Its subagents run in threads of their own; their asks still come to us.
        let other = p["threadId"].is_string() && !self.c.session.is_empty() && p["threadId"] != self.c.session.as_str();
        if other && v.get("id").is_none() {
            return false;
        }
        if method.is_empty() {
            // A reply to one of ours.
            if let Some(error) = v.get("error") {
                let message = error_message(error, "codex");
                if v["id"] == THREAD || v["id"] == INITIALIZE {
                    events.push(Event::Error { message: format!("codex didn't start: {message}") });
                } else if self.c.busy && self.turn.is_empty() {
                    self.c.done(Some(message), events); // turn/start failed
                } else {
                    events.push(Event::Error { message });
                }
                return true;
            }
            if v["id"] == INITIALIZE {
                self.c.send(json!({"method":"initialized","params":{}}));
                let mut params = json!({"cwd":self.c.cwd,"threadSource":"user"});
                if !self.model.is_empty() {
                    params["model"] = json!(self.model);
                }
                let method = match (self.from.as_str(), self.fork) {
                    ("", _) => "thread/start",
                    (_, true) => "thread/fork",
                    (_, false) => "thread/resume",
                };
                if method != "thread/start" {
                    params["threadId"] = json!(self.from);
                }
                self.c.send(json!({"id":THREAD,"method":method,"params":params}));
            } else if v["id"] == THREAD {
                self.c.session = string(&v["result"]["thread"]["id"]);
                self.c.ready = !self.c.session.is_empty();
                events.extend(self.start_next());
            } else if let Some(turn) = v["result"]["turn"]["id"].as_str().filter(|_| self.c.busy && self.turn.is_empty()) {
                self.turn = turn.into();
                if self.cancelled {
                    self.interrupt();
                }
            }
            return false;
        }
        if let Some(id) = v.get("id").cloned() {
            self.request(id, &method, p.clone(), events);
            return false;
        }
        match method.as_str() {
            "turn/started" if self.turn.is_empty() => {
                self.turn = string(&p["turn"]["id"]);
                if self.cancelled {
                    self.interrupt();
                }
            }
            "serverRequest/resolved" => self.c.asks.retain(|a| a.id != p["requestId"]),
            "item/started" | "item/completed" => self.item(&p["item"], method == "item/started", events),
            "turn/completed" => {
                let turn = &p["turn"];
                let error = (turn["status"] == "failed").then(|| error_message(&turn["error"], "codex"));
                if turn["status"] == "interrupted" {
                    self.c.last_text.clear(); // what it was about to do isn't a summary
                }
                self.turn.clear();
                self.c.done(error, events);
                events.extend(self.start_next());
            }
            "error" if p["willRetry"] != true => events.push(Event::Error { message: error_message(&p["error"], "codex") }),
            "thread/tokenUsage/updated" => {
                if let Some(tokens) = p["tokenUsage"]["total"]["totalTokens"].as_u64() {
                    events.push(Event::Usage { tokens, cost: None });
                }
            }
            _ => {}
        }
        chat_line(&method)
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{receive, sent};
    use super::super::{Agent, start};
    use super::*;

    fn started(session: Option<&str>, fork: bool) -> (Agent, Vec<Value>) {
        let (argv, mut a) = start("/opt/bin/codex", Path::new("/repo"), "gpt-6", "high", "", session, fork);
        assert_eq!(argv, ["/opt/bin/codex", "app-server"]);
        assert_eq!(sent(&mut a)[0]["method"], "initialize");
        receive(&mut a, json!({"id":0,"result":{"userAgent":"codex"}}));
        let out = sent(&mut a);
        assert_eq!(out[0]["method"], "initialized");
        receive(&mut a, json!({"id":1,"result":{"thread":{"id":"th-2"}}}));
        assert_eq!(a.session(), "th-2");
        (a, out)
    }

    #[test]
    fn threads_start_resume_or_fork() {
        let (_, out) = started(None, false);
        assert_eq!(out[1]["method"], "thread/start");
        assert_eq!(out[1]["params"], json!({"cwd":"/repo","threadSource":"user","model":"gpt-6"}), "nothing else overridden");
        let (_, out) = started(Some("th-1"), false);
        assert_eq!((out[1]["method"].as_str(), out[1]["params"]["threadId"].as_str()), (Some("thread/resume"), Some("th-1")));
        let (_, out) = started(Some("th-1"), true);
        assert_eq!(out[1]["method"], "thread/fork");
        let (_, out) = started(Some(""), false);
        assert_eq!(out[1]["method"], "thread/start", "no thread saved: a new one");
    }

    #[test]
    fn turns_queue_summarize_and_count_tokens() {
        let (mut a, _) = started(None, false);
        assert_eq!(a.prompt("first".into()), vec![Event::Prompt { text: "first".into() }]);
        let turn = &sent(&mut a)[0];
        assert_eq!(turn["method"], "turn/start");
        assert_eq!(turn["params"]["input"][0]["text"], "first");
        assert_eq!(turn["params"]["effort"], "high");
        assert!(a.prompt("second".into()).is_empty());
        receive(&mut a, json!({"id":turn["id"],"result":{"turn":{"id":"t1"}}}));
        let cmd = json!({"type":"commandExecution","id":"c1","command":"/bin/zsh -lc 'cargo test'","commandActions":[{"type":"unknown","command":"cargo test"}],"status":"inProgress"});
        assert_eq!(receive(&mut a, json!({"method":"item/started","params":{"threadId":"th-2","item":cmd}})), vec![Event::ToolStart { kind: ToolKind::Bash, target: "cargo test".into() }]);
        let done = json!({"type":"commandExecution","id":"c1","command":"x","status":"failed"});
        assert_eq!(receive(&mut a, json!({"method":"item/completed","params":{"threadId":"th-2","item":done}})), vec![Event::ToolEnd { ok: false }]);
        let other = json!({"method":"item/started","params":{"threadId":"sub","item":cmd}});
        assert!(a.feed(format!("{other}\n").as_bytes()) == (vec![], vec![]), "a subagent's thread stays out");
        receive(&mut a, json!({"method":"item/completed","params":{"threadId":"th-2","item":{"type":"agentMessage","id":"m","text":"Done."}}}));
        let usage = receive(&mut a, json!({"method":"thread/tokenUsage/updated","params":{"threadId":"th-2","tokenUsage":{"total":{"totalTokens":99}}}}));
        assert_eq!(usage, vec![Event::Usage { tokens: 99, cost: None }]);
        let events = receive(&mut a, json!({"method":"turn/completed","params":{"threadId":"th-2","turn":{"id":"t1","status":"completed"}}}));
        assert_eq!(events, vec![Event::Summary { text: "Done.".into() }, Event::TurnDone, Event::Prompt { text: "second".into() }]);
    }

    #[test]
    fn approvals_and_questions_answer_by_request_id() {
        let (mut a, _) = started(None, false);
        a.prompt("go".into());
        sent(&mut a);
        let ask = json!({"method":"item/commandExecution/requestApproval","id":0,"params":{"threadId":"th-2","command":"/bin/zsh -lc 'rm x'","reason":"why"}});
        assert_eq!(receive(&mut a, ask), vec![Event::PermissionRequest { tool: "Bash".into(), detail: "run /bin/zsh -lc 'rm x'".into() }]);
        a.answer(true, "").unwrap();
        assert_eq!(sent(&mut a)[0], json!({"id":0,"result":{"decision":"accept"}}));
        let qs = json!([{"id":"a","question":"Which?","options":[{"label":"X"},{"label":"Y"}]},{"id":"b","question":"Why?","options":null}]);
        let ask = json!({"method":"item/tool/requestUserInput","id":"q","params":{"threadId":"th-2","questions":qs}});
        assert_eq!(receive(&mut a, ask), vec![Event::Question { text: "Which?".into(), options: vec!["X".into(), "Y".into()] }]);
        assert!(a.answer(true, "").is_err());
        assert_eq!(a.prompt("X".into()), vec![Event::Question { text: "Why?".into(), options: vec![] }]);
        assert!(a.prompt("because".into()).is_empty());
        assert_eq!(sent(&mut a)[0], json!({"id":"q","result":{"answers":{"a":{"answers":["X"]},"b":{"answers":["because"]}}}}));
        receive(&mut a, json!({"method":"item/fileChange/requestApproval","id":5,"params":{"threadId":"th-2"}}));
        receive(&mut a, json!({"method":"serverRequest/resolved","params":{"threadId":"th-2","requestId":5}}));
        assert!(!a.asking(), "resolved elsewhere");
    }

    #[test]
    fn cancel_before_and_after_the_turn_has_an_id() {
        let (mut a, _) = started(None, false);
        a.prompt("go".into());
        let start = sent(&mut a)[0].clone();
        a.cancel();
        assert!(sent(&mut a).is_empty(), "no turn id yet");
        receive(&mut a, json!({"id":start["id"],"result":{"turn":{"id":"t1"}}}));
        let out = sent(&mut a);
        assert_eq!((out[0]["method"].as_str(), out[0]["params"]["turnId"].as_str()), (Some("turn/interrupt"), Some("t1")));
        let ask = json!({"method":"item/fileChange/requestApproval","id":7,"params":{"threadId":"th-2"}});
        receive(&mut a, ask);
        a.cancel();
        let out = sent(&mut a);
        assert_eq!(out[0], json!({"id":7,"result":{"decision":"decline"}}));
        assert_eq!(out[1]["method"], "turn/interrupt");
        let events = receive(&mut a, json!({"method":"turn/completed","params":{"threadId":"th-2","turn":{"id":"t1","status":"interrupted"}}}));
        assert_eq!(events, vec![Event::TurnDone]);
    }

    #[test]
    fn failures_surface() {
        let (mut a, _) = started(None, false);
        a.prompt("go".into());
        let turn = sent(&mut a)[0].clone();
        let events = receive(&mut a, json!({"id":turn["id"],"error":{"code":-1,"message":"model not found"}}));
        assert_eq!(events, vec![Event::Error { message: "model not found".into() }]);
        assert!(!a.busy());
        a.prompt("again".into());
        receive(&mut a, json!({"method":"error","params":{"threadId":"th-2","willRetry":true,"error":{"message":"retrying"}}}));
        let events = receive(&mut a, json!({"method":"turn/completed","params":{"threadId":"th-2","turn":{"id":"t2","status":"failed","error":{"message":"usage limit"}}}}));
        assert_eq!(events, vec![Event::Error { message: "usage limit".into() }]);
        let mut fresh = start("codex", Path::new("/repo"), "", "", "", None, false).1;
        assert_eq!(receive(&mut fresh, json!({"id":0,"error":{"message":"not logged in"}})), vec![Event::Error { message: "codex didn't start: not logged in".into() }]);
    }
}

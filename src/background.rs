//! Stateful JSONL transports. No terminal emulation or ANSI scraping is involved.
use crate::{
    adapter,
    proto::{Event, ToolKind},
};
use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};
use std::path::Path;

const MAX_LINE: usize = 16 << 20;

pub fn supported(agent: &str) -> bool {
    matches!(adapter::kind(agent), "claude" | "codex" | "pi" | "gemini")
}

/// Launch arguments intentionally preserve each agent's configured permissions and login.
pub fn argv(agent: &str, model: &str, resume: bool) -> Vec<String> {
    let args: &[&str] = match adapter::kind(agent) {
        "claude" => &[
            "--print",
            "--verbose",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--include-partial-messages",
            "--permission-prompt-tool",
            "stdio",
        ],
        "codex" => &["app-server", "--listen", "stdio://"],
        "pi" => &["--mode", "rpc"],
        "gemini" => &["--acp"],
        _ => &[],
    };
    let mut argv = vec![agent.to_string()];
    argv.extend(args.iter().map(|s| s.to_string()));
    if resume && matches!(adapter::kind(agent), "claude" | "pi") {
        argv.push("--continue".into());
    }
    if !model.is_empty() && adapter::kind(agent) != "codex" {
        argv.extend(["--model".into(), model.into()]);
    }
    argv
}

#[derive(Clone)]
struct Ask {
    id: Value,
    method: String,
    params: Value,
    question: usize,
    answers: serde_json::Map<String, Value>,
}

pub struct Agent {
    kind: String,
    cwd: String,
    model: String,
    pub session: String,
    ready: bool,
    busy: bool,
    cancel_requested: bool,
    next_id: u64,
    turn: String,
    message: String,
    last_text: String,
    last_error: Option<String>,
    streamed_messages: HashSet<String>,
    prompts: VecDeque<String>,
    asks: VecDeque<Ask>,
    input: Vec<u8>,
    /// Retained across partial nonblocking writes by the daemon.
    pub output: Vec<u8>,
}

impl Agent {
    pub fn new(agent: &str, cwd: &Path, model: &str, session: String) -> Self {
        let kind = adapter::kind(agent).to_string();
        let ready = kind == "pi";
        let mut a = Self {
            kind,
            cwd: cwd.display().to_string(),
            model: model.into(),
            session,
            ready,
            busy: false,
            cancel_requested: false,
            next_id: 10,
            turn: String::new(),
            message: String::new(),
            last_text: String::new(),
            last_error: None,
            streamed_messages: HashSet::new(),
            prompts: VecDeque::new(),
            asks: VecDeque::new(),
            input: Vec::new(),
            output: Vec::new(),
        };
        match a.kind.as_str() {
            "claude" => a.send(json!({"type":"control_request","request_id":"arranger-init","request":{"subtype":"initialize"}})),
            "codex" => a.send(json!({"id":0,"method":"initialize","params":{"clientInfo":{"name":"arranger","title":"Arranger","version":env!("CARGO_PKG_VERSION")}}})),
            "gemini" => a.rpc(0, "initialize", json!({"protocolVersion":1,"clientCapabilities":{},"clientInfo":{"name":"arranger","version":env!("CARGO_PKG_VERSION")}})),
            _ => {}
        }
        a
    }
    fn send(&mut self, value: Value) {
        self.output.extend(serde_json::to_vec(&value).unwrap());
        self.output.push(b'\n');
    }
    fn rpc(&mut self, id: u64, method: &str, params: Value) {
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}));
    }
    pub fn asking(&self) -> bool {
        !self.asks.is_empty()
    }
    pub fn prompt(&mut self, text: String) -> Result<(), String> {
        if self.prompts.len() >= 100 {
            return Err("Too many queued messages; wait for the agent to catch up".into());
        }
        if let Some(mut ask) = self.asks.front().cloned() {
            let claude_question =
                ask.method == "can_use_tool" && ask.params["tool_name"] == "AskUserQuestion";
            if ask.method == "item/tool/requestUserInput" || claude_question {
                let questions = if claude_question {
                    &ask.params["input"]["questions"]
                } else {
                    &ask.params["questions"]
                };
                let q = &questions[ask.question];
                let key = string(&q[if claude_question { "question" } else { "id" }]);
                ask.answers.insert(
                    key,
                    if claude_question {
                        json!(text)
                    } else {
                        json!({"answers":[text]})
                    },
                );
                ask.question += 1;
                let count = questions.as_array().map_or(0, Vec::len);
                self.asks.pop_front();
                if ask.question < count {
                    self.asks.push_front(ask);
                } else if claude_question {
                    let mut input = ask.params["input"].clone();
                    input["answers"] = json!(ask.answers);
                    self.claude_answer(&ask, json!({"behavior":"allow","updatedInput":input}));
                } else {
                    self.send(json!({"id":ask.id,"result":{"answers":ask.answers}}));
                }
                return Ok(());
            }
            if matches!(ask.method.as_str(), "select" | "input" | "editor") {
                self.asks.pop_front();
                self.send(json!({"type":"extension_ui_response","id":ask.id,"value":text}));
                return Ok(());
            }
        }
        self.prompts.push_back(text);
        self.start_next();
        Ok(())
    }
    fn start_next(&mut self) {
        if !self.ready || self.busy {
            return;
        }
        let Some(text) = self.prompts.pop_front() else {
            return;
        };
        self.busy = true;
        self.cancel_requested = false;
        self.last_text.clear();
        self.last_error = None;
        let id = self.next_id;
        self.next_id += 1;
        match self.kind.as_str() {
            "claude" => self.send(json!({"type":"user","message":{"role":"user","content":text}})),
            "pi" => self.send(json!({"type":"prompt","id":id.to_string(),"message":text})),
            "codex" => self.send(json!({"id":id,"method":"turn/start","params":{"threadId":self.session,"input":[{"type":"text","text":text}]}})),
            "gemini" => self.rpc(id, "session/prompt", json!({"sessionId":self.session,"prompt":[{"type":"text","text":text}]})),
            _ => {}
        }
    }
    pub fn cancel(&mut self) {
        self.prompts.clear();
        // Resolve blocking permission dialogs before interrupting the turn.
        while self.asking() {
            let ask = self.asks.front().unwrap().clone();
            if ask.method == "item/tool/requestUserInput" {
                self.asks.pop_front();
                self.send(json!({"id":ask.id,"result":{"answers":{}}}));
            } else if matches!(ask.method.as_str(), "select" | "input" | "editor") {
                self.asks.pop_front();
                self.send(json!({"type":"extension_ui_response","id":ask.id,"cancelled":true}));
            } else {
                let _ = self.answer(false, "Turn cancelled");
            }
        }
        self.cancel_requested = true;
        match self.kind.as_str() {
            "claude" => self.send(json!({"type":"control_request","request_id":format!("cancel-{}",self.next_id),"request":{"subtype":"interrupt"}})),
            "pi" => self.send(json!({"type":"abort"})),
            "codex" if !self.turn.is_empty() => self.send(json!({"id":self.next_id,"method":"turn/interrupt","params":{"threadId":self.session,"turnId":self.turn}})),
            "gemini" => self.send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":self.session}})),
            _ => {}
        }
        self.next_id += 1;
    }
    fn claude_answer(&mut self, ask: &Ask, response: Value) {
        self.send(json!({"type":"control_response","response":{"subtype":"success","request_id":ask.id,"response":response}}));
    }
    pub fn answer(&mut self, allow: bool, message: &str) -> Result<(), String> {
        let Some(ask) = self.asks.pop_front() else {
            return Err("The agent isn't waiting for an answer".into());
        };
        if ask.method == "item/tool/requestUserInput"
            || matches!(ask.method.as_str(), "select" | "input" | "editor")
        {
            self.asks.push_front(ask);
            return Err("Answer this question in the message box".into());
        }
        match self.kind.as_str() {
            "claude" => {
                let response = if allow {
                    json!({"behavior":"allow","updatedInput":ask.params["input"]})
                } else {
                    json!({"behavior":"deny","message":message})
                };
                self.claude_answer(&ask, response);
            }
            "codex" => {
                let result = if ask.method == "item/permissions/requestApproval" {
                    json!({"permissions":if allow { ask.params["permissions"].clone() } else { json!({}) },"scope":"turn"})
                } else {
                    json!({"decision":if allow {"accept"} else {"decline"}})
                };
                self.send(json!({"id":ask.id,"result":result}));
            }
            "pi" => {
                self.send(json!({"type":"extension_ui_response","id":ask.id,"confirmed":allow}))
            }
            "gemini" => {
                let option = ask.params["options"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|o| o["kind"] == if allow { "allow_once" } else { "reject_once" });
                let outcome = option.map_or(
                    json!({"outcome":"cancelled"}),
                    |o| json!({"outcome":"selected","optionId":o["optionId"]}),
                );
                self.send(json!({"jsonrpc":"2.0","id":ask.id,"result":{"outcome":outcome}}));
            }
            _ => {}
        }
        Ok(())
    }
    pub fn pending_need(&self) -> Option<Event> {
        self.asks.front().map(Self::need)
    }
    fn need(ask: &Ask) -> Event {
        let p = &ask.params;
        let q = match ask.method.as_str() {
            "item/tool/requestUserInput" => Some((
                &p["questions"][ask.question]["question"],
                &p["questions"][ask.question]["options"],
            )),
            "can_use_tool" if p["tool_name"] == "AskUserQuestion" => Some((
                &p["input"]["questions"][ask.question]["question"],
                &p["input"]["questions"][ask.question]["options"],
            )),
            "select" | "input" | "editor" => Some((&p["title"], &p["options"])),
            _ => None,
        };
        if let Some((text, options)) = q {
            return Event::Question {
                text: string(text),
                options: options
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|o| {
                        o.as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| string(&o["label"]))
                    })
                    .collect(),
            };
        }
        Event::PermissionRequest {
            tool: p["tool_name"].as_str().unwrap_or(&ask.method).into(),
            detail: [
                p.get("command"),
                p.get("reason"),
                p.get("message"),
                p.get("title"),
                p.get("toolCall"),
                p.get("input"),
            ]
            .into_iter()
            .flatten()
            .map(string)
            .find(|s| !s.is_empty())
            .unwrap_or_default(),
        }
    }
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Event> {
        self.input.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(end) = self.input.iter().position(|b| *b == b'\n') {
            let line: Vec<_> = self.input.drain(..=end).collect();
            if line.len() > MAX_LINE {
                events.push(Event::Error {
                    message: "Agent output exceeded the JSON message size limit".into(),
                });
                continue;
            }
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            match serde_json::from_slice::<Value>(&line) {
                Ok(v) => match self.kind.as_str() {
                    "codex" => self.codex(v, &mut events),
                    "claude" => self.claude(v, &mut events),
                    "pi" => self.pi(v, &mut events),
                    "gemini" => self.gemini(v, &mut events),
                    _ => {}
                },
                Err(_) => events.push(Event::Error {
                    message: "Agent returned invalid JSON; see its diagnostic log".into(),
                }),
            }
        }
        if self.input.len() > MAX_LINE {
            self.input.clear();
            events.push(Event::Error {
                message: "Agent output exceeded the JSON message size limit".into(),
            });
        }
        events
    }
    fn done(&mut self, events: &mut Vec<Event>) {
        self.busy = false;
        self.turn.clear();
        self.asks.clear();
        if !self.last_text.is_empty() {
            events.push(Event::Summary {
                text: self.last_text.clone(),
            });
        }
        events.push(Event::TurnDone);
        if let Some(message) = self.last_error.take() {
            let queued = self.prompts.len();
            self.prompts.clear();
            events.push(Event::Error {
                message: if queued > 0 {
                    format!("{message} ({queued} queued message(s) were not sent)")
                } else {
                    message
                },
            });
        } else {
            self.start_next();
        }
        if self.busy {
            events.push(Event::TurnStarted);
        }
    }
    fn ask(&mut self, id: Value, method: &str, params: Value, events: &mut Vec<Event>) {
        let ask = Ask {
            id,
            method: method.into(),
            params,
            question: 0,
            answers: serde_json::Map::new(),
        };
        if self.asks.is_empty() {
            events.push(Self::need(&ask));
        }
        self.asks.push_back(ask);
    }
    fn text(&mut self, id: String, text: String, append: bool, events: &mut Vec<Event>) {
        if append {
            if !text.is_empty() {
                self.last_error = None;
            }
            self.last_text.push_str(&text);
        } else {
            self.last_text = text.clone();
        }
        if !text.is_empty() {
            events.push(Event::Message { id, text, append });
        }
    }
    fn codex(&mut self, v: Value, events: &mut Vec<Event>) {
        if let Some(error) = v.get("error") {
            self.last_error = Some(error_message(error));
            self.done(events);
            return;
        }
        if v["id"] == 0 && v.get("result").is_some() {
            self.send(json!({"method":"initialized","params":{}}));
            let mut params = json!({"cwd":self.cwd});
            if !self.model.is_empty() {
                params["model"] = json!(self.model);
            }
            let method = if self.session.is_empty() {
                "thread/start"
            } else {
                params["threadId"] = json!(self.session);
                "thread/resume"
            };
            self.send(json!({"id":1,"method":method,"params":params}));
        } else if v["id"] == 1 && v.get("result").is_some() {
            self.session = string(&v["result"]["thread"]["id"]);
            self.ready = !self.session.is_empty();
            self.start_next();
        }
        let p = &v["params"];
        let method = v["method"].as_str().unwrap_or("");
        if let Some(id) = v.get("id").filter(|_| !method.is_empty()) {
            if matches!(
                method,
                "item/commandExecution/requestApproval"
                    | "item/fileChange/requestApproval"
                    | "item/permissions/requestApproval"
                    | "item/tool/requestUserInput"
            ) {
                self.ask(id.clone(), method, p.clone(), events);
            } else {
                self.send(json!({"id":id,"error":{"code":-32601,"message":"This interaction is not supported by Arranger"}}));
                events.push(Event::Error {
                    message: format!("Unsupported agent interaction: {method}"),
                });
            }
            return;
        }
        match method {
            "turn/started" => {
                self.turn = string(&p["turn"]["id"]);
                if self.cancel_requested {
                    self.send(json!({"id":self.next_id,"method":"turn/interrupt","params":{"threadId":self.session,"turnId":self.turn}}));
                    self.next_id += 1;
                }
            }
            "serverRequest/resolved" => {
                self.asks.retain(|a| a.id != p["requestId"]);
            }
            "item/agentMessage/delta" => {
                self.text(string(&p["itemId"]), string(&p["delta"]), true, events)
            }
            "item/started" | "item/completed" => {
                let item = &p["item"];
                let started = method == "item/started";
                match item["type"].as_str().unwrap_or("") {
                    "agentMessage" if !started => {
                        self.text(string(&item["id"]), string(&item["text"]), false, events)
                    }
                    "commandExecution" if started => events.push(Event::ToolStart {
                        kind: ToolKind::Bash,
                        target: string(&item["command"]),
                    }),
                    "fileChange" if started => {
                        for change in item["changes"].as_array().into_iter().flatten() {
                            events.push(Event::ToolStart {
                                kind: ToolKind::Edit,
                                target: string(&change["path"]),
                            });
                        }
                    }
                    "mcpToolCall" if started => events.push(Event::ToolStart {
                        kind: ToolKind::Other,
                        target: string(&item["tool"]),
                    }),
                    "commandExecution" | "fileChange" | "mcpToolCall" if !started => {
                        output(&item["aggregatedOutput"], events);
                        events.push(Event::ToolEnd {
                            ok: item["status"] == "completed",
                        });
                    }
                    _ => {}
                }
            }
            "turn/completed" => {
                if p["turn"]["status"] == "failed" {
                    self.last_error = Some(error_message(&p["turn"]["error"]));
                }
                self.done(events);
            }
            "error" => {
                let message = error_message(&p["error"]);
                if p["willRetry"] == true {
                    events.push(Event::Retrying { message });
                } else {
                    self.last_error = Some(message.clone());
                    events.push(Event::Error { message });
                }
            }
            "thread/tokenUsage/updated" => {
                if let Some(tokens) = p["tokenUsage"]["total"]["totalTokens"].as_u64() {
                    events.push(Event::Usage { tokens, cost: None });
                }
            }
            _ => {}
        }
    }
    fn claude(&mut self, v: Value, events: &mut Vec<Event>) {
        if !v["parent_tool_use_id"].is_null() {
            return;
        }
        match v["type"].as_str().unwrap_or("") {
            "control_response" if v["response"]["request_id"] == "arranger-init" => {
                if v["response"]["subtype"] == "success" {
                    self.ready = true;
                    self.start_next();
                } else {
                    events.push(Event::Error {
                        message: string(&v["response"]["error"]),
                    });
                }
            }
            "control_cancel_request" => self.asks.retain(|a| a.id != v["request_id"]),
            "control_request" if v["request"]["subtype"] == "can_use_tool" => self.ask(
                v["request_id"].clone(),
                "can_use_tool",
                v["request"].clone(),
                events,
            ),
            "stream_event" => {
                let e = &v["event"];
                match e["type"].as_str().unwrap_or("") {
                    "message_start" => self.message = string(&e["message"]["id"]),
                    "content_block_delta" if e["delta"]["type"] == "text_delta" => {
                        self.streamed_messages.insert(self.message.clone());
                        self.text(
                            format!("{}-{}", self.message, e["index"]),
                            string(&e["delta"]["text"]),
                            true,
                            events,
                        );
                    }
                    _ => {}
                }
            }
            "assistant"
                if v["isApiErrorMessage"] == true
                    || v.get("error").is_some_and(|e| !e.is_null()) =>
            {
                let text = v["message"]["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|c| c["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                let message = if text.is_empty() {
                    error_message(&v["error"])
                } else {
                    text
                };
                self.last_error = Some(message.clone());
                events.push(Event::Error { message });
            }
            "assistant" => {
                let message = &v["message"];
                for (i, c) in message["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    match c["type"].as_str().unwrap_or("") {
                        "text" => {
                            if self
                                .streamed_messages
                                .contains(message["id"].as_str().unwrap_or(""))
                            {
                                self.last_text = string(&c["text"]);
                            } else {
                                self.text(
                                    format!("{}-{i}", string(&message["id"])),
                                    string(&c["text"]),
                                    false,
                                    events,
                                );
                            }
                        }
                        "tool_use" => events.extend(adapter::tool_start(
                            c["name"].as_str().unwrap_or("tool"),
                            &c["input"],
                            &self.cwd,
                        )),
                        _ => {}
                    }
                }
            }
            "user" => {
                for c in v["message"]["content"].as_array().into_iter().flatten() {
                    if c["type"] == "tool_result" {
                        content_output(&c["content"], events);
                        events.push(Event::ToolEnd {
                            ok: c["is_error"] != true,
                        });
                    }
                }
            }
            "result" => {
                if v["is_error"] == true {
                    self.last_error = Some(
                        v.get("errors")
                            .map(error_message)
                            .unwrap_or_else(|| error_message(&v["result"])),
                    );
                }
                self.done(events);
                let u = &v["usage"];
                events.push(Event::Usage {
                    tokens: [
                        "input_tokens",
                        "output_tokens",
                        "cache_creation_input_tokens",
                        "cache_read_input_tokens",
                    ]
                    .iter()
                    .filter_map(|k| u[k].as_u64())
                    .sum(),
                    cost: v["total_cost_usd"].as_f64(),
                });
            }
            _ => {}
        }
    }
    fn pi(&mut self, v: Value, events: &mut Vec<Event>) {
        match v["type"].as_str().unwrap_or("") {
            "message_start" if v["message"]["role"] == "assistant" => {
                self.message = format!("pi-{}-{}", self.next_id, v["message"]["timestamp"]);
            }
            "message_update" if v["assistantMessageEvent"]["type"] == "text_delta" => self.text(
                format!(
                    "{}-{}",
                    self.message, v["assistantMessageEvent"]["contentIndex"]
                ),
                string(&v["assistantMessageEvent"]["delta"]),
                true,
                events,
            ),
            "message_end" if v["message"]["role"] == "assistant" => {
                for (i, c) in v["message"]["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    if c["type"] == "text" {
                        self.text(
                            format!("{}-{i}", self.message),
                            string(&c["text"]),
                            false,
                            events,
                        );
                    }
                }
                if let Some(error) = v["message"]["errorMessage"].as_str() {
                    self.last_error = Some(error.into());
                    events.push(Event::Error {
                        message: error.into(),
                    });
                }
            }
            "tool_execution_start" => events.extend(adapter::tool_start(
                v["toolName"].as_str().unwrap_or("tool"),
                &v["args"],
                &self.cwd,
            )),
            "tool_execution_end" => {
                content_output(&v["result"]["content"], events);
                events.push(Event::ToolEnd {
                    ok: v["isError"] != true,
                });
            }
            "agent_end" if v["willRetry"] != true => self.done(events),
            "agent_settled" if self.busy => self.done(events),
            "auto_retry_start" | "summarization_retry_scheduled" => {
                self.last_error = None;
                events.push(Event::Retrying {
                    message: error_message(&v["errorMessage"]),
                });
            }
            "auto_retry_end" if v["success"] == false => {
                self.last_error = Some(error_message(&v["finalError"]));
                self.done(events);
            }
            "agent_start" => {
                self.last_error = None;
                events.push(Event::TurnStarted);
            }
            "response" if v["success"] == false => {
                self.busy = false;
                events.push(Event::Error {
                    message: string(&v["error"]),
                });
            }
            "extension_ui_request" => {
                let method = v["method"].as_str().unwrap_or("");
                if matches!(method, "select" | "confirm" | "input" | "editor") {
                    self.ask(v["id"].clone(), method, v.clone(), events);
                }
            }
            _ => {}
        }
    }
    fn gemini(&mut self, v: Value, events: &mut Vec<Event>) {
        if let Some(error) = v.get("error") {
            self.last_error = Some(error_message(error));
            self.done(events);
            return;
        }
        if v["id"] == 0 && v.get("result").is_some() {
            if self.session.is_empty() {
                self.rpc(1, "session/new", json!({"cwd":self.cwd,"mcpServers":[]}));
            } else {
                self.rpc(
                    1,
                    "session/load",
                    json!({"sessionId":self.session,"cwd":self.cwd,"mcpServers":[]}),
                );
            }
        } else if v["id"] == 1 && v.get("result").is_some() {
            if let Some(s) = v["result"]["sessionId"].as_str() {
                self.session = s.into();
            }
            self.ready = !self.session.is_empty();
            self.start_next();
        } else if v["result"].get("stopReason").is_some() {
            self.done(events);
        }
        match v["method"].as_str().unwrap_or("") {
            "session/request_permission" => self.ask(v["id"].clone(), "session/request_permission", v["params"].clone(), events),
            "session/update" => {
                let u = &v["params"]["update"];
                match u["sessionUpdate"].as_str().unwrap_or("") {
                    "agent_message_chunk" if u["content"]["type"] == "text" => self.text(format!("gemini-{}",self.next_id),string(&u["content"]["text"]),true,events),
                    "tool_call" => events.push(Event::ToolStart { kind: ToolKind::Other, target: string(&u["title"]) }),
                    "tool_call_update" if u["status"] == "completed" || u["status"] == "failed" => {
                        for c in u["content"].as_array().into_iter().flatten() { if c["type"] == "content" { output(&c["content"]["text"],events); } }
                        events.push(Event::ToolEnd { ok: u["status"] == "completed" });
                    }
                    _ => {}
                }
            }
            method if v.get("id").is_some() && !method.is_empty() => self.send(json!({"jsonrpc":"2.0","id":v["id"],"error":{"code":-32601,"message":"Unsupported client capability"}})),
            _ => {}
        }
    }
}
fn error_message(v: &Value) -> String {
    if let Some(message) = v["message"].as_str() {
        message.into()
    } else {
        let text = string(v);
        if text.is_empty() {
            "Agent request failed".into()
        } else {
            text
        }
    }
}
fn string(v: &Value) -> String {
    v.as_str().map(str::to_string).unwrap_or_else(|| {
        if v.is_null() {
            String::new()
        } else {
            v.to_string()
        }
    })
}

fn output(v: &Value, events: &mut Vec<Event>) {
    if let Some(text) = v.as_str().filter(|s| !s.is_empty()) {
        let mut start = text.len().saturating_sub(32 * 1024);
        while !text.is_char_boundary(start) {
            start += 1;
        }
        let text = if start > 0 {
            format!("[earlier tool output omitted]\n{}", &text[start..])
        } else {
            text.into()
        };
        events.push(Event::ToolOutput { text });
    }
}
fn content_output(v: &Value, events: &mut Vec<Event>) {
    if v.is_string() {
        output(v, events);
    } else {
        for c in v.as_array().into_iter().flatten() {
            if c["type"] == "text" {
                output(&c["text"], events);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn receive(a: &mut Agent, v: Value) -> Vec<Event> {
        let mut bytes = serde_json::to_vec(&v).unwrap();
        bytes.push(b'\n');
        a.feed(&bytes)
    }
    fn sent(a: &mut Agent) -> Vec<Value> {
        let bytes = std::mem::take(&mut a.output);
        bytes
            .split(|b| *b == b'\n')
            .filter(|s| !s.is_empty())
            .map(|s| serde_json::from_slice(s).unwrap())
            .collect()
    }
    fn codex() -> Agent {
        let mut a = Agent::new("codex", Path::new("/repo"), "my-model", String::new());
        a.prompt("first".into()).unwrap();
        assert_eq!(sent(&mut a)[0]["method"], "initialize");
        receive(&mut a, json!({"id":0,"result":{}}));
        let messages = sent(&mut a);
        assert_eq!(messages[0]["method"], "initialized");
        assert_eq!(messages[1]["params"]["model"], "my-model");
        assert!(messages[1]["params"].get("approvalPolicy").is_none());
        receive(
            &mut a,
            json!({"id":1,"result":{"thread":{"id":"thread-1"}}}),
        );
        assert_eq!(sent(&mut a)[0]["params"]["input"][0]["text"], "first");
        a
    }
    #[test]
    fn codex_handshake_queues_turns_and_preserves_resume_id() {
        let mut a = codex();
        a.prompt("second\nline".into()).unwrap();
        assert!(sent(&mut a).is_empty());
        let events = receive(
            &mut a,
            json!({"method":"turn/completed","params":{"turn":{"status":"completed"}}}),
        );
        assert!(matches!(
            events.as_slice(),
            [Event::TurnDone, Event::TurnStarted]
        ));
        assert_eq!(
            sent(&mut a)[0]["params"]["input"][0]["text"],
            "second\nline"
        );
        let mut resumed = Agent::new("codex", Path::new("/repo"), "", a.session);
        sent(&mut resumed);
        receive(&mut resumed, json!({"id":0,"result":{}}));
        let messages = sent(&mut resumed);
        assert_eq!(messages[1]["method"], "thread/resume");
        assert_eq!(messages[1]["params"]["threadId"], "thread-1");
    }
    #[test]
    fn codex_approvals_and_multiple_questions_correlate_responses() {
        let mut a = codex();
        receive(
            &mut a,
            json!({"id":91,"method":"item/commandExecution/requestApproval","params":{"command":"cargo test"}}),
        );
        assert!(
            matches!(a.pending_need(),Some(Event::PermissionRequest {detail,..}) if detail=="cargo test")
        );
        a.answer(false, "no").unwrap();
        assert_eq!(
            sent(&mut a)[0],
            json!({"id":91,"result":{"decision":"decline"}})
        );
        receive(
            &mut a,
            json!({"id":"questions","method":"item/tool/requestUserInput","params":{"questions":[{"id":"color","question":"Color?","options":[{"label":"Blue"}]},{"id":"size","question":"Size?","options":[]}]}}),
        );
        a.prompt("Blue".into()).unwrap();
        assert!(sent(&mut a).is_empty());
        assert!(matches!(a.pending_need(),Some(Event::Question {text,..}) if text=="Size?"));
        a.prompt("Large".into()).unwrap();
        assert_eq!(
            sent(&mut a)[0],
            json!({"id":"questions","result":{"answers":{"color":{"answers":["Blue"]},"size":{"answers":["Large"]}}}})
        );
        assert!(!a.asking());
    }
    #[test]
    fn cancel_before_turn_started_is_sent_when_turn_id_arrives() {
        let mut a = codex();
        a.prompt("queued".into()).unwrap();
        a.cancel();
        assert!(sent(&mut a).is_empty());
        receive(
            &mut a,
            json!({"method":"turn/started","params":{"turn":{"id":"turn-1"}}}),
        );
        let messages = sent(&mut a);
        assert_eq!(messages[0]["method"], "turn/interrupt");
        assert_eq!(messages[0]["params"]["turnId"], "turn-1");
        receive(
            &mut a,
            json!({"method":"turn/completed","params":{"turn":{"status":"interrupted"}}}),
        );
        assert!(sent(&mut a).is_empty());
        a.prompt("new turn".into()).unwrap();
        assert_eq!(sent(&mut a)[0]["method"], "turn/start");
    }
    #[test]
    fn claude_partial_json_and_content_blocks_do_not_duplicate_messages() {
        let mut a = Agent::new("claude", Path::new("/repo"), "", String::new());
        a.prompt("hello".into()).unwrap();
        assert_eq!(sent(&mut a)[0]["request"]["subtype"], "initialize");
        receive(
            &mut a,
            json!({"type":"control_response","response":{"request_id":"arranger-init","subtype":"success"}}),
        );
        assert_eq!(sent(&mut a)[0]["message"]["content"], "hello");
        receive(
            &mut a,
            json!({"type":"stream_event","event":{"type":"message_start","message":{"id":"msg"}}}),
        );
        let bytes=b"{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}}\n";
        assert!(a.feed(&bytes[..20]).is_empty());
        let events = a.feed(&bytes[20..]);
        assert_eq!(
            events,
            vec![Event::Message {
                id: "msg-2".into(),
                text: "hello".into(),
                append: true
            }]
        );
        // Claude may emit only the completed block, with a different array index.
        assert!(receive(&mut a,json!({"type":"assistant","message":{"id":"msg","content":[{"type":"text","text":"hello"}]}})).is_empty());
        receive(
            &mut a,
            json!({"type":"control_request","request_id":"permission","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"}}}),
        );
        a.answer(true, "").unwrap();
        assert_eq!(
            sent(&mut a)[0]["response"],
            json!({"subtype":"success","request_id":"permission","response":{"behavior":"allow","updatedInput":{"command":"ls"}}})
        );
    }
    #[test]
    fn pi_and_gemini_use_native_questions_and_streams() {
        let mut pi = Agent::new("pi", Path::new("/repo"), "", String::new());
        pi.prompt("hello".into()).unwrap();
        assert_eq!(sent(&mut pi)[0]["type"], "prompt");
        receive(
            &mut pi,
            json!({"type":"message_start","message":{"role":"assistant","timestamp":100}}),
        );
        let events = receive(
            &mut pi,
            json!({"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"hi"}}),
        );
        assert!(matches!(&events[0],Event::Message {text,..} if text=="hi"));
        receive(
            &mut pi,
            json!({"type":"extension_ui_request","id":"select","method":"select","title":"Pick","options":["A","B"]}),
        );
        pi.prompt("B".into()).unwrap();
        assert_eq!(
            sent(&mut pi)[0],
            json!({"type":"extension_ui_response","id":"select","value":"B"})
        );
        let mut gemini = Agent::new("gemini", Path::new("/repo"), "", String::new());
        gemini.prompt("hello".into()).unwrap();
        sent(&mut gemini);
        receive(&mut gemini, json!({"id":0,"result":{"protocolVersion":1}}));
        assert_eq!(sent(&mut gemini)[0]["method"], "session/new");
        receive(&mut gemini, json!({"id":1,"result":{"sessionId":"g-1"}}));
        assert_eq!(sent(&mut gemini)[0]["params"]["sessionId"], "g-1");
        receive(
            &mut gemini,
            json!({"id":5,"method":"session/request_permission","params":{"toolCall":{"title":"Run test"},"options":[{"optionId":"yes","kind":"allow_once"},{"optionId":"no","kind":"reject_once"}]}}),
        );
        gemini.answer(true, "").unwrap();
        assert_eq!(
            sent(&mut gemini)[0]["result"],
            json!({"outcome":{"outcome":"selected","optionId":"yes"}})
        );
    }
    #[test]
    fn capacity_failure_is_terminal_but_explicit_retries_can_recover() {
        let mut a = codex();
        let message = "Selected model is at capacity. Please try a different model.";
        let events = receive(
            &mut a,
            json!({"method":"error","params":{"error":{"message":message},"willRetry":true}}),
        );
        assert_eq!(
            events,
            vec![Event::Retrying {
                message: message.into()
            }]
        );
        assert!(a.busy);
        receive(
            &mut a,
            json!({"method":"item/agentMessage/delta","params":{"itemId":"recovered","delta":"Available again"}}),
        );
        let events = receive(
            &mut a,
            json!({"method":"turn/completed","params":{"turn":{"status":"completed"}}}),
        );
        assert!(!events.iter().any(|e| matches!(e, Event::Error { .. })));
        a.prompt("try again".into()).unwrap();
        sent(&mut a);
        a.prompt("queued work".into()).unwrap();
        receive(
            &mut a,
            json!({"method":"error","params":{"error":{"message":message},"willRetry":false}}),
        );
        let events = receive(
            &mut a,
            json!({"method":"turn/completed","params":{"turn":{"status":"failed","error":{"message":message}}}}),
        );
        assert!(
            matches!(events.last(),Some(Event::Error {message}) if message.contains("Selected model is at capacity"))
        );
        assert!(!a.busy);
        assert!(
            sent(&mut a).is_empty(),
            "a failed turn must not automatically start queued work"
        );
    }

    #[test]
    fn errors_and_unknown_requests_are_visible_and_do_not_auto_approve() {
        let mut a = codex();
        let events = receive(
            &mut a,
            json!({"id":55,"method":"unsupported/approval","params":{}}),
        );
        assert!(matches!(&events[0], Event::Error { .. }));
        assert_eq!(sent(&mut a)[0]["error"]["code"], -32601);
        let events = receive(
            &mut a,
            json!({"method":"turn/completed","params":{"turn":{"status":"failed","error":{"message":"No credentials"}}}}),
        );
        assert!(
            matches!(events.last(),Some(Event::Error {message}) if message.contains("No credentials"))
        );
        assert!(matches!(
            a.feed(b"bad json\n").first(),
            Some(Event::Error { .. })
        ));
    }
}

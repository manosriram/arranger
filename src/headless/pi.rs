//! pi over `pi --mode rpc`: JSON commands on stdin, its agent events on stdout.
//!
//! pi has no permission prompts of its own; extensions' dialogs (select, confirm, input) come
//! as `extension_ui_request`s and are put to the user like any ask.

use super::{Ask, Core, Transport, string};
use crate::adapter;
use crate::proto::Event;
use serde_json::{Value, json};
use std::path::Path;

/// `session` is a session file (pi's `--session` and `--fork` take one from any directory).
pub fn argv(model: &str, effort: &str, session: Option<&str>, fork: bool) -> Vec<String> {
    let mut argv: Vec<String> = vec!["--mode".into(), "rpc".into()];
    match session {
        Some("") => argv.push("--continue".into()),
        Some(file) => argv.extend([if fork { "--fork" } else { "--session" }.into(), file.into()]),
        None => {}
    }
    if !model.is_empty() {
        argv.extend(["--model".into(), model.into()]);
    }
    if !effort.is_empty() {
        argv.extend(["--thinking".into(), effort.into()]);
    }
    argv
}

pub struct Pi {
    c: Core,
}

impl Pi {
    pub fn new(cwd: &Path) -> Self {
        let mut c = Core::new(cwd);
        // It takes prompts straight away; its state says which session file it's writing.
        c.ready = true;
        c.send(json!({"id":"arranger-state","type":"get_state"}));
        Pi { c }
    }

    fn start_next(&mut self) -> Vec<Event> {
        let Some(text) = self.c.next_prompt() else { return Vec::new() };
        let id = format!("arranger-{}", self.c.id());
        self.c.send(json!({"id":id,"type":"prompt","message":text}));
        vec![Event::Prompt { text }]
    }
}

impl Transport for Pi {
    fn core(&self) -> &Core {
        &self.c
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.c
    }

    fn prompt(&mut self, text: String) -> Vec<Event> {
        if let Some(ask) = self.c.asks.front().filter(|a| a.is_question()) {
            let id = ask.id.clone();
            self.c.asks.pop_front();
            self.c.send(json!({"type":"extension_ui_response","id":id,"value":text}));
            return self.c.asks.front().map(Ask::need).into_iter().collect();
        }
        self.c.prompts.push_back(text);
        self.start_next()
    }

    fn answer(&mut self, allow: bool, _message: &str) -> Result<(), String> {
        let Some(ask) = self.c.asks.front() else { return Err("the agent isn't waiting for an answer".into()) };
        if ask.is_question() {
            return Err("it's asking a question: answer with `arranger send`".into());
        }
        let id = self.c.asks.pop_front().unwrap().id;
        self.c.send(json!({"type":"extension_ui_response","id":id,"confirmed":allow}));
        Ok(())
    }

    fn cancel(&mut self) {
        self.c.prompts.clear();
        while let Some(ask) = self.c.asks.pop_front() {
            self.c.send(json!({"type":"extension_ui_response","id":ask.id,"cancelled":true}));
        }
        if self.c.busy {
            self.c.send(json!({"type":"abort"}));
        }
    }

    fn line(&mut self, v: Value, events: &mut Vec<Event>) -> bool {
        match v["type"].as_str().unwrap_or("") {
            "response" if v["id"] == "arranger-state" => {
                self.c.session = string(&v["data"]["sessionFile"]);
                return false;
            }
            "response" if v["success"] == false => {
                let message = string(&v["error"]);
                if v["command"] == "prompt" && self.c.busy {
                    self.c.done(Some(message), events);
                } else {
                    events.push(Event::Error { message });
                }
            }
            "response" => return false,
            "message_end" if v["message"]["role"] == "assistant" => {
                let m = &v["message"];
                let text: Vec<&str> = m["content"].as_array().into_iter().flatten().filter(|c| c["type"] == "text").filter_map(|c| c["text"].as_str()).collect();
                if !text.is_empty() {
                    self.c.last_text = text.join("\n");
                }
                if let Some(error) = m["errorMessage"].as_str().filter(|_| m["stopReason"] == "error") {
                    events.push(Event::Error { message: error.into() });
                }
                let u = &m["usage"];
                if let Some(tokens) = ["input", "output", "cacheRead", "cacheWrite"].iter().map(|k| u[k].as_u64()).sum::<Option<u64>>() {
                    events.push(Event::Usage { tokens, cost: u["cost"]["total"].as_f64() });
                }
            }
            "tool_execution_start" => events.extend(adapter::tool_start(v["toolName"].as_str().unwrap_or("tool"), &v["args"], &self.c.cwd)),
            "tool_execution_end" => events.push(Event::ToolEnd { ok: v["isError"] != true }),
            "agent_end" if v["willRetry"] != true => {
                self.c.done(None, events);
                events.extend(self.start_next());
            }
            "auto_retry_end" if v["success"] == false => events.push(Event::Error { message: string(&v["finalError"]) }),
            "extension_ui_request" => {
                let (id, method) = (v["id"].clone(), string(&v["method"]));
                let title = string(&v["title"]);
                let ask = match method.as_str() {
                    "confirm" => {
                        let detail = [title.clone(), string(&v["message"])].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(": ");
                        Ask::permission(id, &method, v.clone(), "Confirm", detail)
                    }
                    "select" | "input" | "editor" => {
                        let options = v["options"].as_array().into_iter().flatten().map(|o| o.as_str().map(str::to_string).unwrap_or_else(|| string(&o["label"]))).collect();
                        Ask::questions(id, &method, v.clone(), vec![(String::new(), title, options)])
                    }
                    _ => return false, // notify, setStatus…: nothing to answer
                };
                self.c.ask(ask, events);
                return false;
            }
            "extension_error" => events.push(Event::Error { message: format!("pi extension: {}", string(&v["error"])) }),
            _ => {}
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{receive, sent};
    use super::super::start;
    use super::*;
    use crate::proto::ToolKind;

    #[test]
    fn argv_carries_sessions_model_and_thinking() {
        let (a, _) = start("pi", Path::new("/r"), "sonnet", "high", Some("/s/x.jsonl"), true);
        assert_eq!(a, ["pi", "--mode", "rpc", "--fork", "/s/x.jsonl", "--model", "sonnet", "--thinking", "high"]);
        assert!(argv("", "", Some("/s/x.jsonl"), false).windows(2).any(|w| w == ["--session", "/s/x.jsonl"]));
        assert_eq!(argv("", "", Some(""), false), ["--mode", "rpc", "--continue"]);
    }

    #[test]
    fn a_turn_from_prompt_to_agent_end() {
        let (_, mut a) = start("pi", Path::new("/repo"), "", "", None, false);
        assert_eq!(sent(&mut a)[0]["type"], "get_state");
        receive(&mut a, json!({"id":"arranger-state","type":"response","command":"get_state","success":true,"data":{"sessionFile":"/s/1.jsonl"}}));
        assert_eq!(a.session(), "/s/1.jsonl");
        assert_eq!(a.prompt("go".into()), vec![Event::Prompt { text: "go".into() }]);
        assert_eq!(sent(&mut a)[0]["message"], "go");
        assert!(a.prompt("next".into()).is_empty(), "queued while it runs");
        let start = json!({"type":"tool_execution_start","toolCallId":"c","toolName":"bash","args":{"command":"ls"}});
        assert_eq!(receive(&mut a, start), vec![Event::ToolStart { kind: ToolKind::Bash, target: "ls".into() }]);
        assert_eq!(receive(&mut a, json!({"type":"tool_execution_end","toolCallId":"c","isError":true})), vec![Event::ToolEnd { ok: false }]);
        let end = json!({"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"Done."}],"usage":{"input":1,"output":2,"cacheRead":0,"cacheWrite":0,"cost":{"total":0.5}}}});
        assert_eq!(receive(&mut a, end), vec![Event::Usage { tokens: 3, cost: Some(0.5) }]);
        let events = receive(&mut a, json!({"type":"agent_end","messages":[]}));
        assert_eq!(events, vec![Event::Summary { text: "Done.".into() }, Event::TurnDone, Event::Prompt { text: "next".into() }]);
    }

    #[test]
    fn extension_dialogs_are_asks() {
        let (_, mut a) = start("pi", Path::new("/repo"), "", "", None, false);
        a.prompt("go".into());
        sent(&mut a);
        let confirm = json!({"type":"extension_ui_request","id":"u1","method":"confirm","title":"Run rm?","message":"rm -rf build"});
        assert_eq!(receive(&mut a, confirm), vec![Event::PermissionRequest { tool: "Confirm".into(), detail: "Run rm?: rm -rf build".into() }]);
        a.answer(false, "").unwrap();
        assert_eq!(sent(&mut a)[0], json!({"type":"extension_ui_response","id":"u1","confirmed":false}));
        let select = json!({"type":"extension_ui_request","id":"u2","method":"select","title":"Which?","options":["a","b"]});
        assert_eq!(receive(&mut a, select), vec![Event::Question { text: "Which?".into(), options: vec!["a".into(), "b".into()] }]);
        assert!(a.prompt("b".into()).is_empty());
        assert_eq!(sent(&mut a)[0], json!({"type":"extension_ui_response","id":"u2","value":"b"}));
        receive(&mut a, json!({"type":"extension_ui_request","id":"u3","method":"input","title":"Name?"}));
        a.cancel();
        let out = sent(&mut a);
        assert_eq!(out[0], json!({"type":"extension_ui_response","id":"u3","cancelled":true}));
        assert_eq!(out[1], json!({"type":"abort"}));
    }
}

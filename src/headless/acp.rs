//! The Agent Client Protocol (JSON-RPC on stdio), as `gemini --acp` speaks it.
//!
//! arranger offers no client file system or terminal, so the agent uses its own tools. Its
//! stream doesn't repeat the user's prompts; the daemon logs them for the chat.

use super::{Ask, Core, Transport, error_message, string};
use crate::proto::{Event, ToolKind};
use serde_json::{Value, json};
use std::path::Path;

const INITIALIZE: u64 = 0;
const SESSION: u64 = 1;

pub fn gemini_argv(model: &str) -> Vec<String> {
    let mut argv = vec!["--acp".to_string()];
    if !model.is_empty() {
        argv.extend(["--model".into(), model.into()]);
    }
    argv
}

pub struct Acp {
    c: Core,
    /// The session to carry on, if any.
    from: String,
    /// Loading a session replays its history, which the chat log already has.
    replaying: bool,
}

impl Acp {
    pub fn new(cwd: &Path, session: Option<&str>) -> Self {
        let mut c = Acp { c: Core::new(cwd), from: session.unwrap_or_default().into(), replaying: false };
        let capabilities = json!({"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false});
        let info = json!({"name":"arranger","title":"Arranger","version":env!("CARGO_PKG_VERSION")});
        c.rpc(INITIALIZE, "initialize", json!({"protocolVersion":1,"clientCapabilities":capabilities,"clientInfo":info}));
        c
    }

    fn rpc(&mut self, id: u64, method: &str, params: Value) {
        self.c.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}));
    }

    fn start_next(&mut self) -> Vec<Event> {
        let Some(text) = self.c.next_prompt() else { return Vec::new() };
        let id = self.c.id();
        self.rpc(id, "session/prompt", json!({"sessionId":self.c.session,"prompt":[{"type":"text","text":text}]}));
        vec![Event::Prompt { text }]
    }

    /// An Allow once action must never select a persistent permission grant.
    fn choose(&mut self, ask: &Ask, allow: bool) {
        let kinds: &[&str] = if allow { &["allow_once"] } else { &["reject_once", "reject_always"] };
        let options = ask.params["options"].as_array().cloned().unwrap_or_default();
        let option = kinds.iter().find_map(|k| options.iter().find(|o| o["kind"] == *k));
        let outcome = option.map_or(json!({"outcome":"cancelled"}), |o| json!({"outcome":"selected","optionId":o["optionId"]}));
        self.c.send(json!({"jsonrpc":"2.0","id":ask.id,"result":{"outcome":outcome}}));
    }
}

/// ACP's tool kinds as arranger's.
pub(crate) fn tool_kind(kind: &str) -> ToolKind {
    match kind {
        "edit" | "delete" | "move" => ToolKind::Edit,
        "read" => ToolKind::Read,
        "execute" => ToolKind::Bash,
        "search" => ToolKind::Search,
        "fetch" => ToolKind::Web,
        _ => ToolKind::Other,
    }
}

impl Transport for Acp {
    fn core(&self) -> &Core {
        &self.c
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.c
    }

    fn echoes(&self) -> bool {
        false
    }

    fn prompt(&mut self, text: String) -> Vec<Event> {
        self.c.prompts.push_back(text);
        self.start_next()
    }

    fn answer(&mut self, allow: bool, _message: &str) -> Result<(), String> {
        let Some(ask) = self.c.asks.pop_front() else { return Err("the agent isn't waiting for an answer".into()) };
        self.choose(&ask, allow);
        Ok(())
    }

    fn cancel(&mut self) {
        self.c.prompts.clear();
        while let Some(ask) = self.c.asks.pop_front() {
            self.c.send(json!({"jsonrpc":"2.0","id":ask.id,"result":{"outcome":{"outcome":"cancelled"}}}));
        }
        if self.c.busy {
            self.c.send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":self.c.session}}));
        }
    }

    fn line(&mut self, v: Value, events: &mut Vec<Event>) -> bool {
        let method = v["method"].as_str().unwrap_or("").to_string();
        if method.is_empty() {
            if let Some(error) = v.get("error") {
                let message = error_message(error, "gemini");
                if v["id"] == INITIALIZE || v["id"] == SESSION {
                    events.push(Event::Error { message: format!("gemini didn't start: {message}") });
                } else if self.c.busy {
                    self.c.done(Some(message), events);
                }
                return true;
            }
            let r = &v["result"];
            if v["id"] == INITIALIZE {
                let caps = &r["agentCapabilities"];
                let cwd = self.c.cwd.clone();
                let mut params = json!({"cwd":cwd,"mcpServers":[]});
                // Resume carries on without replaying; load replays, which the log already has.
                let method = if self.from.is_empty() {
                    "session/new"
                } else if !caps["sessionCapabilities"]["resume"].is_null() {
                    params["sessionId"] = json!(self.from);
                    "session/resume"
                } else if caps["loadSession"] == true {
                    params["sessionId"] = json!(self.from);
                    self.replaying = true;
                    "session/load"
                } else {
                    events.push(Event::Error { message: "gemini can't carry on its last session; starting a new one".into() });
                    "session/new"
                };
                self.rpc(SESSION, method, params);
            } else if v["id"] == SESSION {
                self.c.session = r["sessionId"].as_str().map_or_else(|| self.from.clone(), str::to_string);
                self.replaying = false;
                self.c.ready = !self.c.session.is_empty();
                events.extend(self.start_next());
            } else if let Some(reason) = r["stopReason"].as_str() {
                let error = matches!(reason, "refusal" | "max_tokens" | "max_turn_requests").then(|| format!("gemini stopped: {}", reason.replace('_', " ")));
                self.c.done(error, events);
                events.extend(self.start_next());
                return true;
            }
            return false;
        }
        if let Some(id) = v.get("id").cloned() {
            if method == "session/request_permission" {
                let call = &v["params"]["toolCall"];
                let title = string(&call["title"]);
                let ask = Ask::permission(id, &method, v["params"].clone(), &string(&call["kind"]), title);
                self.c.ask(ask, events);
            } else {
                self.c.send(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":format!("arranger doesn't offer {method}")}}));
            }
            return false;
        }
        if method != "session/update" {
            return false;
        }
        if self.replaying {
            return false;
        }
        let u = &v["params"]["update"];
        match u["sessionUpdate"].as_str().unwrap_or("") {
            "agent_message_chunk" => {
                if let Some(text) = u["content"]["text"].as_str() {
                    self.c.last_text.push_str(text);
                }
            }
            "tool_call" => {
                self.c.last_text.clear(); // the summary is what it says after its last tool
                events.push(Event::ToolStart { kind: tool_kind(u["kind"].as_str().unwrap_or("")), target: string(&u["title"]) });
            }
            "tool_call_update" if matches!(u["status"].as_str(), Some("completed" | "failed")) => events.push(Event::ToolEnd { ok: u["status"] == "completed" }),
            _ => {}
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{receive, sent};
    use super::super::{Agent, start};
    use super::*;

    fn started(session: Option<&str>, caps: Value) -> (Agent, Value) {
        let (argv, mut a) = start("gemini", Path::new("/repo"), "pro", "", session, false);
        assert_eq!(argv, ["gemini", "--acp", "--model", "pro"]);
        assert_eq!(sent(&mut a)[0]["method"], "initialize");
        receive(&mut a, json!({"jsonrpc":"2.0","id":0,"result":{"protocolVersion":1,"agentCapabilities":caps}}));
        let open = sent(&mut a)[0].clone();
        (a, open)
    }

    #[test]
    fn sessions_start_resume_or_load() {
        let (mut a, open) = started(None, json!({}));
        assert_eq!(open["method"], "session/new");
        receive(&mut a, json!({"jsonrpc":"2.0","id":1,"result":{"sessionId":"g1"}}));
        assert_eq!(a.session(), "g1");
        let (_, open) = started(Some("g1"), json!({"sessionCapabilities":{"resume":{}}}));
        assert_eq!((open["method"].as_str(), open["params"]["sessionId"].as_str()), (Some("session/resume"), Some("g1")));
        let (mut a, open) = started(Some("g1"), json!({"loadSession":true}));
        assert_eq!(open["method"], "session/load");
        let replay = json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"g1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"old"}}}});
        assert!(a.feed(format!("{replay}\n").as_bytes()).1.is_empty(), "replayed history stays out of the log");
        receive(&mut a, json!({"jsonrpc":"2.0","id":1,"result":{}}));
        assert_eq!(a.session(), "g1");
        assert!(!a.feed(format!("{replay}\n").as_bytes()).1.is_empty());
    }

    #[test]
    fn a_turn_with_a_permission_ask() {
        let (mut a, _) = started(None, json!({}));
        assert!(a.prompt("go".into()).is_empty(), "waits for its session");
        let events = receive(&mut a, json!({"jsonrpc":"2.0","id":1,"result":{"sessionId":"g1"}}));
        assert_eq!(events, vec![Event::Prompt { text: "go".into() }]);
        let prompt = sent(&mut a)[0].clone();
        assert_eq!(prompt["params"], json!({"sessionId":"g1","prompt":[{"type":"text","text":"go"}]}));
        let call = json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"g1","update":{"sessionUpdate":"tool_call","toolCallId":"t","title":"npm test","kind":"execute","status":"pending"}}});
        assert_eq!(receive(&mut a, call), vec![Event::ToolStart { kind: ToolKind::Bash, target: "npm test".into() }]);
        let options = json!([{"optionId":"y","name":"Allow","kind":"allow_once"},{"optionId":"n","name":"Reject","kind":"reject_once"}]);
        let ask = json!({"jsonrpc":"2.0","id":7,"method":"session/request_permission","params":{"sessionId":"g1","toolCall":{"toolCallId":"t","title":"npm test","kind":"execute"},"options":options}});
        assert_eq!(receive(&mut a, ask), vec![Event::PermissionRequest { tool: "execute".into(), detail: "npm test".into() }]);
        a.answer(true, "").unwrap();
        assert_eq!(sent(&mut a)[0]["result"], json!({"outcome":{"outcome":"selected","optionId":"y"}}));
        let update = |s: &str| json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"g1","update":{"sessionUpdate":"tool_call_update","toolCallId":"t","status":s}}});
        assert!(receive(&mut a, update("in_progress")).is_empty());
        assert_eq!(receive(&mut a, update("completed")), vec![Event::ToolEnd { ok: true }]);
        for chunk in ["All ", "good."] {
            receive(&mut a, json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"g1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":chunk}}}}));
        }
        let events = receive(&mut a, json!({"jsonrpc":"2.0","id":prompt["id"],"result":{"stopReason":"end_turn"}}));
        assert_eq!(events, vec![Event::Summary { text: "All good.".into() }, Event::TurnDone]);
    }

    #[test]
    fn cancel_and_unoffered_requests() {
        let (mut a, _) = started(None, json!({}));
        receive(&mut a, json!({"jsonrpc":"2.0","id":1,"result":{"sessionId":"g1"}}));
        a.prompt("go".into());
        sent(&mut a);
        receive(&mut a, json!({"jsonrpc":"2.0","id":3,"method":"fs/read_text_file","params":{}}));
        assert_eq!(sent(&mut a)[0]["error"]["code"], -32601);
        receive(&mut a, json!({"jsonrpc":"2.0","id":8,"method":"session/request_permission","params":{"toolCall":{},"options":[]}}));
        a.cancel();
        let out = sent(&mut a);
        assert_eq!(out[0]["result"]["outcome"]["outcome"], "cancelled");
        assert_eq!(out[1]["method"], "session/cancel");
    }

    #[test]
    fn allow_once_never_grants_a_persistent_permission() {
        let (mut a, _) = started(None, json!({}));
        receive(&mut a, json!({"jsonrpc":"2.0","id":8,"method":"session/request_permission","params":{"toolCall":{},"options":[{"optionId":"always","kind":"allow_always"}]}}));
        a.answer(true, "").unwrap();
        assert_eq!(sent(&mut a)[0]["result"]["outcome"]["outcome"], "cancelled");
    }
}

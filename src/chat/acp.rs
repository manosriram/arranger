//! ACP session updates as chat items. Message and thought chunks carry no id: consecutive chunks
//! grow one item until something else comes. The user's prompts are the daemon's
//! `arranger_user` lines, since the agent doesn't repeat them.

use super::{Chat, string, text_diff};
use crate::headless::acp_tool_kind;
use crate::proto::{ChatItem, DiffLine};
use serde_json::Value;

impl Chat {
    pub(super) fn acp(&mut self, v: &Value) {
        if v.get("error").is_some_and(|e| !e.is_null()) {
            return self.notice(string(&v["error"]["message"]), true);
        }
        if v["result"]["stopReason"].is_string() {
            self.stop_tools();
        }
        match v["result"]["stopReason"].as_str() {
            Some("cancelled") => return self.notice("Interrupted".into(), false),
            Some(r @ ("refusal" | "max_tokens" | "max_turn_requests")) => return self.notice(format!("gemini stopped: {}", r.replace('_', " ")), true),
            Some(_) => return self.end_chunks(),
            None => {}
        }
        if v["method"] != "session/update" {
            return;
        }
        let u = &v["params"]["update"];
        match u["sessionUpdate"].as_str().unwrap_or("") {
            kind @ ("agent_message_chunk" | "agent_thought_chunk") => {
                let thinking = kind == "agent_thought_chunk";
                // A switch between thinking and text starts a new item.
                let other = if thinking { "acp-text" } else { "acp-thought" };
                self.blocks.remove(other);
                let id = if thinking { "acp-thought" } else { "acp-text" };
                self.grow(id.into(), thinking, u["content"]["text"].as_str().unwrap_or(""));
            }
            "user_message_chunk" => self.user_text(string(&u["content"]["text"])),
            "tool_call" => {
                self.end_chunks();
                let id = string(&u["toolCallId"]);
                let (diff, output) = content(u);
                let input = if u["rawInput"].is_null() { String::new() } else { serde_json::to_string_pretty(&u["rawInput"]).unwrap_or_default() };
                let kind = acp_tool_kind(u["kind"].as_str().unwrap_or(""));
                let tool = ChatItem::Tool { id: id.clone(), name: string(&u["kind"]), kind, target: string(&u["title"]), input, output, ok: None, diff };
                self.tool_call(id, tool);
            }
            "tool_call_update" => {
                let id = string(&u["toolCallId"]);
                let Some(&i) = self.tools.get(&id) else { return };
                let (diff, out) = content(u);
                let ok = match u["status"].as_str() {
                    Some("completed") => Some(true),
                    Some("failed") => Some(false),
                    _ => None,
                };
                let title = u["title"].as_str().map(str::to_string);
                self.set(i, |item| {
                    if let ChatItem::Tool { target, diff: d, .. } = item {
                        if let Some(t) = title {
                            *target = t;
                        }
                        if !diff.is_empty() {
                            *d = diff;
                        }
                    }
                });
                self.tool_done(&id, out, ok);
            }
            "plan" => {
                let items = u["entries"].as_array().into_iter().flatten().map(|e| (string(&e["content"]), string(&e["status"]))).collect();
                self.todos("acp-plan".into(), items);
            }
            _ => {}
        }
    }

    /// The streamed text so far is done; the next chunk starts a new item.
    fn end_chunks(&mut self) {
        self.blocks.remove("acp-text");
        self.blocks.remove("acp-thought");
    }
}

/// A tool call's content: its diffs, and its text output.
fn content(u: &Value) -> (Vec<DiffLine>, String) {
    let (mut diff, mut text) = (Vec::new(), Vec::new());
    for c in u["content"].as_array().into_iter().flatten() {
        match c["type"].as_str().unwrap_or("") {
            "diff" => diff.extend(text_diff(c["oldText"].as_str().unwrap_or(""), &string(&c["newText"]))),
            "content" => text.extend(c["content"]["text"].as_str().map(str::to_string)),
            _ => {}
        }
    }
    (diff, text.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::ToolKind;
    use serde_json::json;

    fn update(u: Value) -> Value {
        json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"g","update":u}})
    }

    #[test]
    fn chunks_tools_and_plans() {
        let mut c = Chat::new("gemini", "/repo");
        for v in [
            json!({"type":"arranger_user","text":"fix it"}),
            update(json!({"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"hm"}})),
            update(json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"On "}})),
            update(json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"it"}})),
            update(json!({"sessionUpdate":"tool_call","toolCallId":"t","title":"Edit a.rs","kind":"edit","status":"pending"})),
            update(json!({"sessionUpdate":"tool_call_update","toolCallId":"t","status":"completed","content":[{"type":"diff","path":"/repo/a.rs","oldText":"a\n","newText":"b\n"}]})),
            update(json!({"sessionUpdate":"plan","entries":[{"content":"Fix","status":"in_progress"}]})),
            update(json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Done."}})),
            json!({"jsonrpc":"2.0","id":11,"result":{"stopReason":"cancelled"}}),
        ] {
            c.feed(format!("{v}\n").as_bytes());
        }
        assert_eq!(c.items[0], ChatItem::User { text: "fix it".into() });
        assert_eq!(c.items[1], ChatItem::Thinking { text: "hm".into() });
        assert_eq!(c.items[2], ChatItem::Assistant { text: "On it".into() });
        let ChatItem::Tool { kind, target, ok, diff, .. } = &c.items[3] else { panic!("{:?}", c.items[3]) };
        assert_eq!((*kind, target.as_str(), *ok, diff.len()), (ToolKind::Edit, "Edit a.rs", Some(true), 2));
        assert_eq!(c.items[4], ChatItem::Todos { items: vec![("Fix".into(), "in_progress".into())] });
        assert_eq!(c.items[5], ChatItem::Assistant { text: "Done.".into() });
        assert_eq!(c.items[6], ChatItem::Notice { text: "Interrupted".into(), error: false });
    }
}

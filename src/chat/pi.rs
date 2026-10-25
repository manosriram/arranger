//! pi's RPC events as chat items: messages stream block by block (`contentIndex`), tools run
//! between `tool_execution_start` and `_end`.

use super::{Chat, string, text_diff};
use crate::proto::ChatItem;
use serde_json::Value;

impl Chat {
    pub(super) fn pi(&mut self, v: &Value) {
        let m = &v["message"];
        match v["type"].as_str().unwrap_or("") {
            "message_start" if m["role"] == "user" => {
                let content = &m["content"];
                let text = content.as_str().map(str::to_string).unwrap_or_else(|| super::texts(content));
                self.user_text(text);
            }
            "message_start" if m["role"] == "assistant" => self.message = format!("pi-{}", self.items.len()),
            "message_update" => {
                let e = &v["assistantMessageEvent"];
                let id = format!("{}-{}", self.message, e["contentIndex"].as_u64().unwrap_or(0));
                match e["type"].as_str().unwrap_or("") {
                    "text_delta" => self.grow(id, false, e["delta"].as_str().unwrap_or("")),
                    "thinking_delta" => self.grow(id, true, e["delta"].as_str().unwrap_or("")),
                    _ => {}
                }
            }
            "message_end" if m["role"] == "assistant" => {
                for (i, block) in m["content"].as_array().into_iter().flatten().enumerate() {
                    let id = format!("{}-{i}", self.message);
                    match block["type"].as_str().unwrap_or("") {
                        "text" => self.complete(id, false, string(&block["text"])),
                        "thinking" => self.complete(id, true, string(&block["thinking"])),
                        _ => {}
                    }
                }
                match m["stopReason"].as_str() {
                    Some("error") => self.notice(m["errorMessage"].as_str().unwrap_or("pi stopped on an error").into(), true),
                    Some("aborted") => self.notice("Interrupted".into(), false),
                    _ => {}
                }
            }
            "tool_execution_start" => {
                let (name, args) = (v["toolName"].as_str().unwrap_or("tool"), &v["args"]);
                let (kind, target) = self.kind_target(name, args);
                let diff = match name {
                    "edit" => text_diff(&string(&args["oldText"]), &string(&args["newText"])),
                    "write" => text_diff("", &string(&args["content"])),
                    _ => Vec::new(),
                };
                let input = serde_json::to_string_pretty(args).unwrap_or_default();
                let tool = ChatItem::Tool { id: string(&v["toolCallId"]), name: name.into(), kind, target, input, output: String::new(), ok: None, diff };
                self.tool_call(string(&v["toolCallId"]), tool);
            }
            "tool_execution_end" => {
                let content = &v["result"]["content"];
                let out = content.as_str().map(str::to_string).unwrap_or_else(|| super::texts(content));
                self.tool_done(&string(&v["toolCallId"]), out, Some(v["isError"] != true));
            }
            "agent_end" => self.stop_tools(),
            "auto_retry_end" if v["success"] == false => self.notice(string(&v["finalError"]), true),
            "compaction_end" if !v["result"].is_null() => self.notice("Conversation compacted".into(), false),
            "response" if v["success"] == false => self.notice(string(&v["error"]), true),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::ToolKind;
    use serde_json::json;

    #[test]
    fn messages_stream_by_block_and_tools_get_results() {
        let mut c = Chat::new("pi", "/repo");
        let update = |kind: &str, i: u64, delta: &str| json!({"type":"message_update","assistantMessageEvent":{"type":kind,"contentIndex":i,"delta":delta}});
        for v in [
            json!({"type":"message_start","message":{"role":"user","content":"fix it"}}),
            json!({"type":"message_start","message":{"role":"assistant","content":[]}}),
            update("thinking_delta", 0, "hm"),
            update("text_delta", 1, "Fix"),
            update("text_delta", 1, "ing"),
            json!({"type":"message_end","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hmm"},{"type":"text","text":"Fixing."},{"type":"toolCall","id":"c","name":"edit"}]}}),
            json!({"type":"tool_execution_start","toolCallId":"c","toolName":"edit","args":{"path":"/repo/a.rs","oldText":"a\n","newText":"b\n"}}),
            json!({"type":"tool_execution_end","toolCallId":"c","result":{"content":[{"type":"text","text":"Edited"}]},"isError":false}),
            json!({"type":"message_start","message":{"role":"assistant","content":[]}}),
            json!({"type":"message_end","message":{"role":"assistant","content":[],"stopReason":"aborted"}}),
        ] {
            c.feed(format!("{v}\n").as_bytes());
        }
        assert_eq!(c.items[0], ChatItem::User { text: "fix it".into() });
        assert_eq!(c.items[1], ChatItem::Thinking { text: "hmm".into() });
        assert_eq!(c.items[2], ChatItem::Assistant { text: "Fixing.".into() });
        let ChatItem::Tool { kind, target, output, ok, diff, .. } = &c.items[3] else { panic!("{:?}", c.items[3]) };
        assert_eq!((*kind, target.as_str(), output.as_str(), *ok, diff.len()), (ToolKind::Edit, "a.rs", "Edited", Some(true), 2));
        assert_eq!(c.items[4], ChatItem::Notice { text: "Interrupted".into(), error: false });
    }
}

//! codex app-server notifications as chat items: thread items start and complete, messages and
//! reasoning summaries stream as deltas, the plan comes whole on each update.

use super::{Chat, MAX_WRITE_LINES, string, unified};
use crate::headless::codex_command;
use crate::proto::{ChatItem, DiffLine, ToolKind};
use serde_json::Value;

impl Chat {
    pub(super) fn codex(&mut self, v: &Value) {
        let p = &v["params"];
        match v["method"].as_str().unwrap_or("") {
            "item/started" => self.codex_item(&p["item"], true),
            "item/completed" => self.codex_item(&p["item"], false),
            "item/agentMessage/delta" => self.grow(string(&p["itemId"]), false, p["delta"].as_str().unwrap_or("")),
            "item/reasoning/summaryTextDelta" => self.grow(string(&p["itemId"]), true, p["delta"].as_str().unwrap_or("")),
            "item/reasoning/summaryPartAdded" => self.grow(string(&p["itemId"]), true, "\n\n"),
            "turn/plan/updated" => {
                let items = p["plan"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|s| (string(&s["step"]), if s["status"] == "inProgress" { "in_progress".into() } else { string(&s["status"]) }))
                    .collect();
                self.todos(format!("plan-{}", string(&p["turnId"])), items);
            }
            "turn/completed" => {
                self.stop_tools();
                let turn = &p["turn"];
                match turn["status"].as_str() {
                    Some("failed") => {
                        let e = &turn["error"];
                        self.notice(if e["message"].is_string() { string(&e["message"]) } else { "codex stopped on an error".into() }, true);
                    }
                    Some("interrupted") => self.notice("Interrupted".into(), false),
                    _ => {}
                }
            }
            "error" if p["willRetry"] != true => self.notice(string(&p["error"]["message"]), true),
            "thread/compacted" => self.notice("Conversation compacted".into(), false),
            _ => {}
        }
    }

    fn codex_item(&mut self, item: &Value, started: bool) {
        let id = string(&item["id"]);
        match item["type"].as_str().unwrap_or("") {
            "userMessage" if started => {
                let text = item["content"].as_array().into_iter().flatten().filter_map(|c| c["text"].as_str()).collect::<Vec<_>>().join("\n");
                self.user_text(text);
            }
            "agentMessage" if !started => self.complete(id, false, string(&item["text"])),
            "reasoning" if !started => {
                let summary = item["summary"].as_array().into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>().join("\n\n");
                self.complete(id, true, summary);
            }
            "commandExecution" if started => {
                let command = codex_command(item);
                let tool = ChatItem::Tool { id: id.clone(), name: "Bash".into(), kind: ToolKind::Bash, target: command.clone(), input: command, output: String::new(), ok: None, diff: Vec::new() };
                self.tool_call(id, tool);
            }
            "commandExecution" => {
                let ok = item["status"] == "completed" && item["exitCode"].as_i64().is_none_or(|c| c == 0);
                self.tool_done(&id, string(&item["aggregatedOutput"]), Some(ok));
            }
            "fileChange" if started => {
                let changes = item["changes"].as_array().cloned().unwrap_or_default();
                let cwd = self.cwd.clone();
                let paths: Vec<String> = changes.iter().map(|c| string(&c["path"])).map(|p| p.strip_prefix(&cwd).map_or(p.clone(), |r| r.trim_start_matches('/').to_string())).collect();
                let diff = changes.iter().flat_map(change_diff).collect();
                let tool = ChatItem::Tool { id: id.clone(), name: "Edit".into(), kind: ToolKind::Edit, target: paths.join(", "), input: paths.join("\n"), output: String::new(), ok: None, diff };
                self.tool_call(id, tool);
            }
            "fileChange" => self.tool_done(&id, String::new(), Some(item["status"] == "completed")),
            "mcpToolCall" | "dynamicToolCall" if started => {
                let server = item["server"].as_str().or(item["namespace"].as_str()).unwrap_or("");
                let name = if server.is_empty() { string(&item["tool"]) } else { format!("{server}.{}", string(&item["tool"])) };
                let input = serde_json::to_string_pretty(&item["arguments"]).unwrap_or_default();
                let tool = ChatItem::Tool { id: id.clone(), name: name.clone(), kind: ToolKind::Other, target: name, input, output: String::new(), ok: None, diff: Vec::new() };
                self.tool_call(id, tool);
            }
            "mcpToolCall" | "dynamicToolCall" => {
                let result = if item["error"].is_object() { &item["error"] } else { &item["result"] };
                let blocks = result["content"].as_array().or(item["contentItems"].as_array()).cloned().unwrap_or_default();
                let text: Vec<String> = blocks.iter().filter_map(|c| c["text"].as_str().map(str::to_string)).collect();
                let out = if text.is_empty() && !result.is_null() { result.to_string() } else { text.join("\n") };
                self.tool_done(&id, out, Some(item["status"] == "completed"));
            }
            "webSearch" if started => {
                let query = string(&item["query"]);
                let tool = ChatItem::Tool { id: id.clone(), name: "WebSearch".into(), kind: ToolKind::Web, target: query.clone(), input: query, output: String::new(), ok: None, diff: Vec::new() };
                self.tool_call(id, tool);
            }
            "webSearch" => self.tool_done(&id, String::new(), Some(true)),
            "contextCompaction" if !started => self.notice("Conversation compacted".into(), false),
            _ => {}
        }
    }
}

/// One file's change: an added or deleted file's text, or an update's unified diff.
fn change_diff(change: &Value) -> Vec<DiffLine> {
    let diff = string(&change["diff"]);
    match change["kind"]["type"].as_str() {
        Some(kind @ ("add" | "delete")) => {
            let (sign, add) = if kind == "add" { ('+', true) } else { ('-', false) };
            diff.split_inclusive('\n')
                .take(MAX_WRITE_LINES)
                .enumerate()
                .map(|(i, l)| DiffLine { kind: sign, old: if add { 0 } else { i as u32 + 1 }, new: if add { i as u32 + 1 } else { 0 }, text: l.to_string() })
                .collect()
        }
        _ => unified(&diff),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn feed(c: &mut Chat, lines: &[Value]) {
        for v in lines {
            c.feed(format!("{v}\n").as_bytes());
        }
    }

    fn note(method: &str, params: Value) -> Value {
        json!({"method":method,"params":params})
    }

    #[test]
    fn a_turn_of_messages_commands_and_edits() {
        let mut c = Chat::new("/usr/bin/codex", "/repo");
        let cmd = json!({"type":"commandExecution","id":"c1","command":"/bin/zsh -lc 'cat a'","commandActions":[{"command":"cat a"}],"status":"inProgress"});
        let edit = json!({"type":"fileChange","id":"f1","status":"inProgress","changes":[
            {"path":"/repo/src/a.rs","kind":{"type":"update","move_path":null},"diff":"@@ -3,2 +3,2 @@\n let a = 1;\n-let b = 2;\n+let b = 3;\n"},
            {"path":"/repo/new.txt","kind":{"type":"add"},"diff":"hi\n"}]});
        feed(&mut c, &[
            note("item/started", json!({"item":{"type":"userMessage","id":"u","content":[{"type":"text","text":"fix it"}]}})),
            note("item/completed", json!({"item":{"type":"userMessage","id":"u","content":[{"type":"text","text":"fix it"}]}})),
            note("item/reasoning/summaryTextDelta", json!({"itemId":"r","delta":"Looking"})),
            note("item/completed", json!({"item":{"type":"reasoning","id":"r","summary":["Looking at a"],"content":[]}})),
            note("item/started", json!({"item":{"type":"agentMessage","id":"m","text":""}})),
            note("item/agentMessage/delta", json!({"itemId":"m","delta":"On "})),
            note("item/agentMessage/delta", json!({"itemId":"m","delta":"it"})),
            note("item/started", json!({"item":cmd})),
            note("item/completed", json!({"item":{"type":"commandExecution","id":"c1","status":"completed","exitCode":1,"aggregatedOutput":"nope"}})),
            note("item/started", json!({"item":edit})),
            note("item/completed", json!({"item":{"type":"fileChange","id":"f1","status":"completed"}})),
            note("turn/plan/updated", json!({"turnId":"t","plan":[{"step":"Read","status":"completed"},{"step":"Fix","status":"inProgress"}]})),
            note("turn/plan/updated", json!({"turnId":"t","plan":[{"step":"Read","status":"completed"},{"step":"Fix","status":"completed"}]})),
            note("item/completed", json!({"item":{"type":"agentMessage","id":"m","text":"On it."}})),
            note("turn/completed", json!({"turn":{"status":"interrupted"}})),
        ]);
        assert_eq!(c.items[0], ChatItem::User { text: "fix it".into() });
        assert_eq!(c.items[1], ChatItem::Thinking { text: "Looking at a".into() });
        assert_eq!(c.items[2], ChatItem::Assistant { text: "On it.".into() });
        let ChatItem::Tool { target, output, ok, .. } = &c.items[3] else { panic!("{:?}", c.items[3]) };
        assert_eq!((target.as_str(), output.as_str(), *ok), ("cat a", "nope", Some(false)));
        let ChatItem::Tool { kind, target, diff, ok, .. } = &c.items[4] else { panic!() };
        assert_eq!((*kind, target.as_str(), *ok), (ToolKind::Edit, "src/a.rs, new.txt", Some(true)));
        let shown: Vec<(char, u32, u32, &str)> = diff.iter().map(|l| (l.kind, l.old, l.new, l.text.as_str())).collect();
        assert_eq!(shown, vec![(' ', 3, 3, "let a = 1;\n"), ('-', 4, 0, "let b = 2;\n"), ('+', 0, 4, "let b = 3;\n"), ('+', 0, 1, "hi\n")]);
        assert_eq!(c.items[5], ChatItem::Todos { items: vec![("Read".into(), "completed".into()), ("Fix".into(), "completed".into())] });
        assert_eq!(c.items[6], ChatItem::Notice { text: "Interrupted".into(), error: false });
        assert_eq!(c.items.len(), 7);
    }
}

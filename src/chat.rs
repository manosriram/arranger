//! A headless task's conversation as `ChatItem`s, built from its agent's output lines: what
//! the daemon serves to chat views, live and (from the task's log) after it has ended.
//!
//! Streamed deltas grow a text or thinking item in place; the complete message that follows
//! replaces what streamed. Tool results land on their call. claude's stream-json is read here,
//! the other agents' protocols in `chat/`.

mod acp;
mod codex;
mod pi;

use crate::adapter;
use crate::proto::{ChatItem, DiffLine, Event, ToolKind};
use crate::review;
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};

/// Tool output kept per call; the rest is cut from the middle.
const MAX_OUTPUT: usize = 16 * 1024;
/// Lines of a written file shown as its diff.
const MAX_WRITE_LINES: usize = 400;

#[derive(Default)]
pub struct Chat {
    pub items: Vec<ChatItem>,
    /// The agent's kind, which says how to read its lines.
    agent: String,
    /// Items changed or added since `take_changed`.
    changed: BTreeSet<usize>,
    cwd: String,
    input: crate::security::JsonLines,
    received: usize,
    /// The message streaming now, and its blocks' items by stream index.
    message: String,
    streamed: HashMap<u64, usize>,
    /// Streamed items per message, oldest first, that its complete blocks haven't replaced yet.
    pending: HashMap<String, Vec<usize>>,
    tools: HashMap<String, usize>,
    /// Text, thinking and plan items still growing, by the agent's id for them.
    blocks: HashMap<String, usize>,
}

impl Chat {
    pub fn new(agent: &str, cwd: &str) -> Self {
        Chat { agent: adapter::kind(agent).into(), cwd: cwd.into(), ..Chat::default() }
    }

    /// Indices changed or added since the last call, in order.
    pub fn take_changed(&mut self) -> Vec<usize> {
        std::mem::take(&mut self.changed).into_iter().collect()
    }

    /// Raw stdout bytes; only complete lines count.
    pub fn feed(&mut self, bytes: &[u8]) {
        if self.received > crate::security::MAX_LOG { return; }
        self.received = self.received.saturating_add(bytes.len());
        if self.received > crate::security::MAX_LOG {
            self.notice("Chat reached its 64 MiB display limit; start another task to continue.".into(), true);
            return;
        }
        let (lines, oversized) = self.input.feed(bytes);
        if oversized { self.notice("Skipped an oversized JSON log record.".into(), true); }
        for line in lines {
            // Older logs start with a terminal size mark glued to the first line.
            let json = line.iter().position(|&b| b == b'{').map_or(&line[..0], |i| &line[i..]);
            if let Ok(v) = serde_json::from_slice::<Value>(json) {
                self.message(&v);
            }
        }
    }

    fn push(&mut self, item: ChatItem) -> usize {
        self.items.push(item);
        self.changed.insert(self.items.len() - 1);
        self.items.len() - 1
    }

    fn set(&mut self, i: usize, f: impl FnOnce(&mut ChatItem)) {
        f(&mut self.items[i]);
        self.changed.insert(i);
    }

    /// An error or notice, once: arranger and claude may both say a turn was interrupted.
    pub fn notice(&mut self, text: String, error: bool) {
        let item = ChatItem::Notice { text, error };
        if self.items.last() != Some(&item) {
            self.push(item);
        }
    }

    pub fn message(&mut self, v: &Value) {
        match v["type"].as_str().unwrap_or("") {
            // The daemon's own lines, logged among the agent's.
            "arranger_notice" => return self.notice(string(&v["text"]), v["error"] == true),
            "arranger_user" => return self.user_text(string(&v["text"])),
            _ => {}
        }
        match self.agent.as_str() {
            "codex" => self.codex(v),
            "pi" => self.pi(v),
            "gemini" => self.acp(v),
            _ => self.claude(v),
        }
    }

    fn user_text(&mut self, text: String) {
        if !text.trim().is_empty() {
            self.push(ChatItem::User { text });
        }
    }

    /// Grows (or starts) the text or thinking item the agent calls `id`.
    fn grow(&mut self, id: String, thinking: bool, more: &str) {
        if more.is_empty() {
            return;
        }
        match self.blocks.get(&id) {
            Some(&i) => self.set(i, |item| match item {
                ChatItem::Assistant { text } | ChatItem::Thinking { text } => text.push_str(more),
                _ => {}
            }),
            None => {
                let text = more.to_string();
                let i = self.push(if thinking { ChatItem::Thinking { text } } else { ChatItem::Assistant { text } });
                self.blocks.insert(id, i);
            }
        }
    }

    /// Sets the text or thinking item `id` to its complete text, adding it if nothing streamed.
    fn complete(&mut self, id: String, thinking: bool, text: String) {
        let item = if thinking { ChatItem::Thinking { text } } else { ChatItem::Assistant { text } };
        match self.blocks.remove(&id) {
            Some(i) => self.set(i, |old| *old = item),
            None if matches!(&item, ChatItem::Assistant { text } | ChatItem::Thinking { text } if text.trim().is_empty()) => {}
            None => {
                self.push(item);
            }
        }
    }

    /// A tool call starting; `id` is the agent's, for its result.
    fn tool_call(&mut self, id: String, item: ChatItem) {
        let i = self.push(item);
        self.tools.insert(id, i);
    }

    /// A tool call's result, by the agent's id.
    fn tool_done(&mut self, id: &str, out: String, ok: Option<bool>) {
        let Some(&i) = self.tools.get(id) else { return };
        self.set(i, |item| {
            if let ChatItem::Tool { output, ok: o, .. } = item {
                if !out.is_empty() {
                    *output = clip(out);
                }
                if ok.is_some() {
                    *o = ok;
                }
            }
        });
    }

    /// The turn ended: calls still without a result never get one (it was stopped).
    fn stop_tools(&mut self) {
        let open: Vec<usize> = self.tools.values().copied().filter(|&i| matches!(self.items[i], ChatItem::Tool { ok: None, .. })).collect();
        for i in open {
            self.set(i, |item| {
                if let ChatItem::Tool { ok, .. } = item {
                    *ok = Some(false);
                }
            });
        }
        self.tools.clear();
    }

    /// The agent's plan, kept in one item per `id` (a turn) as it changes.
    fn todos(&mut self, id: String, items: Vec<(String, String)>) {
        match self.blocks.get(&id) {
            Some(&i) => self.set(i, |item| *item = ChatItem::Todos { items }),
            None => {
                let i = self.push(ChatItem::Todos { items });
                self.blocks.insert(id, i);
            }
        }
    }

    /// A tool item's kind and target, read the way the event stream reads it.
    fn kind_target(&self, name: &str, input: &Value) -> (ToolKind, String) {
        match adapter::tool_start(name, input, &self.cwd).into_iter().next() {
            Some(Event::ToolStart { kind, target }) => (kind, target),
            _ => (ToolKind::Other, String::new()),
        }
    }

    fn claude(&mut self, v: &Value) {
        // Subagents' own steps stay inside their Agent call.
        if !v["parent_tool_use_id"].is_null() {
            return;
        }
        if v["type"] == "result" {
            self.stop_tools();
        }
        match v["type"].as_str().unwrap_or("") {
            "stream_event" => self.stream(&v["event"]),
            "assistant" if v["isApiErrorMessage"] == true || v.get("error").is_some_and(|e| !e.is_null()) => {
                let text = texts(&v["message"]["content"]);
                self.notice(if text.is_empty() { string(&v["error"]) } else { text }, true);
            }
            "assistant" => {
                let id = string(&v["message"]["id"]);
                for block in v["message"]["content"].as_array().into_iter().flatten() {
                    self.block(&id, block);
                }
            }
            "user" => self.user(v),
            "result" if v["is_error"] == true && v["subtype"] != "error_during_execution" => {
                let errors = v.get("errors").filter(|e| !e.is_null()).unwrap_or(&v["result"]);
                self.notice(error_text(errors), true);
            }
            "system" if v["subtype"] == "compact_boundary" => self.notice("Conversation compacted".into(), false),
            _ => {}
        }
    }

    fn stream(&mut self, e: &Value) {
        match e["type"].as_str().unwrap_or("") {
            "message_start" => {
                self.message = string(&e["message"]["id"]);
                self.streamed.clear();
            }
            "content_block_start" => {
                let item = match e["content_block"]["type"].as_str() {
                    Some("text") => ChatItem::Assistant { text: String::new() },
                    Some("thinking") => ChatItem::Thinking { text: String::new() },
                    _ => return,
                };
                let i = self.push(item);
                self.streamed.insert(e["index"].as_u64().unwrap_or(0), i);
                self.pending.entry(self.message.clone()).or_default().push(i);
            }
            "content_block_delta" => {
                let Some(&i) = self.streamed.get(&e["index"].as_u64().unwrap_or(0)) else { return };
                let d = &e["delta"];
                let more = d["text"].as_str().or(d["thinking"].as_str()).unwrap_or("");
                if !more.is_empty() {
                    self.set(i, |item| match item {
                        ChatItem::Assistant { text } | ChatItem::Thinking { text } => text.push_str(more),
                        _ => {}
                    });
                }
            }
            _ => {}
        }
    }

    /// One block of a complete assistant message.
    fn block(&mut self, message: &str, b: &Value) {
        let kind = b["type"].as_str().unwrap_or("");
        let text = match kind {
            "text" => string(&b["text"]),
            "thinking" => string(&b["thinking"]),
            "tool_use" => return self.tool(b),
            _ => return,
        };
        let thinking = kind == "thinking";
        // The streamed item it completes: the oldest of this message's of the same kind.
        let items = &self.items;
        let streamed = self.pending.get_mut(message).and_then(|pending| {
            let at = pending.iter().position(|&i| matches!(items[i], ChatItem::Thinking { .. }) == thinking)?;
            Some(pending.remove(at))
        });
        let item = if thinking { ChatItem::Thinking { text } } else { ChatItem::Assistant { text } };
        match streamed {
            Some(i) => self.set(i, |old| *old = item),
            None if matches!(&item, ChatItem::Assistant { text } | ChatItem::Thinking { text } if text.is_empty()) => {}
            None => {
                self.push(item);
            }
        }
    }

    fn tool(&mut self, b: &Value) {
        let (name, input) = (b["name"].as_str().unwrap_or("tool"), &b["input"]);
        if name == "TodoWrite" {
            let items = input["todos"].as_array().into_iter().flatten().map(|t| (string(&t["content"]), string(&t["status"]))).collect();
            self.tool_call(string(&b["id"]), ChatItem::Todos { items });
            return;
        }
        let (kind, target) = self.kind_target(name, input);
        let item = ChatItem::Tool {
            id: string(&b["id"]),
            name: name.into(),
            kind,
            target,
            input: serde_json::to_string_pretty(input).unwrap_or_default(),
            output: String::new(),
            ok: None,
            diff: edit_diff(name, input),
        };
        self.tool_call(string(&b["id"]), item);
    }

    fn user(&mut self, v: &Value) {
        let content = &v["message"]["content"];
        let results: Vec<&Value> = content.as_array().into_iter().flatten().filter(|c| c["type"] == "tool_result").collect();
        if results.is_empty() {
            // A prompt, echoed back (--replay-user-messages), or claude's note of an interrupt.
            let text = content.as_str().map(str::to_string).unwrap_or_else(|| texts(content));
            if text.starts_with("[Request interrupted") {
                self.notice("Interrupted".into(), false);
            } else if !text.trim().is_empty() && v["isMeta"] != true {
                self.push(ChatItem::User { text });
            }
            return;
        }
        for r in results {
            let Some(&i) = self.tools.get(r["tool_use_id"].as_str().unwrap_or("")) else { continue };
            let out = r["content"].as_str().map(str::to_string).unwrap_or_else(|| texts(&r["content"]));
            let failed = r["is_error"] == true;
            self.set(i, |item| {
                if let ChatItem::Tool { output, ok, .. } = item {
                    *output = clip(out);
                    *ok = Some(!failed);
                }
            });
        }
    }
}

/// What an Edit, MultiEdit or Write changes, as diff lines.
fn edit_diff(name: &str, input: &Value) -> Vec<DiffLine> {
    let pairs: Vec<(String, String)> = match name {
        "Edit" => vec![(string(&input["old_string"]), string(&input["new_string"]))],
        "MultiEdit" => input["edits"].as_array().into_iter().flatten().map(|e| (string(&e["old_string"]), string(&e["new_string"]))).collect(),
        "Write" => {
            let content = string(&input["content"]);
            let mut lines: Vec<&str> = content.split_inclusive('\n').collect();
            lines.truncate(MAX_WRITE_LINES);
            vec![(String::new(), lines.concat())]
        }
        _ => return Vec::new(),
    };
    pairs.iter().flat_map(|(old, new)| text_diff(old, new)).collect()
}

/// Diff lines from one text to another.
fn text_diff(old: &str, new: &str) -> Vec<DiffLine> {
    let line = |s: &str| if s.is_empty() || s.ends_with('\n') { s.to_string() } else { format!("{s}\n") };
    review::hunks(&line(old), &line(new)).into_iter().flat_map(|h| h.lines).collect()
}

/// Diff lines from a unified diff's hunks (headers skipped).
fn unified(diff: &str) -> Vec<DiffLine> {
    let (mut old, mut new) = (0u32, 0u32);
    let mut lines = Vec::new();
    for l in diff.split_inclusive('\n') {
        if let Some(h) = l.strip_prefix("@@ -") {
            let mut nums = h.split(['+', ' ', ',']).filter(|s| !s.is_empty());
            old = nums.next().and_then(|n| n.parse().ok()).unwrap_or(1);
            new = h.split_once('+').and_then(|(_, r)| r.split([',', ' ']).next()).and_then(|n| n.parse().ok()).unwrap_or(1);
            continue;
        }
        if l.starts_with("+++") || l.starts_with("---") || l.starts_with("diff ") || l.starts_with("index ") || l.starts_with('\\') {
            continue;
        }
        let (kind, text) = match l.chars().next() {
            Some(c @ ('+' | '-' | ' ')) => (c, &l[1..]),
            _ => continue,
        };
        let (o, n) = match kind {
            '+' => (0, new),
            '-' => (old, 0),
            _ => (old, new),
        };
        if kind != '+' {
            old += 1;
        }
        if kind != '-' {
            new += 1;
        }
        lines.push(DiffLine { kind, old: o, new: n, text: text.to_string() });
    }
    lines
}

fn clip(s: String) -> String {
    if s.len() <= MAX_OUTPUT {
        return s;
    }
    let (mut head, mut tail) = (MAX_OUTPUT / 2, s.len() - MAX_OUTPUT / 2);
    while !s.is_char_boundary(head) {
        head -= 1;
    }
    while !s.is_char_boundary(tail) {
        tail += 1;
    }
    format!("{}\n… {} bytes cut …\n{}", &s[..head], tail - head, &s[tail..])
}

fn texts(content: &Value) -> String {
    content.as_array().into_iter().flatten().filter_map(|c| c["text"].as_str()).collect::<Vec<_>>().join("\n")
}

fn error_text(v: &Value) -> String {
    match v {
        Value::Array(a) if !a.is_empty() => a.iter().map(string).collect::<Vec<_>>().join("; "),
        Value::String(s) if !s.is_empty() => s.clone(),
        _ => "claude stopped on an error".into(),
    }
}

fn string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        _ => v.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::ToolKind;
    use serde_json::json;

    fn feed(c: &mut Chat, lines: &[Value]) {
        for v in lines {
            c.feed(format!("{v}\n").as_bytes());
        }
    }

    fn delta(index: u64, kind: &str, text: &str) -> Value {
        let field = if kind == "thinking_delta" { "thinking" } else { "text" };
        json!({"type":"stream_event","event":{"type":"content_block_delta","index":index,"delta":{"type":kind,field:text}}})
    }

    #[test]
    fn streamed_text_grows_then_the_complete_block_replaces_it() {
        let mut c = Chat::new("claude", "/repo");
        feed(&mut c, &[
            json!({"type":"user","message":{"role":"user","content":"fix it"}}),
            json!({"type":"stream_event","event":{"type":"message_start","message":{"id":"m1"}}}),
            json!({"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"thinking"}}}),
            delta(0, "thinking_delta", "hmm"),
            json!({"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"text"}}}),
            delta(1, "text_delta", "Look"),
            delta(1, "text_delta", "ing"),
        ]);
        assert_eq!(c.items[1], ChatItem::Thinking { text: "hmm".into() });
        assert_eq!(c.items[2], ChatItem::Assistant { text: "Looking".into() });
        assert_eq!(c.take_changed(), vec![0, 1, 2]);
        // Complete blocks arrive one message each; they land on what streamed, in order.
        feed(&mut c, &[
            json!({"type":"assistant","message":{"id":"m1","content":[{"type":"thinking","thinking":"hmm."}]}}),
            json!({"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"Looking now."}]}}),
        ]);
        assert_eq!(c.items.len(), 3);
        assert_eq!(c.items[1], ChatItem::Thinking { text: "hmm.".into() });
        assert_eq!(c.items[2], ChatItem::Assistant { text: "Looking now.".into() });
        assert_eq!(c.take_changed(), vec![1, 2]);
    }

    #[test]
    fn tool_calls_get_their_results_and_edits_a_diff() {
        let mut c = Chat::new("claude", "/repo");
        feed(&mut c, &[
            json!({"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":"/repo/src/a.rs","old_string":"let x = 1;","new_string":"let x = 2;"}}]}}),
            json!({"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"t2","name":"Bash","input":{"command":"cargo test"}}]}}),
            json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t2","content":"ok","is_error":true}]}}),
        ]);
        let ChatItem::Tool { kind, target, diff, ok, .. } = &c.items[0] else { panic!("{:?}", c.items[0]) };
        assert_eq!((*kind, target.as_str(), *ok), (ToolKind::Edit, "src/a.rs", None));
        assert_eq!(diff.iter().map(|l| format!("{}{}", l.kind, l.text)).collect::<String>(), "-let x = 1;\n+let x = 2;\n");
        let ChatItem::Tool { target, output, ok, .. } = &c.items[1] else { panic!() };
        assert_eq!((target.as_str(), output.as_str(), *ok), ("cargo test", "ok", Some(false)));
    }

    #[test]
    fn todos_errors_interrupts_and_subagents() {
        let mut c = Chat::new("claude", "/repo");
        feed(&mut c, &[
            json!({"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"t1","name":"TodoWrite","input":{"todos":[{"content":"Read","status":"completed"},{"content":"Fix","status":"in_progress"}]}}]}}),
            json!({"type":"assistant","parent_tool_use_id":"t9","message":{"id":"m2","content":[{"type":"text","text":"sub"}]}}),
            json!({"type":"user","message":{"content":[{"type":"text","text":"[Request interrupted by user]"}]}}),
            json!({"type":"result","subtype":"error_max_turns","is_error":true,"errors":["Reached max turns"]}),
            json!({"type":"result","subtype":"error_during_execution","is_error":true}),
        ]);
        assert_eq!(c.items, vec![
            ChatItem::Todos { items: vec![("Read".into(), "completed".into()), ("Fix".into(), "in_progress".into())] },
            ChatItem::Notice { text: "Interrupted".into(), error: false },
            ChatItem::Notice { text: "Reached max turns".into(), error: true },
        ]);
    }

    #[test]
    fn long_output_is_cut_in_the_middle() {
        let s = format!("{}é{}", "a".repeat(MAX_OUTPUT), "b".repeat(MAX_OUTPUT));
        let cut = clip(s);
        assert!(cut.len() < MAX_OUTPUT + 100 && cut.starts_with('a') && cut.ends_with('b'), "{}", cut.len());
    }
}

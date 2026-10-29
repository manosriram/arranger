#!/usr/bin/env python3
# Fake claude (stream-json) for chat screenshots: answers its own initialize, then on the first
# user message emits a run of back-to-back Read tool calls with output, so the chat groups them.
import json, sys, time

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

def read_line():
    return json.loads(sys.stdin.readline())

# A small set of files a Read would return, latest last (bottom of the window).
# (path, text, offset, limit): what each Read covers, so the chat can show its line range.
FILES = [
    ("src/http.rs", "use std::time::Duration;\n\npub struct Client { base: Url, retries: u32 }\n\nimpl Client {\n    pub fn new(base: Url) -> Self {\n        Client { base, retries: 3 }\n    }\n", 1, 8),
    ("src/cache.rs", "use std::collections::HashMap;\n\npub struct Cache<K, V> { map: HashMap<K, V>, cap: usize }\n\nimpl<K, V> Cache<K, V> {\n    pub fn get(&self, k: &K) -> Option<&V> { self.map.get(k) }\n}\n", 40, 9),
    ("tests/server.rs", "#[test]\nfn serves_health() {\n    let srv = spawn_server();\n    assert_eq!(get(&srv, \"/health\"), 200);\n}\n", 12, 5),
]

# The initialize response needs a success control_response plus a system init line.
send({
    "type": "system",
    "subtype": "init",
    "session_id": "demo-session",
    "slash_commands": [],
    "terminal_slash_commands": [],
    "permissionMode": "default",
})

tool_id = 0

while True:
    try:
        v = read_line()
    except Exception:
        break
    typ = v.get("type")
    if typ == "control_request":
        rid = v.get("request_id")
        sub = v.get("request", {}).get("subtype")
        if sub == "initialize":
            send({"type": "control_response", "response": {"subtype": "success", "request_id": rid, "response": {}}})
        else:
            # Anything else (can_use_tool etc.) that isn't registered: answer with an error, as
            # arranger itself does for unregistered requests, to avoid hanging.
            send({"type": "control_response", "response": {"subtype": "error", "request_id": rid, "error": "unhandled"}})
    elif typ == "user":
        # The agent's prompt. Reply with a message that reads several files back to back.
        content = []
        ids = []
        for path, text, offset, limit in FILES:
            tool_id += 1
            tid = f"t{tool_id}"
            ids.append(tid)
            content.append({"type": "tool_use", "id": tid, "name": "Read", "input": {"file_path": f"/repo/{path}", "offset": offset, "limit": limit}})
        send({"type": "assistant", "message": {"id": "m1", "content": content}})
        time.sleep(0.2)
        # The tool results, as claude reports them.
        results = []
        for tid, (path, text, offset, limit) in zip(ids, FILES):
            results.append({"type": "tool_result", "tool_use_id": tid, "content": text})
        send({"type": "user", "message": {"content": results}})
        time.sleep(0.2)
        # A short closing text and the turn result.
        send({"type": "assistant", "message": {"id": "m2", "content": [{"type": "text", "text": "`retry starts from the client's configured base URL`"}]}})
        send({"type": "result", "subtype": "success", "is_error": False, "result": "done", "usage": {"input_tokens": 100, "output_tokens": 50}})

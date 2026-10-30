#!/usr/bin/env python3
# Demo agent for README screenshots: looks like a coding agent, edits a file, reports through
# arranger's hooks, then waits. Installed as `claude`/`codex`/`gemini`; the prompt picks the act.
import json, os, subprocess, sys, time

kind = os.path.basename(sys.argv[0])
prompt = sys.argv[-1] if len(sys.argv) > 1 else ""
prompt = prompt.removeprefix("--prompt-interactive=") if prompt.startswith("--prompt-interactive=") or not prompt.startswith("-") else ""  # gemini's form
hook_bin, cwd = os.environ.get("ARRANGER_BIN", "arranger"), os.getcwd()

def hook(payload):
    p = subprocess.Popen([hook_bin, "hook", kind], stdin=subprocess.PIPE, stdout=subprocess.DEVNULL)
    p.communicate(json.dumps({"cwd": cwd, **payload}).encode())

def tool(name, **inp):
    hook({"hook_event_name": "PreToolUse", "tool_name": name, "tool_input": inp})
    time.sleep(0.2)

def say(*lines):
    for l in lines:
        print(l, flush=True)
        time.sleep(0.05)

def chat():
    """claude in a chat: its stream-json, as arranger's headless driver reads it."""
    n = 0
    def out(v):
        print(json.dumps(v), flush=True)
    def text(t):
        nonlocal n
        n += 1
        out({"type": "assistant", "message": {"id": f"m{n}", "role": "assistant", "content": [{"type": "text", "text": t}]}})
        time.sleep(0.1)
    def use(name, result, **inp):
        nonlocal n
        n += 1
        out({"type": "assistant", "message": {"id": f"m{n}", "role": "assistant", "content": [{"type": "tool_use", "id": f"t{n}", "name": name, "input": inp}]}})
        if result is not None:
            out({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": f"t{n}", "content": result}]}})
        time.sleep(0.1)
        return f"t{n}"
    def done():
        out({"type": "result", "subtype": "success", "is_error": False, "usage": {"input_tokens": 18240, "output_tokens": 1460}, "total_cost_usd": 0.21})

    for line in sys.stdin:
        v = json.loads(line)
        if v.get("type") == "control_request" and v["request"].get("subtype") == "initialize":
            out({"type": "system", "subtype": "init", "session_id": "demo", "permissionMode": "default", "slash_commands": [], "terminal_slash_commands": []})
            out({"type": "control_response", "response": {"subtype": "success", "request_id": v["request_id"], "response": {}}})
        elif v.get("type") == "user":
            break
    else:
        return
    prompt = v["message"]["content"]
    out(v)  # --replay-user-messages
    if "flaky" in prompt:
        before = ("use std::time::Duration;\n\n#[test]\nfn serves_health() {\n    let srv = spawn_server();\n"
                  "    std::thread::sleep(Duration::from_millis(50));\n    assert_eq!(get(&srv, \"/health\"), 200);\n}\n")
        with open("tests/server.rs", "w") as f:
            f.write(before)
        text("I'll look at how the test waits for the server before it calls `/health`.")
        use("Grep", "tests/server.rs:6:    std::thread::sleep(Duration::from_millis(50));\ntests/cache.rs:12:    std::thread::sleep(Duration::from_millis(5));",
            pattern="sleep\\(", path=f"{cwd}/tests")
        use("Read", before, file_path=f"{cwd}/tests/server.rs")
        use("Read", "pub fn spawn_server() -> Server {\n    let listener = TcpListener::bind(\"127.0.0.1:0\").unwrap();\n"
            "    let addr = listener.local_addr().unwrap();\n    thread::spawn(move || serve(listener));\n    Server { addr }\n}",
            file_path=f"{cwd}/src/http.rs", offset=40, limit=20)
        text("The test sleeps a fixed 50 ms and hopes the server is up. On a loaded CI runner `serve` sometimes hasn't "
             "accepted yet, so the first request gets refused. I'll poll the port instead of sleeping.")
        imports = ("use std::net::TcpStream;\nuse std::time::Duration;\n\n/// Blocks until the server accepts a connection.\n"
                   "fn wait_ready(srv: &Server) {\n    while TcpStream::connect(srv.addr).is_err() {\n"
                   "        std::thread::sleep(Duration::from_millis(5));\n    }\n}\n")
        mid = before.replace("use std::time::Duration;\n", imports)
        with open("tests/server.rs", "w") as f:
            f.write(mid)
        use("Edit", "The file tests/server.rs has been updated.", file_path=f"{cwd}/tests/server.rs",
            old_string="use std::time::Duration;\n", new_string=imports)
        old, new = "    std::thread::sleep(Duration::from_millis(50));\n", "    wait_ready(&srv);\n"
        with open("tests/server.rs", "w") as f:
            f.write(mid.replace(old, new))
        use("Edit", "The file tests/server.rs has been updated.", file_path=f"{cwd}/tests/server.rs", old_string=old, new_string=new)
        use("Bash", "running 1 test\ntest serves_health ... ok\n\ntest result: ok. 1 passed; 0 failed; 0 ignored",
            command="cargo test serves_health")
        use("Bash", "200 runs: 200 passed, 0 failed", command="for i in $(seq 200); do cargo test -q serves_health || exit 1; done")
        text("Fixed the flaky `serves_health` test. It slept 50 ms and hoped the server was listening; it now calls "
             "`wait_ready`, which polls the port until it accepts. Passes 200/200 in a loop.")
        done()
    elif "retry" in prompt:
        text("I'll add jittered exponential backoff to `send`, then run the http tests.")
        use("Read", "pub fn send(req: Request) -> Result<Response> { … }", file_path=f"{cwd}/src/http.rs")
        add = "\n/// Retries 5xx and timeouts with jittered exponential backoff, up to `max` tries.\npub fn backoff(attempt: u32) -> Duration {\n    Duration::from_millis(100 * 2u64.pow(attempt)).min(MAX_DELAY)\n}\n"
        with open("src/http.rs", "a") as f:
            f.write(add)
        use("Edit", "The file src/http.rs has been updated.", file_path=f"{cwd}/src/http.rs", old_string="", new_string=add)
        use("Bash", None, command="cargo test http::")
        out({"type": "control_request", "request_id": "p1", "request": {"subtype": "can_use_tool", "tool_name": "Bash", "input": {"command": "cargo test http::"}}})
    else:
        text("Retraining on last week's clicks; 3 of 12 epochs done.")
        use("Bash", None, command="python train.py --since 7d")
    for _ in sys.stdin:  # an agent idles until told otherwise
        pass
    while True:
        time.sleep(1)

if "stream-json" in sys.argv:
    chat()
    sys.exit()

say(f"\x1b[1m{kind}\x1b[0m  ·  {cwd.replace(os.environ['HOME'], '~')}", "", f"\x1b[2m> {prompt}\x1b[0m", "")
hook({"hook_event_name": "UserPromptSubmit", "prompt": prompt})

if "retry" in prompt:
    tool("Read", file_path=f"{cwd}/src/http.rs")
    say("Read src/http.rs (212 lines)", "Adding exponential backoff to `send`.")
    with open("src/http.rs", "a") as f:
        f.write("\n/// Retries 5xx and timeouts with jittered exponential backoff, up to `max` tries.\n"
                "pub fn backoff(attempt: u32) -> Duration {\n    Duration::from_millis(100 * 2u64.pow(attempt)).min(MAX_DELAY)\n}\n")
    tool("Edit", file_path=f"{cwd}/src/http.rs")
    hook({"hook_event_name": "PostToolUse", "tool_response": {}})
    say("Update(src/http.rs)", "  Added 4 lines", "", "Bash(cargo test http::)")
    hook({"hook_event_name": "PermissionRequest", "tool_name": "Bash", "tool_input": {"command": "cargo test http::"}})
elif "flaky" in prompt:
    tool("Grep", pattern="sleep\\(")
    say("Search(pattern: \"sleep\\(\")", "  Found 3 files", "The test races the server's bind; waiting on the port instead.")
    with open("tests/server.rs", "w") as f:
        f.write("#[test]\nfn serves_health() {\n    let srv = spawn_server();\n    srv.wait_ready();\n    assert_eq!(get(&srv, \"/health\"), 200);\n}\n")
    tool("Edit", file_path=f"{cwd}/tests/server.rs")
    hook({"hook_event_name": "PostToolUse", "tool_response": {}})
    tool("Bash", command="cargo test serves_health -- --test-threads=1")
    say("Bash(cargo test serves_health)", "  test serves_health ... ok", "  test result: ok. 1 passed; 0 failed")
    hook({"hook_event_name": "PostToolUse", "tool_response": {}})
    hook({"hook_event_name": "Stop", "last_assistant_message": "Fixed the flaky `serves_health` test: it now waits for the port."})
    say("", "Fixed the flaky `serves_health` test: it now waits for the port.")
elif "document" in prompt:
    tool("Read", file_path=f"{cwd}/README.md")
    say("Read README.md", "Which section should the config table go under?")
    hook({"hook_event_name": "PreToolUse", "tool_name": "AskUserQuestion", "tool_input": {"questions": [
        {"question": "Where should the config reference go?", "options": [{"label": "Under Install"}, {"label": "New Config section"}]}]}})
else:  # works on a file the prompt names, so busy screenshots don't repeat one line
    path, note = next(((p, n) for k, p, n in [
        ("logging", "src/middleware.rs", "Moving the request log into a tower layer."),
        ("pagination", "src/orders.rs", "Adding cursor pagination to list_orders."),
        ("tokio", "Cargo.toml", "Bumping tokio to 1.48; fixing the renamed APIs."),
        ("ranking", "train.py", "Retraining on last week's clicks; 3 of 12 epochs done."),
        ("redeploy", "Dockerfile", "Moving the base image to debian:13-slim."),
    ] if k in prompt), ("src/cache.rs", "Swapping the Mutex<HashMap> for a sharded map."))
    tool("Read", file_path=f"{cwd}/{path}")
    say(f"Read {path}", note)
    with open(path, "a") as f:
        f.write("\n// wip\n")
    tool("Edit", file_path=f"{cwd}/{path}")

while True:  # an agent idles at its prompt until told otherwise
    time.sleep(1)

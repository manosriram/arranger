#!/usr/bin/env python3
# Demo agent for README screenshots: looks like a coding agent, edits a file, reports through
# arranger's hooks, then waits. Installed as `claude`/`codex`/`gemini`; the prompt picks the act.
import json, os, subprocess, sys, time

kind = os.path.basename(sys.argv[0])
prompt = sys.argv[-1] if len(sys.argv) > 1 and not sys.argv[-1].startswith("-") else ""
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

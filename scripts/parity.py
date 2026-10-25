#!/usr/bin/env python3
# Parity check: does a headless agent (as arranger runs it) send the API the same request as the
# interactive one? Runs one prompt each way through a logging proxy, keeps the first main-model
# request of each, and prints what differs.
#
#   scripts/parity.py [--claude PATH] [--model M] [--out DIR] [--keep]
#   scripts/parity.py --agent codex [--codex PATH] [--model M] [--out DIR] [--keep]
#
# claude goes through ANTHROPIC_BASE_URL; headless claude is stream-json. codex goes through its
# `openai_base_url` config; headless codex is `codex app-server`.
#
# Each run is killed once its request is recorded, so this costs one short request per variant.
# Request bodies and non-secret headers land in DIR (default: a temp dir) for a closer look.
import argparse, fcntl, http.client, http.server, json, os, pty, re, select, shutil, signal, struct
import subprocess, sys, tempfile, termios, threading, time, uuid, zlib

UPSTREAM = os.environ.get("PARITY_UPSTREAM", "api.anthropic.com")
# codex signed in with ChatGPT talks to this; with an API key, use https://api.openai.com/v1.
CODEX_UPSTREAM = os.environ.get("PARITY_CODEX_UPSTREAM", "https://chatgpt.com/backend-api/codex")
SECRET_HEADERS = {"authorization", "x-api-key", "cookie", "proxy-authorization"}
# Markers of a Claude session this script may itself run under (see adapter::PARENT_SESSION_ENV).
PARENT_ENV = ["CLAUDECODE", "CLAUDE_PID", "CLAUDE_CODE_ENTRYPOINT", "CLAUDE_CODE_EXECPATH", "CLAUDE_CODE_SESSION_ID",
              "CLAUDE_CODE_CHILD_SESSION", "CLAUDE_CODE_SESSION_ATTENDED", "CLAUDE_CODE_MESSAGING_SOCKET",
              "CLAUDE_CODE_MESSAGING_TOKEN"]
# How arranger starts headless claude (background::argv), and how ac59e61 did.
HEADLESS = ["--output-format", "stream-json", "--verbose", "--input-format", "stream-json", "--include-partial-messages",
            "--replay-user-messages", "--permission-prompt-tool", "stdio", "--setting-sources", "user", "--tools", "default"]
OLD = ["--print", "--verbose", "--input-format", "stream-json", "--output-format", "stream-json",
       "--include-partial-messages", "--permission-prompt-tool", "stdio"]


class Proxy(http.server.ThreadingHTTPServer):
    """Forwards everything to UPSTREAM over TLS and records /v1/messages request bodies."""
    daemon_threads = True

    def __init__(self, host=UPSTREAM, prefix="", record="/v1/messages"):
        super().__init__(("127.0.0.1", 0), Handler)
        self.requests, self.lock = [], threading.Lock()
        # Paths under the proxy map to https://host/prefix...; requests to `record` are kept.
        self.host, self.prefix, self.record = host, prefix, record


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def forward(self):
        if self.headers.get("Upgrade"):
            # codex tries a websocket first; refuse it so it falls back to plain HTTP.
            self.protocol_version = "HTTP/1.1"
            self.send_response(404)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        body = self.rfile.read(int(self.headers.get("Content-Length") or 0))
        if self.path.split("?")[0].endswith(self.server.record):
            raw = decode(body, self.headers.get("Content-Encoding"))
            try:
                kept = {k.lower(): v for k, v in self.headers.items() if k.lower() not in SECRET_HEADERS}
                with self.server.lock:
                    self.server.requests.append({"headers": kept, "body": json.loads(raw)})
            except ValueError:
                pass
        host = self.server.host
        conn = http.client.HTTPSConnection(host, timeout=600)
        headers = {k: v for k, v in self.headers.items() if k.lower() not in ("host", "connection")}
        headers["Host"] = host
        conn.request(self.command, self.server.prefix + self.path, body=body, headers=headers)
        resp = conn.getresponse()
        self.send_response_only(resp.status, resp.reason)
        for k, v in resp.getheaders():
            if k.lower() not in ("transfer-encoding", "connection"):
                self.send_header(k, v)
        self.send_header("Connection", "close")
        self.end_headers()
        while chunk := resp.read1(65536):
            try:
                self.wfile.write(chunk)
                self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                break
        conn.close()

    do_GET = do_POST = do_PUT = do_DELETE = do_PATCH = do_HEAD = forward


def decode(body, encoding):
    if encoding == "gzip":
        return zlib.decompress(body, 47)
    if encoding == "zstd":  # codex compresses its requests
        return subprocess.run(["zstd", "-dc"], input=body, capture_output=True, check=True).stdout
    return body


def env(proxy):
    e = {k: v for k, v in os.environ.items() if k not in PARENT_ENV}
    e["ANTHROPIC_BASE_URL"] = f"http://127.0.0.1:{proxy.server_address[1]}"
    return e


def wait_for(proxy, marker, proc, timeout, poke=None, field="messages"):
    """The first recorded main-loop request (it has tools; side calls like session naming don't)
    whose `field` (claude's messages, codex's input) mentions `marker`, or None."""
    end = time.time() + timeout
    while time.time() < end and proc.poll() is None:
        if poke:
            poke()
        with proxy.lock:
            for r in proxy.requests:
                # codex's code mode sends its tools as an input item
                main = r["body"].get("tools") or any(i.get("type") == "additional_tools" for i in r["body"].get("input") or [])
                if main and marker in json.dumps(r["body"].get(field, [])):
                    return r
        time.sleep(0.2)
    return None


def stop(proc):
    try:
        os.killpg(proc.pid, signal.SIGTERM)
        proc.wait(5)
    except (ProcessLookupError, PermissionError, subprocess.TimeoutExpired):
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except (ProcessLookupError, PermissionError):
            pass


def tty(cmd, cwd, environ):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
    proc = subprocess.Popen(cmd, cwd=cwd, env={**environ, "TERM": "xterm-256color"},
                            stdin=slave, stdout=slave, stderr=slave, start_new_session=True)
    os.close(slave)
    return proc, master


def drain(master, screen):
    """`screen` plus whatever the terminal drew since, and its last part as plain lowercase text."""
    end = time.time() + 0.5  # it may never stop drawing
    while time.time() < end and select.select([master], [], [], 0)[0]:
        try:
            screen += os.read(master, 65536)
        except OSError:
            break
    return screen, re.sub(rb"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07]*\x07", b"", screen[-8000:]).lower()


def codex_config(proxy):
    """The `-c` both codex runs share: the proxy."""
    path = re.sub(r"^https://[^/]+", "", CODEX_UPSTREAM)
    return ["-c", f'openai_base_url="http://127.0.0.1:{proxy.server_address[1]}{path}"']


# What a scratch CODEX_HOME leaves out of the real one: history, and where trust gets saved.
CODEX_PRIVATE = {"config.toml", "sessions", "archived_sessions", "log", "history.jsonl", "session_index.jsonl", "shell_snapshots",
                 "tmp", "app-server-control", "app-server-daemon", "thread-writer-locks"}


def codex_home(cwd):
    """A CODEX_HOME like the user's (login, config, instructions, skills) where the fixture is
    trusted, so neither run shows the trust prompt or saves its answer to the real config."""
    real = os.environ.get("CODEX_HOME") or os.path.expanduser("~/.codex")
    home = tempfile.mkdtemp(prefix="parity-codex-home-")
    for name in os.listdir(real):
        if name not in CODEX_PRIVATE and not name.endswith((".sqlite", ".sqlite-wal", ".sqlite-shm", ".db")):
            os.symlink(os.path.join(real, name), os.path.join(home, name))
    config = open(os.path.join(real, "config.toml")).read() if os.path.exists(os.path.join(real, "config.toml")) else ""
    with open(os.path.join(home, "config.toml"), "w") as f:
        f.write(config + f'\n[projects."{cwd}"]\ntrust_level = "trusted"\n')
    return home


def run_codex_interactive(proxy, codex, args, cwd, prompt, marker):
    proc, master = tty([codex, *args], cwd, env(proxy))
    screen, state = b"", {"typed": 0, "start": time.time()}

    def poke():
        nonlocal screen
        screen, plain = drain(master, screen)
        # Type once its prompt box is up; press Enter again later in case a notice took the first.
        since = time.time() - state["start"]
        if state["typed"] == 0 and (b"state=idle" in plain or b"ask codex" in plain) and since > 3:
            os.write(master, b"\x1b[200~" + prompt.encode() + b"\x1b[201~")
            time.sleep(0.5)
            os.write(master, b"\r")
            state["typed"] = 1
        elif state["typed"] == 1 and since > 20:
            os.write(master, b"\r")
            state["typed"] = 2

    try:
        found = wait_for(proxy, marker, proc, 120, poke, field="input")
        if found is None and os.environ.get("PARITY_DEBUG"):
            print(drain(master, screen)[1][-3000:].decode("utf8", "replace"), state, len(proxy.requests), proc.poll())
        return found
    finally:
        stop(proc)
        os.close(master)


def run_codex_headless(proxy, codex, args, cwd, prompt, marker, model):
    """codex app-server, started and driven the way headless/codex.rs does."""
    proc = subprocess.Popen([codex, "app-server", *args], cwd=cwd, env=env(proxy), stdin=subprocess.PIPE,
                            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, start_new_session=True)
    send = lambda v: (proc.stdin.write((json.dumps(v) + "\n").encode()), proc.stdin.flush())
    info = {"name": "arranger", "title": "Arranger", "version": "parity"}
    init = {"clientInfo": info, **json.loads(os.environ.get("PARITY_CODEX_INIT") or "{}")}  # to try variations
    send({"id": 0, "method": "initialize", "params": init})

    def drive():
        for line in proc.stdout:
            v = json.loads(line)
            if os.environ.get("PARITY_DEBUG") and ("error" in v or v.get("method") in ("error", "turn/completed")):
                print("app-server:", line.decode()[:400].strip())
            if v.get("id") == 0 and "result" in v:
                send({"method": "initialized", "params": {}})
                params = {"cwd": cwd, "threadSource": "user", **({"model": model} if model else {})}
                send({"id": 1, "method": "thread/start", "params": params})
            elif v.get("id") == 1 and "result" in v:
                thread = v["result"]["thread"]["id"]
                time.sleep(float(os.environ.get("PARITY_CODEX_DELAY") or 0))
                text = [{"type": "text", "text": prompt, "text_elements": []}]
                send({"id": 2, "method": "turn/start", "params": {"threadId": thread, "input": text, "turnTrigger": "user"}})

    threading.Thread(target=drive, daemon=True).start()
    try:
        return wait_for(proxy, marker, proc, 120, field="input")
    finally:
        stop(proc)


def codex_summary(req, cwd):
    """codex's request, normalized: its tools come as an `additional_tools` input item, its
    instructions as developer messages, and how the turn runs (sandbox, effort) as metadata."""
    b = req["body"]
    items, tools = [], {}

    def add_tools(ts, prefix=""):
        for t in ts:
            if t.get("type") == "namespace":
                add_tools(t.get("tools", []), prefix + t.get("name", "") + ".")
            else:
                tools[prefix + (t.get("name") or t.get("type"))] = normalize(json.dumps(t, sort_keys=True), cwd)

    add_tools(b.get("tools") or [])
    for it in b.get("input", []):
        if it.get("type") == "additional_tools":
            add_tools(it.get("tools", []))
            continue
        content = it.get("content")
        texts_ = [c.get("text", "") for c in content] if isinstance(content, list) else [json.dumps(it, sort_keys=True)]
        items.append(f"[{it.get('role', it.get('type'))}] " + normalize("\n".join(texts_), cwd))
    meta = b.get("client_metadata") or {}
    turn = json.loads(meta.get("x-codex-turn-metadata") or "{}")
    keys = ["sandbox", "sandbox_mode", "auto_review_enabled", "node_repl_auto_review_required", "node_repl_disabled",
            "model", "reasoning_effort", "thread_source", "turn_trigger", "request_kind", "agent_name"]
    return {
        "model": b.get("model"),
        "reasoning": b.get("reasoning"),
        "text": b.get("text"),
        "include": b.get("include"),
        "parallel_tool_calls": b.get("parallel_tool_calls"),
        "service_tier": b.get("service_tier"),
        "tool_choice": b.get("tool_choice"),
        "access_programs": b.get("access_programs"),
        "store": b.get("store"),
        "client_metadata": sorted(meta.keys()),
        "turn": {k: turn.get(k) for k in keys},
        "instructions": [normalize(b.get("instructions") or "", cwd)],
        "tools": tools,
        "input": items,
    }


def run_interactive(proxy, claude, args, cwd, prompt, marker):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
    proc = subprocess.Popen([claude, *args], cwd=cwd, env={**env(proxy), "TERM": "xterm-256color"},
                            stdin=slave, stdout=slave, stderr=slave, start_new_session=True)
    os.close(slave)
    screen, state = b"", {"typed": False, "trusted": False, "start": time.time()}

    def poke():
        nonlocal screen
        while select.select([master], [], [], 0)[0]:
            try:
                screen += os.read(master, 65536)
            except OSError:
                return
        plain = re.sub(rb"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07]*\x07", b"", screen[-8000:]).lower()
        if not state["trusted"] and b"trustthisfolder" in plain.replace(b" ", b""):
            # Its folder-trust menu defaults to "No, exit"; move to "Yes" first.
            state["trusted"], state["start"] = True, time.time()
            os.write(master, b"\x1b[B")
            time.sleep(0.3)
            os.write(master, b"\r")
        elif not state["typed"] and time.time() - state["start"] > 4:
            state["typed"] = True
            os.write(master, b"\x1b[200~" + prompt.encode() + b"\x1b[201~")
            time.sleep(0.5)
            os.write(master, b"\r")

    try:
        return wait_for(proxy, marker, proc, 90, poke)
    finally:
        stop(proc)
        os.close(master)


def run_headless(proxy, claude, args, cwd, prompt, marker, initialize):
    proc = subprocess.Popen([claude, *args], cwd=cwd, env=env(proxy), stdin=subprocess.PIPE,
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
    lines = [{"type": "control_request", "request_id": "init", "request": {"subtype": "initialize", **initialize}},
             {"type": "user", "message": {"role": "user", "content": prompt}}]
    proc.stdin.write("".join(json.dumps(l) + "\n" for l in lines).encode())
    proc.stdin.flush()
    try:
        return wait_for(proxy, marker, proc, 90)
    finally:
        stop(proc)


def texts(content):
    if isinstance(content, str):
        return [content]
    return [b.get("text", json.dumps(b, sort_keys=True)) for b in content or []]


def normalize(s, cwd):
    s = s.replace(cwd, "<cwd>")
    s = re.sub(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}", "<uuid>", s)
    return re.sub(r"\b\d{4}-\d{2}-\d{2}\b", "<date>", s)


def summary(req, cwd):
    """The parts of a request that decide what the model sees, normalized for comparison."""
    b = req["body"]
    return {
        "model": b.get("model"),
        "max_tokens": b.get("max_tokens"),
        "thinking": b.get("thinking"),
        "output_config": b.get("output_config"),
        "temperature": b.get("temperature"),
        "anthropic-beta": sorted((req["headers"].get("anthropic-beta") or "").split(",")),
        "system": [normalize(t, cwd) for t in texts(b.get("system"))],
        "tools": {t.get("name"): json.dumps(t, sort_keys=True) for t in b.get("tools", [])},
        "first_message": [normalize(t, cwd) for m in b.get("messages", [])[:1] for t in texts(m.get("content"))],
    }


def diff_lines(a, b):
    import difflib
    return list(difflib.unified_diff(a.splitlines(), b.splitlines(), "interactive", "headless", n=1, lineterm=""))


# Differences that come from headless claude being the SDK entrypoint (`cc_entrypoint=sdk-cli`),
# not from how arranger starts it. Seen on claude 2.1.310; anything else fails the check.
KNOWN_SYSTEM = [
    r"^x-anthropic-billing-header: .*",
    r"^You are Claude Code, Anthropic's official CLI for Claude\.$",
    r"^You are a Claude agent, built on Anthropic's Claude Agent SDK\.$",
    r"^ - If you need the user to run a shell command themselves .*",  # the TUI's `!` prefix
    r"^EndConversation \(deferred tool\).*",
    r"^To make a small change to a page you already published with the Artifact tool.*",
]
# Tools only the interactive TUI offers (claude.ai artifacts, /feedback).
INTERACTIVE_ONLY_TOOLS = {"Artifact", "SendFeedback"}
# What codex gives its own TUI by client name (originator `codex-tui`), not by how arranger starts
# it; arranger doesn't pass itself off as the TUI. Other clients get plugin-install suggestions
# (a `<recommended_plugins>` list and the exec tool's `request_plugin_install`), the TUI a `cyber`
# access program. Seen on codex 0.163.0.
KNOWN_CODEX = {"access_programs"}
CODEX_PLUGIN_SUGGESTIONS = [r"\n?<recommended_plugins>.*?</recommended_plugins>\n?",
                            r"### `request_plugin_install`.*?```ts\n.*?```\n\n?"]


def without_plugin_suggestions(s):
    for pattern in CODEX_PLUGIN_SUGGESTIONS:
        s = re.sub(pattern, "", s, flags=re.S)
    return s


def without_known(blocks):
    lines = "\n\n".join(blocks).splitlines()
    kept = [l for l in lines if not any(re.match(k, l) for k in KNOWN_SYSTEM)]
    return re.sub(r"\n{3,}", "\n\n", "\n".join(kept)).strip()


def compare(name, base, other):
    """Prints how `other` differs from interactive; true when nothing unexplained does."""
    print(f"\n=== interactive vs {name}")
    ok = True
    for key in base:
        x, y = base[key], other[key]
        if x == y:
            continue
        if key in KNOWN_CODEX:
            print(f"- {key}: {json.dumps(x)} vs {json.dumps(y)} (codex-tui's by client name)")
            continue
        if key in ("input", "tools"):
            def strip(v):
                if key == "input":
                    return [without_plugin_suggestions(t) for t in v]
                tools = {k: json.loads(t) for k, t in v.items()}
                for t in tools.values():
                    t["description"] = without_plugin_suggestions(t.get("description") or "")
                return tools
            if strip(x) == strip(y):
                print(f"- {key}: only codex's plugin-install suggestions differ (by client name)")
                continue
        if key == "tools":
            missing, extra = sorted(set(x) - set(y) - INTERACTIVE_ONLY_TOOLS), sorted(set(y) - set(x))
            changed = sorted(k for k in set(x) & set(y) if x[k] != y[k])
            ok &= not missing and not extra
            print(f"- tools: missing {missing}, extra {extra}, wording differs (SDK variant) in {changed}")
        elif key == "system":
            unexplained = without_known(x) != without_known(y)
            ok &= not unexplained
            print(f"- system: {sum(map(len, x))} vs {sum(map(len, y))} chars, "
                  f"{'UNEXPLAINED changes' if unexplained else 'only SDK-entrypoint lines differ'}")
            for line in diff_lines("\n\n".join(x), "\n\n".join(y))[:60]:
                print("    " + line[:200])
        elif key == "instructions":
            ok = False
            print(f"- instructions: {sum(map(len, x))} vs {sum(map(len, y))} chars")
            for line in diff_lines("\n\n".join(x), "\n\n".join(y))[:60]:
                print("    " + line[:200])
        elif key in ("first_message", "input"):
            ok = False
            print(f"- {key}: {len(x)} vs {len(y)} blocks")
            for line in diff_lines("\n\n".join(x), "\n\n".join(y))[:60]:
                print("    " + line[:200])
        else:
            ok = False
            print(f"- {key}: {json.dumps(x)} vs {json.dumps(y)}")
    print("parity: ok" if ok else "parity: FAILED")
    return ok


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--agent", default="claude", choices=["claude", "codex"])
    p.add_argument("--codex", default=shutil.which("codex") or "codex")
    p.add_argument("--claude", default=shutil.which("claude") or "claude")
    p.add_argument("--model", default="")
    p.add_argument("--permission-mode", default="default", help="passed to headless claude; arranger requires approvals by default")
    p.add_argument("--out", default="")
    p.add_argument("--keep", action="store_true", help="keep the scratch repo")
    a = p.parse_args()
    out = a.out or tempfile.mkdtemp(prefix="parity-out-")
    os.makedirs(out, exist_ok=True)
    cwd = os.path.realpath(tempfile.mkdtemp(prefix="parity-repo-"))
    subprocess.run(["git", "init", "-q", cwd], check=True)
    with open(os.path.join(cwd, "CLAUDE.md"), "w") as f:
        f.write("This repo is a parity check fixture. Keep answers short.\n")
    subprocess.run(["git", "-C", cwd, "add", "."], check=True)
    subprocess.run(["git", "-C", cwd, "-c", "user.name=parity", "-c", "user.email=parity@example.com",
                    "commit", "-qm", "fixture"], check=True)

    if a.agent == "codex":
        host = re.match(r"https://([^/]+)", CODEX_UPSTREAM).group(1)
        proxy = Proxy(host, record="/responses")
    else:
        proxy = Proxy()
    threading.Thread(target=proxy.serve_forever, daemon=True).start()
    model = ["--model", a.model] if a.model else []
    marker = lambda: f"PARITY-{uuid.uuid4().hex[:8]}"
    cfg = codex_config(proxy)
    if a.agent == "codex":
        os.environ["CODEX_HOME"] = codex_home(cwd)
    # Headless codex first: a TUI killed mid-start can leave its remote plugins unsynced for the next run.
    variants = [
        ("headless", lambda m: run_codex_headless(proxy, a.codex, cfg, cwd, f"Reply with just OK. {m}", m, a.model)),
        ("interactive", lambda m: run_codex_interactive(proxy, a.codex, cfg + model, cwd, f"Reply with just OK. {m}", m)),
    ] if a.agent == "codex" else [
        ("interactive", lambda m: run_interactive(proxy, a.claude, model, cwd, f"Reply with just OK. {m}", m)),
        ("old (ac59e61)", lambda m: run_headless(proxy, a.claude, OLD + model, cwd, f"Reply with just OK. {m}", m, {})),
        ("headless", lambda m: run_headless(proxy, a.claude, HEADLESS + ["--permission-mode", a.permission_mode] + model,
                                            cwd, f"Reply with just OK. {m}", m, {})),
    ]
    results = {}
    for name, run in variants:
        m = marker()
        req = run(m)
        if req is None:
            sys.exit(f"{name}: no request with {m} reached the proxy (is {a.agent} logged in?)")
        for key in ("messages", "input"):  # the marker is per run; hide it from the diff
            if key in req["body"]:
                req["body"][key] = json.loads(json.dumps(req["body"][key]).replace(m, "PARITY"))
        slug = re.sub(r"\W+", "-", name).strip("-")
        with open(os.path.join(out, f"{slug}.json"), "w") as f:
            json.dump(req, f, indent=1)
        results[name] = (codex_summary if a.agent == "codex" else summary)(req, cwd)
        print(f"{name}: recorded ({len(results[name]['tools'])} tools)")

    if "old (ac59e61)" in results:
        compare("old (ac59e61)", results["interactive"], results["old (ac59e61)"])  # for the record
    ok = compare("headless", results["interactive"], results["headless"])
    print(f"\nrequests in {out}")
    if not a.keep:
        shutil.rmtree(cwd, ignore_errors=True)
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    try:
        main()
    finally:
        if os.environ.get("CODEX_HOME", "").startswith(tempfile.gettempdir()) and "parity-codex-home-" in os.environ["CODEX_HOME"]:
            shutil.rmtree(os.environ["CODEX_HOME"], ignore_errors=True)  # only links into the real one

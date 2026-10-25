# Chat UI redesign: our UI, the real agent behind it

## Decision

Talk to each agent over its structured protocol and draw the chat ourselves, like t3code
and paseo do. Keep the PTY as a per-task fallback.

| Agent  | Transport                                        |
|--------|--------------------------------------------------|
| claude | `claude` stream-json + control protocol (what the Agent SDK wraps) |
| codex  | `codex app-server` (JSON-RPC over stdio)         |
| pi     | `pi --mode rpc`                                  |
| gemini | `gemini --acp`                                   |

All spoken from Rust in the daemon. No Node. `ac59e61:src/background.rs` already has a
first cut of all four; we restore it and fix what it got wrong.

### Parity rules for claude (from t3code, paseo, the SDK's `sdk.mjs` 0.3.310, and phase 1)

```sh
claude --output-format stream-json --verbose --input-format stream-json \
       --include-partial-messages --permission-prompt-tool stdio \
       --setting-sources user,project,local --tools default \
       [--permission-mode auto] [--model M] [--resume ID [--fork-session]]
```

- **No `--system-prompt`, and no `systemPrompt` in `initialize`.** Leaving it out is what
  keeps Claude Code's own prompt. Sending one (even `""`) replaces it.
- **`--permission-mode`**: interactive claude starts in the settings' `defaultMode`, else the
  account default (auto for us); headless starts in "default". So pass `auto` unless a
  settings file sets `defaultMode` (`headless::default_mode`).
- Arranger's own instructions, if any, go in `appendSystemPrompt` in `initialize`.
- Use the user's own `claude` binary and environment (minus `PARENT_SESSION_ENV`).
- No `--settings` hooks: the stream already carries tools, asks and turn ends.

## Phases

Each phase is one arranger task, merges on its own, and leaves the app working. Chat stays
behind a toggle until phase 5.

### Phase 1: Parity harness ✅

`scripts/parity.py [--model M] [--permission-mode M] [--out DIR]`: a logging proxy on
`ANTHROPIC_BASE_URL`, one prompt each through interactive claude (in a PTY), the old
`ac59e61` argv and today's headless argv; keeps each run's first main-loop request and
diffs model, max tokens, thinking, effort, betas, system prompt, tools and the first user
message (CLAUDE.md and context). Exits 0 when headless matches apart from known lines.

Results (claude 2.1.310, Opus 5.5):

- Same in all three: model, max tokens, thinking (adaptive), effort, the first user message
  (CLAUDE.md, git status, user context) and the tool list, bar two interactive-only tools.
- **Old `ac59e61` argv vs interactive:** the same request as headless, except it ran in the
  "default" permission mode instead of auto (no auto-mode classifier, so it prompted for
  more). So the flags it lacked (`--setting-sources`, `--tools`) changed nothing; what it
  lost was auto mode and, more likely, a UI that only showed text.
- **Headless with `--permission-mode auto` vs interactive:** differs only in lines that come
  from claude seeing itself as the SDK entrypoint (`cc_entrypoint=sdk-cli`):
  - first line "You are a Claude agent, built on Anthropic's Claude Agent SDK." instead of
    "You are Claude Code, Anthropic's official CLI for Claude.";
  - no `!`-prefix hint, no EndConversation / Artifact guidance, no `Artifact` /
    `SendFeedback` tools (TUI and claude.ai features);
  - Agent/Bash descriptions say subagents and commands run in the background by default.
  These aren't ours to change (the entrypoint also tags billing); `KNOWN_SYSTEM` in the
  script lists them, so anything new fails the check.

Rerun it when claude updates.

### Phase 2: Headless claude in the daemon ✅

- `src/headless.rs`: `Claude` speaks the control protocol (initialize without a system
  prompt, user messages, `can_use_tool` asks incl. AskUserQuestion answered one message per
  question, interrupt, unknown control requests answered with an error) and maps the stream
  to `Event`s. Codex/pi/gemini stay in `ac59e61` until phases 6–7.
- Daemon: `Task.headless: Option<Claude>` over `pty::spawn_stdio` (socket pair, own session,
  stderr to `logs/<id>.stderr`); writes queue in `Claude::output` and flush on writable.
  Session id from `system/init` in `adapters/session-<id>`; revive = `--resume ID`, clone
  copies that session file and runs `--resume ID --fork-session`. The JSONL stream is logged
  to `logs/<id>.jsonl.zst`. A failed exit records the stderr tail as an error.
- `arranger new --headless claude …`, `arranger interrupt <id>`; send/approve/deny/revive/
  clone/events/ls work as for PTY tasks. `TaskInfo.headless` for the GUI; attaching shows a
  short note (until phase 4) and Esc still interrupts.
- Tests: unit tests in `headless.rs`; `tests/cli.rs::headless_task` runs a stream-json fake
  through ask/approve/deny, interrupt, queueing, revive, clone and a crash.

### Phase 3: Chat model and protocol ✅

- `ChatItem` in `proto.rs`: User, Assistant (markdown), Thinking, Tool (kind, target, input,
  output, ok, diff as `DiffLine`s), Todos, Notice. Asks aren't items: the chat shows the
  task's `need` pinned above the message box.
- `src/chat.rs` builds them from stream-json: deltas grow the streaming item, the complete
  block replaces it, tool results land on their call (output cut to 16 KB from the middle),
  Edit/MultiEdit/Write become diffs via `review::hunks`. `--replay-user-messages` echoes
  prompts into the stream, so the log alone rebuilds the whole chat.
- Daemon: `chats` per task, loaded from `logs/<id>.jsonl.zst` (+ unflushed buffer) on first
  use; `Request::Chat { id, limit }` = last page + subscription, `ChatPage` for older pages,
  `ChatPatch { index, item }` pushed at most every 33 ms. Arranger's own notes (interrupted,
  a crash's stderr) are logged as `arranger_notice` lines so they survive a reload.
- `arranger chat <id>` prints a thread. Tests: `chat.rs` units; `headless_task` checks a live
  subscription, and the full chat across runs after a daemon restart.

### Phase 4: Chat view in the GUI ✅ (first cut)

- Mock: "Headless chat · Gruvbox" on the canvas (Final page).
- `src/bin/arranger-gui/chat.rs`: per-item layout cache (re-laid only when that item changes,
  is toggled, or the width changes), draws only visible rows, follows the newest unless
  scrolled up, pages older items in on scroll-up keeping the view still.
- Items: user block, markdown (headings, bullets, fences, bold/`code`), thinking collapsed,
  tool rows "✓ Run cargo test   2 lines ▸" opening to output/diff (small edits and failures
  open by themselves), plan checklist, notices. Pinned: Allow/Deny or a question's options,
  then the running line. Message box: per-task drafts, ↵ send, ⇧↵ newline, Esc (vim: Esc Esc)
  interrupts, ⌘I / `i` focus it; sending to an ended task revives it with that message.
- Not yet: selecting/copying chat text, `@` completion in the message box, and opening the
  same session in a terminal (the Terminal tab's shell can run `claude --resume`).

### Phase 5: Make it the default for claude ✅

- New-task dialog: a [chat | terminal] toggle beside Start (Ctrl+T), on by default for agents in
  `headless::default_on` (claude, codex); the choice is saved per task. `arranger new` does the
  same, `--terminal` opts out, `--headless` opts pi/gemini in.
- Effort: an `effort` chip after model (Alt+Tab cycles), stored per task (`tasks.effort`, proto
  VERSION 18), passed as claude `--effort`, codex `effort` on each turn (PTY: `-c
  model_reasoning_effort`), pi `--thinking`. Clones keep their source's.
- Slash commands: claude's init lists `slash_commands` and `terminal_slash_commands`. A command in
  the latter, or one of its TUI-only built-ins it doesn't list (`/permissions`, `/rewind`…), isn't
  sent: the daemon replies `Terminal { command }` and the window opens the Terminal tab with
  `claude '/cmd'` typed (the CLI prints it). Everything else goes through, skills included.
- README, vim.json updated.
- Parity re-run on claude 2.1.310: still only the SDK-entrypoint lines differ. `just parity` runs
  both checks; release.yml says to run it before tagging (CI has no logins).
- Fixed on the way: headless logs started with the terminal size mark, which ate the first line.

### Phase 6: codex ✅

- `headless/` is now one `Transport` trait (shared `Core`: queue, asks, summary) with claude,
  codex, pi and ACP implementations; `chat/` reads each one's lines into `ChatItem`s.
- codex: `codex app-server`, nothing overridden (sandbox, approvals, model come from its config,
  as in the TUI). thread/start | resume | fork (clone), turn/start with effort, turn/interrupt
  (deferred until the turn has an id), approvals (command, file change, permissions),
  requestUserInput questions, MCP elicitations declined. Subagent threads stay out of the chat.
  Only lines the chat uses are logged (no rate limits, output deltas).
- Parity (`scripts/parity.py --agent codex`, codex 0.163.0): proxied through `openai_base_url` in a
  scratch CODEX_HOME (same login, config, skills; the fixture trusted there, so nothing is saved
  to the real config). Instructions, skills, tools, sandbox (workspace-write), effort and model
  match. What differs is keyed on the client name: the TUI is itself an app-server client
  (`codex-tui`) and gets a `cyber` access program; other clients get plugin-install suggestions.
  arranger keeps its own name rather than pass itself off as codex's TUI.
  `threadSource`/`turnTrigger` = "user", as the TUI sends.
- Smoke test with real codex: ask → approve → file written; second turn; interrupt; revive
  (thread/resume remembers); clone (thread/fork, new thread id, remembers).
- An arranger worktree of a trusted repo is trusted by codex too (workspace-write).

### Phase 7: pi and gemini (built, parity unchecked)

- pi `--mode rpc` (`--session`/`--fork` a session file, `--continue`, `--thinking`); extension
  dialogs (confirm → permission, select/input/editor → question). gemini `--acp`: session/new,
  session/resume when offered, else session/load with its replay kept out of the log; permission
  options picked by kind; prompts logged by arranger (ACP doesn't repeat them).
- Tested against fakes only: neither CLI is installed here, so no parity check. They stay off by
  default (`default_on`); turn them on per task. Before defaulting on: extend parity.py to them.
- gemini can't clone (its sessions are per directory).

## Still open

- Parity for pi and gemini; then add them to `default_on`.
- Chat: text selection/copy, `@` completion in the message box.
- A cloned task's chat starts at the clone (the source's history isn't copied into its log).
- Opening a terminal-only command types `claude '/cmd'`, a fresh claude, not the task's session
  (two processes on one session would clash); fine for settings-style commands.

## Alternative (if phase 1 fails)

Keep the interactive CLI in a hidden PTY, build `ChatItem`s from its session JSONL
(`transcript_path` arrives in every hook payload), send input as bracketed paste + CR.
Phases 3–5 stay the same; only phase 2's transport changes. Costs: no token streaming,
fragile input.

## Risks

- **CLI updates change behaviour or the protocol.** The parity script and the replayable
  JSONL logs catch it; pin a minimum `claude` version.
- **Features missing in stream-json** (some slash commands, plan-mode UI). The terminal
  fallback covers them; collect the list during phase 4.
- **Two sources of state** (hooks and stream). The stream owns chat content, hooks own
  needs-you/sidebar state, as today.

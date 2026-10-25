<p align="center"><img src="assets/banner.svg" alt="arranger" width="100%"></p>

arranger runs many CLI coding agents (Claude Code, Codex, Gemini, pi) side by side, each on its own git worktree. A small daemon keeps them alive when the window closes and watches what each one is doing (edits, commands, permission requests, questions). The window shows who needs you, lets you answer in place, and takes you from diff review to a PR.

Linux and macOS. Rust throughout; no webview.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/screenshot-dark.png">
  <img alt="arranger workspace with six agents on three machines, permission and question cards in the sidebar, and a completed Codex task selected" src="assets/screenshot.png">
</picture>

## Install

```sh
curl -fsSL https://github.com/manosriram/arranger/releases/latest/download/install.sh | sh
```

On macOS this also puts **arranger.app** in `~/Applications`: open it from Finder, Spotlight or the Dock, then pick a project folder (any folder works; arranger sets up git in it).

Or build it: `cargo build --release` (makes `target/release/arranger` and `arranger-gui`; `just app` wraps them in `target/arranger.app`).

Requires `git`. Install `gh` if you want to open PRs from the app.

## Run

```sh
cd your/repo
arranger
```

That's it: it starts the daemon if needed and opens the window. Closing the window leaves the agents running; run `arranger` again to get back to them.

## Use

| Key | Action |
|-----|--------|
| ⌘N | New task: pick an agent and its model, write the prompt |
| ⌘D | Clone the selected task, including its chat and edits |
| ⌘J / ⌘K | Next / previous agent |
| ⌘I | Type into the selected agent's terminal |
| ⌘R | Review the diff: revert hunks, comment, then merge (or commit, or open a PR). Greyed out while there's nothing to review |
| ⌘E | Terminal: a shell in the selected task's worktree, kept running like the agents |
| ⌘T | Switch between dark and light |

On Linux use Ctrl+Shift in place of ⌘.

In the new-task dialog, Tab cycles agents, Ctrl+Tab cycles models and Alt+Tab cycles reasoning effort. Ctrl+T switches between running the task in a chat or in a terminal (the toggle beside Start). Click the agent, model, effort, machine or base-branch controls to choose them directly; Enter starts the task.

In the new-task prompt, type `@` to find repository files. Use ↑/↓ to choose, Enter or Tab to insert a reference, and Esc to dismiss the picker. Files come from the selected machine’s repository; ignored files are excluded.

Claude Code and Codex tasks run in a chat by default: arranger drives the agent over its own protocol (claude's stream-json, `codex app-server`) and draws the conversation itself, with streaming replies, tool calls and their diffs, the plan, and what the agent asks you pinned above the message box. Enter sends, Shift+Enter starts a new line, Esc stops the turn. A slash command only the agent's terminal has (claude's `/permissions`, `/rewind`…) opens the Terminal tab with it typed instead. pi and Gemini can run in a chat too (turn it on in the dialog); their parity with the interactive agents isn't checked yet.

A task started in a terminal runs the agent there, drawn as-is and sized to the pane. When it asks for permission or a choice, answer in its own menu: ↑/↓ and ↵, or with Vim keys on, normal mode switches on by itself and j/k/↵ work the menu.

A soft ding plays when an agent finishes or needs your help. Toggle **Ding sound** in the scheme menu (top right) to turn it off or on; your preference is saved. On Linux, sound requires `paplay` (PulseAudio/PipeWire) or `aplay` (ALSA).

Each task gets its own worktree on a new branch, `arranger/<id>`, made from the base branch you pick when you start it (the current branch by default). The base branch only changes when you merge into it from Review.

The **Terminal** tab is a shell in the selected task's worktree, for running the app or tests before merging. It keeps running when you switch tasks or close the window. A `cd` out of the worktree steps back in, and on macOS a sandbox also hides the main checkout and the other worktrees and allows writes only in the worktree, its git dir, temp dirs and `~/.*` (tool caches, history).

**Run in worktree** in Review opens a terminal in the task’s checkout (over SSH for remote tasks). Run your app or tests there before merging; uncommitted edits are included. From the CLI, `arranger run <id>` opens a shell, or `arranger run <id> <command> [args…]` runs a command with its original exit status.

If merging hits a conflict, `main` is left alone: the conflict lands in the agent's worktree, and Review offers to have the agent resolve it (or take one side) before merging again.

**Clone** in the task's header (⌘D, or `c` with Vim keys on) clones the selected task: a new task on its own worktree and branch, made from the task's current state (uncommitted edits too), whose agent carries on from a fork of the same chat. The new-task box opens showing what the task was asked so far and asks what the clone should do differently; leave it empty to just fork, and pick another model if you like. Cloning works for Claude Code and Codex, and for pi in a chat.

In the clone dialog, the source agent, machine and branch are fixed and shown as disabled controls. Tab cycles models, ↑ recalls the source task's prompts, and Enter creates the clone.

An agent that has ended can be revived: it resumes its last session in the same worktree, with a new prompt if you give one (also how Review hands a conflict to an agent that has already exited).

Agents waiting on a permission show it as a card at the top of the sidebar, with Allow once and Deny right there. **See all** on a machine's header opens every task on it, ended ones too, grouped by state with one action per row: Approve, Answer, Review, Open or Revive.

Eight colour schemes are in the menu at the top right: Gruvbox, Ink, Moss and Ember (dark), Gruvbox Light, Snow, Paper and Linen (light).

Prefer vim keys? Turn on **Vim keys** in the scheme menu (top right). The bindings are in [vim.json](vim.json), and `?` shows them in the app.

In normal mode, `h` moves left and `l` moves right through **Workspace → Review → Terminal**, skipping unavailable tabs and stopping at either end. Press `H` (Shift+H) to open **See all** for the selected task's machine. Press `d` on a task in Workspace to open a confirmation dialog before deleting the thread and its worktree; `y` confirms and `n` or Esc cancels.

Everything also works from the terminal:

```sh
arranger new claude "fix the flaky test"   # start an agent on a fresh worktree (in a chat)
arranger ls                                # agents and what they're doing
arranger attach 3                          # its terminal; Ctrl-\ detaches
arranger approve 3                         # or: deny 3 [reason]
arranger run 3 cargo test                  # test directly in its worktree
arranger diff 3                            # changes since its base or your last commit
arranger send 3 "also update the docs"     # type into it
arranger interrupt 3                       # stop its current turn
arranger revive 3 "carry on"               # resume an ended agent
arranger clone 3 "try it with a cache"     # its chat and edits, on a new worktree
arranger commit 3 --merge                  # commit, merge into its base branch
arranger commit 3 --pr                     # or: commit, push, open a PR
arranger stop                              # stop the daemon and its agents
```

`arranger new` runs claude and codex headless, in a chat, unless you pass `--terminal`; `--headless` turns it on for pi and gemini, and `--effort <level>` sets the reasoning effort. Headless agents start with the flags that keep them as close to the interactive agent as possible: `scripts/parity.py` (and `scripts/parity.py --agent codex`) sends one prompt both ways through a logging proxy and checks the API requests match. `arranger chat <id>` prints the conversation; `events`, `send`, `approve`/`deny` and `interrupt` work as for any task, and revive and clone resume its session.

## Other machines

Agents can run on other machines too, side by side with local ones in the same window. Install arranger on the machine (same release), then click **Add machine** in the sidebar, or:

```sh
arranger machine add gpu-box you@10.0.4.12 ~/src/api   # name, ssh target, default repo
```

The app reaches each machine with your own `ssh` (keys, agent, `~/.ssh/config`) and starts its daemon if it isn't running. Nothing listens on the network.

## Develop

```sh
just test        # cargo test
just bench-mem   # daemon RSS with 20 fake agents
just screenshot  # re-render the README screenshots (light and dark) with demo agents
```

Screenshot generation requires `ffmpeg`, or macOS's built-in `sips`.

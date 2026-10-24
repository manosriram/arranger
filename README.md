<p align="center"><img src="assets/banner.svg" alt="arranger" width="100%"></p>

arranger runs many CLI coding agents (Claude Code, Codex, Gemini, pi) side by side, each on its own git worktree. A small daemon keeps them alive when the window closes and watches what each one is doing (edits, commands, permission requests, questions). The window shows who needs you, lets you answer in place, and takes you from diff review to a PR.

Linux and macOS. Rust throughout; no webview.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/screenshot-dark.png">
  <img alt="arranger with six agents on three machines: two need you, three are working, and the selected one is done" src="assets/screenshot.png">
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
| ⌘J / ⌘K | Next / previous agent |
| ⌘I | Message the selected agent |
| ⌘1 / ⌘2 | Allow / deny a permission request |
| ⌘R | Review the diff: revert hunks, comment, then merge (or commit, or open a PR). Greyed out while there's nothing to review |
| ⌘T | Switch between dark and light |

On Linux use Ctrl+Shift in place of ⌘.

A soft ding plays when an agent finishes or needs your help. On Linux, sound requires `paplay` (PulseAudio/PipeWire) or `aplay` (ALSA).

Each task gets its own worktree on a new branch, `arranger/<id>`, made from the base branch you pick when you start it (the current branch by default). The base branch only changes when you merge into it from Review.

**Run in worktree** in Review opens a terminal in the task’s checkout (over SSH for remote tasks). Run your app or tests there before merging; uncommitted edits are included. From the CLI, `arranger run <id>` opens a shell, or `arranger run <id> <command> [args…]` runs a command with its original exit status.

If merging hits a conflict, `main` is left alone: the conflict lands in the agent's worktree, and Review offers to have the agent resolve it (or take one side) before merging again.

An agent that has ended can be revived: it resumes its last session in the same worktree, with a new prompt if you give one (also how Review hands a conflict to an agent that has already exited).

The **Overview** tab lists every agent on every machine in one table, grouped by state, with one action per row: Approve, Answer, Review or Open. Ended agents live there too; click "N ended" under a machine to see that machine's.

Eight colour schemes (four dark, four light) are in the menu at the top right.

Prefer vim keys? Turn on **Vim keys** in the scheme menu (top right). The bindings are in [vim.json](vim.json), and `?` shows them in the app.

Everything also works from the terminal:

```sh
arranger new claude "fix the flaky test"   # start an agent on a fresh worktree
arranger ls                                # agents and what they're doing
arranger attach 3                          # its terminal; Ctrl-\ detaches
arranger approve 3                         # or: deny 3 [reason]
arranger run 3 cargo test                  # test directly in its worktree
arranger diff 3                            # changes since its base or your last commit
arranger send 3 "also update the docs"     # type into it
arranger revive 3 "carry on"               # resume an ended agent
arranger commit 3 --merge                  # commit, merge into its base branch
arranger commit 3 --pr                     # or: commit, push, open a PR
arranger stop                              # stop the daemon and its agents
```

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

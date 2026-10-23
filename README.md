<p align="center"><img src="assets/banner.svg" alt="arranger" width="100%"></p>

arranger runs many CLI coding agents (Claude Code, Codex, Gemini, pi) side by side, each on its own git worktree. A small daemon keeps them alive when the window closes and watches what each one is doing (edits, commands, permission requests, questions). The window shows who needs you, lets you answer in place, and takes you from diff review to a PR.

Linux and macOS. Rust throughout; no webview.

## Build

```sh
cargo build --release   # builds target/release/arranger and arranger-gui
```

Requires `git`. Install `gh` if you want to open PRs from the app.

## Use

Run `arranger` inside a git repo. It starts the daemon if needed and opens the window.

| Key | Action |
|-----|--------|
| ⌘N | New task: pick an agent, write the prompt |
| ⌘J / ⌘K | Next / previous agent |
| ⌘I | Message the selected agent |
| ⌘1 / ⌘2 | Allow / deny a permission request |
| ⌘R | Review the diff: revert hunks, comment, commit, open a PR |

On Linux use Ctrl+Shift in place of ⌘.

Everything also works from the terminal:

```sh
arranger new claude "fix the flaky test"   # start an agent on a fresh worktree
arranger ls                                # agents and what they're doing
arranger attach 3                          # its terminal; Ctrl-\ detaches
arranger approve 3                         # or: deny 3 [reason]
arranger diff 3                            # changes against its base
arranger commit 3 --pr                     # commit, push, open a PR
arranger stop                              # stop the daemon and its agents
```

## Develop

```sh
just test        # cargo test
just bench-mem   # daemon RSS with 20 fake agents
```

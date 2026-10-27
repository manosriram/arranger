# Pipeline: setup → working → checks → summary → review → merge

## Decision

Every thread runs one pipeline, driven by the daemon:

```
Setup ──▶ Working ──▶ Checks ──▶ Summary ──▶ Review ──▶ Merge
  once     a trip      planned per trip,       on the tree      the tree that
           = one turn  run by arranger         that passed      passed is merged
             ▲            │ fail
             └────────────┘ send failures to the agent (a new trip)
```

- **No config in the repository.** Agents design setup and checks. Arranger keeps plans,
  approvals and settings in its own database.
- **Trip** = one agent turn, from a prompt to `Event::TurnDone`.
- **Arranger runs every step itself** and records exit codes. The agent never reports its own
  checks as passed.
- **Everything is keyed on the worktree's tree hash** (a temporary index, `git add -A`, then
  `write-tree`, without touching the real index). Checks pass for a tree, the summary describes a
  tree, and merge compares against the tree that passed.
- GitHub integration (CI, PR comments) is out of scope.

The header shows the stages as a strip:
`Setup ✓ › Working ● › Checks › Summary › Review › Merge`.

## Side-runs (`src/side.rs`)

One-off, read-only runs of an agent CLI that answer JSON matching a schema. They never touch a
task's chat or session.

- **claude:** `claude -p --output-format json --json-schema <schema> --tools Read,Grep,Glob
  --no-session-persistence`, with the prompt on stdin. The answer is in `structured_output`, and
  the cost in `total_cost_usd`.
- **codex:** `codex exec --ephemeral --sandbox read-only --output-schema <file> -o <file> -`.
- **Which one:** the task's own agent if it's claude or codex, otherwise whichever is installed.
  `$ARRANGER_PLANNER` overrides this: a program for tests, or `off`.

## 1. Setup

- **The plan.** A side-run in the main checkout gets the list of ignored, small, top-level files
  there (`.env`, local config) and answers:
  - `copy`: which of those listed files the worktree needs;
  - `commands`: install/generate commands, run in order. No building, testing or servers;
  - `note`.

  Arranger keeps only files from the list it offered, and single-line commands of at most 500
  characters, at most 10 of them.
- **Cache.** `setup_plans (repo, key, plan, approved)`, where `key` is a hash of the manifests and
  lockfiles at the task's base commit plus the offered file names. The agent plans once; later
  tasks reuse the plan until dependencies change. Tasks planning the same key at once share one
  side-run.
- **Approval.** Plain installs (`npm ci`, `pnpm install --frozen-lockfile`, `cargo fetch`,
  `uv sync`, …) run without asking. Any other command needs your approval once per plan, stored as
  `approved`.
- **Run.**
  - Copy the files: regular files only, through anchored directory handles, never overwriting.
  - Then `sh` runs each command in the worktree, in its own process group, printing `$ cmd` before
    each and stopping at the first failure.
  - Environment: `ARRANGER_TASK`, `ARRANGER_ROOT`, `ARRANGER_WORKTREE`, `PORT`.
  - Output goes to `logs/<id>.setup`. Timeout 20 min.
- **Task state.**
  - The database state is `setup` until setup ends. Stages: planning → approval → running →
    ok / failed / skipped.
  - The list shows `setting-up` (planning or running) or `setup-needs-you` (approval or failed).
  - Then: a new task is queued for its agent, and a clone starts its forked session.
  - A daemon restart turns planning or running into failed ("Interrupted").
- **Actions:**
  - Approve: run it and remember it.
  - Skip: start the agent without setup.
  - Retry: run the same plan again.
  - Replan: forget the plan and plan again.

  In the window, these appear on the needs-you card and in the setup panel. On the CLI:
  `arranger setup <id> [approve|skip|retry|replan|log]`.

## 2. Checks

### Trigger

On `TurnDone` (the Stop hook for terminal tasks, the result message for headless ones), a pass
starts 2 s later if all of these hold:

- the task is idle, with no ask and no queued prompts;
- the tree hash differs from the last tree that had a pass;
- no pass is already running.

Other triggers:

- **Run checks** (a button, `arranger checks <id> --run`) always runs.
- **Merge on a stale tree** runs a pass first.

A prompt sent while a pass runs stops the pass (SIGTERM to the process group, SIGKILL after 5 s)
and marks it stale.

### Planning

A **separate read-only planner** side-run, so the agent isn't grading its own work. It sees:

- the trip's prompts;
- the diff since the last passing tree, capped at 100 KB;
- the manifests;
- the previous trip's checks and their results.

It answers `{checks: [{name, command, why}], none_reason}`.

**The final list (built by arranger):**

- the planner's checks;
- **plus every check that failed earlier and hasn't passed since**, even if the planner left it out.

So the agent can't get rid of a failure by having it go unchecked. If code changed and the list is
empty, the result is "Unverified" (amber). If only docs or non-code files changed, the result is
"No checks needed".

**Approval:**

- These run without asking: a built-in list (`cargo test|check|clippy|build`, `npm|pnpm|yarn|bun
  run <existing script>`, `go test|vet`, `pytest`, `tsc --noEmit`, `eslint`, `ruff check`,
  `mypy`, `make|just <existing target>`), and commands you've already approved for the repo
  (`approved_checks`).
- Anything else waits in needs-you for Approve / Always allow / Edit / Remove.

### Running

- **Order and settings:** one check at a time, all of them even after a failure.
  `FORCE_COLOR=1 CLICOLOR_FORCE=1 CARGO_TERM_COLOR=always`. Timeout 10 min (an app setting).
- **Mutation guard:** a check that changed the tree has the tree from before it restored, and is
  marked ✗ "changed files".
- **Manual runs:** **Run checks** and per-check **Rerun**, from the Checks tab or Review.

### Failure

- **Needs-you** "Checks failed (2/4)", with:
  - **Send to agent:** the failing commands, exit codes and log tails, prefilled in the message
    box. That starts a new trip.
  - **Rerun**
  - **Accept anyway**
- **Auto-fix rounds** (an app setting, default 0) send failures back to the agent automatically.

## 3. Console: check logs

- **Storage.** `logs/<task>/<job>.log.zst`, capped at 32 MiB.
- **Normalising** (`src/console.rs`):
  - `\r` overwrites, so progress bars collapse to their final state;
  - SGR colours are kept, other escapes are dropped;
  - error lines are flagged (`error[:\[]`, `FAILED`, `panicked`, `Traceback`, `npm ERR!`, …).
- **Protocol.** `Request::Console { job, start, limit }` returns a page plus a subscription, and
  `ConsolePatch` pushes updates every ≤50 ms.
- **Checks tab** (Workspace → Checks → Review):
  - **Rail:** trips and their steps (setup, planner, checks, summary).
  - **Console header:** name, command, exit code, duration, origin, Rerun / Stop / Send to agent.
  - **Console body:** line numbers, error gutter marks, tail-follow, ⌘F search, jump to first
    error, wrap, copy.
- **Replaces** Review's check-output popup.

## 4. Summary

- **When.** Once a pass succeeds (or is accepted), a forked side-run of the working agent's
  session writes the summary:

  | Agent | Fork |
  |---|---|
  | claude | `--resume <id> --fork-session -p` |
  | codex | `thread/fork` |
  | gemini | fresh, with the diff |

- **Answer:** `{title, overview, files: [{path, change}], risks}`. Arranger fills in
  `verified_by` from the checks that passed.
- **Storage.** `summaries (task, tree, json)`. Review shows the summary whose tree matches, or marks
  it out of date with a Regenerate button.
- **Uses.** It becomes the default commit/merge message.

## 5. Review: as now, plus

- **Banner:** passed on this exact diff, stale (with Run checks), unverified, or failed but
  accepted.
- **Per-file notes** from the summary.
- Comments sent to the agent start a new trip.

## 6. Merge: as now, plus

- **Gate.** If the current tree isn't the tree that passed, the button becomes a red **Force
  merge**. The CLI needs `--force`.
- **Base moved.** When merging base into the task's branch changes the tree, a quick pass runs (the
  last list, no planner) before the fast-forward.
- **Message.** The commit message is prefilled from the summary.

## Phases

1. **Setup:**
   - side-runs and the setup plan, cache, approval and run;
   - the `setup` task state;
   - the stage strip (Setup and Working are live; the rest are placeholders);
   - the setup panel, and needs-you card buttons;
   - the CLI.
2. **Console + manual checks:** `console.rs`, the Checks tab, `arranger checks`, Run checks with a
   command list you give.
3. **Trigger + planner:** the TurnDone trigger, the check planner, building the list, approvals,
   the mutation guard.
4. **Failure loop:** Send to agent, Accept anyway, auto-fix.
5. **Summary.**
6. **Review and Merge gates.**

## Progress in worktree 145

Phases 1–3 were carried forward from worktree 143. Phase 4 now includes editable failure
reports in the GUI, acceptance tied to the checked tree, needs-you status, rerunning the
latest list, and persisted auto-fix rounds (default 0; configurable in Checks or the CLI).
Auto-fix rounds reset when checks pass or a prompt is sent through Arranger. The report
composer also works for terminal tasks. Phases 5–6 are now implemented too:

- Structured summaries are stored by task and tree, generated after passing/skipped/accepted
  checks, and refreshed on request. Claude resumes a fork; Codex uses a separate app-server
  thread fork. Missing sessions and other agents use fresh read-only runs with context.
- Review shows verification and summary freshness, per-file notes, and defaults the commit
  message from a matching summary. Verification is taken from Arranger's actual runs.
- Normal merges require a matching passed/skipped/accepted run. Stale merge attempts request
  checks; the GUI exposes Force merge and the CLI requires --force for an override.
- Merge preparation never updates the base. If integrating the base changes the tree, the last
  check list runs again; only a successful run allows the final fast-forward. Snapshot changes,
  changed commit-hook output, further base moves, or interrupted/failed checks block that update.
- Tests use fake Claude/Codex processes; no paid model calls are made by the test suite.

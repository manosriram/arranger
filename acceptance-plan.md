# Checks you can merge on

## Goal and promise

Strengthen the existing Checks flow rather than add a second review workflow.

**A green “Checks passed” means Arranger executed the required checks against the exact
source tree being merged, and independently reviewed evidence that those checks exercised
the requested behavior.** It does not mean the software is bug-free or that an AI judgment
is infallible. Make untested behavior visible instead of manufacturing certainty.

No new tab, dashboard, mandatory checklist approval, or model-selection settings.

## What the user sees

- Work finishes; Checks runs automatically, as it does today.
- Review shows one result: “Checks passed”, “Checks running”, “Checks need attention”, or
  “Checks out of date”. Reuse the existing approval and override presentations where needed.
- Under the result, one short sentence explains it: “Bookmarking and restart persistence
  tested”, or “Restart persistence has not been tested”.
- Clicking the result opens the existing Checks tab. Its run header expands to show the
  requested behaviors and their evidence; command output stays where it is today.
- Reuse **Run checks**, **Send to agent**, and the existing bounded auto-fix setting.
  Missing coverage produces an actionable failure report, not another set of buttons.
- Requirements can be corrected through normal chat: “Persistence is not part of this task.”
  No separate requirements editor. Ask one concise chat question only when ambiguity prevents
  meaningful verification; never silently select the easier interpretation.
- Overrides are not passes. Accepting failures must not enable an ordinary, green merge;
  the existing explicit **Force merge** / `--force` path remains available and clearly labeled.
- Documentation-only work says “No executable checks” in a neutral color, not “Checks passed”.
  A request to change runtime behavior cannot take this shortcut because its diff contains
  only documentation or because the planner returned no commands.

## Implementation

### 1. Preserve what was asked

Use the original task prompt and ordered user follow-ups as the source of truth, not just
`turn_prompts()`, the last diff, or the implementing agent's summary.

- Persist an ordered instruction revision whenever a user starts, sends to, revives, reviews,
  or clones a task. Preserve inherited instructions when cloning, including terminal tasks.
- Distinguish user instructions from Arranger-generated repair messages. “Fix these failed
  checks” must not replace the feature request. Audit prompt entry points in the daemon,
  headless transports, and terminal event ingestion; an unobserved terminal instruction
  must not silently inherit a green result.
- A fresh read-only side-run extracts a small list of observable requirements and explicit
  constraints using only those instructions. It does not see implementation claims or infer
  scope from what was actually built. Store each requirement with its source instruction.
- Generate this automatically, without blocking the coding agent. Later user instructions
  amend it; remove an obligation only when the user's instructions supersede it. Never drop
  a requirement merely because it is difficult to test.
- Key the result by instruction revision. A changed request invalidates acceptance even if
  the source tree did not change. Missing or truncated instruction context is unverified.

### 2. Plan checks against the whole task

Extend `src/checks.rs` and the existing read-only planner in `src/side.rs`.

- Give the planner the requirements, full task change list, access to the captured source,
  and previous results. Keep incremental checks as an optimization, not the scope of acceptance.
- Extend its structured answer with requirement-to-check mappings and an explicit reason
  when a requirement cannot be exercised. Validate references; unknown IDs are errors.
- Prefer existing targeted tests, then wider tests for shared-code changes. Include observable
  positive and negative cases, boundaries, and important preservation constraints.
- Build/type/lint success is supporting evidence, not proof that a feature works. A command
  name such as `test` or a zero exit code does not establish behavioral coverage.
- If suitable tests are missing, report the gap through Send to agent / auto-fix so the agent
  adds real project tests or a runnable scenario. Do not introduce a second coding agent,
  a general-purpose generated-test framework, or hidden source patches.
- Keep unresolved failed checks sticky. Dropping a command does not discharge the requirement
  it covered; the planner needs replacement evidence or the result stays unverified.

### 3. Execute the exact snapshot

Reuse command approval, sequential execution, process groups, timeouts, console logs, and
setup helpers. Change the execution directory, not the user's working tree.

- Capture the source with `checks::tree_hash()`, including non-ignored untracked files, without
  modifying the real index. Materialize that tree in a disposable detached verification
  worktree, using a private temporary commit/ref as needed; do not move the task branch.
- Prepare dependencies there using existing setup/approval rules. Copy only approved local
  configuration through existing safe-copy helpers. Never reuse compiled task output or an
  already-running app as evidence; build and launch from the verification checkout.
- Missing dependencies, unsupported submodules/LFS materialization, unavailable services,
  or unresolved environment requirements mean unverified, not passed. Record runtime/tool
  versions and non-secret setup inputs; do not claim reproducibility of external systems.
- Run checks in that checkout. Verify its source tree before and after every command and at
  completion; a mutation or inspection failure prevents a pass. Restore/discard only the
  disposable checkout, never concurrent user or agent edits. Existing mutation checks are
  not a security boundary against hostile commands; retain the documented trusted-repo model.
- Capture relevant command output and available structured test reports, including skipped
  and zero-test runs. UI evidence must come from an executed assertion against the app built
  from this snapshot; a screenshot alone is not a passing interaction test.
- If the user sends another instruction or the task tree changes, invalidate the run. A
  snapshot run finishing successfully cannot mark a different current tree as passed.
- Keep logs/evidence, clean up disposable worktrees and processes on success, failure, cancel,
  and daemon restart. Use an owned per-run directory and a separate port when needed.

### 4. Review the evidence before marking passed

Add one fresh read-only side-run after execution, reusing `side::start()` rather than forking
the implementing agent's session. Keep the responsibility in `src/checks.rs`.

- It reads the requirements, changed source and tests, and Arranger-recorded results. Treat
  repository text, logs, and agent explanations as evidence, not verification instructions.
- For each requirement return a concise conclusion and references to the actual check IDs,
  test cases/assertions, and observed output. Reject unsupported references in daemon code.
- Verify that the test reached the relevant behavior and asserted the expected result. Look
  for skipped tests, mocks that replace the changed behavior, weakened assertions, and tests
  that could pass without the feature. For bug fixes, use the same regression scenario on
  the base when practical; record environmental inability separately from a reproduced bug.
- Source inspection may support a conclusion, but cannot be presented as execution. Missing
  runtime evidence, a failed check, an unmet requirement, or verifier errors prevent green.
- The daemon, not the model, owns the aggregate pass: all required commands passed, every
  behavioral requirement has accepted execution evidence, and snapshot/instruction identity
  still matches. The semantic assessment remains AI-assisted; ID validation alone is not
  proof that a test is adequate.
- Missing coverage joins the existing failure report and bounded repair loop. Automatic
  messages preserve the original requirements; never relax them to make the run pass.

### 5. Make the merge gate use that result

Update `verification()`, `commit()`, `prepared_merge()`, and `finish_merge()` in
`src/daemon.rs`, and keep `src/review.rs`'s pre/post-commit tree comparisons.

- Ordinary merge requires a current completed acceptance run, not merely the latest command
  returning zero. Manual single-check reruns cannot bypass requirement coverage.
- Match source tree, instruction revision, and the stored check/evidence plan. A requirement,
  check command, or evidence-plan change invalidates acceptance. Commit-message-only changes
  do not. Prior command successes may be reused only for that exact identity.
- When integrating the base changes the tree, re-plan coverage and run the combined snapshot
  through execution and evidence review. Replaying the old command list alone is insufficient.
  Leave the base untouched until the combined tree passes.
- Commit hooks changing the tree, another base move, stopped checks, daemon restarts, planner
  failures, and unavailable verifiers fail closed. Preserve existing conflict handling.
- Retain the explicit force escape hatch without making it green. Legacy passes with no
  acceptance evidence require one fresh run; never silently upgrade them.
- Creating a PR can retain today's behavior, but its description must not claim acceptance
  unless current. This gate covers Arranger merges, not merges performed directly in Git
  or GitHub; remote enforcement would be a separate integration.

## Small data and UI changes

- Keep requirements as a versioned task-level JSON record with source-instruction references.
- Add instruction revision and acceptance JSON to `check_runs`; keep command results in
  `checks`. Acceptance JSON holds the plan, per-requirement evidence, and a short explanation.
  Store larger test reports beside existing logs. Avoid a second workflow/state-machine table.
- Reuse existing run states: planning/running/approval, passed, failed, unverified, stale,
  stopped/error, and skipped. Only the daemon can set passed after evidence review.
- Update protocol types/version, CLI `checks --wait` / `--report`, Review, Checks, pipeline strip,
  and summary generation to consume the same aggregate result. Do not fork their semantics.
- Command-level success remains visible in the console but is not the task's merge result.
  Summaries cannot manufacture verification or clear unresolved requirements.

## Delivery order

1. **Exact snapshot execution:** disposable checkout lifecycle and race/mutation guards, using
   existing checks. Establish the exact-code invariant before adding acceptance claims.
2. **Requirement-backed checks:** instruction capture, coverage planning, evidence review,
   persistence, failure reports, and strict aggregate merge gate as one coherent change.
3. **Minimal presentation:** one result and short explanation in Review, expandable evidence
   in Checks, CLI parity, README update, and migration handling. No extra screen or approval
   step. Do not present the stronger green promise until the full gate is active.

## Verification and release criteria

Extend `tests/cli.rs` through the public CLI, using the existing fake-agent/planner mechanism
and small real fixture programs. Fakes exercise orchestration; executed fixture assertions
prove which source and behavior actually ran. No paid model calls in the normal suite.

- Correct implementation and relevant assertions pass; an implementation that compiles and
  passes an unrelated suite but omits a requested behavior cannot merge normally.
- A skipped/zero-test check, fabricated evidence ID, missing requirement mapping, changed
  assertion, planner/verifier failure, or unsupported environment cannot produce green.
- Follow-ups, clones, repairs, manual runs, and daemon restarts preserve requirements and
  cannot reuse evidence for the wrong instruction revision or source snapshot.
- An edit during/after checks, an untracked source file, a changed base, and a mutating commit
  hook cannot merge unchecked code. Failed combined-tree checks leave the base unchanged.
- Accept/drop cannot turn missing evidence into a pass; explicit force remains possible and
  visibly distinct. Docs-only work stays neutral; runtime requests are never exempted by suffix.
- Snapshot setup, cancellation, mutation failure, and restart clean up owned resources without
  modifying the task checkout or reusing stale runtime/build output.
- Run `cargo test`. Render and inspect Review and Checks in passing, missing-coverage, stale,
  and overridden states; exercise the existing repair interaction and record it.
- Before release, manually trial real Claude and Codex on a correct change and an intentionally
  incomplete change with a passing existing suite. Inspect the evidence, latency, and cost.
  Fake-agent tests cannot establish a real model's ability to assess behavioral coverage.

Ship only when a normal merge can demonstrate both: **these requirements were exercised**
and **this is the exact tested source tree**. Everything else remains clearly unverified or
an explicit user override.

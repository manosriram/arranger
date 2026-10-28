# Project-independent acceptance checks

Arranger verifies the target repository using its own executable tests. It does not
require Arranger GUI state, inject UI hooks into applications, or assume one framework.
The read-only planner derives acceptance criteria from the task, inspects the project's
test infrastructure, selects commands, and reports concrete coverage gaps.

For GUI work, discover the application type and its supported driver:

| Target application | Required evidence |
| --- | --- |
| Web application | Browser interactions with rendered DOM, input/focus, navigation, persisted state, and relevant viewport/layout assertions |
| Desktop application | Its UI framework or platform automation driving the application, asserting resulting state and relevant rendered layout |
| Mobile application | Its device/emulator test harness exercising taps, navigation, lifecycle and resulting state |
| Custom-rendered GUI | A project-supported input/rendering harness that drives real application code and checks state/layout or reviewed visual baselines |

Use the harness already in the project. When missing, the coding agent adds appropriate
tests and, where necessary, project-local automation using that framework's conventions.
Dependency installation and privileged/device access follow normal agent permissions.
A compile, source-string search, synthetic substitute UI, or screenshot capture alone
does not establish interaction correctness. Missing drivers, displays, credentials, or
unclear expected visuals must be stated precisely, never replaced by a fabricated pass.

## Planner cost control

The Checks tab's **Check planner** switch applies to all repositories on its daemon,
persists across restart, and defaults to On. Use `arranger settings check_planner off`
or `on` from the CLI (0/1 are accepted too). Off stops active check planners and prevents
new automatic check plans and repair prompts. Existing command execution and explicit
check commands still work. Asking for a plan while Off returns an actionable error;
it does not silently invoke a model. Re-enabling applies to future turns or an explicit
plan request. Setup and summary side-runs are separate from this switch. Merge gates
still require recorded verification or an explicit force override.

## Missing-test repair

When an automatic plan has uncovered outcomes, useful existing checks run first. Arranger
then sends the coverage gaps to the idle coding agent, asking it to add durable regression
tests and run them. Failed checks are included too when the same run has coverage gaps.
After that turn, an independent planner reassesses the work and Arranger executes checks.
Replanning also happens if the agent made no edits, so explanations of prerequisites are
considered; an unchanged tree is not silently treated as covered.

The loop allows two test-writing attempts per task by default. Configure it with
`arranger settings coverage_fix 0` (disable) or a number from 1 to 10. Attempts are stored
in the database and survive restart. They reset after verification succeeds or the user
sends a new prompt through Arranger. A report is not sent twice for the same run.
If attempts are exhausted, the agent is unavailable, or prerequisites remain unresolved,
the task stays Unverified and appears under Needs you. Normal merge remains blocked.
`arranger checks <id> --report` prints the missing-test instructions for manual use.
Failed-check repair has its separate `auto_fix` setting and budget.

## Commands and artifacts

Use isolated fixtures, timeouts, and process cleanup. Commands must be a single readable
line of at most 500 bytes. Substantial tests belong in repository files visible in the
diff, not encoded shell commands. The check planner cannot edit the repository; the
coding agent implements missing tests using the same review process as application code.
Custom commands keep the existing approval rules.

Write evidence such as test reports, screenshots and interaction traces to
`$ARRANGER_CHECK_ARTIFACTS`, outside the checkout. Daemon-managed evidence retains the
latest three runs per task plus active runs. It is reset before the same check executes
again and removed when the task is deleted. Startup also removes orphaned check folders.
Do not regenerate an expected visual baseline from the changed result and call the
match verification. Use reviewed references and deterministic fixture data.

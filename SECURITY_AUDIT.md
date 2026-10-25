# Security audit and remediation

Audit date: 2026-10-26. Baseline: commit `bba69c9`, package version `0.1.27`. Changes are in this working tree; no release has been published or running production daemon replaced.

## Assessment

The review found exploitable input and filesystem handling, unsafe permission defaults, and release/operational hardening gaps. The findings below have corresponding code changes. The revised application passes the local debug and optimized test suites and a RustSec audit with no vulnerabilities or advisory warnings.

This is a source review and targeted regression exercise, not a guarantee that every vulnerability has been discovered. Arranger remains a trusted, single-user desktop application. It must not be advertised as an isolation boundary for hostile code or processes sharing the user's UID. Production deployment for that threat model requires OS isolation; see the remaining requirements below.

## Scope and method

Reviewed the daemon's Unix IPC and request handlers, CLI/client lifecycle, filesystem and session handling, Git worktree/review/clone operations, terminal emulation and paste/replay paths, headless Claude/Codex/pi/ACP permission handling, remote SSH setup, GUI process-launch paths, Cargo dependencies, installer and GitHub release workflow. Development helper scripts were also checked where changes affected their behavior.

The review combined source inspection, command/path/permission tracing, searches for dangerous APIs and secret-like values, dependency advisory checking, and regression tests that exercise actual daemon processes. The secret-pattern search did not identify a credential; it was a heuristic search, not a historical or enterprise secret-scanner assessment.

Severity labels describe plausible impact in the relevant attack scenario. They are qualitative review judgments, not assigned CVEs or CVSS scores. Some entries are hardening improvements rather than independently exploitable vulnerabilities.

## Findings and fixes

| ID | Risk | Original behavior and impact | Remediation |
| --- | --- | --- | --- |
| A01 | High | Clients connected to a predictable socket in a shared temporary directory without checking ownership or file type. Startup could unlink a socket while another daemon was starting. This allowed socket impersonation opportunities and cross-instance interference. | Put sockets inside owner-checked `0700` per-user directories; check socket ownership, type and mode; reject unsafe ancestors; hold an exclusive startup/lifetime lock before binding or removing stale sockets. |
| A02 | High | Default filesystem modes could expose transcripts, prompts, database contents and generated adapters to other local users. Ordinary writes followed symlinks and could modify hard-link targets. | Use private application directories, a `077` daemon umask, `0600` sensitive files, no-follow opens and regular-file/owner/link-count checks. Tighten existing application directory modes. Use descriptor-based chmod. |
| A03 | High | Starting or reviving a task automatically executed the checkout's `.arranger/setup` before the agent and its approval flow started. A repository or changed task checkout could execute code at that point. | Remove automatic execution. Setup is now an explicit action performed after the user reviews it. Regression test commits an executable setup script and verifies it is never run. |
| A04 | High | Claude chat mode selected automatic permission mode when no setting existed and loaded project/local settings without an interactive trust step. | Start chat mode with `--permission-mode default` and `--setting-sources user`. Remove Arranger's automatic-mode selection. User-level settings and agent installations remain trusted. |
| A05 | High | ACP's Allow once implementation fell back to an `allow_always` option when no once option existed, granting more authority than the UI action expressed. | Select only `allow_once`. Cancel if it is unavailable. Add a regression test for an agent offering only a permanent grant. |
| A06 | High, conditional on malicious input | SSH targets were added without an option terminator, allowing option-shaped input to be parsed as SSH configuration, potentially including local command execution through SSH options. Tunnel options were appended after the destination. | Validate host/user@host syntax; reject leading hyphens, whitespace and shell characters; place all options before `--` and the destination; validate remote socket paths before constructing forwarding specifications. Apply validation to GUI-opened remote terminals too. |
| A07 | High | Review used ordinary file reads and cloning used `fs::copy` on untracked paths. Symlinks could disclose files outside the worktree or turn their contents into ordinary cloned files; destination directory links could redirect writes. Special files could block reads. | Walk components using directory handles with `openat` and no-follow flags. Review displays a symlink's target text. Clone preserves symlinks without reading their targets, refuses symlinked destination directories and uses exclusive destination creation. Reject traversal and special files. |
| A08 | Medium | Shutdown trusted a persisted PID. A stale or modified PID file could cause a signal to an unrelated process owned by the user. | Add a shutdown request over the protected socket, bump protocol to 19, and stop using the PID file as signal authority. Integration test replaces the PID with a separate process's PID and verifies that process survives. |
| A09 | Medium | Hook commands did not escape apostrophes in executable paths. Codex effort settings interpolated strings into TOML, and pi positional prompts lacked an option separator. | Use proper shell word quoting and serialized configuration strings; add `--` before pi prompt text. Remove the blanket Codex hook-trust bypass flag. |
| A10 | Medium | Unified-patch headers interpolated repository filenames directly. Newlines and tabs could alter patch structure or target interpretation. | Use Git-compatible C quoting for path bytes, preserving the `/dev/null` sentinel. Test reverting a filename containing a newline, tab, quote and backslash. Git apply's own path restrictions remain enabled. |
| A11 | Medium | Daemon connections accumulated bytes before frame parsing without an input-buffer ceiling or connection-count limit. Output checks happened after extending buffers. Connections could remain partially initialized indefinitely. | Bound incoming connections and input/output buffers; reject oversized frame headers early; expire idle initial/partial requests; check output size before extending; handle zero-byte writes. Return a readable error for oversized replies. |
| A12 | Medium | JSON-line limits were checked after complete lines had already been parsed. Clearing an oversized fragment could let a suffix be interpreted as a fresh record. Prompt queues and chat/agent streams could keep growing. | Assemble bounded records and discard a whole oversized record through its newline. Bound queued prompts and stream/display bytes. Stop a headless task that exceeds its stream limit. Flush Arranger-generated chat records at the usual chunk threshold. |
| A13 | Medium | Replay decoded compressed logs without an output ceiling and read entire compressed files into memory. A large/compressed log could exhaust memory. | Stream decompression through a bounded reader, limit decoded output to 64 MiB and bound compressed log responses. Apply limits to both daemon chat loading and GUI terminal replay. |
| A14 | Medium | Client-supplied or replayed terminal dimensions could request grids approaching 65535 × 65535 cells. | Clamp dimensions before terminal-grid allocation and daemon PTY resizing to 2–400 columns and 1–200 rows. Test maximum integer dimensions without a large allocation. |
| A15 | Medium | Agent/repository text was printed raw in non-terminal CLI reports. Clipboard text could contain terminal controls or end bracketed paste early; unbracketed multiline pastes could submit commands. | Escape controls in ordinary CLI reports. Strip controls from pasted/prompt text before generating bracketed-paste wrappers. Convert line breaks to spaces in unbracketed paste, requiring Enter to execute. Explicit terminal attachment remains a raw terminal stream. |
| A16 | Hardening | Remote connections accepted previously unknown host keys automatically and inherited SSH-agent/X11 forwarding choices. Forwarded socket naming could collide after sanitization or between GUI processes. Endpoint output and execution time were unbounded. | Require known host keys; disable agent/X11 forwarding; request private forwarded-socket permissions. Use bounded, process-specific socket names. Bound endpoint stdout/stderr and enforce a startup deadline. Reap timed-out tunnel processes. |
| A17 | Medium / supply chain | The installer streamed curl directly into tar. POSIX shell pipeline status could hide a failed download, and archives had no separately checked digest. Workflow actions floated by tag, build jobs inherited release-write permission, and checkouts retained credentials. | Complete downloads first; require HTTPS and a matching SHA-256 before extraction. Publish checksums. Pin actions by commit, disable checkout credential persistence, restrict write permission to publication, use locked builds, add job timeouts, and make publication depend on a clean advisory audit. |
| A18 | Maintenance | RustSec reported two unmaintained transitive crates: `atomic-polyfill` and `ttf-parser`. They were warnings rather than known exploitable vulnerabilities in this application. | Disable unused postcard default features and use Wayland's supported titleless decoration feature. Prune both crates and their unused dependencies from Cargo.lock without a blanket dependency upgrade. |
| A19 | Hardening | Hook asks for nonexistent tasks could stay pending. Multiple asks on one connection could accumulate ambiguous state. | Deny asks without a live task and additional asks already pending on the same connection. Retain the existing deny/cancel handling for unsupported headless methods. |
| A20 | Hardening | The sandbox profile's home-path regex did not escape all regex and string-literal metacharacters. | Escape the regex and then the profile string literal. Keep tests covering generated rules and the existing macOS shell-confinement integration check. This improves profile correctness but does not turn the Terminal tab into full hostile-code isolation. |

Implementation centers on [src/security.rs](src/security.rs), with call-site changes in [daemon](src/daemon.rs), [client](src/client.rs), [protocol](src/proto.rs), [SSH support](src/machine.rs), [review](src/review.rs), [agent adapters](src/adapter.rs), [headless handling](src/headless.rs), [terminal handling](src/screen.rs) and the GUI.

## Dependency evidence

Used `cargo-audit 0.22.2` with the [RustSec advisory database](https://github.com/RustSec/advisory-db). The fetched database contained 1352 advisories at commit `21d909e4c8bd4e14c7aea58bf980c881a0053ab1`, last updated 2026-10-24.

| Lockfile | Packages | Known vulnerabilities | Advisory warnings |
| --- | ---: | ---: | ---: |
| Before | 348 | 0 | 2 unmaintained-crate warnings |
| After | 339 | 0 | 0 |

Removed warning advisories: `RUSTSEC-2023-0089` (`atomic-polyfill`) and `RUSTSEC-2026-0192` (`ttf-parser`). No advisory ignore list was introduced. Auditing the lockfile covers packages recorded for other target platforms, although platform execution was only performed locally on macOS.

The final check passed with warnings denied:

```sh
cargo audit --deny warnings
```

For the local repeat check, the command additionally selected the temporary advisory database with `--db` and used `--no-fetch`, so it checked the same fetched snapshot.

## Validation performed

Environment: macOS ARM64; Rust/Cargo 1.97.1.

| Check | Result |
| --- | --- |
| `cargo test --locked` | Passed: 58 library, 15 GUI and 5 daemon/CLI integration tests; 78 total |
| `cargo test --release --locked` | Passed: the same 78 tests using optimized builds |
| `cargo clippy --locked --all-targets -- -D warnings -A clippy::too_many_arguments -A clippy::type_complexity` | Passed |
| `python3 tests/install.py` | Passed: 4 offline installer tests |
| `sh -n scripts/install.sh` | Passed |
| `bash -n scripts/demo.sh` | Passed |
| Python syntax compilation of installer tests, parity helper and demo tunnel | Passed |
| `git diff --check` | Passed |
| RustSec audit with warnings denied | Passed: 0 vulnerabilities, 0 warnings |

Clippy's two explicit exceptions preserve the existing UI/launch APIs' parameter counts and tuple payloads. Other warnings are errors. No security lint or advisory was suppressed.

The baseline debug unit tests passed, but its daemon integration tests could not bind/start under the workspace sandbox. The completed integration and optimized runs used the required socket/process permissions and passed. The existing macOS sandbox integration test probes for inherited sandbox restrictions and can skip its confinement portion if the environment forbids applying another sandbox; passing the test suite is not a general sandbox certification.

Meaningful regression coverage includes:

- An executable repository setup script is never automatically run.
- Application directories/socket have private modes.
- An oversized frame is rejected while the daemon continues answering valid clients.
- A nonexistent task's permission ask is denied.
- A tampered PID file cannot redirect shutdown to another process.
- Sensitive writes reject symlinks and hard links without modifying their targets.
- Review reads link text, clone preserves links, and symlinked intermediate/destination directories are refused.
- Oversized fragmented JSON cannot reenter the protocol through a forged suffix.
- Special filenames survive safe patch generation and revert.
- Allow once does not become a permanent ACP grant.
- Terminal dimensions, SSH endpoint capture, traversal inputs and paste controls are bounded or rejected.
- Failed downloads/checksum mismatches never reach archive extraction or overwrite an existing installation.
- Task creation, approvals/denials, shell confinement, review/merge, revive, clone and headless ACP/chat continue working through a real daemon.

The demo SSH shim was updated to use a real private Unix-socket relay instead of a symlink, and its broad process-name kill was removed. Screenshot rendering itself was not run.

## New operational limits

| Boundary | Limit / behavior |
| --- | --- |
| Incoming daemon buffer / frame | 1 MiB per connection |
| Outgoing frame / queued bytes | 16 MiB; oversized replies return an error |
| Client connections | 128 |
| Initial/partial request inactivity | 30 seconds |
| Agent JSON record | 1 MiB; oversized headless records stop the task |
| Headless output per process run | 64 MiB; excessive output stops the task |
| Decoded log / chat display input | 64 MiB |
| Queued headless prompts | 32 prompts, 1 MiB aggregate text |
| Terminal dimensions | 2–400 columns, 1–200 rows |
| File review | 2 MiB per side/file; 8 MiB aggregate; 2000 displayed changed files |
| Event-history materialization | 8 MiB JSON or 100,000 events; larger requests return an error |
| Commit-job captured output | First 1 MiB retained; later output drained |
| SSH endpoint capture | 64 KiB each for stdout/stderr; 30-second deadline |
| Simultaneous agents | Default 20; environment override clamped to 1–64 |

These are per-operation/per-task safeguards, not a total process-memory or disk quota. There is no automatic transcript-retention policy. Large workflows may need a new task, direct Git review or external retention/volume limits. Repeatedly reviving a very large task does not remove its stored history.

## Compatibility and upgrade procedure

1. Stop existing daemons with the **old executable**, before replacing it. Closing the GUI leaves the daemon alive. Do not remove a live daemon's socket to force a new instance.
2. Deploy the same new release to local and remote machines. The protocol is now **19**. Unsafe or old sockets are deliberately refused; there is no insecure compatibility fallback.
3. New default socket path: `${XDG_RUNTIME_DIR:-/tmp}/arranger-<uid>/arranger.sock`. `TMPDIR` is no longer used as the default socket root, avoiding long macOS socket paths. `ARRANGER_HOME` still overrides data and socket locations and must identify a dedicated absolute directory with trusted ancestors.
4. SSH to a new host yourself, verify its fingerprint through a trusted channel, then add it. Existing machine names containing spaces/punctuation outside letters, digits, hyphens and underscores must be renamed; invalid saved entries are not loaded.
5. Review and run setup scripts explicitly. Claude chat mode loads user settings only and uses `default` permissions. Codex terminal hooks follow its normal trust mechanism. Existing explicit user-level tool permissions remain the user's responsibility.
6. New installer code requires checksum assets. It fails closed on older releases without them. Release publication now produces these assets.
7. On Wayland, decoration controls remain available but the decoration's title text is omitted to remove the unmaintained parser. The macOS UI is unaffected by that dependency choice.

Existing data-directory modes are tightened, but that cannot revoke file descriptors already opened by another process, undo a past disclosure, or protect old backups. If there is evidence of past credential exposure, rotate those credentials through their provider.

## Remaining production requirements and limits of assurance

**Trusted-user deployment only.** A process sharing the user's UID can use the daemon's client interface and approve operations, start tasks or send shell input. Agent executables and configured Git/shell hooks run with the user's authority. Unix modes do not separate these processes. Hostile workloads require a separate OS account or VM, and an agent sandbox that cannot reach Arranger's control socket. The application's permission cards are not an adversarial-agent security boundary.

**Release-owner controls.** Checksums protect against corruption or mismatched downloads relative to the trusted publisher. An attacker controlling both an archive and its manifest can replace both. This work does not supply independent release signatures, macOS signing/notarization credentials, protected-tag settings or GitHub environment approvals. Those require the release owner's account configuration and credentials. The initial installer still relies on the trust of its HTTPS source; review it before execution where stronger assurance is needed.

**Cross-platform execution.** Added Linux/macOS CI and release gates, but the hosted workflows have not been executed from this worktree. Linux, Intel macOS and real remote SSH connections require their release-matrix checks. Native logged-in agent parity checks were not run: they use external accounts and paid model calls. Claude/Codex/pi/Gemini CLI compatibility beyond the included protocol fixtures must be checked against the versions actually deployed.

**Resource and availability controls.** Limits are local to selected boundaries. Git operations and diff work can still occupy the daemon event loop; trusted hooks and subprocesses may be slow. Per-task histories and on-disk logs require operational retention and quotas. This audit did not perform coverage-guided fuzzing, prolonged concurrency/load testing, malicious Git pack fuzzing or a live network penetration test.

**Data handling.** Private permissions are not encryption. Provider SDKs, Git helpers, SSH configuration, executable discovery through PATH and user settings remain trusted. Configure account permissions, disk encryption, backups and provider policies for the intended environment. This audit did not examine provider infrastructure or remote-machine configuration.

For an ordinary trusted single-user deployment, the identified code issues have been remediated and the locally executable checks pass. Claims of universal vulnerability absence or readiness for hostile multi-tenant operation would exceed the evidence from this review.

## Ongoing maintenance

- [Security workflow](.github/workflows/security.yml): advisory checks on changes and weekly; macOS/Linux tests; Clippy; offline installer tests.
- [Release workflow](.github/workflows/release.yml): locked builds and optimized tests, fixed action commits, checkout credential persistence disabled, read-only build/audit permissions, checksums, and an audit dependency before publication.
- [Dependabot](.github/dependabot.yml): weekly Cargo and GitHub Actions update proposals.
- [Security policy](SECURITY.md): threat model, reporting guidance and operational requirements.

SSH option and forwarding semantics were checked against the [OpenSSH manual](https://man.openbsd.org/ssh). Dependency methodology follows the [RustSec cargo-audit documentation](https://github.com/rustsec/rustsec/blob/main/cargo-audit/README.md).

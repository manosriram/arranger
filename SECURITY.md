# Security policy

Arranger is intended for one trusted OS user running trusted coding-agent executables and repositories. It provides task management and approval presentation, not a security boundary between programs sharing a Unix UID. Never run the daemon as root.

## Trust boundaries

- Other unprivileged OS users must not be able to connect to or replace daemon sockets, read task transcripts or overwrite generated adapter code. Application directories are owner-checked and private, and application file writes reject symlinks, hard links and special files.
- Agent output, repository file contents, filenames and remote endpoint replies are treated as potentially malformed. Parsing, decoding, display and connection buffers have limits. File review and untracked-file cloning do not follow symlinks out of a checkout.
- Only known SSH host keys are accepted. Hostnames cannot inject options, forwarded socket paths cannot introduce forwarding delimiters, and agent/X11 forwarding are disabled. Your SSH configuration and any ProxyCommand it contains remain trusted configuration.
- Explicit user approvals must preserve their scope. ACP's Allow once never selects Allow always. Unsupported requests are denied or cancelled. Claude chat mode uses interactive approvals and does not load repository settings.

## What requires OS isolation

Agents and processes with your UID can access Arranger state and issue client requests, including approvals, task launches and shell input. File permissions do not prevent this. A malicious process already running as your UID, an administrator, a compromised agent executable or a compromised remote account falls outside the daemon's local-user boundary. An agent's own sandbox must block access to Arranger's control socket if the agent is being treated as adversarial.

Use a dedicated account or VM without personal credentials for hostile repositories or workloads. Git worktrees share repository metadata. Git hooks, filters, credential helpers and shell startup files can execute code when their associated tools run. Review them before using a repository. The macOS Terminal-tab profile is limited confinement with broad cache/Git access; Linux has directory hooks only. Neither is a general-purpose hostile-code sandbox.

Transcripts and task metadata are stored locally and may contain source code, prompts, paths and tool output. Private permissions do not encrypt this data. Apply disk encryption, backup access controls, retention policies and volume quotas appropriate to the workload. Configure each agent's model provider and credential handling independently.

## Reporting and maintenance

Report suspected vulnerabilities privately to the repository maintainer through GitHub private vulnerability reporting when available. Avoid posting credentials, proprietary transcripts or exploit details in public issues before coordination.

CI checks the lockfile with RustSec on changes and weekly, denies advisory warnings, and tests macOS and Linux. Releases depend on a clean advisory audit. Dependabot proposes Cargo and GitHub Actions updates. Review action and dependency updates before merging. Only deployed, rebuilt releases receive fixes; installing a newer client does not update an already running daemon.

Release checksums are published with each archive. Checksums share the release's trust root. Independent release signatures, protected tags and signing/notarization credentials must be configured by the release owner for stronger publisher assurance.

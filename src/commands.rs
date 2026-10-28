//! Common slash commands offered by the chat composer. Agents may also accept custom commands.
pub fn names(agent: &str) -> &'static [&'static str] {
    match crate::adapter::kind(agent) {
        "claude" => &["compact", "context", "cost", "review", "help", "model", "permissions", "status", "doctor", "memory", "rewind", "resume", "skills"],
        "codex" => &["compact", "review", "model", "permissions", "status", "diff", "skills", "mcp", "plan", "resume", "fork", "new", "init", "feedback"],
        _ => &[],
    }
}

/// Only a leading slash followed by a command name; paths and prose stay literal.
pub fn name(text: &str) -> Option<&str> {
    let name = text.strip_prefix('/')?.split_whitespace().next()?;
    (!name.contains('/')).then_some(name)
}

pub(crate) fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// Compact, terminal-safe run explanation and requirement evidence for `checks` output.
/// Command rows and the aggregate state remain the caller's responsibility.
pub fn check_run_details(run: &crate::proto::CheckRun) -> String {
    let mut lines = Vec::new();
    if !run.note.trim().is_empty() {
        lines.push(format!("  {}", check_excerpt(&run.note, 240)));
    }
    if run.note.trim().is_empty() && !run.acceptance.summary.trim().is_empty() {
        lines.push(format!("  {}", check_excerpt(&run.acceptance.summary, 240)));
    }
    for requirement in &run.acceptance.requirements {
        let mark = if run.state == "skipped" { "–" } else if requirement.verified { "✓" } else { "!" };
        lines.push(format!("  {mark} {} · instruction {}", check_excerpt(&requirement.text, 160), requirement.source));
        let evidence = if requirement.evidence.trim().is_empty() { "No execution evidence" } else { &requirement.evidence };
        lines.push(format!("    {}", check_excerpt(evidence, 240)));
    }
    lines.join("\n")
}

/// Single-paragraph check preview shared by the CLI and GUI; `max` must be positive.
pub fn check_excerpt(text: &str, max: usize) -> String {
    let text = crate::security::terminal_text(text).split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= max { return text; }
    let mut excerpt: String = text.chars().take(max - 1).collect();
    excerpt.truncate(excerpt.trim_end().len());
    excerpt.push('…');
    excerpt
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{Acceptance, CheckRun, Requirement};

    #[test]
    fn check_details_preserve_requirements_and_missing_evidence() {
        let mut run = CheckRun {
            id: 1, number: 1, started: 0, ended: 0, tree: String::new(),
            state: "unverified".into(), auto: false, accepted: false, checks: Vec::new(),
            note: "Restart persistence\nhas not been tested".into(),
            acceptance: Acceptance {
                summary: "Restart persistence\nhas not been tested".into(),
                requirements: vec![
                    Requirement { text: "Bookmark an item".into(), source: 1, verified: true, evidence: "bookmark_test: saved item observed".into(), ..Default::default() },
                    Requirement { text: "Keep bookmarks after restart".into(), source: 2, ..Default::default() },
                ],
                ..Default::default()
            },
        };
        assert_eq!(check_run_details(&run), "  Restart persistence has not been tested\n  ✓ Bookmark an item · instruction 1\n    bookmark_test: saved item observed\n  ! Keep bookmarks after restart · instruction 2\n    No execution evidence");
        run.note = format!("\x1b[31m{}\x1b[0m", "✓".repeat(300));
        run.acceptance = Acceptance::default();
        let details = check_run_details(&run);
        assert!(!details.contains('\x1b'));
        assert_eq!(details.chars().count(), 242); // indentation plus a 240-character excerpt
        assert!(details.ends_with('…'));
        assert_eq!(check_excerpt("  First paragraph.\n\nSecond\tparagraph.  ", 240), "First paragraph. Second paragraph.");
        assert_eq!(check_excerpt(&"✓".repeat(240), 240), "✓".repeat(240));
        assert_eq!(check_excerpt(&"✓".repeat(241), 240), format!("{}…", "✓".repeat(239)));
        assert_eq!(check_excerpt("one two three", 9), "one two…");
    }
}

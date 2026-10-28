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

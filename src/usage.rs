//! Read Claude's own /usage screen without sending a model prompt.
use crate::{proto::PlanLimit, pty, screen::Screen};
use alacritty_terminal::{
    grid::Dimensions,
    index::{Column, Line},
};
use std::{
    io,
    process::Command,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

pub fn watch() -> (mpsc::Receiver<Vec<PlanLimit>>, mpsc::SyncSender<()>) {
    let (tx, rx) = mpsc::channel();
    let (refresh, requests) = mpsc::sync_channel(1);
    thread::spawn(move || {
        loop {
            if tx.send(collect().unwrap_or_default()).is_err() {
                break;
            }
            match requests.recv_timeout(Duration::from_secs(300)) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });
    (rx, refresh)
}

fn collect() -> io::Result<Vec<PlanLimit>> {
    // Outside any repository, with project settings and MCP servers disabled.
    let dir = std::env::temp_dir().join(format!("arranger-usage-{}", std::process::id()));
    std::fs::create_dir(&dir)?;
    let result = (|| {
        let mut cmd = Command::new("claude");
        for key in crate::adapter::PARENT_SESSION_ENV {
            cmd.env_remove(key);
        }
        cmd.current_dir(&dir).env("TERM", "xterm-256color").args([
            "--setting-sources",
            "user",
            "--strict-mcp-config",
            "--mcp-config",
            "{\"mcpServers\":{}}",
            "--",
            "/usage",
        ]);
        let mut p = pty::spawn(cmd, 120, 80)?;
        let mut screen = Screen::new(120, 80);
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut last_change = Instant::now();
        let mut limits = Vec::new();
        let mut trusted = false;
        let mut trust_due = None;
        let mut confirm_due = None;
        while Instant::now() < deadline {
            let mut buf = [0; 32768];
            match rustix::io::read(&p.master, &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let replies = screen.feed(&buf[..n]);
                    if !replies.is_empty() {
                        let _ = rustix::io::write(&p.master, &replies);
                    }
                    let grid = screen.grid();
                    let text = (0..grid.screen_lines())
                        .map(|r| {
                            (0..grid.columns())
                                .map(|c| grid[Line(r as i32)][Column(c)].c)
                                .collect::<String>()
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    limits = parse(&text);
                    // This is the empty folder we just created, never a user's repository.
                    if !trusted
                        && text.contains("❯ No, exit")
                        && text.contains("Yes, I trust this folder")
                    {
                        trust_due = Some(Instant::now() + Duration::from_millis(750));
                        trusted = true;
                    }
                    last_change = Instant::now();
                }
                Err(rustix::io::Errno::AGAIN) => {}
                Err(_) => break,
            }
            if trust_due.is_some_and(|due| Instant::now() >= due) {
                let _ = rustix::io::write(&p.master, b"\x1b[B");
                trust_due = None;
                confirm_due = Some(Instant::now() + Duration::from_millis(250));
            }
            if confirm_due.is_some_and(|due| Instant::now() >= due) {
                let _ = rustix::io::write(&p.master, b"\r");
                confirm_due = None;
            }
            if !limits.is_empty() && last_change.elapsed() > Duration::from_secs(2) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        // The PTY child is a session leader; also stop any helpers it launched.
        if let Some(pid) = rustix::process::Pid::from_raw(p.child.id() as i32) {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
        let _ = p.child.wait();
        #[cfg(test)]
        if limits.is_empty() {
            let grid = screen.grid();
            for r in 0..grid.screen_lines() {
                let line: String = (0..grid.columns())
                    .map(|c| grid[Line(r as i32)][Column(c)].c)
                    .collect();
                if !line.trim().is_empty() {
                    eprintln!("{}", line.trim_end());
                }
            }
        }
        Ok(limits)
    })();
    let _ = std::fs::remove_dir(&dir);
    result
}

fn parse(text: &str) -> Vec<PlanLimit> {
    let mut limits = Vec::new();
    let mut window = None;
    for line in text.lines() {
        let line = line.trim();
        let lower = line.to_lowercase();
        if lower.starts_with("current session") {
            window = Some("5h".to_string());
        } else if lower.starts_with("current week") {
            window = Some(match line.find('(') {
                Some(i) if lower[i..].contains("all models") => "week".to_string(),
                Some(i) => format!("week {}", line[i..].to_lowercase().replace(" only", "")),
                None => "week".to_string(),
            });
        }
        if let Some(i) = lower.find("% used") {
            let number = lower[..i]
                .rsplit(|c: char| !c.is_ascii_digit() && c != '.')
                .next()
                .unwrap_or("");
            if let (Some(window), Ok(used)) = (window.take(), number.parse::<f64>())
                && (0.0..=100.0).contains(&used)
            {
                limits.push(PlanLimit {
                    agent: "claude".into(),
                    window,
                    used: Some(used),
                    ..Default::default()
                });
            }
        }
    }
    // In the API-billed CLI the Usage tab contains only session totals; its billing
    // banner can already have scrolled off the screen when the tab finishes drawing.
    let session_only = text.contains("Total cost:")
        && text.contains("Total duration (API):")
        && text.contains("Usage:")
        && !text.contains("Current session");
    if limits.is_empty() && (text.contains("API Usage Billing") || session_only) {
        limits.push(PlanLimit {
            agent: "claude".into(),
            window: "API billing".into(),
            status: "api_billing".into(),
            ..Default::default()
        });
    }
    limits
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "launches the installed Claude CLI"]
    fn installed_cli_usage() {
        let limits = collect().unwrap();
        assert!(
            !limits.is_empty(),
            "Claude /usage did not report usage or API billing within 20s"
        );
        eprintln!("{limits:?}");
    }
    #[test]
    fn reads_windows_without_confusing_session_tokens_with_plan_usage() {
        let limits = parse(
            "Current session\n████ 42% used\nResets 4pm\nCurrent week (all models)\n 81% used\nCurrent week (Sonnet only)\n 12% used",
        );
        assert_eq!(
            limits
                .iter()
                .map(|l| (l.window.as_str(), l.used))
                .collect::<Vec<_>>(),
            vec![
                ("5h", Some(42.0)),
                ("week", Some(81.0)),
                ("week (sonnet)", Some(12.0))
            ]
        );
        assert!(parse("Loading usage…\nTotal cost: $0.00\nUsage: 0 input, 0 output").is_empty());
        assert_eq!(
            parse("Opus · API Usage Billing\nSession\nUsage: 0 input")[0].used,
            None
        );
        assert!(parse("Current session\n150% used").is_empty());
    }
}

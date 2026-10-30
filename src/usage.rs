//! Read Claude's own /usage screen and pi's connected models without sending a model prompt.
use crate::{proto::PlanLimit, pty, screen::Screen};
use alacritty_terminal::{
    grid::Dimensions,
    index::{Column, Line},
};
use std::{
    io,
    process::Command,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

pub fn watch() -> (mpsc::Receiver<Vec<PlanLimit>>, mpsc::SyncSender<()>) {
    let (tx, rx) = mpsc::channel();
    let (refresh, requests) = mpsc::sync_channel(1);
    thread::spawn(move || {
        loop {
            let mut limits = collect().unwrap_or_default();
            limits.extend(collect_pi().unwrap_or_default());
            if tx.send(limits).is_err() {
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

/// A fresh directory for every probe, including concurrent probes and stale crash leftovers.
struct ProbeDir(std::path::PathBuf);

impl ProbeDir {
    fn new() -> io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let path = std::env::temp_dir().join(format!(
                "arranger-usage-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
    }
}

impl Drop for ProbeDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.0);
    }
}

fn screen_text(screen: &Screen) -> String {
    let grid = screen.grid();
    (0..grid.screen_lines())
        .map(|r| {
            (0..grid.columns())
                .map(|c| grid[Line(r as i32)][Column(c)].c)
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Only navigate a recognized trust dialog, and derive direction from its selection.
fn trust_keys(text: &str) -> Option<&'static [u8]> {
    let lines: Vec<_> = text.lines().collect();
    let yes = lines
        .iter()
        .position(|l| l.contains("Yes, I trust this folder"))?;
    let no = lines.iter().position(|l| l.contains("No, exit"))?;
    if lines[yes].contains('❯') {
        Some(b"\r")
    } else if lines[no].contains('❯') {
        Some(if yes < no { b"\x1b[A" } else { b"\x1b[B" })
    } else {
        None
    }
}

fn collect() -> io::Result<Vec<PlanLimit>> {
    // Outside any repository, with project settings and MCP servers disabled.
    let dir = ProbeDir::new()?;
    let result = (|| {
        let mut cmd = Command::new("claude");
        for key in crate::adapter::PARENT_SESSION_ENV {
            cmd.env_remove(key);
        }
        cmd.current_dir(&dir.0).env("TERM", "xterm-256color").args([
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
        let mut trust_action = None;
        let mut trust_pending = None;
        while Instant::now() < deadline {
            let mut buf = [0; 32768];
            match rustix::io::read(&p.master, &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let replies = screen.feed(&buf[..n]);
                    if !replies.is_empty() {
                        let _ = rustix::io::write(&p.master, &replies);
                    }
                    let text = screen_text(&screen);
                    let next = parse(&text);
                    // Cursor motion, spinners and terminal queries do not change usage.
                    if next
                        .iter()
                        .map(|l| (&l.window, l.used, &l.status))
                        .ne(limits
                            .iter()
                            .map(|l: &PlanLimit| (&l.window, l.used, &l.status)))
                    {
                        last_change = Instant::now();
                        limits = next;
                    }
                    // This is our fresh empty folder, never a user's repository.
                    let keys = trust_keys(&text);
                    if keys != trust_pending.map(|(keys, _)| keys) {
                        trust_pending = keys.map(|keys| (keys, Instant::now()));
                    }
                }
                Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => {}
                Err(_) => break,
            }
            // Let the dialog install its input handler before navigating. Confirm only
            // after a subsequent frame visibly selects the affirmative option.
            if let Some((keys, since)) = trust_pending
                && since.elapsed() >= Duration::from_millis(750)
                && trust_action != Some(keys)
            {
                rustix::io::write(&p.master, keys)?;
                trust_action = Some(keys);
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
    result
}

/// pi's `--list-models` table: one row per model whose provider has usable credentials.
fn collect_pi() -> io::Result<Vec<PlanLimit>> {
    let dir = ProbeDir::new()?;
    let result = (|| {
        let mut cmd = Command::new("pi");
        for key in crate::adapter::PARENT_SESSION_ENV {
            cmd.env_remove(key);
        }
        cmd.current_dir(&dir.0).arg("--list-models");
        let out = cmd.output()?;
        if !out.status.success() {
            return Ok(Vec::new());
        }
        Ok(parse_pi(&String::from_utf8_lossy(&out.stdout)))
    })();
    result
}

fn parse_pi(text: &str) -> Vec<PlanLimit> {
    text.lines()
        .skip(1) // provider  model  context  max-out  thinking  images
        .filter_map(|line| {
            let mut cols = line.split_whitespace();
            let provider = cols.next()?;
            let model = cols.next()?;
            Some(PlanLimit {
                agent: "pi".into(),
                window: format!("{provider}/{model}"),
                used: None,
                resets: 0,
                status: "connected".into(),
                seen: 0,
            })
        })
        .collect()
}

fn parse(text: &str) -> Vec<PlanLimit> {
    let mut limits = Vec::new();
    let mut window = None;
    for line in text.lines() {
        let line = line
            .trim()
            .trim_matches(|c: char| matches!(c, '│' | '┃' | '║'))
            .trim();
        let lower = line.to_lowercase();
        if lower == "current session" || lower.starts_with("current session ") {
            window = Some("5h".to_string());
        } else if lower == "current week" || lower.starts_with("current week ") {
            window = Some(match line.find('(') {
                Some(i) if lower[i..].contains("all models") => "week".to_string(),
                Some(i) => format!("week {}", line[i..].to_lowercase().replace(" only", "")),
                None => "week".to_string(),
            });
        } else if lower.starts_with("resets") || lower.starts_with("extra usage") {
            window = None;
        }
        if let Some(i) = lower
            .find('%')
            .filter(|&i| lower[i + 1..].trim_start().starts_with("used"))
        {
            let number = lower[..i]
                .rsplit(|c: char| !c.is_ascii_digit() && c != '.' && c != '-' && c != '+')
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
    // A newly drawn heading without its percentage is an incomplete frame. Do not
    // let a previously drawn window make the collector finish during loading.
    if window.is_some() {
        return Vec::new();
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
    fn reads_terminal_redraws_and_decorated_sections() {
        let mut screen = Screen::new(120, 80);
        // An incomplete draw must not leave the old percentage on screen.
        screen.feed(b"Current session\r\n42% used");
        assert_eq!(parse(&screen_text(&screen))[0].used, Some(42.0));
        let redraw = "\x1b[2J\x1b[H│ Current session │\r\n│ \x1b[32m12.5%   used\x1b[0m │\r\n│ Resets 4pm │\r\n│ Current week (all models) │\r\n│ 81% used │";
        // Exercise escape sequences and UTF-8 split across arbitrary read boundaries.
        for byte in redraw.as_bytes() {
            screen.feed(&[*byte]);
        }
        let limits = parse(&screen_text(&screen));
        assert_eq!(
            limits
                .iter()
                .map(|l| (l.window.as_str(), l.used))
                .collect::<Vec<_>>(),
            vec![("5h", Some(12.5)), ("week", Some(81.0))]
        );
        screen.feed(b"\x1b[2J\x1b[HLoading usage...");
        assert!(parse(&screen_text(&screen)).is_empty());
    }

    #[test]
    fn unrelated_sections_cannot_supply_a_missing_percentage() {
        assert!(parse("Current session\nResets 4pm\nExtra usage\n42% used").is_empty());
        assert!(parse("Current session\n-5% used").is_empty());
        assert!(parse("Current session\nNaN% used").is_empty());
        assert!(
            parse("Current session\n42% used\nCurrent week (all models)\nLoading...").is_empty()
        );
        assert_eq!(parse("Current session\n100% used")[0].used, Some(100.0));
    }

    #[test]
    fn trust_navigation_follows_the_selected_option() {
        assert_eq!(
            trust_keys("Yes, I trust this folder\n❯ No, exit"),
            Some(&b"\x1b[A"[..])
        );
        assert_eq!(
            trust_keys("❯ No, exit\nYes, I trust this folder"),
            Some(&b"\x1b[B"[..])
        );
        assert_eq!(
            trust_keys("❯ Yes, I trust this folder\nNo, exit"),
            Some(&b"\r"[..])
        );
        assert_eq!(trust_keys("Yes, I trust this folder\nNo, exit"), None);
        assert_eq!(trust_keys("❯ Yes, proceed"), None);
    }

    #[test]
    fn probe_directories_are_unique_and_removed_on_drop() {
        let a = ProbeDir::new().unwrap();
        let b = ProbeDir::new().unwrap();
        assert_ne!(a.0, b.0);
        let path = a.0.clone();
        drop(a);
        assert!(!path.exists());
        assert!(b.0.is_dir());
    }

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
    fn reads_pi_connected_models() {
        let limits = parse_pi(
            "provider  model            context  max-out  thinking  images\n\
             deepseek  deepseek-flash   1M       384K     yes       yes   \n\
             deepseek  deepseek-v4-pro  1M       384K     yes       no    \n",
        );
        assert_eq!(
            limits
                .iter()
                .map(|l| (l.agent.as_str(), l.window.as_str(), l.status.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("pi", "deepseek/deepseek-flash", "connected"),
                ("pi", "deepseek/deepseek-v4-pro", "connected")
            ]
        );
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

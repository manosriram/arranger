//! A task whose worktree is being set up: what the plan does, its output so far, and the buttons
//! to approve, retry or skip it. The agent starts once setup ends.
use super::*;

/// How often the output is fetched again while setup runs.
const LOG_EVERY: Duration = Duration::from_millis(800);

impl App {
    pub(super) fn draw_setup(&mut self, r: &mut Renderer, t: &Theme, sec: Rect, task: &TaskInfo) {
        let key = (task.machine, task.id);
        self.fetch_setup_log(key, &task.setup);
        let x = sec.x + 28.0;
        let w = (sec.w - 56.0).min(900.0);
        let mut y = sec.y + 24.0;

        let (heading, note) = match task.setup.as_str() {
            "planning" => ("Planning the setup", "An agent is reading the repository to work out what a fresh worktree needs before the agent can build and test. It plans once per repository, until its dependencies change."),
            "approval" => ("Approve the setup", "These commands aren't plain dependency installs, so they run once you approve. Approval is remembered for this repository until its dependencies change."),
            "failed" => ("Setup failed", "Retry it, have the agent plan it again, or skip setup and start the agent anyway."),
            _ => ("Setting up the worktree", "The agent starts as soon as this finishes."),
        };
        r.text(x, y, heading, UI, 15.0, 600, if task.setup == "failed" { t.del } else { t.text }, w);
        y += 26.0;
        y += self.wrapped(r, x, y, w, note, UI, 12.5, t.text3, 19.0) + 14.0;

        // The plan (or, failed, why), one line each: `copy …`, or `$ command  · why`.
        if !task.setup_detail.is_empty() {
            for line in task.setup_detail.lines().take(14) {
                let (main, why) = line.split_once("  · ").unwrap_or((line, ""));
                let color = if task.setup == "failed" { t.del } else if main.starts_with("$ ") { t.text } else { t.text2 };
                let mw = r.text(x, y, main, MONO, 12.0, 400, color, w);
                if !why.is_empty() && mw + 24.0 < w {
                    r.text(x + mw + 16.0, y, why, UI, 12.0, 400, t.text3, w - mw - 16.0);
                }
                y += 20.0;
            }
            y += 10.0;
        }

        let buttons: &[(&str, SetupAction, bool)] = match task.setup.as_str() {
            "approval" => &[("Approve and run", SetupAction::Approve, true), ("Plan again", SetupAction::Replan, false), ("Skip setup", SetupAction::Skip, false)],
            "failed" => &[("Retry", SetupAction::Retry, true), ("Plan again", SetupAction::Replan, false), ("Skip setup", SetupAction::Skip, false)],
            _ => &[("Skip setup", SetupAction::Skip, false)],
        };
        let mut bx = x;
        for &(label, action, primary) in buttons {
            let bw = r.measure(UI, 12.5, 500, label) + 28.0;
            let b = Rect::new(bx, y, bw, 32.0);
            if primary {
                r.rect(b, 0.0, t.primary_bg);
            } else {
                r.frame(b, 0.0, t.bg, t.border2);
            }
            r.text(b.x + 14.0, r.center(b, UI, 12.5), label, UI, 12.5, 500, if primary { t.primary_fg } else { t.text }, bw);
            if self.hit(r, b) {
                self.actions.push(Action::Setup(key, action));
            }
            bx += bw + 8.0;
        }
        y += 32.0 + 22.0;

        // Its output, newest at the bottom.
        let log = self.setup_log.as_ref().filter(|l| l.0 == key).map_or("", |l| l.2.as_str());
        if log.is_empty() {
            return;
        }
        r.text(x, y, "Output", UI, 11.0, 500, t.text3, w);
        y += 18.0;
        let bottom = sec.y + sec.h - 20.0;
        if bottom - y < 60.0 {
            return;
        }
        let pane = Rect::new(x, y, w, bottom - y);
        r.frame(pane, 0.0, t.bg, t.border);
        let rows = ((pane.h - 20.0) / LINE_H).floor().max(1.0) as usize;
        let lines: Vec<&str> = log.lines().collect();
        r.set_clip(Some(pane));
        let mut ly = pane.y + 10.0;
        for line in &lines[lines.len().saturating_sub(rows)..] {
            let color = if line.starts_with("$ ") { t.text } else if line.starts_with("exited with") || line.starts_with("stopped after") { t.del } else { t.text2 };
            r.text(pane.x + 12.0, ly, line, MONO, 11.5, if line.starts_with("$ ") { 600 } else { 400 }, color, pane.w - 24.0);
            ly += LINE_H;
        }
        r.set_clip(None);
    }

    /// Text wrapped at word boundaries to `w`; returns the height it took.
    pub(super) fn wrapped(&self, r: &mut Renderer, x: f32, y: f32, w: f32, text: &str, font: usize, size: f32, color: Rgba, line_h: f32) -> f32 {
        let mut line = String::new();
        let mut ly = y;
        for word in text.split_whitespace() {
            let next = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
            if !line.is_empty() && r.measure(font, size, 400, &next) > w {
                r.text(x, ly, &line, font, size, 400, color, w);
                ly += line_h;
                line = word.to_string();
            } else {
                line = next;
            }
        }
        if !line.is_empty() {
            r.text(x, ly, &line, font, size, 400, color, w);
            ly += line_h;
        }
        ly - y
    }

    /// Fetches the task's setup output: for another task, once its stage moves on, and again every
    /// `LOG_EVERY` while it runs (waking the window for it, since nothing else may redraw).
    fn fetch_setup_log(&mut self, key: TaskKey, stage: &str) {
        let fresh = self.setup_log.as_ref().is_some_and(|l| l.0 == key && l.1 == stage && (stage != "running" || l.3.elapsed() < LOG_EVERY));
        if fresh || self.setup_log_pending {
            return;
        }
        self.setup_log_pending = true;
        let (sock, p, stage) = (self.sock(key.0), self.proxy.clone(), stage.to_string());
        thread::spawn(move || {
            let text = match request_at(sock.as_deref(), &Request::SetupLog { id: key.1 }) {
                Ok(Reply::Log(bytes)) => arranger::setup::tail(&bytes, 400),
                _ => String::new(),
            };
            let running = stage == "running";
            let _ = p.send_event(Msg::SetupLog(key, stage, text));
            if running {
                thread::sleep(LOG_EVERY);
                let _ = p.send_event(Msg::Redraw);
            }
        });
    }
}

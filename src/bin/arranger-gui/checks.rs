//! The Checks tab: a task's check runs on the left (and the commands to run next), the selected
//! check's output on the right as a clean console: colours kept, progress redraws collapsed,
//! error lines marked, following the end while it runs.
use super::*;
use arranger::proto::{CheckRun, ConsoleLine, Style};

const RAIL_W: f32 = 360.0;
const CON_SIZE: f32 = 12.0;
const CON_LINE: f32 = 18.0;
/// Lines fetched at a time.
const PAGE: usize = 5000;
const RUNS_EVERY: Duration = Duration::from_millis(1000);
const CONSOLE_EVERY: Duration = Duration::from_millis(400);

pub struct ChecksView {
    pub task: u64,
    pub host: usize,
    runs: Vec<CheckRun>,
    expanded_runs: std::collections::HashSet<u64>,
    loaded: bool,
    runs_pending: bool,
    runs_at: Instant,
    /// The task's checks summary when the runs were fetched: a change means fetch again.
    seen: proto::ChecksSummary,
    /// The check whose output shows.
    selected: Option<u64>,
    console: Option<ConsoleView>,
    /// Commands for the next run, and the one being typed.
    pub draft: Vec<String>,
    pub input: String,
    error: String,
    rail_scroll: f32,
    auto_fix: u32,
}

struct ConsoleView {
    check: u64,
    lines: Vec<ConsoleLine>,
    total: usize,
    errors: usize,
    done: bool,
    pending: bool,
    fetched: Instant,
    /// The first visual row on screen, and whether it follows the end.
    top: f32,
    follow: bool,
    /// Cumulative wrapped rows per line, for this many columns.
    rows: (usize, Vec<usize>),
    /// Where the console was drawn, for the wheel.
    area: Rect,
}

#[derive(Clone, Debug)]
pub enum ChecksAction {
    Select(u64),
    ToggleRun(u64),
    SendFailures,
    Accept,
    AutoFix,
    RetryLatest,
    /// Add the typed command to the next run (or, with nothing typed, run).
    Add,
    Remove(usize),
    Run,
    Stop,
    /// Run one check again, on its own.
    Rerun(String, String),
    /// Scroll the console to the next error line after what's on screen.
    NextError,
    Copy,
    /// j/k in vim's normal mode: the next or previous check.
    Step(i32),
    /// Run the planned checks waiting on you; `true`: and don't ask about them again here.
    Approve(bool),
    /// Stop requiring a check that failed before.
    Drop(String),
}

impl App {
    pub(super) fn open_checks(&mut self) {
        let Some(task) = self.current() else { return };
        let (task, host) = (task.id, task.machine);
        if self.checks.as_ref().is_some_and(|c| (c.host, c.task) == (host, task)) {
            return;
        }
        (self.review, self.machine_tasks, self.shell) = (None, None, false);
        let auto_fix = match request_at(self.sock(host).as_deref(), &Request::Setting { key: "auto_fix".into(), value: None }) {
            Ok(Reply::Text(value)) => value.parse().unwrap_or(0),
            _ => 0,
        };
        self.checks = Some(ChecksView {
            task,
            host,
            runs: Vec::new(),
            expanded_runs: Default::default(),
            loaded: false,
            runs_pending: false,
            runs_at: Instant::now(),
            seen: Default::default(),
            selected: None,
            console: None,
            draft: Vec::new(),
            input: String::new(),
            error: String::new(),
            rail_scroll: 0.0,
            auto_fix,
        });
        self.focus = self.rest();
    }

    /// The Checks tab can open: the selected task has a worktree and is past setup.
    pub(super) fn checkable(&self) -> bool {
        self.current().is_some_and(|t| t.state != "queued" && !t.worktree.is_empty() && !is_setup(&t.state))
    }

    pub(super) fn checks_action(&mut self, action: ChecksAction) {
        let Some(cv) = &mut self.checks else { return };
        let (host, id) = (cv.host, cv.task);
        match action {
            ChecksAction::ToggleRun(run) => {
                if !cv.expanded_runs.remove(&run) { cv.expanded_runs.insert(run); }
            }
            ChecksAction::RetryLatest => {
                let Some(run) = cv.runs.first() else { return };
                let checks = run.checks.iter().map(|c| proto::CheckSpec { name: c.name.clone(), command: c.command.clone(), why: c.why.clone() }).collect();
                self.run_checks_now(host, id, checks);
            }
            ChecksAction::AutoFix => {
                let rounds = (cv.auto_fix + 1) % 11;
                match request_at(self.sock(host).as_deref(), &Request::Setting { key: "auto_fix".into(), value: Some(rounds.to_string()) }) {
                    Ok(Reply::Text(value)) => self.checks.as_mut().unwrap().auto_fix = value.parse().unwrap_or(0),
                    Ok(Reply::Error(e)) => self.checks.as_mut().unwrap().error = e,
                    Err(e) => self.checks.as_mut().unwrap().error = e.to_string(),
                    _ => {}
                }
            }
            ChecksAction::SendFailures => {
                match request_at(self.sock(host).as_deref(), &Request::CheckReport { id }) {
                    Ok(Reply::Text(text)) => {
                        let draft = self.drafts.entry((host, id)).or_default();
                        if !draft.is_empty() { draft.push_str("\n\n"); }
                        draft.push_str(&text);
                        self.caret = Some((Focus::Message, draft.len()));
                        self.anchor = None;
                        self.checks = None;
                        self.focus = Focus::Message;
                    }
                    Ok(Reply::Error(e)) => self.checks.as_mut().unwrap().error = e,
                    Err(e) => self.checks.as_mut().unwrap().error = e.to_string(),
                    _ => {}
                }
            }
            ChecksAction::Accept => {
                match request_at(self.sock(host).as_deref(), &Request::AcceptChecks { id }) {
                    Ok(Reply::Ok) => self.checks.as_mut().unwrap().error.clear(),
                    Ok(Reply::Error(e)) => self.checks.as_mut().unwrap().error = e,
                    Err(e) => self.checks.as_mut().unwrap().error = e.to_string(),
                    _ => {}
                }
                self.refresh_runs();
            }
            ChecksAction::Select(check) => {
                cv.selected = Some(check);
                cv.console = None;
            }
            ChecksAction::Add => {
                let cmd = cv.input.trim().to_string();
                if cmd.is_empty() {
                    return self.checks_action(ChecksAction::Run);
                }
                cv.draft.push(cmd);
                cv.input.clear();
                self.anchor = None;
            }
            ChecksAction::Remove(i) => {
                if i < cv.draft.len() {
                    cv.draft.remove(i);
                }
            }
            ChecksAction::Run => {
                let checks: Vec<proto::CheckSpec> = cv.draft.iter().map(|c| proto::CheckSpec { name: String::new(), command: c.clone(), why: String::new() }).collect();
                self.run_checks_now(host, id, checks);
            }
            ChecksAction::Rerun(name, command) => self.run_checks_now(host, id, vec![proto::CheckSpec { name, command, why: String::new() }]),
            ChecksAction::Stop => {
                let reply = request_at(self.sock(host).as_deref(), &Request::StopChecks { id });
                if let (Ok(Reply::Error(e)), Some(cv)) = (reply, &mut self.checks) {
                    cv.error = e;
                }
                self.refresh_runs();
            }
            ChecksAction::NextError => {
                let Some(con) = cv.console.as_mut() else { return };
                let first = con.rows.1.partition_point(|&end| end as f32 <= con.top + 1.0);
                let next = con.lines.iter().enumerate().skip(first + 1).find(|(_, l)| l.error).or_else(|| con.lines.iter().enumerate().find(|(_, l)| l.error));
                if let Some((i, _)) = next {
                    con.top = if i == 0 { 0.0 } else { con.rows.1[i - 1] as f32 };
                    con.follow = false;
                }
            }
            ChecksAction::Copy => {
                if let Some(con) = &cv.console {
                    let text: Vec<String> = con.lines.iter().map(|l| l.spans.iter().map(|s| s.0.as_str()).collect()).collect();
                    set_clipboard(&text.join("\n"));
                }
            }
            ChecksAction::Approve(always) => {
                let reply = request_at(self.sock(host).as_deref(), &Request::ApproveChecks { id, always });
                if let (Ok(Reply::Error(e)), Some(cv)) = (reply, &mut self.checks) {
                    cv.error = e;
                }
                self.refresh_runs();
            }
            ChecksAction::Drop(command) => {
                let reply = request_at(self.sock(host).as_deref(), &Request::DropCheck { id, command });
                if let (Ok(Reply::Error(e)), Some(cv)) = (reply, &mut self.checks) {
                    cv.error = e;
                }
                self.refresh_runs();
            }
            ChecksAction::Step(d) => {
                let ids: Vec<u64> = cv.runs.iter().filter(|r| cv.expanded_runs.contains(&r.id)).flat_map(|r| r.checks.iter().map(|c| c.id)).collect();
                let at = cv.selected.and_then(|s| ids.iter().position(|&i| i == s)).unwrap_or(0) as i32;
                if let Some(&next) = ids.get((at + d).clamp(0, ids.len() as i32 - 1).max(0) as usize) {
                    cv.selected = Some(next);
                    cv.console = None;
                }
            }
        }
    }

    fn run_checks_now(&mut self, host: usize, id: u64, checks: Vec<proto::CheckSpec>) {
        let reply = request_at(self.sock(host).as_deref(), &Request::RunChecks { id, checks }).map_err(|e| e.to_string());
        let Some(cv) = &mut self.checks else { return };
        cv.error = match reply {
            Ok(Reply::Ok) => String::new(),
            Ok(Reply::Error(e)) | Err(e) => e,
            Ok(r) => format!("unexpected reply: {r:?}"),
        };
        // The new run's first check, once it's in.
        cv.selected = None;
        cv.console = None;
        self.refresh_runs();
    }

    fn refresh_runs(&mut self) {
        let Some(cv) = &mut self.checks else { return };
        if cv.runs_pending {
            return;
        }
        cv.runs_pending = true;
        cv.runs_at = Instant::now();
        let (host, id) = (cv.host, cv.task);
        self.spawn_request(host, Tag::CheckRuns((host, id)), Request::Checks { id });
    }

    pub(super) fn on_checks_reply(&mut self, tag: Tag, reply: Result<Reply, String>) {
        let Some(cv) = &mut self.checks else { return };
        match (tag, reply) {
            (Tag::CheckRuns(key), reply) if key == (cv.host, cv.task) => {
                cv.runs_pending = false;
                let first = !cv.loaded;
                cv.loaded = true;
                match reply {
                    Ok(Reply::Checks(runs)) => {
                        // The next run starts as the last one's commands.
                        if first && cv.draft.is_empty() {
                            let ran = runs.iter().find(|r| !matches!(r.state.as_str(), "planning" | "approval") && !r.checks.is_empty());
                            cv.draft = ran.map(|r| r.checks.iter().map(|c| c.command.clone()).collect()).unwrap_or_default();
                        }
                        if runs.first().map(|r| r.id) != cv.runs.first().map(|r| r.id) {
                            cv.expanded_runs.clear();
                            if let Some(run) = runs.first() { cv.expanded_runs.insert(run.id); }
                            cv.selected = None;
                            cv.rail_scroll = 0.0;
                        }
                        cv.runs = runs;
                        // Show something: the latest run's first failure, else what runs, else its first.
                        let known = cv.selected.is_some_and(|s| cv.runs.iter().any(|r| r.checks.iter().any(|c| c.id == s)));
                        if !known {
                            cv.selected = cv.runs.first().and_then(|r| {
                                r.checks.iter().find(|c| c.state == "failed").or_else(|| r.checks.iter().find(|c| c.state == "running")).or(r.checks.first()).map(|c| c.id)
                            });
                            cv.console = None;
                        }
                        if cv.runs.first().is_some_and(|r| r.state == "running") {
                            let p = self.proxy.clone();
                            thread::spawn(move || {
                                thread::sleep(RUNS_EVERY);
                                let _ = p.send_event(Msg::Redraw);
                            });
                        }
                    }
                    Ok(Reply::Error(e)) | Err(e) => cv.error = e,
                    _ => {}
                }
            }
            (Tag::Console(key, check), reply) if key == (cv.host, cv.task) => {
                let Some(con) = cv.console.as_mut().filter(|c| c.check == check) else { return };
                con.pending = false;
                con.fetched = Instant::now();
                if let Ok(Reply::Console { start, total, done, lines, .. }) = reply {
                    // The last line may have grown since: lines from `start` on are replaced.
                    con.errors -= con.lines[start.min(con.lines.len())..].iter().filter(|l| l.error).count();
                    con.errors += lines.iter().filter(|l| l.error).count();
                    con.lines.truncate(start);
                    con.lines.extend(lines);
                    con.total = total;
                    con.done = done && con.lines.len() >= total;
                    con.rows.1.truncate(start);
                    let more = con.lines.len() < total;
                    if more || !con.done {
                        let p = self.proxy.clone();
                        thread::spawn(move || {
                            if !more {
                                thread::sleep(CONSOLE_EVERY);
                            }
                            let _ = p.send_event(Msg::Redraw);
                        });
                    }
                }
            }
            _ => {}
        }
    }

    /// Scrolls the console when the mouse is over it; true if it took the wheel.
    pub(super) fn scroll_checks(&mut self, dy: f32) -> bool {
        let mouse = self.mouse;
        let Some(cv) = &mut self.checks else { return false };
        if mouse.0 < RAIL_W {
            cv.rail_scroll = (cv.rail_scroll - dy).max(0.0);
            return true;
        }
        let Some(con) = cv.console.as_mut().filter(|c| c.area.contains(mouse)) else { return true };
        con.top = (con.top - dy / CON_LINE).max(0.0);
        con.follow = false;
        true
    }

    pub(super) fn draw_checks(&mut self, r: &mut Renderer, t: &Theme, area: Rect) {
        let Some(task) = self.hosts.get(self.checks.as_ref().unwrap().host).and_then(|h| h.tasks.iter().find(|x| x.id == self.checks.as_ref().unwrap().task)).cloned() else {
            self.checks = None;
            return;
        };
        // Fetch the runs when the task's checks moved on, and while one runs.
        let cv = self.checks.as_mut().unwrap();
        let stale = !cv.loaded || cv.seen != task.checks || (cv.runs.first().is_some_and(|r| r.state == "running") && cv.runs_at.elapsed() >= RUNS_EVERY);
        if stale && !cv.runs_pending {
            cv.seen = task.checks.clone();
            self.refresh_runs();
        }
        let rail = Rect::new(area.x, area.y, RAIL_W, area.h);
        r.rect(rail, 0.0, t.panel);
        r.rect(Rect::new(rail.x + rail.w - 1.0, rail.y, 1.0, rail.h), 0.0, t.border);
        self.draw_checks_rail(r, t, rail, &task);
        let main = Rect::new(area.x + RAIL_W, area.y, area.w - RAIL_W, area.h);
        r.rect(main, 0.0, t.term);
        self.draw_console(r, t, main);
    }

    fn draw_checks_rail(&mut self, r: &mut Renderer, t: &Theme, rail: Rect, task: &TaskInfo) {
        let x = rail.x + 16.0;
        let w = rail.w - 32.0;
        let mut y = rail.y + 16.0;
        r.text(x, y, &title(task), UI, 13.5, 600, t.text, w);
        y += 22.0;
        r.text(x, y, &format!("#{} · {}", task.id, task.branch), MONO, 11.0, 400, t.text3, w);
        y += 28.0;

        // The next run: its commands, a field to add one, Run (or Stop while one runs).
        let running = self.checks.as_ref().unwrap().runs.first().is_some_and(|r| matches!(r.state.as_str(), "running" | "planning"));
        r.text(x, y, "RUN CHECKS", UI, 11.0, 600, t.text3, w);
        y += 20.0;
        let draft = self.checks.as_ref().unwrap().draft.clone();
        for (i, cmd) in draft.iter().enumerate() {
            let row = Rect::new(x, y, w, 24.0);
            r.text(x, r.center(row, MONO, 11.5), &format!("$ {cmd}"), MONO, 11.5, 400, t.text, w - 24.0);
            let rm = Rect::new(x + w - 18.0, row.y + 4.0, 16.0, 16.0);
            r.text(rm.x + 3.0, r.center(rm, UI, 12.0), "✕", UI, 12.0, 400, t.text3, 16.0);
            if self.hit(r, rm) {
                self.actions.push(Action::Checks(ChecksAction::Remove(i)));
            }
            y += 24.0;
        }
        let field = Rect::new(x, y + 2.0, w, 28.0);
        let focused = self.focus == Focus::CheckCmd;
        r.frame(field, 0.0, t.bg, if focused { t.work } else { t.border });
        let input = self.checks.as_ref().unwrap().input.clone();
        if input.is_empty() && !focused {
            r.text(field.x + 10.0, r.center(field, MONO, 11.5), "Add a command, e.g. cargo test", MONO, 11.5, 400, t.text3, field.w - 20.0);
        } else {
            self.field_text(r, t, field.x + 10.0, r.center(field, MONO, 11.5), &input, MONO, 11.5, field.w - 20.0, (Focus::CheckCmd, 0), focused, self.caret_for(Focus::CheckCmd, &input), t.work);
        }
        if self.hit_as(r, field, Cursor::Text) {
            self.actions.push(Action::Focus(Focus::CheckCmd));
        }
        y += 40.0;
        let (label, action, enabled) = if running { ("Stop", ChecksAction::Stop, true) } else { ("Run checks", ChecksAction::Run, !draft.is_empty()) };
        let bw = r.measure(UI, 12.5, 500, label) + 28.0;
        let b = Rect::new(x, y, bw, 30.0);
        if running {
            r.frame(b, 0.0, t.bg, t.del);
        } else {
            r.rect(b, 0.0, if enabled { t.primary_bg } else { t.raised });
        }
        let fg = if running { t.del } else if enabled { t.primary_fg } else { alpha(t.text3, 120) };
        r.text(b.x + 14.0, r.center(b, UI, 12.5), label, UI, 12.5, 500, fg, bw);
        if enabled && self.hit(r, b) {
            self.actions.push(Action::Checks(action));
        }
        y += 38.0;
        let rounds = self.checks.as_ref().unwrap().auto_fix;
        let label = format!("Auto-fix rounds: {rounds}");
        let b = Rect::new(x, y, w, 26.0);
        r.text(b.x, r.center(b, UI, 12.0), &label, UI, 12.0, 500, t.text2, w);
        if self.hit(r, b) { self.actions.push(Action::Checks(ChecksAction::AutoFix)); }
        y += 30.0;
        let error = self.checks.as_ref().unwrap().error.clone();
        if !error.is_empty() {
            r.text(x, y, &error, UI, 12.0, 400, t.del, w);
            y += 22.0;
        }
        y += 10.0;
        r.rect(Rect::new(rail.x, y, rail.w - 1.0, 1.0), 0.0, t.border);
        y += 14.0;

        // Runs, newest first; only the latest starts expanded.
        let list = Rect::new(rail.x, y, rail.w - 1.0, rail.y + rail.h - y);
        r.set_clip(Some(list));
        let cv = self.checks.as_ref().unwrap();
        let (runs, selected, loaded) = (cv.runs.clone(), cv.selected, cv.loaded);
        let expanded_runs = cv.expanded_runs.clone();
        let mut y = y - cv.rail_scroll;
        if runs.is_empty() {
            let note = if loaded { "No checks have run yet. Add the commands that prove the work holds up (tests, build, lint) and run them." } else { "Loading…" };
            self.wrapped(r, x, y, w, note, UI, 12.0, t.text3, 18.0);
        }
        for run in &runs {
            let passed = run.checks.iter().filter(|c| c.state == "passed").count();
            let (mark, color) = state_mark(t, &run.state);
            r.text(x, y, mark, UI, 12.0, 600, color, 14.0);
            let state = match run.state.as_str() {
                "failed" if run.accepted => "failed · accepted",
                "planning" => "planning checks…",
                "approval" => "waits for approval",
                "skipped" => "no checks needed",
                "stale" => "stale: the agent went on",
                s => s,
            };
            let counts = if run.checks.is_empty() { String::new() } else { format!(" · {passed}/{}", run.checks.len()) };
            let expanded = expanded_runs.contains(&run.id);
            let arrow = if expanded { "▾" } else { "▸" };
            let head = format!("{arrow} Run {} · {state}{counts}", run.id);
            let header = Rect::new(rail.x + 6.0, y - 4.0, rail.w - 13.0, 24.0);
            if header.y + header.h >= list.y && header.y <= list.y + list.h && self.hit(r, header) {
                self.actions.push(Action::Checks(ChecksAction::ToggleRun(run.id)));
            }
            r.text(x + 18.0, y, &head, UI, 12.0, 600, t.text, w - 80.0);
            let when = if run.started == 0 { String::new() } else { ago(run.started) };
            let ww = r.measure(MONO, 11.0, 400, &when);
            r.text(x + w - ww, y, &when, MONO, 11.0, 400, t.text3, ww + 2.0);
            y += 20.0;
            if !expanded {
                y += 12.0;
                continue;
            }
            let by = if run.auto { "after the agent's turn" } else { "run by you" };
            r.text(x + 18.0, y, by, UI, 11.0, 400, t.text3, w - 18.0);
            y += 18.0;
            if !run.note.is_empty() {
                let color = if matches!(run.state.as_str(), "unverified" | "error") { color } else { t.text2 };
                y += self.wrapped(r, x + 18.0, y, w - 18.0, &run.note, UI, 11.5, color, 17.0) + 2.0;
            }
            if run.state == "failed" && !run.accepted && runs.first().is_some_and(|r| r.id == run.id) {
                let mut bx = x + 18.0;
                for (label, action) in [("Send to agent", ChecksAction::SendFailures), ("Accept anyway", ChecksAction::Accept)] {
                    let bw = r.measure(UI, 12.0, 500, label) + 18.0;
                    let b = Rect::new(bx, y + 4.0, bw, 28.0);
                    r.frame(b, 0.0, t.bg, t.border2);
                    r.text(b.x + 9.0, r.center(b, UI, 12.0), label, UI, 12.0, 500, t.text, bw);
                    if self.hit(r, b) { self.actions.push(Action::Checks(action)); }
                    bx += bw + 6.0;
                }
                y += 36.0;
                let b = Rect::new(x + 18.0, y, 64.0, 26.0);
                r.frame(b, 0.0, t.bg, t.border2);
                r.text(b.x + 9.0, r.center(b, UI, 12.0), "Rerun", UI, 12.0, 500, t.text, b.w);
                if self.hit(r, b) { self.actions.push(Action::Checks(ChecksAction::RetryLatest)); }
                y += 30.0;
            }
            // Planned checks that aren't plainly check-only wait for you.
            if run.state == "approval" {
                y += 4.0;
                let mut bx = x + 18.0;
                for (label, action, primary) in [("Approve", ChecksAction::Approve(false), true), ("Always allow", ChecksAction::Approve(true), false), ("Skip", ChecksAction::Stop, false)] {
                    let bw = r.measure(UI, 12.0, 500, label) + 22.0;
                    let b = Rect::new(bx, y, bw, 28.0);
                    if primary {
                        r.rect(b, 0.0, t.needs);
                    } else {
                        r.frame(b, 0.0, t.bg, t.border2);
                    }
                    r.text(b.x + 11.0, r.center(b, UI, 12.0), label, UI, 12.0, 500, if primary { t.on_accent } else { t.text }, bw);
                    if self.hit(r, b) {
                        self.actions.push(Action::Checks(action));
                    }
                    bx += bw + 8.0;
                }
                y += 36.0;
            }
            y += 4.0;
            for c in &run.checks {
                let row = Rect::new(rail.x + 6.0, y - 4.0, rail.w - 13.0, 26.0);
                if row.y + row.h < list.y || row.y > list.y + list.h {
                    y += 26.0;
                    continue;
                }
                let on = selected == Some(c.id);
                if on {
                    r.rect(row, 0.0, t.sel);
                }
                let (mark, color) = state_mark(t, &c.state);
                r.text(x + 6.0, r.center(row, UI, 12.0), mark, UI, 12.0, 600, color, 14.0);
                let took = match (c.started, c.ended) {
                    (s, e) if s > 0 && e >= s => format!("{}s", e - s),
                    (s, 0) if s > 0 && c.state == "running" => ago(s),
                    _ => String::new(),
                };
                let took = if c.origin == "sticky" && took.is_empty() { "failed before".to_string() } else { took };
                let tw = r.measure(MONO, 11.0, 400, &took);
                r.text(x + 24.0, r.center(row, MONO, 11.5), &c.name, MONO, 11.5, if on { 600 } else { 400 }, if c.state == "pending" { t.text3 } else { t.text }, w - 34.0 - tw);
                r.text(x + w - tw, r.center(row, MONO, 11.0), &took, MONO, 11.0, 400, t.text3, tw + 2.0);
                if self.hit(r, row) {
                    self.actions.push(Action::Checks(ChecksAction::Select(c.id)));
                }
                y += 26.0;
            }
            y += 12.0;
        }
        r.set_clip(None);
    }

    fn draw_console(&mut self, r: &mut Renderer, t: &Theme, area: Rect) {
        let cv = self.checks.as_mut().unwrap();
        let Some(check) = cv.selected.and_then(|s| cv.runs.iter().flat_map(|r| r.checks.iter()).find(|c| c.id == s)).cloned() else {
            return;
        };
        let (host, task) = (cv.host, cv.task);
        let con = cv.console.get_or_insert_with(|| ConsoleView {
            check: check.id,
            lines: Vec::new(),
            total: 0,
            errors: 0,
            done: false,
            pending: false,
            fetched: Instant::now() - CONSOLE_EVERY,
            top: 0.0,
            follow: true,
            rows: (0, Vec::new()),
            area,
        });
        // More to fetch: pages not yet in, or it's still going.
        let behind = con.lines.len() < con.total || (!con.done && con.fetched.elapsed() >= CONSOLE_EVERY);
        if !con.pending && (behind || (con.total == 0 && !con.done)) {
            con.pending = true;
            let start = if con.lines.len() < con.total { con.lines.len() } else { con.lines.len().saturating_sub(1) };
            self.spawn_request(host, Tag::Console((host, task), check.id), Request::Console { check: check.id, start, limit: PAGE });
        }

        // Header: what ran, how it went, and what to do with it.
        let x = area.x + 24.0;
        let w = area.w - 48.0;
        let mut y = area.y + 18.0;
        let (mark, color) = state_mark(t, &check.state);
        r.text(x, y, mark, UI, 15.0, 600, color, 18.0);
        r.text(x + 22.0, y, &check.name, UI, 15.0, 600, t.text, w - 22.0);
        y += 26.0;
        if check.name != check.command {
            r.text(x, y, &format!("$ {}", check.command), MONO, 12.0, 400, t.text2, w);
            y += 22.0;
        }
        let status = match (check.state.as_str(), check.exit) {
            ("running", _) => format!("running · {}", ago(check.started)),
            ("pending", _) => "waiting for the checks before it".into(),
            ("stopped", _) => "stopped".into(),
            (state, Some(code)) => format!("{state} · exit {code} · {}s", check.ended.saturating_sub(check.started)),
            (state, None) => format!("{state} · {}s", check.ended.saturating_sub(check.started)),
        };
        let sw = r.text(x, y, &status, UI, 12.0, 500, color, w);
        let why = match (check.origin.as_str(), check.why.is_empty()) {
            ("sticky", _) => format!("· failed before, so it runs until it passes{}", if check.why.is_empty() { String::new() } else { format!(" · {}", check.why) }),
            (_, false) => format!("· {}", check.why),
            _ => String::new(),
        };
        r.text(x + sw + 12.0, y, &why, UI, 12.0, 400, t.text3, w - sw - 12.0);
        y += 22.0;
        if !check.note.is_empty() {
            r.text(x, y, &check.note, UI, 12.0, 500, t.del, w);
            y += 22.0;
        }
        y += 4.0;
        let errors = self.checks.as_ref().unwrap().console.as_ref().map_or(0, |c| c.errors);
        let running = check.state == "running" || check.state == "pending";
        let mut buttons: Vec<(String, ChecksAction)> = Vec::new();
        if !running {
            buttons.push(("Rerun".into(), ChecksAction::Rerun(check.name.clone(), check.command.clone())));
        }
        if errors > 0 {
            buttons.push((format!("Next error ({errors})"), ChecksAction::NextError));
        }
        buttons.push(("Copy output".into(), ChecksAction::Copy));
        if check.state == "failed" {
            buttons.push(("Don't require".into(), ChecksAction::Drop(check.command.clone())));
        }
        let mut bx = x;
        for (label, action) in buttons {
            let bw = r.measure(UI, 12.0, 500, &label) + 24.0;
            let b = Rect::new(bx, y, bw, 28.0);
            r.frame(b, 0.0, t.bg, t.border2);
            r.text(b.x + 12.0, r.center(b, UI, 12.0), &label, UI, 12.0, 500, t.text, bw);
            if self.hit(r, b) {
                self.actions.push(Action::Checks(action));
            }
            bx += bw + 8.0;
        }
        y += 40.0;

        // The output: wrapped to the pane, following the end unless scrolled up.
        let pane = Rect::new(x - 8.0, y, w + 16.0, area.y + area.h - y - 12.0);
        r.frame(pane, 0.0, t.bg, t.border);
        let cell = r.advance(MONO, CON_SIZE, 400, '0');
        let gutter = 52.0;
        let cols = (((pane.w - gutter - 16.0) / cell).floor() as usize).max(20);
        let visible = ((pane.h - 16.0) / CON_LINE).floor().max(1.0);
        let con = self.checks.as_mut().unwrap().console.as_mut().unwrap();
        con.area = pane;
        update_rows(&con.lines, cols, &mut con.rows);
        let total_rows = con.rows.1.last().copied().unwrap_or(0) as f32;
        let bottom = (total_rows - visible).max(0.0);
        if con.follow || con.top >= bottom {
            (con.top, con.follow) = (bottom, true);
        }
        if con.lines.is_empty() {
            let note = if con.done || check.state == "pending" { "No output" } else { "Waiting for output…" };
            r.text(pane.x + 16.0, pane.y + 12.0, note, MONO, CON_SIZE, 400, t.text3, pane.w);
            return;
        }
        r.set_clip(Some(pane));
        let top = con.top.floor() as usize;
        let mut line = con.rows.1.partition_point(|&end| end <= top);
        let mut row = if line == 0 { 0 } else { con.rows.1[line - 1] };
        let mut ry = pane.y + 8.0 - (con.top - top as f32) * CON_LINE - (top - row) as f32 * CON_LINE;
        while line < con.lines.len() && ry < pane.y + pane.h {
            let l = &con.lines[line];
            let n = con.rows.1[line] - row;
            if l.error {
                r.rect(Rect::new(pane.x + 1.0, ry - 2.0, 3.0, n as f32 * CON_LINE), 0.0, t.del);
            }
            let num = (line + 1).to_string();
            let nw = r.measure(MONO, 11.0, 400, &num);
            r.text(pane.x + gutter - 12.0 - nw, ry, &num, MONO, 11.0, 400, alpha(t.text3, 150), nw + 2.0);
            // Spans across wrapped rows, a character cell each.
            let mut col = 0usize;
            for (text, style) in &l.spans {
                let (fg, bg) = colors(t, style);
                for chunk in chunks(text, col, cols) {
                    let (r_i, c_i) = ((col) / cols, col % cols);
                    let cx = pane.x + gutter + c_i as f32 * cell;
                    let cy = ry + r_i as f32 * CON_LINE;
                    let k = chunk.chars().count();
                    if let Some(bg) = bg {
                        r.rect(Rect::new(cx, cy - 2.0, k as f32 * cell, CON_LINE), 0.0, bg);
                    }
                    if cy + CON_LINE > pane.y && cy < pane.y + pane.h {
                        r.text(cx, cy, chunk, MONO, CON_SIZE, if style.bold { 700 } else { 400 }, fg, k as f32 * cell + cell);
                    }
                    col += k;
                }
            }
            ry += n as f32 * CON_LINE;
            row += n;
            line += 1;
        }
        r.set_clip(None);
        if !con.follow {
            let label = "↓ Follow";
            let bw = r.measure(UI, 11.5, 500, label) + 20.0;
            let b = Rect::new(pane.x + pane.w - bw - 12.0, pane.y + pane.h - 36.0, bw, 26.0);
            r.rect(b, 0.0, t.raised);
            r.text(b.x + 10.0, r.center(b, UI, 11.5), label, UI, 11.5, 500, t.text, bw);
            if self.hit(r, b) {
                let con = self.checks.as_mut().unwrap().console.as_mut().unwrap();
                con.follow = true;
            }
        }
    }
}

/// Keep the unchanged prefix when a console page replaces its trailing lines.
fn update_rows(lines: &[ConsoleLine], cols: usize, rows: &mut (usize, Vec<usize>)) {
    if rows.0 != cols {
        rows.0 = cols;
        rows.1.clear();
    }
    let mut acc = rows.1.last().copied().unwrap_or(0);
    for line in &lines[rows.1.len()..] {
        let n: usize = line.spans.iter().map(|s| s.0.chars().count()).sum();
        acc += n.div_ceil(cols).max(1);
        rows.1.push(acc);
    }
}

/// A run's or check's state mark.
pub(super) fn state_mark(t: &Theme, state: &str) -> (&'static str, Rgba) {
    match state {
        "passed" => ("✓", t.done),
        "failed" => ("✗", t.del),
        "running" | "planning" => ("●", t.work),
        "approval" => ("■", t.needs),
        "unverified" => ("!", t.needs),
        "error" => ("!", t.del),
        "stopped" | "stale" | "skipped" => ("–", t.text3),
        _ => ("○", t.text3),
    }
}

fn colors(t: &Theme, s: &Style) -> (Rgba, Option<Rgba>) {
    let pick = |c: proto::Color| match c {
        proto::Color::Idx(i) => t.indexed(i),
        proto::Color::Rgb(r, g, b) => [r, g, b, 255],
    };
    let fg = s.fg.map_or(t.text, pick);
    let fg = if s.dim { alpha(fg, 150) } else { fg };
    let bg = s.bg.map(pick);
    (readable(fg, bg.unwrap_or(t.bg)), bg)
}

/// `text` split where it crosses a row boundary, starting at column `col` of rows `cols` wide.
fn chunks(text: &str, col: usize, cols: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    for (at, (i, _)) in (col..).zip(text.char_indices()) {
        if at.is_multiple_of(cols) && at != col && i > start {
            out.push(&text[start..i]);
            start = i;
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapping_updates_replaced_tail_and_resize() {
        let line = |text: &str| ConsoleLine { spans: vec![(text.into(), Style::default())], error: false };
        let mut lines = vec![line("abcd"), line("✓✓✓✓✓"), line("")];
        let mut rows = (0, Vec::new());
        update_rows(&lines, 4, &mut rows);
        assert_eq!(rows.1, [1, 3, 4]);
        // A live output reply replaces the last line and appends more output.
        rows.1.truncate(2);
        lines[2] = line("123456789");
        lines.push(line("next"));
        update_rows(&lines, 4, &mut rows);
        assert_eq!(rows.1, [1, 3, 6, 7]);
        update_rows(&lines, 8, &mut rows);
        assert_eq!(rows.1, [1, 2, 4, 5]);
        update_rows(&lines, 8, &mut rows);
        assert_eq!(rows.1, [1, 2, 4, 5]);
    }

    #[test]
    fn spans_split_at_row_ends() {
        assert_eq!(chunks("abcdef", 0, 4), ["abcd", "ef"]);
        assert_eq!(chunks("abcdef", 2, 4), ["ab", "cdef"]);
        assert_eq!(chunks("ab", 4, 4), ["ab"]);
        assert_eq!(chunks("✓✓✓", 3, 4), ["✓", "✓✓"]);
    }
}

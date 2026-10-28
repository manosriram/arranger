//! Review screen (docs/ui/Review.dc.html): a task's diff against its base, per-hunk
//! revert/accept, line notes sent back to the agent, and commit / PR / discard.

use super::*;
use arranger::proto::{FileDiff, Hunk, Ship};
use std::collections::HashSet;

pub const STRIP_H: f32 = 56.0;
const BAR_H: f32 = 62.0;
const LINE: f32 = 21.0;
const HUNK_H: f32 = 36.0;
const CARD_HEAD: f32 = 40.0;
const NOTE_X: f32 = 110.0;

pub struct CheckOutput {
    command: String,
    output: String,
    wrapped_lines: Vec<String>,
    wrapped_width: Option<f32>,
    ok: Option<bool>,
    pub scroll: f32,
    next: Instant,
    pending: bool,
}

impl CheckOutput {
    fn set_output(&mut self, output: String) {
        if self.output != output {
            self.output = output;
            self.wrapped_width = None;
        }
    }
}

pub struct ReviewView {
    pub check_output: Option<CheckOutput>,
    pub task: u64,
    /// The machine the task runs on.
    pub host: usize,
    pub data: Option<proto::Review>,
    pub file: usize,
    pub split: bool,
    pub scroll: f32,
    /// The left rail (files, checks, summary) scrolls on its own.
    pub rail_scroll: f32,
    /// "path\0header" of hunks marked reviewed; they collapse.
    accepted: HashSet<String>,
    pub notes: Vec<Note>,
    pub editing: Option<usize>,
    pub message: String,
    refreshed: Instant,
    default_message: String,
    status: String,
    error: bool,
    busy: bool,
    discard_armed: bool,
    /// The Merge button's menu (commit only, open a PR) is open, and where it was drawn last frame.
    ship_menu: Option<Rect>,
    /// How the last commit went: (last line of git's output, ok). Shown in the emptied message box.
    shipped: Option<(String, bool)>,
    /// The commit in flight, for the button's "Merging…".
    shipping: Option<Ship>,
    /// The button's confirmation ("Merged into main") and when it started; it shows for FLASH.
    flash: Option<(String, Instant)>,
}

pub const FLASH: Duration = Duration::from_millis(2000);

impl ReviewView {
    /// Something on screen moves: the button's dots while busy, or its confirmation fading.
    pub fn animating(&self) -> bool {
        self.data.as_ref().is_some_and(|d| d.summary_state == "running") || self.check_output.is_some() || self.busy || self.flash.as_ref().is_some_and(|f| f.1.elapsed() < FLASH)
    }
}

pub struct Note {
    path: String,
    header: String,
    /// Line in the new file, or in the old one for a removed line.
    line: u32,
    new_side: bool,
    pub text: String,
}

pub enum ReviewAction {
    Close,
    Run,
    Refresh,
    GenerateSummary,
    Check,
    File(usize),
    Split(bool),
    Revert(String, String),
    Accept(String),
    Comment { path: String, header: String, line: u32, new_side: bool },
    SendNote(usize),
    SaveNote,
    EditNote(usize),
    DeleteNote(usize),
    Commit(Ship),
    ShipMenu(bool),
    /// Prompt the agent to resolve the merge's conflicts, then go back to its chat.
    AskResolve,
    AbortMerge,
    /// Settle every conflict with the task's branch (true) or its base.
    TakeSide(bool),
    Discard,
    FocusMessage,
}

pub enum Tag {
    Review(u64),
    Summary,
    CheckOutput(u64, String),
    /// A background diff for the Review tab's enabled state.
    Changes(crate::TaskKey),
    Revert,
    Resolve,
    Commit,
    Note,
    Discard,
    Message,
    /// Deleting a task from vim's `d`.
    Delete,
    /// A task's events, for the last prompt it was sent: the clone dialog shows it.
    LastPrompt(crate::TaskKey),
    /// The Checks tab's runs, and a check's output.
    CheckRuns(crate::TaskKey),
    Console(crate::TaskKey, u64),
}

fn key(path: &str, header: &str) -> String {
    format!("{path}\0{header}")
}

/// Inline markdown runs: (text, bold, code). `**`/`__` toggle bold and backticks code; `*`/`_` emphasis
/// markers are dropped, links keep their text.
pub(super) fn spans(line: &str) -> Vec<(String, bool, bool)> {
    let mut out = vec![(String::new(), false, false)];
    let (mut bold, mut code) = (false, false);
    let mut rest = line;
    while let Some(c) = rest.chars().next() {
        let link = (!code && c == '[').then(|| rest[1..].split_once("](")).flatten();
        let link = link.and_then(|(text, a)| Some((text, a.split_once(')')?.1)));
        if c == '`' || (!code && (rest.starts_with("**") || rest.starts_with("__"))) {
            let n = if c == '`' { code = !code; 1 } else { bold = !bold; 2 };
            rest = &rest[n..];
            out.push((String::new(), bold, code));
        } else if !code && (c == '*' || c == '_') && {
            // An emphasis marker opens before a word or closes after one; snake_case keeps its `_`.
            let prev = out.last().unwrap().0.ends_with(char::is_alphanumeric);
            let next = rest[1..].chars().next();
            (!prev && next.is_some_and(|n| !n.is_whitespace())) || (prev && !next.is_some_and(char::is_alphanumeric))
        } {
            rest = &rest[1..];
        } else if let Some((text, after)) = link {
            out.last_mut().unwrap().0.push_str(text);
            rest = after;
        } else {
            out.last_mut().unwrap().0.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    out.retain(|s| !s.0.is_empty());
    out
}

/// Markdown as plain text on one line (the sidebar's quoted summary).
pub fn plain(line: &str) -> String {
    let line = line.trim_start_matches(['#', '-', '*', '>', ' ']);
    spans(line).into_iter().map(|s| s.0).collect()
}

/// Line height of the summary's prose: roomy enough to read at a glance in JetBrains Mono.
const SUMMARY_LINE_H: f32 = 22.0;

/// Draws an agent's markdown (headings, bullets, code blocks, bold and `code` runs) wrapped to `w`
/// from `y`; returns the y below it.
fn draw_markdown(r: &mut Renderer, t: &Theme, text: &str, x: f32, mut y: f32, w: f32) -> f32 {
    let mut fenced = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            r.text(x + 8.0, y, &line.replace('\t', "    "), MONO, 12.0, 400, t.tc, w - 8.0);
            y += 18.0;
            continue;
        }
        if trimmed.is_empty() {
            y += 10.0;
            continue;
        }
        let heading = trimmed.starts_with('#');
        let ordered = trimmed.split_once(". ").filter(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        let (marker, body) = if heading {
            (None, trimmed.trim_start_matches('#').trim_start().to_string())
        } else if let Some(b) = ["- ", "* ", "+ "].iter().find_map(|m| trimmed.strip_prefix(m)) {
            (Some("•".to_string()), b.to_string())
        } else if let Some((n, b)) = ordered {
            (Some(format!("{n}.")), b.to_string())
        } else {
            (None, trimmed.to_string())
        };
        let indent = if marker.is_some() { 14.0 } else { 0.0 };
        if let Some(m) = &marker {
            r.text(x, y, m, UI, 13.0, 400, t.text3, indent);
        }
        // Greedy wrap over words, each keeping its run's style.
        let (mut cx, right) = (x + indent, x + w);
        for (run, bold, code) in spans(&body) {
            let (font, size, weight) = if code { (MONO, 12.0, 400) } else { (UI, 13.0, if bold || heading { 600 } else { 400 }) };
            let color = if code { t.tc } else if bold || heading { t.text } else { t.text2 };
            let space = r.advance(font, size, weight, ' ');
            for (i, word) in run.split(' ').enumerate() {
                let ww = r.measure(font, size, weight, word);
                if cx + ww > right && cx > x + indent {
                    (cx, y) = (x + indent, y + SUMMARY_LINE_H);
                }
                let dy = if code { 1.0 } else { 0.0 };
                cx += r.text(cx, y + dy, word, font, size, weight, color, right - cx);
                if i + 1 < run.split(' ').count() {
                    cx += space;
                }
            }
        }
        y += if heading { SUMMARY_LINE_H + 4.0 } else { SUMMARY_LINE_H };
    }
    y
}

fn clean(text: &str) -> String {
    text.trim_end_matches(['\n', '\r']).replace('\t', "    ")
}

impl App {
    pub fn open_review(&mut self) {
        let Some(task) = self.current() else { return };
        if self.review.as_ref().is_some_and(|r| (r.host, r.task) == (task.machine, task.id)) {
            self.review_action(ReviewAction::Refresh);
            return;
        }
        let message = title(task);
        self.review = Some(ReviewView {
            check_output: None,
            task: task.id,
            host: task.machine,
            data: None,
            file: 0,
            split: false,
            scroll: 0.0,
            rail_scroll: 0.0,
            accepted: HashSet::new(),
            notes: Vec::new(),
            editing: None,
            default_message: message.clone(),
            message,
            refreshed: Instant::now(),
            status: "Loading…".into(),
            error: false,
            busy: false,
            discard_armed: false,
            ship_menu: None,
            shipped: None,
            shipping: None,
            flash: None,
        });
        self.checks = None;
        self.focus = self.rest();
        self.review_action(ReviewAction::Refresh);
    }

    pub fn spawn_request(&self, host: usize, tag: Tag, req: Request) {
        let (p, sock) = (self.proxy.clone(), self.sock(host));
        thread::spawn(move || {
            let reply = request_at(sock.as_deref(), &req).map_err(|e| e.to_string());
            let _ = p.send_event(Msg::Reply(tag, reply));
        });
    }

    pub fn on_reply(&mut self, tag: Tag, reply: Result<Reply, String>) {
        if matches!(tag, Tag::CheckRuns(_) | Tag::Console(..)) {
            return self.on_checks_reply(tag, reply);
        }
        if let Tag::Changes(key) = tag {
            // An error (no worktree, say) means there's nothing to look at either.
            let has = matches!(&reply, Ok(Reply::Review(d)) if !d.files.is_empty() || d.merging || !d.conflicts.is_empty());
            if let Some(c) = self.changes.get_mut(&key) {
                c.1 = Some(has);
            }
            match &reply {
                Ok(Reply::Review(d)) => {
                    let (added, removed) = d.files.iter().fold((0, 0), |(a, r), f| (a + f.added, r + f.removed));
                    self.stats.insert(key, (d.base.clone(), d.files.len(), added, removed));
                }
                _ => {
                    self.stats.remove(&key);
                }
            }
            return;
        }
        if let Tag::LastPrompt(key) = tag {
            let clone = self.dialog.as_mut().and_then(|d| d.clone.as_mut()).filter(|c| c.key == key);
            if let (Some(c), Ok(Reply::Events(events))) = (clone, reply) {
                let last = events.into_iter().rev().find_map(|(_, e)| match e {
                    Event::Prompt { text } => Some(text),
                    _ => None,
                });
                // Its first prompt is shown already.
                c.last = last.filter(|l| l.trim() != c.first.trim()).unwrap_or_default();
            }
            return;
        }
        let Some(rv) = &mut self.review else {
            if let (Tag::Message | Tag::Delete, Err(e)) = (tag, reply) {
                eprintln!("arranger: {e}");
            }
            return;
        };
        let mut refresh = false;
        match (tag, reply) {
            (Tag::CheckOutput(id, command), reply) if id == rv.task => {
                if let Some(check) = rv.check_output.as_mut().filter(|c| c.command == command) {
                    check.pending = false;
                    check.next = Instant::now() + Duration::from_millis(300);
                    match reply {
                        Ok(Reply::Chat { items, .. }) => {
                            if let Some((output, ok)) = items.into_iter().rev().find_map(|item| match item {
                                proto::ChatItem::Tool { target, output, ok, .. } if target == command => Some((output, ok)),
                                _ => None,
                            }) {
                                check.set_output(output);
                                check.ok = ok;
                            } else {
                                check.set_output("No output captured for this check.".into());
                            }
                        }
                        Ok(Reply::Error(e)) | Err(e) => {
                            check.set_output(format!("Output unavailable: {e}"));
                            check.next = Instant::now() + Duration::from_secs(60);
                        }
                        _ => {}
                    }
                }
            }
            (Tag::Review(id), Ok(Reply::Review(data))) if id == rv.task => {
                rv.refreshed = Instant::now();
                if let Some(s) = data.work_summary.as_ref().filter(|s| s.tree == data.tree && data.summary_state == "ready") {
                    let message = arranger::summary::message(s);
                    if rv.message == rv.default_message { rv.message = message.clone(); }
                    rv.default_message = message;
                } else if data.summary_state == "stale" && rv.message == rv.default_message {
                    rv.message.clear();
                    rv.default_message.clear();
                }
                rv.file = rv.file.min(data.files.len().saturating_sub(1));
                rv.data = Some(*data);
                if !rv.busy && !rv.error {
                    rv.status.clear();
                }
            }
            (Tag::Summary, Ok(_)) => refresh = true,
            (Tag::Revert | Tag::Resolve, Ok(_)) => refresh = true,
            (Tag::Commit, Ok(Reply::JobDone { ok, output })) => {
                rv.busy = false;
                let line = output.lines().last().unwrap_or(if ok { "Done" } else { "Failed" }).to_string();
                if ok {
                    rv.message.clear();
                    rv.status.clear();
                } else {
                    rv.status = line.clone();
                }
                rv.error = !ok;
                if ok {
                    let base = rv.data.as_ref().map_or("", |d| d.base.as_str());
                    let label = match rv.shipping {
                        Some(Ship::Merge) => format!("Merged into {base}"),
                        Some(Ship::Pr) => "PR opened".into(),
                        // "committed 1a2b3c4 subject"
                        _ => format!("Committed {}", line.split(' ').nth(1).unwrap_or("")),
                    };
                    rv.flash = Some((label, Instant::now()));
                }
                rv.shipping = None;
                rv.shipped = Some((line, ok));
                refresh = true;
            }
            (Tag::Commit, Err(e)) => { rv.busy = false; rv.status = e; rv.error = true; refresh = true; },
            (Tag::Commit, _) => rv.busy = false,
            (Tag::Note, Ok(_)) => {
                rv.status = "Sent to the agent".into();
                rv.error = false;
            }
            (Tag::Discard, Ok(_)) => {
                self.review = None;
                return;
            }
            (_, Err(e)) => {
                rv.status = e;
                rv.error = true;
            }
            _ => {}
        }
        if refresh {
            self.review_action(ReviewAction::Refresh);
        }
    }

    pub fn review_action(&mut self, action: ReviewAction) {
        let agent = self.review.as_ref().and_then(|rv| self.task((rv.host, rv.task))).map(|t| adapter::kind(&t.agent).to_string());
        if matches!(action, ReviewAction::Run) {
            let result = self.review.as_ref().and_then(|rv| self.task((rv.host, rv.task)).map(|task| (rv.host, task.worktree.clone())))
                .ok_or_else(|| std::io::Error::other("Task is no longer available"))
                .and_then(|(host, path)| open_worktree_terminal(&path, self.hosts[host].cfg.as_ref().map(|c| c.ssh.as_str())));
            if let Some(rv) = &mut self.review {
                rv.error = result.is_err();
                rv.status = match result { Ok(()) => "Opened a terminal in the worktree".into(), Err(e) => e.to_string() };
            }
            return;
        }
        let rest = self.rest();
        let Some(rv) = &mut self.review else { return };
        let (id, host) = (rv.task, rv.host);
        if !matches!(action, ReviewAction::Discard) {
            rv.discard_armed = false;
        }
        match action {
            ReviewAction::Run => unreachable!(),
            ReviewAction::Close => {
                self.review = None;
                self.focus = rest;
            }
            ReviewAction::Check => self.open_checks(),
            ReviewAction::GenerateSummary => self.spawn_request(host, Tag::Summary, Request::GenerateSummary { id }),
            ReviewAction::Refresh => self.spawn_request(host, Tag::Review(id), Request::Review { id }),
            ReviewAction::File(i) => {
                rv.file = i;
                rv.scroll = 0.0;
            }
            ReviewAction::Split(s) => rv.split = s,
            ReviewAction::Revert(path, header) => {
                rv.status = format!("Reverting a hunk in {path}…");
                rv.error = false;
                self.spawn_request(host, Tag::Revert, Request::Revert { id, path, header });
            }
            ReviewAction::Accept(k) => {
                if !rv.accepted.remove(&k) {
                    rv.accepted.insert(k);
                }
            }
            ReviewAction::Comment { path, header, line, new_side } => {
                let existing = rv.notes.iter().position(|n| n.path == path && n.line == line && n.new_side == new_side);
                let i = existing.unwrap_or_else(|| {
                    rv.notes.push(Note { path, header, line, new_side, text: String::new() });
                    rv.notes.len() - 1
                });
                rv.editing = Some(i);
                self.focus = Focus::Note;
                self.anchor = None;
            }
            ReviewAction::SaveNote => {
                rv.editing = None;
                rv.notes.retain(|n| !n.text.trim().is_empty());
                self.focus = rest;
            }
            ReviewAction::EditNote(i) => {
                rv.editing = Some(i);
                self.focus = Focus::Note;
            }
            ReviewAction::DeleteNote(i) => {
                if i < rv.notes.len() {
                    rv.notes.remove(i);
                }
                rv.editing = None;
                self.focus = rest;
            }
            ReviewAction::SendNote(i) => {
                let Some(note) = rv.notes.get(i).filter(|n| !n.text.trim().is_empty()) else { return };
                let side = if note.new_side { "" } else { " (removed line)" };
                let text = format!("Review note on {}:{}{side}: {}", note.path, note.line, note.text.trim());
                rv.notes.remove(i);
                rv.editing = None;
                rv.status = format!("Sending to {}…", agent.unwrap_or_else(|| "the agent".into()));
                self.focus = rest;
                self.spawn_request(host, Tag::Note, Request::Prompt { id, text });
            }
            ReviewAction::Commit(ship) => {
                rv.ship_menu = None;
                rv.shipped = None;
                rv.flash = None;
                if rv.busy {
                    return;
                }
                rv.busy = true;
                rv.error = false;
                rv.shipping = Some(ship);
                rv.status = match ship {
                    Ship::Commit => "Committing…",
                    Ship::Merge => "Committing and merging…",
                    Ship::Pr => "Committing, pushing and opening a PR…",
                }
                .into();
                let message = rv.message.clone();
                self.focus = rest;
                let force = ship == Ship::Merge && rv.data.as_ref().is_some_and(|d| !matches!(d.verification.as_str(), "passed" | "accepted" | "skipped"));
                self.spawn_request(host, Tag::Commit, Request::Commit { id, message, ship, force });
            }
            ReviewAction::ShipMenu(open) => rv.ship_menu = open.then(Rect::default),
            ReviewAction::AskResolve => {
                let Some(d) = &rv.data else { return };
                let files = d.conflicts.iter().map(|f| format!("`{f}`")).collect::<Vec<_>>().join(", ");
                let text = format!(
                    "Merging `{}` into this branch conflicts in {files}. Resolve the conflicts, keeping the intent of both sides, then `git add` the files and finish the merge with `git commit --no-edit`. Don't change anything else.",
                    d.base
                );
                if self.task((host, id)).is_some_and(|t| group(&t.state) == 4) {
                    self.actions.push(Action::Revive((host, id), text)); // it reads the prompt as it comes back
                    return;
                }
                self.review = None;
                self.focus = rest;
                self.spawn_request(host, Tag::Note, Request::Prompt { id, text });
            }
            ReviewAction::AbortMerge => {
                rv.ship_menu = None;
                rv.status = "Aborting the merge…".into();
                self.spawn_request(host, Tag::Resolve, Request::AbortMerge { id });
            }
            ReviewAction::TakeSide(ours) => {
                rv.ship_menu = None;
                rv.status = "Taking one side…".into();
                self.spawn_request(host, Tag::Resolve, Request::TakeSide { id, ours });
            }
            ReviewAction::FocusMessage => self.focus = Focus::CommitMsg,
            ReviewAction::Discard => {
                if rv.discard_armed {
                    rv.status = "Discarding the worktree…".into();
                    self.spawn_request(host, Tag::Discard, Request::Kill { id });
                } else {
                    rv.discard_armed = true;
                }
            }
        }
    }

    pub fn note_text(&mut self) -> Option<&mut String> {
        let rv = self.review.as_mut()?;
        let i = rv.editing?;
        rv.notes.get_mut(i).map(|n| &mut n.text)
    }

    pub fn draw_review(&mut self, r: &mut Renderer, t: &Theme, area: Rect) {
        if let Some(rv) = self.review.as_mut().filter(|r| r.data.as_ref().is_some_and(|d| d.summary_state == "running") && r.refreshed.elapsed() > Duration::from_secs(1)) {
            rv.refreshed = Instant::now();
            let (host, id) = (rv.host, rv.task);
            self.spawn_request(host, Tag::Review(id), Request::Review { id });
        }
        let Some(rv) = self.review.as_ref() else { return };
        let task = self.task((rv.host, rv.task)).cloned();
        let agent = task.as_ref().map(|t| adapter::kind(&t.agent).to_string()).unwrap_or_default();
        let mut acts = Vec::new();
        // A click on the open Merge menu is the menu's alone, whatever lies under it.
        let menu_click = self.click.filter(|p| rv.ship_menu.is_some_and(|m| m.contains(*p)));
        if menu_click.is_some() {
            self.click = None;
        }

        // Task strip.
        let strip = Rect::new(area.x, area.y, area.w, STRIP_H);
        r.rect(Rect::new(area.x, strip.y + STRIP_H - 1.0, area.w, 1.0), 0.0, t.border);
        let back = Rect::new(area.x + 24.0, strip.y + 14.0, 28.0, 28.0);
        r.frame(back, 4.0, t.bg, t.border2);
        // Drawn, not a glyph: the font's arrow sits off its box's centre.
        r.path(7, "M46 32L18 32M30 19L17 32L30 45", Some(6.0), back.x + 6.0, back.y + 6.0, 16.0, t.tc);
        if self.hit(r, back) {
            acts.push(ReviewAction::Close);
        }
        let name = task.as_ref().map(title).unwrap_or_default();
        let tw = r.text(back.x + 42.0, r.center(strip, UI, 16.0), &name, UI, 16.0, 500, t.text, area.w * 0.32);
        let (branch, base) = rv.data.as_ref().map_or((String::new(), String::new()), |d| (d.branch.clone(), d.base.clone()));
        r.text(back.x + 56.0 + tw, r.center(strip, MONO, 12.0), &format!("{branch} → {base}"), MONO, 12.0, 400, t.text3, 260.0);

        // Right side: [Unified|Split], totals, status.
        let seg_w = r.measure(UI, 12.0, 400, "Unified") + r.measure(UI, 12.0, 400, "Split") + 46.0;
        let seg = Rect::new(area.x + area.w - 24.0 - seg_w, strip.y + 15.0, seg_w, 26.0);
        r.frame(seg, 4.0, t.bg, t.border);
        let mut sx = seg.x + 3.0;
        for (label, on, split) in [("Unified", !rv.split, false), ("Split", rv.split, true)] {
            let bw = r.measure(UI, 12.0, 400, label) + 20.0;
            let b = Rect::new(sx, seg.y + 3.0, bw, 20.0);
            if on {
                r.rect(b, 3.0, t.raised);
            }
            r.text(b.x + 10.0, r.center(b, UI, 12.0), label, UI, 12.0, 400, if on { t.text } else { t.text3 }, bw);
            if self.hit(r, b) {
                acts.push(ReviewAction::Split(split));
            }
            sx += bw;
        }
        let run_w = r.measure(UI, 12.0, 500, "Run in worktree") + 24.0;
        let run = Rect::new(seg.x - 12.0 - run_w, seg.y, run_w, seg.h);
        r.frame(run, 4.0, t.bg, t.border2);
        r.text(run.x + 12.0, r.center(run, UI, 12.0), "Run in worktree", UI, 12.0, 500, t.text, run_w - 24.0);
        if self.hit(r, run) {
            acts.push(ReviewAction::Run);
        }
        let mut right = run.x - 14.0;
        if let Some(d) = &rv.data {
            let (add, del) = d.files.iter().fold((0, 0), |(a, b), f| (a + f.added, b + f.removed));
            let parts = [
                (format!("{} file{} · ", d.files.len(), if d.files.len() == 1 { "" } else { "s" }), t.text3),
                (format!("+{add} "), t.add),
                (format!("−{del}"), t.del),
            ];
            let total: f32 = parts.iter().map(|(s, _)| r.measure(MONO, 12.0, 400, s)).sum();
            let mut x = right - total;
            for (s, c) in &parts {
                x += r.text(x, r.center(strip, MONO, 12.0), s, MONO, 12.0, 400, *c, 200.0);
            }
            right -= total + 16.0;
        }
        if !rv.status.is_empty() {
            let max = (right - (back.x + 330.0 + tw)).max(80.0);
            let sw = r.measure(UI, 12.0, 400, &rv.status).min(max);
            r.text(right - sw, r.center(strip, UI, 12.0), &rv.status, UI, 12.0, 400, if rv.error { t.del } else { t.text2 }, max);
        }

        let body = Rect::new(area.x, area.y + STRIP_H, area.w, area.h - STRIP_H);
        let files: &[FileDiff] = rv.data.as_ref().map_or(&[], |d| &d.files);

        // Left rail.
        let rail = Rect::new(body.x, body.y, self.rail_w, body.h);
        r.rect(rail, 0.0, t.panel);
        r.rect(Rect::new(rail.x + self.rail_w - 1.0, rail.y, 1.0, rail.h), 0.0, t.border);
        r.set_clip(Some(rail));
        let mut y = rail.y + 16.0 - rv.rail_scroll;
        let section = |r: &mut Renderer, y: f32, label: &str| {
            r.text(rail.x + 16.0, y, label, UI, 11.0, 500, t.text3, 200.0);
        };
        if let Some(d) = &rv.data {
            let (label, color) = match d.verification.as_str() {
                "passed" => ("Passed on this exact diff", t.add),
                "accepted" => ("Failed checks accepted", t.needs),
                "skipped" => ("Checks skipped for this diff", t.needs),
                "stale" => ("Checks are out of date", t.needs),
                "failed" => ("Checks failed", t.del),
                "running" => ("Checks running", t.work),
                _ => ("This diff is unverified", t.needs),
            };
            r.text(rail.x + 16.0, y, label, UI, 12.0, 500, color, rail.w - 32.0);
            y += 25.0;
            let b = Rect::new(rail.x + 16.0, y, 100.0, 26.0);
            r.frame(b, 0.0, t.bg, t.border2);
            r.text(b.x + 9.0, r.center(b, UI, 12.0), "Run checks", UI, 12.0, 500, t.text, b.w);
            if self.hit(r, b) { acts.push(ReviewAction::Check); }
            y += 40.0;
        }
        section(r, y, "FILES");
        y += 22.0;
        if files.is_empty() && rv.data.is_some() {
            r.text(rail.x + 16.0, y + 6.0, "No changes yet", UI, 12.0, 400, t.text3, 240.0);
            y += 28.0;
        }
        for (i, f) in files.iter().enumerate() {
            let row = Rect::new(rail.x + 8.0, y, self.rail_w - 17.0, 32.0);
            let on = i == rv.file;
            if on {
                r.frame(row, 4.0, t.sel, t.sel_ring);
            }
            let conflicted = rv.data.as_ref().is_some_and(|d| d.conflicts.contains(&f.path));
            let counts = if conflicted { "conflict".to_string() } else { format!("+{} −{}", f.added, f.removed) };
            let cw = r.measure(MONO, 12.0, 400, &counts);
            let (dir, file) = f.path.rsplit_once('/').map_or(("", f.path.as_str()), |(d, n)| (d, n));
            let avail = row.w - cw - 30.0;
            let mut x = row.x + 10.0;
            if !dir.is_empty() {
                x += r.text(x, r.center(row, MONO, 12.0), &format!("{dir}/"), MONO, 12.0, 400, t.text3, avail * 0.55);
            }
            let fc = if f.status == 'D' { t.text3 } else if on { t.tc_on } else { t.tc };
            r.text(x, r.center(row, MONO, 12.0), file, MONO, 12.0, 400, fc, row.x + 10.0 + avail - x);
            let ax = row.x + row.w - 10.0 - cw;
            if conflicted {
                r.text(ax, r.center(row, MONO, 12.0), &counts, MONO, 12.0, 400, t.needs, 80.0);
            } else {
                let aw = r.text(ax, r.center(row, MONO, 12.0), &format!("+{}", f.added), MONO, 12.0, 400, t.add, 80.0);
                r.text(ax + aw + r.measure(MONO, 12.0, 400, " "), r.center(row, MONO, 12.0), &format!("−{}", f.removed), MONO, 12.0, 400, t.del, 80.0);
            }
            if self.hit(r, row) {
                acts.push(ReviewAction::File(i));
            }
            y += 34.0;
            if let Some(change) = rv.data.as_ref().and_then(|d| d.work_summary.as_ref().filter(|s| s.tree == d.tree)).and_then(|s| s.files.iter().find(|(path, _)| path == &f.path)).map(|(_, change)| change) {
                y = draw_markdown(r, t, change, rail.x + 24.0, y, self.rail_w - 40.0) + 8.0;
            }
        }
        if let Some(d) = rv.data.as_ref().filter(|d| !d.checks.is_empty()) {
            y += 18.0;
            section(r, y, "CHECKS");
            y += 24.0;
            for (cmd, ok) in &d.checks {
                let row = Rect::new(rail.x + 16.0, y, self.rail_w - 32.0, 20.0);
                let (icon, ic, label) = match ok {
                    Some(true) => ("✓", t.add, "passed"),
                    Some(false) => ("✗", t.del, "failed"),
                    None => ("■", t.needs, "running"),
                };
                r.text(row.x, r.center(row, MONO, 12.0), icon, MONO, 12.0, 600, ic, 16.0);
                let lw = r.measure(MONO, 12.0, 400, label);
                r.text(row.x + 21.0, r.center(row, MONO, 12.0), cmd, MONO, 12.0, 400, t.tc, row.w - 30.0 - lw);
                r.text(row.x + row.w - lw, r.center(row, MONO, 12.0), label, MONO, 12.0, 400, if ok.is_none() { t.needs } else { t.text3 }, lw + 2.0);
                if self.hit(r, row) { acts.push(ReviewAction::Check); }
                y += 28.0;
            }
        }
        if let Some(d) = &rv.data {
            y += 14.0;
            let status = format!("Summary: {}", d.summary_state);
            r.text(rail.x + 16.0, y, &status, UI, 11.5, 500, if d.summary_state == "ready" { t.add } else { t.needs }, rail.w - 32.0);
            y += 22.0;
            if d.summary_state != "running" {
                let b = Rect::new(rail.x + 16.0, y, 160.0, 26.0);
                r.frame(b, 0.0, t.bg, t.border2);
                r.text(b.x + 9.0, r.center(b, UI, 12.0), "Regenerate summary", UI, 12.0, 500, t.text, b.w);
                if self.hit(r, b) { acts.push(ReviewAction::GenerateSummary); }
                y += 32.0;
            }
        }
        if let Some(summary) = rv.data.as_ref().map(|d| d.summary.trim()).filter(|s| !s.is_empty()) {
            y += 18.0;
            section(r, y, "AGENT SUMMARY");
            y = draw_markdown(r, t, summary, rail.x + 16.0, y + 24.0, self.rail_w - 32.0);
        }
        r.set_clip(None);
        let rail_max = y + rv.rail_scroll + 20.0 - (rail.y + rail.h);

        // Diff column and commit bar.
        let col = Rect::new(body.x + self.rail_w, body.y, body.w - self.rail_w, body.h - BAR_H);
        r.rect(col, 0.0, t.term);
        // Nothing to merge: no changes, not mid-merge, and nothing committed but not yet merged.
        let idle = rv.data.as_ref().is_some_and(|d| d.files.is_empty() && !d.merging)
            && task.as_ref().is_some_and(|t| matches!(t.shipped.as_str(), "" | "merged"));
        // A merge under way: say so above the diff, with a way back out.
        let mut view = col;
        if let Some(d) = rv.data.as_ref().filter(|d| d.merging) {
            let n = d.conflicts.len();
            let (head, sub) = if n > 0 {
                (format!("Merging into {} conflicts in {n} file{}", d.base, if n == 1 { "" } else { "s" }), format!("{} is untouched: it was merged into {} here, so the conflicts can be fixed in this worktree.", d.base, d.branch))
            } else {
                ("Conflicts resolved".to_string(), format!("Merge into {} to finish.", d.base))
            };
            let banner = Rect::new(col.x + 16.0, col.y + 16.0, col.w - 32.0, 62.0);
            let c = if n > 0 { t.needs } else { t.done };
            r.frame(banner, 6.0, blend(t.term, c, 0.08), alpha(c, 0x59));
            r.path(10, "M32 8L60 56H4L32 8ZM32 26V38M32 46V47", Some(5.0), banner.x + 16.0, banner.y + 21.0, 18.0, c);
            r.text(banner.x + 46.0, banner.y + 14.0, &head, UI, 13.0, 600, c, banner.w - 200.0);
            r.text(banner.x + 46.0, banner.y + 34.0, &sub, UI, 12.5, 400, t.text2, banner.w - 200.0);
            let label = "Abort merge";
            let aw = r.measure(UI, 12.5, 400, label) + 24.0;
            let abort = Rect::new(banner.x + banner.w - 16.0 - aw, banner.y + 16.0, aw, 30.0);
            r.frame(abort, 4.0, blend(t.term, c, 0.08), t.border2);
            r.text(abort.x + 12.0, r.center(abort, UI, 12.5), label, UI, 12.5, 400, t.text, aw);
            if !rv.busy && self.hit(r, abort) {
                acts.push(ReviewAction::AbortMerge);
            }
            view = Rect { y: col.y + 78.0, h: col.h - 78.0, ..col };
        }
        if let Some(file) = files.get(rv.file) {
            r.set_clip(Some(view));
            let max_scroll = self.draw_file(r, t, view, file, &agent, &mut acts);
            r.set_clip(None);
            if let Some(rv) = &mut self.review {
                rv.scroll = rv.scroll.min(max_scroll.max(0.0));
            }
        } else {
            let shipped = task.as_ref().map_or("", |t| t.shipped.as_str());
            if rv.data.is_some() && !shipped.is_empty() {
                // Done: a big check, what happened, and (once merged) cleaning up.
                let base = rv.data.as_ref().map_or("", |d| d.base.as_str());
                let branch = rv.data.as_ref().map_or("", |d| d.branch.as_str());
                let head = match shipped {
                    "merged" => format!("Merged into {base}"),
                    "PR" => "PR opened".into(),
                    _ => format!("Committed on {branch}"),
                };
                let (cx, cy) = (col.x + col.w / 2.0, col.y + col.h / 2.0 - 40.0);
                r.rect(Rect::new(cx - 28.0, cy - 28.0, 56.0, 56.0), 28.0, alpha(t.done, 0x26));
                r.path(8, "M14 33.6L25.6 45.2L50 18.8", Some(7.0), cx - 14.0, cy - 14.0, 28.0, t.done);
                let hw = r.measure(UI, 16.0, 600, &head);
                r.text(cx - hw / 2.0, cy + 44.0, &head, UI, 16.0, 600, t.text, col.w);
                let sub = "Nothing new to review";
                let sw = r.measure(UI, 12.5, 400, sub);
                r.text(cx - sw / 2.0, cy + 70.0, sub, UI, 12.5, 400, t.text3, col.w);
                if shipped == "merged" {
                    let label = if rv.discard_armed { "Click again to remove the worktree" } else { "Clean up worktree" };
                    let bw = r.measure(UI, 13.0, 400, label) + 28.0;
                    let b = Rect::new(cx - bw / 2.0, cy + 100.0, bw, 34.0);
                    r.frame(b, 4.0, t.term, t.border2);
                    r.text(b.x + 14.0, r.center(b, UI, 13.0), label, UI, 13.0, 400, t.text, bw);
                    if !rv.busy && self.hit(r, b) {
                        acts.push(ReviewAction::Discard);
                    }
                }
            } else {
                let msg = if rv.data.is_none() { "Loading…" } else { "Nothing to review: no changes yet" };
                let mw = r.measure(UI, 13.0, 400, msg);
                r.text(col.x + (col.w - mw) / 2.0, col.y + col.h / 2.0 - 8.0, msg, UI, 13.0, 400, t.text3, col.w);
            }
        }
        if let Some(rv) = &mut self.review {
            rv.rail_scroll = rv.rail_scroll.min(rail_max.max(0.0));
        }
        let ended = task.as_ref().is_some_and(|t| group(&t.state) == 4);
        let menu = self.draw_commit_bar(r, t, Rect::new(col.x, col.y + col.h, col.w, BAR_H), &agent, ended, idle, menu_click, &mut acts);
        if let Some(rv) = self.review.as_mut().filter(|rv| rv.ship_menu.is_some()) {
            rv.ship_menu = menu;
        }
        self.actions.extend(acts.into_iter().map(Action::Review));
    }

    pub fn draw_check_output(&mut self, r: &mut Renderer, t: &Theme, w: f32, h: f32) {
        let Some(rv) = self.review.as_mut() else { return };
        let Some(check) = rv.check_output.as_mut() else { return };
        let request = if !check.pending && Instant::now() >= check.next {
            check.pending = true;
            Some((rv.host, rv.task, check.command.clone()))
        } else { None };
        r.set_clip(None);
        let bounds = Rect::new(0.0, TITLE_H, w, (h - TITLE_H).max(0.0));
        r.rect(bounds, 0.0, t.scrim);
        r.target(bounds, Cursor::Arrow);
        let pw = (w - 32.0).clamp(1.0, 960.0);
        let ph = (bounds.h - 32.0).clamp(1.0, 640.0);
        let panel = Rect::new((w - pw) / 2.0, bounds.y + (bounds.h - ph) / 2.0, pw, ph);
        r.frame(panel, 8.0, t.dialog, t.border2);
        r.target(panel, Cursor::Arrow);
        r.text(panel.x + 16.0, panel.y + 16.0, &check.command, MONO, 13.0, 600, t.text, panel.w - 120.0);
        let close = Rect::new(panel.x + panel.w - 80.0, panel.y + 8.0, 68.0, 32.0);
        r.frame(close, 4.0, t.raised, t.border2);
        r.text(close.x + 8.0, close.y + 8.0, "Close ✕", UI, 12.0, 500, t.text, close.w - 16.0);
        let label = match check.ok { None => "Running • live output", Some(true) => "Passed", Some(false) => "Failed" };
        r.text(panel.x + 16.0, panel.y + 40.0, label, UI, 12.0, 400, t.text3, panel.w - 32.0);
        let logs = Rect::new(panel.x + 12.0, panel.y + 64.0, (panel.w - 24.0).max(1.0), (panel.h - 76.0).max(1.0));
        r.frame(logs, 4.0, t.term, t.border2);
        let body = Rect::new(logs.x + 10.0, logs.y + 10.0, (logs.w - 28.0).max(1.0), (logs.h - 20.0).max(1.0));
        // Wrapping measures every character; only redo it when content or width changes.
        if check.wrapped_width != Some(body.w) {
            let output = if check.output.is_empty() { "Waiting for output…" } else { &check.output };
            check.wrapped_lines = output.lines().flat_map(|line| wrap(r, line, MONO, 12.0, body.w)).collect();
            check.wrapped_width = Some(body.w);
        }
        let lines = &check.wrapped_lines;
        let max = (lines.len() as f32 * 18.0 - body.h).max(0.0);
        check.scroll = check.scroll.min(max);
        r.set_clip(Some(body));
        let offset = max - check.scroll;
        let first = (offset / 18.0).floor() as usize;
        let end = (((offset + body.h) / 18.0).ceil() as usize).min(lines.len());
        for (i, line) in lines.iter().enumerate().take(end).skip(first) {
            let y = body.y + i as f32 * 18.0 - offset;
            if y + 18.0 >= body.y && y < body.y + body.h {
                r.text(body.x, y, line, MONO, 12.0, 400, readable(t.text, t.term), body.w);
            }
        }
        r.set_clip(None);
        if max > 0.0 {
            let track = Rect::new(logs.x + logs.w - 10.0, body.y, 4.0, body.h);
            let thumb_h = (body.h * body.h / (max + body.h)).max(20.0).min(body.h);
            r.rect(track, 0.0, t.border);
            r.rect(Rect::new(track.x, track.y + (body.h - thumb_h) * (1.0 - check.scroll / max), track.w, thumb_h), 0.0, t.text3);
        }
        r.target(close, Cursor::Pointer);
        let dismiss = self.click.take().is_some_and(|p| close.contains(p) || !panel.contains(p));
        if dismiss { self.review.as_mut().unwrap().check_output = None; }
        if let Some((host, id, command)) = request {
            self.spawn_request(host, Tag::CheckOutput(id, command), Request::ChatPage { id, end: usize::MAX, limit: usize::MAX });
        }
    }

    /// Draws one file's card; returns how far it can scroll.
    fn draw_file(&self, r: &mut Renderer, t: &Theme, col: Rect, file: &FileDiff, agent: &str, acts: &mut Vec<ReviewAction>) -> f32 {
        let rv = self.review.as_ref().unwrap();
        let card_w = col.w - 40.0;
        let x = col.x + 20.0;
        let note_w = (card_w - NOTE_X - 20.0).min(560.0);
        let notes_at = |h: &Hunk, line: u32, new_side: bool| -> Vec<usize> {
            let at = |n: &Note| n.path == file.path && n.header == h.header && n.line == line && n.new_side == new_side;
            (0..rv.notes.len()).filter(|&i| at(&rv.notes[i])).collect()
        };
        let note_h = |r: &Renderer, n: &Note| 24.0 + 18.0 + 8.0 + wrap(r, &n.text, UI, 13.0, note_w - 28.0).len().max(1) as f32 * 20.0 + 12.0 + 28.0 + 12.0 + 8.0;

        // Rows: unified lines, or split pairs (old, new).
        let rows = |h: &Hunk| -> Vec<(Option<usize>, Option<usize>)> {
            if !rv.split {
                return (0..h.lines.len()).map(|i| (Some(i), None)).collect();
            }
            let mut out = Vec::new();
            let mut i = 0;
            while i < h.lines.len() {
                if h.lines[i].kind == ' ' {
                    out.push((Some(i), Some(i)));
                    i += 1;
                    continue;
                }
                let dels: Vec<usize> = (i..h.lines.len()).take_while(|&k| h.lines[k].kind == '-').collect();
                let after = i + dels.len();
                let adds: Vec<usize> = (after..h.lines.len()).take_while(|&k| h.lines[k].kind == '+').collect();
                for k in 0..dels.len().max(adds.len()) {
                    out.push((dels.get(k).copied(), adds.get(k).copied()));
                }
                i = after + adds.len();
            }
            out
        };
        let anchor = |h: &Hunk, i: usize| -> (u32, bool) {
            let l = &h.lines[i];
            if l.kind == '-' { (l.old, false) } else { (l.new, true) }
        };

        // Height first, for the card background.
        let mut height = CARD_HEAD;
        if file.binary {
            height += 40.0;
        }
        for h in &file.hunks {
            height += HUNK_H;
            if rv.accepted.contains(&key(&file.path, &h.header)) {
                continue;
            }
            for (a, b) in rows(h) {
                height += LINE;
                for i in [a, b].into_iter().flatten() {
                    let (line, side) = anchor(h, i);
                    height += notes_at(h, line, side).iter().map(|&i| note_h(r, &rv.notes[i])).sum::<f32>();
                    if rv.split {
                        break; // one anchor per split row
                    }
                }
            }
        }

        let top = col.y + 16.0 - rv.scroll;
        let card = Rect::new(x, top, card_w, height);
        r.frame(card, 6.0, t.bg, t.border);
        let head = Rect::new(x + 1.0, top + 1.0, card_w - 2.0, CARD_HEAD - 1.0);
        r.rect(head, 5.0, t.dialog);
        r.rect(Rect::new(x + 1.0, top + CARD_HEAD - 1.0, card_w - 2.0, 1.0), 0.0, t.border);
        let pw = r.text(x + 14.0, r.center(head, MONO, 12.0), &file.path, MONO, 12.0, 400, t.text, card_w * 0.6);
        let kind = match file.status {
            'A' => " new file",
            'D' => " deleted",
            _ => "",
        };
        let mut cx = x + 26.0 + pw;
        cx += r.text(cx, r.center(head, MONO, 12.0), &format!("+{} ", file.added), MONO, 12.0, 400, t.add, 80.0);
        cx += r.text(cx, r.center(head, MONO, 12.0), &format!("−{}", file.removed), MONO, 12.0, 400, t.del, 80.0);
        r.text(cx, r.center(head, MONO, 12.0), kind, MONO, 12.0, 400, t.text3, 120.0);

        let mut y = top + CARD_HEAD;
        if file.binary {
            r.text(x + 14.0, y + 12.0, "Binary file", UI, 12.5, 400, t.text3, 200.0);
        }
        let visible = |y: f32, h: f32| y + h >= col.y && y <= col.y + col.h;
        let (add, add_bg, del, del_bg) = t.diff();
        for h in &file.hunks {
            let k = key(&file.path, &h.header);
            let accepted = rv.accepted.contains(&k);
            let hr = Rect::new(x + 1.0, y, card_w - 2.0, HUNK_H);
            if visible(y, HUNK_H) {
                r.rect(hr, 0.0, t.panel);
                r.rect(Rect::new(hr.x, y, hr.w, 1.0), 0.0, t.sel);
                r.rect(Rect::new(hr.x, y + HUNK_H - 1.0, hr.w, 1.0), 0.0, t.sel);
                let accept_label = if accepted { "Accepted ✓" } else { "Accept" };
                let aw = r.measure(UI, 12.0, 400, accept_label) + 16.0;
                let accept = Rect::new(hr.x + hr.w - 14.0 - aw, y + 6.0, aw, 24.0);
                let rw = r.measure(UI, 12.0, 400, "Revert hunk") + 16.0;
                let revert = Rect::new(accept.x - 8.0 - rw, y + 6.0, rw, 24.0);
                r.text(hr.x + 13.0, r.center(hr, MONO, 12.0), &h.header, MONO, 12.0, 400, t.work, revert.x - hr.x - 26.0);
                r.frame(revert, 3.0, t.panel, t.border2);
                r.text(revert.x + 8.0, r.center(revert, UI, 12.0), "Revert hunk", UI, 12.0, 400, t.tc, rw);
                r.frame(accept, 3.0, if accepted { t.panel } else { t.sel }, t.border2);
                r.text(accept.x + 8.0, r.center(accept, UI, 12.0), accept_label, UI, 12.0, 400, if accepted { t.done } else { t.text }, aw);
                if self.hit(r, revert) {
                    acts.push(ReviewAction::Revert(file.path.clone(), h.header.clone()));
                } else if self.hit(r, accept) {
                    acts.push(ReviewAction::Accept(k.clone()));
                }
            }
            y += HUNK_H;
            if accepted {
                continue;
            }
            for (a, b) in rows(h) {
                let row = Rect::new(x + 1.0, y, card_w - 2.0, LINE);
                if visible(y, LINE) {
                    let halves: Vec<(Rect, Option<usize>)> = if rv.split {
                        let half = row.w / 2.0;
                        vec![(Rect::new(row.x, y, half, LINE), a), (Rect::new(row.x + half, y, half, LINE), b)]
                    } else {
                        vec![(row, a)]
                    };
                    for (cell, idx) in &halves {
                        let Some(i) = idx else {
                            r.rect(*cell, 0.0, alpha(t.faint, 0x10));
                            continue;
                        };
                        let l = &h.lines[*i];
                        let (bg, sign, sc, fg) = match l.kind {
                            '+' => (Some(add_bg), "+", add, t.text),
                            '-' => (Some(del_bg), "−", del, t.text),
                            _ => (None, "", t.faint, t.text2),
                        };
                        if let Some(bg) = bg {
                            r.rect(*cell, 0.0, bg);
                        }
                        let num = |n: u32| if n == 0 { String::new() } else { n.to_string() };
                        let base_y = r.center(*cell, MONO, 12.5);
                        let mut nx = cell.x;
                        let nums = if !rv.split { vec![l.old, l.new] } else if cell.x == row.x { vec![l.old] } else { vec![l.new] };
                        for n in nums {
                            let s = num(n);
                            let sw = r.measure(MONO, 12.0, 400, &s);
                            r.text(nx + 34.0 - sw, base_y, &s, MONO, 12.0, 400, t.faint, 40.0);
                            nx += 44.0;
                        }
                        r.rect(Rect::new(nx - 1.0, y, 1.0, LINE), 0.0, t.sel);
                        r.text(nx + 7.0, base_y, sign, MONO, 12.5, 400, sc, 20.0);
                        r.text(nx + 22.0, base_y, &clean(&l.text), MONO, 12.5, 400, fg, cell.x + cell.w - nx - 30.0);
                    }
                    if self.hit(r, row) {
                        let pick = if rv.split { b.or(a) } else { a }.unwrap();
                        let (line, new_side) = anchor(h, pick);
                        acts.push(ReviewAction::Comment { path: file.path.clone(), header: h.header.clone(), line, new_side });
                    }
                }
                y += LINE;
                for i in [a, b].into_iter().flatten() {
                    let (line, side) = anchor(h, i);
                    for ni in notes_at(h, line, side) {
                        let note = &rv.notes[ni];
                        let nh = note_h(r, note);
                        if visible(y, nh) {
                            self.draw_note(r, t, Rect::new(x + NOTE_X, y + 8.0, note_w, nh - 16.0), ni, note, agent, acts);
                        }
                        y += nh;
                    }
                    if rv.split {
                        break;
                    }
                }
            }
        }
        height + 32.0 - col.h
    }

    fn draw_note(&self, r: &mut Renderer, t: &Theme, b: Rect, i: usize, note: &Note, agent: &str, acts: &mut Vec<ReviewAction>) {
        let rv = self.review.as_ref().unwrap();
        let editing = rv.editing == Some(i);
        r.frame(b, 6.0, t.dialog, if editing { t.work } else { t.border2 });
        let hx = b.x + 14.0;
        let badge = Rect::new(hx, b.y + 12.0, 18.0, 18.0);
        r.rect(badge, 3.0, t.primary_bg);
        r.text(badge.x + 5.0, r.center(badge, UI, 10.0), "Y", UI, 10.0, 600, t.primary_fg, 12.0);
        let yw = r.text(hx + 26.0, r.center(badge, UI, 12.0), "You", UI, 12.0, 500, t.text, 40.0);
        let side = if note.new_side { "" } else { " (removed)" };
        let state = if editing { "draft" } else { "saved" };
        r.text(hx + 32.0 + yw, r.center(badge, UI, 12.0), &format!("· line {}{side} · {state}", note.line), UI, 12.0, 400, t.text3, b.w - 90.0);
        let lines = wrap(r, &note.text, UI, 13.0, b.w - 28.0);
        let starts = line_starts(&note.text, &lines);
        let caret = self.caret_for(Focus::Note, &note.text).filter(|_| editing).map(|at| caret_line_at(&lines, &starts, at));
        let mut y = b.y + 38.0;
        for (k, (line, &start)) in lines.iter().zip(&starts).enumerate() {
            if editing {
                let at = caret.filter(|c| c.0 == k).map(|c| c.1);
                self.field_text(r, t, hx, y, line, UI, 13.0, b.w - 28.0, (Focus::Note, start), self.focus == Focus::Note, at, t.work);
            } else {
                r.text(hx, y, line, UI, 13.0, 400, t.text, b.w - 28.0);
            }
            y += 20.0;
        }
        if note.text.is_empty() && !editing {
            r.text(hx, b.y + 38.0, "Empty note", UI, 13.0, 400, t.faint, 200.0);
        }
        let by = b.y + b.h - 40.0;
        let send_label = format!("Send to {agent}");
        let key_label = mod_label("↵");
        let sw = r.measure(UI, 12.0, 500, &send_label) + r.measure(MONO, 10.0, 400, &key_label) + 28.0;
        let send = Rect::new(hx, by, sw, 28.0);
        r.rect(send, 4.0, t.primary_bg);
        let lw = r.text(send.x + 10.0, r.center(send, UI, 12.0), &send_label, UI, 12.0, 500, t.primary_fg, sw);
        r.text(send.x + 18.0 + lw, r.center(send, MONO, 10.0), &key_label, MONO, 10.0, 400, alpha(t.primary_fg, 150), 60.0);
        let second = if editing { "Save note" } else { "Delete" };
        let ow = r.measure(UI, 12.0, 400, second) + 20.0;
        let other = Rect::new(send.x + sw + 8.0, by, ow, 28.0);
        r.frame(other, 4.0, t.dialog, t.border2);
        r.text(other.x + 10.0, r.center(other, UI, 12.0), second, UI, 12.0, 400, t.tc, ow);
        if self.hit(r, send) {
            acts.push(ReviewAction::SendNote(i));
        } else if self.hit(r, other) {
            acts.push(if editing { ReviewAction::SaveNote } else { ReviewAction::DeleteNote(i) });
        } else if self.hit(r, b) && !editing {
            acts.push(ReviewAction::EditNote(i));
        }
    }

    /// Commit message, Discard, and Merge with a menu for the other ways to ship. Returns where the
    /// menu was drawn, if open.
    #[allow(clippy::too_many_arguments)]
    fn draw_commit_bar(&self, r: &mut Renderer, t: &Theme, bar: Rect, agent: &str, ended: bool, idle: bool, menu_click: Option<(f32, f32)>, acts: &mut Vec<ReviewAction>) -> Option<Rect> {
        let rv = self.review.as_ref().unwrap();
        r.rect(bar, 0.0, t.bg);
        r.rect(Rect::new(bar.x, bar.y, bar.w, 1.0), 0.0, t.border);
        let h = 38.0;
        let by = bar.y + 12.0;
        let base = rv.data.as_ref().map_or("", |d| d.base.as_str());
        let branch = rv.data.as_ref().map_or("", |d| d.branch.as_str());
        // Conflicts turn the main button into asking the agent; ▾ then settles them by side instead.
        let conflicted = rv.data.as_ref().is_some_and(|d| !d.conflicts.is_empty());
        let force_merge = rv.data.as_ref().is_some_and(|d| !matches!(d.verification.as_str(), "passed" | "accepted" | "skipped"));
        let merge_label = match (conflicted, ended) {
            (true, false) => format!("Ask {agent} to resolve"),
            (true, true) => format!("Revive {agent} to resolve"),
            _ if force_merge => "Force merge".to_string(),
            _ if base.is_empty() => "Merge".to_string(),
            _ => format!("Merge into {base}"),
        };
        let merge_key = if conflicted { "" } else if cfg!(target_os = "macos") { "⌘⇧↵" } else { "Ctrl⇧↵" };
        let mw = r.measure(UI, 13.0, 500, &merge_label) + r.measure(MONO, 10.0, 400, merge_key) + 38.0;
        let ship = Rect::new(bar.x + bar.w - 16.0 - mw - 30.0, by, mw + 30.0, h);
        let (merge, chev) = (Rect { w: mw, ..ship }, Rect::new(ship.x + mw, by, 30.0, h));
        let discard_label = if rv.discard_armed { "Click again to discard" } else { "Discard worktree" };
        let dw = r.measure(UI, 13.0, 400, discard_label) + 28.0;
        let discard = Rect::new(ship.x - 10.0 - dw, by, dw, h);

        let field = Rect::new(bar.x + 16.0, by, discard.x - 10.0 - bar.x - 16.0, h);
        let focused = self.focus == Focus::CommitMsg;
        r.frame(field, 4.0, t.panel, if focused { t.work } else { t.border2 });
        let lw = r.text(field.x + 12.0, r.center(field, UI, 11.0), "COMMIT", UI, 11.0, 400, t.text3, 80.0);
        let fx = field.x + 22.0 + lw;
        if let Some((line, ok)) = rv.shipped.as_ref().filter(|_| rv.message.is_empty() && !focused) {
            let (mark, c) = if *ok { ("✓ ", t.done) } else { ("✕ ", t.del) };
            r.text(fx, r.center(field, UI, 12.5), &format!("{mark}{line}"), UI, 12.5, 500, c, field.w - lw - 34.0);
        } else if rv.message.is_empty() && !focused {
            r.text(fx, r.center(field, MONO, 12.5), "commit message", MONO, 12.5, 400, t.faint, field.w - lw - 34.0);
        } else {
            self.field_text(r, t, fx, r.center(field, MONO, 12.5), &rv.message, MONO, 12.5, field.w - lw - 34.0, (Focus::CommitMsg, 0), focused, self.caret_for(Focus::CommitMsg, &rv.message), t.work);
        }
        if self.hit_as(r, field, Cursor::Text) {
            acts.push(ReviewAction::FocusMessage);
        }

        let dim = |c: Rgba| if rv.busy { alpha(c, 110) } else { c };
        r.frame(discard, 4.0, t.bg, if rv.discard_armed { t.del } else { t.border2 });
        r.text(discard.x + 14.0, r.center(discard, UI, 13.0), discard_label, UI, 13.0, 400, dim(t.del), dw);
        // Confirmation: green with "✓ Merged into main", easing back to normal over its last fifth.
        let flash = rv.flash.as_ref().map(|(l, at)| (l, at.elapsed().as_secs_f32() / FLASH.as_secs_f32())).filter(|f| f.1 < 1.0);
        let fill = match flash {
            Some((_, k)) => blend(t.done, t.primary_bg, ((k - 0.8) / 0.2).clamp(0.0, 1.0)),
            None if idle => t.raised,
            None if force_merge && !conflicted => dim(t.del),
            None => dim(t.primary_bg),
        };
        r.rect(ship, 4.0, fill);
        let fg = if flash.is_some() { readable(t.on_accent, t.done) } else if idle { t.text3 } else { t.primary_fg };
        if let Some((label, _)) = flash.filter(|f| f.1 < 0.8) {
            r.text(ship.x + 14.0, r.center(ship, UI, 13.0), &format!("✓  {label}"), UI, 13.0, 600, fg, ship.w - 20.0);
        } else if let Some(s) = rv.shipping.filter(|_| rv.busy) {
            let verb = match s {
                Ship::Commit => "Committing",
                Ship::Merge => "Merging",
                Ship::Pr => "Opening PR",
            };
            let ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis());
            let dots = ".".repeat(1 + (ms / 400) as usize % 3);
            r.text(ship.x + 14.0, r.center(ship, UI, 13.0), &format!("{verb}{dots}"), UI, 13.0, 500, alpha(t.primary_fg, 200), ship.w - 20.0);
        } else {
            let mlw = r.text(merge.x + 14.0, r.center(merge, UI, 13.0), &merge_label, UI, 13.0, 500, fg, mw);
            r.text(merge.x + 24.0 + mlw, r.center(merge, MONO, 10.0), merge_key, MONO, 10.0, 400, alpha(fg, 150), 60.0);
            r.rect(Rect::new(chev.x, by + 9.0, 1.0, h - 18.0), 0.0, alpha(fg, 70));
            r.path(9, "M18 26L32 40L46 26", Some(6.0), chev.x + 9.0, by + 13.0, 12.0, fg);
        }
        let open = rv.ship_menu.is_some();
        if !rv.busy {
            if self.hit(r, discard) {
                acts.push(ReviewAction::Discard);
            } else if self.hit(r, merge) && !idle {
                // An ended agent is revived first, with the resolve prompt.
                acts.push(if conflicted { ReviewAction::AskResolve } else { ReviewAction::Commit(Ship::Merge) });
            } else if self.hit(r, chev) {
                acts.push(ReviewAction::ShipMenu(!open));
            }
        }
        if !open {
            return None;
        }
        if self.click.is_some_and(|p| !chev.contains(p)) {
            acts.push(ReviewAction::ShipMenu(false)); // a click anywhere else closes it
        }

        // The menu opens upward from the button, right-aligned to it.
        let items = if conflicted {
            vec![(format!("Take all of {branch}"), String::new(), ReviewAction::TakeSide(true)), (format!("Take all of {base}"), String::new(), ReviewAction::TakeSide(false))]
        } else {
            vec![("Commit only".into(), "↵ in the message".into(), ReviewAction::Commit(Ship::Commit)), ("Commit & open PR".into(), String::new(), ReviewAction::Commit(Ship::Pr))]
        };
        let menu_w = 220.0f32.max(ship.w);
        let menu = Rect::new(ship.x + ship.w - menu_w, ship.y - 8.0 - (8.0 + items.len() as f32 * 30.0), menu_w, 8.0 + items.len() as f32 * 30.0);
        r.rect(Rect::new(menu.x - 1.0, menu.y + 3.0, menu.w + 2.0, menu.h + 2.0), 8.0, alpha([0, 0, 0, 255], if t.dark { 0x60 } else { 0x18 }));
        r.frame(menu, 6.0, t.bg, t.border2);
        r.target(menu, Cursor::Arrow);
        for (i, (label, hint, action)) in items.into_iter().enumerate() {
            let row = Rect::new(menu.x + 4.0, menu.y + 4.0 + i as f32 * 30.0, menu.w - 8.0, 30.0);
            r.text(row.x + 10.0, r.center(row, UI, 13.0), &label, UI, 13.0, 400, t.text, row.w - 20.0);
            let hw = r.measure(UI, 11.0, 400, &hint);
            r.text(row.x + row.w - 10.0 - hw, r.center(row, UI, 11.0), &hint, UI, 11.0, 400, t.text3, hw + 2.0);
            r.target(row, Cursor::Pointer);
            if menu_click.is_some_and(|p| row.contains(p)) {
                acts.push(action);
            }
        }
        Some(menu)
    }
}

/// Quote for the shell, including paths containing spaces or apostrophes.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn open_worktree_terminal(path: &str, remote: Option<&str>) -> std::io::Result<()> {
    if path.is_empty() {
        return Err(std::io::Error::other("Task has no worktree"));
    }
    if let Some(host) = remote { arranger::machine::validate_target(host)?; }
    let script = format!("cd -- {} && exec \"${{SHELL:-/bin/sh}}\" -l", shell_quote(path));
    let command = match remote {
        Some(host) => format!("ssh -t -o StrictHostKeyChecking=yes -o ForwardAgent=no -o ForwardX11=no -- {} {}", shell_quote(host), shell_quote(&script)),
        None => script,
    };
    if cfg!(target_os = "macos") {
        // Create the worktree window before activating Terminal: activating a
        // stopped app first also opens an empty startup window.
        // Pass the shell command as argv, never as AppleScript source.
        let status = Command::new("osascript")
            .args(["-e", "on run argv\ntell application \"Terminal\"\ndo script (item 1 of argv)\nactivate\nend tell\nend run", "--", &command])
            .status()?;
        if status.success() { return Ok(()); }
        return Err(std::io::Error::other("Could not open Terminal"));
    }
    for terminal in ["x-terminal-emulator", "xterm"] {
        match Command::new(terminal).args(["-e", "/bin/sh", "-c", &command]).spawn() {
            Ok(mut child) => { std::thread::spawn(move || { let _ = child.wait(); }); return Ok(()); }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("Install x-terminal-emulator or xterm to open a worktree terminal"))
}

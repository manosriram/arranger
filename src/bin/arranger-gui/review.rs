//! Review screen (docs/ui/Review.dc.html): a task's diff against its base, per-hunk
//! revert/accept, line notes sent back to the agent, and commit / PR / discard.

use super::*;
use arranger::proto::{FileDiff, Hunk};
use std::collections::HashSet;

const STRIP_H: f32 = 56.0;
const RAIL_W: f32 = 280.0;
const BAR_H: f32 = 62.0;
const LINE: f32 = 21.0;
const HUNK_H: f32 = 36.0;
const CARD_HEAD: f32 = 40.0;
const NOTE_X: f32 = 110.0;

pub struct ReviewView {
    pub task: u64,
    /// The machine the task runs on.
    pub host: usize,
    pub data: Option<proto::Review>,
    pub file: usize,
    pub split: bool,
    pub scroll: f32,
    /// "path\0header" of hunks marked reviewed; they collapse.
    accepted: HashSet<String>,
    pub notes: Vec<Note>,
    pub editing: Option<usize>,
    pub message: String,
    status: String,
    error: bool,
    busy: bool,
    discard_armed: bool,
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
    Refresh,
    File(usize),
    Split(bool),
    Revert(String, String),
    Accept(String),
    Comment { path: String, header: String, line: u32, new_side: bool },
    SendNote(usize),
    SaveNote,
    EditNote(usize),
    DeleteNote(usize),
    Commit(bool),
    Discard,
    FocusMessage,
}

pub enum Tag {
    Review(u64),
    Revert,
    Commit,
    Note,
    Discard,
    Message,
}

fn key(path: &str, header: &str) -> String {
    format!("{path}\0{header}")
}

/// Inline markdown runs: (text, bold, code). `**`/`__` toggle bold and backticks code; `*`/`_` emphasis
/// markers are dropped, links keep their text.
fn spans(line: &str) -> Vec<(String, bool, bool)> {
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

/// Draws an agent's markdown (headings, bullets, code blocks, bold and `code` runs) wrapped to `w`
/// from `y`, stopping at `max_y`.
fn draw_markdown(r: &mut Renderer, t: &Theme, text: &str, x: f32, mut y: f32, w: f32, max_y: f32) {
    let mut fenced = false;
    for line in text.lines() {
        if y > max_y {
            return;
        }
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
            y += 8.0;
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
                    (cx, y) = (x + indent, y + 20.0);
                    if y > max_y {
                        return;
                    }
                }
                let dy = if code { 1.0 } else { 0.0 };
                cx += r.text(cx, y + dy, word, font, size, weight, color, right - cx);
                if i + 1 < run.split(' ').count() {
                    cx += space;
                }
            }
        }
        y += if heading { 24.0 } else { 20.0 };
    }
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
            task: task.id,
            host: task.machine,
            data: None,
            file: 0,
            split: false,
            scroll: 0.0,
            accepted: HashSet::new(),
            notes: Vec::new(),
            editing: None,
            message,
            status: "Loading…".into(),
            error: false,
            busy: false,
            discard_armed: false,
        });
        self.focus = Focus::Term;
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
        let Some(rv) = &mut self.review else {
            if let (Tag::Message, Err(e)) = (tag, reply) {
                eprintln!("arranger: {e}");
            }
            return;
        };
        let mut refresh = false;
        match (tag, reply) {
            (Tag::Review(id), Ok(Reply::Review(data))) if id == rv.task => {
                rv.file = rv.file.min(data.files.len().saturating_sub(1));
                rv.data = Some(data);
                if !rv.busy && !rv.error {
                    rv.status.clear();
                }
            }
            (Tag::Revert, Ok(_)) => refresh = true,
            (Tag::Commit, Ok(Reply::JobDone { ok, output })) => {
                rv.busy = false;
                rv.error = !ok;
                rv.status = output.lines().last().unwrap_or(if ok { "Done" } else { "Failed" }).to_string();
                refresh = true;
            }
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
        let Some(rv) = &mut self.review else { return };
        let (id, host) = (rv.task, rv.host);
        if !matches!(action, ReviewAction::Discard) {
            rv.discard_armed = false;
        }
        match action {
            ReviewAction::Close => {
                self.review = None;
                self.focus = Focus::Term;
            }
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
                self.select_all = false;
            }
            ReviewAction::SaveNote => {
                rv.editing = None;
                rv.notes.retain(|n| !n.text.trim().is_empty());
                self.focus = Focus::Term;
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
                self.focus = Focus::Term;
            }
            ReviewAction::SendNote(i) => {
                let Some(note) = rv.notes.get(i).filter(|n| !n.text.trim().is_empty()) else { return };
                let side = if note.new_side { "" } else { " (removed line)" };
                let text = format!("Review note on {}:{}{side}: {}", note.path, note.line, note.text.trim());
                rv.notes.remove(i);
                rv.editing = None;
                rv.status = format!("Sending to {}…", agent.unwrap_or_else(|| "the agent".into()));
                self.focus = Focus::Term;
                self.spawn_request(host, Tag::Note, Request::Prompt { id, text });
            }
            ReviewAction::Commit(pr) => {
                if rv.busy {
                    return;
                }
                rv.busy = true;
                rv.error = false;
                rv.status = if pr { "Committing, pushing and opening a PR…".into() } else { "Committing…".into() };
                let message = rv.message.clone();
                self.focus = Focus::Term;
                self.spawn_request(host, Tag::Commit, Request::Commit { id, message, pr });
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
        let Some(rv) = self.review.as_ref() else { return };
        let task = self.task((rv.host, rv.task)).cloned();
        let agent = task.as_ref().map(|t| adapter::kind(&t.agent).to_string()).unwrap_or_default();
        let mut acts = Vec::new();

        // Task strip.
        let strip = Rect::new(area.x, area.y, area.w, STRIP_H);
        r.rect(Rect::new(area.x, strip.y + STRIP_H - 1.0, area.w, 1.0), 0.0, t.border);
        let back = Rect::new(area.x + 24.0, strip.y + 14.0, 28.0, 28.0);
        r.frame(back, 4.0, t.bg, t.border2);
        r.text(back.x + 8.0, r.center(back, MONO, 14.0), "←", MONO, 14.0, 400, t.tc, 20.0);
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
        let mut right = seg.x - 14.0;
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
        let rail = Rect::new(body.x, body.y, RAIL_W, body.h);
        r.rect(rail, 0.0, t.panel);
        r.rect(Rect::new(rail.x + RAIL_W - 1.0, rail.y, 1.0, rail.h), 0.0, t.border);
        r.set_clip(Some(rail));
        let mut y = rail.y + 16.0;
        let section = |r: &mut Renderer, y: f32, label: &str| {
            r.text(rail.x + 16.0, y, label, UI, 11.0, 500, t.text3, 200.0);
        };
        section(r, y, "FILES");
        y += 22.0;
        if files.is_empty() && rv.data.is_some() {
            r.text(rail.x + 16.0, y + 6.0, "No changes yet", UI, 12.0, 400, t.text3, 240.0);
            y += 28.0;
        }
        for (i, f) in files.iter().enumerate() {
            let row = Rect::new(rail.x + 8.0, y, RAIL_W - 17.0, 32.0);
            let on = i == rv.file;
            if on {
                r.frame(row, 4.0, t.sel, t.sel_ring);
            }
            let counts = format!("+{} −{}", f.added, f.removed);
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
            let aw = r.text(ax, r.center(row, MONO, 12.0), &format!("+{}", f.added), MONO, 12.0, 400, t.add, 80.0);
            r.text(ax + aw + r.measure(MONO, 12.0, 400, " "), r.center(row, MONO, 12.0), &format!("−{}", f.removed), MONO, 12.0, 400, t.del, 80.0);
            if self.hit(r, row) {
                acts.push(ReviewAction::File(i));
            }
            y += 34.0;
        }
        if let Some(d) = rv.data.as_ref().filter(|d| !d.checks.is_empty()) {
            y += 18.0;
            section(r, y, "CHECKS");
            y += 24.0;
            for (cmd, ok) in &d.checks {
                let row = Rect::new(rail.x + 16.0, y, RAIL_W - 32.0, 20.0);
                let (icon, ic, label) = match ok {
                    Some(true) => ("✓", t.add, "passed"),
                    Some(false) => ("✗", t.del, "failed"),
                    None => ("■", t.needs, "running"),
                };
                r.text(row.x, r.center(row, MONO, 12.0), icon, MONO, 12.0, 600, ic, 16.0);
                let lw = r.measure(MONO, 12.0, 400, label);
                r.text(row.x + 21.0, r.center(row, MONO, 12.0), cmd, MONO, 12.0, 400, t.tc, row.w - 30.0 - lw);
                r.text(row.x + row.w - lw, r.center(row, MONO, 12.0), label, MONO, 12.0, 400, if ok.is_none() { t.needs } else { t.text3 }, lw + 2.0);
                y += 28.0;
            }
        }
        if let Some(summary) = rv.data.as_ref().map(|d| d.summary.trim()).filter(|s| !s.is_empty()) {
            y += 18.0;
            section(r, y, "AGENT SUMMARY");
            draw_markdown(r, t, summary, rail.x + 16.0, y + 24.0, RAIL_W - 32.0, rail.y + rail.h - 20.0);
        }
        r.set_clip(None);

        // Diff column and commit bar.
        let col = Rect::new(body.x + RAIL_W, body.y, body.w - RAIL_W, body.h - BAR_H);
        r.rect(col, 0.0, t.term);
        if let Some(file) = files.get(rv.file) {
            r.set_clip(Some(col));
            let max_scroll = self.draw_file(r, t, col, file, &agent, &mut acts);
            r.set_clip(None);
            if let Some(rv) = &mut self.review {
                rv.scroll = rv.scroll.min(max_scroll.max(0.0));
            }
        } else {
            let msg = if rv.data.is_none() { "Loading…" } else { "Nothing to review: the worktree matches its base" };
            let mw = r.measure(UI, 13.0, 400, msg);
            r.text(col.x + (col.w - mw) / 2.0, col.y + col.h / 2.0 - 8.0, msg, UI, 13.0, 400, t.text3, col.w);
        }
        self.draw_commit_bar(r, t, Rect::new(col.x, col.y + col.h, col.w, BAR_H), &mut acts);
        self.actions.extend(acts.into_iter().map(Action::Review));
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
        let tint = |c: Rgba| blend(t.text, c, 0.35);
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
                            '+' => (Some(alpha(t.add, 0x12)), "+", t.add, tint(t.add)),
                            '-' => (Some(alpha(t.del, 0x12)), "−", t.del, tint(t.del)),
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
        let mut y = b.y + 38.0;
        for (k, line) in lines.iter().enumerate() {
            if editing && k == lines.len() - 1 {
                self.field_text(r, t, hx, y, line, UI, 13.0, b.w - 28.0, self.focus == Focus::Note, t.work);
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

    fn draw_commit_bar(&self, r: &mut Renderer, t: &Theme, bar: Rect, acts: &mut Vec<ReviewAction>) {
        let rv = self.review.as_ref().unwrap();
        r.rect(bar, 0.0, t.bg);
        r.rect(Rect::new(bar.x, bar.y, bar.w, 1.0), 0.0, t.border);
        let h = 38.0;
        let by = bar.y + 12.0;
        let pr_label = "Commit & open PR";
        let pr_key = if cfg!(target_os = "macos") { "⌘⇧↵" } else { "Ctrl⇧↵" };
        let pw = r.measure(UI, 13.0, 500, pr_label) + r.measure(MONO, 10.0, 400, pr_key) + 38.0;
        let pr = Rect::new(bar.x + bar.w - 16.0 - pw, by, pw, h);
        let cw = r.measure(UI, 13.0, 400, "Commit") + 28.0;
        let commit = Rect::new(pr.x - 10.0 - cw, by, cw, h);
        let discard_label = if rv.discard_armed { "Click again to discard" } else { "Discard worktree" };
        let dw = r.measure(UI, 13.0, 400, discard_label) + 28.0;
        let discard = Rect::new(commit.x - 10.0 - dw, by, dw, h);

        let field = Rect::new(bar.x + 16.0, by, discard.x - 10.0 - bar.x - 16.0, h);
        let focused = self.focus == Focus::CommitMsg;
        r.frame(field, 4.0, t.panel, if focused { t.work } else { t.border2 });
        let lw = r.text(field.x + 12.0, r.center(field, UI, 11.0), "COMMIT", UI, 11.0, 400, t.text3, 80.0);
        let fx = field.x + 22.0 + lw;
        if rv.message.is_empty() && !focused {
            r.text(fx, r.center(field, MONO, 12.5), "commit message", MONO, 12.5, 400, t.faint, field.w - lw - 34.0);
        } else {
            self.field_text(r, t, fx, r.center(field, MONO, 12.5), &rv.message, MONO, 12.5, field.w - lw - 34.0, focused, t.work);
        }
        if self.hit_as(r, field, Cursor::Text) {
            acts.push(ReviewAction::FocusMessage);
        }

        let dim = |c: Rgba| if rv.busy { alpha(c, 110) } else { c };
        r.frame(discard, 4.0, t.bg, if rv.discard_armed { t.del } else { t.border2 });
        r.text(discard.x + 14.0, r.center(discard, UI, 13.0), discard_label, UI, 13.0, 400, dim(t.del), dw);
        r.frame(commit, 4.0, t.dialog, t.border2);
        r.text(commit.x + 14.0, r.center(commit, UI, 13.0), "Commit", UI, 13.0, 400, dim(t.text), cw);
        r.rect(pr, 4.0, dim(t.primary_bg));
        let plw = r.text(pr.x + 14.0, r.center(pr, UI, 13.0), pr_label, UI, 13.0, 500, t.primary_fg, pw);
        r.text(pr.x + 24.0 + plw, r.center(pr, MONO, 10.0), pr_key, MONO, 10.0, 400, alpha(t.primary_fg, 150), 60.0);
        if !rv.busy {
            if self.hit(r, discard) {
                acts.push(ReviewAction::Discard);
            } else if self.hit(r, commit) {
                acts.push(ReviewAction::Commit(false));
            } else if self.hit(r, pr) {
                acts.push(ReviewAction::Commit(true));
            }
        }
    }
}

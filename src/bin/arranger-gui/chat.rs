//! A headless task's chat (docs: plan phase 4, the "Headless chat" artboard): its conversation
//! from the daemon, drawn from a layout cached per item, so a streaming reply re-lays out only
//! the item it grows and a long thread draws only the rows on screen. Older items load a page at
//! a time as you scroll up.
use super::*;
use arranger::proto::{ChatItem, ToolKind};
use std::collections::HashMap;

/// Items fetched at a time.
const PAGE: usize = 200;
const GAP: f32 = 14.0;
const BODY: f32 = 13.0;
const BODY_H: f32 = 21.0;
const CODE: f32 = 12.0;
const CODE_H: f32 = 18.0;
const PAD: f32 = 24.0;
const MAX_W: f32 = 860.0;
/// Lines of a tool's output or diff shown when opened.
const MAX_LINES: usize = 400;
/// An edit this small shows its diff without being opened.
const SMALL_DIFF: usize = 24;
/// Lines of the message box before it scrolls.
const COMPOSER_LINES: usize = 8;

#[derive(Clone, Copy, PartialEq)]
enum Ink {
    Text,
    Text2,
    Dim,
    Faint,
    Code,
    Add,
    Del,
    Bad,
    Kind(ToolKind),
}

#[derive(Clone, Copy, PartialEq)]
enum Fill {
    User,
    Code,
    Add,
    Del,
    Box,
}

struct Run {
    x: f32,
    text: String,
    font: usize,
    size: f32,
    weight: u16,
    ink: Ink,
    max_w: f32,
}

struct Row {
    y: f32,
    h: f32,
    runs: Vec<Run>,
    fill: Option<Fill>,
    /// Clicking it opens or closes the item's details.
    toggle: bool,
}

struct Laid {
    rows: Vec<Row>,
    h: f32,
}

pub struct ChatView {
    pub key: TaskKey,
    seq: u64,
    writer: UnixStream,
    /// Index of `items[0]` in the whole conversation.
    start: usize,
    items: Vec<ChatItem>,
    laid: Vec<Option<Laid>>,
    loaded: bool,
    /// An older page is on its way.
    loading: bool,
    width: f32,
    /// Items the user opened (true) or closed, by conversation index.
    open: HashMap<usize, bool>,
    /// Pixels from the top of what's loaded; None follows the newest.
    top: Option<f32>,
    /// How far `top` can go, at the last frame.
    max_top: f32,
    /// Items an older page just put in front, whose height `top` must skip.
    prepended: usize,
}

impl Drop for ChatView {
    fn drop(&mut self) {
        let _ = self.writer.shutdown(Shutdown::Both); // ends the reader thread too
    }
}

impl ChatView {
    fn is_open(&self, i: usize) -> bool {
        if let Some(&o) = self.open.get(&(self.start + i)) {
            return o;
        }
        match &self.items[i] {
            ChatItem::Tool { ok: Some(false), .. } => true,
            ChatItem::Tool { diff, .. } => !diff.is_empty() && diff.iter().filter(|l| l.kind != ' ').count() <= SMALL_DIFF,
            _ => false,
        }
    }

    fn toggle(&mut self, i: usize) {
        let open = !self.is_open(i);
        self.open.insert(self.start + i, open);
        self.laid[i] = None;
    }

    /// Wheel movement: `dy` pixels towards the top.
    pub fn scroll(&mut self, dy: f32) {
        let top = self.top.unwrap_or(self.max_top) - dy;
        self.top = (top < self.max_top).then_some(top.max(0.0));
    }
}

/// Greedy word wrap of styled runs into rows `line_h` high from `x0`, `w` wide. A word wider than
/// a line breaks between characters.
fn wrap_runs(r: &Renderer, runs: &[(String, usize, f32, u16, Ink)], x0: f32, w: f32, line_h: f32, fill: Option<Fill>) -> Vec<Row> {
    let mut rows = vec![Row { y: 0.0, h: line_h, runs: Vec::new(), fill, toggle: false }];
    let mut x = x0;
    for (text, font, size, weight, ink) in runs {
        let space = r.advance(*font, *size, *weight, ' ');
        for (i, word) in text.split(' ').enumerate() {
            if i > 0 {
                x += space;
            }
            let mut word = word.to_string();
            loop {
                let ww = r.measure(*font, *size, *weight, &word);
                if x + ww <= x0 + w || word.is_empty() {
                    break;
                }
                if x > x0 {
                    rows.push(Row { y: 0.0, h: line_h, runs: Vec::new(), fill, toggle: false });
                    x = x0;
                    continue;
                }
                // Alone on its line and still too wide: as much as fits, the rest on the next.
                let mut cut = 0;
                let mut cw = 0.0;
                for (j, ch) in word.char_indices() {
                    let a = r.advance(*font, *size, *weight, ch);
                    if cw + a > w && j > 0 {
                        break;
                    }
                    cw += a;
                    cut = j + ch.len_utf8();
                }
                let rest = word.split_off(cut);
                rows.last_mut().unwrap().runs.push(Run { x, text: word, font: *font, size: *size, weight: *weight, ink: *ink, max_w: w });
                rows.push(Row { y: 0.0, h: line_h, runs: Vec::new(), fill, toggle: false });
                (x, word) = (x0, rest);
            }
            if !word.is_empty() {
                let ww = r.measure(*font, *size, *weight, &word);
                rows.last_mut().unwrap().runs.push(Run { x, text: word, font: *font, size: *size, weight: *weight, ink: *ink, max_w: ww + 1.0 });
                x += ww;
            }
        }
    }
    rows
}

fn spacer(h: f32, fill: Option<Fill>) -> Row {
    Row { y: 0.0, h, runs: Vec::new(), fill, toggle: false }
}

fn one(x: f32, text: &str, font: usize, size: f32, weight: u16, ink: Ink, max_w: f32) -> Run {
    Run { x, text: text.into(), font, size, weight, ink, max_w }
}

/// An agent's markdown: paragraphs, headings, bullets, fenced code, and bold and `code` runs.
fn markdown(r: &Renderer, text: &str, w: f32) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut fenced = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            fenced = !fenced;
            rows.push(spacer(6.0, Some(Fill::Code)));
            continue;
        }
        if fenced {
            let runs = [(line.replace('\t', "    "), MONO, CODE, 400, Ink::Code)];
            rows.extend(wrap_runs(r, &runs, 12.0, w - 24.0, CODE_H, Some(Fill::Code)));
            continue;
        }
        if trimmed.is_empty() {
            rows.push(spacer(8.0, None));
            continue;
        }
        let heading = trimmed.starts_with('#');
        let ordered = trimmed.split_once(". ").filter(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        let (marker, body) = if heading {
            (None, trimmed.trim_start_matches('#').trim_start())
        } else if let Some(b) = ["- ", "* ", "+ "].iter().find_map(|m| trimmed.strip_prefix(m)) {
            (Some("•".to_string()), b)
        } else if let Some((n, b)) = ordered {
            (Some(format!("{n}.")), b)
        } else {
            (None, trimmed)
        };
        let lead = (line.len() - trimmed.len()) as f32 * 4.0;
        let indent = lead + if marker.is_some() { 20.0 } else { 0.0 };
        let runs: Vec<_> = review::spans(body)
            .into_iter()
            .map(|(s, bold, code)| {
                let ink = if code { Ink::Code } else { Ink::Text };
                (s, if code { MONO } else { UI }, BODY, if bold || heading { 600 } else { 400 }, ink)
            })
            .collect();
        let mut wrapped = wrap_runs(r, &runs, indent, w - indent, BODY_H, None);
        if let Some(m) = marker {
            wrapped[0].runs.insert(0, one(lead, &m, UI, BODY, 400, Ink::Dim, 20.0));
        }
        if heading {
            wrapped.last_mut().unwrap().h += 4.0;
        }
        rows.extend(wrapped);
    }
    rows
}

fn tool_verb(kind: ToolKind, name: &str) -> String {
    match kind {
        ToolKind::Edit => "Edit",
        ToolKind::Read => "Read",
        ToolKind::Bash => "Run",
        ToolKind::Search => "Search",
        ToolKind::Web => "Fetch",
        ToolKind::Other => name,
    }
    .to_string()
}

fn layout(r: &Renderer, item: &ChatItem, w: f32, open: bool) -> Laid {
    let mut rows = Vec::new();
    match item {
        ChatItem::User { text } => {
            rows.push(spacer(8.0, Some(Fill::User)));
            rows.push(Row { y: 0.0, h: 18.0, runs: vec![one(14.0, "YOU", UI, 10.5, 600, Ink::Dim, 60.0)], fill: Some(Fill::User), toggle: false });
            for para in text.lines() {
                rows.extend(wrap_runs(r, &[(para.to_string(), UI, BODY, 400, Ink::Text)], 14.0, w - 28.0, BODY_H, Some(Fill::User)));
            }
            rows.push(spacer(8.0, Some(Fill::User)));
        }
        ChatItem::Assistant { text } => rows = markdown(r, text, w),
        ChatItem::Thinking { text } => {
            let n = text.lines().filter(|l| !l.trim().is_empty()).count();
            let head = vec![
                one(0.0, if open { "▾" } else { "▸" }, UI, 12.0, 400, Ink::Dim, 14.0),
                one(18.0, "Thinking", UI, 12.0, 400, Ink::Dim, 100.0),
                one(104.0, &format!("{n} line{}", if n == 1 { "" } else { "s" }), UI, 12.0, 400, Ink::Faint, 120.0),
            ];
            rows.push(Row { y: 0.0, h: 22.0, runs: head, fill: None, toggle: true });
            if open {
                for para in text.lines() {
                    rows.extend(wrap_runs(r, &[(para.to_string(), UI, 12.0, 400, Ink::Dim)], 18.0, w - 18.0, 19.0, None));
                }
            }
        }
        ChatItem::Tool { name, kind, target, output, ok, diff, .. } => {
            let (mark, mark_ink) = match ok {
                Some(true) => ("✓", Ink::Add),
                Some(false) => ("✗", Ink::Bad),
                None => ("…", Ink::Dim),
            };
            let (adds, dels) = (diff.iter().filter(|l| l.kind == '+').count(), diff.iter().filter(|l| l.kind == '-').count());
            let lines = output.lines().count();
            let meta = if !diff.is_empty() {
                format!("+{adds} −{dels}")
            } else if ok.is_none() {
                "running".into()
            } else if lines > 0 {
                format!("{lines} line{}", if lines == 1 { "" } else { "s" })
            } else {
                String::new()
            };
            let more = !diff.is_empty() || !output.trim().is_empty();
            let mw = r.measure(UI, 11.5, 400, &meta);
            let target = if target.is_empty() { name.as_str() } else { target.as_str() };
            let mut head = vec![
                one(0.0, mark, UI, 12.5, 600, mark_ink, 14.0),
                one(20.0, &tool_verb(*kind, name), UI, 12.5, 500, Ink::Kind(*kind), 70.0),
                one(96.0, target, MONO, 12.5, 400, Ink::Text, (w - 96.0 - mw - 40.0).max(40.0)),
                one(w - mw - 22.0, &meta, UI, 11.5, 400, Ink::Dim, mw + 1.0),
            ];
            if more {
                head.push(one(w - 10.0, if open { "▾" } else { "▸" }, UI, 11.0, 400, Ink::Faint, 12.0));
            }
            rows.push(Row { y: 0.0, h: 24.0, runs: head, fill: None, toggle: more });
            if open && more {
                let x0 = 22.0;
                rows.push(spacer(4.0, None));
                rows.push(Row { y: 0.0, h: 6.0, runs: Vec::new(), fill: Some(Fill::Box), toggle: false });
                let body: Vec<(String, Ink, Fill)> = if !diff.is_empty() {
                    diff.iter()
                        .take(MAX_LINES)
                        .map(|l| {
                            let (ink, fill) = match l.kind {
                                '+' => (Ink::Add, Fill::Add),
                                '-' => (Ink::Del, Fill::Del),
                                _ => (Ink::Text2, Fill::Box),
                            };
                            (format!("{} {}", l.kind, l.text.trim_end_matches(['\n', '\r'])), ink, fill)
                        })
                        .collect()
                } else {
                    output.lines().take(MAX_LINES).map(|l| (l.to_string(), if *ok == Some(false) { Ink::Del } else { Ink::Text2 }, Fill::Box)).collect()
                };
                for (text, ink, fill) in body {
                    let mut wrapped = wrap_runs(r, &[(text.replace('\t', "    "), MONO, CODE, 400, ink)], x0 + 12.0, w - x0 - 24.0, CODE_H, Some(fill));
                    for row in &mut wrapped {
                        row.runs.iter_mut().for_each(|run| run.x = run.x.max(x0 + 12.0));
                    }
                    rows.extend(wrapped);
                }
                rows.push(Row { y: 0.0, h: 6.0, runs: Vec::new(), fill: Some(Fill::Box), toggle: false });
            }
        }
        ChatItem::Todos { items } => {
            rows.push(spacer(8.0, Some(Fill::Box)));
            rows.push(Row { y: 0.0, h: 18.0, runs: vec![one(12.0, "PLAN", UI, 10.5, 600, Ink::Dim, 60.0)], fill: Some(Fill::Box), toggle: false });
            for (step, state) in items {
                let (mark, ink) = match state.as_str() {
                    "completed" => ("✓", Ink::Dim),
                    "in_progress" => ("›", Ink::Text),
                    _ => ("·", Ink::Faint),
                };
                let mut wrapped = wrap_runs(r, &[(step.clone(), UI, 12.5, 400, ink)], 34.0, w - 46.0, 20.0, Some(Fill::Box));
                wrapped[0].runs.insert(0, one(12.0, mark, UI, 12.5, 600, ink, 16.0));
                rows.extend(wrapped);
            }
            rows.push(spacer(8.0, Some(Fill::Box)));
        }
        ChatItem::Notice { text, error } => {
            let (mark, ink) = if *error { ("!", Ink::Bad) } else { ("·", Ink::Dim) };
            let mut wrapped = wrap_runs(r, &[(text.clone(), UI, 12.0, 400, ink)], 16.0, w - 16.0, 19.0, None);
            wrapped[0].runs.insert(0, one(0.0, mark, UI, 12.0, 600, ink, 12.0));
            rows.extend(wrapped);
        }
    }
    let mut y = 0.0;
    for row in &mut rows {
        row.y = y;
        y += row.h;
    }
    Laid { rows, h: y }
}

fn ink(t: &Theme, ink: Ink) -> Rgba {
    let (add, _, del, _) = t.diff();
    match ink {
        Ink::Text => t.text,
        Ink::Text2 => t.text2,
        Ink::Dim => t.text3,
        Ink::Faint => t.faint,
        Ink::Code => t.tc,
        Ink::Add => add,
        Ink::Del | Ink::Bad => del,
        Ink::Kind(ToolKind::Edit) => t.needs,
        Ink::Kind(ToolKind::Bash) => accent("pi"),
        Ink::Kind(ToolKind::Other) => t.text2,
        Ink::Kind(_) => t.work,
    }
}

fn fill(t: &Theme, f: Fill) -> Rgba {
    let (_, add, _, del) = t.diff();
    match f {
        Fill::User => t.raised,
        Fill::Code | Fill::Box => t.bg,
        Fill::Add => add,
        Fill::Del => del,
    }
}

impl App {
    /// The selected task's chat should show: it runs headless and the Terminal tab is off.
    pub(super) fn chat_wanted(&self) -> Option<TaskKey> {
        self.current().filter(|t| t.headless && !self.shell).map(|t| (t.machine, t.id))
    }

    /// Subscribes to the selected task's chat, or drops the one no longer shown.
    pub(super) fn sync_chat(&mut self) {
        let want = self.chat_wanted();
        if self.chat.as_ref().map(|c| c.key) == want {
            return;
        }
        self.chat = None;
        let Some(key @ (m, id)) = want else { return };
        let Ok(mut sock) = connect_at(self.sock(m).as_deref()) else { return };
        if sock.write_all(&proto::frame(&Request::Chat { id, limit: PAGE })).is_err() {
            return;
        }
        let Ok(mut reader) = sock.try_clone() else { return };
        self.seq += 1;
        let (seq, p) = (self.seq, self.proxy.clone());
        thread::spawn(move || {
            while let Ok(reply @ (Reply::Chat { .. } | Reply::ChatPatch { .. })) = proto::read_msg(&mut reader) {
                if p.send_event(Msg::Chat(seq, reply)).is_err() {
                    return;
                }
            }
        });
        self.chat = Some(ChatView {
            key,
            seq,
            writer: sock,
            start: 0,
            items: Vec::new(),
            laid: Vec::new(),
            loaded: false,
            loading: false,
            width: 0.0,
            open: HashMap::new(),
            top: None,
            max_top: 0.0,
            prepended: 0,
        });
    }

    pub(super) fn on_chat(&mut self, seq: u64, reply: Reply) {
        let Some(c) = self.chat.as_mut().filter(|c| c.seq == seq) else { return };
        match reply {
            Reply::Chat { start, items, .. } if !c.loaded => {
                c.laid = items.iter().map(|_| None).collect();
                (c.start, c.items, c.loaded) = (start, items, true);
            }
            // An older page, just in front of what's here.
            Reply::Chat { start, mut items, .. } if start + items.len() == c.start => {
                c.loading = false;
                c.prepended = items.len();
                let mut laid: Vec<Option<Laid>> = items.iter().map(|_| None).collect();
                laid.append(&mut c.laid);
                items.append(&mut c.items);
                (c.start, c.items, c.laid) = (start, items, laid);
            }
            Reply::Chat { .. } => c.loading = false,
            Reply::ChatPatch { index, item } if index >= c.start && c.loaded => {
                let i = index - c.start;
                if i < c.items.len() {
                    (c.items[i], c.laid[i]) = (item, None);
                } else if i == c.items.len() {
                    c.items.push(item);
                    c.laid.push(None);
                }
            }
            _ => {}
        }
    }

    /// Sends the message box's text to the selected headless task (reviving it if it has ended).
    pub(super) fn send_chat(&mut self, text: Option<String>) {
        let Some(task) = self.current().cloned() else { return };
        let key = (task.machine, task.id);
        let text = match text {
            Some(text) => text,
            None => {
                let draft = self.drafts.remove(&key).unwrap_or_default();
                self.caret = None;
                self.anchor = None;
                draft
            }
        };
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        if !is_live(&task.state) {
            self.actions.push(Action::Revive(key, text));
            return;
        }
        if let Some(c) = &mut self.chat {
            c.top = None; // what you send, you see
        }
        if let Ok(Reply::Terminal { command }) = request_at(self.sock(task.machine).as_deref(), &Request::Prompt { id: task.id, text }) {
            // Only the agent's own terminal has that command: the Terminal tab, with it typed.
            match &mut self.session {
                Some(s) if s.shell && s.task == key => {
                    let _ = s.writer.write_all(&proto::frame(&Request::Input(command.into_bytes())));
                }
                _ => {
                    self.typed = Some((key, command));
                    self.actions.push(Action::Shell(true));
                }
            }
        }
    }

    pub(super) fn interrupt_chat(&mut self) {
        if let Some(task) = self.current().filter(|t| t.headless && matches!(group(&t.state), 0 | 1)) {
            let _ = request_at(self.sock(task.machine).as_deref(), &Request::Interrupt { id: task.id });
        }
    }

    /// The conversation, what's waiting on you and the message box, in `sec` under the header.
    pub(super) fn draw_chat(&mut self, r: &mut Renderer, t: &Theme, sec: Rect, task: &TaskInfo, ended: bool) {
        let key = (task.machine, task.id);
        let focused = self.focus == Focus::Message && self.dialog.is_none();

        // Bottom up: the message box (unless the task has ended: its Revive bar is there), then what
        // it asks, then what it's doing.
        let mut bottom = sec.y + sec.h;
        if !ended {
            bottom = self.draw_composer(r, t, Rect::new(sec.x, sec.y, sec.w, sec.h), task, focused);
        }
        let w = (sec.w - 2.0 * PAD).min(MAX_W);
        let x = sec.x + PAD;
        if task.asking {
            bottom = self.draw_ask(r, t, x, w, bottom, task);
        }
        if task.state == "running" {
            let cy = bottom - 16.0;
            self.icon(r, t, task, x + 6.0, cy);
            let line = if task.action.is_empty() { "working".to_string() } else { task.action.clone() };
            r.text(x + 24.0, cy - 8.0, &line, UI, 12.0, 400, t.text3, w - 24.0);
            bottom -= 30.0;
        }

        let area = Rect::new(sec.x, sec.y, sec.w, bottom - sec.y);
        let Some(c) = self.chat.as_mut().filter(|c| c.key == key) else { return };
        if !c.loaded {
            r.text(x, area.y + 18.0, "Loading the conversation…", UI, 12.0, 400, t.text3, w);
            return;
        }
        if c.width != w {
            c.width = w;
            c.laid.iter_mut().for_each(|l| *l = None);
        }
        for i in 0..c.items.len() {
            if c.laid[i].is_none() {
                let open = c.is_open(i);
                c.laid[i] = Some(layout(r, &c.items[i], w, open));
            }
        }
        let heights: Vec<f32> = c.laid.iter().map(|l| l.as_ref().map_or(0.0, |l| l.h)).collect();
        if c.prepended > 0 {
            let added: f32 = heights[..c.prepended].iter().map(|h| h + GAP).sum();
            c.top = c.top.map(|top| top + added);
            c.prepended = 0;
        }
        let total = 18.0 + heights.iter().sum::<f32>() + GAP * heights.len().saturating_sub(1) as f32 + 18.0;
        c.max_top = (total - area.h).max(0.0);
        let top = c.top.map_or(c.max_top, |y| y.min(c.max_top));
        if top <= 0.0 && c.start > 0 && !c.loading {
            c.loading = true;
            let (sock, p, seq, end) = (self.hosts[key.0].sock(), self.proxy.clone(), c.seq, c.start);
            thread::spawn(move || {
                if let Ok(reply) = request_at(sock.as_deref(), &Request::ChatPage { id: key.1, end, limit: PAGE }) {
                    let _ = p.send_event(Msg::Chat(seq, reply));
                }
            });
        }

        let old_clip = r.clip();
        r.set_clip(Some(area));
        let mut toggled = None;
        let mut y = area.y + 18.0 - top;
        if c.start > 0 && y > area.y {
            r.text(x, y - 16.0, "Loading earlier messages…", UI, 11.5, 400, t.faint, w);
        }
        for (i, h) in heights.iter().enumerate() {
            if y > area.y + area.h {
                break;
            }
            if y + h >= area.y {
                let laid = c.laid[i].as_ref().unwrap();
                for row in &laid.rows {
                    let ry = y + row.y;
                    if ry + row.h < area.y || ry > area.y + area.h {
                        continue;
                    }
                    let band = Rect::new(x, ry, w, row.h);
                    if let Some(f) = row.fill {
                        r.rect(band, 0.0, fill(t, f));
                    }
                    if row.toggle {
                        r.target(band, Cursor::Pointer);
                        if self.click.is_some_and(|p| band.contains(p) && area.contains(p)) {
                            toggled = Some(i);
                        }
                    }
                    for run in &row.runs {
                        let ty = r.center(Rect::new(0.0, ry, 0.0, row.h), run.font, run.size);
                        r.text(x + run.x, ty, &run.text, run.font, run.size, run.weight, ink(t, run.ink), run.max_w);
                    }
                }
            }
            y += h + GAP;
        }
        r.set_clip(old_clip);
        if let Some(i) = toggled {
            c.toggle(i);
            self.click = None;
            self.redraw();
        }
    }

    /// What the agent waits on, pinned above the message box: Allow/Deny, or a question's options
    /// (or type an answer). Returns the y above it.
    fn draw_ask(&mut self, r: &mut Renderer, t: &Theme, x: f32, w: f32, bottom: f32, task: &TaskInfo) -> f32 {
        let key = (task.machine, task.id);
        let (label, text, options): (&str, String, Vec<String>) = match &task.need {
            // Its detail is the action line ("running cargo test"); asked, it reads "run cargo test".
            Some(Event::PermissionRequest { tool, detail }) => {
                let verbs = [("running ", "run "), ("editing ", "edit "), ("reading ", "read "), ("searching ", "search "), ("fetching ", "fetch "), ("using ", "use ")];
                let ask = verbs.iter().find_map(|(ing, verb)| detail.strip_prefix(ing).map(|rest| format!("{verb}{rest}")));
                ("Wants to", ask.unwrap_or_else(|| if detail.is_empty() { format!("use {tool}") } else { detail.clone() }), Vec::new())
            }
            Some(Event::Question { text, options }) => ("Asks", text.clone(), options.clone()),
            _ => return bottom,
        };
        let question = matches!(task.need, Some(Event::Question { .. }));
        let lines = wrap(r, &text, UI, 12.5, w - 120.0);
        let h = (20.0 + lines.len() as f32 * 19.0).max(42.0) + if question { 40.0 } else { 0.0 };
        let card = Rect::new(x, bottom - h - 12.0, w, h);
        r.frame(card, 0.0, t.needs_bg, t.needs_line);
        r.text(card.x + 14.0, card.y + 10.0, label, UI, 12.0, 600, t.needs, 80.0);
        for (i, line) in lines.iter().enumerate() {
            r.text(card.x + 90.0, card.y + 10.0 + i as f32 * 19.0, line, UI, 12.5, 400, t.text, w - 220.0);
        }
        if question {
            // Its options as buttons; anything else is typed in the message box.
            let mut bx = card.x + 90.0;
            let by = card.y + h - 36.0;
            for o in &options {
                let bw = r.measure(MONO, 12.0, 400, o) + 24.0;
                if bx + bw > card.x + w - 14.0 {
                    break;
                }
                let b = Rect::new(bx, by, bw, 26.0);
                r.frame(b, 0.0, t.bg, t.border2);
                r.text(b.x + 12.0, r.center(b, MONO, 12.0), o, MONO, 12.0, 400, t.text, bw);
                if self.hit(r, b) {
                    self.send_chat(Some(o.clone()));
                }
                bx += bw + 8.0;
            }
            let hint = "or type an answer below";
            r.text(bx + 4.0, r.center(Rect::new(0.0, by, 0.0, 26.0), UI, 11.5), hint, UI, 11.5, 400, t.text3, (card.x + w - bx - 8.0).max(0.0));
        } else {
            let allow = Rect::new(card.x + w - 110.0, card.y + 8.0, 98.0, 26.0);
            let deny = Rect::new(allow.x - 70.0, allow.y, 62.0, 26.0);
            r.frame(deny, 0.0, t.needs_bg, t.border2);
            r.text(deny.x + 14.0, r.center(deny, UI, 12.0), "Deny", UI, 12.0, 500, t.text, 60.0);
            r.rect(allow, 0.0, t.needs);
            r.text(allow.x + 12.0, r.center(allow, UI, 12.0), "Allow once", UI, 12.0, 600, t.on_accent, 90.0);
            if self.hit(r, deny) {
                self.actions.push(Action::Answer(key, false));
            }
            if self.hit(r, allow) {
                self.actions.push(Action::Answer(key, true));
            }
        }
        card.y - 4.0
    }

    /// The message box at the bottom of `sec`; returns the y above it.
    fn draw_composer(&mut self, r: &mut Renderer, t: &Theme, sec: Rect, task: &TaskInfo, focused: bool) -> f32 {
        let key = (task.machine, task.id);
        let draft = self.drafts.get(&key).cloned().unwrap_or_default();
        let send_w = 76.0;
        let text_w = sec.w - 32.0 - 24.0 - send_w - 8.0;
        let lines = wrap(r, &draft, UI, BODY, text_w);
        let starts = line_starts(&draft, &lines);
        let caret = self.caret_for(Focus::Message, &draft).map(|at| caret_line_at(&lines, &starts, at));
        let shown = lines.len().clamp(2, COMPOSER_LINES);
        let first = caret.map_or(lines.len().saturating_sub(shown), |(line, _)| line.saturating_sub(shown - 1)).min(lines.len().saturating_sub(shown));
        let box_h = shown as f32 * 20.0 + 18.0;
        let foot = 26.0;
        let outer = Rect::new(sec.x, sec.y + sec.h - box_h - foot - 12.0, sec.w, box_h + foot + 12.0);
        r.rect(outer, 0.0, t.bg);
        r.rect(Rect::new(outer.x, outer.y, outer.w, 1.0), 0.0, t.border);
        let b = Rect::new(sec.x + 16.0, outer.y + 10.0, sec.w - 32.0, box_h);
        r.frame(b, 0.0, t.term, if focused { t.work } else { t.border2 });
        if self.hit_as(r, Rect::new(b.x, b.y, b.w - send_w, b.h), Cursor::Text) {
            self.actions.push(Action::Focus(Focus::Message));
        }
        let tx = b.x + 12.0;
        if draft.is_empty() {
            let hint = format!("Message {}", adapter::kind(&task.agent));
            r.text(tx, b.y + 9.0, &hint, UI, BODY, 400, t.faint, text_w);
            if focused && self.caret_on {
                r.rect(Rect::new(tx, b.y + 9.0 - BODY * 0.15, 1.5, BODY * 1.35), 0.0, t.text);
            }
        } else {
            for (i, line) in lines.iter().enumerate().skip(first).take(shown) {
                let y = b.y + 9.0 + (i - first) as f32 * 20.0;
                let at = caret.filter(|c| c.0 == i).map(|c| c.1);
                self.field_text(r, t, tx, y, line, UI, BODY, text_w, (Focus::Message, starts[i]), focused, at, t.text);
            }
        }
        let send = Rect::new(b.x + b.w - send_w - 6.0, b.y + b.h - 34.0, send_w, 28.0);
        let ready = !draft.trim().is_empty();
        r.rect(send, 0.0, if ready { t.primary_bg } else { t.raised });
        r.text(send.x + 12.0, r.center(send, UI, 12.0), "Send ↵", UI, 12.0, 600, if ready { t.primary_fg } else { t.text3 }, send_w);
        if ready && self.hit(r, send) {
            self.actions.push(Action::SendChat);
        }
        let mut hx = b.x + 2.0;
        let fy = b.y + b.h + 7.0;
        let esc = if self.vim { "Esc Esc" } else { "Esc" };
        for (k, what) in [("↵", "send"), ("⇧↵", "new line"), (esc, "interrupt")] {
            hx += r.text(hx, fy, k, UI, 11.0, 500, t.text3, 80.0) + 6.0;
            hx += r.text(hx, fy, what, UI, 11.0, 400, t.faint, 120.0) + 18.0;
        }
        outer.y
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_edits_and_failures_open_by_themselves() {
        let line = |kind| arranger::proto::DiffLine { kind, old: 0, new: 0, text: "x\n".into() };
        let tool = |ok, diff| ChatItem::Tool { id: "t".into(), name: "Edit".into(), kind: ToolKind::Edit, target: "a".into(), input: String::new(), output: "out".into(), ok, diff };
        let (sock, _) = UnixStream::pair().unwrap();
        let mut c = ChatView { key: (0, 1), seq: 0, writer: sock, start: 10, items: vec![tool(Some(true), vec![line('+')]), tool(Some(false), vec![]), tool(Some(true), vec![line('+'); SMALL_DIFF + 1])], laid: vec![None, None, None], loaded: true, loading: false, width: 0.0, open: HashMap::new(), top: None, max_top: 0.0, prepended: 0 };
        assert_eq!((c.is_open(0), c.is_open(1), c.is_open(2)), (true, true, false));
        c.toggle(0);
        assert!(!c.is_open(0) && c.open.get(&10) == Some(&false), "kept by conversation index");
        c.max_top = 500.0;
        c.scroll(100.0);
        assert_eq!(c.top, Some(400.0));
        c.scroll(-200.0);
        assert_eq!(c.top, None, "back at the bottom it follows again");
    }
}

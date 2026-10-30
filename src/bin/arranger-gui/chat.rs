//! A headless task's chat (docs: plan phase 4, the "Headless chat" artboard): its conversation
//! from the daemon, drawn from a layout cached per item, so a streaming reply re-lays out only
//! the item it grows and a long thread draws only the rows on screen. Older items load a page at
//! a time as you scroll up.
use super::*;
use arranger::headless;
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
/// Lines of the message box before it scrolls.
const COMPOSER_LINES: usize = 8;
/// Height of a read window's always-visible header bar.
const READ_BAR_H: f32 = 22.0;
/// Largest scrollable viewport a run of back-to-back reads gets; the rest scrolls inside.
const READ_VIEW_MAX: f32 = 320.0;
/// Scrollbar gutter on the right of a read window.
const READ_BAR_W: f32 = 8.0;
/// Height of the agent's header over each of its turns.
const TURN_H: f32 = 28.0;
/// Between tool cards back to back, tighter than `GAP` so a turn's steps read as one group.
const STEP_GAP: f32 = 6.0;
/// Room a chip of inline code takes at each side of its text.
const CHIP_PAD: f32 = 4.0;
/// Width of the coloured stripe at the left of a card.
const STRIPE: f32 = 2.0;

#[derive(Default)]
struct Slash {
    query: String,
    selected: usize,
    dismissed: bool,
}

fn slash_query(text: &str, at: usize) -> Option<&str> {
    let query = text.strip_prefix('/')?;
    (at == text.len() && !query.chars().any(char::is_whitespace) && !query.contains('/')).then_some(query)
}

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
    /// Borders: a code block's stripe.
    Line,
    /// The user's own stripe.
    Primary,
    Kind(ToolKind),
}

#[derive(Clone, Copy, PartialEq)]
enum Fill {
    User,
    Code,
    Add,
    Del,
    Box,
    /// A tool's output, sunk into its card.
    Well,
    /// A horizontal rule across the row's middle.
    Rule,
    /// A plan's progress bar, this far done (0 to 1).
    Progress(f32),
}

struct Run {
    x: f32,
    text: String,
    font: usize,
    size: f32,
    weight: u16,
    ink: Ink,
    max_w: f32,
    /// Inline code: drawn on a chip.
    chip: bool,
}

struct Row {
    y: f32,
    h: f32,
    runs: Vec<Run>,
    fill: Option<Fill>,
    /// Clicking it opens or closes the item's details.
    toggle: bool,
    /// A stripe down the left edge, in this ink.
    bar: Option<Ink>,
}

struct Laid {
    rows: Vec<Row>,
    h: f32,
}

/// One item drawn on its own, a run of consecutive reads drawn as one scrollable window, or the
/// agent's header over the turn that starts at an item.
#[derive(Clone, Copy)]
enum View {
    Item(usize),
    Reads(usize, usize),
    Turn(usize),
}

/// The scrollable content of a run of back-to-back reads, in window-local coordinates.
struct ReadWin {
    rows: Vec<Row>,
    content_h: f32,
}

/// Where a read window was drawn, for wheel scrolling that hits it rather than the thread.
#[derive(Clone, Copy)]
struct ReadHit {
    /// Conversation index of the first read in the run.
    key: usize,
    /// Whole window, header bar included.
    rect: Rect,
    content_h: f32,
    viewport_h: f32,
}

pub struct ChatView {
    hist: Option<usize>,
    selection: Option<((usize, usize, usize), (usize, usize, usize))>,
    slash: Slash,
    pub key: TaskKey,
    seq: u64,
    writer: UnixStream,
    /// Index of `items[0]` in the whole conversation.
    start: usize,
    items: Vec<ChatItem>,
    laid: Vec<Option<Laid>>,
    /// Cached layout of a run of reads, at the run's first item; `None` elsewhere.
    read_win: Vec<Option<ReadWin>>,
    /// Internal scroll per read window by conversation index of the run's first item;
    /// `None` follows the newest (the bottom).
    read_scroll: HashMap<usize, Option<f32>>,
    /// Read windows drawn in the last frame, for the wheel to scroll the one under the mouse.
    read_bounds: Vec<ReadHit>,
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
    pub(super) fn recall_prompt(&mut self, text: &mut String, up: bool) -> bool {
        let prompts: Vec<String> = self.items.iter().filter_map(|item| match item {
            ChatItem::User { text } => Some(text.clone()),
            _ => None,
        }).collect();
        recall(&prompts, &mut self.hist, text, up)
    }

    fn is_open(&self, i: usize) -> bool {
        self.open.get(&(self.start + i)).copied().unwrap_or(false)
    }

    fn toggle(&mut self, i: usize) {
        // Keep the clicked header in place when its details change the content height.
        // Following the newest item here would jump to the end of the opened output.
        self.top = Some(self.top.unwrap_or(self.max_top).min(self.max_top));
        let open = !self.is_open(i);
        self.open.insert(self.start + i, open);
        self.laid[i] = None;
    }

    /// Wheel movement: `dy` pixels towards the top.
    pub fn scroll(&mut self, dy: f32) {
        let top = self.top.unwrap_or(self.max_top) - dy;
        self.top = (top < self.max_top).then_some(top.max(0.0));
    }

    /// A read item changed: drop the cached window of the run it belongs to, and the stale
    /// single-item layouts of its reads.
    fn invalidate_read(&mut self, i: usize) {
        let mut s = i;
        while s > 0 && matches!(self.items[s - 1], ChatItem::Tool { kind: ToolKind::Read, .. }) {
            s -= 1;
        }
        let mut k = s;
        while k < self.items.len() && matches!(self.items[k], ChatItem::Tool { kind: ToolKind::Read, .. }) {
            if let Some(laid) = self.laid.get_mut(k) {
                *laid = None;
            }
            k += 1;
        }
        if let Some(win) = self.read_win.get_mut(s) {
            *win = None;
        }
    }

    /// Scrolls the read window under the mouse; false means the wheel moves the thread instead.
    pub fn scroll_read(&mut self, mouse: (f32, f32), dy: f32) -> bool {
        let Some(hit) = self.read_bounds.iter().find(|h| h.rect.contains(mouse)).copied() else {
            return false;
        };
        let max = (hit.content_h - hit.viewport_h).max(0.0);
        if max <= 0.0 {
            return false; // nothing to scroll inside: let the wheel move the thread
        }
        let cur = self.read_scroll.get(&hit.key).copied().flatten().unwrap_or(max);
        let next = cur - dy; // dy is towards the top, like the thread's own scroll.
        self.read_scroll.insert(hit.key, (next < max - 0.5).then_some(next.max(0.0)));
        true
    }
}

/// Greedy word wrap of styled runs into rows `line_h` high from `x0`, `w` wide. A word wider than
/// a line breaks between characters. Words of one run that land on one row stay one run, so an
/// inline code chip covers its spaces too. Inline code (code ink on an unfilled row) goes on a chip,
/// with room kept clear on both sides of it.
fn wrap_runs(r: &Renderer, runs: &[(String, usize, f32, u16, Ink)], x0: f32, w: f32, line_h: f32, fill: Option<Fill>) -> Vec<Row> {
    let mut rows = vec![Row { y: 0.0, h: line_h, runs: Vec::new(), fill, toggle: false, bar: None }];
    let mut x = x0;
    // The run (by index into `runs`) and word the current row's last piece came from.
    let mut from: Option<(usize, usize)> = None;
    for (k, (text, font, size, weight, ink)) in runs.iter().enumerate() {
        let space = r.advance(*font, *size, *weight, ' ');
        let chip = *ink == Ink::Code && fill.is_none();
        // A new piece of this run steps in past a chip's edge: its own, or the one before it.
        let edge = |row: &Row, from: Option<(usize, usize)>| {
            if from.is_some_and(|(fk, _)| fk == k) {
                return 0.0;
            }
            let mut pad = if chip { CHIP_PAD } else { 0.0 };
            if row.runs.last().is_some_and(|run| run.chip) {
                pad += CHIP_PAD;
            }
            pad
        };
        for (i, word) in text.split(' ').enumerate() {
            if i > 0 {
                x += space;
            }
            let mut word = word.to_string();
            loop {
                let pad = edge(rows.last().unwrap(), from);
                let ww = r.measure(*font, *size, *weight, &word);
                if x + pad + ww <= x0 + w || word.is_empty() {
                    break;
                }
                if x > x0 {
                    rows.push(Row { y: 0.0, h: line_h, runs: Vec::new(), fill, toggle: false, bar: None });
                    (x, from) = (x0, None);
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
                x += pad;
                rows.last_mut().unwrap().runs.push(Run { x, text: word, font: *font, size: *size, weight: *weight, ink: *ink, max_w: w, chip });
                rows.push(Row { y: 0.0, h: line_h, runs: Vec::new(), fill, toggle: false, bar: None });
                (x, word, from) = (x0, rest, None);
            }
            if !word.is_empty() {
                let ww = r.measure(*font, *size, *weight, &word);
                x += edge(rows.last().unwrap(), from);
                let row = rows.last_mut().unwrap();
                match (row.runs.last_mut(), from) {
                    (Some(prev), Some((fk, fi))) if fk == k => {
                        // As many spaces as the layout advanced past.
                        prev.text.push_str(&" ".repeat(i - fi));
                        prev.text.push_str(&word);
                        prev.max_w = x + ww - prev.x + 1.0;
                    }
                    _ => row.runs.push(Run { x, text: word, font: *font, size: *size, weight: *weight, ink: *ink, max_w: ww + 1.0, chip }),
                }
                from = Some((k, i));
                x += ww;
            }
        }
    }
    rows
}

fn spacer(h: f32, fill: Option<Fill>) -> Row {
    Row { y: 0.0, h, runs: Vec::new(), fill, toggle: false, bar: None }
}

fn one(x: f32, text: &str, font: usize, size: f32, weight: u16, ink: Ink, max_w: f32) -> Run {
    Run { x, text: text.into(), font, size, weight, ink, max_w, chip: false }
}

/// An agent's markdown: paragraphs, headings, bullets, quotes, rules, fenced code, and bold and
/// `code` runs.
fn markdown(r: &Renderer, text: &str, w: f32) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    let mut fenced = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if let Some(lang) = trimmed.strip_prefix("```") {
            fenced = !fenced;
            let lang = lang.trim();
            // The opening fence names the block's language, if it has one.
            let mut row = spacer(if fenced && !lang.is_empty() { 24.0 } else { 8.0 }, Some(Fill::Code));
            if fenced && !lang.is_empty() {
                row.runs.push(one(14.0, lang, UI, 10.5, 600, Ink::Faint, w - 28.0));
            }
            row.bar = Some(Ink::Line);
            rows.push(row);
            continue;
        }
        if fenced {
            let runs = [(line.replace('\t', "    "), MONO, CODE, 400, Ink::Code)];
            let mut wrapped = wrap_runs(r, &runs, 14.0, w - 28.0, CODE_H, Some(Fill::Code));
            wrapped.iter_mut().for_each(|row| row.bar = Some(Ink::Line));
            rows.extend(wrapped);
            continue;
        }
        if trimmed.is_empty() {
            rows.push(spacer(8.0, None));
            continue;
        }
        if image_path(trimmed).is_some() {
            rows.push(Row { y: 0.0, h: 26.0, runs: vec![one(0.0, trimmed, UI, BODY, 400, Ink::Text, w)], fill: Some(Fill::Box), toggle: false, bar: None });
            continue;
        }
        if trimmed.trim_end().len() >= 3 && ['-', '*', '_'].iter().any(|&c| trimmed.trim_end().chars().all(|ch| ch == c)) {
            rows.push(spacer(17.0, Some(Fill::Rule)));
            continue;
        }
        let level = trimmed.chars().take_while(|&c| c == '#').count();
        let heading = level > 0 && trimmed[level..].starts_with(' ');
        let quote = trimmed.strip_prefix('>').map(str::trim_start);
        let ordered = trimmed.split_once(". ").filter(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        let (marker, body) = if heading {
            (None, trimmed[level..].trim_start())
        } else if let Some(q) = quote {
            (None, q)
        } else if let Some(b) = ["- ", "* ", "+ "].iter().find_map(|m| trimmed.strip_prefix(m)) {
            (Some("•".to_string()), b)
        } else if let Some((n, b)) = ordered {
            (Some(format!("{n}.")), b)
        } else {
            (None, trimmed)
        };
        let (size, line_h) = match (heading, level) {
            (true, 1) => (18.0, 28.0),
            (true, 2) => (16.0, 25.0),
            (true, _) => (14.0, 22.0),
            _ => (BODY, BODY_H),
        };
        let lead = (line.len() - trimmed.len()) as f32 * 4.0 + if quote.is_some() { 14.0 } else { 0.0 };
        let indent = lead + if marker.is_some() { 20.0 } else { 0.0 };
        let base = if quote.is_some() { Ink::Text2 } else { Ink::Text };
        let runs: Vec<_> = review::spans(body)
            .into_iter()
            .map(|(s, bold, code)| {
                let ink = if code { Ink::Code } else { base };
                let size = if code { size - 1.0 } else { size };
                (s, if code { MONO } else { UI }, size, if bold || heading { 600 } else { 400 }, ink)
            })
            .collect();
        let mut wrapped = wrap_runs(r, &runs, indent, w - indent, line_h, None);
        if quote.is_some() {
            wrapped.iter_mut().for_each(|row| row.bar = Some(Ink::Dim));
        }
        if let Some(m) = marker {
            wrapped[0].runs.insert(0, one(lead, &m, UI, BODY, 400, Ink::Dim, 20.0));
        }
        if heading {
            // Room above a heading that follows text, and a little below it.
            if rows.last().is_some_and(|row| !row.runs.is_empty()) {
                rows.push(spacer(8.0, None));
            }
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

/// Consecutive reads become one scrollable window; anything else, and a lone read, stays as-is.
/// Each turn of the agent's opens with its header. `at_start`: `items[0]` begins the conversation.
fn views(items: &[ChatItem], at_start: bool) -> Vec<View> {
    let mut views = Vec::new();
    let mut i = 0;
    while i < items.len() {
        if turn_starts(items, i, at_start) {
            views.push(View::Turn(i));
        }
        if matches!(items[i], ChatItem::Tool { kind: ToolKind::Read, .. }) {
            let mut j = i;
            while j < items.len() && matches!(items[j], ChatItem::Tool { kind: ToolKind::Read, .. }) {
                j += 1;
            }
            if j - i >= 2 {
                views.push(View::Reads(i, j - i));
                i = j;
                continue;
            }
        }
        views.push(View::Item(i));
        i += 1;
    }
    views
}

fn read_window_h(content_h: f32) -> f32 {
    READ_BAR_H + content_h.min(READ_VIEW_MAX)
}

/// Height the items an older page just put in front add above the old viewport. A read run that
/// the join merged contributes only its growth, not its whole window again.
fn prepended_added(r: &Renderer, c: &ChatView, views: &[View], heights: &[f32], gaps: &[f32], prepended: usize, w: f32, local: bool) -> f32 {
    let mut added = 0.0;
    for (v, view) in views.iter().enumerate() {
        match *view {
            View::Item(i) => {
                if i < prepended {
                    added += heights[v] + gaps[v];
                }
            }
            // A header at the join is new too: the item before it only just arrived.
            View::Turn(i) => {
                if i <= prepended {
                    added += heights[v] + gaps[v];
                }
            }
            View::Reads(start, len) => {
                let end = start + len;
                if end <= prepended {
                    added += heights[v] + gaps[v];
                } else if start < prepended {
                    // The run used to begin at `prepended`; the prepended reads only grew it.
                    let old = &c.items[prepended..end];
                    let old_h = if old.len() >= 2 {
                        read_window_h(read_win_layout(r, old, w).content_h)
                    } else {
                        layout(r, &old[0], w, c.is_open(prepended), local).h
                    };
                    added += heights[v] - old_h;
                }
            }
        }
    }
    added
}

fn intersect(a: Rect, b: Rect) -> Rect {
    let x = a.x.max(b.x);
    let y = a.y.max(b.y);
    let r = (a.x + a.w).min(b.x + b.w);
    let btm = (a.y + a.h).min(b.y + b.h);
    Rect::new(x, y, (r - x).max(0.0), (btm - y).max(0.0))
}

fn draw_row(r: &mut Renderer, t: &Theme, x: f32, ry: f32, w: f32, row: &Row) {
    paint_row(r, t, x, ry, w, row);
    for run in &row.runs {
        let ty = r.center(Rect::new(0.0, ry, 0.0, row.h), run.font, run.size);
        r.text(x + run.x, ty, &run.text, run.font, run.size, run.weight, ink(t, run.ink), run.max_w);
    }
}

/// The lines a Read covered: `(first, count)`. `input` is the tool's args JSON; claude and pi
/// carry `offset`/`limit` there, other agents don't, so we fall back on the output's own lines.
fn read_range(input: &str, output: &str) -> (u32, u32) {
    let parsed: serde_json::Value = serde_json::from_str(input).unwrap_or(serde_json::Value::Null);
    let offset = ["offset", "start_line", "first_line"].iter().find_map(|k| parsed.get(*k).and_then(serde_json::Value::as_u64)).unwrap_or(0);
    let limit = ["limit", "lines", "count"].iter().find_map(|k| parsed.get(*k).and_then(serde_json::Value::as_u64));
    let n = limit.unwrap_or_else(|| output.lines().count() as u64).max(1);
    (offset as u32 + 1, n as u32)
}

/// A collapsed read's summary line: "read lines x to y (n lines)".
fn read_summary(input: &str, output: &str, ok: Option<bool>) -> String {
    if ok.is_none() {
        return "reading…".into();
    }
    if ok == Some(false) {
        return "read failed".into();
    }
    let (first, n) = read_range(input, output);
    let last = first + n - 1;
    if n == 1 {
        format!("read line {first}")
    } else {
        format!("read lines {first} to {last} ({n} lines)")
    }
}

/// The content of one run of reads: a collapsed summary per read, latest below the earlier ones.
fn read_win_layout(r: &Renderer, items: &[ChatItem], w: f32) -> ReadWin {
    let mut rows = Vec::new();
    let x0 = 12.0;
    let right = 12.0 + READ_BAR_W; // right margin plus the scrollbar gutter
    for item in items {
        let ChatItem::Tool { name, target, input, output, ok, .. } = item else { continue };
        let target = if target.is_empty() { name.as_str() } else { target.as_str() };
        let target_w = r.measure(MONO, 12.5, 400, target);
        rows.push(Row {
            y: 0.0,
            h: 22.0,
            runs: vec![
                one(12.0, "Read", UI, 12.5, 500, Ink::Kind(ToolKind::Read), 40.0),
                one(52.0, target, MONO, 12.5, 400, Ink::Text, (w - 52.0 - right - 20.0).max(target_w.min(w - 52.0 - right))),
            ],
            fill: None,
            toggle: false,
            bar: None,
        });
        let summary = read_summary(input, output, *ok);
        let ink = if *ok == Some(false) { Ink::Del } else { Ink::Dim };
        rows.push(Row {
            y: 0.0,
            h: CODE_H,
            runs: vec![one(x0 + 8.0, &summary, UI, CODE, 400, ink, (w - x0 - right - 8.0).max(40.0))],
            fill: Some(Fill::Box),
            toggle: false,
            bar: None,
        });
        rows.push(spacer(8.0, Some(Fill::Box)));
    }
    let mut y = 0.0;
    for row in &mut rows {
        row.y = y;
        y += row.h;
    }
    ReadWin { rows, content_h: y }
}

fn layout(r: &Renderer, item: &ChatItem, w: f32, open: bool, local: bool) -> Laid {
    let mut rows = Vec::new();
    match item {
        ChatItem::User { text } => {
            rows.push(spacer(10.0, Some(Fill::User)));
            rows.push(Row { y: 0.0, h: 20.0, runs: vec![one(16.0, "You", UI, 11.5, 600, Ink::Dim, 60.0)], fill: Some(Fill::User), toggle: false, bar: None });
            for para in text.lines() {
                if image_path(para).is_some() {
                    rows.push(Row { y: 0.0, h: 26.0, runs: vec![one(16.0, para, UI, BODY, 400, Ink::Text, w - 32.0)], fill: Some(Fill::User), toggle: false, bar: None });
                } else {
                    rows.extend(wrap_runs(r, &[(para.to_string(), UI, BODY, 400, Ink::Text)], 16.0, w - 32.0, BODY_H, Some(Fill::User)));
                }
            }
            rows.push(spacer(10.0, Some(Fill::User)));
            rows.iter_mut().for_each(|row| row.bar = Some(Ink::Primary));
        }
        ChatItem::Assistant { text } => rows = markdown(r, text, w),
        ChatItem::Thinking { text } => {
            let n = text.lines().filter(|l| !l.trim().is_empty()).count();
            let count = format!("{n} line{}", if n == 1 { "" } else { "s" });
            let cw = r.measure(UI, 11.5, 400, &count);
            let tw = r.measure(UI, 12.0, 500, "Thinking");
            let mut head = vec![
                one(14.0, if open { "▾" } else { "▸" }, UI, 11.0, 400, Ink::Dim, 14.0),
                one(30.0, "Thinking", UI, 12.0, 500, Ink::Dim, tw + 1.0),
                one(w - cw - 14.0, &count, UI, 11.5, 400, Ink::Faint, cw + 1.0),
            ];
            // Closed, its first line, so a glance says what it was about.
            if let Some(first) = text.lines().map(str::trim).find(|l| !l.is_empty()).filter(|_| !open) {
                let px = 30.0 + tw + 12.0;
                head.insert(2, one(px, first, UI, 12.0, 400, Ink::Faint, (w - cw - 28.0 - px).max(0.0)));
            }
            rows.push(Row { y: 0.0, h: 28.0, runs: head, fill: None, toggle: true, bar: Some(Ink::Faint) });
            if open {
                for para in text.lines() {
                    rows.extend(wrap_runs(r, &[(para.to_string(), UI, 12.0, 400, Ink::Dim)], 30.0, w - 44.0, 19.0, None));
                }
                rows.push(spacer(8.0, None));
                rows.iter_mut().for_each(|row| row.bar = Some(Ink::Faint));
            }
        }
        ChatItem::Tool { name, kind, target, output, ok, diff, .. } => {
            let (mark, mark_ink) = match ok {
                Some(true) => ("✓", Ink::Add),
                Some(false) => ("✗", Ink::Bad),
                None => ("…", Ink::Dim),
            };
            let stripe = if *ok == Some(false) { Ink::Bad } else { Ink::Kind(*kind) };
            let (adds, dels) = (diff.iter().filter(|l| l.kind == '+').count(), diff.iter().filter(|l| l.kind == '-').count());
            let lines = output.lines().count();
            // At the right: what an edit changed, or how much came back.
            let meta: Vec<(String, Ink)> = if !diff.is_empty() {
                vec![(format!("+{adds}"), Ink::Add), (format!("−{dels}"), Ink::Del)]
            } else if ok.is_none() {
                vec![("running".into(), Ink::Dim)]
            } else if lines > 0 {
                vec![(format!("{lines} line{}", if lines == 1 { "" } else { "s" }), Ink::Dim)]
            } else {
                Vec::new()
            };
            let more = !diff.is_empty() || !output.trim().is_empty();
            let target = if target.is_empty() { name.as_str() } else { target.as_str() };
            // A long tool name (an MCP tool's) pushes the target over, up to a third of the line.
            let verb = tool_verb(*kind, name);
            let vw = r.measure(UI, 12.5, 600, &verb).min(w / 3.0);
            let tx = (34.0 + vw + 12.0).max(100.0);
            let right = w - if more { 32.0 } else { 14.0 };
            let mut mx = right;
            let mut meta_runs = Vec::new();
            for (text, ink) in meta.iter().rev() {
                let mw = r.measure(UI, 11.5, 500, text);
                mx -= mw;
                meta_runs.insert(0, one(mx, text, UI, 11.5, 500, *ink, mw + 1.0));
                mx -= 6.0;
            }
            let mut head = vec![
                one(14.0, mark, UI, 12.5, 600, mark_ink, 14.0),
                one(34.0, &verb, UI, 12.5, 600, Ink::Kind(*kind), vw + 1.0),
                one(tx, target, MONO, 12.5, 400, Ink::Text, (mx - tx - 12.0).max(40.0)),
            ];
            head.extend(meta_runs);
            if more {
                head.push(one(w - 22.0, if open { "▾" } else { "▸" }, UI, 11.0, 400, Ink::Dim, 12.0));
            }
            rows.push(Row { y: 0.0, h: 32.0, runs: head, fill: Some(Fill::Box), toggle: more, bar: Some(stripe) });
            // Closed, the first line of what came back, under the target.
            let preview = output.lines().map(str::trim).find(|l| !l.is_empty()).filter(|_| !open && diff.is_empty() && *kind != ToolKind::Read);
            if let Some(first) = preview {
                let ink = if *ok == Some(false) { Ink::Del } else { Ink::Dim };
                rows[0].h = 26.0;
                rows.push(Row { y: 0.0, h: 18.0, runs: vec![one(tx, first, MONO, 11.5, 400, ink, (right - tx).max(40.0))], fill: Some(Fill::Box), toggle: more, bar: Some(stripe) });
                rows.push(Row { y: 0.0, h: 8.0, runs: Vec::new(), fill: Some(Fill::Box), toggle: more, bar: Some(stripe) });
            }
            if open && more {
                let x0 = 34.0;
                rows.push(spacer(8.0, Some(Fill::Well)));
                let body: Vec<(String, Ink, Fill)> = if !diff.is_empty() {
                    diff.iter()
                        .take(MAX_LINES)
                        .map(|l| {
                            let (ink, fill) = match l.kind {
                                '+' => (Ink::Add, Fill::Add),
                                '-' => (Ink::Del, Fill::Del),
                                _ => (Ink::Text2, Fill::Well),
                            };
                            (format!("{} {}", l.kind, l.text.trim_end_matches(['\n', '\r'])), ink, fill)
                        })
                        .collect()
                } else {
                    output.lines().take(MAX_LINES).map(|l| (l.to_string(), if *ok == Some(false) { Ink::Del } else { Ink::Text2 }, Fill::Well)).collect()
                };
                for (text, ink, fill) in body {
                    let mut wrapped = wrap_runs(r, &[(text.replace('\t', "    "), MONO, CODE, 400, ink)], x0, w - x0 - 14.0, CODE_H, Some(fill));
                    for row in &mut wrapped {
                        row.runs.iter_mut().for_each(|run| run.x = run.x.max(x0));
                    }
                    rows.extend(wrapped);
                }
                rows.push(spacer(8.0, Some(Fill::Well)));
                rows.iter_mut().for_each(|row| row.bar = Some(stripe));
            }
        }
        ChatItem::Todos { items } => {
            let done = items.iter().filter(|(_, s)| s == "completed").count();
            let progress = format!("{done} of {} done", items.len());
            let pw = r.measure(UI, 11.5, 500, &progress);
            rows.push(spacer(10.0, Some(Fill::Box)));
            rows.push(Row {
                y: 0.0,
                h: 20.0,
                runs: vec![one(14.0, "Plan", UI, 12.0, 600, Ink::Text, 60.0), one(w - pw - 14.0, &progress, UI, 11.5, 500, Ink::Dim, pw + 1.0)],
                fill: Some(Fill::Box),
                toggle: false,
                bar: None,
            });
            rows.push(spacer(12.0, Some(Fill::Progress(done as f32 / items.len().max(1) as f32))));
            for (step, state) in items {
                let (mark, mark_ink, ink, weight) = match state.as_str() {
                    "completed" => ("✓", Ink::Add, Ink::Dim, 400),
                    "in_progress" => ("›", Ink::Kind(ToolKind::Read), Ink::Text, 600),
                    _ => ("○", Ink::Faint, Ink::Text2, 400),
                };
                let mut wrapped = wrap_runs(r, &[(step.clone(), UI, 12.5, weight, ink)], 36.0, w - 50.0, 22.0, Some(Fill::Box));
                wrapped[0].runs.insert(0, one(14.0, mark, UI, 12.5, 600, mark_ink, 16.0));
                rows.extend(wrapped);
            }
            rows.push(spacer(10.0, Some(Fill::Box)));
            rows.iter_mut().for_each(|row| row.bar = Some(Ink::Kind(ToolKind::Read)));
        }
        ChatItem::Notice { text, error } => {
            let (mark, ink) = if *error { ("!", Ink::Bad) } else { ("·", Ink::Dim) };
            let (x0, fill) = if *error { (34.0, Some(Fill::Del)) } else { (16.0, None) };
            let mut wrapped = wrap_runs(r, &[(text.clone(), UI, 12.0, if *error { 500 } else { 400 }, ink)], x0, w - x0 - 14.0, 20.0, fill);
            wrapped[0].runs.insert(0, one(x0 - 16.0, mark, UI, 12.0, 700, ink, 12.0));
            if *error {
                rows.push(spacer(7.0, fill));
                rows.extend(wrapped);
                rows.push(spacer(7.0, fill));
                rows.iter_mut().for_each(|row| row.bar = Some(Ink::Bad));
            } else {
                rows.extend(wrapped);
            }
        }
    }
    let mut y = 0.0;
    for row in &mut rows {
        if local && image_path(&row_text(row)).is_some() {
            row.h = 266.0;
        }
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
        Ink::Line => t.border2,
        Ink::Primary => t.primary_bg,
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
        Fill::Code | Fill::Box | Fill::Rule | Fill::Progress(_) => t.bg,
        Fill::Well => t.term,
        Fill::Add => add,
        Fill::Del => del,
    }
}

/// A row's background: its fill (a rule or a progress bar draws instead), its stripe, and the
/// chips under inline code.
fn paint_row(r: &mut Renderer, t: &Theme, x: f32, ry: f32, w: f32, row: &Row) {
    match row.fill {
        Some(Fill::Rule) => r.rect(Rect::new(x, ry + (row.h / 2.0).floor(), w, 1.0), 0.0, t.border2),
        Some(Fill::Progress(done)) => {
            r.rect(Rect::new(x, ry, w, row.h), 0.0, fill(t, Fill::Box));
            let track = Rect::new(x + 14.0, ry + (row.h - 3.0) / 2.0, w - 28.0, 3.0);
            r.rect(track, 0.0, t.border2);
            r.rect(Rect::new(track.x, track.y, track.w * done.clamp(0.0, 1.0), track.h), 0.0, t.done);
        }
        Some(f) => r.rect(Rect::new(x, ry, w, row.h), 0.0, fill(t, f)),
        None => {}
    }
    if let Some(bar) = row.bar {
        r.rect(Rect::new(x, ry, STRIPE, row.h), 0.0, ink(t, bar));
    }
    for run in row.runs.iter().filter(|run| run.chip) {
        let tw = r.measure(run.font, run.size, run.weight, &run.text).min(run.max_w);
        let ch = (run.size * 1.55).min(row.h).round();
        r.rect(Rect::new(x + run.x - CHIP_PAD, ry + ((row.h - ch) / 2.0).round(), tw + 2.0 * CHIP_PAD, ch), 0.0, t.raised);
    }
}

/// Starts a turn: the item after what the user sent, or the first one there is.
fn turn_starts(items: &[ChatItem], i: usize, at_start: bool) -> bool {
    !matches!(items[i], ChatItem::User { .. }) && if i == 0 { at_start } else { matches!(items[i - 1], ChatItem::User { .. }) }
}

/// What a turn (the items up to the next thing the user sent) did, for its header:
/// "2 reads · 1 edit · 1 command · 1 failed".
fn turn_stats(items: &[ChatItem]) -> String {
    let mut counts = [0usize; 6];
    let mut failed = 0;
    for item in items.iter().take_while(|item| !matches!(item, ChatItem::User { .. })) {
        if let ChatItem::Tool { kind, ok, .. } = item {
            counts[*kind as usize] += 1;
            failed += usize::from(*ok == Some(false));
        }
    }
    // In ToolKind's order: Edit, Read, Bash, Search, Web, Other.
    let names = [("edit", "edits"), ("read", "reads"), ("command", "commands"), ("search", "searches"), ("fetch", "fetches"), ("tool", "tools")];
    let order = [1, 3, 0, 2, 4, 5];
    let mut parts: Vec<String> = order
        .iter()
        .filter(|&&k| counts[k] > 0)
        .map(|&k| format!("{} {}", counts[k], if counts[k] == 1 { names[k].0 } else { names[k].1 }))
        .collect();
    if failed > 0 {
        parts.push(format!("{failed} failed"));
    }
    parts.join(" · ")
}

fn is_step(items: &[ChatItem], view: View) -> bool {
    match view {
        View::Reads(..) => true,
        View::Item(i) => matches!(items[i], ChatItem::Tool { .. }),
        View::Turn(_) => false,
    }
}

/// Space under each view: tight between tool cards and under a turn's header.
fn gaps(items: &[ChatItem], views: &[View]) -> Vec<f32> {
    (0..views.len())
        .map(|v| match (views[v], views.get(v + 1)) {
            (_, None) => 0.0,
            (View::Turn(_), _) => 4.0,
            (a, Some(&b)) if is_step(items, a) && is_step(items, b) => STEP_GAP,
            _ => GAP,
        })
        .collect()
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
            hist: None,
            selection: None,
            slash: Slash::default(),
            key,
            seq,
            writer: sock,
            start: 0,
            items: Vec::new(),
            laid: Vec::new(),
            read_win: Vec::new(),
            read_scroll: HashMap::new(),
            read_bounds: Vec::new(),
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
                c.read_win = items.iter().map(|_| None).collect();
                (c.start, c.items, c.loaded) = (start, items, true);
            }
            // An older page, just in front of what's here.
            Reply::Chat { start, mut items, .. } if start + items.len() == c.start => {
                c.loading = false;
                c.prepended = items.len();
                let mut laid: Vec<Option<Laid>> = items.iter().map(|_| None).collect();
                let mut read_win: Vec<Option<ReadWin>> = items.iter().map(|_| None).collect();
                laid.append(&mut c.laid);
                read_win.append(&mut c.read_win);
                items.append(&mut c.items);
                (c.start, c.items, c.laid) = (start, items, laid);
                c.read_win = read_win;
                // A read at the join may have merged with the run that was first before.
                c.invalidate_read(c.prepended);
            }
            Reply::Chat { .. } => c.loading = false,
            Reply::ChatPatch { index, item } if index >= c.start && c.loaded => {
                let i = index - c.start;
                if i < c.items.len() {
                    (c.items[i], c.laid[i]) = (item, None);
                    c.invalidate_read(i);
                } else if i == c.items.len() {
                    c.items.push(item);
                    c.laid.push(None);
                    c.read_win.push(None);
                    c.invalidate_read(i);
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
        if let Some(c) = &mut self.chat {
            c.hist = None;
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

    /// Shift+Tab: the selected chat's agent to its next permission mode.
    pub(super) fn cycle_mode(&mut self) {
        if let Some(task) = self.current().filter(|t| t.headless && !t.mode.is_empty() && is_live(&t.state)) {
            let mode = headless::next_mode(&task.mode).to_string();
            let _ = request_at(self.sock(task.machine).as_deref(), &Request::SetMode { id: task.id, mode });
        }
    }

    fn slash_matches(&mut self) -> Vec<&'static str> {
        if self.focus != Focus::Message || self.dialog.is_some() || self.anchor.is_some() {
            return Vec::new();
        }
        let Some(task) = self.current() else { return Vec::new() };
        if task.asking { return Vec::new(); }
        let key = (task.machine, task.id);
        let names = arranger::commands::names(&task.agent);
        let text = self.drafts.get(&key).map(String::as_str).unwrap_or("");
        let query = slash_query(text, caret_at(self.caret, Focus::Message, text));
        let Some(c) = self.chat.as_mut().filter(|c| c.key == key) else { return Vec::new() };
        let Some(query) = query else {
            c.slash = Slash::default();
            return Vec::new();
        };
        if c.slash.query != text {
            c.slash = Slash { query: text.into(), ..Slash::default() };
        }
        if c.slash.dismissed { return Vec::new(); }
        names.iter().copied().filter(|name| name.starts_with(query)).collect()
    }

    fn accept_slash(&mut self, name: &str) {
        let Some(key) = self.chat.as_ref().map(|c| c.key) else { return };
        let text = format!("/{name} ");
        self.caret = Some((Focus::Message, text.len()));
        self.anchor = None;
        self.drafts.insert(key, text);
    }

    pub(super) fn slash_key(&mut self, key: &Key) -> bool {
        let matches = self.slash_matches();
        if matches.is_empty() { return false; }
        let slash = &mut self.chat.as_mut().unwrap().slash;
        slash.selected = slash.selected.min(matches.len() - 1);
        match key {
            Key::Named(NamedKey::ArrowDown) => slash.selected = (slash.selected + 1) % matches.len(),
            Key::Named(NamedKey::ArrowUp) => slash.selected = (slash.selected + matches.len() - 1) % matches.len(),
            Key::Named(NamedKey::Escape) => slash.dismissed = true,
            Key::Named(NamedKey::Enter | NamedKey::Tab) => {
                let name = matches[slash.selected];
                self.accept_slash(name);
            }
            _ => return false,
        }
        true
    }

    fn draw_slash(&mut self, r: &mut Renderer, t: &Theme, sec: Rect, bottom: f32) -> f32 {
        let matches = self.slash_matches();
        if matches.is_empty() { return bottom; }
        let slash = &mut self.chat.as_mut().unwrap().slash;
        slash.selected = slash.selected.min(matches.len() - 1);
        let count = matches.len().min(6).min(((bottom - sec.y) / 30.0).floor().max(0.0) as usize);
        if count == 0 { return bottom; }
        let first = slash.selected.saturating_sub(count - 1);
        let panel = Rect::new(sec.x + 16.0, bottom - count as f32 * 30.0 - 8.0, sec.w - 32.0, count as f32 * 30.0 + 8.0);
        r.frame(panel, 0.0, t.dialog, t.border2);
        let mut picked = None;
        for (i, name) in matches.iter().enumerate().skip(first).take(count) {
            let row = Rect::new(panel.x + 4.0, panel.y + 4.0 + (i - first) as f32 * 30.0, panel.w - 8.0, 30.0);
            if i == slash.selected { r.rect(row, 0.0, t.sel); }
            r.text(row.x + 10.0, r.center(row, MONO, 13.0), &format!("/{name}"), MONO, 13.0, 400, t.text, row.w - 20.0);
            r.target(row, Cursor::Pointer);
            if self.click.is_some_and(|p| row.contains(p)) { picked = Some(*name); }
        }
        if let Some(name) = picked {
            self.accept_slash(name);
            self.click = None;
        }
        panel.y
    }

    /// The conversation, what's waiting on you and the message box, in `sec` under the header.
    pub(super) fn draw_chat(&mut self, r: &mut Renderer, t: &Theme, sec: Rect, task: &TaskInfo, ended: bool) {
        self.ask_bounds = None;
        let key = (task.machine, task.id);
        let focused = self.focus == Focus::Message && self.dialog.is_none();

        // Bottom up: the message box (unless the task has ended: its Revive bar is there), then what
        // it asks, then what it's doing.
        let mut bottom = sec.y + sec.h;
        if !ended {
            bottom = self.draw_composer(r, t, Rect::new(sec.x, sec.y, sec.w, sec.h), task, focused);
            bottom = self.draw_slash(r, t, sec, bottom);
        }
        let w = (sec.w - 2.0 * PAD).min(MAX_W);
        let x = sec.x + PAD;
        if task.asking {
            bottom = self.draw_ask(r, t, x, w, bottom, sec.y, task);
        }
        if task.state == "running" {
            let cy = bottom - 16.0;
            self.icon(r, t, task, x + 6.0, cy);
            let mut line = if task.action.is_empty() { "working".to_string() } else { task.action.clone() };
            if task.since != 0 {
                line = format!("{line} · {}", ago(task.since));
            }
            r.text(x + 24.0, cy - 8.0, &line, UI, 12.0, 400, t.text3, w - 24.0);
            bottom -= 30.0;
        }

        let area = Rect::new(sec.x, sec.y, sec.w, bottom - sec.y);
        let Some(c) = self.chat.as_mut().filter(|c| c.key == key) else { return };
        c.read_bounds.clear();
        if !c.loaded {
            r.text(x, area.y + 18.0, "Loading the conversation…", UI, 12.0, 400, t.text3, w);
            return;
        }
        if c.width != w {
            c.width = w;
            c.laid.iter_mut().for_each(|l| *l = None);
            c.read_win.iter_mut().for_each(|l| *l = None);
        }
        let views = views(&c.items, c.start == 0);
        let gaps = gaps(&c.items, &views);
        let mut heights: Vec<f32> = Vec::with_capacity(views.len());
        for view in &views {
            match *view {
                View::Item(i) => {
                    if c.laid[i].is_none() {
                        let open = c.is_open(i);
                        c.laid[i] = Some(layout(r, &c.items[i], w, open, task.machine == 0));
                    }
                    heights.push(c.laid[i].as_ref().unwrap().h);
                }
                View::Reads(start, len) => {
                    if c.read_win[start].is_none() {
                        c.read_win[start] = Some(read_win_layout(r, &c.items[start..start + len], w));
                    }
                    heights.push(read_window_h(c.read_win[start].as_ref().unwrap().content_h));
                }
                View::Turn(_) => heights.push(TURN_H),
            }
        }
        if c.prepended > 0 {
            let added = prepended_added(r, c, &views, &heights, &gaps, c.prepended, w, task.machine == 0);
            c.top = c.top.map(|top| top + added);
            c.prepended = 0;
        }
        let total = 18.0 + heights.iter().sum::<f32>() + gaps.iter().sum::<f32>() + 18.0;
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
        let mut image = None;
        let pressed = self.click.is_some_and(|p| area.contains(p)) && self.dialog.is_none();
        if pressed {
            c.selection = None;
            self.chat_copy.clear();
            self.focus = Focus::Term;
        }
        // Resolve the moving endpoint before painting so every row highlights the same
        // range. Clamp drags to the nearest visible text row, including margins and gaps.
        if pressed || self.chat_drag {
            let mut nearest = None;
            let mut distance = f32::INFINITY;
            let mut interactive_press = false;
            let mut row_y = area.y + 18.0 - top;
            for (v, view) in views.iter().enumerate() {
                let h = heights[v];
                if let View::Item(i) = *view {
                    let laid = c.laid[i].as_ref().unwrap();
                    for (j, row) in laid.rows.iter().enumerate() {
                        let ry = row_y + row.y;
                        let text = row_text(row);
                        if pressed && Rect::new(x, ry, w, row.h).contains(self.mouse)
                            && (row.toggle || (task.machine == 0 && image_path(&text).is_some())) {
                            interactive_press = true;
                        }
                        if ry + row.h < area.y || ry > area.y + area.h
                            || row.toggle || text.is_empty()
                            || (task.machine == 0 && image_path(&text).is_some()) {
                            continue;
                        }
                        let dy = (self.mouse.1 - self.mouse.1.clamp(ry, ry + row.h)).abs();
                        if dy < distance {
                            distance = dy;
                            nearest = Some((i, j));
                        }
                    }
                }
                row_y += h + gaps[v];
            }
            if let Some((i, j)) = nearest.filter(|_| !interactive_press) {
                let row = &c.laid[i].as_ref().unwrap().rows[j];
                let mut offset = 0;
                for run in &row.runs {
                    if offset > 0 { offset += 1; }
                    let mut rx = x + run.x;
                    for ch in run.text.chars() {
                        let advance = r.advance(run.font, run.size, run.weight, ch);
                        if self.mouse.0 < rx + advance / 2.0 { break; }
                        offset += ch.len_utf8();
                        rx += advance;
                    }
                    if self.mouse.0 < rx + 1.0 { break; }
                }
                let point = (c.start + i, j, offset.min(row_text(row).len()));
                if pressed {
                    c.selection = Some((point, point));
                    self.chat_drag = true;
                } else if let Some(selection) = &mut c.selection {
                    selection.1 = point;
                }
            }
        }
        let mut y = area.y + 18.0 - top;
        if c.start > 0 && y > area.y {
            r.text(x, y - 16.0, "Loading earlier messages…", UI, 11.5, 400, t.faint, w);
        }
        for (v, view) in views.iter().enumerate() {
            let h = heights[v];
            if y > area.y + area.h {
                break;
            }
            if y + h >= area.y {
                match *view {
                    View::Item(i) => {
                        let laid = c.laid[i].as_ref().unwrap();
                        for (row_index, row) in laid.rows.iter().enumerate() {
                            let ry = y + row.y;
                            if ry + row.h < area.y || ry > area.y + area.h {
                                continue;
                            }
                            let band = Rect::new(x, ry, w, row.h);
                            paint_row(r, t, x, ry, w, row);
                            if row.toggle {
                                r.target(band, Cursor::Pointer);
                                if self.click.is_some_and(|p| band.contains(p) && area.contains(p)) {
                                    toggled = Some(i);
                                }
                            }
                            let text = row_text(row);
                            let linked_image = (task.machine == 0).then(|| image_path(&text)).flatten();
                            if let Some(path) = linked_image.as_ref() {
                                r.target(band, Cursor::Pointer);
                                if pressed && band.contains(self.mouse) { image = Some(path.clone()); }
                            }
                            if !row.toggle && !text.is_empty() && linked_image.is_none() {
                                r.target(band, Cursor::Text);
                            }
                            if let Some(path) = linked_image.as_ref() {
                                let preview = Rect::new(x + 14.0, ry + 6.0, (w - 28.0).max(1.0), 232.0);
                                if !r.image(path, preview) {
                                    r.text(preview.x, preview.y + 12.0, "Preview unavailable · click to open", UI, 12.0, 400, t.text3, preview.w);
                                }
                                let label = path.file_name().unwrap_or_default().to_string_lossy();
                                r.text(preview.x, ry + 244.0, &label, UI, 11.0, 400, t.text3, preview.w);
                                continue;
                            }
                            let mut run_start = 0;
                            for run in &row.runs {
                                let ty = r.center(Rect::new(0.0, ry, 0.0, row.h), run.font, run.size);
                                if let Some((a, b)) = c.selection {
                                    let (a, b) = if a <= b { (a, b) } else { (b, a) };
                                    let point = (c.start + i, row_index);
                                    if point >= (a.0, a.1) && point <= (b.0, b.1) && a != b {
                                        let from = if point == (a.0, a.1) { a.2.saturating_sub(run_start).min(run.text.len()) } else { 0 };
                                        let to = if point == (b.0, b.1) { b.2.saturating_sub(run_start).min(run.text.len()) } else { run.text.len() };
                                        if from < to && run.text.is_char_boundary(from) && run.text.is_char_boundary(to) {
                                            let left = r.measure(run.font, run.size, run.weight, &run.text[..from]);
                                            let width = r.measure(run.font, run.size, run.weight, &run.text[from..to]);
                                            r.rect(Rect::new(x + run.x + left, ry, width, row.h), 0.0, t.sel);
                                        }
                                    }
                                }
                                r.text(x + run.x, ty, &run.text, run.font, run.size, run.weight, ink(t, run.ink), run.max_w);
                                run_start += run.text.len() + 1;
                            }
                        }
                    }
                    View::Turn(i) => {
                        // The agent, its model, a hairline, and what the turn did so far.
                        let band = Rect::new(x, y, w, h);
                        let cy = (y + h / 2.0).round();
                        r.rect(Rect::new(x, cy - 4.0, 8.0, 8.0), 0.0, accent(&task.agent));
                        let mut tx = x + 16.0;
                        tx += r.text(tx, r.center(band, UI, 12.5), &task.agent, UI, 12.5, 600, t.text, 160.0);
                        if !task.model.is_empty() {
                            tx += 8.0;
                            tx += r.text(tx, r.center(band, MONO, 11.5), &task.model, MONO, 11.5, 400, t.text3, 220.0);
                        }
                        let stats = turn_stats(&c.items[i..]);
                        let sw = r.measure(UI, 11.5, 400, &stats).min((x + w - tx - 40.0).max(0.0));
                        if sw > 0.0 {
                            r.text(x + w - sw, r.center(band, UI, 11.5), &stats, UI, 11.5, 400, t.text3, sw);
                        }
                        let (rx, rw) = (tx + 12.0, x + w - sw - 12.0 - (tx + 12.0));
                        if rw > 0.0 {
                            r.rect(Rect::new(rx, cy, rw, 1.0), 0.0, t.border);
                        }
                    }
                    View::Reads(start, len) => {
                        let win = c.read_win[start].as_ref().unwrap();
                        let content_h = win.content_h;
                        let viewport_h = content_h.min(READ_VIEW_MAX);
                        let window_h = READ_BAR_H + viewport_h;
                        let key = c.start + start;
                        let max = (content_h - viewport_h).max(0.0);
                        let scroll = c.read_scroll.get(&key).copied().flatten().unwrap_or(max).min(max);
                        c.read_bounds.push(ReadHit {
                            key,
                            rect: Rect::new(x, y, w, window_h),
                            content_h,
                            viewport_h,
                        });

                        let rect = Rect::new(x, y, w, window_h);
                        r.rect(rect, 0.0, fill(t, Fill::Box));
                        let bar = Rect::new(x + 1.0, y + 1.0, w - 2.0, READ_BAR_H - 1.0);
                        r.rect(bar, 0.0, fill(t, Fill::User));
                        let label = format!("{len} reads");
                        let hint = "latest at the bottom";
                        let hint_w = r.measure(UI, 11.0, 400, hint);
                        r.text(x + 12.0, r.center(bar, UI, 11.5), &label, UI, 11.5, 600, t.text, (w - hint_w - 36.0).max(40.0));
                        r.text(x + w - hint_w - 12.0, r.center(bar, UI, 11.0), hint, UI, 11.0, 400, t.faint, hint_w);

                        let viewport = Rect::new(x, y + READ_BAR_H, w, viewport_h);
                        let win_clip = r.clip();
                        r.set_clip(Some(intersect(viewport, area)));
                        for row in &win.rows {
                            let ry = viewport.y + row.y - scroll;
                            if ry + row.h < viewport.y || ry > viewport.y + viewport_h {
                                continue;
                            }
                            draw_row(r, t, x, ry, w, row);
                        }
                        r.set_clip(win_clip);

                        if max > 0.0 {
                            let rail = (viewport_h - 4.0).max(1.0);
                            let thumb_h = (rail * viewport_h / content_h).max(16.0).min(rail);
                            let thumb_y = viewport.y + 2.0 + (rail - thumb_h) * scroll / max;
                            r.rect(Rect::new(x + w - 6.0, viewport.y + 2.0, 3.0, rail), 1.5, t.border2);
                            r.rect(Rect::new(x + w - 6.0, thumb_y, 3.0, thumb_h), 1.5, t.text3);
                        }
                        // The border goes on last so scrolled content can't cover it.
                        r.rect(Rect::new(x, y, w, 1.0), 0.0, t.border2);
                        r.rect(Rect::new(x, y + window_h - 1.0, w, 1.0), 0.0, t.border2);
                        r.rect(Rect::new(x + w - 1.0, y, 1.0, window_h), 0.0, t.border2);
                        r.rect(Rect::new(x, y, STRIPE, window_h), 0.0, ink(t, Ink::Kind(ToolKind::Read)));
                    }
                }
            }
            y += h + gaps[v];
        }
        r.set_clip(old_clip);
        if let Some((a, b)) = c.selection {
            let (a, b) = if a <= b { (a, b) } else { (b, a) };
            let mut selected = Vec::new();
            for (i, laid) in c.laid.iter().enumerate() {
                if let Some(laid) = laid {
                    for (j, row) in laid.rows.iter().enumerate() {
                        let point = (c.start + i, j);
                        if point < (a.0, a.1) || point > (b.0, b.1) { continue; }
                        let text = row_text(row);
                        let from = if point == (a.0, a.1) { a.2.min(text.len()) } else { 0 };
                        let to = if point == (b.0, b.1) { b.2.min(text.len()) } else { text.len() };
                        if from <= to && text.is_char_boundary(from) && text.is_char_boundary(to) {
                            selected.push(text[from..to].to_string());
                        }
                    }
                }
            }
            self.chat_copy = selected.join("\n");
        }
        if let Some(path) = image.take() { open_image(&path); }
        if let Some(i) = toggled {
            c.toggle(i);
            self.click = None;
            self.redraw();
        }
    }

    /// What the agent waits on, pinned above the message box: Allow/Deny, or a question's options
    /// (or type an answer). Returns the y above it.
    fn draw_ask(&mut self, r: &mut Renderer, t: &Theme, x: f32, w: f32, bottom: f32, top: f32, task: &TaskInfo) -> f32 {
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
        // The text runs from 90 to the card's right edge, or to the Deny button (at w - 180).
        let text_w = if question { w - 104.0 } else { w - 36.0 }.max(80.0);
        let lines = wrap(r, &text, UI, 12.5, 400, text_w);
        // A question's options as buttons, as many rows as they take, the hint after the last.
        let hint = "or type an answer below";
        let hint_w = r.measure(UI, 11.5, 400, hint) + 4.0;
        let (left, right) = (90.0, w - 14.0);
        let mut buttons = Vec::new();
        let (mut bx, mut row) = (left, 0);
        for o in &options {
            let bw = (r.measure(MONO, 12.0, 400, o) + 24.0).min(right - left);
            if bx + bw > right && bx > left {
                (bx, row) = (left, row + 1);
            }
            buttons.push((bx, row, bw, o));
            bx += bw + 8.0;
        }
        let hint_at = if bx + hint_w > right && bx > left { (left, row + 1) } else { (bx, row) };
        let rows = hint_at.1 + 1;
        let h = if question {
            (20.0 + lines.len() as f32 * 19.0).max(42.0) + rows as f32 * 34.0 + 6.0
        } else {
            (54.0 + lines.len() as f32 * 19.0).min(220.0).min((bottom - top - 16.0).max(73.0))
        };
        let card = Rect::new(x, bottom - h - 12.0, w, h);
        r.frame(card, 0.0, t.needs_bg, t.needs_line);
        r.text(card.x + 14.0, card.y + 10.0, label, UI, 12.0, 600, t.needs, 80.0);
        if question {
            for (i, line) in lines.iter().enumerate() {
                r.text(card.x + 90.0, card.y + 10.0 + i as f32 * 19.0, line, UI, 12.5, 400, t.text, text_w);
            }
        } else {
            let identity = (key, task.since, text);
            if self.ask_identity.as_ref() != Some(&identity) {
                self.ask_scroll = 0.0;
                self.ask_identity = Some(identity);
            }
            let viewport = Rect::new(card.x + 14.0, card.y + 44.0, w - 28.0, h - 54.0);
            self.ask_bounds = Some(viewport);
            let content_h = lines.len() as f32 * 19.0;
            self.ask_scroll = self.ask_scroll.min((content_h - viewport.h).max(0.0));
            let old_clip = r.clip();
            r.set_clip(Some(viewport));
            for (i, line) in lines.iter().enumerate() {
                let y = viewport.y + i as f32 * 19.0 - self.ask_scroll;
                if y + 19.0 > viewport.y && y < viewport.y + viewport.h {
                    r.text(viewport.x, y, line, UI, 12.5, 400, t.text, text_w);
                }
            }
            r.set_clip(old_clip);
            if content_h > viewport.h {
                let thumb_h = (viewport.h * viewport.h / content_h).max(12.0).min(viewport.h);
                let thumb_y = viewport.y + (viewport.h - thumb_h) * self.ask_scroll / (content_h - viewport.h);
                r.rect(Rect::new(card.x + w - 7.0, thumb_y, 3.0, thumb_h), 1.5, t.text3);
            }
        }
        if question {
            // Anything not among the options is typed in the message box.
            let row_y = |row: usize| card.y + h - rows as f32 * 34.0 - 2.0 + row as f32 * 34.0;
            for (bx, row, bw, o) in buttons {
                let b = Rect::new(card.x + bx, row_y(row), bw, 26.0);
                r.frame(b, 0.0, t.bg, t.border2);
                r.text(b.x + 12.0, r.center(b, MONO, 12.0), o, MONO, 12.0, 400, t.text, bw - 24.0);
                if self.hit(r, b) {
                    self.send_chat(Some(o.clone()));
                }
            }
            let (hx, hrow) = hint_at;
            let hx = card.x + hx + if hx > left { 4.0 } else { 0.0 };
            r.text(hx, r.center(Rect::new(0.0, row_y(hrow), 0.0, 26.0), UI, 11.5), hint, UI, 11.5, 400, t.text3, (card.x + right - hx).max(0.0));
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
    pub(super) fn draw_composer(&mut self, r: &mut Renderer, t: &Theme, sec: Rect, task: &TaskInfo, focused: bool) -> f32 {
        let key = (task.machine, task.id);
        let draft = self.drafts.get(&key).cloned().unwrap_or_default();
        let send_w = 76.0;
        let text_w = sec.w - 32.0 - 24.0 - send_w - 8.0;
        let lines = wrap(r, &draft, UI, BODY, 400, text_w);
        let starts = line_starts(&draft, &lines);
        let caret = self.caret_for(Focus::Message, &draft).map(|at| caret_line_at(&lines, &starts, at));
        let shown = lines.len().clamp(2, COMPOSER_LINES);
        let first = caret.map_or(lines.len().saturating_sub(shown), |(line, _)| line.saturating_sub(shown - 1)).min(lines.len().saturating_sub(shown));
        let images: Vec<_> = draft.lines().filter_map(image_path).collect();
        let image_h = if images.is_empty() { 0.0 } else { 26.0 };
        let box_h = shown as f32 * 20.0 + 18.0 + image_h;
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
            r.text(tx, b.y + 9.0, &hint, UI, BODY, 400, t.text3, text_w);
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
        let mut ix = tx;
        for path in &images {
            let label = format!("View {}", path.file_name().unwrap_or_default().to_string_lossy());
            let width = (r.measure(UI, 11.0, 400, &label) + 16.0).min(220.0);
            if ix + width > b.x + b.w - send_w { break; }
            let button = Rect::new(ix, b.y + b.h - 28.0, width, 22.0);
            r.frame(button, 0.0, t.raised, t.border2);
            r.text(ix + 8.0, r.center(button, UI, 11.0), &label, UI, 11.0, 400, t.text, width - 16.0);
            if self.hit(r, button) { open_image(path); }
            ix += width + 6.0;
        }
        let send = Rect::new(b.x + b.w - send_w - 6.0, b.y + b.h - 34.0, send_w, 28.0);
        let stopping = task.headless && matches!(group(&task.state), 0 | 1);
        let ready = stopping || !draft.trim().is_empty();
        let label = if stopping { "■ Stop" } else { "Send ↵" };
        r.rect(send, 4.0, if ready { t.primary_bg } else { t.raised });
        r.text(send.x + 12.0, r.center(send, UI, 12.0), label, UI, 12.0, 600, if ready { t.primary_fg } else { t.text3 }, send_w);
        if ready && self.hit(r, send) {
            if stopping {
                self.interrupt_chat();
            } else {
                self.actions.push(Action::SendChat);
            }
        }
        let mut hx = b.x + 2.0;
        let fy = b.y + b.h + 7.0;
        let esc = if self.vim { "Esc Esc" } else { "Esc" };
        let hints = if group(&task.state) <= 1 { 2 } else { 1 };
        for (k, what) in [("⇧↵", "new line"), (esc, "interrupt")].into_iter().take(hints) {
            hx += r.text(hx, fy, k, MONO, 10.5, 400, t.text3, 80.0) + 6.0;
            hx += r.text(hx, fy, what, UI, 11.0, 400, t.text3, 120.0) + 18.0;
        }
        // Its permission mode at the right; ⇧⇥ or a click moves to the next.
        if !task.mode.is_empty() {
            let label = headless::mode_label(&task.mode);
            let ink = match task.mode.as_str() {
                "acceptEdits" => t.needs,
                "plan" => t.work,
                "auto" => t.done,
                _ => t.text3,
            };
            let (kw, lw) = (r.measure(UI, 11.0, 500, "⇧⇥"), r.measure(UI, 11.0, 500, label));
            let mx = b.x + b.w - 2.0 - lw - 6.0 - kw;
            if mx > hx {
                r.text(mx, fy, "⇧⇥", UI, 11.0, 500, t.text3, kw + 2.0);
                r.text(mx + kw + 6.0, fy, label, UI, 11.0, 500, ink, lw + 2.0);
                if self.hit(r, Rect::new(mx - 4.0, fy - 4.0, kw + lw + 14.0, 20.0)) {
                    self.cycle_mode();
                }
            }
        }
        outer.y
    }
}

fn row_text(row: &Row) -> String {
    row.runs.iter().map(|r| r.text.as_str()).collect::<Vec<_>>().join(" ")
}

pub(super) fn is_image(path: &std::path::Path) -> bool {
    path.extension().and_then(|s| s.to_str()).is_some_and(|s| matches!(s.to_ascii_lowercase().as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tiff" | "heic"))
}

fn image_path(text: &str) -> Option<std::path::PathBuf> {
    let (_, rest) = text.split_once("](")?;
    let path = std::path::PathBuf::from(rest.strip_suffix(')')?);
    (is_image(&path) && path.is_file()).then_some(path)
}

fn open_image(path: &std::path::Path) {
    let program = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let _ = Command::new(program).arg(path).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_references_preserve_spaces_and_filter_other_files() {
        let path = std::env::temp_dir().join(format!("arranger image {}.PNG", std::process::id()));
        std::fs::write(&path, b"test").unwrap();
        assert_eq!(image_path(&format!("![image]({})", path.display())), Some(path.clone()));
        assert!(is_image(std::path::Path::new("photo.JPEG")));
        assert!(!is_image(std::path::Path::new("script.sh")));
        std::fs::remove_file(&path).unwrap();
        assert!(image_path(&format!("![image]({})", path.display())).is_none());
    }

    #[test]
    fn slash_completion_only_at_command_end() {
        assert_eq!(slash_query("/", 1), Some(""));
        assert_eq!(slash_query("/comp", 5), Some("comp"));
        assert_eq!(slash_query("/compact ", 9), None);
        assert_eq!(slash_query("/compact instructions", 21), None);
        assert_eq!(slash_query("/tmp/file", 9), None);
        assert_eq!(slash_query("explain /compact", 16), None);
        assert_eq!(slash_query("/compact", 3), None);
    }

    #[test]
    fn reads_group_back_to_back_only() {
        let read = |id: &str| ChatItem::Tool { id: id.into(), name: "Read".into(), kind: ToolKind::Read, target: "f".into(), input: String::new(), output: String::new(), ok: Some(true), diff: vec![] };
        let edit = || ChatItem::Tool { id: "e".into(), name: "Edit".into(), kind: ToolKind::Edit, target: "f".into(), input: String::new(), output: String::new(), ok: Some(true), diff: vec![] };
        let v = views(&[read("a"), read("b"), edit(), read("c")], false);
        assert_eq!(v.len(), 3);
        assert!(matches!(v[0], View::Reads(0, 2)));
        assert!(matches!(v[1], View::Item(2)));
        assert!(matches!(v[2], View::Item(3)));
    }

    #[test]
    fn each_turn_opens_with_a_header_that_counts_its_steps() {
        let tool = |kind, ok| ChatItem::Tool { id: "t".into(), name: "T".into(), kind, target: "f".into(), input: String::new(), output: String::new(), ok, diff: vec![] };
        let user = || ChatItem::User { text: "go".into() };
        let reply = || ChatItem::Assistant { text: "done".into() };
        let items = [
            reply(),
            tool(ToolKind::Read, Some(true)),
            tool(ToolKind::Read, Some(true)),
            tool(ToolKind::Bash, Some(false)),
            user(),
            tool(ToolKind::Edit, Some(true)),
            user(),
        ];
        let v = views(&items, true);
        assert!(matches!(v[0], View::Turn(0)) && matches!(v[1], View::Item(0)) && matches!(v[2], View::Reads(1, 2)));
        assert!(matches!(v[5], View::Turn(5)) && matches!(v[6], View::Item(5)) && matches!(v[7], View::Item(6)));
        assert_eq!(v.len(), 8, "no header under a prompt nothing answered yet");
        // Mid-conversation, the first loaded item may not start a turn.
        assert!(matches!(views(&items, false)[0], View::Item(0)));
        assert_eq!(turn_stats(&items), "2 reads · 1 command · 1 failed");
        assert_eq!(turn_stats(&items[5..]), "1 edit");
        assert_eq!(turn_stats(&[reply()]), "");
        let g = gaps(&items, &v);
        assert_eq!((g[0], g[1], g[2], g[3]), (4.0, GAP, STEP_GAP, GAP));
        assert_eq!(*g.last().unwrap(), 0.0);
    }

    #[test]
    fn read_summaries_collapse_to_a_line_range() {
        // claude: offset (0-based) + limit.
        let input = r#"{"file_path": "/repo/src/a.rs", "offset": 10, "limit": 25}"#;
        let output = (0..25).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        assert_eq!(read_range(input, output.as_str()).1, 25);
        assert_eq!(read_summary(input, output.as_str(), Some(true)), "read lines 11 to 35 (25 lines)");
        // No offset: the range starts at line 1 and counts the output's own lines.
        let no_input = "";
        let out3 = "a\nb\nc";
        assert_eq!(read_summary(no_input, out3, Some(true)), "read lines 1 to 3 (3 lines)");
        assert_eq!(read_summary(no_input, "only", Some(true)), "read line 1");
        // While running and on failure.
        assert_eq!(read_summary(input, output.as_str(), None), "reading…");
        assert_eq!(read_summary(input, output.as_str(), Some(false)), "read failed");
    }

    #[test]
    fn tools_stay_collapsed_until_opened() {
        let line = |kind| arranger::proto::DiffLine { kind, old: 0, new: 0, text: "x\n".into() };
        let tool = |ok, diff| ChatItem::Tool { id: "t".into(), name: "Edit".into(), kind: ToolKind::Edit, target: "a".into(), input: String::new(), output: "out".into(), ok, diff };
        let (sock, _) = UnixStream::pair().unwrap();
        let mut c = ChatView { hist: None, selection: None, slash: Slash::default(), key: (0, 1), seq: 0, writer: sock, start: 10, items: vec![tool(Some(true), vec![line('+')]), tool(Some(false), vec![]), tool(None, vec![])], laid: vec![None, None, None], read_win: vec![None, None, None], read_scroll: HashMap::new(), read_bounds: Vec::new(), loaded: true, loading: false, width: 0.0, open: HashMap::new(), top: None, max_top: 0.0, prepended: 0 };
        assert_eq!((c.is_open(0), c.is_open(1), c.is_open(2)), (false, false, false));
        c.max_top = 500.0;
        c.toggle(0);
        assert_eq!(c.top, Some(500.0), "opening output pins the current viewport instead of following the bottom");
        assert!(c.is_open(0) && c.open.get(&10) == Some(&true), "kept by conversation index");
        c.top = Some(200.0);
        c.toggle(0);
        assert_eq!(c.top, Some(200.0), "toggling while scrolled up preserves the viewport");
        assert!(!c.is_open(0) && c.open.get(&10) == Some(&false), "kept by conversation index");
        c.top = None;
        c.max_top = 500.0;
        c.scroll(100.0);
        assert_eq!(c.top, Some(400.0));
        c.scroll(-200.0);
        assert_eq!(c.top, None, "back at the bottom it follows again");
        c.items = vec![
            ChatItem::User { text: "first prompt".into() },
            ChatItem::Assistant { text: "reply".into() },
            ChatItem::User { text: "second\nprompt 🦀".into() },
        ];
        let mut draft = String::new();
        assert!(c.recall_prompt(&mut draft, true));
        assert_eq!(draft, "second\nprompt 🦀");
        assert!(c.recall_prompt(&mut draft, true));
        assert_eq!(draft, "first prompt");
        assert!(c.recall_prompt(&mut draft, false));
        assert_eq!(draft, "second\nprompt 🦀");
        assert!(c.recall_prompt(&mut draft, false));
        assert!(draft.is_empty());
        draft.push_str("new draft");
        assert!(!c.recall_prompt(&mut draft, true));
        assert_eq!(draft, "new draft");
    }
}

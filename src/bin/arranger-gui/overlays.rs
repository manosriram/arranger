//! What floats over the Workspace: one machine's every task (its sidebar header's "See all"), the
//! scheme menu and the vim key sheet.

use super::*;

const HEAD_H: f32 = 52.0;
const TOOLBAR_H: f32 = 48.0;
const GROUP_ROW_H: f32 = 28.0;
const ROW_H: f32 = 40.0;
const FOOT_H: f32 = 34.0;
/// Rows the machine popup draws at first, and adds each time its list is scrolled near the end.
pub const MT_PAGE: usize = 50;
const GROUPS: [&str; 5] = ["Needs you", "Running", "Queued", "Done", "Ended"];

impl App {
    /// One machine's tasks in every state, in a popup over the Workspace: a filter and state tabs on
    /// top, then rows grouped by state, each with the one thing to do next (Revive for an agent
    /// that has ended). Rows load a page at a time as the list scrolls.
    pub fn draw_machine_tasks(&mut self, r: &mut Renderer, t: &Theme, w: f32, h: f32) {
        let Some(m) = self.machine_tasks else { return };
        let mut acts = Vec::new();
        let colors = [t.needs, t.work, t.faint, t.done, t.faint];
        r.rect(Rect::new(0.0, 0.0, w, h), 0.0, t.scrim);
        r.target(Rect::new(0.0, 0.0, w, h), Cursor::Arrow); // nothing under the scrim reacts
        let pw = (w - 32.0).min(920.0);
        let ph = (h - 128.0).clamp(240.0, 720.0);
        let pop = Rect::new((w - pw) / 2.0, (h - ph) / 2.0, pw, ph);
        r.frame(pop, 0.0, t.dialog, t.border2);
        r.target(pop, Cursor::Arrow);
        if self.click.is_some_and(|p| !pop.contains(p)) {
            acts.push(Action::MachineTasks(None));
        }

        // Header: ■ This Mac  local · 14 tasks                    esc
        let host = &self.hosts[m];
        let head = Rect::new(pop.x, pop.y, pop.w, HEAD_H);
        r.rect(Rect::new(pop.x, head.y + head.h - 1.0, pop.w, 1.0), 0.0, t.border);
        let up = matches!(host.link, Link::Up(_));
        r.rect(Rect::new(head.x + 20.0, head.y + head.h / 2.0 - 4.0, 8.0, 8.0), 0.0, if up { t.done } else { t.del });
        let nw = r.text(head.x + 38.0, r.center(head, UI, 15.0), host.name(), UI, 15.0, 600, t.text, 300.0);
        let addr = host.cfg.as_ref().map_or("local", |c| c.ssh.as_str());
        let total = host.tasks.len();
        let sub = format!("{addr} · {total} task{}", if total == 1 { "" } else { "s" });
        r.text(head.x + 50.0 + nw, r.center(head, UI, 12.5), &sub, UI, 12.5, 400, t.text3, pop.w - nw - 160.0);
        let esc = Rect::new(head.x + head.w - 64.0, head.y + (head.h - 28.0) / 2.0, 44.0, 28.0);
        r.frame(esc, 0.0, t.dialog, t.border2);
        r.text(esc.x + 9.0, r.center(esc, MONO, 12.0), "esc", MONO, 12.0, 400, t.text3, 40.0);
        if self.hit(r, esc) {
            acts.push(Action::MachineTasks(None));
        }

        // Toolbar: filter, then a tab per state with its count, as many as fit.
        let bar = Rect::new(pop.x, head.y + head.h, pop.w, TOOLBAR_H);
        r.rect(Rect::new(pop.x, bar.y + bar.h - 1.0, pop.w, 1.0), 0.0, t.border);
        let f = Rect::new(bar.x + 20.0, bar.y + 9.0, (pop.w * 0.3).min(260.0), 30.0);
        if self.input(r, t, f, &self.mt_filter.clone(), "Filter by title or branch", Focus::TasksFilter, UI) {
            acts.push(Action::Focus(Focus::TasksFilter));
        }
        let mut counts = [0; 5];
        for task in &host.tasks {
            counts[group(&task.state)] += 1;
        }
        let mut x = f.x + f.w + 10.0;
        for (g, label) in std::iter::once((None, "All")).chain(GROUPS.iter().enumerate().map(|(i, l)| (Some(i), *l))) {
            let n = g.map_or(total, |g| counts[g]).to_string();
            let tw = r.measure(UI, 12.5, 400, label) + r.measure(MONO, 12.5, 400, &n) + 22.0;
            if x + tw > pop.x + pop.w - 20.0 {
                break;
            }
            let tab = Rect::new(x, f.y, tw, 30.0);
            let on = self.mt_group == g;
            if on {
                r.rect(tab, 0.0, t.raised);
            }
            let lw = r.text(tab.x + 8.0, r.center(tab, UI, 12.5), label, UI, 12.5, 400, if on { t.text } else { t.text2 }, tw);
            let nc = match g {
                Some(g @ (0 | 1)) => colors[g],
                _ => t.text3,
            };
            r.text(tab.x + 14.0 + lw, r.center(tab, MONO, 12.5), &n, MONO, 12.5, 400, nc, 40.0);
            if self.hit(r, tab) {
                acts.push(Action::MtGroup(g));
            }
            x += tw + 1.0;
        }

        // Columns: icon | Task | Agent | Branch | Latest | Time | action.
        let inner = pop.w - 40.0;
        let fixed = 30.0 + 80.0 + 60.0 + 104.0;
        let flex = (inner - fixed).max(300.0) / 4.2;
        let cols = [("", 30.0), ("task", flex * 1.9), ("agent", 80.0), ("branch", flex * 0.7), ("latest", flex * 1.6), ("time", 60.0), ("", 104.0)];
        let col_x: Vec<f32> = cols.iter().scan(pop.x + 20.0, |x, (_, w)| { let at = *x; *x += w; Some(at) }).collect();
        let col = |name: &str| cols.iter().position(|c| c.0 == name).map(|i| (col_x[i], cols[i].1 - 14.0)).unwrap();

        // Rows, grouped by state; the first `mt_shown` of them.
        let foot = Rect::new(pop.x, pop.y + pop.h - FOOT_H, pop.w, FOOT_H);
        let list = Rect::new(pop.x + 1.0, bar.y + bar.h, pop.w - 2.0, foot.y - bar.y - bar.h);
        r.set_clip(Some(list));
        let clicked = |p: Option<(f32, f32)>| p.is_some_and(|p| list.contains(p));
        let tasks = self.mt_tasks();
        let shown = tasks.len().min(self.mt_shown);
        let mut y = list.y - self.mt_scroll;
        if tasks.is_empty() {
            let note = if total == 0 { format!("No tasks on {} yet", host.name()) } else { "No matches".into() };
            r.text(pop.x + 50.0, y + 18.0, &note, UI, 13.0, 400, t.text3, pop.w - 100.0);
        }
        let mut last_group = None;
        for task in &tasks[..shown] {
            let g = group(&task.state);
            if last_group != Some(g) {
                last_group = Some(g);
                let row = Rect::new(list.x, y, list.w, GROUP_ROW_H);
                r.rect(row, 0.0, t.panel);
                r.rect(Rect::new(row.x, row.y + row.h - 1.0, row.w, 1.0), 0.0, t.border);
                r.rect(Rect::new(pop.x + 20.0, row.y + row.h / 2.0 - 3.0, 6.0, 6.0), 0.0, colors[g]);
                let lw = r.text(pop.x + 34.0, r.center(row, UI, 11.5), &GROUPS[g].to_uppercase(), UI, 11.5, 600, t.text3, 200.0);
                r.text(pop.x + 42.0 + lw, r.center(row, MONO, 11.5), &counts[g].to_string(), MONO, 11.5, 400, t.faint, 40.0);
                y += GROUP_ROW_H;
            }
            let row = Rect::new(list.x, y, list.w, ROW_H);
            y += ROW_H;
            if row.y + row.h < list.y || row.y > list.y + list.h {
                continue;
            }
            let key = (task.machine, task.id);
            let on = self.selected == Some(key);
            let bg = if on { t.sel } else { t.dialog };
            if on {
                r.rect(row, 0.0, t.sel);
            }
            r.rect(Rect::new(row.x, row.y + row.h - 1.0, row.w, 1.0), 0.0, t.row_line);
            if self.hit(r, row) && clicked(self.click) {
                acts.push(Action::Select(key));
            }
            self.icon(r, t, task, col_x[0], row.y + row.h / 2.0);
            let (x, w) = col("task");
            let bw = if task.shipped.is_empty() { 0.0 } else { r.measure(UI, 11.0, 500, &format!("✓ {}", task.shipped)) + 16.0 };
            let tc = if g == 4 { t.text2 } else if on { t.tc_on } else { t.text };
            let tw = r.text(x, r.center(row, UI, 13.0), &title(task), UI, 13.0, 500, tc, w - bw);
            ship_badge(r, t, &task.shipped, x + tw + 6.0, row.y + row.h / 2.0);
            let (x, w) = col("agent");
            let kind = adapter::kind(&task.agent);
            r.text(x, r.center(row, MONO, 12.0), kind, MONO, 12.0, 400, accent(kind), w);
            let (x, w) = col("branch");
            r.text(x, r.center(row, MONO, 12.0), &task.branch, MONO, 12.0, 400, t.text3, w);
            let (x, w) = col("latest");
            let queue_pos = host.tasks.iter().filter(|q| q.state == "queued" && q.id <= task.id).count();
            let (latest, lc) = match self.revive_error.as_ref().filter(|e| e.0 == key) {
                Some((_, e)) => (format!("couldn't revive: {e}"), t.del),
                None => (status_line(task, queue_pos).replace('\n', " "), if g == 0 { t.needs } else { t.text3 }),
            };
            r.text(x, r.center(row, MONO, 12.0), &latest, MONO, 12.0, 400, lc, w);
            let (x, w) = col("time");
            let time = match g {
                2 => "—".to_string(),
                3 | 4 if task.updated != 0 => ago(task.updated),
                _ => age(task),
            };
            let tw = r.measure(MONO, 12.0, 400, &time);
            r.text(x + w - tw, r.center(row, MONO, 12.0), &time, MONO, 12.0, 400, t.text3, tw + 2.0);

            // The one thing to do next.
            let approve = g == 0 && task.asking && matches!(task.need, Some(Event::PermissionRequest { .. }));
            let (label, fill, fg) = match g {
                0 if approve => ("Approve", Some(t.needs), t.on_accent),
                0 => ("Answer", Some(t.needs), t.on_accent),
                3 => ("Review", Some(t.primary_bg), t.primary_fg),
                4 => ("Revive", None, t.text),
                _ => ("Open", None, t.text2),
            };
            let bw = r.measure(UI, 12.0, 500, label) + 20.0;
            let b = Rect::new(pop.x + pop.w - 20.0 - bw, row.y + (ROW_H - 28.0) / 2.0, bw, 28.0);
            match fill {
                Some(c) => r.rect(b, 0.0, c),
                None => r.frame(b, 0.0, bg, t.border2),
            }
            r.text(b.x + 10.0, r.center(b, UI, 12.0), label, UI, 12.0, 500, fg, bw);
            if self.hit(r, b) && clicked(self.click) {
                acts.retain(|a| !matches!(a, Action::Select(_)));
                acts.extend(match label {
                    // Answered before Select attaches its terminal, which would take the ask over.
                    "Approve" => vec![Action::Answer(key, true), Action::Select(key)],
                    "Answer" => vec![Action::Select(key), Action::Answering],
                    "Review" => vec![Action::Select(key), Action::MachineTasks(None), Action::OpenReview],
                    "Revive" => vec![Action::Revive(key, String::new())],
                    _ => vec![Action::Select(key), Action::MachineTasks(None), Action::Focus(Focus::Term)],
                });
            }
        }
        let matched = tasks.len();
        drop(tasks);
        let content_h = y + self.mt_scroll - list.y;
        self.mt_scroll = self.mt_scroll.min((content_h - list.h + 8.0).max(0.0));
        // Near the end of what's loaded: load the next page.
        if shown < matched && y < list.y + list.h + 4.0 * ROW_H {
            acts.push(Action::MtMore);
        }
        r.set_clip(None);

        r.rect(Rect::new(pop.x, foot.y, pop.w, 1.0), 0.0, t.border);
        let fy = r.center(foot, UI, 12.0);
        r.text(foot.x + 20.0, fy, &format!("Showing {shown} of {matched}"), UI, 12.0, 400, t.text3, 300.0);
        let note = "Revive picks up an ended agent's last session in its worktree";
        let nw = r.measure(UI, 12.0, 400, note);
        if nw < pop.w - 260.0 {
            r.text(foot.x + foot.w - 20.0 - nw, fy, note, UI, 12.0, 400, t.text3, nw + 2.0);
        }
        self.actions.extend(acts);
    }

    /// The scheme menu under its title-bar button: the eight schemes, then the preference switches.
    pub fn draw_menu(&mut self, r: &mut Renderer, t: &Theme) {
        let b = self.scheme_btn;
        let menu = Rect::new(b.x + b.w - 232.0, b.y + b.h + 6.0, 232.0, 12.0 + 2.0 * 24.0 + 8.0 * 30.0 + 13.0 + 2.0 * 30.0);
        r.rect(Rect::new(menu.x - 1.0, menu.y + 3.0, menu.w + 2.0, menu.h + 2.0), 8.0, alpha([0, 0, 0, 255], if t.dark { 0x60 } else { 0x18 }));
        r.frame(menu, 6.0, t.bg, t.border2);
        r.target(menu, Cursor::Arrow);
        let mut acts = Vec::new();
        let mut y = menu.y + 6.0;
        for (dark, label) in [(true, "DARK"), (false, "LIGHT")] {
            r.text(menu.x + 14.0, y + 8.0, label, UI, 10.5, 500, t.text3, 100.0);
            y += 24.0;
            for (i, th) in THEMES.iter().enumerate().filter(|(_, th)| th.dark == dark) {
                let row = Rect::new(menu.x + 6.0, y, menu.w - 12.0, 30.0);
                let on = i == self.theme;
                if on {
                    r.rect(row, 3.0, t.sel);
                }
                // Swatch: ground, then text and the three state colours.
                let sw = Rect::new(row.x + 8.0, row.y + 8.0, 34.0, 14.0);
                r.rect(sw, 3.0, t.border2);
                let inner = sw.inset(1.0);
                r.rect(Rect::new(inner.x, inner.y, 13.0, inner.h), 2.0, th.bg);
                for (k, c) in [th.text, th.needs, th.work, th.done].iter().enumerate() {
                    r.rect(Rect::new(inner.x + 13.0 + k as f32 * 5.0, inner.y, 5.0, inner.h), 0.0, *c);
                }
                r.text(row.x + 52.0, r.center(row, UI, 12.5), th.name, UI, 12.5, 400, t.text, 120.0);
                if on {
                    r.path(8, "M14 33.6L25.6 45.2L50 18.8", Some(7.0), row.x + row.w - 22.0, row.y + 9.0, 12.0, t.text);
                }
                if self.hit(r, row) {
                    acts.push(Action::PickTheme(i));
                }
                y += 30.0;
            }
        }
        r.rect(Rect::new(menu.x + 8.0, y + 6.0, menu.w - 16.0, 1.0), 0.0, t.border);
        y += 13.0;
        for (label, enabled, action) in [
            ("Vim keys", self.vim, Action::ToggleVim),
            ("Ding sound", !self.ding_muted, Action::ToggleDing),
        ] {
            let row = Rect::new(menu.x + 6.0, y, menu.w - 12.0, 30.0);
            r.text(row.x + 8.0, r.center(row, UI, 12.5), label, UI, 12.5, 400, t.text, 120.0);
            let track = Rect::new(row.x + row.w - 34.0, row.y + 8.0, 26.0, 14.0);
            let state = if enabled { "on" } else { "off" };
            let sw = r.measure(MONO, 10.5, 400, state);
            r.text(track.x - 8.0 - sw, r.center(row, MONO, 10.5), state, MONO, 10.5, 400, t.text3, sw + 2.0);
            r.rect(track, 7.0, if enabled { t.work } else { t.border2 });
            r.rect(Rect::new(track.x + if enabled { 14.0 } else { 2.0 }, track.y + 2.0, 10.0, 10.0), 5.0, if enabled { t.on_accent } else { t.text3 });
            if self.hit(r, row) {
                acts.push(action);
            }
            y += 30.0;
        }
        // A click anywhere else closes it.
        if self.click.is_some_and(|p| !menu.contains(p) && !b.contains(p)) {
            acts.push(Action::ToggleMenu);
        }
        self.actions.extend(acts);
    }

    /// Vim's keybinding sheet, above the status bar on the right.
    pub fn draw_help(&mut self, r: &mut Renderer, t: &Theme, w: f32, h: f32) {
        const SECTIONS: [(&str, &[(&str, &str)]); 3] = [
            ("MOVE", &[("j / k", "next / prev agent"), ("gg  G", "first / last"), ("J / K", "next / prev needing you"), ("h / l", "prev / next view"), ("/", "filter agents")]),
            ("ACT", &[("o  ↵", "type into agent"), ("j / k", "its menu, when it asks"), ("↵", "pick from the menu"), ("r", "review diff"), ("d", "delete task"), ("n", "new task"), ("m", "add machine")]),
            ("MODE", &[("i", "type into agent"), ("esc", "normal mode"), ("esc esc", "stop the agent"), ("?", "this sheet")]),
        ];
        let sw = 720.0_f32.min(w - 24.0);
        let sheet = Rect::new(w - 12.0 - sw, h - STATUS_H - 8.0 - 234.0, sw, 234.0);
        r.rect(Rect::new(sheet.x - 1.0, sheet.y + 4.0, sheet.w + 2.0, sheet.h + 2.0), 8.0, alpha([0, 0, 0, 255], if t.dark { 0x60 } else { 0x18 }));
        r.frame(sheet, 6.0, t.bg, t.border2);
        r.target(sheet, Cursor::Arrow);
        let head = Rect::new(sheet.x, sheet.y, sheet.w, 34.0);
        r.rect(Rect::new(sheet.x + 1.0, head.y + head.h - 1.0, sheet.w - 2.0, 1.0), 0.0, t.border);
        let lw = r.text(sheet.x + 12.0, r.center(head, UI, 12.5), "Keybindings", UI, 12.5, 500, t.text, 120.0);
        r.text(sheet.x + 20.0 + lw, r.center(head, MONO, 10.5), "vim · normal mode", MONO, 10.5, 400, t.text3, 200.0);
        let esc = Rect::new(sheet.x + sheet.w - 40.0, head.y + 6.0, 30.0, 22.0);
        r.frame(esc, 3.0, t.bg, t.border2);
        r.text(esc.x + 6.0, r.center(esc, MONO, 10.5), "esc", MONO, 10.5, 400, t.text3, 30.0);
        if self.hit(r, esc) {
            self.help = false;
        }
        let cw = (sheet.w - 24.0 - 32.0) / 3.0;
        for (i, (label, keys)) in SECTIONS.iter().enumerate() {
            let x = sheet.x + 12.0 + i as f32 * (cw + 16.0);
            let mut y = head.y + head.h + 12.0;
            r.text(x, y, label, UI, 10.5, 500, t.text3, cw);
            y += 20.0;
            // One key column per section, so the descriptions line up.
            let col = keys.iter().map(|(k, _)| r.measure(MONO, 10.5, 400, k).max(22.0) + 8.0).fold(0.0, f32::max);
            for (k, d) in keys.iter() {
                let kw = r.measure(MONO, 10.5, 400, k).max(22.0) + 8.0;
                let kb = Rect::new(x, y, kw, 19.0);
                r.frame(kb, 3.0, t.panel, t.border2);
                r.text(kb.x + (kw - r.measure(MONO, 10.5, 400, k)) / 2.0, r.center(kb, MONO, 10.5), k, MONO, 10.5, 400, t.text, kw);
                r.text(x + col + 8.0, r.center(kb, UI, 12.0), d, UI, 12.0, 400, t.text2, cw - col - 8.0);
                y += 23.0;
            }
        }
    }
}

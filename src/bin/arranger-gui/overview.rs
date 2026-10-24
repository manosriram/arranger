//! Overview: one dense table of every agent on every machine; and the scheme menu and vim key
//! sheet, which float over any view.

use super::*;

const TOOLBAR_H: f32 = 42.0;
const HEAD_H: f32 = 28.0;
const GROUP_ROW_H: f32 = 26.0;
const ROW_H: f32 = 32.0;
const GROUPS: [&str; 5] = ["Needs you", "Running", "Queued", "Done", "Ended"];

impl App {
    pub fn draw_overview(&mut self, r: &mut Renderer, t: &Theme, area: Rect) {
        let mut acts = Vec::new();
        let colors = [t.needs, t.work, t.faint, t.done, t.faint];

        // Toolbar: filter, then a tab per state with its count.
        let bar = Rect::new(area.x, area.y, area.w, TOOLBAR_H);
        r.rect(Rect::new(area.x, bar.y + bar.h - 1.0, area.w, 1.0), 0.0, t.border);
        let f = Rect::new(area.x + 12.0, bar.y + 8.0, 240.0, 26.0);
        if self.input(r, t, f, &self.filter.clone(), "Filter by title, branch, agent", Focus::Filter, UI) {
            acts.push(Action::Focus(Focus::Filter));
        }
        let mut counts = [0; 5];
        for task in self.all_tasks().filter(|x| self.ov_host.is_none_or(|m| x.machine == m)) {
            counts[group(&task.state)] += 1;
        }
        let mut x = f.x + f.w + 8.0;
        let total = counts.iter().sum::<usize>();
        for (g, label) in std::iter::once((None, "All")).chain(GROUPS.iter().enumerate().map(|(i, l)| (Some(i), *l))) {
            let n = g.map_or(total, |g| counts[g]).to_string();
            let tw = r.measure(UI, 12.0, 400, label) + r.measure(MONO, 12.0, 400, &n) + 20.0;
            let tab = Rect::new(x, f.y, tw, 26.0);
            let on = self.ov_group == g;
            if on {
                r.rect(tab, 3.0, t.raised);
            }
            let lw = r.text(tab.x + 8.0, r.center(tab, UI, 12.0), label, UI, 12.0, 400, if on { t.text } else { t.text2 }, tw);
            let nc = match g {
                Some(g @ (0 | 1)) => colors[g],
                _ => t.text3,
            };
            r.text(tab.x + 12.0 + lw, r.center(tab, MONO, 12.0), &n, MONO, 12.0, 400, nc, 40.0);
            if self.hit(r, tab) {
                acts.push(Action::OvGroup(g));
            }
            x += tw + 1.0;
        }
        if let Some(m) = self.ov_host {
            // The machine filter, as a chip that clears it.
            let name = self.hosts[m].name();
            let cw = r.measure(UI, 12.0, 400, name) + 34.0;
            let chip = Rect::new(x + 8.0, f.y, cw, 26.0);
            r.frame(chip, 3.0, t.bg, t.border2);
            r.text(chip.x + 9.0, r.center(chip, UI, 12.0), name, UI, 12.0, 400, t.text, cw);
            r.text(chip.x + cw - 18.0, r.center(chip, UI, 12.0), "×", UI, 12.0, 400, t.text3, 12.0);
            if self.hit(r, chip) {
                acts.push(Action::OvHost(None));
            }
        }

        // Columns: icon | Task | Machine | Agent | Branch | Latest | Time | action.
        let multi = self.hosts.iter().filter(|h| !h.removed).count() > 1;
        let inner = area.w - 24.0;
        let fixed = 28.0 + 72.0 + 52.0 + 112.0 + if multi { 110.0 } else { 0.0 };
        let flex = (inner - fixed).max(300.0) / 4.2;
        let (task_w, branch_w, latest_w) = (flex * 1.3, flex * 0.9, flex * 2.0);
        let mut cols = vec![("", 28.0), ("TASK", task_w)];
        if multi {
            cols.push(("MACHINE", 110.0));
        }
        cols.extend([("AGENT", 72.0), ("BRANCH", branch_w), ("LATEST", latest_w), ("TIME", 52.0), ("", 112.0)]);
        let col_x: Vec<f32> = cols.iter().scan(area.x + 12.0, |x, (_, w)| { let at = *x; *x += w; Some(at) }).collect();
        let col = |name: &str| cols.iter().position(|c| c.0 == name).map(|i| (col_x[i], cols[i].1 - 12.0)).unwrap();

        let head = Rect::new(area.x, bar.y + bar.h, area.w, HEAD_H);
        r.rect(Rect::new(area.x, head.y + head.h - 1.0, area.w, 1.0), 0.0, t.border);
        for (i, (name, w)) in cols.iter().enumerate() {
            let tx = if *name == "TIME" { col_x[i] + w - 12.0 - r.measure(UI, 10.5, 500, name) } else { col_x[i] };
            r.text(tx, r.center(head, UI, 10.5), name, UI, 10.5, 500, t.text3, *w);
        }

        // Rows, grouped by state.
        let list = Rect::new(area.x, head.y + head.h, area.w, area.y + area.h - head.y - head.h);
        r.set_clip(Some(list));
        let clicked = |p: Option<(f32, f32)>| p.is_some_and(|p| list.contains(p));
        let tasks = self.ov_tasks();
        let mut y = list.y - self.ov_scroll;
        if tasks.is_empty() {
            let note = if total == 0 { format!("No tasks yet · {} to start one", mod_label("N")) } else { "No matches".into() };
            r.text(area.x + 40.0, y + 16.0, &note, UI, 12.5, 400, t.text3, 400.0);
        }
        let mut last_group = None;
        for task in &tasks {
            let g = group(&task.state);
            if last_group != Some(g) {
                last_group = Some(g);
                let row = Rect::new(area.x, y, area.w, GROUP_ROW_H);
                r.rect(row, 0.0, t.panel);
                r.rect(Rect::new(row.x, row.y + row.h - 1.0, row.w, 1.0), 0.0, t.border);
                r.rect(Rect::new(area.x + 12.0, row.y + row.h / 2.0 - 2.5, 5.0, 5.0), 1.0, colors[g]);
                let lw = r.text(area.x + 23.0, r.center(row, UI, 10.5), &GROUPS[g].to_uppercase(), UI, 10.5, 500, t.text3, 200.0);
                let n = tasks.iter().filter(|x| group(&x.state) == g).count().to_string();
                r.text(area.x + 29.0 + lw, r.center(row, MONO, 10.5), &n, MONO, 10.5, 400, t.faint, 40.0);
                y += GROUP_ROW_H;
            }
            let row = Rect::new(area.x, y, area.w, ROW_H);
            y += ROW_H;
            if row.y + row.h < list.y || row.y > list.y + list.h {
                continue;
            }
            let key = (task.machine, task.id);
            let on = self.selected == Some(key);
            let bg = if on { t.sel } else { t.bg };
            if on {
                r.frame(Rect::new(row.x + 4.0, row.y + 1.0, row.w - 8.0, row.h - 2.0), 3.0, t.sel, t.sel_ring);
            } else {
                r.rect(Rect::new(row.x, row.y + row.h - 1.0, row.w, 1.0), 0.0, t.row_line);
            }
            if self.hit(r, row) && clicked(self.click) {
                acts.push(Action::Select(key));
            }
            state_icon(r, t, &task.state, col_x[0], row.y + row.h / 2.0, task.since, self.sweep());
            let (x, w) = col("TASK");
            let bw = if task.shipped.is_empty() { 0.0 } else { r.measure(UI, 10.5, 500, &format!("✓ {}", task.shipped)) + 16.0 };
            let tw = r.text(x, r.center(row, UI, 12.5), &title(task), UI, 12.5, 400, if g == 4 { t.text3 } else if on { t.tc_on } else { t.text }, w - bw);
            ship_badge(r, t, &task.shipped, x + tw + 6.0, row.y + row.h / 2.0);
            if multi {
                let (x, w) = col("MACHINE");
                r.text(x, r.center(row, MONO, 11.0), self.hosts[task.machine].name(), MONO, 11.0, 400, t.text2, w);
            }
            let (x, w) = col("AGENT");
            let kind = adapter::kind(&task.agent);
            r.text(x, r.center(row, MONO, 11.0), kind, MONO, 11.0, 400, accent(kind), w);
            let (x, w) = col("BRANCH");
            r.text(x, r.center(row, MONO, 11.0), &task.branch, MONO, 11.0, 400, t.text3, w);
            let (x, w) = col("LATEST");
            let queue_pos = self.hosts[task.machine].tasks.iter().filter(|q| q.state == "queued" && q.id <= task.id).count();
            let lc = match g {
                0 => t.needs,
                3 => t.text2,
                _ => t.text3,
            };
            r.text(x, r.center(row, MONO, 11.0), &status_line(task, queue_pos).replace('\n', " "), MONO, 11.0, 400, lc, w);
            let (x, w) = col("TIME");
            let time = match g {
                2 => "—".to_string(),
                3 if task.since != 0 => ago(task.since),
                _ => age(task),
            };
            let tw = r.measure(MONO, 11.0, 400, &time);
            r.text(x + w - tw, r.center(row, MONO, 11.0), &time, MONO, 11.0, 400, t.text3, tw + 2.0);

            // The one thing to do next.
            let approve = g == 0 && task.asking && matches!(task.need, Some(Event::PermissionRequest { .. }));
            let (label, fill, fg) = match g {
                0 if approve => ("Approve", Some(t.needs), t.on_accent),
                0 => ("Answer", Some(t.needs), t.on_accent),
                3 => ("Review", Some(t.primary_bg), t.primary_fg),
                _ => ("Open", None, t.text2),
            };
            let bw = r.measure(UI, 11.5, 500, label) + 16.0;
            let b = Rect::new(row.x + row.w - 12.0 - bw, row.y + 5.0, bw, 22.0);
            match fill {
                Some(c) => r.rect(b, 3.0, c),
                None => r.frame(b, 3.0, bg, t.border2),
            }
            r.text(b.x + 8.0, r.center(b, UI, 11.5), label, UI, 11.5, 500, fg, bw);
            if self.hit(r, b) && clicked(self.click) {
                acts.retain(|a| !matches!(a, Action::Select(_)));
                acts.push(Action::Select(key));
                acts.extend(match label {
                    "Approve" => vec![Action::Answer(true)],
                    "Review" => vec![Action::OpenReview],
                    "Answer" => vec![Action::Overview(false), Action::Focus(Focus::Message)],
                    _ => vec![Action::Overview(false), Action::Focus(Focus::Message)], // insert, unless ended
                });
            }
        }
        drop(tasks);
        let content_h = y + self.ov_scroll - list.y;
        self.ov_scroll = self.ov_scroll.min((content_h - list.h + 8.0).max(0.0));
        r.set_clip(None);
        self.actions.extend(acts);
    }

    /// The scheme menu under its title-bar button: the eight schemes, then the vim keys switch.
    pub fn draw_menu(&mut self, r: &mut Renderer, t: &Theme) {
        let b = self.scheme_btn;
        let menu = Rect::new(b.x + b.w - 232.0, b.y + b.h + 6.0, 232.0, 12.0 + 2.0 * 24.0 + 8.0 * 30.0 + 13.0 + 30.0);
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
        let row = Rect::new(menu.x + 6.0, y, menu.w - 12.0, 30.0);
        r.text(row.x + 8.0, r.center(row, UI, 12.5), "Vim keys", UI, 12.5, 400, t.text, 120.0);
        let track = Rect::new(row.x + row.w - 34.0, row.y + 8.0, 26.0, 14.0);
        let state = if self.vim { "on" } else { "off" };
        let sw = r.measure(MONO, 10.5, 400, state);
        r.text(track.x - 8.0 - sw, r.center(row, MONO, 10.5), state, MONO, 10.5, 400, t.text3, sw + 2.0);
        r.rect(track, 7.0, if self.vim { t.work } else { t.border2 });
        r.rect(Rect::new(track.x + if self.vim { 14.0 } else { 2.0 }, track.y + 2.0, 10.0, 10.0), 5.0, if self.vim { t.on_accent } else { t.text3 });
        if self.hit(r, row) {
            acts.push(Action::ToggleVim);
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
            ("ACT", &[("o  ↵", "attach to agent"), ("a", "allow command"), ("x", "deny command"), ("r", "review diff"), ("d", "delete task"), ("n", "new task"), ("m", "add machine")]),
            ("MODE", &[("i", "message agent"), ("esc", "normal mode"), ("esc esc", "stop the agent"), ("?", "this sheet")]),
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

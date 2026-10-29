//! What floats over the Workspace: one machine's every task, the settings menu, bookmarks, plan
//! usage and the vim key sheet.

use super::*;

const HEAD_H: f32 = 52.0;
const TOOLBAR_H: f32 = 48.0;
const GROUP_ROW_H: f32 = 28.0;
const ROW_H: f32 = 40.0;
const FOOT_H: f32 = 34.0;
/// Rows the machine popup draws at first, and adds each time its list is scrolled near the end.
pub const MT_PAGE: usize = 50;
const GROUPS: [&str; 5] = ["Needs you", "Running", "Queued", "Done", "Ended"];
/// Bookmarks the title-bar list shows.
const BOOKMARKS_MAX: usize = 12;

impl App {
    pub(super) fn task_is_selected(&self, key: TaskKey) -> bool {
        self.multi_select.as_ref().map_or(self.selected == Some(key), |tasks| tasks.contains(&key))
    }

    pub(super) fn register_task_region(&self, r: &Renderer, key: TaskKey, row: Rect) {
        let row = if let Some(clip) = r.clip() {
            let x = row.x.max(clip.x);
            let y = row.y.max(clip.y);
            Rect::new(x, y, (row.x + row.w).min(clip.x + clip.w) - x, (row.y + row.h).min(clip.y + clip.h) - y)
        } else { row };
        if row.w > 0.0 && row.h > 0.0 { self.task_regions.borrow_mut().push((key, row)); }
    }

    /// A non-interactive preview beside a dense row, so the list keeps space for task titles.
    pub fn draw_task_hover(&self, r: &mut Renderer, t: &Theme, w: f32, h: f32) {
        if !self.layered || self.welcome || self.review.is_some() || self.multi_select.is_some() || !self.dense() { return; }
        let regions = self.task_regions.borrow();
        let Some(&(key, row)) = regions.iter().rev().find(|(_, row)| row.contains(self.mouse)) else { return };
        let Some(task) = self.task(key).filter(|task| group(&task.state) != 0) else { return };
        let width = 380.0_f32.min(w - self.sidebar_w - 16.0);
        if width < 120.0 { return; }
        let titles = wrap(r, &title(task), UI, 13.0, 500, width - 24.0);
        let queue_pos = self.hosts[key.0].tasks.iter().filter(|x| x.state == "queued" && x.id <= task.id).count();
        let activity = wrap(r, &status_line(task, queue_pos), UI, 12.0, 400, width - 24.0);
        let height = 52.0 + titles.len() as f32 * 18.0 + activity.len() as f32 * 17.0;
        let pop = Rect::new(self.sidebar_w + 8.0, row.y.min(h - STATUS_H - height - 8.0).max(TITLE_H + 8.0), width, height);
        r.frame(pop, 4.0, t.dialog, t.border2);
        let mut y = pop.y + 12.0;
        for line in titles {
            r.text(pop.x + 12.0, y, &line, UI, 13.0, 500, t.text, width - 24.0);
            y += 18.0;
        }
        let agent = format!("{} · {}", adapter::kind(&task.agent), self.hosts[key.0].name());
        r.text(pop.x + 12.0, y + 3.0, &agent, UI, 11.0, 400, t.text3, width - 24.0);
        y += 26.0;
        for line in activity {
            r.text(pop.x + 12.0, y, &line, UI, 12.0, 400, t.text2, width - 24.0);
            y += 17.0;
        }
    }

    pub fn draw_task_selection(&mut self, r: &mut Renderer, t: &Theme, w: f32, h: f32) {
        if self.confirm_delete.is_some() { return; }
        if let Some(tasks) = &self.multi_select {
            let bar = Rect::new(8.0, h - 66.0, (self.sidebar_w - 16.0).min(w - 16.0), 36.0);
            r.frame(bar, 4.0, t.dialog, t.border2);
            let label = format!("Delete {} · d", tasks.len());
            let delete = Rect::new(bar.x + 8.0, bar.y, bar.w - 80.0, bar.h);
            let cancel = Rect::new(bar.x + bar.w - 68.0, bar.y, 64.0, bar.h);
            r.text(delete.x, r.center(delete, UI, 12.0), &label, UI, 12.0, 500, t.del, delete.w);
            r.text(cancel.x, r.center(cancel, UI, 12.0), "Esc · exit", UI, 12.0, 400, t.text, cancel.w);
            if self.hit(r, delete) { self.confirm_bulk_delete(); self.actions.clear(); }
            if self.hit(r, cancel) { self.multi_select = None; self.actions.clear(); }
        }
        if let Some((key, (x, y))) = self.task_menu {
            let Some(task) = self.task(key) else { self.task_menu = None; return };
            let name = title(task);
            let can_clone = task.state != "queued" && !task.worktree.is_empty();
            let width = 380.0_f32.min(w - 16.0);
            let lines = wrap(r, &name, UI, 13.0, 500, width - 60.0);
            let title_h = 24.0 + lines.len() as f32 * 19.0;
            let mut details = vec![
                ("Branch", task.branch.clone(), MONO),
                ("Machine", self.hosts[key.0].name().to_string(), UI),
                ("View", if task.headless { "Chat" } else { "Terminal" }.to_string(), UI),
                ("Elapsed", age(task), MONO),
            ];
            if task.tokens > 0 || task.cost.is_some() {
                let mut usage = format!("{} tokens", short_count(task.tokens));
                if let Some(cost) = task.cost { usage.push_str(&format!(" · ${cost:.2}")); }
                details.push(("Usage", usage, MONO));
            }
            if let Some(source) = task.cloned_from { details.push(("Cloned from", format!("#{source}"), MONO)); }
            let details: Vec<_> = details.into_iter().map(|(label, value, font)| {
                (label, wrap(r, &value, font, 11.5, 400, width - 108.0), font)
            }).collect();
            let details_h = 12.0 + details.iter().map(|(_, lines, _)| lines.len() as f32 * 16.0 + 6.0).sum::<f32>();
            let height = title_h + details_h + if can_clone { 114.0 } else { 76.0 };
            let menu = Rect::new(x.min(w - width - 8.0).max(8.0), y.min(h - height - 8.0).max(8.0), width, height);
            r.frame(menu, 4.0, t.dialog, t.border2);
            r.target(menu, Cursor::Arrow);
            for (i, line) in lines.iter().enumerate() {
                r.text(menu.x + 12.0, menu.y + 12.0 + i as f32 * 19.0, line, UI, 13.0, 500, t.text, menu.w - 60.0);
            }
            let copy = Rect::new(menu.x + menu.w - 40.0, menu.y + 7.0, 28.0, 28.0);
            r.path(13, "M18 42H10V10H42V18M22 22H54V54H22Z", Some(5.0), copy.x + 6.0, copy.y + 6.0, 16.0, t.text3);
            if self.hit(r, copy) {
                set_clipboard(&name);
                self.task_menu = None;
            }
            r.rect(Rect::new(menu.x + 8.0, menu.y + title_h, menu.w - 16.0, 1.0), 0.0, t.border);
            let mut dy = menu.y + title_h + 10.0;
            for (label, lines, font) in &details {
                r.text(menu.x + 12.0, dy, label, UI, 11.5, 400, t.text3, 78.0);
                for (i, line) in lines.iter().enumerate() {
                    r.text(menu.x + 96.0, dy + i as f32 * 16.0, line, *font, 11.5, 400, t.text2, menu.w - 108.0);
                }
                dy += lines.len() as f32 * 16.0 + 6.0;
            }
            let actions_y = menu.y + title_h + details_h;
            r.rect(Rect::new(menu.x + 8.0, actions_y, menu.w - 16.0, 1.0), 0.0, t.border);
            let label = if self.is_bookmarked(key) { "Remove bookmark" } else { "Bookmark thread" };
            let mut labels = vec![(label, None)];
            if can_clone { labels.push(("Clone task", Some(mod_label("D")))); }
            labels.push(("Select task", None));
            for (i, (label, shortcut)) in labels.iter().enumerate() {
                let row = Rect::new(menu.x, actions_y + i as f32 * 38.0, menu.w, 38.0);
                let sw = shortcut.as_ref().map_or(0.0, |s| r.measure(MONO, 10.5, 400, s) + 12.0);
                r.text(row.x + 12.0, r.center(row, UI, 13.0), label, UI, 13.0, 400, t.text2, row.w - 24.0 - sw);
                if let Some(shortcut) = shortcut {
                    r.text(row.x + row.w - sw, r.center(row, MONO, 10.5), shortcut, MONO, 10.5, 400, t.text3, sw);
                }
                if self.hit(r, row) {
                    match i {
                        0 => self.actions.push(Action::ToggleBookmark(key)),
                        1 if can_clone => self.actions.push(Action::Clone(key)),
                        _ => {
                            self.multi_select = Some(vec![key]);
                            self.answering = None;
                        }
                    }
                    self.task_menu = None;
                }
            }
            if self.click.is_some_and(|p| !menu.contains(p)) { self.task_menu = None; }
        }
    }

    pub fn draw_delete_confirmation(&mut self, r: &mut Renderer, t: &Theme, w: f32, h: f32) {
        let Some(tasks) = self.confirm_delete.clone() else { return };
        let name = if tasks.len() == 1 { self.task(tasks[0]).map(title).unwrap_or_default() } else { format!("{} tasks selected", tasks.len()) };
        let bounds = Rect::new(0.0, 0.0, w, h);
        r.rect(bounds, 0.0, t.scrim);
        r.target(bounds, Cursor::Arrow);
        let pw = (w - 32.0).min(480.0);
        let pop = Rect::new((w - pw) / 2.0, (h - 202.0) / 2.0, pw, 202.0);
        r.frame(pop, 8.0, t.dialog, t.border2);
        r.target(pop, Cursor::Arrow);
        r.text(pop.x + 24.0, pop.y + 22.0, "Delete selected tasks?", UI, 17.0, 600, t.text, pw - 48.0);
        r.text(pop.x + 24.0, pop.y + 58.0, &name, UI, 13.0, 500, t.text, pw - 48.0);
        r.text(pop.x + 24.0, pop.y + 84.0, "Selected tasks and their worktrees will be deleted.", UI, 13.0, 400, t.text2, pw - 48.0);
        let delete = Rect::new(pop.x + pw - 136.0, pop.y + 140.0, 112.0, 38.0);
        let cancel = Rect::new(delete.x - 122.0, delete.y, 112.0, 38.0);
        r.frame(cancel, 4.0, t.dialog, t.border2);
        r.text(cancel.x + 14.0, r.center(cancel, UI, 13.0), "Cancel · n", UI, 13.0, 500, t.text, 90.0);
        r.rect(delete, 4.0, t.del);
        r.text(delete.x + 14.0, r.center(delete, UI, 13.0), "Delete · y", UI, 13.0, 600, readable(t.on_accent, t.del), 90.0);
        if self.hit(r, delete) {
            self.confirm_delete = None;
            self.actions.extend(tasks.into_iter().map(Action::Delete));
            self.multi_select = None;
        } else if self.hit(r, cancel) || self.click.is_some_and(|p| !pop.contains(p)) {
            self.confirm_delete = None;
        }
    }

    /// One machine's tasks in every state, in a popup over the Workspace: a filter and state tabs on
    /// top, then rows grouped by state, each with the one thing to do next (Revive for an agent
    /// that has ended). Rows load a page at a time as the list scrolls.
    pub fn draw_machine_tasks(&mut self, r: &mut Renderer, t: &Theme, w: f32, h: f32) {
        if self.machine_tasks.is_none() { return; }
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

        let head = Rect::new(pop.x, pop.y, pop.w, HEAD_H);
        r.rect(Rect::new(pop.x, head.y + head.h - 1.0, pop.w, 1.0), 0.0, t.border);
        let total = self.all_tasks().filter(|task| self.mt_machine.is_none_or(|m| task.machine == m)).count();
        r.text(head.x + 20.0, r.center(head, UI, 15.0), &format!("All machines · {total} tasks"), UI, 15.0, 600, t.text, pop.w - 100.0);
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
        for task in self.all_tasks().filter(|task| self.mt_machine.is_none_or(|m| task.machine == m)) {
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

        let machines = Rect::new(pop.x, bar.y + bar.h, pop.w, TOOLBAR_H);
        let label = self.mt_machine.and_then(|m| self.hosts.get(m)).map_or("All machines", |host| host.name());
        let label = format!("Machine: {label}  →");
        let button = Rect::new(machines.x + 20.0, machines.y + 9.0, (r.measure(UI, 12.5, 400, &label) + 20.0).min(pop.w - 40.0), 30.0);
        r.frame(button, 0.0, t.raised, t.border2);
        r.text(button.x + 10.0, r.center(button, UI, 12.5), &label, UI, 12.5, 400, t.text2, button.w - 20.0);
        if self.hit(r, button) {
            let next = self.hosts.iter().enumerate()
                .find(|(m, host)| !host.removed && self.mt_machine.is_none_or(|current| *m > current))
                .map(|(m, _)| m);
            acts.push(Action::MtMachine(next));
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
        let list = Rect::new(pop.x + 1.0, machines.y + machines.h, pop.w - 2.0, foot.y - machines.y - machines.h);
        r.set_clip(Some(list));
        let clicked = |p: Option<(f32, f32)>| p.is_some_and(|p| list.contains(p));
        let reveal = self.mt_reveal.take();
        let tasks = self.mt_tasks();
        let mut shown = tasks.len().min(self.mt_shown);
        let mut scroll = self.mt_scroll;
        if let Some(index) = reveal.and_then(|key| tasks.iter().position(|t| (t.machine, t.id) == key)) {
            shown = shown.max(index + 1);
            let mut top = 0.0;
            let mut previous_group = None;
            for task in &tasks[..=index] {
                let g = (task.machine, group(&task.state));
                if previous_group != Some(g) { top += GROUP_ROW_H; }
                previous_group = Some(g);
                top += ROW_H;
            }
            let bottom = top;
            top -= ROW_H;
            if top < scroll { scroll = top; }
            if bottom > scroll + list.h { scroll = bottom - list.h; }
        }
        let mut y = list.y - scroll;
        if tasks.is_empty() {
            let note = if total == 0 { "No tasks yet" } else { "No matches" };
            r.text(pop.x + 50.0, y + 18.0, note, UI, 13.0, 400, t.text3, pop.w - 100.0);
        }
        let mut last_group = None;
        for task in &tasks[..shown] {
            let g = group(&task.state);
            if last_group != Some((task.machine, g)) {
                last_group = Some((task.machine, g));
                let row = Rect::new(list.x, y, list.w, GROUP_ROW_H);
                r.rect(row, 0.0, t.panel);
                r.rect(Rect::new(row.x, row.y + row.h - 1.0, row.w, 1.0), 0.0, t.border);
                r.rect(Rect::new(pop.x + 20.0, row.y + row.h / 2.0 - 3.0, 6.0, 6.0), 0.0, colors[g]);
                let lw = r.text(pop.x + 34.0, r.center(row, UI, 11.5), &format!("{} · {}", self.hosts[task.machine].name(), GROUPS[g].to_uppercase()), UI, 11.5, 600, t.text3, 200.0);
                r.text(pop.x + 42.0 + lw, r.center(row, MONO, 11.5), &self.hosts[task.machine].tasks.iter().filter(|task| group(&task.state) == g).count().to_string(), MONO, 11.5, 400, t.faint, 40.0);
                y += GROUP_ROW_H;
            }
            let row = Rect::new(list.x, y, list.w, ROW_H);
            y += ROW_H;
            if row.y + row.h < list.y || row.y > list.y + list.h {
                continue;
            }
            let key = (task.machine, task.id);
            let on = self.task_is_selected(key);
            let bg = if on { t.sel } else { t.dialog };
            if on {
                r.rect(row, 0.0, t.sel);
            }
            r.rect(Rect::new(row.x, row.y + row.h - 1.0, row.w, 1.0), 0.0, t.row_line);
            if self.hit(r, row) && clicked(self.click) {
                acts.push(Action::Select(key));
            }
            self.register_task_region(r, key, row);
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
            let queue_pos = self.hosts[task.machine].tasks.iter().filter(|q| q.state == "queued" && q.id <= task.id).count();
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

            if self.multi_select.is_some() { continue; }
            // The one thing to do next.
            let approve = g == 0 && task.asking && matches!(task.need, Some(Event::PermissionRequest { .. }));
            let (label, fill, fg) = match g {
                // Its setup panel has the buttons.
                0 if task.state == "setup-needs-you" => ("Set up", Some(t.needs), t.on_accent),
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
        self.mt_shown = self.mt_shown.max(shown);
        let content_h = y + scroll - list.y;
        self.mt_scroll = scroll.min((content_h - list.h + 8.0).max(0.0));
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

    /// The settings menu's box under its title-bar button: the schemes in two columns, the
    /// switches, then the machines.
    pub fn menu_rect(&self) -> Rect {
        let b = self.scheme_btn;
        let hosts = self.hosts.iter().filter(|h| !h.removed).count() as f32;
        let h = 6.0 + 24.0 + THEMES.len().div_ceil(2) as f32 * 28.0 + 13.0 + 2.0 * 30.0 + 13.0 + 24.0 + hosts * 28.0 + 40.0 + 6.0;
        Rect::new(b.x + b.w - 300.0, b.y + b.h + 6.0, 300.0, h)
    }

    pub fn draw_menu(&mut self, r: &mut Renderer, t: &Theme) {
        let b = self.scheme_btn;
        let menu = self.menu_rect();
        r.rect(Rect::new(menu.x - 1.0, menu.y + 3.0, menu.w + 2.0, menu.h + 2.0), 0.0, alpha([0, 0, 0, 255], if t.dark { 0x60 } else { 0x18 }));
        r.frame(menu, 0.0, t.bg, t.border2);
        r.target(menu, Cursor::Arrow);
        let mut acts = Vec::new();
        let mut y = menu.y + 6.0;
        r.text(menu.x + 14.0, y + 8.0, "THEME", UI, 10.5, 500, t.text3, 100.0);
        y += 24.0;
        // Dark schemes, then light, two to a row.
        let order: Vec<usize> = (0..THEMES.len()).filter(|&i| THEMES[i].dark).chain((0..THEMES.len()).filter(|&i| !THEMES[i].dark)).collect();
        let col_w = (menu.w - 12.0) / 2.0;
        for (k, &i) in order.iter().enumerate() {
            let th = &THEMES[i];
            let row = Rect::new(menu.x + 6.0 + (k % 2) as f32 * col_w, y + (k / 2) as f32 * 28.0, col_w - 2.0, 28.0);
            let on = i == self.theme;
            if on {
                r.rect(row, 0.0, t.sel);
            }
            // Swatch: ground, then text and the three state colours.
            let sw = Rect::new(row.x + 8.0, row.y + 8.0, 26.0, 12.0);
            r.rect(sw, 0.0, t.border2);
            let inner = sw.inset(1.0);
            r.rect(Rect::new(inner.x, inner.y, 8.0, inner.h), 0.0, th.bg);
            for (c, color) in [th.text, th.needs, th.work, th.done].iter().enumerate() {
                r.rect(Rect::new(inner.x + 8.0 + c as f32 * 4.0, inner.y, 4.0, inner.h), 0.0, *color);
            }
            r.text(row.x + 42.0, r.center(row, UI, 12.5), th.name, UI, 12.5, 400, if on { t.text } else { t.text2 }, row.w - 48.0);
            if self.hit(r, row) {
                acts.push(Action::PickTheme(i));
            }
        }
        y += THEMES.len().div_ceil(2) as f32 * 28.0;
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
            r.rect(track, 0.0, if enabled { t.work } else { t.border2 });
            r.rect(Rect::new(track.x + if enabled { 14.0 } else { 2.0 }, track.y + 2.0, 10.0, 10.0), 0.0, if enabled { t.on_accent } else { t.text3 });
            if self.hit(r, row) {
                acts.push(action);
            }
            y += 30.0;
        }
        r.rect(Rect::new(menu.x + 8.0, y + 6.0, menu.w - 16.0, 1.0), 0.0, t.border);
        y += 13.0;
        r.text(menu.x + 14.0, y + 8.0, "MACHINES", UI, 10.5, 500, t.text3, 100.0);
        y += 24.0;
        for (i, h) in self.hosts.iter().enumerate().filter(|(_, h)| !h.removed) {
            let row = Rect::new(menu.x + 6.0, y, menu.w - 12.0, 28.0);
            let (dot, note, nc) = match &h.link {
                Link::Up(_) if h.cfg.is_none() => (t.done, "local".to_string(), t.text3),
                Link::Up(ms) => (t.done, format!("{ms} ms"), t.text3),
                Link::Connecting => (t.needs, "connecting…".to_string(), t.text3),
                Link::Down(_) => (t.del, "unreachable".to_string(), t.del),
            };
            r.rect(Rect::new(row.x + 8.0, row.y + 10.0, 8.0, 8.0), 0.0, dot);
            let nw = r.measure(MONO, 11.0, 400, &note);
            r.text(row.x + row.w - 8.0 - nw, r.center(row, MONO, 11.0), &note, MONO, 11.0, 400, nc, nw + 2.0);
            r.text(row.x + 24.0, r.center(row, UI, 12.5), h.name(), UI, 12.5, 500, t.text, row.w - nw - 44.0);
            // Its every task, ended ones too.
            if self.hit(r, row) {
                acts.extend([Action::MachineTasks(Some(i)), Action::ToggleMenu]);
            }
            y += 28.0;
        }
        let add = Rect::new(menu.x + 10.0, y + 6.0, menu.w - 20.0, 28.0);
        r.frame(add, 0.0, t.bg, t.border2);
        let label = "+ Add machine";
        let lw = r.measure(UI, 12.0, 400, label);
        r.text(add.x + (add.w - lw) / 2.0, r.center(add, UI, 12.0), label, UI, 12.0, 400, t.text2, add.w);
        if self.hit(r, add) {
            acts.extend([Action::OpenAddHost, Action::ToggleMenu]);
        }
        // A click anywhere else closes it.
        if clicked_outside(self.click, menu, b) {
            acts.push(Action::ToggleMenu);
        }
        self.actions.extend(acts);
    }

    /// Bookmarked threads, most recently active first.
    fn bookmarked(&self) -> Vec<&TaskInfo> {
        bookmark_rows(self.all_tasks(), |t| self.is_bookmarked((t.machine, t.id)))
    }

    pub fn bookmarks_rect(&self) -> Rect {
        let b = self.bookmarks_btn;
        let rows = self.bookmarked().len().clamp(1, BOOKMARKS_MAX) as f32;
        Rect::new(b.x + b.w - 360.0, b.y + b.h + 6.0, 360.0, 12.0 + rows * 34.0)
    }

    /// The bookmarks list under its title-bar button; a row opens that thread.
    pub fn draw_bookmarks(&mut self, r: &mut Renderer, t: &Theme) {
        let b = self.bookmarks_btn;
        let pop = self.bookmarks_rect();
        r.rect(Rect::new(pop.x - 1.0, pop.y + 3.0, pop.w + 2.0, pop.h + 2.0), 0.0, alpha([0, 0, 0, 255], if t.dark { 0x60 } else { 0x18 }));
        r.frame(pop, 0.0, t.bg, t.border2);
        r.target(pop, Cursor::Arrow);
        let mut acts = Vec::new();
        let saved = self.bookmarked();
        if saved.is_empty() {
            let row = Rect::new(pop.x, pop.y + 6.0, pop.w, 34.0);
            r.text(row.x + 14.0, r.center(row, UI, 12.0), "Bookmark a thread to keep it here", UI, 12.0, 400, t.text3, row.w - 28.0);
        }
        for (k, task) in saved.iter().take(BOOKMARKS_MAX).enumerate() {
            let row = Rect::new(pop.x + 6.0, pop.y + 6.0 + k as f32 * 34.0, pop.w - 12.0, 34.0);
            let key = (task.machine, task.id);
            if self.selected == Some(key) {
                r.rect(row, 0.0, t.sel);
            }
            self.icon(r, t, task, row.x + 8.0, row.y + row.h / 2.0);
            let right = adapter::kind(&task.agent);
            let rw = r.measure(MONO, 11.5, 400, right);
            r.text(row.x + row.w - 8.0 - rw, r.center(row, MONO, 11.5), right, MONO, 11.5, 400, t.text3, rw + 2.0);
            let tc = if group(&task.state) == 4 { t.text3 } else { t.text };
            r.text(row.x + 30.0, r.center(row, UI, 12.5), &title(task), UI, 12.5, 500, tc, row.w - rw - 50.0);
            if self.hit(r, row) {
                acts.extend([Action::Select(key), Action::ToggleBookmarks]);
            }
        }
        if clicked_outside(self.click, pop, b) {
            acts.push(Action::ToggleBookmarks);
        }
        self.actions.extend(acts);
    }

    pub fn usage_rect(&self, w: f32, h: f32) -> Rect {
        let limits = self.plan_limits();
        // Same height as before pi: the plan-usage rows set the window, pi's models scroll.
        let rows = usage_height_rows(&limits);
        let ph = 14.0 + 22.0 + rows as f32 * 24.0 + 8.0 * USAGE_AGENTS.len() as f32 + 34.0;
        let pw = 470.0_f32.min(w - 20.0);
        let x = (self.usage_btn.x + self.usage_btn.w - pw).max(10.0);
        Rect::new(x, h - STATUS_H - 8.0 - ph, pw, ph)
    }

    /// Plan usage above its status-bar chip: every window each agent reported, pi's connected
    /// models, and when each plan window resets. The window stays its old height; the rows scroll.
    pub fn draw_usage(&mut self, r: &mut Renderer, t: &Theme, w: f32, h: f32) {
        let pop = self.usage_rect(w, h);
        self.usage_bounds = pop;
        r.rect(Rect::new(pop.x - 1.0, pop.y + 3.0, pop.w + 2.0, pop.h + 2.0), 0.0, alpha([0, 0, 0, 255], if t.dark { 0x60 } else { 0x18 }));
        r.frame(pop, 0.0, t.dialog, t.border2);
        r.target(pop, Cursor::Arrow);
        let limits = self.plan_limits();
        let rows = usage_rows(&limits);

        r.text(pop.x + 16.0, pop.y + 16.0, "PLAN USAGE", UI, 10.5, 500, t.text3, 200.0);
        let refresh = Rect::new(pop.x + pop.w - 38.0, pop.y + 8.0, 22.0, 22.0);
        r.frame(refresh, 3.0, t.dialog, t.border2);
        let symbol_w = r.measure(UI, 15.0, 500, "↻");
        r.text(refresh.x + (refresh.w - symbol_w) / 2.0, r.center(refresh, UI, 15.0), "↻", UI, 15.0, 500, t.text, symbol_w + 2.0);
        if self.hit(r, refresh) {
            self.actions.push(Action::RefreshUsage);
        }

        let foot = Rect::new(pop.x, pop.y + pop.h - 32.0, pop.w, 32.0);
        let list = Rect::new(pop.x + 1.0, pop.y + 32.0, pop.w - 2.0, foot.y - pop.y - 32.0);
        let content_h = rows as f32 * 24.0 + 8.0 * USAGE_POPUP_AGENTS.len() as f32;
        self.usage_scroll = self.usage_scroll.min((content_h + 6.0 - list.h).max(0.0));
        r.set_clip(Some(list));

        let x = pop.x + 16.0;
        let bar_x = x + 110.0;
        let bar_w = pop.w - 16.0 - 110.0 - 170.0;
        let mut y = pop.y + 32.0 - self.usage_scroll;
        for agent in USAGE_POPUP_AGENTS {
            let row = Rect::new(x, y, pop.w - 32.0, 24.0);
            r.text(x, r.center(row, UI, 12.5), agent, UI, 12.5, 600, t.text, 100.0);
            let live = self.all_tasks().filter(|t| adapter::kind(&t.agent) == agent && matches!(group(&t.state), 0..=2)).count();
            let note = match live { 0 => String::new(), 1 => "1 task running".into(), n => format!("{n} tasks running") };
            let nw = r.measure(UI, 11.5, 400, &note);
            r.text(row.x + row.w - nw, r.center(row, UI, 11.5), &note, UI, 11.5, 400, t.text3, nw + 2.0);
            y += 24.0;
            let mine: Vec<&PlanLimit> = limits.iter().filter(|l| l.agent == agent).collect();
            if mine.is_empty() {
                let row = Rect::new(x, y, pop.w - 32.0, 24.0);
                let note = match agent {
                    "claude" => "Waiting for Claude /usage; needs a signed-in CLI".into(),
                    "pi" => "No connected models; pi --list-models shows them".into(),
                    _ => format!("Nothing yet: it shows once a {agent} chat task runs"),
                };
                r.text(x + 12.0, r.center(row, UI, 11.5), &note, UI, 11.5, 400, t.text3, row.w - 12.0);
                y += 24.0;
            }
            for l in mine {
                let row = Rect::new(x, y, pop.w - 32.0, 24.0);
                let cy = r.center(row, MONO, 11.5);
                r.text(x + 12.0, cy, &l.window, MONO, 11.5, 400, t.text3, 96.0);
                let color = limit_color(t, l);
                let track = Rect::new(bar_x, row.y + 9.0, bar_w, 6.0);
                r.rect(track, 0.0, t.border2);
                let fill = l.used.unwrap_or(if l.status == "rejected" { 100.0 } else { 0.0 }).clamp(0.0, 100.0) as f32 / 100.0;
                r.rect(Rect::new(track.x, track.y, track.w * fill, track.h), 0.0, color);
                let pct = limit_label(l);
                r.text(bar_x + bar_w + 10.0, cy, &pct, MONO, 11.5, 500, if color == t.work { t.text } else { color }, 60.0);
                if l.resets > 0 {
                    r.text(bar_x + bar_w + 70.0, cy, &format!("resets {}", until(l.resets)), MONO, 11.5, 400, t.text3, 100.0);
                }
                y += 24.0;
            }
            y += 8.0;
        }
        r.set_clip(None);

        r.rect(Rect::new(pop.x + 12.0, foot.y, pop.w - 24.0, 1.0), 0.0, t.border);
        let totals: Vec<String> = USAGE_POPUP_AGENTS
            .iter()
            .filter_map(|agent| {
                let tasks: Vec<&TaskInfo> = self.all_tasks().filter(|t| adapter::kind(&t.agent) == *agent).collect();
                let tokens: u64 = tasks.iter().map(|t| t.tokens).sum();
                let cost: f64 = tasks.iter().filter_map(|t| t.cost).sum();
                (tokens > 0).then(|| if cost > 0.0 { format!("{agent} {} tok ${cost:.2}", short_count(tokens)) } else { format!("{agent} {} tok", short_count(tokens)) })
            })
            .collect();
        let fy = r.center(foot, UI, 11.5);
        let seen = limits.iter().map(|l| l.seen).max().unwrap_or(0);
        let right = if seen > 0 { format!("updated {} ago", ago(seen)) } else { String::new() };
        let rw = r.measure(UI, 11.5, 400, &right);
        r.text(foot.x + foot.w - 16.0 - rw, fy, &right, UI, 11.5, 400, t.text3, rw + 2.0);
        let left = if totals.is_empty() { "Tokens per task show in its header".to_string() } else { format!("tasks · {}", totals.join(" · ")) };
        r.text(foot.x + 16.0, fy, &left, UI, 11.5, 400, t.text3, foot.w - rw - 44.0);
        if clicked_outside(self.click, pop, self.usage_btn) {
            self.actions.push(Action::ToggleUsage);
        }
    }

    /// Vim's keybinding sheet, above the status bar on the right.
    pub fn draw_help(&mut self, r: &mut Renderer, t: &Theme, w: f32, h: f32) {
        const SECTIONS: [(&str, &[(&str, &str)]); 3] = [
            ("MOVE", &[("j / k", "next / prev agent"), ("gg  G", "first / last"), ("J / K", "next / prev needing you"), ("h / l", "prev / next view"), ("H", "see all machines"), ("/", "filter agents")]),
            ("ACT", &[("o  ↵", "type into agent"), ("j / k", "its menu, when it asks"), ("↵", "pick from the menu"), ("y / n", "allow once / deny"),("r", "review diff"), ("V", "Select multiple tasks"), ("d", "delete selected tasks"), ("c", "clone task"), ("n", "new task"), ("m", "add machine")]),
            ("MODE", &[("i", "type into agent"), ("esc", "normal mode"), ("esc esc", "stop the agent"), ("?", "this sheet")]),
        ];
        let sw = 720.0_f32.min(w - 24.0);
        // Tall enough for the longest section: header, label, its rows.
        let sh = 34.0 + 12.0 + 20.0 + SECTIONS.iter().map(|s| s.1.len()).max().unwrap_or(0) as f32 * 23.0 + 8.0;
        let sheet = Rect::new(w - 12.0 - sw, h - STATUS_H - 8.0 - sh, sw, sh);
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

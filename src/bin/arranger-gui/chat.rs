//! Native conversation state. The same reducer handles a saved history and live events.
use super::*;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Role {
    User,
    Assistant,
    Tool,
    Error,
    Notice,
}

pub struct Entry {
    pub id: Option<String>,
    pub role: Role,
    pub text: String,
}
#[derive(Default)]
pub struct Chat {
    pub entries: Vec<Entry>,
    streamed: bool,
    height: f32,
}
impl Chat {
    pub fn event(&mut self, event: Event) {
        match event {
            Event::Prompt { text } => {
                self.push(Role::User, text);
            }
            Event::TurnDone | Event::TurnStarted => self.streamed = false,
            Event::Message { id, text, append } => {
                self.streamed = true;
                if let Some(entry) = self.entries.iter_mut().find(|e| e.id.as_ref() == Some(&id)) {
                    if append {
                        entry.text.push_str(&text);
                    } else {
                        entry.text = text;
                    }
                } else {
                    self.entries.push(Entry {
                        id: Some(id),
                        role: Role::Assistant,
                        text,
                    });
                }
            }
            Event::Summary { text } if !self.streamed => self.push(Role::Assistant, text),
            Event::ToolOutput { text } => self.push(Role::Tool, text),
            Event::ToolStart { .. } => self.push(Role::Tool, event.describe()),
            Event::ToolEnd { ok } => {
                if let Some(entry) = self.entries.iter_mut().rev().find(|e| e.role == Role::Tool) {
                    entry.text.push_str(if ok { "  ✓" } else { "  failed" });
                }
            }
            Event::Retrying { message } => self.push(Role::Notice, format!("Retrying: {message}")),
            Event::Error { message } => self.push(Role::Error, message),
            Event::Question { text, .. } | Event::Attention { text } => {
                self.push(Role::Notice, text)
            }
            Event::PermissionRequest { detail, .. } => {
                self.push(Role::Notice, format!("Permission requested: {detail}"))
            }
            _ => {}
        }
    }
    fn push(&mut self, role: Role, text: String) {
        if !text.is_empty() {
            self.entries.push(Entry {
                id: None,
                role,
                text,
            });
        }
    }
}
struct Row {
    text: String,
    role: Role,
    label: bool,
    code: bool,
}
const ROW: f32 = 21.0;

/// Wrap at character boundaries, retaining indentation and supporting long paths/URLs.
fn body_lines(text: &str, fits: impl Fn(&str) -> bool) -> Vec<String> {
    let mut out = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        for ch in paragraph.chars() {
            line.push(ch);
            if line.chars().count() > 1 && !fits(&line) {
                let last = line.pop().unwrap();
                let split = line.rfind(' ').filter(|&i| i > 0);
                if let Some(at) = split {
                    let rest = line.split_off(at + 1);
                    line.pop();
                    out.push(std::mem::take(&mut line));
                    line = rest;
                } else {
                    out.push(std::mem::take(&mut line));
                }
                line.push(last);
            }
        }
        out.push(line);
    }
    out
}

impl App {
    pub fn draw_chat(&mut self, r: &mut Renderer, t: &Theme, area: Rect, task: &TaskInfo) {
        let key = (task.machine, task.id);
        let mut rows = Vec::new();
        for entry in &self.chat.entries {
            let label = match entry.role {
                Role::User => "You",
                Role::Assistant => adapter::kind(&task.agent),
                Role::Error => "Error",
                Role::Notice => "Notice",
                Role::Tool => "",
            };
            if !label.is_empty() {
                rows.push(Row {
                    text: label.into(),
                    role: entry.role,
                    label: true,
                    code: false,
                });
            }
            let mut code = entry.role == Role::Tool;
            for paragraph in entry.text.split('\n') {
                if paragraph.trim_start().starts_with("```") {
                    code = !code;
                    continue;
                }
                let font = if code { MONO } else { UI };
                for text in body_lines(paragraph, |line| {
                    r.measure(font, 13.0, 400, line) <= area.w - 24.0
                }) {
                    rows.push(Row {
                        text,
                        role: entry.role,
                        label: false,
                        code,
                    });
                }
            }
            rows.push(Row {
                text: String::new(),
                role: entry.role,
                label: false,
                code: false,
            });
        }
        let height = rows.len() as f32 * ROW;
        if self.wheel > 0.0 {
            self.wheel += height - self.chat.height;
        }
        self.chat.height = height;
        self.wheel = self.wheel.clamp(0.0, (height - area.h).max(0.0));
        let origin = area.y - (height - area.h).max(0.0) + self.wheel;
        if self.chat_selection.is_some_and(|s| s.0 != key) {
            self.chat_selection = None;
            self.chat_copy.clear();
        }
        let pressed = self.click.is_some_and(|p| area.contains(p))
            && self.dialog.is_none()
            && self.add_host.is_none()
            && !self.menu;
        if (pressed || self.chat_drag) && !rows.is_empty() {
            let row = (((self.mouse.1.clamp(area.y, area.y + area.h) - origin) / ROW).floor()
                as usize)
                .min(rows.len() - 1);
            let line = &rows[row];
            let font = if line.code { MONO } else { UI };
            let mut x = area.x + 12.0;
            let at = line
                .text
                .char_indices()
                .find(|(_, c)| {
                    let w = r.advance(font, 13.0, 400, *c);
                    x += w;
                    self.mouse.0 < x - w / 2.0
                })
                .map_or(line.text.len(), |(i, _)| i);
            let point = (row as i32, at);
            if pressed {
                self.chat_selection = Some((key, point, point));
                self.chat_drag = true;
            } else if let Some(s) = &mut self.chat_selection {
                s.2 = point;
            }
        }
        let selection = self
            .chat_selection
            .filter(|s| s.0 == key)
            .map(|s| (s.1.min(s.2), s.1.max(s.2)));
        let strings: Vec<_> = rows.iter().map(|r| r.text.as_str()).collect();
        self.chat_copy = selection.map_or_else(String::new, |(a, b)| selected(&strings, a, b));
        r.set_clip(Some(area));
        if rows.is_empty() {
            let note = if task.state == "queued" {
                "Queued · starts when a slot frees up"
            } else {
                "Send a message to start the conversation"
            };
            r.text(
                area.x + 12.0,
                area.y + 12.0,
                note,
                UI,
                13.0,
                400,
                t.text3,
                area.w - 24.0,
            );
        }
        for (i, row) in rows.iter().enumerate() {
            let y = origin + i as f32 * ROW;
            if y + ROW < area.y || y > area.y + area.h {
                continue;
            }
            if row.role == Role::User {
                r.rect(Rect::new(area.x, y, area.w, ROW), 2.0, t.raised);
            }
            let font = if row.code { MONO } else { UI };
            if let Some((a, b)) = selection.filter(|(a, b)| (a.0..=b.0).contains(&(i as i32))) {
                let start = if a.0 == i as i32 {
                    a.1.min(row.text.len())
                } else {
                    0
                };
                let end = if b.0 == i as i32 {
                    b.1.min(row.text.len())
                } else {
                    row.text.len()
                };
                if row.text.is_char_boundary(start) && row.text.is_char_boundary(end) {
                    let x = r.measure(font, 13.0, 400, &row.text[..start]);
                    let w = r.measure(font, 13.0, 400, &row.text[start..end]);
                    r.rect(
                        Rect::new(area.x + 12.0 + x, y, w, ROW),
                        0.0,
                        alpha(t.work, 0x50),
                    );
                }
            }
            let color = match row.role {
                Role::Error => t.del,
                Role::Tool | Role::Notice => t.text3,
                _ => t.text,
            };
            r.text(
                area.x + 12.0,
                y + 2.0,
                &row.text,
                font,
                if row.label { 11.0 } else { 13.0 },
                if row.label { 600 } else { 400 },
                color,
                area.w - 24.0,
            );
        }
        r.set_clip(None);
    }
}
fn selected(rows: &[&str], a: (i32, usize), b: (i32, usize)) -> String {
    rows.iter()
        .enumerate()
        .filter(|(i, _)| (a.0..=b.0).contains(&(*i as i32)))
        .map(|(i, text)| {
            let start = if i as i32 == a.0 {
                a.1.min(text.len())
            } else {
                0
            };
            let end = if i as i32 == b.0 {
                b.1.min(text.len())
            } else {
                text.len()
            };
            text.get(start..end).unwrap_or("")
        })
        .collect::<Vec<_>>()
        .join("\n")
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn live_and_replayed_messages_replace_final_text_without_duplicate_summary() {
        let events = vec![
            Event::Prompt {
                text: "hello".into(),
            },
            Event::Message {
                id: "a".into(),
                text: "hel".into(),
                append: true,
            },
            Event::Message {
                id: "a".into(),
                text: "lo".into(),
                append: true,
            },
            Event::Message {
                id: "a".into(),
                text: "hello!".into(),
                append: false,
            },
            Event::Summary {
                text: "hello!".into(),
            },
            Event::TurnDone,
        ];
        let mut chat = Chat::default();
        for e in events {
            chat.event(e);
        }
        assert_eq!(chat.entries.len(), 2);
        assert_eq!(chat.entries[1].text, "hello!");
        chat.event(Event::Prompt {
            text: "again".into(),
        });
        chat.event(Event::Summary {
            text: "legacy reply".into(),
        });
        assert_eq!(chat.entries.last().unwrap().text, "legacy reply");
    }
    #[test]
    fn wraps_long_words_and_copies_unicode() {
        assert_eq!(
            body_lines("你好abc", |s| s.chars().count() <= 2),
            vec!["你好", "ab", "c"]
        );
        assert_eq!(selected(&["你好", "world"], (0, 3), (1, 3)), "好\nwor");
    }
}

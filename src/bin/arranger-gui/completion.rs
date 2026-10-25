//! Repository file references in the new-task prompt.
use super::*;

#[derive(Default)]
pub(super) struct Completion {
    source: Option<(usize, String)>,
    pub files: Vec<String>,
    query: Option<(usize, usize, String)>,
    dismissed: Option<(usize, usize, String)>,
    selected: usize,
    pub bounds: Option<Rect>,
}

/// A reference starts at a word boundary; email addresses and completed references stay literal.
fn reference(prompt: &str, at: usize) -> Option<(usize, usize, String)> {
    let before = &prompt[..at];
    let start = before.rfind('@')?;
    if start > 0 && !prompt[..start].chars().next_back()?.is_whitespace() {
        return None;
    }
    let query = &prompt[start + 1..at];
    if query.chars().any(char::is_whitespace) {
        return None;
    }
    Some((start, at, query.to_lowercase()))
}

impl App {
    pub(super) fn refresh_completion(&mut self) {
        let Some(d) = &mut self.dialog else { return };
        let query = (self.focus == Focus::Prompt && self.anchor.is_none())
            .then(|| reference(&d.prompt, caret_at(self.caret, Focus::Prompt, &d.prompt)))
            .flatten();
        if d.completion.query != query {
            d.completion.selected = 0;
            d.completion.dismissed = None;
            d.completion.query = query;
        }
        let source = (d.host, d.repo.clone());
        if d.completion.source.as_ref() != Some(&source) {
            d.completion = Completion {
                source: Some(source.clone()),
                query: d.completion.query.take(),
                ..Completion::default()
            };
            let (host, repo) = source;
            let (sock, proxy) = (self.sock(host), self.proxy.clone());
            thread::spawn(move || {
                let reply = request_at(sock.as_deref(), &Request::Paths { repo: repo.clone().into() }).map_err(|e| e.to_string());
                let _ = proxy.send_event(Msg::Paths(host, repo, reply));
            });
        }
    }

    fn completion_matches(&self) -> Vec<String> {
        let Some(d) = &self.dialog else { return Vec::new() };
        let c = &d.completion;
        let Some(query) = &c.query else { return Vec::new() };
        if c.dismissed.as_ref() == Some(query) {
            return Vec::new();
        }
        c.files.iter().filter(|p| p.to_lowercase().contains(&query.2)).cloned().collect()
    }

    fn accept_completion(&mut self, path: &str) {
        let Some(d) = &mut self.dialog else { return };
        let Some((from, to, _)) = d.completion.query.clone() else { return };
        // Quote paths containing spaces so the reference remains unambiguous to the agent.
        let text = if path.chars().any(char::is_whitespace) {
            format!("@{path:?} ")
        } else {
            format!("@{path} ")
        };
        let end = d.prompt[to..].find(char::is_whitespace).map_or(d.prompt.len(), |i| to + i);
        d.prompt.replace_range(from..end, &text);
        d.hist = None;
        d.completion.query = None;
        d.completion.bounds = None;
        self.caret = Some((Focus::Prompt, from + text.len()));
        self.anchor = None;
    }

    pub(super) fn completion_key(&mut self, key: &Key) -> bool {
        if self.focus != Focus::Prompt || self.dialog.is_none() {
            return false;
        }
        self.refresh_completion();
        let matches = self.completion_matches();
        if matches.is_empty() {
            return false;
        }
        let c = &mut self.dialog.as_mut().unwrap().completion;
        c.selected = c.selected.min(matches.len() - 1);
        match key {
            Key::Named(NamedKey::ArrowDown) => c.selected = (c.selected + 1) % matches.len(),
            Key::Named(NamedKey::ArrowUp) => c.selected = (c.selected + matches.len() - 1) % matches.len(),
            Key::Named(NamedKey::Escape) => {
                c.dismissed = c.query.clone();
                c.bounds = None;
            }
            Key::Named(NamedKey::Enter | NamedKey::Tab) => {
                let path = matches[c.selected].clone();
                self.accept_completion(&path);
            }
            _ => return false,
        }
        true
    }

    pub(super) fn draw_completion(&mut self, r: &mut Renderer, t: &Theme, area: Rect, click: Option<(f32, f32)>) {
        let matches = self.completion_matches();
        let Some(d) = &mut self.dialog else { return };
        d.completion.bounds = None;
        if matches.is_empty() {
            return;
        }
        let c = &mut d.completion;
        c.selected = c.selected.min(matches.len() - 1);
        let count = matches.len().min(8).min((area.h / 32.0).floor().max(1.0) as usize);
        let first = c.selected.saturating_sub(count - 1);
        let panel = Rect::new(area.x, area.y, area.w, count as f32 * 32.0 + 8.0);
        c.bounds = Some(panel);
        r.frame(panel, 0.0, t.dialog, t.border2);
        r.target(panel, Cursor::Arrow);
        let mut picked = None;
        for (i, path) in matches.iter().enumerate().skip(first).take(count) {
            let row = Rect::new(panel.x + 4.0, panel.y + 4.0 + (i - first) as f32 * 32.0, panel.w - 8.0, 32.0);
            if i == c.selected {
                r.rect(row, 0.0, t.sel);
            }
            r.text(row.x + 10.0, r.center(row, MONO, 13.0), path, MONO, 13.0, 400, t.text, row.w - 20.0);
            r.target(row, Cursor::Pointer);
            if click.is_some_and(|p| row.contains(p)) {
                picked = Some(path.clone());
            }
        }
        if let Some(path) = picked {
            self.accept_completion(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::reference;

    #[test]
    fn reference_at_caret() {
        assert_eq!(reference("Fix @src/ma please", 11), Some((4, 11, "src/ma".into())));
        assert_eq!(reference("@", 1), Some((0, 1, String::new())));
        assert_eq!(reference("é @SRC", 7), Some((3, 7, "src".into())));
        assert_eq!(reference("mail@example.com", 16), None);
        assert_eq!(reference("@src/main.rs done", 17), None);
        assert_eq!(reference("@one @two", 9), Some((5, 9, "two".into())));
    }
}

//! A check's output as clean console lines: colours kept, `\r` redraws (progress bars) collapsed
//! to what was left on the line, cursor and other escapes dropped, and lines that look like errors
//! marked. Fed the raw bytes as they come.

use crate::proto::{Color, ConsoleLine, Style};

/// Longer lines are cut; more lines than this are dropped (with a note).
const MAX_COLS: usize = 4000;
const MAX_LINES: usize = 200_000;

#[derive(Default)]
pub struct Console {
    lines: Vec<ConsoleLine>,
    /// The line being written: one styled cell per character, so `\r` can overwrite it.
    cells: Vec<(char, Style)>,
    col: usize,
    style: Style,
    /// An escape sequence, or a UTF-8 character, split across feeds.
    pending: Vec<u8>,
    dropped: bool,
}

enum Esc {
    /// Not a whole sequence yet.
    Partial,
    /// `len` bytes, ending a CSI with `final` and these parameters.
    Csi { len: usize, params: String, last: u8 },
    /// Any other sequence (OSC, charset…), `len` bytes, ignored.
    Other(usize),
}

impl Console {
    pub fn feed(&mut self, bytes: &[u8]) {
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(bytes);
        let mut i = 0;
        while i < buf.len() {
            let b = buf[i];
            match b {
                0x1b => match escape(&buf[i..]) {
                    Esc::Partial => break,
                    Esc::Csi { len, params, last } => {
                        self.csi(&params, last);
                        i += len;
                    }
                    Esc::Other(len) => i += len,
                },
                b'\n' => {
                    self.end_line();
                    i += 1;
                }
                b'\r' => {
                    self.col = 0;
                    i += 1;
                }
                0x08 => {
                    self.col = self.col.saturating_sub(1);
                    i += 1;
                }
                b'\t' => {
                    let to = (self.col / 8 + 1) * 8;
                    while self.col < to {
                        self.put(' ');
                    }
                    i += 1;
                }
                0..0x20 | 0x7f => i += 1,
                _ => {
                    let len = utf8_len(b);
                    if i + len > buf.len() {
                        break;
                    }
                    match std::str::from_utf8(&buf[i..i + len]) {
                        Ok(s) => self.put(s.chars().next().unwrap()),
                        Err(_) => self.put('\u{fffd}'),
                    }
                    i += len.max(1);
                }
            }
        }
        // Keep an unfinished escape or character for the next feed, unless it's junk.
        if buf.len() - i < 64 {
            self.pending = buf.split_off(i);
        }
    }

    /// Every line so far, the one being written last (if it has anything).
    pub fn len(&self) -> usize {
        self.lines.len() + usize::from(!self.cells.is_empty())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Lines `start..start + limit`.
    pub fn lines(&self, start: usize, limit: usize) -> Vec<ConsoleLine> {
        let end = (start + limit).min(self.len());
        (start..end).map(|i| self.lines.get(i).cloned().unwrap_or_else(|| line(&self.cells))).collect()
    }

    fn put(&mut self, c: char) {
        if self.col >= MAX_COLS {
            return;
        }
        while self.cells.len() < self.col {
            self.cells.push((' ', Style::default()));
        }
        if self.col < self.cells.len() {
            self.cells[self.col] = (c, self.style);
        } else {
            self.cells.push((c, self.style));
        }
        self.col += 1;
    }

    fn end_line(&mut self) {
        let cells = std::mem::take(&mut self.cells);
        self.col = 0;
        if self.lines.len() >= MAX_LINES {
            if !self.dropped {
                self.dropped = true;
                let note = format!("… more than {MAX_LINES} lines; the rest isn't shown");
                self.lines.push(ConsoleLine { spans: vec![(note, Style { dim: true, ..Style::default() })], error: false });
            }
            return;
        }
        self.lines.push(line(&cells));
    }

    fn csi(&mut self, params: &str, last: u8) {
        let nums = || params.split(';').map(|p| p.parse::<u32>().unwrap_or(0));
        match last {
            b'm' if !params.starts_with(['?', '>', '<', '=']) => self.sgr(&nums().collect::<Vec<_>>()),
            // Erase in line: 0 (default) to the end, 1 to the start, 2 all of it.
            b'K' => match nums().next().unwrap_or(0) {
                0 => self.cells.truncate(self.col),
                1 => {
                    for c in self.cells.iter_mut().take(self.col + 1) {
                        *c = (' ', Style::default());
                    }
                }
                _ => self.cells.clear(),
            },
            // Cursor to a column (1-based), or back/forward on the line.
            b'G' => self.col = (nums().next().unwrap_or(1).max(1) - 1) as usize,
            b'D' => self.col = self.col.saturating_sub(nums().next().unwrap_or(1).max(1) as usize),
            b'C' => self.col = (self.col + nums().next().unwrap_or(1).max(1) as usize).min(MAX_COLS),
            _ => {}
        }
    }

    fn sgr(&mut self, codes: &[u32]) {
        let s = &mut self.style;
        let mut i = 0;
        while i < codes.len() {
            match codes[i] {
                0 => *s = Style::default(),
                1 => s.bold = true,
                2 => s.dim = true,
                3 => s.italic = true,
                4 => s.underline = true,
                22 => (s.bold, s.dim) = (false, false),
                23 => s.italic = false,
                24 => s.underline = false,
                c @ 30..=37 => s.fg = Some(Color::Idx(c as u8 - 30)),
                c @ 90..=97 => s.fg = Some(Color::Idx(c as u8 - 90 + 8)),
                39 => s.fg = None,
                c @ 40..=47 => s.bg = Some(Color::Idx(c as u8 - 40)),
                c @ 100..=107 => s.bg = Some(Color::Idx(c as u8 - 100 + 8)),
                49 => s.bg = None,
                c @ (38 | 48) => {
                    let color = match codes.get(i + 1) {
                        Some(5) => {
                            i += 2;
                            codes.get(i).map(|&n| Color::Idx(n as u8))
                        }
                        Some(2) => {
                            i += 4;
                            match codes.get(i - 2..=i) {
                                Some([r, g, b]) => Some(Color::Rgb(*r as u8, *g as u8, *b as u8)),
                                _ => None,
                            }
                        }
                        _ => None,
                    };
                    if c == 38 { s.fg = color } else { s.bg = color }
                }
                _ => {}
            }
            i += 1;
        }
    }
}

/// A finished line: runs of one style, and whether it looks like an error.
fn line(cells: &[(char, Style)]) -> ConsoleLine {
    let mut spans: Vec<(String, Style)> = Vec::new();
    let end = cells.iter().rposition(|c| c.0 != ' ' || c.1.bg.is_some()).map_or(0, |i| i + 1);
    for &(c, style) in &cells[..end] {
        match spans.last_mut() {
            Some((text, s)) if *s == style => text.push(c),
            _ => spans.push((c.to_string(), style)),
        }
    }
    let text: String = spans.iter().map(|s| s.0.as_str()).collect();
    ConsoleLine { error: is_error(&text), spans }
}

/// The shapes errors take in common tools' output.
pub fn is_error(text: &str) -> bool {
    let t = text.trim_start();
    let lower = t.to_lowercase();
    lower.starts_with("error") && t[5..].starts_with([':', '[', ' ', '('])
        || lower.contains(" error:") || lower.contains("error[e") || lower.contains("): error ")
        || t.contains("panicked at")
        || t.starts_with("FAIL") || t.contains(" FAILED") || t.contains("FAILED ")
        || t.starts_with("Traceback (most recent call last)")
        || t.starts_with("npm ERR!") || t.starts_with("npm error")
        || t.starts_with(['✗', '✕', '×', '✘'])
        || lower.starts_with("fatal:")
        || t.starts_with("E   ") // pytest's assertion detail
}

fn utf8_len(first: u8) -> usize {
    match first {
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

fn escape(b: &[u8]) -> Esc {
    match b.get(1) {
        None => Esc::Partial,
        Some(b'[') => {
            for (k, &c) in b.iter().enumerate().skip(2) {
                if (0x40..=0x7e).contains(&c) {
                    return Esc::Csi { len: k + 1, params: String::from_utf8_lossy(&b[2..k]).into_owned(), last: c };
                }
                if k > 64 {
                    return Esc::Other(k);
                }
            }
            Esc::Partial
        }
        // OSC: up to BEL or ESC \.
        Some(b']') => {
            for k in 2..b.len() {
                if b[k] == 0x07 {
                    return Esc::Other(k + 1);
                }
                if b[k] == 0x1b && b.get(k + 1) == Some(&b'\\') {
                    return Esc::Other(k + 2);
                }
                if k > 4096 {
                    return Esc::Other(k);
                }
            }
            Esc::Partial
        }
        // Charset selection and the like take one more byte.
        Some(b'(' | b')' | b'#' | b'%') => if b.len() < 3 { Esc::Partial } else { Esc::Other(3) },
        Some(_) => Esc::Other(2),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(c: &Console) -> Vec<String> {
        c.lines(0, usize::MAX).iter().map(|l| l.spans.iter().map(|s| s.0.as_str()).collect()).collect()
    }

    #[test]
    fn progress_collapses_and_partial_lines_show() {
        let mut c = Console::default();
        c.feed(b"Downloading  10%\rDownloading  55%\rDownloading 100%\nCompiling a\r\x1b[2KCompiling b\nrunning");
        assert_eq!(text(&c), ["Downloading 100%", "Compiling b", "running"]);
        assert_eq!(c.len(), 3);
        c.feed(b" 3 tests\r\n");
        assert_eq!(text(&c), ["Downloading 100%", "Compiling b", "running 3 tests"]);
    }

    #[test]
    fn colours_become_spans_and_other_escapes_go() {
        let mut c = Console::default();
        c.feed(b"\x1b]0;title\x07\x1b[?25l\x1b[1;31merror\x1b[0m: \x1b[38;5;208mbad\x1b[39m \x1b[38;2;1;2;3mx\x1b[m\n");
        let l = &c.lines(0, 1)[0];
        assert!(l.error);
        assert_eq!(l.spans.iter().map(|s| s.0.as_str()).collect::<Vec<_>>(), ["error", ": ", "bad", " ", "x"]);
        assert_eq!(l.spans[0].1, Style { fg: Some(Color::Idx(1)), bold: true, ..Style::default() });
        assert_eq!(l.spans[2].1.fg, Some(Color::Idx(208)));
        assert_eq!(l.spans[4].1.fg, Some(Color::Rgb(1, 2, 3)));
        assert_eq!(l.spans[3].1, Style::default());
    }

    #[test]
    fn sequences_and_characters_split_across_feeds() {
        let mut c = Console::default();
        let all = "\x1b[32mok ✓\x1b[0m done\n".as_bytes();
        for b in all {
            c.feed(std::slice::from_ref(b));
        }
        assert_eq!(text(&c), ["ok ✓ done"]);
        assert_eq!(c.lines(0, 1)[0].spans[0].1.fg, Some(Color::Idx(2)));
    }

    #[test]
    fn tabs_backspaces_and_trailing_space() {
        let mut c = Console::default();
        c.feed(b"a\tb   \nabc\x08\x08X\n");
        assert_eq!(text(&c), ["a       b", "aXc"]);
    }

    #[test]
    fn errors_are_recognised() {
        for e in ["error[E0308]: mismatched types", "  error: could not compile", "thread 'x' panicked at src/a.rs:1", "FAIL src/a.test.ts",
            "test foo ... FAILED", "Traceback (most recent call last):", "npm ERR! code 1", "✗ 2 failed", "fatal: not a git repository", "src/a.ts(3,1): error TS2304: x"] {
            assert!(is_error(e), "{e}");
        }
        for ok in ["test result: ok. 3 passed", "0 errors", "Compiling errors v0.1", "warning: unused", "errorless"] {
            assert!(!is_error(ok), "{ok}");
        }
    }
}

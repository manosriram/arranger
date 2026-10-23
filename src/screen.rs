//! Per-task terminal state, and the escape sequences that redraw it for a new viewer.

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, Rgb};
use std::cell::RefCell;
use std::fmt::Write;
use std::rc::Rc;

const SCROLLBACK: usize = 2000;

/// Collects the bytes the terminal wants written back to the PTY (query replies).
#[derive(Clone, Default)]
struct Replies(Rc<RefCell<Vec<u8>>>);

impl EventListener for Replies {
    fn send_event(&self, event: Event) {
        if let Event::PtyWrite(s) = event {
            self.0.borrow_mut().extend_from_slice(s.as_bytes());
        }
    }
}

struct Size {
    cols: usize,
    rows: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

pub struct Screen {
    term: Term<Replies>,
    parser: Processor,
    replies: Replies,
}

impl Screen {
    pub fn new(cols: u16, rows: u16) -> Self {
        let replies = Replies::default();
        let config = Config { scrolling_history: SCROLLBACK, ..Config::default() };
        let size = Size { cols: cols.into(), rows: rows.into() };
        Self { term: Term::new(config, &size, replies.clone()), parser: Processor::new(), replies }
    }

    /// Feeds PTY output and returns any replies the terminal wants sent back.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.parser.advance(&mut self.term, bytes);
        self.replies.0.take()
    }

    pub fn grid(&self) -> &alacritty_terminal::Grid<Cell> {
        self.term.grid()
    }

    pub fn mode(&self) -> TermMode {
        *self.term.mode()
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.term.resize(Size { cols: cols.into(), rows: rows.into() });
    }

    /// Escape sequences that reproduce the visible screen, cursor and input modes.
    pub fn snapshot(&self) -> Vec<u8> {
        let mode = *self.term.mode();
        let grid = self.term.grid();
        let mut s = String::new();
        if mode.contains(TermMode::ALT_SCREEN) {
            s.push_str("\x1b[?1049h");
        }
        s.push_str("\x1b[0m\x1b[H\x1b[2J");
        for row in 0..grid.screen_lines() {
            let line = &grid[Line(row as i32)];
            let end = (0..grid.columns()).rposition(|c| !blank(&line[Column(c)])).map_or(0, |i| i + 1);
            let _ = write!(s, "\x1b[{};1H", row + 1);
            let mut pen = String::new();
            for col in 0..end {
                let cell = &line[Column(col)];
                if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                    continue;
                }
                let sgr = sgr(cell);
                if sgr != pen {
                    s.push_str(&sgr);
                    pen = sgr;
                }
                s.push(cell.c);
                s.extend(cell.zerowidth().into_iter().flatten());
            }
            s.push_str("\x1b[0m");
        }
        let at = grid.cursor.point;
        let _ = write!(s, "\x1b[{};{}H", at.line.0 + 1, at.column.0 + 1);
        for (flag, code) in [
            (TermMode::APP_CURSOR, 1),
            (TermMode::MOUSE_REPORT_CLICK, 1000),
            (TermMode::MOUSE_DRAG, 1002),
            (TermMode::MOUSE_MOTION, 1003),
            (TermMode::FOCUS_IN_OUT, 1004),
            (TermMode::SGR_MOUSE, 1006),
            (TermMode::BRACKETED_PASTE, 2004),
        ] {
            if mode.contains(flag) {
                let _ = write!(s, "\x1b[?{code}h");
            }
        }
        if mode.contains(TermMode::APP_KEYPAD) {
            s.push_str("\x1b=");
        }
        let kitty = (mode & TermMode::KITTY_KEYBOARD_PROTOCOL).bits() >> TermMode::DISAMBIGUATE_ESC_CODES.bits().trailing_zeros();
        if kitty != 0 {
            let _ = write!(s, "\x1b[>{kitty}u");
        }
        s.push_str(if mode.contains(TermMode::SHOW_CURSOR) { "\x1b[?25h" } else { "\x1b[?25l" });
        s.into_bytes()
    }
}

fn blank(c: &Cell) -> bool {
    c.c == ' ' && c.bg == Color::Named(NamedColor::Background) && c.flags.is_empty()
}

fn sgr(c: &Cell) -> String {
    let mut s = String::from("\x1b[0");
    for (flag, code) in [
        (Flags::BOLD, 1),
        (Flags::DIM, 2),
        (Flags::ITALIC, 3),
        (Flags::ALL_UNDERLINES, 4),
        (Flags::INVERSE, 7),
        (Flags::HIDDEN, 8),
        (Flags::STRIKEOUT, 9),
    ] {
        if c.flags.intersects(flag) {
            let _ = write!(s, ";{code}");
        }
    }
    color(&mut s, c.fg, 30);
    color(&mut s, c.bg, 40);
    s.push('m');
    s
}

/// `base` is 30 for foreground, 40 for background.
fn color(s: &mut String, color: Color, base: usize) {
    let _ = match color {
        Color::Spec(Rgb { r, g, b }) => write!(s, ";{};2;{r};{g};{b}", base + 8),
        Color::Indexed(i) => write!(s, ";{};5;{i}", base + 8),
        Color::Named(n) => {
            let n = n as usize;
            let dim = NamedColor::DimBlack as usize;
            match n {
                0..8 => write!(s, ";{}", base + n),
                8..16 => write!(s, ";{}", base + 60 + n - 8),
                _ if (dim..dim + 8).contains(&n) => write!(s, ";{}", base + n - dim),
                _ => Ok(()), // default foreground/background
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &Screen) -> Vec<String> {
        let g = s.term.grid();
        (0..g.screen_lines())
            .map(|r| (0..g.columns()).map(|c| g[Line(r as i32)][Column(c)].c).collect())
            .collect()
    }

    #[test]
    fn snapshot_reproduces_screen() {
        let mut a = Screen::new(20, 4);
        a.feed(b"hello\r\n\x1b[1;31mred\x1b[0m \x1b[48;5;22mbg\x1b[0m\r\n\xe4\xbd\xa0\xe5\xa5\xbd wide\x1b[2;3H\x1b[?2004h");
        let mut b = Screen::new(20, 4);
        b.feed(&a.snapshot());
        assert_eq!(text(&a), text(&b));
        let (ga, gb) = (a.term.grid(), b.term.grid());
        assert_eq!(ga.cursor.point, gb.cursor.point);
        assert_eq!(ga[Line(1)][Column(0)].fg, gb[Line(1)][Column(0)].fg);
        assert_eq!(ga[Line(1)][Column(4)].bg, gb[Line(1)][Column(4)].bg);
        assert!(b.term.mode().contains(TermMode::BRACKETED_PASTE));
    }
}

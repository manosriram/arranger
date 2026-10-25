//! Per-task terminal state, and the escape sequences that redraw it for a new viewer.

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, Rgb};
use std::cell::RefCell;
use std::fmt::Write;
use std::rc::Rc;

const SCROLLBACK: usize = 2000;

pub fn bounded_size(cols: u16, rows: u16) -> (u16, u16) {
    (cols.clamp(2, 400), rows.clamp(1, 200))
}

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
        let (cols, rows) = bounded_size(cols, rows);
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

    /// Moves the view `lines` into the scrollback (positive = up); 0 rows back is live.
    pub fn scroll(&mut self, lines: i32) {
        self.term.scroll_display(Scroll::Delta(lines));
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        let (cols, rows) = bounded_size(cols, rows);
        self.term.resize(Size { cols: cols.into(), rows: rows.into() });
    }

    /// Escape sequences that reproduce the scrollback, visible screen, cursor and input modes.
    pub fn snapshot(&self) -> Vec<u8> {
        let mode = *self.term.mode();
        let grid = self.term.grid();
        let mut s = String::new();
        if mode.contains(TermMode::ALT_SCREEN) {
            s.push_str("\x1b[?1049h");
        }
        // The scrollback first, so a new viewer can scroll back past the moment it attached: each
        // line printed, then enough blank lines to push them all off the top of the screen.
        // Soft-wrapped lines run on so they wrap (and reflow) as they did here. Clearing only after
        // that, on a blank screen, keeps alacritty from pushing a stray blank line into history.
        // ponytail: a history line wrapping onto the screen's first row loses its wrap mark.
        let history = grid.history_size();
        if history > 0 {
            s.push_str("\x1b[0m\x1b[H");
            for row in (1..=history).rev() {
                let line = &grid[Line(-(row as i32))];
                write_row(&mut s, line, grid.columns());
                if !line[Column(grid.columns() - 1)].flags.contains(Flags::WRAPLINE) {
                    s.push_str("\r\n");
                }
            }
            s.push_str(&"\r\n".repeat(grid.screen_lines() - 1));
        }
        s.push_str("\x1b[0m\x1b[H\x1b[2J");
        for row in 0..grid.screen_lines() {
            let _ = write!(s, "\x1b[{};1H", row + 1);
            write_row(&mut s, &grid[Line(row as i32)], grid.columns());
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

/// One row's text and colours, up to its last non-blank cell.
fn write_row(s: &mut String, line: &alacritty_terminal::grid::Row<Cell>, columns: usize) {
    let end = (0..columns).rposition(|c| !blank(&line[Column(c)])).map_or(0, |i| i + 1);
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

    #[test]
    fn terminal_dimensions_are_bounded_before_allocation() {
        assert_eq!(bounded_size(0, 0), (2, 1));
        let screen = Screen::new(u16::MAX, u16::MAX);
        assert_eq!((screen.grid().columns(), screen.grid().screen_lines()), (400, 200));
    }

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

    #[test]
    fn snapshot_carries_scrollback() {
        let mut a = Screen::new(10, 4);
        for i in 0..20 {
            a.feed(format!("line {i}\r\n").as_bytes());
        }
        a.feed(b"a long line that wraps\r\nmore\r\nand more\r\nyet more\r\nlast");
        let mut b = Screen::new(10, 4);
        b.feed(&a.snapshot());
        let all = |s: &Screen| {
            let g = s.term.grid();
            (-(g.history_size() as i32)..g.screen_lines() as i32)
                .map(|r| (0..g.columns()).map(|c| g[Line(r)][Column(c)].c).collect::<String>())
                .collect::<Vec<_>>()
        };
        assert_eq!(all(&a), all(&b));
        assert_eq!(a.term.grid().cursor.point, b.term.grid().cursor.point);
        let wraps = |s: &Screen| (1..=s.term.grid().history_size()).filter(|&r| s.term.grid()[Line(-(r as i32))][Column(9)].flags.contains(Flags::WRAPLINE)).count();
        assert_eq!(wraps(&a), 2);
        assert_eq!(wraps(&a), wraps(&b));
    }
}

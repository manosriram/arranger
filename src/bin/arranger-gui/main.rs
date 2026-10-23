//! `arranger-gui` (what `arranger` with no args runs): the Workspace window (docs/ui/Main.dc.html, NewTask.dc.html).
//! Immediate mode: every redraw lays out and draws everything, and resolves the click
//! that triggered it. Redraws happen only on input or daemon pushes, so idle CPU is zero.

mod render;
mod review;
mod theme;

use arranger::adapter;
use arranger::client::{connect, request};
use arranger::proto::{self, Event, Reply, Request, TaskInfo};
use arranger::screen::Screen;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color, NamedColor};
use glutin::config::ConfigTemplateBuilder;
use glutin::context::{ContextApi, ContextAttributesBuilder, GlProfile, NotCurrentGlContext, PossiblyCurrentContext, Version};
use glutin::display::{GetGlDisplay, GlDisplay};
use glutin::surface::{GlSurface, Surface, SwapInterval, WindowSurface};
use glutin_winit::{DisplayBuilder, GlWindow};
use raw_window_handle::HasWindowHandle;
use render::{MONO, Rect, Renderer, UI};
use std::io::{self, Write};
use std::net::Shutdown;
use std::num::NonZeroU32;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use review::{ReviewAction, ReviewView, Tag};
use theme::{AGENTS, GRAPHITE, PAPER, Rgba, Theme, accent, alpha, blend};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowAttributes, WindowId};

const TITLE_H: f32 = 36.0;
const STATUS_H: f32 = 22.0;
const SIDEBAR_W: f32 = 248.0;
const HEADER_H: f32 = 40.0;
const ROW_H: f32 = 28.0;
/// A row with its second line (what the agent is doing / did).
const ROW2_H: f32 = 42.0;
const GROUP_H: f32 = 24.0;
const TERM_SIZE: f32 = 12.0;
const LINE_H: f32 = 18.0;
const GROUPS: [&str; 5] = ["Needs you", "Running", "Queued", "Done", "Ended"];

pub enum Msg {
    Tasks(Vec<TaskInfo>),
    Output(u64, Vec<u8>),
    Exited(u64),
    /// A finished task's recorded PTY output.
    History(u64, Vec<u8>),
    Lost,
    Reply(Tag, Result<Reply, String>),
}

struct Gfx {
    window: Window,
    surface: Surface<WindowSurface>,
    context: PossiblyCurrentContext,
    r: Renderer,
}

/// The focused task's attach stream and its local copy of the terminal.
struct Session {
    seq: u64,
    task: u64,
    writer: UnixStream,
    screen: Screen,
    size: (u16, u16),
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.writer.shutdown(Shutdown::Both); // ends the reader thread too
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Focus {
    Term,
    Filter,
    Message,
    Prompt,
    Base,
    Note,
    CommitMsg,
}

struct Dialog {
    prompt: String,
    agent: usize,
    base: String,
    error: String,
}

enum Action {
    Select(u64),
    Focus(Focus),
    NewTask,
    ToggleTheme,
    Answer(bool),
    Input(Vec<u8>),
    PickAgent(usize),
    Start(bool),
    CloseDialog,
    OpenReview,
    Review(ReviewAction),
}

struct App {
    proxy: EventLoopProxy<Msg>,
    repo: PathBuf,
    gfx: Option<Gfx>,
    tasks: Vec<TaskInfo>,
    selected: Option<u64>,
    session: Option<Session>,
    seq: u64,
    pane: (u16, u16),
    dark: bool,
    focus: Focus,
    filter: String,
    message: String,
    dialog: Option<Dialog>,
    review: Option<ReviewView>,
    /// Replay of a finished task's log, shown in place of a live session.
    history: Option<(u64, Screen)>,
    /// Wheel movement over the terminal not yet worth a whole line.
    wheel: f32,
    mods: ModifiersState,
    mouse: (f32, f32),
    click: Option<(f32, f32)>,
    actions: Vec<Action>,
    scroll: f32,
    /// ⌘A in a text field: the next edit replaces the whole field.
    select_all: bool,
    caret_on: bool,
    blink_at: Instant,
    lost: bool,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("arranger: {e}");
        std::process::exit(1);
    }
}

fn run() -> io::Result<()> {
    let event_loop = EventLoop::<Msg>::with_user_event().build().map_err(io::Error::other)?;
    let proxy = event_loop.create_proxy();
    let mut watch = connect()?;
    watch.write_all(&proto::frame(&Request::Watch))?;
    let p = proxy.clone();
    thread::spawn(move || {
        while let Ok(Reply::Tasks(tasks)) = proto::read_msg(&mut watch) {
            if p.send_event(Msg::Tasks(tasks)).is_err() {
                return;
            }
        }
        let _ = p.send_event(Msg::Lost);
    });
    let mut app = App {
        proxy,
        repo: std::env::current_dir()?,
        gfx: None,
        tasks: Vec::new(),
        selected: None,
        session: None,
        seq: 0,
        pane: (80, 24),
        dark: true,
        focus: Focus::Term,
        filter: String::new(),
        message: String::new(),
        dialog: None,
        review: None,
        history: None,
        wheel: 0.0,
        mods: ModifiersState::empty(),
        mouse: (0.0, 0.0),
        click: None,
        actions: Vec::new(),
        scroll: 0.0,
        select_all: false,
        caret_on: true,
        blink_at: Instant::now(),
        lost: false,
    };
    event_loop.run_app(&mut app).map_err(io::Error::other)
}

fn init_gfx(el: &ActiveEventLoop) -> Result<Gfx, String> {
    let snapshot = std::env::var_os("ARRANGER_SNAPSHOT").is_some();
    let attrs = WindowAttributes::default()
        .with_title("arranger")
        .with_inner_size(LogicalSize::new(1280.0, 800.0))
        .with_visible(!snapshot) // dev snapshots render without showing a window or taking focus
        .with_active(!snapshot);
    let (window, config) = DisplayBuilder::new()
        .with_window_attributes(Some(attrs))
        .build(el, ConfigTemplateBuilder::new(), |mut configs| configs.next().expect("a GL config"))
        .map_err(|e| e.to_string())?;
    let window = window.ok_or("no window")?;
    let display = config.display();
    let handle = window.window_handle().map_err(|e| e.to_string())?.as_raw();
    let ctx = ContextAttributesBuilder::new()
        .with_context_api(ContextApi::OpenGl(Some(Version::new(3, 3))))
        .with_profile(GlProfile::Core)
        .build(Some(handle));
    let not_current = unsafe { display.create_context(&config, &ctx) }.map_err(|e| e.to_string())?;
    let surface_attrs = window.build_surface_attributes(Default::default()).map_err(|e| e.to_string())?;
    let surface = unsafe { display.create_window_surface(&config, &surface_attrs) }.map_err(|e| e.to_string())?;
    let context = not_current.make_current(&surface).map_err(|e| e.to_string())?;
    let _ = surface.set_swap_interval(&context, SwapInterval::Wait(NonZeroU32::MIN));
    let gl = unsafe { glow::Context::from_loader_function_cstr(|s| display.get_proc_address(s)) };
    Ok(Gfx { window, surface, context, r: Renderer::new(gl)? })
}

const BLINK: Duration = Duration::from_millis(530);

impl ApplicationHandler<Msg> for App {
    fn new_events(&mut self, _: &ActiveEventLoop, cause: StartCause) {
        if let StartCause::ResumeTimeReached { .. } = cause {
            self.caret_on = !self.caret_on;
            self.blink_at = Instant::now() + BLINK;
            self.redraw();
        }
    }

    /// Wakes only to blink the caret, and only while a text field has focus.
    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        if self.focus == Focus::Term {
            el.set_control_flow(ControlFlow::Wait);
        } else {
            el.set_control_flow(ControlFlow::WaitUntil(self.blink_at));
        }
    }

    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.gfx.is_some() {
            return;
        }
        match init_gfx(el) {
            Ok(gfx) => self.gfx = Some(gfx),
            Err(e) => {
                eprintln!("arranger: can't open a window: {e}");
                el.exit();
            }
        }
        el.set_control_flow(ControlFlow::Wait);
    }

    fn user_event(&mut self, _: &ActiveEventLoop, msg: Msg) {
        match msg {
            Msg::Tasks(tasks) => {
                self.tasks = tasks;
                if self.selected.is_none_or(|id| self.task(id).is_none()) {
                    self.selected = self.order().first().copied();
                }
                self.sync_session();
            }
            Msg::Output(seq, bytes) => {
                if let Some(s) = self.session.as_mut().filter(|s| s.seq == seq) {
                    let replies = s.screen.feed(&bytes);
                    if !replies.is_empty() {
                        let _ = s.writer.write_all(&proto::frame(&Request::Input(replies)));
                    }
                }
            }
            Msg::Exited(seq) => {
                if self.session.as_ref().is_some_and(|s| s.seq == seq) {
                    self.session = None;
                }
            }
            Msg::History(id, bytes) => {
                if self.selected == Some(id) {
                    // Full-screen agents (Claude Code) leave the alternate screen on exit, wiping the
                    // chat; stop at that point to keep their last frame.
                    let end = bytes.windows(8).rposition(|w| w == b"\x1b[?1049l").unwrap_or(bytes.len());
                    let (cols, rows) = last_size(&bytes[..end]).unwrap_or(self.pane);
                    let mut screen = Screen::new(cols, rows);
                    screen.feed(&bytes[..end]);
                    self.history = Some((id, screen));
                }
            }
            Msg::Lost => self.lost = true,
            Msg::Reply(tag, reply) => self.on_reply(tag, reply),
        }
        self.redraw();
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::Resized(size) => {
                if let Some(g) = &self.gfx {
                    if let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) {
                        g.surface.resize(&g.context, w, h);
                    }
                }
                self.redraw();
            }
            WindowEvent::ScaleFactorChanged { .. } => self.redraw(),
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::CursorMoved { position, .. } => {
                let dpi = self.gfx.as_ref().map_or(1.0, |g| g.window.scale_factor());
                self.mouse = ((position.x / dpi) as f32, (position.y / dpi) as f32);
            }
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } => {
                self.click = Some(self.mouse);
                self.redraw();
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y * ROW_H,
                    MouseScrollDelta::PixelDelta(p) => p.y as f32,
                };
                if let Some(rv) = self.review.as_mut().filter(|_| self.mouse.1 > TITLE_H + 56.0) {
                    rv.scroll = (rv.scroll - dy).max(0.0);
                    self.redraw();
                } else if self.mouse.0 < SIDEBAR_W {
                    self.scroll = (self.scroll - dy).max(0.0);
                    self.redraw();
                } else if self.review.is_none() {
                    self.wheel += dy / LINE_H;
                    let lines = self.wheel.trunc();
                    self.wheel -= lines;
                    let id = self.selected;
                    let screen = match (&mut self.session, &mut self.history) {
                        (Some(s), _) if Some(s.task) == id => Some(&mut s.screen),
                        (_, Some((h, screen))) if Some(*h) == id => Some(screen),
                        _ => None,
                    };
                    if let Some(screen) = screen.filter(|_| lines != 0.0) {
                        screen.scroll(lines as i32);
                        self.redraw();
                    }
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                self.caret_on = true;
                self.blink_at = Instant::now() + BLINK;
                self.key(&event);
                self.apply();
                self.redraw();
            }
            WindowEvent::RedrawRequested => self.draw(),
            _ => {}
        }
    }
}

// ---------- state ----------

fn is_live(state: &str) -> bool {
    matches!(state, "running" | "needs-you" | "done")
}

fn group(state: &str) -> usize {
    match state {
        "needs-you" => 0,
        "running" => 1,
        "queued" => 2,
        "done" => 3,
        _ => 4, // the agent process is gone: exited, dead, failed
    }
}

fn state_color(t: &Theme, state: &str) -> Rgba {
    [t.needs, t.work, t.faint, t.done, t.faint][group(state)]
}

fn title(task: &TaskInfo) -> String {
    let first = task.title.lines().next().unwrap_or("").trim();
    if first.is_empty() { format!("{} #{}", adapter::kind(&task.agent), task.id) } else { first.to_string() }
}

fn age(task: &TaskInfo) -> String {
    if task.state == "queued" || task.created == 0 {
        return "—".into();
    }
    ago(task.created)
}

/// "40s", "6m", "2h", "3d" since unix second `t`.
fn ago(t: u64) -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let s = now.saturating_sub(t);
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

/// The sidebar row's second line: what the agent is doing now, or what it last did.
fn status_line(task: &TaskInfo, queue_pos: usize) -> String {
    let since = if task.since == 0 { String::new() } else { format!(" · {}", ago(task.since)) };
    let waiting = if task.since == 0 { String::new() } else { format!(" · waiting {}", ago(task.since)) };
    match (group(&task.state), &task.need) {
        (0, Some(Event::PermissionRequest { tool, detail })) => match detail.strip_prefix("running ") {
            Some(cmd) => format!("$ {cmd}?{waiting}"),
            None => format!("{tool} · {detail}?{waiting}"),
        },
        (0, Some(Event::Question { text, .. })) => format!("{text}{waiting}"),
        (0, Some(Event::Error { message })) => message.clone(),
        (0, _) => format!("needs you{waiting}"),
        (1, _) if task.action.is_empty() => format!("working{since}"),
        (1, _) => format!("{}{since}", task.action),
        (2, _) => format!("#{queue_pos} in queue"),
        (3, _) if task.summary.is_empty() => "finished its turn".into(),
        (3, _) => format!("“{}”", task.summary),
        _ => match task.state.as_str() {
            "dead" => "died with the daemon · history kept".into(),
            "failed" => "failed to start".into(),
            _ => "exited · history kept".into(),
        },
    }
}

/// The header pill: why the task is where it is.
fn reason(task: &TaskInfo) -> String {
    match (&task.need, task.state.as_str()) {
        (Some(Event::PermissionRequest { .. }), _) => "Needs approval".into(),
        (Some(Event::Question { .. }), _) => "Asked a question".into(),
        (Some(Event::Error { .. }), _) => "Error".into(),
        (_, "needs-you") => "Needs you".into(),
        (_, "running") if !task.action.is_empty() => {
            let mut c = task.action.chars();
            c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
        }
        (_, s) => {
            let mut c = s.chars();
            c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
        }
    }
}

fn app_mod(m: ModifiersState) -> bool {
    if cfg!(target_os = "macos") { m.super_key() } else { m.control_key() && m.shift_key() }
}

fn mod_label(key: &str) -> String {
    if cfg!(target_os = "macos") { format!("⌘{key}") } else { format!("Ctrl⇧{key}") }
}

fn clipboard() -> String {
    let cmds: &[&[&str]] = if cfg!(target_os = "macos") {
        &[&["pbpaste"]]
    } else {
        &[&["wl-paste", "--no-newline"], &["xclip", "-o", "-selection", "clipboard"]]
    };
    cmds.iter()
        .find_map(|c| Command::new(c[0]).args(&c[1..]).output().ok().filter(|o| o.status.success()))
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// Bytes a terminal sends for a key press.
fn key_bytes(ev: &KeyEvent, mods: ModifiersState, app_cursor: bool) -> Option<Vec<u8>> {
    let arrow = |c: char| if app_cursor { format!("\x1bO{c}") } else { format!("\x1b[{c}") };
    let s = match &ev.logical_key {
        Key::Named(n) => match n {
            NamedKey::Enter => "\r".into(),
            NamedKey::Backspace => "\x7f".into(),
            NamedKey::Tab if mods.shift_key() => "\x1b[Z".into(),
            NamedKey::Tab => "\t".into(),
            NamedKey::Escape => "\x1b".into(),
            NamedKey::Space if mods.control_key() => "\0".into(),
            NamedKey::Space => " ".into(),
            NamedKey::ArrowUp => arrow('A'),
            NamedKey::ArrowDown => arrow('B'),
            NamedKey::ArrowRight => arrow('C'),
            NamedKey::ArrowLeft => arrow('D'),
            NamedKey::Home => "\x1b[H".into(),
            NamedKey::End => "\x1b[F".into(),
            NamedKey::PageUp => "\x1b[5~".into(),
            NamedKey::PageDown => "\x1b[6~".into(),
            NamedKey::Delete => "\x1b[3~".into(),
            NamedKey::Insert => "\x1b[2~".into(),
            NamedKey::F1 => "\x1bOP".into(),
            NamedKey::F2 => "\x1bOQ".into(),
            NamedKey::F3 => "\x1bOR".into(),
            NamedKey::F4 => "\x1bOS".into(),
            _ => return None,
        },
        Key::Character(c) if mods.control_key() => {
            let ch = c.chars().next()?.to_ascii_lowercase();
            let b = match ch {
                'a'..='z' => ch as u8 - b'a' + 1,
                '[' | '3' => 0x1b,
                '\\' | '4' => 0x1c,
                ']' | '5' => 0x1d,
                '^' | '6' => 0x1e,
                '_' | '-' | '7' => 0x1f,
                '@' | ' ' | '2' => 0,
                _ => return ev.text.as_ref().map(|t| t.as_bytes().to_vec()),
            };
            return Some(vec![b]);
        }
        Key::Character(c) => ev.text.as_deref().unwrap_or(c).to_string(),
        _ => return None,
    };
    let mut bytes = s.into_bytes();
    if mods.alt_key() && !cfg!(target_os = "macos") {
        bytes.insert(0, 0x1b); // Alt as Meta; macOS Option composes characters instead
    }
    Some(bytes)
}

fn term_color(t: &Theme, c: Color, fg: bool) -> Option<Rgba> {
    match c {
        Color::Spec(rgb) => Some([rgb.r, rgb.g, rgb.b, 255]),
        Color::Indexed(i) => Some(t.indexed(i)),
        Color::Named(n) => {
            let (i, dim) = (n as usize, NamedColor::DimBlack as usize);
            match n {
                NamedColor::Background => None,
                NamedColor::DimForeground => Some(t.text3),
                NamedColor::BrightForeground => Some(t.tc_on),
                _ if i < 16 => Some(t.ansi(i as u8)),
                _ if (dim..dim + 8).contains(&i) => Some(alpha(t.ansi((i - dim) as u8), 170)),
                _ if fg => Some(t.tc),
                _ => None,
            }
        }
    }
}

/// Splits `text` into lines no wider than `max_w`.
fn wrap(r: &Renderer, text: &str, font: usize, size: f32, max_w: f32) -> Vec<String> {
    let mut lines = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        for word in para.split(' ') {
            let candidate = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
            if !line.is_empty() && r.measure(font, size, 400, &candidate) > max_w {
                lines.push(std::mem::take(&mut line));
                line = word.to_string();
            } else {
                line = candidate;
            }
        }
        lines.push(line);
    }
    lines
}

impl App {
    fn theme(&self) -> &'static Theme {
        if self.dark { &GRAPHITE } else { &PAPER }
    }

    fn task(&self, id: u64) -> Option<&TaskInfo> {
        self.tasks.iter().find(|t| t.id == id)
    }

    fn current(&self) -> Option<&TaskInfo> {
        self.selected.and_then(|id| self.task(id))
    }

    fn visible(&self) -> Vec<&TaskInfo> {
        let f = self.filter.to_lowercase();
        let mut v: Vec<&TaskInfo> = self
            .tasks
            .iter()
            .filter(|t| f.is_empty() || format!("{} {} {}", title(t), t.agent, t.branch).to_lowercase().contains(&f))
            .collect();
        // Done: most recently finished first; everything else in start order.
        v.sort_by_key(|t| (group(&t.state), if group(&t.state) == 3 { u64::MAX - t.since } else { t.id }));
        v
    }

    /// Sidebar order, for keyboard switching.
    fn order(&self) -> Vec<u64> {
        self.visible().iter().map(|t| t.id).collect()
    }

    fn redraw(&self) {
        if let Some(g) = &self.gfx {
            g.window.request_redraw();
        }
    }

    fn sync_session(&mut self) {
        let want = self.current().filter(|t| is_live(&t.state)).map(|t| t.id);
        self.load_history();
        if self.session.as_ref().map(|s| s.task) == want {
            return;
        }
        self.session = None;
        let Some(id) = want else { return };
        let (cols, rows) = self.pane;
        let Ok(mut sock) = connect() else { return };
        if sock.write_all(&proto::frame(&Request::Attach { id, cols, rows, native: false })).is_err() {
            return;
        }
        let Ok(mut reader) = sock.try_clone() else { return };
        self.seq += 1;
        let (seq, p) = (self.seq, self.proxy.clone());
        thread::spawn(move || {
            while let Ok(Reply::Output(bytes)) = proto::read_msg(&mut reader) {
                if p.send_event(Msg::Output(seq, bytes)).is_err() {
                    return;
                }
            }
            let _ = p.send_event(Msg::Exited(seq));
        });
        self.session = Some(Session { seq, task: id, writer: sock, screen: Screen::new(cols, rows), size: (cols, rows) });
    }

    /// For a task that is no longer running, replays its PTY log so its chat stays readable.
    fn load_history(&mut self) {
        let Some(task) = self.current().filter(|t| !is_live(&t.state) && t.state != "queued") else {
            self.history = None;
            return;
        };
        if self.history.as_ref().is_some_and(|h| h.0 == task.id) {
            return;
        }
        let (id, p) = (task.id, self.proxy.clone());
        self.history = None;
        thread::spawn(move || {
            let path = proto::data_dir().join(format!("logs/{id}.zst"));
            if let Ok(bytes) = std::fs::File::open(path).and_then(zstd::decode_all) {
                let _ = p.send_event(Msg::History(id, bytes));
            }
        });
    }

    fn send_input(&mut self, bytes: Vec<u8>) {
        if let Some(s) = &mut self.session {
            let _ = s.writer.write_all(&proto::frame(&Request::Input(bytes)));
        }
    }

    fn move_selection(&mut self, d: i32) {
        let order = self.order();
        let i = self.selected.and_then(|id| order.iter().position(|&x| x == id)).unwrap_or(0) as i32;
        if let Some(&id) = order.get((i + d).clamp(0, order.len() as i32 - 1).max(0) as usize) {
            self.actions.push(Action::Select(id));
        }
    }

    fn open_dialog(&mut self) {
        let base = Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|| "HEAD".into());
        self.dialog = Some(Dialog { prompt: String::new(), agent: 0, base, error: String::new() });
        self.focus = Focus::Prompt;
        self.select_all = false;
    }

    /// Starts the dialog's task and switches to it; `attach` puts keys straight into its terminal.
    fn start(&mut self, attach: bool) {
        let Some(d) = &mut self.dialog else { return };
        let req = Request::New {
            agent: AGENTS[d.agent].into(),
            repo: self.repo.clone(),
            base: Some(d.base.trim().to_string()).filter(|b| !b.is_empty()),
            prompt: Some(d.prompt.trim().to_string()).filter(|p| !p.is_empty()),
        };
        match request(&req) {
            Ok(Reply::Created { id, .. }) => {
                self.dialog = None;
                self.focus = if attach { Focus::Term } else { Focus::Message };
                // Fetch the list now: the watch push may not have the new task yet, and selection
                // would fall back to the first row.
                if let Ok(Reply::Tasks(tasks)) = request(&Request::List { files: false }) {
                    self.tasks = tasks;
                }
                self.selected = Some(id);
                self.sync_session();
            }
            Ok(other) => d.error = format!("{other:?}"),
            Err(e) => d.error = e.to_string(),
        }
    }

    fn key(&mut self, ev: &KeyEvent) {
        let m = self.mods;
        if app_mod(m) {
            match &ev.logical_key {
                Key::Character(c) => match c.to_lowercase().as_str() {
                    "n" => self.actions.push(Action::NewTask),
                    "r" => self.actions.push(Action::OpenReview),
                    "j" | "k" if self.review.is_some() => {
                        let files = self.review.as_ref().and_then(|rv| rv.data.as_ref()).map_or(0, |d| d.files.len());
                        let cur = self.review.as_ref().map_or(0, |rv| rv.file);
                        let next = if c.to_lowercase() == "j" { (cur + 1).min(files.saturating_sub(1)) } else { cur.saturating_sub(1) };
                        self.actions.push(Action::Review(ReviewAction::File(next)));
                    }
                    "j" => self.move_selection(1),
                    "k" => self.move_selection(-1),
                    "1" => self.actions.push(Action::Answer(true)),
                    "2" => self.actions.push(Action::Answer(false)),
                    "t" => self.actions.push(Action::ToggleTheme),
                    "f" => self.actions.push(Action::Focus(Focus::Filter)),
                    "i" => self.actions.push(Action::Focus(Focus::Message)),
                    "v" => self.paste(),
                    "a" if self.focus != Focus::Term => self.select_all = true,
                    _ => {}
                },
                Key::Named(NamedKey::Backspace) if self.focus != Focus::Term => {
                    let focus = self.focus;
                    self.field(focus).clear();
                    self.select_all = false;
                }
                Key::Named(NamedKey::ArrowDown) => self.move_selection(1),
                Key::Named(NamedKey::ArrowUp) => self.move_selection(-1),
                Key::Named(NamedKey::Enter) if self.dialog.is_some() => self.actions.push(Action::Start(true)),
                Key::Named(NamedKey::Enter) if self.review.is_some() => {
                    let note = self.review.as_ref().and_then(|rv| rv.editing).filter(|_| self.focus == Focus::Note);
                    self.actions.push(Action::Review(note.map_or(ReviewAction::Commit(true), ReviewAction::SendNote)));
                }
                _ => {}
            }
            return;
        }
        if self.dialog.is_some() && ev.logical_key == Key::Named(NamedKey::Tab) {
            self.actions.push(Action::PickAgent(usize::MAX));
            return;
        }
        if self.focus == Focus::Term && self.review.is_some() && self.dialog.is_none() {
            if ev.logical_key == Key::Named(NamedKey::Escape) {
                self.actions.push(Action::Review(ReviewAction::Close));
            }
            return; // the agent's terminal isn't on screen
        }
        let typed = ev.text.as_ref().filter(|t| t.chars().any(|c| !c.is_control()) && !m.control_key() && !m.alt_key());
        if self.focus == Focus::Term && typed.is_some() && self.session.as_ref().and_then(|s| agent_box(&s.screen)).is_some() {
            // The agent's own input box is hidden behind ours: typing goes to ours.
            self.focus = Focus::Message;
        }
        if self.focus == Focus::Term {
            let app_cursor = self.session.as_ref().is_some_and(|s| s.screen.mode().contains(TermMode::APP_CURSOR));
            if let Some(bytes) = key_bytes(ev, m, app_cursor) {
                self.send_input(bytes);
            }
            return;
        }
        let focus = self.focus;
        let linux_ctrl = !cfg!(target_os = "macos") && m.control_key();
        if linux_ctrl && ev.logical_key == Key::Character("a".into()) {
            self.select_all = true;
            return;
        }
        let replace = std::mem::take(&mut self.select_all);
        match &ev.logical_key {
            Key::Named(NamedKey::Escape) => {
                if self.dialog.is_some() {
                    self.actions.push(Action::CloseDialog);
                } else if focus == Focus::Note {
                    self.actions.push(Action::Review(ReviewAction::SaveNote));
                } else {
                    self.focus = Focus::Term;
                }
            }
            Key::Named(NamedKey::Tab) if focus == Focus::Message && m.shift_key() => self.send_input(b"\x1b[Z".to_vec()), // cycles the agent's mode
            Key::Named(NamedKey::Enter) if matches!(focus, Focus::Prompt | Focus::Note) && m.shift_key() => self.field(focus).push('\n'),
            Key::Named(NamedKey::Enter) => match focus {
                Focus::Message => {
                    let text = std::mem::take(&mut self.message);
                    if let Some(id) = self.selected.filter(|_| !text.trim().is_empty()) {
                        self.spawn_request(Tag::Message, Request::Prompt { id, text });
                    }
                }
                Focus::Note => self.actions.push(Action::Review(ReviewAction::SaveNote)),
                Focus::CommitMsg => self.actions.push(Action::Review(ReviewAction::Commit(false))),
                Focus::Prompt | Focus::Base => self.actions.push(Action::Start(false)),
                _ => self.focus = Focus::Term,
            },
            Key::Named(NamedKey::Backspace) => {
                let field = self.field(focus);
                if replace {
                    field.clear();
                } else if m.alt_key() || linux_ctrl {
                    // Option/Alt/Ctrl+Backspace: delete the word before the caret.
                    let keep = field.trim_end().rfind(|c: char| c.is_whitespace()).map_or(0, |i| i + 1);
                    field.truncate(keep);
                } else {
                    field.pop();
                }
                self.scroll = 0.0;
            }
            _ => {
                if let Some(text) = ev.text.as_ref().filter(|t| t.chars().any(|c| !c.is_control())) {
                    let field = self.field(focus);
                    if replace {
                        field.clear();
                    }
                    field.extend(text.chars().filter(|c| !c.is_control()));
                } else {
                    self.select_all = replace; // modifiers and other keys keep the selection
                }
                self.scroll = 0.0;
            }
        }
    }

    fn field(&mut self, focus: Focus) -> &mut String {
        if focus == Focus::Note && self.note_text().is_some() {
            return self.note_text().unwrap();
        }
        if let (Focus::CommitMsg, Some(rv)) = (focus, &mut self.review) {
            return &mut rv.message;
        }
        match (focus, &mut self.dialog) {
            (Focus::Prompt, Some(d)) => &mut d.prompt,
            (Focus::Base, Some(d)) => &mut d.base,
            (Focus::Filter, _) => &mut self.filter,
            _ => &mut self.message,
        }
    }

    fn paste(&mut self) {
        let text = clipboard();
        if self.focus == Focus::Term {
            let bracketed = self.session.as_ref().is_some_and(|s| s.screen.mode().contains(TermMode::BRACKETED_PASTE));
            let bytes = if bracketed { format!("\x1b[200~{text}\x1b[201~") } else { text.replace('\n', "\r") };
            self.send_input(bytes.into_bytes());
        } else {
            let focus = self.focus;
            if std::mem::take(&mut self.select_all) {
                self.field(focus).clear();
            }
            self.field(focus).push_str(&text);
        }
    }

    fn apply(&mut self) {
        for action in std::mem::take(&mut self.actions) {
            match action {
                Action::Select(id) => {
                    self.selected = Some(id);
                    self.focus = Focus::Term;
                    self.sync_session();
                }
                Action::Focus(f) => {
                    self.select_all &= self.focus == f;
                    self.focus = f;
                }
                Action::NewTask => self.open_dialog(),
                Action::CloseDialog => {
                    self.dialog = None;
                    self.focus = Focus::Term;
                }
                Action::ToggleTheme => self.dark = !self.dark,
                Action::Answer(allow) => {
                    if let Some(id) = self.current().filter(|t| t.asking).map(|t| t.id) {
                        let _ = request(&Request::Answer { id, allow, message: String::new() });
                    }
                }
                Action::Input(bytes) => self.send_input(bytes),
                Action::PickAgent(i) => {
                    if let Some(d) = &mut self.dialog {
                        d.agent = if i == usize::MAX { (d.agent + 1) % AGENTS.len() } else { i };
                    }
                }
                Action::Start(attach) => self.start(attach),
                Action::OpenReview => {
                    if self.dialog.is_none() {
                        self.open_review();
                    }
                }
                Action::Review(a) => self.review_action(a),
            }
        }
    }

    /// Text field contents with the ⌘A highlight and a blinking caret at the end. Returns the width.
    fn field_text(&self, r: &mut Renderer, t: &Theme, x: f32, y: f32, s: &str, font: usize, size: f32, max_w: f32, focused: bool, caret: Rgba) -> f32 {
        let w = r.text(x, y, s, font, size, 400, t.text, max_w);
        let h = size * 1.35;
        if focused && self.select_all && !s.is_empty() {
            r.rect(Rect::new(x - 1.0, y - size * 0.15, w + 2.0, h), 2.0, alpha(t.work, 0x50));
        } else if focused && self.caret_on {
            r.rect(Rect::new(x + w + 1.0, y - size * 0.15, 1.5, h), 0.0, caret);
        }
        w
    }

    fn hit(&self, r: Rect) -> bool {
        self.click.is_some_and(|p| r.contains(p))
    }

    // ---------- drawing ----------

    fn draw(&mut self) {
        let Some(mut gfx) = self.gfx.take() else { return };
        let size = gfx.window.inner_size();
        let dpi = gfx.window.scale_factor() as f32;
        let (w, h) = (size.width as f32 / dpi, size.height as f32 / dpi);
        let t = self.theme();
        let r = &mut gfx.r;
        r.begin(size.width, size.height, dpi, t.bg);

        self.draw_title(r, t, w);
        if self.review.is_some() {
            self.draw_review(r, t, Rect::new(0.0, TITLE_H, w, h - TITLE_H - STATUS_H));
        } else {
            self.draw_sidebar(r, t, h);
            self.draw_main(r, t, Rect::new(SIDEBAR_W, TITLE_H, w - SIDEBAR_W, h - TITLE_H - STATUS_H));
        }
        self.draw_status(r, t, w, h);
        if self.dialog.is_some() {
            self.draw_dialog(r, t, w, h);
        }
        r.finish();
        if let Ok(path) = std::env::var("ARRANGER_SNAPSHOT") {
            let _ = r.snapshot(&path);
        }
        let _ = gfx.surface.swap_buffers(&gfx.context);
        self.gfx = Some(gfx);

        let clicked = self.click.take().is_some();
        let had_actions = !self.actions.is_empty();
        self.apply();
        if clicked && had_actions {
            self.redraw();
        }
    }

    fn draw_title(&mut self, r: &mut Renderer, t: &Theme, w: f32) {
        r.rect(Rect::new(0.0, TITLE_H - 1.0, w, 1.0), 0.0, t.border);
        logo(r, 12.0, 10.0, 16.0, t.text, t.needs);
        let bar = Rect::new(0.0, 0.0, w, TITLE_H);
        let x = 33.0 + r.text(33.0, r.center(bar, UI, 13.0), "arranger", UI, 13.0, 600, t.text, 200.0);
        let home = std::env::var("HOME").unwrap_or_default();
        let repo = self.repo.display().to_string();
        let repo = repo.strip_prefix(&home).map_or(repo.clone(), |rest| format!("~{rest}"));
        let rw = r.text(x + 10.0, r.center(bar, MONO, 11.0), &repo, MONO, 11.0, 400, t.text3, 400.0);

        // Views: Workspace | Review.
        let mut nx = x + 26.0 + rw;
        for (label, on) in [("Workspace", self.review.is_none()), ("Review", self.review.is_some())] {
            let tw = r.measure(UI, 12.0, 400, label) + 16.0;
            let tab = Rect::new(nx, 7.0, tw, 22.0);
            if on {
                r.rect(tab, 3.0, t.raised);
            }
            r.text(tab.x + 8.0, r.center(tab, UI, 12.0), label, UI, 12.0, 400, if on { t.text } else { t.text3 }, tw);
            if self.hit(tab) && !on {
                self.actions.push(if label == "Review" { Action::OpenReview } else { Action::Review(ReviewAction::Close) });
            }
            nx += tw + 1.0;
        }

        // Scheme toggle: [swatch] Graphite
        let label_w = r.measure(UI, 12.0, 400, t.name);
        let b = Rect::new(w - 10.0 - (label_w + 37.0), 5.0, label_w + 37.0, 26.0);
        r.frame(b, 3.0, t.bg, t.border);
        r.rect(Rect::new(b.x + 7.0, b.y + 6.0, 14.0, 14.0), 3.0, t.border2);
        r.rect(Rect::new(b.x + 8.0, b.y + 7.0, 6.0, 12.0), 2.0, t.bg);
        r.rect(Rect::new(b.x + 14.0, b.y + 7.0, 6.0, 12.0), 2.0, t.needs);
        r.text(b.x + 28.0, r.center(b, UI, 12.0), t.name, UI, 12.0, 400, t.text2, label_w + 4.0);
        if self.hit(b) {
            self.actions.push(Action::ToggleTheme);
        }
    }

    fn draw_sidebar(&mut self, r: &mut Renderer, t: &Theme, h: f32) {
        let side = Rect::new(0.0, TITLE_H, SIDEBAR_W, h - TITLE_H - STATUS_H);
        r.rect(side, 0.0, t.panel);
        r.rect(Rect::new(SIDEBAR_W - 1.0, TITLE_H, 1.0, side.h), 0.0, t.border);

        // Filter + new task.
        let f = Rect::new(8.0, TITLE_H + 8.0, SIDEBAR_W - 16.0 - 32.0, 26.0);
        let focused = self.focus == Focus::Filter;
        r.frame(f, 3.0, t.bg, if focused { t.work } else { t.border });
        for (i, lw) in [16.0, 10.0, 4.0].iter().enumerate() {
            r.rect(Rect::new(f.x + 8.0 + (16.0 - lw) / 2.0 * 0.7, f.y + 8.0 + i as f32 * 4.0, lw * 0.7, 1.4), 0.5, t.text3);
        }
        let n = self.tasks.len();
        if self.filter.is_empty() && !focused {
            r.text(f.x + 25.0, r.center(f, UI, 12.0), &format!("Filter {n} agents"), UI, 12.0, 400, t.text3, f.w - 50.0);
        } else {
            self.field_text(r, t, f.x + 25.0, r.center(f, UI, 12.0), &self.filter.clone(), UI, 12.0, f.w - 50.0, focused, t.work);
        }
        r.text(f.x + f.w - 8.0 - r.measure(MONO, 10.0, 400, &mod_label("F")), r.center(f, MONO, 10.0), &mod_label("F"), MONO, 10.0, 400, t.text3, 60.0);
        if self.hit(f) {
            self.actions.push(Action::Focus(Focus::Filter));
        }
        let plus = Rect::new(SIDEBAR_W - 8.0 - 26.0, f.y, 26.0, 26.0);
        r.rect(plus, 3.0, t.primary_bg);
        r.rect(Rect::new(plus.x + 7.5, plus.y + 12.0, 11.0, 2.0), 1.0, t.primary_fg);
        r.rect(Rect::new(plus.x + 12.0, plus.y + 7.5, 2.0, 11.0), 1.0, t.primary_fg);
        if self.hit(plus) {
            self.actions.push(Action::NewTask);
        }

        // Grouped rows.
        let list = Rect::new(0.0, f.y + f.h + 8.0, SIDEBAR_W - 1.0, side.y + side.h - (f.y + f.h + 8.0));
        r.set_clip(Some(list));
        let visible = self.visible();
        let mut y = list.y - self.scroll;
        let mut picks = Vec::new();
        for (g, label) in GROUPS.iter().enumerate() {
            let items: Vec<&TaskInfo> = visible.iter().copied().filter(|t| group(&t.state) == g).collect();
            if items.is_empty() {
                continue;
            }
            let color = [t.needs, t.work, t.faint, t.done, t.faint][g];
            let head = Rect::new(12.0, y + 6.0, SIDEBAR_W, GROUP_H - 6.0);
            r.rect(Rect::new(12.0, head.y + head.h / 2.0 - 2.5, 5.0, 5.0), 1.0, color);
            let lw = r.text(23.0, r.center(head, UI, 10.5), &label.to_uppercase(), UI, 10.5, 500, t.text3, 150.0);
            r.text(29.0 + lw, r.center(head, MONO, 10.5), &items.len().to_string(), MONO, 10.5, 400, t.faint, 40.0);
            y += GROUP_H;
            for (pos, task) in items.into_iter().enumerate() {
                let line = status_line(task, pos + 1);
                let row = Rect::new(6.0, y, SIDEBAR_W - 13.0, ROW2_H);
                let top = Rect::new(row.x, row.y + 2.0, row.w, 22.0);
                let on = self.selected == Some(task.id);
                let bg = if on { t.sel } else { t.panel };
                if on {
                    r.frame(row, 3.0, bg, t.sel_ring);
                }
                state_icon(r, t, &task.state, row.x + 8.0, top.y + top.h / 2.0, bg);
                // Right: how long it's been idle once done, else how long it has existed.
                let time = if g == 3 && task.since != 0 { ago(task.since) } else { age(task) };
                let tw = r.measure(MONO, 10.5, 400, &time);
                let text_x = row.x + 28.0;
                let ended = g == 4;
                let tc = if ended { t.text3 } else if on { t.tc_on } else { t.tc };
                r.text(text_x, r.center(top, UI, 12.5), &title(task), UI, 12.5, 400, tc, row.w - 28.0 - tw - 22.0);
                r.rect(Rect::new(row.x + row.w - 8.0 - tw - 11.0, top.y + top.h / 2.0 - 2.5, 5.0, 5.0), 1.0, accent(adapter::kind(&task.agent)));
                r.text(row.x + row.w - 8.0 - tw, r.center(top, MONO, 10.5), &time, MONO, 10.5, 400, t.text3, tw + 2.0);
                let sub = Rect::new(text_x, top.y + top.h - 3.0, row.w - 36.0, 16.0);
                let sc = match g {
                    0 => t.needs,
                    4 => t.faint,
                    _ => t.text3,
                };
                r.text(sub.x, r.center(sub, UI, 11.5), &line.replace('\n', " "), UI, 11.5, 400, sc, sub.w);
                if self.hit(row) && list.contains(self.click.unwrap()) {
                    picks.push(task.id);
                }
                y += ROW2_H;
            }
        }
        let content_h = y + self.scroll - list.y;
        self.scroll = self.scroll.min((content_h - list.h + 8.0).max(0.0));
        r.set_clip(None);
        if self.tasks.is_empty() {
            r.text(16.0, list.y + 10.0, &format!("No tasks yet · {} to start one", mod_label("N")), UI, 12.0, 400, t.text3, SIDEBAR_W - 24.0);
        }
        self.actions.extend(picks.into_iter().map(Action::Select));
    }

    fn draw_main(&mut self, r: &mut Renderer, t: &Theme, area: Rect) {
        let Some(task) = self.current().cloned() else {
            let msg = format!("Start a task with {}", mod_label("N"));
            let mw = r.measure(UI, 13.0, 400, &msg);
            logo(r, area.x + (area.w - 64.0) / 2.0, area.y + area.h / 2.0 - 84.0, 64.0, t.faint, t.needs);
            r.text(area.x + (area.w - mw) / 2.0, area.y + area.h / 2.0 - 8.0, &msg, UI, 13.0, 400, t.text3, area.w);
            return;
        };
        let color = state_color(t, &task.state);
        let tint = alpha(color, if t.dark { 0x14 } else { 0x12 });

        // Header.
        let head = Rect::new(area.x, area.y, area.w, HEADER_H);
        r.rect(Rect::new(area.x, area.y + HEADER_H - 1.0, area.w, 1.0), 0.0, t.border);
        let why = reason(&task);
        let pw = r.measure(UI, 11.0, 500, &why).min(260.0) + 23.0;
        let pill = Rect::new(area.x + 16.0, area.y + 10.0, pw, 20.0);
        r.rect(pill, 3.0, tint);
        r.rect(Rect::new(pill.x + 6.0, pill.y + 7.5, 5.0, 5.0), 1.0, color);
        r.text(pill.x + 16.0, r.center(pill, UI, 11.0), &why, UI, 11.0, 500, color, pw - 20.0);
        let kind = adapter::kind(&task.agent);
        let meta = format!("· {} · {}", task.branch, age(&task));
        let meta_w = r.measure(MONO, 11.0, 400, kind) + r.measure(MONO, 11.0, 400, &meta) + 4.0;
        let review_label = "Review";
        let review_key = mod_label("R");
        let bw = r.measure(UI, 12.0, 500, review_label) + r.measure(MONO, 10.5, 400, &review_key) + 28.0;
        let review = Rect::new(head.x + head.w - 12.0 - bw, head.y + 7.0, bw, 26.0);
        r.rect(review, 3.0, t.primary_bg);
        let rlw = r.text(review.x + 10.0, r.center(review, UI, 12.0), review_label, UI, 12.0, 500, t.primary_fg, bw);
        r.text(review.x + 18.0 + rlw, r.center(review, MONO, 10.5), &review_key, MONO, 10.5, 400, alpha(t.primary_fg, 160), 60.0);
        if self.hit(review) {
            self.actions.push(Action::OpenReview);
        }
        let tx = pill.x + pw + 10.0;
        let title_w = r.text(tx, r.center(head, UI, 13.5), &title(&task), UI, 13.5, 500, t.text, (head.w - (tx - head.x) - meta_w - 26.0 - bw).max(40.0));
        let mx = tx + title_w + 10.0;
        let kw = r.text(mx, r.center(head, MONO, 11.0), kind, MONO, 11.0, 400, accent(kind), 100.0);
        r.text(mx + kw + 4.0, r.center(head, MONO, 11.0), &meta, MONO, 11.0, 400, t.text3, meta_w);

        // Message box at the bottom.
        let input_area = Rect::new(area.x, area.y + area.h - 49.0, area.w, 49.0);
        r.rect(Rect::new(area.x, input_area.y, area.w, 1.0), 0.0, t.border);
        let ib = Rect::new(area.x + 10.0, input_area.y + 9.0, area.w - 20.0, 32.0);
        let focused = self.focus == Focus::Message;
        r.frame(ib, 4.0, t.panel, if focused { t.work } else { t.border2 });
        r.text(ib.x + 10.0, r.center(ib, MONO, 12.5), "›", MONO, 12.5, 400, t.text3, 20.0);
        // The agent's mode (read from its hidden footer); click or ⇧Tab cycles it.
        let mut text_w = ib.w - 60.0;
        if let Some((_, mode)) = self.session.as_ref().filter(|s| s.task == task.id).and_then(|s| agent_box(&s.screen)) {
            let label = if mode.is_empty() { "default mode".to_string() } else { mode };
            let cw = r.measure(MONO, 10.5, 400, &label) + r.measure(UI, 10.0, 400, "⇧Tab") + 26.0;
            let chip = Rect::new(ib.x + ib.w - 34.0 - cw, ib.y + 6.0, cw, 20.0);
            r.rect(chip, 3.0, t.raised);
            let lw = r.text(chip.x + 8.0, r.center(chip, MONO, 10.5), &label, MONO, 10.5, 400, t.text2, cw);
            r.text(chip.x + 16.0 + lw, r.center(chip, UI, 10.0), "⇧Tab", UI, 10.0, 400, t.text3, 40.0);
            if self.hit(chip) {
                self.actions.push(Action::Input(b"\x1b[Z".to_vec()));
            }
            text_w -= cw + 8.0;
        }
        if self.message.is_empty() && !focused {
            r.text(ib.x + 26.0, r.center(ib, UI, 12.5), &format!("Message {kind}  ·  {}", mod_label("I")), UI, 12.5, 400, t.text3, text_w);
        } else {
            self.field_text(r, t, ib.x + 26.0, r.center(ib, UI, 12.5), &self.message.clone(), UI, 12.5, text_w, focused, t.work);
        }
        r.text(ib.x + ib.w - 22.0, r.center(ib, MONO, 10.5), "↵", MONO, 10.5, 400, t.text3, 20.0);
        if self.hit(ib) && self.actions.is_empty() {
            self.actions.push(Action::Focus(Focus::Message));
        }

        // Terminal section.
        let sec = Rect::new(area.x, area.y + HEADER_H, area.w, input_area.y - area.y - HEADER_H);
        r.rect(sec, 0.0, t.term);
        if self.hit(sec) {
            self.actions.push(Action::Focus(Focus::Term));
        }
        let bar_h = if task.need.is_some() { 48.0 } else { 0.0 };
        let grid = Rect::new(sec.x + 16.0, sec.y + 12.0, sec.w - 32.0, sec.h - 28.0 - bar_h);
        let cell_w = r.advance(MONO, TERM_SIZE, 400, '0');
        let size = (((grid.w / cell_w) as u16).max(2), ((grid.h / LINE_H) as u16).max(1));
        self.pane = size;
        let term_focus = self.focus == Focus::Term && self.dialog.is_none();
        let screen = match (&mut self.session, &mut self.history) {
            (Some(s), _) if s.task == task.id => {
                if s.size != size {
                    s.size = size;
                    s.screen.resize(size.0, size.1);
                    let _ = s.writer.write_all(&proto::frame(&Request::Resize { cols: size.0, rows: size.1 }));
                }
                Some(&s.screen)
            }
            (_, Some((id, screen))) if *id == task.id => Some(&*screen), // kept at the size it was drawn for
            _ => None,
        };
        match screen {
            Some(screen) => {
                // Stop above the agent's own input box: ours below replaces it.
                let rows = agent_box(screen).map_or(screen.grid().screen_lines(), |(row, _)| row) as f32;
                // A replay taller than the pane keeps its last text in view.
                let last = (0..rows as usize).rev().find(|&row| (0..screen.grid().columns()).any(|c| screen.grid()[Line(row as i32)][Column(c)].c != ' '));
                let lift = (last.map_or(0.0, |l| l as f32 + 1.0) * LINE_H - grid.h).max(0.0);
                r.set_clip(Some(Rect::new(sec.x, sec.y, sec.w, (sec.h - bar_h).min(grid.y - sec.y + rows * LINE_H - lift))));
                draw_term(r, t, screen, Rect { y: grid.y - lift, ..grid }, cell_w, term_focus);
                r.set_clip(None);
            }
            None => {
                let note = match task.state.as_str() {
                    "queued" => "Queued · starts when a slot frees up".to_string(),
                    "exited" => "Agent exited".into(),
                    "dead" => "Agent died with the daemon (no output recorded)".into(),
                    "failed" => "Agent failed to start".into(),
                    _ => "Connecting…".into(),
                };
                r.text(grid.x, grid.y, &note, MONO, TERM_SIZE, 400, t.text3, grid.w);
            }
        }
        if let Some(need) = &task.need {
            let bar = Rect::new(sec.x + 16.0, sec.y + sec.h - 16.0 - 38.0, sec.w - 32.0, 38.0);
            self.draw_need(r, t, bar, &task, need, color, tint);
        }
    }

    fn draw_need(&mut self, r: &mut Renderer, t: &Theme, bar: Rect, task: &TaskInfo, need: &Event, color: Rgba, tint: Rgba) {
        r.frame(bar, 4.0, t.term, alpha(color, 0x4D));
        r.rect(bar.inset(1.0), 3.0, tint);
        let (label, text, buttons): (&str, String, Vec<(String, String, Action)>) = match need {
            Event::PermissionRequest { tool, detail } => {
                let text = match detail.strip_prefix("running ") {
                    Some(cmd) => format!("$ {cmd}"),
                    None => format!("{tool} · {detail}"),
                };
                let buttons = if task.asking {
                    vec![
                        ("Allow".into(), mod_label("1"), Action::Answer(true)),
                        ("Deny".into(), mod_label("2"), Action::Answer(false)),
                    ]
                } else {
                    Vec::new()
                };
                (if tool == "Bash" { "RUN?" } else { "ALLOW?" }, text, buttons)
            }
            Event::Question { text, options } => {
                let buttons = options
                    .iter()
                    .enumerate()
                    .take(4)
                    .map(|(i, o)| (o.clone(), (i + 1).to_string(), Action::Input((i + 1).to_string().into_bytes())))
                    .collect();
                ("ASKS", text.clone(), buttons)
            }
            Event::Error { message } => ("ERROR", message.clone(), Vec::new()),
            other => ("NOTICE", other.describe(), Vec::new()),
        };
        let lw = r.text(bar.x + 10.0, r.center(bar, UI, 11.0), label, UI, 11.0, 600, color, 80.0);
        let mut bx = bar.x + bar.w - 8.0;
        let mut rects = Vec::new();
        for (i, (name, key, _)) in buttons.iter().enumerate().rev() {
            let bw = r.measure(UI, 12.0, 500, name).min(160.0) + r.measure(MONO, 10.0, 400, key) + 22.0;
            bx -= bw;
            let b = Rect::new(bx, bar.y + 7.0, bw, 24.0);
            let primary = i == 0 && matches!(need, Event::PermissionRequest { .. });
            if primary {
                r.rect(b, 3.0, color);
            } else {
                r.frame(b, 3.0, t.bg, t.border2);
            }
            let fg = if primary { t.on_accent } else { t.text };
            let nw = r.text(b.x + 8.0, r.center(b, UI, 12.0), name, UI, 12.0, 500, fg, 160.0);
            r.text(b.x + 14.0 + nw, r.center(b, MONO, 10.0), key, MONO, 10.0, 400, if primary { alpha(fg, 150) } else { t.text3 }, 60.0);
            rects.push((i, b));
            bx -= 6.0;
        }
        if !task.asking && matches!(need, Event::PermissionRequest { .. }) {
            let note = "answer in the agent's terminal";
            let nw = r.measure(UI, 11.0, 400, note);
            bx -= nw;
            r.text(bx, r.center(bar, UI, 11.0), note, UI, 11.0, 400, t.text3, nw + 2.0);
        }
        let font = if matches!(need, Event::PermissionRequest { .. }) { MONO } else { UI };
        r.text(bar.x + 20.0 + lw, r.center(bar, font, 12.0), &text.replace('\n', " "), font, 12.0, 400, t.text, bx - (bar.x + 28.0 + lw));
        for (i, (_, _, action)) in buttons.into_iter().enumerate() {
            if rects.iter().any(|(j, b)| *j == i && self.hit(*b)) {
                self.actions.push(action);
            }
        }
    }

    fn draw_status(&mut self, r: &mut Renderer, t: &Theme, w: f32, h: f32) {
        let bar = Rect::new(0.0, h - STATUS_H, w, STATUS_H);
        r.rect(Rect::new(0.0, bar.y, w, 1.0), 0.0, t.border);
        let mut counts = [0; 4];
        for task in &self.tasks {
            counts[group(&task.state).min(3)] += 1;
        }
        let mut x = 12.0;
        let y = r.center(bar, MONO, 10.5);
        for (i, (sym, label)) in [("■", "need you"), ("●", "running"), ("○", "queued"), ("✓", "done")].iter().enumerate() {
            let c = [t.needs, t.work, t.text3, t.done][i];
            x += r.text(x, y, &format!("{sym} {} {label}", counts[i]), MONO, 10.5, 400, c, 200.0) + 14.0;
        }
        let right = if self.lost { "daemon disconnected".to_string() } else { format!("{} new · {} theme", mod_label("N"), mod_label("T")) };
        let rw = r.measure(MONO, 10.5, 400, &right);
        r.text(w - 12.0 - rw, y, &right, MONO, 10.5, 400, if self.lost { t.del } else { t.text3 }, rw + 2.0);
    }

    fn draw_dialog(&mut self, r: &mut Renderer, t: &Theme, w: f32, h: f32) {
        let Some(d) = &self.dialog else { return };
        let (prompt, agent_i, base, error) = (d.prompt.clone(), d.agent, d.base.clone(), d.error.clone());
        let agent = AGENTS[agent_i];
        let a = accent(agent);
        r.rect(Rect::new(0.0, 0.0, w, h), 0.0, t.scrim);
        let dw = (w - 32.0).min(680.0);
        let dx = (w - dw) / 2.0;
        let prompt_lines = wrap(r, &prompt, UI, 18.0, dw - 36.0);
        let text_h = (prompt_lines.len().max(3) as f32) * 27.0;
        let dh = 2.0 + 34.0 + 12.0 + text_h + 16.0 + 78.0 + 58.0;
        let dlg = Rect::new(dx, 88.0, dw, dh);
        r.rect(Rect::new(dlg.x - 1.0, dlg.y - 1.0, dlg.w + 2.0, dlg.h + 2.0), 9.0, alpha(a, 0x33));
        r.frame(dlg, 8.0, t.dialog, t.border2);
        r.rect(Rect::new(dlg.x + 1.0, dlg.y + 1.0, dlg.w - 2.0, 2.0), 1.0, a);

        // Header: NEW TASK  ~/repo        [agent]  Esc
        let hy = dlg.y + 12.0;
        let head = Rect::new(dlg.x + 18.0, hy, dlg.w - 36.0, 20.0);
        let lw = r.text(head.x, r.center(head, UI, 11.0), "NEW TASK", UI, 11.0, 500, t.text3, 100.0);
        let repo = self.repo.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        r.text(head.x + lw + 10.0, r.center(head, MONO, 11.0), &repo, MONO, 11.0, 400, t.faint, 200.0);
        let esc = Rect::new(head.x + head.w - 30.0, head.y + 2.0, 30.0, 16.0);
        r.frame(esc, 3.0, t.dialog, t.border2);
        r.text(esc.x + 6.0, r.center(esc, MONO, 10.0), "Esc", MONO, 10.0, 400, t.text3, 30.0);
        if self.hit(esc) {
            self.actions.push(Action::CloseDialog);
        }
        let pw = r.measure(MONO, 11.0, 400, agent) + 21.0;
        let pill = Rect::new(esc.x - 8.0 - pw, head.y, pw, 20.0);
        r.rect(pill, 3.0, alpha(a, 0x1A));
        r.rect(Rect::new(pill.x + 7.0, pill.y + 7.0, 6.0, 6.0), 1.0, a);
        r.text(pill.x + 17.0, r.center(pill, MONO, 11.0), agent, MONO, 11.0, 400, a, pw);

        // Prompt.
        let ty = hy + 20.0 + 14.0;
        let prompt_focus = self.focus == Focus::Prompt;
        if prompt.is_empty() {
            r.text(dlg.x + 18.0, ty + 4.0, "Describe the task", UI, 18.0, 400, t.faint, dlg.w - 36.0);
        }
        for (i, line) in prompt_lines.iter().enumerate() {
            let last = i == prompt_lines.len() - 1;
            let y = ty + 4.0 + i as f32 * 27.0;
            if last {
                self.field_text(r, t, dlg.x + 18.0, y, line, UI, 18.0, dlg.w - 36.0, prompt_focus, a);
            } else {
                let lw = r.text(dlg.x + 18.0, y, line, UI, 18.0, 400, t.text, dlg.w - 36.0);
                if prompt_focus && self.select_all {
                    r.rect(Rect::new(dlg.x + 17.0, y - 2.0, lw + 2.0, 24.0), 2.0, alpha(t.work, 0x50));
                }
            }
        }
        let prompt_area = Rect::new(dlg.x, ty, dlg.w, text_h + 16.0);
        if self.hit(prompt_area) {
            self.actions.push(Action::Focus(Focus::Prompt));
        }

        // Agent | Branch.
        let gy = ty + text_h + 16.0;
        r.rect(Rect::new(dlg.x + 1.0, gy, dlg.w - 2.0, 1.0), 0.0, t.border);
        r.rect(Rect::new(dlg.x + dlg.w / 2.0, gy, 1.0, 78.0), 0.0, t.border);
        r.text(dlg.x + 18.0, gy + 14.0, "Agent", UI, 11.0, 400, t.text3, 100.0);
        let mut ax = dlg.x + 18.0;
        for (i, name) in AGENTS.iter().enumerate() {
            let c = accent(name);
            let on = i == agent_i;
            let bw = r.measure(MONO, 12.0, 400, name) + 33.0;
            let b = Rect::new(ax, gy + 36.0, bw, 28.0);
            r.frame(b, 4.0, t.dialog, if on { c } else { t.border2 });
            if on {
                r.rect(b.inset(1.0), 3.0, alpha(c, 0x1F));
            }
            r.rect(Rect::new(b.x + 10.0, b.y + 11.0, 6.0, 6.0), 1.0, if on { c } else { alpha(c, 140) });
            r.text(b.x + 23.0, r.center(b, MONO, 12.0), name, MONO, 12.0, 400, if on { c } else { t.tc }, bw);
            if self.hit(b) {
                self.actions.push(Action::PickAgent(i));
            }
            ax += bw + 4.0;
        }
        let bx = dlg.x + dlg.w / 2.0 + 18.0;
        r.text(bx, gy + 14.0, "Branch", UI, 11.0, 400, t.text3, 100.0);
        let row = Rect::new(bx, gy + 36.0, dlg.w / 2.0 - 36.0, 28.0);
        let base_focus = self.focus == Focus::Base;
        let bw = if base_focus {
            self.field_text(r, t, row.x, r.center(row, MONO, 12.0), &base, MONO, 12.0, 160.0, true, a) + 4.0
        } else {
            r.text(row.x, r.center(row, MONO, 12.0), &base, MONO, 12.0, 400, t.text3, 160.0)
        };
        if self.hit(Rect::new(row.x - 4.0, row.y, bw + 8.0, row.h)) {
            self.actions.push(Action::Focus(Focus::Base));
        }
        let arrow_x = row.x + bw + 8.0;
        r.text(arrow_x, r.center(row, MONO, 12.0), "→", MONO, 12.0, 400, t.faint, 20.0);
        let nw = r.text(arrow_x + 18.0, r.center(row, MONO, 12.0), "arranger/<id>", MONO, 12.0, 400, t.text, 140.0);
        let chip = Rect::new(arrow_x + 26.0 + nw, row.y + 6.0, 32.0, 16.0);
        r.frame(chip, 3.0, t.dialog, t.border2);
        r.text(chip.x + 5.0, r.center(chip, UI, 10.0), "auto", UI, 10.0, 400, t.text3, 30.0);

        // Footer.
        let fy = gy + 78.0;
        let foot = Rect::new(dlg.x + 1.0, fy, dlg.w - 2.0, dlg.y + dlg.h - fy - 1.0);
        r.rect(Rect::new(dlg.x + 1.0, fy, dlg.w - 2.0, 1.0), 0.0, t.border);
        r.rect(Rect::new(foot.x, foot.y + 1.0, foot.w, foot.h - 1.0), 7.0, t.panel);
        let start_label = format!("Start {agent}");
        let sw = r.measure(UI, 13.0, 500, &start_label) + 40.0;
        let start = Rect::new(foot.x + foot.w - 14.0 - sw, foot.y + 12.0, sw, 34.0);
        r.rect(start, 4.0, a);
        let slw = r.text(start.x + 14.0, r.center(start, UI, 13.0), &start_label, UI, 13.0, 500, t.on_accent, sw);
        r.text(start.x + 20.0 + slw, r.center(start, MONO, 10.0), "↵", MONO, 10.0, 400, alpha(t.on_accent, 150), 20.0);
        let attach_key = mod_label("↵");
        let aw = r.measure(UI, 13.0, 400, "Start & attach") + r.measure(MONO, 10.0, 400, &attach_key) + 32.0;
        let attach = Rect::new(start.x - 8.0 - aw, start.y, aw, 34.0);
        r.frame(attach, 4.0, t.panel, t.border2);
        let alw = r.text(attach.x + 12.0, r.center(attach, UI, 13.0), "Start & attach", UI, 13.0, 400, t.text, aw);
        r.text(attach.x + 20.0 + alw, r.center(attach, MONO, 10.0), &attach_key, MONO, 10.0, 400, t.text3, 60.0);
        if !error.is_empty() {
            r.text(foot.x + 18.0, r.center(start, UI, 12.0), &error, UI, 12.0, 400, t.del, attach.x - foot.x - 30.0);
        }
        if self.hit(start) {
            self.actions.push(Action::Start(false));
        }
        if self.hit(attach) {
            self.actions.push(Action::Start(true));
        }
    }
}

/// State icon in a 12px box at (x, cy): square, ring, faint ring, check.
fn state_icon(r: &mut Renderer, t: &Theme, state: &str, x: f32, cy: f32, bg: Rgba) {
    let c = state_color(t, state);
    match group(state) {
        0 => r.rect(Rect::new(x + 2.0, cy - 4.0, 8.0, 8.0), 1.0, c),
        1 | 2 => {
            let (rad, stroke) = if group(state) == 1 { (4.5, 1.6) } else { (4.0, 1.1) };
            r.rect(Rect::new(x + 6.0 - rad, cy - rad, rad * 2.0, rad * 2.0), rad, c);
            let inner = rad - stroke;
            r.rect(Rect::new(x + 6.0 - inner, cy - inner, inner * 2.0, inner * 2.0), inner, bg);
        }
        _ => {
            let (a, _) = r.vmetrics(UI, 12.0);
            let color = if matches!(state, "dead" | "failed") { t.faint } else { c };
            r.glyph_char(UI, '✓', x, cy - 6.0 + a * 0.85, 12.0, 600, color);
        }
    }
}

/// The last terminal size recorded in a task log (the daemon writes `ESC [8;rows;cols t` on resize).
fn last_size(log: &[u8]) -> Option<(u16, u16)> {
    let at = log.windows(4).rposition(|w| w == b"\x1b[8;")?;
    let rest = &log[at + 4..];
    let end = rest.iter().take(12).position(|&b| b == b't')?;
    let (rows, cols) = std::str::from_utf8(&rest[..end]).ok()?.split_once(';')?;
    Some((cols.parse().ok()?, rows.parse().ok()?))
}

/// The agent's own input box at the bottom of its screen (Claude Code: a rule, `❯ input`, a rule,
/// then a footer). Returns the box's first row and the mode named in the footer ("" if none).
/// None when the screen is scrolled back or shows something else there (a menu, a dialog).
fn agent_box(screen: &Screen) -> Option<(usize, String)> {
    let grid = screen.grid();
    if grid.display_offset() != 0 {
        return None;
    }
    let rows: Vec<String> = (0..grid.screen_lines())
        .map(|row| (0..grid.columns()).map(|c| grid[Line(row as i32)][Column(c)].c).collect())
        .collect();
    let rule = |s: &String| s.trim().chars().count() >= 20 && s.trim().chars().all(|c| c == '─');
    let lower = rows.iter().rposition(rule)?;
    let upper = rows[..lower].iter().rposition(rule)?;
    if lower - upper > 12 || !rows[upper + 1].trim_start().starts_with(['❯', '>']) {
        return None;
    }
    // Footer: "-- INSERT --  ⏵⏵ auto mode on (shift+tab to cycle) · …"
    let mode = rows[lower + 1..]
        .iter()
        .find_map(|r| r.find("(shift+tab to cycle)").map(|i| r[..i].rsplit("--").next().unwrap_or("").trim_matches(|c: char| !c.is_alphanumeric()).to_string()))
        .unwrap_or_default();
    Some((upper, mode))
}

/// The arranger mark ("Ensemble"): arcs of players around an amber conductor, from the logo's SVG.
/// `size` is the mark's box; below 24 px it uses the solid two-arc variant, above it the dotted one.
fn logo(r: &mut Renderer, x: f32, y: f32, size: f32, ink: Rgba, amber: Rgba) {
    if size < 24.0 {
        r.path(1, "M5 45A27 27 0 0 1 59 45M19 45A13 13 0 0 1 45 45", Some(7.0), x, y, size, ink);
        r.path(2, "M26 46a6 6 0 1 0 12 0a6 6 0 1 0 -12 0", None, x, y, size, amber);
        return;
    }
    // Dots of 5.5 units spaced evenly along each arc: what the SVG's `stroke-dasharray="0 …"` draws.
    let mut dots = String::new();
    for (radius, n) in [(28.0f32, 10), (19.0, 7), (10.0, 4)] {
        for i in 0..=n {
            let a = std::f32::consts::PI * (1.0 - i as f32 / n as f32);
            let (cx, cy) = (32.0 + radius * a.cos(), 45.0 - radius * a.sin());
            dots += &format!("M{} {cy}a2.75 2.75 0 1 0 5.5 0a2.75 2.75 0 1 0 -5.5 0", cx - 2.75);
        }
    }
    r.path(3, &dots, None, x, y, size, ink);
    r.path(4, "M27.5 45a4.5 4.5 0 1 0 9 0a4.5 4.5 0 1 0 -9 0", None, x, y, size, amber);
}

fn draw_term(r: &mut Renderer, t: &Theme, screen: &Screen, area: Rect, cell_w: f32, focused: bool) {
    let grid = screen.grid();
    let mode = screen.mode();
    let (asc, desc) = r.vmetrics(MONO, TERM_SIZE);
    let base = (LINE_H - (asc + desc)) / 2.0 + asc;
    let offset = grid.display_offset() as i32;
    for row in 0..grid.screen_lines() {
        let line = &grid[Line(row as i32 - offset)];
        let y = area.y + row as f32 * LINE_H;
        for col in 0..grid.columns() {
            let cell = &line[Column(col)];
            if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                continue;
            }
            let mut fg = term_color(t, cell.fg, true).unwrap_or(t.tc);
            let mut bg = term_color(t, cell.bg, false);
            if cell.flags.contains(Flags::INVERSE) {
                (fg, bg) = (bg.unwrap_or(t.term), Some(fg));
            }
            if cell.flags.contains(Flags::DIM) {
                fg = alpha(fg, 150);
            }
            let x = area.x + col as f32 * cell_w;
            let width = if cell.flags.contains(Flags::WIDE_CHAR) { cell_w * 2.0 } else { cell_w };
            if let Some(bg) = bg {
                r.rect(Rect::new(x, y, width, LINE_H), 0.0, bg);
            }
            if cell.c != ' ' && !cell.flags.contains(Flags::HIDDEN) {
                let weight = if cell.flags.contains(Flags::BOLD) { 600 } else { 400 };
                r.glyph_char(MONO, cell.c, x, y + base, TERM_SIZE, weight, fg);
            }
            if cell.flags.intersects(Flags::ALL_UNDERLINES) {
                r.rect(Rect::new(x, y + base + 2.0, width, 1.0), 0.0, fg);
            }
        }
    }
    if mode.contains(TermMode::SHOW_CURSOR) && offset == 0 {
        let p = grid.cursor.point;
        let cur = Rect::new(area.x + p.column.0 as f32 * cell_w, area.y + p.line.0 as f32 * LINE_H, cell_w, LINE_H);
        if focused {
            r.rect(cur, 0.0, alpha(t.text, 120));
        } else {
            let c = alpha(t.text, 120);
            for side in [
                Rect::new(cur.x, cur.y, cur.w, 1.0),
                Rect::new(cur.x, cur.y + cur.h - 1.0, cur.w, 1.0),
                Rect::new(cur.x, cur.y, 1.0, cur.h),
                Rect::new(cur.x + cur.w - 1.0, cur.y, 1.0, cur.h),
            ] {
                r.rect(side, 0.0, c);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_claude_input_box_and_mode() {
        let rule = "─".repeat(40);
        let mut s = Screen::new(60, 8);
        s.feed(format!("chat\r\n\r\n{rule}\r\n❯ draft\r\n{rule}\r\n-- INSERT --  ⏵⏵ auto mode on (shift+tab to cycle) · ← for agents").as_bytes());
        assert_eq!(agent_box(&s), Some((2, "auto mode on".to_string())));

        let mut menu = Screen::new(60, 8);
        menu.feed(format!("{rule}\r\nAccessing workspace:\r\n❯ 1. Yes\r\n{rule}\r\n").as_bytes());
        assert_eq!(agent_box(&menu), None);
        assert_eq!(last_size(b"x\x1b[8;24;80tyy\x1b[8;62;236tzz"), Some((236, 62)));
    }
}

//! `arranger-gui` (what `arranger` with no args runs): the Workspace window (docs/ui/Main.dc.html, NewTask.dc.html).
//! Immediate mode: every redraw lays out and draws everything, and resolves the click
//! that triggered it. Redraws happen only on input or daemon pushes, so idle CPU is zero.

mod overview;
mod sound;
mod render;
mod review;
mod theme;
mod welcome;

use arranger::adapter;
use arranger::client::{connect, connect_at, request_at};
use arranger::machine::{self, Machine};
use arranger::proto::{self, Event, Reply, Request, Ship, TaskInfo};
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
use render::{Cursor, MONO, Rect, Renderer, UI};
use std::io::{self, Write};
use std::net::Shutdown;
use std::num::NonZeroU32;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use review::{ReviewAction, ReviewView, Tag};
use theme::{AGENTS, Rgba, THEMES, Theme, accent, accent_deep, alpha, blend, models, readable};
#[cfg(test)]
use theme::{GRAPHITE, PAPER};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{CursorIcon, Window, WindowAttributes, WindowId};

const TITLE_H: f32 = 36.0;
const STATUS_H: f32 = 22.0;
const SIDEBAR_W: f32 = 272.0;
const HEADER_H: f32 = 80.0;
const ROW_H: f32 = 28.0;
/// A row with its second line (what the agent is doing / did).
const ROW2_H: f32 = 42.0;
const GROUP_H: f32 = 24.0;
const TERM_SIZE: f32 = 12.0;
const LINE_H: f32 = 18.0;
/// The chat message box shows up to this many lines, this far apart.
const MSG_LINES: usize = 8;
const MSG_LINE_H: f32 = 18.0;

/// A task across machines: (machine index, task id). Ids repeat between machines.
pub type TaskKey = (usize, u64);

pub enum Msg {
    Tasks(usize, Vec<TaskInfo>),
    Output(u64, Vec<u8>),
    Exited(u64),
    /// A finished task's recorded PTY output.
    History(TaskKey, Vec<u8>),
    Link(usize, Link),
    Reply(Tag, Result<Reply, String>),
    /// The add-machine dialog's connection test.
    Tested(Result<String, String>),
}

/// How a machine's daemon is reachable.
#[derive(Clone, PartialEq)]
pub enum Link {
    Connecting,
    /// Round trip of a ping through the tunnel; 0 for this machine.
    Up(u32),
    Down(String),
}

/// A machine whose daemon this window shows: this one (`cfg` None) or one reached over ssh.
struct Host {
    cfg: Option<Machine>,
    link: Link,
    tasks: Vec<TaskInfo>,
    /// Removed this session; kept so other hosts' indices stay valid.
    removed: bool,
    /// Wakes the link thread to retry now; dropping it ends the thread.
    retry: Option<mpsc::Sender<()>>,
    stop: Arc<AtomicBool>,
    tunnel: Arc<Mutex<Option<std::process::Child>>>,
}

impl Host {
    fn name(&self) -> &str {
        self.cfg.as_ref().map_or(if cfg!(target_os = "macos") { "This Mac" } else { "This machine" }, |c| &c.name)
    }

    /// The daemon socket to use; None is the local daemon (started on demand).
    fn sock(&self) -> Option<PathBuf> {
        self.cfg.as_ref().map(|c| machine::local_socket(&c.name))
    }
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
    task: TaskKey,
    writer: UnixStream,
    screen: Screen,
    size: (u16, u16),
    /// Rows of the agent's own input box. The terminal is this much taller than the pane, so the
    /// box sits just below it, out of sight, and the agent's text fills the pane.
    extra: u16,
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.writer.shutdown(Shutdown::Both); // ends the reader thread too
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Focus {
    Term,
    /// Vim keys' normal mode: keys move and act instead of typing.
    Normal,
    Filter,
    Message,
    Prompt,
    Base,
    Repo,
    Note,
    CommitMsg,
    HostSsh,
    HostName,
    HostRepo,
}

struct Dialog {
    prompt: String,
    agent: usize,
    /// Index into the agent's `models`; 0 is its default.
    model: usize,
    host: usize,
    /// Repo on the chosen machine.
    repo: String,
    base: String,
    /// Which of `App::prompts` ↑/↓ put in the prompt box.
    hist: Option<usize>,
    /// Local branches, most recent first; empty on other machines, where the base is typed.
    branches: Vec<String>,
    /// The branch list is open, and where it was drawn last frame.
    list: Option<Rect>,
    error: String,
}

/// Add-machine dialog.
struct AddHost {
    ssh: String,
    name: String,
    repo: String,
    /// Connection test: running, or its outcome.
    test: Option<Result<String, String>>,
    testing: bool,
}

enum Action {
    Select(TaskKey),
    Focus(Focus),
    NewTask,
    ToggleTheme,
    Answer(bool),
    Input(Vec<u8>),
    PickAgent(usize),
    PickModel(usize),
    Start(bool),
    CloseDialog,
    OpenReview,
    Review(ReviewAction),
    /// Folds or unfolds a sidebar section ("needs" or "host:<i>").
    Toggle(String),
    PickHost(usize),
    OpenAddHost,
    TestHost,
    AddHost,
    Reconnect(usize),
    RemoveHost(usize),
    PickTheme(usize),
    ToggleMenu,
    ToggleVim,
    /// Shows (true) or leaves the Overview table.
    Overview(bool),
    OvGroup(Option<usize>),
    OvHost(Option<usize>),
    /// Open this folder as the project, or pick one first (None).
    OpenProject(Option<PathBuf>),
    /// Pick a name and place for a new project folder, and open it.
    NewProject,
    /// Back to the welcome screen, to switch projects.
    Welcome,
    /// Start an ended task's agent again (resuming its session), sending it this prompt if any.
    Revive(TaskKey, String),
    /// Stop the task's agent and remove it: its worktree, log and history.
    Delete(TaskKey),
}

#[derive(Clone, Copy, PartialEq)]
enum Divider {
    Sidebar,
    Rail,
}

struct App {
    proxy: EventLoopProxy<Msg>,
    repo: PathBuf,
    gfx: Option<Gfx>,
    hosts: Vec<Host>,
    selected: Option<TaskKey>,
    session: Option<Session>,
    seq: u64,
    pane: (u16, u16),
    /// Index into THEMES.
    theme: usize,
    /// Vim keys: a normal mode for moving and acting without the mouse.
    vim: bool,
    /// The scheme menu is open; `scheme_btn` is the title-bar button that opens it.
    menu: bool,
    scheme_btn: Rect,
    /// The vim keybinding sheet.
    help: bool,
    /// `g` was pressed and waits for a second `g`.
    pending_g: bool,
    /// `d` was pressed on this task and waits for `y` (delete it) or `n`.
    confirm_delete: Option<TaskKey>,
    /// When Esc was last pressed in vim mode: a second one soon after goes to the agent.
    last_esc: Option<Instant>,
    /// New-task prompts, oldest first (saved in gui.json), for ↑ in the dialog.
    prompts: Vec<String>,
    /// Messages sent from the chat box, oldest first (saved in gui.json), for ↑ there; and which
    /// one ↑/↓ put in the box.
    sent: Vec<String>,
    sent_hist: Option<usize>,
    /// No project yet (opened outside one, or asked to switch): the welcome screen shows.
    welcome: bool,
    /// Project folders opened here, most recent first (saved in gui.json).
    recent: Vec<String>,
    /// Why the last folder couldn't be opened as a project.
    welcome_error: String,
    /// Why the last Revive of a task failed, shown in its Revive bar.
    revive_error: Option<(TaskKey, String)>,
    /// The Overview table is showing, filtered to one state group (None: all).
    overview: bool,
    ov_group: Option<usize>,
    /// ...and to one machine, from its header's "N ended" link.
    ov_host: Option<usize>,
    ov_scroll: f32,
    /// ⌘= / ⌘- / ⌘0: scales the whole window on top of the display's own scale.
    zoom: f32,
    focus: Focus,
    filter: String,
    message: String,
    dialog: Option<Dialog>,
    add_host: Option<AddHost>,
    /// A machine whose Remove was clicked once; the next click removes it.
    remove_armed: Option<usize>,
    review: Option<ReviewView>,
    /// Whether each probed task has anything to review, keyed by the state it was probed in.
    changes: std::collections::HashMap<TaskKey, (String, Option<bool>)>,
    /// Replay of a finished task's log, shown in place of a live session.
    history: Option<(TaskKey, Screen)>,
    /// How far into the top line the chat is scrolled, 0..1 of a line: the chat scrolls by pixel
    /// though the terminal keeps whole lines.
    wheel: f32,
    /// Wheel movement for a full-screen agent not yet worth a wheel tick.
    ticks: f32,
    /// Mouse-wheel scrolling still to apply: notches glide over a few frames instead of jumping;
    /// and when the last frame of it ran.
    glide: f32,
    glide_at: Instant,
    mods: ModifiersState,
    mouse: (f32, f32),
    /// The click target under the mouse at the last frame, and the cursor shown for it.
    hot: Option<usize>,
    cursor: Cursor,
    click: Option<(f32, f32)>,
    actions: Vec<Action>,
    scroll: f32,
    /// Sidebar sections folded to their header.
    collapsed: std::collections::HashSet<String>,
    /// The selection's fixed end in the focused field (the caret is the moving end), from ⇧-moves,
    /// a mouse drag or ⌘A. The next edit replaces the text between them.
    anchor: Option<(Focus, usize)>,
    /// The text field a mouse press landed in, while the button is held: moving drags a selection.
    text_drag: Option<Focus>,
    /// The field and byte offset under a press or drag, found while drawing that field.
    text_hit: std::cell::Cell<Option<(Focus, usize)>>,
    /// Caret in the focused field, as a byte offset; None (or another field) is its end.
    caret: Option<(Focus, usize)>,
    /// Sidebar and review-rail widths, which their dividers drag.
    sidebar_w: f32,
    rail_w: f32,
    dragging: Option<Divider>,
    caret_on: bool,
    blink_at: Instant,
    /// Whether the window has focus: running icons only play while it does.
    focused: bool,
    /// The clock running icons play to.
    epoch: Instant,
    /// The selected task's diff at its last probe: base branch, files, lines added and removed.
    stats: std::collections::HashMap<TaskKey, (String, usize, u32, u32)>,
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
            if p.send_event(Msg::Tasks(0, tasks)).is_err() {
                return;
            }
        }
        let _ = p.send_event(Msg::Link(0, Link::Down("daemon disconnected".into())));
    });
    let mut app = App {
        proxy,
        repo: std::env::current_dir()?,
        gfx: None,
        hosts: Vec::new(),
        selected: None,
        session: None,
        seq: 0,
        pane: (80, 24),
        theme: 0,
        vim: false,
        menu: false,
        scheme_btn: Rect::new(0.0, 0.0, 0.0, 0.0),
        help: false,
        confirm_delete: None,
        pending_g: false,
        last_esc: None,
        prompts: Vec::new(),
        sent: Vec::new(),
        sent_hist: None,
        revive_error: None,
        welcome: false,
        recent: Vec::new(),
        welcome_error: String::new(),
        overview: false,
        ov_group: None,
        ov_host: None,
        ov_scroll: 0.0,
        zoom: 1.0,
        focus: Focus::Term,
        filter: String::new(),
        message: String::new(),
        dialog: None,
        add_host: None,
        remove_armed: None,
        review: None,
        changes: Default::default(),
        history: None,
        wheel: 0.0,
        ticks: 0.0,
        glide: 0.0,
        glide_at: Instant::now(),
        mods: ModifiersState::empty(),
        mouse: (0.0, 0.0),
        hot: None,
        cursor: Cursor::Arrow,
        click: None,
        actions: Vec::new(),
        scroll: 0.0,
        collapsed: Default::default(),
        anchor: None,
        text_drag: None,
        text_hit: Default::default(),
        caret: None,
        sidebar_w: SIDEBAR_W,
        rail_w: 280.0,
        dragging: None,
        caret_on: true,
        blink_at: Instant::now(),
        focused: true,
        epoch: Instant::now(),
        stats: Default::default(),
    };
    let settings = Settings::load();
    app.theme = THEMES.iter().position(|t| t.name == settings.theme).unwrap_or(0);
    app.vim = settings.vim;
    app.prompts = settings.prompts;
    app.sent = settings.sent;
    app.recent = settings.recent;
    // Started inside a project (from a terminal): that's the one. Else (Finder, the Dock) ask.
    match welcome::project_root(&app.repo) {
        Some(top) => {
            app.repo = top.clone();
            app.remember(&top);
        }
        None => app.welcome = true,
    }
    app.focus = app.rest();
    app.link_host(None);
    for m in machine::load() {
        app.link_host(Some(m));
    }
    event_loop.run_app(&mut app).map_err(io::Error::other)
}

/// Window preferences, kept in <data>/gui.json.
#[derive(serde::Serialize, serde::Deserialize, Default)]
#[serde(default)]
struct Settings {
    theme: String,
    vim: bool,
    /// New-task prompts, oldest first, for ↑ in the dialog.
    prompts: Vec<String>,
    /// Chat messages, oldest first, for ↑ in the message box.
    sent: Vec<String>,
    /// Project folders, most recent first, for the welcome screen.
    recent: Vec<String>,
}

impl Settings {
    fn path() -> PathBuf {
        proto::data_dir().join("gui.json")
    }

    fn load() -> Settings {
        std::fs::read(Self::path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }
}

/// A bare binary has no bundle, so macOS shows a generic icon in the Dock; set ours.
#[cfg(target_os = "macos")]
fn dock_icon() {
    use objc2::ClassType;
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::{MainThreadMarker, NSData};
    let Some(mtm) = MainThreadMarker::new() else { return };
    let data = NSData::with_bytes(include_bytes!("../../../assets/icon.png"));
    if let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) {
        unsafe { NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image)) };
    }
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
    #[cfg(target_os = "macos")]
    dock_icon();
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
            if Instant::now() >= self.blink_at {
                self.caret_on = !self.caret_on;
                self.blink_at = Instant::now() + BLINK;
            }
            if self.glide != 0.0 {
                self.glide_step();
            }
            self.redraw();
        }
    }

    /// Wakes only to blink the caret while a text field has focus, to animate the Review button, to
    /// glide a wheel notch, or to play running icons.
    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        let blink = (!matches!(self.focus, Focus::Term | Focus::Normal)).then_some(self.blink_at);
        let moving = self.glide != 0.0 || self.review.as_ref().is_some_and(|rv| rv.animating());
        // A glide asks for frames at 120Hz; vsync holds that to a 60Hz display's own rate.
        let ms = if self.glide != 0.0 { 8 } else if moving { 33 } else { 80 };
        let frame = (moving || self.sweep().is_some()).then(|| Instant::now() + Duration::from_millis(ms));
        match blink.into_iter().chain(frame).min() {
            Some(at) => el.set_control_flow(ControlFlow::WaitUntil(at)),
            None => el.set_control_flow(ControlFlow::Wait),
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
            Msg::Tasks(m, tasks) => {
                self.user_event_tasks(m, tasks);
                if self.selected.is_none_or(|k| self.task(k).is_none()) {
                    // Dev aid for screenshots (`ARRANGER_SELECT=<task id>`): start on that local task.
                    let pick = std::env::var("ARRANGER_SELECT").ok().and_then(|v| v.parse().ok()).map(|id| (0, id)).filter(|&k| self.task(k).is_some());
                    self.selected = pick.or_else(|| self.order().first().copied());
                }
                self.sync_session();
            }
            Msg::Link(m, link) => {
                if !matches!(link, Link::Up(_)) && m != 0 {
                    self.hosts[m].tasks.clear(); // stale once the link is gone
                }
                self.hosts[m].link = link;
                if self.selected.is_some_and(|k| k.0 == m && self.task(k).is_none()) {
                    self.selected = None;
                    self.sync_session();
                }
            }
            Msg::Tested(res) => {
                if let Some(a) = &mut self.add_host {
                    a.testing = false;
                    a.test = Some(res);
                }
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
            Msg::Reply(tag, reply) => self.on_reply(tag, reply),
        }
        self.redraw();
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                // ponytail: tunnels only die with a clean close; a crash leaves ssh -N running until
                // its link drops. A watchdog pipe to each ssh would cover that.
                for h in &self.hosts {
                    h.stop.store(true, Ordering::Relaxed);
                    if let Some(mut child) = h.tunnel.lock().unwrap().take() {
                        let _ = child.kill();
                    }
                }
                el.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(g) = &self.gfx {
                    if let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) {
                        g.surface.resize(&g.context, w, h);
                    }
                }
                self.redraw();
            }
            WindowEvent::ScaleFactorChanged { .. } => self.redraw(),
            WindowEvent::Focused(on) => {
                self.focused = on;
                self.redraw();
            }
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::CursorMoved { position, .. } => {
                let dpi = self.scale() as f64;
                self.mouse = ((position.x / dpi) as f32, (position.y / dpi) as f32);
                if let Some(d) = self.dragging {
                    let max = self.gfx.as_ref().map_or(1280.0, |g| g.window.inner_size().width as f32 / self.scale()) - 400.0;
                    let w = self.mouse.0.clamp(200.0, max.max(200.0));
                    *if d == Divider::Sidebar { &mut self.sidebar_w } else { &mut self.rail_w } = w;
                    self.redraw();
                }
                if self.text_drag.is_some() {
                    self.redraw();
                }
                self.hover_changed();
            }
            WindowEvent::CursorLeft { .. } => {
                self.mouse = (-1.0, -1.0);
                self.hover_changed();
            }
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } => {
                self.dragging = self.divider_at(self.mouse);
                if self.dragging.is_none() {
                    self.click = Some(self.mouse);
                }
                self.redraw();
            }
            WindowEvent::MouseInput { state: ElementState::Released, button: MouseButton::Left, .. } => (self.dragging, self.text_drag) = (None, None),
            WindowEvent::MouseWheel { delta, .. } => match delta {
                // A wheel notch glides; a trackpad already sends fine steps.
                MouseScrollDelta::LineDelta(_, y) => {
                    if self.glide == 0.0 {
                        self.glide_at = Instant::now();
                    }
                    self.glide += y * ROW_H * 1.5;
                    self.redraw();
                }
                MouseScrollDelta::PixelDelta(p) => self.scroll_by(p.y as f32),
            },
            WindowEvent::DroppedFile(path) if self.welcome => {
                self.actions.push(Action::OpenProject(Some(path)));
                self.apply();
                self.redraw();
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

/// A sidebar section and its rows. `host` None is "Needs you", which spans machines.
struct Section<'a> {
    key: String,
    host: Option<usize>,
    tasks: Vec<&'a TaskInfo>,
    /// Tasks whose agent has ended; left out of `tasks` (Overview lists them) unless filtering or selected.
    ended: usize,
}

/// Automatic reconnect attempts after a machine's link fails; then it waits for Reconnect.
const AUTO_RETRIES: u32 = 3;

/// Keeps machine `m`'s daemon reachable: runs `arranger endpoint` over ssh, forwards its socket,
/// streams its task list, and on any failure reports why and retries with backoff, up to
/// `AUTO_RETRIES` times (or at once when the window asks). Ends when the machine is removed.
fn keep_linked(m: usize, cfg: Machine, p: EventLoopProxy<Msg>, retry: mpsc::Receiver<()>, stop: Arc<AtomicBool>, tunnel: Arc<Mutex<Option<std::process::Child>>>) {
    let local = machine::local_socket(&cfg.name);
    let (mut tries, mut wait) = (0, 2);
    while !stop.load(Ordering::Relaxed) {
        let _ = p.send_event(Msg::Link(m, Link::Connecting));
        let err = match machine::endpoint(&cfg.ssh).and_then(|remote| machine::tunnel(&cfg.ssh, &remote, &local)) {
            Ok(child) => {
                *tunnel.lock().unwrap() = Some(child);
                let watch = || -> io::Result<(UnixStream, u32)> {
                    let t0 = Instant::now();
                    request_at(Some(&local), &Request::Ping)?;
                    let ms = t0.elapsed().as_millis() as u32;
                    let mut w = connect_at(Some(&local))?;
                    w.write_all(&proto::frame(&Request::Watch))?;
                    Ok((w, ms))
                };
                let err = match watch() {
                    Ok((mut w, ms)) => {
                        let _ = p.send_event(Msg::Link(m, Link::Up(ms.max(1))));
                        loop {
                            match proto::read_msg(&mut w) {
                                Ok(Reply::Tasks(tasks)) => {
                                    // Linked for real once a task list arrives; a link that drops
                                    // before that still spends a retry.
                                    (tries, wait) = (0, 2);
                                    if p.send_event(Msg::Tasks(m, tasks)).is_err() {
                                        return;
                                    }
                                }
                                // A reply that doesn't decode (read errors are I/O kinds; decode errors are `Other`).
                                Err(e) if e.kind() == io::ErrorKind::Other => {
                                    break "its daemon is a different arranger build; install the same release on both".to_string();
                                }
                                _ => break "connection lost".to_string(),
                            }
                        }
                    }
                    Err(e) => e.to_string(),
                };
                if let Some(mut child) = tunnel.lock().unwrap().take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                err
            }
            Err(e) => e.to_string(),
        };
        if stop.load(Ordering::Relaxed) || p.send_event(Msg::Link(m, Link::Down(err))).is_err() {
            return;
        }
        if tries == AUTO_RETRIES {
            // Out of automatic retries: stay down until the window asks to reconnect.
            if retry.recv().is_err() {
                return;
            }
            (tries, wait) = (0, 2);
            continue;
        }
        match retry.recv_timeout(Duration::from_secs(wait)) {
            Ok(()) => (tries, wait) = (0, 2),
            Err(mpsc::RecvTimeoutError::Timeout) => (tries, wait) = (tries + 1, wait * 2),
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn is_live(state: &str) -> bool {
    matches!(state, "running" | "needs-you" | "done")
}

/// Unix seconds of the task's latest activity, for ordering.
fn last_event(t: &TaskInfo) -> u64 {
    t.updated.max(t.since).max(t.created)
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

/// A machine's header line: "1 needs you · 2 running · 1 done", or "no tasks".
fn host_summary(tasks: &[TaskInfo]) -> String {
    let mut counts = [0; 5];
    for t in tasks {
        counts[group(&t.state)] += 1;
    }
    let names = ["needs you", "running", "queued", "done", "ended"];
    let parts: Vec<String> = counts.iter().zip(names).filter(|(n, _)| **n > 0).map(|(n, s)| format!("{n} {s}")).collect();
    if parts.is_empty() { "no tasks".into() } else { parts.join(" · ") }
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
        (3, _) => format!("“{}”", review::plain(&task.summary)),
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

fn set_clipboard(text: &str) {
    let cmds: &[&[&str]] = if cfg!(target_os = "macos") {
        &[&["pbcopy"]]
    } else {
        &[&["wl-copy"], &["xclip", "-i", "-selection", "clipboard"]]
    };
    for c in cmds {
        if let Ok(mut child) = Command::new(c[0]).args(&c[1..]).stdin(std::process::Stdio::piped()).spawn() {
            let _ = child.stdin.take().map(|mut i| i.write_all(text.as_bytes()));
            if child.wait().is_ok_and(|s| s.success()) {
                return;
            }
        }
    }
}

/// Steps `text` (a prompt or message box) through `list`, oldest first, for ↑ (`up`) or ↓, while
/// the box is empty or still shows what the last step put there (`hist`). False if it isn't.
fn recall(list: &[String], hist: &mut Option<usize>, text: &mut String, up: bool) -> bool {
    let browsing = hist.filter(|&i| list.get(i) == Some(text));
    if !text.is_empty() && browsing.is_none() {
        return false;
    }
    let next = match (browsing, up) {
        (None, true) => list.len().checked_sub(1),
        (Some(i), true) => Some(i.saturating_sub(1)),
        (Some(i), false) => Some(i + 1).filter(|&j| j < list.len()),
        (None, false) => None,
    };
    *hist = next;
    *text = next.map_or_else(String::new, |i| list[i].clone());
    true
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
        THEMES[self.theme]
    }

    fn task(&self, (m, id): TaskKey) -> Option<&TaskInfo> {
        self.hosts.get(m)?.tasks.iter().find(|t| t.id == id)
    }

    fn current(&self) -> Option<&TaskInfo> {
        self.selected.and_then(|k| self.task(k))
    }

    /// Seconds on the status icons' clock, while one that moves is on screen (running, needs you,
    /// or done in the last two seconds) and the window has focus; None draws them still.
    fn sweep(&self) -> Option<f32> {
        let now = unix_secs();
        let moving = |t: &TaskInfo| match group(&t.state) {
            0 | 1 => true,
            3 => now - (t.since as f64) < 2.0,
            _ => false,
        };
        let shown = !self.welcome && self.review.is_none() && self.all_tasks().any(moving);
        (shown && self.focused).then(|| self.epoch.elapsed().as_secs_f32())
    }

    /// The selected task has a diff to review; true until its probe answers.
    fn reviewable(&self) -> bool {
        self.current().is_some_and(|t| self.changes.get(&(t.machine, t.id)).is_none_or(|c| c.1 != Some(false)))
    }

    /// Asks for the selected task's diff whenever its state has moved since the last ask.
    fn probe_changes(&mut self) {
        let Some(t) = self.current() else { return };
        let (key, stamp) = ((t.machine, t.id), format!("{} {} {} {}", t.state, t.since, t.shipped, t.action));
        if self.changes.get(&key).is_some_and(|c| c.0 == stamp) {
            return;
        }
        let prev = self.changes.get(&key).and_then(|c| c.1);
        self.changes.insert(key, (stamp, prev));
        self.spawn_request(key.0, Tag::Changes(key), Request::Review { id: key.1 });
    }

    fn all_tasks(&self) -> impl Iterator<Item = &TaskInfo> {
        self.hosts.iter().filter(|h| !h.removed).flat_map(|h| &h.tasks)
    }

    fn sock(&self, m: usize) -> Option<PathBuf> {
        self.hosts.get(m).and_then(Host::sock)
    }

    /// Sidebar sections: "Needs you" across every machine, then each machine's other tasks,
    /// most recently active first within each state.
    fn sections(&self) -> Vec<Section<'_>> {
        let f = self.filter.to_lowercase();
        let shown = |t: &&TaskInfo| {
            f.is_empty() || format!("{} {} {} {}", title(t), t.agent, t.branch, self.hosts[t.machine].name()).to_lowercase().contains(&f)
        };
        let sort = |v: &mut Vec<&TaskInfo>| v.sort_by_key(|t| (group(&t.state), std::cmp::Reverse(last_event(t)), std::cmp::Reverse(t.id)));
        let mut needs: Vec<&TaskInfo> = self.all_tasks().filter(shown).filter(|t| group(&t.state) == 0).collect();
        sort(&mut needs);
        let mut out = vec![Section { key: "needs".into(), host: None, tasks: needs, ended: 0 }];
        for (i, h) in self.hosts.iter().enumerate().filter(|(_, h)| !h.removed) {
            let mut tasks: Vec<&TaskInfo> = h.tasks.iter().filter(shown).filter(|t| group(&t.state) != 0).collect();
            sort(&mut tasks);
            let ended = tasks.iter().filter(|t| group(&t.state) == 4).count();
            if f.is_empty() {
                tasks.retain(|t| group(&t.state) != 4 || self.selected == Some((t.machine, t.id)));
            }
            out.push(Section { key: format!("host:{i}"), host: Some(i), tasks, ended });
        }
        // Unreachable machines sink to the bottom; the sort is stable, so order is otherwise kept.
        out.sort_by_key(|s| s.host.is_some_and(|m| matches!(self.hosts[m].link, Link::Down(_))));
        out
    }

    /// The Overview's rows: by state, most recently active first, through the filter and the state tabs.
    fn ov_tasks(&self) -> Vec<&TaskInfo> {
        let f = self.filter.to_lowercase();
        let mut v: Vec<&TaskInfo> = self
            .all_tasks()
            .filter(|t| self.ov_group.is_none_or(|g| group(&t.state) == g) && self.ov_host.is_none_or(|m| t.machine == m))
            .filter(|t| f.is_empty() || format!("{} {} {} {}", title(t), t.agent, t.branch, self.hosts[t.machine].name()).to_lowercase().contains(&f))
            .collect();
        v.sort_by_key(|t| (group(&t.state), std::cmp::Reverse(last_event(t)), t.machine, std::cmp::Reverse(t.id)));
        v
    }

    /// Sidebar order (or the Overview's), for keyboard switching: rows of folded sections are skipped.
    fn order(&self) -> Vec<TaskKey> {
        if self.overview {
            return self.ov_tasks().iter().map(|t| (t.machine, t.id)).collect();
        }
        let sections = self.sections();
        let open = sections.iter().filter(|s| !self.collapsed.contains(&s.key));
        open.flat_map(|s| s.tasks.iter().map(|t| (t.machine, t.id))).collect()
    }

    /// Adds a machine to the window; a remote one gets a thread that keeps its ssh link up.
    fn link_host(&mut self, cfg: Option<Machine>) {
        let i = self.hosts.len();
        let (tx, rx) = mpsc::channel();
        let host = Host {
            cfg: cfg.clone(),
            link: if cfg.is_some() { Link::Connecting } else { Link::Up(0) },
            tasks: Vec::new(),
            removed: false,
            retry: cfg.is_some().then_some(tx),
            stop: Arc::default(),
            tunnel: Arc::default(),
        };
        if let Some(cfg) = cfg {
            let (p, stop, tunnel) = (self.proxy.clone(), host.stop.clone(), host.tunnel.clone());
            thread::spawn(move || keep_linked(i, cfg, p, rx, stop, tunnel));
        }
        self.hosts.push(host);
    }

    fn user_event_tasks(&mut self, m: usize, mut tasks: Vec<TaskInfo>) {
        for t in &mut tasks {
            t.machine = m;
        }
        if let Some(h) = self.hosts.get_mut(m).filter(|h| !h.removed) {
            if tasks.iter().any(|task| {
                h.tasks.iter().find(|old| old.id == task.id).is_some_and(|old| {
                    sound::should_ding(&old.state, &task.state)
                })
            }) {
                sound::ding();
            }
            h.tasks = tasks;
        }
    }

    /// The add-machine dialog's Add: saves it and starts linking.
    fn save_host(&mut self) {
        let Some(a) = &self.add_host else { return };
        let ssh = a.ssh.trim().to_string();
        if ssh.is_empty() {
            return;
        }
        // Default name: the host part of user@host.
        let name = Some(a.name.trim().to_string()).filter(|n| !n.is_empty()).unwrap_or_else(|| ssh.rsplit('@').next().unwrap_or(&ssh).to_string());
        if self.hosts.iter().any(|h| !h.removed && h.name() == name) {
            if let Some(a) = &mut self.add_host {
                a.test = Some(Err(format!("a machine named {name} is already added")));
            }
            return;
        }
        let cfg = Machine { name, ssh, repo: a.repo.trim().to_string() };
        let mut list = machine::load();
        list.push(cfg.clone());
        let _ = machine::save(&list);
        self.add_host = None;
        self.focus = self.rest();
        self.link_host(Some(cfg));
    }

    fn remove_host(&mut self, m: usize) {
        let Some(h) = self.hosts.get_mut(m).filter(|h| h.cfg.is_some()) else { return };
        h.stop.store(true, Ordering::Relaxed);
        if let Some(mut child) = h.tunnel.lock().unwrap().take() {
            let _ = child.kill();
        }
        h.retry = None;
        h.removed = true;
        h.tasks.clear();
        let name = h.name().to_string();
        let mut list = machine::load();
        list.retain(|c| c.name != name);
        let _ = machine::save(&list);
        if self.selected.is_some_and(|k| k.0 == m) {
            self.selected = None;
            self.sync_session();
        }
    }

    /// Where focus rests when no field has it: the agent's terminal, or vim's normal mode.
    fn rest(&self) -> Focus {
        if self.vim { Focus::Normal } else { Focus::Term }
    }

    fn set_theme(&mut self, i: usize) {
        self.theme = i;
        self.save_settings();
    }

    fn save_settings(&self) {
        let s = Settings { theme: THEMES[self.theme].name.into(), vim: self.vim, prompts: self.prompts.clone(), sent: self.sent.clone(), recent: self.recent.clone() };
        let _ = std::fs::write(Settings::path(), serde_json::to_vec_pretty(&s).unwrap_or_default());
    }

    /// Selects the next (or previous) task that needs you, wrapping around.
    fn jump_needs(&mut self, forward: bool) {
        let order = self.order();
        let needs: Vec<usize> = (0..order.len()).filter(|&i| self.task(order[i]).is_some_and(|t| group(&t.state) == 0)).collect();
        let cur = self.selected.and_then(|k| order.iter().position(|&x| x == k));
        let pick = match (forward, cur) {
            (true, Some(c)) => needs.iter().find(|&&i| i > c).or(needs.first()),
            (false, Some(c)) => needs.iter().rev().find(|&&i| i < c).or(needs.last()),
            (true, None) => needs.first(),
            (false, None) => needs.last(),
        };
        if let Some(&i) = pick {
            self.actions.push(Action::Select(order[i]));
        }
    }

    /// Opens the selected task's chat from the Overview, typing into its message box (insert
    /// mode) unless the agent has ended.
    fn open_from_overview(&mut self) {
        self.actions.push(Action::Overview(false));
        self.actions.push(Action::Focus(Focus::Message)); // ignored for an ended agent
    }

    /// Steps Workspace → Overview → Review → Workspace (or back). Review needs a selected task,
    /// so without one it's passed over.
    fn cycle_view(&mut self, forward: bool) {
        let cur = if self.review.is_some() { 2 } else if self.overview { 1 } else { 0 };
        let mut next = if forward { (cur + 1) % 3 } else { (cur + 2) % 3 };
        if next == 2 && !self.reviewable() {
            next = if forward { 0 } else { 1 };
        }
        self.actions.push(match next {
            0 => Action::Overview(false),
            1 => Action::Overview(true),
            _ => Action::OpenReview,
        });
    }

    /// Vim's normal mode: one key, one command (`gg` takes two).
    fn normal_key(&mut self, ev: &KeyEvent) {
        let key = match &ev.logical_key {
            Key::Character(c) => c.to_string(),
            Key::Named(NamedKey::ArrowDown) => "j".into(),
            Key::Named(NamedKey::ArrowUp) => "k".into(),
            Key::Named(NamedKey::Enter) => "o".into(),
            Key::Named(NamedKey::Escape) => "esc".into(),
            _ => return,
        };
        let g = std::mem::take(&mut self.pending_g);
        // `d` asked to delete a task: `y` does, any other key doesn't.
        if let Some(task) = self.confirm_delete.take() {
            if key == "y" {
                self.actions.push(Action::Delete(task));
            }
            return;
        }
        let files = self.review.as_ref().and_then(|rv| rv.data.as_ref()).map_or(0, |d| d.files.len());
        let file = self.review.as_ref().map(|rv| rv.file);
        match key.as_str() {
            "j" if file.is_some() => self.actions.push(Action::Review(ReviewAction::File((file.unwrap() + 1).min(files.saturating_sub(1))))),
            "k" if file.is_some() => self.actions.push(Action::Review(ReviewAction::File(file.unwrap().saturating_sub(1)))),
            "j" => self.move_selection(1),
            "k" => self.move_selection(-1),
            "g" if g => self.move_selection(-(1 << 20)),
            "g" => self.pending_g = true,
            "G" => self.move_selection(1 << 20),
            "J" => self.jump_needs(true),
            "K" => self.jump_needs(false),
            "/" => self.actions.push(Action::Focus(Focus::Filter)),
            // Insert: the selected agent's message box, from whichever view.
            "i" if self.current().is_some() => {
                self.overview = false;
                self.review = None;
                self.actions.push(Action::Focus(Focus::Message));
            }
            "h" | "l" => self.cycle_view(key == "l"),
            "o" if self.overview => self.open_from_overview(),
            "o" if self.review.is_none() && self.current().is_some() => {
                self.overview = false;
                self.actions.push(Action::Focus(Focus::Term));
            }
            "a" => self.actions.push(Action::Answer(true)),
            "x" => self.actions.push(Action::Answer(false)),
            "d" if self.review.is_none() && !self.overview => self.confirm_delete = self.current().map(|t| (t.machine, t.id)),
            "r" => self.actions.push(Action::OpenReview),
            "n" => self.actions.push(Action::NewTask),
            "m" if self.review.is_none() && !self.overview => self.actions.push(Action::OpenAddHost),
            "?" => self.help = !self.help,
            "esc" if self.help => self.help = false,
            "esc" if self.review.is_some() => self.actions.push(Action::Review(ReviewAction::Close)),
            "esc" if self.overview => self.actions.push(Action::Overview(false)),
            _ => {}
        }
    }

    /// The sidebar's or review rail's divider under `p`, which a press starts dragging.
    fn divider_at(&self, p: (f32, f32)) -> Option<Divider> {
        if self.dialog.is_some() || self.add_host.is_some() || p.1 < TITLE_H {
            return None;
        }
        match &self.review {
            None if self.overview => None,
            None => ((p.0 - self.sidebar_w).abs() <= 4.0).then_some(Divider::Sidebar),
            Some(_) => ((p.0 - self.rail_w).abs() <= 4.0 && p.1 > TITLE_H + review::STRIP_H).then_some(Divider::Rail),
        }
    }

    /// Scrolls whatever is under the mouse by `dy` pixels (positive = towards the top).
    fn scroll_by(&mut self, dy: f32) {
        if let Some(rv) = self.review.as_mut().filter(|_| self.mouse.1 > TITLE_H + review::STRIP_H) {
            let s = if self.mouse.0 < self.rail_w { &mut rv.rail_scroll } else { &mut rv.scroll };
            *s = (*s - dy).max(0.0);
        } else if self.overview {
            self.ov_scroll = (self.ov_scroll - dy).max(0.0);
        } else if self.mouse.0 < self.sidebar_w {
            self.scroll = (self.scroll - dy).max(0.0);
        } else if self.review.is_none() && !self.welcome {
            let id = self.selected;
            if let Some(s) = self.session.as_ref().filter(|s| Some(s.task) == id && s.screen.mode().contains(TermMode::ALT_SCREEN)) {
                // A full-screen agent scrolls itself, a few lines per tick: one tick per three lines
                // moved, as a wheel notch would, or it races.
                self.ticks += dy / (3.0 * LINE_H);
                let n = self.ticks.trunc();
                self.ticks -= n;
                if let Some(bytes) = self.wheel_bytes(&s.screen, n as i32).filter(|_| n != 0.0) {
                    self.send_input(bytes);
                }
                return;
            }
            let screen = match (&mut self.session, &mut self.history) {
                (Some(s), _) if Some(s.task) == id => &mut s.screen,
                (_, Some((h, screen))) if Some(*h) == id => screen,
                _ => return,
            };
            // Whole lines go to the terminal's scrollback; the rest is a pixel offset.
            self.wheel += dy / LINE_H;
            let lines = self.wheel.floor();
            self.wheel -= lines;
            screen.scroll(lines as i32);
            let g = screen.grid();
            if g.display_offset() == 0 && lines < 0.0 || g.display_offset() == g.history_size() {
                self.wheel = 0.0; // at the bottom, or the oldest line
            }
        }
        self.redraw();
    }

    /// One frame of a gliding wheel notch: 30% of what's left per 60Hz frame, eased the same at
    /// any frame rate.
    fn glide_step(&mut self) {
        let dt = self.glide_at.elapsed().as_secs_f32().min(0.05);
        self.glide_at = Instant::now();
        let share = 1.0 - 0.7f32.powf(dt * 60.0);
        let step = if self.glide.abs() < 1.0 { self.glide } else { self.glide * share };
        self.glide -= step;
        self.scroll_by(step);
    }

    /// A full-screen agent (alternate screen, no scrollback here) scrolls its own chat: the
    /// wheel goes to it as mouse-wheel reports, or as arrow keys when it doesn't take the mouse.
    fn wheel_bytes(&self, screen: &Screen, lines: i32) -> Option<Vec<u8>> {
        let mode = screen.mode();
        if !mode.contains(TermMode::ALT_SCREEN) {
            return None;
        }
        let up = lines > 0;
        let n = lines.unsigned_abs() as usize;
        if mode.intersects(TermMode::MOUSE_REPORT_CLICK | TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION) {
            let cell_w = self.gfx.as_ref()?.r.advance(MONO, TERM_SIZE, 400, '0');
            let col = ((self.mouse.0 - self.sidebar_w - 16.0) / cell_w).max(0.0) as u32 + 1;
            let row = ((self.mouse.1 - TITLE_H - HEADER_H - 12.0) / LINE_H).max(0.0) as u32 + 1;
            let button = if up { 64 } else { 65 };
            let one = if mode.contains(TermMode::SGR_MOUSE) {
                format!("\x1b[<{button};{col};{row}M").into_bytes()
            } else {
                vec![0x1b, b'[', b'M', 32 + button as u8, (32 + col.min(223)) as u8, (32 + row.min(223)) as u8]
            };
            return Some(one.repeat(n));
        }
        let app = mode.contains(TermMode::APP_CURSOR);
        let key: &[u8] = match (up, app) {
            (true, true) => b"\x1bOA",
            (true, false) => b"\x1b[A",
            (false, true) => b"\x1bOB",
            (false, false) => b"\x1b[B",
        };
        Some(key.repeat(n))
    }

    /// Redraws only when the mouse crosses into another target, so moving it costs nothing otherwise.
    fn hover_changed(&self) {
        if self.gfx.as_ref().is_some_and(|g| g.r.hot(self.mouse) != self.hot) {
            self.redraw();
        }
    }

    /// Physical pixels per layout pixel: the display's scale times the zoom.
    fn scale(&self) -> f32 {
        self.gfx.as_ref().map_or(1.0, |g| g.window.scale_factor() as f32) * self.zoom
    }

    fn redraw(&self) {
        if let Some(g) = &self.gfx {
            g.window.request_redraw();
        }
    }

    fn sync_session(&mut self) {
        let want = self.current().filter(|t| is_live(&t.state)).map(|t| (t.machine, t.id));
        self.load_history();
        if self.session.as_ref().map(|s| s.task) == want {
            return;
        }
        self.session = None;
        let Some(key @ (m, id)) = want else { return };
        let (cols, rows) = self.pane;
        let Ok(mut sock) = connect_at(self.sock(m).as_deref()) else { return };
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
        self.session = Some(Session { seq, task: key, writer: sock, screen: Screen::new(cols, rows), size: (cols, rows), extra: 0 });
    }

    /// For a task that is no longer running, replays its PTY log so its chat stays readable.
    fn load_history(&mut self) {
        let Some(task) = self.current().filter(|t| !is_live(&t.state) && t.state != "queued") else {
            self.history = None;
            return;
        };
        let key = (task.machine, task.id);
        if self.history.as_ref().is_some_and(|h| h.0 == key) {
            return;
        }
        let (sock, p) = (self.sock(key.0), self.proxy.clone());
        self.history = None;
        thread::spawn(move || {
            if let Ok(Reply::Log(zst)) = request_at(sock.as_deref(), &Request::Log { id: key.1 }) {
                if let Ok(bytes) = zstd::decode_all(&zst[..]) {
                    let _ = p.send_event(Msg::History(key, bytes));
                }
            }
        });
    }

    fn send_input(&mut self, bytes: Vec<u8>) {
        self.wheel = 0.0; // typing jumps the chat back to the bottom
        if let Some(s) = &mut self.session {
            let _ = s.writer.write_all(&proto::frame(&Request::Input(bytes)));
        }
    }

    fn move_selection(&mut self, d: i32) {
        let order = self.order();
        if order.is_empty() {
            return;
        }
        let i = self.selected.and_then(|id| order.iter().position(|&x| x == id)).unwrap_or(0) as i32;
        if let Some(&id) = order.get((i + d).clamp(0, order.len() as i32 - 1) as usize) {
            self.actions.push(Action::Select(id));
        }
    }

    fn open_dialog(&mut self) {
        // Starts on the selected task's machine.
        let host = self.current().map_or(0, |t| t.machine);
        self.dialog = Some(Dialog { prompt: String::new(), agent: 0, model: 0, host: 0, repo: String::new(), base: String::new(), hist: None, branches: Vec::new(), list: None, error: String::new() });
        self.pick_host(host);
        self.focus = Focus::Prompt;
        self.anchor = None;
    }

    /// Points the new-task dialog at machine `m`: its default repo, and here the current branch.
    fn pick_host(&mut self, m: usize) {
        let Some(h) = self.hosts.get(m).filter(|h| !h.removed) else { return };
        let (repo, base, branches) = match &h.cfg {
            Some(c) => (c.repo.clone(), String::new(), Vec::new()), // the remote's checked-out branch
            None => {
                let base = Command::new("git")
                    .arg("-C")
                    .arg(&self.repo)
                    .args(["rev-parse", "--abbrev-ref", "HEAD"])
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .unwrap_or_else(|| "HEAD".into());
                let branches = Command::new("git")
                    .arg("-C")
                    .arg(&self.repo)
                    .args(["for-each-ref", "--sort=-committerdate", "--format=%(refname:short)", "refs/heads"])
                    .output()
                    .map(|o| String::from_utf8_lossy(&o.stdout).lines().map(str::to_string).collect())
                    .unwrap_or_default();
                (self.repo.display().to_string(), base, branches)
            }
        };
        if let Some(d) = &mut self.dialog {
            (d.host, d.repo, d.base, d.branches, d.list) = (m, repo, base, branches, None);
        }
    }

    /// Starts the dialog's task and switches to it; `attach` puts keys straight into its terminal.
    fn start(&mut self, attach: bool) {
        let sock = self.dialog.as_ref().and_then(|d| self.sock(d.host));
        let Some(d) = &mut self.dialog else { return };
        let m = d.host;
        if d.repo.trim().is_empty() {
            d.error = "Which repo on that machine? Type its path.".into();
            return;
        }
        let req = Request::New {
            agent: AGENTS[d.agent].into(),
            repo: d.repo.trim().into(),
            base: Some(d.base.trim().to_string()).filter(|b| !b.is_empty()),
            prompt: Some(d.prompt.trim().to_string()).filter(|p| !p.is_empty()),
            model: Some(models(AGENTS[d.agent])[d.model].1.to_string()).filter(|m| !m.is_empty()),
        };
        match request_at(sock.as_deref(), &req) {
            Ok(Reply::Created { id, .. }) => {
                let prompt = d.prompt.trim().to_string();
                if !prompt.is_empty() && self.prompts.last() != Some(&prompt) {
                    self.prompts.push(prompt);
                    let over = self.prompts.len().saturating_sub(50);
                    self.prompts.drain(..over);
                    self.save_settings();
                }
                self.dialog = None;
                self.focus = if attach { Focus::Term } else if self.vim { Focus::Normal } else { Focus::Message };
                // Fetch the list now: the watch push may not have the new task yet, and selection
                // would fall back to the first row.
                if let Ok(Reply::Tasks(tasks)) = request_at(sock.as_deref(), &Request::List { files: false }) {
                    self.user_event_tasks(m, tasks);
                }
                self.selected = Some((m, id));
                self.sync_session();
            }
            Ok(other) => d.error = format!("{other:?}"),
            Err(e) => d.error = e.to_string(),
        }
    }

    fn key(&mut self, ev: &KeyEvent) {
        let m = self.mods;
        if self.welcome {
            // Nothing else to drive yet: ⌘O opens a folder, ⌘N starts a new project, Esc goes back.
            if ev.logical_key == Key::Named(NamedKey::Escape) && welcome::project_root(&self.repo).is_some() {
                self.welcome = false;
            }
            if let (true, Key::Character(c)) = (app_mod(m), &ev.logical_key) {
                match c.to_lowercase().as_str() {
                    "o" => self.actions.push(Action::OpenProject(None)),
                    "n" => self.actions.push(Action::NewProject),
                    _ => {}
                }
            }
            return;
        }
        if self.menu && ev.logical_key == Key::Named(NamedKey::Escape) {
            self.menu = false;
            return;
        }
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
                    // Rounded to tenths so repeated steps land on the same glyph sizes in the atlas.
                    "=" | "+" => self.zoom = ((self.zoom + 0.1) * 10.0).round().min(20.0) / 10.0,
                    "-" | "_" => self.zoom = ((self.zoom - 0.1) * 10.0).round().max(6.0) / 10.0,
                    "0" | ")" => self.zoom = 1.0,
                    "f" => self.actions.push(Action::Focus(Focus::Filter)),
                    "i" => self.actions.push(Action::Focus(Focus::Message)),
                    "v" => self.paste(),
                    "a" if self.focus != Focus::Term => self.select_all(),
                    "c" | "x" if self.focus != Focus::Term => {
                        if let Some((from, to)) = self.selection() {
                            let focus = self.focus;
                            set_clipboard(&self.field(focus)[from..to]);
                            if c.to_lowercase() == "x" {
                                self.insert("");
                            }
                        }
                    }
                    _ => {}
                },
                Key::Named(NamedKey::Backspace) if self.focus != Focus::Term => {
                    let focus = self.focus;
                    self.field(focus).clear();
                    self.anchor = None;
                }
                // ⌘← / ⌘→: start or end of the field (⇧ selects to there, on macOS).
                Key::Named(k @ (NamedKey::ArrowLeft | NamedKey::ArrowRight)) if self.focus != Focus::Term => {
                    let focus = self.focus;
                    let end = if *k == NamedKey::ArrowLeft { 0 } else { self.field(focus).len() };
                    self.move_caret(end, m.shift_key() && cfg!(target_os = "macos"));
                }
                Key::Named(NamedKey::ArrowDown) => self.move_selection(1),
                Key::Named(NamedKey::ArrowUp) => self.move_selection(-1),
                Key::Named(NamedKey::Enter) if self.dialog.is_some() => self.actions.push(Action::Start(true)),
                Key::Named(NamedKey::Enter) if self.review.is_some() => {
                    let note = self.review.as_ref().and_then(|rv| rv.editing).filter(|_| self.focus == Focus::Note);
                    self.actions.push(Action::Review(note.map_or(ReviewAction::Commit(Ship::Merge), ReviewAction::SendNote)));
                }
                _ => {}
            }
            return;
        }
        if self.add_host.is_some() && ev.logical_key == Key::Named(NamedKey::Tab) {
            let order = [Focus::HostSsh, Focus::HostName, Focus::HostRepo];
            let i = order.iter().position(|f| *f == self.focus).map_or(0, |i| (i + 1) % order.len());
            self.actions.push(Action::Focus(order[i]));
            return;
        }
        if self.dialog.is_some() && ev.logical_key == Key::Named(NamedKey::Tab) {
            self.actions.push(if m.shift_key() { Action::PickModel(usize::MAX) } else { Action::PickAgent(usize::MAX) });
            return;
        }
        // Vim: Esc twice quickly is the agent's own Esc (stop Claude), while it's on screen.
        if self.vim && ev.logical_key == Key::Named(NamedKey::Escape) && !ev.repeat {
            let double = self.last_esc.is_some_and(|at| at.elapsed() < Duration::from_millis(400));
            self.last_esc = (!double).then(Instant::now);
            if double && self.review.is_none() && !self.overview && self.dialog.is_none() && self.add_host.is_none() && self.session.is_some() {
                self.send_input(b"\x1b".to_vec());
                self.focus = Focus::Normal;
                return;
            }
        }
        // Vim: Esc anywhere is normal mode, closing whatever was being typed into (the agent's
        // terminal doesn't get it).
        if self.vim && self.focus != Focus::Normal && ev.logical_key == Key::Named(NamedKey::Escape) {
            if self.add_host.take().is_none() && self.dialog.is_some() {
                self.actions.push(Action::CloseDialog);
            }
            if self.focus == Focus::Note {
                self.actions.push(Action::Review(ReviewAction::SaveNote));
            }
            self.focus = Focus::Normal;
            return;
        }
        if self.focus == Focus::Normal {
            self.normal_key(ev);
            return;
        }
        if self.focus == Focus::Term && self.overview && self.dialog.is_none() {
            // The table has no terminal: arrows move, ↵ opens the task, Esc goes back.
            match &ev.logical_key {
                Key::Named(NamedKey::ArrowDown) => self.move_selection(1),
                Key::Named(NamedKey::ArrowUp) => self.move_selection(-1),
                Key::Named(NamedKey::Enter) => self.open_from_overview(),
                Key::Named(NamedKey::Escape) => self.actions.push(Action::Overview(false)),
                _ => {}
            }
            return;
        }
        if self.focus == Focus::Term && self.review.is_some() && self.dialog.is_none() {
            if ev.logical_key == Key::Named(NamedKey::Escape) {
                self.actions.push(Action::Review(ReviewAction::Close));
            }
            return; // the agent's terminal isn't on screen
        }
        let typed = ev.text.as_ref().filter(|t| t.chars().any(|c| !c.is_control()) && !m.control_key() && !m.alt_key());
        let kind = self.current().map(|t| adapter::kind(&t.agent).to_string()).unwrap_or_default();
        if self.focus == Focus::Term && typed.is_some() && self.session.as_ref().and_then(|s| agent_box(&s.screen, &kind)).is_some() {
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
            self.select_all();
            return;
        }
        // ↑/↓ in an empty prompt or message box (or one showing a recalled entry) step through
        // earlier ones.
        if let Key::Named(k @ (NamedKey::ArrowUp | NamedKey::ArrowDown)) = &ev.logical_key {
            let up = *k == NamedKey::ArrowUp;
            let recalled = match (focus, &mut self.dialog) {
                (Focus::Prompt, Some(d)) => recall(&self.prompts, &mut d.hist, &mut d.prompt, up),
                (Focus::Message, _) => recall(&self.sent, &mut self.sent_hist, &mut self.message, up),
                _ => false,
            };
            if recalled {
                let len = self.field(focus).len();
                self.move_caret(len, false);
                return;
            }
        }
        let shift = m.shift_key();
        let word = m.alt_key() || linux_ctrl;
        let caret = self.caret;
        let sel = self.selection();
        let field = self.field(focus);
        let at = caret_at(caret, focus, field);
        // Caret keys: ←/→ by character (⌥ by word), ↑/Home to the start, ↓/End to the end; with ⇧
        // they select. Without, ←/→ drop a selection at its edge.
        let moved = match (&ev.logical_key, sel.filter(|_| !shift)) {
            (Key::Named(NamedKey::ArrowLeft), Some((from, _))) => Some(from),
            (Key::Named(NamedKey::ArrowRight), Some((_, to))) => Some(to),
            (Key::Named(NamedKey::ArrowLeft), _) if word => Some(word_left(field, at)),
            (Key::Named(NamedKey::ArrowLeft), _) => Some(field[..at].char_indices().next_back().map_or(0, |(i, _)| i)),
            (Key::Named(NamedKey::ArrowRight), _) if word => Some(word_right(field, at)),
            (Key::Named(NamedKey::ArrowRight), _) => Some(field[at..].chars().next().map_or(at, |c| at + c.len_utf8())),
            (Key::Named(NamedKey::ArrowUp | NamedKey::Home), _) => Some(0),
            (Key::Named(NamedKey::ArrowDown | NamedKey::End), _) => Some(field.len()),
            _ => None,
        };
        if let Some(to) = moved {
            self.move_caret(to, shift);
            return;
        }
        match &ev.logical_key {
            Key::Named(NamedKey::Escape) => {
                if self.add_host.is_some() {
                    self.add_host = None;
                    self.focus = self.rest();
                } else if self.dialog.is_some() {
                    self.actions.push(Action::CloseDialog);
                } else if focus == Focus::Note {
                    self.actions.push(Action::Review(ReviewAction::SaveNote));
                } else {
                    self.focus = self.rest();
                }
            }
            Key::Named(NamedKey::Tab) if focus == Focus::Message && m.shift_key() => self.send_input(b"\x1b[Z".to_vec()), // cycles the agent's mode
            Key::Named(NamedKey::Enter) if matches!(focus, Focus::Prompt | Focus::Note | Focus::Message) && shift => self.insert("\n"),
            Key::Named(NamedKey::Enter) => match focus {
                Focus::Message => {
                    let text = std::mem::take(&mut self.message);
                    self.anchor = None;
                    if let Some((m, id)) = self.selected.filter(|_| !text.trim().is_empty()) {
                        let kept = text.trim_end().to_string();
                        if self.sent.last() != Some(&kept) {
                            self.sent.push(kept);
                            let over = self.sent.len().saturating_sub(100);
                            self.sent.drain(..over);
                            self.save_settings();
                        }
                        self.sent_hist = None;
                        self.spawn_request(m, Tag::Message, Request::Prompt { id, text });
                    }
                }
                Focus::Note => self.actions.push(Action::Review(ReviewAction::SaveNote)),
                Focus::CommitMsg => self.actions.push(Action::Review(ReviewAction::Commit(Ship::Commit))),
                Focus::Prompt | Focus::Base | Focus::Repo => self.actions.push(Action::Start(false)),
                Focus::HostSsh | Focus::HostName | Focus::HostRepo => self.actions.push(Action::AddHost),
                _ => self.focus = self.rest(),
            },
            Key::Named(k @ (NamedKey::Backspace | NamedKey::Delete)) => {
                // The selection, or else the character (⌥: word) before the caret, or after it for Delete.
                if sel.is_none() {
                    let field = self.field(focus);
                    let to = match *k {
                        NamedKey::Delete => field[at..].chars().next().map_or(at, |c| at + c.len_utf8()),
                        _ if word => word_left(field, at),
                        _ => field[..at].char_indices().next_back().map_or(0, |(i, _)| i),
                    };
                    self.anchor = Some((focus, to));
                }
                self.insert("");
                self.scroll = 0.0;
            }
            _ => {
                if let Some(text) = ev.text.as_ref().filter(|t| t.chars().any(|c| !c.is_control())) {
                    let text: String = text.chars().filter(|c| !c.is_control()).collect();
                    self.insert(&text);
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
        if let Some(a) = &mut self.add_host {
            match focus {
                Focus::HostSsh => return &mut a.ssh,
                Focus::HostName => return &mut a.name,
                Focus::HostRepo => return &mut a.repo,
                _ => {}
            }
        }
        match (focus, &mut self.dialog) {
            (Focus::Prompt, Some(d)) => &mut d.prompt,
            (Focus::Base, Some(d)) => &mut d.base,
            (Focus::Repo, Some(d)) => &mut d.repo,
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
            self.insert(&text);
        }
    }

    /// The selected byte range in the focused field, if any.
    fn selection(&mut self) -> Option<(usize, usize)> {
        let (focus, a) = self.anchor.filter(|a| a.0 == self.focus)?;
        let caret = self.caret;
        let s = self.field(focus);
        let (a, at) = (caret_at(Some((focus, a)), focus, s), caret_at(caret, focus, s));
        (a != at).then(|| (a.min(at), a.max(at)))
    }

    fn select_all(&mut self) {
        let focus = self.focus;
        let len = self.field(focus).len();
        self.caret = Some((focus, len));
        self.anchor = Some((focus, 0));
    }

    /// Moves the focused field's caret to byte `to`, extending the selection if `extend`, else dropping it.
    fn move_caret(&mut self, to: usize, extend: bool) {
        let focus = self.focus;
        let caret = self.caret;
        let at = caret_at(caret, focus, self.field(focus));
        self.anchor = extend.then(|| self.anchor.filter(|a| a.0 == focus).unwrap_or((focus, at)));
        self.caret = Some((focus, to));
    }

    /// Types `text` into the focused field at the caret, replacing the selection.
    fn insert(&mut self, text: &str) {
        let focus = self.focus;
        let caret = self.caret;
        let (from, to) = match self.selection() {
            Some(sel) => sel,
            None => {
                let at = caret_at(caret, focus, self.field(focus));
                (at, at)
            }
        };
        self.field(focus).replace_range(from..to, text);
        self.caret = Some((focus, from + text.len()));
        self.anchor = None;
    }

    fn apply(&mut self) {
        for action in std::mem::take(&mut self.actions) {
            match action {
                Action::Select(id) => {
                    self.selected = Some(id);
                    (self.wheel, self.ticks, self.glide) = (0.0, 0.0, 0.0);
                    // An open dialog keeps its field until it's closed.
                    if self.dialog.is_none() && self.add_host.is_none() {
                        self.focus = self.rest();
                    }
                    self.sync_session();
                }
                // An ended agent has no message box to type into.
                Action::Focus(Focus::Message) if self.current().is_none_or(|t| group(&t.state) == 4) => {}
                Action::Focus(f) => {
                    self.confirm_delete = None;
                    if self.focus != f {
                        self.anchor = None;
                    }
                    self.focus = f;
                }
                Action::NewTask => self.open_dialog(),
                Action::CloseDialog => {
                    self.dialog = None;
                    self.focus = self.rest();
                }
                // ⌘T: the same slot in the other half of the list, dark <-> light.
                Action::ToggleTheme => self.set_theme((self.theme + 4) % THEMES.len()),
                Action::PickTheme(i) => {
                    self.set_theme(i);
                    self.menu = false;
                }
                Action::ToggleMenu => self.menu = !self.menu,
                Action::ToggleVim => {
                    self.vim = !self.vim;
                    (self.help, self.pending_g) = (false, false);
                    if matches!(self.focus, Focus::Term | Focus::Normal) {
                        self.focus = self.rest();
                    }
                    self.save_settings();
                }
                Action::Overview(on) => {
                    self.overview = on;
                    self.review = None;
                    if !on {
                        self.ov_host = None;
                        self.focus = self.rest();
                    }
                }
                Action::OvGroup(g) => {
                    self.ov_group = g;
                    self.ov_scroll = 0.0;
                }
                Action::OpenProject(path) => self.open_project(path, false),
                Action::NewProject => self.open_project(None, true),
                Action::Welcome => {
                    (self.welcome, self.review, self.overview) = (true, None, false);
                    self.welcome_error.clear();
                }
                Action::Revive(key @ (m, id), prompt) => {
                    let sock = self.sock(m);
                    match request_at(sock.as_deref(), &Request::Revive { id, prompt }) {
                        Ok(Reply::Ok) => {
                            // Its chat attaches by itself once the task shows as running.
                            self.revive_error = None;
                            (self.review, self.overview, self.selected) = (None, false, Some(key));
                            self.focus = if self.vim { Focus::Normal } else { Focus::Message };
                        }
                        Ok(r) => self.revive_error = Some((key, format!("unexpected reply: {r:?}"))),
                        Err(e) => self.revive_error = Some((key, e.to_string())),
                    }
                }
                Action::Delete(key @ (m, id)) => {
                    self.spawn_request(m, Tag::Delete, Request::Kill { id });
                    self.stats.remove(&key);
                    self.changes.remove(&key);
                }
                Action::OvHost(m) => {
                    self.ov_host = m;
                    self.ov_scroll = 0.0;
                }
                Action::Answer(allow) => {
                    if let Some((m, id)) = self.current().filter(|t| t.asking).map(|t| (t.machine, t.id)) {
                        let _ = request_at(self.sock(m).as_deref(), &Request::Answer { id, allow, message: String::new() });
                    }
                }
                Action::Input(bytes) => self.send_input(bytes),
                Action::PickAgent(i) => {
                    if let Some(d) = &mut self.dialog {
                        let agent = if i == usize::MAX { (d.agent + 1) % AGENTS.len() } else { i };
                        if agent != d.agent {
                            (d.agent, d.model) = (agent, 0); // models are per agent
                        }
                    }
                }
                Action::PickModel(i) => {
                    if let Some(d) = &mut self.dialog {
                        d.model = if i == usize::MAX { (d.model + 1) % models(AGENTS[d.agent]).len() } else { i };
                    }
                }
                Action::Start(attach) => self.start(attach),
                Action::OpenReview => {
                    if self.dialog.is_none() && (self.review.is_some() || self.reviewable()) {
                        self.open_review();
                    }
                }
                Action::Review(a) => self.review_action(a),
                Action::Toggle(key) => {
                    if !self.collapsed.remove(&key) {
                        self.collapsed.insert(key);
                    }
                }
                Action::PickHost(m) => self.pick_host(m),
                Action::OpenAddHost => {
                    self.add_host = Some(AddHost { ssh: String::new(), name: String::new(), repo: String::new(), test: None, testing: false });
                    self.focus = Focus::HostSsh;
                    self.anchor = None;
                }
                Action::TestHost => {
                    if let Some(a) = self.add_host.as_mut().filter(|a| !a.testing && !a.ssh.trim().is_empty()) {
                        a.testing = true;
                        a.test = None;
                        let (ssh, p) = (a.ssh.trim().to_string(), self.proxy.clone());
                        thread::spawn(move || {
                            let res = machine::endpoint(&ssh).map(|sock| format!("arranger ok · daemon at {}", sock.display()));
                            let _ = p.send_event(Msg::Tested(res.map_err(|e| e.to_string())));
                        });
                    }
                }
                Action::AddHost => self.save_host(),
                Action::Reconnect(m) => {
                    if let Some(tx) = self.hosts.get(m).and_then(|h| h.retry.as_ref()) {
                        let _ = tx.send(());
                    }
                }
                Action::RemoveHost(m) => {
                    if self.remove_armed == Some(m) {
                        self.remove_armed = None;
                        self.remove_host(m);
                    } else {
                        self.remove_armed = Some(m);
                    }
                }
            }
        }
    }

    /// Text field contents with the selection highlighted and, at byte `caret` of `s`, a blinking
    /// caret. `s` is field `src.0` from byte `src.1` on (all of it, or one wrapped line): a mouse
    /// press or drag over it puts the caret there. Returns the width.
    fn field_text(&self, r: &mut Renderer, t: &Theme, x: f32, y: f32, s: &str, font: usize, size: f32, max_w: f32, src: (Focus, usize), focused: bool, caret: Option<usize>, color: Rgba) -> f32 {
        let w = r.text(x, y, s, font, size, 400, t.text, max_w);
        let (top, h) = (y - size * 0.15, size * 1.35);
        let (field, base) = src;
        let band = (y - size * 0.35, y + size * 1.4);
        let over = |p: (f32, f32)| p.1 >= band.0 && p.1 < band.1;
        let press = self.click.filter(|p| over(*p) && p.0 >= x - 10.0 && p.0 < x + max_w + 10.0);
        if let Some(p) = press.or(Some(self.mouse).filter(|p| self.text_drag == Some(field) && over(*p))) {
            // The character boundary nearest the mouse.
            let mut cx = x;
            let at = s.char_indices().find(|&(_, ch)| {
                let a = r.advance(font, size, 400, ch);
                cx += a;
                p.0 < cx - a / 2.0
            });
            self.text_hit.set(Some((field, base + at.map_or(s.len(), |(i, _)| i))));
        }
        let sel = self.anchor.zip(self.caret).filter(|(a, c)| focused && a.0 == field && c.0 == field && a.1 != c.1);
        if let Some((from, to)) = sel.map(|(a, c)| (a.1.min(c.1), a.1.max(c.1))).filter(|&(from, to)| to > base && from <= base + s.len()) {
            let edge = |i: usize| {
                let i = i.clamp(base, base + s.len()) - base;
                if s.is_char_boundary(i) { r.measure(font, size, 400, &s[..i]).min(w) } else { 0.0 }
            };
            // Past the line's end it runs on to the next line: a sliver shows that.
            let end = if to > base + s.len() { edge(to) + size * 0.3 } else { edge(to) };
            r.rect(Rect::new(x + edge(from), top, end - edge(from), h), 2.0, alpha(t.work, 0x50));
        } else if let Some(at) = caret.filter(|_| focused && self.caret_on) {
            let cx = x + 1.0 + if at >= s.len() { w } else { r.measure(font, size, 400, &s[..at]).min(w) };
            r.rect(Rect::new(cx, top, 1.5, h), 0.0, color);
        }
        w
    }

    /// The caret's byte offset in `s` while field `focus` has focus.
    fn caret_for(&self, focus: Focus, s: &str) -> Option<usize> {
        (self.focus == focus).then(|| caret_at(self.caret, focus, s))
    }

    /// A button or row: was it clicked this frame? Hover gives it a tint and a hand cursor.
    fn hit(&self, r: &mut Renderer, rect: Rect) -> bool {
        self.hit_as(r, rect, Cursor::Pointer)
    }

    fn hit_as(&self, r: &mut Renderer, rect: Rect, cursor: Cursor) -> bool {
        r.target(rect, cursor);
        self.click.is_some_and(|p| rect.contains(p) && r.in_clip(p))
    }

    // ---------- drawing ----------

    fn draw(&mut self) {
        let Some(mut gfx) = self.gfx.take() else { return };
        let size = gfx.window.inner_size();
        let dpi = gfx.window.scale_factor() as f32 * self.zoom;
        let (w, h) = (size.width as f32 / dpi, size.height as f32 / dpi);
        let t = self.theme();
        let r = &mut gfx.r;
        r.begin(size.width, size.height, dpi, t.bg);

        self.probe_changes();
        // Under an open dialog nothing reacts: the views below draw without the click.
        let modal_click = (self.dialog.is_some() || self.add_host.is_some()).then(|| self.click.take());
        self.draw_title(r, t, w);
        if self.welcome {
            self.draw_welcome(r, t, Rect::new(0.0, TITLE_H, w, h - TITLE_H));
        } else if self.review.is_some() {
            self.draw_review(r, t, Rect::new(0.0, TITLE_H, w, h - TITLE_H - STATUS_H));
        } else if self.overview {
            self.draw_overview(r, t, Rect::new(0.0, TITLE_H, w, h - TITLE_H - STATUS_H));
        } else {
            self.draw_sidebar(r, t, h);
            self.draw_main(r, t, Rect::new(self.sidebar_w, TITLE_H, w - self.sidebar_w, h - TITLE_H - STATUS_H));
        }
        if !self.welcome && (self.review.is_some() || !self.overview) {
            let (x, top) = if self.review.is_some() { (self.rail_w, TITLE_H + review::STRIP_H) } else { (self.sidebar_w, TITLE_H) };
            r.target(Rect::new(x - 4.0, top, 8.0, h - top - STATUS_H), Cursor::Resize);
        }
        if !self.welcome {
            self.draw_status(r, t, w, h);
        }
        if let Some(click) = modal_click {
            self.click = click;
        }
        if self.dialog.is_some() {
            self.draw_dialog(r, t, w, h);
        }
        if self.add_host.is_some() {
            self.draw_add_host(r, t, w, h);
        }
        if self.help && self.vim {
            self.draw_help(r, t, w, h);
        }
        if self.menu {
            self.draw_menu(r, t);
        }
        let cursor = r.finish(self.mouse);
        self.hot = r.hot(self.mouse);
        if cursor != self.cursor {
            self.cursor = cursor;
            gfx.window.set_cursor(match cursor {
                Cursor::Arrow => CursorIcon::Default,
                Cursor::Pointer => CursorIcon::Pointer,
                Cursor::Text => CursorIcon::Text,
                Cursor::Resize => CursorIcon::ColResize,
            });
        }
        if let Ok(path) = std::env::var("ARRANGER_SNAPSHOT") {
            let _ = r.snapshot(&path);
        }
        let _ = gfx.surface.swap_buffers(&gfx.context);
        self.gfx = Some(gfx);

        let clicked = self.click.take().is_some();
        let had_actions = !self.actions.is_empty();
        self.apply();
        // After the click's own actions (which focus the field): a press puts the caret there,
        // ⇧ extending the selection, and starts a drag; dragging moves the caret, selecting. A field
        // the click didn't focus was under something else (the dialog's scrim).
        if let Some((field, at)) = self.text_hit.take().filter(|h| h.0 == self.focus) {
            if clicked {
                let end = self.field(field).len();
                let from = self.caret.filter(|c| c.0 == field).map_or(end, |c| c.1.min(end));
                let keep = self.mods.shift_key() && self.focus == field;
                self.anchor = Some(self.anchor.filter(|a| keep && a.0 == field).unwrap_or((field, if keep { from } else { at })));
                self.text_drag = Some(field);
            }
            if self.caret != Some((field, at)) {
                self.caret = Some((field, at));
                self.redraw();
            }
        }
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
        if !self.welcome {
            r.text(x + 8.0, r.center(bar, UI, 13.0), "/", UI, 13.0, 400, t.faint, 20.0);
        }
        let x = x + 14.0;
        let rw = if self.welcome { 0.0 } else { r.text(x + 10.0, r.center(bar, MONO, 11.0), &repo, MONO, 11.0, 400, t.text2, 400.0) };
        // The project path: click it to switch projects.
        if !self.welcome && self.hit(r, Rect::new(x + 4.0, 7.0, rw + 12.0, 22.0)) {
            self.actions.push(Action::Welcome);
        }

        // Views: Workspace | Overview | Review.
        let mut nx = x + 26.0 + rw;
        let (review, overview) = (self.review.is_some(), self.overview && self.review.is_none());
        let tabs = [("Workspace", !review && !overview), ("Overview", overview), ("Review", review)];
        for (label, on) in tabs.into_iter().filter(|_| !self.welcome) {
            let tw = r.measure(UI, 12.0, if on { 500 } else { 400 }, label) + 20.0;
            let tab = Rect::new(nx, 0.0, tw, TITLE_H);
            if on {
                r.rect(Rect::new(tab.x + 10.0, TITLE_H - 2.0, tw - 20.0, 2.0), 0.0, t.text);
            }
            let off = label == "Review" && !on && !self.reviewable();
            r.text(tab.x + 10.0, r.center(tab, UI, 12.0), label, UI, 12.0, if on { 500 } else { 400 }, if on { t.text } else if off { alpha(t.text3, 90) } else { t.text2 }, tw);
            if self.hit(r, tab) && !on && !off {
                self.actions.push(match label {
                    "Review" => Action::OpenReview,
                    "Overview" => Action::Overview(true),
                    _ if review => Action::Review(ReviewAction::Close),
                    _ => Action::Overview(false),
                });
            }
            nx += tw;
        }

        // Scheme menu button: [swatch] Graphite ˅
        let label_w = r.measure(UI, 12.0, 400, t.name);
        let b = Rect::new(w - 10.0 - (label_w + 51.0), 5.0, label_w + 51.0, 26.0);
        self.scheme_btn = b;
        r.frame(b, 3.0, t.bg, t.border);
        r.rect(Rect::new(b.x + 7.0, b.y + 6.0, 14.0, 14.0), 3.0, t.border2);
        r.rect(Rect::new(b.x + 8.0, b.y + 7.0, 6.0, 12.0), 2.0, t.bg);
        r.rect(Rect::new(b.x + 14.0, b.y + 7.0, 6.0, 12.0), 2.0, t.needs);
        r.text(b.x + 28.0, r.center(b, UI, 12.0), t.name, UI, 12.0, 400, t.text2, label_w + 4.0);
        r.path(5, "M16 24L48 24L32 42Z", None, b.x + b.w - 17.0, b.y + 8.0, 10.0, t.text3);
        if self.hit(r, b) {
            self.actions.push(Action::ToggleMenu);
        }
    }

    fn draw_sidebar(&mut self, r: &mut Renderer, t: &Theme, h: f32) {
        let side = Rect::new(0.0, TITLE_H, self.sidebar_w, h - TITLE_H - STATUS_H);
        r.rect(side, 0.0, t.panel);
        r.rect(Rect::new(self.sidebar_w - 1.0, TITLE_H, 1.0, side.h), 0.0, t.border);

        // Filter + new task.
        let f = Rect::new(8.0, TITLE_H + 8.0, self.sidebar_w - 16.0 - 32.0, 26.0);
        let focused = self.focus == Focus::Filter;
        r.frame(f, 3.0, t.bg, if focused { t.work } else { t.border });
        for (i, lw) in [16.0, 10.0, 4.0].iter().enumerate() {
            r.rect(Rect::new(f.x + 8.0 + (16.0 - lw) / 2.0 * 0.7, f.y + 8.0 + i as f32 * 4.0, lw * 0.7, 1.4), 0.5, t.text3);
        }
        let n = self.all_tasks().count();
        let hosts = self.hosts.iter().filter(|h| !h.removed).count();
        let placeholder = if hosts > 1 { format!("Filter {n} agents on {hosts} machines") } else { format!("Filter {n} agents") };
        if self.filter.is_empty() && !focused {
            r.text(f.x + 25.0, r.center(f, UI, 12.0), &placeholder, UI, 12.0, 400, t.text3, f.w - 50.0);
        } else {
            let filter = self.filter.clone();
            self.field_text(r, t, f.x + 25.0, r.center(f, UI, 12.0), &filter, UI, 12.0, f.w - 50.0, (Focus::Filter, 0), focused, self.caret_for(Focus::Filter, &filter), t.work);
        }
        r.text(f.x + f.w - 8.0 - r.measure(MONO, 10.0, 400, &mod_label("F")), r.center(f, MONO, 10.0), &mod_label("F"), MONO, 10.0, 400, t.text3, 60.0);
        if self.hit_as(r, f, Cursor::Text) {
            self.actions.push(Action::Focus(Focus::Filter));
        }
        let plus = Rect::new(self.sidebar_w - 8.0 - 26.0, f.y, 26.0, 26.0);
        r.rect(plus, 3.0, t.primary_bg);
        r.rect(Rect::new(plus.x + 7.5, plus.y + 12.0, 11.0, 2.0), 1.0, t.primary_fg);
        r.rect(Rect::new(plus.x + 12.0, plus.y + 7.5, 2.0, 11.0), 1.0, t.primary_fg);
        if self.hit(r, plus) {
            self.actions.push(Action::NewTask);
        }

        // Sections: Needs you (every machine), then one per machine.
        let add = Rect::new(8.0, side.y + side.h - 8.0 - 28.0, self.sidebar_w - 16.0, 28.0);
        let list = Rect::new(0.0, f.y + f.h + 8.0, self.sidebar_w - 1.0, add.y - 6.0 - (f.y + f.h + 8.0));
        r.set_clip(Some(list));
        let multi = self.hosts.iter().filter(|h| !h.removed).count() > 1;
        let clicked = |p: Option<(f32, f32)>| p.is_some_and(|p| list.contains(p));
        let mut y = list.y - self.scroll;
        let mut picks = Vec::new();
        let sections = self.sections();
        for (i, sec) in sections.iter().enumerate() {
            let folded = self.collapsed.contains(&sec.key);
            let chevron = |r: &mut Renderer, cy: f32| {
                let (id, path) = if folded { (6, "M24 16L42 32L24 48Z") } else { (5, "M16 24L48 24L32 42Z") };
                r.path(id, path, None, self.sidebar_w - 26.0, cy - 5.0, 10.0, t.text3);
            };
            let mut host_down = None;
            match sec.host {
                None if sec.tasks.is_empty() => continue,
                None => {
                    let head = Rect::new(6.0, y + 4.0, self.sidebar_w - 13.0, GROUP_H - 4.0);
                    let lw = r.text(14.0, r.center(head, UI, 10.5), "NEEDS YOU", UI, 10.5, 500, t.needs, 150.0);
                    r.text(20.0 + lw, r.center(head, MONO, 10.5), &sec.tasks.len().to_string(), MONO, 10.5, 400, t.needs, 40.0);
                    if multi {
                        let note = "all machines";
                        let nw = r.measure(UI, 10.5, 400, note);
                        r.text(self.sidebar_w - 34.0 - nw, r.center(head, UI, 10.5), note, UI, 10.5, 400, t.text3, nw + 2.0);
                    }
                    chevron(r, head.y + head.h / 2.0);
                    if self.hit(r, head) && clicked(self.click) {
                        picks.push(Action::Toggle(sec.key.clone()));
                    }
                    y += GROUP_H;
                }
                Some(m) => {
                    let h = &self.hosts[m];
                    if i > 0 {
                        r.rect(Rect::new(6.0, y + 4.0, self.sidebar_w - 13.0, 1.0), 0.0, t.border);
                    }
                    let head = Rect::new(6.0, y + 8.0, self.sidebar_w - 13.0, 38.0);
                    let (dot, status, sc) = match &h.link {
                        Link::Up(ms) => {
                            let mut s = host_summary(&h.tasks);
                            if h.cfg.is_some() {
                                s += &format!(" · {ms} ms");
                            }
                            (t.done, s, t.text3)
                        }
                        Link::Connecting => (t.needs, "connecting…".to_string(), t.text3),
                        Link::Down(e) => {
                            host_down = Some(m);
                            (t.del, format!("unreachable · {e}"), t.del)
                        }
                    };
                    r.rect(Rect::new(head.x + 6.0, head.y + 9.0, 7.0, 7.0), 3.5, dot);
                    let top = Rect::new(head.x, head.y + 2.0, head.w, 20.0);
                    let nw = r.text(head.x + 20.0, r.center(top, UI, 12.5), h.name(), UI, 12.5, 600, t.text, 140.0);
                    let addr = h.cfg.as_ref().map_or("local", |c| c.ssh.as_str());
                    r.text(head.x + 26.0 + nw, r.center(top, MONO, 10.5), addr, MONO, 10.5, 400, t.text3, head.w - nw - 60.0);
                    let sub = Rect::new(head.x + 20.0, head.y + 21.0, head.w - 40.0, 15.0);
                    let sy = r.center(sub, UI, 11.0);
                    // "N ended" is a link to them in Overview.
                    let ended = h.tasks.iter().filter(|t| group(&t.state) == 4).count();
                    let label = format!("{ended} ended");
                    let mut link = Rect::default();
                    match status.find(&label).filter(|_| ended > 0 && matches!(h.link, Link::Up(_))) {
                        Some(at) => {
                            let x = sub.x + r.text(sub.x, sy, &status[..at], UI, 11.0, 400, sc, sub.w);
                            let lw = r.text(x, sy, &label, UI, 11.0, 400, t.text2, sub.x + sub.w - x);
                            r.rect(Rect::new(x, sy + 13.0, lw, 1.0), 0.0, alpha(t.text2, 0x60));
                            r.text(x + lw, sy, &status[at + label.len()..], UI, 11.0, 400, sc, sub.x + sub.w - x - lw);
                            link = Rect::new(x - 2.0, sub.y, lw + 4.0, sub.h);
                            if self.hit(r, link) && clicked(self.click) {
                                picks.extend([Action::Overview(true), Action::OvGroup(Some(4)), Action::OvHost(Some(m))]);
                            }
                        }
                        None => {
                            r.text(sub.x, sy, &status, UI, 11.0, 400, sc, sub.w);
                        }
                    }
                    chevron(r, top.y + top.h / 2.0);
                    if self.hit(r, head) && clicked(self.click) && !self.click.is_some_and(|p| link.contains(p)) {
                        picks.push(Action::Toggle(sec.key.clone()));
                    }
                    y += 50.0;
                }
            }
            if let Some(m) = host_down {
                // Reconnect / Remove under an unreachable machine.
                let mut bx = 26.0;
                let armed = self.remove_armed == Some(m);
                for (label, action) in [("Reconnect", Action::Reconnect(m)), (if armed { "Click again to remove" } else { "Remove" }, Action::RemoveHost(m))] {
                    let bw = r.measure(UI, 11.5, 400, label) + 16.0;
                    let b = Rect::new(bx, y, bw, 22.0);
                    r.frame(b, 3.0, t.panel, if armed && bx > 26.0 { t.del } else { t.border2 });
                    r.text(b.x + 8.0, r.center(b, UI, 11.5), label, UI, 11.5, 400, if bx > 26.0 { t.del } else { t.text }, bw);
                    if self.hit(r, b) && clicked(self.click) {
                        picks.push(action);
                    }
                    bx += bw + 6.0;
                }
                y += 30.0;
                continue;
            }
            if folded {
                continue;
            }
            if sec.host.is_some_and(|m| matches!(self.hosts[m].link, Link::Up(_))) && sec.tasks.is_empty() && sec.ended == 0 {
                let note = if self.filter.is_empty() { format!("No tasks · {} to start one", mod_label("N")) } else { "No matches".into() };
                r.text(26.0, y + 2.0, &note, UI, 11.5, 400, t.text3, self.sidebar_w - 40.0);
                y += 24.0;
            }
            for k in 0..=sec.tasks.len() {
                let Some(task) = sec.tasks.get(k) else { break };
                let row = Rect::new(6.0, y, self.sidebar_w - 13.0, ROW2_H);
                let chip = (sec.host.is_none() && multi).then(|| self.hosts[task.machine].name());
                if self.draw_row(r, t, task, row, chip) && clicked(self.click) {
                    picks.push(Action::Select((task.machine, task.id)));
                }
                y += ROW2_H;
            }
        }
        drop(sections);
        let content_h = y + self.scroll - list.y;
        self.scroll = self.scroll.min((content_h - list.h + 8.0).max(0.0));
        r.set_clip(None);

        r.frame(add, 4.0, t.panel, t.border2);
        let label = "+ Add machine";
        let lw = r.measure(UI, 12.0, 400, label);
        r.text(add.x + (add.w - lw) / 2.0, r.center(add, UI, 12.0), label, UI, 12.0, 400, t.text2, add.w);
        if self.hit(r, add) {
            picks.push(Action::OpenAddHost);
        }
        self.actions.extend(picks);
    }

    /// One task row: state icon, title, agent and age (or the machine, in Needs you), and what
    /// it's doing. Returns whether it was clicked.
    fn draw_row(&self, r: &mut Renderer, t: &Theme, task: &TaskInfo, row: Rect, machine: Option<&str>) -> bool {
        let g = group(&task.state);
        let queue_pos = self.hosts[task.machine].tasks.iter().filter(|x| x.state == "queued" && x.id <= task.id).count();
        let line = status_line(task, queue_pos);
        let top = Rect::new(row.x, row.y + 2.0, row.w, 22.0);
        let on = self.selected == Some((task.machine, task.id));
        let bg = if on { t.sel } else { t.panel };
        if on {
            r.frame(row, 3.0, bg, t.sel_ring);
        }
        state_icon(r, t, &task.state, row.x + 8.0, top.y + top.h / 2.0, task.since, self.sweep());
        let text_x = row.x + 28.0;
        let right_w = if let Some(name) = machine {
            let cw = r.measure(MONO, 10.0, 400, name) + 10.0;
            let chip = Rect::new(row.x + row.w - 8.0 - cw, top.y + 4.0, cw, 15.0);
            r.frame(chip, 3.0, bg, t.border2);
            r.text(chip.x + 5.0, r.center(chip, MONO, 10.0), name, MONO, 10.0, 400, t.text2, cw);
            cw
        } else {
            let kind = adapter::kind(&task.agent);
            let kw = r.measure(MONO, 10.5, 400, kind);
            r.text(row.x + row.w - 8.0 - kw, r.center(top, MONO, 10.5), kind, MONO, 10.5, 400, t.text3, kw + 2.0);
            kw
        };
        let tc = if g == 4 { t.text3 } else if on { t.tc_on } else { t.tc };
        let room = row.w - 28.0 - right_w - 14.0;
        let bw = if task.shipped.is_empty() { 0.0 } else { r.measure(UI, 10.5, 500, &format!("✓ {}", task.shipped)) + 16.0 };
        let tw = r.text(text_x, r.center(top, UI, 12.5), &title(task), UI, 12.5, 400, tc, room - bw);
        ship_badge(r, t, &task.shipped, text_x + tw + 6.0, top.y + top.h / 2.0);
        let sub = Rect::new(text_x, top.y + top.h - 3.0, row.w - 36.0, 16.0);
        let sc = match g {
            0 => t.needs,
            4 => t.faint,
            _ => t.text3,
        };
        r.text(sub.x, r.center(sub, UI, 11.5), &line.replace('\n', " "), UI, 11.5, 400, sc, sub.w);
        self.hit(r, row)
    }

    fn draw_main(&mut self, r: &mut Renderer, t: &Theme, area: Rect) {
        let Some(task) = self.current().cloned() else {
            let msg = format!("Start a task with {}", mod_label("N"));
            let mw = r.measure(UI, 13.0, 400, &msg);
            logo(r, area.x + (area.w - 64.0) / 2.0, area.y + area.h / 2.0 - 84.0, 64.0, t.faint, t.needs);
            r.text(area.x + (area.w - mw) / 2.0, area.y + area.h / 2.0 - 8.0, &msg, UI, 13.0, 400, t.text3, area.w);
            // A button too, for anyone who doesn't know the shortcut.
            let label = "New task";
            let bw = r.measure(UI, 13.0, 500, label) + 32.0;
            let b = Rect::new(area.x + (area.w - bw) / 2.0, area.y + area.h / 2.0 + 22.0, bw, 34.0);
            r.rect(b, 4.0, t.primary_bg);
            r.text(b.x + 16.0, r.center(b, UI, 13.0), label, UI, 13.0, 500, t.primary_fg, bw);
            if self.hit(r, b) {
                self.actions.push(Action::NewTask);
            }
            return;
        };
        let color = state_color(t, &task.state);
        let tint = alpha(color, if t.dark { 0x14 } else { 0x12 });

        // Header: why it's here, the title and Review; under them, the task's details.
        let head = Rect::new(area.x, area.y, area.w, HEADER_H);
        r.rect(Rect::new(area.x, area.y + HEADER_H - 1.0, area.w, 1.0), 0.0, t.border);
        let top = Rect::new(area.x, area.y + 10.0, area.w, 28.0);
        let why = reason(&task);
        let pw = r.measure(UI, 11.0, 500, &why).min(260.0) + 23.0;
        let pill = Rect::new(area.x + 20.0, top.y + 4.0, pw, 20.0);
        r.rect(pill, 3.0, tint);
        r.rect(Rect::new(pill.x + 6.0, pill.y + 7.5, 5.0, 5.0), 1.0, color);
        r.text(pill.x + 16.0, r.center(pill, UI, 11.0), &why, UI, 11.0, 500, color, pw - 20.0);
        let kind = adapter::kind(&task.agent);
        let host = &self.hosts[task.machine];
        let review_label = "Review changes";
        let review_key = mod_label("R");
        let bw = r.measure(UI, 12.0, 500, review_label) + r.measure(MONO, 10.5, 400, &review_key) + 30.0;
        let review = Rect::new(head.x + head.w - 16.0 - bw, top.y, bw, 28.0);
        let can = self.reviewable();
        let (bg, fg) = if can { (t.primary_bg, t.primary_fg) } else { (t.raised, alpha(t.text3, 120)) };
        r.rect(review, 4.0, bg);
        let rlw = r.text(review.x + 12.0, r.center(review, UI, 12.0), review_label, UI, 12.0, 500, fg, bw);
        r.text(review.x + 18.0 + rlw, r.center(review, MONO, 10.5), &review_key, MONO, 10.5, 400, alpha(fg, 160), 60.0);
        if can && self.hit(r, review) {
            self.actions.push(Action::OpenReview);
        }
        let tx = pill.x + pw + 8.0 + ship_badge(r, t, &task.shipped, pill.x + pw + 8.0, pill.y + pill.h / 2.0) + 4.0;
        r.text(tx, r.center(top, UI, 15.0), &title(&task), UI, 15.0, 600, t.text, (review.x - tx - 16.0).max(40.0));

        // Details: label over value, as many as fit.
        let stats = self.stats.get(&(task.machine, task.id)).filter(|_| self.changes.get(&(task.machine, task.id)).is_some_and(|c| c.1.is_some()));
        let from = stats.map(|s| s.0.as_str()).filter(|b| !b.is_empty() && *b != "HEAD" && *b != task.branch);
        let branch = match from {
            Some(base) => format!("{} from {base}", task.branch),
            None => task.branch.clone(),
        };
        let changes: Vec<(String, Rgba)> = match stats {
            Some(&(_, 0, ..)) => vec![("none yet".into(), t.text3)],
            Some(&(_, files, added, removed)) => vec![
                (format!("+{added}"), t.add),
                (format!(" −{removed}"), t.del),
                (format!(" · {files} file{}", if files == 1 { "" } else { "s" }), t.text),
            ],
            None => vec![("—".into(), t.text3)],
        };
        let details: [(&str, Vec<(String, Rgba)>); 5] = [
            ("Agent", vec![(kind.to_string(), accent(kind))]),
            ("Branch", vec![(branch, t.text)]),
            ("Machine", vec![(host.name().to_string(), t.text)]),
            ("Changes", changes),
            ("Elapsed", vec![(age(&task), t.text)]),
        ];
        let (ly, vy) = (area.y + 46.0, area.y + 60.0);
        let mut dx = area.x + 20.0;
        for (label, parts) in details {
            let w = parts.iter().map(|(s, _)| r.measure(MONO, 11.5, 400, s)).sum::<f32>().max(r.measure(UI, 10.5, 400, label));
            if dx + w > head.x + head.w - 16.0 {
                break;
            }
            r.text(dx, ly, label, UI, 10.5, 400, t.text3, w + 2.0);
            let mut px = dx;
            for (s, c) in &parts {
                px += r.text(px, vy, s, MONO, 11.5, 400, *c, w + 2.0);
            }
            dx += w + 32.0;
        }

        // Message box at the bottom; once the agent has ended, a bar to bring it back instead.
        let ended = group(&task.state) == 4;
        let input_area = Rect::new(area.x, area.y + area.h - 49.0, area.w, 49.0);
        if ended {
            r.rect(Rect::new(area.x, input_area.y, area.w, 1.0), 0.0, t.border);
            let label = format!("Revive {}", adapter::kind(&task.agent));
            let bw = r.measure(UI, 12.5, 500, &label) + 28.0;
            let b = Rect::new(area.x + area.w - 10.0 - bw, input_area.y + 9.0, bw, 32.0);
            let key = (task.machine, task.id);
            let note = match self.revive_error.as_ref().filter(|e| e.0 == key) {
                Some((_, e)) => (format!("Couldn't revive: {e}"), t.del),
                None => (format!("{} has ended · revive it to pick up where it left off", adapter::kind(&task.agent)), t.text3),
            };
            r.text(area.x + 16.0, r.center(b, UI, 12.5), &note.0, UI, 12.5, 400, note.1, b.x - area.x - 32.0);
            r.rect(b, 4.0, t.primary_bg);
            r.text(b.x + 14.0, r.center(b, UI, 12.5), &label, UI, 12.5, 500, t.primary_fg, bw);
            if self.hit(r, b) {
                self.actions.push(Action::Revive(key, String::new()));
            }
        }
        // The message box grows upwards over the chat as lines are added (the agent's pane keeps
        // its size: resizing would make it redraw), up to MSG_LINES, then shows the caret's part.
        let mode = self.session.as_ref().filter(|s| s.task == (task.machine, task.id)).and_then(|s| agent_box(&s.screen, kind)).map(|(_, mode)| mode);
        let chip_label = mode.map(|m| if m.is_empty() { "default mode".to_string() } else { m });
        let chip_w = chip_label.as_ref().map(|l| r.measure(MONO, 10.5, 400, l) + r.measure(UI, 10.0, 400, "⇧Tab") + 26.0);
        let text_w = area.w - 80.0 - chip_w.map_or(0.0, |w| w + 8.0);
        let message = self.message.clone();
        let lines = wrap(r, &message, UI, 12.5, text_w);
        let caret = self.caret_for(Focus::Message, &message).map(|at| caret_line(&message, &lines, at));
        let shown = lines.len().min(MSG_LINES);
        let first = caret.map_or(0, |c| (c.0 + 1).saturating_sub(shown)).min(lines.len() - shown);
        let grow = (shown - 1) as f32 * MSG_LINE_H;
        let ib = Rect::new(area.x + 10.0, input_area.y + 9.0 - grow, area.w - 20.0, 32.0 + grow);

        // Terminal section.
        let sec = Rect::new(area.x, area.y + HEADER_H, area.w, input_area.y - area.y - HEADER_H);
        r.rect(sec, 0.0, t.term);
        if self.hit_as(r, sec, Cursor::Text) && !self.click.is_some_and(|p| !ended && ib.contains(p)) {
            self.actions.push(Action::Focus(Focus::Term));
        }
        let bar_h = if task.need.is_some() { 48.0 } else { 0.0 };
        let grid = Rect::new(sec.x + 16.0, sec.y + 12.0, sec.w - 32.0, sec.h - 20.0 - bar_h);
        let cell_w = r.advance(MONO, TERM_SIZE, 400, '0');
        let size = (((grid.w / cell_w) as u16).max(2), ((grid.h / LINE_H) as u16).max(1));
        self.pane = size;
        let term_focus = self.focus == Focus::Term && self.dialog.is_none();
        let mut box_rows = None;
        let extra = self.session.as_ref().filter(|s| s.task == (task.machine, task.id)).map_or(0, |s| s.extra as usize);
        let screen = match (&mut self.session, &mut self.history) {
            (Some(s), _) if s.task == (task.machine, task.id) => {
                let size = (size.0, size.1 + s.extra);
                if s.size != size {
                    s.size = size;
                    s.screen.resize(size.0, size.1);
                    let _ = s.writer.write_all(&proto::frame(&Request::Resize { cols: size.0, rows: size.1 }));
                }
                Some(&s.screen)
            }
            (_, Some((key, screen))) if *key == (task.machine, task.id) => Some(&*screen), // kept at the size it was drawn for
            _ => None,
        };
        match screen {
            Some(screen) => {
                // Stop above the agent's own input box: ours below replaces it.
                let found = agent_box(screen, kind).map(|(row, _)| row);
                // Down to its last text, not the screen bottom: codex draws inline, so the blank rows
                // below its box grow with the pty, and counting them grew the pty again each frame
                // (45→49→53→57→53 rows), until codex panicked on the shrink.
                let g = screen.grid();
                let text = |v: usize| (0..g.columns()).any(|c| g[Line(v as i32)][Column(c)].c != ' ');
                box_rows = found.map(|row| ((row..g.screen_lines()).rfind(|&v| text(v)).unwrap_or(row) + 1 - row) as u16);
                // Scrolled back, the box can't be found, but it's still the bottom `extra` rows:
                // keep them out, or the chat jumps by the box's height as scrolling starts.
                let scrolled = screen.grid().display_offset() > 0;
                let rows = found.or((scrolled && extra > 0).then(|| screen.grid().screen_lines() - extra)).unwrap_or(screen.grid().screen_lines()) as f32;
                // The last text sits on the pane's bottom, just above our input box, with scrollback
                // filling the space above it. When everything fits, the first text sits at the top instead.
                let g = screen.grid();
                let off = g.display_offset() as i32;
                let blank = |v: i32| (0..g.columns()).all(|c| g[Line(v - off)][Column(c)].c == ' ');
                let last = (0..rows as i32).rev().find(|&v| !blank(v));
                let mut shift = grid.h - last.map_or(grid.h, |l| (l + 1) as f32 * LINE_H);
                let above = g.history_size() as i32 - off; // view rows of scrollback above row 0
                if (shift / LINE_H).ceil() as i32 >= above {
                    let first = (-above..=last.unwrap_or(-1)).find(|&v| !blank(v)).unwrap_or(0);
                    shift = shift.min(-first as f32 * LINE_H);
                }
                let frac = self.wheel * LINE_H; // scrolled partway into the line above
                r.set_clip(Some(Rect::new(sec.x, sec.y, sec.w, (sec.h - bar_h).min(grid.y + shift + frac - sec.y + rows * LINE_H))));
                draw_term(r, t, screen, Rect { y: grid.y + shift + frac, ..grid }, sec.y, cell_w, term_focus);
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
        // Learn the agent's box height from the live screen; the next frame resizes to hide it.
        // Exactly the box: more and a full-screen agent's top rows (your message) fall off the
        // pane. Not the blank rows above it either, which come and go as text streams; resizing
        // each time made the agent redraw (stutter).
        if let (Some(b), Some(s)) = (box_rows.filter(|&b| b <= 16), self.session.as_mut().filter(|s| s.task == (task.machine, task.id))) {
            s.extra = b;
        }
        if let Some(need) = &task.need {
            let bar = Rect::new(sec.x + 16.0, sec.y + sec.h - 16.0 - 38.0 - if ended { 0.0 } else { grow }, sec.w - 32.0, 38.0);
            self.draw_need(r, t, bar, &task, need, color, tint);
        }
        if !ended {
            r.rect(Rect::new(area.x, ib.y - 9.0, area.w, input_area.y + input_area.h - ib.y + 9.0), 0.0, t.bg);
            r.rect(Rect::new(area.x, ib.y - 9.0, area.w, 1.0), 0.0, t.border);
            let focused = self.focus == Focus::Message;
            r.frame(ib, 4.0, t.panel, if focused { t.work } else { t.border2 });
            let ib_hit = self.hit_as(r, ib, Cursor::Text); // before the chip, so the chip is on top
            let row = |i: usize| Rect::new(ib.x, ib.y + i as f32 * MSG_LINE_H, ib.w, 32.0);
            r.text(ib.x + 10.0, r.center(row(0), MONO, 12.5), "›", MONO, 12.5, 400, t.text3, 20.0);
            // The agent's mode (read from its hidden footer); click or ⇧Tab cycles it.
            if let (Some(label), Some(cw)) = (&chip_label, chip_w) {
                let last = row(shown - 1);
                let chip = Rect::new(ib.x + ib.w - 34.0 - cw, last.y + 6.0, cw, 20.0);
                r.rect(chip, 3.0, t.raised);
                let lw = r.text(chip.x + 8.0, r.center(chip, MONO, 10.5), label, MONO, 10.5, 400, t.text2, cw);
                r.text(chip.x + 16.0 + lw, r.center(chip, UI, 10.0), "⇧Tab", UI, 10.0, 400, t.text3, 40.0);
                if self.hit(r, chip) {
                    self.actions.push(Action::Input(b"\x1b[Z".to_vec()));
                }
            }
            if self.message.is_empty() && !focused {
                let host = &self.hosts[task.machine];
                let on = host.cfg.as_ref().map(|_| format!(" on {}", host.name())).unwrap_or_default();
                if self.focus == Focus::Normal {
                    r.text(ib.x + 26.0, r.center(ib, UI, 12.5), &format!("i to message {kind}{on}"), UI, 12.5, 400, t.text3, text_w);
                } else {
                    // The shortcut in mono, whose I has serifs; the UI font's is a bare stroke.
                    let hw = r.text(ib.x + 26.0, r.center(ib, UI, 12.5), &format!("Message {kind}{on}  ·  "), UI, 12.5, 400, t.text3, text_w);
                    r.text(ib.x + 26.0 + hw, r.center(ib, MONO, 11.5), &mod_label("I"), MONO, 11.5, 400, t.text3, (text_w - hw).max(0.0));
                }
            } else {
                let starts = line_starts(&message, &lines);
                for i in first..first + shown {
                    let at = caret.filter(|c| c.0 == i).map(|c| c.1);
                    let y = r.center(row(i - first), UI, 12.5);
                    self.field_text(r, t, ib.x + 26.0, y, &lines[i], UI, 12.5, text_w, (Focus::Message, starts[i]), focused, at, t.work);
                }
            }
            r.text(ib.x + ib.w - 22.0, r.center(row(shown - 1), MONO, 10.5), "↵", MONO, 10.5, 400, t.text3, 20.0);
            if ib_hit && self.actions.is_empty() {
                self.actions.push(Action::Focus(Focus::Message));
            }
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
            if rects.iter().any(|(j, b)| *j == i && self.hit(r, *b)) {
                self.actions.push(action);
            }
        }
    }

    fn draw_status(&mut self, r: &mut Renderer, t: &Theme, w: f32, h: f32) {
        let bar = Rect::new(0.0, h - STATUS_H, w, STATUS_H);
        r.rect(Rect::new(0.0, bar.y, w, 1.0), 0.0, t.border);
        let mut counts = [0; 4];
        for task in self.all_tasks() {
            counts[group(&task.state).min(3)] += 1;
        }
        let mut x = 12.0;
        let y = r.center(bar, MONO, 10.5);
        if self.vim {
            // NORMAL, or INSERT while keys go to a field or the agent.
            let insert = self.focus != Focus::Normal;
            let label = if insert { "INSERT" } else { "NORMAL" };
            let pw = r.measure(MONO, 10.5, 500, label) + 18.0;
            let pill = Rect::new(0.0, bar.y + 1.0, pw, STATUS_H - 1.0);
            r.rect(pill, 0.0, if insert { t.work } else { t.raised });
            r.text(9.0, y, label, MONO, 10.5, 500, if insert { t.on_accent } else { t.text }, pw);
            x = pw + 12.0;
        }
        let confirm = self.confirm_delete.filter(|_| self.focus == Focus::Normal).and_then(|k| self.task(k)).map(title);
        if let Some(name) = &confirm {
            // In place of the counts: the question `d` asked.
            let qw = r.text(x, y, &format!("Delete task “{name}” and its worktree?"), MONO, 10.5, 500, t.del, (w / 2.0 - x).max(80.0));
            r.text(x + qw + 10.0, y, "y / n", MONO, 10.5, 400, t.text2, 60.0);
        }
        for (i, (sym, label)) in [("■", "need you"), ("●", "running"), ("○", "queued"), ("✓", "done")].iter().enumerate().filter(|_| confirm.is_none()) {
            let c = [t.needs, t.work, t.text3, t.done][i];
            x += r.text(x, y, &format!("{sym} {} {label}", counts[i]), MONO, 10.5, 400, c, 200.0) + 14.0;
        }
        let lost = matches!(self.hosts[0].link, Link::Down(_));
        let remotes: Vec<&Host> = self.hosts.iter().filter(|h| h.cfg.is_some() && !h.removed).collect();
        let right = if lost {
            "daemon disconnected".to_string()
        } else if !remotes.is_empty() {
            let up = 1 + remotes.iter().filter(|h| matches!(h.link, Link::Up(_))).count();
            format!("{up}/{} machines connected", remotes.len() + 1)
        } else {
            format!("{} new · {} theme", mod_label("N"), mod_label("T"))
        };
        let mut right_x = w - 12.0;
        if self.vim {
            let keys = Rect::new(right_x - 46.0, bar.y + 3.0, 46.0, 16.0);
            r.frame(keys, 2.0, t.bg, t.border2);
            r.text(keys.x + 5.0, r.center(keys, MONO, 10.0), "? keys", MONO, 10.0, 400, t.text2, 46.0);
            if self.hit(r, keys) {
                self.help = !self.help;
            }
            right_x = keys.x - 12.0;
            if self.pending_g {
                r.text(right_x - 10.0, y, "g", MONO, 10.5, 400, t.text2, 20.0);
                right_x -= 22.0;
            }
        }
        let rw = r.measure(MONO, 10.5, 400, &right);
        r.text(right_x - rw, y, &right, MONO, 10.5, 400, if lost { t.del } else { t.text3 }, rw + 2.0);
    }

    /// A one-line text field; returns whether it was clicked.
    fn input(&self, r: &mut Renderer, t: &Theme, b: Rect, value: &str, placeholder: &str, focus: Focus, font: usize) -> bool {
        let focused = self.focus == focus;
        r.frame(b, 4.0, t.bg, if focused { t.work } else { t.border2 });
        let y = r.center(b, font, 12.5);
        if value.is_empty() && !focused {
            r.text(b.x + 10.0, y, placeholder, font, 12.5, 400, t.faint, b.w - 20.0);
        } else {
            self.field_text(r, t, b.x + 10.0, y, value, font, 12.5, b.w - 20.0, (focus, 0), focused, self.caret_for(focus, value), t.work);
        }
        self.hit_as(r, b, Cursor::Text)
    }

    fn draw_add_host(&mut self, r: &mut Renderer, t: &Theme, w: f32, h: f32) {
        let Some(a) = &self.add_host else { return };
        let (ssh, name, repo, test, testing) = (a.ssh.clone(), a.name.clone(), a.repo.clone(), a.test.clone(), a.testing);
        r.rect(Rect::new(0.0, 0.0, w, h), 0.0, t.scrim);
        r.target(Rect::new(0.0, 0.0, w, h), Cursor::Arrow);
        let dw = (w - 32.0).min(560.0);
        let dlg = Rect::new((w - dw) / 2.0, 100.0, dw, 300.0);
        r.frame(dlg, 8.0, t.dialog, t.border2);
        r.rect(Rect::new(dlg.x + 1.0, dlg.y + 1.0, dlg.w - 2.0, 2.0), 1.0, t.work);
        let x = dlg.x + 18.0;
        let fw = dlg.w - 36.0;
        r.text(x, dlg.y + 16.0, "ADD MACHINE", UI, 11.0, 500, t.text3, 200.0);
        let esc = Rect::new(dlg.x + dlg.w - 48.0, dlg.y + 14.0, 30.0, 16.0);
        r.frame(esc, 3.0, t.dialog, t.border2);
        r.text(esc.x + 6.0, r.center(esc, MONO, 10.0), "Esc", MONO, 10.0, 400, t.text3, 30.0);
        let mut acts = Vec::new();
        let rest = self.rest();
        if self.hit(r, esc) {
            acts.push(Action::Focus(rest)); // closes, below
        }

        let mut y = dlg.y + 44.0;
        r.text(x, y, "SSH target", UI, 11.0, 400, t.text2, 200.0);
        if self.input(r, t, Rect::new(x, y + 18.0, fw, 32.0), &ssh, "user@host", Focus::HostSsh, MONO) {
            acts.push(Action::Focus(Focus::HostSsh));
        }
        r.text(x, y + 56.0, "user@host or a Host from ~/.ssh/config. Uses your SSH keys and agent.", UI, 11.0, 400, t.text3, fw);
        y += 84.0;
        let half = (fw - 12.0) / 2.0;
        r.text(x, y, "Name", UI, 11.0, 400, t.text2, 200.0);
        r.text(x + half + 12.0, y, "Default repo", UI, 11.0, 400, t.text2, 200.0);
        let default_name = ssh.trim().rsplit('@').next().unwrap_or("").to_string();
        if self.input(r, t, Rect::new(x, y + 18.0, half, 32.0), &name, if default_name.is_empty() { "gpu-box" } else { &default_name }, Focus::HostName, UI) {
            acts.push(Action::Focus(Focus::HostName));
        }
        if self.input(r, t, Rect::new(x + half + 12.0, y + 18.0, half, 32.0), &repo, "~/src/project", Focus::HostRepo, MONO) {
            acts.push(Action::Focus(Focus::HostRepo));
        }
        y += 66.0;

        let (icon, msg, color) = match (&test, testing) {
            (_, true) => ("…", "Connecting over ssh…".to_string(), t.text3),
            (Some(Ok(m)), _) => ("✓", m.clone(), t.add),
            (Some(Err(e)), _) => ("✗", e.clone(), t.del),
            (None, _) => ("", "The machine needs arranger installed, the same release as this app.".to_string(), t.text3),
        };
        let line = Rect::new(x, y, fw, 22.0);
        let iw = if icon.is_empty() { 0.0 } else { r.text(x, r.center(line, MONO, 12.0), icon, MONO, 12.0, 600, color, 20.0) + 8.0 };
        r.text(x + iw, r.center(line, MONO, 12.0), &msg, MONO, 12.0, 400, if icon == "✓" { t.text } else { color }, fw - iw);

        let foot = Rect::new(dlg.x + 1.0, dlg.y + dlg.h - 58.0, dlg.w - 2.0, 57.0);
        r.rect(Rect::new(foot.x, foot.y, foot.w, 1.0), 0.0, t.border);
        let add_label = "Add machine ↵";
        let aw = r.measure(UI, 13.0, 500, add_label) + 32.0;
        let add = Rect::new(foot.x + foot.w - 14.0 - aw, foot.y + 12.0, aw, 34.0);
        // Disabled until there's somewhere to connect to, drawn like other disabled buttons.
        let off = ssh.trim().is_empty();
        r.rect(add, 4.0, if off { t.raised } else { t.work });
        r.text(add.x + 16.0, r.center(add, UI, 13.0), add_label, UI, 13.0, 500, if off { t.text3 } else { t.on_accent }, aw);
        let tw = r.measure(UI, 13.0, 400, "Test connection") + 28.0;
        let test_b = Rect::new(add.x - 8.0 - tw, add.y, tw, 34.0);
        r.frame(test_b, 4.0, t.panel, t.border2);
        r.text(test_b.x + 14.0, r.center(test_b, UI, 13.0), "Test connection", UI, 13.0, 400, t.text, tw);
        if self.hit(r, add) {
            acts.push(Action::AddHost);
        } else if self.hit(r, test_b) {
            acts.push(Action::TestHost);
        }
        if acts.iter().any(|a| matches!(a, Action::Focus(f) if *f == rest)) {
            self.add_host = None;
        }
        self.actions.extend(acts);
    }

    fn draw_dialog(&mut self, r: &mut Renderer, t: &Theme, w: f32, h: f32) {
        let Some(d) = &self.dialog else { return };
        let (prompt, agent_i, model_i, base, error) = (d.prompt.clone(), d.agent, d.model, d.base.clone(), d.error.clone());
        let (branches, list) = (d.branches.clone(), d.list);
        // A click on the open branch list is the list's alone, whatever lies under it.
        let list_click = self.click.filter(|p| list.is_some_and(|l| l.contains(*p)));
        if list_click.is_some() {
            self.click = None;
        }
        let (host_i, repo_path) = (d.host, d.repo.clone());
        // Machine | Repo row, once there is more than one machine to choose.
        let hosts: Vec<(usize, String, bool)> =
            self.hosts.iter().enumerate().filter(|(_, h)| !h.removed).map(|(i, h)| (i, h.name().to_string(), matches!(h.link, Link::Up(_)))).collect();
        let host_row = if hosts.len() > 1 { 78.0 } else { 0.0 };
        let agent = AGENTS[agent_i];
        let a = accent(agent);
        r.rect(Rect::new(0.0, 0.0, w, h), 0.0, t.scrim);
        r.target(Rect::new(0.0, 0.0, w, h), Cursor::Arrow); // nothing under the scrim reacts
        let dw = (w - 32.0).min(680.0);
        let dx = (w - dw) / 2.0;
        let prompt_lines = wrap(r, &prompt, UI, 18.0, dw - 36.0);
        let text_h = (prompt_lines.len().max(3) as f32) * 27.0;
        let dh = 2.0 + 34.0 + 12.0 + text_h + 16.0 + 78.0 + host_row + 58.0;
        let dlg = Rect::new(dx, 88.0, dw, dh);
        r.rect(Rect::new(dlg.x - 1.0, dlg.y - 1.0, dlg.w + 2.0, dlg.h + 2.0), 9.0, alpha(a, 0x33));
        r.frame(dlg, 8.0, t.dialog, t.border2);
        r.rect(Rect::new(dlg.x + 1.0, dlg.y + 1.0, dlg.w - 2.0, 2.0), 1.0, a);

        // Header: NEW TASK  ~/repo        [agent]  Esc
        let hy = dlg.y + 12.0;
        let head = Rect::new(dlg.x + 18.0, hy, dlg.w - 36.0, 20.0);
        let lw = r.text(head.x, r.center(head, UI, 11.0), "NEW TASK", UI, 11.0, 500, t.text3, 100.0);
        let repo_name = std::path::Path::new(repo_path.trim()).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let repo = if hosts.len() > 1 { format!("{} · {repo_name}", self.hosts[host_i].name()) } else { repo_name };
        r.text(head.x + lw + 10.0, r.center(head, MONO, 11.0), &repo, MONO, 11.0, 400, t.faint, 200.0);
        let esc = Rect::new(head.x + head.w - 30.0, head.y + 2.0, 30.0, 16.0);
        r.frame(esc, 3.0, t.dialog, t.border2);
        r.text(esc.x + 6.0, r.center(esc, MONO, 10.0), "Esc", MONO, 10.0, 400, t.text3, 30.0);
        if self.hit(r, esc) {
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
        let caret = self.caret_for(Focus::Prompt, &prompt).map(|at| caret_line(&prompt, &prompt_lines, at));
        for (i, (line, start)) in prompt_lines.iter().zip(line_starts(&prompt, &prompt_lines)).enumerate() {
            let y = ty + 4.0 + i as f32 * 27.0;
            let at = caret.filter(|c| c.0 == i).map(|c| c.1);
            self.field_text(r, t, dlg.x + 18.0, y, line, UI, 18.0, dlg.w - 36.0, (Focus::Prompt, start), prompt_focus, at, a);
        }
        let prompt_area = Rect::new(dlg.x, ty, dlg.w, text_h + 16.0);
        if self.hit_as(r, prompt_area, Cursor::Text) {
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
            if self.hit(r, b) {
                self.actions.push(Action::PickAgent(i));
            }
            ax += bw + 4.0;
        }
        let bx = dlg.x + dlg.w / 2.0 + 18.0;
        r.text(bx, gy + 14.0, "Base branch", UI, 11.0, 400, t.text3, 100.0);
        let row = Rect::new(bx, gy + 36.0, dlg.w / 2.0 - 36.0, 28.0);
        let base_focus = self.focus == Focus::Base;
        let shown = if base.is_empty() && !base_focus { "current branch" } else { &base };
        let pick = !branches.is_empty();
        let bfw = (r.measure(MONO, 12.0, 400, shown) + if pick { 38.0 } else { 20.0 }).clamp(90.0, 160.0);
        let bfield = Rect::new(bx - 8.0, row.y, bfw, row.h);
        r.frame(bfield, 4.0, t.dialog, if base_focus || list.is_some() { a } else { t.border2 });
        let fy = r.center(bfield, MONO, 12.0);
        if base_focus {
            self.field_text(r, t, bfield.x + 10.0, fy, &base, MONO, 12.0, bfw - 20.0, (Focus::Base, 0), true, self.caret_for(Focus::Base, &base), a);
        } else {
            r.text(bfield.x + 10.0, fy, shown, MONO, 12.0, 400, t.text, bfw - if pick { 38.0 } else { 20.0 });
        }
        if pick {
            r.path(9, "M18 26L32 40L46 26", Some(6.0), bfield.x + bfw - 22.0, bfield.y + 8.0, 12.0, t.text3);
            if self.hit(r, bfield) {
                let d = self.dialog.as_mut().unwrap();
                d.list = if list.is_some() { None } else { Some(Rect::default()) };
            }
        } else if self.hit_as(r, bfield, Cursor::Text) {
            self.actions.push(Action::Focus(Focus::Base));
        }
        let bw = bfield.x + bfw - row.x;
        let arrow_x = row.x + bw + 8.0;
        r.text(arrow_x, r.center(row, MONO, 12.0), "→", MONO, 12.0, 400, t.faint, 20.0);
        // ponytail: a guess; the daemon picks the id, so a skipped id or a racing `arranger new` makes it one higher
        let next = self.hosts[host_i].tasks.iter().map(|t| t.id).max().unwrap_or(0) + 1;
        let nw = r.text(arrow_x + 18.0, r.center(row, MONO, 12.0), &format!("arranger/{next}"), MONO, 12.0, 400, t.text3, 140.0);
        let chip = Rect::new(arrow_x + 26.0 + nw, row.y + 6.0, 32.0, 16.0);
        r.frame(chip, 3.0, t.dialog, t.border2);
        r.text(chip.x + 5.0, r.center(chip, UI, 10.0), "auto", UI, 10.0, 400, t.text3, 30.0);

        // Machine | Repo.
        if host_row > 0.0 {
            let my = gy + 78.0;
            r.rect(Rect::new(dlg.x + 1.0, my, dlg.w - 2.0, 1.0), 0.0, t.border);
            r.rect(Rect::new(dlg.x + dlg.w / 2.0, my, 1.0, 78.0), 0.0, t.border);
            r.text(dlg.x + 18.0, my + 14.0, "Machine", UI, 11.0, 400, t.text3, 100.0);
            let mut mx = dlg.x + 18.0;
            for (i, name, up) in &hosts {
                let on = *i == host_i;
                let bw = r.measure(UI, 12.0, 400, name) + 30.0;
                if mx + bw > dlg.x + dlg.w / 2.0 - 10.0 {
                    break; // ponytail: chips that don't fit are hidden; a dropdown if people add many machines
                }
                let b = Rect::new(mx, my + 36.0, bw, 28.0);
                r.frame(b, 4.0, t.dialog, if on { a } else { t.border2 });
                if on {
                    r.rect(b.inset(1.0), 3.0, alpha(a, 0x1F));
                }
                r.rect(Rect::new(b.x + 10.0, b.y + 11.0, 6.0, 6.0), 3.0, if *up { t.done } else { t.del });
                r.text(b.x + 22.0, r.center(b, UI, 12.0), name, UI, 12.0, 400, if on { t.text } else { t.tc }, bw);
                if self.hit(r, b) {
                    self.actions.push(Action::PickHost(*i));
                }
                mx += bw + 4.0;
            }
            let rx = dlg.x + dlg.w / 2.0 + 18.0;
            r.text(rx, my + 14.0, "Repo on that machine", UI, 11.0, 400, t.text3, 200.0);
            let field = Rect::new(rx - 8.0, my + 34.0, dlg.w / 2.0 - 20.0, 32.0);
            if self.input(r, t, field, &repo_path, "~/path/to/repo", Focus::Repo, MONO) {
                self.actions.push(Action::Focus(Focus::Repo));
            }
        }

        // Footer.
        let fy = gy + 78.0 + host_row;
        let foot = Rect::new(dlg.x + 1.0, fy, dlg.w - 2.0, dlg.y + dlg.h - fy - 1.0);
        r.rect(Rect::new(dlg.x + 1.0, fy, dlg.w - 2.0, 1.0), 0.0, t.border);
        r.rect(Rect::new(foot.x, foot.y + 1.0, foot.w, foot.h - 1.0), 7.0, t.panel);
        // Start <agent> · <model> ↵ | ⌄ — the chevron (or Shift+Tab) cycles the agent's models.
        let model = models(agent)[model_i].0;
        let start_label = if model_i == 0 { format!("Start {agent}") } else { format!("Start {agent} · {model}") };
        let sw = r.measure(UI, 13.0, 500, &start_label) + 40.0;
        let cw = 28.0;
        let chev = Rect::new(foot.x + foot.w - 14.0 - cw, foot.y + 12.0, cw, 34.0);
        let start = Rect::new(chev.x - sw, chev.y, sw, 34.0);
        let fill = if t.dark { a } else { accent_deep(agent) };
        r.rect(Rect::new(start.x, start.y, sw + cw, 34.0), 4.0, fill);
        r.rect(Rect::new(chev.x, chev.y + 7.0, 1.0, 20.0), 0.0, alpha(t.on_accent, 90));
        let slw = r.text(start.x + 14.0, r.center(start, UI, 13.0), &start_label, UI, 13.0, 500, t.on_accent, sw);
        r.text(start.x + 20.0 + slw, r.center(start, MONO, 10.0), "↵", MONO, 10.0, 400, alpha(t.on_accent, 150), 20.0);
        r.path(9, "M18 26L32 40L46 26", Some(6.0), chev.x + 8.0, chev.y + 11.0, 12.0, t.on_accent);
        if self.hit(r, chev) {
            self.actions.push(Action::PickModel(usize::MAX)); // ponytail: cycles; a menu like the branch list if the lists grow
        }
        let attach_key = mod_label("↵");
        let aw = r.measure(UI, 13.0, 400, "Start & attach") + r.measure(MONO, 10.0, 400, &attach_key) + 32.0;
        let attach = Rect::new(start.x - 8.0 - aw, start.y, aw, 34.0);
        r.frame(attach, 4.0, t.panel, t.border2);
        let alw = r.text(attach.x + 12.0, r.center(attach, UI, 13.0), "Start & attach", UI, 13.0, 400, t.text, aw);
        r.text(attach.x + 20.0 + alw, r.center(attach, MONO, 10.0), &attach_key, MONO, 10.0, 400, t.text3, 60.0);
        if !error.is_empty() {
            r.text(foot.x + 18.0, r.center(start, UI, 12.0), &error, UI, 12.0, 400, t.del, attach.x - foot.x - 30.0);
        }
        if self.hit(r, start) {
            self.actions.push(Action::Start(false));
        }
        if self.hit(r, attach) {
            self.actions.push(Action::Start(true));
        }

        // The branch list, over everything else. ponytail: the 8 most recent branches; scroll or type-to-filter if that's too few
        let Some(d) = self.dialog.as_mut() else { return };
        if d.list.is_none() {
            return;
        }
        if self.click.is_some_and(|p| !bfield.contains(p)) {
            d.list = None; // a click anywhere else closes it
            return;
        }
        let shown: Vec<&String> = branches.iter().take(8).collect();
        let lw = shown.iter().map(|b| r.measure(MONO, 12.0, 400, b)).fold(bfw, |m, x| m.max(x + 44.0)).min(dlg.w / 2.0 - 20.0);
        let lr = Rect::new(bfield.x, bfield.y + bfield.h + 4.0, lw, 8.0 + shown.len() as f32 * 28.0);
        d.list = Some(lr);
        r.rect(Rect::new(lr.x - 1.0, lr.y + 3.0, lr.w + 2.0, lr.h + 2.0), 8.0, alpha([0, 0, 0, 255], if t.dark { 0x60 } else { 0x18 }));
        r.frame(lr, 6.0, t.dialog, t.border2);
        r.target(lr, Cursor::Arrow);
        for (i, b) in shown.iter().enumerate() {
            let row = Rect::new(lr.x + 4.0, lr.y + 4.0 + i as f32 * 28.0, lr.w - 8.0, 28.0);
            let on = **b == base;
            if on {
                r.rect(row, 3.0, t.sel);
            }
            r.text(row.x + 8.0, r.center(row, MONO, 12.0), b, MONO, 12.0, 400, t.text, row.w - 36.0);
            if on {
                r.path(8, "M14 33.6L25.6 45.2L50 18.8", Some(7.0), row.x + row.w - 20.0, row.y + 8.0, 12.0, t.text);
            }
            r.target(row, Cursor::Pointer);
            if list_click.is_some_and(|p| row.contains(p)) {
                (d.base, d.list) = ((*b).clone(), None);
            }
        }
    }
}

/// Byte offset of the caret in field `focus` holding `s`; its end unless the caret was moved there.
fn caret_at(caret: Option<(Focus, usize)>, focus: Focus, s: &str) -> usize {
    let mut at = caret.filter(|c| c.0 == focus).map_or(s.len(), |c| c.1.min(s.len()));
    while !s.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// The start of the word before byte `at`.
fn word_left(s: &str, at: usize) -> usize {
    s[..at].trim_end().char_indices().rev().find(|(_, c)| c.is_whitespace()).map_or(0, |(i, c)| i + c.len_utf8())
}

/// The end of the word after byte `at`.
fn word_right(s: &str, at: usize) -> usize {
    let rest = &s[at..];
    let word = rest.trim_start();
    at + rest.len() - word.len() + word.find(char::is_whitespace).unwrap_or(word.len())
}

/// Which of `text`'s wrapped `lines` holds byte `at`, and where in it.
fn caret_line(text: &str, lines: &[String], at: usize) -> (usize, usize) {
    for (i, (line, start)) in lines.iter().zip(line_starts(text, lines)).enumerate() {
        if at <= start + line.len() {
            return (i, at.saturating_sub(start));
        }
    }
    (lines.len().saturating_sub(1), lines.last().map_or(0, |l| l.len()))
}

/// Where in `text` each of its wrapped `lines` starts. Lines are pieces of `text` in order, so
/// each is found after the last.
fn line_starts(text: &str, lines: &[String]) -> Vec<usize> {
    let mut pos = 0;
    lines
        .iter()
        .map(|line| {
            let start = text[pos..].find(line.as_str()).map_or(pos, |o| pos + o);
            pos = start + line.len();
            start
        })
        .collect()
}

/// State icon in a 12px box at (x, cy): square, ring, faint ring, check.
/// Green "✓ merged" chip for a task whose work left Review; its width (0 if none).
fn ship_badge(r: &mut Renderer, t: &Theme, shipped: &str, x: f32, cy: f32) -> f32 {
    if shipped.is_empty() {
        return 0.0;
    }
    let label = format!("✓ {shipped}");
    let w = r.measure(UI, 10.5, 500, &label) + 10.0;
    let chip = Rect::new(x, cy - 8.0, w, 16.0);
    r.rect(chip, 3.0, alpha(t.done, 0x26));
    r.text(chip.x + 5.0, r.center(chip, UI, 10.5), &label, UI, 10.5, 500, t.done, w);
    w
}

/// The status icons, all drawn from the logo's mark: eight players on two arcs around a centre,
/// 18 px wide, centred on `x + 6` and `cy`. Shape and colour carry the state so each reads still:
/// - needs you: grey players, the amber centre breathing (the logo's conductor, waiting on you)
/// - running: the players light up left to right
/// - queued: the same seats, outlined
/// - done: the players joined into two solid arcs (the logo's small form); ended: the same in grey
///
/// `sweep` is the icons' clock in seconds, None to draw them still; `since` is when the task last
/// changed state (unix seconds), from which a finished task's dots join into arcs, once.
fn state_icon(r: &mut Renderer, t: &Theme, state: &str, x: f32, cy: f32, since: u64, sweep: Option<f32>) {
    let c = state_color(t, state);
    let (x, y) = (x - 3.0, cy - 5.3);
    match group(state) {
        0 => {
            for (i, &(px, py)) in PLAYERS.iter().enumerate() {
                r.path(20 + i as u16, &dot(px, py, 4.7), None, x, y, MARK, t.faint);
            }
            let k = sweep.map_or(1.0, |s| 0.725 + 0.275 * (s / 1.4 * std::f32::consts::TAU).cos());
            r.path(39, &dot(31.8, 32.0, 8.5), None, x, y, MARK, alpha(c, (255.0 * k) as u8));
        }
        1 => {
            for (i, &(px, py)) in PLAYERS.iter().enumerate() {
                // Each dot's pulse starts 0.15 s after the one before: up over 0.48 s, down over 0.48 s, then rests.
                let lit = sweep.map_or(1.0, |s| {
                    let k = (s - i as f32 * 0.15).rem_euclid(1.6) / 1.6;
                    if k < 0.3 { ease(k / 0.3) } else if k < 0.6 { 1.0 - ease((k - 0.3) / 0.3) } else { 0.0 }
                });
                r.path(20 + i as u16, &dot(px, py, 4.7), None, x, y, MARK, alpha(c, (64.0 + 191.0 * lit) as u8));
            }
            r.path(28, &dot(31.8, 32.0, 5.2), None, x, y, MARK, c);
        }
        2 => {
            for (i, &(px, py)) in PLAYERS.iter().enumerate() {
                r.path(30 + i as u16, &dot(px, py, 3.8), Some(2.6), x, y, MARK, t.text3);
            }
            r.path(38, &dot(31.8, 32.0, 4.2), Some(2.6), x, y, MARK, t.text3);
        }
        g => {
            let c = if g == 4 { t.faint } else { c };
            // Just finished: the dots fade as the arcs come in, over 0.8 s.
            let k = match sweep.filter(|_| g == 3) {
                Some(_) => ease(((unix_secs() - since as f64) as f32 - 0.2).clamp(0.0, 0.8) / 0.8),
                None => 1.0,
            };
            if k < 1.0 {
                for (i, &(px, py)) in PLAYERS.iter().enumerate() {
                    r.path(20 + i as u16, &dot(px, py, 4.7), None, x, y, MARK, alpha(c, (255.0 * (1.0 - k)) as u8));
                }
            }
            r.path(29, "M5.45 32A26.35 26.35 0 0 1 58.15 32M17.7 32A14.1 14.1 0 0 1 45.9 32", Some(5.6), x, y, MARK, alpha(c, (255.0 * k) as u8));
            r.path(28, &dot(31.8, 32.0, 5.2), None, x, y, MARK, c);
        }
    }
}

/// The status icons' width, and the box their 64-unit paths are laid out in.
const MARK: f32 = 18.0;

/// The logo's players in a status icon's 64-unit box: two arcs around the centre at (31.8, 32).
const PLAYERS: [(f32, f32); 8] = [(5.6, 32.0), (13.4, 13.4), (17.9, 32.0), (31.8, 5.6), (31.8, 17.9), (46.1, 32.0), (50.6, 13.4), (58.4, 32.0)];

/// A circle as SVG path data.
fn dot(cx: f32, cy: f32, rad: f32) -> String {
    format!("M{} {cy}a{rad} {rad} 0 1 0 {} 0a{rad} {rad} 0 1 0 -{} 0", cx - rad, rad * 2.0, rad * 2.0)
}

/// Smoothstep: 0..1 eased in and out.
fn ease(v: f32) -> f32 {
    v * v * (3.0 - 2.0 * v)
}

fn unix_secs() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

/// The last terminal size recorded in a task log (the daemon writes `ESC [8;rows;cols t` on resize).
fn last_size(log: &[u8]) -> Option<(u16, u16)> {
    let at = log.windows(4).rposition(|w| w == b"\x1b[8;")?;
    let rest = &log[at + 4..];
    let end = rest.iter().take(12).position(|&b| b == b't')?;
    let (rows, cols) = std::str::from_utf8(&rest[..end]).ok()?.split_once(';')?;
    Some((cols.parse().ok()?, rows.parse().ok()?))
}

/// The agent's own input box at the bottom of its screen, which ours replaces. Returns the box's
/// first row and the mode its footer shows ("" if none). None when the screen is scrolled back or
/// shows something else there (a menu, a dialog), so those stay on screen.
fn agent_box(screen: &Screen, kind: &str) -> Option<(usize, String)> {
    let grid = screen.grid();
    if grid.display_offset() != 0 {
        return None;
    }
    let rows: Vec<String> = (0..grid.screen_lines())
        .map(|row| (0..grid.columns()).map(|c| grid[Line(row as i32)][Column(c)].c).collect::<String>().trim_end().to_string())
        .collect();
    if kind == "codex" {
        // `› draft`, a blank row, then `model effort · cwd … Plan mode` and a hints row. Menus use `› 1. …`.
        let top = rows.iter().rposition(|r| r.starts_with("› "))?;
        let numbered = rows[top][4..].split_once('.').is_some_and(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        let below: Vec<&String> = rows[top + 1..].iter().filter(|r| !r.is_empty()).collect();
        if numbered || below.len() > 8 || !below.iter().any(|r| r.contains(" · ")) {
            return None;
        }
        let plan = below.iter().any(|r| r.ends_with("Plan mode"));
        return Some((top, if plan { "plan mode".into() } else { String::new() }));
    }
    let rule = |s: &String| s.trim().chars().count() >= 20 && s.trim().chars().all(|c| c == '─');
    let lower = rows.iter().rposition(rule)?;
    let upper = rows[..lower].iter().rposition(rule)?;
    if lower - upper > 12 || rows[lower + 1..].iter().filter(|r| !r.is_empty()).count() > 4 {
        return None;
    }
    if kind == "pi" {
        // Input between two rules; the footer's last row ends with the model and thinking level.
        let last = rows[lower + 1..].iter().rfind(|r| !r.is_empty());
        let mode = last.and_then(|r| r.trim().rsplit("  ").next()).filter(|m| *m != "unknown").unwrap_or("");
        return Some((upper, mode.trim().to_string()));
    }
    // Claude Code: `❯ draft` between two rules; footer "-- INSERT --  ⏵⏵ auto mode on (shift+tab to cycle) · …"
    if !rows[upper + 1].trim_start().starts_with(['❯', '>']) {
        return None;
    }
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

/// Draws the screen with its first row at `area.y`, and scrollback above that up to `top`.
fn draw_term(r: &mut Renderer, t: &Theme, screen: &Screen, area: Rect, top: f32, cell_w: f32, focused: bool) {
    let grid = screen.grid();
    let mode = screen.mode();
    let (asc, desc) = r.vmetrics(MONO, TERM_SIZE);
    let base = (LINE_H - (asc + desc)) / 2.0 + asc;
    let offset = grid.display_offset() as i32;
    let above = (((area.y - top) / LINE_H).ceil().max(0.0) as i32).min(grid.history_size() as i32 - offset);
    for row in -above..grid.screen_lines() as i32 {
        let line = &grid[Line(row - offset)];
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
            fg = readable(fg, bg.unwrap_or(t.term));
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
    fn caret_moves_and_maps_to_wrapped_lines() {
        assert_eq!(word_left("fix the bug", 11), 8);
        assert_eq!(word_right("fix the bug", 3), 7);
        assert_eq!(caret_at(Some((Focus::Message, 99)), Focus::Message, "héllo"), 6);
        assert_eq!(caret_at(Some((Focus::Message, 2)), Focus::Message, "héllo"), 1); // inside é
        assert_eq!(caret_at(Some((Focus::Filter, 1)), Focus::Message, "abc"), 3); // another field's caret
        let lines = vec!["fix the".to_string(), "bug".to_string()];
        assert_eq!(caret_line("fix the bug", &lines, 5), (0, 5));
        assert_eq!(caret_line("fix the bug", &lines, 9), (1, 1));
        assert_eq!(caret_line("fix the bug", &lines, 11), (1, 3));
    }

    #[test]
    fn wrapped_lines_know_where_they_start() {
        let text = "fix the bug\nand the test";
        let lines = vec!["fix the".to_string(), "bug".to_string(), "and the".to_string(), "test".to_string()];
        assert_eq!(line_starts(text, &lines), vec![0, 8, 12, 20]);
    }

    #[test]
    fn up_and_down_recall_earlier_messages() {
        let list = vec!["one".to_string(), "two".to_string()];
        let (mut hist, mut text) = (None, String::new());
        assert!(recall(&list, &mut hist, &mut text, true));
        assert_eq!(text, "two");
        assert!(recall(&list, &mut hist, &mut text, true));
        assert!(recall(&list, &mut hist, &mut text, true)); // stays on the oldest
        assert_eq!(text, "one");
        assert!(recall(&list, &mut hist, &mut text, false));
        assert!(recall(&list, &mut hist, &mut text, false)); // past the newest: empty again
        assert_eq!((text.as_str(), hist), ("", None));
        text.push_str("typed");
        assert!(!recall(&list, &mut hist, &mut text, true)); // ↑ in a typed message moves the caret instead
    }

    #[test]
    fn markdown_runs() {
        assert_eq!(review::plain("## Fixed **the** `bug` in [docs](http://x) _now_"), "Fixed the bug in docs now");
        assert_eq!(review::plain("- snake_case stays"), "snake_case stays");
    }

    #[test]
    fn finds_claude_input_box_and_mode() {
        let rule = "─".repeat(40);
        let mut s = Screen::new(60, 8);
        s.feed(format!("chat\r\n\r\n{rule}\r\n❯ draft\r\n{rule}\r\n-- INSERT --  ⏵⏵ auto mode on (shift+tab to cycle) · ← for agents").as_bytes());
        assert_eq!(agent_box(&s, "claude"), Some((2, "auto mode on".to_string())));

        let mut menu = Screen::new(60, 8);
        menu.feed(format!("{rule}\r\nAccessing workspace:\r\n❯ 1. Yes\r\n{rule}\r\n").as_bytes());
        assert_eq!(agent_box(&menu, "claude"), None);

        let mut codex = Screen::new(80, 8);
        codex.feed("  >_ OpenAI Codex\r\n\r\n› fix it\r\n\r\n  GPT medium · ~/repo     Plan mode\r\n  ? for shortcuts".as_bytes());
        assert_eq!(agent_box(&codex, "codex"), Some((2, "plan mode".to_string())));
        let mut trust = Screen::new(80, 8);
        trust.feed("  Trust this folder?\r\n\r\n› 1. Trust and continue\r\n  2. Quit\r\n\r\n  enter continue · esc quit".as_bytes());
        assert_eq!(agent_box(&trust, "codex"), None);

        let mut pi = Screen::new(60, 8);
        pi.feed(format!("chat\r\n{rule}\r\ndraft\r\n{rule}\r\n~/repo\r\n0.0%/0 (auto)        sonnet • high").as_bytes());
        assert_eq!(agent_box(&pi, "pi"), Some((1, "sonnet • high".to_string())));
        assert_eq!(last_size(b"x\x1b[8;24;80tyy\x1b[8;62;236tzz"), Some((236, 62)));
    }

    #[test]
    fn agent_white_text_stays_readable_on_paper() {
        let white = [255; 4];
        assert_eq!(readable(white, GRAPHITE.term), white);
        assert_ne!(readable(white, PAPER.term), white);
        let grey = [153, 153, 153, 255];
        assert!(readable(grey, PAPER.term)[0] > readable(white, PAPER.term)[0], "white stays darker than grey");
        assert_eq!(readable(grey, GRAPHITE.term), grey);
    }
}

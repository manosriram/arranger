use arranger::client::{connect, request};
use arranger::proto::{self, Reply, Request, Ship};
use arranger::{adapter, daemon, machine};
use rustix::termios::{self, OptionalActions};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::{env, thread};

const USAGE: &str = "usage: arranger            open the workspace window
       arranger [ls [--files] | new <agent> [--branch <base>] [prompt] | attach <id> | kill <id>
                 | approve <id> | deny <id> [reason] | events <id> | diff <id> | revert <id> <path> <hunk#>
                 | send <id> <text> | revive <id> [prompt] | commit <id> [--merge|--pr] [message] | ping | stop | serve | hook <adapter>
                 | machine [ls | add <name> <user@host> [repo] | rm <name>] | endpoint]";
/// Ctrl-\, raw and in kitty keyboard encoding.
const DETACH: &[&[u8]] = &[b"\x1c", b"\x1b[92;5u"];

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args[..] {
        ["serve"] => daemon::run(),
        [] => gui(),
        ["ls"] => ls(false),
        ["ls", "--files"] => ls(true),
        ["hook", adapter] => {
            let _ = hook(adapter); // never fail the agent's hook; it carries on without us
            Ok(())
        }
        ["approve", id] => parse_id(id).and_then(|id| answer(id, true, String::new())),
        ["deny", id, ref reason @ ..] => parse_id(id).and_then(|id| answer(id, false, reason.join(" "))),
        ["events", id] => parse_id(id).and_then(events),
        ["ping"] => request(&Request::Ping).map(|_| println!("pong")),
        ["stop"] => arranger::client::stop(),
        // Run over ssh by a GUI adding this machine: start the daemon, say where it listens.
        ["endpoint"] => connect().map(|_| println!("{} {}", proto::VERSION, proto::socket_path().display())),
        ["machine"] | ["machine", "ls"] => {
            for m in machine::load() {
                println!("{:<14} {:<28} {}", m.name, m.ssh, m.repo);
            }
            Ok(())
        }
        ["machine", "add", name, ssh, ref repo @ ..] => add_machine(name, ssh, &repo.join(" ")),
        ["machine", "rm", name] => {
            let mut list = machine::load();
            list.retain(|m| m.name != name);
            machine::save(&list)
        }
        ["diff", id] => parse_id(id).and_then(diff),
        ["revert", id, path, hunk] => parse_id(id).and_then(|id| revert(id, path, hunk)),
        ["send", id, ref text @ ..] => {
            parse_id(id).and_then(|id| request(&Request::Prompt { id, text: text.join(" ") }).map(drop))
        }
        ["revive", id, ref text @ ..] => {
            parse_id(id).and_then(|id| request(&Request::Revive { id, prompt: text.join(" ") }).map(drop))
        }
        ["commit", id, "--merge", ref msg @ ..] => parse_id(id).and_then(|id| commit(id, msg, Ship::Merge)),
        ["commit", id, "--pr", ref msg @ ..] => parse_id(id).and_then(|id| commit(id, msg, Ship::Pr)),
        ["commit", id, ref msg @ ..] => parse_id(id).and_then(|id| commit(id, msg, Ship::Commit)),
        ["new", agent, "--branch", base, ref prompt @ ..] => new(agent, Some(base), prompt),
        ["new", agent, ref prompt @ ..] => new(agent, None, prompt),
        ["attach", id] => parse_id(id).and_then(attach),
        ["kill", id] => parse_id(id).and_then(|id| request(&Request::Kill { id }).map(drop)),
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("arranger: {e}");
        std::process::exit(1);
    }
}

fn add_machine(name: &str, ssh: &str, repo: &str) -> io::Result<()> {
    let sock = machine::endpoint(ssh)?;
    println!("{name}: daemon at {}", sock.display());
    let mut list = machine::load();
    list.retain(|m| m.name != name);
    list.push(machine::Machine { name: name.into(), ssh: ssh.into(), repo: repo.into() });
    machine::save(&list)
}

/// Replaces this process with `arranger-gui`, found next to this binary or on PATH.
fn gui() -> io::Result<()> {
    let exe = env::current_exe()?;
    let sibling = exe.with_file_name("arranger-gui");
    let program = if sibling.exists() { sibling } else { "arranger-gui".into() };
    // Inside arranger.app macOS already has its name and icon; a bare binary needs the link.
    // Run it by its real path (the command may be a symlink into the app), so macOS sees the bundle.
    let bundle = fs::canonicalize(&program).ok().filter(|p| p.to_string_lossy().contains(".app/Contents/MacOS/"));
    let program = match bundle {
        Some(real) => real,
        None if cfg!(target_os = "macos") => named_link(&program).unwrap_or(program),
        None => program,
    };
    Err(Command::new(program).env("ARRANGER_CLI", exe).exec())
}

/// macOS names an app without a bundle after its executable file, in the menu bar and the Dock.
/// So the GUI runs through `<data>/app/arranger`, a hard link to `arranger-gui`, re-made when that
/// changes. None when it can't be made (another volume, say); the GUI then runs as is.
fn named_link(gui: &std::path::Path) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let gui = fs::canonicalize(gui).ok()?;
    let link = proto::data_dir().join("app/arranger");
    let (a, b) = (fs::metadata(&gui).ok()?, fs::metadata(&link));
    if !b.is_ok_and(|b| (b.dev(), b.ino()) == (a.dev(), a.ino())) {
        fs::create_dir_all(link.parent()?).ok()?;
        let _ = fs::remove_file(&link);
        fs::hard_link(&gui, &link).ok()?;
    }
    Some(link)
}

fn parse_id(s: &str) -> io::Result<u64> {
    s.parse().map_err(|_| io::Error::other(format!("bad task id: {s}")))
}

fn unexpected(reply: Reply) -> io::Error {
    io::Error::other(format!("unexpected reply: {reply:?}"))
}

fn ls(files: bool) -> io::Result<()> {
    let reply = request(&Request::List { files })?;
    let Reply::Tasks(tasks) = reply else { return Err(unexpected(reply)) };
    println!("{:<4} {:<10} {:<10} {:<14} ACTION", "ID", "STATE", "AGENT", "BRANCH");
    for t in tasks {
        let action: String = t.action.replace('\n', " ").chars().take(60).collect();
        println!("{:<4} {:<10} {:<10} {:<14} {action}", t.id, t.state, adapter::kind(&t.agent), t.branch);
        for f in t.files {
            println!("       {f}");
        }
    }
    Ok(())
}

fn diff(id: u64) -> io::Result<()> {
    let reply = request(&Request::Review { id })?;
    let Reply::Review(r) = reply else { return Err(unexpected(reply)) };
    println!("{} -> {}", r.branch, r.base);
    for f in r.files {
        println!("\n{} {}  +{} -{}{}", f.status, f.path, f.added, f.removed, if f.binary { "  (binary)" } else { "" });
        for (i, h) in f.hunks.iter().enumerate() {
            println!("[{}] {}", i + 1, h.header);
            for l in &h.lines {
                print!("{}{}", l.kind, l.text);
                if !l.text.ends_with('\n') {
                    println!("\n\\ No newline at end of file");
                }
            }
        }
    }
    Ok(())
}

/// `hunk` is the 1-based number `diff` prints.
fn revert(id: u64, path: &str, hunk: &str) -> io::Result<()> {
    let reply = request(&Request::Review { id })?;
    let Reply::Review(r) = reply else { return Err(unexpected(reply)) };
    let n: usize = hunk.parse().map_err(|_| io::Error::other("hunk is the number diff prints"))?;
    let header = r
        .files
        .iter()
        .find(|f| f.path == path)
        .and_then(|f| f.hunks.get(n.wrapping_sub(1)))
        .map(|h| h.header.clone())
        .ok_or_else(|| io::Error::other(format!("no hunk {n} in {path}")))?;
    request(&Request::Revert { id, path: path.into(), header }).map(drop)
}

fn commit(id: u64, message: &[&str], ship: Ship) -> io::Result<()> {
    match request(&Request::Commit { id, message: message.join(" "), ship })? {
        Reply::JobDone { ok, output } => {
            println!("{output}");
            if ok { Ok(()) } else { Err(io::Error::other("commit failed")) }
        }
        reply => Err(unexpected(reply)),
    }
}

fn answer(id: u64, allow: bool, message: String) -> io::Result<()> {
    request(&Request::Answer { id, allow, message }).map(drop)
}

fn events(id: u64) -> io::Result<()> {
    let reply = request(&Request::Events { id })?;
    let Reply::Events(events) = reply else { return Err(unexpected(reply)) };
    let start = events.first().map_or(0, |e| e.0);
    for (ts, event) in events {
        let secs = (ts - start) / 1000;
        println!("+{:02}:{:02}  {}", secs / 60, secs % 60, event.describe().replace('\n', " "));
    }
    Ok(())
}

/// `arranger hook <adapter>`: run by agents. Reads hook JSON (one document, or a stream for pi),
/// forwards events, and for permission asks waits for the human and prints the decision.
fn hook(adapter: &str) -> io::Result<()> {
    let Some(task) = env::var("ARRANGER_TASK").ok().and_then(|t| t.parse().ok()) else { return Ok(()) };
    let mut sock = UnixStream::connect(proto::socket_path())?; // no auto-start: no daemon, no tracking
    let mut totals = adapter::Totals::default();
    for value in serde_json::Deserializer::from_reader(io::stdin().lock()).into_iter() {
        let value: serde_json::Value = value?;
        // rollout-<time>-<uuid>.jsonl names the session too
        let rollout = value["transcript_path"].as_str().and_then(|p| p.strip_suffix(".jsonl")).and_then(|p| p.get(p.len().saturating_sub(36)..));
        if let Some(session) = value["session_id"].as_str().or(rollout).filter(|_| adapter == "codex") {
            let _ = fs::write(adapter::codex_session(&proto::data_dir(), task), session);
        }
        let (events, ask) = adapter::parse(adapter, &value, &mut totals);
        for event in events {
            if ask && matches!(event, proto::Event::PermissionRequest { .. }) {
                sock.write_all(&proto::frame(&Request::Ask { task, event }))?;
                if let Reply::Decision { allow: Some(allow), message } = proto::read_msg(&mut sock)? {
                    println!("{}", adapter::decision(allow, &message));
                }
            } else {
                sock.write_all(&proto::frame(&Request::Event { task, event }))?;
            }
        }
    }
    Ok(())
}

fn new(agent: &str, base: Option<&str>, prompt: &[&str]) -> io::Result<()> {
    let prompt = Some(prompt.join(" ")).filter(|p| !p.is_empty());
    let req = Request::New { agent: agent.into(), repo: env::current_dir()?, base: base.map(Into::into), prompt, model: None };
    match request(&req)? {
        Reply::Created { id, state } => Ok(println!("{id} {state}")),
        reply => Err(unexpected(reply)),
    }
}

fn tty_size(tty: &File) -> io::Result<(u16, u16)> {
    let ws = termios::tcgetwinsize(tty)?;
    Ok((ws.ws_col, ws.ws_row))
}

fn attach(id: u64) -> io::Result<()> {
    let tty = File::options().read(true).write(true).open("/dev/tty")?;
    let (cols, rows) = tty_size(&tty)?;
    let mut sock = connect()?;
    sock.write_all(&proto::frame(&Request::Attach { id, cols, rows, native: true }))?;
    let snapshot = match proto::read_msg(&mut sock)? {
        Reply::Output(bytes) => bytes,
        Reply::Error(e) => return Err(io::Error::other(e)),
        reply => return Err(unexpected(reply)),
    };

    let saved = termios::tcgetattr(&tty)?;
    let mut raw = saved.clone();
    raw.make_raw();
    termios::tcsetattr(&tty, OptionalActions::Now, &raw)?;
    let result = stream(&tty, &mut sock, &snapshot);
    termios::tcsetattr(&tty, OptionalActions::Now, &saved)?;
    // Undo modes the agent may have turned on in this terminal.
    let mut out = io::stdout();
    out.write_all(b"\x1b[0m\x1b[?1049l\x1b[?25h\x1b[?1l\x1b>\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1006l\x1b[?2004l\x1b[<99u\r\n")?;
    match result? {
        true => println!("[exited]"),
        false => println!("[detached from {id}]"),
    }
    Ok(())
}

fn send(writer: &Mutex<UnixStream>, req: &Request) -> io::Result<()> {
    writer.lock().unwrap().write_all(&proto::frame(req))
}

/// Pumps tty <-> daemon until detach (false) or the task exits (true).
/// Blocking threads, not poll(): macOS poll() doesn't support tty devices.
fn stream(tty: &File, sock: &mut UnixStream, snapshot: &[u8]) -> io::Result<bool> {
    let writer = Arc::new(Mutex::new(sock.try_clone()?));
    let detached = Arc::new(AtomicBool::new(false));

    let (mut keys, w, done, sock2) = (tty.try_clone()?, writer.clone(), detached.clone(), sock.try_clone()?);
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            let n = keys.read(&mut buf).unwrap_or(0);
            let input = &buf[..n];
            let cut = DETACH.iter().filter_map(|k| input.windows(k.len()).position(|w| w == *k)).min();
            let send_now = &input[..cut.unwrap_or(n)];
            if !send_now.is_empty() && send(&w, &Request::Input(send_now.to_vec())).is_err() {
                return;
            }
            if cut.is_some() || n == 0 {
                done.store(true, Ordering::SeqCst);
                let _ = sock2.shutdown(Shutdown::Both);
                return;
            }
        }
    });

    let (winch, winch_w) = UnixStream::pair()?;
    signal_hook::low_level::pipe::register(signal_hook::consts::SIGWINCH, winch_w)?;
    let size_tty = tty.try_clone()?;
    thread::spawn(move || {
        let mut buf = [0u8; 64];
        while matches!((&winch).read(&mut buf), Ok(n) if n > 0) {
            let Ok((cols, rows)) = tty_size(&size_tty) else { return };
            if send(&writer, &Request::Resize { cols, rows }).is_err() {
                return;
            }
        }
    });

    let mut out = io::stdout().lock();
    out.write_all(snapshot)?;
    out.flush()?;
    let mut inbuf = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = match sock.read(&mut buf) {
            Ok(n) if n > 0 => n,
            _ if detached.load(Ordering::SeqCst) => return Ok(false),
            Ok(_) => return Err(io::Error::other("daemon went away")),
            Err(e) => return Err(e),
        };
        inbuf.extend_from_slice(&buf[..n]);
        while let Some(msg) = proto::unframe(&mut inbuf) {
            match msg? {
                Reply::Output(bytes) => out.write_all(&bytes)?,
                Reply::Exited => return Ok(true),
                reply => return Err(unexpected(reply)),
            }
        }
        out.flush()?;
    }
}

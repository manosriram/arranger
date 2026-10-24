//! Welcome: where a window opened outside a project starts (from Finder or the Dock, say). Pick a
//! folder, make a new one, or reopen a recent one; any folder becomes a git project on the way in.

use super::*;
use std::path::Path;

impl App {
    pub fn draw_welcome(&mut self, r: &mut Renderer, t: &Theme, area: Rect) {
        let mut acts = Vec::new();
        let side_w = 400.0f32.min(area.w * 0.4);

        // Left: what this is, and the ways in.
        let left = Rect::new(area.x, area.y, area.w - side_w, area.h);
        let x = left.x + (left.w * 0.12).max(32.0);
        let text_w = (left.w - (x - left.x) - 48.0).min(470.0);
        let mut y = left.y + left.h / 2.0 - 190.0;
        logo(r, x, y, 56.0, t.text, t.needs);
        y += 80.0;
        r.text(x, y, "Pick a project to work on", UI, 30.0, 600, t.text, text_w + 100.0);
        y += 50.0;
        let about = "arranger runs AI coding helpers side by side. Each works on its own copy of your project, and nothing changes in your files until you approve it.";
        for line in wrap(r, about, UI, 15.0, text_w) {
            r.text(x, y, &line, UI, 15.0, 400, t.text2, text_w);
            y += 23.0;
        }
        y += 22.0;
        let open_label = "Open a folder…";
        let ow = r.measure(UI, 14.0, 600, open_label) + 36.0;
        let open = Rect::new(x, y, ow, 44.0);
        r.rect(open, 0.0, t.primary_bg);
        r.text(open.x + 18.0, r.center(open, UI, 14.0), open_label, UI, 14.0, 600, t.primary_fg, ow);
        if self.hit(r, open) {
            acts.push(Action::OpenProject(None));
        }
        let new_label = "Start a new project";
        let nw = r.measure(UI, 14.0, 400, new_label) + 36.0;
        let new = Rect::new(open.x + ow + 10.0, y, nw, 44.0);
        r.frame(new, 0.0, t.bg, t.border2);
        r.text(new.x + 18.0, r.center(new, UI, 14.0), new_label, UI, 14.0, 400, t.text, nw);
        if self.hit(r, new) {
            acts.push(Action::NewProject);
        }
        y += 64.0;
        let drop = Rect::new(x, y, text_w, 96.0);
        r.frame(drop, 0.0, t.bg, t.border2);
        let hint = "or drop a folder here";
        let hw = r.measure(UI, 13.0, 400, hint);
        r.text(drop.x + (drop.w - hw) / 2.0, r.center(drop, UI, 13.0), hint, UI, 13.0, 400, t.text3, drop.w);
        if !self.welcome_error.is_empty() {
            r.text(x, drop.y + drop.h + 16.0, &self.welcome_error, UI, 13.0, 400, t.del, text_w);
        }

        // Right: recent projects, one compact row each.
        let side = Rect::new(area.x + area.w - side_w, area.y, side_w, area.h);
        r.rect(side, 0.0, t.panel);
        r.rect(Rect::new(side.x, side.y, 1.0, side.h), 0.0, t.border);
        let (sx, sw) = (side.x + 28.0, side.w - 56.0);
        let mut y = side.y + 40.0;
        r.text(sx, y, "RECENT", UI, 11.0, 500, t.text3, sw);
        y += 26.0;
        let home = std::env::var("HOME").unwrap_or_default();
        let recent: Vec<String> = self.recent.iter().filter(|p| Path::new(p).is_dir()).cloned().collect();
        if recent.is_empty() {
            r.text(sx, y + 4.0, "Projects you open show up here.", UI, 13.0, 400, t.text3, sw);
        }
        for path in recent.iter().take(10) {
            let row = Rect::new(sx, y, sw, 36.0);
            r.rect(Rect::new(row.x, row.y + row.h - 1.0, row.w, 1.0), 0.0, t.border);
            let name = Path::new(path).file_name().map_or(path.as_str(), |n| n.to_str().unwrap_or(path));
            let nw = r.text(row.x + 2.0, r.center(row, UI, 13.0), name, UI, 13.0, 500, t.text, sw * 0.5);
            let shown = path.strip_prefix(&home).map_or(path.clone(), |rest| format!("~{rest}"));
            r.text(row.x + nw + 12.0, r.center(row, MONO, 11.0), &shown, MONO, 11.0, 400, t.text3, sw - nw - 14.0);
            if self.hit(r, row) {
                acts.push(Action::OpenProject(Some(path.into())));
            }
            y += 36.0;
        }
        self.actions.extend(acts);
    }

    /// Makes `path` (or a folder picked now, if None) the current project. Folders that aren't git
    /// projects yet become one; a fresh project opens the new-task dialog.
    pub fn open_project(&mut self, path: Option<PathBuf>, new: bool) {
        let path = match path.map_or_else(|| pick_folder(new), |p| Ok(Some(p))) {
            Ok(Some(p)) => p,
            Ok(None) => return, // cancelled
            Err(e) => return self.welcome_error = e,
        };
        match prepare(&path) {
            Ok((top, fresh)) => {
                self.repo = top.clone();
                self.welcome = false;
                self.welcome_error.clear();
                self.remember(&top);
                if fresh {
                    self.open_dialog();
                }
            }
            Err(e) => self.welcome_error = e,
        }
    }

    /// Puts a project first in the recent list (kept in gui.json).
    pub fn remember(&mut self, top: &Path) {
        let p = top.display().to_string();
        self.recent.retain(|r| *r != p);
        self.recent.insert(0, p);
        self.recent.truncate(10);
        self.save_settings();
    }
}

/// The git project `path` is in: its top folder.
pub fn project_root(path: &Path) -> Option<PathBuf> {
    gix::discover(path).ok()?.workdir().map(Path::to_path_buf)
}

/// Asks for a folder with the system's own dialog: an existing one, or (`new`) a name for a new
/// one. None if cancelled.
fn pick_folder(new: bool) -> Result<Option<PathBuf>, String> {
    let out = if cfg!(target_os = "macos") {
        let script = if new {
            r#"POSIX path of (choose file name with prompt "Name your new project" default name "my-project")"#
        } else {
            r#"POSIX path of (choose folder with prompt "Pick a project folder")"#
        };
        Command::new("osascript").args(["-e", script]).output()
    } else {
        let mut z = Command::new("zenity");
        z.args(["--file-selection", "--directory"]);
        if new {
            z.args(["--save", "--title=Name your new project"]);
        }
        z.output()
    };
    let out = out.map_err(|_| "No folder picker here (install zenity), or start arranger from a terminal inside your project".to_string())?;
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok((out.status.success() && !path.is_empty()).then(|| PathBuf::from(path)))
}

/// The project at `path`, making the folder and a git repo with a first commit when needed (agents
/// work on copies branched from it). Returns its top folder, and whether it was just set up.
fn prepare(path: &Path) -> Result<(PathBuf, bool), String> {
    if let Some(top) = project_root(path) {
        return Ok((top, false));
    }
    std::fs::create_dir_all(path).map_err(|e| format!("Couldn't make {}: {e}", path.display()))?;
    let git = |args: &[&str]| -> Result<(), String> {
        let out = Command::new("git").arg("-C").arg(path).args(args).output().map_err(|_| "arranger needs git; install it first".to_string())?;
        if out.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&out.stderr).trim().to_string()) }
    };
    git(&["init", "-q"])?;
    git(&["add", "-A"])?;
    // Someone who has never used git has no name set; commit as arranger rather than fail.
    let named = Command::new("git").arg("-C").arg(path).args(["config", "user.email"]).output().is_ok_and(|o| o.status.success());
    let who: &[&str] = if named { &[] } else { &["-c", "user.name=arranger", "-c", "user.email=arranger@localhost"] };
    git(&[who, &["commit", "-q", "--allow-empty", "-m", "Starting point"]].concat())?;
    Ok((path.canonicalize().unwrap_or(path.into()), true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_folder_becomes_a_project() {
        let dir = std::env::temp_dir().join(format!("arranger-welcome-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("notes.txt"), "hi").unwrap();
        let (top, fresh) = prepare(&dir).unwrap();
        assert!(fresh && project_root(&dir).is_some());
        let log = Command::new("git").arg("-C").arg(&top).args(["log", "--format=%s", "--name-only"]).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&log.stdout).split_whitespace().collect::<Vec<_>>(), ["Starting", "point", "notes.txt"]);
        assert!(!prepare(&dir).unwrap().1); // already one
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

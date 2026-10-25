//! The Terminal tab's shell: the user's own shell, started in a task's worktree and kept there.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::Path;
use std::process::Command;

/// zsh reads these from `$ZDOTDIR`; each passes on to the user's own, and .zshrc then adds the
/// cd hook and hands `$ZDOTDIR` back to `$HOME`, so .zlogin and nested shells are the user's.
const ZSHENV: &str = "[ -f \"$HOME/.zshenv\" ] && . \"$HOME/.zshenv\"\n";
const ZPROFILE: &str = "[ -f \"$HOME/.zprofile\" ] && . \"$HOME/.zprofile\"\n";
const ZSHRC: &str = r#"HISTFILE="$HOME/.zsh_history"
ZDOTDIR="$HOME"
[ -f "$HOME/.zshrc" ] && . "$HOME/.zshrc"
_arranger_jail() {
  case "$PWD/" in
    "$ARRANGER_ROOT"/*) ;;
    *)
      print -u2 "arranger: $PWD is outside this worktree"
      case "$OLDPWD/" in
        "$ARRANGER_ROOT"/*) builtin cd -q -- "$OLDPWD" ;;
        *) builtin cd -q -- "$ARRANGER_ROOT" ;;
      esac ;;
  esac
}
autoload -Uz add-zsh-hook && add-zsh-hook chpwd _arranger_jail
"#;
const BASHRC: &str = r#"[ -f "$HOME/.bashrc" ] && . "$HOME/.bashrc"
_arranger_jail() {
  case "$PWD/" in
    "$ARRANGER_ROOT"/*) ;;
    *) echo "arranger: $PWD is outside this worktree" >&2; builtin cd -- "$ARRANGER_ROOT" ;;
  esac
}
# Like zsh's chpwd: step back at once, not only at the next prompt, so the rest of the line runs inside.
cd() { builtin cd "$@"; local s=$?; _arranger_jail; return $s; }
PROMPT_COMMAND="_arranger_jail${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
"#;

/// PATH as the user's terminal has it. Apps opened from Finder, Spotlight or the Dock get launchd's
/// bare /usr/bin:/bin:/usr/sbin:/sbin, where agents installed in ~/.local/bin, Homebrew and the
/// like aren't found. Asks an interactive login shell (rc files are where installers add to PATH),
/// then keeps the current entries and the usual install dirs after it.
pub fn login_path() -> OsString {
    const MARK: &str = "__ARRANGER_PATH__";
    let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/zsh".into());
    // printenv, not $PATH: fish's "$PATH" joins with spaces. The mark skips whatever rc files print.
    let from_shell = Command::new(&shell)
        .args(["-ilc", &format!("echo {MARK}; /usr/bin/printenv PATH")])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()
        .and_then(|mut child| {
            // An rc file that waits on something mustn't hold the app up.
            for _ in 0..150 {
                match child.try_wait() {
                    Ok(Some(_)) => {
                        let mut out = String::new();
                        io::Read::read_to_string(&mut child.stdout.take()?, &mut out).ok()?;
                        return out.split_once(MARK)?.1.lines().map(str::trim).find(|l| !l.is_empty()).map(String::from);
                    }
                    Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
                    Err(_) => break,
                }
            }
            let _ = child.kill();
            let _ = child.wait();
            None
        })
        .unwrap_or_default();
    let home = std::env::var("HOME").unwrap_or_default();
    let usual = [".local/bin", ".claude/local", ".cargo/bin", ".bun/bin"].map(|d| format!("{home}/{d}"));
    let current = std::env::var("PATH").unwrap_or_default();
    let mut dirs: Vec<&str> = Vec::new();
    let usual = usual.iter().map(String::as_str).chain(["/opt/homebrew/bin", "/usr/local/bin"]).filter(|d| Path::new(d).is_dir());
    for dir in from_shell.split(':').chain(current.split(':')).chain(usual) {
        if !dir.is_empty() && !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs.join(":").into()
}

/// The user's shell in `wt`, with `rc` (a scratch dir) holding its startup files. zsh and bash
/// step back into the worktree after a `cd` out of it. On macOS a sandbox also hides `repo`'s
/// checkout and the other worktrees, and allows writes only in the worktree, the repo's git dir,
/// temp dirs, /dev and `~/.*` (tool caches, history).
// ponytail: Linux gets the cd hook only; wrap in bwrap or landlock when it needs the same walls.
pub fn command(wt: &Path, repo: &Path, rc: &Path) -> io::Result<Command> {
    let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| {
        if cfg!(target_os = "macos") { "/bin/zsh".into() } else { "/bin/sh".into() }
    });
    let mut argv: Vec<OsString> = vec![shell.clone().into()];
    let mut zdotdir = None;
    match Path::new(&shell).file_name().and_then(|n| n.to_str()) {
        Some("zsh") => {
            crate::security::private_dir(rc)?;
            let dir = rc.join("zsh");
            crate::security::private_dir(&dir)?;
            crate::security::write_private(&dir.join(".zshenv"), ZSHENV.as_bytes())?;
            crate::security::write_private(&dir.join(".zprofile"), ZPROFILE.as_bytes())?;
            crate::security::write_private(&dir.join(".zshrc"), ZSHRC.as_bytes())?;
            argv.push("-l".into());
            zdotdir = Some(dir);
        }
        Some("bash") => {
            crate::security::private_dir(rc)?;
            crate::security::write_private(&rc.join("bashrc"), BASHRC.as_bytes())?;
            argv.extend(["--rcfile".into(), rc.join("bashrc").into(), "-i".into()]);
        }
        _ => argv.push("-l".into()),
    }
    let sandbox = Path::new("/usr/bin/sandbox-exec");
    if cfg!(target_os = "macos") && sandbox.exists() {
        // The sandbox matches real paths: /var/… is /private/var/… to it.
        let real = |p: &Path| fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let (wt, repo, home) = (real(wt), real(repo), real(Path::new(&std::env::var("HOME").unwrap_or_default())));
        let others = wt.parent().unwrap_or(&wt);
        let rules = profile(&wt, &repo, others, &home);
        argv.splice(0..0, [sandbox.into(), "-p".into(), rules.into()]);
    }
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).current_dir(wt).env("ARRANGER_ROOT", wt);
    if let Some(dir) = zdotdir {
        cmd.env("ZDOTDIR", dir);
    }
    Ok(cmd)
}

/// A sandbox-exec profile: `repo`'s files (bar .git) and `others` (bar `wt`) can't be read or
/// written, and nothing else outside `wt`, .git, temp dirs, /dev or `home`'s dotfiles can be written.
fn profile(wt: &Path, repo: &Path, others: &Path, home: &Path) -> String {
    let (wt, git, repo, others) = (q(wt), q(&repo.join(".git")), q(repo), q(others));
    // Escape regex syntax first, then the sandbox string literal; paths can contain both.
    let home: String = home.to_string_lossy().chars().map(|c| if ".\\^$|?*+()[]{}".contains(c) { format!("\\{c}") } else { c.to_string() }).collect();
    let dotfiles = q(Path::new(&format!("^{home}/[.][^/]+")));
    format!(
        "(version 1)(allow default)
(deny file-read-data file-write*
  (require-all (subpath {repo}) (require-not (subpath {git})))
  (require-all (subpath {others}) (require-not (subpath {wt}))))
(deny file-write* (require-not (require-any
  (subpath {wt}) (subpath {git}) (subpath \"/dev\") (subpath \"/private/tmp\") (subpath \"/private/var/folders\")
  (regex #{dotfiles}))))"
    )
}

fn q(p: &Path) -> String {
    format!("\"{}\"", p.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_walls_off_the_checkout_and_other_worktrees() {
        let p = profile(Path::new("/d/wt/api/2"), Path::new("/src/api"), Path::new("/d/wt/api"), Path::new("/Users/a.b"));
        assert!(p.contains(r#"(require-all (subpath "/src/api") (require-not (subpath "/src/api/.git")))"#));
        assert!(p.contains(r#"(require-all (subpath "/d/wt/api") (require-not (subpath "/d/wt/api/2")))"#));
        assert!(p.contains(r#"(regex #"^/Users/a\\.b/[.][^/]+")"#));
        assert_eq!(q(Path::new(r#"/a "b"\c"#)), r#""/a \"b\"\\c""#);
    }
}

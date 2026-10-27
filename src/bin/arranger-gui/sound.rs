use std::process::{Command, Stdio};

pub fn should_ding(previous: &str, current: &str) -> bool {
    previous != current && matches!(current, "done" | "needs-you" | "setup-needs-you")
}

/// Keep file IO and audio playback off the window's event loop.
pub fn ding() {
    std::thread::spawn(|| {
        // Serialize playback so simultaneous machine updates don't amplify the sound.
        static PLAYING: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let Ok(_guard) = PLAYING.try_lock() else { return };
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("arranger-ding-{}-{stamp}", std::process::id()));
        use std::os::unix::fs::DirBuilderExt;
        if std::fs::DirBuilder::new().mode(0o700).create(&dir).is_err() {
            return;
        }
        let path = dir.join("ding.wav");
        if arranger::security::write_private(&path, include_bytes!("../../../assets/ding.wav")).is_err() {
            let _ = std::fs::remove_dir(dir);
            return;
        }
        #[cfg(target_os = "macos")]
        let players = ["afplay"];
        #[cfg(not(target_os = "macos"))]
        let players = ["paplay", "aplay"];
        for player in players {
            if Command::new(player)
                .arg(&path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
            {
                break;
            }
        }
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir(dir);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_attention_transitions_ding() {
        for state in ["done", "needs-you"] {
            assert!(should_ding("running", state));
            assert!(!should_ding(state, state));
            assert!(!should_ding(state, "running"));
        }
        assert!(!should_ding("running", "exited"));
        assert!(should_ding("needs-you", "done"));
    }
}

use std::io::Write;
use std::process::{Command, Stdio};

/// Cap on copied text so a runaway block can't be shoved at the clipboard.
const MAX_CLIPBOARD_BYTES: usize = 1_048_576; // 1 MiB

/// Copy text to the system clipboard via a platform CLI tool: `clip`/`clip.exe`
/// (Windows/WSL), `pbcopy` (macOS), or `wl-copy`/`xclip`/`xsel` (Linux). No
/// native-clipboard dependency — ignis stays a single binary.
pub fn set_clipboard(text: &str) -> Result<(), String> {
    if text.len() > MAX_CLIPBOARD_BYTES {
        return Err(format!(
            "Content too large ({} bytes, max {})",
            text.len(),
            MAX_CLIPBOARD_BYTES
        ));
    }
    try_platform_clipboard(text)
}

fn try_platform_clipboard(text: &str) -> Result<(), String> {
    let commands: Vec<(&str, Vec<&str>)> = match std::env::consts::OS {
        "windows" => vec![("clip", vec![])],
        "macos" => vec![("pbcopy", vec![])],
        "linux" => vec![
            ("clip.exe", vec![]),                       // WSL
            ("wl-copy", vec![]),                        // Wayland
            ("xclip", vec!["-selection", "clipboard"]), // X11
            ("xsel", vec!["-b"]),                       // X11 alternative
        ],
        _ => vec![],
    };

    for (cmd, args) in commands {
        if try_clip_command(cmd, &args, text).is_ok() {
            return Ok(());
        }
    }

    Err("Clipboard unavailable. Try selecting text with Shift+click or use your terminal's copy mode.".to_string())
}

fn try_clip_command(cmd: &str, args: &[&str], text: &str) -> Result<(), String> {
    let mut command = Command::new(cmd);
    command.args(args).stdin(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| format!("{cmd} not found: {e}"))?;
    if let Some(stdin) = child.stdin.take() {
        let mut writer = std::io::BufWriter::new(stdin);
        writer
            .write_all(text.as_bytes())
            .map_err(|e| e.to_string())?;
        writer.flush().map_err(|e| e.to_string())?;
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{cmd} exited with status {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::set_clipboard;

    #[test]
    fn set_clipboard_does_not_panic() {
        // We can't reliably roundtrip in CI/headless environments,
        // but we can verify the function doesn't panic and returns a result.
        let _ = set_clipboard("Hello world");
        let _ = set_clipboard("你好世界 🎉");
        let _ = set_clipboard("");
    }
}

//! Push-to-talk voice input — local, no cloud. Record the mic (ffmpeg /
//! avfoundation) → transcribe with whisper.cpp (`whisper-cli`) → drop the text
//! into the orchestrator bar for review + routing. Press `v` to start, `v` again
//! to stop. Model + device are env-overridable.
//!
//! Speed/quality: defaults to `ggml-large-v3-turbo` (~2s for a short command on
//! M-series). Set `AGENTMASTER_WHISPER_MODEL` to a `ggml-base`/`small` for lower
//! latency, or `AGENTMASTER_AUDIO_DEV` (e.g. `:0`) if `:default` mis-selects.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

fn which(bin: &str) -> bool {
    Command::new("which")
        .arg(bin)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Resolve the whisper ggml model: env override first, then the best local file.
pub fn model_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("AGENTMASTER_WHISPER_MODEL") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    [
        "Library/Application Support/voxflow/models/whisper.cpp/models/ggml-large-v3-turbo.bin",
        "Library/Application Support/superwhisper/ggml-large-v3-turbo.bin",
        "Library/Application Support/superwhisper/ggml-small.bin",
        "Library/Application Support/voxflow/models/whisper.cpp/models/ggml-base.bin",
    ]
    .iter()
    .map(|rel| PathBuf::from(&home).join(rel))
    .find(|p| p.exists())
}

/// Is the full voice pipeline available (recorder + transcriber + model)?
pub fn available() -> bool {
    which("whisper-cli") && which("ffmpeg") && model_path().is_some()
}

/// Start recording the default mic into a fresh wav (16 kHz mono). Returns the
/// running ffmpeg child + the wav path; stop with [`stop_recording`].
pub fn start_recording() -> Option<(Child, PathBuf)> {
    let wav = std::env::temp_dir().join("agentmaster-voice.wav");
    let dev = std::env::var("AGENTMASTER_AUDIO_DEV").unwrap_or_else(|_| ":default".into());
    let child = Command::new("ffmpeg")
        .args([
            "-loglevel",
            "error",
            "-f",
            "avfoundation",
            "-i",
            &dev,
            "-ar",
            "16000",
            "-ac",
            "1",
            "-y",
        ])
        .arg(&wav)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    Some((child, wav))
}

/// Stop the recorder gracefully so ffmpeg finalizes the wav (write `q` to its
/// stdin), then wait for it to exit.
pub fn stop_recording(mut child: Child) {
    if let Some(mut sin) = child.stdin.take() {
        let _ = sin.write_all(b"q\n");
        let _ = sin.flush();
    }
    let _ = child.wait();
}

/// Transcribe a wav with whisper.cpp. Language auto-detected (handles DE + EN).
pub fn transcribe(wav: &Path) -> Option<String> {
    let model = model_path()?;
    let out = Command::new("whisper-cli")
        .arg("-m")
        .arg(&model)
        .arg("-f")
        .arg(wav)
        .args(["-nt", "-np", "-l", "auto"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

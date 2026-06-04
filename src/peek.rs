//! Zero-tax inspection: read what an agent last said straight off its session
//! transcript (cmux / session-restore / Claude Code JSONL) instead of paying an
//! LLM to report. This is the agentmaster port of the orchestrator's `peek` —
//! structural digest of the on-disk JSONL, never a token round-trip.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// A compact, human-first read of a session's tail.
#[derive(Debug, Default, Clone)]
pub struct Digest {
    pub last_user: String,
    pub last_assistant: String,
    pub next_action: String,
    /// ISO-8601 timestamp of the last assistant message — when the agent last
    /// actually responded. The honest "last response" time.
    pub last_assistant_ts: Option<String>,
}

/// Pull the plain text out of a Claude-Code `message.content` field, which may be
/// a bare string or a list of typed blocks.
fn extract_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter(|i| i.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|i| i.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Digest the last `n` lines of a transcript into (last_user, last_assistant,
/// inferred next action). Tolerant of malformed lines and hook/system noise.
pub fn digest(path: &Path, n: usize) -> Digest {
    let mut d = Digest::default();
    let Ok(text) = std::fs::read_to_string(path) else {
        return d;
    };
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    for line in &lines[start..] {
        let Ok(ev) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let typ = ev.get("type").and_then(Value::as_str).unwrap_or("");
        let content = ev.pointer("/message/content").unwrap_or(&Value::Null);
        let text = extract_text(content);
        if text.is_empty() {
            continue;
        }
        match typ {
            // Skip injected hook/system blocks that start with a tag.
            "user" if !text.trim_start().starts_with('<') => d.last_user = text,
            "assistant" => {
                d.last_assistant = text;
                d.last_assistant_ts = ev
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            _ => {}
        }
    }
    d.next_action = next_action(&d.last_assistant, &d.last_user);
    d
}

/// Normalize a cmux workspace title to a stable match key: drop the leading
/// glyph/agent/status decoration (everything up to the last ` · `) and any
/// trailing ` [ref]`, lowercased. Robust to the status glyph drifting between a
/// snapshot read and a `cmux top` read.
pub fn norm_title(s: &str) -> String {
    let t = s.rsplit_once(" · ").map(|(_, b)| b).unwrap_or(s).trim();
    let t = match (t.rfind(" ["), t.ends_with(']')) {
        (Some(i), true) => &t[..i],
        _ => t,
    };
    t.trim().to_lowercase()
}

/// Map each live cmux workspace (by normalized title) to its Claude transcript.
/// Refreshes the cmux snapshot first (≈90ms) so just-spawned sessions are seen,
/// then reads `name → session_id` from it and resolves the transcript path.
pub fn cmux_transcripts() -> std::collections::HashMap<String, PathBuf> {
    use std::collections::HashMap;
    let mut map = HashMap::new();
    // Best-effort fresh snapshot; ignore failure (we then read whatever exists).
    let _ = std::process::Command::new("cmux-snapshot").output();
    let Ok(home) = std::env::var("HOME") else {
        return map;
    };
    let dir = PathBuf::from(&home).join(".cmux-snapshots");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return map;
    };
    let latest = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json"))
        .max_by_key(|p| {
            std::fs::metadata(p)
                .and_then(|m| m.modified())
                .ok()
                .map(|t| t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default())
        });
    let Some(latest) = latest else { return map };
    let Ok(text) = std::fs::read_to_string(&latest) else {
        return map;
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return map;
    };
    for ws in v
        .get("workspaces")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(name) = ws.get("name").and_then(Value::as_str) else {
            continue;
        };
        let key = norm_title(name);
        let surfaces = ws
            .pointer("/layout/surfaces")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for s in surfaces {
            if let Some(sid) = s.get("session_id").and_then(Value::as_str)
                && let Some(p) = resolve(sid)
            {
                map.insert(key.clone(), p);
                break;
            }
        }
    }
    map
}

/// Heuristic "what's the next move": pull an explicit next-step/TODO/checkbox/
/// numbered line from the assistant, else fall back to the last open question,
/// else the user's last ask. Plain substring scan, no regex dependency.
fn next_action(assistant: &str, user: &str) -> String {
    if assistant.is_empty() {
        return clip(user, 160);
    }
    const MARKERS: &[&str] = &[
        "nächster schritt",
        "next step",
        "next action",
        "als nächstes",
        "todo:",
        "→ ",
    ];
    for raw in assistant.lines() {
        let low = raw.to_lowercase();
        if let Some(m) = MARKERS.iter().find(|m| low.contains(**m)) {
            // Take the text after the marker on that line.
            if let Some(idx) = low.find(*m) {
                let tail = raw[idx + m.len()..].trim_start_matches([':', ' ', '-']);
                if !tail.trim().is_empty() {
                    return clip(tail.trim(), 180);
                }
            }
            return clip(raw.trim(), 180);
        }
        let t = raw.trim_start();
        if t.starts_with("- [ ]") || t.starts_with("1.") || t.starts_with("- ") {
            return clip(t, 180);
        }
    }
    if let Some(q) = assistant.lines().rev().find(|l| l.trim().ends_with('?')) {
        return format!("answer: {}", clip(q.trim(), 160));
    }
    clip(user, 160)
}

fn clip(s: &str, max: usize) -> String {
    let one = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= max {
        one
    } else {
        let mut t: String = one.chars().take(max).collect();
        t.push('…');
        t
    }
}

/// Resolve a Claude-Code session id (or a partial) to its transcript path under
/// `~/.claude/projects/*/<id>.jsonl`. Returns the first match. A real path passed
/// through is returned as-is.
pub fn resolve(id_or_path: &str) -> Option<PathBuf> {
    let direct = PathBuf::from(id_or_path);
    if direct.is_file() {
        return Some(direct);
    }
    let home = std::env::var("HOME").ok()?;
    let root = PathBuf::from(home).join(".claude/projects");
    let entries = std::fs::read_dir(&root).ok()?;
    for proj in entries.flatten() {
        let Ok(files) = std::fs::read_dir(proj.path()) else {
            continue;
        };
        for f in files.flatten() {
            let p = f.path();
            if p.extension().and_then(|e| e.to_str()) == Some("jsonl")
                && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
                && stem.starts_with(id_or_path)
            {
                return Some(p);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn norm_title_matches_across_glyph_drift() {
        // snapshot read and `cmux top` read may show a different status glyph —
        // both must normalize to the same key.
        assert_eq!(norm_title("🟣 Claude ▶ · kill dead code"), "kill dead code");
        assert_eq!(norm_title("🟣 Claude ✓ · kill dead code"), "kill dead code");
        assert_eq!(
            norm_title("🧠 Codex 🧪 · score projects [ws:5]"),
            "score projects"
        );
        assert_eq!(norm_title("plain"), "plain");
    }

    #[test]
    fn digests_user_and_assistant() {
        let mut f = tempfile();
        writeln!(
            f.0,
            r#"{{"type":"user","message":{{"content":"fix the parser"}}}}"#
        )
        .unwrap();
        writeln!(
            f.0,
            r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"done.\nNext step: run the tests"}}]}}}}"#
        )
        .unwrap();
        let d = digest(&f.1, 50);
        assert_eq!(d.last_user, "fix the parser");
        assert!(d.last_assistant.contains("done"));
        assert_eq!(d.next_action, "run the tests");
    }

    #[test]
    fn skips_hook_user_blocks() {
        let mut f = tempfile();
        writeln!(
            f.0,
            r#"{{"type":"user","message":{{"content":"<system-reminder>noise</system-reminder>"}}}}"#
        )
        .unwrap();
        writeln!(
            f.0,
            r#"{{"type":"user","message":{{"content":"real question"}}}}"#
        )
        .unwrap();
        let d = digest(&f.1, 50);
        assert_eq!(d.last_user, "real question");
    }

    // Minimal temp-file helper (avoids a dev-dependency).
    fn tempfile() -> (std::fs::File, PathBuf) {
        let mut p = std::env::temp_dir();
        // Deterministic-enough unique name without Date/rand (forbidden in some envs).
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        p.push(format!("am-peek-test-{n}.jsonl"));
        let f = std::fs::File::create(&p).unwrap();
        (f, p)
    }
}

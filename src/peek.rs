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

/// Map every running agent PID → its transcript, by scanning `ps` for
/// `… --resume <session-id> …` (Claude) and `… --session <id> …`-style args and
/// resolving the id to a transcript. This is the LIVE, exact, universal link:
/// a cmux/tmux workspace reports the agent's pid, and the agent process carries
/// its own session id on its command line — no snapshot, no fuzzy title match,
/// covers every running agent including just-spawned ones.
pub fn pid_transcripts() -> std::collections::HashMap<u32, PathBuf> {
    use std::collections::HashMap;
    let mut map = HashMap::new();
    let Ok(out) = std::process::Command::new("ps")
        .args(["-ax", "-o", "pid=,command="])
        .output()
    else {
        return map;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut codex_pids: Vec<u32> = Vec::new();
    for line in text.lines() {
        let line = line.trim_start();
        let Some((pid_s, rest)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let Ok(pid) = pid_s.trim().parse::<u32>() else {
            continue;
        };
        // Find a session id following a --resume / --session / -r flag (Claude).
        let toks: Vec<&str> = rest.split_whitespace().collect();
        let mut sid = None;
        for w in toks.windows(2) {
            if matches!(w[0], "--resume" | "--session" | "--session-id" | "-r")
                && looks_like_session_id(w[1])
            {
                sid = Some(w[1]);
                break;
            }
        }
        if let Some(sid) = sid {
            if let Some(p) = resolve(sid) {
                map.insert(pid, p);
            }
        } else if rest.contains("/codex") && !rest.contains(".app") && !rest.contains("Framework") {
            // Codex CLI carries no session id on argv; resolve it by cwd below.
            codex_pids.push(pid);
        }
    }
    // Codex: build a cwd → newest-rollout index once, then map each codex pid via
    // its working directory (lsof). Covers the codex half of the fleet.
    if !codex_pids.is_empty() {
        let idx = codex_rollouts_by_cwd();
        if !idx.is_empty() {
            for pid in codex_pids {
                if let Some(cwd) = pid_cwd(pid)
                    && let Some(p) = idx.get(&cwd)
                {
                    map.insert(pid, p.clone());
                }
            }
        }
    }
    map
}

/// Working directory of a process via `lsof` (the portable way on macOS).
fn pid_cwd(pid: u32) -> Option<String> {
    let out = std::process::Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix('n').map(str::to_string))
}

/// Index `~/.codex/sessions/**` rollouts (recent only) by their recorded `cwd`,
/// keeping the most-recently-modified rollout per cwd. Codex writes a
/// `session_meta` first line with `payload.cwd`.
fn codex_rollouts_by_cwd() -> std::collections::HashMap<String, PathBuf> {
    use std::collections::HashMap;
    let mut idx: HashMap<String, (PathBuf, std::time::SystemTime)> = HashMap::new();
    let Ok(home) = std::env::var("HOME") else {
        return HashMap::new();
    };
    let root = PathBuf::from(home).join(".codex/sessions");
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(72 * 3600);
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                stack.push(p);
                continue;
            }
            if p.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(mtime) = std::fs::metadata(&p).and_then(|m| m.modified()) else {
                continue;
            };
            if mtime < cutoff {
                continue;
            }
            if let Some(cwd) = codex_rollout_cwd(&p) {
                match idx.get(&cwd) {
                    Some((_, t)) if *t >= mtime => {}
                    _ => {
                        idx.insert(cwd, (p, mtime));
                    }
                }
            }
        }
    }
    idx.into_iter().map(|(k, (p, _))| (k, p)).collect()
}

/// Pull `payload.cwd` out of a codex rollout's first `session_meta` line.
fn codex_rollout_cwd(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines().take(5) {
        if let Ok(v) = serde_json::from_str::<Value>(line)
            && v.get("type").and_then(Value::as_str) == Some("session_meta")
            && let Some(cwd) = v.pointer("/payload/cwd").and_then(Value::as_str)
        {
            return Some(cwd.to_string());
        }
    }
    None
}

/// A session id is a uuid-ish token (hex + dashes, length ≥ 16) — cheap guard so
/// we don't try to resolve arbitrary flag values.
fn looks_like_session_id(s: &str) -> bool {
    s.len() >= 16 && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-') && s.contains('-')
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
    fn session_id_guard() {
        assert!(looks_like_session_id(
            "f002d084-d11a-4493-b72f-ab8572d16204"
        ));
        assert!(!looks_like_session_id("--dangerously-skip-permissions"));
        assert!(!looks_like_session_id("short"));
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

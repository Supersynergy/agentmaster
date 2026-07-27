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

/// True if a multiplexer-provided title is a system fragment rather than a real
/// task a human would recognise (`</task-notification>`, `## ▸ Last user request`,
/// a bare shell line, a URL, "Last login…"). Strips the cmux `glyph Agent state ·`
/// decoration first. When this is true we prefer the transcript's first prompt.
pub fn is_fragment_title(raw: &str) -> bool {
    let t = raw.rsplit(" · ").next().unwrap_or(raw).trim();
    let t = match (t.rfind(" ["), t.ends_with(']')) {
        (Some(i), true) => t[..i].trim(),
        _ => t,
    };
    // A leading ⏱ is cmux's marker for a non-agent shell tab (raw terminal text
    // as the title); strip it so the underlying junk is judged on its merits.
    let t = t.trim_start_matches('⏱').trim();
    t.is_empty()
        || t.starts_with('<')
        || t.starts_with("## ")
        || t.starts_with("http")
        || t.starts_with('│')
        || t.contains("Last login")
        || t.contains("Tips for getting started")
}

/// The first real user prompt in a transcript — the human's original ask, i.e.
/// the agent's actual task. Reads only the file head (early-returns on the first
/// non-hook user line), so it's cheap to call at import. Skips injected
/// `<…>`/system blocks. Gives an agent a meaningful title when its multiplexer
/// title is a fragment.
pub fn first_prompt(path: &Path) -> Option<String> {
    use std::io::BufRead;
    let f = std::fs::File::open(path).ok()?;
    for line in std::io::BufReader::new(f)
        .lines()
        .map_while(Result::ok)
        .take(400)
    {
        let Ok(ev) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if ev.get("type").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let content = ev.pointer("/message/content").unwrap_or(&Value::Null);
        let t = extract_text(content);
        let t = t.trim();
        if t.is_empty() || t.starts_with('<') || t.starts_with("## ▸") {
            continue;
        }
        return Some(clip(t, 200));
    }
    None
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

/// Raw tail of the last `n` transcript lines — undigested, just the text content
/// of each event in order. Useful for `agentmaster peek <id> --tail N` when you
/// want to see what the agent is actually doing right now (stream-of-thought)
/// rather than a structural digest. Empty lines skipped, hook blocks stripped.
pub fn tail(path: &Path, n: usize) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    let mut out = Vec::new();
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
        // Skip injected hook/system blocks (same filter as `digest`).
        if typ == "user" && text.trim_start().starts_with('<') {
            continue;
        }
        let tag = match typ {
            "user" => "🧑",
            "assistant" => "🤖",
            _ => "·",
        };
        out.push(format!("{tag} {text}"));
    }
    out
}

/// Exact session link for EVERY cmux workspace — live, idle, OR hibernated —
/// from the `resume_binding` cmux persists so it can resume an agent (the engine
/// behind cmux Agent Hibernation). For each workspace we read its
/// `resume_binding.checkpoint_id` (= the Claude session id) and resolve it to a
/// transcript, keyed by the STABLE `workspace:NN` ref. Unlike the snapshot's
/// title match, the ref never drifts as the agent works, so a linked time can't
/// later attach to the wrong tab. Covers the hibernated tabs that have no live
/// pid and whose title moved on since their last snapshot.
///
/// Two `cmux rpc` calls per workspace (~18ms each); runs on the discovery thread,
/// never the UI. Codex workspaces carry no checkpoint binding and fall through to
/// the pid/cwd paths.
///
/// Returns `(ref → transcript, workspace-UUID → ref)`. The second map lets the
/// live `cmux events` stream (whose `set_status` payload carries the workspace
/// UUID) resolve an event back to a `workspace:NN` agent without an extra RPC.
pub fn cmux_checkpoints() -> (
    std::collections::HashMap<String, PathBuf>,
    std::collections::HashMap<String, String>,
) {
    use std::collections::HashMap;
    let mut map = HashMap::new();
    let mut uuids: HashMap<String, String> = HashMap::new();
    let Ok(out) = std::process::Command::new("cmux")
        .args(["rpc", "workspace.list", "{}"])
        .output()
    else {
        return (map, uuids);
    };
    let Ok(v) = serde_json::from_slice::<Value>(&out.stdout) else {
        return (map, uuids);
    };
    for w in v
        .get("workspaces")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let (Some(id), Some(ws_ref)) = (
            w.get("id").and_then(Value::as_str),
            w.get("ref").and_then(Value::as_str),
        ) else {
            continue;
        };
        uuids.insert(id.to_string(), ws_ref.to_string());
        let params = format!("{{\"workspace_id\":\"{id}\"}}");
        let Ok(so) = std::process::Command::new("cmux")
            .args(["rpc", "surface.list", &params])
            .output()
        else {
            continue;
        };
        let Ok(sv) = serde_json::from_slice::<Value>(&so.stdout) else {
            continue;
        };
        for s in sv
            .get("surfaces")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(cid) = s
                .pointer("/resume_binding/checkpoint_id")
                .and_then(Value::as_str)
                && let Some(p) = resolve(cid)
            {
                map.insert(ws_ref.to_string(), p);
                break;
            }
        }
    }
    (map, uuids)
}

/// Parse a `cmux events` line. Returns `(workspace-UUID, pid, status)` for a
/// sidebar `set_status` event — the live status push behind the tab status bar —
/// else `None`. The payload arg looks like:
/// `claude_code Needs input --icon=… --tab=<UUID> --panel=… --pid=37706`, i.e.
/// `<agent-kind> <status words> --flags`.
pub fn parse_set_status(line: &str) -> Option<(Option<String>, Option<u32>, String)> {
    let v: Value = serde_json::from_str(line).ok()?;
    if v.pointer("/payload/command").and_then(Value::as_str) != Some("set_status") {
        return None;
    }
    let args = v.pointer("/payload/args").and_then(Value::as_str)?;
    // Status = everything between the leading agent-kind word and the first flag.
    let head = args.split(" --").next().unwrap_or("");
    let status = head
        .split_once(' ')
        .map(|(_, s)| s.trim().to_string())
        .unwrap_or_default();
    if status.is_empty() {
        return None;
    }
    let flag = |name: &str| {
        args.split_whitespace()
            .find_map(|t| t.strip_prefix(name).map(str::to_string))
    };
    let uuid = flag("--tab=");
    let pid = flag("--pid=").and_then(|s| s.parse::<u32>().ok());
    Some((uuid, pid, status))
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

/// Normalize a cmux workspace title to a stable match key: drop the leading
/// glyph/agent/status decoration (everything up to the last ` · `) and a trailing
/// ` [ref]`, lowercased — robust to the status glyph drifting between reads.
pub fn norm_title(s: &str) -> String {
    let t = s.rsplit_once(" · ").map(|(_, b)| b).unwrap_or(s).trim();
    let t = match (t.rfind(" ["), t.ends_with(']')) {
        (Some(i), true) => &t[..i],
        _ => t,
    };
    t.trim().to_lowercase()
}

/// Recover transcripts for DEAD / hibernated cmux tabs (no live pid) from the cmux
/// snapshot. Keyed by normalized title so the main thread can match it after the
/// exact pid path misses. Resolution ladder per surface, exact-first: `full_path`
/// (the snapshot already recorded the transcript path) → `session_id` resolved
/// under `~/.claude/projects` → `dir` fallback (newest transcript in that cwd: a
/// Claude project dir for claude surfaces, else the newest Codex rollout for that
/// cwd). The `dir` step is a "last activity in this dir" approximation that covers
/// tabs whose session id cmux never captured (most dead/hibernated codex + closed
/// tabs). Refreshes the snapshot first (≈90ms) so it is current.
pub fn snapshot_transcripts() -> std::collections::HashMap<String, PathBuf> {
    use std::collections::HashMap;
    let mut map = HashMap::new();
    let _ = std::process::Command::new("cmux-snapshot").output();
    let Ok(home) = std::env::var("HOME") else {
        return map;
    };
    let dir = PathBuf::from(&home).join(".cmux-snapshots");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return map;
    };
    let Some(latest) = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json"))
        .max_by_key(|p| {
            std::fs::metadata(p)
                .and_then(|m| m.modified())
                .ok()
                .map(|t| t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default())
        })
    else {
        return map;
    };
    let Ok(text) = std::fs::read_to_string(&latest) else {
        return map;
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return map;
    };
    // Built lazily — only when a codex surface needs the cwd→rollout fallback.
    let mut codex_idx: Option<HashMap<String, PathBuf>> = None;
    for ws in v
        .get("workspaces")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(name) = ws.get("name").and_then(Value::as_str) else {
            continue;
        };
        let surfaces = ws
            .pointer("/layout/surfaces")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut resolved: Option<PathBuf> = None;
        // Pass 1: exact links (full_path, then session_id).
        for s in &surfaces {
            if let Some(fp) = s.get("full_path").and_then(Value::as_str) {
                let p = PathBuf::from(fp);
                if p.is_file() {
                    resolved = Some(p);
                    break;
                }
            }
            if let Some(sid) = s.get("session_id").and_then(Value::as_str)
                && let Some(p) = resolve(sid)
            {
                resolved = Some(p);
                break;
            }
        }
        // Pass 2: cwd fallback (newest transcript in the surface's dir).
        if resolved.is_none() {
            for s in &surfaces {
                let cwd = s
                    .get("dir")
                    .and_then(Value::as_str)
                    .or_else(|| ws.get("cwd").and_then(Value::as_str))
                    .unwrap_or("");
                if cwd.is_empty() {
                    continue;
                }
                let is_claude = s.get("is_claude").and_then(Value::as_bool).unwrap_or(true);
                let hit = if is_claude {
                    claude_newest_in_dir(&home, cwd)
                } else {
                    codex_idx
                        .get_or_insert_with(codex_rollouts_by_cwd)
                        .get(cwd)
                        .cloned()
                };
                if let Some(p) = hit {
                    resolved = Some(p);
                    break;
                }
            }
        }
        if let Some(p) = resolved {
            map.insert(norm_title(name), p);
        }
    }
    map
}

/// Newest Claude transcript under the project dir that encodes `cwd`
/// (`/Users/me/app` → `-Users-me-app`; `/`, `.`, `_` all map to `-`). Best-effort
/// "last activity in this dir" when the snapshot captured no exact session id.
fn claude_newest_in_dir(home: &str, cwd: &str) -> Option<PathBuf> {
    let enc: String = cwd
        .trim_start_matches('/')
        .chars()
        .map(|c| if matches!(c, '/' | '.' | '_') { '-' } else { c })
        .collect();
    let dir = PathBuf::from(home)
        .join(".claude/projects")
        .join(format!("-{enc}"));
    std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jsonl"))
        .max_by_key(|p| {
            std::fs::metadata(p)
                .and_then(|m| m.modified())
                .ok()
                .map(|t| t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default())
        })
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
    fn parse_set_status_pulls_uuid_pid_and_multiword_status() {
        let line = r#"{"category":"sidebar","name":"sidebar.metadata.updated","payload":{"args":"claude_code Needs input --icon=bolt.fill --tab=42B77046-4683-45D6-8D27-090B4EE2ADBE --panel=X --pid=37706","command":"set_status"}}"#;
        let (uuid, pid, status) = parse_set_status(line).unwrap();
        assert_eq!(
            uuid.as_deref(),
            Some("42B77046-4683-45D6-8D27-090B4EE2ADBE")
        );
        assert_eq!(pid, Some(37706));
        assert_eq!(status, "Needs input"); // status is the words before the first flag
        // A non-set_status sidebar event yields nothing.
        assert!(parse_set_status(r#"{"payload":{"command":"clear_notifications"}}"#).is_none());
        // Malformed line is tolerated.
        assert!(parse_set_status("not json").is_none());
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
    fn fragment_titles_detected() {
        assert!(is_fragment_title("🟣 Claude ✓ · </task-notification>"));
        assert!(is_fragment_title(
            "🟣 Claude ▶ · ## ▸ Last user request foo"
        ));
        assert!(is_fragment_title("⏱ https://youtube.com/watch?v=x"));
        assert!(is_fragment_title("⏱ Last login: Thu May 28"));
        // a real task is NOT a fragment
        assert!(!is_fragment_title("🟣 Claude ✓ · fix the auth bug"));
        assert!(!is_fragment_title("kill dead code"));
    }

    #[test]
    fn first_prompt_skips_hook_blocks() {
        let mut f = tempfile();
        writeln!(
            f.0,
            r#"{{"type":"user","message":{{"content":"<system-reminder>noise</system-reminder>"}}}}"#
        )
        .unwrap();
        writeln!(
            f.0,
            r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"ok"}}]}}}}"#
        )
        .unwrap();
        writeln!(
            f.0,
            r#"{{"type":"user","message":{{"content":"build the dashboard"}}}}"#
        )
        .unwrap();
        assert_eq!(first_prompt(&f.1).as_deref(), Some("build the dashboard"));
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

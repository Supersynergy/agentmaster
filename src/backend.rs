//! Backend adapters: drive agents that already live in another multiplexer.
//! Native PTYs (agentmaster owns them) are the floor; this is interop on top.
//! First adapter: tmux — discover panes, read their screen, send keys, kill.
//! Pure `tmux` CLI calls, no extra dependency.

use std::process::{Command, Output};

use anyhow::{Context, Result, bail};

fn command_detail(output: &Output) -> String {
    let raw = if output.stderr.is_empty() {
        &output.stdout
    } else {
        &output.stderr
    };
    let text = String::from_utf8_lossy(raw);
    let compact = text
        .split_whitespace()
        .take(32)
        .collect::<Vec<_>>()
        .join(" ");
    if compact.is_empty() {
        format!("exit status {}", output.status)
    } else {
        compact.chars().take(500).collect()
    }
}

fn checked_output(command: &mut Command, label: &str) -> Result<Output> {
    let output = command.output().with_context(|| format!("start {label}"))?;
    if !output.status.success() {
        bail!("{label} failed: {}", command_detail(&output));
    }
    Ok(output)
}

/// A pane discovered in a running tmux server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalPane {
    pub target: String, // session:window.pane (stable address for send/capture)
    pub command: String,
    pub path: String,
    pub pid: Option<u32>,
    pub title: String,
}

pub fn tmux_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// All panes across every tmux session, or empty if tmux isn't running.
pub fn list_panes() -> Vec<ExternalPane> {
    list_panes_checked().unwrap_or_default()
}

/// Checked variant for headless callers that must distinguish an empty tmux
/// server from an unavailable/stale backend.
pub fn list_panes_checked() -> Result<Vec<ExternalPane>> {
    let fmt = "#{session_name}:#{window_index}.#{pane_index}\t#{pane_current_command}\t#{pane_current_path}\t#{pane_pid}\t#{pane_title}";
    let mut command = Command::new("tmux");
    command.args(["list-panes", "-a", "-F", fmt]);
    let output = checked_output(&mut command, "tmux list-panes")?;
    Ok(parse_panes(&String::from_utf8_lossy(&output.stdout)))
}

/// Parse `tmux list-panes -F` tab-delimited output. Separated out so it is
/// unit-testable without a running tmux server.
pub fn parse_panes(s: &str) -> Vec<ExternalPane> {
    s.lines()
        .filter_map(|l| {
            let mut f = l.splitn(5, '\t');
            let target = f.next()?.to_string();
            if target.is_empty() {
                return None;
            }
            let command = f.next().unwrap_or("").to_string();
            let path = f.next().unwrap_or("").to_string();
            let pid = f.next().and_then(|p| p.trim().parse::<u32>().ok());
            let title = f.next().unwrap_or("").to_string();
            Some(ExternalPane {
                target,
                command,
                path,
                pid,
                title,
            })
        })
        .collect()
}

/// Type a line + Enter into a pane and surface any stale-target/backend error.
pub fn send_keys(target: &str, text: &str) -> Result<()> {
    let mut command = Command::new("tmux");
    command.args(["send-keys", "-t", target, text, "Enter"]);
    checked_output(&mut command, &format!("tmux send-keys {target}"))?;
    Ok(())
}

/// Deliver Ctrl-C to one tmux pane without destroying the pane/session.
pub fn tmux_interrupt(target: &str) -> Result<()> {
    let mut command = Command::new("tmux");
    command.args(["send-keys", "-t", target, "C-c"]);
    checked_output(&mut command, &format!("tmux interrupt {target}"))?;
    Ok(())
}

/// Snapshot a pane's visible screen as plain text.
pub fn capture(target: &str) -> Option<String> {
    match Command::new("tmux")
        .args(["capture-pane", "-p", "-t", target])
        .output()
    {
        Ok(o) if o.status.success() => Some(String::from_utf8_lossy(&o.stdout).to_string()),
        _ => None, // pane gone / tmux error
    }
}

pub fn kill_pane(target: &str) {
    let _ = Command::new("tmux")
        .args(["kill-pane", "-t", target])
        .output();
}

// ---- cmux adapter ---------------------------------------------------------
// cmux exposes a rich CLI over its socket: `cmux top --all` lists every
// workspace with its title (the task), a status tag, and the agent pid; we can
// steer with `cmux send` / `cmux send-key`. Read-only discovery here.

/// A cmux workspace = one agent surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmuxWorkspace {
    pub ws_ref: String,          // workspace:NN — stable handle for send
    pub title: String,           // the task
    pub runtime: Option<String>, // agent tag, e.g. codex/hermes/ggcoder
    pub status: String,          // raw status from tags (working/done/Needs input/…)
    pub pid: Option<u32>,
    /// Repo state cmux now reports as a `git` tag: dirty / ahead / clean / none.
    pub git: Option<String>,
    /// Workflow phase cmux reports as a `phase` tag: e.g. tests-pass / commit.
    pub phase: Option<String>,
}

pub fn cmux_available() -> bool {
    cmux_access().is_ok()
}

/// Confirm that cmux is both installed and reachable from this process. cmux
/// intentionally rejects callers outside its process tree; callers need that
/// error instead of a misleading empty fleet.
pub fn cmux_access() -> Result<()> {
    let mut command = Command::new("cmux");
    command.arg("ping");
    checked_output(&mut command, "cmux ping")?;
    Ok(())
}

pub fn list_cmux() -> Vec<CmuxWorkspace> {
    list_cmux_checked().unwrap_or_default()
}

/// Checked cmux inventory for headless callers that need an actionable error.
pub fn list_cmux_checked() -> Result<Vec<CmuxWorkspace>> {
    // Prefer the stable structured surface (`--json`); the human-readable tree is
    // only kept as a fallback for older cmux builds that lack the flag.
    let structured = Command::new("cmux")
        .args(["top", "--all", "--json"])
        .output();
    if let Ok(output) = structured
        && output.status.success()
        && let Some(workspaces) = parse_cmux_top_json(&output.stdout)
    {
        return Ok(workspaces);
    }
    let mut fallback = Command::new("cmux");
    fallback.args(["top", "--all"]);
    let output = checked_output(&mut fallback, "cmux top --all")?;
    Ok(parse_cmux_top(&String::from_utf8_lossy(&output.stdout)))
}

/// Send a line to a cmux workspace's agent (text, then Return to submit).
pub fn cmux_send(ws_ref: &str, text: &str) -> Result<()> {
    let mut send = Command::new("cmux");
    send.args(["send", "--workspace", ws_ref, text]);
    checked_output(&mut send, &format!("cmux send {ws_ref}"))?;
    let mut enter = Command::new("cmux");
    enter.args(["send-key", "--workspace", ws_ref, "enter"]);
    checked_output(&mut enter, &format!("cmux send-key enter {ws_ref}"))?;
    Ok(())
}

/// Deliver Ctrl-C without closing a cmux workspace or its surrounding session.
pub fn cmux_interrupt(ws_ref: &str) -> Result<()> {
    let mut command = Command::new("cmux");
    command.args(["send-key", "--workspace", ws_ref, "ctrl+c"]);
    checked_output(&mut command, &format!("cmux interrupt {ws_ref}"))?;
    Ok(())
}

/// Switch the cmux UI to a workspace's tab — "jump to the live session". Uses
/// the `workspace.select` RPC (accepts the short `workspace:NN` ref).
pub fn cmux_focus_checked(ws_ref: &str) -> Result<()> {
    let payload = format!("{{\"workspace_id\":\"{ws_ref}\"}}");
    let mut command = Command::new("cmux");
    command.args(["rpc", "workspace.select", &payload]);
    checked_output(&mut command, &format!("cmux focus {ws_ref}"))?;
    Ok(())
}

/// Switch the tmux client's view to a pane's window + select the pane — the tmux
/// equivalent of "jump to the live session". `target` = `session:window.pane`.
pub fn tmux_focus_checked(target: &str) -> Result<()> {
    // window target = everything before the trailing `.pane`.
    let window = target.rsplit_once('.').map(|(w, _)| w).unwrap_or(target);
    let mut select_window = Command::new("tmux");
    select_window.args(["select-window", "-t", window]);
    checked_output(&mut select_window, &format!("tmux focus window {window}"))?;
    let mut select_pane = Command::new("tmux");
    select_pane.args(["select-pane", "-t", target]);
    checked_output(&mut select_pane, &format!("tmux focus pane {target}"))?;
    Ok(())
}

fn extract_quoted(s: &str) -> String {
    match (s.find('"'), s.rfind('"')) {
        (Some(a), Some(b)) if b > a => s[a + 1..b].to_string(),
        _ => String::new(),
    }
}

/// Parse `cmux top --all --json` into one entry per workspace. This is the
/// stable, structured surface — preferred over the human-readable tree. Returns
/// `None` if the bytes are not the expected JSON, so the caller can fall back to
/// [`parse_cmux_top`] for older cmux builds.
pub fn parse_cmux_top_json(bytes: &[u8]) -> Option<Vec<CmuxWorkspace>> {
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let mut out = Vec::new();
    collect_cmux_workspaces(&v, &mut out);
    out.retain(|w| !w.ws_ref.is_empty());
    Some(out)
}

/// Walk the `top --json` tree and collect every node tagged `"kind":"workspace"`.
fn collect_cmux_workspaces(v: &serde_json::Value, out: &mut Vec<CmuxWorkspace>) {
    match v {
        serde_json::Value::Object(map) => {
            if map.get("kind").and_then(|k| k.as_str()) == Some("workspace") {
                out.push(cmux_workspace_from_json(map));
            }
            for child in map.values() {
                collect_cmux_workspaces(child, out);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_cmux_workspaces(item, out);
            }
        }
        _ => {}
    }
}

/// Build one [`CmuxWorkspace`] from a JSON workspace node. Mirrors the text-tree
/// semantics exactly via two passes: the agent-type tag (`claude`/`codex`/…)
/// sets the base status, then the `*_code` tag overrides only when it reports
/// `Needs input` or no base status exists, and carries the agent pid.
fn cmux_workspace_from_json(map: &serde_json::Map<String, serde_json::Value>) -> CmuxWorkspace {
    let mut ws = CmuxWorkspace {
        ws_ref: map
            .get("ref")
            .and_then(|r| r.as_str())
            .unwrap_or("")
            .to_string(),
        title: map
            .get("title")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string(),
        runtime: None,
        status: String::new(),
        pid: None,
        git: None,
        phase: None,
    };
    let Some(tags) = map.get("tags").and_then(|t| t.as_array()) else {
        return ws;
    };
    let tag_kv = |t: &serde_json::Value| -> (String, String) {
        let o = t.as_object();
        let get = |k: &str| {
            o.and_then(|m| m.get(k))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        (get("key"), get("value"))
    };
    // Pass 1: repo/phase tags and the agent-type base status.
    for tag in tags {
        let (key, value) = tag_kv(tag);
        match key.as_str() {
            "git" if ws.git.is_none() => ws.git = Some(value),
            "phase" if ws.phase.is_none() => ws.phase = Some(value),
            "claude" | "codex" | "hermes" | "ggcoder" | "ggadapter" | "gemini" | "cline"
            | "opencode" | "amp" => {
                if ws.runtime.is_none() {
                    ws.runtime = Some(key.clone());
                }
                if ws.status.is_empty() {
                    ws.status = value;
                }
            }
            _ => {}
        }
    }
    // Pass 2: the `*_code` tag — strongest status signal + the agent pid.
    for tag in tags {
        let (key, value) = tag_kv(tag);
        if key.ends_with("_code") {
            if value.eq_ignore_ascii_case("needs input") || ws.status.is_empty() {
                ws.status = value;
            }
            if let Some(pid) = tag.get("pid").and_then(|p| p.as_u64()) {
                ws.pid = Some(pid as u32);
            }
        }
    }
    ws
}

/// Parse `cmux top --all` into one entry per workspace. Tolerant of the tree
/// drawing characters; pulls title, status tag, and pid. Unit-testable.
pub fn parse_cmux_top(s: &str) -> Vec<CmuxWorkspace> {
    let mut out = Vec::new();
    let mut cur: Option<CmuxWorkspace> = None;
    for line in s.lines() {
        if let Some(idx) = line.find("workspace workspace:") {
            if let Some(w) = cur.take() {
                out.push(w);
            }
            let rest = &line[idx + "workspace ".len()..];
            let ws_ref = rest.split_whitespace().next().unwrap_or("").to_string();
            cur = Some(CmuxWorkspace {
                ws_ref,
                title: extract_quoted(rest),
                runtime: None,
                status: String::new(),
                pid: None,
                git: None,
                phase: None,
            });
        } else if let Some(w) = cur.as_mut() {
            // Agent-type status tag, e.g. `tag claude "working"`.
            for runtime in [
                "claude",
                "codex",
                "hermes",
                "ggcoder",
                "ggadapter",
                "gemini",
                "cline",
                "opencode",
                "amp",
            ] {
                if line.contains(&format!("tag {runtime} ")) {
                    if w.runtime.is_none() {
                        w.runtime = Some(runtime.to_string());
                    }
                    if w.status.is_empty() {
                        w.status = extract_quoted(line);
                    }
                }
            }
            // `tag claude_code "Needs input" pid=37706` — strongest signal. Let it
            // win outright; otherwise only fill an empty status.
            if line.contains("_code \"") {
                let v = extract_quoted(line);
                if v.eq_ignore_ascii_case("needs input") || w.status.is_empty() {
                    w.status = v;
                }
            }
            if let Some(p) = line.find("pid=") {
                let digits: String = line[p + 4..]
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                if let Ok(n) = digits.parse() {
                    w.pid = Some(n);
                }
            }
            // Rich workspace tags cmux now emits alongside the agent status.
            if w.git.is_none() && line.contains("tag git \"") {
                w.git = Some(extract_quoted(line));
            }
            if w.phase.is_none() && line.contains("tag phase \"") {
                w.phase = Some(extract_quoted(line));
            }
        }
    }
    if let Some(w) = cur.take() {
        out.push(w);
    }
    out.retain(|w| !w.ws_ref.is_empty());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pane_rows() {
        let s = "main:0.0\tnvim\t/home/u/proj\t4210\teditor\n\
                 work:1.2\tclaude\t/home/u/app\t4310\tagent-1";
        let p = parse_panes(s);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].target, "main:0.0");
        assert_eq!(p[0].command, "nvim");
        assert_eq!(p[0].pid, Some(4210));
        assert_eq!(p[1].target, "work:1.2");
        assert_eq!(p[1].title, "agent-1");
    }

    #[test]
    fn tolerates_missing_fields_and_blank_lines() {
        let s = "\nonly:0.0\tbash\n";
        let p = parse_panes(s);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].command, "bash");
        assert_eq!(p[0].pid, None);
        assert_eq!(p[0].path, "");
    }

    #[test]
    fn parses_cmux_top() {
        let s = r#"
 99.2%  557.5 MB  6  ├── workspace workspace:96 "🟣 Claude ▶ · kill dead code"
  0.0%       0 B  0  │   ├── tag claude "working"
  0.0%       0 B  0  │   ├── tag git "dirty"
  0.0%       0 B  0  │   ├── tag phase "tests-pass"
 99.2%  550.5 MB  5  │   ├── tag claude_code "Running" pid=96335
  0.0%  513.7 MB  6  ├── workspace workspace:108 "🟣 Claude ✓ · answer me"
  0.0%       0 B  0  │   ├── tag claude "done"
  0.0%  507.6 MB  5  │   ├── tag claude_code "Needs input" pid=37706
"#;
        let w = parse_cmux_top(s);
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].ws_ref, "workspace:96");
        assert_eq!(w[0].title, "🟣 Claude ▶ · kill dead code");
        assert_eq!(w[0].runtime.as_deref(), Some("claude"));
        assert_eq!(w[0].status, "working");
        assert_eq!(w[0].pid, Some(96335));
        assert_eq!(w[0].git.as_deref(), Some("dirty"));
        assert_eq!(w[0].phase.as_deref(), Some("tests-pass"));
        // "Needs input" must win over the "done" agent tag.
        assert_eq!(w[1].runtime.as_deref(), Some("claude"));
        assert_eq!(w[1].status.to_lowercase(), "needs input");
        assert_eq!(w[1].pid, Some(37706));
        assert_eq!(w[1].git, None);
    }

    #[test]
    fn parses_cmux_top_json() {
        // Mirrors `parses_cmux_top` against the structured `--json` surface, and
        // shuffles ws:108's tag order so the `_code` tag precedes the agent tag —
        // the two-pass mapping must still let the agent tag set the base status.
        let s = r#"{
          "windows": [{
            "kind": "window",
            "children": [
              { "kind": "workspace", "ref": "workspace:96",
                "title": "🟣 Claude ▶ · kill dead code",
                "tags": [
                  { "kind": "tag", "key": "claude", "value": "working", "pid": null },
                  { "kind": "tag", "key": "git", "value": "dirty", "pid": null },
                  { "kind": "tag", "key": "phase", "value": "tests-pass", "pid": null },
                  { "kind": "tag", "key": "proc", "value": "0", "pid": null },
                  { "kind": "tag", "key": "claude_code", "value": "Running", "pid": 96335 }
                ] },
              { "kind": "workspace", "ref": "workspace:108",
                "title": "🟣 Claude ✓ · answer me",
                "tags": [
                  { "kind": "tag", "key": "claude_code", "value": "Needs input", "pid": 37706 },
                  { "kind": "tag", "key": "claude", "value": "done", "pid": null }
                ] },
              { "kind": "workspace", "ref": "workspace:102",
                "title": "idle shell", "tags": [
                  { "kind": "tag", "key": "phase", "value": "tests-pass", "pid": null }
                ] }
            ]
          }]
        }"#;
        let w = parse_cmux_top_json(s.as_bytes()).expect("valid json");
        assert_eq!(w.len(), 3);
        assert_eq!(w[0].ws_ref, "workspace:96");
        assert_eq!(w[0].title, "🟣 Claude ▶ · kill dead code");
        assert_eq!(w[0].runtime.as_deref(), Some("claude"));
        assert_eq!(w[0].status, "working");
        assert_eq!(w[0].pid, Some(96335));
        assert_eq!(w[0].git.as_deref(), Some("dirty"));
        assert_eq!(w[0].phase.as_deref(), Some("tests-pass"));
        // "Needs input" wins over "done" even though its tag comes first.
        assert_eq!(w[1].runtime.as_deref(), Some("claude"));
        assert_eq!(w[1].status.to_lowercase(), "needs input");
        assert_eq!(w[1].pid, Some(37706));
        assert_eq!(w[1].git, None);
        // A workspace with no agent tag is still listed (no status/pid).
        assert_eq!(w[2].ws_ref, "workspace:102");
        assert_eq!(w[2].status, "");
        assert_eq!(w[2].pid, None);
        assert_eq!(w[2].phase.as_deref(), Some("tests-pass"));
    }

    #[test]
    fn cmux_top_json_rejects_garbage() {
        assert!(parse_cmux_top_json(b"not json").is_none());
    }

    #[test]
    fn parses_hermes_and_ggadapter_runtime_tags() {
        let s = r#"
  0.0%  1 B  1  ├── workspace workspace:200 "hermes worker"
  0.0%  1 B  1  │   ├── tag hermes "working"
  0.0%  1 B  1  │   ├── tag hermes_code "Running" pid=2001
  0.0%  1 B  1  ├── workspace workspace:201 "GGAdapter worker"
  0.0%  1 B  1  │   ├── tag ggadapter "Needs input"
  0.0%  1 B  1  │   ├── tag ggadapter_code "Needs input" pid=2002
"#;
        let workspaces = parse_cmux_top(s);
        assert_eq!(workspaces.len(), 2);
        assert_eq!(workspaces[0].runtime.as_deref(), Some("hermes"));
        assert_eq!(workspaces[0].pid, Some(2001));
        assert_eq!(workspaces[1].runtime.as_deref(), Some("ggadapter"));
        assert_eq!(workspaces[1].status, "Needs input");
    }
}

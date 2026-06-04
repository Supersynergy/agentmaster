//! Backend adapters: drive agents that already live in another multiplexer.
//! Native PTYs (agentmaster owns them) are the floor; this is interop on top.
//! First adapter: tmux — discover panes, read their screen, send keys, kill.
//! Pure `tmux` CLI calls, no extra dependency.

use std::process::Command;

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
    let fmt = "#{session_name}:#{window_index}.#{pane_index}\t#{pane_current_command}\t#{pane_current_path}\t#{pane_pid}\t#{pane_title}";
    match Command::new("tmux")
        .args(["list-panes", "-a", "-F", fmt])
        .output()
    {
        Ok(o) if o.status.success() => parse_panes(&String::from_utf8_lossy(&o.stdout)),
        _ => Vec::new(),
    }
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

/// Type a line + Enter into a pane.
pub fn send_keys(target: &str, text: &str) {
    let _ = Command::new("tmux")
        .args(["send-keys", "-t", target, text, "Enter"])
        .output();
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
    pub ws_ref: String, // workspace:NN — stable handle for send
    pub title: String,  // the task
    pub status: String, // raw status from tags (working/done/Needs input/…)
    pub pid: Option<u32>,
    /// Repo state cmux now reports as a `git` tag: dirty / ahead / clean / none.
    pub git: Option<String>,
    /// Workflow phase cmux reports as a `phase` tag: e.g. tests-pass / commit.
    pub phase: Option<String>,
}

pub fn cmux_available() -> bool {
    Command::new("cmux")
        .arg("ping")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn list_cmux() -> Vec<CmuxWorkspace> {
    match Command::new("cmux").args(["top", "--all"]).output() {
        Ok(o) if o.status.success() => parse_cmux_top(&String::from_utf8_lossy(&o.stdout)),
        _ => Vec::new(),
    }
}

/// Send a line to a cmux workspace's agent (text, then Return to submit).
pub fn cmux_send(ws_ref: &str, text: &str) {
    let _ = Command::new("cmux")
        .args(["send", "--workspace", ws_ref, text])
        .output();
    let _ = Command::new("cmux")
        .args(["send-key", "--workspace", ws_ref, "Return"])
        .output();
}

/// Switch the cmux UI to a workspace's tab — "jump to the live session". Uses the
/// `workspace.select` RPC (accepts the short `workspace:NN` ref). Returns true if
/// the call succeeded, so the caller can report a switch vs a stale ref.
pub fn cmux_focus(ws_ref: &str) -> bool {
    Command::new("cmux")
        .args([
            "rpc",
            "workspace.select",
            &format!("{{\"workspace_id\":\"{ws_ref}\"}}"),
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Switch the tmux client's view to a pane's window + select the pane — the tmux
/// equivalent of "jump to the live session". `target` = `session:window.pane`.
pub fn tmux_focus(target: &str) -> bool {
    // window target = everything before the trailing `.pane`.
    let window = target.rsplit_once('.').map(|(w, _)| w).unwrap_or(target);
    let _ = Command::new("tmux")
        .args(["select-window", "-t", window])
        .output();
    Command::new("tmux")
        .args(["select-pane", "-t", target])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn extract_quoted(s: &str) -> String {
    match (s.find('"'), s.rfind('"')) {
        (Some(a), Some(b)) if b > a => s[a + 1..b].to_string(),
        _ => String::new(),
    }
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
                status: String::new(),
                pid: None,
                git: None,
                phase: None,
            });
        } else if let Some(w) = cur.as_mut() {
            // Agent-type status tag, e.g. `tag claude "working"`.
            if (line.contains("tag claude ")
                || line.contains("tag codex ")
                || line.contains("tag opencode ")
                || line.contains("tag amp "))
                && w.status.is_empty()
            {
                w.status = extract_quoted(line);
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
        assert_eq!(w[0].status, "working");
        assert_eq!(w[0].pid, Some(96335));
        assert_eq!(w[0].git.as_deref(), Some("dirty"));
        assert_eq!(w[0].phase.as_deref(), Some("tests-pass"));
        // "Needs input" must win over the "done" agent tag.
        assert_eq!(w[1].status.to_lowercase(), "needs input");
        assert_eq!(w[1].pid, Some(37706));
        assert_eq!(w[1].git, None);
    }
}

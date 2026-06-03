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
}

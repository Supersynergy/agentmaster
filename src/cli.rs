//! Headless orchestrator surface. Everything you can do to *external* agents
//! (tmux panes + cmux workspaces) without opening the TUI: list, steer one,
//! broadcast to all, and set/inspect goals. Native PTYs live inside the TUI
//! process, so they are intentionally not steerable from a separate CLI call.
//!
//! `--json` is offered everywhere an agent might consume the output (the
//! cli-anything contract). Zero token tax: status is read off `cmux top`/tmux,
//! never asked for.

use std::path::Path;

use anyhow::Result;

use crate::backend;
use crate::store::Store;

/// herdr-style at-a-glance glyph for a raw status string.
pub fn glyph(status: &str) -> char {
    let s = status.to_lowercase();
    if ["need", "input", "wait", "block", "ask", "confirm"]
        .iter()
        .any(|k| s.contains(k))
    {
        '⏸'
    } else if ["done", "complet", "finish", "ready"]
        .iter()
        .any(|k| s.contains(k))
    {
        '✓'
    } else if ["work", "run", "busy", "think", "exec", "stream"]
        .iter()
        .any(|k| s.contains(k))
    {
        '▶'
    } else {
        '·'
    }
}

fn json_escape(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// `ls` — every discoverable live agent across backends. Read-only.
pub fn list(json: bool) -> Result<()> {
    let panes = if backend::tmux_available() {
        backend::list_panes()
    } else {
        Vec::new()
    };
    let ws = if backend::cmux_available() {
        backend::list_cmux()
    } else {
        Vec::new()
    };
    if json {
        let mut items: Vec<String> = Vec::new();
        for p in &panes {
            items.push(format!(
                "{{\"backend\":\"tmux\",\"ref\":{},\"command\":{},\"pid\":{},\"title\":{}}}",
                json_escape(&p.target),
                json_escape(&p.command),
                p.pid
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "null".into()),
                json_escape(&p.title)
            ));
        }
        for w in &ws {
            items.push(format!(
                "{{\"backend\":\"cmux\",\"ref\":{},\"status\":{},\"glyph\":{},\"pid\":{},\"title\":{}}}",
                json_escape(&w.ws_ref),
                json_escape(&w.status),
                json_escape(&glyph(&w.status).to_string()),
                w.pid.map(|v| v.to_string()).unwrap_or_else(|| "null".into()),
                json_escape(&w.title)
            ));
        }
        println!("[{}]", items.join(","));
        return Ok(());
    }
    if panes.is_empty() && ws.is_empty() {
        println!("no live agents (tmux panes / cmux workspaces) discoverable");
        return Ok(());
    }
    for p in &panes {
        println!(
            "[tmux] {:<16} {:<10} pid {:<7} {}",
            p.target,
            p.command,
            p.pid.map(|v| v.to_string()).unwrap_or_else(|| "-".into()),
            p.title
        );
    }
    for w in &ws {
        println!(
            "[cmux] {} {:<14} {:<13} {}",
            glyph(&w.status),
            w.ws_ref,
            if w.status.is_empty() { "-" } else { &w.status },
            w.title
        );
    }
    println!(
        "\n{} tmux · {} cmux   (⏸ blocked · ▶ working · ✓ done · · idle)",
        panes.len(),
        ws.len()
    );
    Ok(())
}

/// Resolve a ref to its backend and send a line. `workspace:NN` → cmux,
/// `session:win.pane` (anything else) → tmux. Returns true if routed.
pub fn send(reference: &str, msg: &str, dir: &Path) -> Result<()> {
    if msg.trim().is_empty() {
        anyhow::bail!("empty message");
    }
    if reference.starts_with("workspace:") {
        backend::cmux_send(reference, msg);
    } else {
        backend::send_keys(reference, msg);
    }
    if let Ok(s) = Store::open(&dir.join("agentmaster.db")) {
        s.log(None, reference, "send", msg);
    }
    println!("sent → {reference}");
    Ok(())
}

/// Broadcast a line to every live agent. cmux always; tmux when `include_tmux`.
/// `needs_input` narrows cmux targets to those waiting on a human.
pub fn broadcast(msg: &str, include_tmux: bool, needs_input: bool, dir: &Path) -> Result<()> {
    if msg.trim().is_empty() {
        anyhow::bail!("empty message");
    }
    let mut n = 0;
    if backend::cmux_available() {
        for w in backend::list_cmux() {
            if needs_input && glyph(&w.status) != '⏸' {
                continue;
            }
            backend::cmux_send(&w.ws_ref, msg);
            println!("→ [cmux] {} {}", w.ws_ref, w.title);
            n += 1;
        }
    }
    if include_tmux && backend::tmux_available() {
        for p in backend::list_panes() {
            if p.command.contains("agentmaster") {
                continue; // never broadcast into our own pane
            }
            backend::send_keys(&p.target, msg);
            println!("→ [tmux] {}", p.target);
            n += 1;
        }
    }
    if let Ok(s) = Store::open(&dir.join("agentmaster.db")) {
        s.log(
            None,
            "orchestrator",
            "broadcast",
            &format!("{n} agents: {msg}"),
        );
    }
    println!("\nbroadcast to {n} live agent(s)");
    Ok(())
}

/// `goal <name> <goal> [--dod ...]` — persist a goal the TUI rehydrates on import.
pub fn goal_set(name: &str, goal: &str, dod: Option<&str>, dir: &Path) -> Result<()> {
    let s = Store::open(&dir.join("agentmaster.db"))?;
    s.set_goal(name, goal, dod);
    s.log(None, name, "goal", goal);
    println!("🎯 goal set on {name}");
    if let Some(d) = dod {
        println!("   ✓dod: {d}");
    }
    Ok(())
}

/// `goals` — list every stored goal + its progress.
pub fn goals_list(json: bool, dir: &Path) -> Result<()> {
    let s = Store::open(&dir.join("agentmaster.db"))?;
    let goals = s.load_goals();
    if json {
        let items: Vec<String> = goals
            .iter()
            .map(|(n, g, d, p)| {
                format!(
                    "{{\"name\":{},\"goal\":{},\"dod\":{},\"progress\":{}}}",
                    json_escape(n),
                    json_escape(g),
                    d.as_deref()
                        .map(json_escape)
                        .unwrap_or_else(|| "null".into()),
                    p
                )
            })
            .collect();
        println!("[{}]", items.join(","));
        return Ok(());
    }
    if goals.is_empty() {
        println!("no goals set");
        return Ok(());
    }
    for (name, goal, dod, progress) in goals {
        println!(
            "🎯 {:>3}%  {:<28} {}{}",
            progress,
            clip(&name, 28),
            clip(&goal, 50),
            dod.map(|d| format!("  ✓dod: {d}")).unwrap_or_default()
        );
    }
    Ok(())
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glyphs_map_status() {
        assert_eq!(glyph("Needs input"), '⏸');
        assert_eq!(glyph("Running"), '▶');
        assert_eq!(glyph("done"), '✓');
        assert_eq!(glyph(""), '·');
    }

    #[test]
    fn json_escapes_quotes() {
        assert_eq!(json_escape("a\"b"), "\"a\\\"b\"");
    }
}

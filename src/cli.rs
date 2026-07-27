//! Headless orchestrator surface. Everything you can do to *external* agents
//! (tmux panes + cmux workspaces) without opening the TUI: list, steer one,
//! broadcast to all, and set/inspect goals. Native PTYs live inside the TUI
//! process, so they are intentionally not steerable from a separate CLI call.
//!
//! `--json` is offered everywhere an agent might consume the output (the
//! cli-anything contract). Zero token tax: status is read off `cmux top`/tmux,
//! never asked for.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::backend;
use crate::peek;
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

/// Jump to an agent's live tab. `workspace:NN` → cmux `workspace.select`,
/// anything else → tmux `select-window`/`select-pane`.
pub fn focus(reference: &str) -> Result<()> {
    let ok = if reference.starts_with("workspace:") {
        backend::cmux_focus(reference)
    } else {
        backend::tmux_focus(reference)
    };
    if ok {
        println!("→ focused {reference}");
        Ok(())
    } else {
        anyhow::bail!("could not focus {reference} (stale ref or backend unavailable)")
    }
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

/// `goal <name> <goal> --oracle <cmd> [--budget N] [--deadline ISO]` — omnigoal
/// lifecycle init. Stores the machine-checkable oracle, token budget cap, and
/// deadline alongside the goal. The TUI and `goal check` rehydrate these.
pub fn goal_init(
    name: &str,
    goal: &str,
    dod: Option<&str>,
    oracle: Option<&str>,
    budget: u64,
    deadline: Option<&str>,
    dir: &Path,
) -> Result<()> {
    let s = Store::open(&dir.join("agentmaster.db"))?;
    s.set_goal_omni(name, goal, dod, oracle, budget, deadline);
    s.log(None, name, "goal-init", goal);
    println!("🎯 omnigoal set on {name}");
    if let Some(d) = dod {
        println!("   ✓dod: {d}");
    }
    if let Some(o) = oracle {
        println!("   🔮 oracle: {o}");
    }
    if budget > 0 {
        println!("   💰 budget: {budget} tokens");
    }
    if let Some(d) = deadline {
        println!("   ⏰ deadline: {d}");
    }
    Ok(())
}

/// `goal check <name>` — run the goal's oracle in a subshell. Ponytail: we
/// shell out, capture stdout/stderr, and classify the result. On failure we
/// scan the output for the bottleneck (first line matching
/// `error|fail|missing|panic|not found|undefined`) and record a try. Three
/// tries and we abandon the goal — no infinite loops.
///
/// Exit codes:
///   0 = oracle passed (goal done)
///   1 = oracle failed (try recorded)
///   2 = 3-try cap hit (goal abandoned)
///   3 = no oracle configured (not an omnigoal)
pub fn goal_check(name: &str, dir: &Path) -> Result<()> {
    use std::process::Command;
    let s = Store::open(&dir.join("agentmaster.db"))?;
    let Some((_goal, _dod, _progress, oracle, budget, deadline, tries, status, _bn, _sum)) =
        s.load_goal_omni(name)
    else {
        anyhow::bail!("no such goal: {name}");
    };
    if status == "done" {
        println!("✓ goal {name} already done");
        return Ok(());
    }
    if status == "abandoned" {
        println!("⚠ goal {name} abandoned (re-init with `goal <name> ... --oracle ...`)");
        return Ok(());
    }
    let Some(oracle_cmd) = oracle else {
        println!("✗ goal {name} has no oracle (not an omnigoal)");
        std::process::exit(3);
    };
    // Budget check: if a cap is set and AM_GOAL_SPEND_<NAME> is provided, gate.
    if budget > 0 {
        let env_key = format!("AM_GOAL_SPEND_{}", name.replace('-', "_").to_uppercase());
        if let Ok(spend_str) = std::env::var(&env_key)
            && let Ok(spend) = spend_str.trim().parse::<u64>()
            && spend >= budget
        {
            let reason = format!("budget cap hit: {spend} >= {budget} tokens");
            s.goal_abandon(name, &reason);
            println!("💸 goal {name} abandoned: {reason}");
            std::process::exit(2);
        }
    }
    // Deadline check: if past deadline, abandon.
    if let Some(dl) = deadline
        && let Ok(now) = chrono::Local::now()
            .to_rfc3339()
            .parse::<chrono::DateTime<chrono::FixedOffset>>()
        && let Ok(dl_ts) = dl.parse::<chrono::DateTime<chrono::FixedOffset>>()
        && now > dl_ts
    {
        let reason = format!("deadline passed: {dl}");
        s.goal_abandon(name, &reason);
        println!("⏰ goal {name} abandoned: {reason}");
        std::process::exit(2);
    }
    // 3-try cap: if we've already tried 3 times, abandon.
    if tries >= 3 {
        let reason = "3-try cap hit without oracle passing";
        s.goal_abandon(name, reason);
        println!("🛑 goal {name} abandoned: {reason}");
        std::process::exit(2);
    }
    println!("🔮 running oracle: {oracle_cmd}");
    let out = Command::new("sh").arg("-c").arg(&oracle_cmd).output();
    match out {
        Ok(o) if o.status.success() => {
            println!("✓ oracle passed — goal {name} done");
            s.goal_close(name, &format!("oracle passed: {oracle_cmd}"));
            Ok(())
        }
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let stderr = String::from_utf8_lossy(&o.stderr);
            let combined = format!("{stdout}\n{stderr}");
            let bottleneck = combined
                .lines()
                .find(|l| {
                    let low = l.to_lowercase();
                    [
                        "error",
                        "fail",
                        "missing",
                        "panic",
                        "not found",
                        "undefined",
                    ]
                    .iter()
                    .any(|k| low.contains(k))
                })
                .map(|l| l.trim().to_string());
            let new_tries = s.goal_record_try(name, bottleneck.as_deref());
            println!("✗ oracle failed (try {new_tries}/3)");
            if let Some(b) = &bottleneck {
                println!("   🧱 bottleneck: {b}");
            } else if !stdout.trim().is_empty() || !stderr.trim().is_empty() {
                println!("   stdout: {}", stdout.trim());
                if !stderr.trim().is_empty() {
                    println!("   stderr: {}", stderr.trim());
                }
            }
            std::process::exit(1);
        }
        Err(e) => {
            let bottleneck = format!("oracle spawn failed: {e}");
            s.goal_record_try(name, Some(&bottleneck));
            println!("✗ {bottleneck}");
            std::process::exit(1);
        }
    }
}

/// `goal close <name> [--summary ...] [--abandon]` — mark a goal done (or
/// abandoned) with a closing summary. The summary is persisted to the goal row
/// + the event stream, so the next session can rehydrate the outcome without
///
/// replaying the transcript.
pub fn goal_close(name: &str, summary: Option<&str>, abandon: bool, dir: &Path) -> Result<()> {
    let s = Store::open(&dir.join("agentmaster.db"))?;
    if s.load_goal_omni(name).is_none() {
        anyhow::bail!("no such goal: {name}");
    }
    let msg = summary.unwrap_or("(no summary provided)");
    if abandon {
        s.goal_abandon(name, msg);
        println!("⚠ goal {name} abandoned: {msg}");
    } else {
        s.goal_close(name, msg);
        println!("✓ goal {name} closed: {msg}");
    }
    Ok(())
}

/// `goal spawn <name> [--capsule path] [--skill name]` — register a subagent
/// against this goal with a bounded capsule + skill. Ponytail: we log the
/// event only — the capsule file lives on disk and is referenced by path,
/// never inlined into the goal row. Cross-agent coordination happens via the
/// goal JSON (the row), not by sharing transcripts.
pub fn goal_spawn(
    name: &str,
    capsule: Option<&str>,
    skill: Option<&str>,
    dir: &Path,
) -> Result<()> {
    let s = Store::open(&dir.join("agentmaster.db"))?;
    if s.load_goal_omni(name).is_none() {
        anyhow::bail!("no such goal: {name}");
    }
    s.goal_spawn(name, capsule, skill);
    println!("🧑‍🚀 spawned subagent for {name}");
    if let Some(c) = capsule {
        println!("   📦 capsule: {c}");
    }
    if let Some(sk) = skill {
        println!("   🧰 skill: {sk}");
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

/// `send --wait <target> <msg>` — handoff-style: send the line, then poll the
/// target's transcript until a NEW assistant message lands (baseline captured
/// pre-send) or the timeout expires. Returns the new assistant text on success,
/// or a "still running" notice on timeout (the agent keeps working — same
/// semantics as CAO's `handoff` primitive). Zero token tax: reads the JSONL
/// directly, never asks the agent to report.
pub fn send_wait(reference: &str, msg: &str, dir: &Path, timeout_secs: u64) -> Result<()> {
    use std::time::{Duration, Instant};
    send(reference, msg, dir)?;
    let baseline = match peek::resolve(reference) {
        Some(p) => peek::digest(&p, 220).last_assistant_ts,
        None => {
            println!("(no transcript linked to {reference}; cannot wait — message sent)");
            return Ok(());
        }
    };
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let mut tick = 0;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(2));
        tick += 1;
        let path = match peek::resolve(reference) {
            Some(p) => p,
            None => continue,
        };
        let d = peek::digest(&path, 220);
        let new_ts = d.last_assistant_ts.as_deref();
        if new_ts.is_some() && new_ts != baseline.as_deref() && !d.last_assistant.is_empty() {
            println!("\n🤖 {d}", d = d.last_assistant);
            return Ok(());
        }
        eprint!(
            "\r  …waiting ({tick}s, {}s left)   ",
            deadline.duration_since(Instant::now()).as_secs()
        );
    }
    eprintln!();
    println!("⏳ still running after {timeout_secs}s — peek {reference} later for the result");
    Ok(())
}

/// `assign <runtime> <task>` — async fan-out: spawn one fresh cmux workspace
/// seeded with the task, log the callback-ref (workspace:NN) so the supervisor
/// can `peek` it later, and return immediately. Fire-and-forget like CAO's
/// `assign` primitive — the worker is expected to work independently. The
/// workspace name is auto-generated from the task slug so the supervisor can
/// find it by name later. Returns the spawned workspace ref.
pub fn assign(runtime: &str, task: &str, name: Option<&str>, dir: &Path) -> Result<String> {
    assign_with_model(runtime, None, task, name, dir)
}

/// `assign` with a verified runtime-level model selector.
pub fn assign_with_model(
    runtime: &str,
    model: Option<&str>,
    task: &str,
    name: Option<&str>,
    dir: &Path,
) -> Result<String> {
    let cmux = std::env::var("CMUX_BIN").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        format!("{home}/.local/bin/cmux")
    });
    let owned_slug: String = name.map(str::to_string).unwrap_or_else(|| {
        let s: String = task
            .chars()
            .take(40)
            .map(|c| {
                if c.is_alphanumeric() || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        s.trim_matches('-').to_string()
    });
    let slug = owned_slug.as_str();
    let spec = crate::runtime::resolve_with_model(runtime, Some(task), model);
    let mut launch = vec![spec.program];
    launch.extend(spec.args);
    let launch_cmd = launch.join(" ");
    let workdir = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".into());
    let out = std::process::Command::new(&cmux)
        .args([
            "new-workspace",
            "--name",
            slug,
            "--cwd",
            &workdir,
            "--command",
            &launch_cmd,
        ])
        .output()?;
    let first = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or("spawned")
        .to_string();
    let ws_ref = first
        .split_whitespace()
        .find(|w| w.starts_with("workspace:"))
        .unwrap_or(&first)
        .to_string();
    if let Ok(s) = Store::open(&dir.join("agentmaster.db")) {
        s.log(None, &ws_ref, "assign", &format!("{runtime}: {task}"));
    }
    println!("assigned → {ws_ref}  ({runtime}, task: {slug})");
    println!("  peek later: agentmaster peek {ws_ref}");
    Ok(ws_ref)
}

/// `models [--tier T] [--json]` — list the model registry. The registry is
/// the single source of truth for cost-ladder decisions (no remote API calls).
/// `--tier` filters to one of `open_weight|mid_tier|frontier_closed`.
pub fn models_list(tier: Option<&str>, json: bool) -> Result<()> {
    use crate::router::{REGISTRY, Tier};
    let filter = match tier {
        Some("open_weight") => Some(Tier::OpenWeight),
        Some("mid_tier") => Some(Tier::Mid),
        Some("frontier_closed") | Some("frontier") => Some(Tier::Frontier),
        Some(t) => {
            anyhow::bail!("unknown tier '{t}' — expected open_weight|mid_tier|frontier_closed")
        }
        None => None,
    };
    let rows: Vec<&crate::router::ModelSpec> = REGISTRY
        .iter()
        .filter(|m| filter.is_none_or(|t| m.tier == t))
        .collect();
    if json {
        let items: Vec<String> = rows
            .iter()
            .map(|m| {
                format!(
                    "{{\"name\":{},\"runtime\":{},\"model\":{},\"vendor\":{},\"tier\":{},\"input\":{},\"output\":{},\"context\":{},\"strengths\":[{}],\"weaknesses\":[{}],\"best_for\":[{}]}}",
                    json_escape(m.name),
                    json_escape(m.runtime),
                    json_escape(m.model_flag),
                    json_escape(m.vendor),
                    json_escape(&m.tier.to_string()),
                    m.input_price,
                    m.output_price,
                    m.context_window,
                    m.strengths.iter().map(|s| json_escape(s)).collect::<Vec<_>>().join(","),
                    m.weaknesses.iter().map(|s| json_escape(s)).collect::<Vec<_>>().join(","),
                    m.best_for.iter().map(|t| json_escape(&t.to_string())).collect::<Vec<_>>().join(","),
                )
            })
            .collect();
        println!("[{}]", items.join(","));
        return Ok(());
    }
    if rows.is_empty() {
        println!("no models match tier '{tier:?}'");
        return Ok(());
    }
    println!("MODEL REGISTRY (2026-07 pricing, USD per 1M tokens)");
    println!(
        "{:<20} {:<10} {:<18} {:<14} {:>7} {:>7} {:>10}",
        "NAME", "RUNTIME", "MODEL FLAG", "TIER", "IN", "OUT", "BLEND"
    );
    println!("{}", "-".repeat(90));
    for m in rows {
        println!(
            "{:<20} {:<10} {:<18} {:<14} {:>7.2} {:>7.2} {:>10.2}",
            m.name,
            m.runtime,
            m.model_flag,
            m.tier,
            m.input_price,
            m.output_price,
            m.blended_cost()
        );
        println!("    strengths : {}", m.strengths.join(", "));
        println!("    weaknesses: {}", m.weaknesses.join(", "));
        println!(
            "    best_for  : {}",
            m.best_for
                .iter()
                .map(|t| t.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}

/// `auto <task>` — classify the task, pick the cheapest passing model, spawn it.
/// Ponytail: one classifier, one pick, reuse `assign`. The only new logic is
/// printing the classified type + picked model before spawning.
pub fn auto_spawn(task: &str, name: Option<&str>, dry_run: bool, dir: &Path) -> Result<()> {
    use crate::router;
    let task_type = router::classify(task);
    let model = router::pick(task_type);
    println!("🧭 classified: {task_type}");
    println!(
        "🎯 picked    : {name} ({runtime}/{flag}, ${blend:.2}/1M blend)",
        name = model.name,
        runtime = model.runtime,
        flag = model.model_flag,
        blend = model.blended_cost()
    );
    if dry_run {
        println!("(--dry-run — not spawning)");
        return Ok(());
    }
    let ws = assign_with_model(model.runtime, Some(model.model_flag), task, name, dir)?;
    if let Ok(s) = Store::open(&dir.join("agentmaster.db")) {
        s.log(None, &ws, "auto", &format!("{}: {}", model.name, task));
    }
    Ok(())
}

pub const DEFAULT_SWARM_MAX_LANES: usize = 3;

/// Explicit swarm request. A named request prevents new safety controls from
/// silently becoming another positional parameter at the CLI boundary.
pub struct SwarmRequest<'a> {
    pub name: &'a str,
    pub task: &'a str,
    pub oracle: &'a str,
    pub n: usize,
    pub fanout: bool,
    pub budget: u64,
    pub budget_tokens_per_lane: u64,
    pub budget_cost: f64,
    pub deadline_secs: u64,
    pub no_parallel: bool,
    pub recall: bool,
    pub dry_run: bool,
}

/// `swarm <name> --oracle "<cmd>" [-n N] <task>` — oracle-gated parallel
/// swarm. The default is deliberately capped at three diverse lanes; a wider
/// search requires explicit `--fanout`. Capsules contain bounded task context,
/// not transcripts. Recall is opt-in to avoid cross-project leakage.
pub fn swarm_spawn(request: SwarmRequest<'_>, dir: &Path) -> Result<()> {
    use crate::{ats, router, swarm as swarm_engine};
    let SwarmRequest {
        name,
        task,
        oracle,
        n,
        fanout,
        budget,
        budget_tokens_per_lane,
        budget_cost,
        deadline_secs,
        no_parallel,
        recall,
        dry_run,
    } = request;
    if n == 0 {
        anyhow::bail!("--n must be at least 1");
    }
    if n > DEFAULT_SWARM_MAX_LANES && !fanout {
        anyhow::bail!(
            "--n {n} exceeds the safe default of {DEFAULT_SWARM_MAX_LANES}; pass --fanout to opt in"
        );
    }
    if budget_cost != 0.0 {
        anyhow::bail!(
            "--budget-cost is refused: registry blend prices are planning estimates, not an invoice-quality live cost ledger"
        );
    }
    if no_parallel && (budget > 0 || budget_tokens_per_lane > 0 || deadline_secs > 0) {
        anyhow::bail!(
            "--no-parallel cannot enforce live budgets or deadlines; remove it or run the guarded parallel swarm"
        );
    }

    let task_type = router::classify(task);
    if n > router::REGISTRY.len() {
        anyhow::bail!(
            "--n {n} exceeds the {} registered models",
            router::REGISTRY.len()
        );
    }
    let budget_requested = budget > 0 || budget_tokens_per_lane > 0;
    let swarm = if budget_requested {
        // A hard budget needs a full usage stream for every selected lane.
        // Route to the metered subset rather than selecting broad lanes and
        // then pretending their missing provider data is zero usage.
        let metered = router::REGISTRY
            .iter()
            .filter(|model| ats::runtime_has_complete_usage(model.runtime))
            .collect::<Vec<_>>();
        if n > metered.len() {
            anyhow::bail!(
                "token budget currently supports at most {} fully metered lane(s) ({})",
                metered.len(),
                metered
                    .iter()
                    .map(|model| model.name)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        metered.into_iter().take(n).collect()
    } else {
        router::pick_swarm(task_type, n)
    };
    if budget > 0 && budget < swarm.len() as u64 {
        anyhow::bail!(
            "--budget {budget} is smaller than the {}/lane swarm; raise it or reduce --n",
            swarm.len()
        );
    }
    if budget_requested {
        if !ats::canonical_ledger_available() {
            anyhow::bail!(
                "token budget refused: agent-token-ledger is unavailable, so AgentMaster cannot verify live usage"
            );
        }
        let incomplete = swarm
            .iter()
            .filter(|model| !ats::runtime_has_complete_usage(model.runtime))
            .map(|model| format!("{} ({})", model.name, model.runtime))
            .collect::<Vec<_>>();
        if !incomplete.is_empty() {
            anyhow::bail!(
                "token budget refused: no complete provider-usage ledger for {}; refusing a fake gate",
                incomplete.join(", ")
            );
        }
    }
    let lane_budget = effective_lane_budget(budget, budget_tokens_per_lane, swarm.len());

    if recall {
        // Recall may contain cross-project context, so it is an explicit
        // operator choice rather than a hidden default side-channel.
        let primed = ats::prime(task);
        let primed_preview = ats::preview(&primed, 360);
        if !primed.is_empty() {
            println!("🧠 synx prime: {primed_preview}");
        } else {
            println!("🧠 synx prime: (empty — brain miss or synx missing, fail-open)");
        }
        if let Some(hit) = ats::swarm_recall(task) {
            println!(
                "🧠 recall: swarm '{swarm}' won with {winner} on '{task}' (tokens={tokens}, cost=${cost:.4})",
                swarm = hit.swarm,
                winner = hit.winner,
                task = hit.task,
                tokens = hit.tokens,
                cost = hit.cost_usd_milli as f64 / 1000.0,
            );
        }
    } else {
        println!("🧠 synx recall: off (pass --recall to opt in)");
    }

    println!(
        "🐝 swarm '{name}' (n={n}, type={task_type}, parallel={parallel})",
        parallel = if no_parallel { "off" } else { "on" }
    );
    println!("   oracle: {oracle}");
    if budget > 0 {
        println!("   total token budget: {budget}");
    }
    if budget_tokens_per_lane > 0 {
        println!("   per-lane token cap: {budget_tokens_per_lane}");
    }
    if let Some(cap) = lane_budget {
        println!("   enforced lane cap: {cap} measured tokens");
    }
    if deadline_secs > 0 {
        println!("   deadline: {deadline_secs}s");
    }
    let hermes_accept_hooks = explicit_env_opt_in(
        std::env::var("AGENTMASTER_HERMES_ACCEPT_HOOKS")
            .ok()
            .as_deref(),
    );
    println!(
        "   Hermes hook trust: {}",
        if hermes_accept_hooks {
            "ENABLED by AGENTMASTER_HERMES_ACCEPT_HOOKS=1 (headless Hermes lanes)"
        } else {
            "off (default; unseen hooks are not auto-approved)"
        }
    );
    // Skill choice belongs to the task, not to a model name. Route once, keep
    // at most one verified path, and never relay the router's raw JSON.
    let routed_skill = ats::route_skill(task);
    println!("   lanes :");
    for (i, m) in swarm.iter().enumerate() {
        let skills_str = routed_skill.as_deref().unwrap_or("(none)");
        println!(
            "     [{i}] {name:<18} {runtime}/{flag}  ${blend:.2}/1M  ({tier})  skills={skills}",
            name = m.name,
            runtime = m.runtime,
            flag = m.model_flag,
            blend = m.blended_cost(),
            tier = m.tier,
            skills = skills_str,
        );
    }
    if dry_run {
        println!("(--dry-run — not spawning)");
        return Ok(());
    }

    // Init the omnigoal so `goal-check` can gate the swarm.
    let s = Store::open(&dir.join("agentmaster.db"))?;
    s.set_goal_omni(name, task, Some(oracle), Some(oracle), budget, None);
    s.log(
        None,
        name,
        "swarm-init",
        &format!(
            "n={n} type={task_type} oracle={oracle} parallel={parallel} per_lane_cap={lane_budget:?} deadline={deadline_secs} recall={recall} hermes_accept_hooks={hermes_accept_hooks}",
            parallel = if no_parallel { "off" } else { "on" },
        ),
    );

    // Legacy sequential path (--no-parallel): keep the old behavior for
    // debugging. Writes capsules + spawns via `assign`, no tokio.
    if no_parallel {
        return swarm_spawn_sequential(
            name,
            task,
            oracle,
            &swarm,
            routed_skill.as_deref(),
            &s,
            dir,
        );
    }

    // Parallel path: build LaneSpecs and hand off to the tokio engine.
    let workdir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut lanes = Vec::with_capacity(swarm.len());
    for (i, m) in swarm.iter().enumerate() {
        // ATS hook 4: ensure per-lane dirs exist (fail-open).
        let _ = ats::ensure_lane_dirs(name, i, m.name);
        // Log the lane-spawn event so the audit trail shows per-lane intent.
        s.log(
            None,
            name,
            "lane-spawn",
            &format!("lane={i} model={} runtime={}", m.name, m.runtime),
        );
        let lane = swarm_engine::LaneSpec {
            lane_index: i,
            swarm_name: name.to_string(),
            model: m,
            task: task.to_string(),
            oracle: oracle.to_string(),
            skill_path: routed_skill.clone(),
            budget_tokens: lane_budget,
            hermes_accept_hooks,
            workdir: workdir.clone(),
        };
        lanes.push(lane);
    }

    let deadline = if deadline_secs > 0 {
        Some(std::time::Duration::from_secs(deadline_secs))
    } else {
        None
    };

    // Build a tokio runtime inline. Ponytail: no global runtime, no async
    // main — we just block on the swarm future.
    let rt = swarm_runtime()?;
    let run = rt.block_on(swarm_engine::run_parallel_swarm(lanes, deadline));

    for lane in &run.lanes {
        if lane.outcome == swarm_engine::LaneOutcome::Passed {
            continue;
        }
        let outcome = lane_outcome(&lane.outcome);
        println!(
            "   lane {} ({}/{}) — {} in {:.1}s, tokens={}",
            lane.lane_index,
            lane.runtime,
            lane.model_name,
            outcome,
            lane.elapsed.as_secs_f64(),
            lane.tokens_used,
        );
        s.log(
            None,
            name,
            "lane-result",
            &format!(
                "lane={} model={} runtime={} outcome={} tokens={} elapsed={:.1}s",
                lane.lane_index,
                lane.model_name,
                lane.runtime,
                outcome,
                lane.tokens_used,
                lane.elapsed.as_secs_f64(),
            ),
        );
    }

    // Report the result.
    match &run.winner {
        Some(w) => {
            println!(
                "\n🏆 winner: lane {} ({}/{}) — PASSED in {:.1}s, tokens={}",
                w.lane_index,
                w.runtime,
                w.model_name,
                w.elapsed.as_secs_f64(),
                w.tokens_used,
            );
            if let Some(ws) = &w.workspace_ref {
                println!("   workspace: {ws}");
            }
            // Remember only when the operator opted into cross-project recall.
            let cost = swarm
                .iter()
                .find(|m| m.name == w.model_name)
                .map(|m| m.blended_cost() * (w.tokens_used as f64 / 1_000_000.0))
                .unwrap_or(0.0);
            if recall {
                let remembered =
                    ats::swarm_remember(name, task, &w.model_name, oracle, w.tokens_used, cost);
                if remembered {
                    println!("   🧠 remembered to synapse brain");
                }
            }
            // Log the goal-close event so the TUI rehydrates the outcome.
            s.log(
                None,
                name,
                "swarm-win",
                &format!(
                    "lane={} model={} runtime={} tokens={} elapsed={:.1}s cost=${cost:.4}",
                    w.lane_index,
                    w.model_name,
                    w.runtime,
                    w.tokens_used,
                    w.elapsed.as_secs_f64(),
                ),
            );
            // Close the omnigoal with a summary.
            s.goal_close(
                name,
                &format!(
                    "swarm won by lane {} ({}, {} tokens, {:.1}s)",
                    w.lane_index,
                    w.model_name,
                    w.tokens_used,
                    w.elapsed.as_secs_f64(),
                ),
            );
        }
        None => {
            println!("\n❌ no lane passed the oracle within the deadline/budget");
            s.log(None, name, "swarm-fail", "no lane passed");
        }
    }

    println!("\nNext: `agentmaster goal-check {name}` to re-run the oracle.");
    Ok(())
}

fn lane_outcome(outcome: &crate::swarm::LaneOutcome) -> String {
    match outcome {
        crate::swarm::LaneOutcome::Passed => "PASSED".to_string(),
        crate::swarm::LaneOutcome::BudgetExceeded => "BUDGET-EXCEEDED".to_string(),
        crate::swarm::LaneOutcome::SpawnFailed(detail) => {
            format!("SPAWN-FAILED: {detail}")
        }
        crate::swarm::LaneOutcome::ChildExited(detail) => {
            format!("CHILD-EXITED: {detail}")
        }
        crate::swarm::LaneOutcome::Timeout => "TIMEOUT".to_string(),
        crate::swarm::LaneOutcome::Pruned => "PRUNED".to_string(),
    }
}

fn explicit_env_opt_in(value: Option<&str>) -> bool {
    value == Some("1")
}

fn effective_lane_budget(total: u64, per_lane: u64, lanes: usize) -> Option<usize> {
    let from_total = (total > 0 && lanes > 0)
        .then(|| total / lanes as u64)
        .and_then(|cap| usize::try_from(cap).ok());
    let direct = (per_lane > 0)
        .then(|| usize::try_from(per_lane).ok())
        .flatten();
    match (from_total, direct) {
        (Some(total_cap), Some(lane_cap)) => Some(total_cap.min(lane_cap)),
        (Some(cap), None) | (None, Some(cap)) => Some(cap),
        (None, None) => None,
    }
}

fn swarm_runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_time()
        .enable_io()
        .build()
        .map_err(|e| anyhow::anyhow!("tokio runtime init failed: {e}"))
}

/// Legacy sequential spawn path. Kept for `--no-parallel` debugging.
fn swarm_spawn_sequential(
    name: &str,
    task: &str,
    oracle: &str,
    swarm: &[&'static crate::router::ModelSpec],
    skill_path: Option<&str>,
    s: &Store,
    dir: &Path,
) -> Result<()> {
    let capsule_dir = dir.join("capsules");
    std::fs::create_dir_all(&capsule_dir).ok();
    for (i, m) in swarm.iter().enumerate() {
        let capsule_path = capsule_dir.join(format!("{name}-lane{i}-{}.md", m.name));
        let lane = crate::swarm::LaneSpec {
            lane_index: i,
            swarm_name: name.to_string(),
            model: m,
            task: task.to_string(),
            oracle: oracle.to_string(),
            skill_path: skill_path.map(str::to_string),
            budget_tokens: None,
            hermes_accept_hooks: false,
            workdir: dir.to_path_buf(),
        };
        let capsule = crate::swarm::build_capsule(&lane);
        std::fs::write(&capsule_path, &capsule)?;
        s.goal_spawn(name, Some(&capsule_path.to_string_lossy()), Some(m.name));
        let launch_task = crate::swarm::capsule_launch_prompt(&capsule_path);
        let lane_name = format!("{name}-lane{i}-{}", m.name);
        let ws = assign_with_model(
            m.runtime,
            Some(m.model_flag),
            &launch_task,
            Some(&lane_name),
            dir,
        )?;
        println!("   [{i}] spawned → {ws}");
    }
    println!("\nNext: `agentmaster goal-check {name}` to run the oracle.");
    println!(
        "First lane to PASS wins; stop only a known lane with `agentmaster focus <lane>` + Ctrl-C."
    );
    Ok(())
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

    #[test]
    fn total_budget_never_allocates_more_than_the_total() {
        assert_eq!(effective_lane_budget(9, 0, 3), Some(3));
        assert_eq!(effective_lane_budget(9, 2, 3), Some(2));
        assert_eq!(effective_lane_budget(0, 2, 3), Some(2));
        assert_eq!(effective_lane_budget(0, 0, 3), None);
    }

    #[test]
    fn lane_failure_text_preserves_diagnostic_detail() {
        assert_eq!(
            lane_outcome(&crate::swarm::LaneOutcome::ChildExited(
                "exit status: 7 before oracle passed".into()
            )),
            "CHILD-EXITED: exit status: 7 before oracle passed"
        );
        assert_eq!(
            lane_outcome(&crate::swarm::LaneOutcome::SpawnFailed(
                "binary missing".into()
            )),
            "SPAWN-FAILED: binary missing"
        );
    }

    #[test]
    fn hermes_hook_trust_env_requires_exact_one() {
        assert!(!explicit_env_opt_in(None));
        assert!(!explicit_env_opt_in(Some("0")));
        assert!(!explicit_env_opt_in(Some("true")));
        assert!(explicit_env_opt_in(Some("1")));
    }

    #[test]
    fn swarm_runtime_enables_process_io() {
        let rt = swarm_runtime().expect("runtime");
        let output = rt.block_on(async {
            tokio::process::Command::new("sh")
                .args(["-c", "printf io-ok"])
                .output()
                .await
        });
        assert_eq!(output.expect("process io").stdout, b"io-ok");
    }
}

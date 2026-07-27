//! agentmaster — one session to see and steer all your agents.
//! Kanban TUI over native PTYs. Zero orchestration tax: you see where, what,
//! which agents, which processes — at a glance. Coordination is on-disk (SQLite),
//! not paid for in LLM tokens.

mod app;
mod ats;
mod backend;
mod cli;
mod fleet;
mod obs;
mod orch;
mod peek;
mod pty;
mod router;
mod runtime;
mod state;
mod store;
mod swarm;
mod ui;
mod voice;

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "agentmaster",
    version,
    about = "See and steer every agent from one session — Kanban TUI, zero orchestration tax"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Launch the Kanban TUI (default)
    Tui,
    /// Print recent events from the audit log (headless observability)
    Events {
        #[arg(short, long, default_value_t = 50)]
        n: i64,
    },
    /// Health check: data dir, sqlite, pty, runtimes. --verbose probes each
    /// found runtime's `--version` so you see not just presence but what's on PATH.
    /// --costs reads the audit log and shows per-runtime send/assign counts (no
    /// token spend — agentmaster never sees provider billing; use `agent-token-ledger`
    /// for that). Ponytail: reuses the existing log, no new accounting state.
    Doctor {
        #[arg(long)]
        verbose: bool,
        #[arg(long)]
        costs: bool,
    },
    /// Search ALL sessions by content (passthrough to session-restore)
    Find { query: String },
    /// Grouped session dashboard (project → sessions, cost, topic)
    Dash {
        #[arg(long)]
        all: bool,
        query: Option<String>,
    },
    /// Cold-start a distilled session into a seeded cmux workspace (or --here)
    Start {
        id: String,
        #[arg(long)]
        here: bool,
        #[arg(long)]
        focus: bool,
    },
    /// Zero-tax peek: read a session's last user/assistant/next off its transcript.
    /// --tail N prints the last N raw events instead of the structural digest.
    Peek {
        id_or_path: String,
        #[arg(long, default_value_t = 0)]
        tail: usize,
    },
    /// Fan-out: spawn one seeded cmux workspace per task in a tasks file
    Batch {
        file: PathBuf,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        model: Option<String>,
    },
    /// List every discoverable live agent (tmux panes + cmux workspaces)
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// Steer one live agent: send a line to a `workspace:NN` or tmux `sess:win.pane`.
    /// `--goal <name>` also pins a goal on that agent name (same as `goal` subcommand,
    /// but combined so you can steer + pin in one shot).
    Send {
        /// agent ref: cmux `workspace:NN` or tmux `session:window.pane`
        target: String,
        /// the message (remaining words are joined)
        #[arg(trailing_var_arg = true, required = true)]
        message: Vec<String>,
        #[arg(long)]
        goal: Option<String>,
        #[arg(long)]
        dod: Option<String>,
        /// wait for the agent's next assistant response (timeout in seconds)
        #[arg(long, default_value_t = 0)]
        wait: u64,
    },
    /// Broadcast a line to every live agent (cmux always; tmux with --tmux)
    Broadcast {
        #[arg(trailing_var_arg = true, required = true)]
        message: Vec<String>,
        #[arg(long)]
        tmux: bool,
        /// only cmux agents currently waiting on input
        #[arg(long)]
        needs_input: bool,
    },
    /// Pin a goal on an agent name; the TUI rehydrates it. Use `::` to add a
    /// definition-of-done:  goal <name> ship it :: all tests pass
    /// Optional omnigoal lifecycle flags turn this into an oracle-gated goal:
    ///   goal <name> ship it :: tests pass --oracle "cargo test -q" --budget 20000
    Goal {
        name: String,
        #[arg(trailing_var_arg = true, required = true)]
        goal: Vec<String>,
        /// machine-checkable oracle: a shell command that exits 0 iff the goal is done
        #[arg(long)]
        oracle: Option<String>,
        /// token budget cap for this goal (0 = unlimited)
        #[arg(long, default_value_t = 0)]
        budget: u64,
        /// ISO deadline (e.g. 2026-12-31T23:59:59+01:00)
        #[arg(long)]
        deadline: Option<String>,
    },
    /// Async fan-out: spawn one fresh cmux workspace seeded with the task and
    /// return immediately. Fire-and-forget like CAO's `assign` primitive — the
    /// worker is expected to work independently. Log the callback-ref so the
    /// supervisor can `peek` it later.
    Assign {
        /// runtime to launch: claude, codex, hermes, ggcoder, aider, opencode,
        /// gemini, cline, shell
        runtime: String,
        /// the task (remaining words are joined)
        #[arg(trailing_var_arg = true, required = true)]
        task: Vec<String>,
        /// explicit workspace name (default: slug from task text)
        #[arg(long)]
        name: Option<String>,
    },
    /// List every stored goal + its progress
    Goals {
        #[arg(long)]
        json: bool,
    },
    /// Omnigoal: run the goal's oracle, identify the bottleneck, record a try.
    /// Exit 0 if the oracle passes (goal done), exit 1 if it fails, exit 2 if
    /// the 3-try cap is hit (goal auto-abandoned).
    GoalCheck { name: String },
    /// Omnigoal: mark a goal done with a closing summary. The summary is
    /// persisted to the goal row + the event stream, so the next session can
    /// rehydrate the outcome without replaying the transcript.
    GoalClose {
        name: String,
        #[arg(long)]
        summary: Option<String>,
        #[arg(long)]
        abandon: bool,
    },
    /// Omnigoal: register a subagent against this goal with a bounded capsule +
    /// skill. Ponytail: we only log the event — the capsule file lives on disk
    /// and is referenced by path, never inlined into the goal row.
    GoalSpawn {
        name: String,
        #[arg(long)]
        capsule: Option<String>,
        #[arg(long)]
        skill: Option<String>,
    },
    /// Jump to an agent's live tab: switch the cmux UI / tmux client to it
    Focus {
        /// agent ref: cmux `workspace:NN` or tmux `session:window.pane`
        target: String,
    },
    /// List all models in the registry with pricing, tier, strengths/weaknesses.
    /// --tier filters to open_weight|mid_tier|frontier_closed. --json for
    /// machine-readable output (for agent-token-saver integration).
    Models {
        #[arg(long)]
        tier: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Auto-pick the cheapest model that's strong at the task type, then
    /// `assign` it. The task text is classified by keywords (shell/projection,
    /// code gen, review, planning, research, long-context, creative, general).
    /// Prints the classified type + picked model + runtime, then spawns.
    /// `--dry-run` shows the pick without spawning.
    Auto {
        /// the task (remaining words are joined)
        #[arg(trailing_var_arg = true, required = true)]
        task: Vec<String>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Oracle-gated swarm: spawn N diverse agents on the same goal in
    /// parallel, first oracle PASS wins. Diversifies across runtimes + tiers
    /// so a single model's failure mode doesn't block convergence. Loser lanes
    /// receive a targeted stop signal and remain inspectable. `--n 3` is the
    /// safe default; wider fan-out requires `--fanout`.
    /// `--oracle "<cmd>"` is required (shell command, exit 0 = done).
    /// Token budgets are enabled only for fully metered lanes; otherwise the
    /// command fails rather than claiming an unverifiable cap. `--budget-cost`
    /// is rejected because static router prices are not a live cost ledger.
    /// `--deadline-secs S` caps swarm wall-clock time. `--no-parallel` falls
    /// back to the legacy sequential spawn (for debugging).
    /// `--dry-run` shows the lane plan without spawning.
    Swarm {
        name: String,
        #[arg(long, short = 'n', default_value_t = 3)]
        n: usize,
        /// Permit more than the safe default of three lanes.
        #[arg(long)]
        fanout: bool,
        #[arg(long)]
        oracle: String,
        /// Total measured token budget across the swarm (requires full metering).
        #[arg(long, default_value_t = 0)]
        budget: u64,
        /// Per-lane measured token budget cap (requires full metering).
        #[arg(long, default_value_t = 0)]
        budget_tokens: u64,
        /// Deprecated: refused because registry prices are planning estimates.
        #[arg(long, default_value_t = 0.0)]
        budget_cost: f64,
        /// Swarm-wide wall-clock deadline in seconds (0 = unlimited).
        #[arg(long, default_value_t = 0)]
        deadline_secs: u64,
        /// Fall back to the legacy sequential spawn (no tokio parallelism).
        #[arg(long)]
        no_parallel: bool,
        /// Opt into Synapse recall and persistence for this swarm.
        #[arg(long)]
        recall: bool,
        /// the task (remaining words are joined)
        #[arg(trailing_var_arg = true, required = true)]
        task: Vec<String>,
        #[arg(long)]
        dry_run: bool,
    },
}

fn home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

fn data_dir() -> PathBuf {
    let mut p = home();
    p.push(".agentmaster");
    p
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let dir = data_dir();
    std::fs::create_dir_all(&dir).ok();
    // Observability: structured JSONL log to ~/.agentmaster/logs/, kept alive via guard.
    let _guard = obs::init_tracing(&dir.join("logs"));

    match cli.cmd.unwrap_or(Cmd::Tui) {
        Cmd::Tui => app::run(dir)?,
        Cmd::Events { n } => {
            let s = store::Store::open(&dir.join("agentmaster.db"))?;
            for (ts, name, kind, msg) in s.recent(n).into_iter().rev() {
                println!("{ts}  {kind:<8} {name:<16} {msg}");
            }
        }
        Cmd::Doctor { verbose, costs } => doctor(&dir, verbose, costs)?,
        Cmd::Find { query } => orch::find(&query)?,
        Cmd::Dash { all, query } => orch::dash(all, query.as_deref())?,
        Cmd::Start { id, here, focus } => orch::start(&id, here, focus)?,
        Cmd::Peek { id_or_path, tail } => match peek::resolve(&id_or_path) {
            Some(path) => {
                if tail > 0 {
                    for line in peek::tail(&path, tail) {
                        println!("{line}");
                    }
                } else {
                    let d = peek::digest(&path, 220);
                    println!("transcript: {}", path.display());
                    if !d.last_user.is_empty() {
                        println!("🧑 {}", d.last_user);
                    }
                    if !d.last_assistant.is_empty() {
                        println!("🤖 {}", d.last_assistant);
                    }
                    if !d.next_action.is_empty() {
                        println!("🎯 next: {}", d.next_action);
                    }
                }
            }
            None => eprintln!("no transcript found for: {id_or_path}"),
        },
        Cmd::Batch { file, yes, model } => {
            let tasks = orch::load_tasks(&file)?;
            orch::batch(&tasks, model.as_deref(), yes)?;
        }
        Cmd::Ls { json } => cli::list(json)?,
        Cmd::Send {
            target,
            message,
            goal,
            dod,
            wait,
        } => {
            let msg = message.join(" ");
            if wait > 0 {
                cli::send_wait(&target, &msg, &dir, wait)?;
            } else {
                cli::send(&target, &msg, &dir)?;
            }
            if let Some(name) = goal {
                let dod_str = dod.filter(|s| !s.trim().is_empty());
                cli::goal_set(&name, &msg, dod_str.as_deref(), &dir)?;
            }
        }
        Cmd::Assign {
            runtime,
            task,
            name,
        } => {
            let t = task.join(" ");
            cli::assign(&runtime, &t, name.as_deref(), &dir)?;
        }
        Cmd::Broadcast {
            message,
            tmux,
            needs_input,
        } => cli::broadcast(&message.join(" "), tmux, needs_input, &dir)?,
        Cmd::Goal {
            name,
            goal,
            oracle,
            budget,
            deadline,
        } => {
            let joined = goal.join(" ");
            let (g, dod) = match joined.split_once("::") {
                Some((g, d)) if !d.trim().is_empty() => (g.trim(), Some(d.trim())),
                _ => (joined.trim(), None),
            };
            if oracle.is_some() || budget > 0 || deadline.is_some() {
                cli::goal_init(
                    &name,
                    g,
                    dod,
                    oracle.as_deref(),
                    budget,
                    deadline.as_deref(),
                    &dir,
                )?
            } else {
                cli::goal_set(&name, g, dod, &dir)?
            }
        }
        Cmd::Goals { json } => cli::goals_list(json, &dir)?,
        Cmd::GoalCheck { name } => cli::goal_check(&name, &dir)?,
        Cmd::GoalClose {
            name,
            summary,
            abandon,
        } => cli::goal_close(&name, summary.as_deref(), abandon, &dir)?,
        Cmd::GoalSpawn {
            name,
            capsule,
            skill,
        } => cli::goal_spawn(&name, capsule.as_deref(), skill.as_deref(), &dir)?,
        Cmd::Focus { target } => cli::focus(&target)?,
        Cmd::Models { tier, json } => cli::models_list(tier.as_deref(), json)?,
        Cmd::Auto {
            task,
            name,
            dry_run,
        } => {
            let t = task.join(" ");
            cli::auto_spawn(&t, name.as_deref(), dry_run, &dir)?
        }
        Cmd::Swarm {
            name,
            n,
            fanout,
            oracle,
            budget,
            budget_tokens,
            budget_cost,
            deadline_secs,
            no_parallel,
            recall,
            task,
            dry_run,
        } => {
            let t = task.join(" ");
            cli::swarm_spawn(
                cli::SwarmRequest {
                    name: &name,
                    task: &t,
                    oracle: &oracle,
                    n,
                    fanout,
                    budget,
                    budget_tokens_per_lane: budget_tokens,
                    budget_cost,
                    deadline_secs,
                    no_parallel,
                    recall,
                    dry_run,
                },
                &dir,
            )?;
        }
    }
    Ok(())
}

fn doctor(dir: &Path, verbose: bool, costs: bool) -> anyhow::Result<()> {
    println!("agentmaster doctor\n");
    println!(
        "  data dir      : {} ({})",
        dir.display(),
        if dir.exists() { "ok" } else { "missing" }
    );
    let db = dir.join("agentmaster.db");
    match store::Store::open(&db) {
        Ok(_) => println!("  sqlite        : ok ({})", db.display()),
        Err(e) => println!("  sqlite        : FAIL {e}"),
    }
    let pty_ok = portable_pty::native_pty_system()
        .openpty(portable_pty::PtySize {
            rows: 1,
            cols: 1,
            pixel_width: 0,
            pixel_height: 0,
        })
        .is_ok();
    println!("  pty           : {}", if pty_ok { "ok" } else { "FAIL" });
    if backend::tmux_available() {
        let n = backend::list_panes().len();
        println!("  tmux          : ok ({n} pane(s) discoverable)");
    } else {
        println!("  tmux          : absent (native PTY still works)");
    }
    if backend::cmux_available() {
        let n = backend::list_cmux().len();
        println!("  cmux          : ok ({n} workspace(s) discoverable)");
    } else {
        println!("  cmux          : absent (run agentmaster inside cmux to steer it)");
    }
    for bin in runtime::KNOWN_RUNTIMES.iter().copied() {
        let found = std::process::Command::new("which")
            .arg(bin)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !found {
            println!("  runtime {bin:<8}: absent",);
            continue;
        }
        let label = if verbose {
            // Probe `--version` then `-V` — different CLIs pick different flags.
            let v = std::process::Command::new(bin)
                .arg("--version")
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.lines().next().unwrap_or("").trim().to_string())
                .or_else(|| {
                    std::process::Command::new(bin)
                        .arg("-V")
                        .output()
                        .ok()
                        .filter(|o| o.status.success())
                        .and_then(|o| String::from_utf8(o.stdout).ok())
                        .map(|s| s.lines().next().unwrap_or("").trim().to_string())
                })
                .unwrap_or_else(|| "(no --version)".into());
            format!("found  {v}")
        } else {
            "found".to_string()
        };
        println!("  runtime {bin:<8}: {label}");
    }
    if costs {
        println!("\n  costs         : per-runtime activity from audit log");
        if let Ok(s) = store::Store::open(&db) {
            let mut counts: std::collections::HashMap<String, (usize, usize)> =
                std::collections::HashMap::new();
            for (_ts, name, kind, _msg) in s.recent(500) {
                let e = counts.entry(name).or_insert((0, 0));
                if kind == "send" {
                    e.0 += 1;
                } else if kind == "assign" {
                    e.1 += 1;
                }
            }
            if counts.is_empty() {
                println!("    (no send/assign events yet)");
            } else {
                let mut rows: Vec<_> = counts.into_iter().collect();
                rows.sort_by_key(|row| std::cmp::Reverse(row.1.0 + row.1.1));
                for (name, (sends, assigns)) in rows {
                    println!("    {:<24} sends={sends:<3} assigns={assigns}", name);
                }
                println!("    (token spend lives in agent-token-ledger, not here)");
            }
        }
    }
    println!(
        "\n  logs          : {}/logs/agentmaster.jsonl",
        dir.display()
    );
    Ok(())
}

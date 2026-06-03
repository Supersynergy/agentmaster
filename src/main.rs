//! agentmaster — one session to see and steer all your agents.
//! Kanban TUI over native PTYs. Zero orchestration tax: you see where, what,
//! which agents, which processes — at a glance. Coordination is on-disk (SQLite),
//! not paid for in LLM tokens.

mod app;
mod backend;
mod cli;
mod fleet;
mod obs;
mod orch;
mod peek;
mod pty;
mod runtime;
mod state;
mod store;
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
    /// Health check: data dir, sqlite, pty, runtimes
    Doctor,
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
    /// Zero-tax peek: read a session's last user/assistant/next off its transcript
    Peek { id_or_path: String },
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
    /// Steer one live agent: send a line to a `workspace:NN` or tmux `sess:win.pane`
    Send {
        /// agent ref: cmux `workspace:NN` or tmux `session:window.pane`
        target: String,
        /// the message (remaining words are joined)
        #[arg(trailing_var_arg = true, required = true)]
        message: Vec<String>,
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
    Goal {
        name: String,
        #[arg(trailing_var_arg = true, required = true)]
        goal: Vec<String>,
    },
    /// List every stored goal + its progress
    Goals {
        #[arg(long)]
        json: bool,
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
        Cmd::Doctor => doctor(&dir)?,
        Cmd::Find { query } => orch::find(&query)?,
        Cmd::Dash { all, query } => orch::dash(all, query.as_deref())?,
        Cmd::Start { id, here, focus } => orch::start(&id, here, focus)?,
        Cmd::Peek { id_or_path } => match peek::resolve(&id_or_path) {
            Some(path) => {
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
            None => eprintln!("no transcript found for: {id_or_path}"),
        },
        Cmd::Batch { file, yes, model } => {
            let tasks = orch::load_tasks(&file)?;
            orch::batch(&tasks, model.as_deref(), yes)?;
        }
        Cmd::Ls { json } => cli::list(json)?,
        Cmd::Send { target, message } => cli::send(&target, &message.join(" "), &dir)?,
        Cmd::Broadcast {
            message,
            tmux,
            needs_input,
        } => cli::broadcast(&message.join(" "), tmux, needs_input, &dir)?,
        Cmd::Goal { name, goal } => {
            let joined = goal.join(" ");
            let (g, dod) = match joined.split_once("::") {
                Some((g, d)) if !d.trim().is_empty() => (g.trim(), Some(d.trim())),
                _ => (joined.trim(), None),
            };
            cli::goal_set(&name, g, dod, &dir)?
        }
        Cmd::Goals { json } => cli::goals_list(json, &dir)?,
    }
    Ok(())
}

fn doctor(dir: &Path) -> anyhow::Result<()> {
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
    for bin in ["claude", "codex", "bash", "zsh"] {
        let found = std::process::Command::new("which")
            .arg(bin)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        println!(
            "  runtime {bin:<6}: {}",
            if found { "found" } else { "absent" }
        );
    }
    println!(
        "\n  logs          : {}/logs/agentmaster.jsonl",
        dir.display()
    );
    Ok(())
}

//! agentmaster — one session to see and steer all your agents.
//! Kanban TUI over native PTYs. Zero orchestration tax: you see where, what,
//! which agents, which processes — at a glance. Coordination is on-disk (SQLite),
//! not paid for in LLM tokens.

mod app;
mod backend;
mod fleet;
mod obs;
mod pty;
mod runtime;
mod state;
mod store;
mod ui;

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

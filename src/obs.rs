//! Observability: structured JSONL tracing to disk + lightweight host metrics
//! for the header. Every meaningful action also emits a `tracing` event, so the
//! JSONL file is a machine-readable mirror of the SQLite audit log.

use std::path::Path;

use sysinfo::System;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

/// Initialise JSON tracing into `dir/agentmaster.jsonl` (daily rotation).
/// Returns a guard that must be held for the program's lifetime to flush logs.
/// Filter via the `AGENTMASTER_LOG` env var (defaults to `info`).
pub fn init_tracing(dir: &Path) -> Option<WorkerGuard> {
    std::fs::create_dir_all(dir).ok()?;
    let appender = tracing_appender::rolling::daily(dir, "agentmaster.jsonl");
    let (nb, guard) = tracing_appender::non_blocking(appender);
    let filter =
        EnvFilter::try_from_env("AGENTMASTER_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let layer = fmt::layer().json().with_target(false).with_writer(nb);
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(layer)
        .try_init();
    Some(guard)
}

/// Host-level metrics shown in the header. Kept tiny on purpose — global CPU and
/// memory only, refreshed on the housekeeping tick.
pub struct Metrics {
    sys: System,
}

impl Metrics {
    pub fn new() -> Self {
        let mut sys = System::new();
        sys.refresh_memory();
        sys.refresh_cpu_usage();
        Metrics { sys }
    }

    pub fn refresh(&mut self) {
        self.sys.refresh_cpu_usage();
        self.sys.refresh_memory();
    }

    pub fn cpu(&self) -> f32 {
        self.sys.global_cpu_usage()
    }

    pub fn mem_used_gb(&self) -> f64 {
        self.sys.used_memory() as f64 / 1_073_741_824.0
    }

    pub fn mem_total_gb(&self) -> f64 {
        self.sys.total_memory() as f64 / 1_073_741_824.0
    }
}

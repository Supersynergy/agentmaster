//! Parallel swarm spawn engine.
//!
//! The ponytail upgrade to `cli::swarm_spawn`: instead of spawning N lanes
//! sequentially with `assign` and waiting for each to hit the oracle, we
//! spawn all N lanes in parallel under a Tokio runtime, poll each lane's
//! oracle in the background, and stop the moment the first lane passes.
//! Loser lanes receive a targeted stop signal; their cmux workspaces stay
//! intact for inspection and are never closed by the controller.
//!
//! Fail-open: if `tokio` isn't available (it always is — it's a Cargo dep),
//! or if a lane's spawn fails, we log + continue with the remaining lanes.
//! No transcript sharing between lanes — each lane is an independent worker
//! with its own capsule, ledger, and share dir.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::process::Command as AsyncCommand;
use tokio::sync::Mutex;

use crate::ats;
use crate::router::{self, ModelSpec};

/// Configuration for a single swarm lane.
#[derive(Debug, Clone)]
pub struct LaneSpec {
    pub lane_index: usize,
    pub swarm_name: String,
    pub model: &'static ModelSpec,
    pub task: String,
    pub oracle: String,
    /// One optional, verified task-specific skill to read by path.
    pub skill_path: Option<String>,
    /// Optional budget cap in tokens. Lane is killed if it exceeds this.
    pub budget_tokens: Option<usize>,
    /// Working directory for the lane's cmux workspace.
    pub workdir: PathBuf,
}

/// Result of running a lane to completion (oracle pass, budget hit, or error).
#[derive(Debug, Clone)]
pub struct LaneResult {
    pub lane_index: usize,
    pub model_name: String,
    pub runtime: String,
    pub workspace_ref: Option<String>,
    pub outcome: LaneOutcome,
    pub elapsed: Duration,
    pub tokens_used: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneOutcome {
    /// Oracle returned exit 0 — the lane won.
    Passed,
    /// Lane exceeded its token budget before passing.
    BudgetExceeded,
    /// Spawn failed (cmux error, runtime missing, etc).
    SpawnFailed(String),
    /// Lane was still running when the swarm-wide deadline hit.
    Timeout,
    /// Lane was pruned because another lane already passed.
    Pruned,
}

/// Run a swarm of N lanes in parallel. Returns the winner (first PASS) if any.
/// Ponytail: one Tokio runtime, N tasks, first PASS wins, rest are pruned.
pub async fn run_parallel_swarm(
    lanes: Vec<LaneSpec>,
    deadline: Option<Duration>,
) -> Option<LaneResult> {
    if lanes.is_empty() {
        return None;
    }
    let winner: Arc<Mutex<Option<LaneResult>>> = Arc::new(Mutex::new(None));
    let mut handles = Vec::with_capacity(lanes.len());

    for lane in lanes {
        let winner_clone = Arc::clone(&winner);
        let handle = tokio::spawn(async move { run_lane(lane, winner_clone, deadline).await });
        handles.push(handle);
    }

    // Wait for all lanes to finish (either they pass, fail, or get pruned).
    let mut last_result: Option<LaneResult> = None;
    for h in handles {
        match h.await {
            Ok(r) => {
                if r.outcome == LaneOutcome::Passed {
                    last_result = Some(r.clone());
                }
            }
            Err(_) => { /* tokio join error — ignore, keep other lanes */ }
        }
    }
    // If any lane passed, the winner mutex holds it.
    let guard = winner.lock().await;
    if let Some(w) = guard.as_ref() {
        return Some(w.clone());
    }
    last_result
}

/// Run a single lane: spawn the cmux workspace, poll the oracle until it
/// passes or the deadline hits. If `winner` is already Some when we finish,
/// we mark ourselves as Pruned.
async fn run_lane(
    lane: LaneSpec,
    winner: Arc<Mutex<Option<LaneResult>>>,
    swarm_deadline: Option<Duration>,
) -> LaneResult {
    let start = Instant::now();
    let model_name = lane.model.name.to_string();
    let runtime = lane.model.runtime.to_string();

    // Fail-open: if cmux isn't installed, return SpawnFailed immediately.
    // The swarm continues with the remaining lanes.
    let cmux_bin = std::env::var("CMUX_BIN").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        format!("{home}/.local/bin/cmux")
    });
    if !Path::new(&cmux_bin).exists() {
        return LaneResult {
            lane_index: lane.lane_index,
            model_name,
            runtime,
            workspace_ref: None,
            outcome: LaneOutcome::SpawnFailed(format!("cmux not found at {cmux_bin}")),
            elapsed: start.elapsed(),
            tokens_used: 0,
        };
    }

    // Ensure per-lane dirs exist (fail-open: ignore errors).
    let _ = ats::ensure_lane_dirs(&lane.swarm_name, lane.lane_index, &model_name);

    // Write the capsule (bounded prompt) for this lane.
    let capsule = build_capsule(&lane);
    let capsule_file = ats::capsule_path(&lane.swarm_name, lane.lane_index, &model_name);
    let _ = std::fs::write(&capsule_file, &capsule);

    // Set up per-lane env vars for kimi-worker isolation.
    let share_dir = ats::share_dir(&lane.swarm_name, lane.lane_index);
    let ledger_file = ats::ledger_path(&lane.swarm_name, lane.lane_index, &model_name);
    let evidence_file = ats::evidence_path(&lane.swarm_name, lane.lane_index, &model_name);

    // Spawn the cmux workspace asynchronously.
    let slug = format!("{}-lane{}-{}", lane.swarm_name, lane.lane_index, model_name);
    let workdir_str = lane.workdir.to_string_lossy().to_string();
    // The capsule has the actual task, oracle, evidence contract and tool
    // route. Passing only `lane.task` silently discarded it for every runtime.
    let launch_prompt = capsule_launch_prompt(&capsule_file);
    let spec = crate::runtime::resolve_with_model(
        &runtime,
        Some(&launch_prompt),
        Some(lane.model.model_flag),
    );
    let mut launch = vec![spec.program];
    launch.extend(spec.args);
    let launch_cmd = launch.join(" ");

    let spawn_result = AsyncCommand::new(&cmux_bin)
        .args([
            "new-workspace",
            "--name",
            &slug,
            "--cwd",
            &workdir_str,
            "--command",
            &launch_cmd,
        ])
        .env("KIMI_WORKER_SHARE_DIR", &share_dir)
        .env("KIMI_WORKER_USAGE_OUT", &ledger_file)
        .env("KIMI_WORKER_EVIDENCE", &evidence_file)
        .env("KIMI_WORKER_RETRIES", "3")
        .env(
            "KIMI_WORKER_LIVE_USAGE",
            if lane.budget_tokens.is_some() {
                "1"
            } else {
                "0"
            },
        )
        .output()
        .await;

    let workspace_ref = match spawn_result {
        Ok(out) if out.status.success() => {
            let first = String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .unwrap_or("spawned")
                .to_string();
            first
                .split_whitespace()
                .find(|w| w.starts_with("workspace:"))
                .unwrap_or(&first)
                .to_string()
        }
        Ok(out) => {
            return LaneResult {
                lane_index: lane.lane_index,
                model_name,
                runtime,
                workspace_ref: None,
                outcome: LaneOutcome::SpawnFailed(format!(
                    "cmux spawn failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )),
                elapsed: start.elapsed(),
                tokens_used: 0,
            };
        }
        Err(e) => {
            return LaneResult {
                lane_index: lane.lane_index,
                model_name,
                runtime,
                workspace_ref: None,
                outcome: LaneOutcome::SpawnFailed(format!("cmux spawn error: {e}")),
                elapsed: start.elapsed(),
                tokens_used: 0,
            };
        }
    };

    // Poll the oracle until it passes, the budget is exceeded, or the
    // swarm-wide deadline hits. Ponytail: 5-second poll interval, no
    // backoff, no jitter — good enough for a first cut.
    let poll_interval = Duration::from_secs(5);
    let deadline_instant = swarm_deadline.map(|d| start + d);

    loop {
        // Check swarm-wide deadline.
        if let Some(dl) = deadline_instant
            && Instant::now() >= dl
        {
            signal_lane_stop(&cmux_bin, &workspace_ref, "swarm deadline reached").await;
            return LaneResult {
                lane_index: lane.lane_index,
                model_name,
                runtime,
                workspace_ref: Some(workspace_ref),
                outcome: LaneOutcome::Timeout,
                elapsed: start.elapsed(),
                tokens_used: read_lane_tokens(&ledger_file),
            };
        }
        // Check if another lane already won — if so, prune ourselves.
        {
            let guard = winner.lock().await;
            if guard.is_some() {
                drop(guard);
                signal_lane_stop(&cmux_bin, &workspace_ref, "another lane passed the oracle").await;
                return LaneResult {
                    lane_index: lane.lane_index,
                    model_name,
                    runtime,
                    workspace_ref: Some(workspace_ref),
                    outcome: LaneOutcome::Pruned,
                    elapsed: start.elapsed(),
                    tokens_used: read_lane_tokens(&ledger_file),
                };
            }
        }
        // Check token budget.
        if let Some(cap) = lane.budget_tokens {
            let used = read_lane_tokens(&ledger_file);
            if used >= cap {
                signal_lane_stop(&cmux_bin, &workspace_ref, "measured token budget exceeded").await;
                return LaneResult {
                    lane_index: lane.lane_index,
                    model_name,
                    runtime,
                    workspace_ref: Some(workspace_ref),
                    outcome: LaneOutcome::BudgetExceeded,
                    elapsed: start.elapsed(),
                    tokens_used: used,
                };
            }
        }
        // Run the oracle in a subshell.
        let oracle_status = tokio::process::Command::new("sh")
            .args(["-c", &lane.oracle])
            .output()
            .await;
        if let Ok(out) = oracle_status
            && out.status.success()
        {
            // We won! Acquire the winner mutex — if someone beat us, prune.
            let mut guard = winner.lock().await;
            if guard.is_some() {
                drop(guard);
                signal_lane_stop(&cmux_bin, &workspace_ref, "another lane passed the oracle").await;
                return LaneResult {
                    lane_index: lane.lane_index,
                    model_name,
                    runtime,
                    workspace_ref: Some(workspace_ref),
                    outcome: LaneOutcome::Pruned,
                    elapsed: start.elapsed(),
                    tokens_used: read_lane_tokens(&ledger_file),
                };
            }
            let result = LaneResult {
                lane_index: lane.lane_index,
                model_name: model_name.clone(),
                runtime: runtime.clone(),
                workspace_ref: Some(workspace_ref.clone()),
                outcome: LaneOutcome::Passed,
                elapsed: start.elapsed(),
                tokens_used: read_lane_tokens(&ledger_file),
            };
            *guard = Some(result.clone());
            return result;
        }
        // Sleep before next poll. If the swarm deadline is closer than
        // the poll interval, sleep only until the deadline.
        let sleep_dur = match deadline_instant {
            Some(dl) => {
                let remaining = dl.saturating_duration_since(Instant::now());
                remaining.min(poll_interval)
            }
            None => poll_interval,
        };
        if sleep_dur.is_zero() {
            continue;
        }
        tokio::time::sleep(sleep_dur).await;
    }
}

/// Read the lane's token usage from its JSONL ledger. Fail-open: returns 0
/// if the file is missing or unparseable.
fn read_lane_tokens(ledger: &Path) -> usize {
    let (i, o) = ats::read_ledger_tokens(ledger);
    i + o
}

/// Stop only the worker process in a workspace we spawned. Deliberately do
/// not call `close-workspace`: cmux can tear down a user's surrounding
/// session, while a retained workspace is useful for evidence and recovery.
async fn signal_lane_stop(cmux_bin: &str, workspace_ref: &str, reason: &str) {
    let message =
        format!("Stop now: {reason}. Do not continue work; return only the capsule result.");
    let _ = AsyncCommand::new(cmux_bin)
        .args(["send", "--workspace", workspace_ref, &message])
        .output()
        .await;
    let _ = AsyncCommand::new(cmux_bin)
        .args(["send-key", "--workspace", workspace_ref, "ctrl+c"])
        .output()
        .await;
}

/// Build a bounded capsule (300-700 tokens) for a lane. The capsule is the
/// ONLY thing the lane sees about the task — no transcript sharing.
const MAX_CAPSULE_BYTES: usize = 2_800;

pub(crate) fn build_capsule(lane: &LaneSpec) -> String {
    let fixed = render_capsule(lane, "");
    let task_budget = MAX_CAPSULE_BYTES.saturating_sub(fixed.len());
    let task = truncate_utf8(&lane.task, task_budget);
    truncate_utf8(&render_capsule(lane, task), MAX_CAPSULE_BYTES).to_string()
}

fn render_capsule(lane: &LaneSpec, task: &str) -> String {
    let model = lane.model;
    let task_type = router::classify(&lane.task);
    let tool_route = tool_route(&lane.task);
    let skill_route = lane
        .skill_path
        .as_deref()
        .map(|path| {
            format!(
                "Read only the routed task skill at `{path}` before work; apply only its relevant instructions."
            )
        })
        .unwrap_or_else(|| "No task-specific skill was selected.".to_string());
    format!(
        "# Swarm capsule: {swarm} lane {idx} ({model_name})\n\
         \n\
         ## Task\n\
         {task}\n\
         \n\
         ## Oracle (PASS condition)\n\
         `{oracle}`\n\
         \n\
         ## Runtime\n\
         - runtime: `{runtime}`\n\
         - model: `{model_flag}`\n\
         - tier: {tier}\n\
         - context_window: {ctx}\n\
         - task_type: {task_type}\n\
         \n\
         ## Constraints\n\
         - Work independently. Do NOT share transcripts with other lanes.\n\
         - Write evidence of your work to `$KIMI_WORKER_EVIDENCE`.\n\
         - Token usage is logged to `$KIMI_WORKER_USAGE_OUT`.\n\
         - Exit 0 only when the oracle passes. Exit 75 to trigger a retry.\n\
         - Return at most 500 tokens: STATUS: PASS|FAIL|BLOCKED; EVIDENCE: path or command+exit; HANDOFF: none|question.\n\
         \n\
         ## Tool route\n\
         {tool_route}\n\
         \n\
         ## Skill route\n\
         {skill_route}\n\
         \n\
         ## Strengths for this lane\n\
         {strengths}\n\
         \n\
         ## Weaknesses (beware)\n\
         {weaknesses}\n\
         ",
        swarm = lane.swarm_name,
        idx = lane.lane_index,
        model_name = model.name,
        task = task,
        oracle = lane.oracle,
        runtime = model.runtime,
        model_flag = model.model_flag,
        tier = model.tier,
        ctx = model.context_window,
        task_type = task_type,
        tool_route = tool_route,
        skill_route = skill_route,
        strengths = model.strengths.join(", "),
        weaknesses = model.weaknesses.join(", "),
    )
}

fn truncate_utf8(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// One deterministic route is enough for a worker capsule. The controller
/// remains responsible for approval and escalation; no global catalog leaks in.
fn tool_route(task: &str) -> &'static str {
    let lower = task.to_lowercase();
    if ["caller", "callee", "impact", "dependency", "graph"]
        .iter()
        .any(|needle| lower.contains(needle))
    {
        "Start `synx ground`; use `graphify query` only with existing graphify-out/graph.json."
    } else if ["log", "stack trace", "stderr", "stdout", "noisy output"]
        .iter()
        .any(|needle| lower.contains(needle))
    {
        "Bound the command first; use `rtk` for supported projections and keep raw output by path."
    } else if [
        "file", "source", "code", "function", "test", "bug", "repo", "import", "symbol",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        "Start exact `rg`; use scoped `tilth --budget 4000` only for structural symbols/imports."
    } else if [
        "fresh",
        "latest",
        "current",
        "documentation",
        "docs",
        "research",
        "api",
        "package",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        "Use bounded primary-source `superweb` before a version, API, price, or policy claim."
    } else {
        "Use the smallest deterministic local check first; escalate only with evidence."
    }
}

pub(crate) fn capsule_launch_prompt(capsule_file: &Path) -> String {
    format!(
        "Read and follow the bounded swarm capsule at {}. Return only its required result.",
        capsule_file.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::REGISTRY;

    fn make_lane(idx: usize, model: &'static ModelSpec) -> LaneSpec {
        LaneSpec {
            lane_index: idx,
            swarm_name: "testswarm".into(),
            model,
            task: "run cargo test".into(),
            oracle: "true".into(),
            skill_path: None,
            budget_tokens: None,
            workdir: std::env::temp_dir(),
        }
    }

    #[test]
    fn capsule_is_bounded_and_contains_required_sections() {
        let lane = make_lane(0, &REGISTRY[0]);
        let capsule = build_capsule(&lane);
        assert!(capsule.contains("# Swarm capsule:"));
        assert!(capsule.contains("## Task"));
        assert!(capsule.contains("## Oracle (PASS condition)"));
        assert!(capsule.contains("## Runtime"));
        assert!(capsule.contains("## Constraints"));
        assert!(capsule.contains("## Tool route"));
        assert!(capsule.contains("## Skill route"));
        assert!(capsule.contains("Do NOT share transcripts"));
        assert!(capsule.contains("Return at most 500 tokens"));
        assert!(
            capsule.len() <= MAX_CAPSULE_BYTES,
            "capsule too long: {} bytes",
            capsule.len()
        );
    }

    #[test]
    fn capsule_includes_model_strengths_and_weaknesses() {
        let lane = make_lane(1, &REGISTRY[1]);
        let capsule = build_capsule(&lane);
        assert!(capsule.contains("Strengths for this lane"));
        assert!(capsule.contains("Weaknesses (beware)"));
    }

    #[test]
    fn capsule_passes_only_the_selected_skill_path() {
        let mut lane = make_lane(0, &REGISTRY[0]);
        lane.skill_path = Some("/tmp/selected-skill/SKILL.md".into());
        let capsule = build_capsule(&lane);
        assert!(capsule.contains("/tmp/selected-skill/SKILL.md"));
        assert!(capsule.contains("Read only the routed task skill"));
    }

    #[test]
    fn capsule_hard_caps_long_utf8_task_without_invalid_text() {
        let mut lane = make_lane(0, &REGISTRY[0]);
        lane.task = "ö".repeat(8_000);
        let capsule = build_capsule(&lane);
        assert!(capsule.len() <= MAX_CAPSULE_BYTES);
        assert!(std::str::from_utf8(capsule.as_bytes()).is_ok());
    }

    #[test]
    fn tool_route_prefers_impact_over_generic_source() {
        assert!(
            tool_route("find caller impact for this source").starts_with("Start `synx ground`")
        );
    }

    #[test]
    fn launch_prompt_references_the_written_capsule() {
        let prompt = capsule_launch_prompt(Path::new("/tmp/worker-capsule.md"));
        assert!(prompt.contains("/tmp/worker-capsule.md"));
        assert!(prompt.contains("required result"));
    }

    #[tokio::test]
    async fn run_parallel_swarm_returns_none_for_empty_input() {
        let result = run_parallel_swarm(Vec::new(), None).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn run_lane_returns_spawn_failed_when_cmux_missing() {
        // Point CMUX_BIN at a nonexistent path so the lane fails fast.
        // SAFETY: test is single-threaded; env var mutation is safe here.
        unsafe {
            std::env::set_var("CMUX_BIN", "/nonexistent/cmux-binary-xyz");
        }
        let lane = make_lane(0, &REGISTRY[0]);
        let winner: Arc<Mutex<Option<LaneResult>>> = Arc::new(Mutex::new(None));
        let result = run_lane(lane, winner, Some(Duration::from_secs(1))).await;
        assert!(matches!(result.outcome, LaneOutcome::SpawnFailed(_)));
        // SAFETY: same single-threaded context.
        unsafe {
            std::env::remove_var("CMUX_BIN");
        }
    }

    #[test]
    fn lane_outcome_equality_works() {
        assert_eq!(LaneOutcome::Passed, LaneOutcome::Passed);
        assert_ne!(LaneOutcome::Passed, LaneOutcome::Pruned);
        assert_ne!(LaneOutcome::Passed, LaneOutcome::BudgetExceeded);
    }
}

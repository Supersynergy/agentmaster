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
use std::process::Stdio;
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
    /// Explicit operator opt-in for Hermes to approve unseen hooks headlessly.
    pub hermes_accept_hooks: bool,
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

#[derive(Debug, Clone)]
pub struct SwarmRunResult {
    pub winner: Option<LaneResult>,
    pub lanes: Vec<LaneResult>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneOutcome {
    /// Oracle returned exit 0 — the lane won.
    Passed,
    /// Lane exceeded its token budget before passing.
    BudgetExceeded,
    /// Spawn failed (cmux error, runtime missing, etc).
    SpawnFailed(String),
    /// A directly spawned runtime exited before its oracle passed.
    ChildExited(String),
    /// Lane was still running when the swarm-wide deadline hit.
    Timeout,
    /// Lane was pruned because another lane already passed.
    Pruned,
}

/// Run a swarm of N lanes in parallel. Returns the winner and every lane
/// result, so the caller can report failures without losing the successful
/// lane.
pub async fn run_parallel_swarm(
    lanes: Vec<LaneSpec>,
    deadline: Option<Duration>,
) -> SwarmRunResult {
    let cmux_bin = std::env::var("CMUX_BIN").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        format!("{home}/.local/bin/cmux")
    });
    run_parallel_swarm_with_cmux(lanes, deadline, &cmux_bin).await
}

async fn run_parallel_swarm_with_cmux(
    lanes: Vec<LaneSpec>,
    deadline: Option<Duration>,
    cmux_bin: &str,
) -> SwarmRunResult {
    if lanes.is_empty() {
        return SwarmRunResult {
            winner: None,
            lanes: Vec::new(),
        };
    }
    let winner: Arc<Mutex<Option<LaneResult>>> = Arc::new(Mutex::new(None));
    let mut handles = Vec::with_capacity(lanes.len());

    for lane in lanes {
        let join_failure = LaneResult {
            lane_index: lane.lane_index,
            model_name: lane.model.name.to_string(),
            runtime: lane.model.runtime.to_string(),
            workspace_ref: None,
            outcome: LaneOutcome::SpawnFailed("lane task join failed".to_string()),
            elapsed: Duration::ZERO,
            tokens_used: 0,
        };
        let winner_clone = Arc::clone(&winner);
        let cmux_bin = cmux_bin.to_string();
        let handle = tokio::spawn(async move {
            run_lane_with_cmux(lane, winner_clone, deadline, &cmux_bin).await
        });
        handles.push((join_failure, handle));
    }

    let mut results = Vec::with_capacity(handles.len());
    for (mut join_failure, handle) in handles {
        match handle.await {
            Ok(result) => results.push(result),
            Err(error) => {
                join_failure.outcome =
                    LaneOutcome::SpawnFailed(format!("lane task join failed: {error}"));
                results.push(join_failure);
            }
        }
    }
    results.sort_by_key(|result| result.lane_index);
    let winner = winner.lock().await.clone();
    SwarmRunResult {
        winner,
        lanes: results,
    }
}

/// Run a single lane: spawn the cmux workspace, poll the oracle until it
/// passes or the deadline hits. If `winner` is already Some when we finish,
/// we mark ourselves as Pruned.
async fn run_lane_with_cmux(
    lane: LaneSpec,
    winner: Arc<Mutex<Option<LaneResult>>>,
    swarm_deadline: Option<Duration>,
    cmux_bin: &str,
) -> LaneResult {
    let start = Instant::now();
    let model_name = lane.model.name.to_string();
    let runtime = lane.model.runtime.to_string();

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

    let slug = format!("{}-lane{}-{}", lane.swarm_name, lane.lane_index, model_name);
    let workdir_str = lane.workdir.to_string_lossy().to_string();
    let launch_prompt = capsule_launch_prompt(&capsule_file);
    let spec = crate::runtime::resolve_headless_with_model(
        &runtime,
        &launch_prompt,
        Some(lane.model.model_flag),
        (runtime == "hermes").then_some(ledger_file.as_path()),
        lane.hermes_accept_hooks,
    );
    let mut direct_child = None;
    let cmux_status = cmux_ping(cmux_bin).await;
    let workspace_ref = if cmux_status.is_ok() {
        let launch_cmd = shell_command(&spec);
        let mut command = AsyncCommand::new(cmux_bin);
        let command = lane_command(
            &mut command,
            &share_dir,
            &ledger_file,
            &evidence_file,
            lane.budget_tokens,
        );
        match command
            .args([
                "new-workspace",
                "--name",
                &slug,
                "--cwd",
                &workdir_str,
                "--command",
                &launch_cmd,
            ])
            .output()
            .await
        {
            Ok(out) if out.status.success() => {
                let first = String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .next()
                    .unwrap_or("spawned")
                    .to_string();
                Some(
                    first
                        .split_whitespace()
                        .find(|word| word.starts_with("workspace:"))
                        .unwrap_or(&first)
                        .to_string(),
                )
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
            Err(error) => {
                return LaneResult {
                    lane_index: lane.lane_index,
                    model_name,
                    runtime,
                    workspace_ref: None,
                    outcome: LaneOutcome::SpawnFailed(format!("cmux spawn error: {error}")),
                    elapsed: start.elapsed(),
                    tokens_used: 0,
                };
            }
        }
    } else {
        let cmux_error = cmux_status.expect_err("checked above");
        let mut command = AsyncCommand::new(&spec.program);
        command
            .args(&spec.args)
            .current_dir(&lane.workdir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let command = lane_command(
            &mut command,
            &share_dir,
            &ledger_file,
            &evidence_file,
            lane.budget_tokens,
        );
        match command.spawn() {
            Ok(child) => {
                direct_child = Some(child);
                None
            }
            Err(error) => {
                return LaneResult {
                    lane_index: lane.lane_index,
                    model_name,
                    runtime,
                    workspace_ref: None,
                    outcome: LaneOutcome::SpawnFailed(format!(
                        "cmux unavailable ({cmux_error}); direct runtime spawn failed: {error}"
                    )),
                    elapsed: start.elapsed(),
                    tokens_used: 0,
                };
            }
        }
    };

    // Poll the oracle until it passes, the budget is exceeded, or the
    // swarm-wide deadline hits. Ponytail: 5-second poll interval, no
    // backoff, no jitter — good enough for a first cut.
    let poll_interval = if direct_child.is_some() {
        Duration::from_millis(50)
    } else {
        Duration::from_secs(5)
    };
    let deadline_instant = swarm_deadline.map(|d| start + d);

    loop {
        // Check swarm-wide deadline.
        if let Some(dl) = deadline_instant
            && Instant::now() >= dl
        {
            stop_lane(
                cmux_bin,
                workspace_ref.as_deref(),
                &mut direct_child,
                "swarm deadline reached",
            )
            .await;
            return LaneResult {
                lane_index: lane.lane_index,
                model_name,
                runtime,
                workspace_ref,
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
                stop_lane(
                    cmux_bin,
                    workspace_ref.as_deref(),
                    &mut direct_child,
                    "another lane passed the oracle",
                )
                .await;
                return LaneResult {
                    lane_index: lane.lane_index,
                    model_name,
                    runtime,
                    workspace_ref,
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
                stop_lane(
                    cmux_bin,
                    workspace_ref.as_deref(),
                    &mut direct_child,
                    "measured token budget exceeded",
                )
                .await;
                return LaneResult {
                    lane_index: lane.lane_index,
                    model_name,
                    runtime,
                    workspace_ref,
                    outcome: LaneOutcome::BudgetExceeded,
                    elapsed: start.elapsed(),
                    tokens_used: used,
                };
            }
        }
        let child_exit = match direct_child.as_mut().map(|child| child.try_wait()) {
            Some(Ok(status)) => status,
            Some(Err(error)) => {
                return LaneResult {
                    lane_index: lane.lane_index,
                    model_name,
                    runtime,
                    workspace_ref,
                    outcome: LaneOutcome::ChildExited(format!(
                        "failed to inspect direct runtime: {error}"
                    )),
                    elapsed: start.elapsed(),
                    tokens_used: read_lane_tokens(&ledger_file),
                };
            }
            None => None,
        };
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
                stop_lane(
                    cmux_bin,
                    workspace_ref.as_deref(),
                    &mut direct_child,
                    "another lane passed the oracle",
                )
                .await;
                return LaneResult {
                    lane_index: lane.lane_index,
                    model_name,
                    runtime,
                    workspace_ref,
                    outcome: LaneOutcome::Pruned,
                    elapsed: start.elapsed(),
                    tokens_used: read_lane_tokens(&ledger_file),
                };
            }
            let result = LaneResult {
                lane_index: lane.lane_index,
                model_name: model_name.clone(),
                runtime: runtime.clone(),
                workspace_ref: workspace_ref.clone(),
                outcome: LaneOutcome::Passed,
                elapsed: start.elapsed(),
                tokens_used: read_lane_tokens(&ledger_file),
            };
            *guard = Some(result.clone());
            stop_direct_child(&mut direct_child).await;
            return result;
        }
        if let Some(status) = child_exit {
            return LaneResult {
                lane_index: lane.lane_index,
                model_name,
                runtime,
                workspace_ref,
                outcome: LaneOutcome::ChildExited(format!(
                    "direct runtime exited with {status} before oracle passed"
                )),
                elapsed: start.elapsed(),
                tokens_used: read_lane_tokens(&ledger_file),
            };
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

fn lane_command<'a>(
    command: &'a mut AsyncCommand,
    share_dir: &Path,
    ledger_file: &Path,
    evidence_file: &Path,
    budget_tokens: Option<usize>,
) -> &'a mut AsyncCommand {
    command
        .env("KIMI_WORKER_SHARE_DIR", share_dir)
        .env("KIMI_WORKER_USAGE_OUT", ledger_file)
        .env("KIMI_WORKER_EVIDENCE", evidence_file)
        .env("KIMI_WORKER_RETRIES", "3")
        .env(
            "KIMI_WORKER_LIVE_USAGE",
            if budget_tokens.is_some() { "1" } else { "0" },
        )
}

async fn cmux_ping(cmux_bin: &str) -> Result<(), String> {
    if !Path::new(cmux_bin).exists() {
        return Err(format!("not found at {cmux_bin}"));
    }
    let mut command = AsyncCommand::new(cmux_bin);
    command.arg("ping");
    match tokio::time::timeout(Duration::from_secs(2), command.output()).await {
        Err(_) => Err("ping timed out after 2s".to_string()),
        Ok(Ok(output)) if output.status.success() => Ok(()),
        Ok(Ok(output)) => {
            let detail = String::from_utf8_lossy(&output.stderr);
            let detail = detail.trim();
            if detail.is_empty() {
                Err(format!("ping exited with {}", output.status))
            } else {
                Err(format!("ping failed: {detail}"))
            }
        }
        Ok(Err(error)) => Err(format!("ping error: {error}")),
    }
}

fn shell_command(spec: &crate::runtime::RuntimeSpec) -> String {
    std::iter::once(&spec.program)
        .chain(spec.args.iter())
        .map(|word| shell_word(word))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_word(word: &str) -> String {
    if !word.is_empty()
        && word
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_+-./:=".contains(&byte))
    {
        return word.to_string();
    }
    format!("'{}'", word.replace('\'', "'\"'\"'"))
}

async fn stop_lane(
    cmux_bin: &str,
    workspace_ref: Option<&str>,
    direct_child: &mut Option<tokio::process::Child>,
    reason: &str,
) {
    if let Some(workspace_ref) = workspace_ref {
        signal_lane_stop(cmux_bin, workspace_ref, reason).await;
    }
    stop_direct_child(direct_child).await;
}

async fn stop_direct_child(child: &mut Option<tokio::process::Child>) {
    let Some(mut child) = child.take() else {
        return;
    };
    let _ = child.start_kill();
    let _ = child.wait().await;
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
    use crate::router::{REGISTRY, TaskType, Tier};

    static FAKE_PASS_MODEL: ModelSpec = ModelSpec {
        name: "fake-pass",
        runtime: "/usr/bin/true",
        model_flag: "unused",
        vendor: "test",
        tier: Tier::OpenWeight,
        input_price: 0.0,
        output_price: 0.0,
        context_window: 1,
        strengths: &[],
        weaknesses: &[],
        best_for: &[TaskType::Verification],
    };

    static FAKE_FAIL_MODEL: ModelSpec = ModelSpec {
        name: "fake-fail",
        runtime: "/usr/bin/false",
        model_flag: "unused",
        vendor: "test",
        tier: Tier::OpenWeight,
        input_price: 0.0,
        output_price: 0.0,
        context_window: 1,
        strengths: &[],
        weaknesses: &[],
        best_for: &[TaskType::Verification],
    };

    fn make_lane(idx: usize, model: &'static ModelSpec) -> LaneSpec {
        LaneSpec {
            lane_index: idx,
            swarm_name: "testswarm".into(),
            model,
            task: "run cargo test".into(),
            oracle: "true".into(),
            skill_path: None,
            budget_tokens: None,
            hermes_accept_hooks: false,
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
        assert!(result.winner.is_none());
        assert!(result.lanes.is_empty());
    }

    #[tokio::test]
    async fn unreachable_cmux_falls_back_to_direct_fake_runtime() {
        let mut lane = make_lane(0, &FAKE_PASS_MODEL);
        lane.oracle = "true".into();
        let winner: Arc<Mutex<Option<LaneResult>>> = Arc::new(Mutex::new(None));
        let result =
            run_lane_with_cmux(lane, winner, Some(Duration::from_secs(1)), "/usr/bin/false").await;
        assert_eq!(result.outcome, LaneOutcome::Passed);
        assert!(result.workspace_ref.is_none());
    }

    #[tokio::test]
    async fn direct_child_exit_before_oracle_is_diagnostic() {
        let mut lane = make_lane(0, &FAKE_FAIL_MODEL);
        lane.oracle = "false".into();
        let winner: Arc<Mutex<Option<LaneResult>>> = Arc::new(Mutex::new(None));
        let result =
            run_lane_with_cmux(lane, winner, Some(Duration::from_secs(1)), "/usr/bin/false").await;
        let LaneOutcome::ChildExited(detail) = result.outcome else {
            panic!("unexpected outcome: {:?}", result.outcome);
        };
        assert!(detail.contains("before oracle passed"));
        assert!(detail.contains("exit status: 1"));
    }

    #[tokio::test]
    async fn parallel_result_keeps_winner_and_all_lane_outcomes() {
        let first = make_lane(0, &FAKE_PASS_MODEL);
        let mut second = make_lane(1, &FAKE_FAIL_MODEL);
        second.oracle = "false".into();
        let result = run_parallel_swarm_with_cmux(
            vec![first, second],
            Some(Duration::from_secs(1)),
            "/usr/bin/false",
        )
        .await;
        assert_eq!(result.lanes.len(), 2);
        assert_eq!(
            result.winner.as_ref().map(|winner| winner.lane_index),
            Some(0)
        );
        assert_eq!(result.lanes[0].outcome, LaneOutcome::Passed);
        assert!(matches!(
            result.lanes[1].outcome,
            LaneOutcome::ChildExited(_) | LaneOutcome::Pruned
        ));
    }

    #[test]
    fn cmux_launch_command_quotes_prompt_as_one_argument() {
        let spec = crate::runtime::RuntimeSpec {
            program: "codex".into(),
            args: vec![
                "exec".into(),
                "read /tmp/capsule; don't expand $HOME".into(),
            ],
        };
        assert_eq!(
            shell_command(&spec),
            "codex exec 'read /tmp/capsule; don'\"'\"'t expand $HOME'"
        );
    }

    #[test]
    fn lane_outcome_equality_works() {
        assert_eq!(LaneOutcome::Passed, LaneOutcome::Passed);
        assert_ne!(LaneOutcome::Passed, LaneOutcome::Pruned);
        assert_ne!(LaneOutcome::Passed, LaneOutcome::BudgetExceeded);
        assert_ne!(
            LaneOutcome::Passed,
            LaneOutcome::ChildExited("exit status: 1".into())
        );
    }
}

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

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use tokio::process::Command as AsyncCommand;
use tokio::sync::Mutex;

use crate::ats;
use crate::router::{self, ModelSpec};

const ORACLE_TIMEOUT: Duration = Duration::from_secs(2);
const PROCESS_STOP_GRACE: Duration = Duration::from_millis(500);
const MAX_DIAGNOSTIC_BYTES: usize = 8 * 1024;
const MAX_DIAGNOSTIC_PREVIEW_CHARS: usize = 240;

struct DirectChild {
    child: tokio::process::Child,
    pgid: Option<u32>,
    stderr_capture: Option<StderrCapture>,
}

#[derive(Default)]
struct CapturedStderr {
    bytes: Vec<u8>,
    total: usize,
}

struct StderrCapture {
    receiver: mpsc::Receiver<CapturedStderr>,
    reader: thread::JoinHandle<()>,
}

struct BoundedCommandOutput {
    status: ExitStatus,
    stdout: CapturedStderr,
    stderr: CapturedStderr,
}

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

    // A PASS that predates this lane cannot be attributed to it. Refuse the
    // lane before launch instead of letting a fast-exiting worker steal a
    // global/shared oracle result left by another lane or an earlier run.
    let preflight_timeout = swarm_deadline.unwrap_or(ORACLE_TIMEOUT).min(ORACLE_TIMEOUT);
    if !preflight_timeout.is_zero()
        && run_oracle_bounded(&lane, &share_dir, &evidence_file, preflight_timeout).await
    {
        return LaneResult {
            lane_index: lane.lane_index,
            model_name,
            runtime,
            workspace_ref: None,
            outcome: LaneOutcome::SpawnFailed(
                "oracle already passed before lane launch; refusing winner attribution".to_string(),
            ),
            elapsed: start.elapsed(),
            tokens_used: read_lane_tokens(&ledger_file),
        };
    }
    if deadline_expired(start, swarm_deadline) {
        return LaneResult {
            lane_index: lane.lane_index,
            model_name,
            runtime,
            workspace_ref: None,
            outcome: LaneOutcome::Timeout,
            elapsed: start.elapsed(),
            tokens_used: read_lane_tokens(&ledger_file),
        };
    }

    let slug = format!("{}-lane{}-{}", lane.swarm_name, lane.lane_index, model_name);
    let workdir_str = lane.workdir.to_string_lossy().to_string();
    let launch_prompt = capsule_launch_prompt(&capsule_file);
    let spec = crate::runtime::resolve_headless_with_model(
        &runtime,
        &launch_prompt,
        Some(lane.model.model_flag),
        (runtime == "hermes").then_some(ledger_file.as_path()),
    );
    let exit_file = share_dir.join("runtime.exit");
    let _ = std::fs::remove_file(&exit_file);
    let mut direct_child = None;
    let cmux_status = cmux_ping(
        cmux_bin,
        remaining_step_timeout(start, swarm_deadline, Duration::from_secs(2)),
    )
    .await;
    let mut cmux_error = cmux_status.err();
    let mut workspace_ref = None;
    if cmux_error.is_none() {
        let launch_cmd = guarded_shell_command(&spec, Some(&exit_file));
        let spawn_timeout = remaining_step_timeout(start, swarm_deadline, Duration::from_secs(5));
        let args = [
            "new-workspace",
            "--name",
            &slug,
            "--cwd",
            &workdir_str,
            "--command",
            &launch_cmd,
        ];
        match run_cmux_bounded(
            cmux_bin,
            &args,
            spawn_timeout,
            Some((&share_dir, &ledger_file, &evidence_file, lane.budget_tokens)),
        )
        .await
        {
            Ok(out) if out.status.success() => {
                let first = bounded_sanitized(&out.stdout.bytes)
                    .lines()
                    .next()
                    .unwrap_or("spawned")
                    .to_string();
                workspace_ref = Some(
                    first
                        .split_whitespace()
                        .find(|word| word.starts_with("workspace:"))
                        .unwrap_or(&first)
                        .to_string(),
                );
            }
            Ok(out) => {
                cmux_error = Some(format!(
                    "spawn exited with {}; stderr={}",
                    out.status,
                    captured_stderr_summary(&out.stderr)
                ));
            }
            Err(error) => {
                cmux_error = Some(format!("spawn error: {error}"));
            }
        }
    }
    if workspace_ref.is_none() {
        if deadline_expired(start, swarm_deadline) {
            return LaneResult {
                lane_index: lane.lane_index,
                model_name,
                runtime,
                workspace_ref: None,
                outcome: LaneOutcome::Timeout,
                elapsed: start.elapsed(),
                tokens_used: read_lane_tokens(&ledger_file),
            };
        }
        match spawn_direct(
            &spec,
            &lane.workdir,
            &share_dir,
            &ledger_file,
            &evidence_file,
            lane.budget_tokens,
        ) {
            Ok(child) => {
                direct_child = Some(child);
            }
            Err(error) => {
                return LaneResult {
                    lane_index: lane.lane_index,
                    model_name,
                    runtime,
                    workspace_ref: None,
                    outcome: LaneOutcome::SpawnFailed(format!(
                        "cmux unavailable ({}); direct runtime spawn failed: {error}",
                        cmux_error.as_deref().unwrap_or("no workspace reference")
                    )),
                    elapsed: start.elapsed(),
                    tokens_used: 0,
                };
            }
        }
    }

    // Poll the oracle until it passes, the budget is exceeded, the runtime
    // exits, or the swarm-wide deadline hits.
    let poll_interval = Duration::from_millis(250);
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
        let cmux_exit_file = workspace_ref.as_ref().map(|_| exit_file.as_path());
        match inspect_lane_exit(&mut direct_child, cmux_exit_file).await {
            Ok(Some(detail)) | Err(detail) => {
                return LaneResult {
                    lane_index: lane.lane_index,
                    model_name,
                    runtime,
                    workspace_ref,
                    outcome: LaneOutcome::ChildExited(detail),
                    elapsed: start.elapsed(),
                    tokens_used: read_lane_tokens(&ledger_file),
                };
            }
            Ok(None) => {}
        }

        let oracle_timeout = deadline_instant
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
            .unwrap_or(ORACLE_TIMEOUT)
            .min(ORACLE_TIMEOUT);
        let oracle_passed = if oracle_timeout.is_zero() {
            false
        } else {
            run_oracle_bounded(&lane, &share_dir, &evidence_file, oracle_timeout).await
        };
        if oracle_passed {
            // A runtime that died while the oracle was executing is not
            // eligible to claim a shared/global PASS produced by another lane.
            match inspect_lane_exit(&mut direct_child, cmux_exit_file).await {
                Ok(Some(detail)) | Err(detail) => {
                    return LaneResult {
                        lane_index: lane.lane_index,
                        model_name,
                        runtime,
                        workspace_ref,
                        outcome: LaneOutcome::ChildExited(format!(
                            "{detail}; oracle PASS was not attributed to an exited lane"
                        )),
                        elapsed: start.elapsed(),
                        tokens_used: read_lane_tokens(&ledger_file),
                    };
                }
                Ok(None) => {}
            }
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

fn deadline_expired(start: Instant, deadline: Option<Duration>) -> bool {
    deadline.is_some_and(|limit| start.elapsed() >= limit)
}

fn remaining_step_timeout(start: Instant, deadline: Option<Duration>, cap: Duration) -> Duration {
    deadline
        .map(|limit| limit.saturating_sub(start.elapsed()))
        .unwrap_or(cap)
        .min(cap)
}

async fn cmux_ping(cmux_bin: &str, timeout: Duration) -> Result<(), String> {
    if timeout.is_zero() {
        return Err("ping skipped: swarm deadline reached".to_string());
    }
    match run_cmux_bounded(cmux_bin, &["ping"], timeout, None).await {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => {
            if output.stderr.bytes.is_empty() {
                Err(format!("ping exited with {}", output.status))
            } else {
                Err(format!(
                    "ping failed: status={} stderr={}",
                    output.status,
                    captured_stderr_summary(&output.stderr)
                ))
            }
        }
        Err(error) => Err(format!("ping error: {error}")),
    }
}

async fn run_cmux_bounded(
    cmux_bin: &str,
    args: &[&str],
    timeout: Duration,
    lane_env: Option<(&Path, &Path, &Path, Option<usize>)>,
) -> Result<BoundedCommandOutput, String> {
    #[cfg(not(unix))]
    {
        let _ = (cmux_bin, args, timeout, lane_env);
        return Err("bounded cmux capture is unsupported on this platform".to_string());
    }

    #[cfg(unix)]
    {
        if timeout.is_zero() {
            return Err("deadline reached before cmux launch".to_string());
        }
        let mut stdout = PrivateCapture::new("stdout").map_err(|error| error.to_string())?;
        let mut stderr = PrivateCapture::new("stderr").map_err(|error| error.to_string())?;
        let stdout_writer = stdout.writer().map_err(|error| error.to_string())?;
        let stderr_writer = stderr.writer().map_err(|error| error.to_string())?;
        // `ulimit -f` units vary between supported Unix shells (512 or 1024
        // bytes). Use the larger unit so either interpretation remains <= cap.
        let file_blocks = MAX_DIAGNOSTIC_BYTES.div_ceil(1024);
        let script = format!("ulimit -f {file_blocks} || exit 125; exec \"$@\"");
        let mut command = AsyncCommand::new("sh");
        command
            .arg("-c")
            .arg(script)
            .arg("agentmaster-cmux")
            .arg(cmux_bin)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout_writer))
            .stderr(Stdio::from(stderr_writer))
            .kill_on_drop(true);
        if let Some((share_dir, ledger_file, evidence_file, budget_tokens)) = lane_env {
            lane_command(
                &mut command,
                share_dir,
                ledger_file,
                evidence_file,
                budget_tokens,
            );
        }
        configure_process_group(&mut command);
        let mut child = command
            .spawn()
            .map_err(|error| format!("spawn failed: {error}"))?;
        let pgid = child.id();
        let status = match tokio::time::timeout(timeout, child.wait()).await {
            // A successful cmux client may intentionally hand work to its
            // server. Do not sweep that group on normal client completion.
            Ok(Ok(status)) => status,
            Ok(Err(error)) => {
                stop_child_process_tree(&mut child, pgid).await;
                return Err(format!("wait failed: {error}"));
            }
            Err(_) => {
                stop_child_process_tree(&mut child, pgid).await;
                return Err(format!("timed out after {:.3}s", timeout.as_secs_f64()));
            }
        };
        Ok(BoundedCommandOutput {
            status,
            stdout: stdout.read().map_err(|error| error.to_string())?,
            stderr: stderr.read().map_err(|error| error.to_string())?,
        })
    }
}

struct PrivateCapture {
    file: File,
}

impl PrivateCapture {
    fn new(label: &str) -> std::io::Result<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::OpenOptionsExt;

        let path =
            std::env::temp_dir().join(format!("agentmaster-cmux-{label}-{}", uuid::Uuid::new_v4()));
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let file = options.open(&path)?;
        if let Err(error) = std::fs::remove_file(&path) {
            drop(file);
            return Err(error);
        }
        Ok(Self { file })
    }

    fn writer(&self) -> std::io::Result<File> {
        self.file.try_clone()
    }

    fn read(&mut self) -> std::io::Result<CapturedStderr> {
        let total = usize::try_from(self.file.metadata()?.len()).unwrap_or(usize::MAX);
        self.file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::with_capacity(total.min(MAX_DIAGNOSTIC_BYTES));
        self.file
            .by_ref()
            .take(MAX_DIAGNOSTIC_BYTES as u64)
            .read_to_end(&mut bytes)?;
        Ok(CapturedStderr { bytes, total })
    }
}

fn shell_command(spec: &crate::runtime::RuntimeSpec) -> String {
    std::iter::once(&spec.program)
        .chain(spec.args.iter())
        .map(|word| shell_word(word))
        .collect::<Vec<_>>()
        .join(" ")
}

fn guarded_shell_command(spec: &crate::runtime::RuntimeSpec, exit_file: Option<&Path>) -> String {
    let script = guarded_shell_script(spec, exit_file);
    format!("sh -c {}", shell_word(&script))
}

fn guarded_shell_script(spec: &crate::runtime::RuntimeSpec, exit_file: Option<&Path>) -> String {
    let command = shell_command(spec);
    let finish = match exit_file {
        Some(path) => format!(
            "printf '%s\\n' \"$status\" > {}",
            shell_word(&path.to_string_lossy())
        ),
        None => ":".to_string(),
    };
    format!("umask 077; {command}; status=$?; {finish}; exit \"$status\"")
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

fn spawn_direct(
    spec: &crate::runtime::RuntimeSpec,
    workdir: &Path,
    share_dir: &Path,
    ledger_file: &Path,
    evidence_file: &Path,
    budget_tokens: Option<usize>,
) -> std::io::Result<DirectChild> {
    let script = guarded_shell_script(spec, None);
    let mut command = AsyncCommand::new("sh");
    command
        .args(["-c", &script])
        .current_dir(workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .kill_on_drop(true);
    configure_process_group(&mut command);
    let stderr_capture = attach_stderr_capture(&mut command)?;
    lane_command(
        &mut command,
        share_dir,
        ledger_file,
        evidence_file,
        budget_tokens,
    );
    let child = command.spawn()?;
    let pgid = child.id();
    Ok(DirectChild {
        child,
        pgid,
        stderr_capture: Some(stderr_capture),
    })
}

#[cfg(unix)]
fn attach_stderr_capture(command: &mut AsyncCommand) -> std::io::Result<StderrCapture> {
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;

    let (mut reader, writer) = UnixStream::pair()?;
    let writer: OwnedFd = writer.into();
    command.stderr(Stdio::from(writer));
    let (sender, receiver) = mpsc::sync_channel(1);
    let reader = thread::spawn(move || {
        let mut captured = CapturedStderr::default();
        let mut chunk = [0_u8; 4096];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    captured.total = captured.total.saturating_add(count);
                    let remaining = MAX_DIAGNOSTIC_BYTES.saturating_sub(captured.bytes.len());
                    captured
                        .bytes
                        .extend_from_slice(&chunk[..count.min(remaining)]);
                }
            }
        }
        let _ = sender.send(captured);
    });
    Ok(StderrCapture { receiver, reader })
}

#[cfg(not(unix))]
fn attach_stderr_capture(command: &mut AsyncCommand) -> std::io::Result<StderrCapture> {
    command.stderr(Stdio::null());
    let (sender, receiver) = mpsc::sync_channel(1);
    let reader = thread::spawn(move || {
        let _ = sender.send(CapturedStderr::default());
    });
    Ok(StderrCapture { receiver, reader })
}

fn configure_process_group(command: &mut AsyncCommand) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
}

async fn inspect_lane_exit(
    direct_child: &mut Option<DirectChild>,
    cmux_exit_file: Option<&Path>,
) -> Result<Option<String>, String> {
    if let Some(direct) = direct_child.as_mut() {
        match direct.child.try_wait() {
            Ok(Some(status)) => {
                let mut direct = direct_child.take().expect("direct child checked above");
                sweep_process_group(direct.pgid).await;
                let stderr = finish_stderr(&mut direct);
                return Ok(Some(format!(
                    "direct runtime exited with {status} before oracle passed; stderr={stderr}"
                )));
            }
            Ok(None) => {}
            Err(error) => return Err(format!("failed to inspect direct runtime: {error}")),
        }
    }
    if let Some(path) = cmux_exit_file
        && path.exists()
    {
        let raw = read_bounded(path, 128).unwrap_or_default();
        let status = bounded_sanitized(&raw);
        return Ok(Some(format!(
            "cmux runtime exited with status {} before oracle passed",
            if status.is_empty() {
                "unknown"
            } else {
                &status
            }
        )));
    }
    Ok(None)
}

fn finish_stderr(direct: &mut DirectChild) -> String {
    let Some(capture) = direct.stderr_capture.take() else {
        return "0B".to_string();
    };
    match capture.receiver.recv_timeout(PROCESS_STOP_GRACE) {
        Ok(captured) => {
            let _ = capture.reader.join();
            captured_stderr_summary(&captured)
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            drop(capture.reader);
            "capture-still-open-after-500ms".to_string()
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => "capture-thread-failed".to_string(),
    }
}

fn captured_stderr_summary(captured: &CapturedStderr) -> String {
    let preview = bounded_sanitized(&captured.bytes);
    let suffix = (captured.total > captured.bytes.len()).then_some(" (truncated)");
    if preview.is_empty() {
        format!("{}B{}", captured.total, suffix.unwrap_or(""))
    } else {
        format!("{}B{} {:?}", captured.total, suffix.unwrap_or(""), preview)
    }
}

fn bounded_sanitized(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_DIAGNOSTIC_BYTES)])
        .chars()
        .filter_map(|ch| match ch {
            '\n' | '\r' | '\t' => Some(' '),
            ch if ch.is_control() => None,
            ch => Some(ch),
        })
        .take(MAX_DIAGNOSTIC_PREVIEW_CHARS)
        .collect::<String>()
        .trim()
        .to_string()
}

fn read_bounded(path: &Path, max_bytes: u64) -> std::io::Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.by_ref().take(max_bytes).read_to_end(&mut bytes)?;
    Ok(bytes)
}

async fn run_oracle_bounded(
    lane: &LaneSpec,
    share_dir: &Path,
    evidence_file: &Path,
    timeout: Duration,
) -> bool {
    let mut command = AsyncCommand::new("sh");
    command
        .args(["-c", &lane.oracle])
        .env("AGENTMASTER_LANE_INDEX", lane.lane_index.to_string())
        .env("AGENTMASTER_LANE_SHARE_DIR", share_dir)
        .env("AGENTMASTER_LANE_EVIDENCE", evidence_file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    configure_process_group(&mut command);
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    let pgid = child.id();
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => {
            sweep_process_group(pgid).await;
            status.success()
        }
        Ok(Err(_)) | Err(_) => {
            stop_child_process_tree(&mut child, pgid).await;
            false
        }
    }
}

async fn stop_lane(
    cmux_bin: &str,
    workspace_ref: Option<&str>,
    direct_child: &mut Option<DirectChild>,
    reason: &str,
) {
    if let Some(workspace_ref) = workspace_ref {
        signal_lane_stop(cmux_bin, workspace_ref, reason).await;
    }
    stop_direct_child(direct_child).await;
}

async fn stop_direct_child(child: &mut Option<DirectChild>) {
    let Some(mut direct) = child.take() else {
        return;
    };
    stop_child_process_tree(&mut direct.child, direct.pgid).await;
    let _ = finish_stderr(&mut direct);
}

async fn stop_child_process_tree(child: &mut tokio::process::Child, pgid: Option<u32>) {
    let deadline = Instant::now() + PROCESS_STOP_GRACE;
    let mut leader_reaped = false;
    #[cfg(unix)]
    if let Some(pgid) = pgid {
        signal_process_group(pgid, "-TERM").await;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if !remaining.is_zero()
            && let Ok(Ok(_)) = tokio::time::timeout(remaining, child.wait()).await
        {
            leader_reaped = true;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if !remaining.is_zero() {
            tokio::time::sleep(remaining).await;
        }
        // Always sweep the independently stored process group after grace:
        // the leader may have exited while a TERM-ignoring descendant lives.
        sweep_process_group(Some(pgid)).await;
    }
    if !leader_reaped {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

async fn sweep_process_group(pgid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pgid) = pgid {
        signal_process_group(pgid, "-KILL").await;
    }
}

#[cfg(unix)]
async fn signal_process_group(pid: u32, signal: &str) {
    let group = format!("-{pid}");
    let mut command = AsyncCommand::new("kill");
    command
        .args([signal, &group])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let _ = tokio::time::timeout(PROCESS_STOP_GRACE, command.status()).await;
}

/// Stop only the worker process in a workspace we spawned. Deliberately do
/// not call `close-workspace`: cmux can tear down a user's surrounding
/// session, while a retained workspace is useful for evidence and recovery.
async fn signal_lane_stop(cmux_bin: &str, workspace_ref: &str, reason: &str) {
    let message =
        format!("Stop now: {reason}. Do not continue work; return only the capsule result.");
    for args in [
        vec!["send", "--workspace", workspace_ref, &message],
        vec!["send-key", "--workspace", workspace_ref, "ctrl+c"],
    ] {
        let mut command = AsyncCommand::new(cmux_bin);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let _ = tokio::time::timeout(PROCESS_STOP_GRACE, command.status()).await;
    }
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

    #[cfg(unix)]
    fn write_test_executable(body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path =
            std::env::temp_dir().join(format!("agentmaster-test-{}.sh", uuid::Uuid::new_v4()));
        std::fs::write(&path, body).expect("write fake executable");
        let mut permissions = std::fs::metadata(&path).expect("metadata").permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&path, permissions).expect("chmod");
        path
    }

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

    static FAKE_LIVE_MODEL: ModelSpec = ModelSpec {
        name: "fake-live",
        runtime: "/usr/bin/yes",
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
        let mut lane = make_lane(0, &FAKE_LIVE_MODEL);
        let marker =
            std::env::temp_dir().join(format!("agentmaster-pass-{}", uuid::Uuid::new_v4()));
        lane.oracle = format!("test -f {}", shell_word(&marker.to_string_lossy()));
        let delayed_marker = marker.clone();
        let writer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            std::fs::write(delayed_marker, b"pass").expect("write delayed marker");
        });
        let winner: Arc<Mutex<Option<LaneResult>>> = Arc::new(Mutex::new(None));
        let result =
            run_lane_with_cmux(lane, winner, Some(Duration::from_secs(1)), "/usr/bin/false").await;
        writer.await.expect("marker task");
        let _ = std::fs::remove_file(marker);
        assert_eq!(result.outcome, LaneOutcome::Passed);
        assert!(result.workspace_ref.is_none());
    }

    #[tokio::test]
    async fn path_resolved_cmux_name_is_attempted() {
        let error = cmux_ping("false", Duration::from_secs(1))
            .await
            .expect_err("false must fail");
        assert!(!error.contains("not found at"), "{error}");
        assert!(
            error.contains("status=") || error.contains("exited"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cmux_spawn_failure_falls_back_to_direct_runtime() {
        let cmux =
            write_test_executable("#!/bin/sh\nif [ \"$1\" = ping ]; then exit 0; fi\nexit 9\n");
        let marker =
            std::env::temp_dir().join(format!("agentmaster-pass-{}", uuid::Uuid::new_v4()));
        let mut lane = make_lane(0, &FAKE_LIVE_MODEL);
        lane.oracle = format!("test -f {}", shell_word(&marker.to_string_lossy()));
        let delayed_marker = marker.clone();
        let writer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            std::fs::write(delayed_marker, b"pass").expect("write delayed marker");
        });
        let winner: Arc<Mutex<Option<LaneResult>>> = Arc::new(Mutex::new(None));
        let result = run_lane_with_cmux(
            lane,
            winner,
            Some(Duration::from_secs(2)),
            &cmux.to_string_lossy(),
        )
        .await;
        writer.await.expect("marker task");
        let _ = std::fs::remove_file(cmux);
        let _ = std::fs::remove_file(marker);
        assert_eq!(result.outcome, LaneOutcome::Passed);
        assert!(result.workspace_ref.is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cmux_runtime_exit_sentinel_prevents_deadline_poll_loop() {
        let cmux = write_test_executable(
            "#!/bin/sh\n\
             if [ \"$1\" = ping ]; then exit 0; fi\n\
             while [ \"$#\" -gt 0 ]; do\n\
               if [ \"$1\" = --command ]; then shift; sh -c \"$1\" >/dev/null 2>&1 & echo workspace:test; exit 0; fi\n\
               shift\n\
             done\n\
             exit 2\n",
        );
        let mut lane = make_lane(0, &FAKE_FAIL_MODEL);
        lane.oracle = "false".into();
        let winner: Arc<Mutex<Option<LaneResult>>> = Arc::new(Mutex::new(None));
        let start = Instant::now();
        let result = run_lane_with_cmux(
            lane,
            winner,
            Some(Duration::from_secs(3)),
            &cmux.to_string_lossy(),
        )
        .await;
        let _ = std::fs::remove_file(cmux);
        assert!(matches!(result.outcome, LaneOutcome::ChildExited(_)));
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn direct_child_exit_before_oracle_is_diagnostic() {
        let mut lane = make_lane(0, &FAKE_FAIL_MODEL);
        lane.oracle = "false".into();
        let winner: Arc<Mutex<Option<LaneResult>>> = Arc::new(Mutex::new(None));
        let result =
            run_lane_with_cmux(lane, winner, Some(Duration::from_secs(2)), "/usr/bin/false").await;
        let LaneOutcome::ChildExited(detail) = result.outcome else {
            panic!("unexpected outcome: {:?}", result.outcome);
        };
        assert!(detail.contains("before oracle passed"));
        assert!(detail.contains("exit status: 1"));
    }

    #[tokio::test]
    async fn preexisting_global_oracle_pass_cannot_be_claimed_by_a_lane() {
        let lane = make_lane(0, &FAKE_FAIL_MODEL);
        let winner: Arc<Mutex<Option<LaneResult>>> = Arc::new(Mutex::new(None));
        let result = run_lane_with_cmux(
            lane,
            Arc::clone(&winner),
            Some(Duration::from_secs(2)),
            "/usr/bin/false",
        )
        .await;
        assert!(
            !matches!(result.outcome, LaneOutcome::Passed),
            "lane claimed preexisting shared oracle: {:?}",
            result.outcome
        );
        assert!(winner.lock().await.is_none());
    }

    #[tokio::test]
    async fn oracle_timeout_is_bounded_by_swarm_deadline() {
        let marker =
            std::env::temp_dir().join(format!("agentmaster-oracle-{}", uuid::Uuid::new_v4()));
        let mut lane = make_lane(0, &FAKE_LIVE_MODEL);
        lane.oracle = format!("sleep 5; touch {}", shell_word(&marker.to_string_lossy()));
        let winner: Arc<Mutex<Option<LaneResult>>> = Arc::new(Mutex::new(None));
        let start = Instant::now();
        let result = run_lane_with_cmux(
            lane,
            winner,
            Some(Duration::from_millis(500)),
            "/usr/bin/false",
        )
        .await;
        assert_eq!(result.outcome, LaneOutcome::Timeout);
        assert!(start.elapsed() < Duration::from_secs(2));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!marker.exists(), "timed-out oracle descendant survived");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stopping_direct_child_kills_its_process_group() {
        let pid_file =
            std::env::temp_dir().join(format!("agentmaster-child-{}", uuid::Uuid::new_v4()));
        let script = format!(
            "(trap '' TERM; while :; do sleep 30; done) & echo $! > {}; wait",
            shell_word(&pid_file.to_string_lossy())
        );
        let spec = crate::runtime::RuntimeSpec {
            program: "sh".into(),
            args: vec!["-c".into(), script],
        };
        let dir = std::env::temp_dir();
        let mut child = Some(
            spawn_direct(&spec, &dir, &dir, &pid_file, &pid_file, None)
                .expect("spawn process tree"),
        );
        for _ in 0..20 {
            if pid_file.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let pid = std::fs::read_to_string(&pid_file)
            .expect("child pid")
            .trim()
            .to_string();
        stop_direct_child(&mut child).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let alive = std::process::Command::new("kill")
            .args(["-0", &pid])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        let _ = std::fs::remove_file(pid_file);
        assert!(
            !alive,
            "TERM-ignoring descendant {pid} survived final KILL sweep"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn normal_direct_leader_exit_sweeps_background_descendant() {
        let pid_file =
            std::env::temp_dir().join(format!("agentmaster-orphan-{}", uuid::Uuid::new_v4()));
        let script = format!(
            "(trap '' HUP TERM; while :; do sleep 30; done) & echo $! > {}; exit 0",
            shell_word(&pid_file.to_string_lossy())
        );
        let spec = crate::runtime::RuntimeSpec {
            program: "sh".into(),
            args: vec!["-c".into(), script],
        };
        let dir = std::env::temp_dir();
        let mut child = Some(
            spawn_direct(&spec, &dir, &dir, &pid_file, &pid_file, None)
                .expect("spawn normal-exit process tree"),
        );
        for _ in 0..50 {
            if pid_file.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let pid = std::fs::read_to_string(&pid_file)
            .expect("background pid")
            .trim()
            .to_string();
        let mut detail = None;
        for _ in 0..50 {
            if let Some(result) = inspect_lane_exit(&mut child, None)
                .await
                .expect("inspect normal exit")
            {
                detail = Some(result);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(detail.is_some(), "direct leader did not report normal exit");
        tokio::time::sleep(Duration::from_millis(50)).await;
        let alive = std::process::Command::new("kill")
            .args(["-0", &pid])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        let _ = std::fs::remove_file(pid_file);
        assert!(!alive, "normal-exit descendant {pid} survived sweep");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn normal_oracle_exit_sweeps_background_descendant() {
        let pid_file =
            std::env::temp_dir().join(format!("agentmaster-oracle-child-{}", uuid::Uuid::new_v4()));
        let mut lane = make_lane(0, &FAKE_LIVE_MODEL);
        lane.oracle = format!(
            "(trap '' HUP TERM; while :; do sleep 30; done) & echo $! > {}; exit 1",
            shell_word(&pid_file.to_string_lossy())
        );
        let passed = run_oracle_bounded(
            &lane,
            &std::env::temp_dir(),
            &pid_file,
            Duration::from_secs(1),
        )
        .await;
        assert!(!passed);
        let pid = std::fs::read_to_string(&pid_file)
            .expect("oracle background pid")
            .trim()
            .to_string();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let alive = std::process::Command::new("kill")
            .args(["-0", &pid])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        let _ = std::fs::remove_file(pid_file);
        assert!(!alive, "normal-exit oracle descendant {pid} survived sweep");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cmux_capture_files_are_hard_bounded() {
        let cmux = write_test_executable("#!/bin/sh\nexec yes 0123456789\n");
        let output = run_cmux_bounded(
            &cmux.to_string_lossy(),
            &["ping"],
            Duration::from_secs(2),
            None,
        )
        .await
        .expect("bounded cmux capture");
        let _ = std::fs::remove_file(cmux);
        assert!(!output.status.success());
        assert!(
            output.stdout.total <= MAX_DIAGNOSTIC_BYTES,
            "stdout file exceeded cap: {}",
            output.stdout.total
        );
        assert!(output.stdout.bytes.len() <= MAX_DIAGNOSTIC_BYTES);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn direct_stderr_is_drained_bounded_and_sanitized() {
        let spec = crate::runtime::RuntimeSpec {
            program: "sh".into(),
            args: vec![
                "-c".into(),
                "printf '\\033[31mprivate\\033[0m\\n' >&2; yes x | head -c 20000 >&2".into(),
            ],
        };
        let dir = std::env::temp_dir();
        let mut direct = spawn_direct(
            &spec,
            &dir,
            &dir,
            &dir.join("usage"),
            &dir.join("evidence"),
            None,
        )
        .expect("spawn stderr writer");
        direct.child.wait().await.expect("wait stderr writer");
        let summary = finish_stderr(&mut direct);
        assert!(summary.contains("(truncated)"), "{summary}");
        assert!(!summary.contains('\u{1b}'), "{summary:?}");
        assert!(
            summary.len() < 400,
            "unbounded diagnostic: {}",
            summary.len()
        );
    }

    #[tokio::test]
    async fn parallel_result_keeps_winner_and_all_lane_outcomes() {
        let marker =
            std::env::temp_dir().join(format!("agentmaster-pass-{}", uuid::Uuid::new_v4()));
        let mut first = make_lane(0, &FAKE_LIVE_MODEL);
        first.oracle = format!("test -f {}", shell_word(&marker.to_string_lossy()));
        let mut second = make_lane(1, &FAKE_FAIL_MODEL);
        second.oracle = "false".into();
        let delayed_marker = marker.clone();
        let writer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            std::fs::write(delayed_marker, b"pass").expect("write delayed marker");
        });
        let result = run_parallel_swarm_with_cmux(
            vec![first, second],
            Some(Duration::from_secs(1)),
            "/usr/bin/false",
        )
        .await;
        writer.await.expect("marker task");
        let _ = std::fs::remove_file(marker);
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

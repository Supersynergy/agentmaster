use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::store::Store;

const MAX_ADAPTER_STDOUT: usize = 1_048_576;
const MAX_STDERR: usize = 65_536;
const MAX_ORACLE_STDOUT: usize = 16_384;
const MAX_USAGE_BYTES: u64 = 65_536;
const ORACLE_TIMEOUT_SECS: u64 = 30;
const CAPABILITY_TIMEOUT_SECS: u64 = 5;
const MAX_WORKERS: usize = 3;
const MAX_PROMPT_BYTES: usize = 1_800;

pub struct EnsembleRequest<'a> {
    pub name: &'a str,
    pub task: &'a str,
    pub oracle: &'a str,
    pub lanes: &'a str,
    pub deadline_secs: u64,
    pub max_tokens: u32,
    pub fresh: bool,
    pub dry_run: bool,
    pub go: bool,
    pub allow_remote: bool,
    pub allow_paid: bool,
}

#[derive(Debug)]
struct LanePolicy {
    remote: bool,
    paid: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AdapterCapability {
    schema_version: u64,
    ask_v2: bool,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    default_max_workers: Option<usize>,
    max_workers: usize,
    #[serde(default)]
    fanout_max_workers: Option<usize>,
    max_result_tokens: u32,
    #[serde(default)]
    max_result_tokens_semantics: Option<String>,
    max_prompt_bytes: usize,
    #[serde(default)]
    tools_available: Option<bool>,
    #[serde(default)]
    capsule_visible_input_tokens_proxy: Option<usize>,
    #[serde(default)]
    route: Option<String>,
    #[serde(default)]
    packet: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AdapterOutput {
    schema: String,
    schema_version: u64,
    status: String,
    exit_code: i32,
    prompt: PromptReference,
    stage: String,
    lanes: AdapterLanes,
    ok: usize,
    total: usize,
    results: Vec<AdapterResult>,
    accounting: UsageOutput,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptReference {
    sha256: String,
    bytes: usize,
    transport: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AdapterLanes {
    requested: String,
    selected: usize,
    cap: usize,
    fanout: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AdapterResult {
    lane: String,
    #[serde(default)]
    model: Option<String>,
    kind: String,
    class: String,
    ok: bool,
    terminal: String,
    ms: u64,
    answer: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    detail: Option<String>,
    #[serde(default)]
    cached: Option<bool>,
    call_started: bool,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    token_count_source: String,
    max_tokens: u32,
    cap_mode: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct UsageOutput {
    schema: String,
    schema_version: u64,
    status: String,
    terminal_records_complete: bool,
    prompt_sha256: String,
    prompt_bytes: usize,
    transport: String,
    stage: String,
    requested_lane_spec: String,
    selected_lane_count: usize,
    lane_cap: usize,
    fanout: bool,
    max_tokens: u32,
    calls_started: usize,
    calls_completed: usize,
    cache_hits: usize,
    input_tokens: TokenAccounting,
    output_tokens: TokenAccounting,
    estimated_cost_usd: (),
    cost_status: String,
    completed: bool,
    failed: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct TokenAccounting {
    reported: u64,
    estimated: u64,
    unknown_calls: usize,
}

struct Captured {
    status: ExitStatus,
    stdout: CapturedStream,
    stderr: CapturedStream,
    timed_out: bool,
}

struct CapturedStream {
    bytes: Vec<u8>,
    total: usize,
    truncated: bool,
}

struct UsageArtifactGuard {
    path: PathBuf,
    validated: bool,
}

impl UsageArtifactGuard {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            validated: false,
        }
    }

    fn keep(&mut self) {
        self.validated = true;
    }
}

impl Drop for UsageArtifactGuard {
    fn drop(&mut self) {
        if !self.validated {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[derive(Serialize)]
struct RunManifest {
    schema_version: u8,
    task_sha256: String,
    task_bytes: usize,
    task_transport: &'static str,
    llmadapter_schema_version: u64,
    llmadapter_max_workers: usize,
    llmadapter_max_result_tokens: u32,
    llmadapter_max_prompt_bytes: usize,
    lanes: String,
    remote: bool,
    paid: bool,
    fresh: bool,
    no_cache: bool,
    requested_max_tokens: u32,
    token_limit_status: &'static str,
    deadline_secs: u64,
    adapter_stdout_limit_bytes: usize,
    usage_artifact: &'static str,
    usage_status: &'static str,
    adapter: Option<ProcessEvidence>,
    adapter_result_status: Option<String>,
    adapter_result_exit_code: Option<i32>,
    adapter_lanes: Vec<LaneEvidence>,
    candidates: Vec<CandidateEvidence>,
    final_status: &'static str,
    total_elapsed_ms: u64,
}

#[derive(Serialize)]
struct ProcessEvidence {
    elapsed_ms: u64,
    exit_code: Option<i32>,
    timed_out: bool,
    stdout_bytes: usize,
    stderr_bytes: usize,
    stdout_truncated: bool,
    stderr_truncated: bool,
}

#[derive(Serialize)]
struct LaneEvidence {
    lane: String,
    model: Option<String>,
    kind: String,
    class: String,
    ok: bool,
    terminal: String,
    elapsed_ms: u64,
    max_tokens: u32,
    cap_mode: String,
    token_count_source: String,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cached: bool,
    call_started: bool,
}

#[derive(Serialize)]
struct CandidateEvidence {
    lane: String,
    answer_sha256: String,
    artifact: String,
    oracle: Option<OracleEvidence>,
    status: &'static str,
}

#[derive(Serialize)]
struct OracleEvidence {
    elapsed_ms: u64,
    exit_code: Option<i32>,
    timed_out: bool,
    stdout_bytes: usize,
    stderr_bytes: usize,
}

struct ManifestGuard {
    path: PathBuf,
    started: Instant,
    document: RunManifest,
}

impl ProcessEvidence {
    fn from_capture(output: &Captured, elapsed: Duration) -> Self {
        Self {
            elapsed_ms: millis(elapsed),
            exit_code: output.status.code(),
            timed_out: output.timed_out,
            stdout_bytes: output.stdout.total,
            stderr_bytes: output.stderr.total,
            stdout_truncated: output.stdout.truncated,
            stderr_truncated: output.stderr.truncated,
        }
    }
}

impl ManifestGuard {
    fn checkpoint(&mut self) -> Result<()> {
        self.document.total_elapsed_ms = millis(self.started.elapsed());
        write_private_json(&self.path, &self.document)
    }

    fn finish(&mut self, status: &'static str) -> Result<()> {
        self.document.final_status = status;
        self.checkpoint()
    }
}

impl Drop for ManifestGuard {
    fn drop(&mut self) {
        if self.document.final_status == "RUNNING" {
            self.document.final_status = "ERROR";
        }
        self.document.total_elapsed_ms = millis(self.started.elapsed());
        let _ = write_private_json(&self.path, &self.document);
    }
}

pub fn run(request: EnsembleRequest<'_>, data_dir: &Path) -> Result<()> {
    validate(&request)?;
    let program = find_on_path("llmadapter")
        .context("llmadapter not found on PATH; install it before using ensemble")?;
    run_with_program(request, data_dir, &program)
}

fn run_with_program(request: EnsembleRequest<'_>, data_dir: &Path, program: &Path) -> Result<()> {
    let policy = validate(&request)?;
    let task_hash = sha256_hex(request.task.as_bytes());
    let task_bytes = request.task.len();
    let no_cache = policy.remote || request.fresh;
    let capability = probe_capability(program, data_dir)?;
    if task_bytes > capability.max_prompt_bytes {
        bail!(
            "task is {task_bytes} bytes; llmadapter v2 capability allows at most {}",
            capability.max_prompt_bytes
        );
    }
    if request.max_tokens > capability.max_result_tokens {
        bail!(
            "requested max tokens {} exceeds llmadapter v2 capability {}",
            request.max_tokens,
            capability.max_result_tokens
        );
    }

    if request.dry_run {
        println!("ensemble dry-run");
        println!("  task sha256 : {task_hash}");
        println!("  task bytes  : {task_bytes}");
        println!("  transport   : private unlinked file descriptor -> stdin");
        println!("  protocol    : llmadapter result schema v2");
        println!("  lanes       : {}", request.lanes);
        println!("  remote      : {}", policy.remote);
        println!("  paid        : {}", policy.paid);
        println!(
            "  token limit : requested ceiling {}; provider enforcement varies",
            request.max_tokens
        );
        println!(
            "  command     : {} ask-v2 --stdin --swarm --lanes {} --max-tokens {} --deadline-secs {} --usage-out <private-path>{}{}{}",
            program.display(),
            request.lanes,
            request.max_tokens,
            request.deadline_secs,
            if policy.remote { " --allow-remote" } else { "" },
            if policy.paid { " --allow-paid" } else { "" },
            if no_cache { " --no-cache" } else { "" }
        );
        return Ok(());
    }

    let safe_name = slug(request.name);
    let run_dir = create_run_dir(data_dir, &safe_name)?;
    let usage_path = run_dir.join("usage.json");
    create_private_file(&usage_path)?;
    let mut usage_guard = UsageArtifactGuard::new(usage_path.clone());
    let started = Instant::now();
    let mut evidence = ManifestGuard {
        path: run_dir.join("manifest.json"),
        started,
        document: RunManifest {
            schema_version: 2,
            task_sha256: task_hash.clone(),
            task_bytes,
            task_transport: "stdin",
            llmadapter_schema_version: capability.schema_version,
            llmadapter_max_workers: capability.max_workers,
            llmadapter_max_result_tokens: capability.max_result_tokens,
            llmadapter_max_prompt_bytes: capability.max_prompt_bytes,
            lanes: request.lanes.to_owned(),
            remote: policy.remote,
            paid: policy.paid,
            fresh: request.fresh,
            no_cache,
            requested_max_tokens: request.max_tokens,
            token_limit_status: "requested ceiling; provider enforcement varies",
            deadline_secs: request.deadline_secs,
            adapter_stdout_limit_bytes: MAX_ADAPTER_STDOUT,
            usage_artifact: "usage.json",
            usage_status: "PENDING",
            adapter: None,
            adapter_result_status: None,
            adapter_result_exit_code: None,
            adapter_lanes: Vec::new(),
            candidates: Vec::new(),
            final_status: "RUNNING",
            total_elapsed_ms: 0,
        },
    };
    evidence.checkpoint()?;

    let store = Store::open(&data_dir.join("agentmaster.db"))?;
    log_status(
        &store,
        &safe_name,
        "START",
        "task_sha256",
        &task_hash,
        &run_dir,
    );

    let mut command = Command::new(program);
    command
        .arg("ask-v2")
        .arg("--stdin")
        .arg("--swarm")
        .arg("--lanes")
        .arg(request.lanes)
        .arg("--max-tokens")
        .arg(request.max_tokens.to_string())
        .arg("--deadline-secs")
        .arg(request.deadline_secs.to_string())
        .arg("--usage-out")
        .arg(&usage_path);
    if policy.remote {
        command.arg("--allow-remote");
    }
    if policy.paid {
        command.arg("--allow-paid");
    }
    if no_cache {
        command.arg("--no-cache");
    }
    if policy.remote {
        command.env("ATS_PII_SHIELD", "1");
    }
    command.stdin(Stdio::from(private_stdin_file(
        &run_dir,
        request.task.as_bytes(),
    )?));

    let adapter_started = Instant::now();
    let output = run_bounded(
        command,
        Duration::from_secs(request.deadline_secs),
        MAX_ADAPTER_STDOUT,
        MAX_STDERR,
        &run_dir,
    )?;
    evidence.document.adapter = Some(ProcessEvidence::from_capture(
        &output,
        adapter_started.elapsed(),
    ));
    evidence.checkpoint()?;
    if output.timed_out {
        log_status(
            &store,
            &safe_name,
            "ERROR",
            "task_sha256",
            &task_hash,
            &run_dir,
        );
        bail!(
            "llmadapter exceeded the {}s ensemble deadline",
            request.deadline_secs
        );
    }
    if output.stdout.truncated {
        bail!(
            "llmadapter JSON exceeded the {}-byte stdout limit",
            MAX_ADAPTER_STDOUT
        );
    }

    let parsed: AdapterOutput =
        serde_json::from_slice(&output.stdout.bytes).context("llmadapter returned invalid JSON")?;
    validate_adapter_output(
        &parsed,
        &output,
        &request,
        &policy,
        &capability,
        &task_hash,
        task_bytes,
    )?;
    evidence.document.adapter_result_status = Some(parsed.status.clone());
    evidence.document.adapter_result_exit_code = Some(parsed.exit_code);
    evidence.document.adapter_lanes = parsed
        .results
        .iter()
        .map(|result| LaneEvidence {
            lane: result.lane.clone(),
            model: result.model.clone(),
            kind: result.kind.clone(),
            class: result.class.clone(),
            ok: result.ok,
            terminal: result.terminal.clone(),
            elapsed_ms: result.ms,
            max_tokens: result.max_tokens,
            cap_mode: result.cap_mode.clone(),
            token_count_source: result.token_count_source.clone(),
            input_tokens: result.input_tokens,
            output_tokens: result.output_tokens,
            cached: result.cached.unwrap_or(false),
            call_started: result.call_started,
        })
        .collect();
    evidence.checkpoint()?;
    validate_usage(
        &usage_path,
        &parsed.accounting,
        &parsed,
        &request,
        &capability,
        &task_hash,
        task_bytes,
    )?;
    usage_guard.keep();
    evidence.document.usage_status = "COMPLETE";
    evidence.checkpoint()?;
    if !output.status.success() {
        log_status(
            &store,
            &safe_name,
            "ERROR",
            "task_sha256",
            &task_hash,
            &run_dir,
        );
        bail!(
            "llmadapter structured failure: status={} exit_code={}",
            parsed.status,
            parsed.exit_code
        );
    }

    let valid_answers = parsed.ok;
    for (index, result) in parsed.results.into_iter().filter(|r| r.ok).enumerate() {
        let answer = result
            .answer
            .filter(|a| !a.trim().is_empty())
            .context("llmadapter marked a result ok but returned no answer")?;
        let answer_path = run_dir.join(format!(
            "answer-{:02}-{}.txt",
            index + 1,
            slug(&result.lane)
        ));
        write_private(&answer_path, answer.as_bytes())?;
        let answer_hash = sha256_hex(answer.as_bytes());
        evidence.document.candidates.push(CandidateEvidence {
            lane: result.lane,
            answer_sha256: answer_hash.clone(),
            artifact: answer_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("answer.txt")
                .to_owned(),
            oracle: None,
            status: "PENDING",
        });
        let candidate_index = evidence.document.candidates.len() - 1;
        evidence.checkpoint()?;

        let mut oracle = Command::new("sh");
        oracle
            .args(["-c", request.oracle])
            .env("AGENTMASTER_RUN_DIR", &run_dir)
            .env("AGENTMASTER_ANSWER_PATH", &answer_path)
            .env("AGENTMASTER_USAGE_PATH", &usage_path);
        let remaining =
            Duration::from_secs(request.deadline_secs).saturating_sub(started.elapsed());
        if remaining.is_zero() {
            log_status(
                &store,
                &safe_name,
                "ERROR",
                "task_sha256",
                &task_hash,
                &run_dir,
            );
            bail!(
                "ensemble exceeded the {}s deadline before oracle completion",
                request.deadline_secs
            );
        }
        let oracle_started = Instant::now();
        let oracle_output = run_bounded(
            oracle,
            remaining.min(Duration::from_secs(ORACLE_TIMEOUT_SECS)),
            MAX_ORACLE_STDOUT,
            MAX_STDERR,
            &run_dir,
        )?;
        let passed = !oracle_output.timed_out
            && oracle_output.status.success()
            && !oracle_output.stdout.truncated
            && !oracle_output.stderr.truncated;
        let candidate = &mut evidence.document.candidates[candidate_index];
        candidate.oracle = Some(OracleEvidence {
            elapsed_ms: millis(oracle_started.elapsed()),
            exit_code: oracle_output.status.code(),
            timed_out: oracle_output.timed_out,
            stdout_bytes: oracle_output.stdout.total,
            stderr_bytes: oracle_output.stderr.total,
        });
        candidate.status = if passed { "PASS" } else { "FAIL" };
        evidence.checkpoint()?;
        log_status(
            &store,
            &safe_name,
            if passed { "PASS" } else { "FAIL" },
            "answer_sha256",
            &answer_hash,
            &answer_path,
        );
        if passed {
            evidence.finish("PASS")?;
            println!("ensemble PASS");
            println!("  answer sha256: {answer_hash}");
            println!("  artifact     : {}", answer_path.display());
            println!("  usage        : {}", usage_path.display());
            return Ok(());
        }
    }

    log_status(
        &store,
        &safe_name,
        "FAIL",
        "task_sha256",
        &task_hash,
        &run_dir,
    );
    evidence.finish("FAIL")?;
    bail!(
        "ensemble exhausted {} valid answer(s) without an oracle PASS; artifacts: {}",
        valid_answers,
        run_dir.display()
    )
}

fn validate(request: &EnsembleRequest<'_>) -> Result<LanePolicy> {
    if request.dry_run == request.go {
        bail!("exactly one of --dry-run or --go is required");
    }
    if request.name.trim().is_empty() || request.task.trim().is_empty() {
        bail!("ensemble name and task must not be empty");
    }
    if request.oracle.trim().is_empty() {
        bail!("--oracle must not be empty");
    }
    if request.deadline_secs == 0 {
        bail!("--deadline-secs must be greater than zero");
    }
    if !(1..=500).contains(&request.max_tokens) {
        bail!("--max-tokens must be between 1 and 500");
    }

    let mut policy = LanePolicy {
        remote: false,
        paid: false,
    };
    let mut count = 0;
    for lane in request.lanes.split(',').map(str::trim) {
        if lane.is_empty() {
            bail!("--lanes contains an empty selector");
        }
        count += 1;
        match lane {
            "local" => {}
            "free" | "cli" => policy.remote = true,
            "paid" | "all" => {
                policy.remote = true;
                policy.paid = true;
            }
            _ => {
                policy.remote = true;
                policy.paid = true;
            }
        }
    }
    if count == 0 {
        bail!("--lanes must not be empty");
    }
    if policy.remote && !request.allow_remote {
        bail!("remote/free/cli or named lanes require --allow-remote");
    }
    if policy.paid && !request.allow_paid {
        bail!("paid, all, or named lanes require --allow-paid");
    }
    Ok(policy)
}

fn probe_capability(program: &Path, data_dir: &Path) -> Result<AdapterCapability> {
    private_dir(data_dir)?;
    let mut command = Command::new(program);
    command
        .args(["contract", "agentmaster capability probe"])
        .stdin(Stdio::null());
    let output = run_bounded(
        command,
        Duration::from_secs(CAPABILITY_TIMEOUT_SECS),
        MAX_USAGE_BYTES as usize,
        MAX_STDERR,
        data_dir,
    )
    .context("llmadapter v2 capability probe failed")?;
    if output.timed_out {
        bail!("llmadapter v2 capability probe timed out");
    }
    if !output.status.success() || output.stdout.truncated {
        bail!(
            "llmadapter v2 capability probe failed: status={} stdout_bytes={} stderr_bytes={}{}",
            output.status,
            output.stdout.total,
            output.stderr.total,
            truncation_note(&output)
        );
    }
    let capability: AdapterCapability = serde_json::from_slice(&output.stdout.bytes)
        .context("llmadapter capability is not parseable v2 JSON")?;
    if capability.schema_version != 2
        || !capability.ask_v2
        || !(1..=MAX_WORKERS).contains(&capability.max_workers)
        || !(1..=500).contains(&capability.max_result_tokens)
        || !(1..=MAX_PROMPT_BYTES).contains(&capability.max_prompt_bytes)
        || capability
            .mode
            .as_deref()
            .is_some_and(|value| value != "swarm")
        || capability
            .default_max_workers
            .is_some_and(|value| value != capability.max_workers)
        || capability
            .fanout_max_workers
            .is_some_and(|value| value < capability.max_workers)
        || capability
            .max_result_tokens_semantics
            .as_deref()
            .is_some_and(|value| value != "requested_ceiling_by_capability")
        || capability.tools_available == Some(true)
        || capability
            .capsule_visible_input_tokens_proxy
            .is_some_and(|value| value == 0 || value > 500)
        || capability
            .route
            .as_deref()
            .is_some_and(|value| value != "none")
        || capability
            .packet
            .as_deref()
            .is_some_and(|value| value.is_empty() || value.len() > 4_096)
    {
        bail!(
            "llmadapter v2 capability required (schema_version=2, ask_v2=true, max_workers<=3, max_result_tokens<=500, max_prompt_bytes<=1800)"
        );
    }
    Ok(capability)
}

#[allow(clippy::too_many_arguments)]
fn validate_adapter_output(
    output: &AdapterOutput,
    captured: &Captured,
    request: &EnsembleRequest<'_>,
    policy: &LanePolicy,
    capability: &AdapterCapability,
    task_hash: &str,
    task_bytes: usize,
) -> Result<()> {
    if output.schema != "llmadapter.result" || output.schema_version != 2 {
        bail!(
            "llmadapter result schema mismatch: schema={} version={}",
            output.schema,
            output.schema_version
        );
    }
    if output.prompt.sha256 != task_hash
        || output.prompt.bytes != task_bytes
        || output.prompt.transport != "stdin"
    {
        bail!("llmadapter prompt hash/size/transport mismatch");
    }
    if output.stage != "worker" {
        bail!("llmadapter returned unsupported stage {}", output.stage);
    }
    let actual_exit = captured
        .status
        .code()
        .context("llmadapter exited without a numeric status")?;
    if output.exit_code != actual_exit {
        bail!(
            "llmadapter exit-code mismatch: process={actual_exit} result={}",
            output.exit_code
        );
    }
    if !matches!(
        output.status.as_str(),
        "ok" | "partial" | "failed" | "invalid"
    ) {
        bail!("llmadapter returned invalid status {}", output.status);
    }
    if (actual_exit == 0) != matches!(output.status.as_str(), "ok" | "partial") {
        bail!(
            "llmadapter status/exit mismatch: status={} exit_code={actual_exit}",
            output.status
        );
    }
    if output.lanes.requested != request.lanes
        || output.lanes.fanout
        || !(1..=capability.max_workers).contains(&output.lanes.cap)
        || output.lanes.selected > output.lanes.cap
    {
        bail!("llmadapter lane selection/cap contract mismatch");
    }
    if output.total != output.results.len()
        || output.total != output.lanes.selected
        || output.total > capability.max_workers
    {
        bail!("llmadapter result count/cap contract mismatch");
    }

    let valid = output
        .results
        .iter()
        .filter(|r| {
            r.ok && r
                .answer
                .as_deref()
                .is_some_and(|answer| !answer.trim().is_empty())
                && matches!(r.terminal.as_str(), "succeeded" | "cached")
        })
        .count();
    if output.ok != valid || (captured.status.success() && valid == 0) {
        bail!(
            "llmadapter result contract failed: ok={} valid_results={valid}",
            output.ok
        );
    }
    for result in &output.results {
        if result.lane.is_empty()
            || !matches!(result.kind.as_str(), "openrouter" | "ollama" | "cli")
            || !matches!(result.class.as_str(), "free" | "paid" | "local" | "cli")
            || !matches!(
                result.terminal.as_str(),
                "succeeded" | "failed" | "timeout" | "output_limit" | "cached"
            )
            || !matches!(
                result.token_count_source.as_str(),
                "provider_reported" | "estimated" | "unknown"
            )
            || !matches!(
                result.cap_mode.as_str(),
                "provider_server" | "local_native" | "advisory_only"
            )
        {
            bail!("llmadapter result contains an invalid lane status");
        }
        let cached = result.cached.unwrap_or(false);
        if result.error.as_ref().is_some_and(|value| value.len() > 80)
            || result.detail.as_ref().is_some_and(|value| value.len() > 80)
            || cached != (result.terminal == "cached")
            || (cached && result.call_started)
            || (matches!(
                result.terminal.as_str(),
                "succeeded" | "timeout" | "output_limit"
            ) && !result.call_started)
        {
            bail!("llmadapter result contains inconsistent terminal evidence");
        }
        if result.max_tokens != request.max_tokens
            || (result.class != "local" && !policy.remote)
            || (result.class == "paid" && !policy.paid)
            || (result.ok != matches!(result.terminal.as_str(), "succeeded" | "cached"))
            || !matches!(
                (
                    result.kind.as_str(),
                    result.class.as_str(),
                    result.cap_mode.as_str()
                ),
                ("openrouter", "free" | "paid", "provider_server")
                    | ("ollama", "local", "local_native")
                    | ("cli", "local" | "cli", "advisory_only")
            )
        {
            bail!("llmadapter result violates lane policy or token contract");
        }
    }
    Ok(())
}

fn validate_usage(
    path: &Path,
    embedded: &UsageOutput,
    output: &AdapterOutput,
    request: &EnsembleRequest<'_>,
    capability: &AdapterCapability,
    task_hash: &str,
    task_bytes: usize,
) -> Result<()> {
    let metadata =
        fs::symlink_metadata(path).context("llmadapter did not write the usage artifact")?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        bail!("llmadapter usage artifact is not a regular file");
    }
    if metadata.len() > MAX_USAGE_BYTES {
        bail!("llmadapter usage artifact exceeded {MAX_USAGE_BYTES} bytes");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!("llmadapter usage artifact permissions are not private");
        }
        if metadata.uid() != unsafe { libc::geteuid() } {
            bail!("llmadapter usage artifact is not owned by the current user");
        }
        let mut options = OpenOptions::new();
        options.read(true).custom_flags(libc::O_NOFOLLOW);
        let file = options
            .open(path)
            .context("llmadapter did not write the usage artifact")?;
        let opened = file.metadata()?;
        if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
            bail!("llmadapter usage artifact changed while opening");
        }
        validate_usage_file(
            file, path, embedded, output, request, capability, task_hash, task_bytes,
        )?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let file = File::open(path).context("llmadapter did not write the usage artifact")?;
        validate_usage_file(
            file, path, embedded, output, request, capability, task_hash, task_bytes,
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_usage_file(
    file: File,
    path: &Path,
    embedded: &UsageOutput,
    output: &AdapterOutput,
    request: &EnsembleRequest<'_>,
    capability: &AdapterCapability,
    task_hash: &str,
    task_bytes: usize,
) -> Result<()> {
    let usage: UsageOutput =
        serde_json::from_reader(file).context("llmadapter wrote invalid or non-v2 usage JSON")?;
    if usage != *embedded {
        bail!("llmadapter usage/accounting mismatch");
    }
    let calls_started = output
        .results
        .iter()
        .filter(|result| result.call_started)
        .count();
    let cache_hits = output
        .results
        .iter()
        .filter(|result| result.cached.unwrap_or(false))
        .count();
    let input_tokens = expected_token_accounting(&output.results, true);
    let output_tokens = expected_token_accounting(&output.results, false);
    if usage.schema != "llmadapter.accounting"
        || usage.schema_version != 2
        || usage.status != "complete"
        || !usage.terminal_records_complete
        || usage.prompt_sha256 != task_hash
        || usage.prompt_bytes != task_bytes
        || usage.transport != "stdin"
        || usage.stage != "worker"
        || usage.requested_lane_spec != request.lanes
        || usage.selected_lane_count != output.results.len()
        || usage.selected_lane_count != output.lanes.selected
        || usage.lane_cap != output.lanes.cap
        || usage.selected_lane_count > usage.lane_cap
        || !(1..=capability.max_workers).contains(&usage.lane_cap)
        || usage.fanout
        || usage.max_tokens != request.max_tokens
        || usage.calls_started != calls_started
        || usage.calls_completed != calls_started
        || usage.cache_hits != cache_hits
        || usage.input_tokens != input_tokens
        || usage.output_tokens != output_tokens
        || usage.cost_status != "unknown"
        || !usage.completed
        || usage.failed != (output.ok == 0)
    {
        bail!("llmadapter usage schema/hash/accounting is incomplete or inconsistent");
    }
    write_private_json(path, &usage).context("failed to canonicalize llmadapter usage artifact")?;
    Ok(())
}

fn expected_token_accounting(results: &[AdapterResult], input: bool) -> TokenAccounting {
    let mut accounting = TokenAccounting {
        reported: 0,
        estimated: 0,
        unknown_calls: 0,
    };
    for result in results {
        if !result.call_started || result.cached.unwrap_or(false) {
            continue;
        }
        let count = if input {
            result.input_tokens
        } else {
            result.output_tokens
        };
        match (result.token_count_source.as_str(), count) {
            ("provider_reported", Some(value)) => accounting.reported += value,
            ("estimated", Some(value)) => accounting.estimated += value,
            _ => accounting.unknown_calls += 1,
        }
    }
    accounting
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn create_run_dir(data_dir: &Path, name: &str) -> Result<PathBuf> {
    let base = data_dir.join("ensembles");
    let named = base.join(name);
    private_dir(&base)?;
    private_dir(&named)?;
    let run_dir = named.join(uuid::Uuid::new_v4().to_string());
    private_dir(&run_dir)?;
    Ok(run_dir)
}

fn private_dir(path: &Path) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && metadata.file_type().is_symlink()
    {
        bail!("refusing symlinked ensemble directory: {}", path.display());
    }
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn create_private_file(path: &Path) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?;
    Ok(())
}

fn private_stdin_file(dir: &Path, bytes: &[u8]) -> Result<File> {
    let path = dir.join(format!(
        ".task-stdin-{}",
        uuid::Uuid::new_v4().as_hyphenated()
    ));
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path)?;
    file.write_all(bytes)?;
    file.flush()?;
    file.seek(SeekFrom::Start(0))?;
    #[cfg(unix)]
    fs::remove_file(&path)?;
    #[cfg(not(unix))]
    {
        drop(file);
        let _ = fs::remove_file(&path);
        bail!("ensemble private stdin transport currently requires macOS or Linux");
    }
    #[cfg(unix)]
    Ok(file)
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    Ok(())
}

fn write_private_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let temp = path.with_file_name(format!(
        ".manifest-{}.tmp",
        uuid::Uuid::new_v4().as_hyphenated()
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> Result<()> {
        let mut file = options.open(&temp)?;
        serde_json::to_writer_pretty(&mut file, value)?;
        file.write_all(b"\n")?;
        file.flush()?;
        fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn run_bounded(
    mut command: Command,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
    _capture_dir: &Path,
) -> Result<Captured> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().context("failed to start subprocess")?;
    let process_group = child.id();
    let mut child_stdout = child
        .stdout
        .take()
        .context("subprocess stdout pipe missing")?;
    let mut child_stderr = child
        .stderr
        .take()
        .context("subprocess stderr pipe missing")?;
    if let Err(error) = set_nonblocking(&child_stdout).and_then(|_| set_nonblocking(&child_stderr))
    {
        kill_process_group(process_group);
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    let mut stdout = empty_capture(stdout_limit);
    let mut stderr = empty_capture(stderr_limit);

    let start = Instant::now();
    let (status, timed_out) = loop {
        let output_limited = match drain_capture(&mut child_stdout, &mut stdout, stdout_limit)
            .and_then(|stdout_limited| {
                drain_capture(&mut child_stderr, &mut stderr, stderr_limit)
                    .map(|stderr_limited| stdout_limited | stderr_limited)
            }) {
            Ok(limited) => limited,
            Err(error) => {
                kill_process_group(process_group);
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.into());
            }
        };
        if output_limited {
            kill_process_group(process_group);
            let _ = child.kill();
            break (child.wait()?, false);
        }
        match child.try_wait() {
            Ok(Some(status)) => break (status, false),
            Ok(None) => {}
            Err(error) => {
                kill_process_group(process_group);
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.into());
            }
        }
        if start.elapsed() >= timeout {
            kill_process_group(process_group);
            let _ = child.kill();
            break (child.wait()?, true);
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    kill_process_group(process_group);
    let _ = drain_capture(&mut child_stdout, &mut stdout, stdout_limit)?;
    let _ = drain_capture(&mut child_stderr, &mut stderr, stderr_limit)?;
    Ok(Captured {
        status,
        stdout,
        stderr,
        timed_out,
    })
}

fn empty_capture(limit: usize) -> CapturedStream {
    CapturedStream {
        bytes: Vec::with_capacity(limit.min(8_192)),
        total: 0,
        truncated: false,
    }
}

#[cfg(unix)]
fn set_nonblocking(stream: &impl std::os::fd::AsRawFd) -> Result<()> {
    let descriptor = stream.as_raw_fd();
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_nonblocking<T>(_stream: &T) -> Result<()> {
    bail!("ensemble subprocess capture currently requires macOS or Linux")
}

fn drain_capture(
    reader: &mut impl Read,
    capture: &mut CapturedStream,
    limit: usize,
) -> std::io::Result<bool> {
    let mut chunk = [0_u8; 8_192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => return Ok(capture.truncated),
            Ok(count) => {
                capture.total = capture.total.saturating_add(count);
                let retained = limit.saturating_sub(capture.bytes.len()).min(count);
                capture.bytes.extend_from_slice(&chunk[..retained]);
                if retained < count {
                    capture.truncated = true;
                    return Ok(true);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                return Ok(capture.truncated);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

fn kill_process_group(process_group: u32) {
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn kill(pid: i32, signal: i32) -> i32;
        }
        // The child owns this process group, so a negative PID targets only its tree.
        unsafe {
            let _ = kill(-(process_group as i32), 9);
        }
    }
}

fn truncation_note(output: &Captured) -> &'static str {
    if output.stdout.truncated || output.stderr.truncated {
        " (diagnostics truncated)"
    } else {
        ""
    }
}

fn millis(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

fn log_status(
    store: &Store,
    name: &str,
    status: &str,
    hash_name: &str,
    hash: &str,
    artifact: &Path,
) {
    store.log(
        None,
        name,
        "ensemble",
        &format!(
            "status={status} {hash_name}={hash} artifact={}",
            artifact.display()
        ),
    );
}

fn slug(value: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for ch in value.chars().take(96) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "ensemble".into()
    } else {
        out
    }
}

fn sha256_hex(input: &[u8]) -> String {
    format!("{:x}", Sha256::digest(input))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request<'a>(task: &'a str, oracle: &'a str) -> EnsembleRequest<'a> {
        EnsembleRequest {
            name: "proof run",
            task,
            oracle,
            lanes: "local",
            deadline_secs: 2,
            max_tokens: 500,
            fresh: false,
            dry_run: false,
            go: true,
            allow_remote: false,
            allow_paid: false,
        }
    }

    fn temp_dir() -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("agentmaster-ensemble-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn accounting(task: &str, lanes: &str, completed: bool) -> serde_json::Value {
        serde_json::json!({
            "schema": "llmadapter.accounting",
            "schema_version": 2,
            "status": "complete",
            "terminal_records_complete": completed,
            "prompt_sha256": sha256_hex(task.as_bytes()),
            "prompt_bytes": task.len(),
            "transport": "stdin",
            "stage": "worker",
            "requested_lane_spec": lanes,
            "selected_lane_count": 3,
            "lane_cap": 3,
            "fanout": false,
            "max_tokens": 500,
            "calls_started": 3,
            "calls_completed": 3,
            "cache_hits": 0,
            "input_tokens": {"reported": 0, "estimated": 3, "unknown_calls": 0},
            "output_tokens": {"reported": 0, "estimated": 3, "unknown_calls": 0},
            "estimated_cost_usd": null,
            "cost_status": "unknown",
            "completed": true,
            "failed": false
        })
    }

    fn adapter_result(task: &str, lanes: &str, accounting: serde_json::Value) -> String {
        let result = |lane: &str, answer: &str| {
            let local = lanes == "local";
            serde_json::json!({
                "lane": lane,
                "kind": if local { "ollama" } else { "cli" },
                "class": if local { "local" } else { "cli" },
                "ok": true,
                "terminal": "succeeded",
                "ms": 1,
                "answer": answer,
                "call_started": true,
                "input_tokens": 1,
                "output_tokens": 1,
                "token_count_source": "estimated",
                "max_tokens": 500,
                "cap_mode": if local { "local_native" } else { "advisory_only" }
            })
        };
        serde_json::to_string(&serde_json::json!({
            "schema": "llmadapter.result",
            "schema_version": 2,
            "status": "ok",
            "exit_code": 0,
            "prompt": {
                "sha256": sha256_hex(task.as_bytes()),
                "bytes": task.len(),
                "transport": "stdin"
            },
            "stage": "worker",
            "lanes": {"requested": lanes, "selected": 3, "cap": 3, "fanout": false},
            "ok": 3,
            "total": 3,
            "results": [
                result("one", "loser"),
                result("two", "winner"),
                result("three", "unused")
            ],
            "accounting": accounting
        }))
        .unwrap()
    }

    fn shell_quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }

    #[cfg(unix)]
    fn fake_adapter(root: &Path, lane: &str, usage_complete: bool, extra_checks: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = root.join("llmadapter");
        let embedded = accounting("fixture task", lane, true);
        let usage =
            serde_json::to_string(&accounting("fixture task", lane, usage_complete)).unwrap();
        let result = adapter_result("fixture task", lane, embedded);
        let script = format!(
            r#"#!/bin/sh
set -eu
if [ "$1" = contract ]; then
  [ "$#" = 2 ]
  [ "$2" = "agentmaster capability probe" ]
  printf '%s' '{{"schema_version":2,"ask_v2":true,"max_workers":3,"max_result_tokens":500,"max_prompt_bytes":1800}}'
  exit 0
fi
[ "$1" = ask-v2 ]
case " $* " in *"fixture task"*) exit 91;; esac
all_args=$*
shift
stdin=0
swarm=0
lanes=
usage_path=
max_tokens=
deadline=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --stdin) stdin=1; shift ;;
    --swarm) swarm=1; shift ;;
    --lanes) lanes=$2; shift 2 ;;
    --max-tokens) max_tokens=$2; shift 2 ;;
    --deadline-secs) deadline=$2; shift 2 ;;
    --usage-out) usage_path=$2; shift 2 ;;
    --allow-remote|--allow-paid|--no-cache) shift ;;
    *) exit 92 ;;
  esac
done
[ "$stdin" = 1 ]
[ "$swarm" = 1 ]
[ "$lanes" = "{lane}" ]
[ "$max_tokens" = 500 ]
[ "$deadline" = 2 ]
[ -n "$usage_path" ]
[ -f /dev/stdin ]
[ "$(cat)" = "fixture task" ]
{extra_checks}
printf '%s' {usage} > "$usage_path"
chmod 600 "$usage_path"
printf '%s' {result}
"#,
            usage = shell_quote(&usage),
            result = shell_quote(&result),
        );
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    fn protocol_adapter(
        root: &Path,
        capability: &str,
        result: &str,
        usage: &str,
        exit_code: i32,
        ask_marker: Option<&Path>,
    ) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = root.join("llmadapter");
        let marker = ask_marker
            .map(|path| format!("touch {}", shell_quote(&path.display().to_string())))
            .unwrap_or_default();
        let script = format!(
            r#"#!/bin/sh
set -eu
if [ "$1" = contract ]; then
  printf '%s' {capability}
  exit 0
fi
[ "$1" = ask-v2 ]
{marker}
case " $* " in *"fixture task"*) exit 91;; esac
usage_path=
while [ "$#" -gt 0 ]; do
  if [ "$1" = --usage-out ]; then
    usage_path=$2
    shift 2
  else
    shift
  fi
done
[ "$(cat)" = "fixture task" ]
printf '%s' {usage} > "$usage_path"
chmod 600 "$usage_path"
printf '%s' {result}
exit {exit_code}
"#,
            capability = shell_quote(capability),
            result = shell_quote(result),
            usage = shell_quote(usage),
        );
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn sha256_matches_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn validates_modes_requested_ceiling_and_lane_permissions() {
        let mut req = request("task", "true");
        req.go = false;
        assert!(validate(&req).is_err());
        req.go = true;
        req.max_tokens = 501;
        assert!(validate(&req).is_err());
        req.max_tokens = 500;
        req.lanes = "free";
        assert!(validate(&req).is_err());
        req.allow_remote = true;
        assert!(validate(&req).is_ok());
        req.lanes = "paid";
        assert!(validate(&req).is_err());
        req.allow_paid = true;
        assert!(validate(&req).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn dry_run_probes_v2_but_never_starts_a_provider() {
        let root = temp_dir();
        let program = fake_adapter(&root, "local", true, "");
        let mut req = request("fixture task", "true");
        req.go = false;
        req.dry_run = true;
        run_with_program(req, &root, &program).unwrap();
        assert!(!root.join("ensembles").exists());
    }

    #[cfg(unix)]
    #[test]
    fn old_adapter_fails_closed_before_ask_v2() {
        let root = temp_dir();
        let ask_marker = root.join("ask-was-called");
        let program = protocol_adapter(
            &root,
            r#"{"max_workers":3,"max_result_tokens":500}"#,
            "{}",
            "{}",
            0,
            Some(&ask_marker),
        );
        let error = run_with_program(request("fixture task", "true"), &root, &program)
            .unwrap_err()
            .to_string();
        assert!(error.contains("capability"));
        assert!(!ask_marker.exists());
        assert!(!root.join("ensembles").exists());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_wrong_capability_version_before_ask_v2() {
        let root = temp_dir();
        let ask_marker = root.join("ask-was-called");
        let program = protocol_adapter(
            &root,
            r#"{"schema_version":3,"ask_v2":true,"max_workers":3,"max_result_tokens":500,"max_prompt_bytes":1800}"#,
            "{}",
            "{}",
            0,
            Some(&ask_marker),
        );
        let error = run_with_program(request("fixture task", "true"), &root, &program)
            .unwrap_err()
            .to_string();
        assert!(error.contains("capability"));
        assert!(!ask_marker.exists());
        assert!(!root.join("ensembles").exists());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_oversized_prompt_before_ask_v2() {
        let root = temp_dir();
        let ask_marker = root.join("ask-was-called");
        let program = protocol_adapter(
            &root,
            r#"{"schema_version":2,"ask_v2":true,"max_workers":3,"max_result_tokens":500,"max_prompt_bytes":4}"#,
            "{}",
            "{}",
            0,
            Some(&ask_marker),
        );
        let error = run_with_program(request("fixture task", "true"), &root, &program)
            .unwrap_err()
            .to_string();
        assert!(error.contains("allows at most 4"));
        assert!(!ask_marker.exists());
        assert!(!root.join("ensembles").exists());
    }

    #[cfg(unix)]
    #[test]
    fn runs_exact_bounded_contract_and_stops_at_first_oracle_pass() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_dir();
        let program = fake_adapter(&root, "local", true, "");
        run_with_program(
            request(
                "fixture task",
                r#"[ "$(cat "$AGENTMASTER_ANSWER_PATH")" = winner ]"#,
            ),
            &root,
            &program,
        )
        .unwrap();

        let named = root.join("ensembles").join("proof-run");
        let run_dir = fs::read_dir(named).unwrap().next().unwrap().unwrap().path();
        assert_eq!(
            fs::metadata(&run_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let mut files: Vec<_> = fs::read_dir(&run_dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        files.sort();
        assert_eq!(files.len(), 4);
        assert!(files.iter().any(|path| path.ends_with("answer-01-one.txt")));
        assert!(files.iter().any(|path| path.ends_with("answer-02-two.txt")));
        assert!(
            !files
                .iter()
                .any(|path| path.ends_with("answer-03-three.txt"))
        );
        for path in &files {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            let content = fs::read_to_string(path).unwrap();
            assert!(!content.contains("fixture task"));
            assert!(!content.contains("\"results\""));
        }
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(run_dir.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["schema_version"], 2);
        assert_eq!(manifest["task_transport"], "stdin");
        assert_eq!(manifest["task_bytes"], 12);
        assert_eq!(manifest["llmadapter_schema_version"], 2);
        assert_eq!(manifest["adapter_result_status"], "ok");
        assert_eq!(manifest["adapter_result_exit_code"], 0);
        assert_eq!(manifest["adapter_lanes"].as_array().unwrap().len(), 3);
        assert_eq!(manifest["adapter_lanes"][0]["cap_mode"], "local_native");
        assert_eq!(manifest["adapter_lanes"][0]["max_tokens"], 500);
        assert_eq!(manifest["requested_max_tokens"], 500);
        assert_eq!(
            manifest["token_limit_status"],
            "requested ceiling; provider enforcement varies"
        );
        assert_eq!(manifest["final_status"], "PASS");
        assert_eq!(manifest["candidates"].as_array().unwrap().len(), 2);
        assert_eq!(manifest["candidates"][0]["status"], "FAIL");
        assert_eq!(manifest["candidates"][1]["status"], "PASS");
        assert_eq!(manifest["candidates"][0]["oracle"]["exit_code"], 1);
        assert_eq!(manifest["candidates"][1]["oracle"]["exit_code"], 0);

        let events = Store::open(&root.join("agentmaster.db"))
            .unwrap()
            .recent(10);
        assert!(events.iter().any(|row| row.3.contains("status=PASS")));
        assert!(events.iter().all(|row| !row.3.contains("fixture task")));
        assert!(events.iter().all(|row| !row.3.contains("winner")));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_result_schema_mismatch_before_oracle() {
        let root = temp_dir();
        let oracle_marker = root.join("oracle-was-called");
        let embedded = accounting("fixture task", "local", true);
        let mut result: serde_json::Value =
            serde_json::from_str(&adapter_result("fixture task", "local", embedded.clone()))
                .unwrap();
        result["schema_version"] = serde_json::json!(1);
        let program = protocol_adapter(
            &root,
            r#"{"schema_version":2,"ask_v2":true,"max_workers":3,"max_result_tokens":500,"max_prompt_bytes":1800}"#,
            &serde_json::to_string(&result).unwrap(),
            &serde_json::to_string(&embedded).unwrap(),
            0,
            None,
        );
        let oracle = format!(
            "touch {}",
            shell_quote(&oracle_marker.display().to_string())
        );
        let error = run_with_program(request("fixture task", &oracle), &root, &program)
            .unwrap_err()
            .to_string();
        assert!(error.contains("schema mismatch"));
        assert!(!oracle_marker.exists());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_empty_lane_name_before_oracle() {
        let root = temp_dir();
        let oracle_marker = root.join("oracle-was-called");
        let embedded = accounting("fixture task", "local", true);
        let mut result: serde_json::Value =
            serde_json::from_str(&adapter_result("fixture task", "local", embedded.clone()))
                .unwrap();
        result["results"][0]["lane"] = serde_json::json!("");
        let program = protocol_adapter(
            &root,
            r#"{"schema_version":2,"ask_v2":true,"max_workers":3,"max_result_tokens":500,"max_prompt_bytes":1800}"#,
            &serde_json::to_string(&result).unwrap(),
            &serde_json::to_string(&embedded).unwrap(),
            0,
            None,
        );
        let oracle = format!(
            "touch {}",
            shell_quote(&oracle_marker.display().to_string())
        );
        let error = run_with_program(request("fixture task", &oracle), &root, &program)
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid lane status"));
        assert!(!oracle_marker.exists());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_usage_mismatch_before_oracle() {
        let root = temp_dir();
        let oracle_marker = root.join("oracle-was-called");
        let embedded = accounting("fixture task", "local", true);
        let mut usage = embedded.clone();
        usage["prompt_sha256"] = serde_json::json!("wrong");
        let program = protocol_adapter(
            &root,
            r#"{"schema_version":2,"ask_v2":true,"max_workers":3,"max_result_tokens":500,"max_prompt_bytes":1800}"#,
            &adapter_result("fixture task", "local", embedded),
            &serde_json::to_string(&usage).unwrap(),
            0,
            None,
        );
        let oracle = format!(
            "touch {}",
            shell_quote(&oracle_marker.display().to_string())
        );
        let error = run_with_program(request("fixture task", &oracle), &root, &program)
            .unwrap_err()
            .to_string();
        assert!(error.contains("usage/accounting mismatch"));
        assert!(!oracle_marker.exists());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_unknown_accounting_fields_and_removes_untrusted_usage() {
        let root = temp_dir();
        let mut embedded = accounting("fixture task", "local", true);
        embedded["unexpected"] = serde_json::json!("must not persist");
        let program = protocol_adapter(
            &root,
            r#"{"schema_version":2,"ask_v2":true,"max_workers":3,"max_result_tokens":500,"max_prompt_bytes":1800}"#,
            &adapter_result("fixture task", "local", embedded.clone()),
            &serde_json::to_string(&embedded).unwrap(),
            0,
            None,
        );
        let error = run_with_program(request("fixture task", "true"), &root, &program)
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid JSON"));
        let run_dir = fs::read_dir(root.join("ensembles").join("proof-run"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(!run_dir.join("usage.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_fabricated_accounting_derived_from_lane_results() {
        let root = temp_dir();
        let mut embedded = accounting("fixture task", "local", true);
        embedded["calls_started"] = serde_json::json!(2);
        embedded["calls_completed"] = serde_json::json!(2);
        let program = protocol_adapter(
            &root,
            r#"{"schema_version":2,"ask_v2":true,"max_workers":3,"max_result_tokens":500,"max_prompt_bytes":1800}"#,
            &adapter_result("fixture task", "local", embedded.clone()),
            &serde_json::to_string(&embedded).unwrap(),
            0,
            None,
        );
        let error = run_with_program(request("fixture task", "true"), &root, &program)
            .unwrap_err()
            .to_string();
        assert!(error.contains("incomplete or inconsistent"));
        let run_dir = fs::read_dir(root.join("ensembles").join("proof-run"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(!run_dir.join("usage.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn parses_nonzero_structured_failure_before_returning_error() {
        let root = temp_dir();
        let oracle_marker = root.join("oracle-was-called");
        let mut usage = accounting("fixture task", "local", true);
        usage["selected_lane_count"] = serde_json::json!(1);
        usage["calls_started"] = serde_json::json!(1);
        usage["calls_completed"] = serde_json::json!(1);
        usage["failed"] = serde_json::json!(true);
        let mut result = serde_json::json!({
            "schema": "llmadapter.result",
            "schema_version": 2,
            "status": "failed",
            "exit_code": 1,
            "prompt": {
                "sha256": sha256_hex(b"fixture task"),
                "bytes": 12,
                "transport": "stdin"
            },
            "stage": "worker",
            "lanes": {"requested": "local", "selected": 1, "cap": 3, "fanout": false},
            "ok": 0,
            "total": 1,
            "results": [{
                "lane": "one",
                "kind": "ollama",
                "class": "local",
                "ok": false,
                "terminal": "failed",
                "ms": 1,
                "error": "provider unavailable",
                "call_started": true,
                "input_tokens": null,
                "output_tokens": null,
                "token_count_source": "unknown",
                "max_tokens": 500,
                "cap_mode": "local_native"
            }],
            "accounting": usage
        });
        usage["input_tokens"] =
            serde_json::json!({"reported": 0, "estimated": 0, "unknown_calls": 1});
        usage["output_tokens"] =
            serde_json::json!({"reported": 0, "estimated": 0, "unknown_calls": 1});
        result["accounting"] = usage;
        let result_string = serde_json::to_string(&result).unwrap();
        let usage_string = serde_json::to_string(&result["accounting"]).unwrap();
        let program = protocol_adapter(
            &root,
            r#"{"schema_version":2,"ask_v2":true,"max_workers":3,"max_result_tokens":500,"max_prompt_bytes":1800}"#,
            &result_string,
            &usage_string,
            1,
            None,
        );
        let oracle = format!(
            "touch {}",
            shell_quote(&oracle_marker.display().to_string())
        );
        let error = run_with_program(request("fixture task", &oracle), &root, &program)
            .unwrap_err()
            .to_string();
        assert!(error.contains("structured failure: status=failed exit_code=1"));
        assert!(!oracle_marker.exists());
        let run_dir = fs::read_dir(root.join("ensembles").join("proof-run"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(run_dir.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["adapter_result_status"], "failed");
        assert_eq!(manifest["adapter_result_exit_code"], 1);
        assert_eq!(manifest["usage_status"], "COMPLETE");
        assert_eq!(manifest["adapter_lanes"][0]["terminal"], "failed");
        assert_eq!(manifest["candidates"].as_array().unwrap().len(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_incomplete_usage_before_oracle() {
        let root = temp_dir();
        let program = fake_adapter(&root, "local", false, "");
        let error = run_with_program(request("fixture task", "true"), &root, &program)
            .unwrap_err()
            .to_string();
        assert!(error.contains("usage/accounting mismatch"));
        let run_dir = fs::read_dir(root.join("ensembles").join("proof-run"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(fs::read_dir(&run_dir).unwrap().count(), 1);
        assert!(!run_dir.join("usage.json").exists());
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(run_dir.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["final_status"], "ERROR");
        assert_eq!(manifest["usage_status"], "PENDING");
    }

    #[cfg(unix)]
    #[test]
    fn remote_or_fresh_forces_no_cache_and_pii_shield() {
        let root = temp_dir();
        let program = fake_adapter(
            &root,
            "free",
            true,
            r#"case " $all_args " in *" --allow-remote "*) ;; *) exit 93;; esac
case " $all_args " in *" --no-cache "*) ;; *) exit 94;; esac
[ "$ATS_PII_SHIELD" = 1 ]"#,
        );
        let mut req = request("fixture task", "true");
        req.lanes = "free";
        req.allow_remote = true;
        run_with_program(req, &root, &program).unwrap();

        let fresh_root = temp_dir();
        let fresh_program = fake_adapter(
            &fresh_root,
            "local",
            true,
            r#"case " $all_args " in *" --no-cache "*) ;; *) exit 94;; esac"#,
        );
        let mut fresh = request("fixture task", "true");
        fresh.fresh = true;
        run_with_program(fresh, &fresh_root, &fresh_program).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn deadline_kills_the_adapter_process_tree() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_dir();
        let program = root.join("slow-llmadapter");
        fs::write(
            &program,
            r#"#!/bin/sh
if [ "$1" = contract ]; then
  printf '%s' '{"schema_version":2,"ask_v2":true,"max_workers":3,"max_result_tokens":500,"max_prompt_bytes":1800}'
  exit 0
fi
sleep 10 &
wait
"#,
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        let mut req = request("fixture task", "true");
        req.deadline_secs = 1;
        let started = Instant::now();
        let error = run_with_program(req, &root, &program)
            .unwrap_err()
            .to_string();
        assert!(error.contains("exceeded the 1s ensemble deadline"));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[cfg(unix)]
    #[test]
    fn normal_leader_exit_does_not_wait_on_descendant_capture_fds() {
        let root = temp_dir();
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 10 & exit 0"]);
        let started = Instant::now();
        let output = run_bounded(
            command,
            Duration::from_secs(2),
            MAX_ADAPTER_STDOUT,
            MAX_STDERR,
            &root,
        )
        .unwrap();
        assert!(output.status.success());
        assert!(!output.timed_out);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(fs::read_dir(root).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn pipe_capture_retains_only_the_configured_limit() {
        let root = temp_dir();
        let mut command = Command::new("sh");
        command.args(["-c", "dd if=/dev/zero bs=1048576 count=1 2>/dev/null"]);
        let output = run_bounded(command, Duration::from_secs(2), 4_096, 1_024, &root).unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.truncated);
        assert_eq!(output.stdout.bytes.len(), 4_096);
        assert!(output.stdout.total > 4_096);
    }

    #[cfg(unix)]
    #[test]
    fn pipe_capture_does_not_limit_child_artifacts() {
        let root = temp_dir();
        let artifact = root.join("large-artifact.bin");
        let mut command = Command::new("sh");
        command.args([
            "-c",
            &format!(
                "dd if=/dev/zero of={} bs=1048576 count=2 2>/dev/null; printf ok",
                shell_quote(&artifact.display().to_string())
            ),
        ]);
        let output = run_bounded(command, Duration::from_secs(2), 4_096, 1_024, &root).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout.bytes, b"ok");
        assert_eq!(fs::metadata(artifact).unwrap().len(), 2 * 1_048_576);
    }
}

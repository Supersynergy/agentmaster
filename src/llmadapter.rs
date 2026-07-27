use std::ffi::OsStr;
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

#[derive(Deserialize)]
struct AdapterOutput {
    ok: usize,
    results: Vec<AdapterResult>,
}

#[derive(Deserialize)]
struct AdapterResult {
    lane: String,
    ok: bool,
    answer: Option<String>,
}

#[derive(Deserialize)]
struct UsageOutput {
    completed: bool,
    failed: bool,
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

#[derive(Serialize)]
struct RunManifest {
    schema_version: u8,
    task_sha256: String,
    task_transport: &'static str,
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
    let no_cache = policy.remote || request.fresh;

    if request.dry_run {
        println!("ensemble dry-run");
        println!("  task sha256 : {task_hash}");
        println!("  transport   : argv (visible to same-host process inspection)");
        println!("  lanes       : {}", request.lanes);
        println!("  remote      : {}", policy.remote);
        println!("  paid        : {}", policy.paid);
        println!(
            "  token limit : requested ceiling {}; provider enforcement varies",
            request.max_tokens
        );
        println!(
            "  command     : {} ask <task> --swarm --lanes {} --max-tokens {} --timeout {} --json --usage-out <private-path>{}",
            program.display(),
            request.lanes,
            request.max_tokens,
            request.deadline_secs,
            if no_cache { " --no-cache" } else { "" }
        );
        return Ok(());
    }

    let safe_name = slug(request.name);
    let run_dir = create_run_dir(data_dir, &safe_name)?;
    let usage_path = run_dir.join("usage.json");
    create_private_file(&usage_path)?;
    let started = Instant::now();
    let mut evidence = ManifestGuard {
        path: run_dir.join("manifest.json"),
        started,
        document: RunManifest {
            schema_version: 1,
            task_sha256: task_hash.clone(),
            task_transport: "argv",
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
    command.args([
        OsStr::new("ask"),
        OsStr::new(request.task),
        OsStr::new("--swarm"),
        OsStr::new("--lanes"),
        OsStr::new(request.lanes),
        OsStr::new("--max-tokens"),
        OsStr::new(&request.max_tokens.to_string()),
        OsStr::new("--timeout"),
        OsStr::new(&request.deadline_secs.to_string()),
        OsStr::new("--json"),
        OsStr::new("--usage-out"),
        usage_path.as_os_str(),
    ]);
    if no_cache {
        command.arg("--no-cache");
    }
    if policy.remote {
        command.env("ATS_PII_SHIELD", "1");
    }
    eprintln!(
        "warning: llmadapter currently receives the task via argv; do not include secrets or PII"
    );

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
            "llmadapter failed: status={} stdout_bytes={} stderr_bytes={}{}",
            output.status,
            output.stdout.total,
            output.stderr.total,
            truncation_note(&output)
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
    validate_adapter_output(&parsed)?;
    validate_usage(&usage_path)?;
    evidence.document.usage_status = "COMPLETE";
    evidence.checkpoint()?;

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
        let passed = !oracle_output.timed_out && oracle_output.status.success();
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
        parsed.ok,
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

fn validate_adapter_output(output: &AdapterOutput) -> Result<()> {
    if output.results.is_empty() {
        bail!("llmadapter returned no results");
    }
    let valid = output
        .results
        .iter()
        .filter(|r| {
            r.ok && r
                .answer
                .as_deref()
                .is_some_and(|answer| !answer.trim().is_empty())
        })
        .count();
    if output.ok != valid || valid == 0 {
        bail!(
            "llmadapter result contract failed: ok={} valid_results={valid}",
            output.ok
        );
    }
    Ok(())
}

fn validate_usage(path: &Path) -> Result<()> {
    let size = fs::metadata(path)
        .context("llmadapter did not write the usage artifact")?
        .len();
    if size > MAX_USAGE_BYTES {
        bail!("llmadapter usage artifact exceeded {MAX_USAGE_BYTES} bytes");
    }
    let file = File::open(path).context("llmadapter did not write the usage artifact")?;
    let usage: UsageOutput =
        serde_json::from_reader(file).context("llmadapter wrote invalid usage JSON")?;
    if !usage.completed || usage.failed {
        bail!(
            "llmadapter usage is incomplete or failed (completed={}, failed={})",
            usage.completed,
            usage.failed
        );
    }
    Ok(())
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
    capture_dir: &Path,
) -> Result<Captured> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut stdout_file = private_capture_file(capture_dir, "stdout")?;
    let mut stderr_file = private_capture_file(capture_dir, "stderr")?;
    command
        .stdout(Stdio::from(stdout_file.try_clone()?))
        .stderr(Stdio::from(stderr_file.try_clone()?));
    let mut child = command.spawn().context("failed to start subprocess")?;
    let process_group = child.id();

    let start = Instant::now();
    let (status, timed_out) = loop {
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
    let stdout = read_capture(&mut stdout_file, stdout_limit)?;
    let stderr = read_capture(&mut stderr_file, stderr_limit)?;
    Ok(Captured {
        status,
        stdout,
        stderr,
        timed_out,
    })
}

fn private_capture_file(dir: &Path, label: &str) -> Result<File> {
    let path = dir.join(format!(
        ".capture-{label}-{}",
        uuid::Uuid::new_v4().as_hyphenated()
    ));
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&path)?;
    #[cfg(unix)]
    fs::remove_file(&path)?;
    #[cfg(not(unix))]
    {
        drop(file);
        let _ = fs::remove_file(&path);
        bail!("ensemble subprocess capture currently requires macOS or Linux");
    }
    #[cfg(unix)]
    Ok(file)
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

fn read_capture(file: &mut File, limit: usize) -> std::io::Result<CapturedStream> {
    let total = file.metadata()?.len().min(usize::MAX as u64) as usize;
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::with_capacity(total.min(limit));
    file.take(limit as u64).read_to_end(&mut bytes)?;
    Ok(CapturedStream {
        truncated: total > limit,
        bytes,
        total,
    })
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

    #[cfg(unix)]
    fn fake_adapter(root: &Path, lane: &str, usage: &str, extra_checks: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = root.join("llmadapter");
        let script = format!(
            r#"#!/bin/sh
set -eu
[ "$1" = ask ]
[ "$2" = "fixture task" ]
[ "$3" = --swarm ]
[ "$4" = --lanes ]
[ "$5" = "{lane}" ]
[ "$6" = --max-tokens ]
[ "$7" = 500 ]
[ "$8" = --timeout ]
[ "$9" = 2 ]
[ "${{10}}" = --json ]
[ "${{11}}" = --usage-out ]
{extra_checks}
printf '%s' '{usage}' > "${{12}}"
printf '%s' '{{"ok":3,"results":[{{"lane":"one","ok":true,"answer":"loser"}},{{"lane":"two","ok":true,"answer":"winner"}},{{"lane":"three","ok":true,"answer":"unused"}}]}}'
"#
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
    fn runs_exact_bounded_contract_and_stops_at_first_oracle_pass() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_dir();
        let program = fake_adapter(
            &root,
            "local",
            r#"{"completed":true,"failed":false}"#,
            r#"[ "$#" = 12 ]"#,
        );
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
        assert_eq!(manifest["task_transport"], "argv");
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
    fn rejects_incomplete_usage_before_oracle() {
        let root = temp_dir();
        let program = fake_adapter(
            &root,
            "local",
            r#"{"completed":false,"failed":false}"#,
            r#"[ "$#" = 12 ]"#,
        );
        let error = run_with_program(request("fixture task", "true"), &root, &program)
            .unwrap_err()
            .to_string();
        assert!(error.contains("usage is incomplete"));
        let run_dir = fs::read_dir(root.join("ensembles").join("proof-run"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(fs::read_dir(&run_dir).unwrap().count(), 2);
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
            r#"{"completed":true,"failed":false}"#,
            r#"[ "$#" = 13 ]
[ "${13}" = --no-cache ]
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
            r#"{"completed":true,"failed":false}"#,
            r#"[ "$#" = 13 ]
[ "${13}" = --no-cache ]"#,
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
        fs::write(&program, "#!/bin/sh\nsleep 10 &\nwait\n").unwrap();
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
}

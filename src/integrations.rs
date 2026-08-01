//! Cross-runtime inventory and explicit CLI bridges.
//!
//! AgentMaster owns fleet discovery and steering. Codex, Hermes, ggcoder,
//! GGAdapter, LLMadapter, and cmux keep ownership of their own command
//! contracts. This module only composes read-only probes and forwards an
//! operator-supplied command to an allow-listed binary.

use std::env;
use std::fs::{self, OpenOptions};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::backend;
use crate::native_control::{self, NativeCommand};
use crate::runtime;
use crate::store::Store;

const ADAPTER_PROBES: &[(&str, &str, &[&str])] = &[
    ("ggadapter-patches", "ggadapter", &["status"]),
    (
        "ggadapter-broker",
        "ggadapter",
        &["broker", "health", "--json"],
    ),
    ("llmadapter", "llmadapter", &["lanes"]),
];

/// Binaries that may be reached through `agentmaster control`.
///
/// This is deliberately an allow-list: the command is an explicit bridge to
/// existing CLIs, not a general shell evaluator.
pub const CONTROL_BINARIES: &[&str] = &[
    "cmux",
    "codex",
    "hermes",
    "ggcoder",
    "ggadapter",
    "llmadapter",
    "cac",
    "callagent",
];

fn executable_on_path(name: &str) -> Option<String> {
    if name.contains('/') {
        return Path::new(name).is_file().then(|| name.to_string());
    }
    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

fn compact_output(stdout: &[u8], stderr: &[u8]) -> String {
    let mut text = String::from_utf8_lossy(stdout).to_string();
    if text.trim().is_empty() {
        text = String::from_utf8_lossy(stderr).to_string();
    }
    let mut lines = text.lines().take(12).collect::<Vec<_>>().join("\n");
    if lines.chars().count() > 2_000 {
        lines = lines.chars().take(1_997).collect();
        lines.push('…');
    }
    lines
}

fn summary_degraded(summary: &str) -> bool {
    ["⚠", "❌", "drift", "stale/missing", "not-ready"]
        .iter()
        .any(|marker| summary.contains(marker))
}

fn broker_health_summary(stdout: &[u8]) -> (String, bool) {
    let Ok(value) = serde_json::from_slice::<Value>(stdout) else {
        return ("broker health returned non-JSON".to_string(), true);
    };
    let ok = value.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let version = value
        .get("ggcoderVersion")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let auth = value
        .get("auth")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let expired = auth
        .iter()
        .filter(|provider| {
            provider
                .get("expired")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .count();
    (
        format!(
            "broker={} ggcoder={} providers={} expired={expired}",
            if ok { "ok" } else { "FAIL" },
            version,
            auth.len()
        ),
        !ok || expired > 0,
    )
}

fn broker_sessions_summary(stdout: &[u8], cwd: &Path) -> (String, bool) {
    let Ok(value) = serde_json::from_slice::<Value>(stdout) else {
        return ("broker sessions returned non-JSON".to_string(), true);
    };
    let ok = value.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let sessions = value
        .get("sessions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let latest = sessions
        .first()
        .and_then(|session| session.get("mtime"))
        .and_then(Value::as_str)
        .unwrap_or("none");
    let project = cwd
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(".");
    (
        format!(
            "project={project} sessions={} latest={latest}",
            sessions.len()
        ),
        !ok,
    )
}

struct Captured {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn probe_path(stream: &str) -> std::path::PathBuf {
    env::temp_dir().join(format!(
        "agentmaster-probe-{}-{}-{stream}.log",
        std::process::id(),
        Uuid::new_v4()
    ))
}

fn kill_process_group(child: &mut Child) {
    #[cfg(unix)]
    {
        // The child is placed in its own process group below. Killing the
        // negative pid stops plugin/MCP descendants as well as the launcher.
        let _ = unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
    }
    let _ = child.kill();
}

/// Run a local probe without allowing a broken provider/plugin to wedge the
/// whole status command. Output is captured in private temp files, so verbose
/// CLI/plugin descendants cannot deadlock a pipe or wedge the status command.
fn run_bounded(binary: &str, args: &[&str], timeout: Duration) -> std::io::Result<Captured> {
    let stdout_path = probe_path("stdout");
    let stderr_path = probe_path("stderr");
    let stdout_file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&stdout_path)?;
    let stderr_file = match OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&stderr_path)
    {
        Ok(file) => file,
        Err(error) => {
            let _ = fs::remove_file(&stdout_path);
            return Err(error);
        }
    };
    let mut command = Command::new(binary);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(stdout_file)
        .stderr(stderr_file);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child: Child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = fs::remove_file(&stdout_path);
            let _ = fs::remove_file(&stderr_path);
            return Err(error);
        }
    };
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            kill_process_group(&mut child);
            let _ = child.wait();
            break child.try_wait()?.expect("killed child must have exited");
        }
        thread::sleep(Duration::from_millis(25));
    };
    let stdout = fs::read(&stdout_path).unwrap_or_default();
    let stderr = fs::read(&stderr_path).unwrap_or_default();
    let _ = fs::remove_file(&stdout_path);
    let _ = fs::remove_file(&stderr_path);
    if timed_out {
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("probe timed out after {} ms", timeout.as_millis()),
        ));
    }
    Ok(Captured {
        status,
        stdout,
        stderr,
    })
}

fn probe(name: &str, binary: &str, args: &[&str]) -> Value {
    let Some(path) = executable_on_path(binary) else {
        return json!({
            "name": name,
            "command": std::iter::once(binary)
                .chain(args.iter().copied())
                .collect::<Vec<_>>()
                .join(" "),
            "path": null,
            "available": false,
            "ok": false,
            "degraded": true,
            "exit_code": null,
            "summary": "not found on PATH"
        });
    };
    let command = std::iter::once(binary)
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ");
    match run_bounded(&path, args, Duration::from_secs(4)) {
        Ok(output) => {
            let (summary, broker_degraded) = if name == "ggadapter-broker" {
                broker_health_summary(&output.stdout)
            } else {
                (compact_output(&output.stdout, &output.stderr), false)
            };
            json!({
                "name": name,
                "command": command,
                "path": path,
                "available": true,
                "ok": output.status.success(),
                "degraded": summary_degraded(&summary) || broker_degraded,
                "exit_code": output.status.code(),
                "summary": summary
            })
        }
        Err(error) => json!({
            "name": name,
            "command": command,
            "path": path,
            "available": true,
            "ok": false,
            "degraded": true,
            "exit_code": null,
            "summary": format!("probe failed: {error}")
        }),
    }
}

fn runtime_version(path: &str) -> Option<String> {
    for args in [["--version"].as_slice(), ["-V"].as_slice()] {
        if let Ok(output) = run_bounded(path, args, Duration::from_secs(2))
            && output.status.success()
        {
            let version = compact_output(&output.stdout, &output.stderr);
            if let Some(first_line) = version.lines().map(str::trim).find(|line| !line.is_empty()) {
                return Some(first_line.to_string());
            }
        }
    }
    None
}

fn runtime_record(name: &'static str) -> Value {
    let path = executable_on_path(name);
    let version = path.as_ref().and_then(|path| runtime_version(path));
    json!({
        "name": name,
        "path": path,
        "available": path.is_some(),
        "version": version
    })
}

fn runtime_inventory() -> Vec<Value> {
    // Version probes are independent and some host CLIs can spend their full
    // timeout in plugin discovery. Keep one slow runtime from serialising the
    // entire inventory.
    let handles = runtime::KNOWN_RUNTIMES
        .iter()
        .map(|name| thread::spawn(move || runtime_record(name)))
        .collect::<Vec<_>>();
    handles
        .into_iter()
        .map(|handle| {
            handle
                .join()
                .unwrap_or_else(|_| json!({"available": false}))
        })
        .collect()
}

/// Print the runtime part of `agentmaster doctor` using the same bounded
/// version probes as `status`.
pub fn runtime_doctor(verbose: bool) {
    if !verbose {
        for name in runtime::KNOWN_RUNTIMES {
            println!(
                "  runtime {name:<8}: {}",
                if executable_on_path(name).is_some() {
                    "found"
                } else {
                    "absent"
                }
            );
        }
        return;
    }
    for item in runtime_inventory() {
        let name = item.get("name").and_then(Value::as_str).unwrap_or("?");
        let available = item
            .get("available")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !available {
            println!("  runtime {name:<8}: absent");
            continue;
        }
        let version = item
            .get("version")
            .and_then(Value::as_str)
            .filter(|version| !version.trim().is_empty())
            .unwrap_or("(no --version)");
        println!("  runtime {name:<8}: found  {version}");
    }
}

fn adapter_inventory() -> Vec<Value> {
    let mut handles = ADAPTER_PROBES
        .iter()
        .map(|(name, binary, args)| thread::spawn(move || probe(name, binary, args)))
        .collect::<Vec<_>>();
    handles.push(thread::spawn(ggadapter_sessions_probe));
    handles
        .into_iter()
        .map(|handle| {
            handle
                .join()
                .unwrap_or_else(|_| json!({"available": false}))
        })
        .collect()
}

fn ggadapter_sessions_probe() -> Value {
    let Some(path) = executable_on_path("ggadapter") else {
        return json!({
            "name": "ggadapter-sessions",
            "command": "ggadapter broker sessions",
            "path": null,
            "available": false,
            "ok": false,
            "degraded": true,
            "exit_code": null,
            "summary": "not found on PATH"
        });
    };
    let cwd = env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());
    let cwd_text = cwd.to_string_lossy().into_owned();
    let args = [
        "broker",
        "sessions",
        "--cwd",
        cwd_text.as_str(),
        "--limit",
        "5",
        "--json",
    ];
    match run_bounded(&path, &args, Duration::from_secs(4)) {
        Ok(output) => {
            let (summary, malformed) = broker_sessions_summary(&output.stdout, &cwd);
            json!({
                "name": "ggadapter-sessions",
                "command": "ggadapter broker sessions --cwd <current> --limit 5 --json",
                "path": path,
                "available": true,
                "ok": output.status.success(),
                "degraded": malformed,
                "exit_code": output.status.code(),
                "summary": summary
            })
        }
        Err(error) => json!({
            "name": "ggadapter-sessions",
            "command": "ggadapter broker sessions --cwd <current> --limit 5 --json",
            "path": path,
            "available": true,
            "ok": false,
            "degraded": true,
            "exit_code": null,
            "summary": format!("probe failed: {error}")
        }),
    }
}

fn tmux_inventory() -> (Vec<Value>, Value) {
    match backend::list_panes_checked() {
        Ok(panes) => (
            panes
                .into_iter()
                .map(|pane| {
                    json!({
                        "backend": "tmux",
                        "ref": pane.target,
                        "command": pane.command,
                        "pid": pane.pid,
                        "cwd": pane.path,
                        "title": pane.title
                    })
                })
                .collect(),
            json!({"available": true, "reachable": true, "detail": "tmux pane inventory available"}),
        ),
        Err(error) => (
            Vec::new(),
            json!({
                "available": backend::tmux_available(),
                "reachable": false,
                "detail": error.to_string()
            }),
        ),
    }
}

fn cmux_inventory() -> (Vec<Value>, Value) {
    match backend::list_cmux_checked() {
        Ok(workspaces) => (
            workspaces
                .into_iter()
                .map(|workspace| {
                    json!({
                        "backend": "cmux",
                        "ref": workspace.ws_ref,
                        "runtime": workspace.runtime,
                        "status": workspace.status,
                        "pid": workspace.pid,
                        "git": workspace.git,
                        "phase": workspace.phase,
                        "title": workspace.title
                    })
                })
                .collect(),
            json!({"available": true, "reachable": true, "detail": "cmux workspace inventory available"}),
        ),
        Err(error) => (
            Vec::new(),
            json!({
                "available": executable_on_path("cmux").is_some(),
                "reachable": false,
                "detail": error.to_string()
            }),
        ),
    }
}

fn native_inventory(dir: &Path) -> (Vec<Value>, Value) {
    match native_control::request(dir, NativeCommand::List) {
        Ok(response) if response.ok => (
            response
                .agents
                .into_iter()
                .map(|agent| {
                    json!({
                        "backend": "native",
                        "ref": agent.reference,
                        "runtime": agent.runtime,
                        "status": agent.status,
                        "pid": agent.pid,
                        "cwd": agent.cwd,
                        "goal": agent.goal,
                        "progress": agent.progress,
                        "last_line": agent.last_line,
                        "lines_total": agent.lines_total,
                        "title": agent.name
                    })
                })
                .collect(),
            json!({"active": true, "detail": "native TUI control socket active"}),
        ),
        Ok(response) => (
            Vec::new(),
            json!({
                "active": false,
                "detail": response.message.unwrap_or_else(|| "native TUI returned an error".into())
            }),
        ),
        Err(error) if native_control::is_inactive_error(&error) => (
            Vec::new(),
            json!({"active": false, "detail": "no AgentMaster TUI owns native PTYs"}),
        ),
        Err(error) => (
            Vec::new(),
            json!({"active": false, "detail": error.to_string()}),
        ),
    }
}

fn goals_inventory(dir: &Path) -> Vec<Value> {
    Store::open(&dir.join("agentmaster.db"))
        .map(|store| {
            store
                .load_goals()
                .into_iter()
                .map(|(name, goal, dod, progress)| {
                    json!({
                        "name": name,
                        "goal": goal,
                        "dod": dod,
                        "progress": progress
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Print one read-only cross-runtime snapshot.
pub fn status(json_output: bool, dir: &Path) -> Result<()> {
    let (native, native_backend) = native_inventory(dir);
    let (tmux, tmux_backend) = tmux_inventory();
    let (cmux, cmux_backend) = cmux_inventory();
    let adapters = adapter_inventory();
    let runtimes = runtime_inventory();
    let goals = goals_inventory(dir);

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "fleet": {"native": native, "tmux": tmux, "cmux": cmux},
                "backends": {
                    "native": native_backend,
                    "tmux": tmux_backend,
                    "cmux": cmux_backend
                },
                "runtimes": runtimes,
                "adapters": adapters,
                "goals": goals
            }))?
        );
        return Ok(());
    }

    println!("agentmaster status");
    println!(
        "  fleet: {} native · {} tmux · {} cmux",
        native.len(),
        tmux.len(),
        cmux.len()
    );
    for item in native.iter().chain(tmux.iter()).chain(cmux.iter()) {
        let reference = item.get("ref").and_then(Value::as_str).unwrap_or("?");
        let runtime = item
            .get("runtime")
            .and_then(Value::as_str)
            .or_else(|| item.get("command").and_then(Value::as_str))
            .unwrap_or("?");
        let status = item.get("status").and_then(Value::as_str).unwrap_or("live");
        let title = item.get("title").and_then(Value::as_str).unwrap_or("");
        println!("    {reference:<30} {runtime:<10} {status:<14} {title}");
    }
    println!("  backends:");
    for (name, backend) in [
        ("native", &native_backend),
        ("tmux", &tmux_backend),
        ("cmux", &cmux_backend),
    ] {
        let active = backend
            .get("active")
            .or_else(|| backend.get("reachable"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let detail = backend.get("detail").and_then(Value::as_str).unwrap_or("");
        println!(
            "    {name:<12} {} {detail}",
            if active { "ok" } else { "unavailable" }
        );
    }

    println!("  adapters:");
    for adapter in &adapters {
        let name = adapter.get("name").and_then(Value::as_str).unwrap_or("?");
        let ok = adapter.get("ok").and_then(Value::as_bool).unwrap_or(false);
        let degraded = adapter
            .get("degraded")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let summary = adapter
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or("")
            .lines()
            .next()
            .unwrap_or("");
        let state = if !ok {
            "FAIL"
        } else if degraded {
            "DEGRADED"
        } else {
            "ok"
        };
        println!("    {name:<12} {state} {summary}");
    }

    let found = runtimes
        .iter()
        .filter(|runtime| {
            runtime
                .get("available")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .count();
    println!("  runtimes: {found}/{} available", runtimes.len());
    println!("  goals: {} stored", goals.len());
    Ok(())
}

/// Print adapter checks as part of `agentmaster doctor`.
pub fn doctor(dir: &Path) {
    for result in adapter_inventory() {
        let name = result.get("name").and_then(Value::as_str).unwrap_or("?");
        let state = if result.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            if result
                .get("degraded")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                "DEGRADED"
            } else {
                "ok"
            }
        } else if result
            .get("available")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            "FAIL"
        } else {
            "absent"
        };
        let summary = result
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or("")
            .lines()
            .next()
            .unwrap_or("");
        println!("  adapter {name:<18}: {state} {summary}");
    }
    let (_, native) = native_inventory(dir);
    let active = native
        .get("active")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let detail = native.get("detail").and_then(Value::as_str).unwrap_or("");
    println!(
        "  native control: {} {detail}",
        if active { "ok" } else { "inactive" }
    );
}

/// Forward an explicit command to one allow-listed local control surface.
pub fn control(target: &str, args: &[String]) -> Result<()> {
    let target = match target {
        "gg-adapter" => "ggadapter",
        "llm-adapter" => "llmadapter",
        other => other,
    };
    if !CONTROL_BINARIES.contains(&target) {
        bail!(
            "unsupported control target '{target}'; allowed: {}",
            CONTROL_BINARIES.join(", ")
        );
    }
    if args.is_empty() {
        let default_args: &[&str] = match target {
            "ggadapter" => &["status"],
            "llmadapter" => &["doctor"],
            "cmux" => &["capabilities"],
            _ => bail!("control {target} requires an explicit subcommand/argument"),
        };
        let status = Command::new(target).args(default_args).status()?;
        if !status.success() {
            bail!("{target} exited with {}", status.code().unwrap_or(1));
        }
        return Ok(());
    }
    let status = Command::new(target).args(args).status()?;
    if !status.success() {
        bail!("{target} exited with {}", status.code().unwrap_or(1));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_output_prefers_stdout_and_caps_lines() {
        let stdout = b"first\nsecond\nthird";
        assert_eq!(compact_output(stdout, b"stderr"), "first\nsecond\nthird");
        let noisy = vec![b'x'; 3_000];
        assert!(compact_output(&noisy, b"").chars().count() <= 2_000);
    }

    #[test]
    fn control_allowlist_contains_the_cross_runtime_surfaces() {
        for target in [
            "cmux",
            "codex",
            "hermes",
            "ggcoder",
            "ggadapter",
            "llmadapter",
        ] {
            assert!(CONTROL_BINARIES.contains(&target), "missing {target}");
        }
    }

    #[test]
    fn adapter_warning_markers_are_degraded_not_hidden_as_healthy() {
        assert!(summary_degraded("patches: ⚠️ drift"));
        assert!(summary_degraded("history: stale/missing sessionPath"));
        assert!(!summary_degraded("all lanes available"));
    }

    #[test]
    fn broker_health_summary_redacts_account_details_and_surfaces_expiry() {
        let raw = br#"{"ok":true,"ggcoderVersion":"5.30.3","auth":[{"provider":"openai","expired":true,"accountId":"secret"}]}"#;
        let (summary, degraded) = broker_health_summary(raw);
        assert!(degraded);
        assert!(summary.contains("ggcoder=5.30.3"));
        assert!(summary.contains("expired=1"));
        assert!(!summary.contains("secret"));
    }

    #[test]
    fn broker_session_summary_keeps_only_count_project_and_mtime() {
        let raw = br#"{"ok":true,"sessions":[{"id":"secret-session","file":"/secret/path","mtime":"2026-08-01T12:00:00Z"}]}"#;
        let (summary, degraded) = broker_sessions_summary(raw, Path::new("/tmp/project"));
        assert!(!degraded);
        assert!(summary.contains("project=project sessions=1"));
        assert!(summary.contains("latest=2026-08-01T12:00:00Z"));
        assert!(!summary.contains("secret-session"));
        assert!(!summary.contains("/secret/path"));
    }
}

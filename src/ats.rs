//! Agent-Token-Saver (ATS) integration layer.
//!
//! Wraps the external ATS CLIs (`synx`, `si`, `agent-token-ledger`,
//! `kimi-worker`) behind a fail-open Rust API. Every function degrades
//! gracefully: if the CLI is missing or errors, the swarm still runs —
//! we just lose the brain-learning / skill-routing / token-tracking
//! side-channels. Ponytail: no new state, no framework, just shells.
//!
//! All paths live under `~/.agentmaster/{capsules,ledgers,share}/` so
//! the swarm's artifacts are introspectable from outside the process.

use std::path::{Path, PathBuf};
use std::process::Command;

/// `~/.agentmaster` — data dir shared with the rest of agentmaster.
fn am_home() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".agentmaster")
}

/// Per-lane capsule path: `~/.agentmaster/capsules/<swarm>-lane<i>-<model>.md`
pub fn capsule_path(swarm: &str, lane: usize, model_name: &str) -> PathBuf {
    am_home()
        .join("capsules")
        .join(format!("{swarm}-lane{lane}-{model_name}.md"))
}

/// Per-lane evidence path: `~/.agentmaster/capsules/<swarm>-lane<i>-<model>-evidence.md`
pub fn evidence_path(swarm: &str, lane: usize, model_name: &str) -> PathBuf {
    am_home()
        .join("capsules")
        .join(format!("{swarm}-lane{lane}-{model_name}-evidence.md"))
}

/// Per-lane token ledger path: `~/.agentmaster/ledgers/<swarm>-lane<i>-<model>.jsonl`
pub fn ledger_path(swarm: &str, lane: usize, model_name: &str) -> PathBuf {
    am_home()
        .join("ledgers")
        .join(format!("{swarm}-lane{lane}-{model_name}.jsonl"))
}

/// Per-lane share dir (KIMI_WORKER_SHARE_DIR isolation): `~/.agentmaster/share/<swarm>-lane<i>`
pub fn share_dir(swarm: &str, lane: usize) -> PathBuf {
    am_home().join("share").join(format!("{swarm}-lane{lane}"))
}

/// Ensure all per-lane directories exist. Call once before spawning a lane.
pub fn ensure_lane_dirs(swarm: &str, lane: usize, model_name: &str) -> std::io::Result<()> {
    for p in [
        capsule_path(swarm, lane, model_name)
            .parent()
            .unwrap_or(Path::new(".")),
        evidence_path(swarm, lane, model_name)
            .parent()
            .unwrap_or(Path::new(".")),
        ledger_path(swarm, lane, model_name)
            .parent()
            .unwrap_or(Path::new(".")),
        &share_dir(swarm, lane),
    ] {
        std::fs::create_dir_all(p)?;
    }
    Ok(())
}

/// `synx hybrid "<topic>" 8` — recall prior context from the Synapse brain.
/// Fail-open: returns empty string on any error (missing binary, non-zero
/// exit, UTF-8 failure). The caller treats empty as "no recall".
pub fn prime(topic: &str) -> String {
    let out = Command::new("synx").args(["hybrid", topic, "8"]).output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => String::new(),
    }
}

/// `echo "<body>" | synx put --title "<title>"` — persist a decision/fact
/// to the Synapse brain. Fail-open: errors are logged to stderr but do
/// not propagate. Returns true on success.
pub fn remember(title: &str, body: &str) -> bool {
    let title_arg = format!("--title={title}");
    let mut cmd = Command::new("synx");
    cmd.args(["put", &title_arg]);
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    match cmd.spawn() {
        Ok(mut child) => {
            use std::io::Write;
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(body.as_bytes());
            }
            matches!(child.wait(), Ok(s) if s.success())
        }
        Err(_) => false,
    }
}

/// Route the real task once, not every model label. Strict JSON prevents an
/// accidental keyword match from adding an unrelated skill to every lane.
/// Fail-open: no verified single path means no skill.
pub fn route_skill(task: &str) -> Option<String> {
    let out = Command::new("si")
        .args(["route", task, "--strict", "--json"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let path = value
        .get("selected")?
        .as_array()?
        .first()?
        .get("path")?
        .as_str()?;
    Path::new(path).is_file().then(|| path.to_string())
}

/// A compact, printable recall preview. Raw Synapse hits stay out of lane
/// capsules and controller output remains bounded.
pub fn preview(text: &str, max_chars: usize) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= max_chars {
        compact
    } else {
        format!("{}…", compact.chars().take(max_chars).collect::<String>())
    }
}

/// Read a lane's token usage from its `KIMI_WORKER_USAGE_OUT` JSONL ledger.
/// Returns (input_tokens, output_tokens). Fail-open: returns (0, 0) if the
/// file is missing or unparseable. Only kimi-worker writes this file today;
/// other runtimes have no token ledger, so this is a lower bound.
pub fn read_ledger_tokens(path: &Path) -> (usize, usize) {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return (0, 0),
    };
    let mut input = 0usize;
    let mut output = 0usize;
    for line in content.lines() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            // kimi-worker emits the canonical ledger record under `usage`;
            // accept top-level records too for compatible providers.
            let usage = v.get("usage").unwrap_or(&v);
            input += usage
                .get("input_tokens")
                .and_then(|n| n.as_u64())
                .unwrap_or(0) as usize;
            input += usage
                .get("cache_creation_input_tokens")
                .and_then(|n| n.as_u64())
                .unwrap_or(0) as usize;
            input += usage
                .get("cache_read_input_tokens")
                .and_then(|n| n.as_u64())
                .unwrap_or(0) as usize;
            output += usage
                .get("output_tokens")
                .and_then(|n| n.as_u64())
                .unwrap_or(0) as usize;
        }
    }
    (input, output)
}

/// Whether a runtime currently writes complete per-lane usage records.
/// Budgets must never be enforced from a zero-valued lower bound.
pub fn runtime_has_complete_usage(runtime: &str) -> bool {
    runtime == "kimi-worker"
}

/// Check whether the canonical token-ledger CLI is available before enabling
/// a hard budget. The individual lane files still hold the measured values.
pub fn canonical_ledger_available() -> bool {
    Command::new("agent-token-ledger")
        .arg("--help")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Write a swarm-result entry to the Synapse brain. Structured as
/// `swarm-result:<name> | task=<task> | winner=<model> | oracle=<oracle> | tokens=<n> | cost=$<x>`
/// so `ats_swarm_recall` can parse it back out of `synx hybrid` results.
pub fn swarm_remember(
    swarm: &str,
    task: &str,
    winner_model: &str,
    oracle: &str,
    tokens: usize,
    cost_usd: f64,
) -> bool {
    let title = format!(
        "swarm-result-{swarm}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    );
    let body = format!(
        "swarm-result:{swarm} | task={task} | winner={winner_model} | oracle={oracle} | tokens={tokens} | cost=${cost_usd:.4}"
    );
    remember(&title, &body)
}

/// Recall a prior swarm result for a similar task. Returns the first
/// `swarm-result:` line found in `synx hybrid <keywords> 8` output, or
/// None on any error. Ponytail: we don't rank hits — first match wins.
pub fn swarm_recall(task: &str) -> Option<SwarmMemoryHit> {
    let keywords = task
        .split_whitespace()
        .take(6)
        .collect::<Vec<_>>()
        .join(" ");
    let out = Command::new("synx")
        .args(["hybrid", &keywords, "8"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    for line in s.lines() {
        if let Some(hit) = SwarmMemoryHit::parse(line) {
            return Some(hit);
        }
    }
    None
}

/// A parsed `swarm-result:` line from the Synapse brain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwarmMemoryHit {
    pub swarm: String,
    pub task: String,
    pub winner: String,
    pub tokens: usize,
    pub cost_usd_milli: usize, // store as integer millis to keep Eq
}

impl SwarmMemoryHit {
    fn parse(line: &str) -> Option<Self> {
        let prefix = "swarm-result:";
        let body = line.trim().find(prefix)?;
        let body = &line[body + prefix.len()..];
        let mut swarm = String::new();
        let mut task = String::new();
        let mut winner = String::new();
        let mut tokens = 0usize;
        let mut cost_milli = 0usize;
        for part in body.split('|') {
            let part = part.trim();
            if let Some((k, v)) = part.split_once('=') {
                let k = k.trim();
                let v = v.trim();
                match k {
                    "task" => task = v.to_string(),
                    "winner" => winner = v.to_string(),
                    "tokens" => tokens = v.parse().unwrap_or(0),
                    "cost" => {
                        // strip leading $ and parse as f64 → millis
                        let v = v.trim_start_matches('$');
                        if let Ok(f) = v.parse::<f64>() {
                            cost_milli = (f * 1000.0).round() as usize;
                        }
                    }
                    _ => {}
                }
            } else if swarm.is_empty() {
                // leading `swarm-result:<name>` before the first `|`
                swarm = part.to_string();
            }
        }
        if winner.is_empty() {
            return None;
        }
        Some(Self {
            swarm,
            task,
            winner,
            tokens,
            cost_usd_milli: cost_milli,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_namespaced_under_am_home() {
        let c = capsule_path("ship", 0, "kimi-k3");
        assert!(
            c.to_string_lossy()
                .contains(".agentmaster/capsules/ship-lane0-kimi-k3.md")
        );
        let e = evidence_path("ship", 2, "claude-fable-5");
        assert!(
            e.to_string_lossy()
                .contains(".agentmaster/capsules/ship-lane2-claude-fable-5-evidence.md")
        );
        let l = ledger_path("ship", 1, "qwen-3-coder");
        assert!(
            l.to_string_lossy()
                .contains(".agentmaster/ledgers/ship-lane1-qwen-3-coder.jsonl")
        );
        let s = share_dir("ship", 3);
        assert!(
            s.to_string_lossy()
                .contains(".agentmaster/share/ship-lane3")
        );
    }

    #[test]
    fn ensure_lane_dirs_creates_all_four() {
        let tmp = std::env::temp_dir().join(format!("am-ats-test-{}", uuid::Uuid::new_v4()));
        // SAFETY: test is single-threaded; env var mutation is safe here.
        unsafe {
            std::env::set_var("HOME", &tmp);
        }
        ensure_lane_dirs("testswarm", 0, "kimi-k3").unwrap();
        assert!(tmp.join(".agentmaster/capsules").exists());
        assert!(tmp.join(".agentmaster/ledgers").exists());
        assert!(tmp.join(".agentmaster/share/testswarm-lane0").exists());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn prime_fails_open_on_missing_binary() {
        // synx is almost certainly not on PATH in unit-test env — fail-open.
        let s = prime("nonexistent-topic-xyz");
        // Either empty (binary missing) or some text (binary present) —
        // but never panics.
        assert!(s.len() < 10_000);
    }

    #[test]
    fn route_skill_fails_open_on_unrelated_query() {
        let v = route_skill("nonexistent task phrase xyz");
        assert!(v.as_deref().is_none_or(|path| Path::new(path).is_file()));
    }

    #[test]
    fn preview_is_single_line_and_bounded() {
        assert_eq!(preview("one\n two\tthree", 20), "one two three");
        assert_eq!(preview("abcdefghijkl", 5), "abcde…");
    }

    #[test]
    fn read_ledger_tokens_returns_zero_on_missing_file() {
        let (i, o) = read_ledger_tokens(Path::new("/nonexistent/path/xyz.jsonl"));
        assert_eq!(i, 0);
        assert_eq!(o, 0);
    }

    #[test]
    fn read_ledger_tokens_parses_jsonl() {
        let tmp = std::env::temp_dir().join(format!("am-ledger-{}.jsonl", uuid::Uuid::new_v4()));
        std::fs::write(&tmp, "{\"input_tokens\":100,\"output_tokens\":50}\n{\"input_tokens\":200,\"output_tokens\":75}\n").unwrap();
        let (i, o) = read_ledger_tokens(&tmp);
        assert_eq!(i, 300);
        assert_eq!(o, 125);
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn read_ledger_tokens_parses_kimi_worker_usage_shape() {
        let tmp =
            std::env::temp_dir().join(format!("am-kimi-ledger-{}.jsonl", uuid::Uuid::new_v4()));
        std::fs::write(
            &tmp,
            "{\"usage\":{\"input_tokens\":100,\"cache_creation_input_tokens\":20,\"cache_read_input_tokens\":30,\"output_tokens\":50}}\n",
        )
        .unwrap();
        let (i, o) = read_ledger_tokens(&tmp);
        assert_eq!(i, 150);
        assert_eq!(o, 50);
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn only_metered_runtime_can_enforce_a_hard_budget() {
        assert!(runtime_has_complete_usage("kimi-worker"));
        assert!(!runtime_has_complete_usage("codex"));
        assert!(!runtime_has_complete_usage("claude"));
    }

    #[test]
    fn swarm_memory_hit_parses_well_formed_line() {
        let line = "swarm-result:ship | task=run cargo test | winner=kimi-k3 | oracle=cargo test | tokens=1234 | cost=$0.0050";
        let hit = SwarmMemoryHit::parse(line).expect("must parse");
        assert_eq!(hit.swarm, "ship");
        assert_eq!(hit.task, "run cargo test");
        assert_eq!(hit.winner, "kimi-k3");
        assert_eq!(hit.tokens, 1234);
        assert_eq!(hit.cost_usd_milli, 5);
    }

    #[test]
    fn swarm_memory_hit_rejects_line_without_winner() {
        let line = "swarm-result:ship | task=foo | oracle=bar | tokens=10 | cost=$0.01";
        assert!(SwarmMemoryHit::parse(line).is_none());
    }
}

//! Route a task to an agent that is already warm.
//!
//! `assign` always cold-starts a workspace. That throws away two things the
//! fleet already paid for: the repo context the agent has read, and the
//! provider's prompt cache for that conversation prefix. Anthropic's prompt
//! cache holds for about five minutes by default, so an agent that answered a
//! minute ago is materially cheaper to talk to than a fresh one — and it is the
//! same agent, so nothing has to be re-explained.
//!
//! This module only ranks and reports; delivery stays in `cli::send`. Ranking
//! is a pure function over observed state, so it is unit-testable and costs no
//! tokens: cwd, title, status and transcript mtime are all read off the host.

use anyhow::Result;
use std::collections::HashSet;
use std::path::Path;
use std::time::SystemTime;

use crate::backend;

/// Anthropic's default prompt-cache window. Inside it, reusing the same
/// conversation is cheap; outside, the prefix is re-read at full price.
const CACHE_WINDOW_SECS: u64 = 300;
/// Below this nothing is a better bet than a clean cold start.
const DEFAULT_MIN_SCORE: i32 = 25;

/// What routing decided to do. Split out so the policy is testable without a
/// live fleet or a spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// A warm agent won; send to it.
    Reuse(String),
    /// Nothing was warm enough; cold-start this runtime.
    ColdStart(String),
    /// Nothing was warm enough and no fallback was offered; report only.
    Report,
}

pub fn decide(ranked: &[Scored], or_assign: Option<&str>, send: bool) -> Decision {
    match (ranked.first(), or_assign) {
        (Some(winner), _) if send || or_assign.is_some() => {
            Decision::Reuse(winner.candidate.reference.clone())
        }
        (None, Some(runtime)) => Decision::ColdStart(runtime.to_string()),
        _ => Decision::Report,
    }
}

pub struct RouteRequest<'a> {
    pub task: &'a str,
    pub send: bool,
    /// Cold-start this runtime when nothing is warm enough. Turns routing into
    /// one command that always takes the cheapest available path.
    pub or_assign: Option<&'a str>,
    /// Consider agents that look busy. Off by default: interrupting a working
    /// agent trades its context for yours.
    pub include_busy: bool,
    pub min_score: i32,
    pub json: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub reference: String,
    pub backend: &'static str,
    pub title: String,
    pub cwd: Option<String>,
    pub busy: bool,
    /// Seconds since this agent last produced output, when observable.
    pub idle_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scored {
    pub candidate: Candidate,
    pub score: i32,
    pub why: Vec<String>,
}

pub fn run(request: RouteRequest<'_>, dir: &Path) -> Result<()> {
    if request.task.trim().is_empty() {
        anyhow::bail!("route needs a task");
    }
    let cwd = std::env::current_dir()
        .ok()
        .map(|path| path.display().to_string());
    let (candidates, skipped_self) = discover();
    if skipped_self > 0 {
        println!("(skipped {skipped_self} workspace(s): this session)");
    }
    let mut ranked = rank(
        &candidates,
        request.task,
        cwd.as_deref(),
        request.include_busy,
    );
    ranked.retain(|scored| scored.score >= request.min_score);

    if request.json {
        let rows: Vec<String> = ranked
            .iter()
            .map(|scored| {
                format!(
                    "{{\"ref\":{},\"backend\":\"{}\",\"score\":{},\"busy\":{},\"idle_secs\":{},\"why\":{}}}",
                    json_string(&scored.candidate.reference),
                    scored.candidate.backend,
                    scored.score,
                    scored.candidate.busy,
                    scored
                        .candidate
                        .idle_secs
                        .map(|secs| secs.to_string())
                        .unwrap_or_else(|| "null".into()),
                    json_string(&scored.why.join("; ")),
                )
            })
            .collect();
        println!("[{}]", rows.join(","));
        return Ok(());
    }

    if ranked.is_empty() {
        println!("no warm agent scored at least {}", request.min_score);
        if let Decision::ColdStart(runtime) = decide(&ranked, request.or_assign, request.send) {
            println!("  cold start: {runtime}");
            let reference = crate::cli::assign_with_model(&runtime, None, request.task, None, dir)?;
            println!("  spawned   : {reference}");
            return Ok(());
        }
        println!(
            "  cold start: agentmaster auto {}",
            shell_quote(request.task)
        );
        return Ok(());
    }

    println!("warm candidates for: {}", request.task);
    for scored in &ranked {
        println!(
            "  [{score:>3}] {reference:<16} {window:<5} {title}",
            score = scored.score,
            reference = scored.candidate.reference,
            window = cache_window(scored.candidate.idle_secs),
            title = truncate(&scored.candidate.title, 48),
        );
        println!("        why: {}", scored.why.join(", "));
    }

    let winner = &ranked[0];
    if request.send || request.or_assign.is_some() {
        crate::cli::send(&winner.candidate.reference, request.task, dir)?;
        println!("sent to {}", winner.candidate.reference);
        return Ok(());
    }
    println!(
        "\nsend it:  agentmaster send {} {}",
        winner.candidate.reference,
        shell_quote(request.task)
    );
    println!(
        "or route+send in one shot:  agentmaster route --send {}",
        shell_quote(request.task)
    );
    Ok(())
}

/// `warm` means inside the provider's prompt-cache window; `cold` means the
/// prefix will be re-read. Unknown when the host gives us no transcript.
fn cache_window(idle_secs: Option<u64>) -> &'static str {
    match idle_secs {
        Some(secs) if secs <= CACHE_WINDOW_SECS => "warm",
        Some(_) => "cold",
        None => "?",
    }
}

/// Every process between us and init. A workspace whose pid is in here IS this
/// session: routing a task to it would hand the work back to ourselves.
fn ancestor_pids() -> HashSet<u32> {
    let mut out = HashSet::new();
    let mut pid = std::process::id();
    for _ in 0..32 {
        if !out.insert(pid) {
            break;
        }
        match parent_pid(pid) {
            Some(parent) if parent > 1 => pid = parent,
            _ => break,
        }
    }
    out
}

fn parent_pid(pid: u32) -> Option<u32> {
    let output = std::process::Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

fn discover() -> (Vec<Candidate>, usize) {
    let mut out = Vec::new();
    let transcripts = crate::peek::pid_transcripts();
    let ancestors = ancestor_pids();
    let mut skipped_self = 0;
    let now = SystemTime::now();

    if backend::cmux_available() {
        for workspace in backend::list_cmux() {
            if workspace.pid.is_some_and(|pid| ancestors.contains(&pid)) {
                skipped_self += 1;
                continue;
            }
            let idle_secs = workspace
                .pid
                .and_then(|pid| transcripts.get(&pid))
                .and_then(|path| std::fs::metadata(path).ok())
                .and_then(|meta| meta.modified().ok())
                .and_then(|modified| now.duration_since(modified).ok())
                .map(|elapsed| elapsed.as_secs());
            out.push(Candidate {
                reference: workspace.ws_ref.clone(),
                backend: "cmux",
                title: workspace.title.clone(),
                cwd: None,
                busy: is_busy(&workspace.status),
                idle_secs,
            });
        }
    }
    if backend::tmux_available() {
        for pane in backend::list_panes() {
            if !is_agent_command(&pane.command) {
                continue;
            }
            out.push(Candidate {
                reference: pane.target.clone(),
                backend: "tmux",
                title: pane.title.clone(),
                cwd: Some(pane.path.clone()),
                // A pane reports its foreground command, not whether the agent
                // is mid-answer. Unknown is treated as available, not idle.
                busy: false,
                idle_secs: None,
            });
        }
    }
    (out, skipped_self)
}

fn is_agent_command(command: &str) -> bool {
    let command = command.to_lowercase();
    [
        "claude", "codex", "hermes", "ggcoder", "aider", "opencode", "gemini", "cline", "cursor",
        "devin",
    ]
    .iter()
    .any(|agent| command.contains(agent))
}

fn is_busy(status: &str) -> bool {
    let status = status.to_lowercase();
    ["working", "running", "busy", "thinking"]
        .iter()
        .any(|marker| status.contains(marker))
}

/// Pure ranking so the policy is testable without a live fleet.
pub fn rank(
    candidates: &[Candidate],
    task: &str,
    cwd: Option<&str>,
    include_busy: bool,
) -> Vec<Scored> {
    let task_words = words(task);
    let mut scored: Vec<Scored> = candidates
        .iter()
        .filter(|candidate| include_busy || !candidate.busy)
        .map(|candidate| {
            let mut score = 0;
            let mut why = Vec::new();

            match (candidate.cwd.as_deref(), cwd) {
                (Some(agent_cwd), Some(here)) if agent_cwd == here => {
                    score += 40;
                    why.push("same directory".to_string());
                }
                (Some(agent_cwd), Some(here))
                    if agent_cwd.starts_with(here) || here.starts_with(agent_cwd) =>
                {
                    score += 20;
                    why.push("same project tree".to_string());
                }
                _ => {}
            }

            let overlap = overlap_score(&task_words, &words(&candidate.title));
            if overlap > 0 {
                score += overlap;
                why.push(format!("topic overlap {overlap}"));
            }

            match candidate.idle_secs {
                Some(secs) if secs <= CACHE_WINDOW_SECS => {
                    score += 20;
                    why.push(format!("inside the {CACHE_WINDOW_SECS}s cache window"));
                }
                Some(secs) if secs <= 3600 => {
                    score += 8;
                    why.push("active within the hour".to_string());
                }
                _ => {}
            }

            if candidate.busy {
                score -= 25;
                why.push("busy — sending now interrupts it".to_string());
            }
            if why.is_empty() {
                why.push("no affinity signal".to_string());
            }
            Scored {
                candidate: candidate.clone(),
                score,
                why,
            }
        })
        .collect();
    scored.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.candidate.reference.cmp(&b.candidate.reference))
    });
    scored
}

/// Up to 30 points for shared vocabulary between the task and what the agent is
/// already doing. Deliberately crude: this picks a conversation to continue, it
/// does not pretend to understand either text.
fn overlap_score(task: &[String], title: &[String]) -> i32 {
    if task.is_empty() || title.is_empty() {
        return 0;
    }
    let shared = title.iter().filter(|word| task.contains(word)).count();
    if shared == 0 {
        return 0;
    }
    let ratio = shared as f32 / task.len().min(title.len()) as f32;
    (ratio * 30.0).round().clamp(0.0, 30.0) as i32
}

fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| word.len() > 3)
        .map(str::to_string)
        .collect()
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    text.chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>()
        + "…"
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn json_string(text: &str) -> String {
    let escaped = text
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', " ");
    format!("\"{escaped}\"")
}

pub fn default_min_score() -> i32 {
    DEFAULT_MIN_SCORE
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(reference: &str, title: &str, cwd: Option<&str>, idle: Option<u64>) -> Candidate {
        Candidate {
            reference: reference.to_string(),
            backend: "cmux",
            title: title.to_string(),
            cwd: cwd.map(str::to_string),
            busy: false,
            idle_secs: idle,
        }
    }

    #[test]
    fn same_directory_outranks_topic_overlap() {
        let here = "/repo/app";
        let candidates = vec![
            candidate("tmux:1.0", "unrelated chore", Some(here), None),
            candidate("workspace:9", "fix the payment retry bug", None, None),
        ];
        let ranked = rank(
            &candidates,
            "fix the payment retry bug again",
            Some(here),
            false,
        );
        assert_eq!(ranked[0].candidate.reference, "tmux:1.0");
    }

    #[test]
    fn an_agent_inside_the_cache_window_beats_a_stale_one() {
        let candidates = vec![
            candidate("workspace:1", "payment retry work", None, Some(30)),
            candidate("workspace:2", "payment retry work", None, Some(7200)),
        ];
        let ranked = rank(&candidates, "continue the payment retry work", None, false);
        assert_eq!(ranked[0].candidate.reference, "workspace:1");
        assert!(ranked[0].why.iter().any(|why| why.contains("cache window")));
    }

    #[test]
    fn busy_agents_are_excluded_unless_asked_for() {
        let mut busy = candidate("workspace:3", "same topic here", None, Some(10));
        busy.busy = true;
        let ranked = rank(std::slice::from_ref(&busy), "same topic here", None, false);
        assert!(ranked.is_empty());
        let forced = rank(std::slice::from_ref(&busy), "same topic here", None, true);
        assert_eq!(forced.len(), 1);
        assert!(forced[0].why.iter().any(|why| why.contains("busy")));
    }

    #[test]
    fn an_unrelated_idle_agent_scores_below_the_cold_start_threshold() {
        let candidates = vec![candidate(
            "workspace:4",
            "writing release notes",
            None,
            None,
        )];
        let ranked = rank(&candidates, "migrate the postgres schema", None, false);
        assert!(ranked[0].score < default_min_score());
    }

    #[test]
    fn short_words_do_not_create_fake_overlap() {
        // "the", "a", "to" must not make two unrelated tasks look related.
        let candidates = vec![candidate("workspace:5", "a to the of and", None, None)];
        let ranked = rank(&candidates, "the a to of and", None, false);
        assert_eq!(ranked[0].score, 0);
    }

    #[test]
    fn cache_window_labels_are_honest_about_the_unknown() {
        assert_eq!(cache_window(Some(10)), "warm");
        assert_eq!(cache_window(Some(CACHE_WINDOW_SECS + 1)), "cold");
        assert_eq!(cache_window(None), "?");
    }

    #[test]
    fn a_warm_winner_is_reused_and_an_empty_field_cold_starts() {
        let warm = vec![Scored {
            candidate: candidate("workspace:7", "same work", None, Some(10)),
            score: 60,
            why: vec!["warm".into()],
        }];
        assert_eq!(
            decide(&warm, Some("claude"), false),
            Decision::Reuse("workspace:7".into())
        );
        assert_eq!(
            decide(&[], Some("claude"), false),
            Decision::ColdStart("claude".into())
        );
        // Without a fallback runtime an empty field stays a report, never a
        // surprise spawn.
        assert_eq!(decide(&[], None, true), Decision::Report);
        // And without --send or --or-assign we only ever print.
        assert_eq!(decide(&warm, None, false), Decision::Report);
    }

    #[test]
    fn our_own_process_chain_is_never_a_routing_target() {
        let ancestors = ancestor_pids();
        assert!(
            ancestors.contains(&std::process::id()),
            "the chain must contain this process, or self-exclusion is a no-op"
        );
    }

    #[test]
    fn busy_detection_reads_cmux_status_text() {
        assert!(is_busy("working"));
        assert!(is_busy("Thinking…"));
        assert!(!is_busy("Needs input"));
        assert!(!is_busy("idle"));
    }
}

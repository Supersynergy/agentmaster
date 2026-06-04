//! State detection. The whole "zero orchestration tax" claim rests here: instead
//! of asking an LLM to report its own status (tokens), we infer status by reading
//! the agent's terminal output — exactly what a human sees. Heuristic, cheap,
//! transparent. Plain substring matching, no regex dependency.

use crate::fleet::{Agent, Status};

/// Lines that mean "the agent is waiting on a human" -> BLOCKED.
const BLOCK_HINTS: &[&str] = &[
    "[y/n]",
    "(y/n)",
    "[y/n/a]",
    "yes/no",
    "do you want",
    "would you like",
    "approve",
    "permission",
    "continue?",
    "proceed?",
    "password",
    "passphrase",
    "press enter",
    "press any key",
    "confirm",
    "overwrite?",
    "are you sure",
    "select an option",
    "waiting for input",
    "enter your",
    "›",
];

/// Lines that mean "a unit of work finished and wants eyes" -> REVIEW.
const REVIEW_HINTS: &[&str] = &[
    "diff --git",
    "files changed",
    "ready for review",
    "opened pull request",
    "created pr",
    "please review",
];

/// Lines that mean "the agent reached a terminal success" -> DONE.
const DONE_HINTS: &[&str] = &[
    "✓ done",
    "build succeeded",
    "all tests passed",
    "completed successfully",
    "goal met",
    "task complete",
    "compilation finished",
    "0 errors",
];

/// Given a fresh output line, return the new status if it should change.
/// Order matters: dod-met > blocked > review > done > working. Terminal states
/// (done/dead) are sticky — late stray output won't resurrect a finished agent.
pub fn detect(agent: &Agent, line: &str) -> Option<Status> {
    if matches!(agent.status, Status::Done | Status::Dead) {
        return None;
    }
    let l = line.to_lowercase();
    // Goal-aware: if the operator pinned a definition-of-done and the agent's
    // output now contains it, the goal is met — strongest terminal signal.
    if let Some(dod) = agent.done_def.as_deref() {
        let d = dod.trim().to_lowercase();
        if d.len() >= 4 && l.contains(&d) {
            return Some(Status::Done);
        }
    }
    if BLOCK_HINTS.iter().any(|h| l.contains(h)) {
        return Some(Status::Blocked);
    }
    if REVIEW_HINTS.iter().any(|h| l.contains(h)) {
        return Some(Status::Review);
    }
    if DONE_HINTS.iter().any(|h| l.contains(h)) {
        return Some(Status::Done);
    }
    Some(Status::Working)
}

/// Ratchet goal progress from an output line. Pure + monotonic: progress only
/// rises, so a transient log line can't walk it backwards. `current` is the
/// agent's existing progress; the return is the new (>=) value, capped 0..=100.
/// Zero orchestration tax — milestones are read from output, never asked for.
pub fn infer_progress(current: u8, line: &str) -> u8 {
    let l = line.to_lowercase();
    let mut p = current;
    // Explicit "step 3/7" / "3 of 7" style → exact ratio.
    if let Some(r) = parse_ratio(&l) {
        p = p.max(r);
    }
    // Milestone keywords each imply a floor.
    const FLOORS: &[(&str, u8)] = &[
        ("cloning", 10),
        ("installing", 20),
        ("building", 35),
        ("compiling", 40),
        ("running tests", 60),
        ("test result", 75),
        ("tests passed", 90),
        ("all tests passed", 90),
        ("0 errors", 85),
        ("ready for review", 92),
        ("build succeeded", 95),
        ("✓ done", 100),
        ("task complete", 100),
        ("completed successfully", 100),
        ("goal met", 100),
    ];
    for (k, floor) in FLOORS {
        if l.contains(k) {
            p = p.max(*floor);
        }
    }
    p.min(100)
}

/// Parse a `N/M` or `N of M` progress ratio embedded in a line → percent.
fn parse_ratio(l: &str) -> Option<u8> {
    // Look for "step N/M", "N/M", "N of M". Cheap scan, no regex dep.
    let norm = l.replace(" of ", "/");
    for tok in norm.split(|c: char| c.is_whitespace() || c == '(' || c == ')') {
        if let Some((a, b)) = tok.split_once('/')
            && let (Ok(n), Ok(m)) = (a.trim().parse::<u32>(), b.trim().parse::<u32>())
            && m > 0
            && n <= m
            && m <= 999
        {
            return Some(((n * 100) / m).min(100) as u8);
        }
    }
    None
}

/// Demote a WORKING agent to IDLE once it has been quiet past the threshold.
/// Routed through `note_status` so the in-state clock resets on the transition.
pub fn idle_sweep(agent: &mut Agent, idle_threshold: i64) {
    if matches!(agent.status, Status::Working) && agent.idle_secs() > idle_threshold {
        agent.note_status(Status::Idle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::Agent;

    fn agent() -> Agent {
        Agent::new(
            1,
            "t".into(),
            "shell".into(),
            "bash".into(),
            vec![],
            "/tmp".into(),
        )
    }

    #[test]
    fn block_beats_working() {
        let a = agent();
        assert_eq!(
            detect(&a, "Do you want to continue? [y/N]"),
            Some(Status::Blocked)
        );
    }

    #[test]
    fn done_is_sticky() {
        let mut a = agent();
        a.status = Status::Done;
        assert_eq!(detect(&a, "anything"), None);
    }

    #[test]
    fn plain_output_is_working() {
        let a = agent();
        assert_eq!(detect(&a, "running cargo nextest"), Some(Status::Working));
    }

    #[test]
    fn dod_match_is_done() {
        let mut a = agent();
        a.done_def = Some("green ci".into());
        assert_eq!(
            detect(&a, "pipeline shows GREEN CI now"),
            Some(Status::Done)
        );
    }

    #[test]
    fn progress_ratchets_and_never_drops() {
        assert_eq!(infer_progress(0, "Building target"), 35);
        assert_eq!(infer_progress(60, "building again"), 60); // can't go down
        assert_eq!(infer_progress(0, "step 3/4 done"), 75);
        assert_eq!(infer_progress(0, "2 of 5 complete"), 40);
        assert_eq!(infer_progress(0, "all tests passed"), 90);
        assert_eq!(infer_progress(90, "✓ done"), 100);
    }
}

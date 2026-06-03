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
/// Order matters: blocked > review > done > working. Terminal states (done/dead)
/// are sticky — late stray output won't resurrect a finished agent.
pub fn detect(agent: &Agent, line: &str) -> Option<Status> {
    if matches!(agent.status, Status::Done | Status::Dead) {
        return None;
    }
    let l = line.to_lowercase();
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

/// Demote a WORKING agent to IDLE once it has been quiet past the threshold.
pub fn idle_sweep(agent: &mut Agent, idle_threshold: i64) {
    if matches!(agent.status, Status::Working) && agent.idle_secs() > idle_threshold {
        agent.status = Status::Idle;
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
}

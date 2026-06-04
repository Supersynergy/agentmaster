//! Fleet model: agents, their lifecycle status, and the Kanban lane mapping.

use std::collections::VecDeque;

use chrono::{DateTime, Local};

use crate::pty::PtyHandle;

/// Fine-grained agent status. `Idle` is a "working agent that went quiet"; the
/// Kanban view folds it back into the WORKING lane but renders it dimmed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Queued,
    Working,
    Idle,
    Blocked,
    Review,
    Done,
    Dead,
}

impl Status {
    pub fn lane(self) -> Lane {
        match self {
            Status::Queued => Lane::Queued,
            Status::Working | Status::Idle => Lane::Working,
            Status::Blocked => Lane::Blocked,
            Status::Review => Lane::Review,
            Status::Done | Status::Dead => Lane::Done,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Status::Queued => "queued",
            Status::Working => "working",
            Status::Idle => "idle",
            Status::Blocked => "blocked",
            Status::Review => "review",
            Status::Done => "done",
            Status::Dead => "dead",
        }
    }
}

/// The five Kanban columns. Lanes == agent state, so the board IS the state.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    Queued,
    Working,
    Blocked,
    Review,
    Done,
}

impl Lane {
    pub const ALL: [Lane; 5] = [
        Lane::Queued,
        Lane::Working,
        Lane::Blocked,
        Lane::Review,
        Lane::Done,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Lane::Queued => "QUEUED",
            Lane::Working => "WORKING",
            Lane::Blocked => "BLOCKED",
            Lane::Review => "REVIEW",
            Lane::Done => "DONE",
        }
    }
}

/// Where an agent runs: a PTY agentmaster owns, or a pane in another multiplexer
/// that we drive via its CLI.
#[derive(Clone, PartialEq, Eq)]
pub enum Source {
    Native,
    Tmux(String), // target = session:window.pane
    Cmux(String), // workspace ref, e.g. workspace:96
}

/// A single agent = one spawned process behind a PTY, plus the observable state
/// we derive from its output. No agent ever coordinates via tokens — all the
/// coordination signal lives here, on disk and in this struct.
pub struct Agent {
    pub id: u64,
    pub name: String,
    pub runtime: String,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub project: String,
    pub branch: Option<String>,
    pub pid: Option<u32>,
    pub status: Status,
    pub started: DateTime<Local>,
    pub last_activity: DateTime<Local>,
    pub last_line: String,
    pub output: VecDeque<String>,
    pub pty: Option<PtyHandle>,
    pub source: Source,
    pub lines_total: u64,
    /// Live per-process resource use, refreshed on the housekeeping tick.
    pub cpu: f32,
    pub mem_bytes: u64,
    /// Operator-set goal for this agent + its definition-of-done. Both persist in
    /// SQLite keyed by name, so re-imported/re-spawned agents keep their goal.
    pub goal: Option<String>,
    pub done_def: Option<String>,
    /// Heuristic 0-100 goal progress, derived from output milestones — never a
    /// token-costing self-report.
    pub progress: u8,
    /// Path to this agent's session transcript (cmux/sr/Claude Code JSONL), when
    /// known. Enables zero-tax `peek` — read what it last said off disk.
    pub transcript: Option<String>,
    /// Timestamp of the last observed status TRANSITION. Drives "how long in this
    /// state" — the operator's real signal (blocked 20s vs blocked 20m), which is
    /// meaningful even for imported agents whose start time we never owned.
    pub last_change: DateTime<Local>,
    /// Epoch seconds of the agent's transcript mtime — the GROUND TRUTH for "when
    /// did it last actually respond / do something", read off disk, not inferred
    /// from polling. None until a transcript is resolved + stat'd.
    pub last_seen: Option<i64>,
}

impl Agent {
    pub fn new(
        id: u64,
        name: String,
        runtime: String,
        program: String,
        args: Vec<String>,
        cwd: String,
    ) -> Self {
        let now = Local::now();
        let project = cwd
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("")
            .to_string();
        Agent {
            id,
            name,
            runtime,
            program,
            args,
            cwd,
            project,
            branch: None,
            pid: None,
            status: Status::Queued,
            started: now,
            last_activity: now,
            last_line: String::new(),
            output: VecDeque::with_capacity(512),
            pty: None,
            source: Source::Native,
            lines_total: 0,
            cpu: 0.0,
            mem_bytes: 0,
            goal: None,
            done_def: None,
            progress: 0,
            transcript: None,
            last_change: now,
            last_seen: None,
        }
    }

    /// True once this agent reached a terminal lane (done or dead).
    pub fn is_terminal(&self) -> bool {
        matches!(self.status, Status::Done | Status::Dead)
    }

    /// Seconds since the agent last wrote to its transcript — i.e. since its last
    /// real response/activity. `None` if we have no transcript for it. This is the
    /// honest answer to "when did the last response end" (ground truth = mtime),
    /// not a guess from status polling.
    pub fn last_response_secs(&self) -> Option<i64> {
        self.last_seen
            .map(|t| (Local::now().timestamp() - t).max(0))
    }

    /// Has the operator set a goal we can track progress against?
    pub fn has_goal(&self) -> bool {
        self.goal.is_some()
    }

    /// Seconds spent in the current status, since the last transition. This is
    /// the honest observability metric for imported agents: we cannot know when
    /// a cmux/tmux agent truly started, but we can time how long it has held a
    /// state — and "blocked 18m" is exactly what tells you where to look.
    pub fn in_status_secs(&self) -> i64 {
        (Local::now() - self.last_change).num_seconds().max(0)
    }

    /// Seconds left on this agent's Claude/Codex prompt cache (1h TTL), counting
    /// down from the LAST RESPONSE. Anchored on the transcript mtime when we have
    /// it (ground truth); otherwise falls back to status (working = hot) / the
    /// observed idle time. At 0 the next turn pays full, uncached input cost.
    pub fn cache_remaining_secs(&self) -> i64 {
        if let Some(since) = self.last_response_secs() {
            (3600 - since).max(0)
        } else if matches!(self.status, Status::Working) {
            3600
        } else {
            (3600 - self.idle_secs()).max(0)
        }
    }

    /// Record an observed status. Returns true iff it changed — and on change
    /// stamps the transition + activity time. Same-status observations leave the
    /// clock running so `in_status_secs` reflects real time-in-state.
    pub fn note_status(&mut self, new: Status) -> bool {
        if new != self.status {
            self.status = new;
            let now = Local::now();
            self.last_change = now;
            self.last_activity = now;
            true
        } else {
            false
        }
    }

    pub fn push_line(&mut self, line: String) {
        self.last_line = line.clone();
        self.last_activity = Local::now();
        self.lines_total += 1;
        self.output.push_back(line);
        while self.output.len() > 500 {
            self.output.pop_front();
        }
    }

    pub fn age_secs(&self) -> i64 {
        (Local::now() - self.started).num_seconds()
    }

    pub fn idle_secs(&self) -> i64 {
        (Local::now() - self.last_activity).num_seconds()
    }
}

/// The whole fleet. Plain Vec — small N, linear scans are free and obvious.
pub struct Fleet {
    pub agents: Vec<Agent>,
    pub next_id: u64,
}

impl Fleet {
    pub fn new() -> Self {
        Fleet {
            agents: Vec::new(),
            next_id: 1,
        }
    }

    pub fn get_mut(&mut self, id: u64) -> Option<&mut Agent> {
        self.agents.iter_mut().find(|a| a.id == id)
    }

    pub fn get(&self, id: u64) -> Option<&Agent> {
        self.agents.iter().find(|a| a.id == id)
    }

    /// Count of agents per lane, indexed like `Lane::ALL`.
    pub fn counts(&self) -> [usize; 5] {
        let mut c = [0usize; 5];
        for a in &self.agents {
            let i = Lane::ALL
                .iter()
                .position(|x| *x == a.status.lane())
                .unwrap_or(0);
            c[i] += 1;
        }
        c
    }

    pub fn in_lane(&self, lane: Lane) -> Vec<&Agent> {
        self.agents
            .iter()
            .filter(|a| a.status.lane() == lane)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn note_status_only_resets_clock_on_change() {
        let mut a = agent();
        assert!(a.note_status(Status::Working)); // Queued -> Working = change
        assert!(!a.note_status(Status::Working)); // same -> no change
        assert!(a.note_status(Status::Blocked)); // change
        assert_eq!(a.status, Status::Blocked);
    }

    #[test]
    fn in_status_secs_non_negative() {
        let a = agent();
        assert!(a.in_status_secs() >= 0);
    }

    #[test]
    fn cache_full_while_working_decays_otherwise() {
        let mut a = agent();
        a.note_status(Status::Working);
        assert_eq!(a.cache_remaining_secs(), 3600); // hot while generating
        a.note_status(Status::Blocked);
        assert!(a.cache_remaining_secs() <= 3600 && a.cache_remaining_secs() > 3500);
    }
}

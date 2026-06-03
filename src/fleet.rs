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
    pub lines_total: u64,
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
            lines_total: 0,
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

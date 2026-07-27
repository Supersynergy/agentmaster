//! On-disk audit log + mail bus (SQLite). This is the coordination substrate:
//! agents and the operator communicate through queryable rows, never through
//! paid LLM round-trips. Also the durable observability trail — every state
//! change and action lands here and survives restarts.

use std::path::Path;

use anyhow::Result;
use rusqlite::{Connection, params};

pub struct Store {
    conn: Connection,
}

pub type OmniGoalRecord = (
    String,
    Option<String>,
    u8,
    Option<String>,
    u64,
    Option<String>,
    u32,
    String,
    Option<String>,
    Option<String>,
);

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            r#"
            PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS events(
              id       INTEGER PRIMARY KEY AUTOINCREMENT,
              ts       TEXT NOT NULL,
              agent_id INTEGER,
              name     TEXT,
              kind     TEXT NOT NULL,
              msg      TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_events_id ON events(id DESC);
            CREATE TABLE IF NOT EXISTS mail(
              id      INTEGER PRIMARY KEY AUTOINCREMENT,
              ts      TEXT NOT NULL,
              from_id INTEGER,
              to_id   INTEGER,
              subject TEXT,
              body    TEXT,
              read    INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS goals(
              name     TEXT PRIMARY KEY,
              goal     TEXT NOT NULL,
              dod      TEXT,
              progress INTEGER NOT NULL DEFAULT 0,
              updated  TEXT NOT NULL,
              oracle       TEXT,
              budget_tokens INTEGER NOT NULL DEFAULT 0,
              deadline     TEXT,
              tries        INTEGER NOT NULL DEFAULT 0,
              status       TEXT NOT NULL DEFAULT 'active',
              bottleneck   TEXT,
              summary      TEXT,
              closed_ts    TEXT
            );
            -- Time-in-state, keyed by a STABLE source ref (cmux:workspace:NN /
            -- tmux:target / native:name) so "blocked 2h" survives an agentmaster
            -- restart instead of resetting to import time. `since` = epoch secs the
            -- agent entered `status`; `last` = epoch secs we last observed it there.
            CREATE TABLE IF NOT EXISTS seen(
              ref    TEXT PRIMARY KEY,
              status TEXT NOT NULL,
              since  INTEGER NOT NULL,
              last   INTEGER NOT NULL
            );
            -- Omnigoal lifecycle columns on legacy DBs: the CREATE TABLE above
            -- already has them on fresh DBs, so the ALTERs below must be tolerant.
            -- SQLite has no IF NOT EXISTS for ADD COLUMN, so we guard via pragma.
            "#,
        )?;
        // Tolerant migration: add omnigoal columns only if missing on legacy DBs.
        let existing: std::collections::HashSet<String> = {
            let mut set = std::collections::HashSet::new();
            let mut stmt = conn.prepare("PRAGMA table_info(goals)")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
            for r in rows.flatten() {
                set.insert(r);
            }
            set
        };
        for (col, ty) in [
            ("oracle", "TEXT"),
            ("budget_tokens", "INTEGER NOT NULL DEFAULT 0"),
            ("deadline", "TEXT"),
            ("tries", "INTEGER NOT NULL DEFAULT 0"),
            ("status", "TEXT NOT NULL DEFAULT 'active'"),
            ("bottleneck", "TEXT"),
            ("summary", "TEXT"),
            ("closed_ts", "TEXT"),
        ] {
            if !existing.contains(col) {
                let _ = conn.execute(&format!("ALTER TABLE goals ADD COLUMN {col} {ty}"), []);
            }
        }
        Ok(Self { conn })
    }

    /// Record an observed status for a stable ref. On a real transition (status
    /// differs from the stored one) the `since` clock restarts; an unchanged
    /// status only bumps `last` (liveness). Cheap upsert, called on every change.
    pub fn save_seen(&self, ref_: &str, status: &str, now: i64) {
        let _ = self.conn.execute(
            "INSERT INTO seen(ref, status, since, last) VALUES(?1,?2,?3,?3)
             ON CONFLICT(ref) DO UPDATE SET
               since = CASE WHEN status=?2 THEN since ELSE ?3 END,
               status = ?2,
               last = ?3",
            params![ref_, status, now],
        );
    }

    /// Restore time-in-state for a ref: `(status, since_epoch)` if known. The
    /// caller uses `since` as `last_change` only when the persisted status still
    /// matches what it just observed — so a stale row can't mis-date a new state.
    pub fn load_seen(&self, ref_: &str) -> Option<(String, i64)> {
        self.conn
            .query_row(
                "SELECT status, since FROM seen WHERE ref=?1",
                params![ref_],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .ok()
    }

    /// Persist an agent's goal + definition-of-done, keyed by name so it survives
    /// restarts and re-imports. Progress resets to 0 on a (re)set.
    pub fn set_goal(&self, name: &str, goal: &str, dod: Option<&str>) {
        let ts = chrono::Local::now().to_rfc3339();
        let _ = self.conn.execute(
            "INSERT INTO goals(name, goal, dod, progress, updated) VALUES(?1,?2,?3,0,?4)
             ON CONFLICT(name) DO UPDATE SET goal=?2, dod=?3, progress=0, updated=?4",
            params![name, goal, dod, ts],
        );
    }

    /// Omnigoal lifecycle: persist a goal with machine-checkable oracle, token
    /// budget cap, deadline, and 3-try counter. Replaces `set_goal` for the
    /// `goal init` path. Status starts `active`, tries=0.
    pub fn set_goal_omni(
        &self,
        name: &str,
        goal: &str,
        dod: Option<&str>,
        oracle: Option<&str>,
        budget_tokens: u64,
        deadline: Option<&str>,
    ) {
        let ts = chrono::Local::now().to_rfc3339();
        let _ = self.conn.execute(
            "INSERT INTO goals(name, goal, dod, progress, updated, oracle, budget_tokens, deadline, tries, status)
             VALUES(?1,?2,?3,0,?4,?5,?6,?7,0,'active')
             ON CONFLICT(name) DO UPDATE SET
               goal=?2, dod=?3, progress=0, updated=?4,
               oracle=?5, budget_tokens=?6, deadline=?7,
               tries=0, status='active', bottleneck=NULL,
               summary=NULL, closed_ts=NULL",
            params![name, goal, dod, ts, oracle, budget_tokens as i64, deadline],
        );
    }

    /// Load the full omnigoal record for a goal: (goal, dod, progress, oracle,
    /// budget, deadline, tries, status, bottleneck, summary). Returns None if
    /// the goal doesn't exist.
    pub fn load_goal_omni(&self, name: &str) -> Option<OmniGoalRecord> {
        self.conn
            .query_row(
                "SELECT goal, dod, progress, oracle, budget_tokens, deadline, tries, status, bottleneck, summary
                 FROM goals WHERE name=?1",
                params![name],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, i64>(2)? as u8,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, i64>(4)? as u64,
                        r.get::<_, Option<String>>(5)?,
                        r.get::<_, i64>(6)? as u32,
                        r.get::<_, String>(7)?,
                        r.get::<_, Option<String>>(8)?,
                        r.get::<_, Option<String>>(9)?,
                    ))
                },
            )
            .ok()
    }

    /// Increment the try counter, persist a bottleneck string, return the new
    /// try count. Called by `goal check` when the oracle fails.
    pub fn goal_record_try(&self, name: &str, bottleneck: Option<&str>) -> u32 {
        let ts = chrono::Local::now().to_rfc3339();
        let _ = self.conn.execute(
            "UPDATE goals SET tries = tries + 1, bottleneck = ?2, updated = ?3 WHERE name = ?1",
            params![name, bottleneck, ts],
        );
        self.conn
            .query_row(
                "SELECT tries FROM goals WHERE name = ?1",
                params![name],
                |r| r.get::<_, i64>(0).map(|v| v as u32),
            )
            .unwrap_or(0)
    }

    /// Mark a goal done with a closing summary. Persisted to the row + logged
    /// to the event stream so `agentmaster events` shows the closure.
    pub fn goal_close(&self, name: &str, summary: &str) {
        let ts = chrono::Local::now().to_rfc3339();
        let _ = self.conn.execute(
            "UPDATE goals SET status='done', summary=?2, closed_ts=?3, progress=100, updated=?3
             WHERE name = ?1",
            params![name, summary, ts],
        );
        self.log(None, name, "goal-close", summary);
    }

    /// Abandon a goal (3-try cap hit, oracle still red). Distinct from `done`
    /// so the audit trail separates convergence from giving up.
    pub fn goal_abandon(&self, name: &str, reason: &str) {
        let ts = chrono::Local::now().to_rfc3339();
        let _ = self.conn.execute(
            "UPDATE goals SET status='abandoned', summary=?2, closed_ts=?3, updated=?3
             WHERE name = ?1",
            params![name, reason, ts],
        );
        self.log(None, name, "goal-abandon", reason);
    }

    /// Append a `goal-spawn` event recording that a subagent was registered
    /// with a bounded capsule + skill for this goal. Ponytail: we log the event
    /// only — the actual capsule file lives on disk and is referenced by path.
    pub fn goal_spawn(&self, name: &str, capsule: Option<&str>, skill: Option<&str>) {
        let msg = match (capsule, skill) {
            (Some(c), Some(s)) => format!("capsule={c} skill={s}"),
            (Some(c), None) => format!("capsule={c}"),
            (None, Some(s)) => format!("skill={s}"),
            (None, None) => "(no capsule, no skill)".into(),
        };
        self.log(None, name, "goal-spawn", &msg);
    }

    /// Update only the derived progress for a goal (cheap, called on milestones).
    pub fn save_progress(&self, name: &str, progress: u8) {
        let ts = chrono::Local::now().to_rfc3339();
        let _ = self.conn.execute(
            "UPDATE goals SET progress=?2, updated=?3 WHERE name=?1",
            params![name, progress as i64, ts],
        );
    }

    /// All stored goals: (name, goal, dod, progress). Used to rehydrate agents on
    /// spawn/import so a goal set yesterday is still there today.
    pub fn load_goals(&self) -> Vec<(String, String, Option<String>, u8)> {
        let mut out = Vec::new();
        if let Ok(mut stmt) = self
            .conn
            .prepare("SELECT name, goal, dod, progress FROM goals")
            && let Ok(rows) = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, i64>(3)? as u8,
                ))
            })
        {
            out.extend(rows.flatten());
        }
        out
    }

    /// Append one event. Best-effort: a logging failure must never crash the UI.
    pub fn log(&self, agent_id: Option<u64>, name: &str, kind: &str, msg: &str) {
        let ts = chrono::Local::now().to_rfc3339();
        let _ = self.conn.execute(
            "INSERT INTO events(ts, agent_id, name, kind, msg) VALUES(?1, ?2, ?3, ?4, ?5)",
            params![ts, agent_id.map(|v| v as i64), name, kind, msg],
        );
    }

    /// Most recent `limit` events, newest first: (ts, name, kind, msg).
    pub fn recent(&self, limit: i64) -> Vec<(String, String, String, String)> {
        let mut out = Vec::new();
        if let Ok(mut stmt) = self
            .conn
            .prepare("SELECT ts, name, kind, msg FROM events ORDER BY id DESC LIMIT ?1")
            && let Ok(rows) = stmt.query_map([limit], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1).unwrap_or_default(),
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3).unwrap_or_default(),
                ))
            })
        {
            out.extend(rows.flatten());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_db() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("am-store-test-{}.db", uuid::Uuid::new_v4()));
        p
    }

    #[test]
    fn seen_keeps_since_on_same_status_resets_on_change() {
        let s = Store::open(&tmp_db()).unwrap();
        s.save_seen("cmux:workspace:1", "blocked", 1000);
        assert_eq!(
            s.load_seen("cmux:workspace:1"),
            Some(("blocked".into(), 1000))
        );
        // Same status observed later: `since` is preserved (the clock keeps running).
        s.save_seen("cmux:workspace:1", "blocked", 1500);
        assert_eq!(
            s.load_seen("cmux:workspace:1"),
            Some(("blocked".into(), 1000))
        );
        // A real transition restarts `since`.
        s.save_seen("cmux:workspace:1", "working", 2000);
        assert_eq!(
            s.load_seen("cmux:workspace:1"),
            Some(("working".into(), 2000))
        );
        assert_eq!(s.load_seen("cmux:workspace:404"), None);
    }
}

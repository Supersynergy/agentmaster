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
              updated  TEXT NOT NULL
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
            "#,
        )?;
        Ok(Store { conn })
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
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        p.push(format!("am-store-test-{n}.db"));
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

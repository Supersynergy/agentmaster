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
            "#,
        )?;
        Ok(Store { conn })
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

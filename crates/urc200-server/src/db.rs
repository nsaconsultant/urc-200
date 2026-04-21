//! SQLite-backed channel library.
//!
//! Channels live in named groups (`"Aviation"`, `"Marine HF"`, `"SATCOM"`, etc.).
//! Frequencies are stored as integer Hz to avoid float precision issues.
//!
//! The rusqlite API is blocking; every public async method here runs the
//! database work on a `spawn_blocking` worker and returns when it's done.

use anyhow::Result;
use rusqlite::{params, Connection};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::task;

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Channel {
    pub id: i64,
    pub group_name: String,
    pub name: String,
    pub rx_hz: u32,
    pub tx_hz: u32,
    pub mode: Option<String>,
    pub step_khz: Option<f32>,
    pub notes: Option<String>,
    pub ctcss_hz: Option<f32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GroupSummary {
    pub name: String,
    pub channel_count: i64,
}

pub struct NewChannel<'a> {
    pub group_name: &'a str,
    pub name: &'a str,
    pub rx_hz: u32,
    pub tx_hz: u32,
    pub mode: Option<&'a str>,
    pub step_khz: Option<f32>,
    pub notes: Option<&'a str>,
}

impl Db {
    pub fn open(path: &PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS channels (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                group_name TEXT NOT NULL,
                name TEXT NOT NULL,
                rx_hz INTEGER NOT NULL,
                tx_hz INTEGER NOT NULL,
                mode TEXT,
                step_khz REAL,
                notes TEXT,
                ctcss_hz REAL,
                created_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
            );
            CREATE INDEX IF NOT EXISTS idx_channels_group ON channels(group_name);
            "#,
        )?;
        // Hand-rolled migration: older DBs predate ctcss_hz. ALTER if missing.
        let has_ctcss: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('channels') WHERE name='ctcss_hz'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if has_ctcss == 0 {
            conn.execute("ALTER TABLE channels ADD COLUMN ctcss_hz REAL", [])?;
        }
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub async fn list_groups(&self) -> Result<Vec<GroupSummary>> {
        let c = self.conn.clone();
        task::spawn_blocking(move || -> Result<_> {
            let conn = c.lock().unwrap();
            let mut stmt = conn.prepare(
                "SELECT group_name, COUNT(*) FROM channels GROUP BY group_name ORDER BY group_name",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(GroupSummary {
                    name: r.get(0)?,
                    channel_count: r.get(1)?,
                })
            })?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
        .await?
    }

    pub async fn list_channels(&self, group_filter: Option<String>) -> Result<Vec<Channel>> {
        let c = self.conn.clone();
        task::spawn_blocking(move || -> Result<_> {
            let conn = c.lock().unwrap();
            let (sql, params_vec): (&str, Vec<Box<dyn rusqlite::ToSql>>) = match &group_filter {
                Some(g) => (
                    "SELECT id, group_name, name, rx_hz, tx_hz, mode, step_khz, notes, ctcss_hz \
                     FROM channels WHERE group_name = ?1 \
                     ORDER BY name",
                    vec![Box::new(g.clone())],
                ),
                None => (
                    "SELECT id, group_name, name, rx_hz, tx_hz, mode, step_khz, notes, ctcss_hz \
                     FROM channels \
                     ORDER BY group_name, name",
                    vec![],
                ),
            };
            let mut stmt = conn.prepare(sql)?;
            let params_refs: Vec<&dyn rusqlite::ToSql> =
                params_vec.iter().map(|b| b.as_ref()).collect();
            let rows = stmt.query_map(params_refs.as_slice(), |r| {
                Ok(Channel {
                    id: r.get(0)?,
                    group_name: r.get(1)?,
                    name: r.get(2)?,
                    rx_hz: r.get::<_, i64>(3)? as u32,
                    tx_hz: r.get::<_, i64>(4)? as u32,
                    mode: r.get(5)?,
                    step_khz: r.get(6)?,
                    notes: r.get(7)?,
                    ctcss_hz: r.get(8)?,
                })
            })?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
        .await?
    }

    pub async fn get_channel(&self, id: i64) -> Result<Option<Channel>> {
        let c = self.conn.clone();
        task::spawn_blocking(move || -> Result<_> {
            let conn = c.lock().unwrap();
            let res = conn.query_row(
                "SELECT id, group_name, name, rx_hz, tx_hz, mode, step_khz, notes, ctcss_hz \
                 FROM channels WHERE id = ?1",
                [id],
                |r| {
                    Ok(Channel {
                        id: r.get(0)?,
                        group_name: r.get(1)?,
                        name: r.get(2)?,
                        rx_hz: r.get::<_, i64>(3)? as u32,
                        tx_hz: r.get::<_, i64>(4)? as u32,
                        mode: r.get(5)?,
                        step_khz: r.get(6)?,
                        notes: r.get(7)?,
                        ctcss_hz: r.get(8)?,
                    })
                },
            );
            match res {
                Ok(ch) => Ok(Some(ch)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(e.into()),
            }
        })
        .await?
    }

    /// Bulk insert. If `replace_group` is Some(name), all existing rows in
    /// that group are deleted in the same transaction so re-import replaces
    /// rather than duplicates.
    pub async fn insert_many(
        &self,
        channels: Vec<OwnedNewChannel>,
        replace_group: Option<String>,
    ) -> Result<usize> {
        let c = self.conn.clone();
        task::spawn_blocking(move || -> Result<usize> {
            let mut conn = c.lock().unwrap();
            let tx = conn.transaction()?;
            {
                if let Some(ref g) = replace_group {
                    tx.execute("DELETE FROM channels WHERE group_name = ?1", [g])?;
                }
                let mut stmt = tx.prepare(
                    "INSERT INTO channels (group_name, name, rx_hz, tx_hz, mode, step_khz, notes, ctcss_hz) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                )?;
                for ch in &channels {
                    stmt.execute(params![
                        ch.group_name,
                        ch.name,
                        ch.rx_hz as i64,
                        ch.tx_hz as i64,
                        ch.mode,
                        ch.step_khz,
                        ch.notes,
                        ch.ctcss_hz,
                    ])?;
                }
            }
            tx.commit()?;
            Ok(channels.len())
        })
        .await?
    }

    pub async fn delete_channel(&self, id: i64) -> Result<bool> {
        let c = self.conn.clone();
        task::spawn_blocking(move || -> Result<bool> {
            let conn = c.lock().unwrap();
            let n = conn.execute("DELETE FROM channels WHERE id = ?1", [id])?;
            Ok(n > 0)
        })
        .await?
    }

    pub async fn delete_group(&self, group_name: String) -> Result<usize> {
        let c = self.conn.clone();
        task::spawn_blocking(move || -> Result<usize> {
            let conn = c.lock().unwrap();
            Ok(conn.execute("DELETE FROM channels WHERE group_name = ?1", [group_name])?)
        })
        .await?
    }
}

#[derive(Debug, Clone)]
pub struct OwnedNewChannel {
    pub group_name: String,
    pub name: String,
    pub rx_hz: u32,
    pub tx_hz: u32,
    pub mode: Option<String>,
    pub step_khz: Option<f32>,
    pub notes: Option<String>,
    pub ctcss_hz: Option<f32>,
}

impl<'a> From<&NewChannel<'a>> for OwnedNewChannel {
    fn from(n: &NewChannel<'a>) -> Self {
        Self {
            group_name: n.group_name.to_string(),
            name: n.name.to_string(),
            rx_hz: n.rx_hz,
            tx_hz: n.tx_hz,
            mode: n.mode.map(|s| s.to_string()),
            step_khz: n.step_khz,
            notes: n.notes.map(|s| s.to_string()),
            ctcss_hz: None,
        }
    }
}

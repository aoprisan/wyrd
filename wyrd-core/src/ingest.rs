//! Recording → SQLite ingestion.

use std::collections::HashMap;
use std::path::Path;

use rusqlite::{params, Connection};
use wyrd_weave::{Event, FrameReader, Record, TaskId};

use crate::error::CoreError;

const SCHEMA: &str = r#"
CREATE TABLE tasks (
    id        INTEGER PRIMARY KEY,
    parent    INTEGER,
    name      TEXT,
    kind      TEXT NOT NULL,
    loc_file  TEXT,
    loc_line  INTEGER,
    spawn_ts  INTEGER NOT NULL,
    end_ts    INTEGER
);
CREATE TABLE resources (
    id            INTEGER PRIMARY KEY,
    parent        INTEGER,
    concrete_type TEXT NOT NULL,
    loc_file      TEXT,
    loc_line      INTEGER,
    is_internal   INTEGER NOT NULL,
    new_ts        INTEGER NOT NULL,
    drop_ts       INTEGER
);
CREATE TABLE polls (
    id       INTEGER PRIMARY KEY,
    task     INTEGER NOT NULL,
    start_ts INTEGER NOT NULL,
    end_ts   INTEGER
);
CREATE TABLE parks (
    id       INTEGER PRIMARY KEY,
    task     INTEGER NOT NULL,
    resource INTEGER NOT NULL,
    op_name  TEXT NOT NULL,
    ts       INTEGER NOT NULL
);
CREATE TABLE wakes (
    id    INTEGER PRIMARY KEY,
    woken INTEGER NOT NULL,
    by    INTEGER,
    ts    INTEGER NOT NULL
);
CREATE TABLE resource_state (
    id       INTEGER PRIMARY KEY,
    resource INTEGER NOT NULL,
    field    TEXT NOT NULL,
    value    INTEGER NOT NULL,
    op       TEXT NOT NULL,
    ts       INTEGER NOT NULL
);
CREATE INDEX idx_polls_task     ON polls(task, start_ts);
CREATE INDEX idx_parks_task     ON parks(task, ts);
CREATE INDEX idx_parks_resource ON parks(resource, ts);
CREATE INDEX idx_wakes_woken    ON wakes(woken, ts);
CREATE INDEX idx_rs_resource    ON resource_state(resource, ts);
"#;

/// Create the schema and tune the connection.
pub(crate) fn init_schema(conn: &Connection) -> Result<(), CoreError> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.execute_batch(SCHEMA)?;
    Ok(())
}

fn op_str(op: wyrd_weave::StateOp) -> &'static str {
    match op {
        wyrd_weave::StateOp::Override => "override",
        wyrd_weave::StateOp::Add => "add",
        wyrd_weave::StateOp::Sub => "sub",
    }
}

fn kind_str(kind: wyrd_weave::TaskKind) -> &'static str {
    match kind {
        wyrd_weave::TaskKind::Task => "task",
        wyrd_weave::TaskKind::Blocking => "blocking",
        wyrd_weave::TaskKind::BlockOn => "block_on",
        wyrd_weave::TaskKind::Other => "other",
    }
}

/// Ingest an iterator of records into an already-initialized connection.
pub(crate) fn ingest_records<I>(conn: &mut Connection, records: I) -> Result<(), CoreError>
where
    I: IntoIterator<Item = Result<Record, wyrd_weave::WeaveError>>,
{
    // Open poll episodes per task, so PollStart/PollEnd can be paired.
    let mut open_polls: HashMap<TaskId, u64> = HashMap::new();

    let tx = conn.transaction()?;
    for record in records {
        let Record { ts, event } = record?;
        let ts = ts as i64;
        match event {
            Event::TaskSpawn {
                id,
                parent,
                name,
                loc,
                kind,
            } => {
                tx.execute(
                    "INSERT OR IGNORE INTO tasks (id, parent, name, kind, loc_file, loc_line, spawn_ts)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        id as i64,
                        parent.map(|p| p as i64),
                        name,
                        kind_str(kind),
                        loc.file,
                        loc.line,
                        ts
                    ],
                )?;
            }
            Event::TaskEnd { id } => {
                tx.execute(
                    "UPDATE tasks SET end_ts = ?2 WHERE id = ?1 AND end_ts IS NULL",
                    params![id as i64, ts],
                )?;
            }
            Event::ResourceNew {
                id,
                parent,
                concrete_type,
                loc,
                is_internal,
            } => {
                tx.execute(
                    "INSERT OR IGNORE INTO resources
                       (id, parent, concrete_type, loc_file, loc_line, is_internal, new_ts)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        id as i64,
                        parent.map(|p| p as i64),
                        concrete_type,
                        loc.file,
                        loc.line,
                        is_internal as i64,
                        ts
                    ],
                )?;
            }
            Event::ResourceDrop { id } => {
                tx.execute(
                    "UPDATE resources SET drop_ts = ?2 WHERE id = ?1 AND drop_ts IS NULL",
                    params![id as i64, ts],
                )?;
            }
            Event::PollStart { task } => {
                open_polls.insert(task, ts as u64);
            }
            Event::PollEnd { task } => {
                if let Some(start) = open_polls.remove(&task) {
                    tx.execute(
                        "INSERT INTO polls (task, start_ts, end_ts) VALUES (?1, ?2, ?3)",
                        params![task as i64, start as i64, ts],
                    )?;
                }
            }
            Event::Park {
                task,
                resource,
                op_name,
            } => {
                tx.execute(
                    "INSERT INTO parks (task, resource, op_name, ts) VALUES (?1, ?2, ?3, ?4)",
                    params![task as i64, resource as i64, op_name, ts],
                )?;
            }
            Event::Wake { woken, by } => {
                tx.execute(
                    "INSERT INTO wakes (woken, by, ts) VALUES (?1, ?2, ?3)",
                    params![woken as i64, by.map(|b| b as i64), ts],
                )?;
            }
            Event::ResourceState {
                id,
                field,
                value,
                op,
            } => {
                tx.execute(
                    "INSERT INTO resource_state (resource, field, value, op, ts)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![id as i64, field, value, op_str(op), ts],
                )?;
            }
        }
    }

    // Poll episodes still open at end-of-recording: record with a NULL end.
    let still_open = open_polls.len();
    for (task, start) in open_polls {
        tx.execute(
            "INSERT INTO polls (task, start_ts, end_ts) VALUES (?1, ?2, NULL)",
            params![task as i64, start as i64],
        )?;
    }

    tx.commit()?;

    #[cfg(feature = "diag")]
    {
        let _ = still_open;
        tracing::debug!(target: "wyrd::core", polls_open_at_end = still_open, "ingest complete");
    }
    #[cfg(not(feature = "diag"))]
    let _ = still_open;

    Ok(())
}

/// Ingest a recording file into `conn`.
pub(crate) fn ingest_file(conn: &mut Connection, path: &Path) -> Result<(), CoreError> {
    let file = std::fs::File::open(path).map_err(wyrd_weave::WeaveError::from)?;
    let reader = FrameReader::new(std::io::BufReader::new(file))?;
    ingest_records(conn, reader)
}

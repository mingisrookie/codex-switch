use std::{
    collections::HashSet,
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use rusqlite::{types::Value as SqlValue, Connection, OpenFlags};
use serde::Serialize;
use serde_json::Value;
use walkdir::WalkDir;

use crate::codex_paths::resolve_user_codex_paths;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadRecord {
    pub id: String,
    pub rollout_path: Option<String>,
    pub title: Option<String>,
    pub preview: Option<String>,
    pub model_provider: Option<String>,
    pub archived: bool,
    pub archived_at: Option<i64>,
    pub updated_at: Option<i64>,
    pub updated_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionFileRecord {
    pub path: PathBuf,
    pub session_id: Option<String>,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionInventory {
    pub home: PathBuf,
    pub thread_count: usize,
    pub session_jsonl_count: usize,
    pub threads: Vec<ThreadRecord>,
    pub session_files: Vec<SessionFileRecord>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SyncDryRun {
    pub source_threads: usize,
    pub target_threads: usize,
    pub new_threads: usize,
    pub duplicate_threads: usize,
}

pub fn scan_sessions(home: &Path) -> Result<SessionInventory, String> {
    let paths = resolve_user_codex_paths(home);
    let threads = scan_threads(&paths.state_db)?;
    let session_files = scan_session_files(&paths.sessions_dir)?;

    Ok(SessionInventory {
        home: home.to_path_buf(),
        thread_count: threads.len(),
        session_jsonl_count: session_files.len(),
        threads,
        session_files,
    })
}

pub fn build_sync_dry_run(sources: &[SessionInventory], target: &SessionInventory) -> SyncDryRun {
    let target_ids = target
        .threads
        .iter()
        .map(|thread| thread.id.as_str())
        .collect::<HashSet<_>>();
    let mut source_ids = HashSet::new();
    let mut duplicate_threads = 0;
    let mut new_threads = 0;

    for source in sources {
        for thread in &source.threads {
            if !source_ids.insert(thread.id.as_str()) {
                continue;
            }
            if target_ids.contains(thread.id.as_str()) {
                duplicate_threads += 1;
            } else {
                new_threads += 1;
            }
        }
    }

    SyncDryRun {
        source_threads: source_ids.len(),
        target_threads: target.threads.len(),
        new_threads,
        duplicate_threads,
    }
}

fn scan_threads(path: &Path) -> Result<Vec<ThreadRecord>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| format!("failed to open state_5.sqlite read-only: {error}"))?;
    let columns = table_columns(&conn, "threads")?;
    if !columns.iter().any(|column| column == "id") {
        return Ok(Vec::new());
    }
    let select = format!("SELECT {} FROM threads", columns.join(", "));
    let mut statement = conn
        .prepare(&select)
        .map_err(|error| format!("failed to prepare threads query: {error}"))?;
    let rows = statement
        .query_map([], |row| {
            let mut values = std::collections::HashMap::new();
            for (index, column) in columns.iter().enumerate() {
                values.insert(column.clone(), row.get::<usize, SqlValue>(index)?);
            }
            Ok(thread_from_values(values))
        })
        .map_err(|error| format!("failed to query threads: {error}"))?;

    let mut threads = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("failed to read threads: {error}"))?;
    threads.sort_by(|left, right| {
        right
            .updated_at_ms
            .cmp(&left.updated_at_ms)
            .then_with(|| right.updated_at.cmp(&left.updated_at))
    });
    Ok(threads)
}

fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>, String> {
    let mut statement = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|error| format!("failed to inspect table {table}: {error}"))?;
    let rows = statement
        .query_map([], |row| row.get::<usize, String>(1))
        .map_err(|error| format!("failed to read table columns: {error}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("failed to collect table columns: {error}"))
}

fn thread_from_values(values: std::collections::HashMap<String, SqlValue>) -> ThreadRecord {
    ThreadRecord {
        id: text_value(values.get("id")).unwrap_or_default(),
        rollout_path: text_value(values.get("rollout_path")),
        title: text_value(values.get("title")),
        preview: text_value(values.get("preview")),
        model_provider: text_value(values.get("model_provider")),
        archived: truthy_value(values.get("archived")),
        archived_at: integer_value(values.get("archived_at")),
        updated_at: integer_value(values.get("updated_at")),
        updated_at_ms: integer_value(values.get("updated_at_ms")),
    }
}

fn text_value(value: Option<&SqlValue>) -> Option<String> {
    match value {
        Some(SqlValue::Text(value)) if !value.is_empty() => Some(value.clone()),
        Some(SqlValue::Integer(value)) => Some(value.to_string()),
        Some(SqlValue::Real(value)) => Some(value.to_string()),
        _ => None,
    }
}

fn integer_value(value: Option<&SqlValue>) -> Option<i64> {
    match value {
        Some(SqlValue::Integer(value)) => Some(*value),
        Some(SqlValue::Real(value)) => Some(*value as i64),
        Some(SqlValue::Text(value)) => value.parse::<i64>().ok(),
        _ => None,
    }
}

fn truthy_value(value: Option<&SqlValue>) -> bool {
    match value {
        Some(SqlValue::Integer(value)) => *value != 0,
        Some(SqlValue::Real(value)) => *value != 0.0,
        Some(SqlValue::Text(value)) => {
            let normalized = value.trim().to_ascii_lowercase();
            matches!(normalized.as_str(), "1" | "true" | "yes")
        }
        _ => false,
    }
}

fn scan_session_files(path: &Path) -> Result<Vec<SessionFileRecord>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    WalkDir::new(path)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.file_type().is_file()
                && entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
        })
        .map(|entry| {
            let path = entry.into_path();
            let bytes = fs::metadata(&path)
                .map_err(|error| format!("failed to stat session jsonl: {error}"))?
                .len();
            Ok(SessionFileRecord {
                session_id: extract_session_id(&path)?,
                path,
                bytes,
            })
        })
        .collect()
}

fn extract_session_id(path: &Path) -> Result<Option<String>, String> {
    let file =
        fs::File::open(path).map_err(|error| format!("failed to open session jsonl: {error}"))?;
    for line in BufReader::new(file).lines().take(25) {
        let line = line.map_err(|error| format!("failed to read session jsonl: {error}"))?;
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("session_meta") {
            continue;
        }
        if let Some(id) = value
            .get("payload")
            .and_then(|payload| payload.get("id"))
            .and_then(Value::as_str)
        {
            return Ok(Some(id.to_string()));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rusqlite::Connection;
    use tempfile::tempdir;

    use super::{build_sync_dry_run, scan_sessions};

    fn create_state_db(path: &std::path::Path, rows: &[(&str, &str)]) {
        let conn = Connection::open(path).unwrap();
        conn.execute(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, updated_at INTEGER, updated_at_ms INTEGER)",
            [],
        )
        .unwrap();
        for (id, rollout_path) in rows {
            conn.execute(
                "INSERT INTO threads (id, rollout_path, updated_at, updated_at_ms) VALUES (?1, ?2, 1, 1000)",
                (id, rollout_path),
            )
            .unwrap();
        }
    }

    #[test]
    fn scans_threads_and_session_meta_jsonl() {
        let temp = tempdir().unwrap();
        let home = temp.path();
        let session_path = home.join("sessions/2026/06/23/rollout-thread-a.jsonl");
        fs::create_dir_all(session_path.parent().unwrap()).unwrap();
        fs::write(
            &session_path,
            r#"{"type":"session_meta","timestamp":"2026-06-23T00:00:00Z","payload":{"id":"thread-a"}}"#,
        )
        .unwrap();
        create_state_db(
            &home.join("state_5.sqlite"),
            &[("thread-a", session_path.to_str().unwrap())],
        );

        let inventory = scan_sessions(home).unwrap();

        assert_eq!(inventory.thread_count, 1);
        assert_eq!(inventory.session_jsonl_count, 1);
        assert_eq!(inventory.threads[0].id, "thread-a");
        assert_eq!(
            inventory.session_files[0].session_id.as_deref(),
            Some("thread-a")
        );
    }

    #[test]
    fn dry_run_counts_new_and_duplicate_threads() {
        let source_temp = tempdir().unwrap();
        let target_temp = tempdir().unwrap();
        create_state_db(
            &source_temp.path().join("state_5.sqlite"),
            &[("thread-a", "a.jsonl"), ("thread-b", "b.jsonl")],
        );
        create_state_db(
            &target_temp.path().join("state_5.sqlite"),
            &[("thread-a", "a.jsonl")],
        );

        let source = scan_sessions(source_temp.path()).unwrap();
        let target = scan_sessions(target_temp.path()).unwrap();
        let dry_run = build_sync_dry_run(&[source], &target);

        assert_eq!(dry_run.new_threads, 1);
        assert_eq!(dry_run.duplicate_threads, 1);
        assert_eq!(dry_run.source_threads, 2);
        assert_eq!(dry_run.target_threads, 1);
    }
}

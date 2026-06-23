use std::{
    fs,
    path::{Path, PathBuf},
};

use rusqlite::{types::Value, Connection, OpenFlags};
use serde::Serialize;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionSyncResult {
    pub inserted_threads: usize,
    pub copied_session_files: usize,
    pub duplicate_threads: usize,
}

#[derive(Debug, Clone)]
struct SourceThread {
    id: String,
    columns: Vec<String>,
    values: Vec<Value>,
    rollout_path: Option<String>,
}

pub fn sync_sessions(source_homes: &[PathBuf], target_home: &Path) -> Result<SessionSyncResult, String> {
    let target_db = target_home.join("state_5.sqlite");
    let target_conn = Connection::open(&target_db)
        .map_err(|error| format!("failed to open target state_5.sqlite: {error}"))?;
    target_conn
        .execute_batch("BEGIN IMMEDIATE")
        .map_err(|error| format!("failed to start session sync transaction: {error}"))?;

    let result = sync_sessions_in_transaction(source_homes, target_home, &target_conn);
    match result {
        Ok(result) => {
            target_conn
                .execute_batch("COMMIT")
                .map_err(|error| format!("failed to commit session sync transaction: {error}"))?;
            Ok(result)
        }
        Err(error) => {
            let _ = target_conn.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

fn sync_sessions_in_transaction(
    source_homes: &[PathBuf],
    target_home: &Path,
    target_conn: &Connection,
) -> Result<SessionSyncResult, String> {
    let mut inserted_threads = 0;
    let mut copied_session_files = 0;
    let mut duplicate_threads = 0;

    for source_home in source_homes {
        let source_db = source_home.join("state_5.sqlite");
        if !source_db.exists() {
            continue;
        }
        let source_conn = Connection::open_with_flags(&source_db, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|error| format!("failed to open source state_5.sqlite: {error}"))?;
        let source_threads = read_source_threads(&source_conn)?;
        for thread in source_threads {
            if thread_exists(&target_conn, &thread.id)? {
                duplicate_threads += 1;
                continue;
            }
            let copied_rollout = copy_rollout_file(source_home, target_home, thread.rollout_path.as_deref())?;
            insert_thread(&target_conn, &thread, copied_rollout.as_deref())?;
            inserted_threads += 1;
            if copied_rollout.is_some() {
                copied_session_files += 1;
            }
        }
        copy_dependent_rows(&source_conn, &target_conn)?;
    }

    Ok(SessionSyncResult {
        inserted_threads,
        copied_session_files,
        duplicate_threads,
    })
}

fn read_source_threads(conn: &Connection) -> Result<Vec<SourceThread>, String> {
    let columns = table_columns(conn, "threads")?;
    if !columns.iter().any(|column| column == "id") {
        return Ok(Vec::new());
    }
    let select = format!("SELECT {} FROM threads", columns.join(", "));
    let mut statement = conn
        .prepare(&select)
        .map_err(|error| format!("failed to prepare source thread query: {error}"))?;
    let id_index = columns
        .iter()
        .position(|column| column == "id")
        .ok_or_else(|| "threads table missing id column".to_string())?;
    let rollout_index = columns.iter().position(|column| column == "rollout_path");
    let rows = statement
        .query_map([], |row| {
            let mut values = Vec::new();
            for index in 0..columns.len() {
                values.push(row.get::<usize, Value>(index)?);
            }
            let id = match &values[id_index] {
                Value::Text(text) => text.clone(),
                _ => String::new(),
            };
            let rollout_path = rollout_index.and_then(|index| match &values[index] {
                Value::Text(text) => Some(text.clone()),
                _ => None,
            });
            Ok(SourceThread {
                id,
                columns: columns.clone(),
                values,
                rollout_path,
            })
        })
        .map_err(|error| format!("failed to read source threads: {error}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("failed to collect source threads: {error}"))
}

fn insert_thread(conn: &Connection, thread: &SourceThread, rollout_path: Option<&str>) -> Result<(), String> {
    let mut values = thread.values.clone();
    if let Some(index) = thread.columns.iter().position(|column| column == "rollout_path") {
        if let Some(path) = rollout_path {
            values[index] = Value::Text(path.to_string());
        }
    }
    let placeholders = (0..thread.columns.len()).map(|_| "?").collect::<Vec<_>>().join(", ");
    let sql = format!(
        "INSERT OR IGNORE INTO threads ({}) VALUES ({})",
        thread.columns.join(", "),
        placeholders
    );
    conn.execute(&sql, rusqlite::params_from_iter(values))
        .map_err(|error| format!("failed to insert target thread: {error}"))?;
    Ok(())
}

fn thread_exists(conn: &Connection, id: &str) -> Result<bool, String> {
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM threads WHERE id = ?1", [id], |row| row.get(0))
        .map_err(|error| format!("failed to check target thread: {error}"))?;
    Ok(count > 0)
}

fn copy_rollout_file(source_home: &Path, target_home: &Path, rollout_path: Option<&str>) -> Result<Option<String>, String> {
    let Some(raw) = rollout_path else {
        return Ok(None);
    };
    let source_path = PathBuf::from(raw);
    if !source_path.exists() {
        return Ok(None);
    }
    let relative = source_path
        .strip_prefix(source_home)
        .ok()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("sessions").join(source_path.file_name().unwrap_or_default()));
    let mut target_path = target_home.join(relative);
    if target_path.exists() && sha256_file(&source_path)? != sha256_file(&target_path)? {
        let stem = target_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("rollout")
            .to_string();
        target_path.set_file_name(format!("{stem}-imported.jsonl"));
    }
    if !target_path.exists() {
        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent).map_err(|error| format!("failed to create target session dir: {error}"))?;
        }
        fs::copy(&source_path, &target_path)
            .map_err(|error| format!("failed to copy session jsonl: {error}"))?;
    }
    Ok(Some(target_path.to_string_lossy().to_string()))
}

fn copy_dependent_rows(source: &Connection, target: &Connection) -> Result<(), String> {
    for (table, column) in [
        ("thread_dynamic_tools", "thread_id"),
        ("thread_goals", "thread_id"),
        ("thread_spawn_edges", "parent_thread_id"),
        ("thread_spawn_edges", "child_thread_id"),
    ] {
        if table_exists(source, table)? && table_exists(target, table)? {
            copy_table_rows(source, target, table, column)?;
        }
    }
    Ok(())
}

fn copy_table_rows(source: &Connection, target: &Connection, table: &str, filter_column: &str) -> Result<(), String> {
    let source_columns = table_columns(source, table)?;
    let target_columns = table_columns(target, table)?;
    let columns = source_columns
        .into_iter()
        .filter(|column| target_columns.contains(column))
        .collect::<Vec<_>>();
    if columns.is_empty() || !columns.iter().any(|column| column == filter_column) {
        return Ok(());
    }
    let select = format!("SELECT {} FROM {table}", columns.join(", "));
    let mut statement = source
        .prepare(&select)
        .map_err(|error| format!("failed to prepare dependent row query: {error}"))?;
    let rows = statement
        .query_map([], |row| {
            let mut values = Vec::new();
            for index in 0..columns.len() {
                values.push(row.get::<usize, Value>(index)?);
            }
            Ok(values)
        })
        .map_err(|error| format!("failed to query dependent rows: {error}"))?;
    let placeholders = (0..columns.len()).map(|_| "?").collect::<Vec<_>>().join(", ");
    let insert = format!("INSERT OR IGNORE INTO {table} ({}) VALUES ({})", columns.join(", "), placeholders);
    for row in rows {
        let values = row.map_err(|error| format!("failed to read dependent row: {error}"))?;
        target
            .execute(&insert, rusqlite::params_from_iter(values))
            .map_err(|error| format!("failed to insert dependent row: {error}"))?;
    }
    Ok(())
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

fn table_exists(conn: &Connection, table: &str) -> Result<bool, String> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |row| row.get(0),
        )
        .map_err(|error| format!("failed to check table existence: {error}"))?;
    Ok(count > 0)
}

fn sha256_file(path: &Path) -> Result<Vec<u8>, String> {
    let bytes = fs::read(path).map_err(|error| format!("failed to read file for hash: {error}"))?;
    Ok(Sha256::digest(bytes).to_vec())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rusqlite::Connection;
    use tempfile::tempdir;

    use super::sync_sessions;

    fn create_db(path: &std::path::Path, threads: &[(&str, &str)]) {
        let conn = Connection::open(path).unwrap();
        conn.execute(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, updated_at INTEGER, updated_at_ms INTEGER)",
            [],
        )
        .unwrap();
        for (id, rollout_path) in threads {
            conn.execute(
                "INSERT INTO threads (id, rollout_path, updated_at, updated_at_ms) VALUES (?1, ?2, 1, 1000)",
                (id, rollout_path),
            )
            .unwrap();
        }
    }

    #[test]
    fn copies_new_threads_and_jsonl_without_duplicates() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let source_jsonl = source.path().join("sessions/2026/06/23/rollout-thread-b.jsonl");
        fs::create_dir_all(source_jsonl.parent().unwrap()).unwrap();
        fs::write(&source_jsonl, r#"{"type":"session_meta","payload":{"id":"thread-b"}}"#).unwrap();
        create_db(&source.path().join("state_5.sqlite"), &[("thread-a", "a.jsonl"), ("thread-b", source_jsonl.to_str().unwrap())]);
        create_db(&target.path().join("state_5.sqlite"), &[("thread-a", "a.jsonl")]);

        let result = sync_sessions(&[source.path().to_path_buf()], target.path()).unwrap();
        let result_again = sync_sessions(&[source.path().to_path_buf()], target.path()).unwrap();

        assert_eq!(result.inserted_threads, 1);
        assert_eq!(result.copied_session_files, 1);
        assert_eq!(result_again.inserted_threads, 0);
        let conn = Connection::open(target.path().join("state_5.sqlite")).unwrap();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0)).unwrap();
        assert_eq!(count, 2);
        assert!(target.path().join("sessions/2026/06/23/rollout-thread-b.jsonl").exists());
    }

    #[test]
    fn rolls_back_thread_insert_when_dependent_row_copy_fails() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let source_conn = Connection::open(source.path().join("state_5.sqlite")).unwrap();
        source_conn
            .execute(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, updated_at INTEGER, updated_at_ms INTEGER)",
                [],
            )
            .unwrap();
        source_conn
            .execute(
                "INSERT INTO threads (id, rollout_path, updated_at, updated_at_ms) VALUES ('thread-b', NULL, 1, 1000)",
                [],
            )
            .unwrap();
        source_conn
            .execute("CREATE TABLE thread_dynamic_tools (thread_id TEXT, \"bad-name\" TEXT)", [])
            .unwrap();
        let target_conn = Connection::open(target.path().join("state_5.sqlite")).unwrap();
        target_conn
            .execute(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, updated_at INTEGER, updated_at_ms INTEGER)",
                [],
            )
            .unwrap();
        target_conn
            .execute("CREATE TABLE thread_dynamic_tools (thread_id TEXT, \"bad-name\" TEXT)", [])
            .unwrap();
        drop(source_conn);
        drop(target_conn);

        let result = sync_sessions(&[source.path().to_path_buf()], target.path());

        assert!(result.is_err());
        let conn = Connection::open(target.path().join("state_5.sqlite")).unwrap();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0)).unwrap();
        assert_eq!(count, 0);
    }
}

use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use rusqlite::{params_from_iter, types::Value as SqlValue, Connection, OpenFlags};
use serde::Serialize;
use serde_json::Value as JsonValue;
use walkdir::WalkDir;

use crate::{
    backup::{create_backup, BackupManifest},
    codex_paths::{local_codex_paths, resolve_user_codex_paths, CodexPaths},
};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ManagedSessionInventory {
    pub current_home: PathBuf,
    pub shared_home: PathBuf,
    pub total_count: usize,
    pub archived_count: usize,
    pub sessions: Vec<ManagedSessionRecord>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ManagedSessionRecord {
    pub id: String,
    pub title: Option<String>,
    pub preview: Option<String>,
    pub model_provider: Option<String>,
    pub updated_at: Option<i64>,
    pub updated_at_ms: Option<i64>,
    pub archived: bool,
    pub archived_at: Option<i64>,
    pub scope: String,
    pub current: Option<ManagedSessionLocation>,
    pub shared: Option<ManagedSessionLocation>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ManagedSessionLocation {
    pub home: PathBuf,
    pub rollout_path: Option<String>,
    pub session_file: Option<PathBuf>,
    pub archived: bool,
    pub archived_at: Option<i64>,
    pub updated_at: Option<i64>,
    pub updated_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionMutationResult {
    pub selected_count: usize,
    pub backups: Vec<BackupManifest>,
    pub deleted_threads: usize,
    pub deleted_session_files: usize,
    pub removed_session_index_entries: usize,
    pub restored_threads: usize,
}

#[derive(Debug, Clone)]
struct SessionSourceRecord {
    id: String,
    title: Option<String>,
    preview: Option<String>,
    model_provider: Option<String>,
    updated_at: Option<i64>,
    updated_at_ms: Option<i64>,
    archived: bool,
    archived_at: Option<i64>,
    rollout_path: Option<String>,
    session_file: Option<PathBuf>,
}

pub fn scan_managed_sessions(
    codex_home: &Path,
    shared_home: &Path,
) -> Result<ManagedSessionInventory, String> {
    let current_paths = resolve_user_codex_paths(codex_home);
    let shared_paths = local_codex_paths(shared_home);
    let current = scan_source(&current_paths)?;
    let shared = scan_source(&shared_paths)?;

    let mut ids = current
        .keys()
        .chain(shared.keys())
        .cloned()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    ids.sort_by(|left, right| {
        sort_millis(current.get(right).or_else(|| shared.get(right)))
            .cmp(&sort_millis(current.get(left).or_else(|| shared.get(left))))
            .then_with(|| left.cmp(right))
    });

    let mut sessions = Vec::new();
    for id in ids {
        let current_record = current.get(&id);
        let shared_record = shared.get(&id);
        let preferred = current_record.or(shared_record);
        let Some(preferred) = preferred else {
            continue;
        };
        let scope = match (current_record.is_some(), shared_record.is_some()) {
            (true, true) => "both",
            (true, false) => "current",
            (false, true) => "shared",
            (false, false) => "unknown",
        };
        sessions.push(ManagedSessionRecord {
            id: id.clone(),
            title: preferred.title.clone(),
            preview: preferred.preview.clone(),
            model_provider: preferred.model_provider.clone(),
            updated_at: preferred.updated_at,
            updated_at_ms: preferred.updated_at_ms,
            archived: preferred.archived,
            archived_at: preferred.archived_at,
            scope: scope.to_string(),
            current: current_record.map(|record| location_from_record(&current_paths, record)),
            shared: shared_record.map(|record| location_from_record(&shared_paths, record)),
        });
    }

    let archived_count = sessions.iter().filter(|session| session.archived).count();
    Ok(ManagedSessionInventory {
        current_home: codex_home.to_path_buf(),
        shared_home: shared_home.to_path_buf(),
        total_count: sessions.len(),
        archived_count,
        sessions,
    })
}

pub fn delete_managed_sessions(
    codex_home: &Path,
    shared_home: &Path,
    backup_root: &Path,
    ids: &[String],
    confirm_unarchived: bool,
) -> Result<SessionMutationResult, String> {
    let selected = normalized_ids(ids);
    if selected.is_empty() {
        return Ok(empty_result(0));
    }
    let inventory = scan_managed_sessions(codex_home, shared_home)?;
    let selected_set = selected.iter().cloned().collect::<HashSet<_>>();
    let has_unarchived = inventory
        .sessions
        .iter()
        .any(|session| selected_set.contains(&session.id) && !session.archived);
    if has_unarchived && !confirm_unarchived {
        return Err("未归档会话删除需要二次确认".to_string());
    }

    let mut result = empty_result(selected.len());
    result
        .backups
        .push(create_backup(codex_home, backup_root, "delete-sessions-current")?);
    fs::create_dir_all(shared_home)
        .map_err(|error| format!("failed to create shared sessions dir: {error}"))?;
    result
        .backups
        .push(create_backup(shared_home, backup_root, "delete-sessions-shared")?);

    let current_paths = resolve_user_codex_paths(codex_home);
    let shared_paths = local_codex_paths(shared_home);
    apply_delete_to_root(&current_paths, &selected_set, &mut result)?;
    apply_delete_to_root(&shared_paths, &selected_set, &mut result)?;
    Ok(result)
}

pub fn restore_sessions_visible(
    codex_home: &Path,
    backup_root: &Path,
    ids: &[String],
) -> Result<SessionMutationResult, String> {
    let selected = normalized_ids(ids);
    if selected.is_empty() {
        return Ok(empty_result(0));
    }
    let selected_set = selected.iter().cloned().collect::<HashSet<_>>();
    let mut result = empty_result(selected.len());
    result
        .backups
        .push(create_backup(codex_home, backup_root, "restore-sessions-visible")?);
    let paths = resolve_user_codex_paths(codex_home);
    result.restored_threads = restore_visible_in_db(&paths.state_db, &selected_set)?;
    Ok(result)
}

fn scan_source(paths: &CodexPaths) -> Result<HashMap<String, SessionSourceRecord>, String> {
    let mut records = read_thread_records(&paths.state_db)?;
    let session_files = read_session_files(&paths.sessions_dir)?;
    for (id, session_file) in session_files {
        records
            .entry(id.clone())
            .and_modify(|record| {
                if record.session_file.is_none() {
                    record.session_file = Some(session_file.clone());
                }
            })
            .or_insert_with(|| SessionSourceRecord {
                id,
                title: None,
                preview: None,
                model_provider: None,
                updated_at: file_modified_seconds(&session_file),
                updated_at_ms: file_modified_millis(&session_file),
                archived: false,
                archived_at: None,
                rollout_path: None,
                session_file: Some(session_file),
            });
    }
    for record in records.values_mut() {
        if record.session_file.is_none() {
            record.session_file = rollout_file_for_record(paths, record);
        }
    }
    Ok(records)
}

fn read_thread_records(path: &Path) -> Result<HashMap<String, SessionSourceRecord>, String> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| format!("failed to open state_5.sqlite read-only: {error}"))?;
    if !table_exists(&conn, "threads")? {
        return Ok(HashMap::new());
    }
    let columns = table_columns(&conn, "threads")?;
    if !columns.iter().any(|column| column == "id") {
        return Ok(HashMap::new());
    }
    let select = format!("SELECT {} FROM threads", columns.join(", "));
    let mut statement = conn
        .prepare(&select)
        .map_err(|error| format!("failed to prepare managed threads query: {error}"))?;
    let rows = statement
        .query_map([], |row| {
            let mut values = HashMap::new();
            for (index, column) in columns.iter().enumerate() {
                values.insert(column.clone(), row.get::<usize, SqlValue>(index)?);
            }
            Ok(record_from_values(values))
        })
        .map_err(|error| format!("failed to query managed threads: {error}"))?;
    let mut records = HashMap::new();
    for row in rows {
        let record =
            row.map_err(|error| format!("failed to collect managed threads: {error}"))?;
        if !record.id.is_empty() {
            records.insert(record.id.clone(), record);
        }
    }
    Ok(records)
}

fn read_session_files(path: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut output = Vec::new();
    for entry in WalkDir::new(path).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file()
            || !entry
                .path()
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
        {
            continue;
        }
        if let Some(id) = session_file_id(entry.path())? {
            output.push((id, entry.into_path()));
        }
    }
    Ok(output)
}

fn apply_delete_to_root(
    paths: &CodexPaths,
    ids: &HashSet<String>,
    result: &mut SessionMutationResult,
) -> Result<(), String> {
    result.deleted_threads += delete_db_rows(&paths.state_db, ids)?;
    result.deleted_session_files += delete_session_files(paths, ids)?;
    result.removed_session_index_entries += remove_session_index_entries(&paths.session_index, ids)?;
    Ok(())
}

fn delete_db_rows(path: &Path, ids: &HashSet<String>) -> Result<usize, String> {
    if !path.exists() {
        return Ok(0);
    }
    let conn = Connection::open(path).map_err(|error| format!("failed to open state_5.sqlite: {error}"))?;
    let mut deleted = 0;
    delete_matching_rows(&conn, "thread_dynamic_tools", &[("thread_id", ids)], &mut deleted)?;
    delete_matching_rows(&conn, "thread_goals", &[("thread_id", ids)], &mut deleted)?;
    delete_matching_rows(
        &conn,
        "thread_spawn_edges",
        &[("parent_thread_id", ids), ("child_thread_id", ids)],
        &mut deleted,
    )?;
    let before = deleted;
    delete_matching_rows(&conn, "threads", &[("id", ids)], &mut deleted)?;
    Ok(deleted - before)
}

fn delete_matching_rows(
    conn: &Connection,
    table: &str,
    filters: &[(&str, &HashSet<String>)],
    deleted: &mut usize,
) -> Result<(), String> {
    if !table_exists(conn, table)? {
        return Ok(());
    }
    let columns = table_columns(conn, table)?;
    let mut clauses = Vec::new();
    let mut values = Vec::new();
    for (column, ids) in filters {
        if !columns.iter().any(|existing| existing == column) || ids.is_empty() {
            continue;
        }
        let placeholders = (0..ids.len()).map(|_| "?").collect::<Vec<_>>().join(", ");
        clauses.push(format!("{column} IN ({placeholders})"));
        values.extend(ids.iter().cloned().map(SqlValue::Text));
    }
    if clauses.is_empty() {
        return Ok(());
    }
    let sql = format!("DELETE FROM {table} WHERE {}", clauses.join(" OR "));
    *deleted += conn
        .execute(&sql, params_from_iter(values))
        .map_err(|error| format!("failed to delete {table} rows: {error}"))?;
    Ok(())
}

fn delete_session_files(paths: &CodexPaths, ids: &HashSet<String>) -> Result<usize, String> {
    if !paths.sessions_dir.exists() {
        return Ok(0);
    }
    let mut deleted = 0;
    for entry in WalkDir::new(&paths.sessions_dir)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file()
            || !entry
                .path()
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
        {
            continue;
        }
        let Some(id) = session_file_id(entry.path())? else {
            continue;
        };
        if ids.contains(&id) && remove_file_under_root(entry.path(), &paths.codex_home)? {
            deleted += 1;
        }
    }
    Ok(deleted)
}

fn remove_session_index_entries(path: &Path, ids: &HashSet<String>) -> Result<usize, String> {
    if !path.exists() {
        return Ok(0);
    }
    let raw = fs::read_to_string(path)
        .map_err(|error| format!("failed to read session_index.jsonl: {error}"))?;
    let mut removed = 0;
    let mut output = String::new();
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let should_remove = session_index_line_id(line)
            .as_ref()
            .is_some_and(|id| ids.contains(id));
        if should_remove {
            removed += 1;
        } else {
            output.push_str(line);
            output.push('\n');
        }
    }
    if removed > 0 {
        fs::write(path, output)
            .map_err(|error| format!("failed to write session_index.jsonl: {error}"))?;
    }
    Ok(removed)
}

fn restore_visible_in_db(path: &Path, ids: &HashSet<String>) -> Result<usize, String> {
    if !path.exists() || ids.is_empty() {
        return Ok(0);
    }
    let conn = Connection::open(path).map_err(|error| format!("failed to open state_5.sqlite: {error}"))?;
    if !table_exists(&conn, "threads")? {
        return Ok(0);
    }
    let columns = table_columns(&conn, "threads")?;
    if !columns.iter().any(|column| column == "archived") {
        return Ok(0);
    }
    let mut assignments = vec!["archived = 0".to_string()];
    if columns.iter().any(|column| column == "archived_at") {
        assignments.push("archived_at = NULL".to_string());
    }
    let placeholders = (0..ids.len()).map(|_| "?").collect::<Vec<_>>().join(", ");
    let sql = format!(
        "UPDATE threads SET {} WHERE id IN ({placeholders})",
        assignments.join(", ")
    );
    conn.execute(
        &sql,
        params_from_iter(ids.iter().cloned().map(SqlValue::Text)),
    )
    .map_err(|error| format!("failed to restore visible sessions: {error}"))
}

fn rollout_file_for_record(paths: &CodexPaths, record: &SessionSourceRecord) -> Option<PathBuf> {
    let rollout_path = PathBuf::from(record.rollout_path.as_ref()?);
    let path = if rollout_path.is_absolute() {
        rollout_path
    } else {
        paths.codex_home.join(rollout_path)
    };
    if path.exists() {
        Some(path)
    } else {
        None
    }
}

fn remove_file_under_root(path: &Path, root: &Path) -> Result<bool, String> {
    let canonical_path = path
        .canonicalize()
        .map_err(|error| format!("failed to resolve session file: {error}"))?;
    let canonical_root = root
        .canonicalize()
        .map_err(|error| format!("failed to resolve Codex home: {error}"))?;
    if !canonical_path.starts_with(canonical_root) {
        return Ok(false);
    }
    fs::remove_file(path).map_err(|error| format!("failed to delete session jsonl: {error}"))?;
    Ok(true)
}

fn location_from_record(paths: &CodexPaths, record: &SessionSourceRecord) -> ManagedSessionLocation {
    ManagedSessionLocation {
        home: paths.codex_home.clone(),
        rollout_path: record.rollout_path.clone(),
        session_file: record.session_file.clone(),
        archived: record.archived,
        archived_at: record.archived_at,
        updated_at: record.updated_at,
        updated_at_ms: record.updated_at_ms,
    }
}

fn record_from_values(values: HashMap<String, SqlValue>) -> SessionSourceRecord {
    SessionSourceRecord {
        id: text_value(values.get("id")).unwrap_or_default(),
        title: text_value(values.get("title")),
        preview: text_value(values.get("preview")),
        model_provider: text_value(values.get("model_provider")),
        updated_at: integer_value(values.get("updated_at")),
        updated_at_ms: integer_value(values.get("updated_at_ms")),
        archived: truthy_value(values.get("archived")),
        archived_at: integer_value(values.get("archived_at")),
        rollout_path: text_value(values.get("rollout_path")),
        session_file: None,
    }
}

fn session_file_id(path: &Path) -> Result<Option<String>, String> {
    let file =
        fs::File::open(path).map_err(|error| format!("failed to open session jsonl: {error}"))?;
    for line in BufReader::new(file).lines().take(25) {
        let line = line.map_err(|error| format!("failed to read session jsonl: {error}"))?;
        let Ok(value) = serde_json::from_str::<JsonValue>(&line) else {
            continue;
        };
        if value.get("type").and_then(JsonValue::as_str) != Some("session_meta") {
            continue;
        }
        if let Some(id) = value
            .get("payload")
            .and_then(|payload| payload.get("id"))
            .and_then(JsonValue::as_str)
        {
            return Ok(Some(id.to_string()));
        }
    }
    Ok(None)
}

fn session_index_line_id(line: &str) -> Option<String> {
    let value = serde_json::from_str::<JsonValue>(line).ok()?;
    value
        .get("id")
        .or_else(|| value.get("session_id"))
        .or_else(|| value.get("sessionId"))
        .and_then(JsonValue::as_str)
        .map(ToOwned::to_owned)
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

fn sort_millis(record: Option<&SessionSourceRecord>) -> Option<i64> {
    record.and_then(|record| record.updated_at_ms.or(record.updated_at.map(|value| value * 1000)))
}

fn file_modified_millis(path: &Path) -> Option<i64> {
    fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
}

fn file_modified_seconds(path: &Path) -> Option<i64> {
    file_modified_millis(path).map(|value| value / 1000)
}

fn normalized_ids(ids: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    ids.iter()
        .map(|id| id.trim())
        .filter(|id| !id.is_empty())
        .filter(|id| seen.insert((*id).to_string()))
        .map(ToOwned::to_owned)
        .collect()
}

fn empty_result(selected_count: usize) -> SessionMutationResult {
    SessionMutationResult {
        selected_count,
        backups: Vec::new(),
        deleted_threads: 0,
        deleted_session_files: 0,
        removed_session_index_entries: 0,
        restored_threads: 0,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rusqlite::Connection;
    use tempfile::tempdir;

    use super::{delete_managed_sessions, restore_sessions_visible, scan_managed_sessions};

    fn create_db(path: &std::path::Path, rows: &[(&str, &str, i64, i64, &str)]) {
        let conn = Connection::open(path).unwrap();
        conn.execute(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                rollout_path TEXT,
                updated_at INTEGER,
                updated_at_ms INTEGER,
                title TEXT,
                preview TEXT,
                model_provider TEXT,
                archived INTEGER,
                archived_at INTEGER
            )",
            [],
        )
        .unwrap();
        for (id, rollout_path, updated_at_ms, archived, title) in rows {
            conn.execute(
                "INSERT INTO threads (id, rollout_path, updated_at, updated_at_ms, title, preview, model_provider, archived, archived_at)
                 VALUES (?1, ?2, 1, ?3, ?5, '', 'openai', ?4, CASE WHEN ?4 = 1 THEN 123 ELSE NULL END)",
                (id, rollout_path, updated_at_ms, archived, title),
            )
            .unwrap();
        }
    }

    fn write_jsonl(home: &std::path::Path, id: &str) -> std::path::PathBuf {
        let path = home.join(format!("sessions/2026/06/30/rollout-{id}.jsonl"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            format!(r#"{{"type":"session_meta","payload":{{"id":"{id}"}}}}"#),
        )
        .unwrap();
        path
    }

    #[test]
    fn merged_view_prefers_current_home_over_shared_sessions() {
        let current = tempdir().unwrap();
        let shared = tempdir().unwrap();
        let current_jsonl = write_jsonl(current.path(), "thread-a");
        let shared_jsonl = write_jsonl(shared.path(), "thread-a");
        write_jsonl(shared.path(), "thread-b");
        create_db(
            &current.path().join("state_5.sqlite"),
            &[("thread-a", current_jsonl.to_str().unwrap(), 2000, 0, "Current")],
        );
        create_db(
            &shared.path().join("state_5.sqlite"),
            &[("thread-a", shared_jsonl.to_str().unwrap(), 1000, 1, "Shared")],
        );

        let inventory = scan_managed_sessions(current.path(), shared.path()).unwrap();

        let thread_a = inventory
            .sessions
            .iter()
            .find(|session| session.id == "thread-a")
            .unwrap();
        assert_eq!(thread_a.scope, "both");
        assert_eq!(thread_a.title.as_deref(), Some("Current"));
        assert!(!thread_a.archived);
        assert_eq!(inventory.total_count, 2);
    }

    #[test]
    fn delete_requires_confirmation_for_unarchived_and_then_deletes_both_roots() {
        let current = tempdir().unwrap();
        let shared = tempdir().unwrap();
        let backup = tempdir().unwrap();
        let current_jsonl = write_jsonl(current.path(), "thread-a");
        let shared_jsonl = write_jsonl(shared.path(), "thread-a");
        create_db(
            &current.path().join("state_5.sqlite"),
            &[("thread-a", current_jsonl.to_str().unwrap(), 1000, 0, "A")],
        );
        create_db(
            &shared.path().join("state_5.sqlite"),
            &[("thread-a", shared_jsonl.to_str().unwrap(), 1000, 0, "A")],
        );
        fs::write(
            current.path().join("session_index.jsonl"),
            "{\"id\":\"thread-a\"}\n",
        )
        .unwrap();

        let rejected = delete_managed_sessions(
            current.path(),
            shared.path(),
            backup.path(),
            &["thread-a".to_string()],
            false,
        )
        .unwrap_err();
        assert!(rejected.contains("二次确认"));

        let result = delete_managed_sessions(
            current.path(),
            shared.path(),
            backup.path(),
            &["thread-a".to_string()],
            true,
        )
        .unwrap();

        assert_eq!(result.backups.len(), 2);
        assert_eq!(result.deleted_threads, 2);
        assert_eq!(result.deleted_session_files, 2);
        assert_eq!(result.removed_session_index_entries, 1);
        assert!(!current_jsonl.exists());
        assert!(!shared_jsonl.exists());
    }

    #[test]
    fn restore_visible_only_updates_current_home() {
        let current = tempdir().unwrap();
        let backup = tempdir().unwrap();
        let current_jsonl = write_jsonl(current.path(), "thread-a");
        create_db(
            &current.path().join("state_5.sqlite"),
            &[("thread-a", current_jsonl.to_str().unwrap(), 1000, 1, "A")],
        );

        let result = restore_sessions_visible(
            current.path(),
            backup.path(),
            &["thread-a".to_string()],
        )
        .unwrap();

        assert_eq!(result.backups.len(), 1);
        assert_eq!(result.restored_threads, 1);
        let conn = Connection::open(current.path().join("state_5.sqlite")).unwrap();
        let archived: i64 = conn
            .query_row("SELECT archived FROM threads WHERE id = 'thread-a'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(archived, 0);
    }
}

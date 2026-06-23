use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::{types::Value, Connection, OpenFlags};
use serde::Serialize;
use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::codex_paths::{local_codex_paths, resolve_user_codex_paths, CodexPaths};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionSyncResult {
    pub inserted_threads: usize,
    pub copied_session_files: usize,
    pub duplicate_threads: usize,
    pub skipped_missing_session_files: usize,
    pub merged_session_index_entries: usize,
}

#[derive(Debug, Clone)]
struct SyncRoot {
    root: PathBuf,
    state_db: PathBuf,
    sessions_dir: PathBuf,
    session_index: PathBuf,
}

#[derive(Debug, Clone)]
struct SourceThread {
    id: String,
    values_by_column: HashMap<String, Value>,
    session_file: PathBuf,
    meta: SessionMeta,
}

#[derive(Debug, Clone, Default)]
struct SessionMeta {
    id: String,
    model_provider: Option<String>,
    source: Option<String>,
    cwd: Option<String>,
    cli_version: Option<String>,
    timestamp_millis: Option<i64>,
}

#[derive(Debug, Clone)]
struct SourceRow {
    values_by_column: HashMap<String, Value>,
}

#[derive(Debug, Clone)]
struct TableColumn {
    name: String,
    not_null: bool,
    default_value: Option<String>,
}

struct RolloutCopy {
    path: String,
    copied: bool,
}

pub fn sync_sessions(
    source_homes: &[PathBuf],
    target_home: &Path,
) -> Result<SessionSyncResult, String> {
    let sources = source_homes
        .iter()
        .map(|home| root_from_paths(local_codex_paths(home)))
        .collect::<Vec<_>>();
    sync_session_roots(
        &sources,
        root_from_paths(local_codex_paths(target_home)),
        None,
    )
}

pub fn sync_sessions_for_provider(
    source_homes: &[PathBuf],
    target_home: &Path,
    provider_id: &str,
) -> Result<SessionSyncResult, String> {
    let sources = source_homes
        .iter()
        .map(|home| root_from_paths(local_codex_paths(home)))
        .collect::<Vec<_>>();
    sync_session_roots(
        &sources,
        root_from_paths(local_codex_paths(target_home)),
        Some(provider_id),
    )
}

pub fn sync_user_home_to_shared(
    codex_home: &Path,
    shared_home: &Path,
) -> Result<SessionSyncResult, String> {
    sync_session_roots(
        &[root_from_paths(resolve_user_codex_paths(codex_home))],
        root_from_paths(local_codex_paths(shared_home)),
        None,
    )
}

pub fn sync_shared_to_user_home(
    shared_home: &Path,
    codex_home: &Path,
    provider_id: &str,
) -> Result<SessionSyncResult, String> {
    sync_session_roots(
        &[root_from_paths(local_codex_paths(shared_home))],
        root_from_paths(resolve_user_codex_paths(codex_home)),
        Some(provider_id),
    )
}

fn root_from_paths(paths: CodexPaths) -> SyncRoot {
    SyncRoot {
        root: paths.codex_home,
        state_db: paths.state_db,
        sessions_dir: paths.sessions_dir,
        session_index: paths.session_index,
    }
}

fn sync_session_roots(
    source_roots: &[SyncRoot],
    target_root: SyncRoot,
    provider_id: Option<&str>,
) -> Result<SessionSyncResult, String> {
    let target_conn = Connection::open(&target_root.state_db)
        .map_err(|error| format!("failed to open target state_5.sqlite: {error}"))?;
    target_conn
        .execute_batch("BEGIN IMMEDIATE")
        .map_err(|error| format!("failed to start session sync transaction: {error}"))?;

    let result =
        sync_sessions_in_transaction(source_roots, &target_root, &target_conn, provider_id);
    match result {
        Ok(mut result) => {
            target_conn
                .execute_batch("COMMIT")
                .map_err(|error| format!("failed to commit session sync transaction: {error}"))?;
            for source_root in source_roots {
                result.merged_session_index_entries +=
                    merge_session_index(source_root, &target_root)?;
            }
            Ok(result)
        }
        Err(error) => {
            let _ = target_conn.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

fn sync_sessions_in_transaction(
    source_roots: &[SyncRoot],
    target_root: &SyncRoot,
    target_conn: &Connection,
    provider_id: Option<&str>,
) -> Result<SessionSyncResult, String> {
    let mut inserted_threads = 0;
    let mut copied_session_files = 0;
    let mut duplicate_threads = 0;
    let mut skipped_missing_session_files = 0;

    for source_root in source_roots {
        let source_conn = open_source_conn(source_root)?;
        let source_threads = read_source_threads(source_root, source_conn.as_ref())?;
        let candidate_ids = source_threads
            .iter()
            .map(|thread| thread.id.clone())
            .collect::<HashSet<_>>();
        skipped_missing_session_files +=
            count_db_rows_without_session_file(source_conn.as_ref(), &candidate_ids)?;

        for thread in source_threads {
            let copied_rollout = copy_rollout_file(target_root, &thread)?;
            if thread_exists(target_conn, &thread.id)? {
                duplicate_threads += 1;
                update_existing_thread(
                    target_conn,
                    &thread.id,
                    Some(copied_rollout.path.as_str()),
                    provider_id,
                )?;
                if copied_rollout.copied {
                    copied_session_files += 1;
                }
                continue;
            }
            insert_thread(
                target_conn,
                &thread,
                copied_rollout.path.as_str(),
                provider_id,
            )?;
            inserted_threads += 1;
            if copied_rollout.copied {
                copied_session_files += 1;
            }
        }
        if let Some(source_conn) = source_conn.as_ref() {
            copy_dependent_rows(source_conn, target_conn, &candidate_ids)?;
        }
    }

    if let Some(provider_id) = provider_id {
        normalize_thread_providers(target_conn, provider_id)?;
        normalize_session_metadata_provider(target_root, provider_id)?;
    }

    Ok(SessionSyncResult {
        inserted_threads,
        copied_session_files,
        duplicate_threads,
        skipped_missing_session_files,
        merged_session_index_entries: 0,
    })
}

fn open_source_conn(source_root: &SyncRoot) -> Result<Option<Connection>, String> {
    if !source_root.state_db.exists() {
        return Ok(None);
    }
    Connection::open_with_flags(&source_root.state_db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map(Some)
        .map_err(|error| format!("failed to open source state_5.sqlite: {error}"))
}

fn read_source_threads(
    source_root: &SyncRoot,
    source_conn: Option<&Connection>,
) -> Result<Vec<SourceThread>, String> {
    let source_rows = if let Some(conn) = source_conn {
        read_source_thread_rows(conn)?
    } else {
        HashMap::new()
    };
    let mut threads = Vec::new();
    for (session_file, meta) in read_session_files(source_root)? {
        if meta.id.trim().is_empty() {
            continue;
        }
        let row = source_rows.get(&meta.id);
        threads.push(SourceThread {
            id: meta.id.clone(),
            values_by_column: row
                .map(|row| row.values_by_column.clone())
                .unwrap_or_default(),
            session_file,
            meta,
        });
    }
    Ok(threads)
}

fn read_source_thread_rows(conn: &Connection) -> Result<HashMap<String, SourceRow>, String> {
    if !table_exists(conn, "threads")? {
        return Ok(HashMap::new());
    }
    let columns = table_columns(conn, "threads")?;
    if !columns.iter().any(|column| column == "id") {
        return Ok(HashMap::new());
    }
    let id_index = columns.iter().position(|column| column == "id").unwrap();
    let select = format!("SELECT {} FROM threads", columns.join(", "));
    let mut statement = conn
        .prepare(&select)
        .map_err(|error| format!("failed to prepare source thread query: {error}"))?;
    let rows = statement
        .query_map([], |row| {
            let mut values = HashMap::new();
            let mut id = String::new();
            for (index, column) in columns.iter().enumerate() {
                let value = row.get::<usize, Value>(index)?;
                if index == id_index {
                    if let Value::Text(text) = &value {
                        id = text.clone();
                    }
                }
                values.insert(column.clone(), value);
            }
            Ok((
                id,
                SourceRow {
                    values_by_column: values,
                },
            ))
        })
        .map_err(|error| format!("failed to read source threads: {error}"))?;

    let mut output = HashMap::new();
    for row in rows {
        let (id, source_row) =
            row.map_err(|error| format!("failed to collect source threads: {error}"))?;
        if !id.is_empty() {
            output.insert(id, source_row);
        }
    }
    Ok(output)
}

fn read_session_files(source_root: &SyncRoot) -> Result<Vec<(PathBuf, SessionMeta)>, String> {
    if !source_root.sessions_dir.exists() {
        return Ok(Vec::new());
    }
    let mut files = WalkDir::new(&source_root.sessions_dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.file_type().is_file()
                && entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
        })
        .map(|entry| entry.into_path())
        .collect::<Vec<_>>();
    files.sort();

    let mut output = Vec::new();
    let mut seen = HashSet::new();
    for path in files {
        let Some(meta) = session_file_meta(&path)? else {
            continue;
        };
        if seen.insert(meta.id.clone()) {
            output.push((path, meta));
        }
    }
    Ok(output)
}

fn count_db_rows_without_session_file(
    source_conn: Option<&Connection>,
    candidate_ids: &HashSet<String>,
) -> Result<usize, String> {
    let Some(conn) = source_conn else {
        return Ok(0);
    };
    if !table_exists(conn, "threads")? {
        return Ok(0);
    }
    let mut statement = conn
        .prepare("SELECT id FROM threads")
        .map_err(|error| format!("failed to prepare missing session query: {error}"))?;
    let rows = statement
        .query_map([], |row| row.get::<usize, String>(0))
        .map_err(|error| format!("failed to query missing sessions: {error}"))?;
    let mut missing = 0;
    for row in rows {
        let id = row.map_err(|error| format!("failed to read missing session row: {error}"))?;
        if !candidate_ids.contains(&id) {
            missing += 1;
        }
    }
    Ok(missing)
}

fn insert_thread(
    conn: &Connection,
    thread: &SourceThread,
    rollout_path: &str,
    provider_id: Option<&str>,
) -> Result<(), String> {
    let schema = table_schema(conn, "threads")?;
    let columns = schema
        .iter()
        .map(|column| column.name.clone())
        .collect::<Vec<_>>();
    let values = schema
        .iter()
        .map(|column| thread_value_for_target_column(thread, column, rollout_path, provider_id))
        .collect::<Result<Vec<_>, _>>()?;
    let placeholders = (0..columns.len())
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "INSERT OR IGNORE INTO threads ({}) VALUES ({})",
        columns.join(", "),
        placeholders
    );
    conn.execute(&sql, rusqlite::params_from_iter(values))
        .map_err(|error| format!("failed to insert target thread: {error}"))?;
    Ok(())
}

fn thread_value_for_target_column(
    thread: &SourceThread,
    column: &TableColumn,
    rollout_path: &str,
    provider_id: Option<&str>,
) -> Result<Value, String> {
    if column.name == "rollout_path" {
        return Ok(Value::Text(rollout_path.to_string()));
    }
    if column.name == "model_provider" {
        return Ok(Value::Text(
            provider_id
                .map(ToOwned::to_owned)
                .or_else(|| thread.meta.model_provider.clone())
                .unwrap_or_else(|| "openai".to_string()),
        ));
    }
    if let Some(value) = thread.values_by_column.get(&column.name) {
        return Ok(value.clone());
    }
    let file_ms = file_modified_millis(&thread.session_file).unwrap_or_else(now_millis);
    let value = match column.name.as_str() {
        "id" => Value::Text(thread.id.clone()),
        "created_at" | "updated_at" | "recency_at" => {
            Value::Integer(thread.meta.timestamp_millis.unwrap_or(file_ms) / 1000)
        }
        "created_at_ms" | "updated_at_ms" | "recency_at_ms" => {
            Value::Integer(thread.meta.timestamp_millis.unwrap_or(file_ms))
        }
        "source" => Value::Text(
            thread
                .meta
                .source
                .clone()
                .unwrap_or_else(|| "cli".to_string()),
        ),
        "cwd" => Value::Text(thread.meta.cwd.clone().unwrap_or_default()),
        "cli_version" => Value::Text(thread.meta.cli_version.clone().unwrap_or_default()),
        "title" | "preview" | "first_user_message" | "sandbox_policy" | "approval_mode" => {
            Value::Text(String::new())
        }
        "tokens_used" | "has_user_event" | "archived" => Value::Integer(0),
        "memory_mode" => Value::Text("enabled".to_string()),
        "thread_source" | "agent_nickname" | "agent_role" | "agent_path" | "model"
        | "reasoning_effort" | "archived_at" | "git_sha" | "git_branch" | "git_origin_url" => {
            Value::Null
        }
        _ if column.default_value.is_some() || !column.not_null => Value::Null,
        _ => Value::Text(String::new()),
    };
    Ok(value)
}

fn thread_exists(conn: &Connection, id: &str) -> Result<bool, String> {
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM threads WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .map_err(|error| format!("failed to check target thread: {error}"))?;
    Ok(count > 0)
}

fn update_existing_thread(
    conn: &Connection,
    thread_id: &str,
    rollout_path: Option<&str>,
    provider_id: Option<&str>,
) -> Result<(), String> {
    let columns = table_columns(conn, "threads")?;
    let mut assignments = Vec::new();
    let mut values = Vec::new();
    if let Some(path) = rollout_path {
        if columns.iter().any(|column| column == "rollout_path") {
            assignments.push("rollout_path = ?");
            values.push(Value::Text(path.to_string()));
        }
    }
    if let Some(provider_id) = provider_id {
        if columns.iter().any(|column| column == "model_provider") {
            assignments.push("model_provider = ?");
            values.push(Value::Text(provider_id.to_string()));
        }
    }
    if assignments.is_empty() {
        return Ok(());
    }
    values.push(Value::Text(thread_id.to_string()));
    let sql = format!("UPDATE threads SET {} WHERE id = ?", assignments.join(", "));
    conn.execute(&sql, rusqlite::params_from_iter(values))
        .map_err(|error| format!("failed to update target thread: {error}"))?;
    Ok(())
}

fn copy_rollout_file(target_root: &SyncRoot, thread: &SourceThread) -> Result<RolloutCopy, String> {
    let relative = rollout_relative_path(&thread.session_file);
    let mut target_path = target_root.root.join(relative);
    if target_path.exists() && sha256_file(&thread.session_file)? != sha256_file(&target_path)? {
        let stem = target_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("rollout")
            .to_string();
        target_path.set_file_name(format!("{stem}-imported.jsonl"));
    }
    let mut copied = false;
    if !target_path.exists() {
        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("failed to create target session dir: {error}"))?;
        }
        fs::copy(&thread.session_file, &target_path)
            .map_err(|error| format!("failed to copy session jsonl: {error}"))?;
        copied = true;
    }
    Ok(RolloutCopy {
        path: target_path.to_string_lossy().to_string(),
        copied,
    })
}

fn rollout_relative_path(source_path: &Path) -> PathBuf {
    relative_from_sessions(source_path).unwrap_or_else(|| {
        PathBuf::from("sessions").join(source_path.file_name().unwrap_or_default())
    })
}

fn relative_from_sessions(path: &Path) -> Option<PathBuf> {
    let mut relative = PathBuf::new();
    let mut found = false;
    for component in path.components() {
        let text = component.as_os_str().to_string_lossy();
        if found || text.eq_ignore_ascii_case("sessions") {
            relative.push(component.as_os_str());
            found = true;
        }
    }
    found.then_some(relative)
}

fn session_file_meta(path: &Path) -> Result<Option<SessionMeta>, String> {
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
        let Some(payload) = value.get("payload") else {
            continue;
        };
        let Some(id) = payload.get("id").and_then(JsonValue::as_str) else {
            continue;
        };
        return Ok(Some(SessionMeta {
            id: id.to_string(),
            model_provider: payload
                .get("model_provider")
                .and_then(JsonValue::as_str)
                .map(ToOwned::to_owned),
            source: payload
                .get("source")
                .and_then(JsonValue::as_str)
                .map(ToOwned::to_owned),
            cwd: payload
                .get("cwd")
                .and_then(JsonValue::as_str)
                .map(ToOwned::to_owned),
            cli_version: payload
                .get("cli_version")
                .and_then(JsonValue::as_str)
                .map(ToOwned::to_owned),
            timestamp_millis: None,
        }));
    }
    Ok(None)
}

fn normalize_thread_providers(conn: &Connection, provider_id: &str) -> Result<(), String> {
    if !table_columns(conn, "threads")?
        .iter()
        .any(|column| column == "model_provider")
    {
        return Ok(());
    }
    conn.execute(
        "UPDATE threads SET model_provider = ?1 WHERE model_provider IS NULL OR model_provider <> ?1",
        [provider_id],
    )
    .map_err(|error| format!("failed to normalize thread providers: {error}"))?;
    Ok(())
}

fn normalize_session_metadata_provider(
    target_root: &SyncRoot,
    provider_id: &str,
) -> Result<(), String> {
    if !target_root.sessions_dir.exists() {
        return Ok(());
    }
    for entry in WalkDir::new(&target_root.sessions_dir)
        .into_iter()
        .filter_map(Result::ok)
    {
        if entry.file_type().is_file()
            && entry
                .path()
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
        {
            rewrite_session_metadata_provider(entry.path(), provider_id)?;
        }
    }
    Ok(())
}

fn rewrite_session_metadata_provider(path: &Path, provider_id: &str) -> Result<(), String> {
    let raw = fs::read_to_string(path)
        .map_err(|error| format!("failed to read session jsonl: {error}"))?;
    let mut changed = false;
    let mut output = String::with_capacity(raw.len());
    for line in raw.split_inclusive('\n') {
        let has_newline = line.ends_with('\n');
        let body = line.trim_end_matches(['\r', '\n']);
        let mut rewritten = None;
        if body.contains("session_meta") {
            if let Ok(mut value) = serde_json::from_str::<JsonValue>(body) {
                if value.get("type").and_then(JsonValue::as_str) == Some("session_meta") {
                    if let Some(payload) =
                        value.get_mut("payload").and_then(JsonValue::as_object_mut)
                    {
                        if payload.get("model_provider").and_then(JsonValue::as_str)
                            != Some(provider_id)
                        {
                            payload.insert(
                                "model_provider".to_string(),
                                JsonValue::String(provider_id.to_string()),
                            );
                            changed = true;
                        }
                        rewritten = Some(serde_json::to_string(&value).map_err(|error| {
                            format!("failed to serialize session metadata: {error}")
                        })?);
                    }
                }
            }
        }
        output.push_str(rewritten.as_deref().unwrap_or(body));
        if has_newline {
            output.push('\n');
        }
    }
    if changed {
        fs::write(path, output)
            .map_err(|error| format!("failed to write session jsonl: {error}"))?;
    }
    Ok(())
}

fn merge_session_index(source_root: &SyncRoot, target_root: &SyncRoot) -> Result<usize, String> {
    if !source_root.session_index.exists() {
        return Ok(0);
    }
    let source = fs::read_to_string(&source_root.session_index)
        .map_err(|error| format!("failed to read session_index.jsonl: {error}"))?;
    if source.trim().is_empty() {
        return Ok(0);
    }
    if let Some(parent) = target_root.session_index.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create session index parent: {error}"))?;
    }
    let target = fs::read_to_string(&target_root.session_index).unwrap_or_default();
    let mut seen = target
        .lines()
        .map(ToOwned::to_owned)
        .collect::<HashSet<_>>();
    let mut appended = Vec::new();
    for line in source.lines().filter(|line| !line.trim().is_empty()) {
        if seen.insert(line.to_string()) {
            appended.push(line.to_string());
        }
    }
    if appended.is_empty() {
        return Ok(0);
    }
    let mut output = target;
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
    for line in &appended {
        output.push_str(line);
        output.push('\n');
    }
    fs::write(&target_root.session_index, output)
        .map_err(|error| format!("failed to write session_index.jsonl: {error}"))?;
    Ok(appended.len())
}

fn copy_dependent_rows(
    source: &Connection,
    target: &Connection,
    candidate_ids: &HashSet<String>,
) -> Result<(), String> {
    if candidate_ids.is_empty() {
        return Ok(());
    }
    for (table, column) in [
        ("thread_dynamic_tools", "thread_id"),
        ("thread_goals", "thread_id"),
        ("thread_spawn_edges", "parent_thread_id"),
        ("thread_spawn_edges", "child_thread_id"),
    ] {
        if table_exists(source, table)? && table_exists(target, table)? {
            copy_table_rows(source, target, table, column, candidate_ids)?;
        }
    }
    Ok(())
}

fn copy_table_rows(
    source: &Connection,
    target: &Connection,
    table: &str,
    filter_column: &str,
    candidate_ids: &HashSet<String>,
) -> Result<(), String> {
    let source_columns = table_columns(source, table)?;
    let target_columns = table_columns(target, table)?;
    let columns = source_columns
        .into_iter()
        .filter(|column| target_columns.contains(column))
        .collect::<Vec<_>>();
    let Some(filter_index) = columns.iter().position(|column| column == filter_column) else {
        return Ok(());
    };
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
    let placeholders = (0..columns.len())
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(", ");
    let insert = format!(
        "INSERT OR IGNORE INTO {table} ({}) VALUES ({})",
        columns.join(", "),
        placeholders
    );
    for row in rows {
        let values = row.map_err(|error| format!("failed to read dependent row: {error}"))?;
        let include =
            matches!(values.get(filter_index), Some(Value::Text(id)) if candidate_ids.contains(id));
        if include {
            target
                .execute(&insert, rusqlite::params_from_iter(values))
                .map_err(|error| format!("failed to insert dependent row: {error}"))?;
        }
    }
    Ok(())
}

fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>, String> {
    table_schema(conn, table).map(|schema| schema.into_iter().map(|column| column.name).collect())
}

fn table_schema(conn: &Connection, table: &str) -> Result<Vec<TableColumn>, String> {
    let mut statement = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|error| format!("failed to inspect table {table}: {error}"))?;
    let rows = statement
        .query_map([], |row| {
            Ok(TableColumn {
                name: row.get::<usize, String>(1)?,
                not_null: row.get::<usize, i64>(3)? != 0,
                default_value: row.get::<usize, Option<String>>(4)?,
            })
        })
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

fn file_modified_millis(path: &Path) -> Option<i64> {
    fs::metadata(path)
        .ok()?
        .modified()
        .ok()
        .and_then(system_time_millis)
}

fn now_millis() -> i64 {
    system_time_millis(SystemTime::now()).unwrap_or(0)
}

fn system_time_millis(time: SystemTime) -> Option<i64> {
    let duration = time.duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(duration.as_millis()).ok()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rusqlite::Connection;
    use tempfile::tempdir;

    use super::{
        sync_sessions, sync_sessions_for_provider, sync_shared_to_user_home,
        sync_user_home_to_shared,
    };

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

    fn create_official_like_db(path: &std::path::Path, threads: &[(&str, &str)]) {
        let conn = Connection::open(path).unwrap();
        conn.execute(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                rollout_path TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                source TEXT NOT NULL,
                model_provider TEXT NOT NULL,
                cwd TEXT NOT NULL,
                title TEXT NOT NULL,
                sandbox_policy TEXT NOT NULL,
                approval_mode TEXT NOT NULL,
                tokens_used INTEGER NOT NULL DEFAULT 0,
                has_user_event INTEGER NOT NULL DEFAULT 0,
                archived INTEGER NOT NULL DEFAULT 0,
                cli_version TEXT NOT NULL DEFAULT '',
                first_user_message TEXT NOT NULL DEFAULT '',
                memory_mode TEXT NOT NULL DEFAULT 'enabled',
                model TEXT,
                reasoning_effort TEXT,
                created_at_ms INTEGER,
                updated_at_ms INTEGER,
                preview TEXT NOT NULL DEFAULT '',
                recency_at INTEGER NOT NULL DEFAULT 0,
                recency_at_ms INTEGER NOT NULL DEFAULT 0
            )",
            [],
        )
        .unwrap();
        for (id, rollout_path) in threads {
            conn.execute(
                "INSERT INTO threads (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, sandbox_policy, approval_mode, created_at_ms, updated_at_ms, recency_at, recency_at_ms) VALUES (?1, ?2, 1, 1, 'cli', 'openai', '', '', '', '', 1000, 1000, 1, 1000)",
                (id, rollout_path),
            )
            .unwrap();
        }
    }

    #[test]
    fn copies_new_threads_and_jsonl_without_duplicates() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let source_jsonl = source
            .path()
            .join("sessions/2026/06/23/rollout-thread-b.jsonl");
        fs::create_dir_all(source_jsonl.parent().unwrap()).unwrap();
        fs::write(
            &source_jsonl,
            r#"{"type":"session_meta","payload":{"id":"thread-b"}}"#,
        )
        .unwrap();
        create_db(
            &source.path().join("state_5.sqlite"),
            &[
                ("thread-a", "a.jsonl"),
                ("thread-b", source_jsonl.to_str().unwrap()),
            ],
        );
        create_db(
            &target.path().join("state_5.sqlite"),
            &[("thread-a", "a.jsonl")],
        );

        let result = sync_sessions(&[source.path().to_path_buf()], target.path()).unwrap();
        let result_again = sync_sessions(&[source.path().to_path_buf()], target.path()).unwrap();

        assert_eq!(result.inserted_threads, 1);
        assert_eq!(result.copied_session_files, 1);
        assert_eq!(result.skipped_missing_session_files, 1);
        assert_eq!(result_again.inserted_threads, 0);
        let conn = Connection::open(target.path().join("state_5.sqlite")).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2);
        assert!(target
            .path()
            .join("sessions/2026/06/23/rollout-thread-b.jsonl")
            .exists());
    }

    #[test]
    fn skips_db_rows_without_rollout_jsonl() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        create_db(
            &source.path().join("state_5.sqlite"),
            &[("thread-a", "missing.jsonl")],
        );
        create_db(&target.path().join("state_5.sqlite"), &[]);

        let result = sync_sessions(&[source.path().to_path_buf()], target.path()).unwrap();

        assert_eq!(result.inserted_threads, 0);
        assert_eq!(result.skipped_missing_session_files, 1);
        let conn = Connection::open(target.path().join("state_5.sqlite")).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn inserts_jsonl_only_thread_into_official_like_schema() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let source_jsonl = source
            .path()
            .join("sessions/2026/06/23/rollout-thread-jsonl-only.jsonl");
        fs::create_dir_all(source_jsonl.parent().unwrap()).unwrap();
        fs::write(
            &source_jsonl,
            r#"{"type":"session_meta","payload":{"id":"thread-jsonl-only","model_provider":"openai","source":"cli","cwd":"C:\\repo","cli_version":"1.0.0"}}"#,
        )
        .unwrap();
        create_official_like_db(&target.path().join("state_5.sqlite"), &[]);

        let result = sync_sessions(&[source.path().to_path_buf()], target.path()).unwrap();

        assert_eq!(result.inserted_threads, 1);
        let conn = Connection::open(target.path().join("state_5.sqlite")).unwrap();
        let (provider, cwd): (String, String) = conn
            .query_row(
                "SELECT model_provider, cwd FROM threads WHERE id = 'thread-jsonl-only'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(provider, "openai");
        assert_eq!(cwd, r"C:\repo");
    }

    #[test]
    fn repairs_duplicate_thread_rollout_and_normalizes_provider_metadata() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let source_jsonl = source
            .path()
            .join("sessions/2026/06/23/rollout-thread-a.jsonl");
        fs::create_dir_all(source_jsonl.parent().unwrap()).unwrap();
        fs::write(
            &source_jsonl,
            concat!(
                r#"{"type":"session_meta","payload":{"id":"thread-a","model_provider":"openai_custom"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"text":"do not rewrite openai_custom in content"}}"#,
                "\n",
            ),
        )
        .unwrap();
        create_db(
            &source.path().join("state_5.sqlite"),
            &[("thread-a", source_jsonl.to_str().unwrap())],
        );
        set_provider(source.path(), "openai_custom");

        let missing_target_rollout = target.path().join("sessions/2026/06/23/missing.jsonl");
        create_db(
            &target.path().join("state_5.sqlite"),
            &[("thread-a", missing_target_rollout.to_str().unwrap())],
        );
        set_provider(target.path(), "openai_custom");

        let result =
            sync_sessions_for_provider(&[source.path().to_path_buf()], target.path(), "openai")
                .unwrap();

        assert_eq!(result.inserted_threads, 0);
        assert_eq!(result.duplicate_threads, 1);
        assert_eq!(result.copied_session_files, 1);
        let target_jsonl = target
            .path()
            .join("sessions/2026/06/23/rollout-thread-a.jsonl");
        let jsonl = fs::read_to_string(&target_jsonl).unwrap();
        assert!(jsonl.contains(r#""model_provider":"openai""#));
        assert!(jsonl.contains("do not rewrite openai_custom in content"));

        let conn = Connection::open(target.path().join("state_5.sqlite")).unwrap();
        let (provider, rollout_path): (String, String) = conn
            .query_row(
                "SELECT model_provider, rollout_path FROM threads WHERE id = 'thread-a'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(provider, "openai");
        assert_eq!(std::path::PathBuf::from(rollout_path), target_jsonl);
    }

    #[test]
    fn merges_session_index_entries() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let source_jsonl = source
            .path()
            .join("sessions/2026/06/23/rollout-thread-a.jsonl");
        fs::create_dir_all(source_jsonl.parent().unwrap()).unwrap();
        fs::write(
            &source_jsonl,
            r#"{"type":"session_meta","payload":{"id":"thread-a"}}"#,
        )
        .unwrap();
        fs::write(
            source.path().join("session_index.jsonl"),
            "{\"id\":\"thread-a\",\"thread_name\":\"A\",\"updated_at\":\"now\"}\n",
        )
        .unwrap();
        create_db(
            &source.path().join("state_5.sqlite"),
            &[("thread-a", source_jsonl.to_str().unwrap())],
        );
        create_db(&target.path().join("state_5.sqlite"), &[]);

        let result = sync_sessions(&[source.path().to_path_buf()], target.path()).unwrap();
        let result_again = sync_sessions(&[source.path().to_path_buf()], target.path()).unwrap();

        assert_eq!(result.merged_session_index_entries, 1);
        assert_eq!(result_again.merged_session_index_entries, 0);
        assert!(
            fs::read_to_string(target.path().join("session_index.jsonl"))
                .unwrap()
                .contains("thread_name")
        );
    }

    #[test]
    fn user_home_sync_respects_configured_sqlite_home() {
        let home = tempdir().unwrap();
        let sqlite_home = tempdir().unwrap();
        let shared = tempdir().unwrap();
        fs::write(
            home.path().join("config.toml"),
            format!("sqlite_home = \"{}\"\n", sqlite_home.path().display()).replace('\\', "\\\\"),
        )
        .unwrap();
        let source_jsonl = home
            .path()
            .join("sessions/2026/06/23/rollout-thread-a.jsonl");
        fs::create_dir_all(source_jsonl.parent().unwrap()).unwrap();
        fs::write(
            &source_jsonl,
            r#"{"type":"session_meta","payload":{"id":"thread-a"}}"#,
        )
        .unwrap();
        create_db(
            &sqlite_home.path().join("state_5.sqlite"),
            &[("thread-a", source_jsonl.to_str().unwrap())],
        );
        create_db(&shared.path().join("state_5.sqlite"), &[]);

        let to_shared = sync_user_home_to_shared(home.path(), shared.path()).unwrap();
        let from_shared = sync_shared_to_user_home(shared.path(), home.path(), "openai").unwrap();

        assert_eq!(to_shared.inserted_threads, 1);
        assert!(from_shared.duplicate_threads >= 1);
        let conn = Connection::open(sqlite_home.path().join("state_5.sqlite")).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM threads WHERE id = 'thread-a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    fn set_provider(home: &std::path::Path, provider: &str) {
        let conn = Connection::open(home.join("state_5.sqlite")).unwrap();
        conn.execute("ALTER TABLE threads ADD COLUMN model_provider TEXT", [])
            .unwrap();
        conn.execute("UPDATE threads SET model_provider = ?1", [provider])
            .unwrap();
    }
}

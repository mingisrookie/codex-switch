use std::{
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rusqlite::{types::Value, Connection, OpenFlags};
use serde::Serialize;
use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};

use crate::{
    codex_paths::{local_codex_paths, resolve_user_codex_paths, CodexPaths},
    file_ops::{atomic_copy, atomic_rewrite, walk_jsonl_files},
};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionSyncResult {
    pub inserted_threads: usize,
    pub copied_session_files: usize,
    pub duplicate_threads: usize,
    pub skipped_missing_session_files: usize,
    pub skipped_archived_threads: usize,
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
        true,
        false,
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
        true,
        true,
    )
}

pub fn sync_user_home_to_shared(
    codex_home: &Path,
    shared_home: &Path,
) -> Result<SessionSyncResult, String> {
    sync_session_roots(
        &[root_from_paths(resolve_user_codex_paths(codex_home)?)],
        root_from_paths(local_codex_paths(shared_home)),
        None,
        true,
        false,
    )
}

pub fn sync_shared_to_user_home(
    shared_home: &Path,
    codex_home: &Path,
    provider_id: &str,
) -> Result<SessionSyncResult, String> {
    sync_session_roots(
        &[root_from_paths(local_codex_paths(shared_home))],
        root_from_paths(resolve_user_codex_paths(codex_home)?),
        Some(provider_id),
        true,
        true,
    )
}

pub fn sync_shared_to_user_home_hot(
    shared_home: &Path,
    codex_home: &Path,
    provider_id: &str,
) -> Result<SessionSyncResult, String> {
    sync_session_roots(
        &[root_from_paths(local_codex_paths(shared_home))],
        root_from_paths(resolve_user_codex_paths(codex_home)?),
        Some(provider_id),
        false,
        false,
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
    allow_existing_replacement: bool,
    rewrite_existing_provider: bool,
) -> Result<SessionSyncResult, String> {
    let target_conn = Connection::open(&target_root.state_db)
        .map_err(|error| format!("failed to open target state_5.sqlite: {error}"))?;
    target_conn
        .busy_timeout(Duration::from_secs(2))
        .map_err(|error| format!("failed to set target SQLite timeout: {error}"))?;
    target_conn
        .execute_batch("BEGIN IMMEDIATE")
        .map_err(|error| format!("failed to start session sync transaction: {error}"))?;

    let result = sync_sessions_in_transaction(
        source_roots,
        &target_root,
        &target_conn,
        provider_id,
        allow_existing_replacement,
        rewrite_existing_provider,
    );
    match result {
        Ok(mut result) => {
            target_conn
                .execute_batch("COMMIT")
                .map_err(|error| format!("failed to commit session sync transaction: {error}"))?;
            let quick_check: String = target_conn
                .query_row("PRAGMA quick_check", [], |row| row.get(0))
                .map_err(|error| format!("failed to verify target state_5.sqlite: {error}"))?;
            if quick_check != "ok" {
                return Err(format!(
                    "target state_5.sqlite failed quick_check: {quick_check}"
                ));
            }
            for source_root in source_roots {
                let allowlist = syncable_session_ids(source_root)?;
                result.merged_session_index_entries +=
                    merge_session_index(source_root, &target_root, &allowlist)?;
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
    allow_existing_replacement: bool,
    rewrite_existing_provider: bool,
) -> Result<SessionSyncResult, String> {
    let mut inserted_threads = 0;
    let mut copied_session_files = 0;
    let mut duplicate_threads = 0;
    let mut skipped_missing_session_files = 0;
    let mut skipped_archived_threads = 0;

    for source_root in source_roots {
        let source_conn = open_source_conn(source_root)?;
        let (source_threads, skipped_archived) =
            read_source_threads(source_root, source_conn.as_ref())?;
        skipped_archived_threads += skipped_archived;
        let candidate_ids = source_threads
            .iter()
            .map(|thread| thread.id.clone())
            .collect::<HashSet<_>>();
        skipped_missing_session_files +=
            count_db_rows_without_session_file(source_conn.as_ref(), &candidate_ids)?;

        for thread in source_threads {
            let copied_rollout =
                copy_rollout_file(target_root, &thread, allow_existing_replacement)?;
            if let Some(provider_id) = provider_id {
                if rewrite_existing_provider || copied_rollout.copied {
                    rewrite_session_metadata_provider(
                        Path::new(&copied_rollout.path),
                        provider_id,
                    )?;
                }
            }
            if thread_exists(target_conn, &thread.id)? {
                duplicate_threads += 1;
                let provider_for_thread = if rewrite_existing_provider || copied_rollout.copied {
                    provider_id
                } else {
                    None
                };
                update_existing_thread(
                    target_conn,
                    &thread.id,
                    Some(copied_rollout.path.as_str()),
                    provider_for_thread,
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

    Ok(SessionSyncResult {
        inserted_threads,
        copied_session_files,
        duplicate_threads,
        skipped_missing_session_files,
        skipped_archived_threads,
        merged_session_index_entries: 0,
    })
}

fn open_source_conn(source_root: &SyncRoot) -> Result<Option<Connection>, String> {
    if !source_root.state_db.exists() {
        return Ok(None);
    }
    let conn = Connection::open_with_flags(&source_root.state_db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| format!("failed to open source state_5.sqlite: {error}"))?;
    conn.busy_timeout(Duration::from_secs(2))
        .map_err(|error| format!("failed to set source SQLite timeout: {error}"))?;
    Ok(Some(conn))
}

fn read_source_threads(
    source_root: &SyncRoot,
    source_conn: Option<&Connection>,
) -> Result<(Vec<SourceThread>, usize), String> {
    let source_rows = if let Some(conn) = source_conn {
        read_source_thread_rows(conn)?
    } else {
        HashMap::new()
    };
    let mut threads = Vec::new();
    let mut skipped_archived = 0;
    for (session_file, meta) in read_session_files(source_root)? {
        if meta.id.trim().is_empty() {
            continue;
        }
        let row = source_rows.get(&meta.id);
        if row.is_some_and(source_row_is_archived) {
            skipped_archived += 1;
            continue;
        }
        threads.push(SourceThread {
            id: meta.id.clone(),
            values_by_column: row
                .map(|row| row.values_by_column.clone())
                .unwrap_or_default(),
            session_file,
            meta,
        });
    }
    Ok((threads, skipped_archived))
}

fn syncable_session_ids(source_root: &SyncRoot) -> Result<HashSet<String>, String> {
    let source_conn = open_source_conn(source_root)?;
    let (threads, _) = read_source_threads(source_root, source_conn.as_ref())?;
    Ok(threads.into_iter().map(|thread| thread.id).collect())
}

fn source_row_is_archived(row: &SourceRow) -> bool {
    archived_value_is_true(row.values_by_column.get("archived"))
}

fn archived_value_is_true(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Integer(value)) => *value != 0,
        Some(Value::Real(value)) => *value != 0.0,
        Some(Value::Text(value)) => {
            let normalized = value.trim().to_ascii_lowercase();
            matches!(normalized.as_str(), "1" | "true" | "yes")
        }
        _ => false,
    }
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
    let files = walk_jsonl_files(&source_root.sessions_dir)?;

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
    let columns = table_columns(conn, "threads")?;
    let has_archived = columns.iter().any(|column| column == "archived");
    let select = if has_archived {
        "SELECT id, archived FROM threads"
    } else {
        "SELECT id FROM threads"
    };
    let mut statement = conn
        .prepare(select)
        .map_err(|error| format!("failed to prepare missing session query: {error}"))?;
    let rows = statement
        .query_map([], |row| {
            let id = row.get::<usize, String>(0)?;
            let archived = if has_archived {
                Some(row.get::<usize, Value>(1)?)
            } else {
                None
            };
            Ok((id, archived))
        })
        .map_err(|error| format!("failed to query missing sessions: {error}"))?;
    let mut missing = 0;
    for row in rows {
        let (id, archived) =
            row.map_err(|error| format!("failed to read missing session row: {error}"))?;
        if archived_value_is_true(archived.as_ref()) {
            continue;
        }
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
        "INSERT INTO threads ({}) VALUES ({})",
        columns.join(", "),
        placeholders
    );
    let inserted = conn
        .execute(&sql, rusqlite::params_from_iter(values))
        .map_err(|error| format!("failed to insert target thread: {error}"))?;
    if inserted != 1 {
        return Err(format!("target thread insert affected {inserted} rows"));
    }
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
        _ => {
            return Err(format!(
                "unsupported threads schema: required column {} has no known value or default",
                column.name
            ))
        }
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

fn copy_rollout_file(
    target_root: &SyncRoot,
    thread: &SourceThread,
    allow_existing_replacement: bool,
) -> Result<RolloutCopy, String> {
    let relative = rollout_relative_path(&thread.session_file);
    let mut target_path = target_root.root.join(relative);
    let mut copied = false;
    if target_path.exists() {
        let source_hash = sha256_file(&thread.session_file)?;
        let target_hash = sha256_file(&target_path)?;
        if source_hash != target_hash {
            let source_len = fs::metadata(&thread.session_file)
                .map_err(|error| format!("failed to inspect source session jsonl: {error}"))?
                .len();
            let target_len = fs::metadata(&target_path)
                .map_err(|error| format!("failed to inspect target session jsonl: {error}"))?
                .len();
            if source_len > target_len && file_is_prefix(&target_path, &thread.session_file)? {
                if allow_existing_replacement {
                    atomic_copy(&thread.session_file, &target_path)?;
                    copied = true;
                }
            } else if !(target_len > source_len
                && file_is_prefix(&thread.session_file, &target_path)?)
            {
                let stem = target_path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or("rollout")
                    .to_string();
                let hash_suffix = source_hash
                    .iter()
                    .take(6)
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>();
                target_path.set_file_name(format!("{stem}-imported-{hash_suffix}.jsonl"));
                if !target_path.exists() {
                    atomic_copy(&thread.session_file, &target_path)?;
                    copied = true;
                }
            }
        }
    } else {
        atomic_copy(&thread.session_file, &target_path)?;
        copied = true;
    }
    Ok(RolloutCopy {
        path: target_path.to_string_lossy().to_string(),
        copied,
    })
}

fn file_is_prefix(shorter: &Path, longer: &Path) -> Result<bool, String> {
    let mut shorter = fs::File::open(shorter)
        .map_err(|error| format!("failed to open prefix candidate: {error}"))?;
    let mut longer = fs::File::open(longer)
        .map_err(|error| format!("failed to open session candidate: {error}"))?;
    let mut short_buffer = [0_u8; 64 * 1024];
    let mut long_buffer = [0_u8; 64 * 1024];
    loop {
        let short_read = shorter
            .read(&mut short_buffer)
            .map_err(|error| format!("failed to read prefix candidate: {error}"))?;
        if short_read == 0 {
            return Ok(true);
        }
        let mut offset = 0;
        while offset < short_read {
            let long_read = longer
                .read(&mut long_buffer[offset..short_read])
                .map_err(|error| format!("failed to read session candidate: {error}"))?;
            if long_read == 0 {
                return Ok(false);
            }
            offset += long_read;
        }
        if short_buffer[..short_read] != long_buffer[..short_read] {
            return Ok(false);
        }
    }
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

fn rewrite_session_metadata_provider(path: &Path, provider_id: &str) -> Result<(), String> {
    if session_file_meta(path)?.and_then(|meta| meta.model_provider)
        == Some(provider_id.to_string())
    {
        return Ok(());
    }
    let source_path = path.to_path_buf();
    atomic_rewrite(path, |output| {
        let source = fs::File::open(&source_path)
            .map_err(|error| format!("failed to open session jsonl for rewrite: {error}"))?;
        let mut reader = BufReader::new(source);
        let mut line = String::new();
        loop {
            line.clear();
            let read = reader
                .read_line(&mut line)
                .map_err(|error| format!("failed to read session jsonl: {error}"))?;
            if read == 0 {
                break;
            }
            let ending = if line.ends_with("\r\n") {
                "\r\n"
            } else if line.ends_with('\n') {
                "\n"
            } else {
                ""
            };
            let body = line.trim_end_matches(['\r', '\n']);
            let mut rewritten = None;
            if body.contains("session_meta") {
                if let Ok(mut value) = serde_json::from_str::<JsonValue>(body) {
                    if value.get("type").and_then(JsonValue::as_str) == Some("session_meta") {
                        if let Some(payload) =
                            value.get_mut("payload").and_then(JsonValue::as_object_mut)
                        {
                            payload.insert(
                                "model_provider".to_string(),
                                JsonValue::String(provider_id.to_string()),
                            );
                            rewritten = Some(serde_json::to_string(&value).map_err(|error| {
                                format!("failed to serialize session metadata: {error}")
                            })?);
                        }
                    }
                }
            }
            output
                .write_all(rewritten.as_deref().unwrap_or(body).as_bytes())
                .map_err(|error| format!("failed to write session jsonl rewrite: {error}"))?;
            output
                .write_all(ending.as_bytes())
                .map_err(|error| format!("failed to write session jsonl ending: {error}"))?;
        }
        Ok(())
    })
}

fn merge_session_index(
    source_root: &SyncRoot,
    target_root: &SyncRoot,
    allowlist: &HashSet<String>,
) -> Result<usize, String> {
    if !source_root.session_index.exists() {
        return Ok(0);
    }
    if let Some(parent) = target_root.session_index.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create session index parent: {error}"))?;
    }
    let mut seen = HashSet::new();
    let mut target_needs_newline = false;
    if target_root.session_index.exists() {
        let mut target = fs::File::open(&target_root.session_index)
            .map_err(|error| format!("failed to open target session_index.jsonl: {error}"))?;
        let length = target
            .metadata()
            .map_err(|error| format!("failed to inspect target session_index.jsonl: {error}"))?
            .len();
        if length > 0 {
            target
                .seek(SeekFrom::End(-1))
                .map_err(|error| format!("failed to seek target session_index.jsonl: {error}"))?;
            let mut last = [0_u8; 1];
            target.read_exact(&mut last).map_err(|error| {
                format!("failed to inspect target session_index.jsonl ending: {error}")
            })?;
            target_needs_newline = last[0] != b'\n';
            target
                .seek(SeekFrom::Start(0))
                .map_err(|error| format!("failed to rewind target session_index.jsonl: {error}"))?;
        }
        for line in BufReader::new(target).lines() {
            let line = line
                .map_err(|error| format!("failed to read target session_index.jsonl: {error}"))?;
            seen.insert(line);
        }
    }

    let source = fs::File::open(&source_root.session_index)
        .map_err(|error| format!("failed to open source session_index.jsonl: {error}"))?;
    let mut appended = Vec::new();
    for line in BufReader::new(source).lines() {
        let line =
            line.map_err(|error| format!("failed to read source session_index.jsonl: {error}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let Some(id) = session_index_line_id(&line) else {
            continue;
        };
        if !allowlist.contains(&id) {
            continue;
        }
        if seen.insert(line.clone()) {
            appended.push(line);
        }
    }
    if appended.is_empty() {
        return Ok(0);
    }
    let mut output = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&target_root.session_index)
        .map_err(|error| {
            format!("failed to open target session_index.jsonl for append: {error}")
        })?;
    let mut encoded = Vec::new();
    if target_needs_newline {
        encoded.push(b'\n');
    }
    for line in &appended {
        encoded.extend_from_slice(line.as_bytes());
        encoded.push(b'\n');
    }
    output
        .write_all(&encoded)
        .map_err(|error| format!("failed to append session_index.jsonl: {error}"))?;
    output
        .sync_data()
        .map_err(|error| format!("failed to sync session_index.jsonl: {error}"))?;
    Ok(appended.len())
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
    let mut file =
        fs::File::open(path).map_err(|error| format!("failed to open file for hash: {error}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("failed to read file for hash: {error}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().to_vec())
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
        merge_session_index, root_from_paths, sync_sessions, sync_sessions_for_provider,
        sync_shared_to_user_home, sync_shared_to_user_home_hot, sync_user_home_to_shared,
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
    fn appending_session_index_repairs_a_missing_trailing_newline() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        fs::write(
            source.path().join("session_index.jsonl"),
            "{\"id\":\"thread-b\"}\n",
        )
        .unwrap();
        fs::write(
            target.path().join("session_index.jsonl"),
            "{\"id\":\"thread-a\"}",
        )
        .unwrap();
        let source_root = root_from_paths(crate::codex_paths::local_codex_paths(source.path()));
        let target_root = root_from_paths(crate::codex_paths::local_codex_paths(target.path()));

        let appended = merge_session_index(
            &source_root,
            &target_root,
            &std::collections::HashSet::from(["thread-b".to_string()]),
        )
        .unwrap();

        assert_eq!(appended, 1);
        let index = fs::read_to_string(target.path().join("session_index.jsonl")).unwrap();
        assert_eq!(index.lines().count(), 2);
        assert!(index.contains("thread-a\"}\n{\"id\":\"thread-b"));
    }

    #[test]
    fn skips_archived_threads_and_session_index_entries() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let active_jsonl = source
            .path()
            .join("sessions/2026/06/23/rollout-active.jsonl");
        let archived_jsonl = source
            .path()
            .join("sessions/2026/06/23/rollout-archived.jsonl");
        fs::create_dir_all(active_jsonl.parent().unwrap()).unwrap();
        fs::write(
            &active_jsonl,
            r#"{"type":"session_meta","payload":{"id":"thread-active"}}"#,
        )
        .unwrap();
        fs::write(
            &archived_jsonl,
            r#"{"type":"session_meta","payload":{"id":"thread-archived"}}"#,
        )
        .unwrap();
        fs::write(
            source.path().join("session_index.jsonl"),
            "{\"id\":\"thread-active\"}\n{\"id\":\"thread-archived\"}\n",
        )
        .unwrap();
        create_db(
            &source.path().join("state_5.sqlite"),
            &[
                ("thread-active", active_jsonl.to_str().unwrap()),
                ("thread-archived", archived_jsonl.to_str().unwrap()),
            ],
        );
        let conn = Connection::open(source.path().join("state_5.sqlite")).unwrap();
        conn.execute(
            "ALTER TABLE threads ADD COLUMN archived INTEGER DEFAULT 0",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE threads SET archived = 1 WHERE id = 'thread-archived'",
            [],
        )
        .unwrap();
        create_db(&target.path().join("state_5.sqlite"), &[]);

        let result = sync_sessions(&[source.path().to_path_buf()], target.path()).unwrap();

        assert_eq!(result.inserted_threads, 1);
        assert_eq!(result.skipped_archived_threads, 1);
        assert!(!target
            .path()
            .join("sessions/2026/06/23/rollout-archived.jsonl")
            .exists());
        let target_index = fs::read_to_string(target.path().join("session_index.jsonl")).unwrap();
        assert!(target_index.contains("thread-active"));
        assert!(!target_index.contains("thread-archived"));
        let target_conn = Connection::open(target.path().join("state_5.sqlite")).unwrap();
        let archived_count: i64 = target_conn
            .query_row(
                "SELECT COUNT(*) FROM threads WHERE id = 'thread-archived'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(archived_count, 0);
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

    #[test]
    fn updates_a_stale_target_when_the_source_is_a_strictly_growing_jsonl() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let relative = "sessions/2026/07/13/rollout-thread-growing.jsonl";
        let source_jsonl = source.path().join(relative);
        let target_jsonl = target.path().join(relative);
        fs::create_dir_all(source_jsonl.parent().unwrap()).unwrap();
        fs::create_dir_all(target_jsonl.parent().unwrap()).unwrap();
        let first = concat!(
            r#"{"type":"session_meta","payload":{"id":"thread-growing"}}"#,
            "\n",
        );
        fs::write(&target_jsonl, first).unwrap();
        fs::write(
            &source_jsonl,
            format!(
                "{first}{}\n",
                r#"{"type":"response_item","payload":{"text":"new tail"}}"#
            ),
        )
        .unwrap();
        create_db(
            &source.path().join("state_5.sqlite"),
            &[("thread-growing", source_jsonl.to_str().unwrap())],
        );
        create_db(
            &target.path().join("state_5.sqlite"),
            &[("thread-growing", target_jsonl.to_str().unwrap())],
        );

        let result = sync_sessions(&[source.path().to_path_buf()], target.path()).unwrap();

        assert_eq!(result.copied_session_files, 1);
        assert!(fs::read_to_string(&target_jsonl)
            .unwrap()
            .contains("new tail"));
    }

    #[test]
    fn divergent_versions_use_content_hashes_instead_of_one_stale_imported_file() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let relative = "sessions/2026/07/13/rollout-thread-conflict.jsonl";
        let source_jsonl = source.path().join(relative);
        let target_jsonl = target.path().join(relative);
        fs::create_dir_all(source_jsonl.parent().unwrap()).unwrap();
        fs::create_dir_all(target_jsonl.parent().unwrap()).unwrap();
        fs::write(
            &target_jsonl,
            r#"{"type":"session_meta","payload":{"id":"thread-conflict","source":"target"}}"#,
        )
        .unwrap();
        fs::write(
            &source_jsonl,
            r#"{"type":"session_meta","payload":{"id":"thread-conflict","source":"source-one"}}"#,
        )
        .unwrap();
        create_db(
            &source.path().join("state_5.sqlite"),
            &[("thread-conflict", source_jsonl.to_str().unwrap())],
        );
        create_db(
            &target.path().join("state_5.sqlite"),
            &[("thread-conflict", target_jsonl.to_str().unwrap())],
        );

        sync_sessions(&[source.path().to_path_buf()], target.path()).unwrap();
        fs::write(
            &source_jsonl,
            r#"{"type":"session_meta","payload":{"id":"thread-conflict","source":"source-two"}}"#,
        )
        .unwrap();
        sync_sessions(&[source.path().to_path_buf()], target.path()).unwrap();

        let imported = fs::read_dir(target_jsonl.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains("-imported-"))
            .count();
        assert_eq!(imported, 2);
    }

    #[test]
    fn hot_sync_does_not_rewrite_an_existing_live_jsonl_for_provider_changes() {
        let shared = tempdir().unwrap();
        let home = tempdir().unwrap();
        let relative = "sessions/2026/07/13/rollout-thread-hot.jsonl";
        let shared_jsonl = shared.path().join(relative);
        let home_jsonl = home.path().join(relative);
        fs::create_dir_all(shared_jsonl.parent().unwrap()).unwrap();
        fs::create_dir_all(home_jsonl.parent().unwrap()).unwrap();
        fs::write(
            &shared_jsonl,
            r#"{"type":"session_meta","payload":{"id":"thread-hot","model_provider":"openai_custom"}}"#,
        )
        .unwrap();
        let live_bytes =
            br#"{"type":"session_meta","payload":{"id":"thread-hot","model_provider":"openai"}}"#;
        fs::write(&home_jsonl, live_bytes).unwrap();
        create_db(
            &shared.path().join("state_5.sqlite"),
            &[("thread-hot", shared_jsonl.to_str().unwrap())],
        );
        create_db(
            &home.path().join("state_5.sqlite"),
            &[("thread-hot", home_jsonl.to_str().unwrap())],
        );

        sync_shared_to_user_home_hot(shared.path(), home.path(), "openai").unwrap();

        assert_eq!(fs::read(&home_jsonl).unwrap(), live_bytes);
    }

    fn set_provider(home: &std::path::Path, provider: &str) {
        let conn = Connection::open(home.join("state_5.sqlite")).unwrap();
        conn.execute("ALTER TABLE threads ADD COLUMN model_provider TEXT", [])
            .unwrap();
        conn.execute("UPDATE threads SET model_provider = ?1", [provider])
            .unwrap();
    }
}

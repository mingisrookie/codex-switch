use std::{
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;

use rusqlite::{types::Value, Connection, OpenFlags};
use serde::Serialize;
use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};

use crate::{
    codex_paths::{local_codex_paths, resolve_user_codex_paths, CodexPaths},
    file_ops::{atomic_create, atomic_write, walk_jsonl_files},
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
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

struct SourceScan {
    threads: Vec<SourceThread>,
    candidate_ids: HashSet<String>,
    skipped_archived_threads: usize,
    skipped_missing_session_files: usize,
}

struct PlannedSourceThread {
    thread: SourceThread,
}

struct PreparedSource {
    source_conn: Option<Connection>,
    threads: Vec<PlannedSourceThread>,
    candidate_ids: HashSet<String>,
    skipped_archived_threads: usize,
    skipped_missing_session_files: usize,
}

#[derive(Debug, Clone)]
struct TableColumn {
    name: String,
    not_null: bool,
    default_value: Option<String>,
}

#[derive(Debug)]
struct RolloutCopy {
    path: String,
    copied: bool,
    source_meta: SessionMeta,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RolloutFileAction {
    Unchanged(PathBuf),
    Create(PathBuf),
    Import(PathBuf),
}

impl RolloutFileAction {
    fn target_path(&self) -> &Path {
        match self {
            Self::Unchanged(path) | Self::Create(path) | Self::Import(path) => path,
        }
    }

    fn writes_session_file(&self) -> bool {
        !matches!(self, Self::Unchanged(_))
    }
}

struct SessionIndexMergePlan {
    lines: Vec<String>,
    target_needs_newline: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionFileWritePolicy {
    Allow,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionIndexWritePolicy {
    MergeAtomic,
    Skip,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExistingRolloutPathPolicy {
    SelectMostComplete,
    // ChatGPT may keep appending the current rollout through an already-open handle.
    PreserveExisting,
}

#[derive(Debug, Clone, Copy)]
struct SessionSyncPolicy {
    allow_existing_replacement: bool,
    update_existing_provider: bool,
    existing_rollout_path: ExistingRolloutPathPolicy,
    session_files: SessionFileWritePolicy,
    session_index: SessionIndexWritePolicy,
}

impl SessionSyncPolicy {
    fn standard(
        allow_existing_replacement: bool,
        update_existing_provider: bool,
        session_files: SessionFileWritePolicy,
    ) -> Self {
        Self {
            allow_existing_replacement,
            update_existing_provider,
            existing_rollout_path: ExistingRolloutPathPolicy::SelectMostComplete,
            session_files,
            session_index: SessionIndexWritePolicy::closed(session_files),
        }
    }

    fn hot_current(session_files: SessionFileWritePolicy) -> Self {
        Self {
            allow_existing_replacement: false,
            update_existing_provider: false,
            existing_rollout_path: ExistingRolloutPathPolicy::PreserveExisting,
            session_files,
            session_index: SessionIndexWritePolicy::hot(session_files),
        }
    }
}

impl SessionIndexWritePolicy {
    fn closed(file_policy: SessionFileWritePolicy) -> Self {
        match file_policy {
            SessionFileWritePolicy::Allow => Self::MergeAtomic,
            SessionFileWritePolicy::Deny => Self::Deny,
        }
    }

    fn hot(file_policy: SessionFileWritePolicy) -> Self {
        match file_policy {
            SessionFileWritePolicy::Allow => Self::Skip,
            SessionFileWritePolicy::Deny => Self::Deny,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionFileRelation {
    Equal,
    LeftExtendsRight,
    RightExtendsLeft,
    Divergent,
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
        SessionSyncPolicy::standard(true, false, SessionFileWritePolicy::Allow),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StableSourceVersion {
    length: u64,
    sha256: Vec<u8>,
    meta: SessionMeta,
}

#[derive(Debug)]
struct StableSourceData {
    version: StableSourceVersion,
    normalized_line_hashes: Vec<Vec<u8>>,
}

struct StableRolloutPlan {
    action: RolloutFileAction,
    source: StableSourceData,
}

const SOURCE_STABILITY_ATTEMPTS: usize = 3;
const SOURCE_CHANGED_PREFIX: &str = "source session JSONL changed";

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
        SessionSyncPolicy::standard(true, true, SessionFileWritePolicy::Allow),
    )
}

pub fn sync_user_home_to_shared(
    codex_home: &Path,
    shared_home: &Path,
) -> Result<SessionSyncResult, String> {
    let current = resolve_user_codex_paths(codex_home)?;
    let shared = local_codex_paths(shared_home);
    sync_user_home_to_shared_with_paths(&current, &shared)
}

pub(crate) fn sync_user_home_to_shared_with_paths(
    current: &CodexPaths,
    shared: &CodexPaths,
) -> Result<SessionSyncResult, String> {
    sync_user_home_to_shared_with_policy(current, shared, SessionFileWritePolicy::Allow)
}

pub(crate) fn sync_user_home_to_shared_with_policy(
    current: &CodexPaths,
    shared: &CodexPaths,
    file_write_policy: SessionFileWritePolicy,
) -> Result<SessionSyncResult, String> {
    sync_session_roots(
        &[root_from_paths(current.clone())],
        root_from_paths(shared.clone()),
        None,
        SessionSyncPolicy::standard(true, false, file_write_policy),
    )
}

pub fn sync_shared_to_user_home(
    shared_home: &Path,
    codex_home: &Path,
    provider_id: &str,
) -> Result<SessionSyncResult, String> {
    let shared = local_codex_paths(shared_home);
    let current = resolve_user_codex_paths(codex_home)?;
    sync_shared_to_user_home_with_paths(&shared, &current, provider_id)
}

pub(crate) fn sync_shared_to_user_home_with_paths(
    shared: &CodexPaths,
    current: &CodexPaths,
    provider_id: &str,
) -> Result<SessionSyncResult, String> {
    sync_shared_to_user_home_with_policy(
        shared,
        current,
        provider_id,
        SessionFileWritePolicy::Allow,
    )
}

pub(crate) fn sync_shared_to_user_home_with_policy(
    shared: &CodexPaths,
    current: &CodexPaths,
    provider_id: &str,
    file_write_policy: SessionFileWritePolicy,
) -> Result<SessionSyncResult, String> {
    sync_session_roots(
        &[root_from_paths(shared.clone())],
        root_from_paths(current.clone()),
        Some(provider_id),
        SessionSyncPolicy::standard(true, true, file_write_policy),
    )
}

pub fn sync_shared_to_user_home_hot(
    shared_home: &Path,
    codex_home: &Path,
    provider_id: &str,
) -> Result<SessionSyncResult, String> {
    let shared = local_codex_paths(shared_home);
    let current = resolve_user_codex_paths(codex_home)?;
    sync_shared_to_user_home_hot_with_paths(&shared, &current, provider_id)
}

pub(crate) fn sync_shared_to_user_home_hot_with_paths(
    shared: &CodexPaths,
    current: &CodexPaths,
    provider_id: &str,
) -> Result<SessionSyncResult, String> {
    sync_shared_to_user_home_hot_with_policy(
        shared,
        current,
        provider_id,
        SessionFileWritePolicy::Allow,
    )
}

pub(crate) fn sync_shared_to_user_home_hot_with_policy(
    shared: &CodexPaths,
    current: &CodexPaths,
    provider_id: &str,
    file_write_policy: SessionFileWritePolicy,
) -> Result<SessionSyncResult, String> {
    sync_session_roots(
        &[root_from_paths(shared.clone())],
        root_from_paths(current.clone()),
        Some(provider_id),
        SessionSyncPolicy::hot_current(file_write_policy),
    )
}

pub(crate) fn runtime_switch_session_files_are_unchanged_with_paths(
    current: &CodexPaths,
    shared: &CodexPaths,
) -> Result<bool, String> {
    if !shared.state_db.is_file() {
        return Ok(false);
    }
    let current = root_from_paths(current.clone());
    let shared = root_from_paths(shared.clone());
    Ok(session_root_files_are_unchanged(&current, &shared)?
        && session_root_files_are_unchanged(&shared, &current)?)
}

fn session_root_files_are_unchanged(
    source_root: &SyncRoot,
    target_root: &SyncRoot,
) -> Result<bool, String> {
    let source_conn = open_source_conn(source_root)?;
    let source_scan = read_source_threads(source_root, source_conn.as_ref())?;
    for thread in &source_scan.threads {
        if plan_rollout_file(target_root, thread)?.writes_session_file() {
            return Ok(false);
        }
    }
    Ok(
        plan_session_index_merge(source_root, target_root, &source_scan.candidate_ids)?
            .lines
            .is_empty(),
    )
}

pub(crate) fn preflight_session_database(path: &Path, label: &str) -> Result<(), String> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|_| format!("failed to open {label} state_5.sqlite"))?;
    conn.busy_timeout(Duration::from_secs(2))
        .map_err(|_| format!("failed to inspect {label} state_5.sqlite"))?;
    let quick_check: String = conn
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(|_| format!("failed to inspect {label} state_5.sqlite"))?;
    if quick_check != "ok" {
        return Err(format!("{label} state_5.sqlite failed quick_check"));
    }
    validate_target_threads_schema(&conn)
        .map_err(|error| format!("{label} state_5.sqlite is incompatible: {error}"))
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
    policy: SessionSyncPolicy,
) -> Result<SessionSyncResult, String> {
    let prepared_sources = prepare_sources(
        source_roots,
        &target_root,
        policy.session_files,
        policy.session_index,
    )?;
    let target_conn = Connection::open(&target_root.state_db)
        .map_err(|error| format!("failed to open target state_5.sqlite: {error}"))?;
    target_conn
        .busy_timeout(Duration::from_secs(2))
        .map_err(|error| format!("failed to set target SQLite timeout: {error}"))?;
    target_conn
        .execute_batch("BEGIN IMMEDIATE")
        .map_err(|error| format!("failed to start session sync transaction: {error}"))?;

    let result = sync_sessions_in_transaction(
        &prepared_sources,
        &target_root,
        &target_conn,
        provider_id,
        policy,
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
            for (source_root, prepared) in source_roots.iter().zip(&prepared_sources) {
                result.merged_session_index_entries += merge_session_index_with_policy(
                    source_root,
                    &target_root,
                    &prepared.candidate_ids,
                    policy.session_index,
                )?;
            }
            Ok(result)
        }
        Err(error) => {
            let _ = target_conn.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

fn prepare_sources(
    source_roots: &[SyncRoot],
    target_root: &SyncRoot,
    file_write_policy: SessionFileWritePolicy,
    index_write_policy: SessionIndexWritePolicy,
) -> Result<Vec<PreparedSource>, String> {
    let mut prepared_sources = Vec::with_capacity(source_roots.len());
    for source_root in source_roots {
        let source_conn = open_source_conn(source_root)?;
        let source_scan = read_source_threads(source_root, source_conn.as_ref())?;
        let mut threads = Vec::with_capacity(source_scan.threads.len());
        for thread in source_scan.threads {
            if file_write_policy == SessionFileWritePolicy::Deny {
                let rollout_action = plan_rollout_file(target_root, &thread)?;
                if rollout_action.writes_session_file() {
                    return Err(
                        "session JSONL changed after fast-path planning; retry the runtime switch"
                            .to_string(),
                    );
                }
            }
            threads.push(PlannedSourceThread { thread });
        }
        if index_write_policy == SessionIndexWritePolicy::Deny
            && !plan_session_index_merge(source_root, target_root, &source_scan.candidate_ids)?
                .lines
                .is_empty()
        {
            return Err(
                "session index changed after fast-path planning; retry the runtime switch"
                    .to_string(),
            );
        }
        prepared_sources.push(PreparedSource {
            source_conn,
            threads,
            candidate_ids: source_scan.candidate_ids,
            skipped_archived_threads: source_scan.skipped_archived_threads,
            skipped_missing_session_files: source_scan.skipped_missing_session_files,
        });
    }
    Ok(prepared_sources)
}

fn sync_sessions_in_transaction(
    prepared_sources: &[PreparedSource],
    target_root: &SyncRoot,
    target_conn: &Connection,
    provider_id: Option<&str>,
    policy: SessionSyncPolicy,
) -> Result<SessionSyncResult, String> {
    let mut inserted_threads = 0;
    let mut copied_session_files = 0;
    let mut duplicate_threads = 0;
    let mut skipped_missing_session_files = 0;
    let mut skipped_archived_threads = 0;

    for prepared in prepared_sources {
        skipped_archived_threads += prepared.skipped_archived_threads;
        skipped_missing_session_files += prepared.skipped_missing_session_files;

        for planned in &prepared.threads {
            let thread = &planned.thread;
            let existing_thread = thread_exists(target_conn, &thread.id)?;
            if existing_thread
                && policy.existing_rollout_path == ExistingRolloutPathPolicy::PreserveExisting
            {
                duplicate_threads += 1;
                continue;
            }
            let existing_rollout =
                existing_thread_rollout_path(target_conn, target_root, &thread.id)?;
            let copied_rollout = copy_rollout_file(
                thread,
                target_root,
                policy.allow_existing_replacement,
                policy.session_files,
                provider_id,
            )?;
            let mut stable_thread = thread.clone();
            stable_thread.meta = copied_rollout.source_meta.clone();
            if existing_thread {
                duplicate_threads += 1;
                let selected_rollout = select_target_rollout_path(
                    existing_rollout.as_deref(),
                    copied_rollout.path.as_str(),
                )?;
                let provider_for_thread = if policy.update_existing_provider
                    || (copied_rollout.copied && copied_rollout.path == selected_rollout)
                {
                    provider_id
                } else {
                    None
                };
                update_existing_thread(
                    target_conn,
                    &thread.id,
                    Some(selected_rollout.as_str()),
                    provider_for_thread,
                )?;
                if copied_rollout.copied {
                    copied_session_files += 1;
                }
                continue;
            }
            insert_thread(
                target_conn,
                &stable_thread,
                copied_rollout.path.as_str(),
                provider_id,
            )?;
            inserted_threads += 1;
            if copied_rollout.copied {
                copied_session_files += 1;
            }
        }
        if let Some(source_conn) = prepared.source_conn.as_ref() {
            copy_dependent_rows(source_conn, target_conn, &prepared.candidate_ids)?;
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
) -> Result<SourceScan, String> {
    let source_rows = if let Some(conn) = source_conn {
        read_source_thread_rows(conn)?
    } else {
        HashMap::new()
    };
    let mut threads = Vec::new();
    let mut skipped_archived = 0;
    for (session_file, meta) in read_session_files(source_root, &source_rows)? {
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
    let candidate_ids = threads
        .iter()
        .map(|thread| thread.id.clone())
        .collect::<HashSet<_>>();
    let skipped_missing_session_files = source_rows
        .iter()
        .filter(|(id, row)| !source_row_is_archived(row) && !candidate_ids.contains(id.as_str()))
        .count();
    Ok(SourceScan {
        threads,
        candidate_ids,
        skipped_archived_threads: skipped_archived,
        skipped_missing_session_files,
    })
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

fn read_session_files(
    source_root: &SyncRoot,
    source_rows: &HashMap<String, SourceRow>,
) -> Result<Vec<(PathBuf, SessionMeta)>, String> {
    if !source_root.sessions_dir.exists() {
        return Ok(Vec::new());
    }
    let files = walk_jsonl_files(&source_root.sessions_dir)?;

    let mut grouped = HashMap::<String, Vec<(PathBuf, SessionMeta)>>::new();
    for path in files {
        let Some(meta) = session_file_meta(&path)? else {
            continue;
        };
        grouped
            .entry(meta.id.clone())
            .or_default()
            .push((path, meta));
    }

    let mut output = Vec::new();
    for (id, mut candidates) in grouped {
        let preferred_relative = source_rows
            .get(&id)
            .and_then(|row| row.values_by_column.get("rollout_path"))
            .and_then(|value| match value {
                Value::Text(path) => relative_from_sessions(Path::new(path)),
                _ => None,
            });
        let mut selected = preferred_relative
            .as_ref()
            .and_then(|preferred| {
                candidates
                    .iter()
                    .position(|(path, _)| relative_from_sessions(path).as_ref() == Some(preferred))
            })
            .unwrap_or_else(|| {
                candidates
                    .iter()
                    .enumerate()
                    .max_by_key(|(_, (path, _))| {
                        let metadata = fs::metadata(path).ok();
                        (
                            metadata.as_ref().map(|value| value.len()).unwrap_or(0),
                            metadata
                                .and_then(|value| value.modified().ok())
                                .and_then(system_time_millis)
                                .unwrap_or(0),
                        )
                    })
                    .map(|(index, _)| index)
                    .unwrap_or(0)
            });
        loop {
            let mut extensions = Vec::new();
            for (index, (path, _)) in candidates.iter().enumerate() {
                if index != selected
                    && session_file_relation(path, &candidates[selected].0)?
                        == SessionFileRelation::LeftExtendsRight
                {
                    extensions.push(index);
                }
            }
            let Some(mut next) = extensions.first().copied() else {
                break;
            };
            let mut ambiguous = false;
            for index in extensions.into_iter().skip(1) {
                match session_file_relation(&candidates[index].0, &candidates[next].0)? {
                    SessionFileRelation::LeftExtendsRight => next = index,
                    SessionFileRelation::RightExtendsLeft | SessionFileRelation::Equal => {}
                    SessionFileRelation::Divergent => {
                        ambiguous = true;
                        break;
                    }
                }
            }
            if ambiguous {
                break;
            }
            selected = next;
        }
        output.push(candidates.swap_remove(selected));
    }
    output.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(output)
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

fn validate_target_threads_schema(conn: &Connection) -> Result<(), String> {
    if !table_exists(conn, "threads")? {
        return Err("threads table is missing".to_string());
    }
    let schema = table_schema(conn, "threads")?;
    for required in ["id", "rollout_path"] {
        if !schema.iter().any(|column| column.name == required) {
            return Err(format!("threads.{required} is missing"));
        }
    }
    for column in schema {
        if column.not_null && column.default_value.is_none() && !known_target_column(&column.name) {
            return Err(format!(
                "unsupported threads schema: required column {} has no known value or default",
                column.name
            ));
        }
    }
    Ok(())
}

fn known_target_column(name: &str) -> bool {
    matches!(
        name,
        "id" | "rollout_path"
            | "model_provider"
            | "created_at"
            | "updated_at"
            | "recency_at"
            | "created_at_ms"
            | "updated_at_ms"
            | "recency_at_ms"
            | "source"
            | "cwd"
            | "cli_version"
            | "title"
            | "preview"
            | "first_user_message"
            | "sandbox_policy"
            | "approval_mode"
            | "tokens_used"
            | "has_user_event"
            | "archived"
            | "memory_mode"
            | "thread_source"
            | "agent_nickname"
            | "agent_role"
            | "agent_path"
            | "model"
            | "reasoning_effort"
            | "archived_at"
            | "git_sha"
            | "git_branch"
            | "git_origin_url"
    )
}

fn thread_exists(conn: &Connection, id: &str) -> Result<bool, String> {
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM threads WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .map_err(|error| format!("failed to check target thread: {error}"))?;
    Ok(count > 0)
}

fn existing_thread_rollout_path(
    conn: &Connection,
    target_root: &SyncRoot,
    id: &str,
) -> Result<Option<String>, String> {
    if !thread_exists(conn, id)? {
        return Ok(None);
    }
    let columns = table_columns(conn, "threads")?;
    if !columns.iter().any(|column| column == "rollout_path") {
        return Ok(None);
    }
    let stored = conn
        .query_row(
            "SELECT rollout_path FROM threads WHERE id = ?1",
            [id],
            |row| row.get::<_, Option<String>>(0),
        )
        .map_err(|error| format!("failed to read target thread rollout path: {error}"))?;
    let Some(stored) = stored else {
        return Ok(None);
    };
    let stored_path = PathBuf::from(&stored);
    let candidate = if stored_path.is_absolute() {
        stored_path
    } else {
        target_root.root.join(stored_path)
    };
    let Ok(relative) = candidate.strip_prefix(&target_root.sessions_dir) else {
        return Ok(None);
    };
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Ok(None);
    }
    if !target_root.sessions_dir.is_dir() || !candidate.is_file() {
        return Ok(None);
    }
    let canonical_candidate = fs::canonicalize(&candidate)
        .map_err(|error| format!("failed to resolve target thread rollout path: {error}"))?;
    let canonical_sessions = fs::canonicalize(&target_root.sessions_dir)
        .map_err(|error| format!("failed to resolve target sessions directory: {error}"))?;
    if !canonical_candidate.starts_with(&canonical_sessions) {
        return Ok(None);
    }
    if session_file_meta(&canonical_candidate)?.is_none_or(|meta| meta.id != id) {
        return Ok(None);
    }
    Ok(Some(candidate.to_string_lossy().to_string()))
}

fn select_target_rollout_path(
    existing_rollout: Option<&str>,
    copied_rollout: &str,
) -> Result<String, String> {
    let Some(existing_rollout) = existing_rollout else {
        return Ok(copied_rollout.to_string());
    };
    let existing = Path::new(existing_rollout);
    let copied = Path::new(copied_rollout);
    if !existing.exists() {
        return Ok(copied_rollout.to_string());
    }
    if !copied.exists() || existing == copied {
        return Ok(existing_rollout.to_string());
    }
    match session_file_relation(copied, existing)? {
        SessionFileRelation::LeftExtendsRight => Ok(copied_rollout.to_string()),
        SessionFileRelation::Equal
        | SessionFileRelation::RightExtendsLeft
        | SessionFileRelation::Divergent => Ok(existing_rollout.to_string()),
    }
}

fn update_existing_thread(
    conn: &Connection,
    thread_id: &str,
    rollout_path: Option<&str>,
    provider_id: Option<&str>,
) -> Result<usize, String> {
    let columns = table_columns(conn, "threads")?;
    let mut assignments = Vec::new();
    let mut differences = Vec::new();
    let mut values = Vec::new();
    if let Some(path) = rollout_path {
        if columns.iter().any(|column| column == "rollout_path") {
            let parameter = values.len() + 1;
            assignments.push(format!("rollout_path = ?{parameter}"));
            differences.push(format!("rollout_path IS NOT ?{parameter}"));
            values.push(Value::Text(path.to_string()));
        }
    }
    if let Some(provider_id) = provider_id {
        if columns.iter().any(|column| column == "model_provider") {
            let parameter = values.len() + 1;
            assignments.push(format!("model_provider = ?{parameter}"));
            differences.push(format!("model_provider IS NOT ?{parameter}"));
            values.push(Value::Text(provider_id.to_string()));
        }
    }
    if assignments.is_empty() {
        return Ok(0);
    }
    let id_parameter = values.len() + 1;
    values.push(Value::Text(thread_id.to_string()));
    let sql = format!(
        "UPDATE threads SET {} WHERE id = ?{} AND ({})",
        assignments.join(", "),
        id_parameter,
        differences.join(" OR ")
    );
    conn.execute(&sql, rusqlite::params_from_iter(values))
        .map_err(|error| format!("failed to update target thread: {error}"))
}

fn copy_rollout_file(
    thread: &SourceThread,
    target_root: &SyncRoot,
    allow_existing_replacement: bool,
    file_write_policy: SessionFileWritePolicy,
    provider_id: Option<&str>,
) -> Result<RolloutCopy, String> {
    let mut last_source_change = None;
    let publish_extensions =
        allow_existing_replacement || file_write_policy == SessionFileWritePolicy::Deny;
    for attempt in 0..SOURCE_STABILITY_ATTEMPTS {
        let plan = plan_stable_rollout_file(target_root, thread, publish_extensions)?;
        let action = plan.action;
        let source = plan.source;
        if action.writes_session_file() && file_write_policy == SessionFileWritePolicy::Deny {
            return Err(
                "session JSONL changed after fast-path planning; retry the runtime switch"
                    .to_string(),
            );
        }

        let target_path = action.target_path().to_path_buf();
        let result = match &action {
            RolloutFileAction::Unchanged(_) => {
                let rechecked = plan_stable_rollout_file(target_root, thread, publish_extensions)?;
                if rechecked.action == action && rechecked.source.version == source.version {
                    return Ok(rollout_copy(&target_path, false, &rechecked.source));
                }
                last_source_change = Some(source_changed(
                    "the source or target changed before an unchanged plan was finalized",
                ));
                Ok(None)
            }
            RolloutFileAction::Create(_) | RolloutFileAction::Import(_) => {
                match create_session_file_if_absent(
                    &thread.session_file,
                    &target_path,
                    &thread.id,
                    provider_id,
                    &source.version,
                ) {
                    Ok(true) => {
                        return Ok(rollout_copy(&target_path, true, &source));
                    }
                    Err(error) => Err(error),
                    Ok(false) => match stable_source_relation_to_target(&source, &target_path)? {
                        SessionFileRelation::Equal | SessionFileRelation::RightExtendsLeft => {
                            return Ok(rollout_copy(&target_path, false, &source));
                        }
                        SessionFileRelation::LeftExtendsRight => {
                            if publish_extensions && matches!(&action, RolloutFileAction::Create(_))
                            {
                                Ok(None)
                            } else if publish_extensions {
                                Err("target imported session JSONL is shorter than its content hash"
                                    .to_string())
                            } else {
                                return Ok(rollout_copy(&target_path, false, &source));
                            }
                        }
                        SessionFileRelation::Divergent
                            if matches!(&action, RolloutFileAction::Create(_)) =>
                        {
                            Ok(None)
                        }
                        SessionFileRelation::Divergent => {
                            Err("target session JSONL changed during no-clobber publish"
                                .to_string())
                        }
                    },
                }
            }
        };
        match result {
            Ok(Some(copy)) => return Ok(copy),
            Ok(None) => continue,
            Err(error) if is_source_changed(&error) => {
                last_source_change = Some(error);
                if attempt + 1 < SOURCE_STABILITY_ATTEMPTS {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_source_change.unwrap_or_else(|| {
        "target session JSONL kept changing during no-clobber publish".to_string()
    }))
}

fn rollout_copy(target: &Path, copied: bool, source: &StableSourceData) -> RolloutCopy {
    RolloutCopy {
        path: target.to_string_lossy().to_string(),
        copied,
        source_meta: source.version.meta.clone(),
    }
}

fn create_session_file_if_absent(
    source: &Path,
    target: &Path,
    expected_id: &str,
    provider_id: Option<&str>,
    expected_version: &StableSourceVersion,
) -> Result<bool, String> {
    atomic_create(target, |output| {
        write_session_file(source, output, expected_id, provider_id, expected_version)
    })
}

fn plan_rollout_file(
    target_root: &SyncRoot,
    thread: &SourceThread,
) -> Result<RolloutFileAction, String> {
    let relative = rollout_relative_path(&thread.session_file);
    let mut target_path = target_root.root.join(relative);
    if !target_path.exists() {
        return Ok(RolloutFileAction::Create(target_path));
    }
    match session_file_relation(&thread.session_file, &target_path)? {
        SessionFileRelation::Equal | SessionFileRelation::RightExtendsLeft => {
            Ok(RolloutFileAction::Unchanged(target_path))
        }
        SessionFileRelation::LeftExtendsRight | SessionFileRelation::Divergent => {
            let source_hash = sha256_file(&thread.session_file)?;
            set_imported_file_name(&mut target_path, &source_hash);
            Ok(RolloutFileAction::Import(target_path))
        }
    }
}

fn plan_stable_rollout_file(
    target_root: &SyncRoot,
    thread: &SourceThread,
    publish_extensions: bool,
) -> Result<StableRolloutPlan, String> {
    let mut last_change = None;
    for attempt in 0..SOURCE_STABILITY_ATTEMPTS {
        match plan_stable_rollout_file_once(target_root, thread, publish_extensions) {
            Ok(plan) => return Ok(plan),
            Err(error) if is_source_changed(&error) => {
                last_change = Some(error);
                if attempt + 1 < SOURCE_STABILITY_ATTEMPTS {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_change
        .unwrap_or_else(|| source_changed("the file did not become stable within the retry limit")))
}

fn plan_stable_rollout_file_once(
    target_root: &SyncRoot,
    thread: &SourceThread,
    publish_extensions: bool,
) -> Result<StableRolloutPlan, String> {
    let source = read_stable_source(&thread.session_file, &thread.id, |_, _, _| Ok(()))?;
    let relative = rollout_relative_path(&thread.session_file);
    let mut target_path = target_root.root.join(relative);
    let action = if !target_path.exists() {
        RolloutFileAction::Create(target_path)
    } else {
        match stable_source_relation_to_target(&source, &target_path)? {
            SessionFileRelation::Equal | SessionFileRelation::RightExtendsLeft => {
                RolloutFileAction::Unchanged(target_path)
            }
            SessionFileRelation::LeftExtendsRight if !publish_extensions => {
                RolloutFileAction::Unchanged(target_path)
            }
            SessionFileRelation::LeftExtendsRight => {
                set_imported_file_name(&mut target_path, &source.version.sha256);
                RolloutFileAction::Import(target_path)
            }
            SessionFileRelation::Divergent => {
                set_imported_file_name(&mut target_path, &source.version.sha256);
                RolloutFileAction::Import(target_path)
            }
        }
    };
    Ok(StableRolloutPlan { action, source })
}

fn set_imported_file_name(target_path: &mut PathBuf, source_hash: &[u8]) {
    let stem = target_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("rollout")
        .to_string();
    let stem = imported_base_stem(&stem);
    let hash_suffix = source_hash
        .iter()
        .take(6)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    target_path.set_file_name(format!("{stem}-imported-{hash_suffix}.jsonl"));
}

fn stable_source_relation_to_target(
    source: &StableSourceData,
    target: &Path,
) -> Result<SessionFileRelation, String> {
    let target = fs::File::open(target)
        .map_err(|error| format!("failed to open target session candidate: {error}"))?;
    stable_source_relation_to_reader(source, &mut BufReader::new(target))
        .map(|(relation, _)| relation)
}

fn stable_source_relation_to_reader(
    source: &StableSourceData,
    target: &mut impl BufRead,
) -> Result<(SessionFileRelation, usize), String> {
    let mut target_lines = 0_usize;
    loop {
        let Some(target_line) = read_normalized_session_line(target)? else {
            let relation = if target_lines == source.normalized_line_hashes.len() {
                SessionFileRelation::Equal
            } else {
                SessionFileRelation::LeftExtendsRight
            };
            return Ok((relation, target_lines));
        };
        if target_lines >= source.normalized_line_hashes.len() {
            return Ok((SessionFileRelation::RightExtendsLeft, target_lines + 1));
        }
        if sha256_bytes(&target_line) != source.normalized_line_hashes[target_lines] {
            return Ok((SessionFileRelation::Divergent, target_lines + 1));
        }
        target_lines += 1;
    }
}

fn read_stable_source<F>(
    source_path: &Path,
    expected_id: &str,
    mut on_line: F,
) -> Result<StableSourceData, String>
where
    F: FnMut(&[u8], &[u8], &JsonValue) -> Result<(), String>,
{
    let source = fs::File::open(source_path)
        .map_err(|error| source_changed(format!("failed to open the planned file: {error}")))?;
    let before_handle = source
        .metadata()
        .map_err(|error| source_changed(format!("failed to inspect the planned file: {error}")))?;
    let before_path = fs::metadata(source_path)
        .map_err(|error| source_changed(format!("failed to inspect the planned path: {error}")))?;
    if !before_handle.is_file() || file_stamp(&before_handle) != file_stamp(&before_path) {
        return Err(source_changed(
            "the planned path changed before it was read",
        ));
    }
    let before_stamp = file_stamp(&before_handle);
    let mut source = BufReader::new(source);

    let mut raw_hash = Sha256::new();
    let mut normalized_line_hashes = Vec::new();
    let mut meta = None;
    let mut total_length = 0_u64;
    let mut raw = Vec::new();
    loop {
        raw.clear();
        let read = source
            .read_until(b'\n', &mut raw)
            .map_err(|error| source_changed(format!("failed while reading the file: {error}")))?;
        if read == 0 {
            break;
        }
        total_length = total_length
            .checked_add(read as u64)
            .ok_or_else(|| source_changed("the file length overflowed"))?;
        raw_hash.update(&raw);
        let body = trim_jsonl_ending(&raw);
        if body.is_empty() {
            return Err(source_changed("the file contains an empty JSONL entry"));
        }
        let value = serde_json::from_slice::<JsonValue>(body)
            .map_err(|_| source_changed("the file has an incomplete or invalid JSONL tail"))?;
        if value.get("type").and_then(JsonValue::as_str) == Some("session_meta") {
            let parsed = session_meta_from_value(&value).ok_or_else(|| {
                source_changed("the file contains session_meta without a valid id")
            })?;
            if parsed.id != expected_id {
                return Err(format!(
                    "source session JSONL id changed from {expected_id} to {}",
                    parsed.id
                ));
            }
            if meta
                .as_ref()
                .is_some_and(|existing: &SessionMeta| existing.id != parsed.id)
            {
                return Err("source session JSONL contains conflicting session ids".to_string());
            }
            if meta.is_none() {
                meta = Some(parsed);
            }
        }
        normalized_line_hashes.push(sha256_bytes(&normalized_session_line(&value, body)?));
        on_line(&raw, body, &value)?;
    }

    let after_handle = source
        .get_ref()
        .metadata()
        .map_err(|error| source_changed(format!("failed to recheck the planned file: {error}")))?;
    let after_path = fs::metadata(source_path)
        .map_err(|error| source_changed(format!("failed to recheck the planned path: {error}")))?;
    if file_stamp(&after_handle) != before_stamp
        || file_stamp(&after_path) != before_stamp
        || total_length != before_stamp.length
    {
        return Err(source_changed("the file changed while it was being read"));
    }
    let meta = meta
        .ok_or_else(|| source_changed("the file does not contain a complete session_meta entry"))?;
    Ok(StableSourceData {
        version: StableSourceVersion {
            length: total_length,
            sha256: raw_hash.finalize().to_vec(),
            meta,
        },
        normalized_line_hashes,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileStamp {
    length: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
}

fn file_stamp(metadata: &fs::Metadata) -> FileStamp {
    FileStamp {
        length: metadata.len(),
        modified: metadata.modified().ok(),
        created: metadata.created().ok(),
    }
}

fn trim_jsonl_ending(mut line: &[u8]) -> &[u8] {
    while line
        .last()
        .is_some_and(|byte| matches!(byte, b'\r' | b'\n'))
    {
        line = &line[..line.len() - 1];
    }
    line
}

fn normalized_session_line(value: &JsonValue, original: &[u8]) -> Result<Vec<u8>, String> {
    if value.get("type").and_then(JsonValue::as_str) != Some("session_meta") {
        return Ok(original.to_vec());
    }
    let mut value = value.clone();
    if let Some(payload) = value.get_mut("payload").and_then(JsonValue::as_object_mut) {
        payload.remove("model_provider");
    }
    serde_json::to_vec(&value)
        .map_err(|error| format!("failed to normalize session metadata: {error}"))
}

fn sha256_bytes(bytes: &[u8]) -> Vec<u8> {
    Sha256::digest(bytes).to_vec()
}

fn source_changed(detail: impl AsRef<str>) -> String {
    format!("{SOURCE_CHANGED_PREFIX}: {}", detail.as_ref())
}

fn is_source_changed(error: &str) -> bool {
    error.starts_with(SOURCE_CHANGED_PREFIX)
}

fn imported_base_stem(stem: &str) -> &str {
    let mut base = stem;
    loop {
        if let Some(stripped) = base.strip_suffix("-imported") {
            base = stripped;
            continue;
        }
        let Some((candidate, suffix)) = base.rsplit_once("-imported-") else {
            break;
        };
        if suffix.len() == 12 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            base = candidate;
            continue;
        }
        break;
    }
    base
}

fn session_file_relation(left: &Path, right: &Path) -> Result<SessionFileRelation, String> {
    let left = fs::File::open(left)
        .map_err(|error| format!("failed to open left session candidate: {error}"))?;
    let right = fs::File::open(right)
        .map_err(|error| format!("failed to open right session candidate: {error}"))?;
    let mut left = BufReader::new(left);
    let mut right = BufReader::new(right);
    loop {
        let left_line = read_normalized_session_line(&mut left)?;
        let right_line = read_normalized_session_line(&mut right)?;
        match (left_line, right_line) {
            (None, None) => return Ok(SessionFileRelation::Equal),
            (Some(_), None) => return Ok(SessionFileRelation::LeftExtendsRight),
            (None, Some(_)) => return Ok(SessionFileRelation::RightExtendsLeft),
            (Some(left_line), Some(right_line)) if left_line != right_line => {
                return Ok(SessionFileRelation::Divergent);
            }
            _ => {}
        }
    }
}

fn read_normalized_session_line(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>, String> {
    let mut line = Vec::new();
    let read = reader
        .read_until(b'\n', &mut line)
        .map_err(|error| format!("failed to read session candidate: {error}"))?;
    if read == 0 {
        return Ok(None);
    }
    while line
        .last()
        .is_some_and(|byte| matches!(byte, b'\r' | b'\n'))
    {
        line.pop();
    }
    let Ok(mut value) = serde_json::from_slice::<JsonValue>(&line) else {
        return Ok(Some(line));
    };
    if value.get("type").and_then(JsonValue::as_str) == Some("session_meta") {
        if let Some(payload) = value.get_mut("payload").and_then(JsonValue::as_object_mut) {
            payload.remove("model_provider");
        }
        return serde_json::to_vec(&value)
            .map(Some)
            .map_err(|error| format!("failed to normalize session metadata: {error}"));
    }
    Ok(Some(line))
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
        if let Some(meta) = session_meta_from_value(&value) {
            return Ok(Some(meta));
        }
    }
    Ok(None)
}

fn session_meta_from_value(value: &JsonValue) -> Option<SessionMeta> {
    if value.get("type").and_then(JsonValue::as_str) != Some("session_meta") {
        return None;
    }
    let payload = value.get("payload")?;
    let id = payload.get("id").and_then(JsonValue::as_str)?;
    Some(SessionMeta {
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
    })
}

fn write_session_file(
    source_path: &Path,
    output: &mut fs::File,
    expected_id: &str,
    provider_id: Option<&str>,
    expected_version: &StableSourceVersion,
) -> Result<(), String> {
    let observed = read_stable_source(source_path, expected_id, |raw, body, value| {
        let Some(provider_id) = provider_id else {
            return output
                .write_all(raw)
                .map_err(|error| format!("failed to copy session JSONL: {error}"));
        };
        if value.get("type").and_then(JsonValue::as_str) != Some("session_meta") {
            return output
                .write_all(raw)
                .map_err(|error| format!("failed to copy session JSONL: {error}"));
        }
        let mut value = value.clone();
        let payload = value
            .get_mut("payload")
            .and_then(JsonValue::as_object_mut)
            .ok_or_else(|| "session_meta payload must be an object".to_string())?;
        payload.insert(
            "model_provider".to_string(),
            JsonValue::String(provider_id.to_string()),
        );
        let rewritten = serde_json::to_vec(&value)
            .map_err(|error| format!("failed to serialize session metadata: {error}"))?;
        output
            .write_all(&rewritten)
            .map_err(|error| format!("failed to write session JSONL rewrite: {error}"))?;
        output
            .write_all(&raw[body.len()..])
            .map_err(|error| format!("failed to write session JSONL ending: {error}"))
    })?;
    if observed.version != *expected_version {
        return Err(source_changed(
            "the copied version no longer matches the planned version",
        ));
    }
    Ok(())
}

#[cfg(test)]
fn merge_session_index(
    source_root: &SyncRoot,
    target_root: &SyncRoot,
    allowlist: &HashSet<String>,
) -> Result<usize, String> {
    merge_session_index_with_policy(
        source_root,
        target_root,
        allowlist,
        SessionIndexWritePolicy::MergeAtomic,
    )
}

fn merge_session_index_with_policy(
    source_root: &SyncRoot,
    target_root: &SyncRoot,
    allowlist: &HashSet<String>,
    write_policy: SessionIndexWritePolicy,
) -> Result<usize, String> {
    if write_policy == SessionIndexWritePolicy::Skip {
        return Ok(0);
    }
    let source_exclusive = write_policy == SessionIndexWritePolicy::Deny;
    let Some(mut source) =
        open_existing_session_index(&source_root.session_index, source_exclusive, "source")?
    else {
        return Ok(0);
    };
    let source_lines = read_session_index_source_lines(&mut source, allowlist)?;

    if write_policy == SessionIndexWritePolicy::Deny {
        if source_lines.is_empty() {
            return Ok(0);
        }
        let Some(mut target) =
            open_existing_session_index(&target_root.session_index, true, "target")?
        else {
            return Err(
                "session index changed after fast-path planning; retry the runtime switch"
                    .to_string(),
            );
        };
        let plan = plan_session_index_merge_from_lines(&source_lines, Some(&mut target))?;
        if plan.lines.is_empty() {
            return Ok(0);
        }
        return Err(
            "session index changed after fast-path planning; retry the runtime switch".to_string(),
        );
    }

    if source_lines.is_empty() {
        return Ok(0);
    }
    let target_bytes = read_session_index_target_bytes(&target_root.session_index, false)?;
    let plan = plan_session_index_merge_from_bytes(&source_lines, &target_bytes)?;
    if plan.lines.is_empty() {
        return Ok(0);
    }
    let mut encoded = target_bytes;
    if plan.target_needs_newline {
        encoded.push(b'\n');
    }
    for line in &plan.lines {
        encoded.extend_from_slice(line.as_bytes());
        encoded.push(b'\n');
    }
    atomic_write(&target_root.session_index, &encoded)
        .map_err(|error| format!("failed to atomically replace session_index.jsonl: {error}"))?;
    Ok(plan.lines.len())
}

fn plan_session_index_merge(
    source_root: &SyncRoot,
    target_root: &SyncRoot,
    allowlist: &HashSet<String>,
) -> Result<SessionIndexMergePlan, String> {
    let Some(mut source) =
        open_existing_session_index(&source_root.session_index, false, "source")?
    else {
        return Ok(SessionIndexMergePlan {
            lines: Vec::new(),
            target_needs_newline: false,
        });
    };
    let source_lines = read_session_index_source_lines(&mut source, allowlist)?;
    let mut target = open_existing_session_index(&target_root.session_index, false, "target")?;
    plan_session_index_merge_from_lines(&source_lines, target.as_mut())
}

fn open_existing_session_index(
    path: &Path,
    exclusive: bool,
    role: &str,
) -> Result<Option<fs::File>, String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    if exclusive {
        options.share_mode(0);
    }
    match options.open(path) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            let action = if exclusive { "lock" } else { "open" };
            Err(format!(
                "failed to {action} {role} session_index.jsonl: {error}"
            ))
        }
    }
}

fn read_session_index_source_lines(
    source: &mut fs::File,
    allowlist: &HashSet<String>,
) -> Result<Vec<String>, String> {
    source
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("failed to rewind source session_index.jsonl: {error}"))?;
    let mut lines = Vec::new();
    for line in BufReader::new(source).lines() {
        let line =
            line.map_err(|error| format!("failed to read source session_index.jsonl: {error}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let Some(id) = session_index_line_id(&line)? else {
            continue;
        };
        if allowlist.contains(&id) {
            lines.push(line);
        }
    }
    Ok(lines)
}

fn plan_session_index_merge_from_lines(
    source_lines: &[String],
    target: Option<&mut fs::File>,
) -> Result<SessionIndexMergePlan, String> {
    let mut target_bytes = Vec::new();
    if let Some(target) = target {
        target
            .seek(SeekFrom::Start(0))
            .map_err(|error| format!("failed to rewind target session_index.jsonl: {error}"))?;
        target
            .read_to_end(&mut target_bytes)
            .map_err(|error| format!("failed to read target session_index.jsonl: {error}"))?;
    }
    plan_session_index_merge_from_bytes(source_lines, &target_bytes)
}

fn plan_session_index_merge_from_bytes(
    source_lines: &[String],
    target_bytes: &[u8],
) -> Result<SessionIndexMergePlan, String> {
    let mut seen = HashSet::new();
    for raw_line in target_bytes.split(|byte| *byte == b'\n') {
        let raw_line = raw_line.strip_suffix(b"\r").unwrap_or(raw_line);
        if raw_line.is_empty() {
            continue;
        }
        let line = std::str::from_utf8(raw_line).map_err(|_| {
            "target session_index.jsonl contains an incomplete or invalid entry".to_string()
        })?;
        if serde_json::from_str::<JsonValue>(line).is_err() {
            return Err(
                "target session_index.jsonl contains an incomplete or invalid entry".to_string(),
            );
        }
        seen.insert(line.to_string());
    }
    let mut missing_lines = Vec::new();
    for line in source_lines {
        if seen.insert(line.clone()) {
            missing_lines.push(line.clone());
        }
    }
    Ok(SessionIndexMergePlan {
        lines: missing_lines,
        target_needs_newline: !target_bytes.is_empty() && !target_bytes.ends_with(b"\n"),
    })
}

fn read_session_index_target_bytes(path: &Path, exclusive: bool) -> Result<Vec<u8>, String> {
    let Some(mut target) = open_existing_session_index(path, exclusive, "target")? else {
        return Ok(Vec::new());
    };
    let mut bytes = Vec::new();
    target
        .read_to_end(&mut bytes)
        .map_err(|error| format!("failed to read target session_index.jsonl: {error}"))?;
    Ok(bytes)
}

fn session_index_line_id(line: &str) -> Result<Option<String>, String> {
    let value = serde_json::from_str::<JsonValue>(line)
        .map_err(|_| "source session_index.jsonl contains an invalid entry".to_string())?;
    Ok(value
        .get("id")
        .or_else(|| value.get("session_id"))
        .or_else(|| value.get("sessionId"))
        .and_then(JsonValue::as_str)
        .map(ToOwned::to_owned))
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
    use std::{fs, io::Write};

    use rusqlite::Connection;
    use tempfile::tempdir;

    use super::{
        copy_rollout_file, merge_session_index, merge_session_index_with_policy, open_source_conn,
        plan_session_index_merge, read_source_threads, read_stable_source, root_from_paths,
        sha256_file, sync_sessions, sync_sessions_for_provider, sync_shared_to_user_home,
        sync_shared_to_user_home_hot, sync_shared_to_user_home_hot_with_paths,
        sync_shared_to_user_home_hot_with_policy, sync_shared_to_user_home_with_paths,
        sync_user_home_to_shared, sync_user_home_to_shared_with_paths, SessionFileWritePolicy,
        SessionIndexWritePolicy,
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
    fn session_index_atomic_merge_repairs_a_missing_trailing_newline() {
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

        let merged = merge_session_index(
            &source_root,
            &target_root,
            &std::collections::HashSet::from(["thread-b".to_string()]),
        )
        .unwrap();

        assert_eq!(merged, 1);
        let index = fs::read_to_string(target.path().join("session_index.jsonl")).unwrap();
        assert_eq!(index.lines().count(), 2);
        assert!(index.contains("thread-a\"}\n{\"id\":\"thread-b"));
    }

    #[test]
    fn session_index_merge_publishes_a_complete_replacement() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let source_index = source.path().join("session_index.jsonl");
        let target_index = target.path().join("session_index.jsonl");
        let original = b"{\"id\":\"thread-a\"}\n";
        fs::write(&source_index, "{\"id\":\"thread-b\"}\n").unwrap();
        fs::write(&target_index, original).unwrap();
        let source_root = root_from_paths(crate::codex_paths::local_codex_paths(source.path()));
        let target_root = root_from_paths(crate::codex_paths::local_codex_paths(target.path()));

        let merged = merge_session_index(
            &source_root,
            &target_root,
            &std::collections::HashSet::from(["thread-b".to_string()]),
        )
        .unwrap();

        assert_eq!(merged, 1);
        let published = fs::read_to_string(&target_index).unwrap();
        assert_eq!(published, "{\"id\":\"thread-a\"}\n{\"id\":\"thread-b\"}\n");
        assert_eq!(
            fs::read_dir(target.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().contains("codex-switch"))
                .count(),
            0
        );
    }

    #[test]
    fn denied_session_index_write_is_rechecked_after_planning() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        fs::write(
            source.path().join("session_index.jsonl"),
            "{\"id\":\"thread-b\"}\n",
        )
        .unwrap();
        fs::write(
            target.path().join("session_index.jsonl"),
            "{\"id\":\"thread-b\"}\n",
        )
        .unwrap();
        let source_root = root_from_paths(crate::codex_paths::local_codex_paths(source.path()));
        let target_root = root_from_paths(crate::codex_paths::local_codex_paths(target.path()));
        let allowlist = std::collections::HashSet::from(["thread-b".to_string()]);
        let initial_plan =
            plan_session_index_merge(&source_root, &target_root, &allowlist).unwrap();
        assert!(initial_plan.lines.is_empty());

        let drifted = b"{\"id\":\"thread-a\"}\n";
        fs::write(target.path().join("session_index.jsonl"), drifted).unwrap();
        let error = merge_session_index_with_policy(
            &source_root,
            &target_root,
            &allowlist,
            SessionIndexWritePolicy::Deny,
        )
        .unwrap_err();

        assert!(error.contains("changed after fast-path planning"));
        assert_eq!(
            fs::read(target.path().join("session_index.jsonl")).unwrap(),
            drifted
        );
    }

    #[test]
    fn session_index_atomic_merge_preserves_an_incomplete_target() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        fs::write(
            source.path().join("session_index.jsonl"),
            "{\"id\":\"thread-b\"}\n",
        )
        .unwrap();
        let target_index = target.path().join("session_index.jsonl");
        let incomplete = b"{\"id\":\"thread-a\"";
        fs::write(&target_index, incomplete).unwrap();
        let source_root = root_from_paths(crate::codex_paths::local_codex_paths(source.path()));
        let target_root = root_from_paths(crate::codex_paths::local_codex_paths(target.path()));

        let error = merge_session_index(
            &source_root,
            &target_root,
            &std::collections::HashSet::from(["thread-b".to_string()]),
        )
        .unwrap_err();

        assert!(error.contains("incomplete or invalid entry"));
        assert_eq!(fs::read(&target_index).unwrap(), incomplete);
    }

    #[cfg(windows)]
    #[test]
    fn session_index_atomic_replace_fails_closed_while_target_is_open() {
        use std::{fs::OpenOptions, os::windows::fs::OpenOptionsExt};

        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        fs::write(
            source.path().join("session_index.jsonl"),
            "{\"id\":\"thread-b\"}\n",
        )
        .unwrap();
        let target_index = target.path().join("session_index.jsonl");
        let original = b"{\"id\":\"thread-a\"}\n";
        fs::write(&target_index, original).unwrap();
        let held_target = OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0x7)
            .open(&target_index)
            .unwrap();
        let source_root = root_from_paths(crate::codex_paths::local_codex_paths(source.path()));
        let target_root = root_from_paths(crate::codex_paths::local_codex_paths(target.path()));

        let error = merge_session_index(
            &source_root,
            &target_root,
            &std::collections::HashSet::from(["thread-b".to_string()]),
        )
        .unwrap_err();

        assert!(error.contains("failed to atomically replace session_index.jsonl"));
        assert_eq!(fs::read(&target_index).unwrap(), original);
        drop(held_target);
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
    fn frozen_user_paths_ignore_config_repoint_for_all_sync_directions() {
        let home = tempdir().unwrap();
        let sqlite_a = tempdir().unwrap();
        let sqlite_b = tempdir().unwrap();
        let shared = tempdir().unwrap();
        fs::write(
            home.path().join("config.toml"),
            format!("sqlite_home = \"{}\"\n", sqlite_a.path().display()).replace('\\', "\\\\"),
        )
        .unwrap();
        let thread_a = home
            .path()
            .join("sessions/2026/07/26/rollout-thread-a.jsonl");
        fs::create_dir_all(thread_a.parent().unwrap()).unwrap();
        fs::write(
            &thread_a,
            r#"{"type":"session_meta","payload":{"id":"thread-a","model_provider":"openai"}}"#,
        )
        .unwrap();
        create_official_like_db(
            &sqlite_a.path().join("state_5.sqlite"),
            &[("thread-a", thread_a.to_str().unwrap())],
        );
        create_official_like_db(&sqlite_b.path().join("state_5.sqlite"), &[]);
        create_official_like_db(&shared.path().join("state_5.sqlite"), &[]);
        let current_paths = crate::codex_paths::resolve_user_codex_paths(home.path()).unwrap();
        let shared_paths = crate::codex_paths::local_codex_paths(shared.path());

        fs::write(
            home.path().join("config.toml"),
            format!("sqlite_home = \"{}\"\n", sqlite_b.path().display()).replace('\\', "\\\\"),
        )
        .unwrap();

        let to_shared = sync_user_home_to_shared_with_paths(&current_paths, &shared_paths).unwrap();
        let from_shared =
            sync_shared_to_user_home_with_paths(&shared_paths, &current_paths, "relay").unwrap();
        assert_eq!(to_shared.inserted_threads, 1);
        assert_eq!(from_shared.duplicate_threads, 1);

        let thread_b = shared
            .path()
            .join("sessions/2026/07/26/rollout-thread-b.jsonl");
        fs::create_dir_all(thread_b.parent().unwrap()).unwrap();
        fs::write(
            &thread_b,
            r#"{"type":"session_meta","payload":{"id":"thread-b","model_provider":"relay"}}"#,
        )
        .unwrap();
        let shared_conn = Connection::open(shared.path().join("state_5.sqlite")).unwrap();
        shared_conn
            .execute(
                "INSERT INTO threads (
                    id, rollout_path, created_at, updated_at, source, model_provider, cwd, title,
                    sandbox_policy, approval_mode, created_at_ms, updated_at_ms, recency_at,
                    recency_at_ms
                ) VALUES (
                    ?1, ?2, 1, 1, 'cli', 'relay', '', '', '', '', 1000, 1000, 1, 1000
                )",
                ("thread-b", thread_b.to_str().unwrap()),
            )
            .unwrap();
        drop(shared_conn);

        let hot = sync_shared_to_user_home_hot_with_paths(&shared_paths, &current_paths, "relay")
            .unwrap();
        assert_eq!(hot.inserted_threads, 1);

        let conn_a = Connection::open(sqlite_a.path().join("state_5.sqlite")).unwrap();
        let (count_a, provider_a): (i64, String) = conn_a
            .query_row(
                "SELECT COUNT(*), MAX(model_provider) FROM threads",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(count_a, 2);
        assert_eq!(provider_a, "relay");

        let conn_b = Connection::open(sqlite_b.path().join("state_5.sqlite")).unwrap();
        let count_b: i64 = conn_b
            .query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count_b, 0);
    }

    #[test]
    fn source_append_after_scan_is_replanned_from_a_stable_version() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let relative = "sessions/2026/07/26/rollout-thread-source-append.jsonl";
        let source_jsonl = source.path().join(relative);
        let target_jsonl = target.path().join(relative);
        fs::create_dir_all(source_jsonl.parent().unwrap()).unwrap();
        fs::create_dir_all(target_jsonl.parent().unwrap()).unwrap();
        let base = concat!(
            r#"{"type":"session_meta","payload":{"id":"thread-source-append"}}"#,
            "\n",
        );
        fs::write(&source_jsonl, base).unwrap();
        fs::write(&target_jsonl, base).unwrap();
        let target_hash = sha256_file(&target_jsonl).unwrap();
        let target_modified = fs::metadata(&target_jsonl).unwrap().modified().unwrap();
        create_db(
            &source.path().join("state_5.sqlite"),
            &[("thread-source-append", source_jsonl.to_str().unwrap())],
        );
        let source_root = root_from_paths(crate::codex_paths::local_codex_paths(source.path()));
        let target_root = root_from_paths(crate::codex_paths::local_codex_paths(target.path()));
        let source_conn = open_source_conn(&source_root).unwrap();
        let scan = read_source_threads(&source_root, source_conn.as_ref()).unwrap();
        assert_eq!(scan.threads.len(), 1);

        let tail = r#"{"type":"response_item","payload":{"text":"after scan"}}"#;
        let mut source_file = fs::OpenOptions::new()
            .append(true)
            .open(&source_jsonl)
            .unwrap();
        writeln!(source_file, "{tail}").unwrap();
        source_file.sync_data().unwrap();

        let copied = copy_rollout_file(
            &scan.threads[0],
            &target_root,
            true,
            SessionFileWritePolicy::Allow,
            None,
        )
        .unwrap();

        assert!(copied.copied);
        assert_ne!(std::path::Path::new(&copied.path), target_jsonl);
        assert_eq!(fs::read(&target_jsonl).unwrap(), base.as_bytes());
        assert_eq!(sha256_file(&target_jsonl).unwrap(), target_hash);
        assert_eq!(
            fs::metadata(&target_jsonl).unwrap().modified().unwrap(),
            target_modified
        );
        assert!(fs::read_to_string(&copied.path)
            .unwrap()
            .contains("after scan"));
    }

    #[test]
    fn source_replacement_after_scan_uses_the_new_complete_version() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let relative = "sessions/2026/07/26/rollout-thread-source-replace.jsonl";
        let source_jsonl = source.path().join(relative);
        let target_jsonl = target.path().join(relative);
        fs::create_dir_all(source_jsonl.parent().unwrap()).unwrap();
        fs::create_dir_all(target_jsonl.parent().unwrap()).unwrap();
        let original = concat!(
            r#"{"type":"session_meta","payload":{"id":"thread-source-replace"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"text":"original"}}"#,
            "\n",
        );
        fs::write(&source_jsonl, original).unwrap();
        fs::write(&target_jsonl, original).unwrap();
        create_db(
            &source.path().join("state_5.sqlite"),
            &[("thread-source-replace", source_jsonl.to_str().unwrap())],
        );
        let source_root = root_from_paths(crate::codex_paths::local_codex_paths(source.path()));
        let target_root = root_from_paths(crate::codex_paths::local_codex_paths(target.path()));
        let source_conn = open_source_conn(&source_root).unwrap();
        let scan = read_source_threads(&source_root, source_conn.as_ref()).unwrap();
        assert_eq!(scan.threads.len(), 1);

        fs::write(
            &source_jsonl,
            concat!(
                r#"{"type":"session_meta","payload":{"id":"thread-source-replace"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"text":"replacement"}}"#,
                "\n",
            ),
        )
        .unwrap();

        let copied = copy_rollout_file(
            &scan.threads[0],
            &target_root,
            true,
            SessionFileWritePolicy::Allow,
            None,
        )
        .unwrap();

        assert!(copied.copied);
        assert_ne!(std::path::Path::new(&copied.path), target_jsonl);
        assert!(fs::read_to_string(copied.path)
            .unwrap()
            .contains("replacement"));
        assert!(fs::read_to_string(&target_jsonl)
            .unwrap()
            .contains("original"));
    }

    #[test]
    fn incomplete_source_tail_is_never_published() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let relative = "sessions/2026/07/26/rollout-thread-incomplete-source.jsonl";
        let source_jsonl = source.path().join(relative);
        fs::create_dir_all(source_jsonl.parent().unwrap()).unwrap();
        fs::write(
            &source_jsonl,
            concat!(
                r#"{"type":"session_meta","payload":{"id":"thread-incomplete-source"}}"#,
                "\n",
                r#"{"type":"response_item","payload":"#,
            ),
        )
        .unwrap();
        create_db(
            &source.path().join("state_5.sqlite"),
            &[("thread-incomplete-source", source_jsonl.to_str().unwrap())],
        );
        let source_root = root_from_paths(crate::codex_paths::local_codex_paths(source.path()));
        let target_root = root_from_paths(crate::codex_paths::local_codex_paths(target.path()));
        let source_conn = open_source_conn(&source_root).unwrap();
        let scan = read_source_threads(&source_root, source_conn.as_ref()).unwrap();

        let error = copy_rollout_file(
            &scan.threads[0],
            &target_root,
            true,
            SessionFileWritePolicy::Allow,
            None,
        )
        .unwrap_err();

        assert!(error.contains("incomplete or invalid JSONL tail"));
        assert!(!target.path().join(relative).exists());
    }

    #[test]
    fn source_change_during_read_is_detected_before_publish() {
        let source = tempdir().unwrap();
        let source_jsonl = source.path().join("rollout-source-changing.jsonl");
        fs::write(
            &source_jsonl,
            concat!(
                r#"{"type":"session_meta","payload":{"id":"thread-changing"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"text":"first"}}"#,
                "\n",
            ),
        )
        .unwrap();
        let mut changed = false;

        let error = read_stable_source(&source_jsonl, "thread-changing", |_, _, _| {
            if !changed {
                changed = true;
                let mut file = fs::OpenOptions::new()
                    .append(true)
                    .open(&source_jsonl)
                    .unwrap();
                file.write_all(
                    concat!(
                        r#"{"type":"response_item","payload":{"text":"concurrent"}}"#,
                        "\n",
                    )
                    .as_bytes(),
                )
                .unwrap();
                file.sync_data().unwrap();
            }
            Ok(())
        })
        .unwrap_err();

        assert!(error.starts_with(super::SOURCE_CHANGED_PREFIX));
    }

    #[test]
    fn hot_sync_deny_allows_db_only_when_session_files_are_unchanged() {
        let shared = tempdir().unwrap();
        let current = tempdir().unwrap();
        let relative = "sessions/2026/07/26/rollout-thread-hot-deny.jsonl";
        let shared_jsonl = shared.path().join(relative);
        let current_jsonl = current.path().join(relative);
        fs::create_dir_all(shared_jsonl.parent().unwrap()).unwrap();
        fs::create_dir_all(current_jsonl.parent().unwrap()).unwrap();
        let bytes = concat!(
            r#"{"type":"session_meta","payload":{"id":"thread-hot-deny"}}"#,
            "\n",
        );
        fs::write(&shared_jsonl, bytes).unwrap();
        fs::write(&current_jsonl, bytes).unwrap();
        create_db(
            &shared.path().join("state_5.sqlite"),
            &[("thread-hot-deny", shared_jsonl.to_str().unwrap())],
        );
        create_db(
            &current.path().join("state_5.sqlite"),
            &[("thread-hot-deny", current_jsonl.to_str().unwrap())],
        );
        let shared_paths = crate::codex_paths::local_codex_paths(shared.path());
        let current_paths = crate::codex_paths::local_codex_paths(current.path());

        let result = sync_shared_to_user_home_hot_with_policy(
            &shared_paths,
            &current_paths,
            "openai",
            SessionFileWritePolicy::Deny,
        )
        .unwrap();
        assert_eq!(result.copied_session_files, 0);

        let mut shared_file = fs::OpenOptions::new()
            .append(true)
            .open(&shared_jsonl)
            .unwrap();
        shared_file
            .write_all(
                concat!(
                    r#"{"type":"response_item","payload":{"text":"drift"}}"#,
                    "\n",
                )
                .as_bytes(),
            )
            .unwrap();
        shared_file.sync_data().unwrap();
        let error = sync_shared_to_user_home_hot_with_policy(
            &shared_paths,
            &current_paths,
            "openai",
            SessionFileWritePolicy::Deny,
        )
        .unwrap_err();

        assert!(error.contains("changed after fast-path planning"));
        assert_eq!(fs::read(&current_jsonl).unwrap(), bytes.as_bytes());
    }

    #[test]
    fn hot_sync_skips_the_live_session_index() {
        let shared = tempdir().unwrap();
        let current = tempdir().unwrap();
        let relative = "sessions/2026/07/26/rollout-thread-hot-index.jsonl";
        let shared_jsonl = shared.path().join(relative);
        let current_jsonl = current.path().join(relative);
        fs::create_dir_all(shared_jsonl.parent().unwrap()).unwrap();
        fs::create_dir_all(current_jsonl.parent().unwrap()).unwrap();
        let bytes = concat!(
            r#"{"type":"session_meta","payload":{"id":"thread-hot-index"}}"#,
            "\n",
        );
        fs::write(&shared_jsonl, bytes).unwrap();
        fs::write(&current_jsonl, bytes).unwrap();
        create_db(
            &shared.path().join("state_5.sqlite"),
            &[("thread-hot-index", shared_jsonl.to_str().unwrap())],
        );
        create_db(
            &current.path().join("state_5.sqlite"),
            &[("thread-hot-index", current_jsonl.to_str().unwrap())],
        );
        fs::write(
            shared.path().join("session_index.jsonl"),
            "{\"id\":\"thread-hot-index\",\"thread_name\":\"Shared\"}\n",
        )
        .unwrap();
        let current_index = current.path().join("session_index.jsonl");
        let original_index = b"{\"id\":\"thread-current\",\"thread_name\":\"Current\"}\n";
        fs::write(&current_index, original_index).unwrap();
        let original_modified = fs::metadata(&current_index).unwrap().modified().unwrap();
        let shared_paths = crate::codex_paths::local_codex_paths(shared.path());
        let current_paths = crate::codex_paths::local_codex_paths(current.path());

        let result = sync_shared_to_user_home_hot_with_policy(
            &shared_paths,
            &current_paths,
            "openai",
            SessionFileWritePolicy::Allow,
        )
        .unwrap();

        assert_eq!(result.merged_session_index_entries, 0);
        assert_eq!(fs::read(&current_index).unwrap(), original_index);
        assert_eq!(
            fs::metadata(&current_index).unwrap().modified().unwrap(),
            original_modified
        );
    }

    #[test]
    fn publishes_a_complete_import_without_mutating_a_stale_target() {
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
        let original_hash = sha256_file(&target_jsonl).unwrap();
        let original_modified = fs::metadata(&target_jsonl).unwrap().modified().unwrap();

        let result = sync_sessions(&[source.path().to_path_buf()], target.path()).unwrap();

        assert_eq!(result.copied_session_files, 1);
        assert_eq!(fs::read(&target_jsonl).unwrap(), first.as_bytes());
        assert_eq!(sha256_file(&target_jsonl).unwrap(), original_hash);
        assert_eq!(
            fs::metadata(&target_jsonl).unwrap().modified().unwrap(),
            original_modified
        );
        let conn = Connection::open(target.path().join("state_5.sqlite")).unwrap();
        let rollout_path: String = conn
            .query_row(
                "SELECT rollout_path FROM threads WHERE id = 'thread-growing'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let published = std::path::PathBuf::from(rollout_path);
        assert_ne!(published, target_jsonl);
        assert!(fs::read_to_string(published).unwrap().contains("new tail"));
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

    #[test]
    fn provider_switch_updates_sqlite_without_rewriting_unchanged_existing_jsonl() {
        let shared = tempdir().unwrap();
        let home = tempdir().unwrap();
        let relative = "sessions/2026/07/25/rollout-thread-existing.jsonl";
        let shared_jsonl = shared.path().join(relative);
        let home_jsonl = home.path().join(relative);
        fs::create_dir_all(shared_jsonl.parent().unwrap()).unwrap();
        fs::create_dir_all(home_jsonl.parent().unwrap()).unwrap();
        let existing_bytes = concat!(
            r#"{"type":"session_meta","payload":{"id":"thread-existing","model_provider":"openai"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"text":"unchanged history"}}"#,
            "\n",
        );
        fs::write(&shared_jsonl, existing_bytes).unwrap();
        fs::write(&home_jsonl, existing_bytes).unwrap();
        create_official_like_db(
            &shared.path().join("state_5.sqlite"),
            &[("thread-existing", shared_jsonl.to_str().unwrap())],
        );
        create_official_like_db(
            &home.path().join("state_5.sqlite"),
            &[("thread-existing", home_jsonl.to_str().unwrap())],
        );
        let modified_before = fs::metadata(&home_jsonl).unwrap().modified().unwrap();

        let result = sync_shared_to_user_home(shared.path(), home.path(), "openai_custom").unwrap();

        assert_eq!(result.copied_session_files, 0);
        assert_eq!(fs::read(&home_jsonl).unwrap(), existing_bytes.as_bytes());
        assert_eq!(
            fs::metadata(&home_jsonl).unwrap().modified().unwrap(),
            modified_before
        );
        let conn = Connection::open(home.path().join("state_5.sqlite")).unwrap();
        let provider: String = conn
            .query_row(
                "SELECT model_provider FROM threads WHERE id = 'thread-existing'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(provider, "openai_custom");
    }

    #[test]
    fn source_database_rollout_wins_over_an_earlier_imported_filename() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let dir = source.path().join("sessions/2026/07/19");
        fs::create_dir_all(&dir).unwrap();
        let base = dir.join("rollout-thread-preferred.jsonl");
        let imported = dir.join("rollout-thread-preferred-imported-deadbeefcafe.jsonl");
        let first = concat!(
            r#"{"type":"session_meta","payload":{"id":"thread-preferred"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"text":"first"}}"#,
            "\n",
        );
        fs::write(&imported, first).unwrap();
        fs::write(
            &base,
            format!(
                "{first}{}\n",
                r#"{"type":"response_item","payload":{"text":"complete tail"}}"#
            ),
        )
        .unwrap();
        create_db(
            &source.path().join("state_5.sqlite"),
            &[("thread-preferred", base.to_str().unwrap())],
        );
        create_db(&target.path().join("state_5.sqlite"), &[]);

        sync_sessions(&[source.path().to_path_buf()], target.path()).unwrap();

        let target_base = target
            .path()
            .join("sessions/2026/07/19/rollout-thread-preferred.jsonl");
        assert!(fs::read_to_string(&target_base)
            .unwrap()
            .contains("complete tail"));
        let conn = Connection::open(target.path().join("state_5.sqlite")).unwrap();
        let rollout_path: String = conn
            .query_row(
                "SELECT rollout_path FROM threads WHERE id = 'thread-preferred'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(std::path::PathBuf::from(rollout_path), target_base);
    }

    #[test]
    fn source_database_rollout_advances_to_a_strictly_more_complete_candidate() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let dir = source.path().join("sessions/2026/07/19");
        fs::create_dir_all(&dir).unwrap();
        let base = dir.join("rollout-thread-heal.jsonl");
        let imported = dir.join("rollout-thread-heal-imported-deadbeefcafe.jsonl");
        let first = concat!(
            r#"{"type":"session_meta","payload":{"id":"thread-heal"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"text":"first"}}"#,
            "\n",
        );
        fs::write(&imported, first).unwrap();
        fs::write(
            &base,
            format!(
                "{first}{}\n",
                r#"{"type":"response_item","payload":{"text":"recovered tail"}}"#
            ),
        )
        .unwrap();
        create_db(
            &source.path().join("state_5.sqlite"),
            &[("thread-heal", imported.to_str().unwrap())],
        );
        create_db(&target.path().join("state_5.sqlite"), &[]);

        sync_sessions(&[source.path().to_path_buf()], target.path()).unwrap();

        let target_base = target
            .path()
            .join("sessions/2026/07/19/rollout-thread-heal.jsonl");
        assert!(fs::read_to_string(&target_base)
            .unwrap()
            .contains("recovered tail"));
        let conn = Connection::open(target.path().join("state_5.sqlite")).unwrap();
        let rollout_path: String = conn
            .query_row(
                "SELECT rollout_path FROM threads WHERE id = 'thread-heal'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(std::path::PathBuf::from(rollout_path), target_base);
    }

    #[test]
    fn divergent_larger_candidate_does_not_override_the_database_rollout() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let dir = source.path().join("sessions/2026/07/19");
        fs::create_dir_all(&dir).unwrap();
        let base = dir.join("rollout-thread-branch.jsonl");
        let imported = dir.join("rollout-thread-branch-imported-deadbeefcafe.jsonl");
        fs::write(
            &imported,
            concat!(
                r#"{"type":"session_meta","payload":{"id":"thread-branch"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"text":"active branch"}}"#,
                "\n",
            ),
        )
        .unwrap();
        fs::write(
            &base,
            concat!(
                r#"{"type":"session_meta","payload":{"id":"thread-branch"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"text":"other branch"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"text":"larger but divergent"}}"#,
                "\n",
            ),
        )
        .unwrap();
        create_db(
            &source.path().join("state_5.sqlite"),
            &[("thread-branch", imported.to_str().unwrap())],
        );
        create_db(&target.path().join("state_5.sqlite"), &[]);

        sync_sessions(&[source.path().to_path_buf()], target.path()).unwrap();

        let target_imported = target
            .path()
            .join("sessions/2026/07/19/rollout-thread-branch-imported-deadbeefcafe.jsonl");
        assert!(fs::read_to_string(&target_imported)
            .unwrap()
            .contains("active branch"));
        let conn = Connection::open(target.path().join("state_5.sqlite")).unwrap();
        let rollout_path: String = conn
            .query_row(
                "SELECT rollout_path FROM threads WHERE id = 'thread-branch'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(std::path::PathBuf::from(rollout_path), target_imported);
    }

    #[test]
    fn shorter_source_copy_does_not_hijack_a_more_complete_target_thread() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let relative = "sessions/2026/07/19";
        let source_jsonl = source
            .path()
            .join(relative)
            .join("rollout-thread-complete-imported-aaaaaaaaaaaa.jsonl");
        let target_jsonl = target
            .path()
            .join(relative)
            .join("rollout-thread-complete.jsonl");
        fs::create_dir_all(source_jsonl.parent().unwrap()).unwrap();
        fs::create_dir_all(target_jsonl.parent().unwrap()).unwrap();
        let first = concat!(
            r#"{"type":"session_meta","payload":{"id":"thread-complete"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"text":"first"}}"#,
            "\n",
        );
        fs::write(&source_jsonl, first).unwrap();
        fs::write(
            &target_jsonl,
            format!(
                "{first}{}\n",
                r#"{"type":"response_item","payload":{"text":"target-only tail"}}"#
            ),
        )
        .unwrap();
        create_db(
            &source.path().join("state_5.sqlite"),
            &[("thread-complete", source_jsonl.to_str().unwrap())],
        );
        create_db(
            &target.path().join("state_5.sqlite"),
            &[("thread-complete", target_jsonl.to_str().unwrap())],
        );

        sync_sessions(&[source.path().to_path_buf()], target.path()).unwrap();

        let conn = Connection::open(target.path().join("state_5.sqlite")).unwrap();
        let rollout_path: String = conn
            .query_row(
                "SELECT rollout_path FROM threads WHERE id = 'thread-complete'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(std::path::PathBuf::from(&rollout_path), target_jsonl);
        assert!(fs::read_to_string(&rollout_path)
            .unwrap()
            .contains("target-only tail"));
    }

    #[test]
    fn provider_only_metadata_change_does_not_create_an_imported_conflict() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let relative = "sessions/2026/07/19/rollout-thread-provider.jsonl";
        let source_jsonl = source.path().join(relative);
        let target_jsonl = target.path().join(relative);
        fs::create_dir_all(source_jsonl.parent().unwrap()).unwrap();
        fs::create_dir_all(target_jsonl.parent().unwrap()).unwrap();
        fs::write(
            &source_jsonl,
            concat!(
                r#"{"type":"session_meta","payload":{"id":"thread-provider","model_provider":"relay"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"text":"same history"}}"#,
                "\n",
            ),
        )
        .unwrap();
        fs::write(
            &target_jsonl,
            concat!(
                r#"{"type":"session_meta","payload":{"id":"thread-provider","model_provider":"openai"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"text":"same history"}}"#,
                "\n",
            ),
        )
        .unwrap();
        create_db(
            &source.path().join("state_5.sqlite"),
            &[("thread-provider", source_jsonl.to_str().unwrap())],
        );
        create_db(
            &target.path().join("state_5.sqlite"),
            &[("thread-provider", target_jsonl.to_str().unwrap())],
        );

        sync_sessions_for_provider(&[source.path().to_path_buf()], target.path(), "openai")
            .unwrap();

        let imported = fs::read_dir(target_jsonl.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains("-imported-"))
            .count();
        assert_eq!(imported, 0);
        let conn = Connection::open(target.path().join("state_5.sqlite")).unwrap();
        let rollout_path: String = conn
            .query_row(
                "SELECT rollout_path FROM threads WHERE id = 'thread-provider'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(std::path::PathBuf::from(rollout_path), target_jsonl);
    }

    #[test]
    fn divergent_imported_copy_uses_the_base_stem_instead_of_chaining_suffixes() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let relative = "sessions/2026/07/19/rollout-thread-chain-imported-aaaaaaaaaaaa.jsonl";
        let source_jsonl = source.path().join(relative);
        let target_jsonl = target.path().join(relative);
        fs::create_dir_all(source_jsonl.parent().unwrap()).unwrap();
        fs::create_dir_all(target_jsonl.parent().unwrap()).unwrap();
        fs::write(
            &source_jsonl,
            r#"{"type":"session_meta","payload":{"id":"thread-chain","source":"source"}}"#,
        )
        .unwrap();
        fs::write(
            &target_jsonl,
            r#"{"type":"session_meta","payload":{"id":"thread-chain","source":"target"}}"#,
        )
        .unwrap();
        create_db(
            &source.path().join("state_5.sqlite"),
            &[("thread-chain", source_jsonl.to_str().unwrap())],
        );
        create_db(
            &target.path().join("state_5.sqlite"),
            &[("thread-chain", target_jsonl.to_str().unwrap())],
        );

        sync_sessions(&[source.path().to_path_buf()], target.path()).unwrap();

        let names = fs::read_dir(target_jsonl.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect::<Vec<_>>();
        assert!(names.iter().any(|name| {
            name.starts_with("rollout-thread-chain-imported-")
                && !name.contains("aaaaaaaaaaaa-imported")
        }));
    }

    #[test]
    fn hot_sync_preserves_active_rollout_when_shorter_candidate_has_a_different_name() {
        let shared = tempdir().unwrap();
        let home = tempdir().unwrap();
        let shared_jsonl = shared
            .path()
            .join("sessions/2026/07/19/rollout-thread-hot-short-imported-aaaaaaaaaaaa.jsonl");
        let home_jsonl = home
            .path()
            .join("sessions/2026/07/19/rollout-thread-hot-short.jsonl");
        let copied_candidate = home
            .path()
            .join("sessions/2026/07/19/rollout-thread-hot-short-imported-aaaaaaaaaaaa.jsonl");
        fs::create_dir_all(shared_jsonl.parent().unwrap()).unwrap();
        fs::create_dir_all(home_jsonl.parent().unwrap()).unwrap();
        fs::write(
            &shared_jsonl,
            concat!(
                r#"{"type":"session_meta","payload":{"id":"thread-hot-short","model_provider":"openai_custom"}}"#,
                "\n",
            ),
        )
        .unwrap();
        let live_bytes = concat!(
            r#"{"type":"session_meta","payload":{"id":"thread-hot-short","model_provider":"live-provider"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"text":"live tail"}}"#,
            "\n",
        );
        fs::write(&home_jsonl, live_bytes).unwrap();
        create_db(
            &shared.path().join("state_5.sqlite"),
            &[("thread-hot-short", shared_jsonl.to_str().unwrap())],
        );
        create_db(
            &home.path().join("state_5.sqlite"),
            &[("thread-hot-short", home_jsonl.to_str().unwrap())],
        );
        set_provider(home.path(), "live-provider");

        let result = sync_shared_to_user_home_hot(shared.path(), home.path(), "openai").unwrap();

        assert_eq!(result.copied_session_files, 0);
        assert!(!copied_candidate.exists());
        assert_eq!(fs::read(&home_jsonl).unwrap(), live_bytes.as_bytes());
        let conn = Connection::open(home.path().join("state_5.sqlite")).unwrap();
        let (provider, rollout_path): (String, String) = conn
            .query_row(
                "SELECT model_provider, rollout_path
                 FROM threads WHERE id = 'thread-hot-short'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(provider, "live-provider");
        assert_eq!(std::path::PathBuf::from(rollout_path), home_jsonl);
    }

    #[test]
    fn hot_sync_keeps_the_live_rollout_visible_when_a_longer_different_name_arrives() {
        let shared = tempdir().unwrap();
        let home = tempdir().unwrap();
        let shared_jsonl = shared
            .path()
            .join("sessions/2026/07/19/rollout-thread-hot-long-imported-aaaaaaaaaaaa.jsonl");
        let home_jsonl = home
            .path()
            .join("sessions/2026/07/19/rollout-thread-hot-long.jsonl");
        let copied_candidate = home
            .path()
            .join("sessions/2026/07/19/rollout-thread-hot-long-imported-aaaaaaaaaaaa.jsonl");
        fs::create_dir_all(shared_jsonl.parent().unwrap()).unwrap();
        fs::create_dir_all(home_jsonl.parent().unwrap()).unwrap();
        let live_prefix = concat!(
            r#"{"type":"session_meta","payload":{"id":"thread-hot-long","model_provider":"live-provider"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"text":"visible before sync"}}"#,
            "\n",
        );
        let shared_history = concat!(
            r#"{"type":"session_meta","payload":{"id":"thread-hot-long","model_provider":"shared-provider"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"text":"visible before sync"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"text":"shared longer history"}}"#,
            "\n",
        );
        fs::write(&shared_jsonl, shared_history).unwrap();
        fs::write(&home_jsonl, live_prefix).unwrap();
        create_official_like_db(
            &shared.path().join("state_5.sqlite"),
            &[("thread-hot-long", shared_jsonl.to_str().unwrap())],
        );
        create_official_like_db(
            &home.path().join("state_5.sqlite"),
            &[("thread-hot-long", home_jsonl.to_str().unwrap())],
        );
        let home_conn = Connection::open(home.path().join("state_5.sqlite")).unwrap();
        home_conn
            .execute(
                "UPDATE threads
                 SET model_provider = 'live-provider', title = 'Visible live title'
                 WHERE id = 'thread-hot-long'",
                [],
            )
            .unwrap();
        drop(home_conn);
        let mut live_writer = fs::OpenOptions::new()
            .append(true)
            .open(&home_jsonl)
            .unwrap();

        let result =
            sync_shared_to_user_home_hot(shared.path(), home.path(), "current-provider").unwrap();
        live_writer
            .write_all(
                concat!(
                    r#"{"type":"response_item","payload":{"text":"writer after sync"}}"#,
                    "\n",
                )
                .as_bytes(),
            )
            .unwrap();
        live_writer.sync_data().unwrap();
        drop(live_writer);

        assert_eq!(result.copied_session_files, 0);
        assert!(!copied_candidate.exists());
        let conn = Connection::open(home.path().join("state_5.sqlite")).unwrap();
        let (rollout_path, provider, title): (String, String, String) = conn
            .query_row(
                "SELECT rollout_path, model_provider, title
                 FROM threads WHERE id = 'thread-hot-long'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(std::path::PathBuf::from(&rollout_path), home_jsonl);
        assert_eq!(provider, "live-provider");
        assert_eq!(title, "Visible live title");
        let visible_history = fs::read_to_string(rollout_path).unwrap();
        assert!(visible_history.contains("visible before sync"));
        assert!(visible_history.contains("writer after sync"));
    }

    #[test]
    fn duplicate_thread_repairs_an_existing_rollout_outside_the_target_home() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let external = tempdir().unwrap();
        let source_jsonl = source
            .path()
            .join("sessions/2026/07/19/rollout-thread-outside.jsonl");
        let external_jsonl = external.path().join("rollout-thread-outside.jsonl");
        fs::create_dir_all(source_jsonl.parent().unwrap()).unwrap();
        let source_bytes = concat!(
            r#"{"type":"session_meta","payload":{"id":"thread-outside","model_provider":"openai_custom"}}"#,
            "\n",
        );
        let external_bytes = concat!(
            r#"{"type":"session_meta","payload":{"id":"thread-outside","model_provider":"external-provider"}}"#,
            "\n",
        );
        fs::write(&source_jsonl, source_bytes).unwrap();
        fs::write(&external_jsonl, external_bytes).unwrap();
        create_db(
            &source.path().join("state_5.sqlite"),
            &[("thread-outside", source_jsonl.to_str().unwrap())],
        );
        create_db(
            &target.path().join("state_5.sqlite"),
            &[("thread-outside", external_jsonl.to_str().unwrap())],
        );

        sync_sessions_for_provider(&[source.path().to_path_buf()], target.path(), "openai")
            .unwrap();

        assert_eq!(
            fs::read(&external_jsonl).unwrap(),
            external_bytes.as_bytes()
        );
        let conn = Connection::open(target.path().join("state_5.sqlite")).unwrap();
        let rollout_path: String = conn
            .query_row(
                "SELECT rollout_path FROM threads WHERE id = 'thread-outside'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(std::path::Path::new(&rollout_path).starts_with(target.path()));
    }

    #[test]
    fn source_database_rollout_matches_the_full_sessions_relative_path() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let stale = source
            .path()
            .join("sessions/2026/07/18/rollout-thread-same-name.jsonl");
        let active = source
            .path()
            .join("sessions/2026/07/19/rollout-thread-same-name.jsonl");
        fs::create_dir_all(stale.parent().unwrap()).unwrap();
        fs::create_dir_all(active.parent().unwrap()).unwrap();
        fs::write(
            &stale,
            concat!(
                r#"{"type":"session_meta","payload":{"id":"thread-same-name"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"text":"stale branch"}}"#,
                "\n",
            ),
        )
        .unwrap();
        fs::write(
            &active,
            concat!(
                r#"{"type":"session_meta","payload":{"id":"thread-same-name"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"text":"active branch"}}"#,
                "\n",
            ),
        )
        .unwrap();
        create_db(
            &source.path().join("state_5.sqlite"),
            &[("thread-same-name", active.to_str().unwrap())],
        );
        create_db(&target.path().join("state_5.sqlite"), &[]);

        sync_sessions(&[source.path().to_path_buf()], target.path()).unwrap();

        let copied = target
            .path()
            .join("sessions/2026/07/19/rollout-thread-same-name.jsonl");
        assert!(fs::read_to_string(copied)
            .unwrap()
            .contains("active branch"));
    }

    #[test]
    fn ambiguous_strict_extensions_do_not_replace_the_database_rollout() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let dir = source.path().join("sessions/2026/07/19");
        fs::create_dir_all(&dir).unwrap();
        let active = dir.join("rollout-thread-ambiguous.jsonl");
        let branch_a = dir.join("rollout-thread-ambiguous-imported-aaaaaaaaaaaa.jsonl");
        let branch_b = dir.join("rollout-thread-ambiguous-imported-bbbbbbbbbbbb.jsonl");
        let prefix = concat!(
            r#"{"type":"session_meta","payload":{"id":"thread-ambiguous"}}"#,
            "\n",
        );
        fs::write(&active, prefix).unwrap();
        fs::write(
            &branch_a,
            format!(
                "{prefix}{}\n",
                r#"{"type":"response_item","payload":{"text":"branch a"}}"#
            ),
        )
        .unwrap();
        fs::write(
            &branch_b,
            format!(
                "{prefix}{}\n",
                r#"{"type":"response_item","payload":{"text":"branch b"}}"#
            ),
        )
        .unwrap();
        create_db(
            &source.path().join("state_5.sqlite"),
            &[("thread-ambiguous", active.to_str().unwrap())],
        );
        create_db(&target.path().join("state_5.sqlite"), &[]);

        sync_sessions(&[source.path().to_path_buf()], target.path()).unwrap();

        let copied = target
            .path()
            .join("sessions/2026/07/19/rollout-thread-ambiguous.jsonl");
        assert_eq!(fs::read_to_string(copied).unwrap(), prefix);
    }

    fn set_provider(home: &std::path::Path, provider: &str) {
        let conn = Connection::open(home.join("state_5.sqlite")).unwrap();
        conn.execute("ALTER TABLE threads ADD COLUMN model_provider TEXT", [])
            .unwrap();
        conn.execute("UPDATE threads SET model_provider = ?1", [provider])
            .unwrap();
    }
}

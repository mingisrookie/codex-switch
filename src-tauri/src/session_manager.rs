use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    time::Duration,
};

use rusqlite::{
    params_from_iter, types::Value as SqlValue, Connection, OpenFlags, TransactionBehavior,
};
use serde::Serialize;
use serde_json::Value as JsonValue;

use crate::{
    backup::{
        create_state_checkpoint_with_paths, preflight_backup_capacity_with_paths, BackupManifest,
        BackupScope, CheckpointRole,
    },
    codex_paths::{local_codex_paths, resolve_user_codex_paths, CodexPaths},
    file_ops::walk_jsonl_files,
};

#[cfg(test)]
use crate::{
    backup::{
        create_backup_with_paths, ensure_roots_disjoint, preflight_backup_capacity_for_sources,
        restore_backup, BackupCapacitySource,
    },
    codex_paths::validate_absolute_root,
    file_ops::atomic_write,
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
pub struct SessionMutationFailure {
    pub message: String,
    pub backups: Vec<BackupManifest>,
}

impl SessionMutationFailure {
    fn new(message: String, backups: Vec<BackupManifest>) -> Self {
        Self { message, backups }
    }

    fn before_backup(message: String) -> Self {
        Self::new(message, Vec::new())
    }
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

#[derive(Debug, Clone, Copy)]
#[cfg(test)]
struct DeleteMutationRoots<'a> {
    current_home: &'a Path,
    shared_home: &'a Path,
    backup_root: &'a Path,
}

pub fn scan_managed_sessions(
    codex_home: &Path,
    shared_home: &Path,
) -> Result<ManagedSessionInventory, String> {
    let current_paths = resolve_user_codex_paths(codex_home)?;
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

#[cfg(test)]
fn delete_managed_sessions(
    codex_home: &Path,
    shared_home: &Path,
    backup_root: &Path,
    ids: &[String],
    confirmed: bool,
) -> Result<SessionMutationResult, String> {
    delete_managed_sessions_detailed(codex_home, shared_home, backup_root, ids, confirmed)
        .map_err(|error| error.message)
}

#[cfg(test)]
fn delete_managed_sessions_detailed(
    codex_home: &Path,
    shared_home: &Path,
    backup_root: &Path,
    ids: &[String],
    confirmed: bool,
) -> Result<SessionMutationResult, SessionMutationFailure> {
    delete_managed_sessions_detailed_with_prepare(
        codex_home,
        shared_home,
        backup_root,
        ids,
        confirmed,
        || Ok(()),
    )
}

#[cfg(test)]
pub(crate) fn delete_managed_sessions_detailed_with_prepare<Prepare>(
    codex_home: &Path,
    shared_home: &Path,
    backup_root: &Path,
    ids: &[String],
    confirmed: bool,
    prepare: Prepare,
) -> Result<SessionMutationResult, SessionMutationFailure>
where
    Prepare: FnOnce() -> Result<(), String>,
{
    #[cfg(test)]
    let fail_after_current = DeleteFailurePoint::None.fail_after_current();
    #[cfg(not(test))]
    let fail_after_current = false;
    delete_managed_sessions_inner(
        DeleteMutationRoots {
            current_home: codex_home,
            shared_home,
            backup_root,
        },
        ids,
        confirmed,
        fail_after_current,
        preflight_delete_backup_capacity,
        prepare,
        ensure_codex_still_closed,
    )
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeleteFailurePoint {
    None,
    AfterCurrent,
}

#[cfg(test)]
impl DeleteFailurePoint {
    fn fail_after_current(self) -> bool {
        self == Self::AfterCurrent
    }
}

#[cfg(test)]
fn delete_managed_sessions_with_failure_detailed(
    codex_home: &Path,
    shared_home: &Path,
    backup_root: &Path,
    ids: &[String],
    confirmed: bool,
    failure_point: DeleteFailurePoint,
) -> Result<SessionMutationResult, SessionMutationFailure> {
    delete_managed_sessions_inner(
        DeleteMutationRoots {
            current_home: codex_home,
            shared_home,
            backup_root,
        },
        ids,
        confirmed,
        failure_point.fail_after_current(),
        preflight_delete_backup_capacity,
        || Ok(()),
        ensure_codex_still_closed,
    )
}

#[cfg(test)]
fn delete_managed_sessions_with_capacity_preflight_detailed<Capacity, Prepare>(
    codex_home: &Path,
    shared_home: &Path,
    backup_root: &Path,
    ids: &[String],
    confirmed: bool,
    preflight_capacity: Capacity,
    prepare: Prepare,
) -> Result<SessionMutationResult, SessionMutationFailure>
where
    Capacity: FnOnce(&Path, &CodexPaths, &Path, &CodexPaths, &Path) -> Result<(), String>,
    Prepare: FnOnce() -> Result<(), String>,
{
    delete_managed_sessions_inner(
        DeleteMutationRoots {
            current_home: codex_home,
            shared_home,
            backup_root,
        },
        ids,
        confirmed,
        false,
        preflight_capacity,
        prepare,
        ensure_codex_still_closed,
    )
}

#[cfg(test)]
fn delete_managed_sessions_with_gates_detailed<Capacity, Prepare, FinalGate>(
    roots: DeleteMutationRoots<'_>,
    ids: &[String],
    confirmed: bool,
    preflight_capacity: Capacity,
    prepare: Prepare,
    final_gate: FinalGate,
) -> Result<SessionMutationResult, SessionMutationFailure>
where
    Capacity: FnOnce(&Path, &CodexPaths, &Path, &CodexPaths, &Path) -> Result<(), String>,
    Prepare: FnOnce() -> Result<(), String>,
    FinalGate: FnOnce(&str) -> Result<(), String>,
{
    delete_managed_sessions_inner(
        roots,
        ids,
        confirmed,
        false,
        preflight_capacity,
        prepare,
        final_gate,
    )
}

#[cfg(test)]
fn delete_managed_sessions_inner<Capacity, Prepare, FinalGate>(
    roots: DeleteMutationRoots<'_>,
    ids: &[String],
    confirmed: bool,
    fail_after_current: bool,
    preflight_capacity: Capacity,
    prepare: Prepare,
    final_gate: FinalGate,
) -> Result<SessionMutationResult, SessionMutationFailure>
where
    Capacity: FnOnce(&Path, &CodexPaths, &Path, &CodexPaths, &Path) -> Result<(), String>,
    Prepare: FnOnce() -> Result<(), String>,
    FinalGate: FnOnce(&str) -> Result<(), String>,
{
    let selected = normalized_ids(ids);
    if selected.is_empty() {
        return Ok(empty_result(0));
    }
    if !confirmed {
        return Err(SessionMutationFailure::before_backup(
            "硬删除会话需要二次确认".to_string(),
        ));
    }

    let selected_set = selected.iter().cloned().collect::<HashSet<_>>();
    let mut result = empty_result(selected.len());
    let (current_paths, shared_paths) =
        resolve_mutation_paths(roots.current_home, roots.shared_home, roots.backup_root)
            .map_err(SessionMutationFailure::before_backup)?;
    preflight_capacity(
        roots.current_home,
        &current_paths,
        roots.shared_home,
        &shared_paths,
        roots.backup_root,
    )
    .map_err(SessionMutationFailure::before_backup)?;
    prepare().map_err(SessionMutationFailure::before_backup)?;
    let current_backup = create_backup_with_paths(
        roots.current_home,
        roots.backup_root,
        "delete-sessions-current",
        current_paths.clone(),
    )
    .map_err(SessionMutationFailure::before_backup)?;
    let shared_backup = create_backup_with_paths(
        roots.shared_home,
        roots.backup_root,
        "delete-sessions-shared",
        shared_paths.clone(),
    )
    .map_err(|message| SessionMutationFailure::new(message, vec![current_backup.clone()]))?;
    result.backups = vec![current_backup.clone(), shared_backup.clone()];

    let backups = result.backups.clone();
    final_gate("delete")
        .map_err(|message| SessionMutationFailure::new(message, backups.clone()))?;
    let mutation = (|| {
        apply_delete_to_root(&current_paths, &selected_set, &mut result)?;
        if fail_after_current {
            return Err("injected failure after current root deletion".to_string());
        }
        apply_delete_to_root(&shared_paths, &selected_set, &mut result)?;
        Ok(result)
    })();

    match mutation {
        Ok(result) => Ok(result),
        Err(error) => {
            let current_restore = restore_backup(&current_backup.backup_dir, roots.current_home);
            let shared_restore = restore_backup(&shared_backup.backup_dir, roots.shared_home);
            match (current_restore, shared_restore) {
                (Ok(_), Ok(_)) => Err(SessionMutationFailure::new(
                    format!("delete failed: {error}; rolled back both roots"),
                    backups,
                )),
                (current, shared) => Err(SessionMutationFailure::new(
                    format!(
                        "delete failed: {error}; rollback failed (current: {}; shared: {})",
                        restore_status(current),
                        restore_status(shared)
                    ),
                    backups,
                )),
            }
        }
    }
}

#[cfg(test)]
fn preflight_delete_backup_capacity(
    current_home: &Path,
    current_paths: &CodexPaths,
    shared_home: &Path,
    shared_paths: &CodexPaths,
    backup_root: &Path,
) -> Result<(), String> {
    preflight_backup_capacity_for_sources(
        backup_root,
        &[
            BackupCapacitySource {
                home: current_home,
                paths: current_paths,
                scope: BackupScope::Full,
            },
            BackupCapacitySource {
                home: shared_home,
                paths: shared_paths,
                scope: BackupScope::Full,
            },
        ],
    )
    .map(|_| ())
}

#[cfg(test)]
fn restore_status(result: Result<crate::backup::RestoreResult, String>) -> String {
    match result {
        Ok(_) => "ok".to_string(),
        Err(error) => error,
    }
}

#[cfg(test)]
fn restore_sessions_visible(
    codex_home: &Path,
    backup_root: &Path,
    ids: &[String],
) -> Result<SessionMutationResult, String> {
    restore_sessions_visible_detailed(codex_home, backup_root, ids).map_err(|error| error.message)
}

#[cfg(test)]
fn restore_sessions_visible_detailed(
    codex_home: &Path,
    backup_root: &Path,
    ids: &[String],
) -> Result<SessionMutationResult, SessionMutationFailure> {
    restore_sessions_visible_detailed_with_prepare(
        codex_home,
        backup_root,
        ids,
        "restore-visible-test",
        || Ok(()),
    )
}

pub(crate) fn restore_sessions_visible_detailed_with_prepare<Prepare>(
    codex_home: &Path,
    backup_root: &Path,
    ids: &[String],
    operation_id: &str,
    prepare: Prepare,
) -> Result<SessionMutationResult, SessionMutationFailure>
where
    Prepare: FnOnce() -> Result<(), String>,
{
    restore_sessions_visible_inner(
        codex_home,
        backup_root,
        ids,
        operation_id,
        preflight_visibility_backup_capacity,
        prepare,
        ensure_codex_still_closed,
    )
}

#[cfg(test)]
fn restore_sessions_visible_with_capacity_preflight_detailed<Capacity, Prepare>(
    codex_home: &Path,
    backup_root: &Path,
    ids: &[String],
    preflight_capacity: Capacity,
    prepare: Prepare,
) -> Result<SessionMutationResult, SessionMutationFailure>
where
    Capacity: FnOnce(&Path, &CodexPaths, &Path) -> Result<(), String>,
    Prepare: FnOnce() -> Result<(), String>,
{
    restore_sessions_visible_inner(
        codex_home,
        backup_root,
        ids,
        "restore-visible-capacity-test",
        preflight_capacity,
        prepare,
        ensure_codex_still_closed,
    )
}

#[cfg(test)]
fn restore_sessions_visible_with_gates_detailed<Capacity, Prepare, FinalGate>(
    codex_home: &Path,
    backup_root: &Path,
    ids: &[String],
    preflight_capacity: Capacity,
    prepare: Prepare,
    final_gate: FinalGate,
) -> Result<SessionMutationResult, SessionMutationFailure>
where
    Capacity: FnOnce(&Path, &CodexPaths, &Path) -> Result<(), String>,
    Prepare: FnOnce() -> Result<(), String>,
    FinalGate: FnOnce(&str) -> Result<(), String>,
{
    restore_sessions_visible_inner(
        codex_home,
        backup_root,
        ids,
        "restore-visible-gates-test",
        preflight_capacity,
        prepare,
        final_gate,
    )
}

fn restore_sessions_visible_inner<Capacity, Prepare, FinalGate>(
    codex_home: &Path,
    backup_root: &Path,
    ids: &[String],
    operation_id: &str,
    preflight_capacity: Capacity,
    prepare: Prepare,
    final_gate: FinalGate,
) -> Result<SessionMutationResult, SessionMutationFailure>
where
    Capacity: FnOnce(&Path, &CodexPaths, &Path) -> Result<(), String>,
    Prepare: FnOnce() -> Result<(), String>,
    FinalGate: FnOnce(&str) -> Result<(), String>,
{
    let selected = normalized_ids(ids);
    if selected.is_empty() {
        return Ok(empty_result(0));
    }
    let selected_set = selected.iter().cloned().collect::<HashSet<_>>();
    let mut result = empty_result(selected.len());
    let paths =
        resolve_user_codex_paths(codex_home).map_err(SessionMutationFailure::before_backup)?;
    preflight_capacity(codex_home, &paths, backup_root)
        .map_err(SessionMutationFailure::before_backup)?;
    prepare().map_err(SessionMutationFailure::before_backup)?;
    result.backups.push(
        create_state_checkpoint_with_paths(
            codex_home,
            backup_root,
            "restore-sessions-visible",
            paths.clone(),
            operation_id,
            CheckpointRole::Visibility,
        )
        .map_err(SessionMutationFailure::before_backup)?,
    );
    final_gate("visibility restore")
        .map_err(|message| SessionMutationFailure::new(message, result.backups.clone()))?;
    result.restored_threads = restore_visible_in_db(&paths.state_db, &selected_set)
        .map_err(|message| SessionMutationFailure::new(message, result.backups.clone()))?;
    Ok(result)
}

fn preflight_visibility_backup_capacity(
    codex_home: &Path,
    paths: &CodexPaths,
    backup_root: &Path,
) -> Result<(), String> {
    preflight_backup_capacity_with_paths(backup_root, codex_home, paths, BackupScope::StateOnly)
        .map(|_| ())
}

#[cfg(test)]
fn resolve_mutation_paths(
    codex_home: &Path,
    shared_home: &Path,
    backup_root: &Path,
) -> Result<(CodexPaths, CodexPaths), String> {
    validate_absolute_root(codex_home, "CODEX_HOME")?;
    validate_absolute_root(shared_home, "shared session root")?;
    validate_absolute_root(backup_root, "backup root")?;
    let current_paths = resolve_user_codex_paths(codex_home)?;
    let shared_paths = local_codex_paths(shared_home);
    for (left, left_label, right, right_label) in [
        (codex_home, "CODEX_HOME", shared_home, "shared session root"),
        (codex_home, "CODEX_HOME", backup_root, "backup root"),
        (
            shared_home,
            "shared session root",
            backup_root,
            "backup root",
        ),
        (
            current_paths.sqlite_home.as_path(),
            "SQLite root",
            shared_home,
            "shared session root",
        ),
        (
            current_paths.sqlite_home.as_path(),
            "SQLite root",
            backup_root,
            "backup root",
        ),
    ] {
        ensure_roots_disjoint(left, left_label, right, right_label)?;
    }
    Ok((current_paths, shared_paths))
}

fn ensure_codex_still_closed(operation: &str) -> Result<(), String> {
    #[cfg(not(test))]
    {
        let managed_running = !crate::process_control::list_codex_processes()?.is_empty();
        let standalone_running =
            !crate::process_control::list_standalone_codex_processes()?.is_empty();
        ensure_closed_process_presence(operation, managed_running, standalone_running)
    }
    #[cfg(test)]
    {
        let _ = operation;
        Ok(())
    }
}

fn ensure_closed_process_presence(
    operation: &str,
    managed_running: bool,
    standalone_running: bool,
) -> Result<(), String> {
    match (managed_running, standalone_running) {
        (false, false) => Ok(()),
        (true, false) => Err(format!(
            "ChatGPT started during {operation} preflight; close it and retry before files are changed"
        )),
        (false, true) => Err(format!(
            "a standalone Codex CLI started during {operation} preflight; close it and retry before files are changed"
        )),
        (true, true) => Err(format!(
            "ChatGPT and a standalone Codex CLI started during {operation} preflight; close them and retry before files are changed"
        )),
    }
}

fn scan_source(paths: &CodexPaths) -> Result<HashMap<String, SessionSourceRecord>, String> {
    let mut records = read_thread_records(&paths.state_db)?;
    for record in records.values_mut() {
        if record.session_file.is_none() {
            record.session_file = rollout_file_for_record(paths, record);
        }
    }
    for (session_files, archived) in [
        (read_session_files(&paths.sessions_dir)?, false),
        (read_session_files(&paths.archived_sessions_dir)?, true),
    ] {
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
                    archived,
                    archived_at: None,
                    rollout_path: None,
                    session_file: Some(session_file),
                });
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
        let record = row.map_err(|error| format!("failed to collect managed threads: {error}"))?;
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
    for path in walk_jsonl_files(path)? {
        if let Some(id) = session_file_id(&path)? {
            output.push((id, path));
        }
    }
    Ok(output)
}

#[cfg(test)]
fn apply_delete_to_root(
    paths: &CodexPaths,
    ids: &HashSet<String>,
    result: &mut SessionMutationResult,
) -> Result<(), String> {
    result.deleted_threads += delete_db_rows(&paths.state_db, ids)?;
    delete_auxiliary_db_rows(paths, ids)?;
    result.deleted_session_files += delete_session_files(paths, ids)?;
    result.removed_session_index_entries +=
        remove_session_index_entries(&paths.session_index, ids)?;
    Ok(())
}

#[cfg(test)]
fn delete_auxiliary_db_rows(paths: &CodexPaths, ids: &HashSet<String>) -> Result<(), String> {
    delete_rows_in_database(
        &paths.goals_db,
        "goals_1.sqlite",
        &[
            ("thread_goal_continuation_deferrals", "thread_id"),
            ("thread_goals", "thread_id"),
        ],
        ids,
    )?;
    delete_rows_in_database(
        &paths.memories_db,
        "memories_1.sqlite",
        &[("stage1_outputs", "thread_id")],
        ids,
    )?;
    delete_rows_in_database(
        &paths.logs_db,
        "logs_2.sqlite",
        &[("logs", "thread_id")],
        ids,
    )
}

#[cfg(test)]
fn delete_rows_in_database(
    path: &Path,
    database_name: &str,
    tables: &[(&str, &str)],
    ids: &HashSet<String>,
) -> Result<(), String> {
    if !path.exists() || ids.is_empty() {
        return Ok(());
    }
    let mut conn = Connection::open(path)
        .map_err(|error| format!("failed to open {database_name}: {error}"))?;
    conn.busy_timeout(Duration::from_secs(2))
        .map_err(|error| format!("failed to configure {database_name} busy timeout: {error}"))?;
    let transaction = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| format!("failed to begin {database_name} delete transaction: {error}"))?;
    let mut expected_rows = Vec::with_capacity(tables.len());
    for &(table, column) in tables {
        expected_rows.push((
            table,
            table_rows_snapshot(
                &transaction,
                table,
                &[(column, ids)],
                SnapshotSelection::NotMatching,
                &[],
            )?,
        ));
    }
    let mut deleted = 0;
    for (table, column) in tables {
        delete_matching_rows(&transaction, table, &[(column, ids)], &mut deleted)?;
    }
    for (table, expected) in expected_rows {
        if table_rows_snapshot(&transaction, table, &[], SnapshotSelection::All, &[])? != expected {
            return Err(format!(
                "failed to verify delete write set in {database_name}.{table}"
            ));
        }
    }
    transaction
        .commit()
        .map_err(|error| format!("failed to commit {database_name} delete transaction: {error}"))
}

#[cfg(test)]
fn delete_db_rows(path: &Path, ids: &HashSet<String>) -> Result<usize, String> {
    if !path.exists() {
        return Ok(0);
    }
    let mut conn = Connection::open(path)
        .map_err(|error| format!("failed to open state_5.sqlite: {error}"))?;
    conn.busy_timeout(Duration::from_secs(2))
        .map_err(|error| format!("failed to configure state_5.sqlite busy timeout: {error}"))?;
    let transaction = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| format!("failed to begin state_5.sqlite delete transaction: {error}"))?;
    let dynamic_tools_expected = table_rows_snapshot(
        &transaction,
        "thread_dynamic_tools",
        &[("thread_id", ids)],
        SnapshotSelection::NotMatching,
        &[],
    )?;
    let thread_goals_expected = table_rows_snapshot(
        &transaction,
        "thread_goals",
        &[("thread_id", ids)],
        SnapshotSelection::NotMatching,
        &[],
    )?;
    let spawn_edges_expected = table_rows_snapshot(
        &transaction,
        "thread_spawn_edges",
        &[("parent_thread_id", ids), ("child_thread_id", ids)],
        SnapshotSelection::NotMatching,
        &[],
    )?;
    let threads_expected = table_rows_snapshot(
        &transaction,
        "threads",
        &[("id", ids)],
        SnapshotSelection::NotMatching,
        &[],
    )?;
    let mut deleted = 0;
    delete_matching_rows(
        &transaction,
        "thread_dynamic_tools",
        &[("thread_id", ids)],
        &mut deleted,
    )?;
    delete_matching_rows(
        &transaction,
        "thread_goals",
        &[("thread_id", ids)],
        &mut deleted,
    )?;
    delete_matching_rows(
        &transaction,
        "thread_spawn_edges",
        &[("parent_thread_id", ids), ("child_thread_id", ids)],
        &mut deleted,
    )?;
    let threads_before = table_row_count(&transaction, "threads")?;
    let selected_threads_before = matching_row_count(&transaction, "threads", "id", ids)?;
    let before = deleted;
    delete_matching_rows(&transaction, "threads", &[("id", ids)], &mut deleted)?;
    let deleted_threads = deleted - before;
    for (table, expected) in [
        ("thread_dynamic_tools", dynamic_tools_expected),
        ("thread_goals", thread_goals_expected),
        ("thread_spawn_edges", spawn_edges_expected),
        ("threads", threads_expected),
    ] {
        if table_rows_snapshot(&transaction, table, &[], SnapshotSelection::All, &[])? != expected {
            return Err(format!(
                "failed to verify state_5.sqlite.{table} delete write set"
            ));
        }
    }
    let expected_threads = threads_before
        .checked_sub(selected_threads_before)
        .ok_or_else(|| "invalid state_5.sqlite thread count".to_string())?;
    if deleted_threads != selected_threads_before
        || table_row_count(&transaction, "threads")? != expected_threads
    {
        return Err("failed to verify state_5.sqlite delete write set".to_string());
    }
    transaction
        .commit()
        .map_err(|error| format!("failed to commit state_5.sqlite delete transaction: {error}"))?;
    Ok(deleted_threads)
}

fn matching_row_count(
    conn: &Connection,
    table: &str,
    column: &str,
    ids: &HashSet<String>,
) -> Result<usize, String> {
    if !table_exists(conn, table)?
        || !table_columns(conn, table)?
            .iter()
            .any(|existing| existing == column)
        || ids.is_empty()
    {
        return Ok(0);
    }
    let placeholders = (0..ids.len()).map(|_| "?").collect::<Vec<_>>().join(", ");
    let sql = format!("SELECT COUNT(*) FROM {table} WHERE {column} IN ({placeholders})");
    let count: i64 = conn
        .query_row(
            &sql,
            params_from_iter(ids.iter().cloned().map(SqlValue::Text)),
            |row| row.get(0),
        )
        .map_err(|error| format!("failed to verify {table} rows: {error}"))?;
    usize::try_from(count).map_err(|_| format!("invalid row count returned by {table}"))
}

#[cfg(test)]
fn table_row_count(conn: &Connection, table: &str) -> Result<usize, String> {
    if !table_exists(conn, table)? {
        return Ok(0);
    }
    let count: i64 = conn
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .map_err(|error| format!("failed to count {table} rows: {error}"))?;
    usize::try_from(count).map_err(|_| format!("invalid row count returned by {table}"))
}

#[derive(Clone, Copy)]
enum SnapshotSelection {
    #[cfg(test)]
    All,
    Matching,
    NotMatching,
}

fn table_rows_snapshot(
    conn: &Connection,
    table: &str,
    filters: &[(&str, &HashSet<String>)],
    selection: SnapshotSelection,
    ignored_columns: &[&str],
) -> Result<Vec<Vec<u8>>, String> {
    if !table_exists(conn, table)? {
        return Ok(Vec::new());
    }
    let columns = table_columns(conn, table)?;
    let mut clauses = Vec::new();
    let mut values = Vec::new();
    for (column, ids) in filters {
        if ids.is_empty() || !columns.iter().any(|existing| existing == column) {
            continue;
        }
        let placeholders = (0..ids.len()).map(|_| "?").collect::<Vec<_>>().join(", ");
        clauses.push(format!(
            "{} IN ({placeholders})",
            quote_sql_identifier(column)
        ));
        values.extend(ids.iter().cloned().map(SqlValue::Text));
    }
    let selector = if clauses.is_empty() {
        "0".to_string()
    } else {
        format!("COALESCE(({}), 0)", clauses.join(" OR "))
    };
    let sql = format!(
        "SELECT *, {selector} AS __codex_switch_selected FROM {}",
        quote_sql_identifier(table)
    );
    let mut statement = conn
        .prepare(&sql)
        .map_err(|error| format!("failed to snapshot {table}: {error}"))?;
    let mut rows = statement
        .query(params_from_iter(values))
        .map_err(|error| format!("failed to query {table} snapshot: {error}"))?;
    let mut snapshot = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|error| format!("failed to collect {table} snapshot: {error}"))?
    {
        let matches = row
            .get::<usize, i64>(columns.len())
            .map_err(|error| format!("failed to read {table} snapshot selector: {error}"))?
            != 0;
        let include = match selection {
            #[cfg(test)]
            SnapshotSelection::All => true,
            SnapshotSelection::Matching => matches,
            SnapshotSelection::NotMatching => !matches,
        };
        if !include {
            continue;
        }
        let mut encoded = Vec::new();
        for (index, column) in columns.iter().enumerate() {
            if ignored_columns.iter().any(|ignored| *ignored == column) {
                continue;
            }
            let value = row
                .get::<usize, SqlValue>(index)
                .map_err(|error| format!("failed to read {table}.{column}: {error}"))?;
            encode_snapshot_value(&mut encoded, &value);
        }
        snapshot.push(encoded);
    }
    snapshot.sort_unstable();
    Ok(snapshot)
}

fn quote_sql_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn encode_snapshot_value(output: &mut Vec<u8>, value: &SqlValue) {
    match value {
        SqlValue::Null => output.push(0),
        SqlValue::Integer(value) => {
            output.push(1);
            output.extend_from_slice(&value.to_le_bytes());
        }
        SqlValue::Real(value) => {
            output.push(2);
            output.extend_from_slice(&value.to_bits().to_le_bytes());
        }
        SqlValue::Text(value) => {
            output.push(3);
            encode_snapshot_bytes(output, value.as_bytes());
        }
        SqlValue::Blob(value) => {
            output.push(4);
            encode_snapshot_bytes(output, value);
        }
    }
}

fn encode_snapshot_bytes(output: &mut Vec<u8>, value: &[u8]) {
    output.extend_from_slice(&(value.len() as u64).to_le_bytes());
    output.extend_from_slice(value);
}

#[cfg(test)]
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

#[cfg(test)]
fn delete_session_files(paths: &CodexPaths, ids: &HashSet<String>) -> Result<usize, String> {
    let mut deleted = 0;
    for root in [&paths.sessions_dir, &paths.archived_sessions_dir] {
        if !root.exists() {
            continue;
        }
        for path in walk_jsonl_files(root)? {
            let Some(id) = session_file_id(&path)? else {
                continue;
            };
            if ids.contains(&id) && remove_file_under_root(&path, root)? {
                deleted += 1;
            }
        }
    }
    Ok(deleted)
}

#[cfg(test)]
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
        atomic_write(path, output.as_bytes())?;
    }
    Ok(removed)
}

fn restore_visible_in_db(path: &Path, ids: &HashSet<String>) -> Result<usize, String> {
    if !path.exists() || ids.is_empty() {
        return Ok(0);
    }
    let mut conn = Connection::open(path)
        .map_err(|error| format!("failed to open state_5.sqlite: {error}"))?;
    conn.busy_timeout(Duration::from_secs(2))
        .map_err(|error| format!("failed to configure state_5.sqlite busy timeout: {error}"))?;
    let transaction = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| {
            format!("failed to begin state_5.sqlite visibility transaction: {error}")
        })?;
    if !table_exists(&transaction, "threads")? {
        return Ok(0);
    }
    let columns = table_columns(&transaction, "threads")?;
    if !columns.iter().any(|column| column == "archived") {
        return Ok(0);
    }
    let selected_existing = matching_row_count(&transaction, "threads", "id", ids)?;
    let unselected_before = table_rows_snapshot(
        &transaction,
        "threads",
        &[("id", ids)],
        SnapshotSelection::NotMatching,
        &[],
    )?;
    let selected_protected_before = table_rows_snapshot(
        &transaction,
        "threads",
        &[("id", ids)],
        SnapshotSelection::Matching,
        &["archived", "archived_at"],
    )?;
    let mut assignments = vec!["archived = 0".to_string()];
    if columns.iter().any(|column| column == "archived_at") {
        assignments.push("archived_at = NULL".to_string());
    }
    let placeholders = (0..ids.len()).map(|_| "?").collect::<Vec<_>>().join(", ");
    let sql = format!(
        "UPDATE threads SET {} WHERE id IN ({placeholders})",
        assignments.join(", ")
    );
    let updated = transaction
        .execute(
            &sql,
            params_from_iter(ids.iter().cloned().map(SqlValue::Text)),
        )
        .map_err(|error| format!("failed to restore visible sessions: {error}"))?;
    let visibility_mismatch = if columns.iter().any(|column| column == "archived_at") {
        "(archived IS NOT 0 OR archived_at IS NOT NULL)"
    } else {
        "archived IS NOT 0"
    };
    let hidden_sql = format!(
        "SELECT COUNT(*) FROM threads WHERE id IN ({placeholders}) AND {visibility_mismatch}"
    );
    let hidden: i64 = transaction
        .query_row(
            &hidden_sql,
            params_from_iter(ids.iter().cloned().map(SqlValue::Text)),
            |row| row.get(0),
        )
        .map_err(|error| format!("failed to verify restored visible sessions: {error}"))?;
    let selected_remaining = matching_row_count(&transaction, "threads", "id", ids)?;
    let unselected_after = table_rows_snapshot(
        &transaction,
        "threads",
        &[("id", ids)],
        SnapshotSelection::NotMatching,
        &[],
    )?;
    let selected_protected_after = table_rows_snapshot(
        &transaction,
        "threads",
        &[("id", ids)],
        SnapshotSelection::Matching,
        &["archived", "archived_at"],
    )?;
    if updated != selected_existing
        || hidden != 0
        || selected_remaining != selected_existing
        || unselected_after != unselected_before
        || selected_protected_after != selected_protected_before
    {
        return Err("failed to verify restored visible sessions".to_string());
    }
    transaction
        .commit()
        .map_err(|error| format!("failed to commit visibility transaction: {error}"))?;
    Ok(updated)
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

#[cfg(test)]
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

fn location_from_record(
    paths: &CodexPaths,
    record: &SessionSourceRecord,
) -> ManagedSessionLocation {
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

#[cfg(test)]
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
    record.and_then(|record| {
        record
            .updated_at_ms
            .or(record.updated_at.map(|value| value * 1000))
    })
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

    use crate::backup::BackupScope;

    use super::{
        delete_db_rows, delete_managed_sessions,
        delete_managed_sessions_with_capacity_preflight_detailed,
        delete_managed_sessions_with_failure_detailed, delete_managed_sessions_with_gates_detailed,
        delete_rows_in_database, empty_result, ensure_closed_process_presence,
        restore_sessions_visible, restore_sessions_visible_detailed,
        restore_sessions_visible_with_capacity_preflight_detailed,
        restore_sessions_visible_with_gates_detailed, restore_visible_in_db, scan_managed_sessions,
        DeleteFailurePoint, DeleteMutationRoots,
    };

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

    fn write_archived_jsonl(home: &std::path::Path, id: &str) -> std::path::PathBuf {
        let path = home.join(format!("archived_sessions/rollout-{id}.jsonl"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            format!(r#"{{"type":"session_meta","payload":{{"id":"{id}"}}}}"#),
        )
        .unwrap();
        path
    }

    fn create_auxiliary_dbs(home: &std::path::Path) {
        let goals = Connection::open(home.join("goals_1.sqlite")).unwrap();
        goals
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 CREATE TABLE thread_goals (
                    thread_id TEXT PRIMARY KEY,
                    goal_id TEXT NOT NULL
                 );
                 CREATE TABLE thread_goal_continuation_deferrals (
                    thread_id TEXT PRIMARY KEY REFERENCES thread_goals(thread_id) ON DELETE CASCADE
                 );
                 INSERT INTO thread_goals VALUES ('thread-a', 'goal-a');
                 INSERT INTO thread_goals VALUES ('thread-b', 'goal-b');
                 INSERT INTO thread_goal_continuation_deferrals VALUES ('thread-a');",
            )
            .unwrap();
        let memories = Connection::open(home.join("memories_1.sqlite")).unwrap();
        memories
            .execute_batch(
                "CREATE TABLE stage1_outputs (thread_id TEXT PRIMARY KEY, raw_memory TEXT);
                 INSERT INTO stage1_outputs VALUES ('thread-a', 'a');
                 INSERT INTO stage1_outputs VALUES ('thread-b', 'b');",
            )
            .unwrap();
        let logs = Connection::open(home.join("logs_2.sqlite")).unwrap();
        logs.execute_batch(
            "CREATE TABLE logs (id INTEGER PRIMARY KEY, thread_id TEXT);
             INSERT INTO logs VALUES (1, 'thread-a');
             INSERT INTO logs VALUES (2, 'thread-b');",
        )
        .unwrap();
    }

    fn count_rows(path: &std::path::Path, table: &str, thread_id: &str) -> i64 {
        Connection::open(path)
            .unwrap()
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE thread_id = ?1"),
                [thread_id],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn snapshot_tree(root: &std::path::Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
        fn collect(
            root: &std::path::Path,
            directory: &std::path::Path,
            output: &mut Vec<(std::path::PathBuf, Vec<u8>)>,
        ) {
            let mut entries = fs::read_dir(directory)
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let path = entry.path();
                if path.is_dir() {
                    collect(root, &path, output);
                } else {
                    output.push((
                        path.strip_prefix(root).unwrap().to_path_buf(),
                        fs::read(path).unwrap(),
                    ));
                }
            }
        }

        let mut output = Vec::new();
        collect(root, root, &mut output);
        output
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
            &[(
                "thread-a",
                current_jsonl.to_str().unwrap(),
                2000,
                0,
                "Current",
            )],
        );
        create_db(
            &shared.path().join("state_5.sqlite"),
            &[(
                "thread-a",
                shared_jsonl.to_str().unwrap(),
                1000,
                1,
                "Shared",
            )],
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
    fn managed_inventory_includes_archived_session_files_and_respects_rollout_path() {
        let current = tempdir().unwrap();
        let shared = tempdir().unwrap();
        let archived_jsonl = write_archived_jsonl(current.path(), "thread-archived");
        write_archived_jsonl(shared.path(), "thread-file-only");
        let rollout_path = archived_jsonl
            .strip_prefix(current.path())
            .unwrap()
            .to_str()
            .unwrap();
        create_db(
            &current.path().join("state_5.sqlite"),
            &[("thread-archived", rollout_path, 1000, 1, "Archived")],
        );

        let inventory = scan_managed_sessions(current.path(), shared.path()).unwrap();

        let archived = inventory
            .sessions
            .iter()
            .find(|session| session.id == "thread-archived")
            .unwrap();
        assert!(archived.archived);
        assert_eq!(
            archived
                .current
                .as_ref()
                .and_then(|location| location.session_file.as_ref()),
            Some(&archived_jsonl)
        );
        let file_only = inventory
            .sessions
            .iter()
            .find(|session| session.id == "thread-file-only")
            .unwrap();
        assert!(file_only.archived);
        assert_eq!(inventory.archived_count, 2);
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
        create_auxiliary_dbs(current.path());
        create_auxiliary_dbs(shared.path());

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
        assert!(result
            .backups
            .iter()
            .all(|backup| backup.scope == BackupScope::Full));
        assert_eq!(result.deleted_threads, 2);
        assert_eq!(result.deleted_session_files, 2);
        assert_eq!(result.removed_session_index_entries, 1);
        assert!(!current_jsonl.exists());
        assert!(!shared_jsonl.exists());
        for home in [current.path(), shared.path()] {
            for (database, table) in [
                ("goals_1.sqlite", "thread_goals"),
                ("memories_1.sqlite", "stage1_outputs"),
                ("logs_2.sqlite", "logs"),
            ] {
                assert_eq!(count_rows(&home.join(database), table, "thread-a"), 0);
                assert_eq!(count_rows(&home.join(database), table, "thread-b"), 1);
            }
        }
    }

    #[test]
    fn hard_delete_removes_archived_rollouts_from_both_roots() {
        let current = tempdir().unwrap();
        let shared = tempdir().unwrap();
        let backup = tempdir().unwrap();
        let current_jsonl = write_archived_jsonl(current.path(), "thread-a");
        let shared_jsonl = write_archived_jsonl(shared.path(), "thread-a");
        create_db(
            &current.path().join("state_5.sqlite"),
            &[(
                "thread-a",
                current_jsonl
                    .strip_prefix(current.path())
                    .unwrap()
                    .to_str()
                    .unwrap(),
                1000,
                1,
                "A",
            )],
        );
        create_db(
            &shared.path().join("state_5.sqlite"),
            &[(
                "thread-a",
                shared_jsonl
                    .strip_prefix(shared.path())
                    .unwrap()
                    .to_str()
                    .unwrap(),
                1000,
                1,
                "A",
            )],
        );

        let result = delete_managed_sessions(
            current.path(),
            shared.path(),
            backup.path(),
            &["thread-a".to_string()],
            true,
        )
        .unwrap();

        assert_eq!(result.deleted_threads, 2);
        assert_eq!(result.deleted_session_files, 2);
        assert!(!current_jsonl.exists());
        assert!(!shared_jsonl.exists());
    }

    #[test]
    fn hard_delete_routes_all_sqlite_mutations_to_external_sqlite_home() {
        let current = tempdir().unwrap();
        let sqlite_home = tempdir().unwrap();
        let shared = tempdir().unwrap();
        let backup = tempdir().unwrap();
        fs::write(
            current.path().join("config.toml"),
            format!("sqlite_home = \"{}\"\n", sqlite_home.path().display()).replace('\\', "\\\\"),
        )
        .unwrap();
        let current_jsonl = write_jsonl(current.path(), "thread-a");
        create_db(
            &sqlite_home.path().join("state_5.sqlite"),
            &[("thread-a", current_jsonl.to_str().unwrap(), 1000, 0, "A")],
        );
        create_auxiliary_dbs(sqlite_home.path());

        let result = delete_managed_sessions(
            current.path(),
            shared.path(),
            backup.path(),
            &["thread-a".to_string()],
            true,
        )
        .unwrap();

        assert_eq!(result.deleted_threads, 1);
        assert!(!current_jsonl.exists());
        for (database, table) in [
            ("goals_1.sqlite", "thread_goals"),
            ("memories_1.sqlite", "stage1_outputs"),
            ("logs_2.sqlite", "logs"),
        ] {
            assert_eq!(
                count_rows(&sqlite_home.path().join(database), table, "thread-a"),
                0
            );
            assert_eq!(
                count_rows(&sqlite_home.path().join(database), table, "thread-b"),
                1
            );
            assert!(!current.path().join(database).exists());
        }
    }

    #[test]
    fn hard_delete_rejects_overlapping_external_sqlite_and_shared_roots() {
        let current = tempdir().unwrap();
        let shared = tempdir().unwrap();
        let backup = tempdir().unwrap();
        fs::write(
            current.path().join("config.toml"),
            format!("sqlite_home = \"{}\"\n", shared.path().display()).replace('\\', "\\\\"),
        )
        .unwrap();

        let error = delete_managed_sessions(
            current.path(),
            shared.path(),
            backup.path(),
            &["thread-a".to_string()],
            true,
        )
        .unwrap_err();

        assert!(error.contains("must not overlap"));
        assert_eq!(fs::read_dir(backup.path()).unwrap().count(), 0);
    }

    #[test]
    fn hard_delete_capacity_failure_precedes_backups_and_live_mutation() {
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
        let current_db_before = fs::read(current.path().join("state_5.sqlite")).unwrap();
        let shared_db_before = fs::read(shared.path().join("state_5.sqlite")).unwrap();
        let current_jsonl_before = fs::read(&current_jsonl).unwrap();
        let shared_jsonl_before = fs::read(&shared_jsonl).unwrap();

        let error = delete_managed_sessions_with_capacity_preflight_detailed(
            current.path(),
            shared.path(),
            backup.path(),
            &["thread-a".to_string()],
            true,
            |_, _, _, _, _| Err("injected capacity failure".to_string()),
            || panic!("capacity failure must precede process preparation"),
        )
        .unwrap_err();

        assert_eq!(error.message, "injected capacity failure");
        assert!(error.backups.is_empty());
        assert_eq!(fs::read_dir(backup.path()).unwrap().count(), 0);
        assert_eq!(
            fs::read(current.path().join("state_5.sqlite")).unwrap(),
            current_db_before
        );
        assert_eq!(
            fs::read(shared.path().join("state_5.sqlite")).unwrap(),
            shared_db_before
        );
        assert_eq!(fs::read(current_jsonl).unwrap(), current_jsonl_before);
        assert_eq!(fs::read(shared_jsonl).unwrap(), shared_jsonl_before);
    }

    #[test]
    fn hard_delete_prepare_failure_precedes_backups_and_live_mutation() {
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
        create_auxiliary_dbs(current.path());
        create_auxiliary_dbs(shared.path());
        fs::write(
            current.path().join("session_index.jsonl"),
            "{\"id\":\"thread-a\"}\n",
        )
        .unwrap();
        fs::write(
            shared.path().join("session_index.jsonl"),
            "{\"id\":\"thread-a\"}\n",
        )
        .unwrap();
        let current_before = snapshot_tree(current.path());
        let shared_before = snapshot_tree(shared.path());

        let error = delete_managed_sessions_with_gates_detailed(
            DeleteMutationRoots {
                current_home: current.path(),
                shared_home: shared.path(),
                backup_root: backup.path(),
            },
            &["thread-a".to_string()],
            true,
            |_, _, _, _, _| Ok(()),
            || Err("injected prepare failure".to_string()),
            |_| panic!("prepare failure must precede the final process gate"),
        )
        .unwrap_err();

        assert_eq!(error.message, "injected prepare failure");
        assert!(error.backups.is_empty());
        assert_eq!(fs::read_dir(backup.path()).unwrap().count(), 0);
        assert_eq!(snapshot_tree(current.path()), current_before);
        assert_eq!(snapshot_tree(shared.path()), shared_before);
    }

    #[test]
    fn hard_delete_final_gate_failure_keeps_two_backups_and_live_data_unchanged() {
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
        create_auxiliary_dbs(current.path());
        create_auxiliary_dbs(shared.path());
        let current_before = snapshot_tree(current.path());
        let shared_before = snapshot_tree(shared.path());

        let error = delete_managed_sessions_with_gates_detailed(
            DeleteMutationRoots {
                current_home: current.path(),
                shared_home: shared.path(),
                backup_root: backup.path(),
            },
            &["thread-a".to_string()],
            true,
            |_, _, _, _, _| Ok(()),
            || Ok(()),
            |operation| Err(format!("injected {operation} final gate failure")),
        )
        .unwrap_err();

        assert!(error.message.contains("delete final gate failure"));
        assert_eq!(error.backups.len(), 2);
        assert_eq!(fs::read_dir(backup.path()).unwrap().count(), 2);
        assert_eq!(snapshot_tree(current.path()), current_before);
        assert_eq!(snapshot_tree(shared.path()), shared_before);
    }

    #[test]
    fn hard_delete_tolerates_missing_auxiliary_tables() {
        let current = tempdir().unwrap();
        let shared = tempdir().unwrap();
        let backup = tempdir().unwrap();
        let current_jsonl = write_jsonl(current.path(), "thread-a");
        create_db(
            &current.path().join("state_5.sqlite"),
            &[("thread-a", current_jsonl.to_str().unwrap(), 1000, 0, "A")],
        );
        for database in ["goals_1.sqlite", "memories_1.sqlite", "logs_2.sqlite"] {
            Connection::open(current.path().join(database))
                .unwrap()
                .execute("CREATE TABLE unrelated (value TEXT)", [])
                .unwrap();
        }

        let result = delete_managed_sessions(
            current.path(),
            shared.path(),
            backup.path(),
            &["thread-a".to_string()],
            true,
        )
        .unwrap();

        assert_eq!(result.deleted_threads, 1);
        assert!(!current_jsonl.exists());
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

        let result =
            restore_sessions_visible(current.path(), backup.path(), &["thread-a".to_string()])
                .unwrap();

        assert_eq!(result.backups.len(), 1);
        assert_eq!(result.backups[0].scope, BackupScope::StateOnly);
        assert_eq!(result.backups[0].tracked_databases, vec!["state_5.sqlite"]);
        assert!(result.backups[0]
            .files
            .iter()
            .all(|file| { file.relative_path == std::path::Path::new("state_5.sqlite") }));
        assert_eq!(result.restored_threads, 1);
        let conn = Connection::open(current.path().join("state_5.sqlite")).unwrap();
        let archived: i64 = conn
            .query_row(
                "SELECT archived FROM threads WHERE id = 'thread-a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(archived, 0);
    }

    #[test]
    fn restore_visibility_capacity_failure_precedes_backup_and_live_mutation() {
        let current = tempdir().unwrap();
        let backup = tempdir().unwrap();
        let current_jsonl = write_jsonl(current.path(), "thread-a");
        create_db(
            &current.path().join("state_5.sqlite"),
            &[("thread-a", current_jsonl.to_str().unwrap(), 1000, 1, "A")],
        );
        let database = current.path().join("state_5.sqlite");
        let database_before = fs::read(&database).unwrap();
        let jsonl_before = fs::read(&current_jsonl).unwrap();

        let error = restore_sessions_visible_with_capacity_preflight_detailed(
            current.path(),
            backup.path(),
            &["thread-a".to_string()],
            |_, _, _| Err("injected capacity failure".to_string()),
            || panic!("capacity failure must precede process preparation"),
        )
        .unwrap_err();

        assert_eq!(error.message, "injected capacity failure");
        assert!(error.backups.is_empty());
        assert_eq!(fs::read_dir(backup.path()).unwrap().count(), 0);
        assert_eq!(fs::read(database).unwrap(), database_before);
        assert_eq!(fs::read(current_jsonl).unwrap(), jsonl_before);
    }

    #[test]
    fn restore_visibility_prepare_failure_precedes_backup_and_live_mutation() {
        let current = tempdir().unwrap();
        let backup = tempdir().unwrap();
        let current_jsonl = write_jsonl(current.path(), "thread-a");
        create_db(
            &current.path().join("state_5.sqlite"),
            &[("thread-a", current_jsonl.to_str().unwrap(), 1000, 1, "A")],
        );
        let current_before = snapshot_tree(current.path());

        let error = restore_sessions_visible_with_gates_detailed(
            current.path(),
            backup.path(),
            &["thread-a".to_string()],
            |_, _, _| Ok(()),
            || Err("injected prepare failure".to_string()),
            |_| panic!("prepare failure must precede the final process gate"),
        )
        .unwrap_err();

        assert_eq!(error.message, "injected prepare failure");
        assert!(error.backups.is_empty());
        assert_eq!(fs::read_dir(backup.path()).unwrap().count(), 0);
        assert_eq!(snapshot_tree(current.path()), current_before);
    }

    #[test]
    fn restore_visibility_final_gate_failure_keeps_backup_and_live_data_unchanged() {
        let current = tempdir().unwrap();
        let backup = tempdir().unwrap();
        let current_jsonl = write_jsonl(current.path(), "thread-a");
        create_db(
            &current.path().join("state_5.sqlite"),
            &[("thread-a", current_jsonl.to_str().unwrap(), 1000, 1, "A")],
        );
        let current_before = snapshot_tree(current.path());

        let error = restore_sessions_visible_with_gates_detailed(
            current.path(),
            backup.path(),
            &["thread-a".to_string()],
            |_, _, _| Ok(()),
            || Ok(()),
            |operation| Err(format!("injected {operation} final gate failure")),
        )
        .unwrap_err();

        assert!(error
            .message
            .contains("visibility restore final gate failure"));
        assert_eq!(error.backups.len(), 1);
        assert_eq!(fs::read_dir(backup.path()).unwrap().count(), 1);
        assert_eq!(snapshot_tree(current.path()), current_before);
    }

    #[test]
    fn empty_ids_skip_path_and_capacity_preflight_for_both_mutations() {
        let relative = std::path::Path::new("must-not-be-resolved");
        let ids = vec![" ".to_string(), String::new()];

        let deleted = delete_managed_sessions_with_capacity_preflight_detailed(
            relative,
            relative,
            relative,
            &ids,
            true,
            |_, _, _, _, _| panic!("empty delete selection must not query capacity"),
            || panic!("empty delete selection must not prepare processes"),
        )
        .unwrap();
        let restored = restore_sessions_visible_with_capacity_preflight_detailed(
            relative,
            relative,
            &ids,
            |_, _, _| panic!("empty visibility selection must not query capacity"),
            || panic!("empty visibility selection must not prepare processes"),
        )
        .unwrap();

        assert_eq!(deleted, empty_result(0));
        assert_eq!(restored, empty_result(0));
    }

    #[test]
    fn final_write_preflight_rejects_managed_and_standalone_process_restarts() {
        assert!(ensure_closed_process_presence("delete", false, false).is_ok());

        let managed = ensure_closed_process_presence("delete", true, false).unwrap_err();
        let standalone = ensure_closed_process_presence("delete", false, true).unwrap_err();
        let both = ensure_closed_process_presence("delete", true, true).unwrap_err();

        assert!(managed.contains("ChatGPT started"), "{managed}");
        assert!(standalone.contains("standalone Codex CLI"), "{standalone}");
        assert!(
            both.contains("ChatGPT and a standalone Codex CLI"),
            "{both}"
        );
    }

    #[test]
    fn archived_sessions_also_require_hard_delete_confirmation() {
        let current = tempdir().unwrap();
        let shared = tempdir().unwrap();
        let backup = tempdir().unwrap();
        let current_jsonl = write_jsonl(current.path(), "thread-a");
        create_db(
            &current.path().join("state_5.sqlite"),
            &[("thread-a", current_jsonl.to_str().unwrap(), 1000, 1, "A")],
        );

        let error = delete_managed_sessions(
            current.path(),
            shared.path(),
            backup.path(),
            &["thread-a".to_string()],
            false,
        )
        .unwrap_err();

        assert!(error.contains("确认"));
        assert!(current_jsonl.exists());
    }

    #[test]
    fn database_delete_rolls_back_dependent_rows_when_thread_delete_fails() {
        let home = tempdir().unwrap();
        let db = home.path().join("state_5.sqlite");
        let conn = Connection::open(&db).unwrap();
        conn.execute("CREATE TABLE threads (id TEXT PRIMARY KEY)", [])
            .unwrap();
        conn.execute(
            "CREATE TABLE thread_goals (thread_id TEXT, goal_id TEXT)",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO threads VALUES ('thread-a')", [])
            .unwrap();
        conn.execute("INSERT INTO thread_goals VALUES ('thread-a', 'goal-a')", [])
            .unwrap();
        conn.execute_batch(
            "CREATE TRIGGER reject_thread_delete BEFORE DELETE ON threads BEGIN SELECT RAISE(ABORT, 'stop'); END;",
        )
        .unwrap();
        drop(conn);
        let ids = ["thread-a".to_string()].into_iter().collect();

        assert!(delete_db_rows(&db, &ids).is_err());

        let conn = Connection::open(&db).unwrap();
        let goals: i64 = conn
            .query_row("SELECT COUNT(*) FROM thread_goals", [], |row| row.get(0))
            .unwrap();
        assert_eq!(goals, 1);
    }

    #[test]
    fn database_delete_rejects_ignored_thread_deletes_and_rolls_back_dependent_rows() {
        let home = tempdir().unwrap();
        let db = home.path().join("state_5.sqlite");
        let conn = Connection::open(&db).unwrap();
        conn.execute("CREATE TABLE threads (id TEXT PRIMARY KEY)", [])
            .unwrap();
        conn.execute(
            "CREATE TABLE thread_goals (thread_id TEXT, goal_id TEXT)",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO threads VALUES ('thread-a')", [])
            .unwrap();
        conn.execute("INSERT INTO thread_goals VALUES ('thread-a', 'goal-a')", [])
            .unwrap();
        conn.execute_batch(
            "CREATE TRIGGER ignore_thread_delete BEFORE DELETE ON threads BEGIN SELECT RAISE(IGNORE); END;",
        )
        .unwrap();
        drop(conn);
        let ids = ["thread-a".to_string()].into_iter().collect();

        let error = delete_db_rows(&db, &ids).unwrap_err();

        assert!(error.contains("delete write set"), "{error}");
        let conn = Connection::open(&db).unwrap();
        let threads: i64 = conn
            .query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0))
            .unwrap();
        let goals: i64 = conn
            .query_row("SELECT COUNT(*) FROM thread_goals", [], |row| row.get(0))
            .unwrap();
        assert_eq!(threads, 1);
        assert_eq!(goals, 1);
    }

    #[test]
    fn database_delete_rejects_trigger_deletion_outside_the_selected_write_set() {
        let home = tempdir().unwrap();
        let db = home.path().join("state_5.sqlite");
        let conn = Connection::open(&db).unwrap();
        conn.execute("CREATE TABLE threads (id TEXT PRIMARY KEY)", [])
            .unwrap();
        conn.execute("INSERT INTO threads VALUES ('thread-a')", [])
            .unwrap();
        conn.execute("INSERT INTO threads VALUES ('thread-b')", [])
            .unwrap();
        conn.execute_batch(
            "CREATE TRIGGER delete_other_thread BEFORE DELETE ON threads
             WHEN OLD.id = 'thread-a'
             BEGIN DELETE FROM threads WHERE id = 'thread-b'; END;",
        )
        .unwrap();
        drop(conn);
        let ids = ["thread-a".to_string()].into_iter().collect();

        let error = delete_db_rows(&db, &ids).unwrap_err();

        assert!(error.contains("delete write set"), "{error}");
        let conn = Connection::open(&db).unwrap();
        let mut statement = conn.prepare("SELECT id FROM threads ORDER BY id").unwrap();
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(ids, vec!["thread-a", "thread-b"]);
    }

    #[test]
    fn auxiliary_delete_rejects_trigger_deletion_outside_the_selected_write_set() {
        let home = tempdir().unwrap();
        let db = home.path().join("memories_1.sqlite");
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "CREATE TABLE stage1_outputs (thread_id TEXT PRIMARY KEY, raw_memory TEXT)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO stage1_outputs VALUES ('thread-a', 'a'), ('thread-b', 'b')",
            [],
        )
        .unwrap();
        conn.execute_batch(
            "CREATE TRIGGER delete_other_memory BEFORE DELETE ON stage1_outputs
             WHEN OLD.thread_id = 'thread-a'
             BEGIN DELETE FROM stage1_outputs WHERE thread_id = 'thread-b'; END;",
        )
        .unwrap();
        drop(conn);
        let ids = ["thread-a".to_string()].into_iter().collect();

        let error = delete_rows_in_database(
            &db,
            "memories_1.sqlite",
            &[("stage1_outputs", "thread_id")],
            &ids,
        )
        .unwrap_err();

        assert!(error.contains("delete write set"), "{error}");
        let conn = Connection::open(&db).unwrap();
        let mut statement = conn
            .prepare("SELECT thread_id FROM stage1_outputs ORDER BY thread_id")
            .unwrap();
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(ids, vec!["thread-a", "thread-b"]);
    }

    #[test]
    fn partial_cross_root_delete_restores_both_verified_backups() {
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
        create_auxiliary_dbs(current.path());

        let error = delete_managed_sessions_with_failure_detailed(
            current.path(),
            shared.path(),
            backup.path(),
            &["thread-a".to_string()],
            true,
            DeleteFailurePoint::AfterCurrent,
        )
        .unwrap_err();

        assert!(error.message.contains("rolled back"));
        assert_eq!(error.backups.len(), 2);
        assert!(current_jsonl.exists());
        assert!(shared_jsonl.exists());
        let current_conn = Connection::open(current.path().join("state_5.sqlite")).unwrap();
        let count: i64 = current_conn
            .query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            count_rows(
                &current.path().join("memories_1.sqlite"),
                "stage1_outputs",
                "thread-a",
            ),
            1
        );
    }

    #[test]
    fn auxiliary_database_failure_restores_state_auxiliary_files_and_index() {
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
        create_auxiliary_dbs(current.path());
        create_auxiliary_dbs(shared.path());
        fs::write(
            current.path().join("session_index.jsonl"),
            "{\"id\":\"thread-a\"}\n",
        )
        .unwrap();
        Connection::open(current.path().join("memories_1.sqlite"))
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER reject_memory_delete
                 BEFORE DELETE ON stage1_outputs
                 WHEN OLD.thread_id = 'thread-a'
                 BEGIN SELECT RAISE(ABORT, 'stop'); END;",
            )
            .unwrap();

        let error = delete_managed_sessions(
            current.path(),
            shared.path(),
            backup.path(),
            &["thread-a".to_string()],
            true,
        )
        .unwrap_err();

        assert!(error.contains("rolled back both roots"));
        assert!(current_jsonl.exists());
        assert!(shared_jsonl.exists());
        assert!(
            fs::read_to_string(current.path().join("session_index.jsonl"))
                .unwrap()
                .contains("thread-a")
        );
        for home in [current.path(), shared.path()] {
            let threads: i64 = Connection::open(home.join("state_5.sqlite"))
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM threads WHERE id = 'thread-a'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(threads, 1);
            for (database, table) in [
                ("goals_1.sqlite", "thread_goals"),
                ("memories_1.sqlite", "stage1_outputs"),
                ("logs_2.sqlite", "logs"),
            ] {
                assert_eq!(count_rows(&home.join(database), table, "thread-a"), 1);
            }
        }
    }

    #[test]
    fn visibility_failure_preserves_the_created_backup_manifest() {
        let current = tempdir().unwrap();
        let backup = tempdir().unwrap();
        let jsonl = write_jsonl(current.path(), "thread-a");
        create_db(
            &current.path().join("state_5.sqlite"),
            &[("thread-a", jsonl.to_str().unwrap(), 1000, 1, "A")],
        );
        let conn = Connection::open(current.path().join("state_5.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER reject_visibility BEFORE UPDATE OF archived ON threads BEGIN SELECT RAISE(ABORT, 'stop'); END;",
        )
        .unwrap();
        drop(conn);

        let error = restore_sessions_visible_detailed(
            current.path(),
            backup.path(),
            &["thread-a".to_string()],
        )
        .unwrap_err();

        assert!(error.message.contains("failed to restore visible sessions"));
        assert_eq!(error.backups.len(), 1);
        assert!(error.backups[0].backup_dir.exists());
    }

    #[test]
    fn visibility_restore_rejects_an_ignored_update() {
        let current = tempdir().unwrap();
        let current_jsonl = write_jsonl(current.path(), "thread-a");
        let database = current.path().join("state_5.sqlite");
        create_db(
            &database,
            &[("thread-a", current_jsonl.to_str().unwrap(), 1000, 1, "A")],
        );
        Connection::open(&database)
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER ignore_visibility
                 BEFORE UPDATE OF archived ON threads
                 BEGIN SELECT RAISE(IGNORE); END;",
            )
            .unwrap();
        let ids = ["thread-a".to_string()].into_iter().collect();

        let error = restore_visible_in_db(&database, &ids).unwrap_err();

        assert!(error.contains("failed to verify restored visible sessions"));
        let conn = Connection::open(&database).unwrap();
        let (archived, archived_at): (i64, Option<i64>) = conn
            .query_row(
                "SELECT archived, archived_at FROM threads WHERE id = 'thread-a'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(archived, 1);
        assert_eq!(archived_at, Some(123));
    }

    #[test]
    fn visibility_restore_rejects_trigger_deletion_of_the_selected_thread() {
        let current = tempdir().unwrap();
        let current_jsonl = write_jsonl(current.path(), "thread-a");
        let database = current.path().join("state_5.sqlite");
        create_db(
            &database,
            &[("thread-a", current_jsonl.to_str().unwrap(), 1000, 1, "A")],
        );
        Connection::open(&database)
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER delete_visible_thread
                 AFTER UPDATE OF archived ON threads
                 BEGIN DELETE FROM threads WHERE id = NEW.id; END;",
            )
            .unwrap();
        let ids = ["thread-a".to_string()].into_iter().collect();

        let error = restore_visible_in_db(&database, &ids).unwrap_err();

        assert!(error.contains("failed to verify restored visible sessions"));
        let count: i64 = Connection::open(&database)
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM threads WHERE id = 'thread-a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn visibility_restore_rejects_trigger_mutation_of_an_unselected_thread() {
        let current = tempdir().unwrap();
        let thread_a_jsonl = write_jsonl(current.path(), "thread-a");
        let thread_b_jsonl = write_jsonl(current.path(), "thread-b");
        let database = current.path().join("state_5.sqlite");
        create_db(
            &database,
            &[
                ("thread-a", thread_a_jsonl.to_str().unwrap(), 1000, 1, "A"),
                ("thread-b", thread_b_jsonl.to_str().unwrap(), 1000, 0, "B"),
            ],
        );
        Connection::open(&database)
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER mutate_unselected_thread
                 AFTER UPDATE OF archived ON threads
                 WHEN NEW.id = 'thread-a'
                 BEGIN UPDATE threads SET title = 'changed' WHERE id = 'thread-b'; END;",
            )
            .unwrap();
        let ids = ["thread-a".to_string()].into_iter().collect();

        let error = restore_visible_in_db(&database, &ids).unwrap_err();

        assert!(error.contains("failed to verify restored visible sessions"));
        let conn = Connection::open(&database).unwrap();
        let title: String = conn
            .query_row(
                "SELECT title FROM threads WHERE id = 'thread-b'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let archived: i64 = conn
            .query_row(
                "SELECT archived FROM threads WHERE id = 'thread-a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(title, "B");
        assert_eq!(archived, 1);
    }

    #[test]
    fn visibility_restore_rejects_trigger_mutation_of_a_protected_selected_column() {
        let current = tempdir().unwrap();
        let current_jsonl = write_jsonl(current.path(), "thread-a");
        let database = current.path().join("state_5.sqlite");
        create_db(
            &database,
            &[("thread-a", current_jsonl.to_str().unwrap(), 1000, 1, "A")],
        );
        Connection::open(&database)
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER mutate_selected_title
                 AFTER UPDATE OF archived ON threads
                 WHEN NEW.id = 'thread-a'
                 BEGIN UPDATE threads SET title = 'changed' WHERE id = NEW.id; END;",
            )
            .unwrap();
        let ids = ["thread-a".to_string()].into_iter().collect();

        let error = restore_visible_in_db(&database, &ids).unwrap_err();

        assert!(error.contains("failed to verify restored visible sessions"));
        let (title, archived): (String, i64) = Connection::open(&database)
            .unwrap()
            .query_row(
                "SELECT title, archived FROM threads WHERE id = 'thread-a'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(title, "A");
        assert_eq!(archived, 1);
    }

    #[test]
    fn visibility_restore_rejects_non_null_archived_timestamp_after_update() {
        let current = tempdir().unwrap();
        let current_jsonl = write_jsonl(current.path(), "thread-a");
        let database = current.path().join("state_5.sqlite");
        create_db(
            &database,
            &[("thread-a", current_jsonl.to_str().unwrap(), 1000, 1, "A")],
        );
        Connection::open(&database)
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER retain_archived_timestamp
                 AFTER UPDATE OF archived ON threads
                 BEGIN UPDATE threads SET archived_at = 999 WHERE id = NEW.id; END;",
            )
            .unwrap();
        let ids = ["thread-a".to_string()].into_iter().collect();

        let error = restore_visible_in_db(&database, &ids).unwrap_err();

        assert!(error.contains("failed to verify restored visible sessions"));
        let (archived, archived_at): (i64, Option<i64>) = Connection::open(&database)
            .unwrap()
            .query_row(
                "SELECT archived, archived_at FROM threads WHERE id = 'thread-a'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(archived, 1);
        assert_eq!(archived_at, Some(123));
    }
}

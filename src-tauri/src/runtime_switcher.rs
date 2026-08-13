use std::path::PathBuf;

#[cfg(test)]
use std::{fs, path::Path, time::Duration};

#[cfg(test)]
use rusqlite::{Connection, OpenFlags, MAIN_DB};
use serde::Serialize;

use crate::{
    backup::{BackupManifest, BackupScope},
    process_control::{
        ChatGptLaunchResult as ProcessChatGptLaunchResult,
        ChatGptLaunchStatus as ProcessChatGptLaunchStatus,
    },
    runtime_store::RuntimeMetadata,
    session_incremental::IncrementalSessionSyncReceipt,
    session_storage::provenance::RouteProvenanceReceipt,
};

#[cfg(test)]
use crate::{
    codex_paths::CodexPaths,
    session_sync::{sync_user_home_to_shared_with_paths, SessionSyncResult},
};

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ChatGptLaunchStatus {
    Launched,
    AlreadyRunning,
    Failed,
    Blocked,
    NotRequested,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChatGptLaunchReceipt {
    pub status: ChatGptLaunchStatus,
    pub message: Option<String>,
}

impl ChatGptLaunchReceipt {
    pub(crate) fn not_requested() -> Self {
        Self {
            status: ChatGptLaunchStatus::NotRequested,
            message: None,
        }
    }
}

impl From<ProcessChatGptLaunchResult> for ChatGptLaunchReceipt {
    fn from(result: ProcessChatGptLaunchResult) -> Self {
        let status = match result.status {
            ProcessChatGptLaunchStatus::Launched => ChatGptLaunchStatus::Launched,
            ProcessChatGptLaunchStatus::AlreadyRunning => ChatGptLaunchStatus::AlreadyRunning,
            ProcessChatGptLaunchStatus::Failed => ChatGptLaunchStatus::Failed,
        };
        Self {
            status,
            message: result.message,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BackupReceiptSummary {
    pub backup_dir: PathBuf,
    pub source_root: PathBuf,
    pub reason: String,
    pub created_at_ms: u128,
    pub scope: BackupScope,
    pub tracked_database_count: usize,
    pub complete_sessions: bool,
}

impl From<&BackupManifest> for BackupReceiptSummary {
    fn from(manifest: &BackupManifest) -> Self {
        Self {
            backup_dir: manifest.backup_dir.clone(),
            source_root: manifest.source_root.clone(),
            reason: manifest.reason.clone(),
            created_at_ms: manifest.created_at_ms,
            scope: manifest.scope,
            tracked_database_count: manifest.tracked_databases.len(),
            complete_sessions: manifest.complete_sessions,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeSwitchResult {
    pub operation_id: String,
    pub changed: bool,
    pub runtime: RuntimeMetadata,
    pub warnings: Vec<String>,
    pub incremental_session_sync: IncrementalSessionSyncReceipt,
    pub route_provenance: RouteProvenanceReceipt,
    pub relay_validation: RelayValidationStatus,
    pub chat_process_state_repaired: bool,
    pub chatgpt_launch: ChatGptLaunchReceipt,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RelayValidationStatus {
    NotApplicable,
    Verified,
    Skipped,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RuntimeSwitchPhase {
    LoadingRuntime,
    ValidatingOfficialAuth,
    VerifyingRelay,
    DetectingApp,
    ClosingApp,
    PreparingRuntime,
    RepairingAppState,
    ApplyingRuntime,
    Verifying,
    RecordingResult,
    SyncingIncrementalSessions,
    RollingBack,
    LaunchingApp,
    Complete,
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RuntimeSwitchOutcome {
    FailedBeforeWrite,
    RolledBack,
    RollbackFailed,
}

#[derive(Debug, Clone)]
pub struct RuntimeSwitchFailure {
    pub message: String,
    pub outcome: RuntimeSwitchOutcome,
    pub operation_id: Option<String>,
}

#[cfg(test)]
pub(crate) fn sync_home_with_shared_complete_with_paths(
    current_paths: &CodexPaths,
    shared_paths: &CodexPaths,
) -> Result<SessionSyncResult, String> {
    ensure_shared_sessions_with_paths(current_paths, shared_paths)?;
    let to_shared = sync_user_home_to_shared_with_paths(current_paths, shared_paths)?;
    let from_shared = crate::session_sync::sync_shared_to_user_home_with_paths(
        shared_paths,
        current_paths,
        "openai",
    )?;
    combine_session_sync_results(to_shared, from_shared)
}

#[cfg(test)]
pub(crate) fn combine_session_sync_results(
    to_shared: SessionSyncResult,
    from_shared: SessionSyncResult,
) -> Result<SessionSyncResult, String> {
    let persistent_session_bytes_added = to_shared
        .persistent_session_bytes_added
        .checked_add(from_shared.persistent_session_bytes_added)
        .ok_or_else(|| "session storage accounting overflowed".to_string())?;
    let persistent_session_bytes_reclaimed = to_shared
        .persistent_session_bytes_reclaimed
        .checked_add(from_shared.persistent_session_bytes_reclaimed)
        .ok_or_else(|| "session storage accounting overflowed".to_string())?;
    let mut obsolete_provider_slots = to_shared.obsolete_provider_slots.clone();
    obsolete_provider_slots.extend(from_shared.obsolete_provider_slots.clone());
    let mut preserved_divergent_thread_ids = to_shared.preserved_divergent_thread_ids.clone();
    preserved_divergent_thread_ids
        .extend(from_shared.preserved_divergent_thread_ids.iter().cloned());
    Ok(SessionSyncResult {
        inserted_threads: to_shared.inserted_threads + from_shared.inserted_threads,
        copied_session_files: to_shared.copied_session_files + from_shared.copied_session_files,
        duplicate_threads: to_shared.duplicate_threads + from_shared.duplicate_threads,
        skipped_missing_session_files: to_shared.skipped_missing_session_files
            + from_shared.skipped_missing_session_files,
        skipped_archived_threads: to_shared.skipped_archived_threads
            + from_shared.skipped_archived_threads,
        merged_session_index_entries: to_shared.merged_session_index_entries
            + from_shared.merged_session_index_entries,
        persistent_session_bytes_added,
        persistent_session_bytes_reclaimed,
        obsolete_provider_slots,
        preserved_divergent_thread_ids,
    })
}

#[cfg(test)]
fn ensure_shared_sessions_with_paths(
    current_paths: &CodexPaths,
    shared_paths: &CodexPaths,
) -> Result<(), String> {
    fs::create_dir_all(&shared_paths.codex_home)
        .map_err(|error| format!("failed to create shared sessions dir: {error}"))?;
    let shared_db = &shared_paths.state_db;
    if !shared_db.exists() {
        let source_db = &current_paths.state_db;
        if !source_db.exists() {
            return Err("state_5.sqlite is required before syncing shared sessions".to_string());
        }
        initialize_shared_database(source_db, shared_db)?;
    }
    Ok(())
}

#[cfg(test)]
fn initialize_shared_database(source: &Path, target: &Path) -> Result<(), String> {
    let source_conn = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| format!("failed to open source state_5.sqlite: {error}"))?;
    source_conn
        .backup(MAIN_DB, target, None)
        .map_err(|error| format!("failed to initialize shared state_5.sqlite: {error}"))?;
    let conn = Connection::open(target)
        .map_err(|error| format!("failed to open initialized shared state_5.sqlite: {error}"))?;
    conn.busy_timeout(Duration::from_secs(2))
        .map_err(|error| format!("failed to set shared SQLite timeout: {error}"))?;
    conn.execute_batch("BEGIN IMMEDIATE")
        .map_err(|error| format!("failed to start shared initialization transaction: {error}"))?;
    let cleared = (|| {
        for table in [
            "thread_dynamic_tools",
            "thread_goals",
            "thread_spawn_edges",
            "threads",
        ] {
            if sqlite_table_exists(&conn, table)? {
                conn.execute(&format!("DELETE FROM {table}"), [])
                    .map_err(|error| format!("failed to clear shared table {table}: {error}"))?;
            }
        }
        Ok::<_, String>(())
    })();
    if let Err(error) = cleared {
        let _ = conn.execute_batch("ROLLBACK");
        let _ = fs::remove_file(target);
        return Err(error);
    }
    conn.execute_batch("COMMIT")
        .map_err(|error| format!("failed to commit shared initialization: {error}"))
}

#[cfg(test)]
fn sqlite_table_exists(conn: &Connection, table: &str) -> Result<bool, String> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |row| row.get(0),
        )
        .map_err(|error| format!("failed to inspect SQLite schema: {error}"))?;
    Ok(count > 0)
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use rusqlite::Connection;
    use tempfile::tempdir;

    use super::sync_home_with_shared_complete_with_paths;
    use crate::codex_paths::local_codex_paths;

    fn create_session_root(
        root: &Path,
        threads: &[(&str, &str, bool)],
    ) -> crate::codex_paths::CodexPaths {
        let paths = local_codex_paths(root);
        fs::create_dir_all(&paths.sessions_dir).unwrap();
        let conn = Connection::open(&paths.state_db).unwrap();
        conn.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                rollout_path TEXT NOT NULL,
                model_provider TEXT NOT NULL,
                archived INTEGER NOT NULL DEFAULT 0
            );",
        )
        .unwrap();
        for (id, provider, archived) in threads {
            let rollout_dir = paths.sessions_dir.join("2026").join("07").join("28");
            fs::create_dir_all(&rollout_dir).unwrap();
            let rollout = rollout_dir.join(format!("rollout-2026-07-28T12-00-00-{id}.jsonl"));
            fs::write(
                &rollout,
                format!(
                    "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"model_provider\":\"{provider}\"}}}}\n"
                ),
            )
            .unwrap();
            conn.execute(
                "INSERT INTO threads (id, rollout_path, model_provider, archived)
                 VALUES (?1, ?2, ?3, ?4)",
                (
                    id,
                    rollout.to_string_lossy().to_string(),
                    provider,
                    i64::from(*archived),
                ),
            )
            .unwrap();
        }
        paths
    }

    #[test]
    fn manual_complete_sync_reconciles_active_threads_and_leaves_archives_untouched() {
        let current = tempdir().unwrap();
        let shared = tempdir().unwrap();
        let current_paths = create_session_root(
            current.path(),
            &[
                ("019fa68f-dd42-76b3-8299-84a865ab5511", "openai", false),
                ("019fa68f-dd42-76b3-8299-84a865ab5512", "openai", true),
            ],
        );
        let shared_paths = create_session_root(
            shared.path(),
            &[(
                "019fa68f-dd42-76b3-8299-84a865ab5513",
                "openai_custom",
                false,
            )],
        );
        fs::create_dir_all(&current_paths.archived_sessions_dir).unwrap();
        let archived_file = current_paths.archived_sessions_dir.join("keep.jsonl");
        fs::write(&archived_file, b"archive-must-not-change\n").unwrap();

        sync_home_with_shared_complete_with_paths(&current_paths, &shared_paths).unwrap();

        assert_eq!(
            fs::read(&archived_file).unwrap(),
            b"archive-must-not-change\n"
        );
        let current_conn = Connection::open(&current_paths.state_db).unwrap();
        let active_count: i64 = current_conn
            .query_row(
                "SELECT COUNT(*) FROM threads WHERE archived = 0",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let archived_count: i64 = current_conn
            .query_row(
                "SELECT COUNT(*) FROM threads WHERE archived != 0",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let imported: (String, String) = current_conn
            .query_row(
                "SELECT model_provider, rollout_path FROM threads WHERE id = ?1",
                ["019fa68f-dd42-76b3-8299-84a865ab5513"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((active_count, archived_count), (2, 1));
        assert_eq!(imported.0, "openai");
        assert!(fs::read_to_string(imported.1)
            .unwrap()
            .contains(r#""model_provider":"openai""#));
        let shared_conn = Connection::open(&shared_paths.state_db).unwrap();
        let archived_in_shared: i64 = shared_conn
            .query_row(
                "SELECT COUNT(*) FROM threads WHERE id = ?1",
                ["019fa68f-dd42-76b3-8299-84a865ab5512"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(archived_in_shared, 0);
    }
}

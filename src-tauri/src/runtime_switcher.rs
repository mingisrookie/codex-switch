use std::{
    fs,
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

use rusqlite::{Connection, OpenFlags, MAIN_DB};
use serde::Serialize;
use toml_edit::DocumentMut;

use crate::{
    backup::{create_local_session_backup, create_runtime_backup, restore_backup, BackupManifest},
    codex_paths::{local_codex_paths, resolve_user_codex_paths},
    config_patch::{plan_runtime_config_patch, ConfigPatchPlan, RuntimeConfigKind},
    file_ops::atomic_write,
    operation_log::operation_id,
    runtime_store::{RuntimeConfidence, RuntimeFiles, RuntimeKind, RuntimeMetadata, RuntimeStore},
    session_sync::{
        preflight_session_database, sync_shared_to_user_home, sync_shared_to_user_home_hot,
        sync_user_home_to_shared, SessionSyncResult,
    },
};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeSwitchResult {
    pub operation_id: String,
    pub changed: bool,
    pub runtime: RuntimeMetadata,
    pub backups: Vec<BackupManifest>,
    pub to_shared: SessionSyncResult,
    pub from_shared: SessionSyncResult,
    pub rolled_back: bool,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RuntimeSwitchPhase {
    DetectingApp,
    ClosingApp,
    VerifyingRelay,
    BackingUpCurrent,
    BackingUpShared,
    SyncingToShared,
    ApplyingRuntime,
    SyncingToCurrent,
    Verifying,
    RollingBack,
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
    pub backups: Vec<BackupManifest>,
    pub outcome: RuntimeSwitchOutcome,
}

pub(crate) struct RuntimeSwitchPlan {
    operation_id: String,
    runtime_files: RuntimeFiles,
    runtime: RuntimeMetadata,
    config_plan: ConfigPatchPlan,
    session_provider: String,
    sqlite_home: PathBuf,
    requires_change: bool,
}

impl RuntimeSwitchPlan {
    pub(crate) fn requires_change(&self) -> bool {
        self.requires_change
    }
}

impl RuntimeSwitchFailure {
    fn new(message: String, backups: Vec<BackupManifest>) -> Self {
        Self {
            message,
            backups,
            outcome: RuntimeSwitchOutcome::FailedBeforeWrite,
        }
    }

    fn before_backup(message: String) -> Self {
        Self::new(message, Vec::new())
    }

    fn rolled_back(message: String, backups: Vec<BackupManifest>) -> Self {
        Self {
            message,
            backups,
            outcome: RuntimeSwitchOutcome::RolledBack,
        }
    }

    fn rollback_failed(message: String, backups: Vec<BackupManifest>) -> Self {
        Self {
            message,
            backups,
            outcome: RuntimeSwitchOutcome::RollbackFailed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchFailurePoint {
    None,
    AfterRuntimeFiles,
    #[cfg(test)]
    AfterRuntimeFilesWithCorruptCurrentBackup,
}

pub fn switch_runtime_files(
    store: &RuntimeStore,
    runtime_id: &str,
    codex_home: &Path,
    backup_root: &Path,
    shared_home: &Path,
) -> Result<RuntimeSwitchResult, String> {
    switch_runtime_files_detailed(store, runtime_id, codex_home, backup_root, shared_home)
        .map_err(|error| error.message)
}

pub fn switch_runtime_files_detailed(
    store: &RuntimeStore,
    runtime_id: &str,
    codex_home: &Path,
    backup_root: &Path,
    shared_home: &Path,
) -> Result<RuntimeSwitchResult, RuntimeSwitchFailure> {
    switch_runtime_files_detailed_with_progress(
        store,
        runtime_id,
        codex_home,
        backup_root,
        shared_home,
        &mut |_| {},
    )
}

pub fn switch_runtime_files_detailed_with_progress(
    store: &RuntimeStore,
    runtime_id: &str,
    codex_home: &Path,
    backup_root: &Path,
    shared_home: &Path,
    on_progress: &mut dyn FnMut(RuntimeSwitchPhase),
) -> Result<RuntimeSwitchResult, RuntimeSwitchFailure> {
    let plan = build_runtime_switch_plan(store, runtime_id, codex_home)
        .map_err(RuntimeSwitchFailure::before_backup)?;
    switch_runtime_files_from_plan(
        store,
        codex_home,
        backup_root,
        shared_home,
        SwitchFailurePoint::None,
        plan,
        on_progress,
    )
}

pub(crate) fn preflight_runtime_switch(
    store: &RuntimeStore,
    runtime_id: &str,
    codex_home: &Path,
) -> Result<RuntimeSwitchPlan, String> {
    build_runtime_switch_plan(store, runtime_id, codex_home)
}

pub(crate) fn preflight_runtime_session_sync(
    codex_home: &Path,
    shared_home: &Path,
) -> Result<(), String> {
    let current = resolve_user_codex_paths(codex_home)?;
    let shared = local_codex_paths(shared_home);
    if !current.state_db.is_file() {
        return Err("state_5.sqlite is required before switching runtimes".to_string());
    }
    preflight_session_database(&current.state_db, "current")?;
    if shared.state_db.exists() {
        preflight_session_database(&shared.state_db, "shared")?;
    }
    Ok(())
}

pub(crate) fn switch_runtime_files_preflighted_with_progress(
    store: &RuntimeStore,
    codex_home: &Path,
    backup_root: &Path,
    shared_home: &Path,
    plan: RuntimeSwitchPlan,
    on_progress: &mut dyn FnMut(RuntimeSwitchPhase),
) -> Result<RuntimeSwitchResult, RuntimeSwitchFailure> {
    switch_runtime_files_from_plan(
        store,
        codex_home,
        backup_root,
        shared_home,
        SwitchFailurePoint::None,
        plan,
        on_progress,
    )
}

#[cfg(test)]
pub fn switch_runtime_files_with_failure_detailed(
    store: &RuntimeStore,
    runtime_id: &str,
    codex_home: &Path,
    backup_root: &Path,
    shared_home: &Path,
    failure_point: SwitchFailurePoint,
) -> Result<RuntimeSwitchResult, RuntimeSwitchFailure> {
    switch_runtime_files_with_failure_and_progress_detailed(
        store,
        runtime_id,
        codex_home,
        backup_root,
        shared_home,
        failure_point,
        &mut |_| {},
    )
}

#[cfg(test)]
fn switch_runtime_files_with_failure_and_progress_detailed(
    store: &RuntimeStore,
    runtime_id: &str,
    codex_home: &Path,
    backup_root: &Path,
    shared_home: &Path,
    failure_point: SwitchFailurePoint,
    on_progress: &mut dyn FnMut(RuntimeSwitchPhase),
) -> Result<RuntimeSwitchResult, RuntimeSwitchFailure> {
    let plan = build_runtime_switch_plan(store, runtime_id, codex_home)
        .map_err(RuntimeSwitchFailure::before_backup)?;
    switch_runtime_files_from_plan(
        store,
        codex_home,
        backup_root,
        shared_home,
        failure_point,
        plan,
        on_progress,
    )
}

fn switch_runtime_files_from_plan(
    store: &RuntimeStore,
    codex_home: &Path,
    backup_root: &Path,
    shared_home: &Path,
    failure_point: SwitchFailurePoint,
    mut plan: RuntimeSwitchPlan,
    on_progress: &mut dyn FnMut(RuntimeSwitchPhase),
) -> Result<RuntimeSwitchResult, RuntimeSwitchFailure> {
    let operation_id = plan.operation_id.clone();
    let runtime_id = plan.runtime.id.clone();
    if !plan.requires_change {
        let active = store
            .detect_active_runtime(codex_home)
            .map_err(RuntimeSwitchFailure::before_backup)?;
        if active.active_runtime_id.as_deref() != Some(runtime_id.as_str())
            || active.confidence != RuntimeConfidence::Exact
        {
            return Err(RuntimeSwitchFailure::before_backup(
                "runtime state changed during switch preflight; retry".to_string(),
            ));
        }
        return Ok(RuntimeSwitchResult {
            operation_id,
            changed: false,
            runtime: store
                .load_metadata(&runtime_id)
                .map_err(RuntimeSwitchFailure::before_backup)?,
            backups: Vec::new(),
            to_shared: empty_sync_result(),
            from_shared: empty_sync_result(),
            rolled_back: false,
        });
    }
    let current_paths =
        resolve_user_codex_paths(codex_home).map_err(RuntimeSwitchFailure::before_backup)?;
    if current_paths.sqlite_home != plan.sqlite_home {
        return Err(RuntimeSwitchFailure::before_backup(
            "SQLite root changed during switch preflight; retry".to_string(),
        ));
    }

    on_progress(RuntimeSwitchPhase::BackingUpCurrent);
    let current_backup = create_runtime_backup(codex_home, backup_root, "switch-runtime-current")
        .map_err(RuntimeSwitchFailure::before_backup)?;
    on_progress(RuntimeSwitchPhase::BackingUpShared);
    let shared_backup =
        create_local_session_backup(shared_home, backup_root, "switch-runtime-shared")
            .map_err(|message| RuntimeSwitchFailure::new(message, vec![current_backup.clone()]))?;
    let backups = vec![current_backup.clone(), shared_backup.clone()];

    #[cfg(not(test))]
    if !crate::process_control::list_codex_processes()
        .map_err(|message| RuntimeSwitchFailure::new(message, backups.clone()))?
        .is_empty()
    {
        return Err(RuntimeSwitchFailure::new(
            "ChatGPT started during switch preflight; close it and retry before files are changed"
                .to_string(),
            backups,
        ));
    }
    let applied = (|| {
        on_progress(RuntimeSwitchPhase::SyncingToShared);
        ensure_shared_sessions(codex_home, shared_home)?;
        let to_shared = sync_user_home_to_shared(codex_home, shared_home)?;
        let (config_plan, session_provider) =
            runtime_config_plan_for_home(&plan.runtime, &plan.runtime_files, codex_home)?;
        plan.config_plan = config_plan;
        plan.session_provider = session_provider;
        on_progress(RuntimeSwitchPhase::ApplyingRuntime);
        atomic_write(&codex_home.join("auth.json"), &plan.runtime_files.auth_json)?;
        atomic_write(
            &codex_home.join("config.toml"),
            plan.config_plan.patched_toml.as_bytes(),
        )?;
        #[cfg(test)]
        if failure_point == SwitchFailurePoint::AfterRuntimeFilesWithCorruptCurrentBackup {
            fs::write(current_backup.backup_dir.join("manifest.json"), b"not-json")
                .map_err(|error| format!("failed to corrupt current backup manifest: {error}"))?;
            return Err("injected failure after runtime files".to_string());
        }
        if failure_point == SwitchFailurePoint::AfterRuntimeFiles {
            return Err("injected failure after runtime files".to_string());
        }
        on_progress(RuntimeSwitchPhase::SyncingToCurrent);
        let from_shared =
            sync_shared_to_user_home(shared_home, codex_home, &plan.session_provider)?;
        on_progress(RuntimeSwitchPhase::Verifying);
        let verified = store.detect_active_runtime(codex_home)?;
        if verified.active_runtime_id.as_deref() != Some(runtime_id.as_str())
            || verified.confidence != RuntimeConfidence::Exact
        {
            return Err("runtime verification did not match the requested target".to_string());
        }
        let runtime = store.mark_used(&runtime_id)?;
        Ok((runtime, to_shared, from_shared))
    })();

    match applied {
        Ok((runtime, to_shared, from_shared)) => Ok(RuntimeSwitchResult {
            operation_id,
            changed: true,
            runtime,
            backups,
            to_shared,
            from_shared,
            rolled_back: false,
        }),
        Err(error) => {
            on_progress(RuntimeSwitchPhase::RollingBack);
            let current_restore = restore_backup(&current_backup.backup_dir, codex_home);
            let shared_restore = restore_backup(&shared_backup.backup_dir, shared_home);
            match (current_restore, shared_restore) {
                (Ok(_), Ok(_)) => Err(RuntimeSwitchFailure::rolled_back(
                    format!("{error}; rolled back to verified snapshots"),
                    backups,
                )),
                (current, shared) => Err(RuntimeSwitchFailure::rollback_failed(
                    format!(
                        "{error}; rollback failed (current: {}; shared: {})",
                        restore_status(current),
                        restore_status(shared)
                    ),
                    backups,
                )),
            }
        }
    }
}

fn build_runtime_switch_plan(
    store: &RuntimeStore,
    runtime_id: &str,
    codex_home: &Path,
) -> Result<RuntimeSwitchPlan, String> {
    let operation_id = operation_id("switch-runtime")?;
    let codex_paths = resolve_user_codex_paths(codex_home)?;
    let runtime_files = store.load_runtime_files(runtime_id)?;
    let runtime = store.load_metadata(runtime_id)?;
    serde_json::from_slice::<serde_json::Value>(&runtime_files.auth_json)
        .map_err(|error| format!("stored runtime auth.json is invalid: {error}"))?;
    let (config_plan, session_provider) =
        runtime_config_plan_for_home(&runtime, &runtime_files, codex_home)?;
    let active = store.detect_active_runtime(codex_home)?;
    let requires_change = active.active_runtime_id.as_deref() != Some(runtime_id)
        || active.confidence != RuntimeConfidence::Exact;
    Ok(RuntimeSwitchPlan {
        operation_id,
        runtime_files,
        runtime,
        config_plan,
        session_provider,
        sqlite_home: codex_paths.sqlite_home,
        requires_change,
    })
}

fn runtime_config_plan_for_home(
    runtime: &RuntimeMetadata,
    runtime_files: &RuntimeFiles,
    codex_home: &Path,
) -> Result<(ConfigPatchPlan, String), String> {
    let live_config = fs::read_to_string(codex_home.join("config.toml"))
        .map_err(|error| format!("failed to read live config.toml: {error}"))?;
    let config_kind = match runtime.kind {
        RuntimeKind::Plus => RuntimeConfigKind::Account,
        RuntimeKind::Relay => RuntimeConfigKind::Relay,
    };
    let config_plan =
        plan_runtime_config_patch(&live_config, &runtime_files.config_toml, config_kind)?;
    let session_provider = session_provider_from_config(&config_plan.patched_toml)?;
    Ok((config_plan, session_provider))
}

pub fn sync_home_with_shared(
    codex_home: &Path,
    shared_home: &Path,
) -> Result<SessionSyncResult, String> {
    let session_provider = session_provider_from_home(codex_home)?;
    ensure_shared_sessions(codex_home, shared_home)?;
    let to_shared = sync_user_home_to_shared(codex_home, shared_home)?;
    let from_shared = sync_shared_to_user_home_hot(shared_home, codex_home, &session_provider)?;
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
    })
}

fn ensure_shared_sessions(codex_home: &Path, shared_home: &Path) -> Result<(), String> {
    fs::create_dir_all(shared_home)
        .map_err(|error| format!("failed to create shared sessions dir: {error}"))?;
    let shared_db = shared_home.join("state_5.sqlite");
    if !shared_db.exists() {
        let source_db = resolve_user_codex_paths(codex_home)?.state_db;
        if !source_db.exists() {
            return Err("state_5.sqlite is required before syncing shared sessions".to_string());
        }
        initialize_shared_database(&source_db, &shared_db)?;
    }
    Ok(())
}

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

fn session_provider_from_home(codex_home: &Path) -> Result<String, String> {
    let config = match fs::read_to_string(codex_home.join("config.toml")) {
        Ok(config) => config,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok("openai".to_string())
        }
        Err(error) => return Err(format!("failed to read config.toml: {error}")),
    };
    session_provider_from_config(&config)
}

fn session_provider_from_config(config: &str) -> Result<String, String> {
    let doc = DocumentMut::from_str(config)
        .map_err(|error| format!("failed to parse config.toml: {error}"))?;
    let provider = doc
        .get("model_provider")
        .and_then(toml_edit::Item::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| "openai".to_string());
    Ok(provider)
}

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

fn empty_sync_result() -> SessionSyncResult {
    SessionSyncResult {
        inserted_threads: 0,
        copied_session_files: 0,
        duplicate_threads: 0,
        skipped_missing_session_files: 0,
        skipped_archived_threads: 0,
        merged_session_index_entries: 0,
    }
}

fn restore_status(result: Result<crate::backup::RestoreResult, String>) -> String {
    match result {
        Ok(_) => "ok".to_string(),
        Err(error) => error,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rusqlite::Connection;
    use tempfile::tempdir;

    use crate::backup::BackupScope;
    use crate::runtime_store::{
        RelayRuntimeInput, RuntimeStore, PLUS_RUNTIME_ID, RELAY_RUNTIME_ID,
    };

    use super::{
        preflight_runtime_session_sync, preflight_runtime_switch, switch_runtime_files,
        switch_runtime_files_detailed_with_progress,
        switch_runtime_files_preflighted_with_progress,
        switch_runtime_files_with_failure_and_progress_detailed, sync_home_with_shared,
        RuntimeSwitchOutcome, RuntimeSwitchPhase, SwitchFailurePoint,
    };

    fn create_state_db(home: &std::path::Path, id: &str, rollout_path: &std::path::Path) {
        let conn = Connection::open(home.join("state_5.sqlite")).unwrap();
        conn.execute(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, updated_at INTEGER, updated_at_ms INTEGER, model_provider TEXT)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO threads (id, rollout_path, updated_at, updated_at_ms, model_provider) VALUES (?1, ?2, 1, 1000, 'openai')",
            (id, rollout_path.to_string_lossy().to_string()),
        )
        .unwrap();
    }

    #[test]
    fn runtime_session_preflight_rejects_missing_or_invalid_databases_before_close() {
        let home = tempdir().unwrap();
        let shared = tempdir().unwrap();

        assert_eq!(
            preflight_runtime_session_sync(home.path(), shared.path()).unwrap_err(),
            "state_5.sqlite is required before switching runtimes"
        );

        let rollout = home.path().join("sessions/rollout-a.jsonl");
        create_state_db(home.path(), "thread-a", &rollout);
        preflight_runtime_session_sync(home.path(), shared.path()).unwrap();

        fs::write(shared.path().join("state_5.sqlite"), b"not sqlite").unwrap();
        assert!(preflight_runtime_session_sync(home.path(), shared.path())
            .unwrap_err()
            .contains("shared state_5.sqlite"));

        fs::remove_file(shared.path().join("state_5.sqlite")).unwrap();
        let conn = Connection::open(shared.path().join("state_5.sqlite")).unwrap();
        conn.execute(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, required_future TEXT NOT NULL)",
            [],
        )
        .unwrap();
        drop(conn);
        assert!(preflight_runtime_session_sync(home.path(), shared.path())
            .unwrap_err()
            .contains("required_future"));
    }

    #[test]
    fn switches_runtime_files_and_keeps_sessions_synced_through_shared_home() {
        let home = tempdir().unwrap();
        let rollout = home.path().join("sessions/2026/06/23/rollout-a.jsonl");
        fs::create_dir_all(rollout.parent().unwrap()).unwrap();
        let rollout_bytes = br#"{"type":"session_meta","payload":{"id":"thread-a"}}"#;
        fs::write(&rollout, rollout_bytes).unwrap();
        let rollout_modified_before = fs::metadata(&rollout).unwrap().modified().unwrap();
        create_state_db(home.path(), "thread-a", &rollout);
        fs::write(
            home.path().join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"fake-plus"}}"#,
        )
        .unwrap();
        fs::write(
            home.path().join("config.toml"),
            "model = \"gpt-5.5\"\nmodel_instructions_file = \"global\"\n",
        )
        .unwrap();
        let store_root = tempdir().unwrap();
        let store = RuntimeStore::new(store_root.path().join("runtimes"));
        store.import_plus_from_home(home.path(), false).unwrap();
        store
            .upsert_relay(
                RelayRuntimeInput {
                    base_url: "relay.example.com".to_string(),
                    api_key: "sk-fake-relay".to_string(),
                    model: "gpt-5.5".to_string(),
                },
                home.path(),
            )
            .unwrap();
        let backup_root = tempdir().unwrap();
        let shared = tempdir().unwrap();
        let mut phases = Vec::new();

        let result = switch_runtime_files_detailed_with_progress(
            &store,
            RELAY_RUNTIME_ID,
            home.path(),
            backup_root.path(),
            shared.path(),
            &mut |phase| phases.push(phase),
        )
        .unwrap();

        assert_eq!(
            phases,
            vec![
                RuntimeSwitchPhase::BackingUpCurrent,
                RuntimeSwitchPhase::BackingUpShared,
                RuntimeSwitchPhase::SyncingToShared,
                RuntimeSwitchPhase::ApplyingRuntime,
                RuntimeSwitchPhase::SyncingToCurrent,
                RuntimeSwitchPhase::Verifying,
            ]
        );
        assert_eq!(result.runtime.id, RELAY_RUNTIME_ID);
        assert_eq!(result.backups.len(), 2);
        assert_eq!(result.backups[0].scope, BackupScope::Runtime);
        assert_eq!(result.backups[1].scope, BackupScope::Sessions);
        assert!(result.backups.iter().all(|backup| {
            backup.tracked_databases == vec!["state_5.sqlite"]
                && !backup.files.iter().any(|file| {
                    matches!(
                        file.relative_path.to_string_lossy().as_ref(),
                        "goals_1.sqlite" | "memories_1.sqlite" | "logs_2.sqlite"
                    )
                })
        }));
        assert!(result.backups[0].backup_dir.join("manifest.json").exists());
        assert!(fs::read_to_string(home.path().join("auth.json"))
            .unwrap()
            .contains("sk-fake-relay"));
        let switched_config = fs::read_to_string(home.path().join("config.toml")).unwrap();
        assert!(switched_config.contains("model_provider = \"openai_custom\""));
        assert!(!switched_config.contains("env_key ="));
        assert!(!switched_config.contains("api_key ="));
        let conn = Connection::open(home.path().join("state_5.sqlite")).unwrap();
        let provider: String = conn
            .query_row(
                "SELECT model_provider FROM threads WHERE id = 'thread-a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(provider, "openai_custom");
        assert_eq!(fs::read(&rollout).unwrap(), rollout_bytes);
        assert_eq!(
            fs::metadata(&rollout).unwrap().modified().unwrap(),
            rollout_modified_before
        );
        assert!(shared.path().join("state_5.sqlite").exists());
        assert!(shared
            .path()
            .join("sessions/2026/06/23/rollout-a.jsonl")
            .exists());
        assert!(home
            .path()
            .join("sessions/2026/06/23/rollout-a.jsonl")
            .exists());
    }

    #[test]
    fn switching_back_to_account_restores_account_auth_and_config_without_relay_provider() {
        let home = tempdir().unwrap();
        let rollout = home.path().join("sessions/2026/06/23/rollout-a.jsonl");
        fs::create_dir_all(rollout.parent().unwrap()).unwrap();
        fs::write(
            &rollout,
            r#"{"type":"session_meta","payload":{"id":"thread-a"}}"#,
        )
        .unwrap();
        create_state_db(home.path(), "thread-a", &rollout);
        fs::write(
            home.path().join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"fake-plus"}}"#,
        )
        .unwrap();
        fs::write(
            home.path().join("config.toml"),
            "model = \"gpt-5.5\"\nmodel_instructions_file = \"global\"\n",
        )
        .unwrap();
        let store_root = tempdir().unwrap();
        let store = RuntimeStore::new(store_root.path().join("runtimes"));
        store.import_plus_from_home(home.path(), false).unwrap();
        fs::write(
            home.path().join("config.toml"),
            concat!(
                "model = \"relay-model\"\n",
                "model_provider = \"openai_custom\"\n",
                "model_instructions_file = \"new-global\"\n",
                "[features]\nfast_mode = true\n",
                "[mcp_servers.current]\ncommand = \"new-command\"\n",
                "[model_providers.openai_custom]\nbase_url = \"https://relay.example.com/v1\"\n",
            ),
        )
        .unwrap();
        let backup_root = tempdir().unwrap();
        let shared = tempdir().unwrap();

        let result = switch_runtime_files(
            &store,
            PLUS_RUNTIME_ID,
            home.path(),
            backup_root.path(),
            shared.path(),
        )
        .unwrap();

        assert_eq!(result.runtime.id, PLUS_RUNTIME_ID);
        assert!(fs::read_to_string(home.path().join("auth.json"))
            .unwrap()
            .contains("fake-plus"));
        let restored_config = fs::read_to_string(home.path().join("config.toml")).unwrap();
        assert!(restored_config.contains("model_instructions_file"));
        assert!(restored_config.contains("new-global"));
        assert!(restored_config.contains("fast_mode = true"));
        assert!(restored_config.contains("new-command"));
        let restored_doc = restored_config.parse::<toml_edit::DocumentMut>().unwrap();
        assert!(restored_doc.get("model_provider").is_none());
    }

    #[test]
    fn failure_after_runtime_files_are_written_restores_current_and_shared_snapshots() {
        let home = tempdir().unwrap();
        let rollout = home.path().join("sessions/2026/07/13/rollout-a.jsonl");
        fs::create_dir_all(rollout.parent().unwrap()).unwrap();
        fs::write(
            &rollout,
            r#"{"type":"session_meta","payload":{"id":"thread-a","model_provider":"openai"}}"#,
        )
        .unwrap();
        create_state_db(home.path(), "thread-a", &rollout);
        fs::write(
            home.path().join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"fake-plus"}}"#,
        )
        .unwrap();
        fs::write(
            home.path().join("config.toml"),
            "model = \"gpt-5.5\"\nmodel_instructions_file = \"global\"\n",
        )
        .unwrap();
        let original_auth = fs::read(home.path().join("auth.json")).unwrap();
        let original_config = fs::read(home.path().join("config.toml")).unwrap();
        let original_rollout = fs::read(&rollout).unwrap();
        let store_root = tempdir().unwrap();
        let store = RuntimeStore::new(store_root.path().join("runtimes"));
        store.import_plus_from_home(home.path(), false).unwrap();
        store
            .upsert_relay(
                RelayRuntimeInput {
                    base_url: "relay.example.com".to_string(),
                    api_key: "sk-fake-relay".to_string(),
                    model: "gpt-5.5".to_string(),
                },
                home.path(),
            )
            .unwrap();
        let backup_root = tempdir().unwrap();
        let shared_parent = tempdir().unwrap();
        let shared = shared_parent.path().join("shared-sessions");

        let mut phases = Vec::new();
        let error = switch_runtime_files_with_failure_and_progress_detailed(
            &store,
            RELAY_RUNTIME_ID,
            home.path(),
            backup_root.path(),
            &shared,
            SwitchFailurePoint::AfterRuntimeFiles,
            &mut |phase| phases.push(phase),
        )
        .unwrap_err();

        assert_eq!(
            phases,
            vec![
                RuntimeSwitchPhase::BackingUpCurrent,
                RuntimeSwitchPhase::BackingUpShared,
                RuntimeSwitchPhase::SyncingToShared,
                RuntimeSwitchPhase::ApplyingRuntime,
                RuntimeSwitchPhase::RollingBack,
            ]
        );
        assert!(error.message.contains("rolled back"));
        assert_eq!(error.backups.len(), 2);
        assert_eq!(
            fs::read(home.path().join("auth.json")).unwrap(),
            original_auth
        );
        assert_eq!(
            fs::read(home.path().join("config.toml")).unwrap(),
            original_config
        );
        assert_eq!(fs::read(&rollout).unwrap(), original_rollout);
        assert!(!shared.join("state_5.sqlite").exists());
    }

    #[test]
    fn rollback_failure_still_restores_other_root() {
        let home = tempdir().unwrap();
        let rollout = home
            .path()
            .join("sessions/2026/07/13/rollout-current.jsonl");
        fs::create_dir_all(rollout.parent().unwrap()).unwrap();
        fs::write(
            &rollout,
            r#"{"type":"session_meta","payload":{"id":"thread-current","model_provider":"openai"}}"#,
        )
        .unwrap();
        create_state_db(home.path(), "thread-current", &rollout);
        fs::write(
            home.path().join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"fake-plus"}}"#,
        )
        .unwrap();
        fs::write(home.path().join("config.toml"), "model = \"gpt-5.5\"\n").unwrap();
        let original_auth = fs::read(home.path().join("auth.json")).unwrap();

        let store_root = tempdir().unwrap();
        let store = RuntimeStore::new(store_root.path().join("runtimes"));
        store.import_plus_from_home(home.path(), false).unwrap();
        store
            .upsert_relay(
                RelayRuntimeInput {
                    base_url: "relay.example.com".to_string(),
                    api_key: "sk-fake-relay".to_string(),
                    model: "gpt-5.5".to_string(),
                },
                home.path(),
            )
            .unwrap();

        let shared = tempdir().unwrap();
        let shared_rollout = shared
            .path()
            .join("sessions/2026/07/12/rollout-shared.jsonl");
        fs::create_dir_all(shared_rollout.parent().unwrap()).unwrap();
        let original_shared_rollout =
            br#"{"type":"session_meta","payload":{"id":"thread-shared"}}"#;
        fs::write(&shared_rollout, original_shared_rollout).unwrap();
        create_state_db(shared.path(), "thread-shared", &shared_rollout);
        let backup_root = tempdir().unwrap();
        let mut phases = Vec::new();

        let error = switch_runtime_files_with_failure_and_progress_detailed(
            &store,
            RELAY_RUNTIME_ID,
            home.path(),
            backup_root.path(),
            shared.path(),
            SwitchFailurePoint::AfterRuntimeFilesWithCorruptCurrentBackup,
            &mut |phase| phases.push(phase),
        )
        .unwrap_err();

        assert_eq!(
            phases,
            vec![
                RuntimeSwitchPhase::BackingUpCurrent,
                RuntimeSwitchPhase::BackingUpShared,
                RuntimeSwitchPhase::SyncingToShared,
                RuntimeSwitchPhase::ApplyingRuntime,
                RuntimeSwitchPhase::RollingBack,
            ]
        );
        assert_eq!(error.outcome, RuntimeSwitchOutcome::RollbackFailed);
        assert!(
            error
                .message
                .contains("rollback failed (current: failed to parse backup manifest"),
            "{}",
            error.message
        );
        assert!(error.message.contains("; shared: ok)"), "{}", error.message);
        assert_ne!(
            fs::read(home.path().join("auth.json")).unwrap(),
            original_auth
        );
        assert_eq!(fs::read(&shared_rollout).unwrap(), original_shared_rollout);
        assert!(!shared
            .path()
            .join("sessions/2026/07/13/rollout-current.jsonl")
            .exists());

        let connection = Connection::open(shared.path().join("state_5.sqlite")).unwrap();
        let mut statement = connection
            .prepare("SELECT id FROM threads ORDER BY id")
            .unwrap();
        let thread_ids = statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(thread_ids, vec!["thread-shared"]);
    }

    #[test]
    fn mode_only_match_is_not_treated_as_an_exact_no_op() {
        let home = tempdir().unwrap();
        let rollout = home.path().join("sessions/2026/07/13/rollout-a.jsonl");
        fs::create_dir_all(rollout.parent().unwrap()).unwrap();
        fs::write(
            &rollout,
            r#"{"type":"session_meta","payload":{"id":"thread-a"}}"#,
        )
        .unwrap();
        create_state_db(home.path(), "thread-a", &rollout);
        fs::write(
            home.path().join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"saved-account"}}"#,
        )
        .unwrap();
        fs::write(home.path().join("config.toml"), "model = \"gpt-5.5\"\n").unwrap();
        let store_root = tempdir().unwrap();
        let store = RuntimeStore::new(store_root.path().join("runtimes"));
        store.import_plus_from_home(home.path(), false).unwrap();
        fs::write(
            home.path().join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"different-account"}}"#,
        )
        .unwrap();
        let backup_root = tempdir().unwrap();
        let shared = tempdir().unwrap();

        let result = switch_runtime_files(
            &store,
            PLUS_RUNTIME_ID,
            home.path(),
            backup_root.path(),
            shared.path(),
        )
        .unwrap();

        assert!(result.changed);
        assert!(fs::read_to_string(home.path().join("auth.json"))
            .unwrap()
            .contains("saved-account"));
    }

    #[test]
    fn exact_runtime_plan_applies_as_a_no_op_without_backups_or_sync_phases() {
        let home = tempdir().unwrap();
        let rollout = home.path().join("sessions/2026/07/25/rollout-a.jsonl");
        fs::create_dir_all(rollout.parent().unwrap()).unwrap();
        fs::write(
            &rollout,
            r#"{"type":"session_meta","payload":{"id":"thread-a"}}"#,
        )
        .unwrap();
        create_state_db(home.path(), "thread-a", &rollout);
        fs::write(
            home.path().join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"saved-account"}}"#,
        )
        .unwrap();
        fs::write(home.path().join("config.toml"), "model = \"gpt-5.5\"\n").unwrap();
        let store_root = tempdir().unwrap();
        let store = RuntimeStore::new(store_root.path().join("runtimes"));
        store.import_plus_from_home(home.path(), false).unwrap();
        let backup_root = tempdir().unwrap();
        let shared = tempdir().unwrap();

        let plan = preflight_runtime_switch(&store, PLUS_RUNTIME_ID, home.path()).unwrap();
        assert!(!plan.requires_change());
        let mut phases = Vec::new();
        let result = switch_runtime_files_preflighted_with_progress(
            &store,
            home.path(),
            backup_root.path(),
            shared.path(),
            plan,
            &mut |phase| phases.push(phase),
        )
        .unwrap();

        assert!(!result.changed);
        assert!(result.backups.is_empty());
        assert!(phases.is_empty());
        assert!(fs::read_dir(backup_root.path()).unwrap().next().is_none());
    }

    #[test]
    fn exact_no_op_plan_fails_if_runtime_drifts_during_preflight() {
        let home = tempdir().unwrap();
        fs::write(
            home.path().join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"saved-account"}}"#,
        )
        .unwrap();
        fs::write(home.path().join("config.toml"), "model = \"gpt-5.5\"\n").unwrap();
        let store_root = tempdir().unwrap();
        let store = RuntimeStore::new(store_root.path().join("runtimes"));
        store.import_plus_from_home(home.path(), false).unwrap();
        let plan = preflight_runtime_switch(&store, PLUS_RUNTIME_ID, home.path()).unwrap();
        assert!(!plan.requires_change());
        fs::write(
            home.path().join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"different-account"}}"#,
        )
        .unwrap();
        let backup_root = tempdir().unwrap();
        let shared = tempdir().unwrap();

        let error = switch_runtime_files_preflighted_with_progress(
            &store,
            home.path(),
            backup_root.path(),
            shared.path(),
            plan,
            &mut |_| {},
        )
        .unwrap_err();

        assert_eq!(
            error.message,
            "runtime state changed during switch preflight; retry"
        );
        assert!(error.backups.is_empty());
    }

    #[test]
    fn changed_runtime_plan_rebases_on_the_latest_live_global_config() {
        let home = tempdir().unwrap();
        let rollout = home.path().join("sessions/2026/07/25/rollout-a.jsonl");
        fs::create_dir_all(rollout.parent().unwrap()).unwrap();
        fs::write(
            &rollout,
            r#"{"type":"session_meta","payload":{"id":"thread-a"}}"#,
        )
        .unwrap();
        create_state_db(home.path(), "thread-a", &rollout);
        fs::write(
            home.path().join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"saved-account"}}"#,
        )
        .unwrap();
        fs::write(home.path().join("config.toml"), "model = \"gpt-5.5\"\n").unwrap();
        let store_root = tempdir().unwrap();
        let store = RuntimeStore::new(store_root.path().join("runtimes"));
        store.import_plus_from_home(home.path(), false).unwrap();
        store
            .upsert_relay(
                RelayRuntimeInput {
                    base_url: "relay.example.com".to_string(),
                    api_key: "sk-fake-relay".to_string(),
                    model: "gpt-5.5".to_string(),
                },
                home.path(),
            )
            .unwrap();
        let plan = preflight_runtime_switch(&store, RELAY_RUNTIME_ID, home.path()).unwrap();
        fs::write(
            home.path().join("config.toml"),
            "model = \"gpt-5.5\"\n[features]\nnew_global = true\n",
        )
        .unwrap();
        let backup_root = tempdir().unwrap();
        let shared = tempdir().unwrap();

        switch_runtime_files_preflighted_with_progress(
            &store,
            home.path(),
            backup_root.path(),
            shared.path(),
            plan,
            &mut |_| {},
        )
        .unwrap();

        let config = fs::read_to_string(home.path().join("config.toml")).unwrap();
        assert!(config.contains("new_global = true"));
        assert!(config.contains("model_provider = \"openai_custom\""));
    }

    #[test]
    fn changed_runtime_plan_rejects_sqlite_root_drift_before_backup() {
        let home = tempdir().unwrap();
        let shared = tempdir().unwrap();
        let rollout = home.path().join("sessions/2026/07/25/rollout-a.jsonl");
        fs::create_dir_all(rollout.parent().unwrap()).unwrap();
        fs::write(
            &rollout,
            r#"{"type":"session_meta","payload":{"id":"thread-a"}}"#,
        )
        .unwrap();
        create_state_db(home.path(), "thread-a", &rollout);
        fs::write(
            home.path().join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"saved-account"}}"#,
        )
        .unwrap();
        fs::write(home.path().join("config.toml"), "model = \"gpt-5.5\"\n").unwrap();
        let store_root = tempdir().unwrap();
        let store = RuntimeStore::new(store_root.path().join("runtimes"));
        store.import_plus_from_home(home.path(), false).unwrap();
        store
            .upsert_relay(
                RelayRuntimeInput {
                    base_url: "relay.example.com".to_string(),
                    api_key: "sk-fake-relay".to_string(),
                    model: "gpt-5.5".to_string(),
                },
                home.path(),
            )
            .unwrap();
        let plan = preflight_runtime_switch(&store, RELAY_RUNTIME_ID, home.path()).unwrap();
        let sqlite_home = shared.path().to_string_lossy().replace('\\', "\\\\");
        fs::write(
            home.path().join("config.toml"),
            format!("model = \"gpt-5.5\"\nsqlite_home = \"{sqlite_home}\"\n"),
        )
        .unwrap();
        let backup_root = tempdir().unwrap();

        let error = switch_runtime_files_preflighted_with_progress(
            &store,
            home.path(),
            backup_root.path(),
            shared.path(),
            plan,
            &mut |_| {},
        )
        .unwrap_err();

        assert_eq!(
            error.message,
            "SQLite root changed during switch preflight; retry"
        );
        assert!(error.backups.is_empty());
        assert!(fs::read_dir(backup_root.path()).unwrap().next().is_none());
    }

    #[test]
    fn shared_sync_is_idempotent_for_existing_threads() {
        let home = tempdir().unwrap();
        let rollout = home.path().join("sessions/2026/06/23/rollout-a.jsonl");
        fs::create_dir_all(rollout.parent().unwrap()).unwrap();
        fs::write(
            &rollout,
            r#"{"type":"session_meta","payload":{"id":"thread-a"}}"#,
        )
        .unwrap();
        create_state_db(home.path(), "thread-a", &rollout);
        let shared = tempdir().unwrap();

        let first = sync_home_with_shared(home.path(), shared.path()).unwrap();
        let second = sync_home_with_shared(home.path(), shared.path()).unwrap();

        assert_eq!(first.inserted_threads, 1);
        assert_eq!(second.inserted_threads, 0);
        assert!(second.duplicate_threads > 0);
    }
}

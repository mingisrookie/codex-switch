use std::{fs, path::Path, str::FromStr, time::Duration};

use rusqlite::{Connection, OpenFlags, MAIN_DB};
use serde::Serialize;
use toml_edit::DocumentMut;

use crate::{
    backup::{create_backup, create_local_backup, restore_backup, BackupManifest},
    codex_paths::resolve_user_codex_paths,
    config_patch::{plan_runtime_config_patch, RuntimeConfigKind},
    file_ops::atomic_write,
    operation_log::operation_id,
    runtime_store::{RuntimeConfidence, RuntimeKind, RuntimeMetadata, RuntimeStore},
    session_sync::{
        sync_shared_to_user_home, sync_shared_to_user_home_hot, sync_user_home_to_shared,
        SessionSyncResult,
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

#[derive(Debug, Clone)]
pub struct RuntimeSwitchFailure {
    pub message: String,
    pub backups: Vec<BackupManifest>,
}

impl RuntimeSwitchFailure {
    fn new(message: String, backups: Vec<BackupManifest>) -> Self {
        Self { message, backups }
    }

    fn before_backup(message: String) -> Self {
        Self::new(message, Vec::new())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchFailurePoint {
    None,
    AfterRuntimeFiles,
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
    switch_runtime_files_internal(
        store,
        runtime_id,
        codex_home,
        backup_root,
        shared_home,
        SwitchFailurePoint::None,
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
    switch_runtime_files_internal(
        store,
        runtime_id,
        codex_home,
        backup_root,
        shared_home,
        failure_point,
    )
}

fn switch_runtime_files_internal(
    store: &RuntimeStore,
    runtime_id: &str,
    codex_home: &Path,
    backup_root: &Path,
    shared_home: &Path,
    failure_point: SwitchFailurePoint,
) -> Result<RuntimeSwitchResult, RuntimeSwitchFailure> {
    let operation_id =
        operation_id("switch-runtime").map_err(RuntimeSwitchFailure::before_backup)?;
    let runtime_files = store
        .load_runtime_files(runtime_id)
        .map_err(RuntimeSwitchFailure::before_backup)?;
    let runtime = store
        .load_metadata(runtime_id)
        .map_err(RuntimeSwitchFailure::before_backup)?;
    serde_json::from_slice::<serde_json::Value>(&runtime_files.auth_json)
        .map_err(|error| format!("stored runtime auth.json is invalid: {error}"))
        .map_err(RuntimeSwitchFailure::before_backup)?;
    let live_config = fs::read_to_string(codex_home.join("config.toml"))
        .map_err(|error| format!("failed to read live config.toml: {error}"))
        .map_err(RuntimeSwitchFailure::before_backup)?;
    let config_kind = match runtime.kind {
        RuntimeKind::Plus => RuntimeConfigKind::Account,
        RuntimeKind::Relay => RuntimeConfigKind::Relay,
    };
    let config_plan =
        plan_runtime_config_patch(&live_config, &runtime_files.config_toml, config_kind)
            .map_err(RuntimeSwitchFailure::before_backup)?;
    let session_provider = session_provider_from_config(&config_plan.patched_toml)
        .map_err(RuntimeSwitchFailure::before_backup)?;

    let active = store
        .detect_active_runtime(codex_home)
        .map_err(RuntimeSwitchFailure::before_backup)?;
    if active.active_runtime_id.as_deref() == Some(runtime_id)
        && active.confidence == RuntimeConfidence::Exact
    {
        return Ok(RuntimeSwitchResult {
            operation_id,
            changed: false,
            runtime,
            backups: Vec::new(),
            to_shared: empty_sync_result(),
            from_shared: empty_sync_result(),
            rolled_back: false,
        });
    }

    let current_backup = create_backup(codex_home, backup_root, "switch-runtime-current")
        .map_err(RuntimeSwitchFailure::before_backup)?;
    let shared_backup = create_local_backup(shared_home, backup_root, "switch-runtime-shared")
        .map_err(|message| RuntimeSwitchFailure::new(message, vec![current_backup.clone()]))?;
    let backups = vec![current_backup.clone(), shared_backup.clone()];

    #[cfg(not(test))]
    if !crate::process_control::list_codex_processes()
        .map_err(|message| RuntimeSwitchFailure::new(message, backups.clone()))?
        .is_empty()
    {
        return Err(RuntimeSwitchFailure::new(
            "Codex started during switch preflight; close it and retry before files are changed"
                .to_string(),
            backups,
        ));
    }
    let applied = (|| {
        ensure_shared_sessions(codex_home, shared_home)?;
        let to_shared = sync_user_home_to_shared(codex_home, shared_home)?;
        atomic_write(&codex_home.join("auth.json"), &runtime_files.auth_json)?;
        atomic_write(
            &codex_home.join("config.toml"),
            config_plan.patched_toml.as_bytes(),
        )?;
        if failure_point == SwitchFailurePoint::AfterRuntimeFiles {
            return Err("injected failure after runtime files".to_string());
        }
        let from_shared = sync_shared_to_user_home(shared_home, codex_home, &session_provider)?;
        let verified = store.detect_active_runtime(codex_home)?;
        if verified.active_runtime_id.as_deref() != Some(runtime_id)
            || verified.confidence != RuntimeConfidence::Exact
        {
            return Err("runtime verification did not match the requested target".to_string());
        }
        let runtime = store.mark_used(runtime_id)?;
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
            let current_restore = restore_backup(&current_backup.backup_dir, codex_home);
            let shared_restore = restore_backup(&shared_backup.backup_dir, shared_home);
            match (current_restore, shared_restore) {
                (Ok(_), Ok(_)) => Err(RuntimeSwitchFailure::new(
                    format!("{error}; rolled back to verified snapshots"),
                    backups,
                )),
                (current, shared) => Err(RuntimeSwitchFailure::new(
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

    use crate::runtime_store::{
        RelayRuntimeInput, RuntimeStore, PLUS_RUNTIME_ID, RELAY_RUNTIME_ID,
    };

    use super::{
        switch_runtime_files, switch_runtime_files_with_failure_detailed, sync_home_with_shared,
        SwitchFailurePoint,
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
    fn switches_runtime_files_and_keeps_sessions_synced_through_shared_home() {
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

        let result = switch_runtime_files(
            &store,
            RELAY_RUNTIME_ID,
            home.path(),
            backup_root.path(),
            shared.path(),
        )
        .unwrap();

        assert_eq!(result.runtime.id, RELAY_RUNTIME_ID);
        assert_eq!(result.backups.len(), 2);
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
        let jsonl =
            fs::read_to_string(home.path().join("sessions/2026/06/23/rollout-a.jsonl")).unwrap();
        assert!(jsonl.contains(r#""model_provider":"openai_custom""#));
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

        let error = switch_runtime_files_with_failure_detailed(
            &store,
            RELAY_RUNTIME_ID,
            home.path(),
            backup_root.path(),
            &shared,
            SwitchFailurePoint::AfterRuntimeFiles,
        )
        .unwrap_err();

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

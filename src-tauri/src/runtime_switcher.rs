use std::{
    fs,
    path::{Path, PathBuf},
    str::FromStr,
};

use serde::Serialize;
use toml_edit::DocumentMut;
use walkdir::WalkDir;

use crate::{
    backup::{create_backup, BackupManifest},
    codex_paths::{local_codex_paths, resolve_user_codex_paths},
    runtime_store::{RuntimeMetadata, RuntimeStore},
    session_sync::{sync_shared_to_user_home, sync_user_home_to_shared, SessionSyncResult},
};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeSwitchResult {
    pub runtime: RuntimeMetadata,
    pub backup: BackupManifest,
    pub to_shared: SessionSyncResult,
    pub from_shared: SessionSyncResult,
}

pub fn switch_runtime_files(
    store: &RuntimeStore,
    runtime_id: &str,
    codex_home: &Path,
    backup_root: &Path,
    shared_home: &Path,
) -> Result<RuntimeSwitchResult, String> {
    let runtime_files = store.load_runtime_files(runtime_id)?;
    let runtime = store.load_metadata(runtime_id)?;
    let session_provider = session_provider_from_config(&runtime_files.config_toml);
    ensure_shared_sessions(codex_home, shared_home)?;
    let to_shared = sync_user_home_to_shared(codex_home, shared_home)?;
    let backup = create_backup(codex_home, backup_root, "switch-runtime")?;

    replace_file(&codex_home.join("auth.json"), &runtime_files.auth_json)?;
    replace_file(
        &codex_home.join("config.toml"),
        runtime_files.config_toml.as_bytes(),
    )?;

    let from_shared = sync_shared_to_user_home(shared_home, codex_home, &session_provider)?;

    Ok(RuntimeSwitchResult {
        runtime,
        backup,
        to_shared,
        from_shared,
    })
}

pub fn sync_home_with_shared(
    codex_home: &Path,
    shared_home: &Path,
) -> Result<SessionSyncResult, String> {
    let session_provider = session_provider_from_home(codex_home);
    ensure_shared_sessions(codex_home, shared_home)?;
    let to_shared = sync_user_home_to_shared(codex_home, shared_home)?;
    let from_shared = sync_shared_to_user_home(shared_home, codex_home, &session_provider)?;
    Ok(SessionSyncResult {
        inserted_threads: to_shared.inserted_threads + from_shared.inserted_threads,
        copied_session_files: to_shared.copied_session_files + from_shared.copied_session_files,
        duplicate_threads: to_shared.duplicate_threads + from_shared.duplicate_threads,
        skipped_missing_session_files: to_shared.skipped_missing_session_files
            + from_shared.skipped_missing_session_files,
        merged_session_index_entries: to_shared.merged_session_index_entries
            + from_shared.merged_session_index_entries,
    })
}

fn ensure_shared_sessions(codex_home: &Path, shared_home: &Path) -> Result<(), String> {
    fs::create_dir_all(shared_home)
        .map_err(|error| format!("failed to create shared sessions dir: {error}"))?;
    let shared_db = shared_home.join("state_5.sqlite");
    if !shared_db.exists() {
        let source_db = resolve_user_codex_paths(codex_home).state_db;
        if !source_db.exists() {
            return Err("state_5.sqlite is required before syncing shared sessions".to_string());
        }
        fs::copy(&source_db, &shared_db)
            .map_err(|error| format!("failed to initialize shared state_5.sqlite: {error}"))?;
    }
    let user_paths = resolve_user_codex_paths(codex_home);
    let shared_paths = local_codex_paths(shared_home);
    copy_sessions_tree(&user_paths.sessions_dir, &shared_paths.sessions_dir)
}

fn copy_sessions_tree(source: &Path, target: &Path) -> Result<(), String> {
    if !source.exists() {
        return Ok(());
    }
    for entry in WalkDir::new(source).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry.path().strip_prefix(source).unwrap_or(entry.path());
        let target_path = target.join(relative);
        if target_path.exists() {
            continue;
        }
        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("failed to create shared session parent: {error}"))?;
        }
        fs::copy(entry.path(), &target_path)
            .map_err(|error| format!("failed to copy shared session file: {error}"))?;
    }
    Ok(())
}

fn replace_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let temp_path = temp_path(path)?;
    fs::write(&temp_path, bytes)
        .map_err(|error| format!("failed to write temporary file: {error}"))?;
    fs::rename(&temp_path, path).map_err(|error| format!("failed to replace file: {error}"))
}

fn session_provider_from_home(codex_home: &Path) -> String {
    fs::read_to_string(codex_home.join("config.toml"))
        .ok()
        .map(|config| session_provider_from_config(&config))
        .unwrap_or_else(|| "openai".to_string())
}

fn session_provider_from_config(config: &str) -> String {
    DocumentMut::from_str(config)
        .ok()
        .and_then(|doc| {
            doc.get("model_provider")
                .and_then(toml_edit::Item::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| "openai".to_string())
}

fn temp_path(path: &Path) -> Result<PathBuf, String> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "target file path must include a file name".to_string())?;
    Ok(path.with_file_name(format!("{file_name}.codex-switch.tmp")))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rusqlite::Connection;
    use tempfile::tempdir;

    use crate::runtime_store::{
        RelayRuntimeInput, RuntimeStore, PLUS_RUNTIME_ID, RELAY_RUNTIME_ID,
    };

    use super::{switch_runtime_files, sync_home_with_shared};

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
        store.import_plus_from_home(home.path()).unwrap();
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
        assert!(result.backup.backup_dir.join("auth.json").exists());
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
        store.import_plus_from_home(home.path()).unwrap();
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
        assert!(!restored_config.contains("openai_custom"));
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

        assert_eq!(first.inserted_threads, 0);
        assert_eq!(second.inserted_threads, 0);
        assert!(second.duplicate_threads > 0);
    }
}

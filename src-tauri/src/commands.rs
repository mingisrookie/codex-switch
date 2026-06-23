use serde::Serialize;
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use crate::{
    backup::{create_backup as create_backup_snapshot, BackupManifest},
    codex_home::{scan_codex_home as scan_home, CodexHomeStatus},
    process_control::{close_codex_processes as close_codex, list_codex_processes as list_processes, CodexProcess},
    profile_store::{ApiProfileInput, ProfileKind, ProfileMetadata, ProfileStore},
    runtime_store::{RelayRuntimeInput, RuntimeMetadata, RuntimeStore},
    runtime_switcher::{switch_runtime_files, sync_home_with_shared, RuntimeSwitchResult},
    session_scan::{
        build_sync_dry_run, scan_sessions as scan_session_inventory, SessionInventory, SyncDryRun,
    },
    session_sync::{sync_sessions, SessionSyncResult},
    switcher::{switch_profile_files, SwitchResult},
};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppStatus {
    pub app_name: &'static str,
    pub phase: &'static str,
    pub codex_home: PathBuf,
}

#[tauri::command]
pub fn get_app_status() -> AppStatus {
    AppStatus {
        app_name: "Codex Switch",
        phase: "MVP scaffold",
        codex_home: default_codex_home(),
    }
}

#[tauri::command]
pub fn scan_codex_home(path: Option<String>) -> Result<CodexHomeStatus, String> {
    scan_home(&resolve_codex_home(path))
}

#[tauri::command]
pub fn scan_sessions(path: Option<String>) -> Result<SessionInventory, String> {
    scan_session_inventory(&resolve_codex_home(path))
}

#[tauri::command]
pub fn dry_run_sync(source_paths: Vec<String>, target_path: Option<String>) -> Result<SyncDryRun, String> {
    let mut sources = Vec::new();
    for source_path in source_paths {
        sources.push(scan_session_inventory(&PathBuf::from(source_path))?);
    }
    let target = scan_session_inventory(&resolve_codex_home(target_path))?;
    Ok(build_sync_dry_run(&sources, &target))
}

#[tauri::command]
pub fn create_backup(destination_root: String, reason: String, path: Option<String>) -> Result<BackupManifest, String> {
    create_backup_snapshot(&resolve_codex_home(path), &PathBuf::from(destination_root), &reason)
}

#[tauri::command]
pub fn list_profiles() -> Result<Vec<ProfileMetadata>, String> {
    ProfileStore::default()?.list_profiles()
}

#[tauri::command]
pub fn import_current_profile(name: String, kind: ProfileKind, path: Option<String>) -> Result<ProfileMetadata, String> {
    ProfileStore::default()?.import_current_profile(&name, kind, &resolve_codex_home(path))
}

#[tauri::command]
pub fn create_api_profile(input: ApiProfileInput) -> Result<ProfileMetadata, String> {
    ProfileStore::default()?.create_api_profile(input)
}

#[tauri::command]
pub fn list_runtimes() -> Result<Vec<RuntimeMetadata>, String> {
    RuntimeStore::default()?.list_runtimes()
}

#[tauri::command]
pub fn import_plus_runtime(path: Option<String>) -> Result<RuntimeMetadata, String> {
    RuntimeStore::default()?.import_plus_from_home(&resolve_codex_home(path))
}

#[tauri::command]
pub fn upsert_relay_runtime(input: RelayRuntimeInput, path: Option<String>) -> Result<RuntimeMetadata, String> {
    RuntimeStore::default()?.upsert_relay(input, &resolve_codex_home(path))
}

#[tauri::command]
pub fn list_codex_processes() -> Result<Vec<CodexProcess>, String> {
    list_processes()
}

#[tauri::command]
pub fn close_codex_processes() -> Result<Vec<CodexProcess>, String> {
    close_codex()
}

#[tauri::command]
pub fn switch_profile(profile_id: String, path: Option<String>) -> Result<SwitchResult, String> {
    let processes = list_processes()?;
    if !processes.is_empty() {
        return Err("Codex is still running; close it before switching profiles".to_string());
    }
    let backup_root = default_backup_root()?;
    switch_profile_files(&ProfileStore::default()?, &profile_id, &resolve_codex_home(path), &backup_root)
}

#[tauri::command]
pub fn switch_runtime(runtime_id: String, path: Option<String>) -> Result<RuntimeSwitchResult, String> {
    let processes = list_processes()?;
    let backup_root = default_backup_root()?;
    let shared_home = default_shared_sessions_root()?;
    switch_runtime_guarded(
        runtime_id,
        path,
        &processes,
        &backup_root,
        &shared_home,
        &RuntimeStore::default()?,
    )
}

#[tauri::command]
pub fn sync_all_sessions(path: Option<String>) -> Result<SessionSyncResult, String> {
    let backup_root = default_backup_root()?;
    let shared_home = default_shared_sessions_root()?;
    sync_all_sessions_guarded(path, &[], &backup_root, &shared_home)
}

#[tauri::command]
pub fn sync_sessions_from_paths(source_paths: Vec<String>, target_path: Option<String>) -> Result<SessionSyncResult, String> {
    let processes = list_processes()?;
    let backup_root = default_backup_root()?;
    sync_sessions_from_paths_guarded(source_paths, target_path, &processes, &backup_root)
}

fn resolve_codex_home(path: Option<String>) -> PathBuf {
    path.filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(default_codex_home)
}

fn default_codex_home() -> PathBuf {
    default_codex_home_from_env(
        std::env::var_os("CODEX_HOME"),
        std::env::var_os("USERPROFILE"),
        std::env::var_os("HOME"),
    )
}

fn default_codex_home_from_env(
    codex_home: Option<OsString>,
    user_profile: Option<OsString>,
    home: Option<OsString>,
) -> PathBuf {
    if let Some(path) = non_empty_os(codex_home) {
        return PathBuf::from(path);
    }
    if let Some(path) = non_empty_os(user_profile) {
        return PathBuf::from(path).join(".codex");
    }
    if let Some(path) = non_empty_os(home) {
        return PathBuf::from(path).join(".codex");
    }
    PathBuf::from(".codex")
}

fn non_empty_os(value: Option<OsString>) -> Option<OsString> {
    value.filter(|item| !item.to_string_lossy().trim().is_empty())
}

fn default_backup_root() -> Result<PathBuf, String> {
    let appdata = std::env::var_os("APPDATA").ok_or_else(|| "APPDATA is not set".to_string())?;
    Ok(PathBuf::from(appdata).join("codex-switch").join("backups"))
}

fn default_shared_sessions_root() -> Result<PathBuf, String> {
    let appdata = std::env::var_os("APPDATA").ok_or_else(|| "APPDATA is not set".to_string())?;
    Ok(PathBuf::from(appdata).join("codex-switch").join("shared-sessions"))
}

fn switch_runtime_guarded(
    runtime_id: String,
    target_path: Option<String>,
    running_processes: &[CodexProcess],
    backup_root: &Path,
    shared_home: &Path,
    store: &RuntimeStore,
) -> Result<RuntimeSwitchResult, String> {
    if !running_processes.is_empty() {
        return Err("Codex is still running; close it before switching runtimes".to_string());
    }
    switch_runtime_files(
        store,
        &runtime_id,
        &resolve_codex_home(target_path),
        backup_root,
        shared_home,
    )
}

fn sync_sessions_from_paths_guarded(
    source_paths: Vec<String>,
    target_path: Option<String>,
    running_processes: &[CodexProcess],
    backup_root: &Path,
) -> Result<SessionSyncResult, String> {
    if !running_processes.is_empty() {
        return Err("Codex is still running; close it before syncing sessions".to_string());
    }
    let target = resolve_codex_home(target_path);
    create_backup_snapshot(&target, backup_root, "sync-sessions")?;
    let sources = source_paths.into_iter().map(PathBuf::from).collect::<Vec<_>>();
    sync_sessions(&sources, &target)
}

fn sync_all_sessions_guarded(
    target_path: Option<String>,
    _running_processes: &[CodexProcess],
    backup_root: &Path,
    shared_home: &Path,
) -> Result<SessionSyncResult, String> {
    let target = resolve_codex_home(target_path);
    create_backup_snapshot(&target, backup_root, "sync-all-sessions")?;
    sync_home_with_shared(&target, shared_home)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rusqlite::Connection;
    use tempfile::tempdir;

    use crate::runtime_store::{RelayRuntimeInput, RuntimeStore, RELAY_RUNTIME_ID};

    use super::{
        default_codex_home_from_env, resolve_codex_home, switch_runtime_guarded, sync_all_sessions_guarded,
        sync_sessions_from_paths_guarded, CodexProcess,
    };

    #[test]
    fn resolves_default_codex_home_from_environment_without_hardcoded_user() {
        let codex_home = default_codex_home_from_env(
            None,
            Some(std::ffi::OsString::from(r"C:\Users\alice")),
            Some(std::ffi::OsString::from(r"C:\Users\ignored")),
        );

        assert_eq!(codex_home, std::path::PathBuf::from(r"C:\Users\alice").join(".codex"));
    }

    #[test]
    fn codex_home_environment_overrides_user_profile_default() {
        let codex_home = default_codex_home_from_env(
            Some(std::ffi::OsString::from(r"D:\portable-codex")),
            Some(std::ffi::OsString::from(r"C:\Users\alice")),
            None,
        );

        assert_eq!(codex_home, std::path::PathBuf::from(r"D:\portable-codex"));
    }

    #[test]
    fn explicit_path_overrides_environment_default() {
        let temp = tempdir().unwrap();
        let resolved = resolve_codex_home(Some(temp.path().to_string_lossy().to_string()));

        assert_eq!(resolved, temp.path());
    }

    fn create_threads_db(home: &std::path::Path, threads: &[&str]) {
        let conn = Connection::open(home.join("state_5.sqlite")).unwrap();
        conn.execute(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, updated_at INTEGER, updated_at_ms INTEGER)",
            [],
        )
        .unwrap();
        for id in threads {
            conn.execute(
                "INSERT INTO threads (id, rollout_path, updated_at, updated_at_ms) VALUES (?1, NULL, 1, 1000)",
                [id],
            )
            .unwrap();
        }
    }

    #[test]
    fn sync_command_rejects_running_processes_before_backup_or_write() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let backup_root = tempdir().unwrap();
        create_threads_db(source.path(), &["thread-a"]);
        create_threads_db(target.path(), &[]);

        let result = sync_sessions_from_paths_guarded(
            vec![source.path().to_string_lossy().to_string()],
            Some(target.path().to_string_lossy().to_string()),
            &[CodexProcess {
                image_name: "codex.exe".to_string(),
                pid: 1234,
            }],
            backup_root.path(),
        );

        assert!(result.unwrap_err().contains("Codex is still running"));
        assert_eq!(fs::read_dir(backup_root.path()).unwrap().count(), 0);
    }

    #[test]
    fn sync_command_creates_backup_before_writing_sessions() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let backup_root = tempdir().unwrap();
        create_threads_db(source.path(), &["thread-a"]);
        create_threads_db(target.path(), &[]);
        fs::write(target.path().join("auth.json"), "{}").unwrap();
        fs::write(target.path().join("config.toml"), "model = \"gpt-5.5\"").unwrap();

        let result = sync_sessions_from_paths_guarded(
            vec![source.path().to_string_lossy().to_string()],
            Some(target.path().to_string_lossy().to_string()),
            &[],
            backup_root.path(),
        )
        .unwrap();

        assert_eq!(result.inserted_threads, 1);
        let backup_dir = fs::read_dir(backup_root.path()).unwrap().next().unwrap().unwrap().path();
        assert!(backup_dir.join("auth.json").exists());
        assert!(backup_dir.join("config.toml").exists());
        assert!(backup_dir.join("state_5.sqlite").exists());
    }

    #[test]
    fn switch_runtime_rejects_running_processes_before_backup_or_write() {
        let home = tempdir().unwrap();
        let store_root = tempdir().unwrap();
        let backup_root = tempdir().unwrap();
        let shared = tempdir().unwrap();
        fs::write(home.path().join("auth.json"), r#"{"auth_mode":"chatgpt","tokens":{"access_token":"fake-plus"}}"#).unwrap();
        fs::write(home.path().join("config.toml"), "model = \"gpt-5.5\"\n").unwrap();
        create_threads_db(home.path(), &["thread-a"]);
        let store = RuntimeStore::new(store_root.path().join("runtimes"));
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

        let result = switch_runtime_guarded(
            RELAY_RUNTIME_ID.to_string(),
            Some(home.path().to_string_lossy().to_string()),
            &[CodexProcess {
                image_name: "codex.exe".to_string(),
                pid: 1234,
            }],
            backup_root.path(),
            shared.path(),
            &store,
        );

        assert!(result.unwrap_err().contains("Codex is still running"));
        assert!(fs::read_to_string(home.path().join("auth.json")).unwrap().contains("fake-plus"));
        assert_eq!(fs::read_dir(backup_root.path()).unwrap().count(), 0);
    }

    #[test]
    fn sync_all_sessions_creates_backup_before_writing_shared_sessions() {
        let home = tempdir().unwrap();
        let shared = tempdir().unwrap();
        let backup_root = tempdir().unwrap();
        create_threads_db(home.path(), &["thread-a"]);
        create_threads_db(shared.path(), &["thread-b"]);
        fs::write(home.path().join("auth.json"), "{}").unwrap();
        fs::write(home.path().join("config.toml"), "model = \"gpt-5.5\"").unwrap();

        let result = sync_all_sessions_guarded(
            Some(home.path().to_string_lossy().to_string()),
            &[],
            backup_root.path(),
            shared.path(),
        )
        .unwrap();

        assert_eq!(result.inserted_threads, 2);
        let backup_dir = fs::read_dir(backup_root.path()).unwrap().next().unwrap().unwrap().path();
        let backup_conn = Connection::open(backup_dir.join("state_5.sqlite")).unwrap();
        let backup_thread_b_count: i64 = backup_conn
            .query_row("SELECT COUNT(*) FROM threads WHERE id = 'thread-b'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(backup_thread_b_count, 0);
        let live_conn = Connection::open(home.path().join("state_5.sqlite")).unwrap();
        let live_thread_b_count: i64 = live_conn
            .query_row("SELECT COUNT(*) FROM threads WHERE id = 'thread-b'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(live_thread_b_count, 1);
    }

    #[test]
    fn sync_all_sessions_allows_running_codex_for_hot_sync() {
        let home = tempdir().unwrap();
        let shared = tempdir().unwrap();
        let backup_root = tempdir().unwrap();
        create_threads_db(home.path(), &["thread-a"]);
        create_threads_db(shared.path(), &["thread-b"]);
        fs::write(home.path().join("auth.json"), "{}").unwrap();
        fs::write(home.path().join("config.toml"), "model = \"gpt-5.5\"").unwrap();

        let result = sync_all_sessions_guarded(
            Some(home.path().to_string_lossy().to_string()),
            &[CodexProcess {
                image_name: "codex.exe".to_string(),
                pid: 1234,
            }],
            backup_root.path(),
            shared.path(),
        )
        .unwrap();

        assert_eq!(result.inserted_threads, 2);
        assert_eq!(fs::read_dir(backup_root.path()).unwrap().count(), 1);
    }
}

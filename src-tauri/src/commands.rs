use serde::Serialize;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard, TryLockError},
    thread,
    time::Duration,
};

#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;

use crate::{
    backup::{
        create_backup as create_backup_snapshot, create_local_backup,
        list_recent_backups as list_backup_snapshots, migrate_legacy_plaintext_auth,
        restore_backup as restore_backup_snapshot, verify_backup, BackupManifest, BackupSummary,
        RestoreResult,
    },
    codex_home::{scan_codex_home as scan_home, CodexHomeStatus},
    operation_log::{
        operation_id, timestamp_millis, OperationAction, OperationLog, OperationPhase,
        OperationRecord, OperationStatus,
    },
    process_control::{
        close_codex_processes as close_codex, list_codex_processes as list_processes, CodexProcess,
    },
    relay_verify::verify_relay,
    runtime_store::{
        RelayRuntimeInput, RuntimeMetadata, RuntimeStatus, RuntimeStore, RELAY_RUNTIME_ID,
    },
    runtime_switcher::{switch_runtime_files_detailed, sync_home_with_shared, RuntimeSwitchResult},
    session_manager::{
        delete_managed_sessions_detailed as delete_sessions,
        restore_sessions_visible_detailed as restore_visible,
        scan_managed_sessions as scan_managed_session_inventory, ManagedSessionInventory,
        SessionMutationResult,
    },
    session_scan::{
        build_sync_dry_run, scan_sessions as scan_session_inventory,
        scan_sessions_local as scan_local_session_inventory, SessionInventory, SyncDryRun,
    },
    session_sync::SessionSyncResult,
    skill_manager::{
        install_skill_at, list_skills_at, save_skill_config_at, SkillConfigInput, SkillId,
        SkillMutationReceipt, SkillStatus,
    },
    update_check::{check_latest_release, UpdateCheckResult},
    update_install::{
        install_latest_update, startup_update_notice, UpdateInstallReceipt, UpdateStartupNotice,
    },
};

static MUTATION_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug)]
struct MutationGuard {
    _process_guard: MutexGuard<'static, ()>,
    _lock_file: File,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppStatus {
    pub app_name: &'static str,
    pub version: &'static str,
    pub phase: &'static str,
    pub codex_home: PathBuf,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AllSessionsDryRun {
    pub to_shared: SyncDryRun,
    pub to_current: SyncDryRun,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSyncReceipt {
    pub operation_id: String,
    pub backups: Vec<BackupManifest>,
    #[serde(flatten)]
    pub result: SessionSyncResult,
    pub rolled_back: bool,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMutationReceipt {
    pub operation_id: String,
    #[serde(flatten)]
    pub result: SessionMutationResult,
    pub rolled_back: bool,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreBackupReceipt {
    pub operation_id: String,
    #[serde(flatten)]
    pub result: RestoreResult,
    pub safety_backup: BackupManifest,
    pub rolled_back: bool,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
struct HotSyncCompensation {
    shared_rolled_back: bool,
    current_backup_dir: PathBuf,
}

#[tauri::command]
pub fn get_app_status() -> AppStatus {
    AppStatus {
        app_name: "Codex Switch",
        version: env!("CARGO_PKG_VERSION"),
        phase: "hardened-mvp",
        codex_home: default_codex_home(),
    }
}

#[tauri::command]
pub async fn check_for_updates() -> Result<UpdateCheckResult, String> {
    tauri::async_runtime::spawn_blocking(check_latest_release)
        .await
        .map_err(|_| "update check worker failed".to_string())?
}

#[tauri::command]
pub async fn install_update(app: tauri::AppHandle) -> Result<UpdateInstallReceipt, String> {
    let receipt = tauri::async_runtime::spawn_blocking(install_latest_update)
        .await
        .map_err(|_| "update installer worker failed".to_string())??;
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(500));
        app.exit(0);
    });
    Ok(receipt)
}

#[tauri::command]
pub fn get_update_startup_notice() -> Option<UpdateStartupNotice> {
    startup_update_notice()
}

#[tauri::command]
pub fn scan_codex_home() -> Result<CodexHomeStatus, String> {
    scan_home(&default_codex_home())
}

#[tauri::command]
pub fn scan_sessions() -> Result<SessionInventory, String> {
    scan_session_inventory(&default_codex_home())
}

#[tauri::command]
pub fn scan_managed_sessions() -> Result<ManagedSessionInventory, String> {
    let shared_home = default_shared_sessions_root()?;
    scan_managed_session_inventory(&default_codex_home(), &shared_home)
}

#[tauri::command]
pub fn dry_run_all_sessions() -> Result<AllSessionsDryRun, String> {
    let current = scan_session_inventory(&default_codex_home())?;
    let shared = scan_local_session_inventory(&default_shared_sessions_root()?)?;
    Ok(AllSessionsDryRun {
        to_shared: build_sync_dry_run(std::slice::from_ref(&current), &shared),
        to_current: build_sync_dry_run(std::slice::from_ref(&shared), &current),
    })
}

#[tauri::command]
pub fn list_runtimes() -> Result<Vec<RuntimeMetadata>, String> {
    RuntimeStore::from_default_root()?.list_runtimes()
}

#[tauri::command]
pub fn scan_runtime_status() -> Result<RuntimeStatus, String> {
    RuntimeStore::from_default_root()?.detect_active_runtime(&default_codex_home())
}

#[tauri::command]
pub fn import_plus_runtime(confirm_overwrite: bool) -> Result<RuntimeMetadata, String> {
    let _mutation_guard = acquire_mutation_lock()?;
    let started = timestamp_millis()?;
    let id = operation_id("import-account")?;
    let result = (|| {
        RuntimeStore::from_default_root()?
            .import_plus_from_home(&default_codex_home(), confirm_overwrite)
    })();
    record_result(
        &id,
        OperationAction::ImportAccount,
        started,
        &result,
        &[],
        BTreeMap::new(),
    );
    result
}

#[tauri::command]
pub fn upsert_relay_runtime(input: RelayRuntimeInput) -> Result<RuntimeMetadata, String> {
    let _mutation_guard = acquire_mutation_lock()?;
    let started = timestamp_millis()?;
    let id = operation_id("save-relay")?;
    let result =
        (|| RuntimeStore::from_default_root()?.upsert_relay(input, &default_codex_home()))();
    record_result(
        &id,
        OperationAction::SaveRelay,
        started,
        &result,
        &[],
        BTreeMap::new(),
    );
    result
}

#[tauri::command]
pub fn test_relay_connection() -> Result<RuntimeMetadata, String> {
    let _mutation_guard = acquire_mutation_lock()?;
    let started = timestamp_millis()?;
    let id = operation_id("verify-relay")?;
    let result = (|| {
        let store = RuntimeStore::from_default_root()?;
        let connection = store.load_relay_connection()?;
        verify_relay(&connection.base_url, &connection.api_key)?;
        store.mark_verified(RELAY_RUNTIME_ID)
    })();
    record_result(
        &id,
        OperationAction::VerifyRelay,
        started,
        &result,
        &[],
        BTreeMap::new(),
    );
    result
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
pub fn switch_runtime(runtime_id: String) -> Result<RuntimeSwitchResult, String> {
    let _mutation_guard = acquire_mutation_lock()?;
    let started = timestamp_millis()?;
    let attempt_id = operation_id("switch-runtime-attempt")?;
    let mut failure_backups = Vec::new();
    let result = (|| {
        ensure_codex_closed("switching runtimes")?;
        let backup_root = default_backup_root()?;
        let shared_home = default_shared_sessions_root()?;
        let store = RuntimeStore::from_default_root()?;
        if runtime_id == RELAY_RUNTIME_ID {
            let connection = store.load_relay_connection()?;
            verify_relay(&connection.base_url, &connection.api_key)?;
            store.mark_verified(RELAY_RUNTIME_ID)?;
        }
        ensure_codex_closed("switching runtimes")?;
        match switch_runtime_files_detailed(
            &store,
            &runtime_id,
            &default_codex_home(),
            &backup_root,
            &shared_home,
        ) {
            Ok(receipt) => Ok(receipt),
            Err(failure) => {
                failure_backups = failure.backups;
                Err(failure.message)
            }
        }
    })();
    let (id, backups, counts) = match &result {
        Ok(receipt) => (
            receipt.operation_id.as_str(),
            receipt.backups.as_slice(),
            BTreeMap::from([
                ("toShared".to_string(), receipt.to_shared.inserted_threads),
                (
                    "fromShared".to_string(),
                    receipt.from_shared.inserted_threads,
                ),
            ]),
        ),
        Err(_) => (
            attempt_id.as_str(),
            failure_backups.as_slice(),
            BTreeMap::new(),
        ),
    };
    record_result(
        id,
        OperationAction::SwitchRuntime,
        started,
        &result,
        backups,
        counts,
    );
    result
}

#[tauri::command]
pub fn sync_all_sessions() -> Result<SessionSyncReceipt, String> {
    let _mutation_guard = acquire_mutation_lock()?;
    let started = timestamp_millis()?;
    let operation_id = operation_id("sync-sessions")?;
    let mut backups = Vec::new();
    let mut failure_status = None;
    let mut result = (|| {
        let backup_root = default_backup_root()?;
        let shared_home = default_shared_sessions_root()?;
        let current_home = default_codex_home();
        let current_backup = create_backup_snapshot(&current_home, &backup_root, "sync-current")?;
        backups.push(current_backup.clone());
        let shared_backup = create_local_backup(&shared_home, &backup_root, "sync-shared")?;
        backups.push(shared_backup.clone());
        match sync_home_with_shared(&current_home, &shared_home) {
            Ok(sync_result) => Ok(SessionSyncReceipt {
                operation_id: operation_id.clone(),
                backups: backups.clone(),
                result: sync_result,
                rolled_back: false,
                warnings: Vec::new(),
            }),
            Err(error) => {
                let compensation = compensate_failed_hot_sync(
                    &current_home,
                    &current_backup,
                    &shared_home,
                    &shared_backup,
                );
                if compensation.shared_rolled_back {
                    failure_status = Some(OperationStatus::Failed);
                    Err(format!(
                        "session sync failed: {error}; restored the shared pool and left the live current home untouched; current safety backup: {}",
                        compensation.current_backup_dir.display()
                    ))
                } else {
                    failure_status = Some(OperationStatus::RollbackFailed);
                    Err(format!(
                        "session sync failed: {error}; shared rollback failed and the live current home was left untouched; use the verified backups"
                    ))
                }
            }
        }
    })();
    match &mut result {
        Ok(receipt) => {
            receipt.warnings = record_success(
                &operation_id,
                OperationAction::SyncSessions,
                started,
                &backups,
                sync_counts(&receipt.result),
            )
            .into_iter()
            .collect();
        }
        Err(error) => {
            let _ = append_operation_record(
                &operation_id,
                OperationAction::SyncSessions,
                failure_status.unwrap_or_else(|| terminal_status(error)),
                started,
                &backups,
                BTreeMap::new(),
            );
        }
    }
    result
}

#[tauri::command]
pub fn delete_managed_sessions(
    ids: Vec<String>,
    confirmed: bool,
) -> Result<SessionMutationReceipt, String> {
    let _mutation_guard = acquire_mutation_lock()?;
    let started = timestamp_millis()?;
    let operation_id = operation_id("delete-sessions")?;
    let mut failure_backups = Vec::new();
    let result = (|| {
        ensure_codex_closed("deleting sessions")?;
        let backup_root = default_backup_root()?;
        let shared_home = default_shared_sessions_root()?;
        match delete_sessions(
            &default_codex_home(),
            &shared_home,
            &backup_root,
            &ids,
            confirmed,
        ) {
            Ok(result) => Ok(result),
            Err(failure) => {
                failure_backups = failure.backups;
                Err(failure.message)
            }
        }
    })();
    finish_session_mutation(
        operation_id,
        OperationAction::DeleteSessions,
        started,
        result,
        &failure_backups,
    )
}

#[tauri::command]
pub fn restore_sessions_visible(ids: Vec<String>) -> Result<SessionMutationReceipt, String> {
    let _mutation_guard = acquire_mutation_lock()?;
    let started = timestamp_millis()?;
    let operation_id = operation_id("restore-visibility")?;
    let mut failure_backups = Vec::new();
    let result = (|| {
        ensure_codex_closed("restoring session visibility")?;
        let backup_root = default_backup_root()?;
        match restore_visible(&default_codex_home(), &backup_root, &ids) {
            Ok(result) => Ok(result),
            Err(failure) => {
                failure_backups = failure.backups;
                Err(failure.message)
            }
        }
    })();
    finish_session_mutation(
        operation_id,
        OperationAction::RestoreVisibility,
        started,
        result,
        &failure_backups,
    )
}

#[tauri::command]
pub fn list_backups() -> Result<Vec<BackupSummary>, String> {
    let backup_root = default_backup_root()?;
    {
        let _migration_guard = acquire_mutation_lock()?;
        migrate_legacy_plaintext_auth(&backup_root)?;
    }
    list_backup_snapshots(&backup_root, 5)
}

#[tauri::command]
pub fn restore_backup(backup_dir: String) -> Result<RestoreBackupReceipt, String> {
    let _mutation_guard = acquire_mutation_lock()?;
    let started = timestamp_millis()?;
    let operation_id = operation_id("restore-backup")?;
    let mut backups = Vec::new();
    let mut failure_status = None;
    let mut result = (|| {
        ensure_codex_closed("restoring a backup")?;
        let backup_root = default_backup_root()?;
        let selected = validate_backup_selection(&backup_root, Path::new(&backup_dir))?;
        let manifest = verify_backup(&selected)?;
        backups.push(manifest.clone());
        let current_home = default_codex_home();
        let shared_home = default_shared_sessions_root()?;
        let target_is_local = manifest.state_db_is_local;
        let target = if manifest.source_root == current_home {
            current_home
        } else if manifest.source_root == shared_home {
            shared_home
        } else {
            return Err("backup source is not one of the managed roots".to_string());
        };
        let safety_backup = if target_is_local {
            create_local_backup(&target, &backup_root, "pre-restore-safety")?
        } else {
            create_backup_snapshot(&target, &backup_root, "pre-restore-safety")?
        };
        backups.push(safety_backup.clone());
        ensure_codex_closed("restoring a backup")?;
        match restore_backup_snapshot(&selected, &target) {
            Ok(restore_result) => Ok(RestoreBackupReceipt {
                operation_id: operation_id.clone(),
                result: restore_result,
                safety_backup,
                rolled_back: false,
                warnings: Vec::new(),
            }),
            Err(error) => {
                let rolled_back =
                    restore_backup_snapshot(&safety_backup.backup_dir, &target).is_ok();
                failure_status = Some(if rolled_back {
                    OperationStatus::RolledBack
                } else {
                    OperationStatus::RollbackFailed
                });
                if rolled_back {
                    Err(format!(
                        "backup restore failed: {error}; restored the safety snapshot"
                    ))
                } else {
                    Err(format!(
                        "backup restore failed: {error}; safety rollback failed"
                    ))
                }
            }
        }
    })();
    match &mut result {
        Ok(receipt) => {
            receipt.warnings = record_success(
                &operation_id,
                OperationAction::RestoreBackup,
                started,
                &backups,
                BTreeMap::from([("restoredFiles".to_string(), receipt.result.restored_files)]),
            )
            .into_iter()
            .collect();
        }
        Err(error) => {
            let _ = append_operation_record(
                &operation_id,
                OperationAction::RestoreBackup,
                failure_status.unwrap_or_else(|| terminal_status(error)),
                started,
                &backups,
                BTreeMap::new(),
            );
        }
    }
    result
}

#[tauri::command]
pub fn list_operation_records(limit: Option<usize>) -> Result<Vec<OperationRecord>, String> {
    operation_log()?.list(limit.unwrap_or(100).min(1_000))
}

#[tauri::command]
pub fn list_skills() -> Result<Vec<SkillStatus>, String> {
    list_skills_at(&skill_codex_home()?, &appdata_root()?)
}

#[tauri::command]
pub fn install_skill(
    skill_id: SkillId,
    confirm_replace: bool,
) -> Result<SkillMutationReceipt, String> {
    let _mutation_guard = acquire_mutation_lock()?;
    let started_at_ms = timestamp_millis()?;
    let attempt_id = operation_id("install-skill-attempt")?;
    let result = (|| {
        ensure_codex_closed("installing or updating a skill")?;
        install_skill_at(
            &skill_codex_home()?,
            &appdata_root()?,
            skill_id,
            confirm_replace,
        )
    })();
    finish_skill_operation(
        attempt_id,
        OperationAction::InstallSkill,
        started_at_ms,
        result,
    )
}

#[tauri::command]
pub fn save_skill_config(input: SkillConfigInput) -> Result<SkillMutationReceipt, String> {
    let _mutation_guard = acquire_mutation_lock()?;
    let started_at_ms = timestamp_millis()?;
    let attempt_id = operation_id("configure-skill-attempt")?;
    let result = (|| {
        ensure_codex_closed("configuring a skill")?;
        save_skill_config_at(&skill_codex_home()?, &appdata_root()?, input)
    })();
    finish_skill_operation(
        attempt_id,
        OperationAction::ConfigureSkill,
        started_at_ms,
        result,
    )
}

fn default_codex_home() -> PathBuf {
    default_codex_home_from_env(
        std::env::var_os("CODEX_HOME"),
        std::env::var_os("USERPROFILE"),
        std::env::var_os("HOME"),
    )
}

fn skill_codex_home() -> Result<PathBuf, String> {
    let home = default_codex_home();
    if home.is_absolute() {
        Ok(home)
    } else {
        Err("CODEX_HOME must resolve to an absolute path for skill operations".to_string())
    }
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

fn appdata_root() -> Result<PathBuf, String> {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| "APPDATA is not set".to_string())
}

fn default_backup_root() -> Result<PathBuf, String> {
    Ok(appdata_root()?.join("codex-switch").join("backups"))
}

fn default_shared_sessions_root() -> Result<PathBuf, String> {
    Ok(appdata_root()?.join("codex-switch").join("shared-sessions"))
}

fn operation_log() -> Result<OperationLog, String> {
    Ok(OperationLog::from_appdata(&appdata_root()?))
}

fn acquire_mutation_lock() -> Result<MutationGuard, String> {
    let lock_path = appdata_root()?.join("codex-switch").join("mutation.lock");
    acquire_mutation_lock_at(&lock_path)
}

fn acquire_mutation_lock_at(lock_path: &Path) -> Result<MutationGuard, String> {
    let process_guard = match MUTATION_LOCK.try_lock() {
        Ok(guard) => Ok(guard),
        Err(TryLockError::WouldBlock) => {
            Err("another Codex Switch mutation is already in progress".to_string())
        }
        Err(TryLockError::Poisoned(error)) => Ok(error.into_inner()),
    }?;
    let lock_file = open_mutation_lock_file(lock_path)?;
    Ok(MutationGuard {
        _process_guard: process_guard,
        _lock_file: lock_file,
    })
}

fn open_mutation_lock_file(lock_path: &Path) -> Result<File, String> {
    let parent = lock_path
        .parent()
        .ok_or_else(|| "mutation lock path has no parent directory".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("failed to create mutation lock directory: {error}"))?;

    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(windows)]
    options.share_mode(0);

    options.open(lock_path).map_err(|error| {
        if matches!(error.raw_os_error(), Some(32 | 33)) {
            "another Codex Switch mutation is already in progress".to_string()
        } else {
            format!("failed to acquire the Codex Switch mutation lock: {error}")
        }
    })
}

fn ensure_codex_closed(action: &str) -> Result<(), String> {
    if list_processes()?.is_empty() {
        Ok(())
    } else {
        Err(format!("Codex is still running; close it before {action}"))
    }
}

fn validate_backup_selection(backup_root: &Path, selected: &Path) -> Result<PathBuf, String> {
    let root = std::fs::canonicalize(backup_root)
        .map_err(|_| "backup storage is not available".to_string())?;
    let selected = std::fs::canonicalize(selected)
        .map_err(|_| "selected backup does not exist".to_string())?;
    if selected.parent() != Some(root.as_path()) || !selected.is_dir() {
        return Err("selected backup is outside the managed backup root".to_string());
    }
    Ok(selected)
}

fn compensate_failed_hot_sync(
    _current_home: &Path,
    current_backup: &BackupManifest,
    shared_home: &Path,
    shared_backup: &BackupManifest,
) -> HotSyncCompensation {
    HotSyncCompensation {
        shared_rolled_back: restore_backup_snapshot(&shared_backup.backup_dir, shared_home).is_ok(),
        current_backup_dir: current_backup.backup_dir.clone(),
    }
}

fn sync_counts(result: &SessionSyncResult) -> BTreeMap<String, usize> {
    BTreeMap::from([
        ("insertedThreads".to_string(), result.inserted_threads),
        (
            "copiedSessionFiles".to_string(),
            result.copied_session_files,
        ),
        ("duplicateThreads".to_string(), result.duplicate_threads),
        (
            "skippedArchivedThreads".to_string(),
            result.skipped_archived_threads,
        ),
    ])
}

fn mutation_counts(result: &SessionMutationResult) -> BTreeMap<String, usize> {
    BTreeMap::from([
        ("selectedCount".to_string(), result.selected_count),
        ("deletedThreads".to_string(), result.deleted_threads),
        (
            "deletedSessionFiles".to_string(),
            result.deleted_session_files,
        ),
        ("restoredThreads".to_string(), result.restored_threads),
    ])
}

fn finish_session_mutation(
    operation_id: String,
    action: OperationAction,
    started_at_ms: u128,
    result: Result<SessionMutationResult, String>,
    failure_backups: &[BackupManifest],
) -> Result<SessionMutationReceipt, String> {
    match result {
        Ok(result) => {
            let warnings = record_success(
                &operation_id,
                action,
                started_at_ms,
                &result.backups,
                mutation_counts(&result),
            )
            .into_iter()
            .collect();
            Ok(SessionMutationReceipt {
                operation_id,
                result,
                rolled_back: false,
                warnings,
            })
        }
        Err(error) => {
            let status = terminal_status(&error);
            let _ = append_operation_record(
                &operation_id,
                action,
                status,
                started_at_ms,
                failure_backups,
                BTreeMap::new(),
            );
            Err(error)
        }
    }
}

fn record_result<T>(
    operation_id: &str,
    action: OperationAction,
    started_at_ms: u128,
    result: &Result<T, String>,
    backups: &[BackupManifest],
    counts: BTreeMap<String, usize>,
) {
    let Ok(log) = operation_log() else {
        return;
    };
    let _ = record_result_to_log(
        &log,
        operation_id,
        action,
        started_at_ms,
        result,
        backups,
        counts,
    );
}

fn record_result_to_log<T>(
    log: &OperationLog,
    operation_id: &str,
    action: OperationAction,
    started_at_ms: u128,
    result: &Result<T, String>,
    backups: &[BackupManifest],
    counts: BTreeMap<String, usize>,
) -> Result<(), String> {
    let status = match result {
        Ok(_) => OperationStatus::Succeeded,
        Err(error) => terminal_status(error),
    };
    append_operation_record_to(
        log,
        operation_id,
        action,
        status,
        started_at_ms,
        backups,
        counts,
    )
}

fn record_success(
    operation_id: &str,
    action: OperationAction,
    started_at_ms: u128,
    backups: &[BackupManifest],
    counts: BTreeMap<String, usize>,
) -> Option<String> {
    append_operation_record(
        operation_id,
        action,
        OperationStatus::Succeeded,
        started_at_ms,
        backups,
        counts,
    )
    .err()
    .map(|_| "操作已成功，但本地操作记录写入失败".to_string())
}

fn terminal_status(error: &str) -> OperationStatus {
    if error.contains("rollback failed") {
        OperationStatus::RollbackFailed
    } else if error.contains("rolled back") {
        OperationStatus::RolledBack
    } else {
        OperationStatus::Failed
    }
}

fn append_operation_record(
    operation_id: &str,
    action: OperationAction,
    status: OperationStatus,
    started_at_ms: u128,
    backups: &[BackupManifest],
    counts: BTreeMap<String, usize>,
) -> Result<(), String> {
    append_operation_record_to(
        &operation_log()?,
        operation_id,
        action,
        status,
        started_at_ms,
        backups,
        counts,
    )
}

fn append_operation_record_to(
    log: &OperationLog,
    operation_id: &str,
    action: OperationAction,
    status: OperationStatus,
    started_at_ms: u128,
    backups: &[BackupManifest],
    counts: BTreeMap<String, usize>,
) -> Result<(), String> {
    let phase = match status {
        OperationStatus::Succeeded => OperationPhase::Complete,
        OperationStatus::Failed => OperationPhase::Apply,
        OperationStatus::RolledBack | OperationStatus::RollbackFailed => OperationPhase::Rollback,
    };
    log.append(&OperationRecord {
        operation_id: operation_id.to_string(),
        action,
        status,
        phase,
        started_at_ms,
        completed_at_ms: timestamp_millis()?,
        backup_dirs: backups
            .iter()
            .map(|backup| backup.backup_dir.clone())
            .collect(),
        counts,
    })
}

fn finish_skill_operation(
    attempt_id: String,
    action: OperationAction,
    started_at_ms: u128,
    result: Result<SkillMutationReceipt, String>,
) -> Result<SkillMutationReceipt, String> {
    match result {
        Ok(mut receipt) => {
            let record = OperationRecord {
                operation_id: receipt.operation_id.clone(),
                action,
                status: OperationStatus::Succeeded,
                phase: OperationPhase::Complete,
                started_at_ms,
                completed_at_ms: timestamp_millis()?,
                backup_dirs: receipt.backup_dir.clone().into_iter().collect(),
                counts: BTreeMap::from([("skillsChanged".to_string(), 1)]),
            };
            if operation_log().and_then(|log| log.append(&record)).is_err() {
                receipt
                    .warnings
                    .push("操作已成功，但本地操作记录写入失败".to_string());
            }
            Ok(receipt)
        }
        Err(error) => {
            let status = terminal_status(&error);
            let phase = match status {
                OperationStatus::Failed => OperationPhase::Apply,
                OperationStatus::RolledBack | OperationStatus::RollbackFailed => {
                    OperationPhase::Rollback
                }
                OperationStatus::Succeeded => OperationPhase::Complete,
            };
            if let Ok(log) = operation_log() {
                let _ = log.append(&OperationRecord {
                    operation_id: attempt_id,
                    action,
                    status,
                    phase,
                    started_at_ms,
                    completed_at_ms: timestamp_millis()?,
                    backup_dirs: Vec::new(),
                    counts: BTreeMap::new(),
                });
            }
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use crate::{
        backup::{create_backup, create_local_backup},
        operation_log::{OperationAction, OperationLog, OperationStatus},
    };

    use super::{
        acquire_mutation_lock_at, compensate_failed_hot_sync, default_codex_home_from_env,
        get_app_status, record_result_to_log, validate_backup_selection,
    };

    #[cfg(windows)]
    use super::open_mutation_lock_file;

    #[test]
    fn app_status_does_not_report_the_retired_scaffold_phase() {
        assert_eq!(get_app_status().phase, "hardened-mvp");
        assert_eq!(get_app_status().version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn mutation_lock_rejects_overlapping_commands_and_recovers_after_release() {
        let root = tempdir().unwrap();
        let lock_path = root.path().join("mutation.lock");
        let first = acquire_mutation_lock_at(&lock_path).unwrap();

        let error = acquire_mutation_lock_at(&lock_path).unwrap_err();
        assert!(error.contains("already in progress"), "{error}");

        drop(first);
        assert!(acquire_mutation_lock_at(&lock_path).is_ok());
    }

    #[test]
    #[cfg(windows)]
    fn mutation_lock_file_is_exclusive_and_released_with_its_handle() {
        let root = tempdir().unwrap();
        let lock_path = root.path().join("mutation.lock");
        let first = open_mutation_lock_file(&lock_path).unwrap();

        let error = open_mutation_lock_file(&lock_path).unwrap_err();
        assert!(error.contains("already in progress"), "{error}");

        drop(first);
        assert!(open_mutation_lock_file(&lock_path).is_ok());
    }

    #[test]
    fn terminal_record_retains_backups_created_before_an_early_failure() {
        let home = tempdir().unwrap();
        let backup_root = tempdir().unwrap();
        let log_root = tempdir().unwrap();
        fs::write(home.path().join("config.toml"), "model = \"test\"\n").unwrap();
        let backup = create_backup(home.path(), backup_root.path(), "partial-attempt").unwrap();
        let log = OperationLog::new(log_root.path().join("operations.jsonl"));
        let result: Result<(), String> = Err("later preflight failed".to_string());

        record_result_to_log(
            &log,
            "partial-attempt",
            OperationAction::RestoreBackup,
            1,
            &result,
            std::slice::from_ref(&backup),
            std::collections::BTreeMap::new(),
        )
        .unwrap();

        let records = log.list(10).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].status, OperationStatus::Failed);
        assert_eq!(records[0].backup_dirs, vec![backup.backup_dir]);
    }

    #[test]
    fn resolves_default_codex_home_from_environment_without_hardcoded_user() {
        let codex_home = default_codex_home_from_env(
            None,
            Some(std::ffi::OsString::from(r"C:\Users\alice")),
            Some(std::ffi::OsString::from(r"C:\Users\ignored")),
        );
        assert_eq!(
            codex_home,
            std::path::PathBuf::from(r"C:\Users\alice").join(".codex")
        );
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
    fn backup_selection_rejects_nested_and_outside_paths() {
        let root = tempdir().unwrap();
        let valid = root.path().join("valid-backup");
        let nested = valid.join("payload");
        fs::create_dir_all(&nested).unwrap();
        let outside = tempdir().unwrap();

        assert_eq!(
            validate_backup_selection(root.path(), &valid).unwrap(),
            fs::canonicalize(&valid).unwrap()
        );
        assert!(validate_backup_selection(root.path(), &nested).is_err());
        assert!(validate_backup_selection(root.path(), outside.path()).is_err());
    }

    #[test]
    fn failed_hot_sync_preserves_concurrent_live_changes_and_restores_only_shared() {
        let current = tempdir().unwrap();
        let shared = tempdir().unwrap();
        let backups = tempdir().unwrap();
        let current_session = current.path().join("sessions/2026/07/13/current.jsonl");
        let shared_session = shared.path().join("sessions/2026/07/13/shared.jsonl");
        fs::create_dir_all(current_session.parent().unwrap()).unwrap();
        fs::create_dir_all(shared_session.parent().unwrap()).unwrap();
        fs::write(&current_session, "current-before\n").unwrap();
        fs::write(&shared_session, "shared-before\n").unwrap();
        let current_backup =
            create_backup(current.path(), backups.path(), "current-before-sync").unwrap();
        let shared_backup =
            create_local_backup(shared.path(), backups.path(), "shared-before-sync").unwrap();

        let concurrent_session = current.path().join("sessions/2026/07/13/concurrent.jsonl");
        fs::write(&concurrent_session, "created-while-sync-was-running\n").unwrap();
        fs::write(&shared_session, "shared-mutated\n").unwrap();

        let compensation = compensate_failed_hot_sync(
            current.path(),
            &current_backup,
            shared.path(),
            &shared_backup,
        );

        assert!(compensation.shared_rolled_back);
        assert!(concurrent_session.exists());
        assert_eq!(
            fs::read_to_string(&shared_session).unwrap(),
            "shared-before\n"
        );
    }
}

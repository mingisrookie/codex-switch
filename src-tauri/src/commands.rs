use serde::Serialize;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex, MutexGuard, TryLockError,
    },
    thread,
    time::Duration,
};
use tauri::ipc::Channel;

#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;

use crate::{
    backup::{
        create_backup as create_backup_snapshot, create_local_backup, create_local_session_backup,
        create_session_backup, list_recent_backups as list_backup_snapshots,
        migrate_legacy_plaintext_auth, preflight_runtime_backup_capacity,
        restore_backup as restore_backup_snapshot, verify_backup, BackupManifest, BackupSummary,
        RestoreResult,
    },
    codex_home::{scan_codex_home as scan_home, CodexHomeStatus},
    codex_paths::validate_absolute_root,
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
    runtime_switcher::{
        preflight_runtime_session_sync, preflight_runtime_switch,
        switch_runtime_files_preflighted_with_progress, sync_home_with_shared,
        RuntimeSwitchOutcome, RuntimeSwitchPhase, RuntimeSwitchResult,
    },
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

static MUTATION_COORDINATOR: MutationCoordinator = MutationCoordinator::new();

#[derive(Debug)]
struct MutationCoordinator {
    process_lock: Mutex<()>,
    shutdown_pending: AtomicBool,
    shutdown_lock_file: Mutex<Option<File>>,
}

impl MutationCoordinator {
    const fn new() -> Self {
        Self {
            process_lock: Mutex::new(()),
            shutdown_pending: AtomicBool::new(false),
            shutdown_lock_file: Mutex::new(None),
        }
    }

    fn acquire<'a>(&'a self, lock_path: &Path) -> Result<MutationGuard<'a>, String> {
        if self.shutdown_pending.load(Ordering::Acquire) {
            return Err(mutation_busy_error());
        }
        let process_guard = match self.process_lock.try_lock() {
            Ok(guard) => Ok(guard),
            Err(TryLockError::WouldBlock) => Err(mutation_busy_error()),
            Err(TryLockError::Poisoned(error)) => Ok(error.into_inner()),
        }?;
        if self.shutdown_pending.load(Ordering::Acquire) {
            return Err(mutation_busy_error());
        }
        let lock_file = open_mutation_lock_file(lock_path)?;
        Ok(MutationGuard {
            coordinator: self,
            _process_guard: process_guard,
            lock_file: Some(lock_file),
        })
    }

    fn blocks_shutdown(&self) -> bool {
        if self.shutdown_pending.load(Ordering::Acquire) {
            return false;
        }
        matches!(self.process_lock.try_lock(), Err(TryLockError::WouldBlock))
    }
}

#[derive(Debug)]
struct MutationGuard<'a> {
    coordinator: &'a MutationCoordinator,
    _process_guard: MutexGuard<'a, ()>,
    lock_file: Option<File>,
}

impl MutationGuard<'_> {
    fn hold_until_process_exit(mut self) {
        let lock_file = self
            .lock_file
            .take()
            .expect("mutation guard must own its lock file");
        let mut shutdown_lock_file = self
            .coordinator
            .shutdown_lock_file
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *shutdown_lock_file = Some(lock_file);
        self.coordinator
            .shutdown_pending
            .store(true, Ordering::Release);
    }
}

fn mutation_busy_error() -> String {
    "another ChatGPT Switch mutation is already in progress".to_string()
}

pub(crate) fn mutation_blocks_shutdown() -> bool {
    MUTATION_COORDINATOR.blocks_shutdown()
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

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeSwitchProgress {
    pub phase: RuntimeSwitchPhase,
    pub timestamp_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<RuntimeSwitchOutcome>,
}

#[derive(Debug, Clone)]
struct HotSyncCompensation {
    shared_rolled_back: bool,
    current_backup_dir: PathBuf,
}

#[tauri::command]
pub fn get_app_status() -> AppStatus {
    AppStatus {
        app_name: "ChatGPT Switch",
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
    let receipt = tauri::async_runtime::spawn_blocking(|| {
        let mutation_guard = acquire_mutation_lock()?;
        let result = install_latest_update();
        if result.is_ok() {
            mutation_guard.hold_until_process_exit();
        }
        result
    })
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
    scan_home(&managed_codex_home()?)
}

#[tauri::command]
pub fn scan_sessions() -> Result<SessionInventory, String> {
    scan_session_inventory(&managed_codex_home()?)
}

#[tauri::command]
pub fn scan_managed_sessions() -> Result<ManagedSessionInventory, String> {
    let shared_home = default_shared_sessions_root()?;
    scan_managed_session_inventory(&managed_codex_home()?, &shared_home)
}

#[tauri::command]
pub fn dry_run_all_sessions() -> Result<AllSessionsDryRun, String> {
    let current = scan_session_inventory(&managed_codex_home()?)?;
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
    RuntimeStore::from_default_root()?.detect_active_runtime(&managed_codex_home()?)
}

#[tauri::command]
pub fn import_plus_runtime(confirm_overwrite: bool) -> Result<RuntimeMetadata, String> {
    let _mutation_guard = acquire_mutation_lock()?;
    let started = timestamp_millis()?;
    let id = operation_id("import-account")?;
    let result = (|| {
        RuntimeStore::from_default_root()?
            .import_plus_from_home(&managed_codex_home()?, confirm_overwrite)
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
        (|| RuntimeStore::from_default_root()?.upsert_relay(input, &managed_codex_home()?))();
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
pub async fn close_codex_processes() -> Result<Vec<CodexProcess>, String> {
    tauri::async_runtime::spawn_blocking(close_codex)
        .await
        .map_err(|_| "ChatGPT process close worker failed".to_string())?
}

#[tauri::command]
pub async fn switch_runtime(
    runtime_id: String,
    on_progress: Channel<RuntimeSwitchProgress>,
) -> Result<RuntimeSwitchResult, String> {
    let worker_progress = on_progress.clone();
    match tauri::async_runtime::spawn_blocking(move || {
        switch_runtime_blocking(runtime_id, worker_progress)
    })
    .await
    {
        Ok(result) => result,
        Err(_) => {
            let error = "runtime switch worker failed".to_string();
            emit_runtime_switch_failure(
                &on_progress,
                &error,
                RuntimeSwitchOutcome::FailedBeforeWrite,
            );
            Err(error)
        }
    }
}

fn switch_runtime_blocking(
    runtime_id: String,
    on_progress: Channel<RuntimeSwitchProgress>,
) -> Result<RuntimeSwitchResult, String> {
    let _mutation_guard = match acquire_mutation_lock() {
        Ok(guard) => guard,
        Err(error) => {
            emit_runtime_switch_failure(
                &on_progress,
                &error,
                RuntimeSwitchOutcome::FailedBeforeWrite,
            );
            return Err(error);
        }
    };
    let started = match timestamp_millis() {
        Ok(started) => started,
        Err(error) => {
            emit_runtime_switch_failure(
                &on_progress,
                &error,
                RuntimeSwitchOutcome::FailedBeforeWrite,
            );
            return Err(error);
        }
    };
    let attempt_id = match operation_id("switch-runtime-attempt") {
        Ok(attempt_id) => attempt_id,
        Err(error) => {
            emit_runtime_switch_failure(
                &on_progress,
                &error,
                RuntimeSwitchOutcome::FailedBeforeWrite,
            );
            return Err(error);
        }
    };
    let mut failure_backups = Vec::new();
    let mut failure_outcome = RuntimeSwitchOutcome::FailedBeforeWrite;
    let result = (|| {
        let store = RuntimeStore::from_default_root()?;
        let current_home = managed_codex_home()?;
        let backup_root = default_backup_root()?;
        let shared_home = default_shared_sessions_root()?;
        let plan = prepare_runtime_switch_before_close(
            runtime_id == RELAY_RUNTIME_ID,
            || {
                let plan = preflight_runtime_switch(&store, &runtime_id, &current_home)?;
                let requires_change = plan.requires_change();
                Ok((plan, requires_change))
            },
            || {
                preflight_runtime_backup_capacity(&backup_root, &current_home, &shared_home)?;
                preflight_runtime_session_sync(&current_home, &shared_home)
            },
            || {
                let connection = store.load_relay_connection()?;
                verify_relay(&connection.base_url, &connection.api_key)?;
                store.mark_verified(RELAY_RUNTIME_ID)?;
                Ok(())
            },
            list_processes,
            || close_codex().map(|_| ()),
            |phase, message| emit_runtime_switch_progress(&on_progress, phase, message),
        )?;
        match switch_runtime_files_preflighted_with_progress(
            &store,
            &current_home,
            &backup_root,
            &shared_home,
            plan,
            &mut |phase| emit_runtime_switch_progress(&on_progress, phase, None),
        ) {
            Ok(receipt) => Ok(receipt),
            Err(failure) => {
                failure_backups = failure.backups;
                failure_outcome = failure.outcome;
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
    emit_runtime_switch_terminal(&on_progress, &result, failure_outcome);
    result
}

fn prepare_runtime_switch_before_close<Plan, Prepare, Capacity, Verify, List, Close, Progress>(
    verify_relay_first: bool,
    mut prepare_switch: Prepare,
    mut verify_backup_capacity: Capacity,
    mut verify_relay_connection: Verify,
    list_managed_processes: List,
    close_managed_processes: Close,
    mut progress: Progress,
) -> Result<Plan, String>
where
    Prepare: FnMut() -> Result<(Plan, bool), String>,
    Capacity: FnMut() -> Result<(), String>,
    Verify: FnMut() -> Result<(), String>,
    List: FnMut() -> Result<Vec<CodexProcess>, String>,
    Close: FnMut() -> Result<(), String>,
    Progress: FnMut(RuntimeSwitchPhase, Option<String>),
{
    let (plan, requires_change) = prepare_switch()?;
    if requires_change {
        verify_backup_capacity()?;
        run_runtime_switch_preflight(
            verify_relay_first,
            verify_relay_connection,
            list_managed_processes,
            close_managed_processes,
            progress,
        )?;
    } else if verify_relay_first {
        progress(RuntimeSwitchPhase::VerifyingRelay, None);
        verify_relay_connection()?;
    }
    Ok(plan)
}

fn run_runtime_switch_preflight<Verify, List, Close, Progress>(
    verify_relay_first: bool,
    mut verify_relay_connection: Verify,
    mut list_managed_processes: List,
    mut close_managed_processes: Close,
    mut progress: Progress,
) -> Result<(), String>
where
    Verify: FnMut() -> Result<(), String>,
    List: FnMut() -> Result<Vec<CodexProcess>, String>,
    Close: FnMut() -> Result<(), String>,
    Progress: FnMut(RuntimeSwitchPhase, Option<String>),
{
    if verify_relay_first {
        progress(RuntimeSwitchPhase::VerifyingRelay, None);
        verify_relay_connection()?;
    }
    progress(RuntimeSwitchPhase::DetectingApp, None);
    let processes = list_managed_processes()?;
    if !processes.is_empty() {
        progress(
            RuntimeSwitchPhase::ClosingApp,
            Some(format!("Closing {} ChatGPT process(es)", processes.len())),
        );
        close_managed_processes()?;
    }
    if list_managed_processes()?.is_empty() {
        Ok(())
    } else {
        Err("ChatGPT is still running; close it before switching runtimes".to_string())
    }
}

fn emit_runtime_switch_progress(
    on_progress: &Channel<RuntimeSwitchProgress>,
    phase: RuntimeSwitchPhase,
    message: Option<String>,
) {
    emit_runtime_switch_progress_event(on_progress, phase, message, None);
}

fn emit_runtime_switch_progress_event(
    on_progress: &Channel<RuntimeSwitchProgress>,
    phase: RuntimeSwitchPhase,
    message: Option<String>,
    outcome: Option<RuntimeSwitchOutcome>,
) {
    let _ = on_progress.send(RuntimeSwitchProgress {
        phase,
        timestamp_ms: timestamp_millis().unwrap_or_default(),
        message,
        outcome,
    });
}

fn emit_runtime_switch_failure(
    on_progress: &Channel<RuntimeSwitchProgress>,
    error: &str,
    outcome: RuntimeSwitchOutcome,
) {
    emit_runtime_switch_progress_event(
        on_progress,
        RuntimeSwitchPhase::Failed,
        Some(error.to_string()),
        Some(outcome),
    );
}

fn emit_runtime_switch_terminal<T>(
    on_progress: &Channel<RuntimeSwitchProgress>,
    result: &Result<T, String>,
    failure_outcome: RuntimeSwitchOutcome,
) {
    match result {
        Ok(_) => {
            emit_runtime_switch_progress(on_progress, RuntimeSwitchPhase::Complete, None);
        }
        Err(error) => {
            emit_runtime_switch_failure(on_progress, error, failure_outcome);
        }
    }
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
        let current_home = managed_codex_home()?;
        let current_backup = create_session_backup(&current_home, &backup_root, "sync-current")?;
        backups.push(current_backup.clone());
        let shared_backup = create_local_session_backup(&shared_home, &backup_root, "sync-shared")?;
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
            &managed_codex_home()?,
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
        match restore_visible(&managed_codex_home()?, &backup_root, &ids) {
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
        let current_home = managed_codex_home()?;
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
    managed_codex_home()
}

fn managed_codex_home() -> Result<PathBuf, String> {
    validate_absolute_root(&default_codex_home(), "CODEX_HOME")
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
    let root = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| "APPDATA is not set".to_string())?;
    validate_absolute_root(&root, "APPDATA")
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

fn acquire_mutation_lock() -> Result<MutationGuard<'static>, String> {
    let lock_path = appdata_root()?.join("codex-switch").join("mutation.lock");
    acquire_mutation_lock_at(&lock_path)
}

fn acquire_mutation_lock_at(lock_path: &Path) -> Result<MutationGuard<'static>, String> {
    MUTATION_COORDINATOR.acquire(lock_path)
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
            "another ChatGPT Switch mutation is already in progress".to_string()
        } else {
            format!("failed to acquire the ChatGPT Switch mutation lock: {error}")
        }
    })
}

fn ensure_codex_closed(action: &str) -> Result<(), String> {
    if list_processes()?.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "ChatGPT is still running; close it before {action}"
        ))
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
    use std::{
        cell::{Cell, RefCell},
        collections::VecDeque,
        fs,
        future::Future,
        sync::{Arc, Mutex},
    };

    use tauri::ipc::{Channel, InvokeResponseBody};
    use tempfile::tempdir;

    use crate::{
        backup::{create_backup, create_local_session_backup, create_session_backup, BackupScope},
        operation_log::{OperationAction, OperationLog, OperationStatus},
        process_control::CodexProcess,
    };

    use super::{
        acquire_mutation_lock_at, close_codex_processes, compensate_failed_hot_sync,
        default_codex_home_from_env, emit_runtime_switch_terminal, get_app_status,
        prepare_runtime_switch_before_close, record_result_to_log, run_runtime_switch_preflight,
        switch_runtime, validate_backup_selection, MutationCoordinator, RuntimeSwitchOutcome,
        RuntimeSwitchPhase, RuntimeSwitchProgress, RuntimeSwitchResult,
    };

    #[cfg(windows)]
    use super::open_mutation_lock_file;

    #[test]
    fn app_status_does_not_report_the_retired_scaffold_phase() {
        assert_eq!(get_app_status().app_name, "ChatGPT Switch");
        assert_eq!(get_app_status().phase, "hardened-mvp");
        assert_eq!(get_app_status().version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn runtime_switch_command_remains_async() {
        fn assert_future<F>(future: F)
        where
            F: Future<Output = Result<RuntimeSwitchResult, String>> + Send,
        {
            drop(future);
        }

        let on_progress = Channel::<RuntimeSwitchProgress>::new(|_| Ok(()));
        assert_future(switch_runtime("relay".to_string(), on_progress));
    }

    #[test]
    fn process_close_command_remains_async() {
        fn assert_future<F>(future: F)
        where
            F: Future<Output = Result<Vec<CodexProcess>, String>> + Send,
        {
            drop(future);
        }

        assert_future(close_codex_processes());
    }

    #[test]
    fn runtime_switch_terminal_progress_is_typed_for_success_and_failure() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        let on_progress = Channel::<RuntimeSwitchProgress>::new(move |body| {
            let InvokeResponseBody::Json(json) = body else {
                panic!("runtime switch progress must be JSON");
            };
            captured
                .lock()
                .unwrap()
                .push(serde_json::from_str::<serde_json::Value>(&json).unwrap());
            Ok(())
        });
        let success: Result<(), String> = Ok(());
        emit_runtime_switch_terminal(
            &on_progress,
            &success,
            RuntimeSwitchOutcome::FailedBeforeWrite,
        );
        let failure: Result<(), String> = Err("switch failed safely".to_string());
        emit_runtime_switch_terminal(
            &on_progress,
            &failure,
            RuntimeSwitchOutcome::FailedBeforeWrite,
        );
        let rolled_back: Result<(), String> = Err("opaque switch failure".to_string());
        emit_runtime_switch_terminal(&on_progress, &rolled_back, RuntimeSwitchOutcome::RolledBack);
        let rollback_failed: Result<(), String> = Err("another opaque switch failure".to_string());
        emit_runtime_switch_terminal(
            &on_progress,
            &rollback_failed,
            RuntimeSwitchOutcome::RollbackFailed,
        );

        let events = events.lock().unwrap();
        assert_eq!(events.len(), 4);
        assert_eq!(events[0]["phase"], "complete");
        assert!(events[0]["timestampMs"].as_u64().unwrap() > 0);
        assert!(events[0].get("message").is_none());
        assert!(events[0].get("outcome").is_none());
        assert_eq!(events[1]["phase"], "failed");
        assert_eq!(events[1]["message"], "switch failed safely");
        assert_eq!(events[1]["outcome"], "failedBeforeWrite");
        assert!(events[1]["timestampMs"].as_u64().unwrap() > 0);
        assert_eq!(events[2]["outcome"], "rolledBack");
        assert_eq!(events[3]["outcome"], "rollbackFailed");
    }

    #[test]
    fn relay_preflight_verifies_before_process_detection_and_never_closes_on_failure() {
        let list_calls = Cell::new(0);
        let close_calls = Cell::new(0);
        let mut phases = Vec::new();

        let error = run_runtime_switch_preflight(
            true,
            || Err("relay verification failed".to_string()),
            || {
                list_calls.set(list_calls.get() + 1);
                Ok(Vec::new())
            },
            || {
                close_calls.set(close_calls.get() + 1);
                Ok(())
            },
            |phase, _| phases.push(phase),
        )
        .unwrap_err();

        assert_eq!(error, "relay verification failed");
        assert_eq!(phases, vec![RuntimeSwitchPhase::VerifyingRelay]);
        assert_eq!(list_calls.get(), 0);
        assert_eq!(close_calls.get(), 0);
    }

    #[test]
    fn relay_and_account_preflight_emit_real_process_phases_in_order() {
        let root = CodexProcess {
            image_name: "ChatGPT.exe".to_string(),
            pid: 1234,
            parent_pid: 42,
            creation_time_100ns: Some(1),
        };
        let mut relay_listings = VecDeque::from([Ok(vec![root]), Ok(Vec::new())]);
        let relay_closed = Cell::new(false);
        let mut relay_phases = Vec::new();
        run_runtime_switch_preflight(
            true,
            || Ok(()),
            || relay_listings.pop_front().expect("unexpected listing"),
            || {
                relay_closed.set(true);
                Ok(())
            },
            |phase, _| relay_phases.push(phase),
        )
        .unwrap();
        assert!(relay_closed.get());
        assert_eq!(
            relay_phases,
            vec![
                RuntimeSwitchPhase::VerifyingRelay,
                RuntimeSwitchPhase::DetectingApp,
                RuntimeSwitchPhase::ClosingApp,
            ]
        );

        let mut account_phases = Vec::new();
        run_runtime_switch_preflight(
            false,
            || panic!("account mode must not verify relay"),
            || Ok(Vec::new()),
            || panic!("there is no managed process to close"),
            |phase, _| account_phases.push(phase),
        )
        .unwrap();
        assert_eq!(account_phases, vec![RuntimeSwitchPhase::DetectingApp]);
    }

    #[test]
    fn runtime_plan_capacity_and_relay_checks_all_finish_before_chatgpt_closes() {
        let calls = RefCell::new(Vec::new());
        let root = CodexProcess {
            image_name: "ChatGPT.exe".to_string(),
            pid: 1234,
            parent_pid: 42,
            creation_time_100ns: Some(1),
        };
        let listings = RefCell::new(VecDeque::from([Ok(vec![root]), Ok(Vec::new())]));

        let plan = prepare_runtime_switch_before_close(
            true,
            || {
                calls.borrow_mut().push("config");
                Ok((true, true))
            },
            || {
                calls.borrow_mut().push("capacity");
                Ok(())
            },
            || {
                calls.borrow_mut().push("relay");
                Ok(())
            },
            || {
                calls.borrow_mut().push("list");
                listings
                    .borrow_mut()
                    .pop_front()
                    .expect("unexpected listing")
            },
            || {
                calls.borrow_mut().push("close");
                Ok(())
            },
            |phase, _| match phase {
                RuntimeSwitchPhase::VerifyingRelay => {
                    calls.borrow_mut().push("phase:verifyingRelay")
                }
                RuntimeSwitchPhase::DetectingApp => calls.borrow_mut().push("phase:detectingApp"),
                RuntimeSwitchPhase::ClosingApp => calls.borrow_mut().push("phase:closingApp"),
                _ => panic!("unexpected pre-close phase"),
            },
        )
        .unwrap();

        assert!(plan);
        assert_eq!(
            calls.into_inner(),
            vec![
                "config",
                "capacity",
                "phase:verifyingRelay",
                "relay",
                "phase:detectingApp",
                "list",
                "phase:closingApp",
                "close",
                "list",
            ]
        );
    }

    #[test]
    fn failed_config_or_capacity_preflight_never_touches_chatgpt() {
        let config_error = prepare_runtime_switch_before_close(
            true,
            || Err::<(bool, bool), _>("config preflight failed".to_string()),
            || panic!("capacity must not run after config failure"),
            || panic!("relay must not run after config failure"),
            || panic!("processes must not be listed after config failure"),
            || panic!("ChatGPT must not close after config failure"),
            |_, _| panic!("progress must not advance after config failure"),
        )
        .unwrap_err();
        assert_eq!(config_error, "config preflight failed");

        let capacity_error = prepare_runtime_switch_before_close(
            true,
            || Ok((true, true)),
            || Err("capacity preflight failed".to_string()),
            || panic!("relay must not run after capacity failure"),
            || panic!("processes must not be listed after capacity failure"),
            || panic!("ChatGPT must not close after capacity failure"),
            |_, _| panic!("process progress must not start after capacity failure"),
        )
        .unwrap_err();
        assert_eq!(capacity_error, "capacity preflight failed");
    }

    #[test]
    fn exact_account_no_op_skips_capacity_detection_and_close() {
        let plan = prepare_runtime_switch_before_close(
            false,
            || Ok((false, false)),
            || panic!("exact no-op must not scan backup capacity"),
            || panic!("account mode must not verify relay"),
            || panic!("exact no-op must not enumerate ChatGPT"),
            || panic!("exact no-op must not close ChatGPT"),
            |_, _| panic!("exact no-op has no pre-close process phase"),
        )
        .unwrap();

        assert!(!plan);
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
    fn successful_update_lock_enters_shutdown_pending_while_failure_releases() {
        let root = tempdir().unwrap();
        let lock_path = root.path().join("mutation.lock");
        let coordinator = MutationCoordinator::new();

        let regular_mutation = coordinator.acquire(&lock_path).unwrap();
        assert!(coordinator.blocks_shutdown());
        drop(regular_mutation);
        assert!(!coordinator.blocks_shutdown());
        assert!(coordinator.acquire(&lock_path).is_ok());

        let successful_attempt = coordinator.acquire(&lock_path).unwrap();
        assert!(coordinator.blocks_shutdown());
        successful_attempt.hold_until_process_exit();
        assert!(!coordinator.blocks_shutdown());
        let error = coordinator.acquire(&lock_path).unwrap_err();
        assert!(error.contains("already in progress"), "{error}");

        #[cfg(windows)]
        assert!(open_mutation_lock_file(&lock_path).is_err());
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
            create_session_backup(current.path(), backups.path(), "current-before-sync").unwrap();
        let shared_backup =
            create_local_session_backup(shared.path(), backups.path(), "shared-before-sync")
                .unwrap();
        assert_eq!(current_backup.scope, BackupScope::Sessions);
        assert_eq!(shared_backup.scope, BackupScope::Sessions);

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

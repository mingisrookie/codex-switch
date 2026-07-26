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
        cleanup_automatic_checkpoints as cleanup_checkpoint_storage, cleanup_transient_checkpoints,
        create_backup_with_paths, create_state_backup_with_paths, delete_verified_full_backup,
        inspect_checkpoint_storage as inspect_checkpoint_storage_at,
        list_recent_backups as list_backup_snapshots, migrate_legacy_plaintext_auth,
        preflight_backup_capacity_for_sources, preflight_backup_capacity_with_paths,
        restore_backup as restore_backup_snapshot, verify_backup, BackupCapacitySource,
        BackupManifest, BackupScope, BackupSummary, CheckpointCleanupReceipt,
        CheckpointCleanupSummary, CheckpointStorageStatus, RestoreResult,
    },
    chat_process_state::repair_after_chatgpt_shutdown,
    codex_home::{scan_codex_home as scan_home, CodexHomeStatus},
    codex_paths::{
        local_codex_paths, resolve_user_codex_paths, validate_absolute_root, CodexPaths,
    },
    operation_log::{
        operation_id, timestamp_millis, OperationAction, OperationLog, OperationPhase,
        OperationRecord, OperationStatus,
    },
    process_control::{
        close_codex_processes as close_codex, list_codex_processes as list_processes,
        list_standalone_codex_processes as list_standalone_processes, CodexProcess,
    },
    relay_verify::verify_relay,
    runtime_store::{
        RelayRuntimeInput, RuntimeMetadata, RuntimeStatus, RuntimeStore, RELAY_RUNTIME_ID,
    },
    runtime_switcher::{
        preflight_hot_session_sync_with_paths, preflight_runtime_session_sync_with_paths,
        preflight_runtime_switch, switch_runtime_files_preflighted_with_progress,
        sync_home_with_shared_preflighted_with_paths, BackupReceiptSummary, RuntimeSwitchOutcome,
        RuntimeSwitchPhase, RuntimeSwitchResult,
    },
    session_manager::{
        delete_managed_sessions_detailed_with_prepare as delete_sessions,
        restore_sessions_visible_detailed_with_prepare as restore_visible,
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
const MAX_LISTED_FULL_BACKUPS: usize = 256;

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
    pub backups: Vec<BackupReceiptSummary>,
    #[serde(flatten)]
    pub result: SessionSyncResult,
    pub rolled_back: bool,
    pub warnings: Vec<String>,
    pub checkpoint_cleanup: CheckpointCleanupSummary,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMutationReceipt {
    pub operation_id: String,
    #[serde(flatten)]
    pub result: SessionMutationReceiptResult,
    pub rolled_back: bool,
    pub warnings: Vec<String>,
    pub checkpoint_cleanup: CheckpointCleanupSummary,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMutationReceiptResult {
    pub selected_count: usize,
    pub backups: Vec<BackupReceiptSummary>,
    pub deleted_threads: usize,
    pub deleted_session_files: usize,
    pub removed_session_index_entries: usize,
    pub restored_threads: usize,
}

impl From<SessionMutationResult> for SessionMutationReceiptResult {
    fn from(result: SessionMutationResult) -> Self {
        Self {
            selected_count: result.selected_count,
            backups: result
                .backups
                .iter()
                .map(BackupReceiptSummary::from)
                .collect(),
            deleted_threads: result.deleted_threads,
            deleted_session_files: result.deleted_session_files,
            removed_session_index_entries: result.removed_session_index_entries,
            restored_threads: result.restored_threads,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreBackupReceipt {
    pub operation_id: String,
    #[serde(flatten)]
    pub result: RestoreResult,
    pub safety_backup: BackupReceiptSummary,
    pub rolled_back: bool,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateFullBackupReceipt {
    pub operation_id: String,
    pub backups: Vec<BackupReceiptSummary>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupDeleteReceipt {
    pub operation_id: String,
    pub backup_dir: PathBuf,
    pub reclaimed_bytes: u64,
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
        ensure_codex_closed("saving the account slot")?;
        RuntimeStore::from_default_root()?
            .import_plus_from_home(&managed_codex_home()?, confirm_overwrite)
    })();
    let _ = record_result(
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
    let _ = record_result(
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
        verify_relay(&connection.base_url, &connection.api_key, &connection.model)?;
        store.mark_verified(RELAY_RUNTIME_ID)
    })();
    let _ = record_result(
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
    let mut checkpoint_root = None;
    emit_runtime_switch_progress(&on_progress, RuntimeSwitchPhase::PlanningSessions, None);
    let mut result = (|| {
        let store = RuntimeStore::from_default_root()?;
        let current_home = managed_codex_home()?;
        let backup_root = default_backup_root()?;
        checkpoint_root = Some(backup_root.clone());
        let shared_home = default_shared_sessions_root()?;
        let shared_paths = local_codex_paths(&shared_home);
        let plan = prepare_runtime_switch_before_close(
            runtime_id == RELAY_RUNTIME_ID,
            || {
                let plan =
                    preflight_runtime_switch(&store, &runtime_id, &current_home, &shared_home)?;
                let requires_change = plan.requires_change();
                Ok((plan, requires_change))
            },
            |plan| {
                preflight_backup_capacity_for_sources(
                    &backup_root,
                    &[
                        BackupCapacitySource {
                            home: &current_home,
                            paths: plan.codex_paths(),
                            scope: plan.current_backup_scope(),
                        },
                        BackupCapacitySource {
                            home: &shared_home,
                            paths: &shared_paths,
                            scope: plan.shared_backup_scope(),
                        },
                    ],
                )?;
                preflight_runtime_session_sync_with_paths(plan.codex_paths(), &shared_paths)
            },
            || {
                let connection = store.load_relay_connection()?;
                verify_relay(&connection.base_url, &connection.api_key, &connection.model)?;
                store.mark_verified(RELAY_RUNTIME_ID)?;
                Ok(())
            },
            || list_managed_processes_for_closed_mutation("switching runtimes"),
            || close_codex().map(|_| ()),
            |phase, message| emit_runtime_switch_progress(&on_progress, phase, message),
        )?;
        if plan.requires_change() {
            repair_after_chatgpt_shutdown(&current_home)?;
        }
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
            receipt.backup_manifests.as_slice(),
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
    let terminal_recorded =
        record_runtime_switch_result(id, started, &result, backups, counts, failure_outcome)
            .is_ok();
    match &mut result {
        Ok(receipt) if receipt.changed => {
            if terminal_recorded {
                emit_runtime_switch_progress(
                    &on_progress,
                    RuntimeSwitchPhase::CleaningCheckpoints,
                    None,
                );
            }
            let cleanup = release_transient_checkpoints(
                checkpoint_root.as_deref(),
                terminal_recorded,
                receipt.backup_manifests.as_slice(),
            );
            receipt.warnings.extend(cleanup.warnings.clone());
            receipt.checkpoint_cleanup = cleanup;
        }
        Err(error)
            if failed_runtime_switch_checkpoints_are_releasable(
                failure_outcome,
                &failure_backups,
            ) =>
        {
            if terminal_recorded {
                emit_runtime_switch_progress(
                    &on_progress,
                    RuntimeSwitchPhase::CleaningCheckpoints,
                    None,
                );
            }
            let cleanup = release_transient_checkpoints(
                checkpoint_root.as_deref(),
                terminal_recorded,
                failure_backups.as_slice(),
            );
            append_warnings_to_error(error, &cleanup.warnings);
        }
        _ => {}
    }
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
    Capacity: FnMut(&Plan) -> Result<(), String>,
    Verify: FnMut() -> Result<(), String>,
    List: FnMut() -> Result<Vec<CodexProcess>, String>,
    Close: FnMut() -> Result<(), String>,
    Progress: FnMut(RuntimeSwitchPhase, Option<String>),
{
    let (plan, requires_change) = prepare_switch()?;
    if requires_change {
        verify_backup_capacity(&plan)?;
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
pub async fn sync_all_sessions() -> Result<SessionSyncReceipt, String> {
    tauri::async_runtime::spawn_blocking(sync_all_sessions_blocking)
        .await
        .map_err(|_| "session sync worker failed".to_string())?
}

fn sync_all_sessions_blocking() -> Result<SessionSyncReceipt, String> {
    let _mutation_guard = acquire_mutation_lock()?;
    let started = timestamp_millis()?;
    let operation_id = operation_id("sync-sessions")?;
    let mut backups = Vec::new();
    let mut failure_status = None;
    let mut failure_phase = OperationPhase::Preflight;
    let mut checkpoint_root = None;
    let mut result = (|| {
        let backup_root = default_backup_root()?;
        checkpoint_root = Some(backup_root.clone());
        let shared_home = default_shared_sessions_root()?;
        let current_home = managed_codex_home()?;
        let current_paths = resolve_user_codex_paths(&current_home)?;
        let shared_paths = local_codex_paths(&shared_home);
        let sync_plan = preflight_hot_session_sync_with_paths(&current_paths, &shared_paths)?;
        let backup_scope = sync_plan.backup_scope();
        let current_backup = after_capacity_preflight(
            || {
                preflight_backup_capacity_for_sources(
                    &backup_root,
                    &[
                        BackupCapacitySource {
                            home: &current_home,
                            paths: &current_paths,
                            scope: backup_scope,
                        },
                        BackupCapacitySource {
                            home: &shared_home,
                            paths: &shared_paths,
                            scope: backup_scope,
                        },
                    ],
                )
                .map(|_| ())
            },
            || {
                ensure_codex_paths_unchanged("session sync", &current_home, &current_paths)?;
                failure_phase = OperationPhase::Backup;
                create_hot_sync_backup_with_paths(
                    &current_home,
                    &backup_root,
                    "sync-current",
                    current_paths.clone(),
                )
            },
        )?;
        backups.push(current_backup.clone());
        let shared_backup = create_hot_sync_backup_with_paths(
            &shared_home,
            &backup_root,
            "sync-shared",
            shared_paths.clone(),
        )?;
        backups.push(shared_backup.clone());
        ensure_codex_paths_unchanged("session sync", &current_home, &current_paths)?;
        failure_phase = OperationPhase::Apply;
        match sync_home_with_shared_preflighted_with_paths(&current_paths, &shared_paths, sync_plan)
        {
            Ok(sync_result) => Ok(SessionSyncReceipt {
                operation_id: operation_id.clone(),
                backups: backups.iter().map(BackupReceiptSummary::from).collect(),
                result: sync_result,
                rolled_back: false,
                warnings: Vec::new(),
                checkpoint_cleanup: CheckpointCleanupSummary::default(),
            }),
            Err(error) => {
                failure_phase = OperationPhase::Rollback;
                let compensation = compensate_failed_hot_sync(
                    &current_home,
                    &current_backup,
                    &shared_home,
                    &shared_backup,
                );
                if compensation.shared_rolled_back {
                    failure_status = Some(OperationStatus::Failed);
                    Err(format!(
                        "session sync failed: {error}; restored shared database state and preserved the current safety checkpoint: {}; monotonic session JSONL/index additions may remain for retry",
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
            let terminal_recorded = record_success_result(
                &operation_id,
                OperationAction::SyncSessions,
                started,
                &backups,
                sync_counts(&receipt.result),
            )
            .is_ok();
            let cleanup = release_transient_checkpoints(
                checkpoint_root.as_deref(),
                terminal_recorded,
                backups.as_slice(),
            );
            receipt.warnings.extend(cleanup.warnings.clone());
            receipt.checkpoint_cleanup = cleanup;
        }
        Err(error) => {
            let status = failure_status.unwrap_or_else(|| terminal_status(error));
            let terminal_recorded =
                record_sync_failure(&operation_id, status, failure_phase, started, &backups)
                    .is_ok();
            if failure_phase == OperationPhase::Backup && !backups.is_empty() {
                let cleanup = release_transient_checkpoints(
                    checkpoint_root.as_deref(),
                    terminal_recorded,
                    backups.as_slice(),
                );
                append_warnings_to_error(error, &cleanup.warnings);
            }
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
    let mut checkpoint_root = None;
    let result = (|| {
        let backup_root = default_backup_root()?;
        checkpoint_root = Some(backup_root.clone());
        let shared_home = default_shared_sessions_root()?;
        let current_home = managed_codex_home()?;
        match delete_sessions(
            &current_home,
            &shared_home,
            &backup_root,
            &ids,
            confirmed,
            || ensure_codex_closed("deleting sessions"),
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
        checkpoint_root.as_deref(),
    )
}

#[tauri::command]
pub fn restore_sessions_visible(ids: Vec<String>) -> Result<SessionMutationReceipt, String> {
    let _mutation_guard = acquire_mutation_lock()?;
    let started = timestamp_millis()?;
    let operation_id = operation_id("restore-visibility")?;
    let mut failure_backups = Vec::new();
    let mut checkpoint_root = None;
    let result = (|| {
        let backup_root = default_backup_root()?;
        checkpoint_root = Some(backup_root.clone());
        let current_home = managed_codex_home()?;
        match restore_visible(&current_home, &backup_root, &ids, || {
            ensure_codex_closed("restoring session visibility")
        }) {
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
        checkpoint_root.as_deref(),
    )
}

#[tauri::command]
pub async fn list_backups() -> Result<Vec<BackupSummary>, String> {
    tauri::async_runtime::spawn_blocking(list_backups_blocking)
        .await
        .map_err(|_| "backup list worker failed".to_string())?
}

fn list_backups_blocking() -> Result<Vec<BackupSummary>, String> {
    let backup_root = default_backup_root()?;
    {
        let _migration_guard = acquire_mutation_lock()?;
        migrate_legacy_plaintext_auth(&backup_root)?;
    }
    list_backups_at(&backup_root)
}

fn list_backups_at(backup_root: &Path) -> Result<Vec<BackupSummary>, String> {
    list_backup_snapshots(backup_root, MAX_LISTED_FULL_BACKUPS)
}

#[tauri::command]
pub async fn inspect_checkpoint_storage() -> Result<CheckpointStorageStatus, String> {
    tauri::async_runtime::spawn_blocking(inspect_checkpoint_storage_blocking)
        .await
        .map_err(|_| "checkpoint storage worker failed".to_string())?
}

fn inspect_checkpoint_storage_blocking() -> Result<CheckpointStorageStatus, String> {
    let _mutation_guard = acquire_mutation_lock()?;
    let records = operation_log()?.list_all_strict()?;
    inspect_checkpoint_storage_at(&default_backup_root()?, &records)
}

#[tauri::command]
pub async fn cleanup_automatic_checkpoints() -> Result<CheckpointCleanupReceipt, String> {
    tauri::async_runtime::spawn_blocking(cleanup_automatic_checkpoints_blocking)
        .await
        .map_err(|_| "checkpoint cleanup worker failed".to_string())?
}

fn cleanup_automatic_checkpoints_blocking() -> Result<CheckpointCleanupReceipt, String> {
    let _mutation_guard = acquire_mutation_lock()?;
    let started = timestamp_millis()?;
    let operation_id = operation_id("cleanup-checkpoints")?;
    let log = operation_log()?;
    let records = log.list_all_strict()?;
    let summary = cleanup_checkpoint_storage(&default_backup_root()?, &records)?;
    let mut receipt = CheckpointCleanupReceipt {
        operation_id: operation_id.clone(),
        attempted_count: summary.attempted_count,
        failed_count: summary.failed_count,
        reclaimed_count: summary.reclaimed_count,
        reclaimed_bytes: summary.reclaimed_bytes,
        retained_count: summary.retained_count,
        warnings: summary.warnings,
    };
    let counts = checkpoint_cleanup_counts(&receipt);
    let terminal = checkpoint_cleanup_terminal(&receipt);
    if append_operation_record_to(
        &log,
        &operation_id,
        OperationAction::CleanupCheckpoints,
        terminal,
        started,
        &[],
        counts,
    )
    .is_err()
    {
        receipt.warnings.push(
            "checkpoint cleanup completed, but local operation history could not be written"
                .to_string(),
        );
    }
    Ok(receipt)
}

fn checkpoint_cleanup_counts(receipt: &CheckpointCleanupReceipt) -> BTreeMap<String, usize> {
    BTreeMap::from([
        ("attemptedCount".to_string(), receipt.attempted_count),
        ("failedCount".to_string(), receipt.failed_count),
        ("reclaimedCount".to_string(), receipt.reclaimed_count),
        (
            "reclaimedBytes".to_string(),
            usize::try_from(receipt.reclaimed_bytes).unwrap_or(usize::MAX),
        ),
        ("retainedCount".to_string(), receipt.retained_count),
    ])
}

fn checkpoint_cleanup_terminal(receipt: &CheckpointCleanupReceipt) -> OperationTerminal {
    if receipt.failed_count == 0 {
        OperationTerminal {
            status: OperationStatus::Succeeded,
            phase: OperationPhase::Complete,
        }
    } else {
        OperationTerminal {
            status: OperationStatus::Failed,
            phase: OperationPhase::Apply,
        }
    }
}

#[tauri::command]
pub async fn create_full_backup() -> Result<CreateFullBackupReceipt, String> {
    tauri::async_runtime::spawn_blocking(create_full_backup_blocking)
        .await
        .map_err(|_| "full backup worker failed".to_string())?
}

fn create_full_backup_blocking() -> Result<CreateFullBackupReceipt, String> {
    let _mutation_guard = acquire_mutation_lock()?;
    let started = timestamp_millis()?;
    let operation_id = operation_id("create-backup")?;
    let mut backups = Vec::new();
    let mut failure_phase = OperationPhase::Preflight;
    let mut result: Result<CreateFullBackupReceipt, String> = (|| {
        let backup_root = default_backup_root()?;
        let current_home = managed_codex_home()?;
        let shared_home = default_shared_sessions_root()?;
        let current_paths = resolve_user_codex_paths(&current_home)?;
        let current_backup_paths = current_paths.clone();
        let shared_paths = local_codex_paths(&shared_home);
        let current_backup = after_capacity_preflight(
            || {
                preflight_before_process_gate(
                    || {
                        preflight_backup_capacity_for_sources(
                            &backup_root,
                            &[
                                BackupCapacitySource {
                                    home: &current_home,
                                    paths: &current_paths,
                                    scope: BackupScope::Full,
                                },
                                BackupCapacitySource {
                                    home: &shared_home,
                                    paths: &shared_paths,
                                    scope: BackupScope::Full,
                                },
                            ],
                        )
                        .map(|_| ())
                    },
                    || close_codex_for_closed_mutation("creating a full backup"),
                )
            },
            || {
                ensure_codex_paths_unchanged("full backup", &current_home, &current_paths)?;
                failure_phase = OperationPhase::Backup;
                create_backup_with_paths(
                    &current_home,
                    &backup_root,
                    "manual-full-current",
                    current_backup_paths,
                )
            },
        )?;
        backups.push(current_backup);
        failure_phase = OperationPhase::Preflight;
        ensure_codex_closed("creating a full backup")?;
        failure_phase = OperationPhase::Backup;
        let shared_backup = create_backup_with_paths(
            &shared_home,
            &backup_root,
            "manual-full-shared",
            shared_paths,
        )?;
        backups.push(shared_backup);
        failure_phase = OperationPhase::Preflight;
        ensure_codex_closed("creating a full backup")?;
        Ok(CreateFullBackupReceipt {
            operation_id: operation_id.clone(),
            backups: backups.iter().map(BackupReceiptSummary::from).collect(),
            warnings: Vec::new(),
        })
    })();
    match &mut result {
        Ok(receipt) => {
            receipt.warnings = record_success(
                &operation_id,
                OperationAction::CreateBackup,
                started,
                &backups,
                BTreeMap::from([(
                    "backupFiles".to_string(),
                    backups.iter().map(|backup| backup.files.len()).sum(),
                )]),
            )
            .into_iter()
            .collect();
        }
        Err(error) => {
            if append_operation_record_with_phase(
                &operation_id,
                OperationAction::CreateBackup,
                terminal_status(error),
                failure_phase,
                started,
                &backups,
                BTreeMap::new(),
            )
            .is_err()
            {
                error.push_str("; local operation history could not be written");
            }
        }
    }
    result
}

#[tauri::command]
pub async fn delete_backup(
    backup_dir: PathBuf,
    confirmed: bool,
) -> Result<BackupDeleteReceipt, String> {
    tauri::async_runtime::spawn_blocking(move || delete_backup_blocking(backup_dir, confirmed))
        .await
        .map_err(|_| "backup deletion worker failed".to_string())?
}

fn delete_backup_blocking(
    backup_dir: PathBuf,
    confirmed: bool,
) -> Result<BackupDeleteReceipt, String> {
    let _mutation_guard = acquire_mutation_lock()?;
    let backup_root = default_backup_root()?;
    delete_backup_at(&backup_root, operation_log(), backup_dir, confirmed)
}

fn delete_backup_at(
    backup_root: &Path,
    log: Result<OperationLog, String>,
    backup_dir: PathBuf,
    confirmed: bool,
) -> Result<BackupDeleteReceipt, String> {
    let started_at_ms = timestamp_millis()?;
    let operation_id = operation_id("delete-backup")?;
    if !confirmed {
        if let Ok(log) = log {
            let _ = append_delete_backup_record(
                &log,
                &operation_id,
                OperationStatus::Failed,
                OperationPhase::Preflight,
                started_at_ms,
                Vec::new(),
                BTreeMap::new(),
            );
        }
        return Err("backup deletion requires explicit confirmation".to_string());
    }

    match delete_verified_full_backup(backup_root, &backup_dir) {
        Ok(deleted) => {
            let mut warnings = Vec::new();
            let counts = BTreeMap::from([
                ("backupsDeleted".to_string(), 1),
                (
                    "reclaimedBytes".to_string(),
                    usize::try_from(deleted.reclaimed_bytes).unwrap_or(usize::MAX),
                ),
            ]);
            let terminal_recorded = log
                .as_ref()
                .map_err(Clone::clone)
                .and_then(|log| {
                    append_delete_backup_record(
                        log,
                        &operation_id,
                        OperationStatus::Succeeded,
                        OperationPhase::Complete,
                        started_at_ms,
                        vec![deleted.backup_dir.clone()],
                        counts,
                    )
                })
                .is_ok();
            if !terminal_recorded {
                warnings
                    .push("恢复点已删除，但本地操作记录未能持久化；删除结果不会回滚".to_string());
            }
            Ok(BackupDeleteReceipt {
                operation_id,
                backup_dir: deleted.backup_dir,
                reclaimed_bytes: deleted.reclaimed_bytes,
                warnings,
            })
        }
        Err(error) => {
            if let Ok(log) = log {
                let _ = append_delete_backup_record(
                    &log,
                    &operation_id,
                    OperationStatus::Failed,
                    OperationPhase::Apply,
                    started_at_ms,
                    Vec::new(),
                    BTreeMap::new(),
                );
            }
            Err(error)
        }
    }
}

fn append_delete_backup_record(
    log: &OperationLog,
    operation_id: &str,
    status: OperationStatus,
    phase: OperationPhase,
    started_at_ms: u128,
    backup_dirs: Vec<PathBuf>,
    counts: BTreeMap<String, usize>,
) -> Result<(), String> {
    log.append(&OperationRecord {
        operation_id: operation_id.to_string(),
        action: OperationAction::DeleteBackup,
        status,
        phase,
        started_at_ms,
        completed_at_ms: timestamp_millis()?,
        backup_dirs,
        counts,
    })
}

#[tauri::command]
pub fn restore_backup(backup_dir: String) -> Result<RestoreBackupReceipt, String> {
    let _mutation_guard = acquire_mutation_lock()?;
    let started = timestamp_millis()?;
    let operation_id = operation_id("restore-backup")?;
    let mut backups = Vec::new();
    let mut failure_status = None;
    let mut failure_phase = OperationPhase::Preflight;
    let mut result = (|| {
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
        let target_paths = if target_is_local {
            local_codex_paths(&target)
        } else {
            resolve_user_codex_paths(&target)?
        };
        let safety_backup = after_capacity_preflight(
            || {
                preflight_before_process_gate(
                    || {
                        preflight_backup_capacity_with_paths(
                            &backup_root,
                            &target,
                            &target_paths,
                            BackupScope::Full,
                        )
                        .map(|_| ())
                    },
                    || ensure_codex_closed("restoring a backup"),
                )
            },
            || {
                failure_phase = OperationPhase::Backup;
                create_backup_with_paths(
                    &target,
                    &backup_root,
                    "pre-restore-safety",
                    target_paths.clone(),
                )
            },
        )?;
        backups.push(safety_backup.clone());
        if !target_is_local {
            failure_phase = OperationPhase::Preflight;
            ensure_codex_paths_unchanged("backup restore", &target, &target_paths)?;
        }
        failure_phase = OperationPhase::Preflight;
        ensure_codex_closed("restoring a backup")?;
        failure_phase = OperationPhase::Apply;
        match restore_backup_snapshot(&selected, &target) {
            Ok(restore_result) => Ok(RestoreBackupReceipt {
                operation_id: operation_id.clone(),
                result: restore_result,
                safety_backup: BackupReceiptSummary::from(&safety_backup),
                rolled_back: false,
                warnings: Vec::new(),
            }),
            Err(error) => {
                failure_phase = OperationPhase::Rollback;
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
            let _ = append_operation_record_with_phase(
                &operation_id,
                OperationAction::RestoreBackup,
                failure_status.unwrap_or_else(|| terminal_status(error)),
                failure_phase,
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

fn create_hot_sync_backup_with_paths(
    home: &Path,
    backup_root: &Path,
    reason: &str,
    paths: CodexPaths,
) -> Result<BackupManifest, String> {
    create_state_backup_with_paths(home, backup_root, reason, paths)
}

fn release_transient_checkpoints(
    backup_root: Option<&Path>,
    terminal_recorded: bool,
    backups: &[BackupManifest],
) -> CheckpointCleanupSummary {
    if backups.is_empty() {
        return CheckpointCleanupSummary::default();
    }
    if !terminal_recorded {
        return CheckpointCleanupSummary {
            retained_count: backups.len(),
            warnings: vec!["终态记录未能持久化；临时检查点已保留".to_string()],
            ..CheckpointCleanupSummary::default()
        };
    }
    match backup_root {
        Some(root) => cleanup_transient_checkpoints(root, backups),
        None => CheckpointCleanupSummary {
            retained_count: backups.len(),
            warnings: vec![
                "automatic checkpoints could not be resolved and were retained".to_string(),
            ],
            ..CheckpointCleanupSummary::default()
        },
    }
}

fn failed_runtime_switch_checkpoints_are_releasable(
    outcome: RuntimeSwitchOutcome,
    backups: &[BackupManifest],
) -> bool {
    !backups.is_empty()
        && matches!(
            outcome,
            RuntimeSwitchOutcome::FailedBeforeWrite | RuntimeSwitchOutcome::RolledBack
        )
}

fn append_warnings_to_error(error: &mut String, warnings: &[String]) {
    if !warnings.is_empty() {
        error.push_str("; ");
        error.push_str(&warnings.join("; "));
    }
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
    ensure_codex_closed_from_processes(action, list_processes()?, list_standalone_processes()?)
}

fn ensure_codex_paths_unchanged(
    action: &str,
    codex_home: &Path,
    expected: &CodexPaths,
) -> Result<(), String> {
    if resolve_user_codex_paths(codex_home)? == *expected {
        Ok(())
    } else {
        Err(format!("Codex paths changed during {action}; retry"))
    }
}

fn ensure_codex_closed_from_processes(
    action: &str,
    managed: Vec<CodexProcess>,
    standalone: Vec<CodexProcess>,
) -> Result<(), String> {
    match (managed.is_empty(), standalone.is_empty()) {
        (true, true) => Ok(()),
        (false, true) => Err(format!(
            "ChatGPT is still running; close it before {action}"
        )),
        (true, false) => Err(format!(
            "a standalone Codex CLI is still running; close it before {action}"
        )),
        (false, false) => Err(format!(
            "ChatGPT and a standalone Codex CLI are still running; close them before {action}"
        )),
    }
}

fn list_managed_processes_for_closed_mutation(action: &str) -> Result<Vec<CodexProcess>, String> {
    let standalone = list_standalone_processes()?;
    if standalone.is_empty() {
        list_processes()
    } else {
        Err(format!(
            "a standalone Codex CLI is still running; close it before {action}"
        ))
    }
}

fn close_codex_for_closed_mutation(action: &str) -> Result<(), String> {
    let processes = list_managed_processes_for_closed_mutation(action)?;
    if !processes.is_empty() {
        close_codex()?;
    }
    ensure_codex_closed(action)
}

fn preflight_before_process_gate<Capacity, ProcessGate>(
    mut preflight_capacity: Capacity,
    mut process_gate: ProcessGate,
) -> Result<(), String>
where
    Capacity: FnMut() -> Result<(), String>,
    ProcessGate: FnMut() -> Result<(), String>,
{
    preflight_capacity()?;
    process_gate()
}

fn after_capacity_preflight<T, Capacity, FirstBackup>(
    preflight_capacity: Capacity,
    create_first_backup: FirstBackup,
) -> Result<T, String>
where
    Capacity: FnOnce() -> Result<(), String>,
    FirstBackup: FnOnce() -> Result<T, String>,
{
    preflight_capacity()?;
    create_first_backup()
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
    checkpoint_root: Option<&Path>,
) -> Result<SessionMutationReceipt, String> {
    finish_session_mutation_with_log(
        operation_log(),
        operation_id,
        action,
        started_at_ms,
        result,
        failure_backups,
        checkpoint_root,
    )
}

fn finish_session_mutation_with_log(
    log: Result<OperationLog, String>,
    operation_id: String,
    action: OperationAction,
    started_at_ms: u128,
    result: Result<SessionMutationResult, String>,
    failure_backups: &[BackupManifest],
    checkpoint_root: Option<&Path>,
) -> Result<SessionMutationReceipt, String> {
    match result {
        Ok(result) => {
            let terminal_recorded = log
                .as_ref()
                .map_err(Clone::clone)
                .and_then(|log| {
                    append_operation_record_to(
                        log,
                        &operation_id,
                        action,
                        OperationTerminal {
                            status: OperationStatus::Succeeded,
                            phase: OperationPhase::Complete,
                        },
                        started_at_ms,
                        &result.backups,
                        mutation_counts(&result),
                    )
                })
                .is_ok();
            let mut warnings = Vec::new();
            let checkpoint_cleanup = if action == OperationAction::RestoreVisibility {
                let cleanup = release_transient_checkpoints(
                    checkpoint_root,
                    terminal_recorded,
                    &result.backups,
                );
                if result.backups.is_empty() && !terminal_recorded {
                    warnings.push("操作已成功，但本地操作记录写入失败".to_string());
                }
                warnings.extend(cleanup.warnings.clone());
                cleanup
            } else {
                if !terminal_recorded {
                    warnings.push("操作已成功，但本地操作记录写入失败".to_string());
                }
                CheckpointCleanupSummary::default()
            };
            Ok(SessionMutationReceipt {
                operation_id,
                result: result.into(),
                rolled_back: false,
                warnings,
                checkpoint_cleanup,
            })
        }
        Err(error) => {
            let status = terminal_status(&error);
            if let Ok(log) = log {
                let _ = append_operation_record_to(
                    &log,
                    &operation_id,
                    action,
                    OperationTerminal {
                        status,
                        phase: operation_phase(&action, &status),
                    },
                    started_at_ms,
                    failure_backups,
                    BTreeMap::new(),
                );
            }
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
) -> Result<(), String> {
    let log = operation_log()?;
    record_result_to_log(
        &log,
        operation_id,
        action,
        started_at_ms,
        result,
        backups,
        counts,
    )
}

fn record_runtime_switch_result<T>(
    operation_id: &str,
    started_at_ms: u128,
    result: &Result<T, String>,
    backups: &[BackupManifest],
    counts: BTreeMap<String, usize>,
    failure_outcome: RuntimeSwitchOutcome,
) -> Result<(), String> {
    record_runtime_switch_result_to_log(
        &operation_log()?,
        operation_id,
        started_at_ms,
        result,
        backups,
        counts,
        failure_outcome,
    )
}

fn record_runtime_switch_result_to_log<T>(
    log: &OperationLog,
    operation_id: &str,
    started_at_ms: u128,
    result: &Result<T, String>,
    backups: &[BackupManifest],
    counts: BTreeMap<String, usize>,
    failure_outcome: RuntimeSwitchOutcome,
) -> Result<(), String> {
    let terminal = match result {
        Ok(_) => OperationTerminal {
            status: OperationStatus::Succeeded,
            phase: OperationPhase::Complete,
        },
        Err(_) => match failure_outcome {
            RuntimeSwitchOutcome::FailedBeforeWrite => OperationTerminal {
                status: OperationStatus::Failed,
                phase: if backups.is_empty() {
                    OperationPhase::Preflight
                } else {
                    OperationPhase::Backup
                },
            },
            RuntimeSwitchOutcome::RolledBack => OperationTerminal {
                status: OperationStatus::RolledBack,
                phase: OperationPhase::Rollback,
            },
            RuntimeSwitchOutcome::RollbackFailed => OperationTerminal {
                status: OperationStatus::RollbackFailed,
                phase: OperationPhase::Rollback,
            },
        },
    };
    append_operation_record_to(
        log,
        operation_id,
        OperationAction::SwitchRuntime,
        terminal,
        started_at_ms,
        backups,
        counts,
    )
}

fn record_sync_failure(
    operation_id: &str,
    status: OperationStatus,
    phase: OperationPhase,
    started_at_ms: u128,
    backups: &[BackupManifest],
) -> Result<(), String> {
    record_sync_failure_to_log(
        &operation_log()?,
        operation_id,
        status,
        phase,
        started_at_ms,
        backups,
    )
}

fn record_sync_failure_to_log(
    log: &OperationLog,
    operation_id: &str,
    status: OperationStatus,
    phase: OperationPhase,
    started_at_ms: u128,
    backups: &[BackupManifest],
) -> Result<(), String> {
    append_operation_record_to(
        log,
        operation_id,
        OperationAction::SyncSessions,
        OperationTerminal { status, phase },
        started_at_ms,
        backups,
        BTreeMap::new(),
    )
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
    let phase = operation_phase(&action, &status);
    append_operation_record_to(
        log,
        operation_id,
        action,
        OperationTerminal { status, phase },
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
    record_success_result(operation_id, action, started_at_ms, backups, counts)
        .err()
        .map(|_| "操作已成功，但本地操作记录写入失败".to_string())
}

fn record_success_result(
    operation_id: &str,
    action: OperationAction,
    started_at_ms: u128,
    backups: &[BackupManifest],
    counts: BTreeMap<String, usize>,
) -> Result<(), String> {
    append_operation_record(
        operation_id,
        action,
        OperationStatus::Succeeded,
        started_at_ms,
        backups,
        counts,
    )
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
    let phase = operation_phase(&action, &status);
    append_operation_record_with_phase(
        operation_id,
        action,
        status,
        phase,
        started_at_ms,
        backups,
        counts,
    )
}

fn append_operation_record_with_phase(
    operation_id: &str,
    action: OperationAction,
    status: OperationStatus,
    phase: OperationPhase,
    started_at_ms: u128,
    backups: &[BackupManifest],
    counts: BTreeMap<String, usize>,
) -> Result<(), String> {
    append_operation_record_to(
        &operation_log()?,
        operation_id,
        action,
        OperationTerminal { status, phase },
        started_at_ms,
        backups,
        counts,
    )
}

fn append_operation_record_to(
    log: &OperationLog,
    operation_id: &str,
    action: OperationAction,
    terminal: OperationTerminal,
    started_at_ms: u128,
    backups: &[BackupManifest],
    counts: BTreeMap<String, usize>,
) -> Result<(), String> {
    log.append(&OperationRecord {
        operation_id: operation_id.to_string(),
        action,
        status: terminal.status,
        phase: terminal.phase,
        started_at_ms,
        completed_at_ms: timestamp_millis()?,
        backup_dirs: backups
            .iter()
            .map(|backup| backup.backup_dir.clone())
            .collect(),
        counts,
    })
}

struct OperationTerminal {
    status: OperationStatus,
    phase: OperationPhase,
}

fn operation_phase(action: &OperationAction, status: &OperationStatus) -> OperationPhase {
    match status {
        OperationStatus::Succeeded => OperationPhase::Complete,
        OperationStatus::RolledBack | OperationStatus::RollbackFailed => OperationPhase::Rollback,
        OperationStatus::Failed if matches!(action, OperationAction::VerifyRelay) => {
            OperationPhase::Verify
        }
        OperationStatus::Failed => OperationPhase::Apply,
    }
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

    use rusqlite::Connection;
    use tauri::ipc::{Channel, InvokeResponseBody};
    use tempfile::tempdir;

    use crate::{
        backup::{
            create_backup, create_runtime_state_backup_with_paths, create_state_backup, BackupScope,
        },
        codex_paths::resolve_user_codex_paths,
        operation_log::{OperationAction, OperationLog, OperationPhase, OperationStatus},
        process_control::CodexProcess,
        session_manager::SessionMutationResult,
    };

    use super::{
        acquire_mutation_lock_at, after_capacity_preflight, append_operation_record_to,
        checkpoint_cleanup_counts, checkpoint_cleanup_terminal, cleanup_automatic_checkpoints,
        close_codex_processes, compensate_failed_hot_sync, create_full_backup,
        default_codex_home_from_env, delete_backup, delete_backup_at, emit_runtime_switch_terminal,
        ensure_codex_closed_from_processes, ensure_codex_paths_unchanged,
        failed_runtime_switch_checkpoints_are_releasable, finish_session_mutation_with_log,
        get_app_status, inspect_checkpoint_storage, list_backups, list_backups_at,
        preflight_before_process_gate, prepare_runtime_switch_before_close, record_result_to_log,
        record_runtime_switch_result_to_log, record_sync_failure_to_log,
        release_transient_checkpoints, run_runtime_switch_preflight, switch_runtime,
        sync_all_sessions, validate_backup_selection, BackupDeleteReceipt, BackupReceiptSummary,
        CheckpointCleanupReceipt, CreateFullBackupReceipt, MutationCoordinator, OperationTerminal,
        RuntimeSwitchOutcome, RuntimeSwitchPhase, RuntimeSwitchProgress, RuntimeSwitchResult,
        MAX_LISTED_FULL_BACKUPS,
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
    fn checkpoint_cleanup_terminal_depends_on_real_failures_not_warnings() {
        let mut receipt = CheckpointCleanupReceipt {
            operation_id: "cleanup-complete".to_string(),
            attempted_count: 4,
            failed_count: 0,
            reclaimed_count: 4,
            reclaimed_bytes: 4096,
            retained_count: 2,
            warnings: vec!["two safety-retained entries were not attempted".to_string()],
        };

        let counts = checkpoint_cleanup_counts(&receipt);
        let terminal = checkpoint_cleanup_terminal(&receipt);

        assert_eq!(counts["attemptedCount"], 4);
        assert_eq!(counts["failedCount"], 0);
        assert_eq!(terminal.status, OperationStatus::Succeeded);
        assert_eq!(terminal.phase, OperationPhase::Complete);

        receipt.failed_count = 1;
        receipt.reclaimed_count = 3;
        let counts = checkpoint_cleanup_counts(&receipt);
        let terminal = checkpoint_cleanup_terminal(&receipt);

        assert_eq!(counts["failedCount"], 1);
        assert_eq!(terminal.status, OperationStatus::Failed);
        assert_eq!(terminal.phase, OperationPhase::Apply);
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
    fn full_backup_command_remains_async() {
        fn assert_future<F>(future: F)
        where
            F: Future<Output = Result<CreateFullBackupReceipt, String>> + Send,
        {
            drop(future);
        }

        assert_future(create_full_backup());
    }

    #[test]
    fn backup_delete_command_remains_async() {
        fn assert_future<F>(future: F)
        where
            F: Future<Output = Result<BackupDeleteReceipt, String>> + Send,
        {
            drop(future);
        }

        assert_future(delete_backup(
            std::path::PathBuf::from("C:/not-invoked"),
            false,
        ));
    }

    #[test]
    fn backup_list_command_remains_async() {
        fn assert_future<F>(future: F)
        where
            F: Future<Output = Result<Vec<crate::backup::BackupSummary>, String>> + Send,
        {
            drop(future);
        }

        assert_future(list_backups());
    }

    #[test]
    fn backup_list_exposes_recovery_points_beyond_the_previous_five_item_cap() {
        let home = tempdir().unwrap();
        fs::write(home.path().join("config.toml"), "model = \"test\"\n").unwrap();
        let backup_root = tempdir().unwrap();
        for index in 0..6 {
            create_backup(
                home.path(),
                backup_root.path(),
                &format!("manual-full-{index}"),
            )
            .unwrap();
        }

        let summaries = list_backups_at(backup_root.path()).unwrap();

        assert_eq!(MAX_LISTED_FULL_BACKUPS, 256);
        assert_eq!(summaries.len(), 6);
        assert!(summaries.iter().all(|summary| summary.verified));
    }

    #[test]
    fn session_sync_and_checkpoint_scan_commands_remain_async() {
        fn assert_future<T, F>(future: F)
        where
            F: Future<Output = Result<T, String>> + Send,
        {
            drop(future);
        }

        assert_future(sync_all_sessions());
        assert_future(inspect_checkpoint_storage());
        assert_future(cleanup_automatic_checkpoints());
    }

    #[test]
    fn full_backup_receipt_uses_plural_two_root_contract() {
        let current = tempdir().unwrap();
        let shared = tempdir().unwrap();
        let backup_root = tempdir().unwrap();
        fs::write(current.path().join("config.toml"), "model = \"current\"\n").unwrap();
        fs::write(shared.path().join("config.toml"), "model = \"shared\"\n").unwrap();
        let mut current_backup =
            create_backup(current.path(), backup_root.path(), "manual-current").unwrap();
        let shared_backup =
            create_backup(shared.path(), backup_root.path(), "manual-shared").unwrap();
        let sample_file = current_backup.files.first().unwrap().clone();
        current_backup.files = vec![sample_file; 4_096];
        let receipt = CreateFullBackupReceipt {
            operation_id: "backup-two-roots".to_string(),
            backups: vec![
                BackupReceiptSummary::from(&current_backup),
                BackupReceiptSummary::from(&shared_backup),
            ],
            warnings: Vec::new(),
        };

        let wire = serde_json::to_value(receipt).unwrap();
        assert_eq!(wire["backups"].as_array().unwrap().len(), 2);
        assert!(wire.get("backup").is_none());
        let encoded = serde_json::to_string(&wire).unwrap();
        assert!(
            encoded.len() < 4_096,
            "compact receipt grew to {} bytes",
            encoded.len()
        );
        for forbidden in [
            "\"files\"",
            "\"sourcePath\"",
            "\"backupPath\"",
            "\"sha256\"",
            "\"trackedDatabases\"",
        ] {
            assert!(!encoded.contains(forbidden), "{forbidden} leaked into IPC");
        }
        assert!(encoded.contains("\"trackedDatabaseCount\""));
    }

    #[test]
    fn verified_full_backup_delete_is_audited_after_the_directory_is_removed() {
        let home = tempdir().unwrap();
        let backup_root = tempdir().unwrap();
        let log_root = tempdir().unwrap();
        fs::write(home.path().join("config.toml"), "model = \"delete-me\"\n").unwrap();
        let backup = create_backup(home.path(), backup_root.path(), "manual-full-current").unwrap();
        let backup_dir = backup.backup_dir.clone();
        let log = OperationLog::new(log_root.path().join("operations.jsonl"));

        let receipt = delete_backup_at(
            backup_root.path(),
            Ok(log.clone()),
            backup_dir.clone(),
            true,
        )
        .unwrap();

        assert_eq!(receipt.backup_dir, backup_dir);
        assert!(receipt.reclaimed_bytes > 0);
        assert!(receipt.warnings.is_empty());
        assert!(!backup_dir.exists());
        let record = log.list(1).unwrap().pop().unwrap();
        assert_eq!(record.action, OperationAction::DeleteBackup);
        assert_eq!(record.status, OperationStatus::Succeeded);
        assert_eq!(record.phase, OperationPhase::Complete);
        assert_eq!(record.backup_dirs, vec![backup_dir]);
        assert_eq!(record.counts["backupsDeleted"], 1);
    }

    #[test]
    fn backup_delete_requires_confirmation_and_preserves_the_full_backup() {
        let home = tempdir().unwrap();
        let backup_root = tempdir().unwrap();
        let log_root = tempdir().unwrap();
        let backup = create_backup(home.path(), backup_root.path(), "manual-full-current").unwrap();
        let backup_dir = backup.backup_dir.clone();
        let log = OperationLog::new(log_root.path().join("operations.jsonl"));

        let error = delete_backup_at(
            backup_root.path(),
            Ok(log.clone()),
            backup_dir.clone(),
            false,
        )
        .unwrap_err();

        assert!(error.contains("explicit confirmation"), "{error}");
        assert!(backup_dir.exists());
        let record = log.list(1).unwrap().pop().unwrap();
        assert_eq!(record.action, OperationAction::DeleteBackup);
        assert_eq!(record.status, OperationStatus::Failed);
        assert_eq!(record.phase, OperationPhase::Preflight);
        assert!(record.backup_dirs.is_empty());
    }

    #[test]
    fn backup_delete_rejects_scoped_checkpoint_without_removing_it() {
        let home = tempdir().unwrap();
        let backup_root = tempdir().unwrap();
        let log_root = tempdir().unwrap();
        let checkpoint =
            create_state_backup(home.path(), backup_root.path(), "sync-current").unwrap();
        let checkpoint_dir = checkpoint.backup_dir.clone();
        let log = OperationLog::new(log_root.path().join("operations.jsonl"));

        let error = delete_backup_at(
            backup_root.path(),
            Ok(log.clone()),
            checkpoint_dir.clone(),
            true,
        )
        .unwrap_err();

        assert!(error.contains("only persistent full backups"), "{error}");
        assert!(checkpoint_dir.exists());
        let record = log.list(1).unwrap().pop().unwrap();
        assert_eq!(record.status, OperationStatus::Failed);
        assert_eq!(record.phase, OperationPhase::Apply);
        assert!(record.backup_dirs.is_empty());
    }

    #[test]
    fn backup_delete_log_failure_reports_warning_without_recreating_deleted_data() {
        let home = tempdir().unwrap();
        let backup_root = tempdir().unwrap();
        let invalid_log_path = tempdir().unwrap();
        fs::write(home.path().join("config.toml"), "model = \"delete-me\"\n").unwrap();
        let backup = create_backup(home.path(), backup_root.path(), "manual-full-current").unwrap();
        let backup_dir = backup.backup_dir.clone();

        let receipt = delete_backup_at(
            backup_root.path(),
            Ok(OperationLog::new(invalid_log_path.path().to_path_buf())),
            backup_dir.clone(),
            true,
        )
        .unwrap();

        assert!(!backup_dir.exists());
        assert_eq!(receipt.warnings.len(), 1);
        assert!(receipt.warnings[0].contains("操作记录未能持久化"));
        assert!(receipt.warnings[0].contains("不会回滚"));
    }

    #[test]
    fn closed_state_mutations_reject_standalone_cli_without_classifying_it_as_chatgpt() {
        let standalone = CodexProcess {
            image_name: "codex.exe".to_string(),
            pid: 5678,
            parent_pid: 42,
            creation_time_100ns: Some(2),
        };
        let error = ensure_codex_closed_from_processes(
            "creating a full backup",
            Vec::new(),
            vec![standalone],
        )
        .unwrap_err();

        assert!(error.contains("standalone Codex CLI"), "{error}");
        assert!(!error.contains("ChatGPT is still running"), "{error}");
    }

    #[test]
    fn capacity_preflight_finishes_before_the_closed_process_gate() {
        let calls = RefCell::new(Vec::new());
        preflight_before_process_gate(
            || {
                calls.borrow_mut().push("capacity");
                Ok(())
            },
            || {
                calls.borrow_mut().push("process");
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(calls.into_inner(), vec!["capacity", "process"]);

        let process_calls = Cell::new(0);
        let error = preflight_before_process_gate(
            || Err("capacity unavailable".to_string()),
            || {
                process_calls.set(process_calls.get() + 1);
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error, "capacity unavailable");
        assert_eq!(process_calls.get(), 0);
    }

    #[test]
    fn capacity_failure_prevents_first_backup_and_live_writes_for_all_command_flows() {
        for operation in ["session sync", "manual full backup", "backup restore"] {
            let live = tempdir().unwrap();
            let backup_root = tempdir().unwrap();
            let live_file = live.path().join("state.bin");
            let original = format!("{operation}-original").into_bytes();
            fs::write(&live_file, &original).unwrap();
            let process_calls = Cell::new(0);

            let result: Result<(), String> = after_capacity_preflight(
                || {
                    if operation == "session sync" {
                        Err("injected capacity failure".to_string())
                    } else {
                        preflight_before_process_gate(
                            || Err("injected capacity failure".to_string()),
                            || {
                                process_calls.set(process_calls.get() + 1);
                                Ok(())
                            },
                        )
                    }
                },
                || {
                    fs::create_dir(backup_root.path().join("first-backup")).unwrap();
                    fs::write(&live_file, b"changed").unwrap();
                    Ok(())
                },
            );

            assert_eq!(result.unwrap_err(), "injected capacity failure");
            assert_eq!(process_calls.get(), 0, "{operation}");
            assert_eq!(fs::read(&live_file).unwrap(), original, "{operation}");
            assert_eq!(
                fs::read_dir(backup_root.path()).unwrap().count(),
                0,
                "{operation}"
            );
        }
    }

    #[test]
    fn frozen_codex_paths_reject_sqlite_root_drift_before_backup_or_apply() {
        let home = tempdir().unwrap();
        let first_sqlite = tempdir().unwrap();
        let second_sqlite = tempdir().unwrap();
        let toml_path = |path: &std::path::Path| path.to_string_lossy().replace('\\', "\\\\");
        fs::write(
            home.path().join("config.toml"),
            format!("sqlite_home = \"{}\"\n", toml_path(first_sqlite.path())),
        )
        .unwrap();
        let frozen = resolve_user_codex_paths(home.path()).unwrap();
        fs::write(
            home.path().join("config.toml"),
            format!("sqlite_home = \"{}\"\n", toml_path(second_sqlite.path())),
        )
        .unwrap();

        let error = ensure_codex_paths_unchanged("session sync", home.path(), &frozen).unwrap_err();
        assert!(error.contains("Codex paths changed"), "{error}");
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
            |_| {
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
            |_| panic!("capacity must not run after config failure"),
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
            |_| Err("capacity preflight failed".to_string()),
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
            |_| panic!("exact no-op must not scan backup capacity"),
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
    fn failure_records_keep_typed_preflight_and_verify_phases() {
        let home = tempdir().unwrap();
        let backup_root = tempdir().unwrap();
        let log_root = tempdir().unwrap();
        fs::write(home.path().join("config.toml"), "model = \"test\"\n").unwrap();
        let backup = create_backup(home.path(), backup_root.path(), "partial-attempt").unwrap();
        let log = OperationLog::new(log_root.path().join("operations.jsonl"));
        append_operation_record_to(
            &log,
            "partial-attempt",
            OperationAction::CreateBackup,
            OperationTerminal {
                status: OperationStatus::Failed,
                phase: OperationPhase::Preflight,
            },
            1,
            std::slice::from_ref(&backup),
            std::collections::BTreeMap::new(),
        )
        .unwrap();
        let relay_result: Result<(), String> = Err("relay unavailable".to_string());
        record_result_to_log(
            &log,
            "verify-relay",
            OperationAction::VerifyRelay,
            2,
            &relay_result,
            &[],
            std::collections::BTreeMap::new(),
        )
        .unwrap();

        let records = log.list(10).unwrap();
        assert_eq!(records.len(), 2);
        let backup_record = records
            .iter()
            .find(|record| record.action == OperationAction::CreateBackup)
            .unwrap();
        assert_eq!(backup_record.status, OperationStatus::Failed);
        assert_eq!(backup_record.phase, OperationPhase::Preflight);
        assert_eq!(backup_record.backup_dirs, vec![backup.backup_dir]);
        let relay_record = records
            .iter()
            .find(|record| record.action == OperationAction::VerifyRelay)
            .unwrap();
        assert_eq!(relay_record.status, OperationStatus::Failed);
        assert_eq!(relay_record.phase, OperationPhase::Verify);
    }

    #[test]
    fn switch_prewrite_failure_cleans_one_or_two_checkpoints_after_backup_terminal() {
        for checkpoint_count in [1_usize, 2] {
            let current = tempdir().unwrap();
            let shared = tempdir().unwrap();
            let backup_root = tempdir().unwrap();
            let log_root = tempdir().unwrap();
            let current_checkpoint = create_runtime_state_backup_with_paths(
                current.path(),
                backup_root.path(),
                "switch-runtime-current",
                resolve_user_codex_paths(current.path()).unwrap(),
            )
            .unwrap();
            let mut checkpoints = vec![current_checkpoint];
            if checkpoint_count == 2 {
                checkpoints.push(
                    create_state_backup(shared.path(), backup_root.path(), "switch-runtime-shared")
                        .unwrap(),
                );
            }
            let log = OperationLog::new(log_root.path().join("operations.jsonl"));
            let result: Result<(), String> = Err("injected prewrite failure".to_string());

            record_runtime_switch_result_to_log(
                &log,
                &format!("switch-prewrite-{checkpoint_count}"),
                1,
                &result,
                &checkpoints,
                std::collections::BTreeMap::new(),
                RuntimeSwitchOutcome::FailedBeforeWrite,
            )
            .unwrap();
            let cleanup =
                release_transient_checkpoints(Some(backup_root.path()), true, &checkpoints);

            assert_eq!(cleanup.reclaimed_count, checkpoint_count);
            assert_eq!(cleanup.retained_count, 0);
            assert!(cleanup.warnings.is_empty());
            assert!(checkpoints
                .iter()
                .all(|checkpoint| !checkpoint.backup_dir.exists()));
            let record = log.list(1).unwrap().pop().unwrap();
            assert_eq!(record.status, OperationStatus::Failed);
            assert_eq!(record.phase, OperationPhase::Backup);
            assert_eq!(record.backup_dirs.len(), checkpoint_count);
        }
    }

    #[test]
    fn sync_prewrite_failure_records_backup_phase_before_releasing_both_checkpoints() {
        let current = tempdir().unwrap();
        let shared = tempdir().unwrap();
        let backup_root = tempdir().unwrap();
        let log_root = tempdir().unwrap();
        let checkpoints = vec![
            create_state_backup(current.path(), backup_root.path(), "sync-current").unwrap(),
            create_state_backup(shared.path(), backup_root.path(), "sync-shared").unwrap(),
        ];
        let log = OperationLog::new(log_root.path().join("operations.jsonl"));

        record_sync_failure_to_log(
            &log,
            "sync-prewrite",
            OperationStatus::Failed,
            OperationPhase::Backup,
            1,
            &checkpoints,
        )
        .unwrap();
        let cleanup = release_transient_checkpoints(Some(backup_root.path()), true, &checkpoints);

        assert_eq!(cleanup.reclaimed_count, 2);
        assert_eq!(cleanup.retained_count, 0);
        assert!(checkpoints
            .iter()
            .all(|checkpoint| !checkpoint.backup_dir.exists()));
        let record = log.list(1).unwrap().pop().unwrap();
        assert_eq!(record.status, OperationStatus::Failed);
        assert_eq!(record.phase, OperationPhase::Backup);
    }

    #[test]
    fn terminal_log_failure_retains_prewrite_checkpoint_and_explains_why() {
        let current = tempdir().unwrap();
        let backup_root = tempdir().unwrap();
        let invalid_log_path = tempdir().unwrap();
        let checkpoint = create_runtime_state_backup_with_paths(
            current.path(),
            backup_root.path(),
            "switch-runtime-current",
            resolve_user_codex_paths(current.path()).unwrap(),
        )
        .unwrap();
        let log = OperationLog::new(invalid_log_path.path().to_path_buf());
        let result: Result<(), String> = Err("injected prewrite failure".to_string());
        let terminal_recorded = record_runtime_switch_result_to_log(
            &log,
            "switch-log-failure",
            1,
            &result,
            std::slice::from_ref(&checkpoint),
            std::collections::BTreeMap::new(),
            RuntimeSwitchOutcome::FailedBeforeWrite,
        )
        .is_ok();

        let cleanup = release_transient_checkpoints(
            Some(backup_root.path()),
            terminal_recorded,
            std::slice::from_ref(&checkpoint),
        );

        assert!(!terminal_recorded);
        assert_eq!(cleanup.reclaimed_count, 0);
        assert_eq!(cleanup.retained_count, 1);
        assert!(checkpoint.backup_dir.exists());
        assert!(cleanup.warnings.join(" ").contains("终态记录未能持久化"));
    }

    #[test]
    fn unverified_apply_failure_keeps_checkpoints_and_records_rollback_phase() {
        let current = tempdir().unwrap();
        let backup_root = tempdir().unwrap();
        let log_root = tempdir().unwrap();
        let checkpoint = create_runtime_state_backup_with_paths(
            current.path(),
            backup_root.path(),
            "switch-runtime-current",
            resolve_user_codex_paths(current.path()).unwrap(),
        )
        .unwrap();
        let log = OperationLog::new(log_root.path().join("operations.jsonl"));
        let result: Result<(), String> = Err("injected apply failure".to_string());

        record_runtime_switch_result_to_log(
            &log,
            "switch-apply-failure",
            1,
            &result,
            std::slice::from_ref(&checkpoint),
            std::collections::BTreeMap::new(),
            RuntimeSwitchOutcome::RollbackFailed,
        )
        .unwrap();

        assert!(!failed_runtime_switch_checkpoints_are_releasable(
            RuntimeSwitchOutcome::RollbackFailed,
            std::slice::from_ref(&checkpoint),
        ));
        assert!(checkpoint.backup_dir.exists());
        let record = log.list(1).unwrap().pop().unwrap();
        assert_eq!(record.status, OperationStatus::RollbackFailed);
        assert_eq!(record.phase, OperationPhase::Rollback);
    }

    #[test]
    fn successful_visibility_restore_releases_state_checkpoint_after_terminal_log() {
        let current = tempdir().unwrap();
        let backup_root = tempdir().unwrap();
        let log_root = tempdir().unwrap();
        let checkpoint = create_state_backup(
            current.path(),
            backup_root.path(),
            "restore-sessions-visible",
        )
        .unwrap();
        let checkpoint_dir = checkpoint.backup_dir.clone();
        let log = OperationLog::new(log_root.path().join("operations.jsonl"));
        let mutation = SessionMutationResult {
            selected_count: 1,
            backups: vec![checkpoint],
            deleted_threads: 0,
            deleted_session_files: 0,
            removed_session_index_entries: 0,
            restored_threads: 1,
        };

        let receipt = finish_session_mutation_with_log(
            Ok(log.clone()),
            "restore-visible-success".to_string(),
            OperationAction::RestoreVisibility,
            1,
            Ok(mutation),
            &[],
            Some(backup_root.path()),
        )
        .unwrap();

        assert_eq!(receipt.checkpoint_cleanup.reclaimed_count, 1);
        assert_eq!(receipt.checkpoint_cleanup.retained_count, 0);
        assert!(!checkpoint_dir.exists());
        let record = log.list(1).unwrap().pop().unwrap();
        assert_eq!(record.status, OperationStatus::Succeeded);
        assert_eq!(record.phase, OperationPhase::Complete);
    }

    #[test]
    fn visibility_restore_log_failure_keeps_state_checkpoint() {
        let current = tempdir().unwrap();
        let backup_root = tempdir().unwrap();
        let invalid_log_path = tempdir().unwrap();
        let checkpoint = create_state_backup(
            current.path(),
            backup_root.path(),
            "restore-sessions-visible",
        )
        .unwrap();
        let checkpoint_dir = checkpoint.backup_dir.clone();
        let mutation = SessionMutationResult {
            selected_count: 1,
            backups: vec![checkpoint],
            deleted_threads: 0,
            deleted_session_files: 0,
            removed_session_index_entries: 0,
            restored_threads: 1,
        };

        let receipt = finish_session_mutation_with_log(
            Ok(OperationLog::new(invalid_log_path.path().to_path_buf())),
            "restore-visible-log-failure".to_string(),
            OperationAction::RestoreVisibility,
            1,
            Ok(mutation),
            &[],
            Some(backup_root.path()),
        )
        .unwrap();

        assert_eq!(receipt.checkpoint_cleanup.reclaimed_count, 0);
        assert_eq!(receipt.checkpoint_cleanup.retained_count, 1);
        assert!(checkpoint_dir.exists());
        assert!(receipt.warnings.join(" ").contains("终态记录未能持久化"));
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
    fn failed_hot_sync_restores_shared_state_without_reverting_monotonic_files() {
        let current = tempdir().unwrap();
        let shared = tempdir().unwrap();
        let backups = tempdir().unwrap();
        let current_session = current.path().join("sessions/2026/07/13/current.jsonl");
        let shared_session = shared.path().join("sessions/2026/07/13/shared.jsonl");
        fs::create_dir_all(current_session.parent().unwrap()).unwrap();
        fs::create_dir_all(shared_session.parent().unwrap()).unwrap();
        fs::write(&current_session, "current-before\n").unwrap();
        fs::write(&shared_session, "shared-before\n").unwrap();
        for home in [current.path(), shared.path()] {
            let connection = Connection::open(home.join("state_5.sqlite")).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE marker (value TEXT NOT NULL);
                     INSERT INTO marker (value) VALUES ('before');",
                )
                .unwrap();
        }
        let current_backup =
            create_state_backup(current.path(), backups.path(), "sync-current").unwrap();
        let shared_backup =
            create_state_backup(shared.path(), backups.path(), "sync-shared").unwrap();
        assert_eq!(current_backup.scope, BackupScope::StateOnly);
        assert_eq!(shared_backup.scope, BackupScope::StateOnly);

        let concurrent_session = current.path().join("sessions/2026/07/13/concurrent.jsonl");
        fs::write(&concurrent_session, "created-while-sync-was-running\n").unwrap();
        fs::write(&shared_session, "shared-mutated\n").unwrap();
        Connection::open(shared.path().join("state_5.sqlite"))
            .unwrap()
            .execute("UPDATE marker SET value = 'mutated'", [])
            .unwrap();

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
            "shared-mutated\n"
        );
        let restored: String = Connection::open(shared.path().join("state_5.sqlite"))
            .unwrap()
            .query_row("SELECT value FROM marker", [], |row| row.get(0))
            .unwrap();
        assert_eq!(restored, "before");
    }
}

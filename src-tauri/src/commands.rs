use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Mutex, MutexGuard, TryLockError,
    },
    thread,
    time::Duration,
};
use tauri::ipc::Channel;

#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;

#[cfg(test)]
use crate::backup::restore_backup as restore_backup_for_test;

use crate::{
    backup::{
        cleanup_automatic_checkpoints as cleanup_checkpoint_storage, cleanup_transient_checkpoints,
        create_backup_with_paths, delete_verified_full_backup,
        inspect_checkpoint_storage as inspect_checkpoint_storage_at,
        list_recent_backups as list_backup_snapshots, migrate_legacy_plaintext_auth,
        preflight_backup_capacity_for_sources, preflight_backup_capacity_with_paths,
        restore_backup_with_recovery as restore_backup_snapshot, verify_backup,
        BackupCapacitySource, BackupManifest, BackupScope, BackupSummary, CheckpointCleanupReceipt,
        CheckpointCleanupSummary, CheckpointStorageStatus, RestoreResult,
    },
    codex_home::{scan_codex_home as scan_home, CodexHomeStatus},
    codex_paths::{
        local_codex_paths, resolve_user_codex_paths, validate_absolute_root, CodexPaths,
    },
    diagnostics::platform::open_directory,
    diagnostics::{
        empty_context, global_runtime, DiagnosticEventInput, DiagnosticEventKind, DiagnosticLevel,
        DiagnosticOperation, DiagnosticRecorder, DiagnosticTerminalStatus,
    },
    mobile_continuity::{self, MobileContinuityItemStatus, MobileContinuityStatus},
    operation_log::{
        operation_id, timestamp_millis, OperationAction, OperationLog, OperationPhase,
        OperationRecord, OperationStatus,
    },
    process_control::{
        cache_chatgpt_launch_target, close_codex_processes as close_codex, launch_cached_chatgpt,
        list_codex_process_inventory as list_process_inventory,
        list_codex_processes as list_processes, CodexProcess,
    },
    request_route_switcher::{
        preflight_request_route_switch, switch_request_route_preflighted_with_progress,
    },
    runtime_session_view::recover_pending_transition,
    runtime_store::{
        RelayRuntimeInput, RelaySwitchPreference, RuntimeMetadata, RuntimeStatus, RuntimeStore,
        PLUS_RUNTIME_ID, RELAY_RUNTIME_ID,
    },
    runtime_switcher::{
        BackupReceiptSummary, ChatGptLaunchReceipt, ChatGptLaunchStatus, RelayValidationStatus,
        RuntimeSwitchOutcome, RuntimeSwitchPhase, RuntimeSwitchResult,
    },
    session_incremental::IncrementalSessionSyncStatus,
    session_manager::{
        restore_sessions_visible_detailed_with_prepare as restore_visible,
        scan_managed_sessions as scan_managed_session_inventory, ManagedSessionInventory,
        SessionMutationResult,
    },
    session_scan::{scan_sessions as scan_session_inventory, SessionInventory},
    session_storage::{
        catalog::discover_database_catalog,
        codex_runtime_verifier::NativeCodexBackupVerifier,
        conflict::{
            clear_deferred_conflict, load_deferred_conflict_ids, load_resolved_conflict_ids,
            migration_conflict_candidates_for_namespace, pending_recovery_conflict_candidates,
            record_deferred_conflict, record_resolved_conflict, restore_import_conflict_candidates,
            stable_migration_conflict_candidates, SessionConflictCandidate, SessionConflictList,
        },
        conflict_resolution::{
            cleanup_conflict_resolution_staging, conflict_resolution_operation_id,
            conflict_runtime_apply_plan, deferred_conflict_resolution_receipt,
            execute_conflict_resolution, load_conflict_resolution_plan,
            prepare_conflict_resolution, recover_interrupted_conflict_resolution,
            validate_conflict_resolution, ConflictResolutionAction, ConflictResolutionFailure,
            ConflictResolutionPlan, ConflictResolutionReceipt, ConflictResolutionRecoveryStatus,
            ConflictResolutionStatus,
        },
        downgrade::{
            execute_downgrade_export, load_downgrade_plan, prepare_downgrade_export,
            receipt_from_manifest, recover_interrupted_downgrade_export, verify_downgrade_package,
            verify_downgrade_package_with_runtime, DowngradeExportReceipt, DowngradeRecoveryStatus,
        },
        investigation::{
            create_session_storage_investigation_task as write_session_storage_investigation_task,
            has_investigation_issues, verify_investigation_task, InvestigationDatabaseSummary,
            SessionStorageInvestigationReceipt,
        },
        legacy_backup::{
            cleanup_reconciliation_staging, execute_legacy_backup_reconciliation,
            list_pending_recovery, load_legacy_backup_plan, load_pending_recovery_source,
            prepare_legacy_backup_reconciliation, reconciliation_receipt_from_applied_state,
            recover_interrupted_legacy_backup_reconciliation, update_pending_recovery_status,
            LegacyBackupReconciliationReceipt, LegacyBackupRecoveryStatus, PendingRecoveryList,
            PendingRecoveryStatus,
        },
        metrics::{has_recorded_reclaim_event, record_reclaimed_bytes},
        migration::{
            collect_inventory, load_migration_preflight, migration_backup_sources_for_preflight,
            persist_migration_preflight, run_migration_preflight, MigrationPreflightReport,
        },
        migration_apply::{
            apply_prepared_migration_classified, cleanup_committed_migration_ownership_witnesses,
            cleanup_migration_staging, cleanup_migration_staging_for_operation,
            load_migration_apply_plan, prepare_migration_apply_plan, recover_interrupted_migration,
            validate_applied_migration, verify_applied_migration_with_runtime, MigrationApplyPlan,
            MigrationApplyReceipt, MigrationPreparationReceipt, MigrationRecoveryStatus,
        },
        migration_backup::{
            create_migration_backup, verify_migration_backup, verify_migration_backup_sources,
            verify_migration_backup_with_runtime, MigrationBackupEntryKind,
            MigrationBackupManifest, MigrationBackupStatus,
        },
        model::ShadowScanReport,
        offline_gc::{
            execute_offline_gc, load_offline_gc_plan, prepare_offline_gc_plan,
            recover_interrupted_offline_gc, rollback_unapplied_offline_gc, validate_offline_gc,
            OfflineGcFailure, OfflineGcReceipt, OfflineGcRecoveryStatus,
        },
        operation_ledger::{
            LedgerDatabaseSnapshot, LedgerFileSnapshot, LedgerRollbackStep, OperationLedgerStore,
            RollbackActionKind, SessionStorageOperationKind, SessionStorageOperationPhase,
        },
        provenance::{record_or_verify_route_epoch, RouteEpochInput, RouteProvenanceReceipt},
        reference_graph::path_key,
        restore_import::{
            abort_unapplied_restore_import, cleanup_committed_restore_import_ownership_witnesses,
            cleanup_restore_import_staging, execute_restore_import_classified,
            load_restore_import_plan, prepare_pending_recovery_import, prepare_restore_import,
            recover_interrupted_restore_import, restore_import_runtime_apply_plan,
            validate_applied_restore_import, PreparedRestoreImport, RestoreImportApplyFailure,
            RestoreImportReceipt, RestoreImportRecoveryStatus,
        },
        retention::{
            run_session_storage_retention, SessionStorageRetentionReceipt,
            SESSION_STORAGE_RETENTION_MS,
        },
        shadow_scan::{
            background_shadow_scan_is_running, load_last_shadow_report,
            request_background_shadow_scan, run_shadow_scan,
        },
        storage_state::{
            clear_canonical_storage_state_for_operation, finalize_canonical_storage_state,
            load_committed_canonical_storage_state, load_session_storage_control_state,
            load_session_storage_settings, prepare_canonical_storage_state,
            set_automatic_cleanup_enabled, CanonicalStorageState, SessionStorageControlState,
        },
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

#[cfg(test)]
use crate::{
    backup::{create_state_checkpoint_with_paths, CheckpointRole},
    runtime_switcher::sync_home_with_shared_complete_with_paths,
    session_incremental::save_session_sync_index,
    session_sync::cleanup_obsolete_provider_slots,
};

static MUTATION_COORDINATOR: MutationCoordinator = MutationCoordinator::new();
static AUTOMATIC_GC_RUNNING: AtomicBool = AtomicBool::new(false);
static AUTOMATIC_GC_PENDING: AtomicBool = AtomicBool::new(false);
static AUTOMATIC_GC_GENERATION: AtomicU64 = AtomicU64::new(0);
static AUTOMATIC_GC_REQUEST: Mutex<Option<AutomaticGcRequest>> = Mutex::new(None);
const AUTOMATIC_GC_SAFE_WINDOW_RETRY: Duration = Duration::from_secs(5);
const MAX_LISTED_FULL_BACKUPS: usize = 256;
const MUTATION_ERROR_ENVELOPE_PREFIX: &str = "__CHATGPT_SWITCH_MUTATION_ERROR_V1__";
const MAX_MUTATION_ERROR_MESSAGE_BYTES: usize = 16 * 1024;
const MAX_MUTATION_CORRELATION_ID_BYTES: usize = 160;

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

    fn release_shutdown_reservation(&self) {
        let mut shutdown_lock_file = self
            .shutdown_lock_file
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        shutdown_lock_file.take();
        self.shutdown_pending.store(false, Ordering::Release);
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

fn begin_command_diagnostic(action: &'static str) -> Option<DiagnosticOperation> {
    global_runtime().map(|runtime| runtime.recorder().begin_operation("commands", action))
}

fn diagnostic_correlation_id(operation: Option<&DiagnosticOperation>) -> Option<String> {
    operation.map(|operation| {
        operation
            .operation_id()
            .unwrap_or_else(|| operation.attempt_id())
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MutationErrorEnvelope<'a> {
    message: &'a str,
    operation_id: &'a str,
}

fn correlate_mutation_result<T>(
    result: Result<T, String>,
    operation: Option<&DiagnosticOperation>,
) -> Result<T, String> {
    result.map_err(|message| {
        let envelope = diagnostic_correlation_id(operation)
            .and_then(|operation_id| encode_mutation_error(&message, &operation_id));
        envelope.unwrap_or(message)
    })
}

fn encode_mutation_error(message: &str, operation_id: &str) -> Option<String> {
    if message.len() > MAX_MUTATION_ERROR_MESSAGE_BYTES
        || !is_valid_mutation_correlation_id(operation_id)
    {
        return None;
    }
    let payload = serde_json::to_string(&MutationErrorEnvelope {
        message,
        operation_id,
    })
    .ok()?;
    Some(format!("{MUTATION_ERROR_ENVELOPE_PREFIX}{payload}"))
}

fn is_valid_mutation_correlation_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    (1..=MAX_MUTATION_CORRELATION_ID_BYTES).contains(&bytes.len())
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn record_diagnostic_phase(operation: Option<&DiagnosticOperation>, phase: &'static str) {
    if let Some(operation) = operation.filter(|operation| !operation.is_terminal_recorded()) {
        let _ = operation.phase(phase, empty_context());
    }
}

fn record_diagnostic_branch(
    operation: Option<&DiagnosticOperation>,
    phase: &'static str,
    error_code: &'static str,
    safe_message: &str,
) {
    if let Some(operation) = operation.filter(|operation| !operation.is_terminal_recorded()) {
        let _ = operation.branch(
            DiagnosticLevel::Error,
            Some(phase),
            Some(error_code),
            Some(safe_message),
            empty_context(),
        );
    }
}

fn record_diagnostic_terminal(
    operation: Option<&DiagnosticOperation>,
    status: DiagnosticTerminalStatus,
    phase: &'static str,
    error_code: Option<&'static str>,
    safe_message: Option<&str>,
) {
    if let Some(operation) = operation {
        let _ = operation.terminal(
            status,
            Some(phase),
            error_code,
            safe_message,
            empty_context(),
        );
    }
}

fn record_diagnostic_result<T>(
    operation: Option<&DiagnosticOperation>,
    result: &Result<T, String>,
    success_phase: &'static str,
    failure_phase: &'static str,
    error_code: &'static str,
) {
    match result {
        Ok(_) => record_diagnostic_terminal(
            operation,
            DiagnosticTerminalStatus::Succeeded,
            success_phase,
            None,
            None,
        ),
        Err(error) => record_diagnostic_terminal(
            operation,
            DiagnosticTerminalStatus::Failed,
            failure_phase,
            Some(error_code),
            Some(error),
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn record_durable_command_result<T>(
    operation: Option<&DiagnosticOperation>,
    result: &Result<T, String>,
    success_status: DiagnosticTerminalStatus,
    success_phase: &'static str,
    success_error_code: Option<&'static str>,
    success_message: Option<&'static str>,
    failure_phase: &'static str,
    failure_error_code: &'static str,
) {
    match result {
        Ok(_) => record_diagnostic_terminal(
            operation,
            success_status,
            success_phase,
            success_error_code,
            success_message,
        ),
        Err(error) => {
            let status = terminal_status(error);
            record_diagnostic_terminal(
                operation,
                diagnostic_status_for_command_error(error),
                if matches!(
                    status,
                    OperationStatus::RolledBack | OperationStatus::RollbackFailed
                ) {
                    "rollback"
                } else {
                    failure_phase
                },
                Some(failure_error_code),
                Some(error),
            );
        }
    }
}

fn mobile_publication_terminal(
    status: &MobileContinuityStatus,
    thread_id: &str,
) -> DiagnosticTerminalStatus {
    match status
        .items
        .iter()
        .find(|item| item.thread_id == thread_id)
        .map(|item| item.status)
    {
        Some(MobileContinuityItemStatus::RemotePublished) => DiagnosticTerminalStatus::Succeeded,
        Some(
            MobileContinuityItemStatus::Conflict
            | MobileContinuityItemStatus::NeedsManual
            | MobileContinuityItemStatus::Paused,
        ) => DiagnosticTerminalStatus::Blocked,
        Some(
            MobileContinuityItemStatus::Queued
            | MobileContinuityItemStatus::Publishing
            | MobileContinuityItemStatus::Partial
            | MobileContinuityItemStatus::Retrying,
        )
        | None => DiagnosticTerminalStatus::Partial,
    }
}

fn diagnostic_status_for_command_error(error: &str) -> DiagnosticTerminalStatus {
    let durable_status = terminal_status(error);
    if matches!(
        durable_status,
        OperationStatus::RolledBack | OperationStatus::RollbackFailed
    ) {
        return diagnostic_terminal_status(durable_status);
    }
    let normalized = error.to_ascii_lowercase();
    if normalized.contains("already in progress")
        || normalized.contains("requires explicit confirmation")
        || normalized.contains("requires confirmation")
        || normalized.contains("must be confirmed")
        || normalized.contains("must be closed")
        || normalized.contains("is still running")
        || normalized.contains(" is required")
        || normalized.contains(" is missing")
        || normalized.contains(" is invalid")
        || normalized.contains("invalid relay base url")
        || normalized.contains("service url is not allowed")
        || normalized.contains("must use https")
        || normalized.contains("must end with /v1")
        || normalized.contains("enter a new api key")
        || error.contains("请先")
        || error.contains("请确认")
        || error.contains("不是 ChatGPT 账号登录态")
    {
        DiagnosticTerminalStatus::Blocked
    } else {
        DiagnosticTerminalStatus::Failed
    }
}

fn record_chatgpt_launch_diagnostic(
    operation: Option<&DiagnosticOperation>,
    result: &Result<ChatGptLaunchReceipt, String>,
) {
    match result {
        Ok(receipt)
            if matches!(
                receipt.status,
                ChatGptLaunchStatus::Launched | ChatGptLaunchStatus::AlreadyRunning
            ) =>
        {
            record_diagnostic_terminal(
                operation,
                DiagnosticTerminalStatus::Succeeded,
                "complete",
                None,
                None,
            )
        }
        Ok(receipt) if receipt.status == ChatGptLaunchStatus::Failed => record_diagnostic_terminal(
            operation,
            DiagnosticTerminalStatus::Failed,
            "apply",
            Some("commands.launch_chatgpt.failed"),
            receipt.message.as_deref(),
        ),
        Ok(receipt) => record_diagnostic_terminal(
            operation,
            DiagnosticTerminalStatus::Blocked,
            "apply",
            Some("commands.launch_chatgpt.blocked"),
            receipt.message.as_deref(),
        ),
        Err(error) => record_diagnostic_terminal(
            operation,
            DiagnosticTerminalStatus::Failed,
            "apply",
            Some("commands.launch_chatgpt.failed"),
            Some(error),
        ),
    }
}

fn record_post_mutation_launch_issue(
    operation: Option<&DiagnosticOperation>,
    launch: &ChatGptLaunchReceipt,
    failed_code: &'static str,
    blocked_code: &'static str,
) -> bool {
    match launch.status {
        ChatGptLaunchStatus::Launched | ChatGptLaunchStatus::AlreadyRunning => false,
        ChatGptLaunchStatus::Failed => {
            record_diagnostic_terminal(
                operation,
                DiagnosticTerminalStatus::Partial,
                "launchingApp",
                Some(failed_code),
                launch.message.as_deref(),
            );
            true
        }
        ChatGptLaunchStatus::Blocked | ChatGptLaunchStatus::NotRequested => {
            record_diagnostic_terminal(
                operation,
                DiagnosticTerminalStatus::Blocked,
                "launchingApp",
                Some(blocked_code),
                launch.message.as_deref(),
            );
            true
        }
    }
}

fn record_runtime_switch_success_diagnostic(
    operation: Option<&DiagnosticOperation>,
    receipt: &RuntimeSwitchResult,
) {
    if record_post_mutation_launch_issue(
        operation,
        &receipt.chatgpt_launch,
        "commands.switch_runtime.launch_failed",
        "commands.switch_runtime.launch_blocked",
    ) {
        return;
    }
    match receipt.incremental_session_sync.status {
        IncrementalSessionSyncStatus::Failed => record_diagnostic_terminal(
            operation,
            DiagnosticTerminalStatus::Partial,
            "syncingIncrementalSessions",
            Some("commands.switch_runtime.incremental_failed"),
            Some("request route changed, but incremental session work failed"),
        ),
        IncrementalSessionSyncStatus::NeedsFullSync | IncrementalSessionSyncStatus::Deferred => {
            record_diagnostic_terminal(
                operation,
                DiagnosticTerminalStatus::Partial,
                "syncingIncrementalSessions",
                Some("commands.switch_runtime.incremental_deferred"),
                Some("request route changed, but session work requires a later manual action"),
            )
        }
        IncrementalSessionSyncStatus::Skipped
        | IncrementalSessionSyncStatus::Unchanged
        | IncrementalSessionSyncStatus::Applied => record_diagnostic_terminal(
            operation,
            DiagnosticTerminalStatus::Succeeded,
            "complete",
            None,
            None,
        ),
    }
}

fn record_session_sync_success_diagnostic(
    operation: Option<&DiagnosticOperation>,
    receipt: &SessionSyncReceipt,
) {
    if receipt.checkpoint_cleanup.failed_count > 0 {
        record_diagnostic_terminal(
            operation,
            DiagnosticTerminalStatus::Partial,
            "complete",
            Some("commands.merge_and_repair_sessions.checkpoint_cleanup_partial"),
            Some("session sync completed with incomplete checkpoint cleanup"),
        );
        return;
    }
    if receipt.chatgpt_launch.status != ChatGptLaunchStatus::NotRequested
        && record_post_mutation_launch_issue(
            operation,
            &receipt.chatgpt_launch,
            "commands.merge_and_repair_sessions.launch_failed",
            "commands.merge_and_repair_sessions.launch_blocked",
        )
    {
        return;
    }
    record_diagnostic_terminal(
        operation,
        DiagnosticTerminalStatus::Succeeded,
        "complete",
        None,
        None,
    );
}

fn record_background_failure_to(
    recorder: &DiagnosticRecorder,
    action: &'static str,
    error_code: &'static str,
    error: &str,
) {
    let _ = recorder.record(
        DiagnosticEventInput::new(
            DiagnosticLevel::Error,
            "commands",
            DiagnosticEventKind::BackgroundFailure,
        )
        .with_action(action)
        .with_phase("query")
        .with_error(error_code, error),
    );
}

fn record_background_result<T>(
    action: &'static str,
    error_code: &'static str,
    result: &Result<T, String>,
) {
    let Some(runtime) = global_runtime() else {
        return;
    };
    record_background_result_to(&runtime.recorder(), action, error_code, result);
}

fn record_background_result_to<T>(
    recorder: &DiagnosticRecorder,
    action: &'static str,
    error_code: &'static str,
    result: &Result<T, String>,
) {
    if let Err(error) = result {
        record_background_failure_to(recorder, action, error_code, error);
    }
}

pub(crate) fn mutation_blocks_shutdown() -> bool {
    MUTATION_COORDINATOR.blocks_shutdown()
}

#[derive(Debug, Clone)]
struct AutomaticGcRequest {
    codex_home: PathBuf,
    data_root: PathBuf,
    baseline_scan_id: Option<String>,
    generation: u64,
}

pub(crate) fn schedule_session_storage_startup_recovery() {
    std::mem::drop(tauri::async_runtime::spawn_blocking(|| {
        let result = recover_session_storage_and_pending_view_at_startup();
        record_background_result(
            "recoverSessionStorageOperationsAtStartup",
            "commands.session_storage_startup_recovery.failed",
            &result,
        );
        let mut allow_automatic_gc = false;
        if result.is_ok() {
            let metrics = reconcile_committed_session_storage_reclaim_metrics();
            record_background_result(
                "reconcileSessionStorageReclaimMetricsAtStartup",
                "commands.session_storage_reclaim_metrics.failed",
                &metrics,
            );
            if metrics.is_ok() {
                let retention = run_session_storage_startup_retention();
                record_background_result(
                    "runSessionStorageRetentionAtStartup",
                    "commands.session_storage_startup_retention.failed",
                    &retention,
                );
                allow_automatic_gc = retention.is_ok();
            }
        }
        // A failed recovery/retention pass must not suppress the read-only
        // Shadow report, but it must suppress every automatic deletion path.
        schedule_cleanup_for_current_roots(allow_automatic_gc);
    }));
}

fn recover_session_storage_and_pending_view_at_startup() -> Result<usize, String> {
    let recovered_count = recover_session_storage_operations_at_startup()?;
    let _mutation_guard = acquire_mutation_lock()?;
    let codex_home = managed_codex_home()?;
    let data_root = appdata_root()?.join("codex-switch");
    ensure_no_codex_writer_for_session_storage(
        "recovering an interrupted canonical session view transition",
    )?;
    let view_recovered = recover_pending_transition(&codex_home, &data_root)?;
    Ok(recovered_count.saturating_add(if view_recovered { 1 } else { 0 }))
}

fn reconcile_committed_session_storage_reclaim_metrics() -> Result<usize, String> {
    let _mutation_guard = acquire_mutation_lock()?;
    let data_root = appdata_root()?.join("codex-switch");
    if !data_root.exists() {
        return Ok(0);
    }
    let store = OperationLedgerStore::new(&data_root);
    let mut reconciled = 0_usize;
    for ledger in store.all()? {
        if ledger.phase != SessionStorageOperationPhase::Committed
            || has_recorded_reclaim_event(&data_root, &ledger.operation_id)?
        {
            continue;
        }
        let reclaimed_bytes = match ledger.kind {
            SessionStorageOperationKind::OfflineGc => {
                load_offline_gc_plan(&data_root, &ledger.operation_id)?
                    .candidates
                    .iter()
                    .try_fold(0_u64, |total, candidate| {
                        total.checked_add(candidate.body_bytes)
                    })
                    .ok_or_else(|| "offline GC reclaimed byte count overflowed".to_string())?
            }
            SessionStorageOperationKind::LegacyBackupReconciliation => {
                let plan = load_legacy_backup_plan(&data_root, &ledger.operation_id)?;
                reconciliation_receipt_from_applied_state(&plan)?.reclaimed_bytes
            }
            _ => continue,
        };
        record_reclaimed_bytes(&data_root, &ledger.operation_id, reclaimed_bytes)?;
        reconciled = reconciled.saturating_add(1);
    }
    Ok(reconciled)
}

fn run_session_storage_startup_retention() -> Result<Option<SessionStorageRetentionReceipt>, String>
{
    let _mutation_guard = acquire_mutation_lock()?;
    let appdata = appdata_root()?;
    let data_root = appdata.join("codex-switch");
    if !data_root.exists() {
        return Ok(None);
    }
    let canonical_root = managed_codex_home()?;
    let cutoff = timestamp_millis()?.saturating_sub(SESSION_STORAGE_RETENTION_MS);
    OperationLog::from_appdata(&appdata).prune_completed_before(cutoff)?;
    let active =
        list_managed_processes_for_closed_mutation("expiring session storage recovery packages")?;
    if !active.is_empty() {
        return Ok(None);
    }
    let migration = load_committed_canonical_storage_state(&data_root, &canonical_root)?;
    let now_ms = timestamp_millis()?;
    let receipt = run_session_storage_retention(
        &data_root,
        migration
            .as_ref()
            .map(|state| state.migration_operation_id.as_str()),
        now_ms,
    )?;
    if receipt.reclaimed_bytes > 0 {
        record_reclaimed_bytes(
            &data_root,
            &format!("session-storage-retention-{now_ms}"),
            receipt.reclaimed_bytes,
        )?;
    }
    Ok(Some(receipt))
}

fn recover_session_storage_operations_at_startup() -> Result<usize, String> {
    let _mutation_guard = acquire_mutation_lock()?;
    let data_root = appdata_root()?.join("codex-switch");
    let store = OperationLedgerStore::new(&data_root);
    let interrupted = store
        .unfinished()?
        .into_iter()
        .filter(|ledger| match ledger.kind {
            SessionStorageOperationKind::Migration => true,
            SessionStorageOperationKind::OfflineGc => true,
            SessionStorageOperationKind::ConflictResolution => matches!(
                ledger.phase,
                SessionStorageOperationPhase::Preflight
                    | SessionStorageOperationPhase::Backup
                    | SessionStorageOperationPhase::BackupVerified
                    | SessionStorageOperationPhase::PlanReady
                    | SessionStorageOperationPhase::Applying
                    | SessionStorageOperationPhase::Validating
                    | SessionStorageOperationPhase::RollingBack
            ),
            SessionStorageOperationKind::DowngradeExport => true,
            SessionStorageOperationKind::RestoreImport => true,
            SessionStorageOperationKind::LegacyBackupReconciliation => true,
        })
        .collect::<Vec<_>>();
    cleanup_terminal_ownership_witnesses(&store, &data_root)?;
    if interrupted.is_empty() {
        return Ok(0);
    }

    let active = list_managed_processes_for_closed_mutation(
        "recovering an interrupted session storage operation",
    )?;
    if !active.is_empty() {
        return Err(format!(
            "session storage recovery is waiting for every Codex writer to close; activeProcesses={}",
            active.len()
        ));
    }

    let mut recovered_count = 0_usize;
    for ledger in interrupted {
        match ledger.kind {
            SessionStorageOperationKind::Migration => {
                if matches!(
                    ledger.phase,
                    SessionStorageOperationPhase::Available
                        | SessionStorageOperationPhase::Preflight
                        | SessionStorageOperationPhase::Backup
                        | SessionStorageOperationPhase::BackupVerified
                        | SessionStorageOperationPhase::PlanReady
                ) || (ledger.phase == SessionStorageOperationPhase::RollingBack
                    && !ledger.live_mutation_started)
                {
                    rollback_unapplied_session_storage_migration(
                        &store,
                        &data_root,
                        &ledger.operation_id,
                        "migrationInterruptedBeforeApply",
                    )?;
                    recovered_count = recovered_count.saturating_add(1);
                    continue;
                }
                let recovery = recover_interrupted_migration(
                    &store,
                    &data_root,
                    &ledger.operation_id,
                    || {
                        ensure_no_codex_writer_for_session_storage(
                            "performing an interrupted session storage migration rollback write",
                        )
                    },
                )?;
                match recovery.status {
                    MigrationRecoveryStatus::RolledBack => {
                        clear_canonical_storage_state_for_operation(
                            &data_root,
                            &ledger.operation_id,
                        )?;
                        recovered_count = recovered_count.saturating_add(1);
                    }
                    MigrationRecoveryStatus::DeferredByLiveWriter => {
                        return Err(
                            "session storage migration recovery was deferred by an active writer"
                                .to_string(),
                        );
                    }
                    MigrationRecoveryStatus::Failed => {
                        return Err(
                            "session storage migration recovery left recoverable residuals"
                                .to_string(),
                        );
                    }
                }
            }
            SessionStorageOperationKind::OfflineGc => {
                if matches!(
                    ledger.phase,
                    SessionStorageOperationPhase::Available
                        | SessionStorageOperationPhase::Preflight
                        | SessionStorageOperationPhase::Backup
                        | SessionStorageOperationPhase::BackupVerified
                        | SessionStorageOperationPhase::PlanReady
                ) {
                    rollback_unapplied_offline_gc(
                        &store,
                        &data_root,
                        &ledger.operation_id,
                        "offlineGcInterruptedBeforeApply",
                    )?;
                    recovered_count = recovered_count.saturating_add(1);
                    continue;
                }
                let recovery = recover_interrupted_offline_gc(
                    &store,
                    &data_root,
                    &ledger.operation_id,
                    || {
                        ensure_no_codex_writer_for_session_storage(
                            "performing an interrupted offline GC rollback write",
                        )
                    },
                )?;
                match recovery {
                    OfflineGcRecoveryStatus::RolledBack => {
                        recovered_count = recovered_count.saturating_add(1);
                    }
                    OfflineGcRecoveryStatus::DeferredByLiveWriter => {
                        return Err(
                            "offline GC recovery was deferred by an active writer".to_string()
                        );
                    }
                    OfflineGcRecoveryStatus::Failed => {
                        return Err("offline GC recovery left recoverable residuals".to_string());
                    }
                }
            }
            SessionStorageOperationKind::ConflictResolution => {
                let recovery = recover_interrupted_conflict_resolution(
                    &store,
                    &data_root,
                    &ledger.operation_id,
                    || {
                        ensure_no_codex_writer_for_session_storage(
                            "performing an interrupted conflict resolution rollback write",
                        )
                    },
                )?;
                match recovery {
                    ConflictResolutionRecoveryStatus::RolledBack => {
                        recovered_count = recovered_count.saturating_add(1);
                    }
                    ConflictResolutionRecoveryStatus::DeferredByLiveWriter => {
                        return Err(
                            "conflict resolution recovery was deferred by an active writer"
                                .to_string(),
                        );
                    }
                    ConflictResolutionRecoveryStatus::Failed => {
                        return Err(
                            "conflict resolution recovery left recoverable residuals".to_string()
                        );
                    }
                }
            }
            SessionStorageOperationKind::DowngradeExport => {
                let recovery = recover_interrupted_downgrade_export(
                    &store,
                    &data_root,
                    &ledger.operation_id,
                    || {
                        ensure_no_codex_writer_for_session_storage(
                            "recovering an interrupted downgrade export",
                        )
                    },
                )?;
                match recovery.status {
                    DowngradeRecoveryStatus::Committed | DowngradeRecoveryStatus::RolledBack => {
                        recovered_count = recovered_count.saturating_add(1);
                    }
                    DowngradeRecoveryStatus::ResidualPreserved => {
                        return Err("downgrade export recovery preserved an unverified residual"
                            .to_string());
                    }
                }
            }
            SessionStorageOperationKind::RestoreImport => {
                let recovery = recover_interrupted_restore_import(
                    &store,
                    &data_root,
                    &ledger.operation_id,
                    || {
                        ensure_no_codex_writer_for_session_storage(
                            "recovering an interrupted downgrade restore import",
                        )
                    },
                )?;
                match recovery {
                    RestoreImportRecoveryStatus::RolledBack => {
                        recovered_count = recovered_count.saturating_add(1);
                    }
                    RestoreImportRecoveryStatus::DeferredByLiveWriter => {
                        return Err(
                            "restore import recovery was deferred by an active writer".to_string()
                        );
                    }
                    RestoreImportRecoveryStatus::Failed => {
                        return Err(
                            "restore import recovery left recoverable residuals".to_string()
                        );
                    }
                }
            }
            SessionStorageOperationKind::LegacyBackupReconciliation => {
                let recovery = recover_interrupted_legacy_backup_reconciliation(
                    &store,
                    &data_root,
                    &ledger.operation_id,
                    || {
                        ensure_no_codex_writer_for_session_storage(
                            "recovering an interrupted legacy backup reconciliation",
                        )
                    },
                )?;
                match recovery {
                    LegacyBackupRecoveryStatus::Committed => {
                        let plan = load_legacy_backup_plan(&data_root, &ledger.operation_id)?;
                        let receipt = reconciliation_receipt_from_applied_state(&plan)?;
                        record_reclaimed_bytes(
                            &data_root,
                            &ledger.operation_id,
                            receipt.reclaimed_bytes,
                        )?;
                        recovered_count = recovered_count.saturating_add(1);
                    }
                    LegacyBackupRecoveryStatus::RolledBack => {
                        recovered_count = recovered_count.saturating_add(1);
                    }
                    LegacyBackupRecoveryStatus::DeferredByLiveWriter => {
                        return Err(
                            "legacy backup recovery was deferred by an active writer".to_string()
                        );
                    }
                    LegacyBackupRecoveryStatus::Failed => {
                        return Err("legacy backup recovery left recoverable residuals".to_string());
                    }
                }
            }
        }
    }
    Ok(recovered_count)
}

fn cleanup_terminal_ownership_witnesses(
    store: &OperationLedgerStore,
    data_root: &Path,
) -> Result<(), String> {
    for ledger in store.all()? {
        if ledger.phase != SessionStorageOperationPhase::Committed {
            continue;
        }
        let cleanup = match ledger.kind {
            SessionStorageOperationKind::Migration => {
                load_migration_apply_plan(data_root, &ledger.operation_id)
                    .and_then(|plan| cleanup_committed_migration_ownership_witnesses(&plan))
            }
            SessionStorageOperationKind::RestoreImport => {
                load_restore_import_plan(data_root, &ledger.operation_id)
                    .and_then(|plan| cleanup_committed_restore_import_ownership_witnesses(&plan))
            }
            _ => continue,
        };
        record_background_result(
            "cleanupCommittedSessionStorageOwnershipWitnessesAtStartup",
            "commands.session_storage_startup.witness_cleanup_failed",
            &cleanup,
        );
    }
    Ok(())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppStatus {
    pub app_name: &'static str,
    pub version: &'static str,
    pub phase: &'static str,
    pub codex_home: PathBuf,
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
    pub chatgpt_launch: ChatGptLaunchReceipt,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub enum SessionSyncPhase {
    Preparing,
    ClosingApp,
    BackingUp,
    Reconciling,
    RecordingResult,
    LaunchingApp,
    Complete,
    Failed,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionSyncProgress {
    pub phase: SessionSyncPhase,
    pub timestamp_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
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
    pub operation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<RuntimeSwitchOutcome>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AppExitRequestResult {
    pub scheduled: bool,
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
    let result = match tauri::async_runtime::spawn_blocking(check_latest_release).await {
        Ok(result) => result,
        Err(_) => Err("update check worker failed".to_string()),
    };
    record_background_result(
        "checkForUpdates",
        "commands.check_for_updates.failed",
        &result,
    );
    result
}

#[tauri::command]
pub async fn install_update(app: tauri::AppHandle) -> Result<UpdateInstallReceipt, String> {
    let diagnostic = begin_command_diagnostic("installUpdate");
    record_diagnostic_phase(diagnostic.as_ref(), "preflight");
    let worker_diagnostic = diagnostic.clone();
    let result = match tauri::async_runtime::spawn_blocking(move || {
        let mutation_guard = acquire_mutation_lock()?;
        record_diagnostic_phase(worker_diagnostic.as_ref(), "apply");
        let result = install_latest_update();
        if result.is_ok() {
            mutation_guard.hold_until_process_exit();
        }
        result
    })
    .await
    {
        Ok(result) => result,
        Err(_) => Err("update installer worker failed".to_string()),
    };
    record_diagnostic_result(
        diagnostic.as_ref(),
        &result,
        "complete",
        "apply",
        "commands.install_update.failed",
    );
    let receipt = correlate_mutation_result(result, diagnostic.as_ref())?;
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(500));
        app.exit(0);
    });
    Ok(receipt)
}

#[tauri::command]
pub fn request_app_exit(app: tauri::AppHandle) -> Result<AppExitRequestResult, String> {
    let diagnostic = begin_command_diagnostic("requestAppExit");
    record_diagnostic_phase(diagnostic.as_ref(), "preflight");
    let result = (|| {
        let lock_path = appdata_root()?.join("codex-switch").join("mutation.lock");
        prepare_app_exit_at(&MUTATION_COORDINATOR, &lock_path)
    })();
    match &result {
        Ok(true) => record_diagnostic_terminal(
            diagnostic.as_ref(),
            DiagnosticTerminalStatus::Succeeded,
            "complete",
            None,
            None,
        ),
        Ok(false) => record_diagnostic_terminal(
            diagnostic.as_ref(),
            DiagnosticTerminalStatus::Blocked,
            "preflight",
            Some("commands.request_app_exit.mutation_busy"),
            Some("application exit is waiting for the active mutation"),
        ),
        Err(error) => record_diagnostic_terminal(
            diagnostic.as_ref(),
            DiagnosticTerminalStatus::Failed,
            "preflight",
            Some("commands.request_app_exit.failed"),
            Some(error),
        ),
    }
    let scheduled = correlate_mutation_result(result, diagnostic.as_ref())?;
    if scheduled {
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            app.exit(0);
            // A prevented or stalled exit must not leave every later mutation
            // permanently blocked for the rest of the process lifetime.
            thread::sleep(Duration::from_secs(2));
            MUTATION_COORDINATOR.release_shutdown_reservation();
        });
    }
    Ok(AppExitRequestResult { scheduled })
}

#[tauri::command]
pub fn get_update_startup_notice() -> Option<UpdateStartupNotice> {
    startup_update_notice()
}

#[tauri::command]
pub fn scan_codex_home() -> Result<CodexHomeStatus, String> {
    let result = (|| scan_home(&managed_codex_home()?))();
    record_background_result("scanCodexHome", "commands.scan_codex_home.failed", &result);
    result
}

#[tauri::command]
pub fn scan_sessions() -> Result<SessionInventory, String> {
    let result = (|| scan_session_inventory(&managed_codex_home()?))();
    record_background_result("scanSessions", "commands.scan_sessions.failed", &result);
    result
}

#[tauri::command]
pub fn scan_managed_sessions() -> Result<ManagedSessionInventory, String> {
    let result = (|| {
        let shared_home = default_shared_sessions_root()?;
        scan_managed_session_inventory(&managed_codex_home()?, &shared_home)
    })();
    record_background_result(
        "scanManagedSessions",
        "commands.scan_managed_sessions.failed",
        &result,
    );
    result
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationCancellationReceipt {
    pub operation_id: String,
    pub backup_retained: bool,
    pub staging_discarded: bool,
}

#[tauri::command]
pub fn get_session_storage_status() -> Result<Option<ShadowScanReport>, String> {
    let result = (|| load_last_shadow_report(&appdata_root()?.join("codex-switch")))();
    record_background_result(
        "getSessionStorageStatus",
        "commands.get_session_storage_status.failed",
        &result,
    );
    result
}

#[tauri::command]
pub fn get_session_storage_control_state() -> Result<SessionStorageControlState, String> {
    let result = (|| {
        let codex_home = managed_codex_home()?;
        let data_root = appdata_root()?.join("codex-switch");
        load_session_storage_control_state(&data_root, &codex_home)
    })();
    record_background_result(
        "getSessionStorageControlState",
        "commands.get_session_storage_control_state.failed",
        &result,
    );
    result
}

#[tauri::command]
pub async fn create_session_storage_investigation_task(
) -> Result<SessionStorageInvestigationReceipt, String> {
    let result = tauri::async_runtime::spawn_blocking(|| {
        let codex_home = managed_codex_home()?;
        let data_root = appdata_root()?.join("codex-switch");
        let report = load_last_shadow_report(&data_root)?
            .ok_or_else(|| "a completed session storage scan is required".to_string())?;
        if !has_investigation_issues(&report) {
            return Err(
                "the latest session storage scan has no issue for Codex investigation".to_string(),
            );
        }
        let discovery = discover_database_catalog(&codex_home, &data_root);
        let databases = discovery
            .descriptors
            .into_iter()
            .map(|database| InvestigationDatabaseSummary {
                database_id: database.id,
                role: database.role,
            })
            .collect::<Vec<_>>();
        write_session_storage_investigation_task(
            &data_root,
            env!("CARGO_PKG_VERSION"),
            &report,
            &databases,
        )
    })
    .await
    .map_err(|_| "session storage investigation task worker failed".to_string())?;
    record_background_result(
        "createSessionStorageInvestigationTask",
        "commands.create_session_storage_investigation_task.failed",
        &result,
    );
    result
}

#[tauri::command]
pub async fn open_session_storage_investigation_task(task_id: String) -> Result<(), String> {
    let result = tauri::async_runtime::spawn_blocking(move || {
        let data_root = appdata_root()?.join("codex-switch");
        let task_dir = verify_investigation_task(&data_root, &task_id)?;
        open_directory(&task_dir)
    })
    .await
    .map_err(|_| "session storage investigation task opener failed".to_string())?;
    record_background_result(
        "openSessionStorageInvestigationTask",
        "commands.open_session_storage_investigation_task.failed",
        &result,
    );
    result
}

#[tauri::command]
pub async fn set_session_storage_automatic_cleanup(
    enabled: bool,
) -> Result<SessionStorageControlState, String> {
    let result = tauri::async_runtime::spawn_blocking(move || {
        let mutation_guard = acquire_mutation_lock()?;
        let codex_home = managed_codex_home()?;
        let data_root = appdata_root()?.join("codex-switch");
        set_automatic_cleanup_enabled(&data_root, enabled)?;
        let state = load_session_storage_control_state(&data_root, &codex_home)?;
        drop(mutation_guard);
        if enabled {
            let _ = request_shadow_and_automatic_gc(codex_home, data_root);
        } else {
            AUTOMATIC_GC_PENDING.store(false, Ordering::Release);
            let _ = request_background_shadow_scan(codex_home, data_root);
        }
        Ok(state)
    })
    .await
    .map_err(|_| "session storage settings worker failed".to_string())?;
    record_background_result(
        "setSessionStorageAutomaticCleanup",
        "commands.set_session_storage_automatic_cleanup.failed",
        &result,
    );
    result
}

#[tauri::command]
pub async fn scan_session_storage() -> Result<ShadowScanReport, String> {
    let result = tauri::async_runtime::spawn_blocking(|| {
        run_shadow_scan(
            &managed_codex_home()?,
            &appdata_root()?.join("codex-switch"),
        )
    })
    .await
    .map_err(|_| "session storage shadow scan worker failed".to_string())?;
    record_background_result(
        "scanSessionStorage",
        "commands.scan_session_storage.failed",
        &result,
    );
    result
}

#[tauri::command]
pub async fn list_session_storage_conflicts(
    migration_operation_id: String,
) -> Result<SessionConflictList, String> {
    let result = tauri::async_runtime::spawn_blocking(move || {
        let _mutation_guard = acquire_mutation_lock()?;
        let codex_home = managed_codex_home()?;
        let data_root = appdata_root()?.join("codex-switch");
        let store = OperationLedgerStore::new(&data_root);
        let canonical_state =
            validate_committed_migration(&data_root, &codex_home, &migration_operation_id)?;
        let mut resolved =
            load_resolved_conflict_ids(&data_root, &codex_home, &migration_operation_id)?;
        let deferred =
            load_deferred_conflict_ids(&data_root, &codex_home, &migration_operation_id)?;
        resolved.extend(
            committed_conflict_resolution_plans(
                &store,
                &data_root,
                &codex_home,
                &migration_operation_id,
            )?
            .into_iter()
            .map(|plan| plan.conflict_id),
        );
        let conflicts = collect_session_conflict_candidates(
            &store,
            &data_root,
            &codex_home,
            &migration_operation_id,
            canonical_state.committed_at_ms,
            &resolved,
        )?
        .into_iter()
        .map(|mut candidate| {
            candidate.summary.deferred = deferred.contains(&candidate.summary.conflict_id);
            candidate.summary
        })
        .collect();
        Ok(SessionConflictList {
            migration_operation_id,
            conflicts,
        })
    })
    .await
    .map_err(|_| "session storage conflict listing worker failed".to_string())?;
    record_background_result(
        "listSessionStorageConflicts",
        "commands.list_session_storage_conflicts.failed",
        &result,
    );
    result
}

#[tauri::command]
pub async fn resolve_session_storage_conflict(
    migration_operation_id: String,
    conflict_id: String,
    action: ConflictResolutionAction,
) -> Result<ConflictResolutionReceipt, String> {
    let result = tauri::async_runtime::spawn_blocking(move || {
        let _mutation_guard = acquire_mutation_lock()?;
        let codex_home = managed_codex_home()?;
        let data_root = appdata_root()?.join("codex-switch");
        let store = OperationLedgerStore::new(&data_root);
        let canonical_state =
            validate_committed_migration(&data_root, &codex_home, &migration_operation_id)?;
        if let Some(plan) = committed_conflict_resolution_plans(
            &store,
            &data_root,
            &codex_home,
            &migration_operation_id,
        )?
        .into_iter()
        .find(|plan| plan.conflict_id == conflict_id)
        {
            let registry_result = record_resolved_conflict(
                &data_root,
                &codex_home,
                &migration_operation_id,
                &conflict_id,
            );
            record_background_result(
                "recordResolvedSessionStorageConflict",
                "commands.session_storage_conflict.resolved_registry_failed",
                &registry_result,
            );
            let deferred_result = clear_deferred_conflict(
                &data_root,
                &codex_home,
                &migration_operation_id,
                &conflict_id,
            );
            record_background_result(
                "clearDeferredSessionStorageConflict",
                "commands.session_storage_conflict.deferred_registry_failed",
                &deferred_result,
            );
            return validate_conflict_resolution(&plan, conflict_resolution_receipt(&plan));
        }
        if load_resolved_conflict_ids(&data_root, &codex_home, &migration_operation_id)?
            .contains(&conflict_id)
        {
            return Err("session conflict was already resolved".to_string());
        }
        let candidate = collect_session_conflict_candidates(
            &store,
            &data_root,
            &codex_home,
            &migration_operation_id,
            canonical_state.committed_at_ms,
            &BTreeSet::new(),
        )?
        .into_iter()
        .find(|candidate| candidate.summary.conflict_id == conflict_id)
        .ok_or_else(|| "session conflict identity is unavailable".to_string())?;
        if action == ConflictResolutionAction::Defer {
            record_deferred_conflict(
                &data_root,
                &codex_home,
                &migration_operation_id,
                &candidate.summary.conflict_id,
            )?;
            return Ok(deferred_conflict_resolution_receipt(
                &migration_operation_id,
                &candidate.summary.conflict_id,
            ));
        }
        let deterministic_id = conflict_resolution_operation_id(&conflict_id)?;
        let operation_id = match store.try_load(&deterministic_id)? {
            None => deterministic_id,
            Some(existing) if !existing.phase.is_terminal() => {
                return Err("an interrupted conflict resolution must be recovered first".to_string())
            }
            Some(_) => operation_id("conflict-resolution")?,
        };
        if candidate.summary.newer_version.is_none() {
            return Err(
                "conflict timestamps are not reliable enough to recommend overwrite".to_string(),
            );
        }
        let report = candidate.resolution_report.as_ref().ok_or_else(|| {
            "conflict content is not reliable enough to permit overwrite".to_string()
        })?;
        ensure_no_codex_writer_for_session_storage("resolving a session storage conflict")?;
        if !store.unfinished()?.is_empty() {
            return Err(
                "another unfinished session storage operation must be recovered first".to_string(),
            );
        }
        store.create(
            &operation_id,
            SessionStorageOperationKind::ConflictResolution,
            &codex_home,
        )?;
        store.transition(&operation_id, SessionStorageOperationPhase::Preflight)?;
        store.transition(&operation_id, SessionStorageOperationPhase::Backup)?;
        let recovery_dir = data_root
            .join("session-storage-v1/conflict-recovery")
            .join(&operation_id);
        store.update(&operation_id, |ledger| {
            ledger.backup_root = Some(recovery_dir.clone());
            Ok(())
        })?;
        let prepared = match prepare_conflict_resolution(
            &codex_home,
            &data_root,
            &operation_id,
            report,
            &conflict_id,
        ) {
            Ok(prepared) => prepared,
            Err(error) => {
                fail_session_storage_operation(
                    &store,
                    &operation_id,
                    "conflictResolutionPlanningFailed",
                )?;
                return Err(error);
            }
        };
        store.update(&operation_id, |ledger| {
            ledger.backup_root = Some(prepared.plan.backup_dir.clone());
            ledger.created_files = prepared.created_files.clone();
            ledger.database_snapshots = prepared.database_snapshots.clone();
            ledger.rollback_steps = prepared.rollback_steps.clone();
            ledger.last_error_code = None;
            Ok(())
        })?;
        store.transition(&operation_id, SessionStorageOperationPhase::BackupVerified)?;
        store.transition(&operation_id, SessionStorageOperationPhase::PlanReady)?;
        store.transition(&operation_id, SessionStorageOperationPhase::Applying)?;
        if let Err(error) = ensure_no_codex_writer_for_session_storage(
            "performing the final conflict resolution write check",
        ) {
            rollback_conflict_resolution_after_failure(
                &store,
                &data_root,
                &operation_id,
                "conflictResolutionWriterAppearedBeforeApply",
            )?;
            return Err(error);
        }
        let receipt = match execute_conflict_resolution(&prepared.plan, || {
            ensure_no_codex_writer_for_session_storage(
                "performing a session storage conflict resolution write",
            )
        }) {
            Ok(receipt) => receipt,
            Err(failure) => {
                let error_code = match &failure {
                    ConflictResolutionFailure::LiveWriteGuard(_) => {
                        "conflictResolutionWriterAppeared"
                    }
                    ConflictResolutionFailure::Operation(_) => "conflictResolutionApplyFailed",
                };
                rollback_conflict_resolution_after_failure(
                    &store,
                    &data_root,
                    &operation_id,
                    error_code,
                )?;
                return Err(failure.message().to_string());
            }
        };
        store.transition(&operation_id, SessionStorageOperationPhase::Validating)?;
        let validation = (|| {
            ensure_no_codex_writer_for_session_storage(
                "validating the session storage conflict resolution",
            )?;
            let mut receipt = validate_conflict_resolution(&prepared.plan, receipt)?;
            let verifier = NativeCodexBackupVerifier::discover()?;
            receipt.runtime_verification = Some(verify_applied_migration_with_runtime(
                &conflict_runtime_apply_plan(&prepared.plan),
                &verifier,
            )?);
            ensure_no_codex_writer_for_session_storage(
                "finishing session storage conflict runtime verification",
            )?;
            validate_conflict_resolution(&prepared.plan, receipt)
        })();
        let receipt = match validation {
            Ok(receipt) => receipt,
            Err(error) => {
                rollback_conflict_resolution_after_failure(
                    &store,
                    &data_root,
                    &operation_id,
                    "conflictResolutionValidationFailed",
                )?;
                return Err(error);
            }
        };
        if let Err(error) = cleanup_conflict_resolution_staging(&prepared.plan) {
            rollback_conflict_resolution_after_failure(
                &store,
                &data_root,
                &operation_id,
                "conflictResolutionStagingCleanupFailed",
            )?;
            return Err(error);
        }
        match transition_to_committed(&store, &operation_id) {
            CommitTransitionDisposition::ConfirmedCommitted => {}
            CommitTransitionDisposition::SafeToRollback(error)
            | CommitTransitionDisposition::PreserveForRecovery(error) => return Err(error),
        }
        let registry_result = record_resolved_conflict(
            &data_root,
            &codex_home,
            &migration_operation_id,
            &conflict_id,
        );
        record_background_result(
            "recordResolvedSessionStorageConflict",
            "commands.session_storage_conflict.resolved_registry_failed",
            &registry_result,
        );
        let deferred_result = clear_deferred_conflict(
            &data_root,
            &codex_home,
            &migration_operation_id,
            &conflict_id,
        );
        record_background_result(
            "clearDeferredSessionStorageConflict",
            "commands.session_storage_conflict.deferred_registry_failed",
            &deferred_result,
        );
        for entry_id in &candidate.pending_recovery_entry_ids {
            let _ = update_pending_recovery_status(
                &data_root,
                &migration_operation_id,
                entry_id,
                PendingRecoveryStatus::Restored,
            );
        }
        let _ = request_shadow_and_automatic_gc(codex_home, data_root);
        Ok(receipt)
    })
    .await
    .map_err(|_| "session storage conflict resolution worker failed".to_string())?;
    record_background_result(
        "resolveSessionStorageConflict",
        "commands.resolve_session_storage_conflict.failed",
        &result,
    );
    result
}

fn validate_committed_migration(
    data_root: &Path,
    canonical_root: &Path,
    migration_operation_id: &str,
) -> Result<CanonicalStorageState, String> {
    let state =
        load_committed_canonical_storage_state(data_root, canonical_root)?.ok_or_else(|| {
            "conflict review requires a committed session storage migration".to_string()
        })?;
    if state.migration_operation_id != migration_operation_id {
        return Err("conflict review requires a committed session storage migration".to_string());
    }
    Ok(state)
}

fn collect_session_conflict_candidates(
    store: &OperationLedgerStore,
    data_root: &Path,
    canonical_root: &Path,
    migration_operation_id: &str,
    migration_committed_at_ms: u128,
    excluded_conflict_ids: &BTreeSet<String>,
) -> Result<Vec<SessionConflictCandidate>, String> {
    let mut by_id = BTreeMap::<String, SessionConflictCandidate>::new();
    let ledgers = store.all()?;
    for ledger in ledgers.iter().filter(|ledger| {
        ledger.kind == SessionStorageOperationKind::Migration
            && ledger.phase == SessionStorageOperationPhase::Committed
            && path_key(&ledger.canonical_root) == path_key(canonical_root)
    }) {
        let report = load_migration_preflight(data_root, &ledger.operation_id)?;
        if report.operation_id != ledger.operation_id
            || path_key(&report.plan.canonical_root) != path_key(canonical_root)
        {
            return Err("retained migration conflict proof is inconsistent".to_string());
        }
        for candidate in migration_conflict_candidates_for_namespace(
            &report,
            migration_operation_id,
            excluded_conflict_ids,
        )? {
            merge_session_conflict_candidate(&mut by_id, candidate);
        }
    }
    for candidate in stable_migration_conflict_candidates(
        canonical_root,
        data_root,
        migration_operation_id,
        excluded_conflict_ids,
    )? {
        merge_session_conflict_candidate(&mut by_id, candidate);
    }
    let now_ms = timestamp_millis()?;
    for ledger in ledgers.into_iter().filter(|ledger| {
        ledger.kind == SessionStorageOperationKind::RestoreImport
            && ledger.phase == SessionStorageOperationPhase::Committed
            && path_key(&ledger.canonical_root) == path_key(canonical_root)
            && ledger.started_at_ms >= migration_committed_at_ms
    }) {
        let plan = load_restore_import_plan(data_root, &ledger.operation_id)?;
        if path_key(&plan.canonical_root) != path_key(canonical_root) {
            return Err("restore conflict proof is inconsistent".to_string());
        }
        for candidate in restore_import_conflict_candidates(migration_operation_id, &plan)? {
            if !excluded_conflict_ids.contains(&candidate.summary.conflict_id) {
                merge_session_conflict_candidate(&mut by_id, candidate);
            }
        }
    }
    let pending = list_pending_recovery(data_root, migration_operation_id)?;
    if pending.entries.iter().any(|entry| {
        entry.status == PendingRecoveryStatus::Pending
            && matches!(
                entry.relation,
                crate::session_storage::legacy_backup::PendingRecoveryRelation::Divergent
                    | crate::session_storage::legacy_backup::PendingRecoveryRelation::Unknown
            )
            && entry.expires_at_ms > now_ms
    }) {
        let inventory = collect_inventory(canonical_root, data_root)?;
        let scan = pending_recovery_conflict_candidates(
            canonical_root,
            data_root,
            migration_operation_id,
            &pending,
            &inventory,
            now_ms,
        )?;
        for candidate in scan.candidates {
            if !excluded_conflict_ids.contains(&candidate.summary.conflict_id) {
                merge_session_conflict_candidate(&mut by_id, candidate);
            }
        }
    }
    Ok(by_id.into_values().collect())
}

fn merge_session_conflict_candidate(
    by_id: &mut BTreeMap<String, SessionConflictCandidate>,
    mut candidate: SessionConflictCandidate,
) {
    let conflict_id = candidate.summary.conflict_id.clone();
    match by_id.get_mut(&conflict_id) {
        Some(existing) => {
            if existing.resolution_report.is_none() && candidate.resolution_report.is_some() {
                existing.summary = candidate.summary;
                existing.resolution_report = candidate.resolution_report.take();
            }
            existing
                .pending_recovery_entry_ids
                .extend(candidate.pending_recovery_entry_ids);
            existing.pending_recovery_entry_ids.sort();
            existing.pending_recovery_entry_ids.dedup();
        }
        None => {
            candidate.pending_recovery_entry_ids.sort();
            candidate.pending_recovery_entry_ids.dedup();
            by_id.insert(conflict_id, candidate);
        }
    }
}

fn committed_conflict_resolution_plans(
    store: &OperationLedgerStore,
    data_root: &Path,
    canonical_root: &Path,
    migration_operation_id: &str,
) -> Result<Vec<ConflictResolutionPlan>, String> {
    let mut plans = Vec::new();
    for ledger in store.all()?.into_iter().filter(|ledger| {
        ledger.kind == SessionStorageOperationKind::ConflictResolution
            && ledger.phase == SessionStorageOperationPhase::Committed
            && path_key(&ledger.canonical_root) == path_key(canonical_root)
    }) {
        let plan = load_conflict_resolution_plan(data_root, &ledger.operation_id)?;
        if plan.operation_id != ledger.operation_id
            || path_key(&plan.canonical_root) != path_key(canonical_root)
        {
            return Err("committed conflict resolution identity changed".to_string());
        }
        if plan.migration_operation_id != migration_operation_id {
            continue;
        }
        plans.push(plan);
    }
    Ok(plans)
}

fn conflict_resolution_receipt(plan: &ConflictResolutionPlan) -> ConflictResolutionReceipt {
    ConflictResolutionReceipt {
        operation_id: Some(plan.operation_id.clone()),
        migration_operation_id: plan.migration_operation_id.clone(),
        conflict_id: plan.conflict_id.clone(),
        status: ConflictResolutionStatus::Resolved,
        chosen_version: Some(plan.chosen_version),
        canonical_updated: plan.session.action
            != crate::session_storage::migration::MigrationSessionAction::KeepCanonical,
        database_view_count: plan.databases.len(),
        recovery_expires_at_ms: Some(plan.recovery_expires_at_ms),
        runtime_verification: None,
        validated: false,
    }
}

#[tauri::command]
pub async fn preflight_session_storage_migration(
    backup_destination: String,
) -> Result<MigrationPreflightReport, String> {
    let result = tauri::async_runtime::spawn_blocking(move || {
        let _mutation_guard = acquire_mutation_lock()?;
        let codex_home = managed_codex_home()?;
        let data_root = appdata_root()?.join("codex-switch");
        let ledger_store = OperationLedgerStore::new(&data_root);
        if !ledger_store.unfinished()?.is_empty() {
            return Err(
                "an unfinished session storage operation must be recovered first".to_string(),
            );
        }
        let active = list_managed_processes_for_closed_mutation(
            "preflighting the session storage migration",
        )?;
        if !active.is_empty() {
            return Err(format!(
                "session storage migration requires every Codex writer to be closed; activeProcesses={}",
                active.len()
            ));
        }
        let operation_id = operation_id("session-migration")?;
        ledger_store.create(
            &operation_id,
            SessionStorageOperationKind::Migration,
            &codex_home,
        )?;
        ledger_store.transition(&operation_id, SessionStorageOperationPhase::Preflight)?;
        let backup_destination = PathBuf::from(backup_destination);
        let preflight = run_migration_preflight(
            &codex_home,
            &data_root,
            &operation_id,
            &backup_destination,
        );
        match preflight {
            Ok(report) => {
                if let Err(error) = persist_migration_preflight(&data_root, &report) {
                    let _ = fail_session_storage_operation(
                        &ledger_store,
                        &operation_id,
                        "preflightPersistenceFailed",
                    );
                    return Err(error);
                }
                record_migration_preflight_in_ledger(
                    &ledger_store,
                    &data_root,
                    &operation_id,
                )?;
                ledger_store.update(&operation_id, |ledger| {
                    ledger.backup_root = Some(backup_destination.join(&operation_id));
                    Ok(())
                })?;
                if !report.ready_for_backup {
                    fail_session_storage_operation(
                        &ledger_store,
                        &operation_id,
                        "preflightBlocked",
                    )?;
                }
                Ok(report)
            }
            Err(error) => {
                let _ = fail_session_storage_operation(
                    &ledger_store,
                    &operation_id,
                    "preflightFailed",
                );
                Err(error)
            }
        }
    })
    .await
    .map_err(|_| "session storage migration preflight worker failed".to_string())?;
    record_background_result(
        "preflightSessionStorageMigration",
        "commands.preflight_session_storage_migration.failed",
        &result,
    );
    result
}

#[tauri::command]
pub async fn create_session_storage_migration_backup(
    operation_id: String,
) -> Result<MigrationBackupManifest, String> {
    let result = tauri::async_runtime::spawn_blocking(move || {
        let _mutation_guard = acquire_mutation_lock()?;
        let codex_home = managed_codex_home()?;
        let data_root = appdata_root()?.join("codex-switch");
        let store = OperationLedgerStore::new(&data_root);
        let unfinished = store.unfinished()?;
        if unfinished
            .iter()
            .any(|ledger| ledger.operation_id != operation_id)
        {
            return Err(
                "another unfinished session storage operation must be recovered first".to_string(),
            );
        }
        let ledger = store.load(&operation_id)?;
        if ledger.kind != SessionStorageOperationKind::Migration
            || !matches!(
                ledger.phase,
                SessionStorageOperationPhase::Preflight | SessionStorageOperationPhase::Backup
            )
            || ledger.canonical_root != codex_home
        {
            return Err("session storage migration ledger is not ready for backup".to_string());
        }
        let active = list_managed_processes_for_closed_mutation(
            "creating the session storage migration backup",
        )?;
        if !active.is_empty() {
            return Err(format!(
                "session storage migration backup requires every Codex writer to be closed; activeProcesses={}",
                active.len()
            ));
        }
        let report = load_migration_preflight(&data_root, &operation_id)?;
        if !report.ready_for_backup {
            return Err("session storage migration preflight is blocked".to_string());
        }
        let sources =
            migration_backup_sources_for_preflight(&codex_home, &data_root, &report)?;
        if ledger.phase == SessionStorageOperationPhase::Preflight {
            store.transition(
                &operation_id,
                SessionStorageOperationPhase::Backup,
            )?;
        }
        let backup_result = (|| {
            let backup_dir = report.backup_destination.join(&operation_id);
            let backup = if backup_dir.exists() {
                verify_migration_backup(&backup_dir)?
            } else {
                create_migration_backup(
                    &report.backup_destination,
                    &operation_id,
                    &sources,
                )?
            };
            verify_migration_backup_sources(&backup, &sources)?;
            record_migration_backup_in_ledger(&store, &operation_id, &backup)?;
            Ok(backup)
        })();
        if backup_result.is_err() {
            let _ = fail_session_storage_operation(
                &store,
                &operation_id,
                "migrationBackupFailed",
            );
        }
        backup_result
    })
    .await
    .map_err(|_| "session storage migration backup worker failed".to_string())?;
    record_background_result(
        "createSessionStorageMigrationBackup",
        "commands.create_session_storage_migration_backup.failed",
        &result,
    );
    result
}

#[tauri::command]
pub async fn verify_session_storage_migration_backup(
    operation_id: String,
) -> Result<MigrationBackupManifest, String> {
    let result = tauri::async_runtime::spawn_blocking(move || {
        let _mutation_guard = acquire_mutation_lock()?;
        let codex_home = managed_codex_home()?;
        let data_root = appdata_root()?.join("codex-switch");
        let store = OperationLedgerStore::new(&data_root);
        let unfinished = store.unfinished()?;
        if unfinished
            .iter()
            .any(|ledger| ledger.operation_id != operation_id)
        {
            return Err(
                "another unfinished session storage operation must be recovered first".to_string(),
            );
        }
        let ledger = store.load(&operation_id)?;
        if ledger.kind != SessionStorageOperationKind::Migration
            || !matches!(
                ledger.phase,
                SessionStorageOperationPhase::Backup
                    | SessionStorageOperationPhase::BackupVerified
            )
            || ledger.canonical_root != codex_home
        {
            return Err(
                "session storage migration ledger is not ready for backup verification"
                    .to_string(),
            );
        }
        let active = list_managed_processes_for_closed_mutation(
            "verifying the session storage migration backup",
        )?;
        if !active.is_empty() {
            return Err(format!(
                "session storage migration backup verification requires every Codex writer to be closed; activeProcesses={}",
                active.len()
            ));
        }
        let report = load_migration_preflight(&data_root, &operation_id)?;
        if !report.ready_for_backup
            || report.operation_id != operation_id
            || report.plan.canonical_root != codex_home
        {
            return Err("session storage migration preflight is not valid for verification".to_string());
        }
        let backup_dir = report.backup_destination.join(&operation_id);
        if ledger.backup_root.as_ref() != Some(&backup_dir) {
            return Err("session storage migration backup is not bound to the ledger".to_string());
        }
        let backup = verify_migration_backup(&backup_dir)?;
        if backup.operation_id != operation_id {
            return Err("session storage migration backup identity changed".to_string());
        }
        if ledger.phase == SessionStorageOperationPhase::BackupVerified {
            if backup.status != MigrationBackupStatus::RuntimeVerified {
                return Err(
                    "session storage migration backup verification state is inconsistent"
                        .to_string(),
                );
            }
            return Ok(backup);
        }
        let verifier = match NativeCodexBackupVerifier::discover() {
            Ok(verifier) => verifier,
            Err(error) => {
                let _ = record_session_storage_operation_error(
                    &store,
                    &operation_id,
                    "nativeCodexUnavailable",
                );
                return Err(error);
            }
        };
        let isolated_root = report
            .backup_destination
            .join(format!(".codex-switch-runtime-verify-{operation_id}"));
        if isolated_root.exists() {
            record_session_storage_operation_error(
                &store,
                &operation_id,
                "isolatedRestoreRecoveryRequired",
            )?;
            return Err(
                "an interrupted isolated backup verification must be recovered first".to_string(),
            );
        }
        let verification = verify_migration_backup_with_runtime(
            &backup_dir,
            &isolated_root,
            &verifier,
        );
        match verification {
            Ok(manifest) => {
                store.update(&operation_id, |ledger| {
                    ledger.last_error_code = None;
                    Ok(())
                })?;
                store.transition(
                    &operation_id,
                    SessionStorageOperationPhase::BackupVerified,
                )?;
                Ok(manifest)
            }
            Err(error) => {
                let _ = record_session_storage_operation_error(
                    &store,
                    &operation_id,
                    "migrationBackupRuntimeVerificationFailed",
                );
                Err(error)
            }
        }
    })
    .await
    .map_err(|_| "session storage migration backup verification worker failed".to_string())?;
    record_background_result(
        "verifySessionStorageMigrationBackup",
        "commands.verify_session_storage_migration_backup.failed",
        &result,
    );
    result
}

#[tauri::command]
pub async fn prepare_session_storage_migration(
    operation_id: String,
) -> Result<MigrationPreparationReceipt, String> {
    let result = tauri::async_runtime::spawn_blocking(move || {
        let _mutation_guard = acquire_mutation_lock()?;
        let codex_home = managed_codex_home()?;
        let data_root = appdata_root()?.join("codex-switch");
        let store = OperationLedgerStore::new(&data_root);
        let unfinished = store.unfinished()?;
        if unfinished
            .iter()
            .any(|ledger| ledger.operation_id != operation_id)
        {
            return Err(
                "another unfinished session storage operation must be recovered first".to_string(),
            );
        }
        let ledger = store.load(&operation_id)?;
        if ledger.kind != SessionStorageOperationKind::Migration
            || !matches!(
                ledger.phase,
                SessionStorageOperationPhase::BackupVerified
                    | SessionStorageOperationPhase::PlanReady
            )
            || ledger.canonical_root != codex_home
        {
            return Err(
                "session storage migration ledger is not ready for apply planning".to_string(),
            );
        }
        let active = list_managed_processes_for_closed_mutation(
            "preparing the session storage migration apply plan",
        )?;
        if !active.is_empty() {
            return Err(format!(
                "session storage migration planning requires every Codex writer to be closed; activeProcesses={}",
                active.len()
            ));
        }
        let report = load_migration_preflight(&data_root, &operation_id)?;
        let backup_dir = report.backup_destination.join(&operation_id);
        if ledger.backup_root.as_ref() != Some(&backup_dir) {
            return Err("session storage migration backup is not bound to the ledger".to_string());
        }
        let backup = verify_migration_backup(&backup_dir)?;
        if backup.status != MigrationBackupStatus::RuntimeVerified {
            return Err("session storage migration backup is not runtime verified".to_string());
        }
        let sources =
            migration_backup_sources_for_preflight(&codex_home, &data_root, &report)?;
        verify_migration_backup_sources(&backup, &sources)?;
        let prepared = match prepare_migration_apply_plan(
            &codex_home,
            &data_root,
            &report,
            &backup,
        ) {
            Ok(prepared) => prepared,
            Err(error) => {
                let _ = record_session_storage_operation_error(
                    &store,
                    &operation_id,
                    "migrationApplyPlanFailed",
                );
                return Err(error);
            }
        };
        store.update(&operation_id, |ledger| {
            let mut known_paths = ledger
                .created_files
                .iter()
                .map(|file| path_key(&file.path))
                .collect::<BTreeSet<_>>();
            for file in &prepared.created_files {
                if known_paths.insert(path_key(&file.path)) {
                    ledger.created_files.push(file.clone());
                }
            }
            ledger.rollback_steps = prepared.rollback_steps.clone();
            ledger.last_error_code = None;
            Ok(())
        })?;
        if ledger.phase == SessionStorageOperationPhase::BackupVerified {
            store.transition(&operation_id, SessionStorageOperationPhase::PlanReady)?;
        }
        Ok(prepared.receipt)
    })
    .await
    .map_err(|_| "session storage migration planning worker failed".to_string())?;
    record_background_result(
        "prepareSessionStorageMigration",
        "commands.prepare_session_storage_migration.failed",
        &result,
    );
    result
}

#[tauri::command]
pub async fn cancel_session_storage_migration(
    operation_id: String,
) -> Result<MigrationCancellationReceipt, String> {
    let result = tauri::async_runtime::spawn_blocking(move || {
        let _mutation_guard = acquire_mutation_lock()?;
        let codex_home = managed_codex_home()?;
        let data_root = appdata_root()?.join("codex-switch");
        let store = OperationLedgerStore::new(&data_root);
        let unfinished = store.unfinished()?;
        if unfinished
            .iter()
            .any(|ledger| ledger.operation_id != operation_id)
        {
            return Err(
                "another unfinished session storage operation must be recovered first".to_string(),
            );
        }
        let ledger = store.load(&operation_id)?;
        if ledger.kind != SessionStorageOperationKind::Migration
            || ledger.canonical_root != codex_home
            || !matches!(
                ledger.phase,
                SessionStorageOperationPhase::Preflight
                    | SessionStorageOperationPhase::Backup
                    | SessionStorageOperationPhase::BackupVerified
                    | SessionStorageOperationPhase::PlanReady
            )
        {
            return Err("session storage migration can no longer be cancelled".to_string());
        }
        store.transition(&operation_id, SessionStorageOperationPhase::RollingBack)?;
        let staging_discarded =
            match cleanup_migration_staging_for_operation(&data_root, &operation_id) {
                Ok(discarded) => discarded,
                Err(error) => {
                    record_session_storage_operation_error(
                        &store,
                        &operation_id,
                        "migrationCancellationCleanupFailed",
                    )?;
                    return Err(error);
                }
            };
        store.transition(&operation_id, SessionStorageOperationPhase::RolledBack)?;
        let backup_retained = ledger.backup_root.as_ref().is_some_and(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy() == operation_id)
                && path.is_dir()
        });
        Ok(MigrationCancellationReceipt {
            operation_id,
            backup_retained,
            staging_discarded,
        })
    })
    .await
    .map_err(|_| "session storage migration cancellation worker failed".to_string())?;
    record_background_result(
        "cancelSessionStorageMigration",
        "commands.cancel_session_storage_migration.failed",
        &result,
    );
    result
}

#[derive(Debug, PartialEq, Eq)]
enum CommitTransitionDisposition {
    ConfirmedCommitted,
    SafeToRollback(String),
    PreserveForRecovery(String),
}

fn classify_failed_commit_transition(
    transition_error: String,
    observed_phase: Result<SessionStorageOperationPhase, String>,
) -> CommitTransitionDisposition {
    match observed_phase {
        Ok(SessionStorageOperationPhase::Committed) => {
            CommitTransitionDisposition::ConfirmedCommitted
        }
        Ok(_) => CommitTransitionDisposition::SafeToRollback(transition_error),
        Err(reload_error) => CommitTransitionDisposition::PreserveForRecovery(format!(
            "{transition_error}; session storage commit state could not be verified: {reload_error}"
        )),
    }
}

fn transition_to_committed(
    store: &OperationLedgerStore,
    operation_id: &str,
) -> CommitTransitionDisposition {
    match store.transition(operation_id, SessionStorageOperationPhase::Committed) {
        Ok(_) => CommitTransitionDisposition::ConfirmedCommitted,
        Err(error) => classify_failed_commit_transition(
            error,
            store.load(operation_id).map(|ledger| ledger.phase),
        ),
    }
}

#[tauri::command]
pub async fn apply_session_storage_migration(
    operation_id: String,
) -> Result<MigrationApplyReceipt, String> {
    let result = tauri::async_runtime::spawn_blocking(move || {
        let _mutation_guard = acquire_mutation_lock()?;
        let codex_home = managed_codex_home()?;
        let data_root = appdata_root()?.join("codex-switch");
        let store = OperationLedgerStore::new(&data_root);
        let unfinished = store.unfinished()?;
        if unfinished
            .iter()
            .any(|ledger| ledger.operation_id != operation_id)
        {
            return Err(
                "another unfinished session storage operation must be recovered first".to_string(),
            );
        }
        let ledger = store.load(&operation_id)?;
        if ledger.kind != SessionStorageOperationKind::Migration
            || ledger.phase != SessionStorageOperationPhase::PlanReady
            || ledger.canonical_root != codex_home
        {
            return Err("session storage migration ledger is not ready to apply".to_string());
        }
        let active =
            list_managed_processes_for_closed_mutation("applying the session storage migration")?;
        if !active.is_empty() {
            return Err(format!(
                "session storage migration requires every Codex writer to be closed; activeProcesses={}",
                active.len()
            ));
        }
        let report = load_migration_preflight(&data_root, &operation_id)?;
        let backup_dir = report.backup_destination.join(&operation_id);
        if ledger.backup_root.as_ref() != Some(&backup_dir) {
            return Err("session storage migration backup is not bound to the ledger".to_string());
        }
        let backup = verify_migration_backup(&backup_dir)?;
        if backup.status != MigrationBackupStatus::RuntimeVerified {
            return Err("session storage migration backup is not runtime verified".to_string());
        }
        let sources =
            migration_backup_sources_for_preflight(&codex_home, &data_root, &report)?;
        verify_migration_backup_sources(&backup, &sources)?;
        let prepared = prepare_migration_apply_plan(
            &codex_home,
            &data_root,
            &report,
            &backup,
        )?;
        if prepared.rollback_steps != ledger.rollback_steps {
            return Err("session storage migration rollback plan changed".to_string());
        }
        store.transition(&operation_id, SessionStorageOperationPhase::Applying)?;
        let active = list_managed_processes_for_closed_mutation(
            "performing the final session storage migration write check",
        )?;
        if !active.is_empty() {
            abort_session_storage_migration_before_apply(
                &store,
                &prepared.plan,
                "writerAppearedBeforeApply",
            )?;
            return Err(format!(
                "a Codex writer appeared before migration apply; activeProcesses={}",
                active.len()
            ));
        }
        if let Err(failure) = apply_prepared_migration_classified(
            &prepared.plan,
            || {
                store.update(&operation_id, |ledger| {
                    ledger.live_mutation_started = true;
                    Ok(())
                })?;
                Ok(())
            },
            || {
                let active = list_managed_processes_for_closed_mutation(
                    "performing a session storage migration write",
                )?;
                if active.is_empty() {
                    Ok(())
                } else {
                    Err(format!(
                        "a Codex writer appeared during migration apply; activeProcesses={}",
                        active.len()
                    ))
                }
            },
        ) {
            let error = failure.message().to_string();
            match failure {
                crate::session_storage::migration_apply::MigrationRollbackFailure::Precondition(_) => {
                    abort_session_storage_migration_before_apply(
                        &store,
                        &prepared.plan,
                        "migrationPreconditionFailed",
                    )?;
                }
                _ => rollback_session_storage_migration_after_failure(
                    &store,
                    &data_root,
                    &prepared.plan,
                    "migrationApplyFailed",
                )?,
            }
            return Err(error);
        }
        store.transition(&operation_id, SessionStorageOperationPhase::Validating)?;
        let validation = (|| {
            ensure_no_codex_writer_for_session_storage(
                "validating the applied session storage migration",
            )?;
            validate_applied_migration(&prepared.plan)?;
            ensure_no_codex_writer_for_session_storage(
                "starting post-apply Codex runtime verification",
            )?;
            let verifier = NativeCodexBackupVerifier::discover()?;
            let runtime = verify_applied_migration_with_runtime(&prepared.plan, &verifier)?;
            ensure_no_codex_writer_for_session_storage(
                "finishing post-apply Codex runtime verification",
            )?;
            let mut receipt = validate_applied_migration(&prepared.plan)?;
            receipt.runtime_verification = Some(runtime);
            Ok(receipt)
        })();
        let receipt = match validation {
            Ok(receipt) => receipt,
            Err(error) => {
                rollback_session_storage_migration_after_failure(
                    &store,
                    &data_root,
                    &prepared.plan,
                    "migrationValidationFailed",
                )?;
                return Err(error);
            }
        };
        if let Err(error) = prepare_canonical_storage_state(
            &data_root,
            &codex_home,
            &operation_id,
            &prepared.plan.inventory_fingerprint,
        ) {
            rollback_session_storage_migration_after_failure(
                &store,
                &data_root,
                &prepared.plan,
                "migrationStatePersistenceFailed",
            )?;
            return Err(error);
        }
        if let Err(error) = cleanup_migration_staging(&prepared.plan) {
            let _ = clear_canonical_storage_state_for_operation(&data_root, &operation_id);
            rollback_session_storage_migration_after_failure(
                &store,
                &data_root,
                &prepared.plan,
                "migrationStagingCleanupFailed",
            )?;
            return Err(error);
        }
        match transition_to_committed(&store, &operation_id) {
            CommitTransitionDisposition::ConfirmedCommitted => {
                finalize_canonical_storage_state(&data_root, &codex_home, &operation_id)?;
                let witness_cleanup =
                    cleanup_committed_migration_ownership_witnesses(&prepared.plan);
                record_background_result(
                    "cleanupCommittedMigrationOwnershipWitnesses",
                    "commands.session_storage_migration.witness_cleanup_failed",
                    &witness_cleanup,
                );
            }
            CommitTransitionDisposition::SafeToRollback(error) => {
                let _ = clear_canonical_storage_state_for_operation(&data_root, &operation_id);
                rollback_session_storage_migration_after_failure(
                    &store,
                    &data_root,
                    &prepared.plan,
                    "migrationCommitFailed",
                )?;
                return Err(error);
            }
            CommitTransitionDisposition::PreserveForRecovery(error) => return Err(error),
        }
        let _ = request_shadow_and_automatic_gc(codex_home, data_root);
        Ok(receipt)
    })
    .await
    .map_err(|_| "session storage migration apply worker failed".to_string())?;
    record_background_result(
        "applySessionStorageMigration",
        "commands.apply_session_storage_migration.failed",
        &result,
    );
    result
}

#[tauri::command]
pub async fn run_session_storage_offline_gc(
    migration_operation_id: String,
) -> Result<OfflineGcReceipt, String> {
    let result = tauri::async_runtime::spawn_blocking(move || {
        run_session_storage_offline_gc_blocking(&migration_operation_id)
    })
    .await
    .map_err(|_| "offline session storage cleanup worker failed".to_string())?;
    record_background_result(
        "runSessionStorageOfflineGc",
        "commands.run_session_storage_offline_gc.failed",
        &result,
    );
    result
}

fn run_session_storage_offline_gc_blocking(
    migration_operation_id: &str,
) -> Result<OfflineGcReceipt, String> {
    run_session_storage_offline_gc_blocking_with_policy(migration_operation_id, false)
}

fn run_automatic_session_storage_offline_gc_blocking(
    migration_operation_id: &str,
) -> Result<OfflineGcReceipt, String> {
    run_session_storage_offline_gc_blocking_with_policy(migration_operation_id, true)
}

fn run_session_storage_offline_gc_blocking_with_policy(
    migration_operation_id: &str,
    require_automatic_cleanup_enabled: bool,
) -> Result<OfflineGcReceipt, String> {
    let _mutation_guard = acquire_mutation_lock()?;
    let codex_home = managed_codex_home()?;
    let data_root = appdata_root()?.join("codex-switch");
    let mut ensure_no_writer = |action: &str| ensure_no_codex_writer_for_session_storage(action);
    run_session_storage_offline_gc_at_with_policy(
        &codex_home,
        &data_root,
        migration_operation_id,
        require_automatic_cleanup_enabled,
        &mut ensure_no_writer,
        true,
    )
}

fn run_session_storage_offline_gc_at_with_policy(
    codex_home: &Path,
    data_root: &Path,
    migration_operation_id: &str,
    require_automatic_cleanup_enabled: bool,
    ensure_no_writer: &mut dyn FnMut(&str) -> Result<(), String>,
    schedule_followup_shadow: bool,
) -> Result<OfflineGcReceipt, String> {
    let store = OperationLedgerStore::new(data_root);
    if require_automatic_cleanup_enabled {
        let automatic_cleanup_enabled =
            load_session_storage_control_state(data_root, codex_home)?.automatic_cleanup_enabled;
        if !offline_gc_setting_allows_execution(true, automatic_cleanup_enabled) {
            return Err(
                "automatic cleanup is disabled; offline deletion remains scan-only until it is enabled"
                    .to_string(),
            );
        }
    }
    ensure_no_writer("running offline session storage cleanup")?;
    require_committed_session_storage_migration(data_root, codex_home, migration_operation_id)?;
    if !store.unfinished()?.is_empty() {
        return Err(
            "another unfinished session storage operation must be recovered first".to_string(),
        );
    }
    let operation_id = operation_id("offline-gc")?;
    store.create(
        &operation_id,
        SessionStorageOperationKind::OfflineGc,
        codex_home,
    )?;
    if let Err(error) = store.transition(&operation_id, SessionStorageOperationPhase::Preflight) {
        rollback_unapplied_offline_gc(
            &store,
            data_root,
            &operation_id,
            "offlineGcPreflightTransitionFailed",
        )?;
        return Err(error);
    }
    let prepared =
        match prepare_offline_gc_plan(codex_home, data_root, &operation_id, migration_operation_id)
        {
            Ok(prepared) => prepared,
            Err(error) => {
                fail_session_storage_operation(&store, &operation_id, "offlineGcPlanningFailed")?;
                return Err(error);
            }
        };
    let arm_result = (|| {
        store.update(&operation_id, |ledger| {
            ledger.backup_root = prepared.plan.backup_dir.clone();
            ledger.created_files.push(prepared.plan_snapshot.clone());
            ledger.rollback_steps = prepared.rollback_steps.clone();
            Ok(())
        })?;
        for phase in [
            SessionStorageOperationPhase::Backup,
            SessionStorageOperationPhase::BackupVerified,
            SessionStorageOperationPhase::PlanReady,
            SessionStorageOperationPhase::Applying,
        ] {
            store.transition(&operation_id, phase)?;
        }
        Ok::<(), String>(())
    })();
    if let Err(error) = arm_result {
        let observed = store.load(&operation_id)?;
        if matches!(
            observed.phase,
            SessionStorageOperationPhase::Available
                | SessionStorageOperationPhase::Preflight
                | SessionStorageOperationPhase::Backup
                | SessionStorageOperationPhase::BackupVerified
                | SessionStorageOperationPhase::PlanReady
        ) {
            rollback_unapplied_offline_gc(
                &store,
                data_root,
                &operation_id,
                "offlineGcArmingFailed",
            )?;
        } else if matches!(
            observed.phase,
            SessionStorageOperationPhase::Applying
                | SessionStorageOperationPhase::Validating
                | SessionStorageOperationPhase::RollingBack
        ) {
            let recovery =
                recover_interrupted_offline_gc(&store, data_root, &operation_id, || {
                    ensure_no_codex_writer_for_session_storage(
                        "recovering offline session storage cleanup arming",
                    )
                })?;
            if recovery != OfflineGcRecoveryStatus::RolledBack {
                return Err("offline GC arming failure remains recoverable".to_string());
            }
        }
        return Err(error);
    }

    let receipt = match execute_offline_gc(&prepared.plan, || {
        ensure_no_writer("performing an offline session storage deletion")
    }) {
        Ok(receipt) => receipt,
        Err(failure) => {
            let error_code = match &failure {
                OfflineGcFailure::LiveWriteGuard(_) => "offlineGcWriterAppeared",
                OfflineGcFailure::Operation(_) => "offlineGcApplyFailed",
            };
            rollback_offline_gc_after_failure(&store, data_root, &operation_id, error_code)?;
            return Err(failure.message().to_string());
        }
    };
    store.transition(&operation_id, SessionStorageOperationPhase::Validating)?;
    let validation = (|| {
        ensure_no_writer("validating offline session storage cleanup")?;
        validate_offline_gc(&prepared.plan, receipt)
    })();
    let receipt = match validation {
        Ok(receipt) => receipt,
        Err(error) => {
            rollback_offline_gc_after_failure(
                &store,
                data_root,
                &operation_id,
                "offlineGcValidationFailed",
            )?;
            return Err(error);
        }
    };
    match transition_to_committed(&store, &operation_id) {
        CommitTransitionDisposition::ConfirmedCommitted => {}
        CommitTransitionDisposition::SafeToRollback(error) => {
            rollback_offline_gc_after_failure(
                &store,
                data_root,
                &operation_id,
                "offlineGcCommitFailed",
            )?;
            return Err(error);
        }
        CommitTransitionDisposition::PreserveForRecovery(error) => return Err(error),
    }
    let metrics_result = record_reclaimed_bytes(data_root, &operation_id, receipt.reclaimed_bytes);
    record_background_result(
        "recordSessionStorageReclaimedBytes",
        "commands.session_storage_gc.metrics_failed",
        &metrics_result,
    );
    if schedule_followup_shadow {
        request_background_shadow_scan(codex_home.to_path_buf(), data_root.to_path_buf());
    }
    Ok(receipt)
}

fn offline_gc_setting_allows_execution(
    automatic_request: bool,
    automatic_cleanup_enabled: bool,
) -> bool {
    !automatic_request || automatic_cleanup_enabled
}

#[cfg(feature = "runtime-evidence")]
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutomaticGcSafeWindowEvidence {
    pub enabled: bool,
    pub baseline_scan_id: Option<String>,
    pub observed_scan_id: Option<String>,
    pub generation: u64,
    pub high_confidence_copy_count: usize,
    pub shadow_scan_running: bool,
    pub active_writer_count: usize,
    pub unfinished_non_gc_operation_count: usize,
    pub decision: String,
    pub writer_guard_observation_count: usize,
    pub receipt: Option<OfflineGcReceipt>,
}

#[cfg(feature = "runtime-evidence")]
pub struct AutomaticGcSafeWindowObservation<'a> {
    pub baseline_scan_id: Option<&'a str>,
    pub observed_scan_id: Option<&'a str>,
    pub generation: u64,
    pub high_confidence_copy_count: usize,
    pub shadow_scan_running: bool,
    pub active_writer_count: usize,
}

#[cfg(feature = "runtime-evidence")]
pub fn run_automatic_gc_safe_window_evidence_at(
    codex_home: &Path,
    data_root: &Path,
    observation: AutomaticGcSafeWindowObservation<'_>,
) -> Result<AutomaticGcSafeWindowEvidence, String> {
    let control = load_session_storage_control_state(data_root, codex_home)?;
    let unfinished_non_gc_operation_count = OperationLedgerStore::new(data_root)
        .unfinished()?
        .into_iter()
        .filter(|ledger| ledger.kind != SessionStorageOperationKind::OfflineGc)
        .count();
    let decision = automatic_gc_decision(
        control.automatic_cleanup_enabled,
        control.migration_operation_id.as_deref(),
        observation.observed_scan_id,
        observation.baseline_scan_id,
        observation.high_confidence_copy_count,
        observation.shadow_scan_running,
        observation.active_writer_count,
        unfinished_non_gc_operation_count,
    );
    let mut writer_guard_observation_count = 0_usize;
    let (decision_name, receipt) = match decision {
        AutomaticGcDecision::Stop => ("stop".to_string(), None),
        AutomaticGcDecision::WaitForFreshScan => ("waitForFreshScan".to_string(), None),
        AutomaticGcDecision::WaitForWriter => ("waitForWriter".to_string(), None),
        AutomaticGcDecision::Blocked(reason) => (format!("blocked:{reason}"), None),
        AutomaticGcDecision::Run(migration_operation_id) => {
            let mut writer_guard = |_action: &str| {
                writer_guard_observation_count = writer_guard_observation_count.saturating_add(1);
                Ok(())
            };
            let receipt = run_session_storage_offline_gc_at_with_policy(
                codex_home,
                data_root,
                &migration_operation_id,
                true,
                &mut writer_guard,
                false,
            )?;
            ("run".to_string(), Some(receipt))
        }
    };
    Ok(AutomaticGcSafeWindowEvidence {
        enabled: control.automatic_cleanup_enabled,
        baseline_scan_id: observation.baseline_scan_id.map(str::to_string),
        observed_scan_id: observation.observed_scan_id.map(str::to_string),
        generation: observation.generation,
        high_confidence_copy_count: observation.high_confidence_copy_count,
        shadow_scan_running: observation.shadow_scan_running,
        active_writer_count: observation.active_writer_count,
        unfinished_non_gc_operation_count,
        decision: decision_name,
        writer_guard_observation_count,
        receipt,
    })
}

#[tauri::command]
pub async fn export_session_storage_downgrade(
    migration_operation_id: String,
    target_version: String,
    destination_root: String,
) -> Result<DowngradeExportReceipt, String> {
    let result = tauri::async_runtime::spawn_blocking(move || {
        let _mutation_guard = acquire_mutation_lock()?;
        let codex_home = managed_codex_home()?;
        let data_root = appdata_root()?.join("codex-switch");
        let store = OperationLedgerStore::new(&data_root);
        if !store.unfinished()?.is_empty() {
            return Err(
                "another unfinished session storage operation must be recovered first".to_string(),
            );
        }
        ensure_no_codex_writer_for_session_storage("exporting isolated v0.2 session storage")?;
        require_committed_session_storage_migration(
            &data_root,
            &codex_home,
            &migration_operation_id,
        )?;

        let operation_id = operation_id("downgrade-export")?;
        store.create(
            &operation_id,
            SessionStorageOperationKind::DowngradeExport,
            &codex_home,
        )?;
        store.transition(&operation_id, SessionStorageOperationPhase::Preflight)?;
        let destination_root = PathBuf::from(destination_root);
        let plan = match prepare_downgrade_export(
            &codex_home,
            &data_root,
            &destination_root,
            &operation_id,
            &target_version,
        ) {
            Ok(plan) => plan,
            Err(error) => {
                fail_session_storage_operation(
                    &store,
                    &operation_id,
                    "downgradeExportPlanningFailed",
                )?;
                return Err(error);
            }
        };
        let persisted = load_downgrade_plan(&data_root, &operation_id)?;
        if persisted != plan {
            fail_session_storage_operation(
                &store,
                &operation_id,
                "downgradeExportPlanIdentityChanged",
            )?;
            return Err("downgrade export plan identity changed".to_string());
        }
        let plan_path = data_root
            .join("session-storage-v1/operations")
            .join(&operation_id)
            .join("downgrade-export-plan.json");
        let (plan_bytes, plan_sha256) =
            crate::session_storage::migration_apply::stable_file_digest(&plan_path)?;
        store.update(&operation_id, |ledger| {
            ledger.backup_root = Some(plan.package_dir.clone());
            ledger.created_files.push(LedgerFileSnapshot {
                path: plan_path.clone(),
                bytes: plan_bytes,
                sha256: plan_sha256,
                created_by_operation: true,
                logical_thread_id: None,
            });
            ledger.rollback_steps = vec![
                LedgerRollbackStep {
                    action: RollbackActionKind::RemoveCreatedFile,
                    source_path: plan.staging_dir.clone(),
                    target_path: plan.staging_dir.clone(),
                    expected_sha256: None,
                    applied_sha256: None,
                    completed: false,
                },
                LedgerRollbackStep {
                    action: RollbackActionKind::RemoveCreatedFile,
                    source_path: plan.package_dir.clone(),
                    target_path: plan.package_dir.clone(),
                    expected_sha256: None,
                    applied_sha256: None,
                    completed: false,
                },
            ];
            Ok(())
        })?;
        for phase in [
            SessionStorageOperationPhase::Backup,
            SessionStorageOperationPhase::BackupVerified,
            SessionStorageOperationPhase::PlanReady,
            SessionStorageOperationPhase::Applying,
        ] {
            store.transition(&operation_id, phase)?;
        }
        let receipt = match execute_downgrade_export(&plan, || {
            ensure_no_codex_writer_for_session_storage(
                "performing an isolated v0.2 session storage export write",
            )
        }) {
            Ok(receipt) => receipt,
            Err(error) => {
                record_session_storage_operation_error(
                    &store,
                    &operation_id,
                    "downgradeExportApplyFailed",
                )?;
                let recovery = recover_interrupted_downgrade_export(
                    &store,
                    &data_root,
                    &operation_id,
                    || {
                        ensure_no_codex_writer_for_session_storage(
                            "recovering a failed downgrade export",
                        )
                    },
                )?;
                return match recovery.status {
                    DowngradeRecoveryStatus::Committed => {
                        let manifest = verify_downgrade_package(&plan.package_dir)?;
                        Ok(receipt_from_manifest(&plan.package_dir, &manifest))
                    }
                    DowngradeRecoveryStatus::RolledBack => Err(error),
                    DowngradeRecoveryStatus::ResidualPreserved => Err(format!(
                        "{error}; downgrade export residual was preserved for recovery"
                    )),
                };
            }
        };
        store.transition(&operation_id, SessionStorageOperationPhase::Validating)?;
        let validation = (|| {
            ensure_no_codex_writer_for_session_storage("validating the isolated v0.2 export")?;
            let manifest = verify_downgrade_package(&receipt.package_dir)?;
            if manifest.operation_id != operation_id || manifest.target != receipt.target {
                return Err("downgrade export verification identity changed".to_string());
            }
            let verifier = NativeCodexBackupVerifier::discover()?;
            let isolated_root = data_root
                .join("session-storage-v1/operations")
                .join(&operation_id)
                .join("downgrade-runtime-verify");
            let manifest = verify_downgrade_package_with_runtime(
                &receipt.package_dir,
                &isolated_root,
                &verifier,
            )?;
            ensure_no_codex_writer_for_session_storage(
                "finishing isolated v0.2 export runtime verification",
            )?;
            if manifest.operation_id != operation_id || manifest.target != receipt.target {
                return Err("downgrade runtime verification identity changed".to_string());
            }
            Ok(receipt_from_manifest(&receipt.package_dir, &manifest))
        })();
        let receipt = match validation {
            Ok(receipt) => receipt,
            Err(error) => {
                record_session_storage_operation_error(
                    &store,
                    &operation_id,
                    "downgradeExportRuntimeVerificationFailed",
                )?;
                let recovery = recover_interrupted_downgrade_export(
                    &store,
                    &data_root,
                    &operation_id,
                    || {
                        ensure_no_codex_writer_for_session_storage(
                            "recovering a failed downgrade runtime verification",
                        )
                    },
                )?;
                match recovery.status {
                    DowngradeRecoveryStatus::Committed => {
                        let manifest = verify_downgrade_package(&receipt.package_dir)?;
                        receipt_from_manifest(&receipt.package_dir, &manifest)
                    }
                    DowngradeRecoveryStatus::RolledBack => return Err(error),
                    DowngradeRecoveryStatus::ResidualPreserved => {
                        return Err(format!(
                            "{error}; downgrade runtime verification residual was preserved for recovery"
                        ))
                    }
                }
            }
        };
        match transition_to_committed(&store, &operation_id) {
            CommitTransitionDisposition::ConfirmedCommitted => {}
            CommitTransitionDisposition::SafeToRollback(error)
            | CommitTransitionDisposition::PreserveForRecovery(error) => return Err(error),
        }
        Ok(receipt)
    })
    .await
    .map_err(|_| "downgrade export worker failed".to_string())?;
    record_background_result(
        "exportSessionStorageDowngrade",
        "commands.export_session_storage_downgrade.failed",
        &result,
    );
    result
}

#[tauri::command]
pub async fn import_session_storage_downgrade(
    migration_operation_id: String,
    package_dir: String,
) -> Result<RestoreImportReceipt, String> {
    let result = tauri::async_runtime::spawn_blocking(move || {
        let _mutation_guard = acquire_mutation_lock()?;
        let codex_home = managed_codex_home()?;
        let data_root = appdata_root()?.join("codex-switch");
        let store = OperationLedgerStore::new(&data_root);
        if !store.unfinished()?.is_empty() {
            return Err(
                "another unfinished session storage operation must be recovered first".to_string(),
            );
        }
        ensure_no_codex_writer_for_session_storage("importing an isolated v0.2 session store")?;
        require_committed_session_storage_migration(
            &data_root,
            &codex_home,
            &migration_operation_id,
        )?;

        let operation_id = operation_id("restore-import")?;
        store.create(
            &operation_id,
            SessionStorageOperationKind::RestoreImport,
            &codex_home,
        )?;
        store.transition(&operation_id, SessionStorageOperationPhase::Preflight)?;
        store.transition(&operation_id, SessionStorageOperationPhase::Backup)?;
        let package_dir = PathBuf::from(package_dir);
        let prepared =
            match prepare_restore_import(&codex_home, &data_root, &package_dir, &operation_id) {
                Ok(prepared) => prepared,
                Err(error) => {
                    fail_session_storage_operation(
                        &store,
                        &operation_id,
                        "restoreImportPlanningFailed",
                    )?;
                    return Err(error);
                }
            };
        complete_prepared_restore_import(&store, &codex_home, &data_root, &operation_id, prepared)
    })
    .await
    .map_err(|_| "downgrade restore import worker failed".to_string())?;
    record_background_result(
        "importSessionStorageDowngrade",
        "commands.import_session_storage_downgrade.failed",
        &result,
    );
    result
}

#[tauri::command]
pub async fn reconcile_session_storage_legacy_backups(
    migration_operation_id: String,
) -> Result<LegacyBackupReconciliationReceipt, String> {
    let result = tauri::async_runtime::spawn_blocking(move || {
        let _mutation_guard = acquire_mutation_lock()?;
        let codex_home = managed_codex_home()?;
        let data_root = appdata_root()?.join("codex-switch");
        let backup_root = default_backup_root()?;
        fs::create_dir_all(&backup_root)
            .map_err(|_| "failed to prepare the legacy backup inventory".to_string())?;
        let store = OperationLedgerStore::new(&data_root);
        if !store.unfinished()?.is_empty() {
            return Err(
                "another unfinished session storage operation must be recovered first".to_string(),
            );
        }
        ensure_no_codex_writer_for_session_storage("reconciling legacy session backups")?;
        let canonical_state = require_committed_session_storage_migration(
            &data_root,
            &codex_home,
            &migration_operation_id,
        )?;

        let operation_id = operation_id("legacy-backup-reconciliation")?;
        store.create(
            &operation_id,
            SessionStorageOperationKind::LegacyBackupReconciliation,
            &codex_home,
        )?;
        store.transition(&operation_id, SessionStorageOperationPhase::Preflight)?;
        store.transition(&operation_id, SessionStorageOperationPhase::Backup)?;
        let prepared = match prepare_legacy_backup_reconciliation(
            &codex_home,
            &data_root,
            &backup_root,
            &migration_operation_id,
            canonical_state.prepared_at_ms,
            &operation_id,
        ) {
            Ok(prepared) => prepared,
            Err(error) => {
                fail_session_storage_operation(
                    &store,
                    &operation_id,
                    "legacyBackupPlanningFailed",
                )?;
                return Err(error);
            }
        };
        let persisted = load_legacy_backup_plan(&data_root, &operation_id)?;
        if persisted != prepared.plan {
            fail_session_storage_operation(
                &store,
                &operation_id,
                "legacyBackupPlanIdentityChanged",
            )?;
            return Err("legacy backup reconciliation plan identity changed".to_string());
        }
        store.update(&operation_id, |ledger| {
            ledger.backup_root = Some(backup_root.clone());
            ledger.created_files = prepared.created_files.clone();
            ledger.last_error_code = None;
            Ok(())
        })?;
        store.transition(&operation_id, SessionStorageOperationPhase::BackupVerified)?;
        store.transition(&operation_id, SessionStorageOperationPhase::PlanReady)?;
        store.transition(&operation_id, SessionStorageOperationPhase::Applying)?;
        let receipt = match execute_legacy_backup_reconciliation(&prepared.plan, || {
            ensure_no_codex_writer_for_session_storage(
                "performing a legacy backup reconciliation mutation",
            )
        }) {
            Ok(receipt) => receipt,
            Err(error) => {
                let recovered = recover_legacy_backup_reconciliation_after_failure(
                    &store,
                    &data_root,
                    &operation_id,
                    "legacyBackupApplyFailed",
                );
                return recovered.map_err(|recovery_error| format!("{error}; {recovery_error}"));
            }
        };
        if let Err(error) =
            store.transition(&operation_id, SessionStorageOperationPhase::Validating)
        {
            if store.load(&operation_id)?.phase != SessionStorageOperationPhase::Validating {
                return recover_legacy_backup_reconciliation_after_failure(
                    &store,
                    &data_root,
                    &operation_id,
                    "legacyBackupValidatingTransitionFailed",
                )
                .map_err(|recovery_error| format!("{error}; {recovery_error}"));
            }
        }
        let receipt = match crate::session_storage::legacy_backup::validate_applied_reconciliation(
            &prepared.plan,
            receipt,
        )
        .and_then(|receipt| {
            cleanup_reconciliation_staging(&prepared.plan)?;
            Ok(receipt)
        }) {
            Ok(receipt) => receipt,
            Err(error) => {
                return recover_legacy_backup_reconciliation_after_failure(
                    &store,
                    &data_root,
                    &operation_id,
                    "legacyBackupValidationFailed",
                )
                .map_err(|recovery_error| format!("{error}; {recovery_error}"));
            }
        };
        match transition_to_committed(&store, &operation_id) {
            CommitTransitionDisposition::ConfirmedCommitted => {}
            CommitTransitionDisposition::SafeToRollback(error) => {
                return recover_legacy_backup_reconciliation_after_failure(
                    &store,
                    &data_root,
                    &operation_id,
                    "legacyBackupCommitFailed",
                )
                .map_err(|recovery_error| format!("{error}; {recovery_error}"));
            }
            CommitTransitionDisposition::PreserveForRecovery(error) => return Err(error),
        }
        record_reclaimed_bytes(&data_root, &operation_id, receipt.reclaimed_bytes)?;
        request_background_shadow_scan(codex_home, data_root);
        Ok(receipt)
    })
    .await
    .map_err(|_| "legacy backup reconciliation worker failed".to_string())?;
    record_background_result(
        "reconcileSessionStorageLegacyBackups",
        "commands.reconcile_session_storage_legacy_backups.failed",
        &result,
    );
    result
}

#[tauri::command]
pub async fn list_session_storage_pending_recovery(
    migration_operation_id: String,
) -> Result<PendingRecoveryList, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let _mutation_guard = acquire_mutation_lock()?;
        let codex_home = managed_codex_home()?;
        let data_root = appdata_root()?.join("codex-switch");
        let store = OperationLedgerStore::new(&data_root);
        let migration = store.load(&migration_operation_id)?;
        if migration.kind != SessionStorageOperationKind::Migration
            || migration.phase != SessionStorageOperationPhase::Committed
            || migration.canonical_root != codex_home
        {
            return Err("pending recovery requires a committed migration".to_string());
        }
        let mut pending = list_pending_recovery(&data_root, &migration_operation_id)?;
        let now_ms = timestamp_millis()?;
        if pending.entries.iter().any(|entry| {
            entry.status == PendingRecoveryStatus::Pending
                && entry.expires_at_ms > now_ms
                && matches!(
                    entry.relation,
                    crate::session_storage::legacy_backup::PendingRecoveryRelation::Divergent
                        | crate::session_storage::legacy_backup::PendingRecoveryRelation::Unknown
                )
        }) {
            let inventory = collect_inventory(&codex_home, &data_root)?;
            let scan = pending_recovery_conflict_candidates(
                &codex_home,
                &data_root,
                &migration_operation_id,
                &pending,
                &inventory,
                now_ms,
            )?;
            let mut resolved_conflicts =
                load_resolved_conflict_ids(&data_root, &codex_home, &migration_operation_id)?;
            resolved_conflicts.extend(
                committed_conflict_resolution_plans(
                    &store,
                    &data_root,
                    &codex_home,
                    &migration_operation_id,
                )?
                .into_iter()
                .map(|plan| plan.conflict_id),
            );
            let mut resolved_entries = scan
                .resolved_by_content_entry_ids
                .into_iter()
                .collect::<BTreeSet<_>>();
            for candidate in scan.candidates {
                if resolved_conflicts.contains(&candidate.summary.conflict_id) {
                    resolved_entries.extend(candidate.pending_recovery_entry_ids);
                }
            }
            for entry in &mut pending.entries {
                if resolved_entries.contains(&entry.entry_id) {
                    entry.status = PendingRecoveryStatus::Restored;
                    entry.restore_allowed = false;
                }
            }
        }
        Ok(pending)
    })
    .await
    .map_err(|_| "pending recovery listing worker failed".to_string())?
}

#[tauri::command]
pub async fn defer_session_storage_pending_recovery(
    migration_operation_id: String,
    entry_id: String,
) -> Result<PendingRecoveryList, String> {
    let _mutation_guard = acquire_mutation_lock()?;
    let codex_home = managed_codex_home()?;
    let data_root = appdata_root()?.join("codex-switch");
    let store = OperationLedgerStore::new(&data_root);
    let migration = store.load(&migration_operation_id)?;
    if migration.kind != SessionStorageOperationKind::Migration
        || migration.phase != SessionStorageOperationPhase::Committed
        || migration.canonical_root != codex_home
    {
        return Err("pending recovery deferral requires a committed migration".to_string());
    }
    update_pending_recovery_status(
        &data_root,
        &migration_operation_id,
        &entry_id,
        PendingRecoveryStatus::Deferred,
    )?;
    list_pending_recovery(&data_root, &migration_operation_id)
}

#[tauri::command]
pub async fn restore_session_storage_pending_recovery(
    migration_operation_id: String,
    entry_id: String,
) -> Result<RestoreImportReceipt, String> {
    let result = tauri::async_runtime::spawn_blocking(move || {
        let _mutation_guard = acquire_mutation_lock()?;
        let codex_home = managed_codex_home()?;
        let data_root = appdata_root()?.join("codex-switch");
        let store = OperationLedgerStore::new(&data_root);
        if !store.unfinished()?.is_empty() {
            return Err(
                "another unfinished session storage operation must be recovered first".to_string(),
            );
        }
        ensure_no_codex_writer_for_session_storage("restoring a pending session")?;
        require_committed_session_storage_migration(
            &data_root,
            &codex_home,
            &migration_operation_id,
        )?;
        let source = load_pending_recovery_source(&data_root, &migration_operation_id, &entry_id)?;
        let operation_id = operation_id("pending-recovery-restore")?;
        store.create(
            &operation_id,
            SessionStorageOperationKind::RestoreImport,
            &codex_home,
        )?;
        store.transition(&operation_id, SessionStorageOperationPhase::Preflight)?;
        store.transition(&operation_id, SessionStorageOperationPhase::Backup)?;
        let prepared = match prepare_pending_recovery_import(
            &codex_home,
            &data_root,
            &source,
            &operation_id,
        ) {
            Ok(prepared) => prepared,
            Err(error) => {
                fail_session_storage_operation(
                    &store,
                    &operation_id,
                    "pendingRecoveryPlanningFailed",
                )?;
                return Err(error);
            }
        };
        let receipt = complete_prepared_restore_import(
            &store,
            &codex_home,
            &data_root,
            &operation_id,
            prepared,
        )?;
        if receipt.conflict_count == 0
            && receipt.imported_new_session_count
                + receipt.imported_extension_count
                + receipt.unchanged_session_count
                + receipt.current_ahead_session_count
                == 1
        {
            update_pending_recovery_status(
                &data_root,
                &migration_operation_id,
                &entry_id,
                PendingRecoveryStatus::Restored,
            )?;
        }
        Ok(receipt)
    })
    .await
    .map_err(|_| "pending recovery restore worker failed".to_string())?;
    record_background_result(
        "restoreSessionStoragePendingRecovery",
        "commands.restore_session_storage_pending_recovery.failed",
        &result,
    );
    result
}

fn complete_prepared_restore_import(
    store: &OperationLedgerStore,
    codex_home: &Path,
    data_root: &Path,
    operation_id: &str,
    prepared: PreparedRestoreImport,
) -> Result<RestoreImportReceipt, String> {
    let persisted = load_restore_import_plan(data_root, operation_id)?;
    if persisted != prepared.plan {
        fail_session_storage_operation(store, operation_id, "restoreImportPlanIdentityChanged")?;
        return Err("restore import plan identity changed".to_string());
    }
    store.update(operation_id, |ledger| {
        ledger.backup_root = Some(prepared.plan.recovery_root.clone());
        ledger.created_files = prepared.created_files.clone();
        ledger.database_snapshots = prepared.database_snapshots.clone();
        ledger.rollback_steps = prepared.rollback_steps.clone();
        ledger.last_error_code = None;
        Ok(())
    })?;
    store.transition(operation_id, SessionStorageOperationPhase::BackupVerified)?;
    store.transition(operation_id, SessionStorageOperationPhase::PlanReady)?;
    store.transition(operation_id, SessionStorageOperationPhase::Applying)?;
    if let Err(error) = ensure_no_codex_writer_for_session_storage(
        "performing the final restore import write check",
    ) {
        abort_restore_import_before_apply(
            store,
            data_root,
            operation_id,
            "restoreImportWriterAppearedBeforeApply",
        )?;
        return Err(error);
    }
    let receipt = match execute_restore_import_classified(
        &prepared.plan,
        || {
            store.update(operation_id, |ledger| {
                ledger.live_mutation_started = true;
                Ok(())
            })?;
            Ok(())
        },
        || ensure_no_codex_writer_for_session_storage("performing a restore import write"),
    ) {
        Ok(receipt) => receipt,
        Err(failure) => {
            let message = failure.message().to_string();
            match failure {
                RestoreImportApplyFailure::Precondition(_) => {
                    abort_restore_import_before_apply(
                        store,
                        data_root,
                        operation_id,
                        "restoreImportPreconditionFailed",
                    )?;
                }
                RestoreImportApplyFailure::Operation(_) => {
                    rollback_restore_import_after_failure(
                        store,
                        data_root,
                        operation_id,
                        "restoreImportApplyFailed",
                    )?;
                }
            }
            return Err(message);
        }
    };
    store.transition(operation_id, SessionStorageOperationPhase::Validating)?;
    let validation = (|| {
        ensure_no_codex_writer_for_session_storage("validating the restore import")?;
        let mut receipt = validate_applied_restore_import(&prepared.plan, receipt)?;
        if let Some(runtime_plan) = restore_import_runtime_apply_plan(&prepared.plan)? {
            let verifier = NativeCodexBackupVerifier::discover()?;
            receipt.runtime_verification = Some(verify_applied_migration_with_runtime(
                &runtime_plan,
                &verifier,
            )?);
        }
        ensure_no_codex_writer_for_session_storage(
            "finishing restore import runtime verification",
        )?;
        validate_applied_restore_import(&prepared.plan, receipt)
    })();
    let receipt = match validation {
        Ok(receipt) => receipt,
        Err(error) => {
            rollback_restore_import_after_failure(
                store,
                data_root,
                operation_id,
                "restoreImportValidationFailed",
            )?;
            return Err(error);
        }
    };
    if let Err(error) = cleanup_restore_import_staging(&prepared.plan) {
        rollback_restore_import_after_failure(
            store,
            data_root,
            operation_id,
            "restoreImportStagingCleanupFailed",
        )?;
        return Err(error);
    }
    match transition_to_committed(store, operation_id) {
        CommitTransitionDisposition::ConfirmedCommitted => {
            let witness_cleanup =
                cleanup_committed_restore_import_ownership_witnesses(&prepared.plan);
            record_background_result(
                "cleanupCommittedRestoreImportOwnershipWitnesses",
                "commands.restore_import.witness_cleanup_failed",
                &witness_cleanup,
            );
        }
        CommitTransitionDisposition::SafeToRollback(error) => {
            rollback_restore_import_after_failure(
                store,
                data_root,
                operation_id,
                "restoreImportCommitFailed",
            )?;
            return Err(error);
        }
        CommitTransitionDisposition::PreserveForRecovery(error) => return Err(error),
    }
    let _ = request_shadow_and_automatic_gc(codex_home.to_path_buf(), data_root.to_path_buf());
    Ok(receipt)
}

fn abort_session_storage_migration_before_apply(
    store: &OperationLedgerStore,
    plan: &MigrationApplyPlan,
    error_code: &str,
) -> Result<(), String> {
    record_session_storage_operation_error(store, &plan.operation_id, error_code)?;
    store.transition(
        &plan.operation_id,
        SessionStorageOperationPhase::RollingBack,
    )?;
    if cleanup_migration_staging(plan).is_err() {
        record_session_storage_operation_error(
            store,
            &plan.operation_id,
            "migrationAbortCleanupFailed",
        )?;
        return Err("session storage migration abort left recoverable residuals".to_string());
    }
    store.transition(&plan.operation_id, SessionStorageOperationPhase::RolledBack)?;
    Ok(())
}

fn rollback_session_storage_migration_after_failure(
    store: &OperationLedgerStore,
    data_root: &Path,
    plan: &MigrationApplyPlan,
    error_code: &str,
) -> Result<(), String> {
    record_session_storage_operation_error(store, &plan.operation_id, error_code)?;
    let recovery = recover_interrupted_migration(store, data_root, &plan.operation_id, || {
        let active = list_managed_processes_for_closed_mutation(
            "rolling back the session storage migration",
        )?;
        if active.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "a Codex writer appeared during migration rollback; activeProcesses={}",
                active.len()
            ))
        }
    })?;
    match recovery.status {
        MigrationRecoveryStatus::RolledBack => Ok(()),
        MigrationRecoveryStatus::DeferredByLiveWriter => Err(
            "session storage migration rollback is waiting for every Codex writer to close"
                .to_string(),
        ),
        MigrationRecoveryStatus::Failed => {
            Err("session storage migration rollback left recoverable residuals".to_string())
        }
    }
}

fn rollback_offline_gc_after_failure(
    store: &OperationLedgerStore,
    data_root: &Path,
    operation_id: &str,
    error_code: &str,
) -> Result<(), String> {
    record_session_storage_operation_error(store, operation_id, error_code)?;
    let recovery = recover_interrupted_offline_gc(store, data_root, operation_id, || {
        ensure_no_codex_writer_for_session_storage("rolling back offline session storage cleanup")
    })?;
    match recovery {
        OfflineGcRecoveryStatus::RolledBack => Ok(()),
        OfflineGcRecoveryStatus::DeferredByLiveWriter => Err(
            "offline session storage cleanup rollback is waiting for every Codex writer to close"
                .to_string(),
        ),
        OfflineGcRecoveryStatus::Failed => {
            Err("offline session storage cleanup rollback left recoverable residuals".to_string())
        }
    }
}

fn rollback_conflict_resolution_after_failure(
    store: &OperationLedgerStore,
    data_root: &Path,
    operation_id: &str,
    error_code: &str,
) -> Result<(), String> {
    record_session_storage_operation_error(store, operation_id, error_code)?;
    let recovery = recover_interrupted_conflict_resolution(store, data_root, operation_id, || {
        ensure_no_codex_writer_for_session_storage(
            "rolling back a session storage conflict resolution",
        )
    })?;
    match recovery {
        ConflictResolutionRecoveryStatus::RolledBack => Ok(()),
        ConflictResolutionRecoveryStatus::DeferredByLiveWriter => Err(
            "session storage conflict rollback is waiting for every Codex writer to close"
                .to_string(),
        ),
        ConflictResolutionRecoveryStatus::Failed => {
            Err("session storage conflict rollback left recoverable residuals".to_string())
        }
    }
}

fn rollback_restore_import_after_failure(
    store: &OperationLedgerStore,
    data_root: &Path,
    operation_id: &str,
    error_code: &str,
) -> Result<(), String> {
    record_session_storage_operation_error(store, operation_id, error_code)?;
    let recovery = recover_interrupted_restore_import(store, data_root, operation_id, || {
        ensure_no_codex_writer_for_session_storage("rolling back a downgrade restore import")
    })?;
    match recovery {
        RestoreImportRecoveryStatus::RolledBack => Ok(()),
        RestoreImportRecoveryStatus::DeferredByLiveWriter => Err(
            "downgrade restore import rollback is waiting for every Codex writer to close"
                .to_string(),
        ),
        RestoreImportRecoveryStatus::Failed => {
            Err("downgrade restore import rollback left recoverable residuals".to_string())
        }
    }
}

fn abort_restore_import_before_apply(
    store: &OperationLedgerStore,
    data_root: &Path,
    operation_id: &str,
    error_code: &str,
) -> Result<(), String> {
    let plan = load_restore_import_plan(data_root, operation_id)?;
    abort_unapplied_restore_import(store, &plan, error_code)
}

fn recover_legacy_backup_reconciliation_after_failure(
    store: &OperationLedgerStore,
    data_root: &Path,
    operation_id: &str,
    error_code: &str,
) -> Result<LegacyBackupReconciliationReceipt, String> {
    if store.load(operation_id)?.phase == SessionStorageOperationPhase::Committed {
        let plan = load_legacy_backup_plan(data_root, operation_id)?;
        let receipt = reconciliation_receipt_from_applied_state(&plan)?;
        record_reclaimed_bytes(data_root, operation_id, receipt.reclaimed_bytes)?;
        return Ok(receipt);
    }
    record_session_storage_operation_error(store, operation_id, error_code)?;
    let recovery =
        recover_interrupted_legacy_backup_reconciliation(store, data_root, operation_id, || {
            ensure_no_codex_writer_for_session_storage("recovering a legacy backup reconciliation")
        })?;
    match recovery {
        LegacyBackupRecoveryStatus::Committed => {
            let plan = load_legacy_backup_plan(data_root, operation_id)?;
            let receipt = reconciliation_receipt_from_applied_state(&plan)?;
            record_reclaimed_bytes(data_root, operation_id, receipt.reclaimed_bytes)?;
            Ok(receipt)
        }
        LegacyBackupRecoveryStatus::RolledBack => {
            Err("legacy backup reconciliation was rolled back before mutation".to_string())
        }
        LegacyBackupRecoveryStatus::DeferredByLiveWriter => Err(
            "legacy backup reconciliation recovery is waiting for every Codex writer to close"
                .to_string(),
        ),
        LegacyBackupRecoveryStatus::Failed => {
            Err("legacy backup reconciliation left recoverable residuals".to_string())
        }
    }
}

fn require_committed_session_storage_migration(
    data_root: &Path,
    canonical_root: &Path,
    migration_operation_id: &str,
) -> Result<CanonicalStorageState, String> {
    let state = load_committed_canonical_storage_state(data_root, canonical_root)?
        .ok_or_else(|| "a committed canonical session storage migration is required".to_string())?;
    if state.migration_operation_id != migration_operation_id {
        return Err("canonical session storage migration identity changed".to_string());
    }
    Ok(state)
}

fn record_migration_backup_in_ledger(
    store: &OperationLedgerStore,
    operation_id: &str,
    backup: &MigrationBackupManifest,
) -> Result<(), String> {
    store
        .update(operation_id, |ledger| {
            ledger.backup_root = Some(backup.backup_dir.clone());
            let mut known_paths = ledger
                .created_files
                .iter()
                .map(|file| path_key(&file.path))
                .collect::<BTreeSet<_>>();
            for entry in &backup.entries {
                let snapshot = LedgerFileSnapshot {
                    path: backup
                        .backup_dir
                        .join("payload")
                        .join(&entry.payload_relative_path),
                    bytes: entry.bytes,
                    sha256: entry.sha256.clone(),
                    created_by_operation: true,
                    logical_thread_id: entry.logical_thread_id.clone(),
                };
                if known_paths.insert(path_key(&snapshot.path)) {
                    ledger.created_files.push(snapshot);
                }
            }
            ledger.database_snapshots = backup
                .entries
                .iter()
                .filter(|entry| entry.kind == MigrationBackupEntryKind::Database)
                .map(|entry| LedgerDatabaseSnapshot {
                    source_path: entry.source_path.clone(),
                    snapshot_path: backup
                        .backup_dir
                        .join("payload")
                        .join(&entry.payload_relative_path),
                    bytes: entry.bytes,
                    sha256: entry.sha256.clone(),
                })
                .collect();
            ledger.last_error_code = None;
            Ok(())
        })
        .map(|_| ())
}

fn record_migration_preflight_in_ledger(
    store: &OperationLedgerStore,
    data_root: &Path,
    operation_id: &str,
) -> Result<(), String> {
    let path = data_root
        .join("session-storage-v1/operations")
        .join(operation_id)
        .join("preflight.json");
    let (bytes, sha256) = crate::session_storage::migration_apply::stable_file_digest(&path)?;
    store
        .update(operation_id, |ledger| {
            if ledger
                .created_files
                .iter()
                .all(|snapshot| path_key(&snapshot.path) != path_key(&path))
            {
                ledger.created_files.push(LedgerFileSnapshot {
                    path: path.clone(),
                    bytes,
                    sha256: sha256.clone(),
                    created_by_operation: true,
                    logical_thread_id: None,
                });
            }
            Ok(())
        })
        .map(|_| ())
}

fn fail_session_storage_operation(
    store: &OperationLedgerStore,
    operation_id: &str,
    error_code: &str,
) -> Result<(), String> {
    store.update(operation_id, |ledger| {
        ledger.last_error_code = Some(error_code.to_string());
        Ok(())
    })?;
    store
        .transition(operation_id, SessionStorageOperationPhase::Failed)
        .map(|_| ())
}

fn record_session_storage_operation_error(
    store: &OperationLedgerStore,
    operation_id: &str,
    error_code: &str,
) -> Result<(), String> {
    store
        .update(operation_id, |ledger| {
            ledger.last_error_code = Some(error_code.to_string());
            Ok(())
        })
        .map(|_| ())
}

#[tauri::command]
pub fn list_runtimes() -> Result<Vec<RuntimeMetadata>, String> {
    let result = (|| RuntimeStore::from_default_root()?.list_runtimes())();
    record_background_result("listRuntimes", "commands.list_runtimes.failed", &result);
    result
}

#[tauri::command]
pub async fn get_mobile_continuity_status() -> Result<MobileContinuityStatus, String> {
    let result = match tauri::async_runtime::spawn_blocking(|| {
        let _mutation_guard = acquire_mutation_lock()?;
        let current_home = managed_codex_home()?;
        let current_paths = resolve_user_codex_paths(&current_home)?;
        mobile_continuity::initialize_status(
            &default_mobile_continuity_state_path()?,
            &current_paths,
        )
    })
    .await
    {
        Ok(result) => result,
        Err(_) => Err("mobile continuity status worker failed".to_string()),
    };
    record_background_result(
        "getMobileContinuityStatus",
        "commands.get_mobile_continuity_status.failed",
        &result,
    );
    result
}

#[tauri::command]
pub async fn set_mobile_continuity_enabled(
    enabled: bool,
) -> Result<MobileContinuityStatus, String> {
    let diagnostic = begin_command_diagnostic("setMobileContinuityEnabled");
    record_diagnostic_phase(diagnostic.as_ref(), "preflight");
    let worker_diagnostic = diagnostic.clone();
    let result = match tauri::async_runtime::spawn_blocking(move || {
        let _mutation_guard = acquire_mutation_lock()?;
        record_diagnostic_phase(worker_diagnostic.as_ref(), "apply");
        let current_home = managed_codex_home()?;
        let current_paths = resolve_user_codex_paths(&current_home)?;
        mobile_continuity::set_enabled(
            &default_mobile_continuity_state_path()?,
            &current_paths,
            enabled,
        )
    })
    .await
    {
        Ok(result) => result,
        Err(_) => Err("mobile continuity settings worker failed".to_string()),
    };
    record_diagnostic_result(
        diagnostic.as_ref(),
        &result,
        "complete",
        "apply",
        "commands.set_mobile_continuity_enabled.failed",
    );
    correlate_mutation_result(result, diagnostic.as_ref())
}

#[tauri::command]
pub async fn acknowledge_mobile_continuity_notice() -> Result<MobileContinuityStatus, String> {
    let diagnostic = begin_command_diagnostic("acknowledgeMobileContinuityNotice");
    record_diagnostic_phase(diagnostic.as_ref(), "preflight");
    let worker_diagnostic = diagnostic.clone();
    let result = match tauri::async_runtime::spawn_blocking(move || {
        let _mutation_guard = acquire_mutation_lock()?;
        record_diagnostic_phase(worker_diagnostic.as_ref(), "apply");
        let current_home = managed_codex_home()?;
        let current_paths = resolve_user_codex_paths(&current_home)?;
        mobile_continuity::acknowledge_notice(
            &default_mobile_continuity_state_path()?,
            &current_paths,
        )
    })
    .await
    {
        Ok(result) => result,
        Err(_) => Err("mobile continuity notice worker failed".to_string()),
    };
    record_diagnostic_result(
        diagnostic.as_ref(),
        &result,
        "complete",
        "apply",
        "commands.acknowledge_mobile_continuity_notice.failed",
    );
    correlate_mutation_result(result, diagnostic.as_ref())
}

#[tauri::command]
pub async fn publish_mobile_continuity_session(
    thread_id: String,
) -> Result<MobileContinuityStatus, String> {
    let diagnostic = begin_command_diagnostic("publishMobileContinuitySession");
    record_diagnostic_phase(diagnostic.as_ref(), "preflight");
    let worker_diagnostic = diagnostic.clone();
    let result = match tauri::async_runtime::spawn_blocking(move || {
        let mut durable = None;
        let mut publication_terminal = None;
        let mut launch_terminal = None;
        let result = (|| {
            let _mutation_guard = acquire_mutation_lock()?;
            let started_at_ms = timestamp_millis()?;
            let operation_id = operation_id("mobile-continuity-manual")?;
            durable = Some((started_at_ms, operation_id));
            record_diagnostic_phase(worker_diagnostic.as_ref(), "apply");
            let current_home = managed_codex_home()?;
            let runtime_status =
                RuntimeStore::from_default_root()?.detect_active_runtime(&current_home)?;
            if runtime_status.active_runtime_id.as_deref() != Some(PLUS_RUNTIME_ID) {
                return Err("请先切回 OpenAI 官方请求端，再发布单个会话".to_string());
            }
            let current_paths = resolve_user_codex_paths(&current_home)?;
            let auth_snapshot = fs::read(current_home.join("auth.json"))
                .map_err(|error| format!("failed to read official auth state: {error}"))?;
            let processes =
                list_managed_processes_for_closed_mutation("publishing a session to Remote")?;
            let _ = cache_chatgpt_launch_target();
            if !processes.is_empty() {
                close_codex().map(|_| ())?;
            }
            ensure_codex_closed("publishing a session to Remote")?;
            let publication = mobile_continuity::publish_single_account_session(
                &default_mobile_continuity_state_path()?,
                &current_paths,
                &thread_id,
            );
            if let Ok(status) = publication.as_ref() {
                publication_terminal = Some(mobile_publication_terminal(status, &thread_id));
            }
            let auth_after = fs::read(current_home.join("auth.json"))
                .map_err(|error| format!("failed to verify official auth state: {error}"));
            record_diagnostic_phase(worker_diagnostic.as_ref(), "launchingApp");
            let launch = ChatGptLaunchReceipt::from(launch_cached_chatgpt());
            launch_terminal = match launch.status {
                ChatGptLaunchStatus::Launched | ChatGptLaunchStatus::AlreadyRunning => None,
                ChatGptLaunchStatus::Failed => Some(DiagnosticTerminalStatus::Partial),
                ChatGptLaunchStatus::Blocked | ChatGptLaunchStatus::NotRequested => {
                    Some(DiagnosticTerminalStatus::Blocked)
                }
            };
            if auth_after? != auth_snapshot {
                return Err(
                    "official auth state changed unexpectedly; session publication stopped"
                        .to_string(),
                );
            }
            let publication = publication?;
            if launch.status == ChatGptLaunchStatus::Failed {
                return Err(format!(
                    "会话发布已完成，但 ChatGPT 重新打开失败：{}",
                    launch
                        .message
                        .as_deref()
                        .unwrap_or("未能确认 Windows 应用已启动")
                ));
            }
            Ok(publication)
        })();
        let counts = result
            .as_ref()
            .map(|status| {
                BTreeMap::from([
                    ("remotePublished".to_string(), status.remote_published),
                    ("partial".to_string(), status.partial),
                    ("conflict".to_string(), status.conflict),
                    ("needsManual".to_string(), status.needs_manual),
                ])
            })
            .unwrap_or_default();
        let durable_recorded = durable
            .as_ref()
            .is_some_and(|(started_at_ms, operation_id)| {
                record_result_with_diagnostic(
                    operation_id,
                    OperationAction::IncrementalSync,
                    *started_at_ms,
                    &result,
                    &[],
                    counts,
                    worker_diagnostic.as_ref(),
                )
                .is_ok()
            });
        match (&result, publication_terminal, launch_terminal) {
            (Ok(_), Some(DiagnosticTerminalStatus::Blocked), _)
            | (Ok(_), _, Some(DiagnosticTerminalStatus::Blocked)) => {
                record_diagnostic_terminal(
                    worker_diagnostic.as_ref(),
                    DiagnosticTerminalStatus::Blocked,
                    "launchingApp",
                    Some("commands.publish_mobile_continuity_session.blocked"),
                    Some("session publication or application relaunch requires user action"),
                );
            }
            (Ok(_), Some(DiagnosticTerminalStatus::Succeeded), None) if durable_recorded => {
                record_diagnostic_terminal(
                    worker_diagnostic.as_ref(),
                    DiagnosticTerminalStatus::Succeeded,
                    "complete",
                    None,
                    None,
                );
            }
            (Ok(_), _, _) => record_diagnostic_terminal(
                worker_diagnostic.as_ref(),
                DiagnosticTerminalStatus::Partial,
                "complete",
                Some("commands.publish_mobile_continuity_session.partial"),
                Some("session publication completed with a partial or unaudited outcome"),
            ),
            (Err(error), Some(_), _) => record_diagnostic_terminal(
                worker_diagnostic.as_ref(),
                DiagnosticTerminalStatus::Partial,
                "launchingApp",
                Some("commands.publish_mobile_continuity_session.post_mutation_failed"),
                Some(error),
            ),
            (Err(_), None, _) => record_durable_command_result(
                worker_diagnostic.as_ref(),
                &result,
                DiagnosticTerminalStatus::Succeeded,
                "complete",
                None,
                None,
                "apply",
                "commands.publish_mobile_continuity_session.failed",
            ),
        }
        result
    })
    .await
    {
        Ok(result) => result,
        Err(_) => {
            let error = "mobile continuity publication worker failed".to_string();
            record_diagnostic_terminal(
                diagnostic.as_ref(),
                DiagnosticTerminalStatus::Failed,
                "workerJoin",
                Some("commands.publish_mobile_continuity_session.worker_join_failed"),
                Some(&error),
            );
            Err(error)
        }
    };
    correlate_mutation_result(result, diagnostic.as_ref())
}

#[tauri::command]
pub fn scan_runtime_status() -> Result<RuntimeStatus, String> {
    let result =
        (|| RuntimeStore::from_default_root()?.detect_active_runtime(&managed_codex_home()?))();
    record_background_result(
        "scanRuntimeStatus",
        "commands.scan_runtime_status.failed",
        &result,
    );
    result
}

#[tauri::command]
pub fn import_plus_runtime(confirm_overwrite: bool) -> Result<RuntimeMetadata, String> {
    let diagnostic = begin_command_diagnostic("importAccount");
    record_diagnostic_phase(diagnostic.as_ref(), "preflight");
    let mut durable_recorded = false;
    let result = (|| {
        let _mutation_guard = acquire_mutation_lock()?;
        let started = timestamp_millis()?;
        let id = operation_id("import-account")?;
        record_diagnostic_phase(diagnostic.as_ref(), "apply");
        let result = (|| {
            ensure_codex_closed("saving the account slot")?;
            RuntimeStore::from_default_root()?
                .import_plus_from_home(&managed_codex_home()?, confirm_overwrite)
        })();
        durable_recorded = record_result_with_diagnostic(
            &id,
            OperationAction::ImportAccount,
            started,
            &result,
            &[],
            BTreeMap::new(),
            diagnostic.as_ref(),
        )
        .is_ok();
        result
    })();
    record_durable_command_result(
        diagnostic.as_ref(),
        &result,
        if durable_recorded {
            DiagnosticTerminalStatus::Succeeded
        } else {
            DiagnosticTerminalStatus::Partial
        },
        "complete",
        (!durable_recorded).then_some("commands.import_account.operation_history_failed"),
        (!durable_recorded).then_some("account import completed without durable operation history"),
        "apply",
        "commands.import_account.failed",
    );
    correlate_mutation_result(result, diagnostic.as_ref())
}

#[tauri::command]
pub fn upsert_relay_runtime(input: RelayRuntimeInput) -> Result<RuntimeMetadata, String> {
    let diagnostic = begin_command_diagnostic("saveRelay");
    record_diagnostic_phase(diagnostic.as_ref(), "preflight");
    let mut durable_recorded = false;
    let result = (|| {
        let _mutation_guard = acquire_mutation_lock()?;
        let started = timestamp_millis()?;
        let id = operation_id("save-relay")?;
        record_diagnostic_phase(diagnostic.as_ref(), "apply");
        let result =
            (|| RuntimeStore::from_default_root()?.upsert_relay(input, &managed_codex_home()?))();
        durable_recorded = record_result_with_diagnostic(
            &id,
            OperationAction::SaveRelay,
            started,
            &result,
            &[],
            BTreeMap::new(),
            diagnostic.as_ref(),
        )
        .is_ok();
        result
    })();
    record_durable_command_result(
        diagnostic.as_ref(),
        &result,
        if durable_recorded {
            DiagnosticTerminalStatus::Succeeded
        } else {
            DiagnosticTerminalStatus::Partial
        },
        "complete",
        (!durable_recorded).then_some("commands.save_relay.operation_history_failed"),
        (!durable_recorded)
            .then_some("relay settings were saved without durable operation history"),
        "apply",
        "commands.save_relay.failed",
    );
    correlate_mutation_result(result, diagnostic.as_ref())
}

#[tauri::command]
pub fn list_codex_processes() -> Result<Vec<CodexProcess>, String> {
    let result = list_processes();
    record_background_result(
        "listCodexProcesses",
        "commands.list_codex_processes.failed",
        &result,
    );
    result
}

#[tauri::command]
pub async fn close_codex_processes() -> Result<Vec<CodexProcess>, String> {
    let diagnostic = begin_command_diagnostic("closeCodexProcesses");
    record_diagnostic_phase(diagnostic.as_ref(), "apply");
    let result = match tauri::async_runtime::spawn_blocking(close_codex).await {
        Ok(result) => result,
        Err(_) => Err("ChatGPT process close worker failed".to_string()),
    };
    record_diagnostic_result(
        diagnostic.as_ref(),
        &result,
        "complete",
        "apply",
        "commands.close_codex_processes.failed",
    );
    correlate_mutation_result(result, diagnostic.as_ref())
}

#[tauri::command]
pub async fn launch_chatgpt() -> Result<ChatGptLaunchReceipt, String> {
    let diagnostic = begin_command_diagnostic("launchChatgpt");
    record_diagnostic_phase(diagnostic.as_ref(), "preflight");
    let worker_diagnostic = diagnostic.clone();
    let result = match tauri::async_runtime::spawn_blocking(move || {
        let _mutation_guard = acquire_mutation_lock()?;
        record_diagnostic_phase(worker_diagnostic.as_ref(), "apply");
        Ok(ChatGptLaunchReceipt::from(launch_cached_chatgpt()))
    })
    .await
    {
        Ok(result) => result,
        Err(_) => Err("ChatGPT launch worker failed".to_string()),
    };
    record_chatgpt_launch_diagnostic(diagnostic.as_ref(), &result);
    correlate_mutation_result(result, diagnostic.as_ref())
}

#[tauri::command]
pub async fn switch_runtime(
    runtime_id: String,
    _relay_preference: Option<RelaySwitchPreference>,
    on_progress: Channel<RuntimeSwitchProgress>,
) -> Result<RuntimeSwitchResult, String> {
    let diagnostic = begin_command_diagnostic("switchRuntime");
    record_diagnostic_phase(diagnostic.as_ref(), "waitingForMutationLock");
    let worker_progress = on_progress.clone();
    let worker_diagnostic = diagnostic.clone();
    let result = match tauri::async_runtime::spawn_blocking(move || {
        switch_runtime_blocking(runtime_id, worker_progress, worker_diagnostic)
    })
    .await
    {
        Ok(result) => result,
        Err(_) => {
            let error = "runtime switch worker failed".to_string();
            emit_runtime_switch_failure_diagnostic(
                &on_progress,
                diagnostic.as_ref(),
                &error,
                RuntimeSwitchOutcome::FailedBeforeWrite,
            );
            record_diagnostic_terminal(
                diagnostic.as_ref(),
                DiagnosticTerminalStatus::Failed,
                "workerJoin",
                Some("commands.switch_runtime.worker_join"),
                Some(&error),
            );
            Err(error)
        }
    };
    let switch_succeeded = result.is_ok();
    let _ = schedule_shadow_scan_after_switch(
        switch_succeeded,
        || {
            Some((
                managed_codex_home().ok()?,
                appdata_root().ok()?.join("codex-switch"),
            ))
        },
        request_shadow_and_automatic_gc,
    );
    correlate_mutation_result(result, diagnostic.as_ref())
}

fn schedule_shadow_scan_after_switch(
    succeeded: bool,
    resolve_roots: impl FnOnce() -> Option<(PathBuf, PathBuf)>,
    schedule: impl FnOnce(PathBuf, PathBuf) -> bool,
) -> bool {
    if !succeeded {
        return false;
    }
    let Some((codex_home, data_root)) = resolve_roots() else {
        return false;
    };
    schedule(codex_home, data_root)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AutomaticGcDecision {
    Stop,
    WaitForFreshScan,
    WaitForWriter,
    Blocked(&'static str),
    Run(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutomaticGcRecoveryDecision {
    Continue,
    WaitForSafeWindow,
}

// Keep every safe-window input explicit at this pure decision boundary; hiding
// one in mutable ambient state would weaken the automatic-GC audit contract.
#[allow(clippy::too_many_arguments)]
fn automatic_gc_decision(
    enabled: bool,
    migration_operation_id: Option<&str>,
    scan_id: Option<&str>,
    baseline_scan_id: Option<&str>,
    high_confidence_copy_count: usize,
    shadow_scan_running: bool,
    active_writer_count: usize,
    unfinished_non_gc_operation_count: usize,
) -> AutomaticGcDecision {
    if !enabled {
        return AutomaticGcDecision::Stop;
    }
    let Some(migration_operation_id) = migration_operation_id else {
        return AutomaticGcDecision::Stop;
    };
    if unfinished_non_gc_operation_count > 0 {
        return AutomaticGcDecision::Blocked(
            "automatic offline cleanup is blocked by an unfinished non-GC operation",
        );
    }
    if scan_id.is_none() || scan_id == baseline_scan_id {
        return if shadow_scan_running {
            AutomaticGcDecision::WaitForFreshScan
        } else {
            AutomaticGcDecision::Blocked(
                "automatic offline cleanup did not receive a fresh Shadow report",
            )
        };
    }
    if active_writer_count > 0 {
        return AutomaticGcDecision::WaitForWriter;
    }
    if high_confidence_copy_count == 0 {
        return AutomaticGcDecision::Stop;
    }
    AutomaticGcDecision::Run(migration_operation_id.to_string())
}

fn schedule_cleanup_for_current_roots(allow_automatic_gc: bool) -> bool {
    let codex_home = match managed_codex_home() {
        Ok(path) => path,
        Err(error) => {
            record_background_result(
                "resolveSessionStorageShadowRoots",
                "commands.session_storage_shadow.root_resolution_failed",
                &Err::<(), String>(error),
            );
            return false;
        }
    };
    let appdata = match appdata_root() {
        Ok(path) => path,
        Err(error) => {
            record_background_result(
                "resolveSessionStorageShadowRoots",
                "commands.session_storage_shadow.root_resolution_failed",
                &Err::<(), String>(error),
            );
            return false;
        }
    };
    let data_root = appdata.join("codex-switch");
    schedule_shadow_with_optional_automatic_gc(
        allow_automatic_gc,
        codex_home,
        data_root,
        request_background_shadow_scan,
        request_shadow_and_automatic_gc,
    )
}

fn schedule_shadow_with_optional_automatic_gc(
    allow_automatic_gc: bool,
    codex_home: PathBuf,
    data_root: PathBuf,
    schedule_shadow: impl FnOnce(PathBuf, PathBuf) -> bool,
    schedule_shadow_and_gc: impl FnOnce(PathBuf, PathBuf) -> bool,
) -> bool {
    if allow_automatic_gc {
        schedule_shadow_and_gc(codex_home, data_root)
    } else {
        schedule_shadow(codex_home, data_root)
    }
}

fn request_shadow_and_automatic_gc(codex_home: PathBuf, data_root: PathBuf) -> bool {
    let automatic_cleanup_enabled = match load_session_storage_settings(&data_root) {
        Ok(settings) => settings.automatic_cleanup_enabled,
        Err(error) => {
            record_background_result(
                "loadAutomaticSessionStorageCleanupSetting",
                "commands.automatic_session_storage_gc.settings_failed",
                &Err::<(), String>(error),
            );
            false
        }
    };
    let baseline_scan_id = match load_last_shadow_report(&data_root) {
        Ok(report) => report.map(|report| report.scan_id),
        Err(error) => {
            record_background_result(
                "loadSessionStorageShadowBaseline",
                "commands.session_storage_shadow.baseline_failed",
                &Err::<(), String>(error),
            );
            None
        }
    };
    let scan_scheduled = request_background_shadow_scan(codex_home.clone(), data_root.clone());
    if scan_scheduled && automatic_cleanup_enabled {
        let _ = request_background_automatic_gc(codex_home, data_root, baseline_scan_id);
    }
    scan_scheduled
}

fn request_background_automatic_gc(
    codex_home: PathBuf,
    data_root: PathBuf,
    baseline_scan_id: Option<String>,
) -> bool {
    let generation = AUTOMATIC_GC_GENERATION
        .fetch_add(1, Ordering::AcqRel)
        .saturating_add(1);
    {
        let mut request = AUTOMATIC_GC_REQUEST
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *request = Some(AutomaticGcRequest {
            codex_home,
            data_root,
            baseline_scan_id,
            generation,
        });
    }
    AUTOMATIC_GC_PENDING.store(true, Ordering::Release);
    ensure_automatic_gc_worker()
}

fn ensure_automatic_gc_worker() -> bool {
    if AUTOMATIC_GC_RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return true;
    }
    match thread::Builder::new()
        .name("session-storage-automatic-gc".to_string())
        .spawn(background_automatic_gc_loop)
    {
        Ok(_) => true,
        Err(_) => {
            AUTOMATIC_GC_RUNNING.store(false, Ordering::Release);
            record_background_result(
                "spawnAutomaticSessionStorageOfflineGc",
                "commands.automatic_session_storage_gc.spawn_failed",
                &Err::<(), String>(
                    "automatic session storage cleanup worker could not be started".to_string(),
                ),
            );
            false
        }
    }
}

fn background_automatic_gc_loop() {
    loop {
        if !AUTOMATIC_GC_PENDING.swap(false, Ordering::AcqRel) {
            break;
        }
        let request = AUTOMATIC_GC_REQUEST
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let Some(request) = request else {
            break;
        };
        match recover_deferred_automatic_offline_gc(&request.data_root) {
            Ok(AutomaticGcRecoveryDecision::Continue) => {}
            Ok(AutomaticGcRecoveryDecision::WaitForSafeWindow) => {
                AUTOMATIC_GC_PENDING.store(true, Ordering::Release);
                thread::sleep(AUTOMATIC_GC_SAFE_WINDOW_RETRY);
                continue;
            }
            Err(error) if error == mutation_busy_error() => {
                AUTOMATIC_GC_PENDING.store(true, Ordering::Release);
                thread::sleep(AUTOMATIC_GC_SAFE_WINDOW_RETRY);
                continue;
            }
            Err(error) => {
                record_background_result(
                    "recoverAutomaticSessionStorageOfflineGc",
                    "commands.automatic_session_storage_gc.recovery_failed",
                    &Err::<(), String>(error),
                );
                continue;
            }
        }
        let retention = run_session_storage_startup_retention();
        if let Err(error) = &retention {
            record_background_result(
                "runSessionStorageRetentionAtSafeWindow",
                "commands.session_storage_safe_window_retention.failed",
                &Err::<(), String>(error.clone()),
            );
            continue;
        }
        let decision = inspect_automatic_gc_window(
            &request.codex_home,
            &request.data_root,
            request.baseline_scan_id.as_deref(),
        );
        if AUTOMATIC_GC_GENERATION.load(Ordering::Acquire) != request.generation {
            AUTOMATIC_GC_PENDING.store(true, Ordering::Release);
            continue;
        }
        match decision {
            Ok(AutomaticGcDecision::Stop) => {}
            Ok(AutomaticGcDecision::WaitForFreshScan) | Ok(AutomaticGcDecision::WaitForWriter) => {
                AUTOMATIC_GC_PENDING.store(true, Ordering::Release);
                thread::sleep(AUTOMATIC_GC_SAFE_WINDOW_RETRY);
            }
            Ok(AutomaticGcDecision::Blocked(error)) => record_background_result(
                "inspectAutomaticSessionStorageOfflineGc",
                "commands.automatic_session_storage_gc.blocked",
                &Err::<(), String>(error.to_string()),
            ),
            Ok(AutomaticGcDecision::Run(migration_operation_id)) => {
                let result =
                    run_automatic_session_storage_offline_gc_blocking(&migration_operation_id);
                if result
                    .as_ref()
                    .err()
                    .is_some_and(|error| automatic_gc_error_is_transient(error))
                {
                    AUTOMATIC_GC_PENDING.store(true, Ordering::Release);
                    thread::sleep(AUTOMATIC_GC_SAFE_WINDOW_RETRY);
                } else {
                    record_background_result(
                        "runAutomaticSessionStorageOfflineGc",
                        "commands.automatic_session_storage_gc.failed",
                        &result,
                    );
                }
            }
            Err(error) => record_background_result(
                "inspectAutomaticSessionStorageOfflineGc",
                "commands.automatic_session_storage_gc.preflight_failed",
                &Err::<(), String>(error),
            ),
        }
    }
    AUTOMATIC_GC_RUNNING.store(false, Ordering::Release);
    if AUTOMATIC_GC_PENDING.load(Ordering::Acquire) {
        let _ = ensure_automatic_gc_worker();
    }
}

fn automatic_gc_error_is_transient(error: &str) -> bool {
    error == mutation_busy_error()
        || error.starts_with(
            "session storage mutation requires every Codex writer to be closed; activeProcesses=",
        )
        || error.starts_with("a standalone Codex CLI is still running; close it before ")
        || error
            == "offline session storage cleanup rollback is waiting for every Codex writer to close"
}

fn recover_deferred_automatic_offline_gc(
    data_root: &Path,
) -> Result<AutomaticGcRecoveryDecision, String> {
    let _mutation_guard = acquire_mutation_lock()?;
    let (managed, standalone) = list_process_inventory()?;
    if !managed.is_empty() || !standalone.is_empty() {
        return Ok(AutomaticGcRecoveryDecision::WaitForSafeWindow);
    }
    let store = OperationLedgerStore::new(data_root);
    let interrupted = store
        .unfinished()?
        .into_iter()
        .filter(|ledger| ledger.kind == SessionStorageOperationKind::OfflineGc)
        .collect::<Vec<_>>();
    for ledger in interrupted {
        if matches!(
            ledger.phase,
            SessionStorageOperationPhase::Available
                | SessionStorageOperationPhase::Preflight
                | SessionStorageOperationPhase::Backup
                | SessionStorageOperationPhase::BackupVerified
                | SessionStorageOperationPhase::PlanReady
        ) {
            rollback_unapplied_offline_gc(
                &store,
                data_root,
                &ledger.operation_id,
                "offlineGcInterruptedBeforeApply",
            )?;
            continue;
        }
        let recovery =
            recover_interrupted_offline_gc(&store, data_root, &ledger.operation_id, || {
                ensure_no_codex_writer_for_session_storage(
                    "recovering automatic offline session storage cleanup",
                )
            })?;
        match recovery {
            OfflineGcRecoveryStatus::RolledBack => {}
            OfflineGcRecoveryStatus::DeferredByLiveWriter => {
                return Ok(AutomaticGcRecoveryDecision::WaitForSafeWindow);
            }
            OfflineGcRecoveryStatus::Failed => {
                return Err(
                    "automatic offline session storage cleanup recovery left recoverable residuals"
                        .to_string(),
                );
            }
        }
    }
    Ok(AutomaticGcRecoveryDecision::Continue)
}

fn inspect_automatic_gc_window(
    codex_home: &Path,
    data_root: &Path,
    baseline_scan_id: Option<&str>,
) -> Result<AutomaticGcDecision, String> {
    if path_key(&managed_codex_home()?) != path_key(codex_home)
        || path_key(&appdata_root()?.join("codex-switch")) != path_key(data_root)
    {
        return Ok(AutomaticGcDecision::Stop);
    }
    let control = load_session_storage_control_state(data_root, codex_home)?;
    let report = load_last_shadow_report(data_root)?;
    let active_writer_count = match list_process_inventory() {
        Ok((managed, standalone)) => managed.len().saturating_add(standalone.len()),
        Err(error) => return Err(error),
    };
    let unfinished_non_gc_operation_count = OperationLedgerStore::new(data_root)
        .unfinished()?
        .into_iter()
        .filter(|ledger| ledger.kind != SessionStorageOperationKind::OfflineGc)
        .count();
    Ok(automatic_gc_decision(
        control.automatic_cleanup_enabled,
        control.migration_operation_id.as_deref(),
        report.as_ref().map(|report| report.scan_id.as_str()),
        baseline_scan_id,
        report
            .as_ref()
            .map(|report| report.summary.high_confidence_copy_count)
            .unwrap_or(0),
        background_shadow_scan_is_running(),
        active_writer_count,
        unfinished_non_gc_operation_count,
    ))
}

fn relay_validation_without_network(runtime_id: &str) -> RelayValidationStatus {
    if runtime_id == RELAY_RUNTIME_ID {
        RelayValidationStatus::Skipped
    } else {
        RelayValidationStatus::NotApplicable
    }
}

fn switch_runtime_blocking(
    runtime_id: String,
    on_progress: Channel<RuntimeSwitchProgress>,
    diagnostic: Option<DiagnosticOperation>,
) -> Result<RuntimeSwitchResult, String> {
    let _mutation_guard = match acquire_mutation_lock() {
        Ok(guard) => guard,
        Err(error) => {
            emit_runtime_switch_failure_diagnostic(
                &on_progress,
                diagnostic.as_ref(),
                &error,
                RuntimeSwitchOutcome::FailedBeforeWrite,
            );
            record_diagnostic_terminal(
                diagnostic.as_ref(),
                diagnostic_status_for_command_error(&error),
                "preflight",
                Some("commands.switch_runtime.mutation_lock"),
                Some(&error),
            );
            return Err(error);
        }
    };
    let started = match timestamp_millis() {
        Ok(started) => started,
        Err(error) => {
            emit_runtime_switch_failure_diagnostic(
                &on_progress,
                diagnostic.as_ref(),
                &error,
                RuntimeSwitchOutcome::FailedBeforeWrite,
            );
            record_diagnostic_terminal(
                diagnostic.as_ref(),
                DiagnosticTerminalStatus::Failed,
                "preflight",
                Some("commands.switch_runtime.timestamp"),
                Some(&error),
            );
            return Err(error);
        }
    };
    let attempt_id = match operation_id("switch-runtime-attempt") {
        Ok(attempt_id) => attempt_id,
        Err(error) => {
            emit_runtime_switch_failure_diagnostic(
                &on_progress,
                diagnostic.as_ref(),
                &error,
                RuntimeSwitchOutcome::FailedBeforeWrite,
            );
            record_diagnostic_terminal(
                diagnostic.as_ref(),
                DiagnosticTerminalStatus::Failed,
                "preflight",
                Some("commands.switch_runtime.operation_id"),
                Some(&error),
            );
            return Err(error);
        }
    };
    let mut failure_outcome = RuntimeSwitchOutcome::FailedBeforeWrite;
    let mut failure_operation_id = None;
    let target_is_account = runtime_id == PLUS_RUNTIME_ID;
    let relay_validation = relay_validation_without_network(&runtime_id);
    let mut launch_target_captured = false;
    emit_runtime_switch_progress_diagnostic(
        &on_progress,
        diagnostic.as_ref(),
        RuntimeSwitchPhase::LoadingRuntime,
        None,
    );
    let mut result = (|| {
        let store = RuntimeStore::from_default_root()?;
        let current_home = managed_codex_home()?;
        emit_runtime_switch_progress_diagnostic(
            &on_progress,
            diagnostic.as_ref(),
            RuntimeSwitchPhase::ValidatingOfficialAuth,
            None,
        );
        let plan = preflight_request_route_switch(&store, &runtime_id, &current_home)?;
        if plan.requires_change() || target_is_account {
            close_runtime_processes_with_progress(
                || {
                    let processes =
                        list_managed_processes_for_closed_mutation("switching request routes")?;
                    capture_chatgpt_launch_target_once(&mut launch_target_captured, || {
                        // The pre-close inventory is the only point at which the
                        // running package identity may be captured.
                        let _ = cache_chatgpt_launch_target();
                    });
                    Ok(processes)
                },
                || close_codex().map(|_| ()),
                |phase, message| {
                    emit_runtime_switch_progress_diagnostic(
                        &on_progress,
                        diagnostic.as_ref(),
                        phase,
                        message,
                    )
                },
            )?;
        } else {
            // A Relay no-op does not enter the process-close gate, so retain
            // the running package identity for the controlled launcher.
            capture_chatgpt_launch_target_once(&mut launch_target_captured, || {
                let _ = cache_chatgpt_launch_target();
            });
        }
        match switch_request_route_preflighted_with_progress(
            &store,
            &current_home,
            plan,
            &mut || ensure_codex_closed("switching request routes"),
            &mut |phase| {
                emit_runtime_switch_progress_diagnostic(
                    &on_progress,
                    diagnostic.as_ref(),
                    phase,
                    None,
                )
            },
        ) {
            Ok(mut receipt) => {
                let provider = if runtime_id == PLUS_RUNTIME_ID {
                    "openai"
                } else {
                    "openai_custom"
                };
                let account_slot =
                    format!("{}:{}", receipt.runtime.id, receipt.runtime.created_at_ms);
                // Bind provenance to the verified route cutover, not to the
                // potentially much earlier switch preflight timestamp.
                let provenance = timestamp_millis()
                    .map_err(|_| "route epoch cutover timestamp is unavailable".to_string())
                    .and_then(|effective_at_ms| {
                        record_or_verify_route_epoch(
                            &store.data_root()?,
                            RouteEpochInput::new(
                                &receipt.operation_id,
                                effective_at_ms,
                                &runtime_id,
                                provider,
                                &account_slot,
                                receipt.runtime.model.as_deref(),
                            ),
                            true,
                        )
                    });
                receipt.route_provenance = match provenance {
                    Ok(provenance) => provenance,
                    Err(error) => {
                        if let Some(diagnostic) = diagnostic.as_ref() {
                            let _ = diagnostic.branch(
                                DiagnosticLevel::Error,
                                Some("recordingProvenance"),
                                Some("commands.switch_runtime.route_provenance"),
                                Some(&error),
                                empty_context(),
                            );
                        }
                        let message =
                            "会话来源账本未写入；为避免产生无来源回合，ChatGPT 已保持关闭"
                                .to_string();
                        receipt.warnings.push(message.clone());
                        RouteProvenanceReceipt::failed(message)
                    }
                };
                Ok(receipt)
            }
            Err(failure) => {
                failure_operation_id = failure.operation_id;
                failure_outcome = failure.outcome;
                Err(failure.message)
            }
        }
    })();
    let (id, counts) = match &result {
        Ok(receipt) => (
            receipt.operation_id.as_str(),
            BTreeMap::from([
                (
                    "requestRouteChanged".to_string(),
                    usize::from(receipt.changed),
                ),
                ("officialAuthPreserved".to_string(), 1),
                (
                    "chatProcessStateRepaired".to_string(),
                    usize::from(receipt.chat_process_state_repaired),
                ),
                (
                    "sessionViewDetectedThreads".to_string(),
                    receipt.incremental_session_sync.detected_threads,
                ),
                (
                    "sessionViewSyncedThreads".to_string(),
                    receipt.incremental_session_sync.synced_threads,
                ),
                (
                    "routeProvenanceReady".to_string(),
                    usize::from(receipt.route_provenance.is_ready()),
                ),
            ]),
        ),
        Err(_) => (
            failure_operation_id
                .as_deref()
                .unwrap_or(attempt_id.as_str()),
            BTreeMap::new(),
        ),
    };
    emit_runtime_switch_progress_diagnostic(
        &on_progress,
        diagnostic.as_ref(),
        RuntimeSwitchPhase::RecordingResult,
        None,
    );
    let terminal_record = match record_runtime_switch_result_with_diagnostic(
        id,
        started,
        &result,
        &[],
        counts,
        failure_outcome,
        diagnostic.as_ref(),
    ) {
        Ok(record) => Some(record),
        Err(error) => {
            if let Some(diagnostic) = diagnostic.as_ref() {
                let _ = diagnostic.branch(
                    DiagnosticLevel::Error,
                    Some("recordingResult"),
                    Some("commands.switch_runtime.operation_log"),
                    Some(&error),
                    empty_context(),
                );
            }
            None
        }
    };
    let terminal_recorded = terminal_record.is_some();
    if let Ok(receipt) = &mut result {
        receipt.relay_validation = relay_validation;
        if successful_switch_requests_chatgpt_launch(receipt.changed) {
            let provenance_ready = receipt.route_provenance.is_ready();
            if terminal_recorded && provenance_ready {
                emit_runtime_switch_progress_diagnostic(
                    &on_progress,
                    diagnostic.as_ref(),
                    RuntimeSwitchPhase::LaunchingApp,
                    None,
                );
            }
            receipt.chatgpt_launch = if provenance_ready {
                launch_chatgpt_after_durable_terminal(terminal_recorded, || {
                    ChatGptLaunchReceipt::from(launch_cached_chatgpt())
                })
            } else {
                ChatGptLaunchReceipt {
                    status: ChatGptLaunchStatus::Blocked,
                    message: receipt.route_provenance.message.clone(),
                }
            };
        }
    } else if launch_target_captured
        && matches!(
            failure_outcome,
            RuntimeSwitchOutcome::FailedBeforeWrite | RuntimeSwitchOutcome::RolledBack
        )
    {
        emit_runtime_switch_progress_diagnostic(
            &on_progress,
            diagnostic.as_ref(),
            RuntimeSwitchPhase::LaunchingApp,
            None,
        );
        let launch = ChatGptLaunchReceipt::from(launch_cached_chatgpt());
        if launch.status == ChatGptLaunchStatus::Failed {
            if let Err(message) = &mut result {
                message.push_str("；请求端未变更或已安全回滚，但 ChatGPT 重新打开失败");
            }
        }
    }
    emit_runtime_switch_terminal_diagnostic(
        &on_progress,
        diagnostic.as_ref(),
        &result,
        failure_outcome,
    );
    if diagnostic
        .as_ref()
        .is_some_and(|diagnostic| !diagnostic.is_terminal_recorded())
    {
        match &result {
            Ok(receipt) if terminal_recorded => {
                record_runtime_switch_success_diagnostic(diagnostic.as_ref(), receipt)
            }
            Ok(_) => record_diagnostic_terminal(
                diagnostic.as_ref(),
                DiagnosticTerminalStatus::Partial,
                "recordingResult",
                Some("commands.switch_runtime.operation_log"),
                Some("request route changed, but the durable operation terminal was unavailable"),
            ),
            Err(error) => record_diagnostic_terminal(
                diagnostic.as_ref(),
                runtime_switch_diagnostic_status(failure_outcome),
                runtime_switch_diagnostic_terminal_phase(failure_outcome),
                Some(runtime_switch_diagnostic_error_code(failure_outcome)),
                Some(error),
            ),
        }
    }
    result
}

fn capture_chatgpt_launch_target_once<Capture>(captured: &mut bool, capture: Capture)
where
    Capture: FnOnce(),
{
    if *captured {
        return;
    }
    capture();
    *captured = true;
}

fn successful_switch_requests_chatgpt_launch(_changed: bool) -> bool {
    // Product contract: both an applied switch and a successful no-op finish
    // by bringing ChatGPT to a running state.
    true
}

fn launch_chatgpt_after_durable_terminal<Launch>(
    terminal_recorded: bool,
    launch: Launch,
) -> ChatGptLaunchReceipt
where
    Launch: FnOnce() -> ChatGptLaunchReceipt,
{
    if !terminal_recorded {
        return ChatGptLaunchReceipt {
            status: ChatGptLaunchStatus::Failed,
            message: Some(
                "ChatGPT was not launched because the runtime switch terminal record could not be persisted."
                    .to_string(),
            ),
        };
    }
    launch()
}

fn close_runtime_processes_with_progress<List, Close, Progress>(
    mut list_managed_processes: List,
    mut close_managed_processes: Close,
    mut progress: Progress,
) -> Result<bool, String>
where
    List: FnMut() -> Result<Vec<CodexProcess>, String>,
    Close: FnMut() -> Result<(), String>,
    Progress: FnMut(RuntimeSwitchPhase, Option<String>),
{
    progress(RuntimeSwitchPhase::DetectingApp, None);
    let processes = list_managed_processes()?;
    let closed_running_processes = !processes.is_empty();
    if !processes.is_empty() {
        progress(
            RuntimeSwitchPhase::ClosingApp,
            Some(format!("Closing {} ChatGPT process(es)", processes.len())),
        );
        close_managed_processes()?;
    }
    if list_managed_processes()?.is_empty() {
        Ok(closed_running_processes)
    } else {
        Err("ChatGPT is still running; close it before switching request routes".to_string())
    }
}

fn emit_runtime_switch_progress_diagnostic(
    on_progress: &Channel<RuntimeSwitchProgress>,
    diagnostic: Option<&DiagnosticOperation>,
    phase: RuntimeSwitchPhase,
    message: Option<String>,
) {
    emit_runtime_switch_progress_event_diagnostic(on_progress, diagnostic, phase, message, None);
}

fn emit_runtime_switch_progress_event_diagnostic(
    on_progress: &Channel<RuntimeSwitchProgress>,
    diagnostic: Option<&DiagnosticOperation>,
    phase: RuntimeSwitchPhase,
    message: Option<String>,
    outcome: Option<RuntimeSwitchOutcome>,
) {
    record_diagnostic_phase(diagnostic, runtime_switch_phase_name(phase));
    let _ = on_progress.send(RuntimeSwitchProgress {
        phase,
        timestamp_ms: timestamp_millis().unwrap_or_default(),
        operation_id: diagnostic_correlation_id(diagnostic),
        message,
        outcome,
    });
}

fn emit_runtime_switch_failure_diagnostic(
    on_progress: &Channel<RuntimeSwitchProgress>,
    diagnostic: Option<&DiagnosticOperation>,
    error: &str,
    outcome: RuntimeSwitchOutcome,
) {
    emit_runtime_switch_progress_event_diagnostic(
        on_progress,
        diagnostic,
        RuntimeSwitchPhase::Failed,
        Some(error.to_string()),
        Some(outcome),
    );
}

#[cfg(test)]
fn emit_runtime_switch_terminal<T>(
    on_progress: &Channel<RuntimeSwitchProgress>,
    result: &Result<T, String>,
    failure_outcome: RuntimeSwitchOutcome,
) {
    emit_runtime_switch_terminal_diagnostic(on_progress, None, result, failure_outcome);
}

fn emit_runtime_switch_terminal_diagnostic<T>(
    on_progress: &Channel<RuntimeSwitchProgress>,
    diagnostic: Option<&DiagnosticOperation>,
    result: &Result<T, String>,
    failure_outcome: RuntimeSwitchOutcome,
) {
    match result {
        Ok(_) => {
            emit_runtime_switch_progress_diagnostic(
                on_progress,
                diagnostic,
                RuntimeSwitchPhase::Complete,
                None,
            );
        }
        Err(error) => {
            emit_runtime_switch_failure_diagnostic(on_progress, diagnostic, error, failure_outcome);
        }
    }
}

fn runtime_switch_phase_name(phase: RuntimeSwitchPhase) -> &'static str {
    match phase {
        RuntimeSwitchPhase::LoadingRuntime => "loadingRuntime",
        RuntimeSwitchPhase::ValidatingOfficialAuth => "validatingOfficialAuth",
        RuntimeSwitchPhase::VerifyingRelay => "verifyingRelay",
        RuntimeSwitchPhase::DetectingApp => "detectingApp",
        RuntimeSwitchPhase::ClosingApp => "closingApp",
        RuntimeSwitchPhase::PreparingRuntime => "preparingRuntime",
        RuntimeSwitchPhase::RepairingAppState => "repairingAppState",
        RuntimeSwitchPhase::ApplyingRuntime => "applyingRuntime",
        RuntimeSwitchPhase::Verifying => "verifying",
        RuntimeSwitchPhase::RecordingResult => "recordingResult",
        RuntimeSwitchPhase::SyncingIncrementalSessions => "syncingIncrementalSessions",
        RuntimeSwitchPhase::RollingBack => "rollingBack",
        RuntimeSwitchPhase::LaunchingApp => "launchingApp",
        RuntimeSwitchPhase::Complete => "complete",
        RuntimeSwitchPhase::Failed => "failed",
    }
}

fn runtime_switch_diagnostic_status(outcome: RuntimeSwitchOutcome) -> DiagnosticTerminalStatus {
    match outcome {
        RuntimeSwitchOutcome::FailedBeforeWrite => DiagnosticTerminalStatus::Failed,
        RuntimeSwitchOutcome::RolledBack => DiagnosticTerminalStatus::RolledBack,
        RuntimeSwitchOutcome::RollbackFailed => DiagnosticTerminalStatus::RollbackFailed,
    }
}

fn runtime_switch_diagnostic_error_code(outcome: RuntimeSwitchOutcome) -> &'static str {
    match outcome {
        RuntimeSwitchOutcome::FailedBeforeWrite => "commands.switch_runtime.failed_before_write",
        RuntimeSwitchOutcome::RolledBack => "commands.switch_runtime.rolled_back",
        RuntimeSwitchOutcome::RollbackFailed => "commands.switch_runtime.rollback_failed",
    }
}

fn runtime_switch_diagnostic_terminal_phase(outcome: RuntimeSwitchOutcome) -> &'static str {
    match outcome {
        RuntimeSwitchOutcome::FailedBeforeWrite => "preflight",
        RuntimeSwitchOutcome::RolledBack | RuntimeSwitchOutcome::RollbackFailed => "rollback",
    }
}

#[tauri::command]
pub async fn merge_and_repair_sessions(
    on_progress: Channel<SessionSyncProgress>,
) -> Result<SessionSyncReceipt, String> {
    let diagnostic = begin_command_diagnostic("mergeAndRepairSessions");
    record_diagnostic_phase(diagnostic.as_ref(), "waitingForMutationLock");
    let worker_progress = on_progress.clone();
    let worker_diagnostic = diagnostic.clone();
    let result = match tauri::async_runtime::spawn_blocking(move || {
        merge_and_repair_sessions_blocking(worker_progress, worker_diagnostic)
    })
    .await
    {
        Ok(result) => result,
        Err(_) => {
            let error = "session sync worker failed".to_string();
            emit_session_sync_progress_diagnostic(
                &on_progress,
                diagnostic.as_ref(),
                SessionSyncPhase::Failed,
                Some(error.clone()),
            );
            record_diagnostic_terminal(
                diagnostic.as_ref(),
                DiagnosticTerminalStatus::Failed,
                "workerJoin",
                Some("commands.merge_and_repair_sessions.worker_join"),
                Some(&error),
            );
            Err(error)
        }
    };
    correlate_mutation_result(result, diagnostic.as_ref())
}

fn merge_and_repair_sessions_blocking(
    on_progress: Channel<SessionSyncProgress>,
    diagnostic: Option<DiagnosticOperation>,
) -> Result<SessionSyncReceipt, String> {
    let _mutation_guard = match acquire_mutation_lock() {
        Ok(guard) => guard,
        Err(error) => {
            emit_session_sync_progress_diagnostic(
                &on_progress,
                diagnostic.as_ref(),
                SessionSyncPhase::Failed,
                Some(error.clone()),
            );
            record_diagnostic_terminal(
                diagnostic.as_ref(),
                diagnostic_status_for_command_error(&error),
                "preflight",
                Some("commands.merge_and_repair_sessions.mutation_lock"),
                Some(&error),
            );
            return Err(error);
        }
    };
    emit_session_sync_progress_diagnostic(
        &on_progress,
        diagnostic.as_ref(),
        SessionSyncPhase::Preparing,
        None,
    );
    let mut operation_id_value = None::<String>;
    let mut closed_running_processes = false;
    let mut apply_started = false;
    let result = (|| {
        let canonical_root = managed_codex_home()?;
        let data_root = appdata_root()?.join("codex-switch");
        let store = OperationLedgerStore::new(&data_root);
        let backup_destination =
            load_session_merge_backup_destination(&data_root, &canonical_root)?;
        if !store.unfinished()?.is_empty() {
            return Err(
                "another unfinished session storage operation must be recovered first".to_string(),
            );
        }
        let operation_id = operation_id("session-merge-repair")?;
        operation_id_value = Some(operation_id.clone());
        if let Some(diagnostic) = diagnostic.as_ref() {
            let _ = diagnostic.bind_operation_id(operation_id.clone());
        }
        store.create(
            &operation_id,
            SessionStorageOperationKind::Migration,
            &canonical_root,
        )?;
        store.transition(&operation_id, SessionStorageOperationPhase::Preflight)?;

        let processes =
            list_managed_processes_for_closed_mutation("merging and repairing canonical sessions")?;
        if !processes.is_empty() {
            cache_chatgpt_launch_target()?;
            emit_session_sync_progress_diagnostic(
                &on_progress,
                diagnostic.as_ref(),
                SessionSyncPhase::ClosingApp,
                Some(format!("Closing {} ChatGPT process(es)", processes.len())),
            );
            close_codex()?;
            closed_running_processes = true;
        }
        ensure_no_codex_writer_for_session_storage("merging and repairing canonical sessions")?;

        let report = run_migration_preflight(
            &canonical_root,
            &data_root,
            &operation_id,
            &backup_destination,
        )?;
        persist_migration_preflight(&data_root, &report)?;
        record_migration_preflight_in_ledger(&store, &data_root, &operation_id)?;
        if !report.ready_for_backup {
            return Err(format!(
                "session merge preflight was blocked; blockerCount={}",
                report.blockers.len()
            ));
        }

        store.transition(&operation_id, SessionStorageOperationPhase::Backup)?;
        emit_session_sync_progress_diagnostic(
            &on_progress,
            diagnostic.as_ref(),
            SessionSyncPhase::BackingUp,
            None,
        );
        let sources = migration_backup_sources_for_preflight(&canonical_root, &data_root, &report)?;
        let backup = create_migration_backup(&report.backup_destination, &operation_id, &sources)?;
        record_migration_backup_in_ledger(&store, &operation_id, &backup)?;
        verify_migration_backup_sources(&backup, &sources)?;
        let verifier = NativeCodexBackupVerifier::discover()?;
        let isolated_root = data_root
            .join("session-storage-v1/operations")
            .join(&operation_id)
            .join("migration-staging");
        let backup =
            verify_migration_backup_with_runtime(&backup.backup_dir, &isolated_root, &verifier)?;
        store.transition(&operation_id, SessionStorageOperationPhase::BackupVerified)?;

        emit_session_sync_progress_diagnostic(
            &on_progress,
            diagnostic.as_ref(),
            SessionSyncPhase::Reconciling,
            None,
        );
        let prepared = prepare_migration_apply_plan(&canonical_root, &data_root, &report, &backup)?;
        record_prepared_migration_in_ledger(&store, &operation_id, &prepared)?;
        store.transition(&operation_id, SessionStorageOperationPhase::PlanReady)?;
        let persistent_session_bytes_added =
            migration_persistent_session_bytes_added(&prepared.plan)?;
        store.transition(&operation_id, SessionStorageOperationPhase::Applying)?;
        apply_started = true;
        if let Err(error) = ensure_no_codex_writer_for_session_storage(
            "performing the final session merge write check",
        ) {
            abort_session_storage_migration_before_apply(
                &store,
                &prepared.plan,
                "sessionMergeWriterAppearedBeforeApply",
            )?;
            return Err(error);
        }
        if let Err(failure) = apply_prepared_migration_classified(
            &prepared.plan,
            || {
                store.update(&operation_id, |ledger| {
                    ledger.live_mutation_started = true;
                    Ok(())
                })?;
                Ok(())
            },
            || ensure_no_codex_writer_for_session_storage("performing a session merge write"),
        ) {
            let error = failure.message().to_string();
            match failure {
                crate::session_storage::migration_apply::MigrationRollbackFailure::Precondition(
                    _,
                ) => {
                    abort_session_storage_migration_before_apply(
                        &store,
                        &prepared.plan,
                        "sessionMergePreconditionFailed",
                    )?;
                }
                _ => rollback_session_storage_migration_after_failure(
                    &store,
                    &data_root,
                    &prepared.plan,
                    "sessionMergeApplyFailed",
                )?,
            }
            return Err(error);
        }
        if let Err(error) =
            store.transition(&operation_id, SessionStorageOperationPhase::Validating)
        {
            rollback_session_storage_migration_after_failure(
                &store,
                &data_root,
                &prepared.plan,
                "sessionMergeValidationTransitionFailed",
            )?;
            return Err(error);
        }
        let validation = (|| {
            ensure_no_codex_writer_for_session_storage("validating the applied session merge")?;
            validate_applied_migration(&prepared.plan)?;
            let runtime = verify_applied_migration_with_runtime(&prepared.plan, &verifier)?;
            ensure_no_codex_writer_for_session_storage(
                "finishing post-merge Codex runtime verification",
            )?;
            let mut receipt = validate_applied_migration(&prepared.plan)?;
            receipt.runtime_verification = Some(runtime);
            Ok(receipt)
        })();
        let apply_receipt = match validation {
            Ok(receipt) => receipt,
            Err(error) => {
                rollback_session_storage_migration_after_failure(
                    &store,
                    &data_root,
                    &prepared.plan,
                    "sessionMergeValidationFailed",
                )?;
                return Err(error);
            }
        };
        if let Err(error) = cleanup_migration_staging(&prepared.plan) {
            rollback_session_storage_migration_after_failure(
                &store,
                &data_root,
                &prepared.plan,
                "sessionMergeStagingCleanupFailed",
            )?;
            return Err(error);
        }
        match transition_to_committed(&store, &operation_id) {
            CommitTransitionDisposition::ConfirmedCommitted => {
                let witness_cleanup =
                    cleanup_committed_migration_ownership_witnesses(&prepared.plan);
                record_background_result(
                    "cleanupCommittedSessionMergeOwnershipWitnesses",
                    "commands.session_merge.witness_cleanup_failed",
                    &witness_cleanup,
                );
            }
            CommitTransitionDisposition::SafeToRollback(error) => {
                rollback_session_storage_migration_after_failure(
                    &store,
                    &data_root,
                    &prepared.plan,
                    "sessionMergeCommitFailed",
                )?;
                return Err(error);
            }
            CommitTransitionDisposition::PreserveForRecovery(error) => return Err(error),
        }
        let _ = request_shadow_and_automatic_gc(canonical_root.clone(), data_root);
        Ok(SessionSyncReceipt {
            operation_id,
            backups: vec![migration_backup_receipt_summary(
                &backup,
                &canonical_root,
                "session-merge-repair",
            )],
            result: session_sync_result_from_migration(
                &report,
                &apply_receipt,
                persistent_session_bytes_added,
            ),
            rolled_back: false,
            warnings: session_merge_warnings(&report),
            checkpoint_cleanup: CheckpointCleanupSummary::default(),
            chatgpt_launch: ChatGptLaunchReceipt::not_requested(),
        })
    })();

    let result = match result {
        Ok(mut receipt) => {
            emit_session_sync_progress_diagnostic(
                &on_progress,
                diagnostic.as_ref(),
                SessionSyncPhase::RecordingResult,
                None,
            );
            if append_session_merge_history(
                &receipt.operation_id,
                OperationStatus::Succeeded,
                OperationPhase::Complete,
                sync_counts(&receipt.result),
            )
            .is_err()
            {
                receipt
                    .warnings
                    .push("会话合并已提交，但本地操作记录写入失败".to_string());
            }
            if closed_running_processes {
                emit_session_sync_progress_diagnostic(
                    &on_progress,
                    diagnostic.as_ref(),
                    SessionSyncPhase::LaunchingApp,
                    None,
                );
                receipt.chatgpt_launch = ChatGptLaunchReceipt::from(launch_cached_chatgpt());
            }
            emit_session_sync_progress_diagnostic(
                &on_progress,
                diagnostic.as_ref(),
                SessionSyncPhase::Complete,
                None,
            );
            Ok(receipt)
        }
        Err(mut error) => {
            if let Some(operation_id) = operation_id_value.as_deref() {
                if !apply_started {
                    if let Err(recovery_error) = rollback_unapplied_session_merge(operation_id) {
                        append_warnings_to_error(&mut error, &[recovery_error]);
                    }
                }
                if let Some((status, phase)) =
                    session_merge_history_terminal(operation_id, apply_started)
                {
                    if append_session_merge_history(operation_id, status, phase, BTreeMap::new())
                        .is_err()
                    {
                        append_warnings_to_error(&mut error, &["本地操作记录写入失败".to_string()]);
                    }
                }
            }
            let safe_to_relaunch = !apply_started
                || operation_id_value.as_deref().is_some_and(|operation_id| {
                    session_storage_operation_phase(operation_id).is_some_and(|phase| {
                        matches!(
                            phase,
                            SessionStorageOperationPhase::Committed
                                | SessionStorageOperationPhase::RolledBack
                        )
                    })
                });
            if closed_running_processes && safe_to_relaunch {
                emit_session_sync_progress_diagnostic(
                    &on_progress,
                    diagnostic.as_ref(),
                    SessionSyncPhase::LaunchingApp,
                    None,
                );
                let launch = ChatGptLaunchReceipt::from(launch_cached_chatgpt());
                if !matches!(
                    launch.status,
                    ChatGptLaunchStatus::Launched | ChatGptLaunchStatus::AlreadyRunning
                ) {
                    append_warnings_to_error(
                        &mut error,
                        &[
                            "ChatGPT could not be reopened after session merge rollback"
                                .to_string(),
                        ],
                    );
                }
            }
            emit_session_sync_progress_diagnostic(
                &on_progress,
                diagnostic.as_ref(),
                SessionSyncPhase::Failed,
                Some(error.clone()),
            );
            Err(error)
        }
    };

    if diagnostic
        .as_ref()
        .is_some_and(|operation| !operation.is_terminal_recorded())
    {
        match &result {
            Ok(receipt) => record_session_sync_success_diagnostic(diagnostic.as_ref(), receipt),
            Err(error) => {
                let rolled_back = operation_id_value.as_deref().is_some_and(|operation_id| {
                    session_storage_operation_phase(operation_id)
                        == Some(SessionStorageOperationPhase::RolledBack)
                });
                record_diagnostic_terminal(
                    diagnostic.as_ref(),
                    if rolled_back {
                        DiagnosticTerminalStatus::RolledBack
                    } else {
                        diagnostic_status_for_command_error(error)
                    },
                    if rolled_back { "rollback" } else { "failed" },
                    Some("commands.merge_and_repair_sessions.failed"),
                    Some(error),
                );
            }
        }
    }
    result
}

fn rollback_unapplied_session_merge(operation_id: &str) -> Result<(), String> {
    let data_root = appdata_root()?.join("codex-switch");
    let store = OperationLedgerStore::new(&data_root);
    rollback_unapplied_session_storage_migration(
        &store,
        &data_root,
        operation_id,
        "sessionMergePreApplyFailed",
    )
}

fn rollback_unapplied_session_storage_migration(
    store: &OperationLedgerStore,
    data_root: &Path,
    operation_id: &str,
    error_code: &str,
) -> Result<(), String> {
    rollback_unapplied_session_storage_migration_with_cleanup(
        store,
        data_root,
        operation_id,
        error_code,
        || cleanup_migration_staging_for_operation(data_root, operation_id).map(|_| ()),
    )
}

fn rollback_unapplied_session_storage_migration_with_cleanup<Cleanup>(
    store: &OperationLedgerStore,
    data_root: &Path,
    operation_id: &str,
    error_code: &str,
    cleanup: Cleanup,
) -> Result<(), String>
where
    Cleanup: FnOnce() -> Result<(), String>,
{
    let ledger = store.load(operation_id)?;
    if ledger.kind != SessionStorageOperationKind::Migration || ledger.phase.is_terminal() {
        return Ok(());
    }
    let preflight_path = data_root
        .join("session-storage-v1/operations")
        .join(operation_id)
        .join("preflight.json");
    if preflight_path.is_file() {
        record_migration_preflight_in_ledger(store, data_root, operation_id)?;
    }
    record_session_storage_operation_error(store, operation_id, error_code)?;
    match ledger.phase {
        SessionStorageOperationPhase::Available
        | SessionStorageOperationPhase::Preflight
        | SessionStorageOperationPhase::Backup
        | SessionStorageOperationPhase::BackupVerified
        | SessionStorageOperationPhase::PlanReady
        | SessionStorageOperationPhase::RollingBack => {
            if ledger.phase != SessionStorageOperationPhase::RollingBack {
                store.transition(operation_id, SessionStorageOperationPhase::RollingBack)?;
            }
            if cleanup().is_err() {
                record_session_storage_operation_error(
                    store,
                    operation_id,
                    "migrationPreApplyCleanupFailed",
                )?;
                return Err(
                    "session storage migration pre-apply cleanup left recoverable residuals"
                        .to_string(),
                );
            }
            store.transition(operation_id, SessionStorageOperationPhase::RolledBack)?;
            Ok(())
        }
        SessionStorageOperationPhase::Applying
        | SessionStorageOperationPhase::Validating
        | SessionStorageOperationPhase::Committed
        | SessionStorageOperationPhase::RolledBack
        | SessionStorageOperationPhase::Failed => Ok(()),
    }
}

fn session_storage_operation_phase(operation_id: &str) -> Option<SessionStorageOperationPhase> {
    let data_root = appdata_root().ok()?.join("codex-switch");
    OperationLedgerStore::new(&data_root)
        .load(operation_id)
        .ok()
        .map(|ledger| ledger.phase)
}

fn session_merge_history_terminal(
    operation_id: &str,
    apply_started: bool,
) -> Option<(OperationStatus, OperationPhase)> {
    match session_storage_operation_phase(operation_id)? {
        SessionStorageOperationPhase::Committed => {
            Some((OperationStatus::Succeeded, OperationPhase::Complete))
        }
        SessionStorageOperationPhase::RolledBack => {
            Some((OperationStatus::RolledBack, OperationPhase::Rollback))
        }
        SessionStorageOperationPhase::Failed => Some((
            OperationStatus::Failed,
            if apply_started {
                OperationPhase::Apply
            } else {
                OperationPhase::Preflight
            },
        )),
        SessionStorageOperationPhase::Available
        | SessionStorageOperationPhase::Preflight
        | SessionStorageOperationPhase::Backup
        | SessionStorageOperationPhase::BackupVerified
        | SessionStorageOperationPhase::PlanReady
        | SessionStorageOperationPhase::Applying
        | SessionStorageOperationPhase::Validating
        | SessionStorageOperationPhase::RollingBack => None,
    }
}

fn append_session_merge_history(
    operation_id: &str,
    status: OperationStatus,
    phase: OperationPhase,
    counts: BTreeMap<String, usize>,
) -> Result<(), String> {
    let data_root = appdata_root()?.join("codex-switch");
    let ledger = OperationLedgerStore::new(&data_root).load(operation_id)?;
    let backup_dirs = ledger.backup_root.into_iter().collect();
    operation_log()?.append(&OperationRecord {
        operation_id: operation_id.to_string(),
        action: OperationAction::SyncSessions,
        status,
        phase,
        started_at_ms: ledger.started_at_ms,
        completed_at_ms: timestamp_millis()?,
        backup_dirs,
        counts,
    })
}

fn record_prepared_migration_in_ledger(
    store: &OperationLedgerStore,
    operation_id: &str,
    prepared: &crate::session_storage::migration_apply::PreparedMigrationApply,
) -> Result<(), String> {
    store
        .update(operation_id, |ledger| {
            let mut known_paths = ledger
                .created_files
                .iter()
                .map(|file| path_key(&file.path))
                .collect::<BTreeSet<_>>();
            for file in &prepared.created_files {
                if known_paths.insert(path_key(&file.path)) {
                    ledger.created_files.push(file.clone());
                }
            }
            ledger.rollback_steps = prepared.rollback_steps.clone();
            ledger.last_error_code = None;
            Ok(())
        })
        .map(|_| ())
}

fn migration_persistent_session_bytes_added(plan: &MigrationApplyPlan) -> Result<u64, String> {
    let mut added = 0_u64;
    for session in &plan.sessions {
        let staged_bytes = match session.staged_path.as_ref() {
            Some(path) => fs::metadata(path)
                .map_err(|_| "prepared session merge file is unavailable".to_string())?
                .len(),
            None => 0,
        };
        let delta = match session.action {
            crate::session_storage::migration::MigrationSessionAction::CopyToCanonical => {
                staged_bytes
            }
            crate::session_storage::migration::MigrationSessionAction::ReplaceCanonicalWithExtension => {
                let before = fs::metadata(&session.target_path)
                    .map_err(|_| "session merge target is unavailable".to_string())?
                    .len();
                staged_bytes.saturating_sub(before)
            }
            crate::session_storage::migration::MigrationSessionAction::KeepCanonical
            | crate::session_storage::migration::MigrationSessionAction::Conflict => 0,
        };
        added = added
            .checked_add(delta)
            .ok_or_else(|| "session merge byte count overflowed".to_string())?;
    }
    Ok(added)
}

fn migration_backup_receipt_summary(
    backup: &MigrationBackupManifest,
    canonical_root: &Path,
    reason: &str,
) -> BackupReceiptSummary {
    BackupReceiptSummary {
        backup_dir: backup.backup_dir.clone(),
        source_root: canonical_root.to_path_buf(),
        reason: reason.to_string(),
        created_at_ms: backup.created_at_ms,
        scope: BackupScope::Full,
        tracked_database_count: backup
            .entries
            .iter()
            .filter(|entry| entry.kind == MigrationBackupEntryKind::Database)
            .count(),
        complete_sessions: true,
    }
}

fn session_sync_result_from_migration(
    report: &MigrationPreflightReport,
    receipt: &MigrationApplyReceipt,
    persistent_session_bytes_added: u64,
) -> SessionSyncResult {
    SessionSyncResult {
        inserted_threads: receipt.canonical_created_count,
        copied_session_files: receipt
            .canonical_created_count
            .saturating_add(receipt.canonical_replaced_count),
        duplicate_threads: report
            .plan
            .sessions
            .iter()
            .filter(|session| !session.duplicates.is_empty())
            .count(),
        skipped_missing_session_files: report.plan.missing_runtime_reference_count,
        skipped_archived_threads: 0,
        merged_session_index_entries: 0,
        persistent_session_bytes_added,
        persistent_session_bytes_reclaimed: 0,
        obsolete_provider_slots: Vec::new(),
        preserved_divergent_thread_ids: Default::default(),
    }
}

fn session_merge_warnings(report: &MigrationPreflightReport) -> Vec<String> {
    let mut warnings = Vec::new();
    if report.conflict_count > 0 {
        warnings.push(format!(
            "{} 个真实分叉未覆盖，已保留并进入冲突处理",
            report.conflict_count
        ));
    }
    if report.anomaly_count > 0 {
        warnings.push(format!(
            "{} 个非本次范围异常仅报告，未自动修复",
            report.anomaly_count
        ));
    }
    warnings
}

fn migration_required_merge_error() -> String {
    "会话合并与修复需要先完成 v0.3 会话迁移；旧“完全同步”已停用".to_string()
}

// The v0.2 provider-materializing implementation is compiled only for its
// compatibility fixtures while the one-time upgrader and downgrade exporter
// are built. It is not registered or reachable from the v0.3 daily command.
#[cfg(test)]
#[allow(dead_code)]
fn legacy_provider_full_sync_for_migration_fixture(
    on_progress: Channel<SessionSyncProgress>,
    diagnostic: Option<DiagnosticOperation>,
) -> Result<SessionSyncReceipt, String> {
    if !legacy_full_sync_available() {
        let error =
            "旧“完全同步”已停用；请先完成 v0.3 会话迁移，再使用“会话合并与修复”".to_string();
        emit_session_sync_progress_diagnostic(
            &on_progress,
            diagnostic.as_ref(),
            SessionSyncPhase::Failed,
            Some(error.clone()),
        );
        record_diagnostic_terminal(
            diagnostic.as_ref(),
            DiagnosticTerminalStatus::Blocked,
            "preflight",
            Some("commands.merge_and_repair_sessions.migration_required"),
            Some(&error),
        );
        return Err(error);
    }
    let _mutation_guard = match acquire_mutation_lock() {
        Ok(guard) => guard,
        Err(error) => {
            emit_session_sync_progress_diagnostic(
                &on_progress,
                diagnostic.as_ref(),
                SessionSyncPhase::Failed,
                Some(error.clone()),
            );
            record_diagnostic_terminal(
                diagnostic.as_ref(),
                diagnostic_status_for_command_error(&error),
                "preflight",
                Some("commands.merge_and_repair_sessions.mutation_lock"),
                Some(&error),
            );
            return Err(error);
        }
    };
    let started = match timestamp_millis() {
        Ok(started) => started,
        Err(error) => {
            emit_session_sync_progress_diagnostic(
                &on_progress,
                diagnostic.as_ref(),
                SessionSyncPhase::Failed,
                Some(error.clone()),
            );
            record_diagnostic_terminal(
                diagnostic.as_ref(),
                DiagnosticTerminalStatus::Failed,
                "preflight",
                Some("commands.merge_and_repair_sessions.timestamp"),
                Some(&error),
            );
            return Err(error);
        }
    };
    let operation_id = match operation_id("sync-sessions") {
        Ok(operation_id) => operation_id,
        Err(error) => {
            emit_session_sync_progress_diagnostic(
                &on_progress,
                diagnostic.as_ref(),
                SessionSyncPhase::Failed,
                Some(error.clone()),
            );
            record_diagnostic_terminal(
                diagnostic.as_ref(),
                DiagnosticTerminalStatus::Failed,
                "preflight",
                Some("commands.merge_and_repair_sessions.operation_id"),
                Some(&error),
            );
            return Err(error);
        }
    };
    let mut backups = Vec::new();
    let mut failure_status = None;
    let mut failure_phase = OperationPhase::Preflight;
    let mut checkpoint_root = None;
    let mut launch_target_captured = false;
    let mut process_gate_entered = false;
    emit_session_sync_progress_diagnostic(
        &on_progress,
        diagnostic.as_ref(),
        SessionSyncPhase::Preparing,
        None,
    );
    let mut result = (|| {
        let backup_root = default_backup_root()?;
        checkpoint_root = Some(backup_root.clone());
        let shared_home = default_shared_sessions_root()?;
        let current_home = managed_codex_home()?;
        let current_paths = resolve_user_codex_paths(&current_home)?;
        let shared_paths = local_codex_paths(&shared_home);
        let processes =
            list_managed_processes_for_closed_mutation("completely syncing active sessions")?;
        capture_chatgpt_launch_target_once(&mut launch_target_captured, || {
            let _ = cache_chatgpt_launch_target();
        });
        if !processes.is_empty() {
            emit_session_sync_progress_diagnostic(
                &on_progress,
                diagnostic.as_ref(),
                SessionSyncPhase::ClosingApp,
                Some(format!("Closing {} ChatGPT process(es)", processes.len())),
            );
            close_codex()?;
        }
        ensure_codex_closed("completely syncing active sessions")?;
        process_gate_entered = true;
        emit_session_sync_progress_diagnostic(
            &on_progress,
            diagnostic.as_ref(),
            SessionSyncPhase::BackingUp,
            None,
        );
        let current_backup = after_capacity_preflight(
            || {
                preflight_backup_capacity_for_sources(
                    &backup_root,
                    &[
                        BackupCapacitySource {
                            home: &current_home,
                            paths: &current_paths,
                            scope: BackupScope::StateOnly,
                        },
                        BackupCapacitySource {
                            home: &shared_home,
                            paths: &shared_paths,
                            scope: BackupScope::StateOnly,
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
                    &operation_id,
                    CheckpointRole::Current,
                )
            },
        )?;
        backups.push(current_backup.clone());
        let shared_backup = create_hot_sync_backup_with_paths(
            &shared_home,
            &backup_root,
            "sync-shared",
            shared_paths.clone(),
            &operation_id,
            CheckpointRole::Shared,
        )?;
        backups.push(shared_backup.clone());
        ensure_codex_paths_unchanged("session sync", &current_home, &current_paths)?;
        ensure_codex_closed("completely syncing active sessions")?;
        failure_phase = OperationPhase::Apply;
        emit_session_sync_progress_diagnostic(
            &on_progress,
            diagnostic.as_ref(),
            SessionSyncPhase::Reconciling,
            None,
        );
        match sync_home_with_shared_complete_with_paths(&current_paths, &shared_paths) {
            Ok(sync_result) => Ok(SessionSyncReceipt {
                operation_id: operation_id.clone(),
                backups: backups.iter().map(BackupReceiptSummary::from).collect(),
                result: sync_result,
                rolled_back: false,
                warnings: Vec::new(),
                checkpoint_cleanup: CheckpointCleanupSummary::default(),
                chatgpt_launch: ChatGptLaunchReceipt::not_requested(),
            }),
            Err(error) => {
                failure_phase = OperationPhase::Rollback;
                let shared_rolled_back =
                    restore_backup_for_test(&shared_backup.backup_dir, &shared_home).is_ok();
                let current_rolled_back =
                    restore_backup_for_test(&current_backup.backup_dir, &current_home).is_ok();
                if shared_rolled_back && current_rolled_back {
                    failure_status = Some(OperationStatus::RolledBack);
                    Err(format!(
                        "session sync failed: {error}; restored both database states; immutable session JSONL additions may remain for retry"
                    ))
                } else {
                    failure_status = Some(OperationStatus::RollbackFailed);
                    Err(format!(
                        "session sync failed: {error}; database rollback failed; keep ChatGPT closed and use the verified checkpoints"
                    ))
                }
            }
        }
    })();
    let durable_terminal_recorded;
    match &mut result {
        Ok(receipt) => {
            if let (Ok(current_home), Ok(shared_home), Ok(index_path)) = (
                managed_codex_home(),
                default_shared_sessions_root(),
                default_session_sync_index_path(),
            ) {
                if let Ok(current_paths) = resolve_user_codex_paths(&current_home) {
                    let shared_paths = local_codex_paths(&shared_home);
                    if save_session_sync_index(&index_path, &current_paths, &shared_paths).is_err()
                    {
                        receipt.warnings.push(
                            "完全同步已完成，但增量索引写入失败；下次切换会要求再次完全同步"
                                .to_string(),
                        );
                    }
                } else {
                    receipt
                        .warnings
                        .push("完全同步已完成，但增量索引路径无法复核".to_string());
                }
            } else {
                receipt
                    .warnings
                    .push("完全同步已完成，但增量索引路径无法复核".to_string());
            }
            emit_session_sync_progress_diagnostic(
                &on_progress,
                diagnostic.as_ref(),
                SessionSyncPhase::RecordingResult,
                None,
            );
            let terminal_record = match record_success_result_with_diagnostic(
                &operation_id,
                OperationAction::SyncSessions,
                started,
                &backups,
                sync_counts(&receipt.result),
                diagnostic.as_ref(),
            ) {
                Ok(record) => Some(record),
                Err(error) => {
                    if let Some(diagnostic) = diagnostic.as_ref() {
                        let _ = diagnostic.branch(
                            DiagnosticLevel::Error,
                            Some("recordingResult"),
                            Some("commands.merge_and_repair_sessions.operation_log"),
                            Some(&error),
                            empty_context(),
                        );
                    }
                    None
                }
            };
            let terminal_recorded = terminal_record.is_some();
            durable_terminal_recorded = terminal_recorded;
            let cleanup = release_transient_checkpoints(
                checkpoint_root.as_deref(),
                terminal_record.as_ref(),
                backups.as_slice(),
            );
            receipt.warnings.extend(cleanup.warnings.clone());
            receipt.checkpoint_cleanup = cleanup;
            if terminal_recorded && !receipt.result.obsolete_provider_slots.is_empty() {
                match (managed_codex_home(), default_shared_sessions_root()) {
                    (Ok(current_home), Ok(shared_home)) => {
                        match resolve_user_codex_paths(&current_home) {
                            Ok(current_paths) => {
                                let shared_paths = local_codex_paths(&shared_home);
                                let gc = cleanup_obsolete_provider_slots(
                                    &receipt.result.obsolete_provider_slots,
                                    &current_paths,
                                    &shared_paths,
                                );
                                receipt.result.persistent_session_bytes_reclaimed = receipt
                                    .result
                                    .persistent_session_bytes_reclaimed
                                    .saturating_add(gc.reclaimed_bytes);
                                receipt.warnings.extend(gc.warnings);
                            }
                            Err(_) => receipt
                                .warnings
                                .push("会话槽位清理路径无法复核，旧槽位已保留".to_string()),
                        }
                    }
                    _ => receipt
                        .warnings
                        .push("会话槽位清理路径无法复核，旧槽位已保留".to_string()),
                }
            }
            emit_session_sync_progress_diagnostic(
                &on_progress,
                diagnostic.as_ref(),
                SessionSyncPhase::LaunchingApp,
                None,
            );
            receipt.chatgpt_launch =
                launch_chatgpt_after_durable_terminal(terminal_recorded, || {
                    ChatGptLaunchReceipt::from(launch_cached_chatgpt())
                });
            emit_session_sync_progress_diagnostic(
                &on_progress,
                diagnostic.as_ref(),
                SessionSyncPhase::Complete,
                None,
            );
        }
        Err(error) => {
            let status = failure_status.unwrap_or_else(|| terminal_status(error));
            let terminal_record = match record_sync_failure_with_diagnostic(
                &operation_id,
                status,
                failure_phase,
                started,
                &backups,
                diagnostic.as_ref(),
                Some(error),
            ) {
                Ok(record) => Some(record),
                Err(log_error) => {
                    if let Some(diagnostic) = diagnostic.as_ref() {
                        let _ = diagnostic.branch(
                            DiagnosticLevel::Error,
                            Some("recordingResult"),
                            Some("commands.merge_and_repair_sessions.operation_log"),
                            Some(&log_error),
                            empty_context(),
                        );
                    }
                    None
                }
            };
            durable_terminal_recorded = terminal_record.is_some();
            if !backups.is_empty()
                && (failure_phase == OperationPhase::Backup
                    || status == OperationStatus::RolledBack)
            {
                let cleanup = release_transient_checkpoints(
                    checkpoint_root.as_deref(),
                    terminal_record.as_ref(),
                    backups.as_slice(),
                );
                append_warnings_to_error(error, &cleanup.warnings);
            }
            if process_gate_entered && status != OperationStatus::RollbackFailed {
                emit_session_sync_progress_diagnostic(
                    &on_progress,
                    diagnostic.as_ref(),
                    SessionSyncPhase::LaunchingApp,
                    None,
                );
                let launch =
                    launch_chatgpt_after_durable_terminal(terminal_record.is_some(), || {
                        ChatGptLaunchReceipt::from(launch_cached_chatgpt())
                    });
                if launch.status == ChatGptLaunchStatus::Failed {
                    append_warnings_to_error(
                        error,
                        &["ChatGPT could not be reopened after session sync rollback".to_string()],
                    );
                }
            }
            emit_session_sync_progress_diagnostic(
                &on_progress,
                diagnostic.as_ref(),
                SessionSyncPhase::Failed,
                Some(error.clone()),
            );
        }
    }
    if diagnostic
        .as_ref()
        .is_some_and(|diagnostic| !diagnostic.is_terminal_recorded())
    {
        match &result {
            Ok(receipt) if durable_terminal_recorded => {
                record_session_sync_success_diagnostic(diagnostic.as_ref(), receipt)
            }
            Ok(_) => record_diagnostic_terminal(
                diagnostic.as_ref(),
                DiagnosticTerminalStatus::Partial,
                "recordingResult",
                Some("commands.merge_and_repair_sessions.operation_log"),
                Some("session sync completed, but the durable operation terminal was unavailable"),
            ),
            Err(error) => {
                let status = failure_status.unwrap_or_else(|| terminal_status(error));
                record_diagnostic_terminal(
                    diagnostic.as_ref(),
                    diagnostic_terminal_status(status),
                    operation_phase_name(failure_phase),
                    Some("commands.merge_and_repair_sessions.failed"),
                    Some(error),
                )
            }
        }
    }
    result
}

#[cfg(test)]
fn legacy_full_sync_available() -> bool {
    false
}

fn emit_session_sync_progress_diagnostic(
    on_progress: &Channel<SessionSyncProgress>,
    diagnostic: Option<&DiagnosticOperation>,
    phase: SessionSyncPhase,
    message: Option<String>,
) {
    record_diagnostic_phase(diagnostic, session_sync_phase_name(phase));
    let _ = on_progress.send(SessionSyncProgress {
        phase,
        timestamp_ms: timestamp_millis().unwrap_or_default(),
        operation_id: diagnostic_correlation_id(diagnostic),
        message,
    });
}

fn session_sync_phase_name(phase: SessionSyncPhase) -> &'static str {
    match phase {
        SessionSyncPhase::Preparing => "preparing",
        SessionSyncPhase::ClosingApp => "closingApp",
        SessionSyncPhase::BackingUp => "backingUp",
        SessionSyncPhase::Reconciling => "reconciling",
        SessionSyncPhase::RecordingResult => "recordingResult",
        SessionSyncPhase::LaunchingApp => "launchingApp",
        SessionSyncPhase::Complete => "complete",
        SessionSyncPhase::Failed => "failed",
    }
}

#[tauri::command]
pub fn restore_sessions_visible(ids: Vec<String>) -> Result<SessionMutationReceipt, String> {
    let diagnostic = begin_command_diagnostic("restoreVisibility");
    record_diagnostic_phase(diagnostic.as_ref(), "preflight");
    let result = (|| {
        let _mutation_guard = acquire_mutation_lock()?;
        let started = timestamp_millis()?;
        let operation_id = operation_id("restore-visibility")?;
        record_diagnostic_phase(diagnostic.as_ref(), "apply");
        let mut failure_backups = Vec::new();
        let mut checkpoint_root = None;
        let result = (|| {
            let backup_root = default_backup_root()?;
            checkpoint_root = Some(backup_root.clone());
            let current_home = managed_codex_home()?;
            match restore_visible(&current_home, &backup_root, &ids, &operation_id, || {
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
            diagnostic.as_ref(),
        )
    })();
    let partial = result.as_ref().is_ok_and(|receipt| {
        receipt.checkpoint_cleanup.failed_count > 0
            || diagnostic
                .as_ref()
                .is_some_and(|operation| operation.operation_id().is_none())
    });
    record_durable_command_result(
        diagnostic.as_ref(),
        &result,
        if partial {
            DiagnosticTerminalStatus::Partial
        } else {
            DiagnosticTerminalStatus::Succeeded
        },
        "complete",
        partial.then_some("commands.restore_visibility.partial"),
        partial.then_some("session visibility was restored with incomplete checkpoint cleanup or operation history"),
        "apply",
        "commands.restore_visibility.failed",
    );
    let _ = schedule_shadow_scan_after_switch(
        result.is_ok(),
        || {
            Some((
                managed_codex_home().ok()?,
                appdata_root().ok()?.join("codex-switch"),
            ))
        },
        request_shadow_and_automatic_gc,
    );
    correlate_mutation_result(result, diagnostic.as_ref())
}

fn load_session_merge_backup_destination(
    data_root: &Path,
    canonical_root: &Path,
) -> Result<PathBuf, String> {
    load_committed_canonical_storage_state(data_root, canonical_root)?
        .map(|state| state.backup_destination)
        .ok_or_else(migration_required_merge_error)
}

#[tauri::command]
pub async fn list_backups() -> Result<Vec<BackupSummary>, String> {
    let result = match tauri::async_runtime::spawn_blocking(list_backups_blocking).await {
        Ok(result) => result,
        Err(_) => Err("backup list worker failed".to_string()),
    };
    record_background_result("listBackups", "commands.list_backups.failed", &result);
    result
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
    let result =
        match tauri::async_runtime::spawn_blocking(inspect_checkpoint_storage_blocking).await {
            Ok(result) => result,
            Err(_) => Err("checkpoint storage worker failed".to_string()),
        };
    record_background_result(
        "inspectCheckpointStorage",
        "commands.inspect_checkpoint_storage.failed",
        &result,
    );
    result
}

fn inspect_checkpoint_storage_blocking() -> Result<CheckpointStorageStatus, String> {
    let _mutation_guard = acquire_mutation_lock()?;
    let records = operation_log()?.list_all_strict()?;
    inspect_checkpoint_storage_at(&default_backup_root()?, &records)
}

#[tauri::command]
pub async fn cleanup_automatic_checkpoints() -> Result<CheckpointCleanupReceipt, String> {
    let diagnostic = begin_command_diagnostic("cleanupCheckpoints");
    record_diagnostic_phase(diagnostic.as_ref(), "preflight");
    let worker_diagnostic = diagnostic.clone();
    let result = match tauri::async_runtime::spawn_blocking(move || {
        cleanup_automatic_checkpoints_blocking(worker_diagnostic.as_ref())
    })
    .await
    {
        Ok(result) => result,
        Err(_) => {
            let error = "checkpoint cleanup worker failed".to_string();
            record_diagnostic_terminal(
                diagnostic.as_ref(),
                DiagnosticTerminalStatus::Failed,
                "workerJoin",
                Some("commands.cleanup_checkpoints.worker_join_failed"),
                Some(&error),
            );
            Err(error)
        }
    };
    correlate_mutation_result(result, diagnostic.as_ref())
}

fn cleanup_automatic_checkpoints_blocking(
    diagnostic: Option<&DiagnosticOperation>,
) -> Result<CheckpointCleanupReceipt, String> {
    let mut durable_recorded = false;
    let result = (|| {
        let _mutation_guard = acquire_mutation_lock()?;
        record_diagnostic_phase(diagnostic, "apply");
        let started = timestamp_millis()?;
        let operation_id = operation_id("cleanup-checkpoints")?;
        let log = operation_log().inspect_err(|error| {
            record_diagnostic_branch(
                diagnostic,
                "recordingResult",
                "commands.operation_history.open_failed",
                error,
            );
        })?;
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
        durable_recorded = append_operation_record_receipt_to_with_diagnostic(
            &log,
            &operation_id,
            OperationAction::CleanupCheckpoints,
            terminal,
            started,
            &[],
            counts,
            diagnostic,
            None,
        )
        .is_ok();
        if !durable_recorded {
            receipt.warnings.push(
                "checkpoint cleanup completed, but local operation history could not be written"
                    .to_string(),
            );
        }
        Ok(receipt)
    })();
    let success_status = result
        .as_ref()
        .map(|receipt| checkpoint_cleanup_diagnostic_status(receipt, durable_recorded))
        .unwrap_or(DiagnosticTerminalStatus::Succeeded);
    let partial = success_status == DiagnosticTerminalStatus::Partial;
    record_durable_command_result(
        diagnostic,
        &result,
        success_status,
        "complete",
        partial.then_some("commands.cleanup_checkpoints.partial"),
        partial.then_some(
            "checkpoint cleanup completed with failed items or missing operation history",
        ),
        "apply",
        "commands.cleanup_checkpoints.failed",
    );
    result
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

fn checkpoint_cleanup_diagnostic_status(
    receipt: &CheckpointCleanupReceipt,
    durable_recorded: bool,
) -> DiagnosticTerminalStatus {
    if receipt.failed_count > 0 || !durable_recorded {
        DiagnosticTerminalStatus::Partial
    } else {
        DiagnosticTerminalStatus::Succeeded
    }
}

#[tauri::command]
pub async fn create_full_backup() -> Result<CreateFullBackupReceipt, String> {
    let diagnostic = begin_command_diagnostic("createBackup");
    record_diagnostic_phase(diagnostic.as_ref(), "preflight");
    let worker_diagnostic = diagnostic.clone();
    let result = match tauri::async_runtime::spawn_blocking(move || {
        let result = create_full_backup_blocking(worker_diagnostic.as_ref());
        let partial = result
            .as_ref()
            .is_ok_and(|receipt| !receipt.warnings.is_empty());
        record_durable_command_result(
            worker_diagnostic.as_ref(),
            &result,
            if partial {
                DiagnosticTerminalStatus::Partial
            } else {
                DiagnosticTerminalStatus::Succeeded
            },
            "complete",
            partial.then_some("commands.create_backup.operation_history_failed"),
            partial.then_some("backup completed without durable operation history"),
            "backup",
            "commands.create_backup.failed",
        );
        result
    })
    .await
    {
        Ok(result) => result,
        Err(_) => {
            let error = "full backup worker failed".to_string();
            record_diagnostic_terminal(
                diagnostic.as_ref(),
                DiagnosticTerminalStatus::Failed,
                "workerJoin",
                Some("commands.create_backup.worker_join_failed"),
                Some(&error),
            );
            Err(error)
        }
    };
    correlate_mutation_result(result, diagnostic.as_ref())
}

fn create_full_backup_blocking(
    diagnostic: Option<&DiagnosticOperation>,
) -> Result<CreateFullBackupReceipt, String> {
    let _mutation_guard = acquire_mutation_lock()?;
    record_diagnostic_phase(diagnostic, "backup");
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
            receipt.warnings = record_success_result_with_diagnostic(
                &operation_id,
                OperationAction::CreateBackup,
                started,
                &backups,
                BTreeMap::from([(
                    "backupFiles".to_string(),
                    backups.iter().map(|backup| backup.files.len()).sum(),
                )]),
                diagnostic,
            )
            .err()
            .map(|_| "操作已成功，但本地操作记录写入失败".to_string())
            .into_iter()
            .collect();
        }
        Err(error) => {
            if append_operation_record_with_phase_and_diagnostic(
                &operation_id,
                OperationAction::CreateBackup,
                terminal_status(error),
                failure_phase,
                started,
                &backups,
                BTreeMap::new(),
                diagnostic,
                Some(error),
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
    let diagnostic = begin_command_diagnostic("deleteBackup");
    record_diagnostic_phase(diagnostic.as_ref(), "preflight");
    let worker_diagnostic = diagnostic.clone();
    let result = match tauri::async_runtime::spawn_blocking(move || {
        let result = delete_backup_blocking(backup_dir, confirmed, worker_diagnostic.as_ref());
        let partial = result
            .as_ref()
            .is_ok_and(|receipt| !receipt.warnings.is_empty());
        record_durable_command_result(
            worker_diagnostic.as_ref(),
            &result,
            if partial {
                DiagnosticTerminalStatus::Partial
            } else {
                DiagnosticTerminalStatus::Succeeded
            },
            "complete",
            partial.then_some("commands.delete_backup.operation_history_failed"),
            partial.then_some("backup deletion completed without durable operation history"),
            "apply",
            "commands.delete_backup.failed",
        );
        result
    })
    .await
    {
        Ok(result) => result,
        Err(_) => {
            let error = "backup deletion worker failed".to_string();
            record_diagnostic_terminal(
                diagnostic.as_ref(),
                DiagnosticTerminalStatus::Failed,
                "workerJoin",
                Some("commands.delete_backup.worker_join_failed"),
                Some(&error),
            );
            Err(error)
        }
    };
    correlate_mutation_result(result, diagnostic.as_ref())
}

fn delete_backup_blocking(
    backup_dir: PathBuf,
    confirmed: bool,
    diagnostic: Option<&DiagnosticOperation>,
) -> Result<BackupDeleteReceipt, String> {
    let _mutation_guard = acquire_mutation_lock()?;
    record_diagnostic_phase(diagnostic, "apply");
    let backup_root = default_backup_root()?;
    delete_backup_at_with_diagnostic(
        &backup_root,
        operation_log(),
        backup_dir,
        confirmed,
        diagnostic,
    )
}

#[cfg(test)]
fn delete_backup_at(
    backup_root: &Path,
    log: Result<OperationLog, String>,
    backup_dir: PathBuf,
    confirmed: bool,
) -> Result<BackupDeleteReceipt, String> {
    delete_backup_at_with_diagnostic(backup_root, log, backup_dir, confirmed, None)
}

fn delete_backup_at_with_diagnostic(
    backup_root: &Path,
    log: Result<OperationLog, String>,
    backup_dir: PathBuf,
    confirmed: bool,
    diagnostic: Option<&DiagnosticOperation>,
) -> Result<BackupDeleteReceipt, String> {
    if let Err(error) = log.as_ref() {
        record_diagnostic_branch(
            diagnostic,
            "recordingResult",
            "commands.operation_history.open_failed",
            error,
        );
    }
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
                diagnostic,
                Some("backup deletion requires explicit confirmation"),
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
                        diagnostic,
                        None,
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
                    diagnostic,
                    Some(&error),
                );
            }
            Err(error)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn append_delete_backup_record(
    log: &OperationLog,
    operation_id: &str,
    status: OperationStatus,
    phase: OperationPhase,
    started_at_ms: u128,
    backup_dirs: Vec<PathBuf>,
    counts: BTreeMap<String, usize>,
    diagnostic: Option<&DiagnosticOperation>,
    diagnostic_error: Option<&str>,
) -> Result<(), String> {
    let record = OperationRecord {
        operation_id: operation_id.to_string(),
        action: OperationAction::DeleteBackup,
        status,
        phase,
        started_at_ms,
        completed_at_ms: timestamp_millis()?,
        backup_dirs,
        counts,
    };
    append_durable_operation_record(log, &record, diagnostic, diagnostic_error)
}

#[tauri::command]
pub fn restore_backup(backup_dir: String) -> Result<RestoreBackupReceipt, String> {
    let diagnostic = begin_command_diagnostic("restoreBackup");
    record_diagnostic_phase(diagnostic.as_ref(), "preflight");
    let result = (|| {
        let _mutation_guard = acquire_mutation_lock()?;
        let started = timestamp_millis()?;
        let operation_id = operation_id("restore-backup")?;
        let mut backups = Vec::new();
        let mut failure_status = None;
        let mut failure_phase = OperationPhase::Preflight;
        let mut result = (|| {
            let data_root = appdata_root()?.join("codex-switch");
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
                    record_diagnostic_phase(diagnostic.as_ref(), "backup");
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
            record_diagnostic_phase(diagnostic.as_ref(), "apply");
            match restore_backup_snapshot(&selected, &target, &data_root, &operation_id) {
                Ok(restore_result) => Ok(RestoreBackupReceipt {
                    operation_id: operation_id.clone(),
                    result: restore_result,
                    safety_backup: BackupReceiptSummary::from(&safety_backup),
                    rolled_back: false,
                    warnings: Vec::new(),
                }),
                Err(error) => {
                    failure_phase = OperationPhase::Rollback;
                    record_diagnostic_phase(diagnostic.as_ref(), "rollback");
                    let rollback_pending = error.contains("rollback remains pending");
                    failure_status = Some(if rollback_pending {
                        OperationStatus::RollbackFailed
                    } else {
                        OperationStatus::RolledBack
                    });
                    Err(format!("backup restore failed: {error}"))
                }
            }
        })();
        match &mut result {
            Ok(receipt) => {
                receipt.warnings = record_success_result_with_diagnostic(
                    &operation_id,
                    OperationAction::RestoreBackup,
                    started,
                    &backups,
                    BTreeMap::from([("restoredFiles".to_string(), receipt.result.restored_files)]),
                    diagnostic.as_ref(),
                )
                .err()
                .map(|_| "操作已成功，但本地操作记录写入失败".to_string())
                .into_iter()
                .collect();
            }
            Err(error) => {
                let _ = append_operation_record_with_phase_and_diagnostic(
                    &operation_id,
                    OperationAction::RestoreBackup,
                    failure_status.unwrap_or_else(|| terminal_status(error)),
                    failure_phase,
                    started,
                    &backups,
                    BTreeMap::new(),
                    diagnostic.as_ref(),
                    Some(error),
                );
            }
        }
        result
    })();
    let partial = result
        .as_ref()
        .is_ok_and(|receipt| !receipt.result.verified || !receipt.warnings.is_empty());
    record_durable_command_result(
        diagnostic.as_ref(),
        &result,
        if partial {
            DiagnosticTerminalStatus::Partial
        } else {
            DiagnosticTerminalStatus::Succeeded
        },
        "complete",
        partial.then_some("commands.restore_backup.partial"),
        partial
            .then_some("backup restore completed without full verification or operation history"),
        "apply",
        "commands.restore_backup.failed",
    );
    let _ = schedule_shadow_scan_after_switch(
        result.is_ok(),
        || {
            Some((
                managed_codex_home().ok()?,
                appdata_root().ok()?.join("codex-switch"),
            ))
        },
        request_shadow_and_automatic_gc,
    );
    correlate_mutation_result(result, diagnostic.as_ref())
}

#[tauri::command]
pub fn list_operation_records(limit: Option<usize>) -> Result<Vec<OperationRecord>, String> {
    let result = (|| operation_log()?.list(limit.unwrap_or(100).min(1_000)))();
    record_background_result(
        "listOperationRecords",
        "commands.list_operation_records.failed",
        &result,
    );
    result
}

#[tauri::command]
pub fn list_skills() -> Result<Vec<SkillStatus>, String> {
    let result = (|| list_skills_at(&skill_codex_home()?, &appdata_root()?))();
    record_background_result("listSkills", "commands.list_skills.failed", &result);
    result
}

#[tauri::command]
pub fn install_skill(
    skill_id: SkillId,
    confirm_replace: bool,
) -> Result<SkillMutationReceipt, String> {
    let diagnostic = begin_command_diagnostic("installSkill");
    record_diagnostic_phase(diagnostic.as_ref(), "preflight");
    let result = (|| {
        let _mutation_guard = acquire_mutation_lock()?;
        let started_at_ms = timestamp_millis()?;
        let attempt_id = operation_id("install-skill-attempt")?;
        record_diagnostic_phase(diagnostic.as_ref(), "apply");
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
            diagnostic.as_ref(),
        )
    })();
    let partial = result
        .as_ref()
        .is_ok_and(|receipt| !receipt.warnings.is_empty());
    record_durable_command_result(
        diagnostic.as_ref(),
        &result,
        if partial {
            DiagnosticTerminalStatus::Partial
        } else {
            DiagnosticTerminalStatus::Succeeded
        },
        "complete",
        partial.then_some("commands.install_skill.partial"),
        partial
            .then_some("skill installation completed with cleanup or operation-history warnings"),
        "apply",
        "commands.install_skill.failed",
    );
    correlate_mutation_result(result, diagnostic.as_ref())
}

#[tauri::command]
pub fn save_skill_config(input: SkillConfigInput) -> Result<SkillMutationReceipt, String> {
    let diagnostic = begin_command_diagnostic("configureSkill");
    record_diagnostic_phase(diagnostic.as_ref(), "preflight");
    let result = (|| {
        let _mutation_guard = acquire_mutation_lock()?;
        let started_at_ms = timestamp_millis()?;
        let attempt_id = operation_id("configure-skill-attempt")?;
        record_diagnostic_phase(diagnostic.as_ref(), "apply");
        let result = (|| {
            ensure_codex_closed("configuring a skill")?;
            save_skill_config_at(&skill_codex_home()?, &appdata_root()?, input)
        })();
        finish_skill_operation(
            attempt_id,
            OperationAction::ConfigureSkill,
            started_at_ms,
            result,
            diagnostic.as_ref(),
        )
    })();
    let partial = result
        .as_ref()
        .is_ok_and(|receipt| !receipt.warnings.is_empty());
    record_durable_command_result(
        diagnostic.as_ref(),
        &result,
        if partial {
            DiagnosticTerminalStatus::Partial
        } else {
            DiagnosticTerminalStatus::Succeeded
        },
        "complete",
        partial.then_some("commands.configure_skill.partial"),
        partial
            .then_some("skill configuration completed with cleanup or operation-history warnings"),
        "apply",
        "commands.configure_skill.failed",
    );
    correlate_mutation_result(result, diagnostic.as_ref())
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

#[cfg(test)]
fn create_hot_sync_backup_with_paths(
    home: &Path,
    backup_root: &Path,
    reason: &str,
    paths: CodexPaths,
    operation_id: &str,
    role: CheckpointRole,
) -> Result<BackupManifest, String> {
    create_state_checkpoint_with_paths(home, backup_root, reason, paths, operation_id, role)
}

fn release_transient_checkpoints(
    backup_root: Option<&Path>,
    terminal_record: Option<&OperationRecord>,
    backups: &[BackupManifest],
) -> CheckpointCleanupSummary {
    if backups.is_empty() {
        return CheckpointCleanupSummary::default();
    }
    let Some(terminal_record) = terminal_record else {
        return CheckpointCleanupSummary {
            retained_count: backups.len(),
            warnings: vec!["终态记录未能持久化；临时检查点已保留".to_string()],
            ..CheckpointCleanupSummary::default()
        };
    };
    match backup_root {
        Some(root) => cleanup_transient_checkpoints(root, terminal_record, backups),
        None => CheckpointCleanupSummary {
            retained_count: backups.len(),
            warnings: vec![
                "automatic checkpoints could not be resolved and were retained".to_string(),
            ],
            ..CheckpointCleanupSummary::default()
        },
    }
}

#[cfg(test)]
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

#[cfg(test)]
fn default_session_sync_index_path() -> Result<PathBuf, String> {
    Ok(appdata_root()?
        .join("codex-switch")
        .join("session-sync-state-v1.json"))
}

fn default_mobile_continuity_state_path() -> Result<PathBuf, String> {
    mobile_continuity::default_state_path()
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

fn prepare_app_exit_at(
    coordinator: &MutationCoordinator,
    lock_path: &Path,
) -> Result<bool, String> {
    match coordinator.acquire(lock_path) {
        Ok(guard) => {
            guard.hold_until_process_exit();
            Ok(true)
        }
        Err(error) if error == mutation_busy_error() => Ok(false),
        Err(error) => Err(error),
    }
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
    let (managed, standalone) = list_process_inventory()?;
    ensure_codex_closed_from_processes(action, managed, standalone)
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
    let (managed, standalone) = list_process_inventory()?;
    if standalone.is_empty() {
        Ok(managed)
    } else {
        Err(format!(
            "a standalone Codex CLI is still running; close it before {action}"
        ))
    }
}

fn ensure_no_codex_writer_for_session_storage(action: &str) -> Result<(), String> {
    let active = list_managed_processes_for_closed_mutation(action)?;
    if active.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "session storage mutation requires every Codex writer to be closed; activeProcesses={}",
            active.len()
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
    diagnostic: Option<&DiagnosticOperation>,
) -> Result<SessionMutationReceipt, String> {
    finish_session_mutation_with_log_and_diagnostic(
        operation_log(),
        operation_id,
        action,
        started_at_ms,
        result,
        failure_backups,
        checkpoint_root,
        diagnostic,
    )
}

#[cfg(test)]
fn finish_session_mutation_with_log(
    log: Result<OperationLog, String>,
    operation_id: String,
    action: OperationAction,
    started_at_ms: u128,
    result: Result<SessionMutationResult, String>,
    failure_backups: &[BackupManifest],
    checkpoint_root: Option<&Path>,
) -> Result<SessionMutationReceipt, String> {
    finish_session_mutation_with_log_and_diagnostic(
        log,
        operation_id,
        action,
        started_at_ms,
        result,
        failure_backups,
        checkpoint_root,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn finish_session_mutation_with_log_and_diagnostic(
    log: Result<OperationLog, String>,
    operation_id: String,
    action: OperationAction,
    started_at_ms: u128,
    result: Result<SessionMutationResult, String>,
    failure_backups: &[BackupManifest],
    checkpoint_root: Option<&Path>,
    diagnostic: Option<&DiagnosticOperation>,
) -> Result<SessionMutationReceipt, String> {
    match result {
        Ok(result) => {
            let terminal_record = log
                .as_ref()
                .map_err(Clone::clone)
                .and_then(|log| {
                    append_operation_record_receipt_to_with_diagnostic(
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
                        diagnostic,
                        None,
                    )
                })
                .ok();
            let terminal_recorded = terminal_record.is_some();
            let mut warnings = Vec::new();
            let checkpoint_cleanup = if action == OperationAction::RestoreVisibility {
                let cleanup = release_transient_checkpoints(
                    checkpoint_root,
                    terminal_record.as_ref(),
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
                let _ = append_operation_record_receipt_to_with_diagnostic(
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
                    diagnostic,
                    Some(&error),
                );
            }
            Err(error)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn record_result_with_diagnostic<T>(
    operation_id: &str,
    action: OperationAction,
    started_at_ms: u128,
    result: &Result<T, String>,
    backups: &[BackupManifest],
    counts: BTreeMap<String, usize>,
    diagnostic: Option<&DiagnosticOperation>,
) -> Result<(), String> {
    let log = operation_log().inspect_err(|error| {
        record_diagnostic_branch(
            diagnostic,
            "recordingResult",
            "commands.operation_history.open_failed",
            error,
        );
    })?;
    let status = match result {
        Ok(_) => OperationStatus::Succeeded,
        Err(error) => terminal_status(error),
    };
    let phase = operation_phase(&action, &status);
    append_operation_record_receipt_to_with_diagnostic(
        &log,
        operation_id,
        action,
        OperationTerminal { status, phase },
        started_at_ms,
        backups,
        counts,
        diagnostic,
        result.as_ref().err().map(String::as_str),
    )
    .map(|_| ())
}

fn record_runtime_switch_result_with_diagnostic<T>(
    operation_id: &str,
    started_at_ms: u128,
    result: &Result<T, String>,
    backups: &[BackupManifest],
    counts: BTreeMap<String, usize>,
    failure_outcome: RuntimeSwitchOutcome,
    diagnostic: Option<&DiagnosticOperation>,
) -> Result<OperationRecord, String> {
    record_runtime_switch_result_to_log_with_diagnostic(
        &operation_log()?,
        operation_id,
        started_at_ms,
        result,
        backups,
        counts,
        failure_outcome,
        diagnostic,
    )
}

#[cfg(test)]
fn record_runtime_switch_result_to_log<T>(
    log: &OperationLog,
    operation_id: &str,
    started_at_ms: u128,
    result: &Result<T, String>,
    backups: &[BackupManifest],
    counts: BTreeMap<String, usize>,
    failure_outcome: RuntimeSwitchOutcome,
) -> Result<OperationRecord, String> {
    record_runtime_switch_result_to_log_with_diagnostic(
        log,
        operation_id,
        started_at_ms,
        result,
        backups,
        counts,
        failure_outcome,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn record_runtime_switch_result_to_log_with_diagnostic<T>(
    log: &OperationLog,
    operation_id: &str,
    started_at_ms: u128,
    result: &Result<T, String>,
    backups: &[BackupManifest],
    counts: BTreeMap<String, usize>,
    failure_outcome: RuntimeSwitchOutcome,
    diagnostic: Option<&DiagnosticOperation>,
) -> Result<OperationRecord, String> {
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
    append_operation_record_receipt_to_with_diagnostic(
        log,
        operation_id,
        OperationAction::SwitchRuntime,
        terminal,
        started_at_ms,
        backups,
        counts,
        diagnostic,
        result.as_ref().err().map(String::as_str),
    )
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
fn record_sync_failure_with_diagnostic(
    operation_id: &str,
    status: OperationStatus,
    phase: OperationPhase,
    started_at_ms: u128,
    backups: &[BackupManifest],
    diagnostic: Option<&DiagnosticOperation>,
    diagnostic_error: Option<&str>,
) -> Result<OperationRecord, String> {
    record_sync_failure_to_log_with_diagnostic(
        &operation_log()?,
        operation_id,
        status,
        phase,
        started_at_ms,
        backups,
        diagnostic,
        diagnostic_error,
    )
}

#[cfg(test)]
fn record_sync_failure_to_log(
    log: &OperationLog,
    operation_id: &str,
    status: OperationStatus,
    phase: OperationPhase,
    started_at_ms: u128,
    backups: &[BackupManifest],
) -> Result<OperationRecord, String> {
    record_sync_failure_to_log_with_diagnostic(
        log,
        operation_id,
        status,
        phase,
        started_at_ms,
        backups,
        None,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
fn record_sync_failure_to_log_with_diagnostic(
    log: &OperationLog,
    operation_id: &str,
    status: OperationStatus,
    phase: OperationPhase,
    started_at_ms: u128,
    backups: &[BackupManifest],
    diagnostic: Option<&DiagnosticOperation>,
    diagnostic_error: Option<&str>,
) -> Result<OperationRecord, String> {
    append_operation_record_receipt_to_with_diagnostic(
        log,
        operation_id,
        OperationAction::SyncSessions,
        OperationTerminal { status, phase },
        started_at_ms,
        backups,
        BTreeMap::new(),
        diagnostic,
        diagnostic_error,
    )
}

#[cfg(test)]
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
    append_operation_record_receipt_to_with_diagnostic(
        log,
        operation_id,
        action,
        OperationTerminal { status, phase },
        started_at_ms,
        backups,
        counts,
        None,
        result.as_ref().err().map(String::as_str),
    )
    .map(|_| ())
}

fn record_success_result_with_diagnostic(
    operation_id: &str,
    action: OperationAction,
    started_at_ms: u128,
    backups: &[BackupManifest],
    counts: BTreeMap<String, usize>,
    diagnostic: Option<&DiagnosticOperation>,
) -> Result<OperationRecord, String> {
    let log = operation_log().inspect_err(|error| {
        record_diagnostic_branch(
            diagnostic,
            "recordingResult",
            "commands.operation_history.open_failed",
            error,
        );
    })?;
    append_operation_record_receipt_to_with_diagnostic(
        &log,
        operation_id,
        action,
        OperationTerminal {
            status: OperationStatus::Succeeded,
            phase: OperationPhase::Complete,
        },
        started_at_ms,
        backups,
        counts,
        diagnostic,
        None,
    )
}

fn terminal_status(error: &str) -> OperationStatus {
    if error.contains("rollback failed") {
        OperationStatus::RollbackFailed
    } else if error.contains("rolled back") || error.contains("restored the safety snapshot") {
        OperationStatus::RolledBack
    } else {
        OperationStatus::Failed
    }
}

#[allow(clippy::too_many_arguments)]
fn append_operation_record_with_phase_and_diagnostic(
    operation_id: &str,
    action: OperationAction,
    status: OperationStatus,
    phase: OperationPhase,
    started_at_ms: u128,
    backups: &[BackupManifest],
    counts: BTreeMap<String, usize>,
    diagnostic: Option<&DiagnosticOperation>,
    diagnostic_error: Option<&str>,
) -> Result<OperationRecord, String> {
    let log = operation_log().inspect_err(|error| {
        record_diagnostic_branch(
            diagnostic,
            "recordingResult",
            "commands.operation_history.open_failed",
            error,
        );
    })?;
    append_operation_record_receipt_to_with_diagnostic(
        &log,
        operation_id,
        action,
        OperationTerminal { status, phase },
        started_at_ms,
        backups,
        counts,
        diagnostic,
        diagnostic_error,
    )
}

#[cfg(test)]
fn append_operation_record_to(
    log: &OperationLog,
    operation_id: &str,
    action: OperationAction,
    terminal: OperationTerminal,
    started_at_ms: u128,
    backups: &[BackupManifest],
    counts: BTreeMap<String, usize>,
) -> Result<(), String> {
    append_operation_record_receipt_to(
        log,
        operation_id,
        action,
        terminal,
        started_at_ms,
        backups,
        counts,
    )
    .map(|_| ())
}

#[cfg(test)]
fn append_operation_record_receipt_to(
    log: &OperationLog,
    operation_id: &str,
    action: OperationAction,
    terminal: OperationTerminal,
    started_at_ms: u128,
    backups: &[BackupManifest],
    counts: BTreeMap<String, usize>,
) -> Result<OperationRecord, String> {
    append_operation_record_receipt_to_with_diagnostic(
        log,
        operation_id,
        action,
        terminal,
        started_at_ms,
        backups,
        counts,
        None,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn append_operation_record_receipt_to_with_diagnostic(
    log: &OperationLog,
    operation_id: &str,
    action: OperationAction,
    terminal: OperationTerminal,
    started_at_ms: u128,
    backups: &[BackupManifest],
    counts: BTreeMap<String, usize>,
    diagnostic: Option<&DiagnosticOperation>,
    diagnostic_error: Option<&str>,
) -> Result<OperationRecord, String> {
    let record = OperationRecord {
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
    };
    append_durable_operation_record(log, &record, diagnostic, diagnostic_error)?;
    Ok(record)
}

fn append_durable_operation_record(
    log: &OperationLog,
    record: &OperationRecord,
    diagnostic: Option<&DiagnosticOperation>,
    diagnostic_error: Option<&str>,
) -> Result<(), String> {
    log.append(record).inspect_err(|error| {
        record_diagnostic_branch(
            diagnostic,
            "recordingResult",
            "commands.operation_history.append_failed",
            error,
        );
    })?;
    mirror_durable_operation_record(record, diagnostic, diagnostic_error);
    Ok(())
}

fn mirror_durable_operation_record(
    record: &OperationRecord,
    diagnostic: Option<&DiagnosticOperation>,
    diagnostic_error: Option<&str>,
) {
    if let Some(diagnostic) = diagnostic {
        let _ = diagnostic.bind_operation_id(record.operation_id.clone());
    } else if let Some(runtime) = global_runtime() {
        mirror_durable_operation_record_to(&runtime.recorder(), record, diagnostic_error);
    }
}

fn mirror_durable_operation_record_to(
    recorder: &DiagnosticRecorder,
    record: &OperationRecord,
    diagnostic_error: Option<&str>,
) {
    let diagnostic = recorder.begin_operation("commands", operation_action_name(record.action));
    mirror_durable_operation_record_to_operation(&diagnostic, record, diagnostic_error);
}

fn mirror_durable_operation_record_to_operation(
    diagnostic: &DiagnosticOperation,
    record: &OperationRecord,
    diagnostic_error: Option<&str>,
) {
    let _ = diagnostic.bind_operation_id(record.operation_id.clone());
    let default_message = operation_status_message(record.status);
    let _ = diagnostic.terminal(
        diagnostic_terminal_status(record.status),
        Some(operation_phase_name(record.phase)),
        operation_status_error_code(record.status),
        diagnostic_error.or(default_message),
        empty_context(),
    );
}

fn operation_action_name(action: OperationAction) -> &'static str {
    match action {
        OperationAction::ImportAccount => "importAccount",
        OperationAction::SaveRelay => "saveRelay",
        OperationAction::VerifyRelay => "verifyRelay",
        OperationAction::SwitchRuntime => "switchRuntime",
        OperationAction::IncrementalSync => "incrementalSync",
        OperationAction::SyncSessions => "syncSessions",
        OperationAction::DeleteSessions => "deleteSessions",
        OperationAction::RestoreVisibility => "restoreVisibility",
        OperationAction::CreateBackup => "createBackup",
        OperationAction::DeleteBackup => "deleteBackup",
        OperationAction::RestoreBackup => "restoreBackup",
        OperationAction::CleanupCheckpoints => "cleanupCheckpoints",
        OperationAction::InstallSkill => "installSkill",
        OperationAction::ConfigureSkill => "configureSkill",
    }
}

fn operation_phase_name(phase: OperationPhase) -> &'static str {
    match phase {
        OperationPhase::Preflight => "preflight",
        OperationPhase::Backup => "backup",
        OperationPhase::Apply => "apply",
        OperationPhase::Verify => "verify",
        OperationPhase::Complete => "complete",
        OperationPhase::Rollback => "rollback",
    }
}

fn diagnostic_terminal_status(status: OperationStatus) -> DiagnosticTerminalStatus {
    match status {
        OperationStatus::Succeeded => DiagnosticTerminalStatus::Succeeded,
        OperationStatus::Failed => DiagnosticTerminalStatus::Failed,
        OperationStatus::RolledBack => DiagnosticTerminalStatus::RolledBack,
        OperationStatus::RollbackFailed => DiagnosticTerminalStatus::RollbackFailed,
    }
}

fn operation_status_error_code(status: OperationStatus) -> Option<&'static str> {
    match status {
        OperationStatus::Succeeded => None,
        OperationStatus::Failed => Some("operation.durable_terminal.failed"),
        OperationStatus::RolledBack => Some("operation.durable_terminal.rolled_back"),
        OperationStatus::RollbackFailed => Some("operation.durable_terminal.rollback_failed"),
    }
}

fn operation_status_message(status: OperationStatus) -> Option<&'static str> {
    match status {
        OperationStatus::Succeeded => None,
        OperationStatus::Failed => Some("operation reached a failed durable terminal"),
        OperationStatus::RolledBack => Some("operation failed and was rolled back"),
        OperationStatus::RollbackFailed => Some("operation failed and rollback did not complete"),
    }
}

#[derive(Debug, Clone, Copy)]
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
    diagnostic: Option<&DiagnosticOperation>,
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
            let durable_result = operation_log()
                .inspect_err(|error| {
                    record_diagnostic_branch(
                        diagnostic,
                        "recordingResult",
                        "commands.operation_history.open_failed",
                        error,
                    );
                })
                .and_then(|log| append_durable_operation_record(&log, &record, diagnostic, None));
            if durable_result.is_err() {
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
            let log = operation_log().inspect_err(|log_error| {
                record_diagnostic_branch(
                    diagnostic,
                    "recordingResult",
                    "commands.operation_history.open_failed",
                    log_error,
                );
            });
            if let Ok(log) = log {
                let record = OperationRecord {
                    operation_id: attempt_id,
                    action,
                    status,
                    phase,
                    started_at_ms,
                    completed_at_ms: timestamp_millis()?,
                    backup_dirs: Vec::new(),
                    counts: BTreeMap::new(),
                };
                let _ = append_durable_operation_record(&log, &record, diagnostic, Some(&error));
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
            create_backup, create_runtime_state_backup_with_paths,
            create_runtime_state_checkpoint_with_paths, create_state_backup,
            create_state_checkpoint_with_paths, CheckpointRole,
        },
        codex_paths::resolve_user_codex_paths,
        diagnostics::{
            DiagnosticEventKind, DiagnosticRecorder, DiagnosticSanitizer, DiagnosticStore,
            DiagnosticTerminalStatus, SanitizerRoots,
        },
        mobile_continuity::{
            MobileContinuityItem, MobileContinuityItemStatus, MobileContinuityStatus,
        },
        operation_log::{
            OperationAction, OperationLog, OperationPhase, OperationRecord, OperationStatus,
        },
        process_control::CodexProcess,
        session_manager::SessionMutationResult,
        session_storage::{
            migration::{persist_migration_preflight, run_migration_preflight},
            operation_ledger::{OperationLedgerStore, SessionStorageOperationKind},
            storage_state::{finalize_canonical_storage_state, prepare_canonical_storage_state},
        },
    };

    use super::{
        acquire_mutation_lock_at, after_capacity_preflight, append_operation_record_to,
        automatic_gc_decision, automatic_gc_error_is_transient, capture_chatgpt_launch_target_once,
        checkpoint_cleanup_counts, checkpoint_cleanup_diagnostic_status,
        checkpoint_cleanup_terminal, classify_failed_commit_transition,
        cleanup_automatic_checkpoints, close_codex_processes,
        close_runtime_processes_with_progress, collect_session_conflict_candidates,
        correlate_mutation_result, create_full_backup, default_codex_home_from_env, delete_backup,
        delete_backup_at, diagnostic_status_for_command_error, diagnostic_terminal_status,
        emit_runtime_switch_progress_diagnostic, emit_runtime_switch_terminal,
        emit_runtime_switch_terminal_diagnostic, emit_session_sync_progress_diagnostic,
        encode_mutation_error, ensure_codex_closed_from_processes, ensure_codex_paths_unchanged,
        failed_runtime_switch_checkpoints_are_releasable, finish_session_mutation_with_log,
        get_app_status, inspect_checkpoint_storage, launch_chatgpt,
        launch_chatgpt_after_durable_terminal, list_backups, list_backups_at,
        load_session_merge_backup_destination, merge_and_repair_sessions,
        mirror_durable_operation_record, mirror_durable_operation_record_to,
        mobile_publication_terminal, mutation_busy_error, offline_gc_setting_allows_execution,
        preflight_before_process_gate, prepare_app_exit_at, record_background_result_to,
        record_chatgpt_launch_diagnostic, record_diagnostic_terminal,
        record_post_mutation_launch_issue, record_result_to_log,
        record_runtime_switch_result_to_log, record_runtime_switch_result_to_log_with_diagnostic,
        record_sync_failure_to_log, record_sync_failure_to_log_with_diagnostic,
        relay_validation_without_network, release_transient_checkpoints,
        rollback_unapplied_session_storage_migration,
        rollback_unapplied_session_storage_migration_with_cleanup,
        schedule_shadow_scan_after_switch, schedule_shadow_with_optional_automatic_gc,
        successful_switch_requests_chatgpt_launch, switch_runtime, validate_backup_selection,
        AutomaticGcDecision, BackupDeleteReceipt, BackupReceiptSummary, ChatGptLaunchReceipt,
        ChatGptLaunchStatus, CheckpointCleanupReceipt, CommitTransitionDisposition,
        CreateFullBackupReceipt, MutationCoordinator, OperationTerminal, RuntimeSwitchOutcome,
        RuntimeSwitchPhase, RuntimeSwitchProgress, RuntimeSwitchResult,
        SessionStorageOperationPhase, SessionSyncPhase, SessionSyncProgress,
        MAX_LISTED_FULL_BACKUPS, MUTATION_ERROR_ENVELOPE_PREFIX,
    };

    #[cfg(windows)]
    use super::open_mutation_lock_file;

    fn diagnostic_recorder(root: &std::path::Path) -> DiagnosticRecorder {
        let store = DiagnosticStore::new(
            root.join("diagnostics"),
            "commands-test-session".to_string(),
            DiagnosticSanitizer::new(SanitizerRoots::default()),
        );
        DiagnosticRecorder::new(store, "commands-test-session".to_string())
    }

    #[test]
    fn app_status_does_not_report_the_retired_scaffold_phase() {
        assert_eq!(get_app_status().app_name, "ChatGPT Switch");
        assert_eq!(get_app_status().phase, "hardened-mvp");
        assert_eq!(get_app_status().version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn mutation_error_envelope_preserves_message_and_prefers_durable_correlation() {
        let temp = tempdir().unwrap();
        let recorder = diagnostic_recorder(temp.path());
        let attempt = recorder.begin_operation("commands", "saveRelay");
        let attempt_id = attempt.attempt_id();
        let message = "relay save failed: original business message";

        let encoded =
            correlate_mutation_result::<()>(Err(message.to_string()), Some(&attempt)).unwrap_err();
        let payload = encoded
            .strip_prefix(MUTATION_ERROR_ENVELOPE_PREFIX)
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(payload).unwrap();
        assert_eq!(value["message"], message);
        assert_eq!(value["operationId"], attempt_id);

        let durable = recorder.begin_operation("commands", "installSkill");
        assert!(durable.bind_operation_id("install-skill-attempt-1780000000000-42-1"));
        let encoded =
            correlate_mutation_result::<()>(Err(message.to_string()), Some(&durable)).unwrap_err();
        let payload = encoded
            .strip_prefix(MUTATION_ERROR_ENVELOPE_PREFIX)
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(payload).unwrap();
        assert_eq!(value["message"], message);
        assert_eq!(
            value["operationId"],
            "install-skill-attempt-1780000000000-42-1"
        );
    }

    #[test]
    fn mutation_error_envelope_rejects_untrusted_correlation_shapes() {
        assert!(encode_mutation_error("failed", "../outside").is_none());
        assert!(encode_mutation_error("failed", "with space").is_none());
        assert!(encode_mutation_error("failed", &"a".repeat(161)).is_none());
        assert!(encode_mutation_error(&"x".repeat(16 * 1024 + 1), "attempt-safe").is_none());
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
        assert_eq!(
            diagnostic_terminal_status(terminal.status),
            DiagnosticTerminalStatus::Succeeded
        );
        assert_eq!(
            checkpoint_cleanup_diagnostic_status(&receipt, true),
            DiagnosticTerminalStatus::Succeeded
        );

        receipt.failed_count = 1;
        receipt.reclaimed_count = 3;
        let counts = checkpoint_cleanup_counts(&receipt);
        let terminal = checkpoint_cleanup_terminal(&receipt);

        assert_eq!(counts["failedCount"], 1);
        assert_eq!(terminal.status, OperationStatus::Failed);
        assert_eq!(terminal.phase, OperationPhase::Apply);
        assert_eq!(
            diagnostic_terminal_status(terminal.status),
            DiagnosticTerminalStatus::Failed
        );
        assert_eq!(
            checkpoint_cleanup_diagnostic_status(&receipt, true),
            DiagnosticTerminalStatus::Partial
        );
        assert_eq!(
            checkpoint_cleanup_diagnostic_status(&receipt, false),
            DiagnosticTerminalStatus::Partial
        );
    }

    #[test]
    fn manual_publication_terminal_uses_only_the_target_item() {
        let mut status = MobileContinuityStatus {
            enabled: true,
            notice_pending: false,
            initialized_at_ms: 1,
            queued: 0,
            publishing: 0,
            remote_published: 1,
            partial: 0,
            conflict: 1,
            needs_manual: 1,
            items: vec![
                MobileContinuityItem {
                    thread_id: "historical-item".to_string(),
                    status: MobileContinuityItemStatus::NeedsManual,
                    attempts: 1,
                    next_retry_at_ms: None,
                    updated_at_ms: 1,
                    failure_category: None,
                    source_fingerprint: None,
                },
                MobileContinuityItem {
                    thread_id: "target-item".to_string(),
                    status: MobileContinuityItemStatus::RemotePublished,
                    attempts: 1,
                    next_retry_at_ms: None,
                    updated_at_ms: 1,
                    failure_category: None,
                    source_fingerprint: None,
                },
            ],
        };

        assert_eq!(
            mobile_publication_terminal(&status, "target-item"),
            DiagnosticTerminalStatus::Succeeded
        );
        status.items[1].status = MobileContinuityItemStatus::Conflict;
        assert_eq!(
            mobile_publication_terminal(&status, "target-item"),
            DiagnosticTerminalStatus::Blocked
        );
        status.items[1].status = MobileContinuityItemStatus::Partial;
        assert_eq!(
            mobile_publication_terminal(&status, "target-item"),
            DiagnosticTerminalStatus::Partial
        );
        assert_eq!(
            mobile_publication_terminal(&status, "missing-item"),
            DiagnosticTerminalStatus::Partial
        );
    }

    #[test]
    fn user_preflight_gates_are_typed_as_blocked() {
        assert_eq!(
            diagnostic_status_for_command_error("backup deletion requires explicit confirmation"),
            DiagnosticTerminalStatus::Blocked
        );
        assert_eq!(
            diagnostic_status_for_command_error(
                "another ChatGPT Switch mutation is already in progress"
            ),
            DiagnosticTerminalStatus::Blocked
        );
        assert_eq!(
            diagnostic_status_for_command_error("API key is required for the first save"),
            DiagnosticTerminalStatus::Blocked
        );
        assert_eq!(
            diagnostic_status_for_command_error("disk write failed"),
            DiagnosticTerminalStatus::Failed
        );
        assert_eq!(
            diagnostic_status_for_command_error(
                "backup restore failed: injected; restored the safety snapshot"
            ),
            DiagnosticTerminalStatus::RolledBack
        );
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
        assert_future(switch_runtime("relay".to_string(), None, on_progress));
    }

    #[test]
    fn automatic_gc_waits_for_a_fresh_scan_and_closed_writer_window() {
        assert_eq!(
            automatic_gc_decision(
                true,
                Some("migration-1"),
                Some("scan-old"),
                Some("scan-old"),
                2,
                true,
                0,
                0,
            ),
            AutomaticGcDecision::WaitForFreshScan
        );
        assert_eq!(
            automatic_gc_decision(
                true,
                Some("migration-1"),
                Some("scan-old"),
                Some("scan-old"),
                2,
                false,
                0,
                0,
            ),
            AutomaticGcDecision::Blocked(
                "automatic offline cleanup did not receive a fresh Shadow report"
            )
        );
        assert_eq!(
            automatic_gc_decision(
                true,
                Some("migration-1"),
                Some("scan-new"),
                Some("scan-old"),
                2,
                false,
                1,
                0,
            ),
            AutomaticGcDecision::WaitForWriter
        );
        assert_eq!(
            automatic_gc_decision(
                true,
                Some("migration-1"),
                Some("scan-new"),
                Some("scan-old"),
                2,
                false,
                0,
                1,
            ),
            AutomaticGcDecision::Blocked(
                "automatic offline cleanup is blocked by an unfinished non-GC operation"
            )
        );
        assert_eq!(
            automatic_gc_decision(
                true,
                Some("migration-1"),
                Some("scan-new"),
                Some("scan-old"),
                2,
                false,
                0,
                0,
            ),
            AutomaticGcDecision::Run("migration-1".to_string())
        );
        assert_eq!(
            automatic_gc_decision(
                false,
                Some("migration-1"),
                Some("scan-new"),
                Some("scan-old"),
                2,
                false,
                0,
                0,
            ),
            AutomaticGcDecision::Stop
        );
        assert_eq!(
            automatic_gc_decision(
                true,
                Some("migration-1"),
                Some("scan-new"),
                Some("scan-old"),
                0,
                false,
                0,
                0,
            ),
            AutomaticGcDecision::Stop
        );
        assert_eq!(
            automatic_gc_decision(
                true,
                Some("migration-1"),
                Some("scan-after-clock-regression"),
                Some("scan-before-clock-regression"),
                1,
                false,
                0,
                0,
            ),
            AutomaticGcDecision::Run("migration-1".to_string())
        );
        assert!(automatic_gc_error_is_transient(&mutation_busy_error()));
        assert!(automatic_gc_error_is_transient(
            "session storage mutation requires every Codex writer to be closed; activeProcesses=1"
        ));
        assert!(automatic_gc_error_is_transient(
            "offline session storage cleanup rollback is waiting for every Codex writer to close"
        ));
        assert!(!automatic_gc_error_is_transient(
            "offline GC candidate checksum changed"
        ));
    }

    #[test]
    fn disabled_automatic_cleanup_blocks_only_automatic_gc() {
        assert!(!offline_gc_setting_allows_execution(true, false));
        assert!(offline_gc_setting_allows_execution(true, true));
        assert!(offline_gc_setting_allows_execution(false, false));
    }

    #[test]
    fn merge_and_conflict_flow_use_v2_after_primary_proof_expiry() {
        let root = tempdir().unwrap();
        let data = root.path().join("data");
        let canonical = root.path().join("canonical");
        let backup_destination = root.path().join("migration-backups");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&canonical).unwrap();
        fs::create_dir_all(&backup_destination).unwrap();
        fs::write(
            canonical.join("config.toml"),
            format!("sqlite_home = {:?}\n", canonical.to_string_lossy()),
        )
        .unwrap();
        Connection::open(canonical.join("state_5.sqlite"))
            .unwrap()
            .execute_batch(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, model_provider TEXT);",
            )
            .unwrap();
        Connection::open(canonical.join("goals_1.sqlite"))
            .unwrap()
            .execute_batch(
                "CREATE TABLE thread_goals (
                    thread_id TEXT PRIMARY KEY NOT NULL,
                    goal_id TEXT NOT NULL,
                    objective TEXT NOT NULL,
                    status TEXT NOT NULL CHECK(status IN ('active','paused','blocked','usage_limited','budget_limited','complete')),
                    token_budget INTEGER,
                    tokens_used INTEGER NOT NULL DEFAULT 0,
                    time_used_seconds INTEGER NOT NULL DEFAULT 0,
                    created_at_ms INTEGER NOT NULL,
                    updated_at_ms INTEGER NOT NULL
                 );
                 CREATE TABLE thread_goal_continuation_deferrals (
                    thread_id TEXT PRIMARY KEY NOT NULL REFERENCES thread_goals(thread_id) ON DELETE CASCADE
                 );",
            )
            .unwrap();
        let operation_id = "migration-merge-proof-expiry";
        let store = OperationLedgerStore::new(&data);
        store
            .create(
                operation_id,
                SessionStorageOperationKind::Migration,
                &canonical,
            )
            .unwrap();
        let preflight =
            run_migration_preflight(&canonical, &data, operation_id, &backup_destination).unwrap();
        assert!(preflight.ready_for_backup, "{:?}", preflight.blockers);
        persist_migration_preflight(&data, &preflight).unwrap();
        store
            .update(operation_id, |ledger| {
                ledger.backup_root = Some(backup_destination.join(operation_id));
                Ok(())
            })
            .unwrap();
        for phase in [
            SessionStorageOperationPhase::Preflight,
            SessionStorageOperationPhase::Backup,
            SessionStorageOperationPhase::BackupVerified,
            SessionStorageOperationPhase::PlanReady,
            SessionStorageOperationPhase::Applying,
            SessionStorageOperationPhase::Validating,
        ] {
            store.transition(operation_id, phase).unwrap();
        }
        prepare_canonical_storage_state(
            &data,
            &canonical,
            operation_id,
            &preflight.plan.inventory_fingerprint,
        )
        .unwrap();
        store
            .transition(operation_id, SessionStorageOperationPhase::Committed)
            .unwrap();
        let state = finalize_canonical_storage_state(&data, &canonical, operation_id).unwrap();
        fs::remove_dir_all(
            data.join("session-storage-v1/operations")
                .join(operation_id),
        )
        .unwrap();

        assert_eq!(
            load_session_merge_backup_destination(&data, &canonical).unwrap(),
            backup_destination
        );
        assert!(collect_session_conflict_candidates(
            &store,
            &data,
            &canonical,
            operation_id,
            state.committed_at_ms,
            &Default::default(),
        )
        .unwrap()
        .is_empty());
    }

    #[test]
    fn ambiguous_migration_commit_is_observed_before_rollback() {
        assert_eq!(
            classify_failed_commit_transition(
                "injected post-write verification failure".to_string(),
                Ok(SessionStorageOperationPhase::Committed),
            ),
            CommitTransitionDisposition::ConfirmedCommitted
        );
        assert_eq!(
            classify_failed_commit_transition(
                "injected pre-write failure".to_string(),
                Ok(SessionStorageOperationPhase::Validating),
            ),
            CommitTransitionDisposition::SafeToRollback("injected pre-write failure".to_string())
        );
        let indeterminate = classify_failed_commit_transition(
            "injected transition failure".to_string(),
            Err("injected reload failure".to_string()),
        );
        assert!(matches!(
            indeterminate,
            CommitTransitionDisposition::PreserveForRecovery(message)
                if message.contains("commit state could not be verified")
        ));
    }

    #[test]
    fn startup_recovery_rolls_back_every_preapply_migration_phase_without_live_writes() {
        for (operation_id, phases) in [
            ("migration-available", Vec::new()),
            (
                "session-merge-repair-plan-ready",
                vec![
                    SessionStorageOperationPhase::Preflight,
                    SessionStorageOperationPhase::Backup,
                    SessionStorageOperationPhase::BackupVerified,
                    SessionStorageOperationPhase::PlanReady,
                ],
            ),
        ] {
            let root = tempdir().unwrap();
            let data_root = root.path().join("data");
            let canonical_root = root.path().join("canonical");
            fs::create_dir_all(&data_root).unwrap();
            fs::create_dir_all(&canonical_root).unwrap();
            let store = OperationLedgerStore::new(&data_root);
            store
                .create(
                    operation_id,
                    SessionStorageOperationKind::Migration,
                    &canonical_root,
                )
                .unwrap();
            for phase in phases {
                store.transition(operation_id, phase).unwrap();
            }
            let staging = data_root
                .join("session-storage-v1/operations")
                .join(operation_id)
                .join("migration-staging");
            fs::create_dir_all(&staging).unwrap();
            fs::write(staging.join("temporary.bin"), b"temporary").unwrap();

            rollback_unapplied_session_storage_migration(
                &store,
                &data_root,
                operation_id,
                "injectedInterruption",
            )
            .unwrap();

            let recovered = store.load(operation_id).unwrap();
            assert_eq!(recovered.phase, SessionStorageOperationPhase::RolledBack);
            assert_eq!(
                recovered.last_error_code.as_deref(),
                Some("injectedInterruption")
            );
            assert!(!staging.exists());
        }
    }

    #[test]
    fn preapply_cleanup_io_failure_stays_rolling_back_and_retries_to_completion() {
        let root = tempdir().unwrap();
        let data_root = root.path().join("data");
        let canonical_root = root.path().join("canonical");
        fs::create_dir_all(&data_root).unwrap();
        fs::create_dir_all(&canonical_root).unwrap();
        let operation_id = "migration-cleanup-retry";
        let store = OperationLedgerStore::new(&data_root);
        store
            .create(
                operation_id,
                SessionStorageOperationKind::Migration,
                &canonical_root,
            )
            .unwrap();
        let staging = data_root
            .join("session-storage-v1/operations")
            .join(operation_id)
            .join("migration-staging");
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("temporary.bin"), b"temporary").unwrap();

        let first = rollback_unapplied_session_storage_migration_with_cleanup(
            &store,
            &data_root,
            operation_id,
            "injectedInterruption",
            || Err("injected cleanup I/O failure".to_string()),
        );
        assert!(first.is_err());
        let retryable = store.load(operation_id).unwrap();
        assert_eq!(retryable.phase, SessionStorageOperationPhase::RollingBack);
        assert!(!retryable.live_mutation_started);
        assert_eq!(
            retryable.last_error_code.as_deref(),
            Some("migrationPreApplyCleanupFailed")
        );
        assert_eq!(store.unfinished().unwrap(), vec![retryable]);

        rollback_unapplied_session_storage_migration(
            &store,
            &data_root,
            operation_id,
            "migrationInterruptedBeforeApply",
        )
        .unwrap();

        let recovered = store.load(operation_id).unwrap();
        assert_eq!(recovered.phase, SessionStorageOperationPhase::RolledBack);
        assert!(!staging.exists());
        assert!(store.unfinished().unwrap().is_empty());
    }

    #[test]
    fn only_a_successful_switch_schedules_one_non_blocking_shadow_scan() {
        let root = tempdir().unwrap();
        let codex_home = root.path().join("codex-home");
        let data_root = root.path().join("data-root");
        let scheduled = Arc::new(Mutex::new(Vec::new()));

        let failed_roots_called = Arc::new(Mutex::new(false));
        let failed_roots_probe = Arc::clone(&failed_roots_called);
        assert!(!schedule_shadow_scan_after_switch(
            false,
            move || {
                *failed_roots_probe.lock().unwrap() = true;
                Some((codex_home.clone(), data_root.clone()))
            },
            |_, _| true,
        ));
        assert!(!*failed_roots_called.lock().unwrap());

        let expected_home = root.path().join("expected-home");
        let expected_data = root.path().join("expected-data");
        let recorded = Arc::clone(&scheduled);
        assert!(schedule_shadow_scan_after_switch(
            true,
            || Some((expected_home.clone(), expected_data.clone())),
            move |home, data| {
                recorded.lock().unwrap().push((home, data));
                true
            },
        ));
        assert_eq!(
            *scheduled.lock().unwrap(),
            vec![(expected_home, expected_data)]
        );
    }

    #[test]
    fn startup_failure_schedules_pure_shadow_without_automatic_gc() {
        let root = tempdir().unwrap();
        let home = root.path().join("canonical");
        let data = root.path().join("data");
        let failure_schedule = RefCell::new(Vec::new());

        assert!(schedule_shadow_with_optional_automatic_gc(
            false,
            home.clone(),
            data.clone(),
            |scheduled_home, scheduled_data| {
                assert_eq!(scheduled_home, home);
                assert_eq!(scheduled_data, data);
                failure_schedule.borrow_mut().push("shadow");
                true
            },
            |_, _| {
                failure_schedule.borrow_mut().push("automatic-gc");
                true
            },
        ));
        assert_eq!(*failure_schedule.borrow(), vec!["shadow"]);

        let success_schedule = RefCell::new(Vec::new());
        assert!(schedule_shadow_with_optional_automatic_gc(
            true,
            root.path().join("canonical"),
            root.path().join("data"),
            |_, _| {
                success_schedule.borrow_mut().push("shadow");
                true
            },
            |_, _| {
                success_schedule.borrow_mut().push("automatic-gc");
                true
            },
        ));
        assert_eq!(*success_schedule.borrow(), vec!["automatic-gc"]);
    }

    #[test]
    fn relay_switch_never_enters_a_network_verification_phase() {
        let status = relay_validation_without_network(crate::runtime_store::RELAY_RUNTIME_ID);
        assert_eq!(
            status,
            crate::runtime_switcher::RelayValidationStatus::Skipped
        );
        assert_eq!(
            relay_validation_without_network(crate::runtime_store::PLUS_RUNTIME_ID),
            crate::runtime_switcher::RelayValidationStatus::NotApplicable
        );
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
    fn chatgpt_launch_command_remains_async() {
        fn assert_future<F>(future: F)
        where
            F: Future<Output = Result<ChatGptLaunchReceipt, String>> + Send,
        {
            drop(future);
        }

        assert_future(launch_chatgpt());
    }

    #[test]
    fn typed_ok_chatgpt_launch_failures_are_diagnostic_failures() {
        let temp = tempdir().unwrap();
        let recorder = diagnostic_recorder(temp.path());
        let failed = recorder.begin_operation("commands", "launchChatgpt");
        let failed_result = Ok(ChatGptLaunchReceipt {
            status: ChatGptLaunchStatus::Failed,
            message: Some("Windows activation failed".to_string()),
        });
        record_chatgpt_launch_diagnostic(Some(&failed), &failed_result);

        let blocked = recorder.begin_operation("commands", "launchChatgpt");
        let blocked_result = Ok(ChatGptLaunchReceipt {
            status: ChatGptLaunchStatus::Blocked,
            message: Some("launch remained blocked".to_string()),
        });
        record_chatgpt_launch_diagnostic(Some(&blocked), &blocked_result);

        let terminals = recorder
            .store()
            .read_events()
            .unwrap()
            .into_iter()
            .filter(|event| event.event_kind == DiagnosticEventKind::OperationTerminal)
            .collect::<Vec<_>>();
        assert_eq!(terminals.len(), 2);
        assert_eq!(
            terminals[0].terminal_status,
            Some(DiagnosticTerminalStatus::Failed)
        );
        assert_eq!(
            terminals[1].terminal_status,
            Some(DiagnosticTerminalStatus::Blocked)
        );
    }

    #[test]
    fn typed_ok_post_mutation_launch_failures_are_partial_or_blocked() {
        let temp = tempdir().unwrap();
        let recorder = diagnostic_recorder(temp.path());
        let failed = recorder.begin_operation("commands", "switchRuntime");
        assert!(record_post_mutation_launch_issue(
            Some(&failed),
            &ChatGptLaunchReceipt {
                status: ChatGptLaunchStatus::Failed,
                message: Some("Windows activation failed".to_string()),
            },
            "commands.switch_runtime.launch_failed",
            "commands.switch_runtime.launch_blocked",
        ));
        let blocked = recorder.begin_operation("commands", "mergeAndRepairSessions");
        assert!(record_post_mutation_launch_issue(
            Some(&blocked),
            &ChatGptLaunchReceipt {
                status: ChatGptLaunchStatus::Blocked,
                message: Some("launch remained blocked".to_string()),
            },
            "commands.merge_and_repair_sessions.launch_failed",
            "commands.merge_and_repair_sessions.launch_blocked",
        ));

        let terminals = recorder
            .store()
            .read_events()
            .unwrap()
            .into_iter()
            .filter(|event| event.event_kind == DiagnosticEventKind::OperationTerminal)
            .collect::<Vec<_>>();
        assert_eq!(terminals.len(), 2);
        assert_eq!(
            terminals[0].terminal_status,
            Some(DiagnosticTerminalStatus::Partial)
        );
        assert_eq!(
            terminals[1].terminal_status,
            Some(DiagnosticTerminalStatus::Blocked)
        );
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

        let on_progress = Channel::<SessionSyncProgress>::new(|_| Ok(()));
        assert_future(merge_and_repair_sessions(on_progress));
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
    fn runtime_switch_progress_carries_attempt_then_durable_correlation() {
        let temp = tempdir().unwrap();
        let recorder = diagnostic_recorder(temp.path());
        let diagnostic = recorder.begin_operation("commands", "switchRuntime");
        let attempt_id = diagnostic.attempt_id();
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

        emit_runtime_switch_progress_diagnostic(
            &on_progress,
            Some(&diagnostic),
            RuntimeSwitchPhase::LoadingRuntime,
            None,
        );
        assert!(diagnostic.bind_operation_id("switch-runtime-durable"));
        emit_runtime_switch_progress_diagnostic(
            &on_progress,
            Some(&diagnostic),
            RuntimeSwitchPhase::Verifying,
            None,
        );
        record_diagnostic_terminal(
            Some(&diagnostic),
            DiagnosticTerminalStatus::Succeeded,
            "complete",
            None,
            None,
        );

        let events = events.lock().unwrap();
        assert_eq!(events[0]["operationId"], attempt_id);
        assert_eq!(events[1]["operationId"], "switch-runtime-durable");
        let diagnostics = recorder.store().read_events().unwrap();
        let phases = diagnostics
            .iter()
            .filter(|event| event.event_kind == DiagnosticEventKind::OperationPhase)
            .filter_map(|event| event.phase.as_deref())
            .collect::<Vec<_>>();
        assert_eq!(phases, vec!["loadingRuntime", "verifying"]);
        assert_eq!(
            diagnostics
                .iter()
                .filter(|event| event.event_kind == DiagnosticEventKind::OperationTerminal)
                .count(),
            1
        );
    }

    #[test]
    fn session_sync_progress_carries_attempt_then_durable_correlation() {
        let temp = tempdir().unwrap();
        let recorder = diagnostic_recorder(temp.path());
        let diagnostic = recorder.begin_operation("commands", "mergeAndRepairSessions");
        let attempt_id = diagnostic.attempt_id();
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        let on_progress = Channel::<SessionSyncProgress>::new(move |body| {
            let InvokeResponseBody::Json(json) = body else {
                panic!("session sync progress must be JSON");
            };
            captured
                .lock()
                .unwrap()
                .push(serde_json::from_str::<serde_json::Value>(&json).unwrap());
            Ok(())
        });

        emit_session_sync_progress_diagnostic(
            &on_progress,
            Some(&diagnostic),
            SessionSyncPhase::Preparing,
            None,
        );
        assert!(diagnostic.bind_operation_id("sync-sessions-durable"));
        emit_session_sync_progress_diagnostic(
            &on_progress,
            Some(&diagnostic),
            SessionSyncPhase::BackingUp,
            None,
        );
        record_diagnostic_terminal(
            Some(&diagnostic),
            DiagnosticTerminalStatus::Succeeded,
            "complete",
            None,
            None,
        );

        let events = events.lock().unwrap();
        assert_eq!(events[0]["operationId"], attempt_id);
        assert_eq!(events[1]["operationId"], "sync-sessions-durable");
        let diagnostics = recorder.store().read_events().unwrap();
        let phases = diagnostics
            .iter()
            .filter(|event| event.event_kind == DiagnosticEventKind::OperationPhase)
            .filter_map(|event| event.phase.as_deref())
            .collect::<Vec<_>>();
        assert_eq!(phases, vec!["preparing", "backingUp"]);
        assert_eq!(
            diagnostics
                .iter()
                .filter(|event| event.event_kind == DiagnosticEventKind::OperationTerminal)
                .count(),
            1
        );
    }

    #[test]
    fn runtime_switch_log_append_failure_keeps_attempt_correlation() {
        let temp = tempdir().unwrap();
        let invalid_log_path = tempdir().unwrap();
        let recorder = diagnostic_recorder(temp.path());
        let diagnostic = recorder.begin_operation("commands", "switchRuntime");
        let attempt_id = diagnostic.attempt_id();
        let progress = Arc::new(Mutex::new(Vec::new()));
        let captured = progress.clone();
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
        let business_result: Result<(), String> = Err("injected switch failure".to_string());

        emit_runtime_switch_progress_diagnostic(
            &on_progress,
            Some(&diagnostic),
            RuntimeSwitchPhase::RecordingResult,
            None,
        );
        let append_result = record_runtime_switch_result_to_log_with_diagnostic(
            &OperationLog::new(invalid_log_path.path().to_path_buf()),
            "switch-runtime-not-durable",
            1,
            &business_result,
            &[],
            std::collections::BTreeMap::new(),
            RuntimeSwitchOutcome::FailedBeforeWrite,
            Some(&diagnostic),
        );
        emit_runtime_switch_terminal_diagnostic(
            &on_progress,
            Some(&diagnostic),
            &business_result,
            RuntimeSwitchOutcome::FailedBeforeWrite,
        );
        record_diagnostic_terminal(
            Some(&diagnostic),
            DiagnosticTerminalStatus::Failed,
            "preflight",
            Some("commands.switch_runtime.failed"),
            business_result.as_ref().err().map(String::as_str),
        );

        assert!(append_result.is_err());
        assert_eq!(business_result, Err("injected switch failure".to_string()));
        assert_eq!(diagnostic.operation_id(), None);
        assert!(diagnostic.is_terminal_recorded());
        let progress = progress.lock().unwrap();
        assert_eq!(progress.len(), 2);
        assert!(progress
            .iter()
            .all(|event| event["operationId"] == attempt_id));
        let events = recorder.store().read_events().unwrap();
        assert!(events.iter().all(|event| event.operation_id.is_none()));
        assert!(!events
            .iter()
            .any(|event| event.event_kind == DiagnosticEventKind::OperationBound));
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_kind == DiagnosticEventKind::OperationTerminal)
                .count(),
            1
        );
    }

    #[test]
    fn session_sync_log_append_failure_keeps_attempt_correlation() {
        let temp = tempdir().unwrap();
        let invalid_log_path = tempdir().unwrap();
        let recorder = diagnostic_recorder(temp.path());
        let diagnostic = recorder.begin_operation("commands", "mergeAndRepairSessions");
        let attempt_id = diagnostic.attempt_id();
        let progress = Arc::new(Mutex::new(Vec::new()));
        let captured = progress.clone();
        let on_progress = Channel::<SessionSyncProgress>::new(move |body| {
            let InvokeResponseBody::Json(json) = body else {
                panic!("session sync progress must be JSON");
            };
            captured
                .lock()
                .unwrap()
                .push(serde_json::from_str::<serde_json::Value>(&json).unwrap());
            Ok(())
        });
        let business_result: Result<(), String> = Err("injected sync failure".to_string());

        emit_session_sync_progress_diagnostic(
            &on_progress,
            Some(&diagnostic),
            SessionSyncPhase::RecordingResult,
            None,
        );
        let append_result = record_sync_failure_to_log_with_diagnostic(
            &OperationLog::new(invalid_log_path.path().to_path_buf()),
            "sync-sessions-not-durable",
            OperationStatus::Failed,
            OperationPhase::Apply,
            1,
            &[],
            Some(&diagnostic),
            business_result.as_ref().err().map(String::as_str),
        );
        emit_session_sync_progress_diagnostic(
            &on_progress,
            Some(&diagnostic),
            SessionSyncPhase::Failed,
            business_result.as_ref().err().cloned(),
        );
        record_diagnostic_terminal(
            Some(&diagnostic),
            DiagnosticTerminalStatus::Failed,
            "apply",
            Some("commands.merge_and_repair_sessions.failed"),
            business_result.as_ref().err().map(String::as_str),
        );

        assert!(append_result.is_err());
        assert_eq!(business_result, Err("injected sync failure".to_string()));
        assert_eq!(diagnostic.operation_id(), None);
        assert!(diagnostic.is_terminal_recorded());
        let progress = progress.lock().unwrap();
        assert_eq!(progress.len(), 2);
        assert!(progress
            .iter()
            .all(|event| event["operationId"] == attempt_id));
        let events = recorder.store().read_events().unwrap();
        assert!(events.iter().all(|event| event.operation_id.is_none()));
        assert!(!events
            .iter()
            .any(|event| event.event_kind == DiagnosticEventKind::OperationBound));
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_kind == DiagnosticEventKind::OperationTerminal)
                .count(),
            1
        );
    }

    #[test]
    fn durable_operation_mirror_preserves_audit_schema_and_records_one_terminal() {
        let temp = tempdir().unwrap();
        let recorder = diagnostic_recorder(temp.path());
        let record = OperationRecord {
            operation_id: "verify-relay-durable".to_string(),
            action: OperationAction::VerifyRelay,
            status: OperationStatus::Failed,
            phase: OperationPhase::Verify,
            started_at_ms: 10,
            completed_at_ms: 20,
            backup_dirs: Vec::new(),
            counts: std::collections::BTreeMap::new(),
        };
        let serialized_before = serde_json::to_value(&record).unwrap();
        let secret_error = "request failed with sk-1234567890abcdefghijklmnop";

        mirror_durable_operation_record_to(&recorder, &record, Some(secret_error));

        assert_eq!(serde_json::to_value(&record).unwrap(), serialized_before);
        assert!(serialized_before.get("safeMessage").is_none());
        let events = recorder.store().read_events().unwrap();
        assert_eq!(events[0].event_kind, DiagnosticEventKind::OperationStarted);
        assert_eq!(events[1].event_kind, DiagnosticEventKind::OperationBound);
        assert_eq!(events[2].event_kind, DiagnosticEventKind::OperationTerminal);
        assert_eq!(
            events[2].operation_id.as_deref(),
            Some("verify-relay-durable")
        );
        assert!(!events[2]
            .safe_message
            .as_deref()
            .unwrap_or_default()
            .contains("sk-1234567890abcdefghijklmnop"));
    }

    #[test]
    fn live_durable_mirror_binds_without_duplicating_the_command_terminal() {
        let temp = tempdir().unwrap();
        let recorder = diagnostic_recorder(temp.path());
        let diagnostic = recorder.begin_operation("commands", "mergeAndRepairSessions");
        let record = OperationRecord {
            operation_id: "sync-sessions-durable".to_string(),
            action: OperationAction::SyncSessions,
            status: OperationStatus::Succeeded,
            phase: OperationPhase::Complete,
            started_at_ms: 10,
            completed_at_ms: 20,
            backup_dirs: Vec::new(),
            counts: std::collections::BTreeMap::new(),
        };

        mirror_durable_operation_record(&record, Some(&diagnostic), None);
        assert!(!diagnostic.is_terminal_recorded());
        record_diagnostic_terminal(
            Some(&diagnostic),
            DiagnosticTerminalStatus::Succeeded,
            "complete",
            None,
            None,
        );
        record_diagnostic_terminal(
            Some(&diagnostic),
            DiagnosticTerminalStatus::Succeeded,
            "complete",
            None,
            None,
        );

        let events = recorder.store().read_events().unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_kind == DiagnosticEventKind::OperationStarted)
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_kind == DiagnosticEventKind::OperationTerminal)
                .count(),
            1
        );
    }

    #[test]
    fn background_queries_record_only_failures_and_use_the_central_sanitizer() {
        let temp = tempdir().unwrap();
        let recorder = diagnostic_recorder(temp.path());
        let success: Result<(), String> = Ok(());
        record_background_result_to(
            &recorder,
            "scanRuntimeStatus",
            "commands.scan_runtime_status.failed",
            &success,
        );
        assert!(recorder.store().read_events().unwrap().is_empty());

        let failure: Result<(), String> =
            Err("failed with sk-1234567890abcdefghijklmnop".to_string());
        record_background_result_to(
            &recorder,
            "scanRuntimeStatus",
            "commands.scan_runtime_status.failed",
            &failure,
        );

        let events = recorder.store().read_events().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_kind, DiagnosticEventKind::BackgroundFailure);
        assert!(!events[0]
            .safe_message
            .as_deref()
            .unwrap_or_default()
            .contains("sk-1234567890abcdefghijklmnop"));
    }

    #[test]
    fn process_close_preflight_emits_real_phases_in_order() {
        let root = CodexProcess {
            image_name: "ChatGPT.exe".to_string(),
            pid: 1234,
            parent_pid: 42,
            creation_time_100ns: Some(1),
        };
        let mut relay_listings: VecDeque<Result<Vec<CodexProcess>, String>> =
            VecDeque::from([Ok(vec![root]), Ok(Vec::new())]);
        let relay_captured = Cell::new(false);
        let relay_closed = Cell::new(false);
        let mut relay_phases = Vec::new();
        close_runtime_processes_with_progress(
            || {
                let processes = relay_listings.pop_front().expect("unexpected listing")?;
                if !processes.is_empty() {
                    relay_captured.set(true);
                }
                Ok(processes)
            },
            || {
                relay_closed.set(true);
                Ok(())
            },
            |phase, _| relay_phases.push(phase),
        )
        .unwrap();
        assert!(relay_captured.get());
        assert!(relay_closed.get());
        assert_eq!(
            relay_phases,
            vec![
                RuntimeSwitchPhase::DetectingApp,
                RuntimeSwitchPhase::ClosingApp,
            ]
        );

        let mut account_phases = Vec::new();
        close_runtime_processes_with_progress(
            || Ok(Vec::new()),
            || panic!("there is no managed process to close"),
            |phase, _| account_phases.push(phase),
        )
        .unwrap();
        assert_eq!(account_phases, vec![RuntimeSwitchPhase::DetectingApp]);
    }

    #[test]
    fn chatgpt_launch_target_capture_runs_only_on_the_first_process_listing() {
        let capture_calls = Cell::new(0);
        let mut captured = false;

        capture_chatgpt_launch_target_once(&mut captured, || {
            capture_calls.set(capture_calls.get() + 1);
        });
        capture_chatgpt_launch_target_once(&mut captured, || {
            capture_calls.set(capture_calls.get() + 1);
        });

        assert!(captured);
        assert_eq!(capture_calls.get(), 1);
    }

    #[test]
    fn chatgpt_launch_requires_a_durable_terminal_and_preserves_typed_failures() {
        let launch_calls = Cell::new(0);
        let terminal_failure = launch_chatgpt_after_durable_terminal(false, || {
            launch_calls.set(launch_calls.get() + 1);
            ChatGptLaunchReceipt {
                status: ChatGptLaunchStatus::Launched,
                message: None,
            }
        });

        assert_eq!(launch_calls.get(), 0);
        assert_eq!(terminal_failure.status, ChatGptLaunchStatus::Failed);
        assert!(terminal_failure
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("terminal record"));

        let activation_failure = launch_chatgpt_after_durable_terminal(true, || {
            launch_calls.set(launch_calls.get() + 1);
            ChatGptLaunchReceipt {
                status: ChatGptLaunchStatus::Failed,
                message: Some("injected activation failure".to_string()),
            }
        });

        assert_eq!(launch_calls.get(), 1);
        assert_eq!(
            activation_failure,
            ChatGptLaunchReceipt {
                status: ChatGptLaunchStatus::Failed,
                message: Some("injected activation failure".to_string()),
            }
        );
    }

    #[test]
    fn successful_noop_switch_still_requests_chatgpt_and_preserves_already_running() {
        assert!(successful_switch_requests_chatgpt_launch(false));

        let launch_calls = Cell::new(0);
        let receipt = launch_chatgpt_after_durable_terminal(true, || {
            launch_calls.set(launch_calls.get() + 1);
            ChatGptLaunchReceipt {
                status: ChatGptLaunchStatus::AlreadyRunning,
                message: None,
            }
        });

        assert_eq!(launch_calls.get(), 1);
        assert_eq!(receipt.status, ChatGptLaunchStatus::AlreadyRunning);
        assert_eq!(receipt.message, None);
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
    fn app_exit_waits_for_mutations_then_holds_the_lock_until_process_exit() {
        let root = tempdir().unwrap();
        let lock_path = root.path().join("mutation.lock");
        let coordinator = MutationCoordinator::new();
        let mutation = coordinator.acquire(&lock_path).unwrap();

        assert!(!prepare_app_exit_at(&coordinator, &lock_path).unwrap());
        drop(mutation);

        assert!(prepare_app_exit_at(&coordinator, &lock_path).unwrap());
        assert!(!coordinator.blocks_shutdown());
        assert!(coordinator.acquire(&lock_path).is_err());
    }

    #[test]
    fn stalled_app_exit_can_release_its_shutdown_reservation() {
        let root = tempdir().unwrap();
        let lock_path = root.path().join("mutation.lock");
        let coordinator = MutationCoordinator::new();

        assert!(prepare_app_exit_at(&coordinator, &lock_path).unwrap());
        assert!(coordinator.acquire(&lock_path).is_err());

        coordinator.release_shutdown_reservation();

        assert!(coordinator.acquire(&lock_path).is_ok());
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
            let operation_id = format!("switch-prewrite-{checkpoint_count}");
            let current_checkpoint = create_runtime_state_checkpoint_with_paths(
                current.path(),
                backup_root.path(),
                "switch-runtime-current",
                resolve_user_codex_paths(current.path()).unwrap(),
                &operation_id,
                CheckpointRole::Current,
            )
            .unwrap();
            let mut checkpoints = vec![current_checkpoint];
            if checkpoint_count == 2 {
                checkpoints.push(
                    create_state_checkpoint_with_paths(
                        shared.path(),
                        backup_root.path(),
                        "switch-runtime-shared",
                        resolve_user_codex_paths(shared.path()).unwrap(),
                        &operation_id,
                        CheckpointRole::Shared,
                    )
                    .unwrap(),
                );
            }
            let log = OperationLog::new(log_root.path().join("operations.jsonl"));
            let result: Result<(), String> = Err("injected prewrite failure".to_string());

            let record = record_runtime_switch_result_to_log(
                &log,
                &operation_id,
                1,
                &result,
                &checkpoints,
                std::collections::BTreeMap::new(),
                RuntimeSwitchOutcome::FailedBeforeWrite,
            )
            .unwrap();
            let cleanup = release_transient_checkpoints(
                Some(backup_root.path()),
                Some(&record),
                &checkpoints,
            );

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
            create_state_checkpoint_with_paths(
                current.path(),
                backup_root.path(),
                "sync-current",
                resolve_user_codex_paths(current.path()).unwrap(),
                "sync-prewrite",
                CheckpointRole::Current,
            )
            .unwrap(),
            create_state_checkpoint_with_paths(
                shared.path(),
                backup_root.path(),
                "sync-shared",
                resolve_user_codex_paths(shared.path()).unwrap(),
                "sync-prewrite",
                CheckpointRole::Shared,
            )
            .unwrap(),
        ];
        let log = OperationLog::new(log_root.path().join("operations.jsonl"));

        let record = record_sync_failure_to_log(
            &log,
            "sync-prewrite",
            OperationStatus::Failed,
            OperationPhase::Backup,
            1,
            &checkpoints,
        )
        .unwrap();
        let cleanup =
            release_transient_checkpoints(Some(backup_root.path()), Some(&record), &checkpoints);

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
            None,
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
        let checkpoint = create_state_checkpoint_with_paths(
            current.path(),
            backup_root.path(),
            "restore-sessions-visible",
            resolve_user_codex_paths(current.path()).unwrap(),
            "restore-visible-success",
            CheckpointRole::Visibility,
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
        let checkpoint = create_state_checkpoint_with_paths(
            current.path(),
            backup_root.path(),
            "restore-sessions-visible",
            resolve_user_codex_paths(current.path()).unwrap(),
            "restore-visible-log-failure",
            CheckpointRole::Visibility,
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
}

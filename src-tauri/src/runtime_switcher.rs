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
    backup::{
        create_runtime_state_checkpoint_with_paths, create_state_checkpoint_with_paths,
        load_process_state_checkpoint, restore_backup, BackupManifest, BackupScope,
        CheckpointCleanupSummary, CheckpointRole,
    },
    chat_process_state::repair_after_shutdown,
    codex_paths::{local_codex_paths, resolve_user_codex_paths, CodexPaths},
    config_patch::{plan_runtime_config_patch, ConfigPatchPlan, RuntimeConfigKind},
    file_ops::atomic_write,
    operation_log::operation_id,
    process_control::{
        ChatGptLaunchResult as ProcessChatGptLaunchResult,
        ChatGptLaunchStatus as ProcessChatGptLaunchStatus,
    },
    runtime_store::{RuntimeConfidence, RuntimeFiles, RuntimeKind, RuntimeMetadata, RuntimeStore},
    session_sync::{
        plan_runtime_session_storage_with_paths, preflight_session_database,
        runtime_switch_session_files_are_unchanged_with_paths,
        sync_shared_to_user_home_hot_with_paths, sync_shared_to_user_home_hot_with_policy,
        sync_shared_to_user_home_with_policy, sync_user_home_to_shared_with_policy,
        RuntimeSessionStoragePlan, SessionFileWritePolicy, SessionSyncResult,
    },
};

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ChatGptLaunchStatus {
    Launched,
    AlreadyRunning,
    Failed,
    NotRequested,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChatGptLaunchReceipt {
    pub status: ChatGptLaunchStatus,
    pub message: Option<String>,
}

impl ChatGptLaunchReceipt {
    fn not_requested() -> Self {
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
    pub backups: Vec<BackupReceiptSummary>,
    #[serde(skip)]
    pub(crate) backup_manifests: Vec<BackupManifest>,
    pub to_shared: SessionSyncResult,
    pub from_shared: SessionSyncResult,
    pub rolled_back: bool,
    pub warnings: Vec<String>,
    pub checkpoint_cleanup: CheckpointCleanupSummary,
    pub chat_process_state_repaired: bool,
    pub chatgpt_launch: ChatGptLaunchReceipt,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RuntimeSwitchPhase {
    PlanningSessions,
    DetectingApp,
    ClosingApp,
    VerifyingRelay,
    BackingUpCurrent,
    BackingUpShared,
    RepairingAppState,
    SyncingToShared,
    ApplyingRuntime,
    SyncingToCurrent,
    Verifying,
    RollingBack,
    CleaningCheckpoints,
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
    pub backups: Vec<BackupManifest>,
    pub outcome: RuntimeSwitchOutcome,
    pub operation_id: Option<String>,
}

pub(crate) struct RuntimeSwitchPlan {
    operation_id: String,
    runtime_files: RuntimeFiles,
    runtime: RuntimeMetadata,
    config_plan: ConfigPatchPlan,
    session_provider: String,
    codex_paths: CodexPaths,
    requires_change: bool,
    session_file_write_policy: SessionFileWritePolicy,
    session_storage_plan: RuntimeSessionStoragePlan,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct HotSessionSyncPlan {
    session_file_write_policy: SessionFileWritePolicy,
}

#[derive(Clone, Copy)]
struct RuntimeSwitchRoots<'a> {
    codex_home: &'a Path,
    backup_root: &'a Path,
    shared_home: &'a Path,
}

impl RuntimeSwitchPlan {
    pub(crate) fn requires_change(&self) -> bool {
        self.requires_change
    }

    pub(crate) fn codex_paths(&self) -> &CodexPaths {
        &self.codex_paths
    }

    pub(crate) fn current_backup_scope(&self) -> BackupScope {
        BackupScope::RuntimeState
    }

    pub(crate) fn shared_backup_scope(&self) -> BackupScope {
        BackupScope::StateOnly
    }

    pub(crate) fn session_storage_plan(&self) -> &RuntimeSessionStoragePlan {
        &self.session_storage_plan
    }
}

impl HotSessionSyncPlan {
    pub(crate) fn backup_scope(&self) -> BackupScope {
        BackupScope::StateOnly
    }
}

impl RuntimeSwitchFailure {
    fn new(message: String, backups: Vec<BackupManifest>) -> Self {
        Self {
            message,
            backups,
            outcome: RuntimeSwitchOutcome::FailedBeforeWrite,
            operation_id: None,
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
            operation_id: None,
        }
    }

    fn rollback_failed(message: String, backups: Vec<BackupManifest>) -> Self {
        Self {
            message,
            backups,
            outcome: RuntimeSwitchOutcome::RollbackFailed,
            operation_id: None,
        }
    }

    fn for_operation(mut self, operation_id: &str) -> Self {
        self.operation_id = Some(operation_id.to_string());
        self
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
    let plan = build_runtime_switch_plan(store, runtime_id, codex_home, shared_home)
        .map_err(RuntimeSwitchFailure::before_backup)?;
    let plan_operation_id = plan.operation_id.clone();
    let mut verify_processes_closed = verify_runtime_processes_closed;
    switch_runtime_files_from_plan(
        store,
        RuntimeSwitchRoots {
            codex_home,
            backup_root,
            shared_home,
        },
        SwitchFailurePoint::None,
        plan,
        &mut verify_processes_closed,
        on_progress,
    )
    .map_err(|failure| failure.for_operation(&plan_operation_id))
}

pub(crate) fn preflight_runtime_switch(
    store: &RuntimeStore,
    runtime_id: &str,
    codex_home: &Path,
    shared_home: &Path,
) -> Result<RuntimeSwitchPlan, String> {
    build_runtime_switch_plan(store, runtime_id, codex_home, shared_home)
}

#[cfg(test)]
pub(crate) fn preflight_runtime_session_sync(
    codex_home: &Path,
    shared_home: &Path,
) -> Result<(), String> {
    let current = resolve_user_codex_paths(codex_home)?;
    let shared = local_codex_paths(shared_home);
    preflight_runtime_session_sync_with_paths(&current, &shared)
}

pub(crate) fn preflight_runtime_session_sync_with_paths(
    current: &CodexPaths,
    shared: &CodexPaths,
) -> Result<(), String> {
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
    let plan_operation_id = plan.operation_id.clone();
    let mut verify_processes_closed = verify_runtime_processes_closed;
    switch_runtime_files_from_plan(
        store,
        RuntimeSwitchRoots {
            codex_home,
            backup_root,
            shared_home,
        },
        SwitchFailurePoint::None,
        plan,
        &mut verify_processes_closed,
        on_progress,
    )
    .map_err(|failure| failure.for_operation(&plan_operation_id))
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
    let plan = build_runtime_switch_plan(store, runtime_id, codex_home, shared_home)
        .map_err(RuntimeSwitchFailure::before_backup)?;
    let plan_operation_id = plan.operation_id.clone();
    let mut verify_processes_closed = verify_runtime_processes_closed;
    switch_runtime_files_from_plan(
        store,
        RuntimeSwitchRoots {
            codex_home,
            backup_root,
            shared_home,
        },
        failure_point,
        plan,
        &mut verify_processes_closed,
        on_progress,
    )
    .map_err(|failure| failure.for_operation(&plan_operation_id))
}

#[cfg(test)]
fn switch_runtime_files_with_process_gate_detailed(
    store: &RuntimeStore,
    runtime_id: &str,
    codex_home: &Path,
    backup_root: &Path,
    shared_home: &Path,
    verify_processes_closed: &mut dyn FnMut() -> Result<(), String>,
    on_progress: &mut dyn FnMut(RuntimeSwitchPhase),
) -> Result<RuntimeSwitchResult, RuntimeSwitchFailure> {
    let plan = build_runtime_switch_plan(store, runtime_id, codex_home, shared_home)
        .map_err(RuntimeSwitchFailure::before_backup)?;
    let plan_operation_id = plan.operation_id.clone();
    switch_runtime_files_from_plan(
        store,
        RuntimeSwitchRoots {
            codex_home,
            backup_root,
            shared_home,
        },
        SwitchFailurePoint::None,
        plan,
        verify_processes_closed,
        on_progress,
    )
    .map_err(|failure| failure.for_operation(&plan_operation_id))
}

fn switch_runtime_files_from_plan(
    store: &RuntimeStore,
    roots: RuntimeSwitchRoots<'_>,
    failure_point: SwitchFailurePoint,
    mut plan: RuntimeSwitchPlan,
    verify_processes_closed: &mut dyn FnMut() -> Result<(), String>,
    on_progress: &mut dyn FnMut(RuntimeSwitchPhase),
) -> Result<RuntimeSwitchResult, RuntimeSwitchFailure> {
    let RuntimeSwitchRoots {
        codex_home,
        backup_root,
        shared_home,
    } = roots;
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
            backup_manifests: Vec::new(),
            to_shared: empty_sync_result(),
            from_shared: empty_sync_result(),
            rolled_back: false,
            warnings: Vec::new(),
            checkpoint_cleanup: CheckpointCleanupSummary::default(),
            chat_process_state_repaired: false,
            chatgpt_launch: ChatGptLaunchReceipt::not_requested(),
        });
    }
    ensure_runtime_paths_unchanged(codex_home, &plan.codex_paths)
        .map_err(RuntimeSwitchFailure::before_backup)?;
    let shared_paths = local_codex_paths(shared_home);
    ensure_runtime_session_plan_unchanged(&plan, &shared_paths)
        .map_err(RuntimeSwitchFailure::before_backup)?;

    on_progress(RuntimeSwitchPhase::BackingUpCurrent);
    let current_backup = create_runtime_state_checkpoint_with_paths(
        codex_home,
        backup_root,
        "switch-runtime-current",
        plan.codex_paths.clone(),
        &operation_id,
        CheckpointRole::Current,
    )
    .map_err(RuntimeSwitchFailure::before_backup)?;
    on_progress(RuntimeSwitchPhase::BackingUpShared);
    let shared_backup = create_state_checkpoint_with_paths(
        shared_home,
        backup_root,
        "switch-runtime-shared",
        shared_paths.clone(),
        &operation_id,
        CheckpointRole::Shared,
    )
    .map_err(|message| RuntimeSwitchFailure::new(message, vec![current_backup.clone()]))?;
    let backups = vec![current_backup.clone(), shared_backup.clone()];

    verify_processes_closed()
        .map_err(|message| RuntimeSwitchFailure::new(message, backups.clone()))?;
    ensure_runtime_session_plan_unchanged(&plan, &shared_paths)
        .map_err(|message| RuntimeSwitchFailure::new(message, backups.clone()))?;
    let applied = (|| {
        on_progress(RuntimeSwitchPhase::RepairingAppState);
        let checkpoint_process_state = load_process_state_checkpoint(&current_backup, codex_home)?;
        verify_processes_closed()?;
        let chat_process_state_repaired =
            repair_after_shutdown(codex_home, checkpoint_process_state.as_deref())?;
        on_progress(RuntimeSwitchPhase::SyncingToShared);
        ensure_runtime_paths_unchanged(codex_home, &plan.codex_paths)?;
        ensure_shared_sessions_with_paths(&plan.codex_paths, &shared_paths)?;
        let to_shared = sync_user_home_to_shared_with_policy(
            &plan.codex_paths,
            &shared_paths,
            plan.session_file_write_policy,
        )?;
        let (config_plan, session_provider) =
            runtime_config_plan_for_home(&plan.runtime, &plan.runtime_files, codex_home)?;
        plan.config_plan = config_plan;
        plan.session_provider = session_provider;
        ensure_runtime_paths_unchanged(codex_home, &plan.codex_paths)?;
        verify_processes_closed()?;
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
        verify_processes_closed()?;
        on_progress(RuntimeSwitchPhase::SyncingToCurrent);
        let from_shared = sync_shared_to_user_home_with_policy(
            &shared_paths,
            &plan.codex_paths,
            &plan.session_provider,
            plan.session_file_write_policy,
        )?;
        on_progress(RuntimeSwitchPhase::Verifying);
        let verified = store.detect_active_runtime(codex_home)?;
        if verified.active_runtime_id.as_deref() != Some(runtime_id.as_str())
            || verified.confidence != RuntimeConfidence::Exact
        {
            return Err("runtime verification did not match the requested target".to_string());
        }
        let runtime = store.mark_used(&runtime_id)?;
        Ok((runtime, to_shared, from_shared, chat_process_state_repaired))
    })();

    match applied {
        Ok((runtime, to_shared, from_shared, chat_process_state_repaired)) => {
            Ok(RuntimeSwitchResult {
                operation_id,
                changed: true,
                runtime,
                backups: backups.iter().map(BackupReceiptSummary::from).collect(),
                backup_manifests: backups,
                to_shared,
                from_shared,
                rolled_back: false,
                warnings: Vec::new(),
                checkpoint_cleanup: CheckpointCleanupSummary::default(),
                chat_process_state_repaired,
                chatgpt_launch: ChatGptLaunchReceipt::not_requested(),
            })
        }
        Err(error) => {
            on_progress(RuntimeSwitchPhase::RollingBack);
            if let Err(gate_error) = verify_processes_closed() {
                return Err(RuntimeSwitchFailure::rollback_failed(
                    format!(
                        "{error}; rollback was not attempted because ChatGPT/Codex activity resumed: {gate_error}"
                    ),
                    backups,
                ));
            }
            let current_restore = restore_backup(&current_backup.backup_dir, codex_home);
            let shared_restore = restore_backup(&shared_backup.backup_dir, shared_home);
            match (current_restore, shared_restore) {
                (Ok(_), Ok(_)) => Err(RuntimeSwitchFailure::rolled_back(
                    format!(
                        "{error}; rolled back runtime and database state to verified snapshots; monotonic session JSONL/index additions may remain for retry"
                    ),
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

fn ensure_runtime_session_plan_unchanged(
    plan: &RuntimeSwitchPlan,
    shared_paths: &CodexPaths,
) -> Result<(), String> {
    let observed = plan_runtime_session_storage_with_paths(
        &plan.codex_paths,
        shared_paths,
        &plan.session_provider,
    )?;
    if observed == plan.session_storage_plan {
        Ok(())
    } else {
        Err(
            "session write set changed after the closed-session capacity check; retry the runtime switch"
                .to_string(),
        )
    }
}

fn build_runtime_switch_plan(
    store: &RuntimeStore,
    runtime_id: &str,
    codex_home: &Path,
    shared_home: &Path,
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
    let shared_paths = local_codex_paths(shared_home);
    let session_storage_plan = if requires_change {
        plan_runtime_session_storage_with_paths(&codex_paths, &shared_paths, &session_provider)?
    } else {
        RuntimeSessionStoragePlan::default()
    };
    let session_file_write_policy = if requires_change && session_storage_plan.is_empty() {
        SessionFileWritePolicy::Deny
    } else {
        SessionFileWritePolicy::Allow
    };
    Ok(RuntimeSwitchPlan {
        operation_id,
        runtime_files,
        runtime,
        config_plan,
        session_provider,
        codex_paths,
        requires_change,
        session_file_write_policy,
        session_storage_plan,
    })
}

fn ensure_runtime_paths_unchanged(codex_home: &Path, expected: &CodexPaths) -> Result<(), String> {
    if resolve_user_codex_paths(codex_home)? == *expected {
        Ok(())
    } else {
        Err("Codex paths changed during runtime switch preflight; retry".to_string())
    }
}

#[cfg(not(test))]
fn verify_runtime_processes_closed() -> Result<(), String> {
    let (managed, standalone) = crate::process_control::list_codex_process_inventory()?;
    match (managed.is_empty(), standalone.is_empty()) {
        (true, true) => Ok(()),
        (false, true) => Err(
            "ChatGPT started during runtime switch; close it and retry before files are changed"
                .to_string(),
        ),
        (true, false) => Err(
            "Codex CLI started during runtime switch; close it and retry before files are changed"
                .to_string(),
        ),
        (false, false) => Err(
            "ChatGPT and Codex CLI started during runtime switch; close them and retry before files are changed"
                .to_string(),
        ),
    }
}

#[cfg(test)]
fn verify_runtime_processes_closed() -> Result<(), String> {
    Ok(())
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
    let current_paths = resolve_user_codex_paths(codex_home)?;
    let shared_paths = local_codex_paths(shared_home);
    sync_home_with_shared_with_paths(&current_paths, &shared_paths)
}

pub(crate) fn sync_home_with_shared_with_paths(
    current_paths: &CodexPaths,
    shared_paths: &CodexPaths,
) -> Result<SessionSyncResult, String> {
    sync_home_with_shared_preflighted_with_paths(
        current_paths,
        shared_paths,
        HotSessionSyncPlan {
            session_file_write_policy: SessionFileWritePolicy::Allow,
        },
    )
}

pub(crate) fn preflight_hot_session_sync_with_paths(
    current_paths: &CodexPaths,
    shared_paths: &CodexPaths,
) -> Result<HotSessionSyncPlan, String> {
    let session_file_write_policy = if runtime_switch_session_files_are_unchanged_with_paths(
        current_paths,
        shared_paths,
        None,
    )? {
        SessionFileWritePolicy::Deny
    } else {
        SessionFileWritePolicy::Allow
    };
    Ok(HotSessionSyncPlan {
        session_file_write_policy,
    })
}

pub(crate) fn sync_home_with_shared_preflighted_with_paths(
    current_paths: &CodexPaths,
    shared_paths: &CodexPaths,
    plan: HotSessionSyncPlan,
) -> Result<SessionSyncResult, String> {
    let session_provider = session_provider_from_home(&current_paths.codex_home)?;
    ensure_shared_sessions_with_paths(current_paths, shared_paths)?;
    let to_shared = sync_user_home_to_shared_with_policy(
        current_paths,
        shared_paths,
        plan.session_file_write_policy,
    )?;
    let from_shared = match plan.session_file_write_policy {
        SessionFileWritePolicy::Allow => {
            sync_shared_to_user_home_hot_with_paths(shared_paths, current_paths, &session_provider)?
        }
        SessionFileWritePolicy::Deny => sync_shared_to_user_home_hot_with_policy(
            shared_paths,
            current_paths,
            &session_provider,
            SessionFileWritePolicy::Deny,
        )?,
    };
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
    })
}

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
        persistent_session_bytes_added: 0,
        persistent_session_bytes_reclaimed: 0,
        obsolete_provider_slots: Vec::new(),
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
    use std::{cell::Cell, fs};

    use rusqlite::Connection;
    use tempfile::tempdir;

    use crate::runtime_store::{
        RelayRuntimeInput, RuntimeStore, PLUS_RUNTIME_ID, RELAY_RUNTIME_ID,
    };
    use crate::{
        backup::{create_state_backup, BackupScope},
        codex_paths::{local_codex_paths, resolve_user_codex_paths},
    };

    use super::{
        preflight_hot_session_sync_with_paths, preflight_runtime_session_sync,
        preflight_runtime_switch, switch_runtime_files,
        switch_runtime_files_detailed_with_progress,
        switch_runtime_files_preflighted_with_progress,
        switch_runtime_files_with_failure_and_progress_detailed,
        switch_runtime_files_with_process_gate_detailed, sync_home_with_shared,
        sync_home_with_shared_preflighted_with_paths, RuntimeSwitchOutcome, RuntimeSwitchPhase,
        SwitchFailurePoint,
    };

    const THREAD_A: &str = "019f8ced-fc55-7a93-8cc5-a18d5b96b4a6";
    const THREAD_FAST: &str = "019f8ced-fc55-7a93-8cc5-a18d5b96b4a7";
    const THREAD_CURRENT: &str = "019f8ced-fc55-7a93-8cc5-a18d5b96b4a8";
    const THREAD_SHARED: &str = "019f8ced-fc55-7a93-8cc5-a18d5b96b4a9";
    const THREAD_FAST_DRIFT: &str = "019f8ced-fc55-7a93-8cc5-a18d5b96b4aa";
    const THREAD_DRIFT: &str = "019f8ced-fc55-7a93-8cc5-a18d5b96b4ab";

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
        create_state_db(home.path(), THREAD_A, &rollout);
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
        let legacy_relative = format!(
            "sessions/2026/06/23/rollout-2026-06-23T11-00-00-{THREAD_A}-imported-deadbeef.jsonl"
        );
        let rollout = home.path().join(&legacy_relative);
        fs::create_dir_all(rollout.parent().unwrap()).unwrap();
        let rollout_bytes = format!(r#"{{"type":"session_meta","payload":{{"id":"{THREAD_A}"}}}}"#);
        fs::write(&rollout, &rollout_bytes).unwrap();
        let rollout_modified_before = fs::metadata(&rollout).unwrap().modified().unwrap();
        create_state_db(home.path(), THREAD_A, &rollout);
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

        let mut result = switch_runtime_files_detailed_with_progress(
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
                RuntimeSwitchPhase::RepairingAppState,
                RuntimeSwitchPhase::SyncingToShared,
                RuntimeSwitchPhase::ApplyingRuntime,
                RuntimeSwitchPhase::SyncingToCurrent,
                RuntimeSwitchPhase::Verifying,
            ]
        );
        assert_eq!(result.runtime.id, RELAY_RUNTIME_ID);
        assert_eq!(result.backups.len(), 2);
        assert_eq!(result.backups[0].scope, BackupScope::RuntimeState);
        assert_eq!(result.backups[1].scope, BackupScope::StateOnly);
        assert!(result
            .backups
            .iter()
            .all(|backup| backup.tracked_database_count == 1));
        assert!(result.backup_manifests.iter().all(|backup| {
            backup.tracked_databases == vec!["state_5.sqlite"]
                && !backup.files.iter().any(|file| {
                    file.relative_path == std::path::Path::new("session_index.jsonl")
                        || file.relative_path.starts_with("sessions")
                        || matches!(
                            file.relative_path.to_string_lossy().as_ref(),
                            "goals_1.sqlite" | "memories_1.sqlite" | "logs_2.sqlite"
                        )
                })
        }));
        assert!(result.backups[0].backup_dir.join("manifest.json").exists());
        let sample_file = result.backup_manifests[0].files.first().unwrap().clone();
        result.backup_manifests[0].files = vec![sample_file; 4_096];
        let wire = serde_json::to_string(&result).unwrap();
        assert!(
            wire.len() < 16_384,
            "compact switch receipt grew to {} bytes",
            wire.len()
        );
        for forbidden in [
            "\"files\"",
            "\"sourcePath\"",
            "\"backupPath\"",
            "\"sha256\"",
            "\"trackedDatabases\"",
        ] {
            assert!(!wire.contains(forbidden), "{forbidden} leaked into IPC");
        }
        assert!(wire.contains("\"trackedDatabaseCount\""));
        assert!(wire.contains("\"chatgptLaunch\":{\"status\":\"notRequested\",\"message\":null}"));
        assert!(fs::read_to_string(home.path().join("auth.json"))
            .unwrap()
            .contains("sk-fake-relay"));
        let switched_config = fs::read_to_string(home.path().join("config.toml")).unwrap();
        assert!(switched_config.contains("model_provider = \"openai_custom\""));
        assert!(!switched_config.contains("env_key ="));
        assert!(!switched_config.contains("api_key ="));
        let conn = Connection::open(home.path().join("state_5.sqlite")).unwrap();
        let (provider, active_rollout): (String, String) = conn
            .query_row(
                "SELECT model_provider, rollout_path FROM threads WHERE id = ?1",
                [THREAD_A],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(provider, "openai_custom");
        let active_rollout = std::path::PathBuf::from(active_rollout);
        assert_ne!(active_rollout, fs::canonicalize(&rollout).unwrap());
        let active_name = active_rollout.file_name().unwrap().to_string_lossy();
        assert!(active_name.starts_with("rollout-2026-06-23T"));
        assert!(active_name.ends_with(&format!("-{THREAD_A}.jsonl")));
        assert!(!active_name.contains("-imported-"));
        assert!(fs::read_to_string(&active_rollout)
            .unwrap()
            .contains("\"model_provider\":\"openai_custom\""));
        assert_eq!(fs::read_to_string(&rollout).unwrap(), rollout_bytes);
        assert_eq!(
            fs::metadata(&rollout).unwrap().modified().unwrap(),
            rollout_modified_before
        );
        assert!(shared.path().join("state_5.sqlite").exists());
        assert!(shared.path().join(&legacy_relative).exists());
        assert!(home.path().join(&legacy_relative).exists());
    }

    #[test]
    fn equivalent_session_files_use_state_only_switch_checkpoints() {
        let home = tempdir().unwrap();
        let shared = tempdir().unwrap();
        let current_rollout = home
            .path()
            .join("sessions/2026/07/26/rollout-fast-path.jsonl");
        let shared_rollout = shared
            .path()
            .join("sessions/2026/07/26/rollout-fast-path.jsonl");
        fs::create_dir_all(current_rollout.parent().unwrap()).unwrap();
        fs::create_dir_all(shared_rollout.parent().unwrap()).unwrap();
        let rollout_bytes =
            format!(r#"{{"type":"session_meta","payload":{{"id":"{THREAD_FAST}"}}}}"#);
        fs::write(&current_rollout, &rollout_bytes).unwrap();
        fs::write(&shared_rollout, &rollout_bytes).unwrap();
        create_state_db(home.path(), THREAD_FAST, &current_rollout);
        create_state_db(shared.path(), THREAD_FAST, &shared_rollout);
        fs::write(
            home.path().join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"fake-plus"}}"#,
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
        let backup_root = tempdir().unwrap();
        let current_modified = fs::metadata(&current_rollout).unwrap().modified().unwrap();
        let shared_modified = fs::metadata(&shared_rollout).unwrap().modified().unwrap();

        let result = switch_runtime_files(
            &store,
            RELAY_RUNTIME_ID,
            home.path(),
            backup_root.path(),
            shared.path(),
        )
        .unwrap();

        assert_eq!(result.backups[0].scope, BackupScope::RuntimeState);
        assert_eq!(result.backups[1].scope, BackupScope::StateOnly);
        assert!(result.backup_manifests.iter().all(|backup| {
            backup.files.iter().all(|file| {
                file.relative_path != std::path::Path::new("session_index.jsonl")
                    && !file.relative_path.starts_with("sessions")
            })
        }));
        assert_eq!(fs::read_to_string(&current_rollout).unwrap(), rollout_bytes);
        assert_eq!(fs::read_to_string(&shared_rollout).unwrap(), rollout_bytes);
        assert_eq!(
            fs::metadata(&current_rollout).unwrap().modified().unwrap(),
            current_modified
        );
        assert_eq!(
            fs::metadata(&shared_rollout).unwrap().modified().unwrap(),
            shared_modified
        );
    }

    #[test]
    fn changed_hot_sync_uses_state_only_checkpoints_while_copying_monotonic_files() {
        let current = tempdir().unwrap();
        let shared = tempdir().unwrap();
        let backup_root = tempdir().unwrap();
        let current_rollout = current
            .path()
            .join("sessions/2026/07/26/rollout-current.jsonl");
        let shared_rollout = shared
            .path()
            .join("sessions/2026/07/26/rollout-shared.jsonl");
        fs::create_dir_all(current_rollout.parent().unwrap()).unwrap();
        fs::create_dir_all(shared_rollout.parent().unwrap()).unwrap();
        let current_bytes =
            format!(r#"{{"type":"session_meta","payload":{{"id":"{THREAD_CURRENT}"}}}}"#);
        let shared_bytes =
            format!(r#"{{"type":"session_meta","payload":{{"id":"{THREAD_SHARED}"}}}}"#);
        fs::write(&current_rollout, &current_bytes).unwrap();
        fs::write(&shared_rollout, &shared_bytes).unwrap();
        create_state_db(current.path(), THREAD_CURRENT, &current_rollout);
        create_state_db(shared.path(), THREAD_SHARED, &shared_rollout);
        let current_paths = resolve_user_codex_paths(current.path()).unwrap();
        let shared_paths = local_codex_paths(shared.path());
        let plan = preflight_hot_session_sync_with_paths(&current_paths, &shared_paths).unwrap();

        assert_eq!(plan.backup_scope(), BackupScope::StateOnly);
        let checkpoints = [
            create_state_backup(current.path(), backup_root.path(), "sync-current").unwrap(),
            create_state_backup(shared.path(), backup_root.path(), "sync-shared").unwrap(),
        ];
        assert!(checkpoints.iter().all(|checkpoint| {
            checkpoint.scope == BackupScope::StateOnly
                && checkpoint.files.iter().all(|file| {
                    file.relative_path != std::path::Path::new("session_index.jsonl")
                        && !file.relative_path.starts_with("sessions")
                })
        }));

        let result =
            sync_home_with_shared_preflighted_with_paths(&current_paths, &shared_paths, plan)
                .unwrap();

        assert_eq!(result.copied_session_files, 2);
        assert_eq!(fs::read_to_string(&current_rollout).unwrap(), current_bytes);
        assert_eq!(fs::read_to_string(&shared_rollout).unwrap(), shared_bytes);
        let current_conn = Connection::open(current.path().join("state_5.sqlite")).unwrap();
        let imported_to_current: String = current_conn
            .query_row(
                "SELECT rollout_path FROM threads WHERE id = ?1",
                [THREAD_SHARED],
                |row| row.get(0),
            )
            .unwrap();
        let imported_to_current = fs::read_to_string(imported_to_current).unwrap();
        assert!(imported_to_current.contains(THREAD_SHARED));
        assert!(imported_to_current.contains("\"model_provider\":\"openai\""));
        let imported_to_shared = fs::read_to_string(
            shared
                .path()
                .join("sessions/2026/07/26/rollout-current.jsonl"),
        )
        .unwrap();
        assert!(imported_to_shared.contains(THREAD_CURRENT));
    }

    #[test]
    fn hot_sync_fast_path_rejects_rollout_drift_without_touching_shared_payload() {
        let current = tempdir().unwrap();
        let shared = tempdir().unwrap();
        let current_rollout = current
            .path()
            .join("sessions/2026/07/26/rollout-fast-drift.jsonl");
        let shared_rollout = shared
            .path()
            .join("sessions/2026/07/26/rollout-fast-drift.jsonl");
        fs::create_dir_all(current_rollout.parent().unwrap()).unwrap();
        fs::create_dir_all(shared_rollout.parent().unwrap()).unwrap();
        let original =
            format!(r#"{{"type":"session_meta","payload":{{"id":"{THREAD_FAST_DRIFT}"}}}}"#);
        fs::write(&current_rollout, &original).unwrap();
        fs::write(&shared_rollout, &original).unwrap();
        create_state_db(current.path(), THREAD_FAST_DRIFT, &current_rollout);
        create_state_db(shared.path(), THREAD_FAST_DRIFT, &shared_rollout);
        let current_paths = resolve_user_codex_paths(current.path()).unwrap();
        let shared_paths = local_codex_paths(shared.path());
        let plan = preflight_hot_session_sync_with_paths(&current_paths, &shared_paths).unwrap();
        assert_eq!(plan.backup_scope(), BackupScope::StateOnly);
        let grown = [
            original.as_bytes(),
            b"\n{\"type\":\"response_item\",\"payload\":{\"text\":\"late\"}}\n",
        ]
        .concat();
        fs::write(&current_rollout, &grown).unwrap();

        let error =
            sync_home_with_shared_preflighted_with_paths(&current_paths, &shared_paths, plan)
                .unwrap_err();

        assert!(
            error.contains("changed after fast-path planning"),
            "{error}"
        );
        assert_eq!(fs::read_to_string(&shared_rollout).unwrap(), original);
        assert_eq!(fs::read(&current_rollout).unwrap(), grown);
    }

    #[test]
    fn fast_path_late_jsonl_drift_fails_before_session_file_writes() {
        let home = tempdir().unwrap();
        let shared = tempdir().unwrap();
        let current_rollout = home.path().join(format!(
            "sessions/2026/07/26/rollout-2026-07-26T12-00-00-{THREAD_DRIFT}.jsonl"
        ));
        let shared_rollout = shared.path().join(format!(
            "sessions/2026/07/26/rollout-2026-07-26T12-00-00-{THREAD_DRIFT}.jsonl"
        ));
        fs::create_dir_all(current_rollout.parent().unwrap()).unwrap();
        fs::create_dir_all(shared_rollout.parent().unwrap()).unwrap();
        let original = format!(
            r#"{{"type":"session_meta","payload":{{"id":"{THREAD_DRIFT}","model_provider":"openai_custom"}}}}"#
        );
        fs::write(&current_rollout, &original).unwrap();
        fs::write(&shared_rollout, &original).unwrap();
        create_state_db(home.path(), THREAD_DRIFT, &current_rollout);
        create_state_db(shared.path(), THREAD_DRIFT, &shared_rollout);
        fs::write(
            home.path().join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"fake-plus"}}"#,
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
        let plan =
            preflight_runtime_switch(&store, RELAY_RUNTIME_ID, home.path(), shared.path()).unwrap();
        let late = [
            original.as_bytes(),
            b"\n{\"type\":\"response_item\",\"payload\":{\"text\":\"late\"}}\n",
        ]
        .concat();
        fs::write(&current_rollout, &late).unwrap();
        let current_modified = fs::metadata(&current_rollout).unwrap().modified().unwrap();
        let shared_modified = fs::metadata(&shared_rollout).unwrap().modified().unwrap();
        let backup_root = tempdir().unwrap();

        let failure = switch_runtime_files_preflighted_with_progress(
            &store,
            home.path(),
            backup_root.path(),
            shared.path(),
            plan,
            &mut |_| {},
        )
        .unwrap_err();

        assert_eq!(failure.outcome, RuntimeSwitchOutcome::FailedBeforeWrite);
        assert_eq!(
            failure.message,
            "session write set changed after the closed-session capacity check; retry the runtime switch"
        );
        assert!(failure.backups.is_empty());
        assert!(fs::read_dir(backup_root.path()).unwrap().next().is_none());
        assert_eq!(fs::read(&current_rollout).unwrap(), late);
        assert_eq!(fs::read_to_string(&shared_rollout).unwrap(), original);
        assert_eq!(
            fs::metadata(&current_rollout).unwrap().modified().unwrap(),
            current_modified
        );
        assert_eq!(
            fs::metadata(&shared_rollout).unwrap().modified().unwrap(),
            shared_modified
        );
    }

    #[test]
    fn process_restart_before_final_current_write_blocks_rollback_without_syncing_to_current() {
        let home = tempdir().unwrap();
        let rollout = home.path().join("sessions/2026/06/23/rollout-a.jsonl");
        fs::create_dir_all(rollout.parent().unwrap()).unwrap();
        fs::write(
            &rollout,
            format!(r#"{{"type":"session_meta","payload":{{"id":"{THREAD_A}"}}}}"#),
        )
        .unwrap();
        create_state_db(home.path(), THREAD_A, &rollout);
        let original_auth =
            br#"{"auth_mode":"chatgpt","tokens":{"access_token":"fake-plus"}}"#.to_vec();
        let original_config = b"model = \"gpt-5.5\"\n".to_vec();
        fs::write(home.path().join("auth.json"), &original_auth).unwrap();
        fs::write(home.path().join("config.toml"), &original_config).unwrap();

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
        let gate_calls = Cell::new(0);
        let mut phases = Vec::new();
        let error = switch_runtime_files_with_process_gate_detailed(
            &store,
            RELAY_RUNTIME_ID,
            home.path(),
            backup_root.path(),
            shared.path(),
            &mut || {
                let call = gate_calls.get() + 1;
                gate_calls.set(call);
                if call >= 3 {
                    Err("standalone Codex CLI restarted".to_string())
                } else {
                    Ok(())
                }
            },
            &mut |phase| phases.push(phase),
        )
        .unwrap_err();

        assert_eq!(gate_calls.get(), 4);
        assert_eq!(error.outcome, RuntimeSwitchOutcome::RollbackFailed);
        assert!(error.message.contains("rollback was not attempted"));
        assert_eq!(
            phases,
            vec![
                RuntimeSwitchPhase::BackingUpCurrent,
                RuntimeSwitchPhase::BackingUpShared,
                RuntimeSwitchPhase::RepairingAppState,
                RuntimeSwitchPhase::SyncingToShared,
                RuntimeSwitchPhase::RollingBack,
            ]
        );
        assert_eq!(
            fs::read(home.path().join("auth.json")).unwrap(),
            original_auth
        );
        assert_eq!(
            fs::read(home.path().join("config.toml")).unwrap(),
            original_config
        );
        assert!(shared.path().join("state_5.sqlite").exists());
    }

    #[test]
    fn switching_back_to_account_restores_account_auth_and_config_without_relay_provider() {
        let home = tempdir().unwrap();
        let rollout = home.path().join("sessions/2026/06/23/rollout-a.jsonl");
        fs::create_dir_all(rollout.parent().unwrap()).unwrap();
        fs::write(
            &rollout,
            format!(r#"{{"type":"session_meta","payload":{{"id":"{THREAD_A}"}}}}"#),
        )
        .unwrap();
        create_state_db(home.path(), THREAD_A, &rollout);
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
        let process_state = home
            .path()
            .join(crate::chat_process_state::CHAT_PROCESS_STATE_RELATIVE_PATH);
        fs::create_dir_all(process_state.parent().unwrap()).unwrap();
        fs::write(&process_state, vec![0_u8; 4096]).unwrap();
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
        assert!(result.chat_process_state_repaired);
        assert_eq!(fs::read(&process_state).unwrap(), b"[]");
        assert!(result.backup_manifests[0].tracked_process_state);
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
    fn failure_after_runtime_files_restores_state_but_keeps_monotonic_session_files() {
        let home = tempdir().unwrap();
        let rollout = home.path().join("sessions/2026/07/13/rollout-a.jsonl");
        fs::create_dir_all(rollout.parent().unwrap()).unwrap();
        fs::write(
            &rollout,
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"{THREAD_A}","model_provider":"openai"}}}}"#
            ),
        )
        .unwrap();
        create_state_db(home.path(), THREAD_A, &rollout);
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
        let process_state = home
            .path()
            .join(crate::chat_process_state::CHAT_PROCESS_STATE_RELATIVE_PATH);
        fs::create_dir_all(process_state.parent().unwrap()).unwrap();
        let original_process_state = vec![0_u8; 4096];
        fs::write(&process_state, &original_process_state).unwrap();
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
                RuntimeSwitchPhase::RepairingAppState,
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
        assert_eq!(fs::read(&process_state).unwrap(), original_process_state);
        assert_eq!(fs::read(&rollout).unwrap(), original_rollout);
        assert!(!shared.join("state_5.sqlite").exists());
        assert_eq!(
            fs::read(shared.join("sessions/2026/07/13/rollout-a.jsonl")).unwrap(),
            original_rollout
        );
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
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"{THREAD_CURRENT}","model_provider":"openai"}}}}"#
            ),
        )
        .unwrap();
        create_state_db(home.path(), THREAD_CURRENT, &rollout);
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
            format!(r#"{{"type":"session_meta","payload":{{"id":"{THREAD_SHARED}"}}}}"#);
        fs::write(&shared_rollout, &original_shared_rollout).unwrap();
        create_state_db(shared.path(), THREAD_SHARED, &shared_rollout);
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
                RuntimeSwitchPhase::RepairingAppState,
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
        assert_eq!(
            fs::read_to_string(&shared_rollout).unwrap(),
            original_shared_rollout
        );
        assert!(shared
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
        assert_eq!(thread_ids, vec![THREAD_SHARED]);
    }

    #[test]
    fn mode_only_match_is_not_treated_as_an_exact_no_op() {
        let home = tempdir().unwrap();
        let rollout = home.path().join("sessions/2026/07/13/rollout-a.jsonl");
        fs::create_dir_all(rollout.parent().unwrap()).unwrap();
        fs::write(
            &rollout,
            format!(r#"{{"type":"session_meta","payload":{{"id":"{THREAD_A}"}}}}"#),
        )
        .unwrap();
        create_state_db(home.path(), THREAD_A, &rollout);
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
            format!(r#"{{"type":"session_meta","payload":{{"id":"{THREAD_A}"}}}}"#),
        )
        .unwrap();
        create_state_db(home.path(), THREAD_A, &rollout);
        fs::write(
            home.path().join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"saved-account"}}"#,
        )
        .unwrap();
        fs::write(home.path().join("config.toml"), "model = \"gpt-5.5\"\n").unwrap();
        let process_state = home
            .path()
            .join(crate::chat_process_state::CHAT_PROCESS_STATE_RELATIVE_PATH);
        fs::create_dir_all(process_state.parent().unwrap()).unwrap();
        let corrupt_process_state = vec![0_u8; 128];
        fs::write(&process_state, &corrupt_process_state).unwrap();
        let store_root = tempdir().unwrap();
        let store = RuntimeStore::new(store_root.path().join("runtimes"));
        store.import_plus_from_home(home.path(), false).unwrap();
        let backup_root = tempdir().unwrap();
        let shared = tempdir().unwrap();

        let plan =
            preflight_runtime_switch(&store, PLUS_RUNTIME_ID, home.path(), shared.path()).unwrap();
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
        assert!(!result.chat_process_state_repaired);
        assert_eq!(fs::read(process_state).unwrap(), corrupt_process_state);
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
        let shared = tempdir().unwrap();
        let plan =
            preflight_runtime_switch(&store, PLUS_RUNTIME_ID, home.path(), shared.path()).unwrap();
        assert!(!plan.requires_change());
        fs::write(
            home.path().join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"different-account"}}"#,
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
            format!(r#"{{"type":"session_meta","payload":{{"id":"{THREAD_A}"}}}}"#),
        )
        .unwrap();
        create_state_db(home.path(), THREAD_A, &rollout);
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
        let shared = tempdir().unwrap();
        let plan =
            preflight_runtime_switch(&store, RELAY_RUNTIME_ID, home.path(), shared.path()).unwrap();
        fs::write(
            home.path().join("config.toml"),
            "model = \"gpt-5.5\"\n[features]\nnew_global = true\n",
        )
        .unwrap();
        let backup_root = tempdir().unwrap();

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
            format!(r#"{{"type":"session_meta","payload":{{"id":"{THREAD_A}"}}}}"#),
        )
        .unwrap();
        create_state_db(home.path(), THREAD_A, &rollout);
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
        let plan =
            preflight_runtime_switch(&store, RELAY_RUNTIME_ID, home.path(), shared.path()).unwrap();
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
            "Codex paths changed during runtime switch preflight; retry"
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
            format!(r#"{{"type":"session_meta","payload":{{"id":"{THREAD_A}"}}}}"#),
        )
        .unwrap();
        create_state_db(home.path(), THREAD_A, &rollout);
        let shared = tempdir().unwrap();

        let first = sync_home_with_shared(home.path(), shared.path()).unwrap();
        let second = sync_home_with_shared(home.path(), shared.path()).unwrap();

        assert_eq!(first.inserted_threads, 1);
        assert_eq!(second.inserted_threads, 0);
        assert!(second.duplicate_threads > 0);
    }
}

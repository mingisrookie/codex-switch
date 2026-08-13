use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
};

use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    file_ops::{atomic_copy, atomic_create, atomic_write},
    operation_log::timestamp_millis,
};

use super::{
    bounded_file::read_regular_file_bounded,
    catalog::discover_database_catalog,
    conflict::{resolve_conflict_by_id, ConflictVersion},
    marker::{inspect_provider_marker, provider_marker_path},
    migration::{MigrationPreflightReport, MigrationSessionAction},
    migration_apply::{
        cleanup_migration_staging, merge_database_views, sqlite_sidecars_absent,
        stable_file_digest, MigrationApplyPlan, MigrationDatabaseApplyEntry,
        MigrationSessionApplyEntry,
    },
    migration_backup::{
        create_migration_backup, verify_migration_backup, verify_migration_backup_sources,
        verify_migration_backup_with_isolated_restore, MigrationBackupEntry,
        MigrationBackupEntryKind, MigrationBackupManifest, MigrationBackupSource,
        MigrationBackupStatus, MigrationRuntimeVerification,
    },
    model::{FileOrigin, MarkerStatus, SessionRelation},
    operation_ledger::{
        LedgerDatabaseSnapshot, LedgerFileSnapshot, LedgerRollbackStep, OperationLedgerStore,
        RollbackActionKind, SessionStorageOperationKind, SessionStorageOperationPhase,
    },
    reference_graph::path_key,
    semantic::read_semantic_session,
    write_barrier::{
        classify_handle_create_crash_state, parent_directory_identity_at_path,
        recover_handle_create, regular_file_identity_at_path, same_persisted_regular_file_identity,
        stage_handle_hardlink_create, DestructiveFileGuard, HandleCreateCrashState,
        HandleCreateIdentityBindings, HandleCreatePaths, HandleCreateRecoveryDecision,
        HandleReplaceIdentityBindings, HandleReplacePaths, RegularFileIdentity,
        WriteExclusionGuard,
    },
};

const CONFLICT_RESOLUTION_SCHEMA_VERSION: u32 = 2;
const MAX_CONFLICT_RESOLUTION_PLAN_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CONFLICT_REPLACEMENT_PHASE_BYTES: u64 = 1024 * 1024;
const CONFLICT_REPLACEMENT_PHASE_CIPHERTEXT_MAGIC: &[u8] = b"CS-CONFLICT-REPLACE-PHASE-1\0";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ConflictResolutionAction {
    Defer,
    UseNewer,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ConflictResolutionStatus {
    Deferred,
    Resolved,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConflictResolutionReceipt {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    pub migration_operation_id: String,
    pub conflict_id: String,
    pub status: ConflictResolutionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chosen_version: Option<ConflictVersion>,
    pub canonical_updated: bool,
    pub database_view_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_expires_at_ms: Option<u128>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_verification: Option<MigrationRuntimeVerification>,
    pub validated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConflictCleanupEntry {
    pub path: PathBuf,
    pub backup_payload: PathBuf,
    pub bytes: u64,
    pub sha256: String,
    pub(crate) parent_identity: RegularFileIdentity,
    pub(crate) original_identity: RegularFileIdentity,
    pub ownership_witness_path: PathBuf,
    pub tombstone_path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker_backup_payload: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) marker_parent_identity: Option<RegularFileIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) marker_original_identity: Option<RegularFileIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker_ownership_witness_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker_tombstone_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConflictDatabaseEntry {
    pub apply: MigrationDatabaseApplyEntry,
    pub live_before_sha256: String,
    pub(crate) live_before_identity: RegularFileIdentity,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ConflictReplacementKind {
    CanonicalSession,
    RuntimeDatabase,
}

/// A deterministic target-local replacement identity persisted before the
/// first live write. The witness remains a hard link to the exact published
/// object, recovery owns the exact previous object, and tombstone receives the
/// published object during rollback. No path in this structure may be chosen
/// after apply starts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConflictReplacementPlan {
    pub kind: ConflictReplacementKind,
    pub target_path: PathBuf,
    pub source_path: PathBuf,
    pub original_sha256: String,
    pub(crate) parent_identity: RegularFileIdentity,
    pub(crate) original_identity: RegularFileIdentity,
    pub replacement_sha256: String,
    pub original_witness_path: PathBuf,
    pub replacement_witness_path: PathBuf,
    pub staging_path: PathBuf,
    pub recovery_path: PathBuf,
    pub tombstone_path: PathBuf,
}

/// Deterministic, target-local names and exact ownership bindings for creating
/// a previously absent canonical session. `source_identity` is persisted
/// before apply and the typed create hard-links that exact object, so a crash
/// can always classify a staged/published file without reopening an unbound
/// name.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConflictCreatedCanonicalPlan {
    pub target_path: PathBuf,
    pub source_path: PathBuf,
    pub expected_sha256: String,
    pub(crate) parent_identity: RegularFileIdentity,
    pub(crate) source_identity: RegularFileIdentity,
    pub staging_path: PathBuf,
    pub rollback_tombstone_path: PathBuf,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum ConflictReplacementPhase {
    Planned,
    WitnessCreating,
    WitnessReady,
    Preparing,
    Prepared,
    Publishing,
    Published,
    Committing,
    CommittedWithRecovery,
    RollbackPreparing,
    RollbackPrepared,
    RolledBack,
    Cleaned,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum ConflictCreatedCanonicalPhase {
    Absent,
    Staging,
    Staged,
    Publishing,
    Published,
    Committing,
    Committed,
    RollbackPreparing,
    RollbackPrepared,
    RolledBack,
    Cleaned,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConflictReplacementPhaseEntry {
    replacement: ConflictReplacementPlan,
    phase: ConflictReplacementPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    replacement_identity: Option<RegularFileIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent_identity: Option<RegularFileIdentity>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConflictCreatedCanonicalPhaseEntry {
    creation: ConflictCreatedCanonicalPlan,
    phase: ConflictCreatedCanonicalPhase,
    identity_bindings: HandleCreateIdentityBindings,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConflictReplacementPhaseRecord {
    schema_version: u32,
    operation_id: String,
    plan_integrity_sha256: String,
    updated_at_ms: u128,
    terminal_cleanup_completed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_canonical: Option<ConflictCreatedCanonicalPhaseEntry>,
    replacements: Vec<ConflictReplacementPhaseEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConflictReplacementPhaseEnvelope {
    record: ConflictReplacementPhaseRecord,
    integrity_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConflictResolutionPlan {
    pub schema_version: u32,
    pub operation_id: String,
    pub migration_operation_id: String,
    pub conflict_id: String,
    pub created_at_ms: u128,
    pub canonical_root: PathBuf,
    pub data_root: PathBuf,
    pub backup_dir: PathBuf,
    pub recovery_expires_at_ms: u128,
    pub staging_root: PathBuf,
    pub chosen_version: ConflictVersion,
    pub chosen_sha256: String,
    pub rejected_sha256: String,
    pub canonical_before_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) canonical_before_identity: Option<RegularFileIdentity>,
    pub session: MigrationSessionApplyEntry,
    pub databases: Vec<ConflictDatabaseEntry>,
    pub cleanup: Vec<ConflictCleanupEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_canonical: Option<ConflictCreatedCanonicalPlan>,
    pub replacements: Vec<ConflictReplacementPlan>,
}

#[derive(Debug, Clone)]
pub struct PreparedConflictResolution {
    pub plan: ConflictResolutionPlan,
    pub plan_snapshot: LedgerFileSnapshot,
    pub created_files: Vec<LedgerFileSnapshot>,
    pub database_snapshots: Vec<LedgerDatabaseSnapshot>,
    pub rollback_steps: Vec<LedgerRollbackStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConflictResolutionPlanEnvelope {
    plan: ConflictResolutionPlan,
    integrity_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictResolutionFailure {
    LiveWriteGuard(String),
    Operation(String),
}

impl ConflictResolutionFailure {
    pub fn message(&self) -> &str {
        match self {
            Self::LiveWriteGuard(message) | Self::Operation(message) => message,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictResolutionRecoveryStatus {
    RolledBack,
    DeferredByLiveWriter,
    Failed,
}

pub fn deferred_conflict_resolution_receipt(
    migration_operation_id: &str,
    conflict_id: &str,
) -> ConflictResolutionReceipt {
    ConflictResolutionReceipt {
        operation_id: None,
        migration_operation_id: migration_operation_id.to_string(),
        conflict_id: conflict_id.to_string(),
        status: ConflictResolutionStatus::Deferred,
        chosen_version: None,
        canonical_updated: false,
        database_view_count: 0,
        recovery_expires_at_ms: None,
        runtime_verification: None,
        validated: true,
    }
}

pub fn conflict_resolution_operation_id(conflict_id: &str) -> Result<String, String> {
    validate_conflict_id(conflict_id)?;
    let digest = format!("{:x}", Sha256::digest(conflict_id.as_bytes()));
    Ok(format!("conflict-resolution-{}", &digest[..32]))
}

pub fn prepare_conflict_resolution(
    canonical_root: &Path,
    data_root: &Path,
    operation_id: &str,
    report: &MigrationPreflightReport,
    conflict_id: &str,
) -> Result<PreparedConflictResolution, String> {
    validate_operation_id(operation_id)?;
    validate_conflict_id(conflict_id)?;
    validate_safe_directory(canonical_root, "canonical root")?;
    validate_safe_directory(data_root, "managed data root")?;
    if report.operation_id != report.plan.operation_id
        || report.plan.canonical_root != canonical_root
        || report.operation_id.trim().is_empty()
    {
        return Err("conflict resolution migration identity changed".to_string());
    }
    let (conflict, summary) = resolve_conflict_by_id(report, conflict_id)?;
    if !matches!(
        summary.relation,
        SessionRelation::Divergent | SessionRelation::LeftPrefix | SessionRelation::RightPrefix
    ) {
        return Err("only a verified session branch can require conflict overwrite".to_string());
    }
    let chosen_version = summary.newer_version.ok_or_else(|| {
        "conflict timestamps are not reliable enough to recommend overwrite".to_string()
    })?;
    if conflict.current_marker_status == MarkerStatus::Invalid
        || conflict.candidate_marker_status == MarkerStatus::Invalid
    {
        return Err("conflict overwrite is blocked by an invalid provider marker".to_string());
    }
    let current_sha256 = conflict
        .current_sha256
        .as_deref()
        .ok_or_else(|| "conflict current version has no verified checksum".to_string())?;
    let (
        chosen_path,
        chosen_sha256,
        chosen_origin,
        rejected_path,
        rejected_sha256,
        rejected_origin,
    ) = match chosen_version {
        ConflictVersion::Current => (
            &conflict.current_path,
            current_sha256,
            conflict.current_origin,
            &conflict.candidate_path,
            conflict.candidate_sha256.as_str(),
            conflict.candidate_origin,
        ),
        ConflictVersion::Candidate => (
            &conflict.candidate_path,
            conflict.candidate_sha256.as_str(),
            conflict.candidate_origin,
            &conflict.current_path,
            current_sha256,
            conflict.current_origin,
        ),
    };
    if path_key(chosen_path) == path_key(rejected_path) {
        return Err("conflict versions do not have distinct storage paths".to_string());
    }

    let operation_root = operation_root(data_root, operation_id)?;
    let plan_path = operation_root.join("conflict-resolution-plan.json");
    if plan_path.exists() {
        let plan = load_conflict_resolution_plan(data_root, operation_id)?;
        if plan.migration_operation_id != report.operation_id
            || plan.conflict_id != conflict_id
            || plan.canonical_root != canonical_root
        {
            return Err("conflict resolution plan identity changed".to_string());
        }
        ensure_conflict_original_witnesses(&plan)?;
        persist_initial_conflict_replacement_phases(&plan)?;
        return prepared_result(plan, &plan_path);
    }

    let discovery = discover_database_catalog(canonical_root, data_root);
    let descriptors = discovery
        .descriptors
        .into_iter()
        .filter(|descriptor| descriptor.role.is_runtime())
        .collect::<Vec<_>>();
    if discovery.errors != 0 || descriptors.is_empty() {
        return Err("conflict resolution could not discover every runtime database".to_string());
    }

    let recovery_root = data_root.join("session-storage-v1/conflict-recovery");
    create_safe_directory(&recovery_root)?;
    let sources = conflict_backup_sources(conflict, current_sha256, &descriptors)?;
    let backup_dir = recovery_root.join(operation_id);
    let backup = if backup_dir.exists() {
        let backup = verify_migration_backup(&backup_dir)?;
        verify_migration_backup_sources(&backup, &sources)?;
        backup
    } else {
        create_migration_backup(&recovery_root, operation_id, &sources)?
    };
    let isolated_root = operation_root.join("conflict-backup-restore-check");
    let backup = verify_migration_backup_with_isolated_restore(&backup.backup_dir, &isolated_root)?;
    if backup.status != MigrationBackupStatus::IsolatedRestoreVerified {
        return Err("conflict recovery package was not restore verified".to_string());
    }

    let staging_root = operation_root.join("conflict-staging");
    create_safe_directory(&staging_root)?;
    let result = (|| {
        let chosen_entry =
            backup_entry_for_source(&backup, chosen_path, MigrationBackupEntryKind::Session)?;
        if chosen_entry.sha256 != chosen_sha256 {
            return Err("conflict chosen version changed during backup".to_string());
        }
        let staged_canonical = staging_root.join("chosen-session.jsonl.stage");
        atomic_copy(
            &backup_payload_path(&backup, chosen_entry)?,
            &staged_canonical,
        )?;
        validate_session_file(&staged_canonical, &conflict.thread_id, chosen_sha256)?;

        let (canonical_before, canonical_before_identity) = if conflict.canonical_path.exists() {
            let semantic = read_semantic_session(&conflict.canonical_path)
                .map_err(|_| "conflict canonical target is invalid".to_string())?;
            if semantic.thread_id != conflict.thread_id {
                return Err("conflict canonical target belongs to another session".to_string());
            }
            let (_, sha256) = stable_file_digest(&conflict.canonical_path)?;
            if sha256 != current_sha256 && sha256 != conflict.candidate_sha256 {
                return Err("conflict canonical target changed after migration".to_string());
            }
            (
                Some(sha256),
                Some(regular_file_identity_at_path(&conflict.canonical_path)?),
            )
        } else {
            (None, None)
        };
        let action = match canonical_before.as_deref() {
            Some(existing) if existing == chosen_sha256 => MigrationSessionAction::KeepCanonical,
            Some(_) => MigrationSessionAction::ReplaceCanonicalWithExtension,
            None => MigrationSessionAction::CopyToCanonical,
        };
        let canonical_backup_payload = canonical_before
            .as_ref()
            .map(|_| {
                backup_entry_for_source(
                    &backup,
                    &conflict.canonical_path,
                    MigrationBackupEntryKind::Session,
                )
                .and_then(|entry| backup_payload_path(&backup, entry))
            })
            .transpose()?;
        let session = MigrationSessionApplyEntry {
            thread_id: conflict.thread_id.clone(),
            action,
            source_path: chosen_path.clone(),
            target_path: conflict.canonical_path.clone(),
            staged_path: Some(staged_canonical),
            expected_sha256: chosen_sha256.to_string(),
            target_before_sha256: canonical_before.clone(),
            target_backup_payload: canonical_backup_payload,
        };

        let mut database_apply = descriptors
            .iter()
            .enumerate()
            .map(|(index, descriptor)| {
                let entry = backup_entry_for_source(
                    &backup,
                    &descriptor.path,
                    MigrationBackupEntryKind::Database,
                )?;
                let (_, live_before_sha256) = stable_file_digest(&descriptor.path)?;
                let staged_path = staging_root.join(format!("database-{index:06}.sqlite.stage"));
                atomic_copy(&backup_payload_path(&backup, entry)?, &staged_path)?;
                let (staged_bytes, staged_sha256) = stable_file_digest(&staged_path)?;
                let live_before_identity = regular_file_identity_at_path(&descriptor.path)?;
                Ok((
                    MigrationDatabaseApplyEntry {
                        database_id: descriptor.id.clone(),
                        role: descriptor.role,
                        target_path: descriptor.path.clone(),
                        staged_path,
                        original_backup_payload: backup_payload_path(&backup, entry)?,
                        original_sha256: entry.sha256.clone(),
                        staged_sha256,
                        staged_bytes,
                    },
                    live_before_sha256,
                    live_before_identity,
                ))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let mut merge_entries = database_apply
            .iter()
            .map(|(entry, _, _)| entry.clone())
            .collect::<Vec<_>>();
        merge_database_views(std::slice::from_ref(&session), &mut merge_entries)?;
        for ((entry, _, _), merged) in database_apply.iter_mut().zip(merge_entries) {
            *entry = merged;
        }
        let databases = database_apply
            .into_iter()
            .map(
                |(apply, live_before_sha256, live_before_identity)| ConflictDatabaseEntry {
                    apply,
                    live_before_sha256,
                    live_before_identity,
                },
            )
            .collect::<Vec<_>>();
        let cleanup = [
            (chosen_path, chosen_origin),
            (rejected_path, rejected_origin),
        ]
        .into_iter()
        .filter(|(path, origin)| {
            path_key(path) != path_key(&conflict.canonical_path)
                && removable_runtime_origin(*origin)
        })
        .map(|(path, _)| cleanup_entry(&backup, operation_id, path, &conflict.thread_id))
        .collect::<Result<Vec<_>, _>>()?;
        let created_canonical = if canonical_before.is_none() {
            Some(build_conflict_created_canonical(operation_id, &session)?)
        } else {
            None
        };
        let replacements = build_conflict_replacements(
            operation_id,
            &session,
            canonical_before.as_deref(),
            &databases,
            None,
        )?;
        let plan = ConflictResolutionPlan {
            schema_version: CONFLICT_RESOLUTION_SCHEMA_VERSION,
            operation_id: operation_id.to_string(),
            migration_operation_id: report.operation_id.clone(),
            conflict_id: conflict_id.to_string(),
            created_at_ms: timestamp_millis()?,
            canonical_root: canonical_root.to_path_buf(),
            data_root: data_root.to_path_buf(),
            backup_dir: backup.backup_dir.clone(),
            recovery_expires_at_ms: backup.expires_at_ms,
            staging_root: staging_root.clone(),
            chosen_version,
            chosen_sha256: chosen_sha256.to_string(),
            rejected_sha256: rejected_sha256.to_string(),
            canonical_before_sha256: canonical_before,
            canonical_before_identity,
            session,
            databases,
            cleanup,
            created_canonical,
            replacements,
        };
        validate_live_artifact_paths_unoccupied(&plan, true)?;
        write_plan(&plan_path, &plan)?;
        ensure_conflict_original_witnesses(&plan)?;
        persist_initial_conflict_replacement_phases(&plan)?;
        prepared_result(plan, &plan_path)
    })();
    if result.is_err() {
        let _ = remove_owned_tree(&staging_root);
    }
    result
}

pub fn load_conflict_resolution_plan(
    data_root: &Path,
    operation_id: &str,
) -> Result<ConflictResolutionPlan, String> {
    let path = operation_root(data_root, operation_id)?.join("conflict-resolution-plan.json");
    let bytes = read_regular_file_bounded(&path, MAX_CONFLICT_RESOLUTION_PLAN_BYTES)
        .map_err(|_| "conflict resolution plan is unreadable".to_string())?;
    let envelope = serde_json::from_slice::<ConflictResolutionPlanEnvelope>(&bytes)
        .map_err(|_| "conflict resolution plan is invalid".to_string())?;
    validate_plan(&envelope.plan)?;
    if envelope.plan.operation_id != operation_id
        || envelope.integrity_sha256 != plan_digest(&envelope.plan)?
    {
        return Err("conflict resolution plan integrity check failed".to_string());
    }
    Ok(envelope.plan)
}

pub fn conflict_runtime_apply_plan(plan: &ConflictResolutionPlan) -> MigrationApplyPlan {
    MigrationApplyPlan {
        schema_version: 1,
        operation_id: plan.operation_id.clone(),
        generated_at_ms: plan.created_at_ms,
        canonical_root: plan.canonical_root.clone(),
        inventory_fingerprint: format!("{:x}", Sha256::digest(plan.conflict_id.as_bytes())),
        backup_dir: plan.backup_dir.clone(),
        staging_root: plan.staging_root.clone(),
        sessions: vec![plan.session.clone()],
        databases: plan
            .databases
            .iter()
            .map(|database| database.apply.clone())
            .collect(),
        conflict_count: 0,
    }
}

pub fn execute_conflict_resolution<Guard>(
    plan: &ConflictResolutionPlan,
    mut before_live_write: Guard,
) -> Result<ConflictResolutionReceipt, ConflictResolutionFailure>
where
    Guard: FnMut() -> Result<(), String>,
{
    validate_plan(plan).map_err(ConflictResolutionFailure::Operation)?;
    verify_plan_backup(plan).map_err(ConflictResolutionFailure::Operation)?;
    validate_initial_conflict_replacement_phases(plan)
        .map_err(ConflictResolutionFailure::Operation)?;
    validate_runtime_database_inventory(plan).map_err(ConflictResolutionFailure::Operation)?;
    let mut barriers =
        acquire_conflict_write_barriers(plan).map_err(ConflictResolutionFailure::Operation)?;
    validate_conflict_apply_preconditions(plan, &mut barriers)
        .map_err(ConflictResolutionFailure::Operation)?;
    apply_canonical(plan, &mut before_live_write, &mut barriers)?;
    for database in &plan.databases {
        let current_sha256 = barriers
            .get_mut(&path_key(&database.apply.target_path))
            .ok_or_else(|| {
                ConflictResolutionFailure::Operation(
                    "conflict runtime database writer barrier is missing".to_string(),
                )
            })?
            .verify_current_path(None)
            .map_err(ConflictResolutionFailure::Operation)?
            .1;
        if current_sha256 == database.apply.staged_sha256 {
            continue;
        }
        if current_sha256 != database.live_before_sha256 {
            return Err(ConflictResolutionFailure::Operation(
                "conflict runtime database changed after planning".to_string(),
            ));
        }
        before_live_write().map_err(ConflictResolutionFailure::LiveWriteGuard)?;
        if !sqlite_sidecars_absent(&database.apply.target_path)
            .map_err(ConflictResolutionFailure::Operation)?
        {
            return Err(ConflictResolutionFailure::Operation(
                "conflict runtime database has an active sidecar before replacement".to_string(),
            ));
        }
        let replacement = replacement_for_target(plan, &database.apply.target_path)
            .map_err(ConflictResolutionFailure::Operation)?;
        publish_conflict_replacement(plan, replacement, &mut barriers).map_err(|error| {
            ConflictResolutionFailure::Operation(format!(
                "conflict runtime database replacement failed: {error}"
            ))
        })?;
        if !sqlite_sidecars_absent(&database.apply.target_path)
            .map_err(ConflictResolutionFailure::Operation)?
        {
            return Err(ConflictResolutionFailure::Operation(
                "conflict runtime database gained a sidecar during replacement".to_string(),
            ));
        }
    }
    validate_runtime_database_views_with_barriers(plan, &mut barriers).map_err(|error| {
        ConflictResolutionFailure::Operation(format!(
            "conflict post-apply database validation failed: {error}"
        ))
    })?;
    for cleanup in &plan.cleanup {
        remove_cleanup_entry(plan, cleanup, &mut before_live_write, &mut barriers).map_err(
            |failure| match failure {
                ConflictResolutionFailure::Operation(error) => {
                    ConflictResolutionFailure::Operation(format!(
                        "conflict cleanup quarantine failed: {error}"
                    ))
                }
                other => other,
            },
        )?;
    }
    validate_conflict_resolution_with_barriers(plan, &mut barriers).map_err(|error| {
        ConflictResolutionFailure::Operation(format!(
            "conflict final guarded validation failed: {error}"
        ))
    })?;
    Ok(ConflictResolutionReceipt {
        operation_id: Some(plan.operation_id.clone()),
        migration_operation_id: plan.migration_operation_id.clone(),
        conflict_id: plan.conflict_id.clone(),
        status: ConflictResolutionStatus::Resolved,
        chosen_version: Some(plan.chosen_version),
        canonical_updated: plan.session.action != MigrationSessionAction::KeepCanonical,
        database_view_count: plan.databases.len(),
        recovery_expires_at_ms: Some(plan.recovery_expires_at_ms),
        runtime_verification: None,
        validated: true,
    })
}

/// Performs the decisive validation while every canonical/database name is
/// still bound to the exact handle acquired before the first live mutation.
/// The public validation below remains useful for runtime verification, but it
/// cannot substitute for this continuous writer/name barrier.
fn validate_conflict_resolution_with_barriers(
    plan: &ConflictResolutionPlan,
    barriers: &mut BTreeMap<String, WriteExclusionGuard>,
) -> Result<(), String> {
    let terminal_cleanup_completed =
        load_conflict_replacement_phases(plan)?.terminal_cleanup_completed;
    {
        let canonical = barriers
            .get_mut(&path_key(&plan.session.target_path))
            .ok_or_else(|| "conflict canonical validation barrier is missing".to_string())?;
        canonical.verify_current_path(Some(&plan.chosen_sha256))?;
        if canonical.identity()?
            != expected_applied_target_identity(plan, &plan.session.target_path)?
        {
            return Err("conflict canonical applied identity changed".to_string());
        }
        validate_session_file(
            &plan.session.target_path,
            &plan.session.thread_id,
            &plan.chosen_sha256,
        )?;
        canonical.verify_current_path(Some(&plan.chosen_sha256))?;
        if canonical.identity()?
            != expected_applied_target_identity(plan, &plan.session.target_path)?
        {
            return Err("conflict canonical applied identity changed".to_string());
        }
    }

    validate_runtime_database_views_with_barriers(plan, barriers)?;
    for database in &plan.databases {
        barriers
            .get_mut(&path_key(&database.apply.target_path))
            .ok_or_else(|| "conflict database validation barrier is missing".to_string())?
            .verify_current_path(Some(&database.apply.staged_sha256))?;
    }
    barriers
        .get_mut(&path_key(&plan.session.target_path))
        .ok_or_else(|| "conflict canonical validation barrier is missing".to_string())?
        .verify_current_path(Some(&plan.chosen_sha256))?;

    for replacement in &plan.replacements {
        let phase = conflict_replacement_phase(plan, replacement)?;
        let target_barrier = barriers
            .get_mut(&path_key(&replacement.target_path))
            .ok_or_else(|| "conflict replacement validation barrier is missing".to_string())?;
        target_barrier.verify_current_path(Some(&replacement.replacement_sha256))?;
        match phase {
            ConflictReplacementPhase::CommittedWithRecovery => {
                let replacement_identity = conflict_replacement_identity(plan, replacement)?;
                if stable_file_digest(&replacement.replacement_witness_path)?.1
                    != replacement.replacement_sha256
                    || target_barrier.verify_same_identity_path(
                        &replacement.replacement_witness_path,
                        Some(&replacement.replacement_sha256),
                    )? != replacement_identity
                    || stable_file_digest(&replacement.recovery_path)?.1
                        != replacement.original_sha256
                    || stable_file_digest(&replacement.original_witness_path)?.1
                        != replacement.original_sha256
                    || !same_persisted_regular_file_identity(
                        &replacement.recovery_path,
                        replacement.original_identity,
                    )
                    .unwrap_or(false)
                    || !same_persisted_regular_file_identity(
                        &replacement.original_witness_path,
                        replacement.original_identity,
                    )
                    .unwrap_or(false)
                    || replacement.staging_path.exists()
                    || replacement.tombstone_path.exists()
                {
                    return Err("conflict replacement ownership or recovery changed".to_string());
                }
            }
            ConflictReplacementPhase::Cleaned => {
                if [
                    &replacement.original_witness_path,
                    &replacement.replacement_witness_path,
                    &replacement.recovery_path,
                    &replacement.staging_path,
                    &replacement.tombstone_path,
                ]
                .iter()
                .any(|path| path.exists())
                {
                    return Err("conflict cleaned replacement left an artifact".to_string());
                }
            }
            _ => return Err("conflict replacement is not durably applied".to_string()),
        }
        if phase == ConflictReplacementPhase::CommittedWithRecovery
            && target_barrier.identity()? != conflict_replacement_identity(plan, replacement)?
        {
            return Err("conflict replacement target identity changed".to_string());
        }
    }
    if let Some(creation) = &plan.created_canonical {
        let phase = created_canonical_phase(plan)?;
        if !matches!(
            phase,
            ConflictCreatedCanonicalPhase::Committed | ConflictCreatedCanonicalPhase::Cleaned
        ) || terminal_cleanup_completed != (phase == ConflictCreatedCanonicalPhase::Cleaned)
            || classify_handle_create_crash_state(
                &typed_created_canonical_paths(creation)?,
                created_canonical_identity_bindings(creation),
                &creation.expected_sha256,
            )? != HandleCreateCrashState::Published
        {
            return Err("conflict-created canonical logical or physical state changed".to_string());
        }
        let barrier = barriers
            .get_mut(&path_key(&plan.session.target_path))
            .ok_or_else(|| "conflict canonical validation barrier is missing".to_string())?;
        barrier.verify_current_path(Some(&creation.expected_sha256))?;
        if barrier.identity()? != creation.source_identity {
            return Err("conflict-created canonical ownership changed".to_string());
        }
    }

    for cleanup in &plan.cleanup {
        ensure_absent(&cleanup.path, "conflict source version")?;
        if terminal_cleanup_completed {
            ensure_absent(&cleanup.tombstone_path, "conflict cleanup tombstone")?;
            ensure_absent(
                &cleanup.ownership_witness_path,
                "conflict cleanup ownership witness",
            )?;
        } else {
            verify_quarantined_cleanup_artifact(
                &cleanup.tombstone_path,
                &cleanup.ownership_witness_path,
                &cleanup.sha256,
                cleanup.original_identity,
            )?;
        }
        if let (Some(marker), Some(tombstone), Some(witness), Some(expected)) = (
            &cleanup.marker_path,
            &cleanup.marker_tombstone_path,
            &cleanup.marker_ownership_witness_path,
            &cleanup.marker_sha256,
        ) {
            ensure_absent(marker, "conflict provider marker")?;
            if terminal_cleanup_completed {
                ensure_absent(tombstone, "conflict provider marker tombstone")?;
                ensure_absent(witness, "conflict provider marker ownership witness")?;
            } else {
                verify_quarantined_cleanup_artifact(
                    tombstone,
                    witness,
                    expected,
                    cleanup.marker_original_identity.ok_or_else(|| {
                        "conflict provider marker identity is missing".to_string()
                    })?,
                )?;
            }
        }
    }
    Ok(())
}

fn verify_quarantined_cleanup_artifact(
    tombstone: &Path,
    witness: &Path,
    expected_sha256: &str,
    expected_identity: RegularFileIdentity,
) -> Result<(), String> {
    if stable_file_digest(tombstone)?.1 != expected_sha256
        || stable_file_digest(witness)?.1 != expected_sha256
        || !same_persisted_regular_file_identity(tombstone, expected_identity).unwrap_or(false)
        || !same_persisted_regular_file_identity(witness, expected_identity).unwrap_or(false)
    {
        return Err("conflict cleanup quarantine ownership changed".to_string());
    }
    Ok(())
}

pub fn validate_conflict_resolution(
    plan: &ConflictResolutionPlan,
    mut receipt: ConflictResolutionReceipt,
) -> Result<ConflictResolutionReceipt, String> {
    validate_plan(plan)?;
    verify_plan_backup(plan)?;
    validate_runtime_database_inventory(plan)?;
    let mut barriers = acquire_conflict_write_barriers(plan)?;
    validate_conflict_resolution_with_barriers(plan, &mut barriers)?;
    if receipt.operation_id.as_deref() != Some(&plan.operation_id)
        || receipt.migration_operation_id != plan.migration_operation_id
        || receipt.conflict_id != plan.conflict_id
        || receipt.status != ConflictResolutionStatus::Resolved
        || receipt.chosen_version != Some(plan.chosen_version)
        || receipt.database_view_count != plan.databases.len()
        || receipt.recovery_expires_at_ms != Some(plan.recovery_expires_at_ms)
    {
        return Err("conflict resolution receipt does not match its plan".to_string());
    }
    receipt.validated = true;
    Ok(receipt)
}

pub fn cleanup_conflict_resolution_staging(plan: &ConflictResolutionPlan) -> Result<(), String> {
    cleanup_migration_staging(&conflict_runtime_apply_plan(plan))
}

/// Removes only operation-owned target-local artifacts after the caller has
/// durably committed the conflict resolution. Every deletion is handle-bound
/// and re-proves the ownership witness; the live target is never deleted.
pub fn cleanup_committed_conflict_resolution_artifacts(
    plan: &ConflictResolutionPlan,
) -> Result<(), String> {
    cleanup_committed_conflict_artifacts(plan)
}

pub fn rollback_conflict_resolution<Guard>(
    store: &OperationLedgerStore,
    plan: &ConflictResolutionPlan,
    mut before_live_write: Guard,
) -> Result<(), ConflictResolutionFailure>
where
    Guard: FnMut() -> Result<(), String>,
{
    validate_plan(plan).map_err(ConflictResolutionFailure::Operation)?;
    verify_plan_backup(plan).map_err(ConflictResolutionFailure::Operation)?;
    let ledger = store
        .load(&plan.operation_id)
        .map_err(ConflictResolutionFailure::Operation)?;
    let expected_steps = rollback_steps(plan);
    if ledger.phase != SessionStorageOperationPhase::RollingBack
        || !rollback_plan_matches(&ledger.rollback_steps, &expected_steps)
    {
        return Err(ConflictResolutionFailure::Operation(
            "conflict resolution rollback plan changed".to_string(),
        ));
    }
    for (index, step) in ledger.rollback_steps.iter().cloned().enumerate() {
        if step.completed {
            continue;
        }
        before_live_write().map_err(ConflictResolutionFailure::LiveWriteGuard)?;
        restore_step(plan, &step).map_err(|error| {
            ConflictResolutionFailure::Operation(format!(
                "conflict rollback step {index} ({:?}) failed: {error}",
                step.action
            ))
        })?;
        store
            .update(&plan.operation_id, |ledger| {
                let current = ledger
                    .rollback_steps
                    .get_mut(index)
                    .ok_or_else(|| "conflict rollback ledger changed".to_string())?;
                if current != &step {
                    return Err("conflict rollback ledger changed".to_string());
                }
                current.completed = true;
                Ok(())
            })
            .map_err(ConflictResolutionFailure::Operation)?;
    }
    validate_rolled_back(plan).map_err(ConflictResolutionFailure::Operation)?;
    cleanup_conflict_resolution_staging(plan).map_err(ConflictResolutionFailure::Operation)
}

pub fn recover_interrupted_conflict_resolution<Guard>(
    store: &OperationLedgerStore,
    data_root: &Path,
    operation_id: &str,
    before_live_write: Guard,
) -> Result<ConflictResolutionRecoveryStatus, String>
where
    Guard: FnMut() -> Result<(), String>,
{
    let mut ledger = store.load(operation_id)?;
    if ledger.kind != SessionStorageOperationKind::ConflictResolution
        || !matches!(
            ledger.phase,
            SessionStorageOperationPhase::Preflight
                | SessionStorageOperationPhase::Backup
                | SessionStorageOperationPhase::BackupVerified
                | SessionStorageOperationPhase::PlanReady
                | SessionStorageOperationPhase::Applying
                | SessionStorageOperationPhase::Validating
                | SessionStorageOperationPhase::RollingBack
        )
    {
        return Err(
            "session storage operation is not an interrupted conflict resolution".to_string(),
        );
    }
    let precommit = matches!(
        ledger.phase,
        SessionStorageOperationPhase::Preflight
            | SessionStorageOperationPhase::Backup
            | SessionStorageOperationPhase::BackupVerified
            | SessionStorageOperationPhase::PlanReady
    );
    let loaded_plan = load_conflict_resolution_plan(data_root, operation_id);
    if matches!(
        ledger.phase,
        SessionStorageOperationPhase::Preflight
            | SessionStorageOperationPhase::Backup
            | SessionStorageOperationPhase::BackupVerified
            | SessionStorageOperationPhase::PlanReady
            | SessionStorageOperationPhase::Applying
            | SessionStorageOperationPhase::Validating
    ) {
        ledger = store.transition(operation_id, SessionStorageOperationPhase::RollingBack)?;
    }
    if precommit && ledger.rollback_steps.is_empty() {
        let cleanup = (|| {
            if let Ok(plan) = &loaded_plan {
                if plan.canonical_root != ledger.canonical_root {
                    return Err("conflict precommit plan identity changed".to_string());
                }
                cleanup_precommit_conflict_artifacts(plan)?;
            }
            cleanup_precommit_conflict_staging(data_root, operation_id)
        })();
        return match cleanup {
            Ok(()) => {
                store.transition(operation_id, SessionStorageOperationPhase::RolledBack)?;
                Ok(ConflictResolutionRecoveryStatus::RolledBack)
            }
            Err(_) => {
                mark_recovery_retry(store, operation_id, "conflictRollbackRetryRequired")?;
                Ok(ConflictResolutionRecoveryStatus::Failed)
            }
        };
    }
    let plan = match loaded_plan {
        Ok(plan)
            if plan.canonical_root == ledger.canonical_root
                && ledger.backup_root.as_ref() == Some(&plan.backup_dir)
                && rollback_plan_matches(&ledger.rollback_steps, &rollback_steps(&plan)) =>
        {
            plan
        }
        Ok(_) | Err(_) => {
            mark_recovery_retry(store, operation_id, "conflictRollbackPlanInvalid")?;
            return Ok(ConflictResolutionRecoveryStatus::Failed);
        }
    };
    match rollback_conflict_resolution(store, &plan, before_live_write) {
        Ok(()) => {
            store.transition(operation_id, SessionStorageOperationPhase::RolledBack)?;
            if cleanup_rolled_back_conflict_artifacts(&plan).is_err() {
                mark_recovery_retry(store, operation_id, "conflictRollbackCleanupPending")?;
            }
            Ok(ConflictResolutionRecoveryStatus::RolledBack)
        }
        Err(ConflictResolutionFailure::LiveWriteGuard(_)) => {
            store.update(operation_id, |ledger| {
                ledger.last_error_code = Some("conflictRollbackWriterActive".to_string());
                Ok(())
            })?;
            Ok(ConflictResolutionRecoveryStatus::DeferredByLiveWriter)
        }
        Err(ConflictResolutionFailure::Operation(_)) => {
            mark_recovery_retry(store, operation_id, "conflictRollbackRetryRequired")?;
            Ok(ConflictResolutionRecoveryStatus::Failed)
        }
    }
}

fn cleanup_precommit_conflict_artifacts(plan: &ConflictResolutionPlan) -> Result<(), String> {
    let phase_path = conflict_replacement_phase_path(plan)?;
    let has_phase_record = phase_path.exists();
    if has_phase_record {
        let record = load_conflict_replacement_phases(plan)?;
        if record.terminal_cleanup_completed
            || record
                .replacements
                .iter()
                .any(|entry| entry.phase != ConflictReplacementPhase::Planned)
        {
            return Err("conflict precommit cleanup phase already advanced".to_string());
        }
    }
    for replacement in &plan.replacements {
        for path in [
            &replacement.replacement_witness_path,
            &replacement.staging_path,
            &replacement.recovery_path,
            &replacement.tombstone_path,
        ] {
            ensure_absent(path, "conflict precommit replacement artifact")?;
        }
        if replacement.original_witness_path.exists() {
            remove_rolled_back_original_witness(replacement)?;
        }
        if has_phase_record {
            transition_conflict_replacement_phase(
                plan,
                replacement,
                &[ConflictReplacementPhase::Planned],
                ConflictReplacementPhase::Cleaned,
            )?;
        }
    }
    if has_phase_record {
        complete_conflict_terminal_cleanup(plan)?;
    }
    Ok(())
}

fn cleanup_precommit_conflict_staging(data_root: &Path, operation_id: &str) -> Result<(), String> {
    let root = operation_root(data_root, operation_id)?;
    for path in [
        root.join("conflict-staging"),
        root.join("conflict-backup-restore-check"),
    ] {
        remove_owned_tree(&path)?;
    }
    Ok(())
}

fn conflict_backup_sources(
    conflict: &super::migration::MigrationConflictPlan,
    current_sha256: &str,
    descriptors: &[super::catalog::DatabaseDescriptor],
) -> Result<Vec<MigrationBackupSource>, String> {
    let mut sources = vec![
        MigrationBackupSource {
            source_path: conflict.current_path.clone(),
            payload_relative_path: PathBuf::from("sessions/current.jsonl"),
            kind: MigrationBackupEntryKind::Session,
            expected_sha256: Some(current_sha256.to_string()),
            logical_thread_id: Some(conflict.thread_id.clone()),
        },
        MigrationBackupSource {
            source_path: conflict.candidate_path.clone(),
            payload_relative_path: PathBuf::from("sessions/candidate.jsonl"),
            kind: MigrationBackupEntryKind::Session,
            expected_sha256: Some(conflict.candidate_sha256.clone()),
            logical_thread_id: Some(conflict.thread_id.clone()),
        },
    ];
    for (label, path, status) in [
        (
            "current",
            &conflict.current_path,
            conflict.current_marker_status,
        ),
        (
            "candidate",
            &conflict.candidate_path,
            conflict.candidate_marker_status,
        ),
    ] {
        if status == MarkerStatus::Invalid {
            return Err("conflict provider marker is invalid".to_string());
        }
        let marker_path = provider_marker_path(path)
            .map_err(|_| "conflict provider marker path is invalid".to_string())?;
        if marker_path.exists() {
            if status != MarkerStatus::Valid {
                return Err("conflict provider marker identity changed".to_string());
            }
            let (_, sha256) = stable_file_digest(&marker_path)?;
            sources.push(MigrationBackupSource {
                source_path: marker_path,
                payload_relative_path: PathBuf::from(format!("markers/{label}.json")),
                kind: MigrationBackupEntryKind::StorageMetadata,
                expected_sha256: Some(sha256),
                logical_thread_id: None,
            });
        }
    }
    for (index, descriptor) in descriptors.iter().enumerate() {
        sources.push(MigrationBackupSource {
            source_path: descriptor.path.clone(),
            payload_relative_path: PathBuf::from(format!("databases/{index:06}.sqlite")),
            kind: MigrationBackupEntryKind::Database,
            expected_sha256: None,
            logical_thread_id: None,
        });
    }
    Ok(sources)
}

fn removable_runtime_origin(origin: FileOrigin) -> bool {
    matches!(
        origin,
        FileOrigin::CanonicalHome | FileOrigin::Shared | FileOrigin::ReferencedExternal
    )
}

fn cleanup_entry(
    backup: &MigrationBackupManifest,
    operation_id: &str,
    path: &Path,
    thread_id: &str,
) -> Result<ConflictCleanupEntry, String> {
    let entry = backup_entry_for_source(backup, path, MigrationBackupEntryKind::Session)?;
    let semantic = read_semantic_session(path)
        .map_err(|_| "conflict cleanup source is invalid".to_string())?;
    if semantic.thread_id != thread_id || hex_digest(semantic.raw_sha256) != entry.sha256 {
        return Err("conflict cleanup source identity changed".to_string());
    }
    let original_identity = regular_file_identity_at_path(path)?;
    let parent_identity = parent_directory_identity_at_path(path)?;
    let marker_path = provider_marker_path(path)
        .map_err(|_| "conflict cleanup marker path is invalid".to_string())?;
    let marker = if marker_path.exists() {
        if inspect_provider_marker(path, Some(&semantic)) != MarkerStatus::Valid {
            return Err("conflict cleanup marker is invalid".to_string());
        }
        let marker_entry = backup_entry_for_source(
            backup,
            &marker_path,
            MigrationBackupEntryKind::StorageMetadata,
        )?;
        let (marker_ownership_witness_path, marker_tombstone_path) =
            cleanup_artifact_paths(operation_id, &marker_path, "provider-marker")?;
        Some((
            marker_path,
            backup_payload_path(backup, marker_entry)?,
            marker_entry.bytes,
            marker_entry.sha256.clone(),
            parent_directory_identity_at_path(&marker_entry.source_path)?,
            regular_file_identity_at_path(&marker_entry.source_path)?,
            marker_ownership_witness_path,
            marker_tombstone_path,
        ))
    } else {
        None
    };
    let (ownership_witness_path, tombstone_path) =
        cleanup_artifact_paths(operation_id, path, "session")?;
    Ok(ConflictCleanupEntry {
        path: path.to_path_buf(),
        backup_payload: backup_payload_path(backup, entry)?,
        bytes: entry.bytes,
        sha256: entry.sha256.clone(),
        parent_identity,
        original_identity,
        ownership_witness_path,
        tombstone_path,
        marker_path: marker.as_ref().map(|value| value.0.clone()),
        marker_backup_payload: marker.as_ref().map(|value| value.1.clone()),
        marker_bytes: marker.as_ref().map(|value| value.2),
        marker_sha256: marker.as_ref().map(|value| value.3.clone()),
        marker_parent_identity: marker.as_ref().map(|value| value.4),
        marker_original_identity: marker.as_ref().map(|value| value.5),
        marker_ownership_witness_path: marker.as_ref().map(|value| value.6.clone()),
        marker_tombstone_path: marker.map(|value| value.7),
    })
}

fn cleanup_artifact_paths(
    operation_id: &str,
    target: &Path,
    label: &str,
) -> Result<(PathBuf, PathBuf), String> {
    validate_operation_id(operation_id)?;
    let parent = target
        .parent()
        .ok_or_else(|| "conflict cleanup target has no parent".to_string())?;
    let file_name = target
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| "conflict cleanup target name is invalid".to_string())?;
    let digest = hex_digest(Sha256::digest(
        format!(
            "conflict-cleanup-v1\0{operation_id}\0{}\0{label}",
            durable_conflict_path_key(target)
        )
        .as_bytes(),
    ));
    let prefix = format!(
        ".{file_name}.codex-switch-conflict-cleanup-{}",
        &digest[..32]
    );
    Ok((
        parent.join(format!("{prefix}.witness")),
        parent.join(format!("{prefix}.tombstone")),
    ))
}

fn build_conflict_created_canonical(
    operation_id: &str,
    session: &MigrationSessionApplyEntry,
) -> Result<ConflictCreatedCanonicalPlan, String> {
    validate_operation_id(operation_id)?;
    let source_path = session
        .staged_path
        .as_ref()
        .ok_or_else(|| "conflict canonical creation source is missing".to_string())?;
    let parent_identity = parent_directory_identity_at_path(&session.target_path)?;
    let source_identity = regular_file_identity_at_path(source_path)?;
    if source_identity.volume_serial_number != parent_identity.volume_serial_number {
        return Err("conflict canonical creation source is not on the target volume".to_string());
    }
    let (staging_path, rollback_tombstone_path) =
        created_canonical_artifact_paths(operation_id, &session.target_path)?;
    Ok(ConflictCreatedCanonicalPlan {
        target_path: session.target_path.clone(),
        source_path: source_path.clone(),
        expected_sha256: session.expected_sha256.clone(),
        parent_identity,
        source_identity,
        staging_path,
        rollback_tombstone_path,
    })
}

fn created_canonical_artifact_paths(
    operation_id: &str,
    target_path: &Path,
) -> Result<(PathBuf, PathBuf), String> {
    let parent = target_path
        .parent()
        .ok_or_else(|| "conflict canonical creation target has no parent".to_string())?;
    let file_name = target_path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| "conflict canonical creation target name is invalid".to_string())?;
    let digest = hex_digest(Sha256::digest(
        format!(
            "conflict-create-v1\0{operation_id}\0{}",
            durable_conflict_path_key(target_path)
        )
        .as_bytes(),
    ));
    let prefix = format!(
        ".{file_name}.codex-switch-conflict-create-{}",
        &digest[..32]
    );
    Ok((
        parent.join(format!("{prefix}.staging")),
        parent.join(format!("{prefix}.tombstone")),
    ))
}

fn typed_created_canonical_paths(
    creation: &ConflictCreatedCanonicalPlan,
) -> Result<HandleCreatePaths, String> {
    HandleCreatePaths::from_persisted_plan(
        creation.target_path.clone(),
        creation.staging_path.clone(),
        creation.rollback_tombstone_path.clone(),
    )
}

fn created_canonical_identity_bindings(
    creation: &ConflictCreatedCanonicalPlan,
) -> HandleCreateIdentityBindings {
    HandleCreateIdentityBindings {
        parent_identity: creation.parent_identity,
        created_identity: creation.source_identity,
    }
}

fn build_conflict_replacements(
    operation_id: &str,
    session: &MigrationSessionApplyEntry,
    canonical_before_sha256: Option<&str>,
    databases: &[ConflictDatabaseEntry],
    persisted_identities: Option<&BTreeMap<String, (RegularFileIdentity, RegularFileIdentity)>>,
) -> Result<Vec<ConflictReplacementPlan>, String> {
    validate_operation_id(operation_id)?;
    let mut replacements = Vec::new();
    if let Some(original_sha256) = canonical_before_sha256 {
        if original_sha256 != session.expected_sha256 {
            let source = session
                .staged_path
                .as_ref()
                .ok_or_else(|| "conflict canonical replacement staging is missing".to_string())?;
            let (parent_identity, original_identity) =
                replacement_persisted_identities(&session.target_path, persisted_identities)?;
            replacements.push(build_conflict_replacement(
                operation_id,
                ConflictReplacementKind::CanonicalSession,
                &session.target_path,
                source,
                original_sha256,
                &session.expected_sha256,
                parent_identity,
                original_identity,
            )?);
        }
    }
    for database in databases
        .iter()
        .filter(|database| database.live_before_sha256 != database.apply.staged_sha256)
    {
        let (parent_identity, original_identity) =
            replacement_persisted_identities(&database.apply.target_path, persisted_identities)?;
        replacements.push(build_conflict_replacement(
            operation_id,
            ConflictReplacementKind::RuntimeDatabase,
            &database.apply.target_path,
            &database.apply.staged_path,
            &database.live_before_sha256,
            &database.apply.staged_sha256,
            parent_identity,
            original_identity,
        )?);
    }
    replacements.sort_by(|left, right| {
        replacement_kind_rank(left.kind)
            .cmp(&replacement_kind_rank(right.kind))
            .then_with(|| {
                durable_conflict_path_key(&left.target_path)
                    .cmp(&durable_conflict_path_key(&right.target_path))
            })
    });
    let mut targets = BTreeSet::new();
    if replacements
        .iter()
        .any(|replacement| !targets.insert(durable_conflict_path_key(&replacement.target_path)))
    {
        return Err("conflict replacement target is duplicated".to_string());
    }
    Ok(replacements)
}

fn replacement_persisted_identities(
    target_path: &Path,
    persisted: Option<&BTreeMap<String, (RegularFileIdentity, RegularFileIdentity)>>,
) -> Result<(RegularFileIdentity, RegularFileIdentity), String> {
    match persisted {
        Some(identities) => identities
            .get(&durable_conflict_path_key(target_path))
            .copied()
            .ok_or_else(|| "conflict replacement persisted identity is missing".to_string()),
        None => Ok((
            parent_directory_identity_at_path(target_path)?,
            regular_file_identity_at_path(target_path)?,
        )),
    }
}

// Replacement identity, content, and path bindings stay explicit so callers
// cannot accidentally reconstruct a weaker crash-recovery plan.
#[allow(clippy::too_many_arguments)]
fn build_conflict_replacement(
    operation_id: &str,
    kind: ConflictReplacementKind,
    target_path: &Path,
    source_path: &Path,
    original_sha256: &str,
    replacement_sha256: &str,
    parent_identity: RegularFileIdentity,
    original_identity: RegularFileIdentity,
) -> Result<ConflictReplacementPlan, String> {
    if !target_path.is_absolute() || !source_path.is_absolute() {
        return Err("conflict replacement path is invalid".to_string());
    }
    let parent = target_path
        .parent()
        .ok_or_else(|| "conflict replacement target has no parent".to_string())?;
    let file_name = target_path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| "conflict replacement target name is invalid".to_string())?;
    let label = match kind {
        ConflictReplacementKind::CanonicalSession => "canonical-session",
        ConflictReplacementKind::RuntimeDatabase => "runtime-database",
    };
    let digest = hex_digest(Sha256::digest(
        format!(
            "conflict-replacement-v1\0{operation_id}\0{}\0{label}",
            durable_conflict_path_key(target_path)
        )
        .as_bytes(),
    ));
    let prefix = format!(
        ".{file_name}.codex-switch-conflict-replace-{}",
        &digest[..32]
    );
    Ok(ConflictReplacementPlan {
        kind,
        target_path: target_path.to_path_buf(),
        source_path: source_path.to_path_buf(),
        original_sha256: original_sha256.to_string(),
        parent_identity,
        original_identity,
        replacement_sha256: replacement_sha256.to_string(),
        original_witness_path: parent.join(format!("{prefix}.original")),
        replacement_witness_path: parent.join(format!("{prefix}.replacement")),
        staging_path: parent.join(format!("{prefix}.staging")),
        recovery_path: parent.join(format!("{prefix}.recovery")),
        tombstone_path: parent.join(format!("{prefix}.tombstone")),
    })
}

fn replacement_kind_rank(kind: ConflictReplacementKind) -> u8 {
    match kind {
        ConflictReplacementKind::CanonicalSession => 0,
        ConflictReplacementKind::RuntimeDatabase => 1,
    }
}

fn replacement_for_target<'a>(
    plan: &'a ConflictResolutionPlan,
    target: &Path,
) -> Result<&'a ConflictReplacementPlan, String> {
    let key = path_key(target);
    let matches = plan
        .replacements
        .iter()
        .filter(|replacement| path_key(&replacement.target_path) == key)
        .collect::<Vec<_>>();
    if matches.len() == 1 {
        Ok(matches[0])
    } else {
        Err("conflict replacement binding is missing or duplicated".to_string())
    }
}

fn expected_applied_target_identity(
    plan: &ConflictResolutionPlan,
    target: &Path,
) -> Result<RegularFileIdentity, String> {
    if let Some(replacement) = plan
        .replacements
        .iter()
        .find(|replacement| path_key(&replacement.target_path) == path_key(target))
    {
        return conflict_replacement_identity(plan, replacement);
    }
    if path_key(target) == path_key(&plan.session.target_path) {
        return match plan.canonical_before_identity {
            Some(identity) => Ok(identity),
            None => created_canonical_identity(plan),
        };
    }
    plan.databases
        .iter()
        .find(|database| path_key(&database.apply.target_path) == path_key(target))
        .map(|database| database.live_before_identity)
        .ok_or_else(|| "conflict applied target identity is not plan-bound".to_string())
}

fn typed_replacement_paths(
    replacement: &ConflictReplacementPlan,
) -> Result<HandleReplacePaths, String> {
    HandleReplacePaths::from_persisted_plan(
        replacement.target_path.clone(),
        replacement.recovery_path.clone(),
        replacement.staging_path.clone(),
        replacement.tombstone_path.clone(),
    )
}

fn acquire_conflict_write_barriers(
    plan: &ConflictResolutionPlan,
) -> Result<BTreeMap<String, WriteExclusionGuard>, String> {
    let mut paths = BTreeMap::<String, PathBuf>::new();
    if plan.canonical_before_sha256.is_some() {
        paths.insert(
            path_key(&plan.session.target_path),
            plan.session.target_path.clone(),
        );
    }
    for database in &plan.databases {
        paths.insert(
            path_key(&database.apply.target_path),
            database.apply.target_path.clone(),
        );
    }
    let mut barriers = BTreeMap::new();
    for (key, path) in paths {
        barriers.insert(key, WriteExclusionGuard::acquire(&path)?);
    }
    Ok(barriers)
}

/// Binds every pre-existing live target to a deterministic hard-link witness
/// after the immutable plan is durable and before apply can begin. Retrying a
/// crash in this preparation window may adopt only the exact target identity;
/// an occupied equal-hash name with a different file ID fails closed.
fn ensure_conflict_original_witnesses(plan: &ConflictResolutionPlan) -> Result<(), String> {
    if plan.replacements.is_empty() {
        return Ok(());
    }
    let mut barriers = acquire_conflict_write_barriers(plan)?;
    for replacement in &plan.replacements {
        if parent_directory_identity_at_path(&replacement.target_path)?
            != replacement.parent_identity
        {
            return Err("conflict replacement parent identity changed".to_string());
        }
        let barrier = barriers
            .get_mut(&path_key(&replacement.target_path))
            .ok_or_else(|| "conflict original witness writer barrier is missing".to_string())?;
        barrier.verify_current_path(Some(&replacement.original_sha256))?;
        if barrier.identity()? != replacement.original_identity {
            return Err("conflict original target identity changed".to_string());
        }
        match fs::symlink_metadata(&replacement.original_witness_path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::hard_link(&replacement.target_path, &replacement.original_witness_path)
                    .map_err(|_| "failed to bind conflict original identity witness".to_string())?;
            }
            Ok(metadata) if metadata.is_file() && !metadata_is_link_or_reparse(&metadata) => {}
            Ok(_) => return Err("conflict original identity witness is unsafe".to_string()),
            Err(_) => return Err("conflict original identity witness is unavailable".to_string()),
        }
        if stable_file_digest(&replacement.original_witness_path)?.1 != replacement.original_sha256
            || !same_persisted_regular_file_identity(
                &replacement.original_witness_path,
                replacement.original_identity,
            )
            .unwrap_or(false)
        {
            return Err("conflict original identity witness changed".to_string());
        }
        barrier.verify_current_path(Some(&replacement.original_sha256))?;
        if barrier.identity()? != replacement.original_identity {
            return Err("conflict original target identity changed".to_string());
        }
    }
    Ok(())
}

fn verify_conflict_original_witness(
    replacement: &ConflictReplacementPlan,
    original_path: &Path,
) -> Result<(), String> {
    if stable_file_digest(original_path)?.1 != replacement.original_sha256
        || stable_file_digest(&replacement.original_witness_path)?.1 != replacement.original_sha256
        || !same_persisted_regular_file_identity(original_path, replacement.original_identity)
            .unwrap_or(false)
        || !same_persisted_regular_file_identity(
            &replacement.original_witness_path,
            replacement.original_identity,
        )
        .unwrap_or(false)
    {
        return Err(conflict_rollback_deferred(
            "original replacement ownership is unproven",
        ));
    }
    Ok(())
}

fn remove_rolled_back_original_witness(
    replacement: &ConflictReplacementPlan,
) -> Result<(), String> {
    match fs::symlink_metadata(&replacement.original_witness_path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Ok(metadata) if metadata.is_file() && !metadata_is_link_or_reparse(&metadata) => {}
        Ok(_) => {
            return Err(conflict_rollback_deferred(
                "original replacement witness is unsafe",
            ))
        }
        Err(_) => {
            return Err(conflict_rollback_deferred(
                "original replacement witness is unavailable",
            ))
        }
    }
    verify_conflict_original_witness(replacement, &replacement.target_path)?;
    let mut witness = DestructiveFileGuard::acquire(&replacement.original_witness_path)
        .map_err(|_| conflict_rollback_deferred("original witness handle is unavailable"))?;
    let identity = witness
        .verify_same_identity_path(&replacement.target_path, Some(&replacement.original_sha256))
        .map_err(|_| conflict_rollback_deferred("original witness identity changed"))?;
    if identity != replacement.original_identity {
        return Err(conflict_rollback_deferred(
            "restored target does not match the original witness",
        ));
    }
    witness
        .delete()
        .map_err(|_| conflict_rollback_deferred("original witness cleanup failed"))
}

fn validate_conflict_apply_preconditions(
    plan: &ConflictResolutionPlan,
    barriers: &mut BTreeMap<String, WriteExclusionGuard>,
) -> Result<(), String> {
    let staged = plan
        .session
        .staged_path
        .as_deref()
        .ok_or_else(|| "conflict staging is missing".to_string())?;
    validate_session_file(staged, &plan.session.thread_id, &plan.chosen_sha256)?;
    match plan.canonical_before_sha256.as_deref() {
        Some(expected) => {
            let barrier = barriers
                .get_mut(&path_key(&plan.session.target_path))
                .ok_or_else(|| "conflict canonical writer barrier is missing".to_string())?;
            barrier.verify_current_path(Some(expected))?;
            if barrier.identity()?
                != plan
                    .canonical_before_identity
                    .ok_or_else(|| "conflict canonical original identity is missing".to_string())?
            {
                return Err("conflict canonical original identity changed".to_string());
            }
        }
        None => {
            ensure_absent(&plan.session.target_path, "conflict canonical target")?;
            let creation = plan
                .created_canonical
                .as_ref()
                .ok_or_else(|| "conflict-created canonical plan is missing".to_string())?;
            if parent_directory_identity_at_path(&creation.target_path)? != creation.parent_identity
                || stable_file_digest(&creation.source_path)?.1 != creation.expected_sha256
                || !same_persisted_regular_file_identity(
                    &creation.source_path,
                    creation.source_identity,
                )?
            {
                return Err("conflict-created canonical ownership changed".to_string());
            }
        }
    }
    for database in &plan.databases {
        let barrier = barriers
            .get_mut(&path_key(&database.apply.target_path))
            .ok_or_else(|| "conflict runtime database writer barrier is missing".to_string())?;
        barrier.verify_current_path(Some(&database.live_before_sha256))?;
        if barrier.identity()? != database.live_before_identity {
            return Err("conflict runtime database identity changed".to_string());
        }
        if !sqlite_sidecars_absent(&database.apply.target_path)? {
            return Err("conflict runtime database has an active sidecar".to_string());
        }
        quick_check_conflict_database(&database.apply.staged_path)?;
        if stable_file_digest(&database.apply.staged_path)?.1 != database.apply.staged_sha256 {
            return Err("conflict staged runtime database changed".to_string());
        }
    }
    for replacement in &plan.replacements {
        if parent_directory_identity_at_path(&replacement.target_path)?
            != replacement.parent_identity
        {
            return Err("conflict replacement parent identity changed".to_string());
        }
        let barrier = barriers
            .get_mut(&path_key(&replacement.target_path))
            .ok_or_else(|| "conflict original witness writer barrier is missing".to_string())?;
        barrier.verify_current_path(Some(&replacement.original_sha256))?;
        if barrier.identity()? != replacement.original_identity {
            return Err("conflict original target identity changed".to_string());
        }
        if stable_file_digest(&replacement.original_witness_path)?.1 != replacement.original_sha256
            || !same_persisted_regular_file_identity(
                &replacement.original_witness_path,
                replacement.original_identity,
            )
            .unwrap_or(false)
        {
            return Err("conflict original identity witness changed".to_string());
        }
        barrier.verify_current_path(Some(&replacement.original_sha256))?;
        if barrier.identity()? != replacement.original_identity {
            return Err("conflict original target identity changed".to_string());
        }
    }
    validate_live_artifact_paths_unoccupied(plan, false)?;
    for cleanup in &plan.cleanup {
        validate_session_file(&cleanup.path, &plan.session.thread_id, &cleanup.sha256)?;
        if !same_persisted_regular_file_identity(&cleanup.path, cleanup.original_identity)? {
            return Err("conflict cleanup source identity changed after planning".to_string());
        }
        if let (Some(marker), Some(expected)) = (&cleanup.marker_path, &cleanup.marker_sha256) {
            let identity = cleanup
                .marker_original_identity
                .ok_or_else(|| "conflict provider marker identity is missing".to_string())?;
            if stable_file_digest(marker)?.1 != *expected
                || !same_persisted_regular_file_identity(marker, identity)?
            {
                return Err("conflict provider marker changed after planning".to_string());
            }
        }
    }
    Ok(())
}

fn validate_live_artifact_paths_unoccupied(
    plan: &ConflictResolutionPlan,
    original_witnesses_must_be_absent: bool,
) -> Result<(), String> {
    let mut paths = Vec::new();
    if let Some(creation) = &plan.created_canonical {
        paths.extend([&creation.staging_path, &creation.rollback_tombstone_path]);
    }
    for replacement in &plan.replacements {
        if original_witnesses_must_be_absent {
            paths.push(&replacement.original_witness_path);
        }
        paths.extend([
            &replacement.replacement_witness_path,
            &replacement.staging_path,
            &replacement.recovery_path,
            &replacement.tombstone_path,
        ]);
    }
    for cleanup in &plan.cleanup {
        paths.extend([&cleanup.ownership_witness_path, &cleanup.tombstone_path]);
        if let Some(path) = &cleanup.marker_ownership_witness_path {
            paths.push(path);
        }
        if let Some(path) = &cleanup.marker_tombstone_path {
            paths.push(path);
        }
    }
    let mut seen = BTreeSet::new();
    for path in paths {
        if !seen.insert(durable_conflict_path_key(path)) {
            return Err("conflict live artifact path is duplicated".to_string());
        }
        match fs::symlink_metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(_) => return Err("conflict live artifact path is already occupied".to_string()),
            Err(_) => return Err("conflict live artifact path is unavailable".to_string()),
        }
    }
    Ok(())
}

fn publish_conflict_replacement(
    plan: &ConflictResolutionPlan,
    replacement: &ConflictReplacementPlan,
    barriers: &mut BTreeMap<String, WriteExclusionGuard>,
) -> Result<(), String> {
    for path in [
        &replacement.replacement_witness_path,
        &replacement.staging_path,
        &replacement.recovery_path,
        &replacement.tombstone_path,
    ] {
        match fs::symlink_metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(_) => return Err("conflict replacement artifact appeared before apply".to_string()),
            Err(_) => {
                return Err("conflict replacement artifact is unavailable before apply".to_string())
            }
        }
    }
    if stable_file_digest(&replacement.source_path)?.1 != replacement.replacement_sha256 {
        return Err("conflict replacement source changed".to_string());
    }
    transition_conflict_replacement_phase(
        plan,
        replacement,
        &[ConflictReplacementPhase::Planned],
        ConflictReplacementPhase::WitnessCreating,
    )?;
    let created = atomic_create(&replacement.replacement_witness_path, |target| {
        let mut source = fs::File::open(&replacement.source_path)
            .map_err(|_| "failed to open conflict replacement source".to_string())?;
        io::copy(&mut source, target)
            .map(|_| ())
            .map_err(|_| "failed to stage conflict replacement identity".to_string())
    })?;
    if !created {
        return Err("conflict replacement witness appeared concurrently".to_string());
    }
    if stable_file_digest(&replacement.replacement_witness_path)?.1
        != replacement.replacement_sha256
    {
        return Err("conflict replacement witness verification failed".to_string());
    }
    let identity_bindings = HandleReplaceIdentityBindings {
        parent_identity: replacement.parent_identity,
        original_identity: replacement.original_identity,
        replacement_identity: regular_file_identity_at_path(&replacement.replacement_witness_path)?,
    };
    record_conflict_replacement_witness_ready(plan, replacement, identity_bindings)?;
    transition_conflict_replacement_phase(
        plan,
        replacement,
        &[ConflictReplacementPhase::WitnessReady],
        ConflictReplacementPhase::Preparing,
    )?;
    let key = path_key(&replacement.target_path);
    let mut barrier = barriers
        .remove(&key)
        .ok_or_else(|| "conflict replacement writer barrier is missing".to_string())?;
    barrier.verify_current_path(Some(&replacement.original_sha256))?;
    if barrier.identity()? != replacement.original_identity {
        return Err("conflict replacement original identity changed".to_string());
    }
    let replace_paths = typed_replacement_paths(replacement)?;
    let staged = barrier.stage_handle_hardlink_replace(
        &replacement.replacement_witness_path,
        &replacement.replacement_sha256,
        &replace_paths,
    )?;
    if staged.identity_bindings()? != identity_bindings {
        return Err("conflict staged replacement identity changed".to_string());
    }
    let prepared = staged.prepare().map_err(|(error, _staged)| error)?;
    if prepared.paths() != &replace_paths {
        return Err("conflict prepared replacement identity changed".to_string());
    }
    transition_conflict_replacement_phase(
        plan,
        replacement,
        &[ConflictReplacementPhase::Preparing],
        ConflictReplacementPhase::Prepared,
    )?;
    transition_conflict_replacement_phase(
        plan,
        replacement,
        &[ConflictReplacementPhase::Prepared],
        ConflictReplacementPhase::Publishing,
    )?;
    let published = prepared.publish().map_err(|(error, _prepared)| error)?;
    if published.paths() != &replace_paths {
        return Err("conflict published replacement identity changed".to_string());
    }
    transition_conflict_replacement_phase(
        plan,
        replacement,
        &[ConflictReplacementPhase::Publishing],
        ConflictReplacementPhase::Published,
    )?;
    transition_conflict_replacement_phase(
        plan,
        replacement,
        &[ConflictReplacementPhase::Published],
        ConflictReplacementPhase::Committing,
    )?;
    let mut resolved = published.commit().map_err(|(error, _published)| error)?;
    if resolved.paths() != &replace_paths {
        return Err("conflict committed replacement identity changed".to_string());
    }
    resolved
        .guard_mut()
        .verify_current_path(Some(&replacement.replacement_sha256))?;
    transition_conflict_replacement_phase(
        plan,
        replacement,
        &[ConflictReplacementPhase::Committing],
        ConflictReplacementPhase::CommittedWithRecovery,
    )?;
    let mut barrier = resolved.retain_for_recovery();
    barrier.verify_current_path(Some(&replacement.replacement_sha256))?;
    if barrier.identity()? != identity_bindings.replacement_identity
        || !same_persisted_regular_file_identity(
            &replacement.replacement_witness_path,
            identity_bindings.replacement_identity,
        )?
    {
        return Err("conflict published replacement ownership changed".to_string());
    }
    barriers.insert(key, barrier);
    Ok(())
}

fn quick_check_conflict_database(path: &Path) -> Result<(), String> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| "failed to open conflict runtime database".to_string())?;
    let quick: String = connection
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(|_| "failed to verify conflict runtime database".to_string())?;
    if quick == "ok" {
        Ok(())
    } else {
        Err("conflict runtime database failed quick_check".to_string())
    }
}

fn apply_canonical<Guard>(
    plan: &ConflictResolutionPlan,
    before_live_write: &mut Guard,
    barriers: &mut BTreeMap<String, WriteExclusionGuard>,
) -> Result<(), ConflictResolutionFailure>
where
    Guard: FnMut() -> Result<(), String>,
{
    let staged = plan.session.staged_path.as_deref().ok_or_else(|| {
        ConflictResolutionFailure::Operation("conflict staging is missing".to_string())
    })?;
    validate_session_file(staged, &plan.session.thread_id, &plan.chosen_sha256)
        .map_err(ConflictResolutionFailure::Operation)?;
    if let Some(original_sha256) = plan.canonical_before_sha256.as_deref() {
        let key = path_key(&plan.session.target_path);
        let current_sha256 = barriers
            .get_mut(&key)
            .ok_or_else(|| {
                ConflictResolutionFailure::Operation(
                    "conflict canonical writer barrier is missing".to_string(),
                )
            })?
            .verify_current_path(Some(original_sha256))
            .map_err(ConflictResolutionFailure::Operation)?
            .1;
        if current_sha256 == plan.chosen_sha256 {
            return Ok(());
        }
        before_live_write().map_err(ConflictResolutionFailure::LiveWriteGuard)?;
        let replacement = replacement_for_target(plan, &plan.session.target_path)
            .map_err(ConflictResolutionFailure::Operation)?;
        publish_conflict_replacement(plan, replacement, barriers).map_err(|error| {
            ConflictResolutionFailure::Operation(format!(
                "conflict canonical replacement failed: {error}"
            ))
        })?;
    } else {
        let creation = plan.created_canonical.as_ref().ok_or_else(|| {
            ConflictResolutionFailure::Operation(
                "conflict-created canonical plan is missing".to_string(),
            )
        })?;
        before_live_write().map_err(ConflictResolutionFailure::LiveWriteGuard)?;
        transition_created_canonical_phase(
            plan,
            &[ConflictCreatedCanonicalPhase::Absent],
            ConflictCreatedCanonicalPhase::Staging,
        )
        .map_err(ConflictResolutionFailure::Operation)?;
        let create_paths = typed_created_canonical_paths(creation)
            .map_err(ConflictResolutionFailure::Operation)?;
        let staged_create =
            stage_handle_hardlink_create(staged, &plan.chosen_sha256, &create_paths)
                .map_err(ConflictResolutionFailure::Operation)?;
        if staged_create
            .identity_bindings()
            .map_err(ConflictResolutionFailure::Operation)?
            != created_canonical_identity_bindings(creation)
        {
            return Err(ConflictResolutionFailure::Operation(
                "conflict-created canonical identity changed during staging".to_string(),
            ));
        }
        transition_created_canonical_phase(
            plan,
            &[ConflictCreatedCanonicalPhase::Staging],
            ConflictCreatedCanonicalPhase::Staged,
        )
        .map_err(ConflictResolutionFailure::Operation)?;
        transition_created_canonical_phase(
            plan,
            &[ConflictCreatedCanonicalPhase::Staged],
            ConflictCreatedCanonicalPhase::Publishing,
        )
        .map_err(ConflictResolutionFailure::Operation)?;
        let published = staged_create
            .publish()
            .map_err(|(error, _staged)| ConflictResolutionFailure::Operation(error))?;
        transition_created_canonical_phase(
            plan,
            &[ConflictCreatedCanonicalPhase::Publishing],
            ConflictCreatedCanonicalPhase::Published,
        )
        .map_err(ConflictResolutionFailure::Operation)?;
        transition_created_canonical_phase(
            plan,
            &[ConflictCreatedCanonicalPhase::Published],
            ConflictCreatedCanonicalPhase::Committing,
        )
        .map_err(ConflictResolutionFailure::Operation)?;
        let resolved = published
            .commit()
            .map_err(|(error, _published)| ConflictResolutionFailure::Operation(error))?;
        transition_created_canonical_phase(
            plan,
            &[ConflictCreatedCanonicalPhase::Committing],
            ConflictCreatedCanonicalPhase::Committed,
        )
        .map_err(ConflictResolutionFailure::Operation)?;
        let mut barrier = resolved.retain_for_recovery().ok_or_else(|| {
            ConflictResolutionFailure::Operation(
                "conflict-created canonical writer barrier is missing".to_string(),
            )
        })?;
        barrier
            .verify_current_path(Some(&plan.chosen_sha256))
            .map_err(ConflictResolutionFailure::Operation)?;
        if barrier
            .identity()
            .map_err(ConflictResolutionFailure::Operation)?
            != creation.source_identity
        {
            return Err(ConflictResolutionFailure::Operation(
                "conflict-created canonical ownership changed".to_string(),
            ));
        }
        barriers.insert(path_key(&plan.session.target_path), barrier);
    }
    validate_session_file(
        &plan.session.target_path,
        &plan.session.thread_id,
        &plan.chosen_sha256,
    )
    .map_err(ConflictResolutionFailure::Operation)
}

fn remove_cleanup_entry<Guard>(
    plan: &ConflictResolutionPlan,
    cleanup: &ConflictCleanupEntry,
    before_live_write: &mut Guard,
    barriers: &mut BTreeMap<String, WriteExclusionGuard>,
) -> Result<(), ConflictResolutionFailure>
where
    Guard: FnMut() -> Result<(), String>,
{
    validate_runtime_database_views_with_barriers(plan, barriers).map_err(|error| {
        ConflictResolutionFailure::Operation(format!(
            "pre-marker database validation failed: {error}"
        ))
    })?;
    if let (Some(marker_path), Some(expected), Some(witness), Some(tombstone)) = (
        &cleanup.marker_path,
        &cleanup.marker_sha256,
        &cleanup.marker_ownership_witness_path,
        &cleanup.marker_tombstone_path,
    ) {
        let identity = cleanup.marker_original_identity.ok_or_else(|| {
            ConflictResolutionFailure::Operation(
                "conflict provider marker identity is missing".to_string(),
            )
        })?;
        before_live_write().map_err(ConflictResolutionFailure::LiveWriteGuard)?;
        quarantine_conflict_cleanup_file(marker_path, expected, identity, witness, tombstone)
            .map_err(|error| {
                ConflictResolutionFailure::Operation(format!(
                    "provider marker quarantine failed: {error}"
                ))
            })?;
    }
    validate_runtime_database_views_with_barriers(plan, barriers).map_err(|error| {
        ConflictResolutionFailure::Operation(format!(
            "post-marker database validation failed: {error}"
        ))
    })?;
    before_live_write().map_err(ConflictResolutionFailure::LiveWriteGuard)?;
    quarantine_conflict_cleanup_file(
        &cleanup.path,
        &cleanup.sha256,
        cleanup.original_identity,
        &cleanup.ownership_witness_path,
        &cleanup.tombstone_path,
    )
    .map_err(|error| {
        ConflictResolutionFailure::Operation(format!("session quarantine failed: {error}"))
    })?;
    ensure_absent(&cleanup.path, "conflict source version")
        .map_err(ConflictResolutionFailure::Operation)
}

fn quarantine_conflict_cleanup_file(
    source: &Path,
    expected_sha256: &str,
    expected_identity: RegularFileIdentity,
    witness: &Path,
    tombstone: &Path,
) -> Result<(), String> {
    match fs::symlink_metadata(source) {
        Ok(metadata) if metadata.is_file() && !metadata_is_link_or_reparse(&metadata) => {}
        Ok(_) => return Err("conflict cleanup source is unsafe".to_string()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err("conflict cleanup source disappeared before quarantine".to_string())
        }
        Err(_) => return Err("conflict cleanup source is unavailable".to_string()),
    }
    for path in [witness, tombstone] {
        match fs::symlink_metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(_) => return Err("conflict cleanup ownership path is occupied".to_string()),
            Err(_) => return Err("conflict cleanup ownership path is unavailable".to_string()),
        }
    }
    if stable_file_digest(source)?.1 != expected_sha256
        || !same_persisted_regular_file_identity(source, expected_identity)?
    {
        return Err("conflict cleanup source changed after planning".to_string());
    }
    fs::hard_link(source, witness)
        .map_err(|_| "failed to create conflict cleanup ownership witness".to_string())?;
    if stable_file_digest(witness)?.1 != expected_sha256
        || !same_persisted_regular_file_identity(witness, expected_identity).unwrap_or(false)
    {
        return Err("conflict cleanup ownership witness changed".to_string());
    }
    let mut destructive = DestructiveFileGuard::acquire(source)?;
    destructive.verify_current_path(Some(expected_sha256))?;
    if destructive.verify_same_identity_path(witness, Some(expected_sha256))? != expected_identity {
        return Err("conflict cleanup source identity changed".to_string());
    }
    destructive.rename_no_replace(tombstone)?;
    drop(destructive);
    if stable_file_digest(tombstone)?.1 != expected_sha256
        || !same_persisted_regular_file_identity(tombstone, expected_identity).unwrap_or(false)
        || !same_persisted_regular_file_identity(witness, expected_identity).unwrap_or(false)
    {
        return Err("conflict cleanup tombstone ownership changed".to_string());
    }
    Ok(())
}

fn validate_runtime_database_inventory(plan: &ConflictResolutionPlan) -> Result<(), String> {
    let discovery = discover_database_catalog(&plan.canonical_root, &plan.data_root);
    if discovery.errors != 0 {
        return Err("conflict resolution runtime database discovery failed".to_string());
    }
    let actual = discovery
        .descriptors
        .iter()
        .filter(|descriptor| descriptor.role.is_runtime())
        .map(|descriptor| path_key(&descriptor.path))
        .collect::<BTreeSet<_>>();
    let expected = plan
        .databases
        .iter()
        .map(|database| path_key(&database.apply.target_path))
        .collect::<BTreeSet<_>>();
    if actual != expected {
        return Err("conflict resolution runtime database inventory changed".to_string());
    }
    Ok(())
}

fn validate_runtime_database_views_with_barriers(
    plan: &ConflictResolutionPlan,
    barriers: &mut BTreeMap<String, WriteExclusionGuard>,
) -> Result<(), String> {
    let canonical_key = path_key(&plan.session.target_path);
    let cleanup_keys = plan
        .cleanup
        .iter()
        .map(|entry| path_key(&entry.path))
        .collect::<BTreeSet<_>>();
    for database in &plan.databases {
        with_guarded_conflict_database_copy(
            plan,
            database,
            "runtime-view",
            &database.apply.staged_sha256,
            barriers,
            |copy| {
                quick_check_conflict_database(copy)?;
                let connection = Connection::open_with_flags(
                    copy,
                    OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )
                .map_err(|_| "failed to inspect conflict runtime database".to_string())?;
                let selected = connection
                    .query_row(
                        "SELECT rollout_path FROM threads WHERE id = ?1",
                        [&plan.session.thread_id],
                        |row| row.get::<_, String>(0),
                    )
                    .map_err(|_| "conflict runtime database is missing the session".to_string())?;
                if path_key(Path::new(&selected)) != canonical_key {
                    return Err(
                        "conflict runtime database does not reference canonical storage"
                            .to_string(),
                    );
                }
                let mut statement = connection
                    .prepare("SELECT rollout_path FROM threads WHERE rollout_path IS NOT NULL")
                    .map_err(|_| "failed to inspect conflict runtime references".to_string())?;
                let rows = statement
                    .query_map([], |row| row.get::<_, String>(0))
                    .map_err(|_| "failed to inspect conflict runtime references".to_string())?;
                for row in rows {
                    let path =
                        row.map_err(|_| "failed to read conflict runtime reference".to_string())?;
                    if cleanup_keys.contains(&path_key(Path::new(&path))) {
                        return Err("a conflict source version is still referenced".to_string());
                    }
                }
                Ok(())
            },
        )?;
    }
    Ok(())
}

fn with_guarded_conflict_database_copy<T>(
    plan: &ConflictResolutionPlan,
    database: &ConflictDatabaseEntry,
    label: &str,
    expected_raw_sha256: &str,
    barriers: &mut BTreeMap<String, WriteExclusionGuard>,
    inspect: impl FnOnce(&Path) -> Result<T, String>,
) -> Result<T, String> {
    if !sqlite_sidecars_absent(&database.apply.target_path)? {
        return Err("conflict runtime database has an active sidecar".to_string());
    }
    let staging_root_created = match fs::symlink_metadata(&plan.staging_root) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            create_safe_directory(&plan.staging_root)?;
            true
        }
        Ok(metadata) if metadata.is_dir() && !metadata_is_link_or_reparse(&metadata) => false,
        Ok(_) => return Err("conflict guarded copy staging root is unsafe".to_string()),
        Err(_) => return Err("conflict guarded copy staging root is unavailable".to_string()),
    };
    let result = (|| {
        let copy_key = hex_digest(Sha256::digest(
            format!(
                "conflict-guarded-copy-v1\0{}\0{}\0{label}",
                database.apply.database_id,
                durable_conflict_path_key(&database.apply.target_path)
            )
            .as_bytes(),
        ));
        let copy = plan
            .staging_root
            .join(format!(".guarded-conflict-{copy_key}.sqlite"));
        if copy.exists() || !sqlite_sidecars_absent(&copy)? {
            return Err("conflict guarded database copy already exists".to_string());
        }
        let expected_identity =
            expected_applied_target_identity(plan, &database.apply.target_path)?;
        let barrier = barriers
            .get_mut(&path_key(&database.apply.target_path))
            .ok_or_else(|| "conflict database writer barrier is missing".to_string())?;
        let source_digest = barrier.copy_current_to_new_file(&copy, Some(expected_raw_sha256))?;
        if barrier.identity()? != expected_identity {
            return Err("conflict database applied identity changed".to_string());
        }
        let copy_identity = regular_file_identity_at_path(&copy)?;
        let inspected = inspect(&copy);
        let copy_unchanged = stable_file_digest(&copy).and_then(|digest| {
            if digest == source_digest && sqlite_sidecars_absent(&copy)? {
                Ok(())
            } else {
                Err("conflict guarded database copy changed".to_string())
            }
        });
        let cleanup =
            remove_exact_identity_conflict_artifact(&copy, &source_digest.1, copy_identity);
        let live_unchanged = barrier
            .verify_current_path(Some(expected_raw_sha256))
            .and_then(|digest| {
                if digest == source_digest
                    && barrier.identity()? == expected_identity
                    && sqlite_sidecars_absent(&database.apply.target_path)?
                {
                    Ok(())
                } else {
                    Err("conflict database changed during guarded validation".to_string())
                }
            });
        match (inspected, copy_unchanged, cleanup, live_unchanged) {
            (Ok(value), Ok(()), Ok(()), Ok(())) => Ok(value),
            (Err(error), _, Ok(()), Ok(())) => Err(error),
            (_, Err(error), Ok(()), Ok(())) => Err(error),
            (_, _, Err(error), Ok(())) => Err(error),
            (_, _, _, Err(error)) => Err(error),
        }
    })();
    let staging_cleanup = if staging_root_created {
        match fs::remove_dir(&plan.staging_root) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err("conflict guarded copy staging cleanup failed".to_string()),
        }
    } else {
        Ok(())
    };
    match (result, staging_cleanup) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Ok(value), Ok(())) => Ok(value),
    }
}

fn cleanup_committed_conflict_artifacts(plan: &ConflictResolutionPlan) -> Result<(), String> {
    validate_plan(plan)?;
    for replacement in &plan.replacements {
        let phase = conflict_replacement_phase(plan, replacement)?;
        if !matches!(
            phase,
            ConflictReplacementPhase::CommittedWithRecovery | ConflictReplacementPhase::Cleaned
        ) {
            return Err(
                "conflict committed cleanup phase does not match its replacement".to_string(),
            );
        }
        let mut target = WriteExclusionGuard::acquire(&replacement.target_path)?;
        target.verify_current_path(Some(&replacement.replacement_sha256))?;
        let replacement_identity = conflict_replacement_identity(plan, replacement)?;
        if target.identity()? != replacement_identity {
            return Err("conflict committed replacement target identity changed".to_string());
        }
        let original_witness_exists = replacement.original_witness_path.exists();
        let witness_exists = replacement.replacement_witness_path.exists();
        let recovery_exists = replacement.recovery_path.exists();
        if phase == ConflictReplacementPhase::Cleaned {
            if original_witness_exists
                || witness_exists
                || recovery_exists
                || replacement.staging_path.exists()
                || replacement.tombstone_path.exists()
            {
                return Err("conflict cleaned replacement left an artifact".to_string());
            }
            continue;
        }
        if witness_exists {
            if stable_file_digest(&replacement.replacement_witness_path)?.1
                != replacement.replacement_sha256
                || !same_persisted_regular_file_identity(
                    &replacement.replacement_witness_path,
                    replacement_identity,
                )
                .unwrap_or(false)
            {
                return Err("conflict committed replacement ownership changed".to_string());
            }
        } else if recovery_exists {
            return Err("conflict committed replacement ownership witness is missing".to_string());
        }
        match (recovery_exists, original_witness_exists) {
            (true, true) => {
                if stable_file_digest(&replacement.recovery_path)?.1 != replacement.original_sha256
                    || stable_file_digest(&replacement.original_witness_path)?.1
                        != replacement.original_sha256
                    || !same_persisted_regular_file_identity(
                        &replacement.recovery_path,
                        replacement.original_identity,
                    )
                    .unwrap_or(false)
                    || !same_persisted_regular_file_identity(
                        &replacement.original_witness_path,
                        replacement.original_identity,
                    )
                    .unwrap_or(false)
                {
                    return Err("conflict committed recovery ownership changed".to_string());
                }
            }
            // Recovery is removed before its surviving exact-identity witness.
            // A crash in that window resumes by deleting only the witness.
            (false, true) => {
                if stable_file_digest(&replacement.original_witness_path)?.1
                    != replacement.original_sha256
                {
                    return Err("conflict committed original witness changed".to_string());
                }
            }
            (true, false) => {
                return Err("conflict committed recovery ownership witness is missing".to_string())
            }
            (false, false) => {}
        }
        drop(target);
        if recovery_exists {
            remove_exact_identity_conflict_artifact(
                &replacement.recovery_path,
                &replacement.original_sha256,
                replacement.original_identity,
            )?;
        }
        if original_witness_exists {
            remove_exact_identity_conflict_artifact(
                &replacement.original_witness_path,
                &replacement.original_sha256,
                replacement.original_identity,
            )?;
        }
        if witness_exists {
            remove_exact_identity_conflict_artifact(
                &replacement.replacement_witness_path,
                &replacement.replacement_sha256,
                replacement_identity,
            )?;
        }
        for path in [&replacement.staging_path, &replacement.tombstone_path] {
            if path.exists() {
                return Err("conflict committed replacement layout is invalid".to_string());
            }
        }
        transition_conflict_replacement_phase(
            plan,
            replacement,
            &[ConflictReplacementPhase::CommittedWithRecovery],
            ConflictReplacementPhase::Cleaned,
        )?;
    }
    if let Some(creation) = &plan.created_canonical {
        let phase = created_canonical_phase(plan)?;
        if !matches!(
            phase,
            ConflictCreatedCanonicalPhase::Committed | ConflictCreatedCanonicalPhase::Cleaned
        ) {
            return Err("conflict-created canonical committed cleanup phase changed".to_string());
        }
        let resolved = recover_handle_create(
            &typed_created_canonical_paths(creation)?,
            created_canonical_identity_bindings(creation),
            &creation.expected_sha256,
            HandleCreateRecoveryDecision::Commit,
        )?;
        let mut target = resolved.retain_for_recovery().ok_or_else(|| {
            "conflict-created canonical committed writer barrier is missing".to_string()
        })?;
        target.verify_current_path(Some(&creation.expected_sha256))?;
        if target.identity()? != creation.source_identity {
            return Err("conflict-created canonical ownership changed".to_string());
        }
        if phase == ConflictCreatedCanonicalPhase::Committed {
            transition_created_canonical_phase(
                plan,
                &[ConflictCreatedCanonicalPhase::Committed],
                ConflictCreatedCanonicalPhase::Cleaned,
            )?;
        }
    }
    for cleanup in &plan.cleanup {
        remove_committed_cleanup_artifact(
            &cleanup.tombstone_path,
            &cleanup.ownership_witness_path,
            &cleanup.sha256,
            cleanup.original_identity,
        )?;
        if let (Some(tombstone), Some(witness), Some(expected)) = (
            &cleanup.marker_tombstone_path,
            &cleanup.marker_ownership_witness_path,
            &cleanup.marker_sha256,
        ) {
            remove_committed_cleanup_artifact(
                tombstone,
                witness,
                expected,
                cleanup
                    .marker_original_identity
                    .ok_or_else(|| "conflict provider marker identity is missing".to_string())?,
            )?;
        }
    }
    complete_conflict_terminal_cleanup(plan)
}

fn remove_committed_cleanup_artifact(
    tombstone: &Path,
    witness: &Path,
    expected_sha256: &str,
    expected_identity: RegularFileIdentity,
) -> Result<(), String> {
    let tombstone_exists = tombstone.exists();
    let witness_exists = witness.exists();
    match (tombstone_exists, witness_exists) {
        (false, false) => return Ok(()),
        (true, true) => {
            if stable_file_digest(tombstone)?.1 != expected_sha256
                || stable_file_digest(witness)?.1 != expected_sha256
                || !same_persisted_regular_file_identity(tombstone, expected_identity)
                    .unwrap_or(false)
                || !same_persisted_regular_file_identity(witness, expected_identity)
                    .unwrap_or(false)
            {
                return Err("conflict committed cleanup ownership changed".to_string());
            }
        }
        // Cleanup deletes the tombstone before its surviving hard-link
        // witness. A crash in that one-way window is safe to resume because
        // the witness is still the only path this function will delete.
        (false, true) => {
            if stable_file_digest(witness)?.1 != expected_sha256 {
                return Err("conflict committed cleanup witness changed".to_string());
            }
            return remove_exact_identity_conflict_artifact(
                witness,
                expected_sha256,
                expected_identity,
            );
        }
        (true, false) => {
            return Err("conflict committed cleanup ownership is incomplete".to_string())
        }
    }
    remove_exact_identity_conflict_artifact(tombstone, expected_sha256, expected_identity)?;
    remove_exact_identity_conflict_artifact(witness, expected_sha256, expected_identity)
}

fn restore_step(plan: &ConflictResolutionPlan, step: &LedgerRollbackStep) -> Result<(), String> {
    let expected = step
        .expected_sha256
        .as_deref()
        .ok_or_else(|| "conflict rollback checksum is missing".to_string())?;
    match step.action {
        RollbackActionKind::RestoreDatabase => {
            rollback_conflict_replacement(plan, step, ConflictReplacementKind::RuntimeDatabase)?;
        }
        RollbackActionKind::RestoreFile => {
            let (_, source_sha256) = stable_file_digest(&step.source_path)?;
            if source_sha256 != expected {
                return Err("conflict file rollback source changed".to_string());
            }
            if path_key(&step.target_path) == path_key(&plan.session.target_path) {
                rollback_conflict_replacement(
                    plan,
                    step,
                    ConflictReplacementKind::CanonicalSession,
                )?;
            } else {
                restore_quarantined_cleanup_file(plan, &step.target_path, expected)?;
            }
        }
        RollbackActionKind::RemoveCreatedFile => {
            rollback_created_canonical(plan, &step.target_path, expected)?;
        }
        RollbackActionKind::RestoreConfig => {
            return Err("conflict rollback contains an unsupported config step".to_string())
        }
    }
    Ok(())
}

fn rollback_created_canonical(
    plan: &ConflictResolutionPlan,
    target: &Path,
    expected_sha256: &str,
) -> Result<(), String> {
    let creation = plan
        .created_canonical
        .as_ref()
        .ok_or_else(|| conflict_rollback_deferred("created canonical plan is missing"))?;
    if path_key(target) != path_key(&creation.target_path)
        || expected_sha256 != creation.expected_sha256
    {
        return Err(conflict_rollback_deferred(
            "created canonical rollback binding changed",
        ));
    }
    if runtime_databases_reference_path(plan, target)? {
        return Err(conflict_rollback_deferred(
            "created canonical is still referenced",
        ));
    }
    let paths = typed_created_canonical_paths(creation)?;
    let bindings = created_canonical_identity_bindings(creation);
    let state = classify_handle_create_crash_state(&paths, bindings, expected_sha256)
        .map_err(|_| conflict_rollback_deferred("created canonical state is unknown"))?;
    let phase = created_canonical_phase(plan)
        .map_err(|_| conflict_rollback_deferred("created canonical phase is unavailable"))?;
    let compatible = match state {
        HandleCreateCrashState::Absent => matches!(
            phase,
            ConflictCreatedCanonicalPhase::Absent
                | ConflictCreatedCanonicalPhase::Staging
                | ConflictCreatedCanonicalPhase::RollbackPrepared
                | ConflictCreatedCanonicalPhase::RolledBack
                | ConflictCreatedCanonicalPhase::Cleaned
        ),
        HandleCreateCrashState::Staged => matches!(
            phase,
            ConflictCreatedCanonicalPhase::Staging
                | ConflictCreatedCanonicalPhase::Staged
                | ConflictCreatedCanonicalPhase::Publishing
                | ConflictCreatedCanonicalPhase::RollbackPreparing
                | ConflictCreatedCanonicalPhase::RollbackPrepared
        ),
        HandleCreateCrashState::Published => matches!(
            phase,
            ConflictCreatedCanonicalPhase::Publishing
                | ConflictCreatedCanonicalPhase::Published
                | ConflictCreatedCanonicalPhase::Committing
                | ConflictCreatedCanonicalPhase::Committed
                | ConflictCreatedCanonicalPhase::RollbackPreparing
        ),
        HandleCreateCrashState::RollbackPrepared => matches!(
            phase,
            ConflictCreatedCanonicalPhase::RollbackPreparing
                | ConflictCreatedCanonicalPhase::RollbackPrepared
        ),
    };
    if !compatible {
        return Err(conflict_rollback_deferred(
            "created canonical logical phase does not match its physical layout",
        ));
    }
    if phase == ConflictCreatedCanonicalPhase::Cleaned {
        return Ok(());
    }
    if phase != ConflictCreatedCanonicalPhase::RollbackPreparing
        && phase != ConflictCreatedCanonicalPhase::RollbackPrepared
        && phase != ConflictCreatedCanonicalPhase::RolledBack
    {
        transition_created_canonical_phase(
            plan,
            &[
                ConflictCreatedCanonicalPhase::Absent,
                ConflictCreatedCanonicalPhase::Staging,
                ConflictCreatedCanonicalPhase::Staged,
                ConflictCreatedCanonicalPhase::Publishing,
                ConflictCreatedCanonicalPhase::Published,
                ConflictCreatedCanonicalPhase::Committing,
                ConflictCreatedCanonicalPhase::Committed,
            ],
            ConflictCreatedCanonicalPhase::RollbackPreparing,
        )?;
    }
    let resolved = recover_handle_create(
        &paths,
        bindings,
        expected_sha256,
        HandleCreateRecoveryDecision::Restore,
    )
    .map_err(|_| conflict_rollback_deferred("created canonical restore was contested"))?;
    if created_canonical_phase(plan)? == ConflictCreatedCanonicalPhase::RollbackPreparing {
        transition_created_canonical_phase(
            plan,
            &[ConflictCreatedCanonicalPhase::RollbackPreparing],
            ConflictCreatedCanonicalPhase::RollbackPrepared,
        )?;
    }
    resolved
        .cleanup_after_durable_terminal()
        .map_err(|(error, _resolved)| conflict_rollback_deferred(&error))?;
    transition_created_canonical_phase(
        plan,
        &[ConflictCreatedCanonicalPhase::RollbackPrepared],
        ConflictCreatedCanonicalPhase::RolledBack,
    )?;
    ensure_absent(target, "conflict-created canonical file")?;
    transition_created_canonical_phase(
        plan,
        &[ConflictCreatedCanonicalPhase::RolledBack],
        ConflictCreatedCanonicalPhase::Cleaned,
    )
}

fn rollback_conflict_replacement(
    plan: &ConflictResolutionPlan,
    step: &LedgerRollbackStep,
    expected_kind: ConflictReplacementKind,
) -> Result<(), String> {
    let expected = step
        .expected_sha256
        .as_deref()
        .ok_or_else(|| "conflict rollback checksum is missing".to_string())?;
    let applied = step
        .applied_sha256
        .as_deref()
        .ok_or_else(|| "conflict rollback applied checksum is missing".to_string())?;
    let replacement = replacement_for_target(plan, &step.target_path)?;
    if replacement.kind != expected_kind
        || replacement.original_sha256 != expected
        || replacement.replacement_sha256 != applied
    {
        return Err("conflict rollback replacement binding changed".to_string());
    }
    let phase = conflict_replacement_phase(plan, replacement).map_err(|error| {
        conflict_rollback_deferred(&format!("logical phase record is unavailable: {error}"))
    })?;
    if matches!(
        phase,
        ConflictReplacementPhase::Planned | ConflictReplacementPhase::WitnessCreating
    ) {
        for path in [
            &replacement.staging_path,
            &replacement.recovery_path,
            &replacement.tombstone_path,
        ] {
            if path.exists() {
                return Err(conflict_rollback_deferred(
                    "pre-witness phase has a replacement artifact",
                ));
            }
        }
        verify_conflict_original_witness(replacement, &replacement.target_path)?;
        if replacement.replacement_witness_path.exists() {
            if phase != ConflictReplacementPhase::WitnessCreating
                || stable_file_digest(&replacement.replacement_witness_path)?.1 != applied
            {
                return Err(conflict_rollback_deferred(
                    "unbound replacement witness cannot be adopted",
                ));
            }
            let bindings = HandleReplaceIdentityBindings {
                parent_identity: replacement.parent_identity,
                original_identity: replacement.original_identity,
                replacement_identity: regular_file_identity_at_path(
                    &replacement.replacement_witness_path,
                )?,
            };
            record_conflict_replacement_witness_ready(plan, replacement, bindings)?;
            remove_exact_identity_conflict_artifact(
                &replacement.replacement_witness_path,
                applied,
                bindings.replacement_identity,
            )?;
            transition_conflict_replacement_phase(
                plan,
                replacement,
                &[ConflictReplacementPhase::WitnessReady],
                ConflictReplacementPhase::Cleaned,
            )?;
        } else {
            transition_conflict_replacement_phase(
                plan,
                replacement,
                &[
                    ConflictReplacementPhase::Planned,
                    ConflictReplacementPhase::WitnessCreating,
                ],
                ConflictReplacementPhase::Cleaned,
            )?;
        }
        return Ok(());
    }
    let replacement_identity = conflict_replacement_identity(plan, replacement)
        .map_err(|_| conflict_rollback_deferred("replacement identity is unavailable"))?;
    let parent_identity = conflict_replacement_parent_identity(plan, replacement)
        .map_err(|_| conflict_rollback_deferred("replacement parent identity is unavailable"))?;
    let paths = typed_replacement_paths(replacement)?;
    let state = super::write_barrier::classify_handle_replace_crash_state(
        &paths,
        HandleReplaceIdentityBindings {
            parent_identity,
            original_identity: replacement.original_identity,
            replacement_identity,
        },
        expected,
        applied,
    )
    .map_err(|_| "conflict replacement rollback deferred: state is unknown".to_string())?;
    match state {
        super::write_barrier::HandleReplaceCrashState::Original => {
            if !matches!(
                phase,
                ConflictReplacementPhase::Planned
                    | ConflictReplacementPhase::WitnessReady
                    | ConflictReplacementPhase::Preparing
                    | ConflictReplacementPhase::RolledBack
                    | ConflictReplacementPhase::Cleaned
            ) {
                return Err(conflict_rollback_deferred(
                    "logical phase does not match the original target layout",
                ));
            }
            if replacement.staging_path.exists()
                || replacement.recovery_path.exists()
                || replacement.tombstone_path.exists()
            {
                return Err(conflict_rollback_deferred(
                    "original target has an unexpected replacement artifact",
                ));
            }
            if replacement.original_witness_path.exists() {
                verify_conflict_original_witness(replacement, &replacement.target_path)?;
            } else if phase != ConflictReplacementPhase::Cleaned {
                return Err(conflict_rollback_deferred(
                    "original replacement witness is missing",
                ));
            }
            if replacement.replacement_witness_path.exists() {
                remove_exact_identity_conflict_artifact(
                    &replacement.replacement_witness_path,
                    applied,
                    replacement_identity,
                )?;
            }
            transition_conflict_replacement_phase(
                plan,
                replacement,
                &[
                    ConflictReplacementPhase::Planned,
                    ConflictReplacementPhase::WitnessReady,
                    ConflictReplacementPhase::Preparing,
                    ConflictReplacementPhase::RolledBack,
                ],
                ConflictReplacementPhase::Cleaned,
            )?;
            return Ok(());
        }
        super::write_barrier::HandleReplaceCrashState::Staged => {
            if !matches!(
                phase,
                ConflictReplacementPhase::Preparing
                    | ConflictReplacementPhase::RollbackPrepared
                    | ConflictReplacementPhase::RolledBack
            ) || !replacement.replacement_witness_path.is_file()
                || stable_file_digest(&replacement.replacement_witness_path)?.1 != applied
                || !same_persisted_regular_file_identity(
                    &replacement.staging_path,
                    replacement_identity,
                )
                .unwrap_or(false)
                || !same_persisted_regular_file_identity(
                    &replacement.replacement_witness_path,
                    replacement_identity,
                )
                .unwrap_or(false)
            {
                return Err(conflict_rollback_deferred(
                    "staged replacement ownership or logical phase is unproven",
                ));
            }
            verify_conflict_original_witness(replacement, &replacement.target_path)?;
            transition_conflict_replacement_phase(
                plan,
                replacement,
                &[
                    ConflictReplacementPhase::Preparing,
                    ConflictReplacementPhase::RollbackPrepared,
                    ConflictReplacementPhase::RolledBack,
                ],
                ConflictReplacementPhase::RolledBack,
            )?;
            return Ok(());
        }
        super::write_barrier::HandleReplaceCrashState::Prepared => {
            if !matches!(
                phase,
                ConflictReplacementPhase::Preparing
                    | ConflictReplacementPhase::Prepared
                    | ConflictReplacementPhase::Publishing
            ) || !replacement.replacement_witness_path.is_file()
                || stable_file_digest(&replacement.replacement_witness_path)?.1 != applied
                || !same_persisted_regular_file_identity(
                    &replacement.staging_path,
                    replacement_identity,
                )
                .unwrap_or(false)
                || !same_persisted_regular_file_identity(
                    &replacement.replacement_witness_path,
                    replacement_identity,
                )
                .unwrap_or(false)
            {
                return Err(conflict_rollback_deferred(
                    "prepared replacement ownership is unproven",
                ));
            }
            verify_conflict_original_witness(replacement, &replacement.recovery_path)?;
            transition_conflict_replacement_phase(
                plan,
                replacement,
                &[
                    ConflictReplacementPhase::Preparing,
                    ConflictReplacementPhase::Prepared,
                    ConflictReplacementPhase::Publishing,
                ],
                ConflictReplacementPhase::RollbackPrepared,
            )?;
            let mut recovery = DestructiveFileGuard::acquire(&replacement.recovery_path)
                .map_err(|_| conflict_rollback_deferred("recovery handle is unavailable"))?;
            recovery
                .verify_current_path(Some(expected))
                .map_err(|_| conflict_rollback_deferred("recovery identity changed"))?;
            recovery
                .rename_no_replace(&replacement.target_path)
                .map_err(|_| conflict_rollback_deferred("target appeared during restore"))?;
            drop(recovery);
            transition_conflict_replacement_phase(
                plan,
                replacement,
                &[ConflictReplacementPhase::RollbackPrepared],
                ConflictReplacementPhase::RolledBack,
            )?;
            return Ok(());
        }
        super::write_barrier::HandleReplaceCrashState::ReplacementWithRecovery => {
            if !matches!(
                phase,
                ConflictReplacementPhase::Publishing
                    | ConflictReplacementPhase::Published
                    | ConflictReplacementPhase::Committing
                    | ConflictReplacementPhase::CommittedWithRecovery
                    | ConflictReplacementPhase::RollbackPreparing
            ) || !replacement.replacement_witness_path.is_file()
                || stable_file_digest(&replacement.replacement_witness_path)?.1 != applied
                || !same_persisted_regular_file_identity(
                    &replacement.target_path,
                    replacement_identity,
                )
                .unwrap_or(false)
                || !same_persisted_regular_file_identity(
                    &replacement.replacement_witness_path,
                    replacement_identity,
                )
                .unwrap_or(false)
            {
                return Err(conflict_rollback_deferred(
                    "target is not the operation replacement identity",
                ));
            }
            verify_conflict_original_witness(replacement, &replacement.recovery_path)?;
            transition_conflict_replacement_phase(
                plan,
                replacement,
                &[
                    ConflictReplacementPhase::Publishing,
                    ConflictReplacementPhase::Published,
                    ConflictReplacementPhase::Committing,
                    ConflictReplacementPhase::CommittedWithRecovery,
                    ConflictReplacementPhase::RollbackPreparing,
                ],
                ConflictReplacementPhase::RollbackPreparing,
            )?;
            let mut target = DestructiveFileGuard::acquire(&replacement.target_path)
                .map_err(|_| conflict_rollback_deferred("target handle is unavailable"))?;
            target
                .verify_current_path(Some(applied))
                .map_err(|_| conflict_rollback_deferred("target changed after apply"))?;
            target
                .rename_no_replace(&replacement.tombstone_path)
                .map_err(|_| conflict_rollback_deferred("target quarantine was contested"))?;
            transition_conflict_replacement_phase(
                plan,
                replacement,
                &[ConflictReplacementPhase::RollbackPreparing],
                ConflictReplacementPhase::RollbackPrepared,
            )?;
        }
        super::write_barrier::HandleReplaceCrashState::ReplacementOnly => {
            // Once the exact original recovery object is gone, restore is no
            // longer possible. A rollback must never synthesize an equal-hash
            // old object or overwrite the committed replacement.
            return Err(conflict_rollback_deferred(
                "committed replacement recovery is no longer available",
            ));
        }
        super::write_barrier::HandleReplaceCrashState::RollbackPrepared => {
            if !matches!(
                phase,
                ConflictReplacementPhase::RollbackPreparing
                    | ConflictReplacementPhase::RollbackPrepared
            ) || !replacement.replacement_witness_path.is_file()
                || stable_file_digest(&replacement.replacement_witness_path)?.1 != applied
                || !same_persisted_regular_file_identity(
                    &replacement.tombstone_path,
                    replacement_identity,
                )
                .unwrap_or(false)
                || !same_persisted_regular_file_identity(
                    &replacement.replacement_witness_path,
                    replacement_identity,
                )
                .unwrap_or(false)
            {
                return Err(conflict_rollback_deferred(
                    "rollback tombstone ownership is unproven",
                ));
            }
            verify_conflict_original_witness(replacement, &replacement.recovery_path)?;
            if phase == ConflictReplacementPhase::RollbackPreparing {
                transition_conflict_replacement_phase(
                    plan,
                    replacement,
                    &[ConflictReplacementPhase::RollbackPreparing],
                    ConflictReplacementPhase::RollbackPrepared,
                )?;
            }
        }
        super::write_barrier::HandleReplaceCrashState::RolledBack => {
            if !matches!(
                phase,
                ConflictReplacementPhase::RollbackPrepared | ConflictReplacementPhase::RolledBack
            ) || !replacement.replacement_witness_path.is_file()
                || stable_file_digest(&replacement.replacement_witness_path)?.1 != applied
                || !same_persisted_regular_file_identity(
                    &replacement.tombstone_path,
                    replacement_identity,
                )
                .unwrap_or(false)
                || !same_persisted_regular_file_identity(
                    &replacement.replacement_witness_path,
                    replacement_identity,
                )
                .unwrap_or(false)
            {
                return Err(conflict_rollback_deferred(
                    "rolled-back tombstone ownership is unproven",
                ));
            }
            verify_conflict_original_witness(replacement, &replacement.target_path)?;
            if phase == ConflictReplacementPhase::RollbackPrepared {
                transition_conflict_replacement_phase(
                    plan,
                    replacement,
                    &[ConflictReplacementPhase::RollbackPrepared],
                    ConflictReplacementPhase::RolledBack,
                )?;
            }
            return Ok(());
        }
    }
    let mut recovery = DestructiveFileGuard::acquire(&replacement.recovery_path)
        .map_err(|_| conflict_rollback_deferred("recovery handle is unavailable"))?;
    recovery
        .verify_current_path(Some(expected))
        .map_err(|_| conflict_rollback_deferred("recovery identity changed"))?;
    recovery
        .rename_no_replace(&replacement.target_path)
        .map_err(|_| conflict_rollback_deferred("target restore was contested"))?;
    drop(recovery);
    verify_conflict_original_witness(replacement, &replacement.target_path)?;
    transition_conflict_replacement_phase(
        plan,
        replacement,
        &[ConflictReplacementPhase::RollbackPrepared],
        ConflictReplacementPhase::RolledBack,
    )
}

fn conflict_rollback_deferred(reason: &str) -> String {
    format!("conflict replacement rollback deferred: {reason}")
}

fn cleanup_rolled_back_conflict_replacement(
    plan: &ConflictResolutionPlan,
    replacement: &ConflictReplacementPlan,
) -> Result<(), String> {
    if stable_file_digest(&replacement.target_path)?.1 != replacement.original_sha256 {
        return Err(conflict_rollback_deferred(
            "restored target verification failed",
        ));
    }
    verify_conflict_original_witness(replacement, &replacement.target_path)?;
    if replacement.recovery_path.exists() {
        return Err(conflict_rollback_deferred(
            "recovery path remained after restore",
        ));
    }
    for (path, expected) in [
        (&replacement.tombstone_path, &replacement.replacement_sha256),
        (
            &replacement.replacement_witness_path,
            &replacement.replacement_sha256,
        ),
        (&replacement.staging_path, &replacement.replacement_sha256),
    ] {
        remove_exact_identity_conflict_artifact(
            path,
            expected,
            conflict_replacement_identity(plan, replacement)?,
        )?;
    }
    Ok(())
}

/// Cleans operation-owned rollback artifacts only after the operation ledger
/// has durably reached `RolledBack`. Until then, exact-identity witnesses stay
/// available so a later rollback step cannot lose its only original object.
fn cleanup_rolled_back_conflict_artifacts(plan: &ConflictResolutionPlan) -> Result<(), String> {
    validate_plan(plan)?;
    for replacement in &plan.replacements {
        match conflict_replacement_phase(plan, replacement)? {
            ConflictReplacementPhase::RolledBack => {
                cleanup_rolled_back_conflict_replacement(plan, replacement)?;
                transition_conflict_replacement_phase(
                    plan,
                    replacement,
                    &[ConflictReplacementPhase::RolledBack],
                    ConflictReplacementPhase::Cleaned,
                )?;
            }
            ConflictReplacementPhase::Cleaned => {
                for path in [
                    &replacement.replacement_witness_path,
                    &replacement.staging_path,
                    &replacement.recovery_path,
                    &replacement.tombstone_path,
                ] {
                    ensure_absent(path, "cleaned conflict rollback artifact")?;
                }
            }
            _ => {
                return Err(
                    "conflict rolled-back cleanup phase does not match its replacement".to_string(),
                )
            }
        }
        remove_rolled_back_original_witness(replacement)?;
    }
    for cleanup in &plan.cleanup {
        cleanup_restored_cleanup_witness(
            &cleanup.path,
            &cleanup.ownership_witness_path,
            &cleanup.tombstone_path,
            &cleanup.sha256,
            cleanup.original_identity,
        )?;
        if let (Some(target), Some(witness), Some(tombstone), Some(expected), Some(identity)) = (
            &cleanup.marker_path,
            &cleanup.marker_ownership_witness_path,
            &cleanup.marker_tombstone_path,
            &cleanup.marker_sha256,
            cleanup.marker_original_identity,
        ) {
            cleanup_restored_cleanup_witness(target, witness, tombstone, expected, identity)?;
        }
    }
    complete_conflict_terminal_cleanup(plan)
}

fn restore_quarantined_cleanup_file(
    plan: &ConflictResolutionPlan,
    target: &Path,
    expected_sha256: &str,
) -> Result<(), String> {
    let (witness, tombstone, expected_identity) = cleanup_artifacts_for_target(plan, target)?;
    match fs::symlink_metadata(target) {
        Ok(metadata) if metadata.is_file() && !metadata_is_link_or_reparse(&metadata) => {
            if stable_file_digest(target)?.1 == expected_sha256 {
                if tombstone.exists()
                    || !same_persisted_regular_file_identity(target, expected_identity)
                        .unwrap_or(false)
                    || (witness.exists()
                        && !same_persisted_regular_file_identity(&witness, expected_identity)
                            .unwrap_or(false))
                {
                    return Err(conflict_rollback_deferred(
                        "cleanup target has a different file identity",
                    ));
                }
                return Ok(());
            }
            return Err(conflict_rollback_deferred(
                "cleanup target changed externally",
            ));
        }
        Ok(_) => return Err(conflict_rollback_deferred("cleanup target is unsafe")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return Err(conflict_rollback_deferred("cleanup target is unavailable")),
    }
    if !witness.is_file()
        || !tombstone.is_file()
        || stable_file_digest(&witness)?.1 != expected_sha256
        || stable_file_digest(&tombstone)?.1 != expected_sha256
        || !same_persisted_regular_file_identity(&witness, expected_identity).unwrap_or(false)
        || !same_persisted_regular_file_identity(&tombstone, expected_identity).unwrap_or(false)
    {
        return Err(conflict_rollback_deferred(
            "cleanup tombstone ownership is unproven",
        ));
    }
    let mut destructive = DestructiveFileGuard::acquire(&tombstone)
        .map_err(|_| conflict_rollback_deferred("cleanup tombstone handle is unavailable"))?;
    destructive
        .verify_current_path(Some(expected_sha256))
        .map_err(|_| conflict_rollback_deferred("cleanup tombstone changed"))?;
    if destructive
        .identity()
        .map_err(|_| conflict_rollback_deferred("cleanup tombstone identity is unavailable"))?
        != expected_identity
    {
        return Err(conflict_rollback_deferred(
            "cleanup tombstone identity changed",
        ));
    }
    destructive
        .verify_same_identity_path(&witness, Some(expected_sha256))
        .map_err(|_| conflict_rollback_deferred("cleanup witness identity changed"))?;
    destructive
        .rename_no_replace(target)
        .map_err(|_| conflict_rollback_deferred("cleanup target appeared during restore"))?;
    destructive
        .verify_same_identity_path(&witness, Some(expected_sha256))
        .map_err(|_| conflict_rollback_deferred("restored cleanup witness identity changed"))?;
    Ok(())
}

fn cleanup_artifacts_for_target(
    plan: &ConflictResolutionPlan,
    target: &Path,
) -> Result<(PathBuf, PathBuf, RegularFileIdentity), String> {
    for cleanup in &plan.cleanup {
        if path_key(&cleanup.path) == path_key(target) {
            return Ok((
                cleanup.ownership_witness_path.clone(),
                cleanup.tombstone_path.clone(),
                cleanup.original_identity,
            ));
        }
        if cleanup
            .marker_path
            .as_ref()
            .is_some_and(|marker| path_key(marker) == path_key(target))
        {
            return match (
                &cleanup.marker_ownership_witness_path,
                &cleanup.marker_tombstone_path,
            ) {
                (Some(witness), Some(tombstone)) => Ok((
                    witness.clone(),
                    tombstone.clone(),
                    cleanup.marker_original_identity.ok_or_else(|| {
                        "conflict marker cleanup identity binding is missing".to_string()
                    })?,
                )),
                _ => Err("conflict marker cleanup artifact binding is missing".to_string()),
            };
        }
    }
    Err("conflict cleanup rollback target is not plan-bound".to_string())
}

fn cleanup_restored_cleanup_witness(
    target: &Path,
    witness: &Path,
    tombstone: &Path,
    expected_sha256: &str,
    expected_identity: RegularFileIdentity,
) -> Result<(), String> {
    if tombstone.exists() {
        return Err(conflict_rollback_deferred(
            "cleanup tombstone remained after restore",
        ));
    }
    match fs::symlink_metadata(witness) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Ok(metadata) if metadata.is_file() && !metadata_is_link_or_reparse(&metadata) => {}
        Ok(_) => return Err(conflict_rollback_deferred("cleanup witness is unsafe")),
        Err(_) => return Err(conflict_rollback_deferred("cleanup witness is unavailable")),
    }
    let mut witness_guard = DestructiveFileGuard::acquire(witness)
        .map_err(|_| conflict_rollback_deferred("cleanup witness handle is unavailable"))?;
    witness_guard
        .verify_current_path(Some(expected_sha256))
        .map_err(|_| conflict_rollback_deferred("cleanup witness changed"))?;
    if witness_guard
        .identity()
        .map_err(|_| conflict_rollback_deferred("cleanup witness identity is unavailable"))?
        != expected_identity
        || witness_guard
            .verify_same_identity_path(target, Some(expected_sha256))
            .map_err(|_| conflict_rollback_deferred("restored cleanup target changed"))?
            != expected_identity
    {
        return Err(conflict_rollback_deferred(
            "restored cleanup ownership changed",
        ));
    }
    witness_guard
        .delete()
        .map_err(|_| conflict_rollback_deferred("cleanup witness removal failed"))
}

fn remove_exact_identity_conflict_artifact(
    path: &Path,
    expected_sha256: &str,
    expected_identity: RegularFileIdentity,
) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Ok(metadata) if metadata.is_file() && !metadata_is_link_or_reparse(&metadata) => {}
        Ok(_) => return Err("conflict artifact cleanup target is unsafe".to_string()),
        Err(_) => return Err("conflict artifact cleanup target is unavailable".to_string()),
    }
    let mut destructive = DestructiveFileGuard::acquire(path)?;
    destructive.verify_current_path(Some(expected_sha256))?;
    if destructive.identity()? != expected_identity {
        return Err("conflict artifact cleanup identity changed".to_string());
    }
    destructive.delete()
}

fn validate_rolled_back(plan: &ConflictResolutionPlan) -> Result<(), String> {
    for database in &plan.databases {
        let (_, sha256) = stable_file_digest(&database.apply.target_path)?;
        if sha256 != database.live_before_sha256 {
            return Err("conflict database rollback did not restore its snapshot".to_string());
        }
        if !same_persisted_regular_file_identity(
            &database.apply.target_path,
            database.live_before_identity,
        )? {
            return Err("conflict database rollback identity changed".to_string());
        }
        if !sqlite_sidecars_absent(&database.apply.target_path)? {
            return Err("conflict database rollback left an active sidecar".to_string());
        }
        quick_check_conflict_database(&database.apply.target_path)?;
    }
    match plan.canonical_before_sha256.as_deref() {
        Some(expected) => {
            let (_, sha256) = stable_file_digest(&plan.session.target_path)?;
            if sha256 != expected {
                return Err(
                    "conflict canonical rollback did not restore the prior version".to_string(),
                );
            }
            if !same_persisted_regular_file_identity(
                &plan.session.target_path,
                plan.canonical_before_identity
                    .ok_or_else(|| "conflict canonical original identity is missing".to_string())?,
            )? {
                return Err("conflict canonical rollback identity changed".to_string());
            }
        }
        None => ensure_absent(&plan.session.target_path, "conflict-created canonical file")?,
    }
    for cleanup in &plan.cleanup {
        let (_, sha256) = stable_file_digest(&cleanup.path)?;
        if sha256 != cleanup.sha256 {
            return Err("conflict source rollback did not restore the original".to_string());
        }
        if let (Some(path), Some(expected)) = (&cleanup.marker_path, &cleanup.marker_sha256) {
            let (_, sha256) = stable_file_digest(path)?;
            if &sha256 != expected {
                return Err("conflict marker rollback did not restore the original".to_string());
            }
        }
    }
    Ok(())
}

fn runtime_databases_reference_path(
    plan: &ConflictResolutionPlan,
    target: &Path,
) -> Result<bool, String> {
    let target_key = path_key(target);
    for database in &plan.databases {
        let connection = Connection::open_with_flags(
            &database.apply.target_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|_| "failed to inspect conflict rollback references".to_string())?;
        let mut statement = connection
            .prepare("SELECT rollout_path FROM threads WHERE rollout_path IS NOT NULL")
            .map_err(|_| "failed to inspect conflict rollback references".to_string())?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|_| "failed to inspect conflict rollback references".to_string())?;
        for row in rows {
            let path = row.map_err(|_| "failed to read conflict rollback reference".to_string())?;
            if path_key(Path::new(&path)) == target_key {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn prepared_result(
    plan: ConflictResolutionPlan,
    plan_path: &Path,
) -> Result<PreparedConflictResolution, String> {
    validate_plan(&plan)?;
    verify_plan_backup(&plan)?;
    let (plan_bytes, plan_sha256) = stable_file_digest(plan_path)?;
    let plan_snapshot = LedgerFileSnapshot {
        path: plan_path.to_path_buf(),
        bytes: plan_bytes,
        sha256: plan_sha256,
        created_by_operation: true,
        logical_thread_id: None,
    };
    let mut created_files = vec![plan_snapshot.clone()];
    if let Some(staged) = &plan.session.staged_path {
        let (bytes, sha256) = stable_file_digest(staged)?;
        created_files.push(LedgerFileSnapshot {
            path: staged.clone(),
            bytes,
            sha256,
            created_by_operation: true,
            logical_thread_id: Some(plan.session.thread_id.clone()),
        });
    }
    for database in &plan.databases {
        let (bytes, sha256) = stable_file_digest(&database.apply.staged_path)?;
        created_files.push(LedgerFileSnapshot {
            path: database.apply.staged_path.clone(),
            bytes,
            sha256,
            created_by_operation: true,
            logical_thread_id: None,
        });
    }
    let database_snapshots = plan
        .databases
        .iter()
        .map(|database| {
            let (bytes, sha256) = stable_file_digest(&database.apply.original_backup_payload)?;
            if sha256 != database.apply.original_sha256 {
                return Err("conflict database backup snapshot changed".to_string());
            }
            Ok(LedgerDatabaseSnapshot {
                source_path: database.apply.target_path.clone(),
                snapshot_path: database.apply.original_backup_payload.clone(),
                bytes,
                sha256,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let rollback_steps = rollback_steps(&plan);
    Ok(PreparedConflictResolution {
        plan,
        plan_snapshot,
        created_files,
        database_snapshots,
        rollback_steps,
    })
}

fn rollback_steps(plan: &ConflictResolutionPlan) -> Vec<LedgerRollbackStep> {
    let mut steps = Vec::new();
    for cleanup in plan.cleanup.iter().rev() {
        steps.push(LedgerRollbackStep {
            action: RollbackActionKind::RestoreFile,
            source_path: cleanup.backup_payload.clone(),
            target_path: cleanup.path.clone(),
            expected_sha256: Some(cleanup.sha256.clone()),
            applied_sha256: None,
            completed: false,
        });
        if let (Some(source), Some(target), Some(expected)) = (
            &cleanup.marker_backup_payload,
            &cleanup.marker_path,
            &cleanup.marker_sha256,
        ) {
            steps.push(LedgerRollbackStep {
                action: RollbackActionKind::RestoreFile,
                source_path: source.clone(),
                target_path: target.clone(),
                expected_sha256: Some(expected.clone()),
                applied_sha256: None,
                completed: false,
            });
        }
    }
    steps.extend(
        plan.databases
            .iter()
            .rev()
            .filter(|database| database.live_before_sha256 != database.apply.staged_sha256)
            .map(|database| LedgerRollbackStep {
                action: RollbackActionKind::RestoreDatabase,
                source_path: database.apply.original_backup_payload.clone(),
                target_path: database.apply.target_path.clone(),
                expected_sha256: Some(database.live_before_sha256.clone()),
                applied_sha256: Some(database.apply.staged_sha256.clone()),
                completed: false,
            }),
    );
    match (
        plan.canonical_before_sha256.as_ref(),
        plan.session.target_backup_payload.as_ref(),
    ) {
        (Some(expected), Some(source)) if expected != &plan.chosen_sha256 => {
            steps.push(LedgerRollbackStep {
                action: RollbackActionKind::RestoreFile,
                source_path: source.clone(),
                target_path: plan.session.target_path.clone(),
                expected_sha256: Some(expected.clone()),
                applied_sha256: Some(plan.chosen_sha256.clone()),
                completed: false,
            })
        }
        (None, _) => steps.push(LedgerRollbackStep {
            action: RollbackActionKind::RemoveCreatedFile,
            source_path: plan
                .session
                .staged_path
                .clone()
                .unwrap_or_else(|| plan.session.source_path.clone()),
            target_path: plan.session.target_path.clone(),
            expected_sha256: Some(plan.chosen_sha256.clone()),
            applied_sha256: Some(plan.chosen_sha256.clone()),
            completed: false,
        }),
        _ => {}
    }
    steps
}

fn rollback_plan_matches(actual: &[LedgerRollbackStep], expected: &[LedgerRollbackStep]) -> bool {
    actual.len() == expected.len()
        && actual.iter().zip(expected).all(|(actual, expected)| {
            actual.action == expected.action
                && actual.source_path == expected.source_path
                && actual.target_path == expected.target_path
                && actual.expected_sha256 == expected.expected_sha256
                && actual.applied_sha256 == expected.applied_sha256
        })
}

fn verify_plan_backup(plan: &ConflictResolutionPlan) -> Result<MigrationBackupManifest, String> {
    let backup = verify_migration_backup(&plan.backup_dir)?;
    if backup.operation_id != plan.operation_id
        || backup.status != MigrationBackupStatus::IsolatedRestoreVerified
        || backup.expires_at_ms != plan.recovery_expires_at_ms
    {
        return Err("conflict recovery package identity changed".to_string());
    }
    Ok(backup)
}

fn validate_plan(plan: &ConflictResolutionPlan) -> Result<(), String> {
    if plan.schema_version != CONFLICT_RESOLUTION_SCHEMA_VERSION {
        return Err("conflict resolution plan version is unsupported".to_string());
    }
    validate_operation_id(&plan.operation_id)?;
    validate_operation_id(&plan.migration_operation_id)?;
    validate_conflict_id(&plan.conflict_id)?;
    for path in [
        &plan.canonical_root,
        &plan.data_root,
        &plan.backup_dir,
        &plan.staging_root,
        &plan.session.source_path,
        &plan.session.target_path,
    ] {
        if !path.is_absolute() {
            return Err("conflict resolution plan path is invalid".to_string());
        }
    }
    if !plan
        .staging_root
        .starts_with(operation_root(&plan.data_root, &plan.operation_id)?)
        || !plan
            .backup_dir
            .starts_with(plan.data_root.join("session-storage-v1/conflict-recovery"))
        || plan.recovery_expires_at_ms < plan.created_at_ms
    {
        return Err("conflict resolution managed path is invalid".to_string());
    }
    validate_sha256(&plan.chosen_sha256)?;
    validate_sha256(&plan.rejected_sha256)?;
    if let Some(value) = &plan.canonical_before_sha256 {
        validate_sha256(value)?;
    }
    if !matches!(
        (
            &plan.canonical_before_sha256,
            plan.canonical_before_identity
        ),
        (Some(_), Some(_)) | (None, None)
    ) {
        return Err("conflict canonical original identity plan is invalid".to_string());
    }
    if plan.session.thread_id.trim().is_empty()
        || plan.session.expected_sha256 != plan.chosen_sha256
        || plan
            .session
            .staged_path
            .as_ref()
            .is_none_or(|path| !path.is_absolute() || !path.starts_with(&plan.staging_root))
        || plan
            .session
            .target_backup_payload
            .as_ref()
            .is_some_and(|path| !path.starts_with(&plan.backup_dir))
    {
        return Err("conflict resolution session plan is invalid".to_string());
    }
    let mut database_paths = BTreeSet::new();
    for database in &plan.databases {
        validate_sha256(&database.live_before_sha256)?;
        validate_sha256(&database.apply.original_sha256)?;
        validate_sha256(&database.apply.staged_sha256)?;
        if !database_paths.insert(durable_conflict_path_key(&database.apply.target_path))
            || !database.apply.target_path.is_absolute()
            || !database.apply.staged_path.starts_with(&plan.staging_root)
            || !database
                .apply
                .original_backup_payload
                .starts_with(&plan.backup_dir)
        {
            return Err("conflict resolution database plan is invalid".to_string());
        }
    }
    if plan.databases.is_empty() {
        return Err("conflict resolution has no runtime database".to_string());
    }
    let mut cleanup_paths = BTreeSet::new();
    for cleanup in &plan.cleanup {
        validate_sha256(&cleanup.sha256)?;
        if !cleanup.path.is_absolute()
            || cleanup.bytes == 0
            || durable_conflict_path_key(&cleanup.path)
                == durable_conflict_path_key(&plan.session.target_path)
            || !cleanup_paths.insert(durable_conflict_path_key(&cleanup.path))
            || !cleanup.backup_payload.starts_with(&plan.backup_dir)
        {
            return Err("conflict cleanup plan is invalid".to_string());
        }
        let expected_cleanup_paths =
            cleanup_artifact_paths(&plan.operation_id, &cleanup.path, "session")?;
        if cleanup.ownership_witness_path != expected_cleanup_paths.0
            || cleanup.tombstone_path != expected_cleanup_paths.1
        {
            return Err("conflict cleanup ownership path binding changed".to_string());
        }
        if cleanup.parent_identity != parent_directory_identity_at_path(&cleanup.path)? {
            return Err("conflict cleanup parent identity changed".to_string());
        }
        match (
            &cleanup.marker_path,
            &cleanup.marker_backup_payload,
            cleanup.marker_bytes,
            &cleanup.marker_sha256,
            cleanup.marker_parent_identity,
            cleanup.marker_original_identity,
            &cleanup.marker_ownership_witness_path,
            &cleanup.marker_tombstone_path,
        ) {
            (None, None, None, None, None, None, None, None) => {}
            (
                Some(path),
                Some(payload),
                Some(bytes),
                Some(sha256),
                Some(parent_identity),
                Some(_identity),
                Some(witness),
                Some(tombstone),
            ) if path.is_absolute()
                && bytes > 0
                && payload.starts_with(&plan.backup_dir)
                && parent_directory_identity_at_path(path).ok() == Some(parent_identity)
                && provider_marker_path(&cleanup.path).ok().as_ref() == Some(path)
                && cleanup_artifact_paths(&plan.operation_id, path, "provider-marker")
                    .is_ok_and(|expected| {
                        expected.0.as_path() == witness.as_path()
                            && expected.1.as_path() == tombstone.as_path()
                    }) =>
            {
                validate_sha256(sha256)?;
            }
            _ => return Err("conflict cleanup marker plan is invalid".to_string()),
        }
        if !cleanup.ownership_witness_path.is_absolute()
            || !cleanup.tombstone_path.is_absolute()
            || cleanup.ownership_witness_path.parent() != cleanup.path.parent()
            || cleanup.tombstone_path.parent() != cleanup.path.parent()
        {
            return Err("conflict cleanup ownership path is invalid".to_string());
        }
    }
    match (
        plan.canonical_before_sha256.as_ref(),
        plan.created_canonical.as_ref(),
    ) {
        (None, Some(creation)) => {
            let expected_paths =
                created_canonical_artifact_paths(&plan.operation_id, &plan.session.target_path)?;
            if creation.target_path != plan.session.target_path
                || plan.session.staged_path.as_ref() != Some(&creation.source_path)
                || creation.expected_sha256 != plan.session.expected_sha256
                || creation.staging_path != expected_paths.0
                || creation.rollback_tombstone_path != expected_paths.1
                || creation.parent_identity
                    != parent_directory_identity_at_path(&creation.target_path)?
            {
                return Err("conflict canonical creation plan changed".to_string());
            }
            let typed = typed_created_canonical_paths(creation)?;
            if typed.target_path() != creation.target_path
                || typed.staging_path() != creation.staging_path
                || typed.rollback_tombstone_path() != creation.rollback_tombstone_path
            {
                return Err("conflict canonical creation typed paths changed".to_string());
            }
        }
        (Some(_), None) => {}
        _ => return Err("conflict canonical creation plan is invalid".to_string()),
    }
    let persisted_identities = plan
        .replacements
        .iter()
        .map(|replacement| {
            (
                durable_conflict_path_key(&replacement.target_path),
                (replacement.parent_identity, replacement.original_identity),
            )
        })
        .collect::<BTreeMap<_, _>>();
    if persisted_identities.len() != plan.replacements.len() {
        return Err("conflict replacement target is duplicated".to_string());
    }
    let expected_replacements = build_conflict_replacements(
        &plan.operation_id,
        &plan.session,
        plan.canonical_before_sha256.as_deref(),
        &plan.databases,
        Some(&persisted_identities),
    )?;
    if plan.replacements != expected_replacements {
        return Err("conflict replacement plan changed".to_string());
    }
    let mut plan_paths = BTreeSet::new();
    for path in [
        Some(&plan.session.source_path),
        Some(&plan.session.target_path),
        plan.session.staged_path.as_ref(),
        plan.session.target_backup_payload.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        plan_paths.insert(durable_conflict_path_key(path));
    }
    for database in &plan.databases {
        for path in [
            &database.apply.target_path,
            &database.apply.staged_path,
            &database.apply.original_backup_payload,
        ] {
            plan_paths.insert(durable_conflict_path_key(path));
        }
    }
    for cleanup in &plan.cleanup {
        for path in [Some(&cleanup.path), cleanup.marker_path.as_ref()]
            .into_iter()
            .flatten()
        {
            plan_paths.insert(durable_conflict_path_key(path));
        }
    }
    let mut reserved = BTreeSet::new();
    if let Some(creation) = &plan.created_canonical {
        for path in [&creation.staging_path, &creation.rollback_tombstone_path] {
            let path_key = durable_conflict_path_key(path);
            if plan_paths.contains(&path_key) || !reserved.insert(path_key) {
                return Err("conflict canonical creation path collides with the plan".to_string());
            }
        }
    }
    for cleanup in &plan.cleanup {
        for path in [
            Some(&cleanup.ownership_witness_path),
            Some(&cleanup.tombstone_path),
            cleanup.marker_ownership_witness_path.as_ref(),
            cleanup.marker_tombstone_path.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            let path_key = durable_conflict_path_key(path);
            if plan_paths.contains(&path_key) || !reserved.insert(path_key) {
                return Err("conflict cleanup artifact path collides with the plan".to_string());
            }
        }
    }
    for replacement in &plan.replacements {
        validate_sha256(&replacement.original_sha256)?;
        validate_sha256(&replacement.replacement_sha256)?;
        let expected_original_identity = match replacement.kind {
            ConflictReplacementKind::CanonicalSession => plan
                .canonical_before_identity
                .ok_or_else(|| "conflict canonical original identity is missing".to_string())?,
            ConflictReplacementKind::RuntimeDatabase => plan
                .databases
                .iter()
                .find(|database| {
                    durable_conflict_path_key(&database.apply.target_path)
                        == durable_conflict_path_key(&replacement.target_path)
                })
                .map(|database| database.live_before_identity)
                .ok_or_else(|| "conflict database original identity is missing".to_string())?,
        };
        if replacement.original_identity != expected_original_identity {
            return Err("conflict replacement original identity binding changed".to_string());
        }
        let typed = typed_replacement_paths(replacement)?;
        if typed.target_path() != replacement.target_path
            || typed.recovery_path() != replacement.recovery_path
            || typed.staging_path() != replacement.staging_path
            || typed.rollback_tombstone_path() != replacement.tombstone_path
        {
            return Err("conflict replacement typed path binding changed".to_string());
        }
        for path in [
            &replacement.original_witness_path,
            &replacement.replacement_witness_path,
            &replacement.staging_path,
            &replacement.recovery_path,
            &replacement.tombstone_path,
        ] {
            if !path.is_absolute()
                || path.parent() != replacement.target_path.parent()
                || plan_paths.contains(&durable_conflict_path_key(path))
                || !reserved.insert(durable_conflict_path_key(path))
            {
                return Err("conflict replacement artifact path is invalid".to_string());
            }
        }
    }
    Ok(())
}

fn write_plan(path: &Path, plan: &ConflictResolutionPlan) -> Result<(), String> {
    validate_plan(plan)?;
    let envelope = ConflictResolutionPlanEnvelope {
        plan: plan.clone(),
        integrity_sha256: plan_digest(plan)?,
    };
    let bytes = serde_json::to_vec_pretty(&envelope)
        .map_err(|_| "failed to serialize conflict resolution plan".to_string())?;
    if bytes.len() as u64 > MAX_CONFLICT_RESOLUTION_PLAN_BYTES {
        return Err("conflict resolution plan reached its size limit".to_string());
    }
    atomic_write(path, &bytes)?;
    let verified = load_plan_from_path(path)?;
    if &verified == plan {
        Ok(())
    } else {
        Err("conflict resolution plan verification failed".to_string())
    }
}

fn load_plan_from_path(path: &Path) -> Result<ConflictResolutionPlan, String> {
    let bytes = read_regular_file_bounded(path, MAX_CONFLICT_RESOLUTION_PLAN_BYTES)
        .map_err(|_| "conflict resolution plan is unreadable".to_string())?;
    let envelope = serde_json::from_slice::<ConflictResolutionPlanEnvelope>(&bytes)
        .map_err(|_| "conflict resolution plan is invalid".to_string())?;
    validate_plan(&envelope.plan)?;
    if envelope.integrity_sha256 != plan_digest(&envelope.plan)? {
        return Err("conflict resolution plan integrity check failed".to_string());
    }
    Ok(envelope.plan)
}

fn plan_digest(plan: &ConflictResolutionPlan) -> Result<String, String> {
    let bytes = serde_json::to_vec(plan)
        .map_err(|_| "failed to serialize conflict resolution plan".to_string())?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn conflict_replacement_phase_path(plan: &ConflictResolutionPlan) -> Result<PathBuf, String> {
    Ok(
        operation_root(&plan.data_root, &plan.operation_id)?
            .join("conflict-replacement-phases.bin"),
    )
}

fn conflict_replacement_phase_digest(
    record: &ConflictReplacementPhaseRecord,
) -> Result<String, String> {
    let bytes = serde_json::to_vec(record)
        .map_err(|_| "failed to serialize conflict replacement phase".to_string())?;
    Ok(hex_digest(Sha256::digest(bytes)))
}

fn persist_initial_conflict_replacement_phases(
    plan: &ConflictResolutionPlan,
) -> Result<(), String> {
    let path = conflict_replacement_phase_path(plan)?;
    if path.exists() {
        let record = load_conflict_replacement_phases(plan)?;
        if record.terminal_cleanup_completed
            || record
                .created_canonical
                .as_ref()
                .is_some_and(|entry| entry.phase != ConflictCreatedCanonicalPhase::Absent)
            || record.replacements.iter().any(|entry| {
                entry.phase != ConflictReplacementPhase::Planned
                    || entry.replacement_identity.is_some()
                    || entry.parent_identity != Some(entry.replacement.parent_identity)
            })
        {
            return Err("conflict replacement phase record already advanced".to_string());
        }
        return Ok(());
    }
    let record = ConflictReplacementPhaseRecord {
        schema_version: CONFLICT_RESOLUTION_SCHEMA_VERSION,
        operation_id: plan.operation_id.clone(),
        plan_integrity_sha256: plan_digest(plan)?,
        updated_at_ms: timestamp_millis()?,
        terminal_cleanup_completed: false,
        created_canonical: plan.created_canonical.clone().map(|creation| {
            let identity_bindings = created_canonical_identity_bindings(&creation);
            ConflictCreatedCanonicalPhaseEntry {
                creation,
                phase: ConflictCreatedCanonicalPhase::Absent,
                identity_bindings,
            }
        }),
        replacements: plan
            .replacements
            .iter()
            .cloned()
            .map(|replacement| {
                let parent_identity = replacement.parent_identity;
                ConflictReplacementPhaseEntry {
                    replacement,
                    phase: ConflictReplacementPhase::Planned,
                    replacement_identity: None,
                    parent_identity: Some(parent_identity),
                }
            })
            .collect(),
    };
    write_conflict_replacement_phases(plan, &record)
}

fn load_conflict_replacement_phases(
    plan: &ConflictResolutionPlan,
) -> Result<ConflictReplacementPhaseRecord, String> {
    let protected = read_regular_file_bounded(
        &conflict_replacement_phase_path(plan)?,
        MAX_CONFLICT_REPLACEMENT_PHASE_BYTES,
    )
    .map_err(|_| "conflict replacement phase record is unreadable".to_string())?;
    let plaintext = if let Some(ciphertext) =
        protected.strip_prefix(CONFLICT_REPLACEMENT_PHASE_CIPHERTEXT_MAGIC)
    {
        crate::crypto::unprotect(ciphertext)
            .map_err(|_| "conflict replacement phase record is unreadable".to_string())?
    } else {
        #[cfg(windows)]
        {
            return Err("conflict replacement phase record is not protected".to_string());
        }
        #[cfg(not(windows))]
        {
            protected
        }
    };
    if plaintext.len() as u64 > MAX_CONFLICT_REPLACEMENT_PHASE_BYTES {
        return Err("conflict replacement phase record reached its size limit".to_string());
    }
    let envelope = serde_json::from_slice::<ConflictReplacementPhaseEnvelope>(&plaintext)
        .map_err(|_| "conflict replacement phase record is invalid".to_string())?;
    if conflict_replacement_phase_digest(&envelope.record)? != envelope.integrity_sha256
        || envelope.record.schema_version != CONFLICT_RESOLUTION_SCHEMA_VERSION
        || envelope.record.operation_id != plan.operation_id
        || envelope.record.plan_integrity_sha256 != plan_digest(plan)?
        || envelope.record.replacements.len() != plan.replacements.len()
        || envelope
            .record
            .replacements
            .iter()
            .zip(&plan.replacements)
            .any(|(entry, replacement)| {
                entry.replacement != *replacement
                    || entry.parent_identity != Some(replacement.parent_identity)
                    || match entry.phase {
                        ConflictReplacementPhase::Planned
                        | ConflictReplacementPhase::WitnessCreating => {
                            entry.replacement_identity.is_some()
                        }
                        ConflictReplacementPhase::Cleaned => false,
                        _ => entry.replacement_identity.is_none(),
                    }
            })
        || match (&plan.created_canonical, &envelope.record.created_canonical) {
            (None, None) => false,
            (Some(creation), Some(entry)) => {
                entry.creation != *creation
                    || entry.identity_bindings != created_canonical_identity_bindings(creation)
            }
            _ => true,
        }
    {
        return Err("conflict replacement phase record integrity check failed".to_string());
    }
    Ok(envelope.record)
}

fn write_conflict_replacement_phases(
    plan: &ConflictResolutionPlan,
    record: &ConflictReplacementPhaseRecord,
) -> Result<(), String> {
    let envelope = ConflictReplacementPhaseEnvelope {
        integrity_sha256: conflict_replacement_phase_digest(record)?,
        record: record.clone(),
    };
    let plaintext = serde_json::to_vec_pretty(&envelope)
        .map_err(|_| "failed to serialize conflict replacement phase".to_string())?;
    if plaintext.len() as u64 > MAX_CONFLICT_REPLACEMENT_PHASE_BYTES {
        return Err("conflict replacement phase record reached its size limit".to_string());
    }
    #[cfg(windows)]
    let bytes = {
        let ciphertext = crate::crypto::protect(&plaintext)
            .map_err(|_| "failed to protect conflict replacement phase".to_string())?;
        let mut protected = Vec::with_capacity(
            CONFLICT_REPLACEMENT_PHASE_CIPHERTEXT_MAGIC.len() + ciphertext.len(),
        );
        protected.extend_from_slice(CONFLICT_REPLACEMENT_PHASE_CIPHERTEXT_MAGIC);
        protected.extend_from_slice(&ciphertext);
        protected
    };
    #[cfg(not(windows))]
    let bytes = plaintext;
    atomic_write(&conflict_replacement_phase_path(plan)?, &bytes)
}

fn conflict_replacement_phase(
    plan: &ConflictResolutionPlan,
    replacement: &ConflictReplacementPlan,
) -> Result<ConflictReplacementPhase, String> {
    load_conflict_replacement_phases(plan)?
        .replacements
        .into_iter()
        .find(|entry| entry.replacement == *replacement)
        .map(|entry| entry.phase)
        .ok_or_else(|| "conflict replacement phase entry is missing".to_string())
}

fn validate_initial_conflict_replacement_phases(
    plan: &ConflictResolutionPlan,
) -> Result<(), String> {
    let record = load_conflict_replacement_phases(plan)?;
    if record.terminal_cleanup_completed
        || record
            .created_canonical
            .as_ref()
            .is_some_and(|entry| entry.phase != ConflictCreatedCanonicalPhase::Absent)
        || record.replacements.iter().any(|entry| {
            entry.phase != ConflictReplacementPhase::Planned
                || entry.replacement_identity.is_some()
                || entry.parent_identity != Some(entry.replacement.parent_identity)
        })
    {
        return Err("conflict replacement phase record already advanced".to_string());
    }
    Ok(())
}

fn transition_conflict_replacement_phase(
    plan: &ConflictResolutionPlan,
    replacement: &ConflictReplacementPlan,
    allowed: &[ConflictReplacementPhase],
    next: ConflictReplacementPhase,
) -> Result<(), String> {
    let mut record = load_conflict_replacement_phases(plan)?;
    let entry = record
        .replacements
        .iter_mut()
        .find(|entry| entry.replacement == *replacement)
        .ok_or_else(|| "conflict replacement phase entry is missing".to_string())?;
    if entry.phase == next {
        return Ok(());
    }
    if !allowed.contains(&entry.phase) {
        return Err("conflict replacement phase transition is invalid".to_string());
    }
    entry.phase = next;
    record.updated_at_ms = timestamp_millis()?;
    write_conflict_replacement_phases(plan, &record)
}

fn record_conflict_replacement_witness_ready(
    plan: &ConflictResolutionPlan,
    replacement: &ConflictReplacementPlan,
    identity_bindings: HandleReplaceIdentityBindings,
) -> Result<(), String> {
    let mut record = load_conflict_replacement_phases(plan)?;
    let entry = record
        .replacements
        .iter_mut()
        .find(|entry| entry.replacement == *replacement)
        .ok_or_else(|| "conflict replacement phase entry is missing".to_string())?;
    if entry.phase == ConflictReplacementPhase::WitnessReady
        && entry.replacement_identity == Some(identity_bindings.replacement_identity)
        && entry.parent_identity == Some(identity_bindings.parent_identity)
    {
        return Ok(());
    }
    if entry.phase != ConflictReplacementPhase::WitnessCreating
        || entry.replacement_identity.is_some()
        || entry.parent_identity != Some(identity_bindings.parent_identity)
        || identity_bindings.parent_identity != replacement.parent_identity
        || identity_bindings.original_identity != replacement.original_identity
    {
        return Err("conflict replacement witness identity transition is invalid".to_string());
    }
    entry.replacement_identity = Some(identity_bindings.replacement_identity);
    entry.parent_identity = Some(identity_bindings.parent_identity);
    entry.phase = ConflictReplacementPhase::WitnessReady;
    record.updated_at_ms = timestamp_millis()?;
    write_conflict_replacement_phases(plan, &record)
}

fn conflict_replacement_identity(
    plan: &ConflictResolutionPlan,
    replacement: &ConflictReplacementPlan,
) -> Result<RegularFileIdentity, String> {
    load_conflict_replacement_phases(plan)?
        .replacements
        .into_iter()
        .find(|entry| entry.replacement == *replacement)
        .and_then(|entry| entry.replacement_identity)
        .ok_or_else(|| "conflict replacement identity witness is missing".to_string())
}

fn conflict_replacement_parent_identity(
    plan: &ConflictResolutionPlan,
    replacement: &ConflictReplacementPlan,
) -> Result<RegularFileIdentity, String> {
    load_conflict_replacement_phases(plan)?
        .replacements
        .into_iter()
        .find(|entry| entry.replacement == *replacement)
        .and_then(|entry| entry.parent_identity)
        .ok_or_else(|| "conflict replacement parent identity witness is missing".to_string())
}

fn transition_created_canonical_phase(
    plan: &ConflictResolutionPlan,
    allowed: &[ConflictCreatedCanonicalPhase],
    next: ConflictCreatedCanonicalPhase,
) -> Result<(), String> {
    let mut record = load_conflict_replacement_phases(plan)?;
    let entry = record
        .created_canonical
        .as_mut()
        .ok_or_else(|| "conflict-created canonical phase entry is missing".to_string())?;
    if entry.phase == next {
        return Ok(());
    }
    if !allowed.contains(&entry.phase) {
        return Err("conflict-created canonical phase transition is invalid".to_string());
    }
    entry.phase = next;
    record.updated_at_ms = timestamp_millis()?;
    write_conflict_replacement_phases(plan, &record)
}

fn created_canonical_identity(
    plan: &ConflictResolutionPlan,
) -> Result<RegularFileIdentity, String> {
    load_conflict_replacement_phases(plan)?
        .created_canonical
        .map(|entry| entry.identity_bindings.created_identity)
        .ok_or_else(|| "conflict-created canonical identity witness is missing".to_string())
}

fn created_canonical_phase(
    plan: &ConflictResolutionPlan,
) -> Result<ConflictCreatedCanonicalPhase, String> {
    load_conflict_replacement_phases(plan)?
        .created_canonical
        .map(|entry| entry.phase)
        .ok_or_else(|| "conflict-created canonical phase entry is missing".to_string())
}

fn complete_conflict_terminal_cleanup(plan: &ConflictResolutionPlan) -> Result<(), String> {
    let mut record = load_conflict_replacement_phases(plan)?;
    if record.terminal_cleanup_completed {
        return Ok(());
    }
    if record
        .replacements
        .iter()
        .any(|entry| entry.phase != ConflictReplacementPhase::Cleaned)
        || record
            .created_canonical
            .as_ref()
            .is_some_and(|entry| entry.phase != ConflictCreatedCanonicalPhase::Cleaned)
    {
        return Err("conflict terminal cleanup has an unfinished replacement".to_string());
    }
    record.terminal_cleanup_completed = true;
    record.updated_at_ms = timestamp_millis()?;
    write_conflict_replacement_phases(plan, &record)
}

fn backup_entry_for_source<'a>(
    backup: &'a MigrationBackupManifest,
    source: &Path,
    kind: MigrationBackupEntryKind,
) -> Result<&'a MigrationBackupEntry, String> {
    let key = path_key(source);
    let matches = backup
        .entries
        .iter()
        .filter(|entry| entry.kind == kind && path_key(&entry.source_path) == key)
        .collect::<Vec<_>>();
    if matches.len() == 1 {
        Ok(matches[0])
    } else {
        Err("conflict recovery package source binding is invalid".to_string())
    }
}

fn backup_payload_path(
    backup: &MigrationBackupManifest,
    entry: &MigrationBackupEntry,
) -> Result<PathBuf, String> {
    let path = backup
        .backup_dir
        .join("payload")
        .join(&entry.payload_relative_path);
    if path.starts_with(backup.backup_dir.join("payload")) {
        Ok(path)
    } else {
        Err("conflict recovery package payload path is unsafe".to_string())
    }
}

fn validate_session_file(path: &Path, thread_id: &str, sha256: &str) -> Result<(), String> {
    let semantic =
        read_semantic_session(path).map_err(|_| "conflict session file is invalid".to_string())?;
    let (bytes, actual_sha256) = stable_file_digest(path)?;
    if semantic.thread_id != thread_id || semantic.bytes != bytes || actual_sha256 != sha256 {
        return Err("conflict session file identity changed".to_string());
    }
    Ok(())
}

fn ensure_absent(path: &Path, label: &str) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(format!("{label} still exists")),
        Err(_) => Err(format!("{label} state is unavailable")),
    }
}

fn operation_root(data_root: &Path, operation_id: &str) -> Result<PathBuf, String> {
    validate_operation_id(operation_id)?;
    if !data_root.is_absolute() {
        return Err("conflict managed data root is invalid".to_string());
    }
    let root = data_root
        .join("session-storage-v1/operations")
        .join(operation_id);
    validate_safe_directory(&root, "operation root")?;
    Ok(root)
}

fn create_safe_directory(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path)
        .map_err(|_| "failed to create conflict storage directory".to_string())?;
    validate_safe_directory(path, "conflict storage directory")
}

fn validate_safe_directory(path: &Path, label: &str) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(format!("{label} path is invalid"));
    }
    let metadata = fs::symlink_metadata(path).map_err(|_| format!("{label} is unavailable"))?;
    if !metadata.is_dir() || metadata_is_link_or_reparse(&metadata) {
        return Err(format!("{label} is unsafe"));
    }
    Ok(())
}

fn remove_owned_tree(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err("conflict staging metadata is unavailable".to_string()),
        Ok(metadata) if !metadata.is_dir() || metadata_is_link_or_reparse(&metadata) => {
            return Err("conflict staging root is unsafe".to_string())
        }
        Ok(_) => {}
    }
    for entry in walkdir::WalkDir::new(path).follow_links(false) {
        let entry = entry.map_err(|_| "conflict staging tree is unreadable".to_string())?;
        if entry.path() != path && entry.file_type().is_symlink() {
            return Err("conflict staging tree contains a link".to_string());
        }
    }
    fs::remove_dir_all(path).map_err(|_| "failed to remove conflict staging tree".to_string())
}

fn validate_operation_id(value: &str) -> Result<(), String> {
    if !value.is_empty()
        && value.len() <= 160
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        Ok(())
    } else {
        Err("conflict operation ID is invalid".to_string())
    }
}

fn validate_conflict_id(value: &str) -> Result<(), String> {
    if value.len() == "conflict-".len() + 64
        && value.starts_with("conflict-")
        && value["conflict-".len()..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err("session conflict ID is invalid".to_string())
    }
}

fn validate_sha256(value: &str) -> Result<(), String> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err("conflict checksum is invalid".to_string())
    }
}

fn hex_digest(digest: impl AsRef<[u8]>) -> String {
    digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Produces a stable key for paths persisted into conflict plans, phase
/// records, and deterministic operation-owned artifact names. Unlike
/// `reference_graph::path_key`, this deliberately does not canonicalize the
/// current filesystem object: the same planned target must keep the same key
/// while it is present, quarantined, absent, or replaced by another identity.
fn durable_conflict_path_key(path: &Path) -> String {
    let normalized = path.to_string_lossy().replace('/', "\\");
    if cfg!(windows) {
        normalized.to_ascii_lowercase()
    } else {
        normalized
    }
}

fn mark_recovery_retry(
    store: &OperationLedgerStore,
    operation_id: &str,
    error_code: &str,
) -> Result<(), String> {
    store.update(operation_id, |ledger| {
        ledger.last_error_code = Some(error_code.to_string());
        Ok(())
    })?;
    Ok(())
}

#[cfg(windows)]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    #[cfg(windows)]
    use std::{fs::OpenOptions, io::Write, os::windows::fs::OpenOptionsExt};

    use rusqlite::Connection;
    use sha2::{Digest, Sha256};
    use tempfile::tempdir;

    use super::{
        cleanup_committed_conflict_resolution_artifacts, cleanup_conflict_resolution_staging,
        conflict_replacement_phase_path, execute_conflict_resolution, prepare_conflict_resolution,
        recover_interrupted_conflict_resolution, transition_conflict_replacement_phase,
        typed_replacement_paths, validate_conflict_resolution, ConflictReplacementKind,
        ConflictReplacementPhase, ConflictReplacementPlan, ConflictResolutionFailure,
        ConflictResolutionRecoveryStatus,
    };
    #[cfg(windows)]
    use crate::session_storage::write_barrier::{DestructiveFileGuard, WriteExclusionGuard};
    use crate::session_storage::{
        conflict::{list_migration_conflicts, load_resolved_conflict_ids},
        marker::provider_marker_path,
        migration::{CanonicalMigrationPlan, MigrationConflictPlan, MigrationPreflightReport},
        migration_backup::{verify_migration_backup, MigrationBackupStatus},
        model::{FileOrigin, MarkerStatus, SessionRelation},
        operation_ledger::{
            OperationLedgerStore, SessionStorageOperationKind, SessionStorageOperationPhase,
        },
        retention::run_session_storage_retention,
        semantic::read_semantic_session,
    };

    fn canonical_replacement(
        prepared: &super::PreparedConflictResolution,
    ) -> ConflictReplacementPlan {
        prepared
            .plan
            .replacements
            .iter()
            .find(|replacement| replacement.kind == ConflictReplacementKind::CanonicalSession)
            .unwrap()
            .clone()
    }

    fn create_replacement_witness(
        plan: &super::ConflictResolutionPlan,
        replacement: &ConflictReplacementPlan,
    ) {
        transition_conflict_replacement_phase(
            plan,
            replacement,
            &[ConflictReplacementPhase::Planned],
            ConflictReplacementPhase::WitnessCreating,
        )
        .unwrap();
        assert!(
            crate::file_ops::atomic_create(&replacement.replacement_witness_path, |target| {
                let mut source =
                    fs::File::open(&replacement.source_path).map_err(|error| error.to_string())?;
                std::io::copy(&mut source, target)
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            })
            .unwrap()
        );
        super::record_conflict_replacement_witness_ready(
            plan,
            replacement,
            crate::session_storage::write_barrier::HandleReplaceIdentityBindings {
                parent_identity: replacement.parent_identity,
                original_identity: replacement.original_identity,
                replacement_identity:
                    crate::session_storage::write_barrier::regular_file_identity_at_path(
                        &replacement.replacement_witness_path,
                    )
                    .unwrap(),
            },
        )
        .unwrap();
    }

    fn write_session(path: &Path, timestamp: &str, messages: &[&str]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut lines = vec![serde_json::json!({
            "type": "session_meta",
            "timestamp": "2026-08-12T00:00:00Z",
            "payload": {"id": "thread-a", "model_provider": "openai"}
        })
        .to_string()];
        lines.extend(messages.iter().map(|message| {
            serde_json::json!({
                "type": "event_msg",
                "timestamp": timestamp,
                "payload": {"type": "user_message", "message": message}
            })
            .to_string()
        }));
        fs::write(path, lines.join("\n") + "\n").unwrap();
    }

    fn write_provider_marker(path: &Path) {
        let bytes = fs::read(path).unwrap();
        let marker = serde_json::json!({
            "version": 1,
            "threadId": "thread-a",
            "providerId": "openai",
            "slotFileName": path.file_name().unwrap().to_string_lossy(),
            "originRelativePath": null,
            "originProvider": null,
            "createdBytes": bytes.len(),
            "createdSha256": format!("{:x}", Sha256::digest(&bytes)),
        });
        fs::write(
            provider_marker_path(path).unwrap(),
            serde_json::to_vec(&marker).unwrap(),
        )
        .unwrap();
    }

    fn fixture() -> (
        tempfile::TempDir,
        std::path::PathBuf,
        std::path::PathBuf,
        MigrationPreflightReport,
        String,
    ) {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let data = root.path().join("data");
        let current = home.join("sessions/2026/08/current.jsonl");
        let candidate = data.join("shared-sessions/sessions/candidate.jsonl");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&data).unwrap();
        fs::write(
            home.join("config.toml"),
            format!("sqlite_home = {:?}\n", home.to_string_lossy()),
        )
        .unwrap();
        write_session(&current, "2026-08-12T00:00:01Z", &["common", "left"]);
        write_session(
            &candidate,
            "2026-08-12T00:00:02Z",
            &["common", "right", "new"],
        );
        write_provider_marker(&candidate);
        let database = Connection::open(home.join("state_5.sqlite")).unwrap();
        database
            .execute_batch(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, model_provider TEXT);",
            )
            .unwrap();
        database
            .execute(
                "INSERT INTO threads VALUES (?1, ?2, 'openai')",
                ("thread-a", current.to_string_lossy().to_string()),
            )
            .unwrap();
        drop(database);
        let shared_database =
            Connection::open(data.join("shared-sessions/state_5.sqlite")).unwrap();
        shared_database
            .execute_batch(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, model_provider TEXT);",
            )
            .unwrap();
        shared_database
            .execute(
                "INSERT INTO threads VALUES (?1, ?2, 'openai')",
                ("thread-a", candidate.to_string_lossy().to_string()),
            )
            .unwrap();
        drop(shared_database);
        let current_semantic = read_semantic_session(&current).unwrap();
        let candidate_semantic = read_semantic_session(&candidate).unwrap();
        let report = MigrationPreflightReport {
            schema_version: 1,
            operation_id: "migration-1".to_string(),
            generated_at_ms: 1,
            canonical_session_count: 1,
            session_file_count: 2,
            provider_copy_count: 1,
            conflict_count: 1,
            anomaly_count: 0,
            estimated_reclaim_bytes: 0,
            backup_source_bytes: 0,
            required_backup_bytes: 0,
            available_backup_bytes: u64::MAX,
            backup_destination: root.path().join("backups"),
            blockers: Vec::new(),
            ready_for_backup: true,
            plan: CanonicalMigrationPlan {
                schema_version: 1,
                operation_id: "migration-1".to_string(),
                generated_at_ms: 1,
                canonical_root: home.clone(),
                inventory_fingerprint: "a".repeat(64),
                sessions: Vec::new(),
                conflicts: vec![MigrationConflictPlan {
                    thread_id: "thread-a".to_string(),
                    current_path: current.clone(),
                    candidate_path: candidate.clone(),
                    canonical_path: current,
                    current_sha256: Some(super::hex_digest(current_semantic.raw_sha256)),
                    candidate_sha256: super::hex_digest(candidate_semantic.raw_sha256),
                    current_origin: FileOrigin::CanonicalHome,
                    candidate_origin: FileOrigin::Shared,
                    current_marker_status: MarkerStatus::Absent,
                    candidate_marker_status: MarkerStatus::Valid,
                    current_message_count: current_semantic.message_count,
                    candidate_message_count: candidate_semantic.message_count,
                    current_last_message_at: current_semantic.last_message_timestamp,
                    candidate_last_message_at: candidate_semantic.last_message_timestamp,
                    current_provider: current_semantic.initial_provider,
                    candidate_provider: candidate_semantic.initial_provider,
                    relation: SessionRelation::Divergent,
                    default_overwrite: false,
                }],
                databases: Vec::new(),
                unclassified_file_count: 0,
                invalid_marker_count: 0,
                missing_runtime_reference_count: 0,
                mismatched_runtime_reference_count: 0,
            },
        };
        let conflict_id = list_migration_conflicts(&report).unwrap().conflicts[0]
            .conflict_id
            .clone();
        (root, home, data, report, conflict_id)
    }

    fn prepare(
        home: &Path,
        data: &Path,
        report: &MigrationPreflightReport,
        conflict_id: &str,
        operation_id: &str,
    ) -> (OperationLedgerStore, super::PreparedConflictResolution) {
        let store = OperationLedgerStore::new(data);
        store
            .create(
                operation_id,
                SessionStorageOperationKind::ConflictResolution,
                home,
            )
            .unwrap();
        store
            .transition(operation_id, SessionStorageOperationPhase::Preflight)
            .unwrap();
        store
            .transition(operation_id, SessionStorageOperationPhase::Backup)
            .unwrap();
        let prepared =
            prepare_conflict_resolution(home, data, operation_id, report, conflict_id).unwrap();
        store
            .update(operation_id, |ledger| {
                ledger.backup_root = Some(prepared.plan.backup_dir.clone());
                ledger.created_files = prepared.created_files.clone();
                ledger.database_snapshots = prepared.database_snapshots.clone();
                ledger.rollback_steps = prepared.rollback_steps.clone();
                Ok(())
            })
            .unwrap();
        for phase in [
            SessionStorageOperationPhase::BackupVerified,
            SessionStorageOperationPhase::PlanReady,
            SessionStorageOperationPhase::Applying,
        ] {
            store.transition(operation_id, phase).unwrap();
        }
        (store, prepared)
    }

    #[test]
    fn explicit_newer_resolution_updates_every_view_and_keeps_a_verified_recovery_package() {
        let (_root, home, data, report, conflict_id) = fixture();
        let (store, prepared) =
            prepare(&home, &data, &report, &conflict_id, "conflict-resolution-1");

        let receipt = execute_conflict_resolution(&prepared.plan, || Ok(())).unwrap();
        store
            .transition(
                "conflict-resolution-1",
                SessionStorageOperationPhase::Validating,
            )
            .unwrap();
        let receipt = validate_conflict_resolution(&prepared.plan, receipt).unwrap();
        assert!(receipt.validated);
        assert_eq!(
            read_semantic_session(&prepared.plan.session.target_path)
                .unwrap()
                .message_count,
            3
        );
        assert!(!report.plan.conflicts[0].candidate_path.exists());
        let database = Connection::open(home.join("state_5.sqlite")).unwrap();
        let rollout_path: String = database
            .query_row(
                "SELECT rollout_path FROM threads WHERE id = 'thread-a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            super::path_key(Path::new(&rollout_path)),
            super::path_key(&prepared.plan.session.target_path)
        );
        let backup = verify_migration_backup(&prepared.plan.backup_dir).unwrap();
        assert_eq!(
            backup.status,
            MigrationBackupStatus::IsolatedRestoreVerified
        );
        assert_eq!(
            backup
                .entries
                .iter()
                .filter(|entry| {
                    entry.kind
                        == crate::session_storage::migration_backup::MigrationBackupEntryKind::Session
                })
                .count(),
            2
        );
        cleanup_conflict_resolution_staging(&prepared.plan).unwrap();
        store
            .transition(
                "conflict-resolution-1",
                SessionStorageOperationPhase::Committed,
            )
            .unwrap();
    }

    #[test]
    fn recovery_package_candidate_is_never_deleted_as_a_runtime_duplicate() {
        let (_root, home, data, mut report, conflict_id) = fixture();
        report.plan.conflicts[0].candidate_origin = FileOrigin::RecoveryPackage;
        let candidate_path = report.plan.conflicts[0].candidate_path.clone();
        let (_store, prepared) = prepare(
            &home,
            &data,
            &report,
            &conflict_id,
            "conflict-resolution-recovery-source",
        );

        let receipt = execute_conflict_resolution(&prepared.plan, || Ok(())).unwrap();
        let receipt = validate_conflict_resolution(&prepared.plan, receipt).unwrap();

        assert!(receipt.validated);
        assert!(candidate_path.exists());
        assert!(prepared.plan.cleanup.is_empty());
        assert_eq!(
            read_semantic_session(&prepared.plan.session.target_path)
                .unwrap()
                .message_count,
            3
        );
    }

    #[test]
    fn retention_persists_resolution_identity_before_expiring_its_audit_ledger() {
        let (_root, home, data, report, conflict_id) = fixture();
        let store = OperationLedgerStore::new(&data);
        store
            .create("migration-1", SessionStorageOperationKind::Migration, &home)
            .unwrap();
        for phase in [
            SessionStorageOperationPhase::Preflight,
            SessionStorageOperationPhase::Backup,
            SessionStorageOperationPhase::BackupVerified,
            SessionStorageOperationPhase::PlanReady,
            SessionStorageOperationPhase::Applying,
            SessionStorageOperationPhase::Validating,
            SessionStorageOperationPhase::Committed,
        ] {
            store.transition("migration-1", phase).unwrap();
        }
        let (store, prepared) = prepare(
            &home,
            &data,
            &report,
            &conflict_id,
            "conflict-resolution-retention",
        );
        let receipt = execute_conflict_resolution(&prepared.plan, || Ok(())).unwrap();
        store
            .transition(
                "conflict-resolution-retention",
                SessionStorageOperationPhase::Validating,
            )
            .unwrap();
        validate_conflict_resolution(&prepared.plan, receipt).unwrap();
        cleanup_conflict_resolution_staging(&prepared.plan).unwrap();
        store
            .transition(
                "conflict-resolution-retention",
                SessionStorageOperationPhase::Committed,
            )
            .unwrap();

        run_session_storage_retention(&data, Some("migration-1"), u128::MAX).unwrap();

        assert!(load_resolved_conflict_ids(&data, &home, "migration-1")
            .unwrap()
            .contains(&conflict_id));
        assert!(!prepared.plan.backup_dir.exists());
    }

    #[test]
    fn a_guard_failure_after_canonical_write_recovers_every_original() {
        let (_root, home, data, report, conflict_id) = fixture();
        let current = report.plan.conflicts[0].current_path.clone();
        let candidate = report.plan.conflicts[0].candidate_path.clone();
        let current_before = fs::read(&current).unwrap();
        let candidate_before = fs::read(&candidate).unwrap();
        let (store, prepared) =
            prepare(&home, &data, &report, &conflict_id, "conflict-resolution-2");
        let mut writes = 0_usize;
        let failure = execute_conflict_resolution(&prepared.plan, || {
            writes = writes.saturating_add(1);
            if writes == 2 {
                Err("writer appeared".to_string())
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        assert!(matches!(
            failure,
            ConflictResolutionFailure::LiveWriteGuard(_)
        ));

        let recovery =
            recover_interrupted_conflict_resolution(&store, &data, "conflict-resolution-2", || {
                Ok(())
            })
            .unwrap();
        assert_eq!(recovery, ConflictResolutionRecoveryStatus::RolledBack);
        assert_eq!(fs::read(&current).unwrap(), current_before);
        assert_eq!(fs::read(&candidate).unwrap(), candidate_before);
        let database = Connection::open(home.join("state_5.sqlite")).unwrap();
        let rollout_path: String = database
            .query_row(
                "SELECT rollout_path FROM threads WHERE id = 'thread-a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            super::path_key(Path::new(&rollout_path)),
            super::path_key(&current)
        );
    }

    #[test]
    fn startup_recovery_closes_a_precommit_operation_without_touching_sessions() {
        let (_root, home, data, report, _conflict_id) = fixture();
        let current = report.plan.conflicts[0].current_path.clone();
        let candidate = report.plan.conflicts[0].candidate_path.clone();
        let current_before = fs::read(&current).unwrap();
        let candidate_before = fs::read(&candidate).unwrap();
        let store = OperationLedgerStore::new(&data);
        store
            .create(
                "conflict-resolution-precommit",
                SessionStorageOperationKind::ConflictResolution,
                &home,
            )
            .unwrap();
        store
            .transition(
                "conflict-resolution-precommit",
                SessionStorageOperationPhase::Preflight,
            )
            .unwrap();
        store
            .transition(
                "conflict-resolution-precommit",
                SessionStorageOperationPhase::Backup,
            )
            .unwrap();

        let recovery = recover_interrupted_conflict_resolution(
            &store,
            &data,
            "conflict-resolution-precommit",
            || Ok(()),
        )
        .unwrap();

        assert_eq!(recovery, ConflictResolutionRecoveryStatus::RolledBack);
        assert_eq!(fs::read(current).unwrap(), current_before);
        assert_eq!(fs::read(candidate).unwrap(), candidate_before);
        assert_eq!(
            store.load("conflict-resolution-precommit").unwrap().phase,
            SessionStorageOperationPhase::RolledBack
        );
    }

    #[cfg(windows)]
    #[test]
    fn delete_access_contender_is_not_overwritten_by_conflict_publish() {
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };

        let (_root, home, data, report, conflict_id) = fixture();
        let (_store, prepared) = prepare(
            &home,
            &data,
            &report,
            &conflict_id,
            "conflict-delete-access-contender",
        );
        let replacement = canonical_replacement(&prepared);
        create_replacement_witness(&prepared.plan, &replacement);
        transition_conflict_replacement_phase(
            &prepared.plan,
            &replacement,
            &[ConflictReplacementPhase::WitnessReady],
            ConflictReplacementPhase::Preparing,
        )
        .unwrap();
        let staged_replace = WriteExclusionGuard::acquire(&replacement.target_path)
            .unwrap()
            .stage_handle_hardlink_replace(
                &replacement.replacement_witness_path,
                &replacement.replacement_sha256,
                &typed_replacement_paths(&replacement).unwrap(),
            )
            .unwrap();
        let prepared_replace = staged_replace.prepare().map_err(|value| value.0).unwrap();
        let contender_bytes = b"conflict delete-access contender\n";
        let mut contender = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(&replacement.target_path)
            .unwrap();
        contender.write_all(contender_bytes).unwrap();
        contender.sync_all().unwrap();

        let (_error, prepared_replace) = prepared_replace.publish().unwrap_err();
        assert_eq!(fs::read(&replacement.target_path).unwrap(), contender_bytes);
        drop(prepared_replace);
        drop(contender);
    }

    #[cfg(windows)]
    #[test]
    fn startup_rollback_recovers_conflict_prepare_crash_window() {
        let (_root, home, data, report, conflict_id) = fixture();
        let canonical = report.plan.conflicts[0].current_path.clone();
        let before = fs::read(&canonical).unwrap();
        let (store, prepared) = prepare(
            &home,
            &data,
            &report,
            &conflict_id,
            "conflict-prepared-crash-window",
        );
        let replacement = canonical_replacement(&prepared);
        create_replacement_witness(&prepared.plan, &replacement);
        transition_conflict_replacement_phase(
            &prepared.plan,
            &replacement,
            &[ConflictReplacementPhase::WitnessReady],
            ConflictReplacementPhase::Preparing,
        )
        .unwrap();
        let staged_replace = WriteExclusionGuard::acquire(&replacement.target_path)
            .unwrap()
            .stage_handle_hardlink_replace(
                &replacement.replacement_witness_path,
                &replacement.replacement_sha256,
                &typed_replacement_paths(&replacement).unwrap(),
            )
            .unwrap();
        let prepared_replace = staged_replace.prepare().map_err(|value| value.0).unwrap();
        drop(prepared_replace);
        assert!(!replacement.target_path.exists());
        assert!(replacement.recovery_path.is_file());
        assert!(replacement.staging_path.is_file());

        let recovery = recover_interrupted_conflict_resolution(
            &store,
            &data,
            "conflict-prepared-crash-window",
            || Ok(()),
        )
        .unwrap();

        assert_eq!(recovery, ConflictResolutionRecoveryStatus::RolledBack);
        assert_eq!(fs::read(canonical).unwrap(), before);
        for path in [
            &replacement.original_witness_path,
            &replacement.replacement_witness_path,
            &replacement.staging_path,
            &replacement.recovery_path,
            &replacement.tombstone_path,
        ] {
            assert!(!path.exists());
        }
    }

    #[cfg(windows)]
    #[test]
    fn startup_rollback_recovers_conflict_rollback_prepared_crash_window() {
        let (_root, home, data, report, conflict_id) = fixture();
        let canonical = report.plan.conflicts[0].current_path.clone();
        let before = fs::read(&canonical).unwrap();
        let (store, prepared) = prepare(
            &home,
            &data,
            &report,
            &conflict_id,
            "conflict-rollback-prepared-window",
        );
        execute_conflict_resolution(&prepared.plan, || Ok(())).unwrap();
        let replacement = canonical_replacement(&prepared);
        transition_conflict_replacement_phase(
            &prepared.plan,
            &replacement,
            &[ConflictReplacementPhase::CommittedWithRecovery],
            ConflictReplacementPhase::RollbackPreparing,
        )
        .unwrap();
        let mut target = DestructiveFileGuard::acquire(&replacement.target_path).unwrap();
        target
            .verify_current_path(Some(&replacement.replacement_sha256))
            .unwrap();
        target
            .rename_no_replace(&replacement.tombstone_path)
            .unwrap();
        drop(target);
        assert!(!replacement.target_path.exists());
        assert!(replacement.recovery_path.is_file());
        assert!(replacement.tombstone_path.is_file());

        let recovery = recover_interrupted_conflict_resolution(
            &store,
            &data,
            "conflict-rollback-prepared-window",
            || Ok(()),
        )
        .unwrap();

        assert_eq!(recovery, ConflictResolutionRecoveryStatus::RolledBack);
        assert_eq!(fs::read(canonical).unwrap(), before);
        for path in [
            &replacement.original_witness_path,
            &replacement.replacement_witness_path,
            &replacement.staging_path,
            &replacement.recovery_path,
            &replacement.tombstone_path,
        ] {
            assert!(!path.exists());
        }
    }

    #[cfg(windows)]
    #[test]
    fn persisted_conflict_path_bindings_are_stable_across_target_states() {
        let root = tempdir().unwrap();
        let parent = root.path().join("MixedCase");
        fs::create_dir_all(&parent).unwrap();
        let target = parent.join("Thread-A.JSONL");
        let replacement_source = root.path().join("replacement.jsonl");
        let alternate = parent.join("alternate.jsonl");
        fs::write(&target, b"original").unwrap();
        fs::write(&replacement_source, b"replacement").unwrap();
        fs::write(&alternate, b"different identity").unwrap();
        let parent_identity =
            crate::session_storage::write_barrier::parent_directory_identity_at_path(&target)
                .unwrap();
        let original_identity =
            crate::session_storage::write_barrier::regular_file_identity_at_path(&target).unwrap();
        let alternate_identity =
            crate::session_storage::write_barrier::regular_file_identity_at_path(&alternate)
                .unwrap();
        assert_ne!(original_identity, alternate_identity);

        let operation_id = "conflict-path-binding-stability";
        let original_sha256 = format!("{:x}", Sha256::digest(b"original"));
        let replacement_sha256 = format!("{:x}", Sha256::digest(b"replacement"));
        let expected_cleanup =
            super::cleanup_artifact_paths(operation_id, &target, "session").unwrap();
        let expected_creation =
            super::created_canonical_artifact_paths(operation_id, &target).unwrap();
        let expected_replacement = super::build_conflict_replacement(
            operation_id,
            ConflictReplacementKind::CanonicalSession,
            &target,
            &replacement_source,
            &original_sha256,
            &replacement_sha256,
            parent_identity,
            original_identity,
        )
        .unwrap();
        let expected_key = super::durable_conflict_path_key(&target);

        fs::remove_file(&target).unwrap();
        assert_eq!(
            super::cleanup_artifact_paths(operation_id, &target, "session").unwrap(),
            expected_cleanup
        );
        assert_eq!(
            super::created_canonical_artifact_paths(operation_id, &target).unwrap(),
            expected_creation
        );
        assert_eq!(
            super::build_conflict_replacement(
                operation_id,
                ConflictReplacementKind::CanonicalSession,
                &target,
                &replacement_source,
                &original_sha256,
                &replacement_sha256,
                parent_identity,
                original_identity,
            )
            .unwrap(),
            expected_replacement
        );
        assert_eq!(super::durable_conflict_path_key(&target), expected_key);

        fs::rename(&alternate, &target).unwrap();
        assert_eq!(
            crate::session_storage::write_barrier::regular_file_identity_at_path(&target).unwrap(),
            alternate_identity
        );
        assert_eq!(
            super::cleanup_artifact_paths(operation_id, &target, "session").unwrap(),
            expected_cleanup
        );
        assert_eq!(
            super::created_canonical_artifact_paths(operation_id, &target).unwrap(),
            expected_creation
        );
        assert_eq!(
            super::build_conflict_replacement(
                operation_id,
                ConflictReplacementKind::CanonicalSession,
                &target,
                &replacement_source,
                &original_sha256,
                &replacement_sha256,
                parent_identity,
                original_identity,
            )
            .unwrap(),
            expected_replacement
        );
        assert_eq!(super::durable_conflict_path_key(&target), expected_key);
    }

    #[cfg(windows)]
    #[test]
    fn late_equal_hash_writer_is_preserved_and_rollback_stays_retryable() {
        use crate::session_storage::bounded_file::same_regular_file_identity;

        let (_root, home, data, report, conflict_id) = fixture();
        let canonical = report.plan.conflicts[0].current_path.clone();
        let before = fs::read(&canonical).unwrap();
        let (store, prepared) = prepare(
            &home,
            &data,
            &report,
            &conflict_id,
            "conflict-late-equal-hash-writer",
        );
        execute_conflict_resolution(&prepared.plan, || Ok(())).unwrap();
        let replacement = canonical_replacement(&prepared);
        let late_bytes = fs::read(&replacement.target_path).unwrap();
        fs::remove_file(&replacement.target_path).unwrap();
        fs::write(&replacement.target_path, &late_bytes).unwrap();
        assert!(!same_regular_file_identity(
            &replacement.target_path,
            &replacement.replacement_witness_path,
        )
        .unwrap());

        let first = recover_interrupted_conflict_resolution(
            &store,
            &data,
            "conflict-late-equal-hash-writer",
            || Ok(()),
        )
        .unwrap();

        assert_eq!(first, ConflictResolutionRecoveryStatus::Failed);
        assert_eq!(fs::read(&replacement.target_path).unwrap(), late_bytes);
        assert_eq!(
            store.load("conflict-late-equal-hash-writer").unwrap().phase,
            SessionStorageOperationPhase::RollingBack
        );

        fs::remove_file(&replacement.target_path).unwrap();
        fs::hard_link(
            &replacement.replacement_witness_path,
            &replacement.target_path,
        )
        .unwrap();
        let second = recover_interrupted_conflict_resolution(
            &store,
            &data,
            "conflict-late-equal-hash-writer",
            || Ok(()),
        )
        .unwrap();
        assert_eq!(second, ConflictResolutionRecoveryStatus::RolledBack);
        assert_eq!(fs::read(canonical).unwrap(), before);
    }

    #[test]
    fn tampered_conflict_phase_record_fails_before_live_mutation() {
        let (_root, home, data, report, conflict_id) = fixture();
        let canonical = report.plan.conflicts[0].current_path.clone();
        let canonical_before = fs::read(&canonical).unwrap();
        let (_store, prepared) =
            prepare(&home, &data, &report, &conflict_id, "conflict-phase-tamper");
        let phase_path = conflict_replacement_phase_path(&prepared.plan).unwrap();
        let mut bytes = fs::read(&phase_path).unwrap();
        let last = bytes.last_mut().unwrap();
        *last ^= 0x5a;
        fs::write(&phase_path, bytes).unwrap();

        let error = execute_conflict_resolution(&prepared.plan, || Ok(())).unwrap_err();

        assert!(matches!(error, ConflictResolutionFailure::Operation(_)));
        assert_eq!(fs::read(canonical).unwrap(), canonical_before);
        for replacement in &prepared.plan.replacements {
            assert!(replacement.original_witness_path.is_file());
            for path in [
                &replacement.replacement_witness_path,
                &replacement.staging_path,
                &replacement.recovery_path,
                &replacement.tombstone_path,
            ] {
                assert!(!path.exists());
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn committed_conflict_artifact_cleanup_is_exact_and_idempotent() {
        let (_root, home, data, report, conflict_id) = fixture();
        let (store, prepared) = prepare(
            &home,
            &data,
            &report,
            &conflict_id,
            "conflict-committed-artifact-cleanup",
        );
        let receipt = execute_conflict_resolution(&prepared.plan, || Ok(())).unwrap();
        store
            .transition(
                "conflict-committed-artifact-cleanup",
                SessionStorageOperationPhase::Validating,
            )
            .unwrap();
        validate_conflict_resolution(&prepared.plan, receipt).unwrap();
        cleanup_conflict_resolution_staging(&prepared.plan).unwrap();
        store
            .transition(
                "conflict-committed-artifact-cleanup",
                SessionStorageOperationPhase::Committed,
            )
            .unwrap();

        cleanup_committed_conflict_resolution_artifacts(&prepared.plan).unwrap();
        cleanup_committed_conflict_resolution_artifacts(&prepared.plan).unwrap();

        for replacement in &prepared.plan.replacements {
            for path in [
                &replacement.original_witness_path,
                &replacement.replacement_witness_path,
                &replacement.staging_path,
                &replacement.recovery_path,
                &replacement.tombstone_path,
            ] {
                assert!(!path.exists());
            }
        }
        for cleanup in &prepared.plan.cleanup {
            assert!(!cleanup.ownership_witness_path.exists());
            assert!(!cleanup.tombstone_path.exists());
            if let Some(path) = &cleanup.marker_ownership_witness_path {
                assert!(!path.exists());
            }
            if let Some(path) = &cleanup.marker_tombstone_path {
                assert!(!path.exists());
            }
        }
        validate_conflict_resolution(
            &prepared.plan,
            super::ConflictResolutionReceipt {
                operation_id: Some(prepared.plan.operation_id.clone()),
                migration_operation_id: prepared.plan.migration_operation_id.clone(),
                conflict_id: prepared.plan.conflict_id.clone(),
                status: super::ConflictResolutionStatus::Resolved,
                chosen_version: Some(prepared.plan.chosen_version),
                canonical_updated: true,
                database_view_count: prepared.plan.databases.len(),
                recovery_expires_at_ms: Some(prepared.plan.recovery_expires_at_ms),
                runtime_verification: None,
                validated: false,
            },
        )
        .unwrap();
    }
}

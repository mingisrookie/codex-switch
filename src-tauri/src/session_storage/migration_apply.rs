use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Read},
    path::{Component, Path, PathBuf},
};

use rusqlite::{types::Value, Connection, OpenFlags, Transaction, TransactionBehavior, MAIN_DB};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    file_ops::{atomic_copy, atomic_create_with_witness, atomic_write, ownership_witness_path},
    operation_log::timestamp_millis,
};

use super::{
    bounded_file::{read_regular_file_bounded, same_regular_file_identity},
    catalog::goals_database_digest,
    migration::{
        MigrationPreflightReport, MigrationSessionAction, MigrationSessionPlan,
        GOALS_DATABASE_PLAN_PREFIX,
    },
    migration_backup::{
        create_migration_backup, verify_migration_backup, verify_migration_backup_with_runtime,
        MigrationBackupEntry, MigrationBackupEntryKind, MigrationBackupManifest,
        MigrationBackupRuntimeVerifier, MigrationBackupSource, MigrationBackupStatus,
        MigrationRuntimeVerification,
    },
    model::DatabaseRole,
    operation_ledger::{
        LedgerFileSnapshot, LedgerRollbackStep, OperationLedgerStore, RollbackActionKind,
        SessionStorageOperationKind, SessionStorageOperationPhase,
    },
    reference_graph::path_key,
    semantic::read_semantic_session,
    write_barrier::{
        classify_handle_replace_crash_state, recover_handle_replace, DestructiveFileGuard,
        HandleReplaceCrashState, HandleReplaceIdentityBindings, HandleReplacePaths,
        HandleReplaceRecoveryDecision, WriteExclusionGuard,
    },
};

const APPLY_PLAN_SCHEMA_VERSION: u32 = 1;
const MAX_APPLY_PLAN_BYTES: u64 = 64 * 1024 * 1024;
const MAX_DEPENDENT_ROWS_PER_READ: usize = 1_000_000;
const MAX_REPLACEMENT_PHASE_BYTES: u64 = 1024 * 1024;
const REPLACEMENT_PHASE_CIPHERTEXT_MAGIC: &[u8] = b"CS-MIGRATION-REPLACE-PHASE-1\0";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MigrationSessionApplyEntry {
    pub thread_id: String,
    pub action: MigrationSessionAction,
    pub source_path: PathBuf,
    pub target_path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged_path: Option<PathBuf>,
    pub expected_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_before_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_backup_payload: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MigrationDatabaseApplyEntry {
    pub database_id: String,
    pub role: DatabaseRole,
    pub target_path: PathBuf,
    pub staged_path: PathBuf,
    pub original_backup_payload: PathBuf,
    pub original_sha256: String,
    pub staged_sha256: String,
    pub staged_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MigrationApplyPlan {
    pub schema_version: u32,
    pub operation_id: String,
    pub generated_at_ms: u128,
    pub canonical_root: PathBuf,
    pub inventory_fingerprint: String,
    pub backup_dir: PathBuf,
    pub staging_root: PathBuf,
    pub sessions: Vec<MigrationSessionApplyEntry>,
    pub databases: Vec<MigrationDatabaseApplyEntry>,
    pub conflict_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MigrationPreparationReceipt {
    pub operation_id: String,
    pub prepared_session_count: usize,
    pub prepared_database_count: usize,
    pub conflict_count: usize,
    pub prepared_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MigrationApplyReceipt {
    pub operation_id: String,
    pub canonical_created_count: usize,
    pub canonical_replaced_count: usize,
    pub database_view_count: usize,
    pub conflict_count: usize,
    pub validated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_verification: Option<MigrationRuntimeVerification>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MigrationRollbackReceipt {
    pub operation_id: String,
    pub completed_step_count: usize,
    pub removed_created_file_count: usize,
    pub restored_file_count: usize,
    pub restored_database_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationRollbackFailure {
    Precondition(String),
    LiveWriteGuard(String),
    Operation(String),
}

impl MigrationRollbackFailure {
    pub fn message(&self) -> &str {
        match self {
            Self::Precondition(message)
            | Self::LiveWriteGuard(message)
            | Self::Operation(message) => message,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationRecoveryStatus {
    RolledBack,
    DeferredByLiveWriter,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationRecoveryReceipt {
    pub operation_id: String,
    pub status: MigrationRecoveryStatus,
    pub rollback: Option<MigrationRollbackReceipt>,
}

#[derive(Debug, Clone)]
pub struct PreparedMigrationApply {
    pub plan: MigrationApplyPlan,
    pub receipt: MigrationPreparationReceipt,
    pub created_files: Vec<LedgerFileSnapshot>,
    pub rollback_steps: Vec<LedgerRollbackStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MigrationApplyPlanEnvelope {
    plan: MigrationApplyPlan,
    replacement_paths: BTreeMap<String, PersistedHandleReplacePaths>,
    integrity_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistedHandleReplacePaths {
    target_path: PathBuf,
    recovery_path: PathBuf,
    staging_path: PathBuf,
    rollback_tombstone_path: PathBuf,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum MigrationHandleReplacePhase {
    Planned,
    Staged,
    Preparing,
    Prepared,
    Publishing,
    Published,
    Committing,
    CommittedWithRecovery,
    CommitCleaning,
    CommittedCleaned,
    RollbackPreparing,
    RolledBackWithTombstone,
    RollbackCleaning,
    RolledBackCleaned,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MigrationHandleReplacePhaseEntry {
    target_path: PathBuf,
    paths: PersistedHandleReplacePaths,
    expected_replacement_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active_replacement_sha256: Option<String>,
    #[serde(default)]
    rollback_replacement: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expected_original_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    identities: Option<HandleReplaceIdentityBindings>,
    phase: MigrationHandleReplacePhase,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MigrationHandleReplacePhaseRecord {
    schema_version: u32,
    operation_id: String,
    plan_integrity_sha256: String,
    updated_at_ms: u128,
    replacements: Vec<MigrationHandleReplacePhaseEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MigrationHandleReplacePhaseEnvelope {
    record: MigrationHandleReplacePhaseRecord,
    integrity_sha256: String,
}

impl PersistedHandleReplacePaths {
    fn typed(&self) -> Result<HandleReplacePaths, String> {
        HandleReplacePaths::from_persisted_plan(
            self.target_path.clone(),
            self.recovery_path.clone(),
            self.staging_path.clone(),
            self.rollback_tombstone_path.clone(),
        )
    }
}

pub fn prepare_migration_apply_plan(
    codex_home: &Path,
    data_root: &Path,
    report: &MigrationPreflightReport,
    backup: &MigrationBackupManifest,
) -> Result<PreparedMigrationApply, String> {
    validate_preparation_inputs(codex_home, data_root, report, backup)?;
    let operation_root = operation_root(data_root, &report.operation_id)?;
    let plan_path = operation_root.join("migration-apply-plan.json");
    if plan_path.exists() {
        let plan = load_migration_apply_plan(data_root, &report.operation_id)?;
        validate_prepared_plan(&plan, report, backup)?;
        persist_initial_migration_replacement_phases(&plan)?;
        return prepared_result(plan, &plan_path);
    }

    let staging_root = operation_root.join("migration-staging");
    if staging_root.exists() {
        return Err("an incomplete migration staging area must be recovered first".to_string());
    }
    create_safe_directory(&staging_root)?;
    let result = (|| {
        let sessions = prepare_session_stages(report, backup, &staging_root)?;
        let databases = prepare_database_stages(report, backup, &staging_root)?;
        let (mut goals_databases, mut state_databases): (Vec<_>, Vec<_>) = databases
            .into_iter()
            .partition(|database| is_goals_database_id(&database.database_id));
        merge_database_views(&sessions, &mut state_databases)
            .map_err(|error| format!("migration state-view merge failed: {error}"))?;
        merge_goals_database_views(&mut goals_databases)
            .map_err(|error| format!("migration goals-view merge failed: {error}"))?;
        state_databases.extend(goals_databases);
        state_databases.sort_by(|left, right| left.database_id.cmp(&right.database_id));
        let databases = state_databases;
        let plan = MigrationApplyPlan {
            schema_version: APPLY_PLAN_SCHEMA_VERSION,
            operation_id: report.operation_id.clone(),
            generated_at_ms: timestamp_millis()?,
            canonical_root: codex_home.to_path_buf(),
            inventory_fingerprint: report.plan.inventory_fingerprint.clone(),
            backup_dir: backup.backup_dir.clone(),
            staging_root: staging_root.clone(),
            sessions,
            databases,
            conflict_count: report.plan.conflicts.len(),
        };
        validate_prepared_plan(&plan, report, backup)?;
        write_apply_plan(&plan_path, &plan)?;
        persist_initial_migration_replacement_phases(&plan)?;
        prepared_result(plan, &plan_path)
    })();
    if result.is_err() {
        let _ = remove_owned_staging_tree(&staging_root);
    }
    result
}

pub fn load_migration_apply_plan(
    data_root: &Path,
    operation_id: &str,
) -> Result<MigrationApplyPlan, String> {
    let path = operation_root(data_root, operation_id)?.join("migration-apply-plan.json");
    let bytes = read_regular_file_bounded(&path, MAX_APPLY_PLAN_BYTES)
        .map_err(|_| "migration apply plan is unreadable".to_string())?;
    let envelope = serde_json::from_slice::<MigrationApplyPlanEnvelope>(&bytes)
        .map_err(|_| "migration apply plan is invalid".to_string())?;
    validate_plan_shape(&envelope.plan)?;
    validate_recovery_bindings(&envelope.plan, &envelope.replacement_paths)?;
    if envelope.integrity_sha256
        != plan_envelope_digest(&envelope.plan, &envelope.replacement_paths)?
    {
        return Err("migration apply plan integrity check failed".to_string());
    }
    if envelope.plan.operation_id != operation_id {
        return Err("migration apply plan identity changed".to_string());
    }
    Ok(envelope.plan)
}

fn prepared_result(
    plan: MigrationApplyPlan,
    plan_path: &Path,
) -> Result<PreparedMigrationApply, String> {
    let mut created_files = Vec::new();
    let mut prepared_bytes = 0_u64;
    for session in &plan.sessions {
        let Some(path) = &session.staged_path else {
            continue;
        };
        let (bytes, sha256) = stable_file_digest(path)?;
        if sha256 != session.expected_sha256 {
            return Err("prepared migration session checksum changed".to_string());
        }
        prepared_bytes = prepared_bytes.saturating_add(bytes);
        created_files.push(LedgerFileSnapshot {
            path: path.clone(),
            bytes,
            sha256,
            created_by_operation: true,
            logical_thread_id: Some(session.thread_id.clone()),
        });
    }
    for database in &plan.databases {
        let (bytes, sha256) = stable_file_digest(&database.staged_path)?;
        if bytes != database.staged_bytes || sha256 != database.staged_sha256 {
            return Err("prepared migration database checksum changed".to_string());
        }
        prepared_bytes = prepared_bytes.saturating_add(bytes);
        created_files.push(LedgerFileSnapshot {
            path: database.staged_path.clone(),
            bytes,
            sha256,
            created_by_operation: true,
            logical_thread_id: None,
        });
    }
    let (plan_bytes, plan_sha256) = stable_file_digest(plan_path)?;
    created_files.push(LedgerFileSnapshot {
        path: plan_path.to_path_buf(),
        bytes: plan_bytes,
        sha256: plan_sha256,
        created_by_operation: true,
        logical_thread_id: None,
    });
    let rollback_steps = rollback_steps(&plan);
    Ok(PreparedMigrationApply {
        receipt: MigrationPreparationReceipt {
            operation_id: plan.operation_id.clone(),
            prepared_session_count: plan
                .sessions
                .iter()
                .filter(|session| session.staged_path.is_some())
                .count(),
            prepared_database_count: plan.databases.len(),
            conflict_count: plan.conflict_count,
            prepared_bytes,
        },
        plan,
        created_files,
        rollback_steps,
    })
}

pub fn apply_prepared_migration<Guard>(
    plan: &MigrationApplyPlan,
    mut before_live_write: Guard,
) -> Result<(), String>
where
    Guard: FnMut() -> Result<(), String>,
{
    validate_plan_shape(plan)?;
    before_live_write()?;
    reconcile_replacement_recovery_state(plan)?;
    let mut barriers = acquire_apply_write_barriers(plan)?;
    validate_apply_preconditions(plan, &mut barriers)?;
    apply_prepared_migration_after_preconditions(plan, before_live_write, &mut barriers)
}

pub fn apply_prepared_migration_classified<Guard>(
    plan: &MigrationApplyPlan,
    mut mark_live_mutation_started: impl FnMut() -> Result<(), String>,
    mut before_live_write: Guard,
) -> Result<(), MigrationRollbackFailure>
where
    Guard: FnMut() -> Result<(), String>,
{
    validate_plan_shape(plan).map_err(MigrationRollbackFailure::Precondition)?;
    mark_live_mutation_started().map_err(MigrationRollbackFailure::Precondition)?;
    before_live_write().map_err(MigrationRollbackFailure::LiveWriteGuard)?;
    reconcile_replacement_recovery_state(plan).map_err(MigrationRollbackFailure::Operation)?;
    let mut barriers =
        acquire_apply_write_barriers(plan).map_err(MigrationRollbackFailure::Precondition)?;
    validate_apply_preconditions(plan, &mut barriers)
        .map_err(MigrationRollbackFailure::Precondition)?;
    apply_prepared_migration_after_preconditions(plan, before_live_write, &mut barriers)
        .map_err(MigrationRollbackFailure::Operation)
}

fn apply_prepared_migration_after_preconditions<Guard>(
    plan: &MigrationApplyPlan,
    mut before_live_write: Guard,
    barriers: &mut BTreeMap<String, WriteExclusionGuard>,
) -> Result<(), String>
where
    Guard: FnMut() -> Result<(), String>,
{
    for session in &plan.sessions {
        match session.action {
            MigrationSessionAction::CopyToCanonical => {
                let staged = session
                    .staged_path
                    .as_ref()
                    .ok_or_else(|| "migration session staging file is missing".to_string())?;
                let witness = ownership_witness_path(&session.target_path, &plan.operation_id)?;
                before_live_write()?;
                let key = path_key(&session.target_path);
                if let Some(barrier) = barriers.get_mut(&key) {
                    barrier.verify_current_path(Some(&session.expected_sha256))?;
                    verify_session_ownership_witness(session, &witness)?;
                } else {
                    let created =
                        atomic_create_with_witness(&session.target_path, &witness, |output| {
                            let mut source = open_regular_file(staged)?;
                            io::copy(&mut source, output).map(|_| ()).map_err(|_| {
                                "failed to publish canonical migration session".to_string()
                            })
                        })?;
                    if !created {
                        return Err("migration canonical target appeared concurrently".to_string());
                    }
                    verify_session_ownership_witness(session, &witness)?;
                    barriers.insert(key, WriteExclusionGuard::acquire(&session.target_path)?);
                }
                verify_applied_session(session)?;
            }
            MigrationSessionAction::ReplaceCanonicalWithExtension => {
                let staged = session
                    .staged_path
                    .as_ref()
                    .ok_or_else(|| "migration session staging file is missing".to_string())?;
                before_live_write()?;
                let before = session
                    .target_before_sha256
                    .as_deref()
                    .ok_or_else(|| "migration canonical precondition is missing".to_string())?;
                let current = barriers
                    .get_mut(&path_key(&session.target_path))
                    .ok_or_else(|| "migration canonical writer barrier is missing".to_string())?
                    .verify_current_path(None)?
                    .1;
                if current != before && current != session.expected_sha256 {
                    return Err("migration canonical precondition changed".to_string());
                }
                if current != session.expected_sha256 {
                    replace_plan_target(
                        plan,
                        barriers,
                        &session.target_path,
                        staged,
                        &session.expected_sha256,
                        false,
                    )?;
                }
                verify_applied_session(session)?;
            }
            MigrationSessionAction::KeepCanonical => {
                barriers
                    .get_mut(&path_key(&session.target_path))
                    .ok_or_else(|| "migration canonical writer barrier is missing".to_string())?
                    .verify_current_path(Some(&session.expected_sha256))?;
                verify_applied_session(session)?;
            }
            MigrationSessionAction::Conflict => {}
        }
    }
    let mut ordered_databases = plan.databases.iter().collect::<Vec<_>>();
    ordered_databases.sort_by_key(|database| {
        (
            is_goals_database_id(&database.database_id),
            database.database_id.clone(),
        )
    });
    for database in ordered_databases {
        before_live_write()?;
        let raw_current = barriers
            .get_mut(&path_key(&database.target_path))
            .ok_or_else(|| "migration database writer barrier is missing".to_string())?
            .verify_current_path(None)?
            .1;
        let current = guarded_database_digest(plan, database, barriers)?;
        if current != database.original_sha256 && current != database.staged_sha256 {
            return Err("migration database changed after planning".to_string());
        }
        if raw_current != database.staged_sha256 {
            replace_plan_target(
                plan,
                barriers,
                &database.target_path,
                &database.staged_path,
                &database.staged_sha256,
                false,
            )?;
        }
        let (_, sha256) = stable_file_digest(&database.target_path)?;
        if sha256 != database.staged_sha256 {
            let staged_matches_plan = stable_file_digest(&database.staged_path)
                .map(|(_, staged_sha256)| staged_sha256 == database.staged_sha256)
                .unwrap_or(false);
            return Err(format!(
                "migration database replacement checksum changed for {} (stagedMatchesPlan={staged_matches_plan})",
                database.database_id
            ));
        }
    }
    Ok(())
}

fn canonical_goals_database(plan: &MigrationApplyPlan) -> Option<&MigrationDatabaseApplyEntry> {
    plan.databases
        .iter()
        .filter(|database| is_goals_database_id(&database.database_id))
        .min_by_key(|database| {
            (
                database_role_rank(database.role),
                durable_migration_path_key(&database.target_path),
            )
        })
}

fn replace_plan_target(
    plan: &MigrationApplyPlan,
    barriers: &mut BTreeMap<String, WriteExclusionGuard>,
    target: &Path,
    source: &Path,
    source_sha256: &str,
    hardlink: bool,
) -> Result<(), String> {
    let key = path_key(target);
    let mut barrier = barriers
        .remove(&key)
        .ok_or_else(|| "migration writer barrier is missing".to_string())?;
    let persisted = replacement_paths_for_target(plan, target)?;
    let paths = persisted.typed()?;
    let expected_original_sha256 = barrier.verify_current_path(None)?.1;
    let phase_entry = migration_replacement_phase_entry(plan, target)?;
    if phase_entry.phase != MigrationHandleReplacePhase::Planned
        || phase_entry.expected_original_sha256.is_some()
        || phase_entry.identities.is_some()
        || phase_entry.expected_replacement_sha256 != source_sha256
    {
        barriers.insert(key, barrier);
        return Err("migration replacement phase is not ready for apply".to_string());
    }
    let staged = if hardlink {
        barrier.stage_handle_hardlink_replace(source, source_sha256, &paths)
    } else {
        barrier.stage_handle_replace(source, source_sha256, &paths)
    }?;
    let identities = staged.identity_bindings()?;
    if let Err(error) = record_staged_migration_replacement(
        plan,
        target,
        &expected_original_sha256,
        source_sha256,
        false,
        identities,
    ) {
        return match staged.restore() {
            Ok(resolved) => match resolved.cleanup_after_durable_terminal() {
                Ok(barrier) => {
                    barriers.insert(key, barrier);
                    Err(error)
                }
                Err((cleanup_error, resolved)) => {
                    barriers.insert(key, resolved.retain_for_recovery());
                    Err(format!(
                        "{error}; migration staged replacement cleanup failed: {cleanup_error}"
                    ))
                }
            },
            Err((restore_error, _)) => Err(format!(
                "{error}; migration staged replacement restoration failed: {restore_error}"
            )),
        };
    }
    transition_migration_replacement_phase(
        plan,
        target,
        &[MigrationHandleReplacePhase::Staged],
        MigrationHandleReplacePhase::Preparing,
    )?;
    let prepared = staged.prepare().map_err(|(error, _)| error)?;
    transition_migration_replacement_phase(
        plan,
        target,
        &[MigrationHandleReplacePhase::Preparing],
        MigrationHandleReplacePhase::Prepared,
    )?;
    transition_migration_replacement_phase(
        plan,
        target,
        &[MigrationHandleReplacePhase::Prepared],
        MigrationHandleReplacePhase::Publishing,
    )?;
    let published = prepared.publish().map_err(|(error, _)| error)?;
    let mut resolved = published.commit().map_err(|(error, _)| error)?;
    resolved
        .guard_mut()
        .verify_current_path(Some(source_sha256))
        .map_err(|error| {
            format!(
                "{error}; migration committed replacement target exists={}",
                target.exists()
            )
        })?;
    if let Err(error) = finish_recovered_migration_replacement_phase(
        plan,
        target,
        &[MigrationHandleReplacePhase::Publishing],
        MigrationHandleReplacePhase::CommittedWithRecovery,
    ) {
        barriers.insert(key, resolved.retain_for_recovery());
        return Err(error);
    }
    barriers.insert(key, resolved.retain_for_recovery());
    Ok(())
}

pub fn validate_applied_migration(
    plan: &MigrationApplyPlan,
) -> Result<MigrationApplyReceipt, String> {
    validate_plan_shape(plan)?;
    for session in &plan.sessions {
        if session.action != MigrationSessionAction::Conflict {
            verify_applied_session(session)?;
        }
    }
    let active_sessions = plan
        .sessions
        .iter()
        .filter(|session| session.action != MigrationSessionAction::Conflict)
        .collect::<Vec<_>>();
    for database in &plan.databases {
        quick_check_sqlite(&database.target_path)?;
        ensure_sqlite_sidecars_absent(&database.target_path)?;
        let (_, sha256) = stable_file_digest(&database.target_path)?;
        if sha256 != database.staged_sha256 {
            return Err("applied migration database changed".to_string());
        }
        let connection = Connection::open_with_flags(
            &database.target_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|_| "failed to open applied migration database".to_string())?;
        if is_goals_database_id(&database.database_id) {
            goals_database_digest(&connection)?;
            continue;
        }
        for session in &active_sessions {
            let path = connection
                .query_row(
                    "SELECT rollout_path FROM threads WHERE id = ?1",
                    [&session.thread_id],
                    |row| row.get::<_, String>(0),
                )
                .map_err(|_| "applied migration database is missing a thread".to_string())?;
            if path_key(Path::new(&path)) != path_key(&session.target_path) {
                return Err("applied migration database thread path is incorrect".to_string());
            }
        }
    }
    if let Some(canonical) = canonical_goals_database(plan) {
        for database in plan
            .databases
            .iter()
            .filter(|database| is_goals_database_id(&database.database_id))
        {
            let (_, sha256) = stable_file_digest(&database.target_path)?;
            if sha256 != canonical.staged_sha256 {
                return Err("goals database views did not converge to exact content".to_string());
            }
        }
    }
    Ok(MigrationApplyReceipt {
        operation_id: plan.operation_id.clone(),
        canonical_created_count: plan
            .sessions
            .iter()
            .filter(|session| session.action == MigrationSessionAction::CopyToCanonical)
            .count(),
        canonical_replaced_count: plan
            .sessions
            .iter()
            .filter(|session| {
                session.action == MigrationSessionAction::ReplaceCanonicalWithExtension
            })
            .count(),
        database_view_count: plan.databases.len(),
        conflict_count: plan.conflict_count,
        validated: true,
        runtime_verification: None,
    })
}

pub fn verify_applied_migration_with_runtime<V: MigrationBackupRuntimeVerifier>(
    plan: &MigrationApplyPlan,
    verifier: &V,
) -> Result<MigrationRuntimeVerification, String> {
    validate_plan_shape(plan)?;
    let active_sessions = plan
        .sessions
        .iter()
        .filter(|session| session.action != MigrationSessionAction::Conflict)
        .collect::<Vec<_>>();
    if active_sessions.is_empty() {
        return Err("applied migration has no session for runtime verification".to_string());
    }
    let canonical_database_count = plan
        .databases
        .iter()
        .filter(|database| {
            database.role == DatabaseRole::CanonicalAccount
                && !is_goals_database_id(&database.database_id)
        })
        .count();
    if canonical_database_count != 1 {
        return Err(
            "applied migration does not have exactly one canonical runtime database".to_string(),
        );
    }

    let validation_root = plan.staging_root.join("post-apply-runtime-validation");
    if validation_root.exists() {
        return Err("post-apply runtime validation staging already exists".to_string());
    }
    create_safe_directory(&validation_root)?;
    let result = (|| {
        let backup_root = validation_root.join("backup");
        create_safe_directory(&backup_root)?;
        let mut sources = Vec::with_capacity(active_sessions.len() + plan.databases.len());
        for (index, session) in active_sessions.iter().enumerate() {
            verify_applied_session(session)?;
            sources.push(MigrationBackupSource {
                source_path: session.target_path.clone(),
                payload_relative_path: PathBuf::from(format!(
                    "canonical/sessions/{index:06}.jsonl"
                )),
                kind: MigrationBackupEntryKind::Session,
                expected_sha256: Some(session.expected_sha256.clone()),
                logical_thread_id: Some(session.thread_id.clone()),
            });
        }
        let canonical_goals = canonical_goals_database(plan);
        for (index, database) in plan.databases.iter().enumerate() {
            if is_goals_database_id(&database.database_id)
                && canonical_goals
                    .is_some_and(|canonical| canonical.database_id != database.database_id)
            {
                continue;
            }
            let payload_relative_path = if is_goals_database_id(&database.database_id) {
                PathBuf::from("databases/canonical-goals_1.sqlite")
            } else if database.role == DatabaseRole::CanonicalAccount {
                PathBuf::from("databases/canonical-state_5.sqlite")
            } else {
                PathBuf::from(format!("databases/views/{index:06}.sqlite"))
            };
            sources.push(MigrationBackupSource {
                source_path: database.target_path.clone(),
                payload_relative_path,
                kind: MigrationBackupEntryKind::Database,
                expected_sha256: None,
                logical_thread_id: None,
            });
        }
        let backup = create_migration_backup(&backup_root, "post-apply", &sources)?;
        let verified = verify_migration_backup_with_runtime(
            &backup.backup_dir,
            &validation_root.join("isolated"),
            verifier,
        )?;
        let runtime = verified
            .runtime_verification
            .ok_or_else(|| "post-apply Codex runtime verification is missing".to_string())?;
        let mut available_categories = runtime.available_categories.clone();
        available_categories.sort();
        available_categories.dedup();
        let mut continued_categories = runtime.continued_categories.clone();
        continued_categories.sort();
        continued_categories.dedup();
        if runtime.expected_session_count != active_sessions.len()
            || runtime.listed_session_count != active_sessions.len()
            || runtime.resumed_session_count != active_sessions.len()
            || runtime.continued_session_count == 0
            || available_categories.is_empty()
            || available_categories != continued_categories
            || (runtime.tool_session_count > 0 && !runtime.tool_round_trip_verified)
            || (runtime.conflict_payload_count > 0 && !runtime.conflict_payloads_verified)
        {
            return Err(
                "post-apply Codex runtime did not verify every canonical session".to_string(),
            );
        }
        Ok(runtime)
    })();
    if result.is_err() {
        let _ = remove_owned_staging_tree(&validation_root);
    }
    result
}

pub fn rollback_migration_plan<Guard>(
    store: &OperationLedgerStore,
    plan: &MigrationApplyPlan,
    mut before_live_write: Guard,
) -> Result<MigrationRollbackReceipt, MigrationRollbackFailure>
where
    Guard: FnMut() -> Result<(), String>,
{
    validate_plan_shape(plan).map_err(MigrationRollbackFailure::Operation)?;
    let ledger = store
        .load(&plan.operation_id)
        .map_err(MigrationRollbackFailure::Operation)?;
    if ledger.phase != SessionStorageOperationPhase::RollingBack {
        return Err(MigrationRollbackFailure::Operation(
            "migration operation is not in rollback".to_string(),
        ));
    }
    if !rollback_step_plan_matches(&ledger.rollback_steps, &rollback_steps(plan)) {
        return Err(MigrationRollbackFailure::Operation(
            "migration rollback plan changed".to_string(),
        ));
    }
    preflight_rollback_migration(plan, &ledger.rollback_steps)
        .map_err(MigrationRollbackFailure::Precondition)?;
    let mut receipt = MigrationRollbackReceipt {
        operation_id: plan.operation_id.clone(),
        completed_step_count: ledger
            .rollback_steps
            .iter()
            .filter(|step| step.completed)
            .count(),
        removed_created_file_count: 0,
        restored_file_count: 0,
        restored_database_count: 0,
    };
    for (index, step) in ledger.rollback_steps.iter().cloned().enumerate() {
        if step.completed {
            continue;
        }
        before_live_write().map_err(MigrationRollbackFailure::LiveWriteGuard)?;
        match step.action {
            RollbackActionKind::RestoreDatabase => {
                restore_database_step(plan, index, &step)
                    .map_err(MigrationRollbackFailure::Operation)?;
                receipt.restored_database_count = receipt.restored_database_count.saturating_add(1);
            }
            RollbackActionKind::RestoreFile => {
                restore_file_step(plan, &step).map_err(MigrationRollbackFailure::Operation)?;
                receipt.restored_file_count = receipt.restored_file_count.saturating_add(1);
            }
            RollbackActionKind::RemoveCreatedFile => {
                remove_created_session_step(plan, &step)
                    .map_err(MigrationRollbackFailure::Operation)?;
                receipt.removed_created_file_count =
                    receipt.removed_created_file_count.saturating_add(1);
            }
            RollbackActionKind::RestoreConfig => {
                return Err(MigrationRollbackFailure::Operation(
                    "migration rollback contains an unsupported config step".to_string(),
                ))
            }
        }
        store
            .update(&plan.operation_id, |ledger| {
                let current = ledger
                    .rollback_steps
                    .get_mut(index)
                    .ok_or_else(|| "migration rollback ledger changed".to_string())?;
                if current != &step {
                    return Err("migration rollback ledger changed".to_string());
                }
                current.completed = true;
                Ok(())
            })
            .map_err(MigrationRollbackFailure::Operation)?;
        receipt.completed_step_count = receipt.completed_step_count.saturating_add(1);
    }
    cleanup_migration_staging(plan).map_err(MigrationRollbackFailure::Operation)?;
    Ok(receipt)
}

fn preflight_rollback_migration(
    plan: &MigrationApplyPlan,
    steps: &[LedgerRollbackStep],
) -> Result<(), String> {
    let phase_before = load_migration_replacement_phases(plan)?;
    for (step_index, step) in steps.iter().enumerate() {
        let expected = step
            .expected_sha256
            .as_deref()
            .ok_or_else(|| "migration rollback expected checksum is missing".to_string())?;
        let applied = step
            .applied_sha256
            .as_deref()
            .ok_or_else(|| "migration rollback applied checksum is missing".to_string())?;
        match step.action {
            RollbackActionKind::RestoreDatabase => {
                quick_check_sqlite(&step.source_path)?;
                ensure_sqlite_sidecars_absent(&step.source_path)?;
                if stable_file_digest(&step.source_path)?.1 != expected {
                    return Err("migration rollback database source changed".to_string());
                }
                let target_present = match fs::symlink_metadata(&step.target_path) {
                    Ok(metadata)
                        if metadata.is_file() && !metadata_is_link_or_reparse(&metadata) =>
                    {
                        true
                    }
                    Ok(_) => return Err("migration rollback database target is unsafe".to_string()),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => false,
                    Err(_) => {
                        return Err("migration rollback database target is unavailable".to_string())
                    }
                };
                if target_present {
                    let mut barrier = WriteExclusionGuard::acquire(&step.target_path)?;
                    let live =
                        guarded_rollback_database_digest(plan, step_index, step, &mut barrier)?;
                    if live != expected && live != applied {
                        return Err(
                            "migration database changed after apply; rollback deferred".to_string()
                        );
                    }
                }
                preflight_rollback_replacement_phase(plan, step, applied, target_present)?;
            }
            RollbackActionKind::RestoreFile => {
                if stable_file_digest(&step.source_path)?.1 != expected {
                    return Err("migration rollback source changed".to_string());
                }
                let target_present = match fs::symlink_metadata(&step.target_path) {
                    Ok(metadata)
                        if metadata.is_file() && !metadata_is_link_or_reparse(&metadata) =>
                    {
                        true
                    }
                    Ok(_) => return Err("migration rollback file target is unsafe".to_string()),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => false,
                    Err(_) => return Err("migration rollback file is unavailable".to_string()),
                };
                if target_present {
                    let mut barrier = WriteExclusionGuard::acquire(&step.target_path)?;
                    let live = barrier.verify_current_path(None)?.1;
                    if live != expected && live != applied {
                        return Err(
                            "migration file changed after apply; rollback deferred".to_string()
                        );
                    }
                    barrier.verify_current_path(Some(&live))?;
                }
                preflight_rollback_replacement_phase(plan, step, applied, target_present)?;
            }
            RollbackActionKind::RemoveCreatedFile => {
                let witness = ownership_witness_path(&step.target_path, &plan.operation_id)?;
                match fs::symlink_metadata(&step.target_path) {
                    Ok(metadata) => {
                        if !metadata.is_file() || metadata_is_link_or_reparse(&metadata) {
                            return Err("migration rollback created file is unsafe".to_string());
                        }
                        let mut barrier = DestructiveFileGuard::acquire(&step.target_path)?;
                        barrier.verify_current_path(Some(expected)).map_err(|_| {
                            "migration rollback created file changed after apply".to_string()
                        })?;
                        barrier
                            .verify_same_identity_path(&witness, Some(expected))
                            .map_err(|_| {
                                "migration rollback ownership witness changed after apply"
                                    .to_string()
                            })?;
                        if rollback_database_sources_reference_path(steps, &step.target_path)? {
                            return Err(
                                "migration rollback created file is still referenced".to_string()
                            );
                        }
                        barrier.verify_current_path(Some(expected))?;
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        if witness.exists() && stable_file_digest(&witness)?.1 != expected {
                            return Err("migration rollback ownership witness changed after apply"
                                .to_string());
                        }
                    }
                    Err(_) => return Err("migration rollback file is unavailable".to_string()),
                }
            }
            RollbackActionKind::RestoreConfig => {
                return Err("migration rollback contains an unsupported config step".to_string())
            }
        }
    }
    if load_migration_replacement_phases(plan)? != phase_before {
        return Err("migration replacement phase changed during rollback preflight".to_string());
    }
    Ok(())
}

fn rollback_database_sources_reference_path(
    steps: &[LedgerRollbackStep],
    target_path: &Path,
) -> Result<bool, String> {
    let target_key = path_key(target_path);
    for step in steps
        .iter()
        .filter(|step| step.action == RollbackActionKind::RestoreDatabase)
    {
        let connection = Connection::open_with_flags(
            &step.source_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|_| "failed to inspect migration rollback source references".to_string())?;
        if !table_exists(&connection, "threads")? {
            continue;
        }
        let mut statement = connection
            .prepare("SELECT rollout_path FROM threads")
            .map_err(|_| "failed to inspect migration rollback source references".to_string())?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|_| "failed to inspect migration rollback source references".to_string())?;
        for row in rows {
            let path = row
                .map_err(|_| "failed to inspect migration rollback source reference".to_string())?;
            if path_key(Path::new(&path)) == target_key {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn preflight_rollback_replacement_phase(
    plan: &MigrationApplyPlan,
    step: &LedgerRollbackStep,
    applied: &str,
    target_present: bool,
) -> Result<(), String> {
    let entry = migration_replacement_phase_entry(plan, &step.target_path)?;
    if entry.expected_replacement_sha256 != applied
        || matches!(
            entry.phase,
            MigrationHandleReplacePhase::CommitCleaning
                | MigrationHandleReplacePhase::CommittedCleaned
        )
    {
        return Err("migration rollback replacement phase is incompatible".to_string());
    }
    validate_migration_replacement_phase_layout(&entry)?;
    if !target_present {
        if matches!(
            entry.phase,
            MigrationHandleReplacePhase::Planned
                | MigrationHandleReplacePhase::Skipped
                | MigrationHandleReplacePhase::Staged
                | MigrationHandleReplacePhase::CommittedWithRecovery
                | MigrationHandleReplacePhase::RolledBackWithTombstone
                | MigrationHandleReplacePhase::RolledBackCleaned
        ) {
            return Err("migration rollback replacement target disappeared".to_string());
        }
        let state = classify_migration_replacement_entry(&entry)?;
        if !matches!(
            state,
            HandleReplaceCrashState::Prepared | HandleReplaceCrashState::RollbackPrepared
        ) {
            return Err("migration rollback replacement target disappeared".to_string());
        }
    }
    Ok(())
}

fn guarded_rollback_database_digest(
    plan: &MigrationApplyPlan,
    step_index: usize,
    step: &LedgerRollbackStep,
    barrier: &mut WriteExclusionGuard,
) -> Result<String, String> {
    let raw_sha256 = barrier.verify_current_path(None)?.1;
    let expected = step
        .expected_sha256
        .as_deref()
        .ok_or_else(|| "migration rollback expected checksum is missing".to_string())?;
    let applied = step
        .applied_sha256
        .as_deref()
        .ok_or_else(|| "migration rollback applied checksum is missing".to_string())?;
    if (raw_sha256 == expected || raw_sha256 == applied)
        && sqlite_sidecars_absent(&step.target_path)?
    {
        return Ok(raw_sha256);
    }
    if !sqlite_sidecars_absent(&step.target_path)? {
        return Err("migration database has active SQLite sidecars".to_string());
    }
    let guarded_copy = plan
        .staging_root
        .join(format!(".rb-copy-{step_index:04}.sqlite"));
    let snapshot = plan
        .staging_root
        .join(format!(".rb-check-{step_index:04}.sqlite"));
    for path in [&guarded_copy, &snapshot] {
        remove_sqlite_sidecars(path)?;
        if path.exists() {
            fs::remove_file(path)
                .map_err(|_| "failed to reset guarded rollback snapshot".to_string())?;
        }
    }
    let result = (|| {
        barrier.copy_current_to_new_file(&guarded_copy, Some(&raw_sha256))?;
        snapshot_sqlite_database(&guarded_copy, &snapshot)?;
        quick_check_sqlite(&snapshot)?;
        let digest = stable_file_digest(&snapshot).map(|(_, sha256)| sha256)?;
        barrier.verify_current_path(Some(&raw_sha256))?;
        Ok(digest)
    })();
    let mut cleanup_error = None;
    for path in [&snapshot, &guarded_copy] {
        if let Err(error) = fs::remove_file(path) {
            if error.kind() != io::ErrorKind::NotFound {
                cleanup_error = Some("failed to remove guarded rollback snapshot".to_string());
            }
        }
        if remove_sqlite_sidecars(path).is_err() {
            cleanup_error = Some("failed to remove guarded rollback sidecars".to_string());
        }
    }
    match (result, cleanup_error) {
        (Ok(value), None) => Ok(value),
        (Err(error), None) => Err(error),
        (Ok(_), Some(error)) => Err(error),
        (Err(error), Some(cleanup)) => Err(format!("{error}; {cleanup}")),
    }
}

pub fn recover_interrupted_migration<Guard>(
    store: &OperationLedgerStore,
    data_root: &Path,
    operation_id: &str,
    before_live_write: Guard,
) -> Result<MigrationRecoveryReceipt, String>
where
    Guard: FnMut() -> Result<(), String>,
{
    let mut ledger = store.load(operation_id)?;
    if ledger.kind != SessionStorageOperationKind::Migration
        || !matches!(
            ledger.phase,
            SessionStorageOperationPhase::Applying
                | SessionStorageOperationPhase::Validating
                | SessionStorageOperationPhase::RollingBack
        )
    {
        return Err("session storage operation is not an interrupted migration".to_string());
    }
    if matches!(
        ledger.phase,
        SessionStorageOperationPhase::Applying | SessionStorageOperationPhase::Validating
    ) {
        if !ledger.live_mutation_started {
            store.update(operation_id, |ledger| {
                ledger.last_error_code = Some("migrationAbortedBeforeLiveMutation".to_string());
                Ok(())
            })?;
            store.transition(operation_id, SessionStorageOperationPhase::RollingBack)?;
            cleanup_migration_staging_for_operation(data_root, operation_id)?;
            store.transition(operation_id, SessionStorageOperationPhase::RolledBack)?;
            return Ok(MigrationRecoveryReceipt {
                operation_id: operation_id.to_string(),
                status: MigrationRecoveryStatus::RolledBack,
                rollback: None,
            });
        }
        ledger = store.transition(operation_id, SessionStorageOperationPhase::RollingBack)?;
    }

    let plan = match load_migration_apply_plan(data_root, operation_id) {
        Ok(plan)
            if plan.canonical_root == ledger.canonical_root
                && ledger.backup_root.as_ref() == Some(&plan.backup_dir)
                && rollback_step_plan_matches(&ledger.rollback_steps, &rollback_steps(&plan)) =>
        {
            plan
        }
        Ok(_) | Err(_) => {
            mark_migration_recovery_retry(store, operation_id, "migrationRollbackPlanInvalid")?;
            return Ok(MigrationRecoveryReceipt {
                operation_id: operation_id.to_string(),
                status: MigrationRecoveryStatus::Failed,
                rollback: None,
            });
        }
    };

    match rollback_migration_plan(store, &plan, before_live_write) {
        Ok(receipt) => {
            store.transition(operation_id, SessionStorageOperationPhase::RolledBack)?;
            Ok(MigrationRecoveryReceipt {
                operation_id: operation_id.to_string(),
                status: MigrationRecoveryStatus::RolledBack,
                rollback: Some(receipt),
            })
        }
        Err(MigrationRollbackFailure::LiveWriteGuard(_)) => {
            store.update(operation_id, |ledger| {
                ledger.last_error_code = Some("migrationRollbackWriterActive".to_string());
                Ok(())
            })?;
            Ok(MigrationRecoveryReceipt {
                operation_id: operation_id.to_string(),
                status: MigrationRecoveryStatus::DeferredByLiveWriter,
                rollback: None,
            })
        }
        Err(MigrationRollbackFailure::Precondition(_) | MigrationRollbackFailure::Operation(_)) => {
            mark_migration_recovery_retry(store, operation_id, "migrationRollbackRetryRequired")?;
            Ok(MigrationRecoveryReceipt {
                operation_id: operation_id.to_string(),
                status: MigrationRecoveryStatus::Failed,
                rollback: None,
            })
        }
    }
}

fn mark_migration_recovery_retry(
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

pub fn cleanup_migration_staging(plan: &MigrationApplyPlan) -> Result<(), String> {
    validate_plan_shape(plan)?;
    remove_owned_staging_tree(&plan.staging_root)
}

/// Removes only the hard-link ownership witnesses retained through the
/// migration commit transition. A missing witness is an idempotent success;
/// an existing witness must still identify the exact expected target object.
pub fn cleanup_committed_migration_ownership_witnesses(
    plan: &MigrationApplyPlan,
) -> Result<(), String> {
    validate_plan_shape(plan)?;
    for session in &plan.sessions {
        if session.action != MigrationSessionAction::CopyToCanonical {
            continue;
        }
        let witness = ownership_witness_path(&session.target_path, &plan.operation_id)?;
        match fs::symlink_metadata(&witness) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err("migration ownership witness is unavailable".to_string()),
        }
        verify_session_ownership_witness(session, &witness)?;
        remove_ownership_witness(&witness, &session.expected_sha256)?;
    }
    persist_initial_migration_replacement_phases(plan)?;
    let record = load_migration_replacement_phases(plan)?;
    for entry in record.replacements {
        match entry.phase {
            MigrationHandleReplacePhase::Planned => {
                validate_migration_replacement_phase_layout(&entry)?;
                let mut target = WriteExclusionGuard::acquire(&entry.target_path)?;
                target.verify_current_path(Some(&entry.expected_replacement_sha256))?;
                drop(target);
                transition_migration_replacement_phase(
                    plan,
                    &entry.target_path,
                    &[MigrationHandleReplacePhase::Planned],
                    MigrationHandleReplacePhase::Skipped,
                )?;
            }
            MigrationHandleReplacePhase::Skipped
            | MigrationHandleReplacePhase::CommittedCleaned => {
                validate_migration_replacement_phase_layout(&entry)?;
            }
            MigrationHandleReplacePhase::CommittedWithRecovery => {
                validate_migration_replacement_phase_layout(&entry)?;
                transition_migration_replacement_phase(
                    plan,
                    &entry.target_path,
                    &[MigrationHandleReplacePhase::CommittedWithRecovery],
                    MigrationHandleReplacePhase::CommitCleaning,
                )?;
                let mut resolved = recover_handle_replace(
                    &entry.paths.typed()?,
                    entry.identities.ok_or_else(|| {
                        "migration committed replacement identity is missing".to_string()
                    })?,
                    entry.expected_original_sha256.as_deref().ok_or_else(|| {
                        "migration committed original checksum is missing".to_string()
                    })?,
                    active_migration_replacement_sha256(&entry)?,
                    HandleReplaceRecoveryDecision::Commit,
                )?;
                resolved
                    .guard_mut()
                    .verify_current_path(Some(active_migration_replacement_sha256(&entry)?))?;
                match resolved.cleanup_after_durable_terminal() {
                    Ok(guard) => drop(guard),
                    Err((error, resolved)) => {
                        drop(resolved.retain_for_recovery());
                        return Err(error);
                    }
                }
                finish_recovered_migration_replacement_phase(
                    plan,
                    &entry.target_path,
                    &[MigrationHandleReplacePhase::CommitCleaning],
                    MigrationHandleReplacePhase::CommittedCleaned,
                )?;
            }
            MigrationHandleReplacePhase::CommitCleaning => {
                validate_migration_replacement_phase_layout(&entry)?;
                let mut resolved = recover_handle_replace(
                    &entry.paths.typed()?,
                    entry.identities.ok_or_else(|| {
                        "migration committed replacement identity is missing".to_string()
                    })?,
                    entry.expected_original_sha256.as_deref().ok_or_else(|| {
                        "migration committed original checksum is missing".to_string()
                    })?,
                    active_migration_replacement_sha256(&entry)?,
                    HandleReplaceRecoveryDecision::Commit,
                )?;
                resolved
                    .guard_mut()
                    .verify_current_path(Some(active_migration_replacement_sha256(&entry)?))?;
                match resolved.cleanup_after_durable_terminal() {
                    Ok(guard) => drop(guard),
                    Err((error, resolved)) => {
                        drop(resolved.retain_for_recovery());
                        return Err(error);
                    }
                }
                finish_recovered_migration_replacement_phase(
                    plan,
                    &entry.target_path,
                    &[MigrationHandleReplacePhase::CommitCleaning],
                    MigrationHandleReplacePhase::CommittedCleaned,
                )?;
            }
            _ => {
                return Err(
                    "migration committed replacement has a nonterminal durable phase".to_string(),
                )
            }
        }
    }
    Ok(())
}

pub fn cleanup_migration_staging_for_operation(
    data_root: &Path,
    operation_id: &str,
) -> Result<bool, String> {
    let staging_root = operation_root(data_root, operation_id)?.join("migration-staging");
    let existed = staging_root.exists();
    remove_owned_staging_tree(&staging_root)?;
    Ok(existed)
}

fn validate_preparation_inputs(
    codex_home: &Path,
    data_root: &Path,
    report: &MigrationPreflightReport,
    backup: &MigrationBackupManifest,
) -> Result<(), String> {
    validate_operation_id(&report.operation_id)?;
    validate_safe_directory(codex_home, "canonical root")?;
    validate_safe_directory(data_root, "managed data root")?;
    if !report.ready_for_backup
        || report.plan.operation_id != report.operation_id
        || report.plan.canonical_root != codex_home
        || backup.operation_id != report.operation_id
        || backup.status != MigrationBackupStatus::RuntimeVerified
    {
        return Err("migration inputs are not ready for apply planning".to_string());
    }
    let verified = verify_migration_backup(&backup.backup_dir)?;
    if &verified != backup {
        return Err("migration backup changed before apply planning".to_string());
    }
    Ok(())
}

fn validate_apply_preconditions(
    plan: &MigrationApplyPlan,
    barriers: &mut BTreeMap<String, WriteExclusionGuard>,
) -> Result<(), String> {
    let existing_created_target = plan.sessions.iter().any(|session| {
        session.action == MigrationSessionAction::CopyToCanonical
            && barriers.contains_key(&path_key(&session.target_path))
    });
    let complete_idempotent_replay = if existing_created_target {
        applied_state_is_complete(plan, barriers)?
    } else {
        false
    };
    for session in &plan.sessions {
        if let Some(staged) = &session.staged_path {
            let semantic = read_semantic_session(staged)
                .map_err(|_| "migration session staging file is invalid".to_string())?;
            let (_, sha256) = stable_file_digest(staged)?;
            if semantic.thread_id != session.thread_id || sha256 != session.expected_sha256 {
                return Err("migration session staging file changed".to_string());
            }
        }
        match session.action {
            MigrationSessionAction::CopyToCanonical => {
                if let Some(barrier) = barriers.get_mut(&path_key(&session.target_path)) {
                    let (_, sha256) = barrier.verify_current_path(None)?;
                    if sha256 != session.expected_sha256 || !complete_idempotent_replay {
                        return Err(
                            "migration canonical target appeared after planning".to_string()
                        );
                    }
                    barrier.verify_current_path(Some(&session.expected_sha256))?;
                }
            }
            MigrationSessionAction::ReplaceCanonicalWithExtension
            | MigrationSessionAction::KeepCanonical => {
                let (_, sha256) = barriers
                    .get_mut(&path_key(&session.target_path))
                    .ok_or_else(|| "migration canonical writer barrier is missing".to_string())?
                    .verify_current_path(None)?;
                let before = session
                    .target_before_sha256
                    .as_deref()
                    .ok_or_else(|| "migration canonical precondition is missing".to_string())?;
                if sha256 != before && sha256 != session.expected_sha256 {
                    return Err("migration canonical precondition changed".to_string());
                }
            }
            MigrationSessionAction::Conflict => {}
        }
    }
    for database in &plan.databases {
        quick_check_sqlite(&database.staged_path).map_err(|error| {
            format!(
                "{error} after merging migration database {}",
                database.database_id
            )
        })?;
        let (bytes, sha256) = stable_file_digest(&database.staged_path)?;
        if bytes != database.staged_bytes || sha256 != database.staged_sha256 {
            return Err("migration database staging file changed".to_string());
        }
        let current = guarded_database_digest(plan, database, barriers)?;
        if current != database.original_sha256 && current != database.staged_sha256 {
            return Err("migration database changed after planning".to_string());
        }
    }
    Ok(())
}

fn reconcile_replacement_recovery_state(plan: &MigrationApplyPlan) -> Result<(), String> {
    persist_initial_migration_replacement_phases(plan)?;
    let record = load_migration_replacement_phases(plan)?;
    for entry in record.replacements {
        validate_migration_replacement_phase_layout(&entry)?;
        match entry.phase {
            MigrationHandleReplacePhase::Planned
            | MigrationHandleReplacePhase::Skipped
            | MigrationHandleReplacePhase::CommittedWithRecovery
            | MigrationHandleReplacePhase::CommitCleaning
            | MigrationHandleReplacePhase::CommittedCleaned => {}
            MigrationHandleReplacePhase::Staged
            | MigrationHandleReplacePhase::Preparing
            | MigrationHandleReplacePhase::Prepared
            | MigrationHandleReplacePhase::Publishing
            | MigrationHandleReplacePhase::Published
            | MigrationHandleReplacePhase::Committing => {
                let resolved = recover_handle_replace(
                    &entry.paths.typed()?,
                    entry
                        .identities
                        .ok_or_else(|| "migration replacement identity is missing".to_string())?,
                    entry.expected_original_sha256.as_deref().ok_or_else(|| {
                        "migration replacement original checksum is missing".to_string()
                    })?,
                    active_migration_replacement_sha256(&entry)?,
                    HandleReplaceRecoveryDecision::Commit,
                )?;
                if let Err(error) = finish_recovered_migration_replacement_phase(
                    plan,
                    &entry.target_path,
                    &[
                        MigrationHandleReplacePhase::Staged,
                        MigrationHandleReplacePhase::Preparing,
                        MigrationHandleReplacePhase::Prepared,
                        MigrationHandleReplacePhase::Publishing,
                        MigrationHandleReplacePhase::Published,
                        MigrationHandleReplacePhase::Committing,
                    ],
                    MigrationHandleReplacePhase::CommittedWithRecovery,
                ) {
                    drop(resolved.retain_for_recovery());
                    return Err(error);
                }
                drop(resolved.retain_for_recovery());
            }
            MigrationHandleReplacePhase::RollbackPreparing
            | MigrationHandleReplacePhase::RolledBackWithTombstone
            | MigrationHandleReplacePhase::RollbackCleaning
            | MigrationHandleReplacePhase::RolledBackCleaned => {
                return Err("migration replacement is already rolling back".to_string())
            }
        }
    }
    Ok(())
}

fn applied_state_is_complete(
    plan: &MigrationApplyPlan,
    barriers: &mut BTreeMap<String, WriteExclusionGuard>,
) -> Result<bool, String> {
    for session in &plan.sessions {
        if session.action == MigrationSessionAction::Conflict {
            continue;
        }
        let Some(barrier) = barriers.get_mut(&path_key(&session.target_path)) else {
            return Ok(false);
        };
        if barrier.verify_current_path(None)?.1 != session.expected_sha256 {
            return Ok(false);
        }
    }
    for database in &plan.databases {
        if guarded_database_digest(plan, database, barriers)? != database.staged_sha256 {
            return Ok(false);
        }
    }
    Ok(true)
}

fn acquire_apply_write_barriers(
    plan: &MigrationApplyPlan,
) -> Result<BTreeMap<String, WriteExclusionGuard>, String> {
    let mut paths = BTreeMap::<String, PathBuf>::new();
    for session in &plan.sessions {
        if session.action == MigrationSessionAction::Conflict {
            continue;
        }
        match fs::symlink_metadata(&session.target_path) {
            Ok(metadata) if metadata.is_file() && !metadata_is_link_or_reparse(&metadata) => {
                paths.insert(path_key(&session.target_path), session.target_path.clone());
            }
            Ok(_) => return Err("migration canonical target is unsafe".to_string()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("migration canonical target is unavailable".to_string()),
        }
    }
    for database in &plan.databases {
        paths.insert(
            path_key(&database.target_path),
            database.target_path.clone(),
        );
    }
    let mut barriers = BTreeMap::new();
    for (key, path) in paths {
        barriers.insert(key, WriteExclusionGuard::acquire(&path)?);
    }
    Ok(barriers)
}

fn guarded_database_digest(
    plan: &MigrationApplyPlan,
    database: &MigrationDatabaseApplyEntry,
    barriers: &mut BTreeMap<String, WriteExclusionGuard>,
) -> Result<String, String> {
    let barrier = barriers
        .get_mut(&path_key(&database.target_path))
        .ok_or_else(|| "migration database writer barrier is missing".to_string())?;
    let raw_sha256 = barrier.verify_current_path(None)?.1;
    if (raw_sha256 == database.original_sha256 || raw_sha256 == database.staged_sha256)
        && sqlite_sidecars_absent(&database.target_path)?
    {
        return Ok(raw_sha256);
    }
    if !sqlite_sidecars_absent(&database.target_path)? {
        return Err("migration database has active SQLite sidecars".to_string());
    }
    let key = &database.database_id;
    let guarded_copy = plan.staging_root.join(format!(".live-guard-{key}.sqlite"));
    let snapshot = plan.staging_root.join(format!(".live-check-{key}.sqlite"));
    for path in [&guarded_copy, &snapshot] {
        remove_sqlite_sidecars(path)?;
        if path.exists() {
            fs::remove_file(path)
                .map_err(|_| "failed to reset migration database recheck".to_string())?;
        }
    }
    let result = (|| {
        barrier.copy_current_to_new_file(&guarded_copy, Some(&raw_sha256))?;
        snapshot_sqlite_database(&guarded_copy, &snapshot)?;
        stable_file_digest(&snapshot).map(|(_, sha256)| sha256)
    })();
    for path in [&snapshot, &guarded_copy] {
        let _ = fs::remove_file(path);
        let _ = remove_sqlite_sidecars(path);
    }
    result
}

fn verify_applied_session(session: &MigrationSessionApplyEntry) -> Result<(), String> {
    let semantic = read_semantic_session(&session.target_path)
        .map_err(|_| "applied canonical migration session is invalid".to_string())?;
    let (bytes, sha256) = stable_file_digest(&session.target_path)?;
    if semantic.thread_id != session.thread_id
        || sha256 != session.expected_sha256
        || bytes != semantic.bytes
    {
        return Err("applied canonical migration session changed".to_string());
    }
    Ok(())
}

pub(crate) fn snapshot_sqlite_database(source: &Path, target: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(source)
        .map_err(|_| "migration database source is unavailable".to_string())?;
    if !metadata.is_file() || metadata_is_link_or_reparse(&metadata) {
        return Err("migration database source is unsafe".to_string());
    }
    if target.exists() {
        fs::remove_file(target)
            .map_err(|_| "failed to reset migration database snapshot".to_string())?;
    }
    remove_sqlite_sidecars(target)?;
    let source_connection = Connection::open_with_flags(
        source,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| "failed to open migration database source".to_string())?;
    source_connection
        .backup(MAIN_DB, target, None)
        .map_err(|_| "failed to snapshot migration database".to_string())?;
    quick_check_sqlite(target)
}

fn restore_database_step(
    plan: &MigrationApplyPlan,
    step_index: usize,
    step: &LedgerRollbackStep,
) -> Result<(), String> {
    let expected = step
        .expected_sha256
        .as_deref()
        .ok_or_else(|| "migration database rollback checksum is missing".to_string())?;
    let applied = step
        .applied_sha256
        .as_deref()
        .ok_or_else(|| "migration database rollback applied checksum is missing".to_string())?;
    let phase = reconcile_rollback_recovery_state(plan, step, expected, applied)?;
    if phase == MigrationHandleReplacePhase::RolledBackCleaned {
        return verify_rollback_database_target(plan, step_index, step, expected);
    }
    let source_path = step.source_path.clone();
    let (_, source_sha256) = stable_file_digest(&source_path)?;
    if source_sha256 != expected {
        return Err("migration rollback source changed".to_string());
    }
    let barrier = WriteExclusionGuard::acquire(&step.target_path)?;
    let mut barrier = barrier;
    let live_sha256 = guarded_rollback_database_digest(plan, step_index, step, &mut barrier)?;
    if live_sha256 == expected {
        if phase == MigrationHandleReplacePhase::Planned {
            transition_migration_replacement_phase(
                plan,
                &step.target_path,
                &[MigrationHandleReplacePhase::Planned],
                MigrationHandleReplacePhase::Skipped,
            )?;
        }
        return Ok(());
    }
    if live_sha256 != applied {
        return Err("migration database changed after apply; rollback deferred".to_string());
    }
    barrier.verify_current_path(None)?;
    barrier = replace_rollback_target(plan, step, barrier, &source_path, expected)?;
    barrier.verify_current_path(Some(expected))?;
    let (_, target_sha256) = stable_file_digest(&step.target_path)?;
    if target_sha256 != expected {
        return Err("migration database rollback verification failed".to_string());
    }
    Ok(())
}

fn restore_file_step(plan: &MigrationApplyPlan, step: &LedgerRollbackStep) -> Result<(), String> {
    let expected = step
        .expected_sha256
        .as_deref()
        .ok_or_else(|| "migration file rollback checksum is missing".to_string())?;
    let applied = step
        .applied_sha256
        .as_deref()
        .ok_or_else(|| "migration file rollback applied checksum is missing".to_string())?;
    let phase = reconcile_rollback_recovery_state(plan, step, expected, applied)?;
    if phase == MigrationHandleReplacePhase::RolledBackCleaned {
        let mut target = WriteExclusionGuard::acquire(&step.target_path)?;
        target.verify_current_path(Some(expected))?;
        return Ok(());
    }
    let source_path = step.source_path.clone();
    let (_, source_sha256) = stable_file_digest(&source_path)?;
    if source_sha256 != expected {
        return Err("migration rollback source changed".to_string());
    }
    let barrier = WriteExclusionGuard::acquire(&step.target_path)?;
    let mut barrier = barrier;
    let live = barrier.verify_current_path(None)?.1;
    if live == expected {
        if phase == MigrationHandleReplacePhase::Planned {
            transition_migration_replacement_phase(
                plan,
                &step.target_path,
                &[MigrationHandleReplacePhase::Planned],
                MigrationHandleReplacePhase::Skipped,
            )?;
        }
        return Ok(());
    }
    if live != applied {
        return Err("migration file changed after apply; rollback deferred".to_string());
    }
    barrier = replace_rollback_target(plan, step, barrier, &source_path, expected)?;
    barrier.verify_current_path(Some(expected))?;
    let (_, target_sha256) = stable_file_digest(&step.target_path)?;
    if target_sha256 != expected {
        return Err("migration file rollback verification failed".to_string());
    }
    Ok(())
}

fn verify_rollback_database_target(
    plan: &MigrationApplyPlan,
    step_index: usize,
    step: &LedgerRollbackStep,
    expected: &str,
) -> Result<(), String> {
    let mut barrier = WriteExclusionGuard::acquire(&step.target_path)?;
    if guarded_rollback_database_digest(plan, step_index, step, &mut barrier)? == expected {
        Ok(())
    } else {
        Err("migration database rollback verification failed".to_string())
    }
}

fn reconcile_rollback_recovery_state(
    plan: &MigrationApplyPlan,
    step: &LedgerRollbackStep,
    expected: &str,
    applied: &str,
) -> Result<MigrationHandleReplacePhase, String> {
    let entry = migration_replacement_phase_entry(plan, &step.target_path)?;
    if entry.expected_replacement_sha256 != applied {
        return Err("migration rollback replacement checksum changed".to_string());
    }
    match entry.phase {
        MigrationHandleReplacePhase::Planned | MigrationHandleReplacePhase::Skipped => {
            validate_migration_replacement_phase_layout(&entry)?;
            if entry.phase == MigrationHandleReplacePhase::Skipped {
                let mut target = WriteExclusionGuard::acquire(&step.target_path)?;
                let live = target.verify_current_path(None)?.1;
                if live == expected {
                    Ok(entry.phase)
                } else {
                    Err("migration rollback skipped replacement target changed".to_string())
                }
            } else {
                Ok(entry.phase)
            }
        }
        MigrationHandleReplacePhase::Staged
        | MigrationHandleReplacePhase::Preparing
        | MigrationHandleReplacePhase::Prepared
        | MigrationHandleReplacePhase::Publishing
        | MigrationHandleReplacePhase::Published
        | MigrationHandleReplacePhase::Committing
        | MigrationHandleReplacePhase::CommittedWithRecovery
        | MigrationHandleReplacePhase::RollbackPreparing
        | MigrationHandleReplacePhase::RolledBackWithTombstone
        | MigrationHandleReplacePhase::RollbackCleaning => {
            let bound_original = if entry.rollback_replacement {
                applied
            } else {
                entry
                    .expected_original_sha256
                    .as_deref()
                    .ok_or_else(|| "migration rollback original checksum is missing".to_string())?
            };
            let bound_replacement = if entry.rollback_replacement {
                expected
            } else {
                applied
            };
            if active_migration_replacement_sha256(&entry)? != bound_replacement {
                return Err("migration rollback original checksum changed".to_string());
            }
            validate_migration_replacement_phase_layout(&entry)?;
            if !matches!(
                entry.phase,
                MigrationHandleReplacePhase::RollbackPreparing
                    | MigrationHandleReplacePhase::RolledBackWithTombstone
                    | MigrationHandleReplacePhase::RollbackCleaning
            ) {
                finish_recovered_migration_replacement_phase(
                    plan,
                    &step.target_path,
                    &[
                        MigrationHandleReplacePhase::Staged,
                        MigrationHandleReplacePhase::Preparing,
                        MigrationHandleReplacePhase::Prepared,
                        MigrationHandleReplacePhase::Publishing,
                        MigrationHandleReplacePhase::Published,
                        MigrationHandleReplacePhase::Committing,
                        MigrationHandleReplacePhase::CommittedWithRecovery,
                    ],
                    MigrationHandleReplacePhase::RollbackPreparing,
                )?;
            }
            let resolved = recover_handle_replace(
                &entry.paths.typed()?,
                entry
                    .identities
                    .ok_or_else(|| "migration rollback identity is missing".to_string())?,
                bound_original,
                active_migration_replacement_sha256(&entry)?,
                if entry.rollback_replacement {
                    HandleReplaceRecoveryDecision::Commit
                } else {
                    HandleReplaceRecoveryDecision::Restore
                },
            )?;
            if entry.phase != MigrationHandleReplacePhase::RollbackCleaning {
                finish_recovered_migration_replacement_phase(
                    plan,
                    &step.target_path,
                    &[
                        MigrationHandleReplacePhase::RollbackPreparing,
                        MigrationHandleReplacePhase::RolledBackWithTombstone,
                    ],
                    MigrationHandleReplacePhase::RolledBackWithTombstone,
                )?;
                transition_migration_replacement_phase(
                    plan,
                    &step.target_path,
                    &[MigrationHandleReplacePhase::RolledBackWithTombstone],
                    MigrationHandleReplacePhase::RollbackCleaning,
                )?;
            }
            match resolved.cleanup_after_durable_terminal() {
                Ok(guard) => drop(guard),
                Err((error, resolved)) => {
                    drop(resolved.retain_for_recovery());
                    return Err(error);
                }
            }
            finish_recovered_migration_replacement_phase(
                plan,
                &step.target_path,
                &[MigrationHandleReplacePhase::RollbackCleaning],
                MigrationHandleReplacePhase::RolledBackCleaned,
            )?;
            Ok(MigrationHandleReplacePhase::RolledBackCleaned)
        }
        MigrationHandleReplacePhase::RolledBackCleaned => {
            validate_migration_replacement_phase_layout(&entry)?;
            Ok(entry.phase)
        }
        MigrationHandleReplacePhase::CommittedCleaned
        | MigrationHandleReplacePhase::CommitCleaning => Err(
            "migration rollback recovery was cleaned before the operation became durable"
                .to_string(),
        ),
    }
}

fn replace_rollback_target(
    plan: &MigrationApplyPlan,
    step: &LedgerRollbackStep,
    barrier: WriteExclusionGuard,
    source: &Path,
    source_sha256: &str,
) -> Result<WriteExclusionGuard, String> {
    let entry = migration_replacement_phase_entry(plan, &step.target_path)?;
    if entry.phase != MigrationHandleReplacePhase::Planned {
        return Err("migration rollback replacement is not in its planned phase".to_string());
    }
    let staged = barrier.stage_handle_replace(source, source_sha256, &entry.paths.typed()?)?;
    let identities = staged.identity_bindings()?;
    record_staged_migration_replacement(
        plan,
        &step.target_path,
        step.applied_sha256
            .as_deref()
            .ok_or_else(|| "migration rollback applied checksum is missing".to_string())?,
        source_sha256,
        true,
        identities,
    )?;
    transition_migration_replacement_phase(
        plan,
        &step.target_path,
        &[MigrationHandleReplacePhase::Staged],
        MigrationHandleReplacePhase::Preparing,
    )?;
    let prepared = staged.prepare().map_err(|(error, _)| error)?;
    transition_migration_replacement_phase(
        plan,
        &step.target_path,
        &[MigrationHandleReplacePhase::Preparing],
        MigrationHandleReplacePhase::Prepared,
    )?;
    transition_migration_replacement_phase(
        plan,
        &step.target_path,
        &[MigrationHandleReplacePhase::Prepared],
        MigrationHandleReplacePhase::Publishing,
    )?;
    let published = prepared.publish().map_err(|(error, _)| error)?;
    let mut resolved = published.commit().map_err(|(error, _)| error)?;
    resolved
        .guard_mut()
        .verify_current_path(Some(source_sha256))?;
    finish_recovered_migration_replacement_phase(
        plan,
        &step.target_path,
        &[MigrationHandleReplacePhase::Publishing],
        MigrationHandleReplacePhase::RolledBackWithTombstone,
    )?;
    transition_migration_replacement_phase(
        plan,
        &step.target_path,
        &[MigrationHandleReplacePhase::RolledBackWithTombstone],
        MigrationHandleReplacePhase::RollbackCleaning,
    )?;
    let barrier = match resolved.cleanup_after_durable_terminal() {
        Ok(barrier) => barrier,
        Err((error, resolved)) => {
            drop(resolved.retain_for_recovery());
            return Err(error);
        }
    };
    finish_recovered_migration_replacement_phase(
        plan,
        &step.target_path,
        &[MigrationHandleReplacePhase::RollbackCleaning],
        MigrationHandleReplacePhase::RolledBackCleaned,
    )?;
    Ok(barrier)
}

fn remove_created_session_step(
    plan: &MigrationApplyPlan,
    step: &LedgerRollbackStep,
) -> Result<(), String> {
    let expected = step
        .expected_sha256
        .as_deref()
        .ok_or_else(|| "migration rollback file checksum is missing".to_string())?;
    let witness = ownership_witness_path(&step.target_path, &plan.operation_id)?;
    let metadata = match fs::symlink_metadata(&step.target_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return remove_ownership_witness_if_present(&witness, expected)
        }
        Err(_) => return Err("migration rollback file is unavailable".to_string()),
    };
    if !metadata.is_file() || metadata_is_link_or_reparse(&metadata) {
        return Err("migration rollback file is unsafe".to_string());
    }
    let (_, sha256) = stable_file_digest(&step.target_path)?;
    if sha256 != expected {
        return Err("migration rollback created file changed".to_string());
    }
    verify_ownership_witness_identity(&step.target_path, &witness, expected)?;
    if migration_databases_reference_path(plan, &step.target_path)? {
        return Err("migration rollback created file is still referenced".to_string());
    }
    let mut barrier = DestructiveFileGuard::acquire(&step.target_path)?;
    barrier.verify_current_path(Some(expected))?;
    barrier.delete()?;
    if step.target_path.exists() {
        return Err("migration rollback file removal was not durable".to_string());
    }
    remove_ownership_witness(&witness, expected)
}

fn verify_session_ownership_witness(
    session: &MigrationSessionApplyEntry,
    witness: &Path,
) -> Result<(), String> {
    verify_ownership_witness_identity(&session.target_path, witness, &session.expected_sha256)
}

fn verify_ownership_witness_identity(
    target: &Path,
    witness: &Path,
    expected_sha256: &str,
) -> Result<(), String> {
    let (_, target_sha256) = stable_file_digest(target)
        .map_err(|_| "migration ownership target is unavailable".to_string())?;
    let (_, witness_sha256) = stable_file_digest(witness)
        .map_err(|_| "migration ownership witness is unavailable".to_string())?;
    if target_sha256 != expected_sha256 || witness_sha256 != expected_sha256 {
        return Err("migration ownership witness checksum changed".to_string());
    }
    match same_regular_file_identity(target, witness) {
        Ok(true) => Ok(()),
        Ok(false) => Err("migration ownership witness identifies a different file".to_string()),
        Err(()) => Err("migration ownership witness identity is unavailable".to_string()),
    }
}

fn remove_ownership_witness_if_present(
    witness: &Path,
    expected_sha256: &str,
) -> Result<(), String> {
    match fs::symlink_metadata(witness) {
        Ok(_) => remove_ownership_witness(witness, expected_sha256),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err("migration ownership witness is unavailable".to_string()),
    }
}

fn remove_ownership_witness(witness: &Path, expected_sha256: &str) -> Result<(), String> {
    let mut barrier = DestructiveFileGuard::acquire(witness)?;
    barrier.verify_current_path(Some(expected_sha256))?;
    barrier.delete()?;
    match fs::symlink_metadata(witness) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        _ => Err("migration ownership witness removal was not durable".to_string()),
    }
}

fn migration_databases_reference_path(
    plan: &MigrationApplyPlan,
    target_path: &Path,
) -> Result<bool, String> {
    let target_key = path_key(target_path);
    for database in &plan.databases {
        quick_check_sqlite(&database.target_path)?;
        let connection = Connection::open_with_flags(
            &database.target_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|_| "failed to inspect migration rollback references".to_string())?;
        if !table_exists(&connection, "threads")? {
            continue;
        }
        let mut statement = connection
            .prepare("SELECT rollout_path FROM threads")
            .map_err(|_| "failed to inspect migration rollback references".to_string())?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|_| "failed to inspect migration rollback references".to_string())?;
        for row in rows {
            let path =
                row.map_err(|_| "failed to inspect migration rollback reference".to_string())?;
            if path_key(Path::new(&path)) == target_key {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn prepare_session_stages(
    report: &MigrationPreflightReport,
    backup: &MigrationBackupManifest,
    staging_root: &Path,
) -> Result<Vec<MigrationSessionApplyEntry>, String> {
    let sessions_root = report.plan.canonical_root.join("sessions");
    let session_staging = staging_root.join("sessions");
    create_safe_directory(&session_staging)?;
    let mut output = Vec::with_capacity(report.plan.sessions.len());
    for (index, session) in report.plan.sessions.iter().enumerate() {
        validate_session_plan(session, &sessions_root)?;
        let staged_path = if matches!(
            session.action,
            MigrationSessionAction::CopyToCanonical
                | MigrationSessionAction::ReplaceCanonicalWithExtension
        ) {
            let retained = backup_entry_for_source(
                backup,
                &session.retained_path,
                MigrationBackupEntryKind::Session,
            )?;
            if retained.sha256 != session.retained_sha256 {
                return Err("migration retained session changed after preflight".to_string());
            }
            let source = backup_payload_path(backup, retained)?;
            let staged = session_staging.join(format!("{index:06}.jsonl.stage"));
            atomic_copy(&source, &staged)?;
            let semantic = read_semantic_session(&staged)
                .map_err(|_| "prepared migration session is invalid".to_string())?;
            let (_, sha256) = stable_file_digest(&staged)?;
            if semantic.thread_id != session.thread_id || sha256 != session.retained_sha256 {
                return Err("prepared migration session identity changed".to_string());
            }
            Some(staged)
        } else {
            None
        };

        let (target_before_sha256, target_backup_payload) = match session.action {
            MigrationSessionAction::CopyToCanonical => {
                if session.canonical_path.exists() {
                    return Err("migration canonical target appeared after preflight".to_string());
                }
                (None, None)
            }
            MigrationSessionAction::ReplaceCanonicalWithExtension => {
                let entry = backup_entry_for_source(
                    backup,
                    &session.canonical_path,
                    MigrationBackupEntryKind::Session,
                )?;
                (
                    Some(entry.sha256.clone()),
                    Some(backup_payload_path(backup, entry)?),
                )
            }
            MigrationSessionAction::KeepCanonical => {
                let entry = backup_entry_for_source(
                    backup,
                    &session.canonical_path,
                    MigrationBackupEntryKind::Session,
                )?;
                if entry.sha256 != session.retained_sha256 {
                    return Err("kept canonical session changed after preflight".to_string());
                }
                (
                    Some(entry.sha256.clone()),
                    Some(backup_payload_path(backup, entry)?),
                )
            }
            MigrationSessionAction::Conflict => (None, None),
        };
        output.push(MigrationSessionApplyEntry {
            thread_id: session.thread_id.clone(),
            action: session.action,
            source_path: session.retained_path.clone(),
            target_path: session.canonical_path.clone(),
            staged_path,
            expected_sha256: session.retained_sha256.clone(),
            target_before_sha256,
            target_backup_payload,
        });
    }
    output.sort_by(|left, right| left.thread_id.cmp(&right.thread_id));
    Ok(output)
}

fn prepare_database_stages(
    report: &MigrationPreflightReport,
    backup: &MigrationBackupManifest,
    staging_root: &Path,
) -> Result<Vec<MigrationDatabaseApplyEntry>, String> {
    let database_staging = staging_root.join("databases");
    create_safe_directory(&database_staging)?;
    let mut output = Vec::with_capacity(report.plan.databases.len());
    let mut goals_groups = BTreeMap::<String, Vec<_>>::new();
    for database in &report.plan.databases {
        if let Some(group) = goals_database_group(&database.database_id) {
            goals_groups
                .entry(group.to_string())
                .or_default()
                .push(database);
        }
    }
    let mut index = 0_usize;
    for database in report
        .plan
        .databases
        .iter()
        .filter(|database| !is_goals_database_id(&database.database_id))
    {
        let entry =
            backup_entry_for_source(backup, &database.path, MigrationBackupEntryKind::Database)?;
        let source = backup_payload_path(backup, entry)?;
        let staged_path = database_staging.join(format!("{index:06}.sqlite.stage"));
        index = index.saturating_add(1);
        atomic_copy(&source, &staged_path)?;
        quick_check_sqlite(&staged_path).map_err(|error| {
            format!(
                "{error} while preparing migration database {}",
                database.database_id
            )
        })?;
        let (staged_bytes, staged_sha256) = stable_file_digest(&staged_path)?;
        output.push(MigrationDatabaseApplyEntry {
            database_id: database.database_id.clone(),
            role: database.role,
            target_path: database.path.clone(),
            staged_path,
            original_backup_payload: source,
            original_sha256: entry.sha256.clone(),
            staged_sha256,
            staged_bytes,
        });
    }
    for (group, mut views) in goals_groups {
        views.sort_by(|left, right| left.database_id.cmp(&right.database_id));
        for view in views {
            let entry =
                backup_entry_for_source(backup, &view.path, MigrationBackupEntryKind::Database)?;
            let source = backup_payload_path(backup, entry)?;
            let staged_path = database_staging.join(format!("{index:06}-{group}.sqlite.stage"));
            index = index.saturating_add(1);
            atomic_copy(&source, &staged_path)?;
            quick_check_sqlite(&staged_path).map_err(|error| {
                format!(
                    "{error} while preparing migration database {}",
                    view.database_id
                )
            })?;
            let (staged_bytes, staged_sha256) = stable_file_digest(&staged_path)?;
            output.push(MigrationDatabaseApplyEntry {
                database_id: view.database_id.clone(),
                role: view.role,
                target_path: view.path.clone(),
                staged_path,
                original_backup_payload: source,
                original_sha256: entry.sha256.clone(),
                staged_sha256,
                staged_bytes,
            });
        }
    }
    output.sort_by(|left, right| left.database_id.cmp(&right.database_id));
    Ok(output)
}

fn is_goals_database_id(database_id: &str) -> bool {
    goals_database_group(database_id).is_some()
}

fn goals_database_group(database_id: &str) -> Option<&str> {
    database_id
        .starts_with(GOALS_DATABASE_PLAN_PREFIX)
        .then_some(())?;
    let (group, view) = database_id.rsplit_once("-view-")?;
    (!group.is_empty() && view.len() == 4 && view.bytes().all(|byte| byte.is_ascii_digit()))
        .then_some(group)
}

#[derive(Debug, Clone)]
struct SourceThreadRow {
    values: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TableColumn {
    name: String,
    declared_type: String,
    not_null: bool,
    default_value: Option<String>,
    primary_key_ordinal: i64,
    hidden: i64,
}

pub(crate) fn merge_database_views(
    sessions: &[MigrationSessionApplyEntry],
    databases: &mut [MigrationDatabaseApplyEntry],
) -> Result<(), String> {
    let active_sessions = sessions
        .iter()
        .filter(|session| session.action != MigrationSessionAction::Conflict)
        .collect::<Vec<_>>();
    if active_sessions.is_empty() {
        return finalize_staged_databases(databases);
    }
    if databases.is_empty() {
        return Err("migration has no runtime database view to repair".to_string());
    }

    let mut source_rows = BTreeMap::<String, SourceThreadRow>::new();
    let mut ordered_indexes = (0..databases.len()).collect::<Vec<_>>();
    ordered_indexes.sort_by_key(|index| {
        (
            database_role_rank(databases[*index].role),
            path_key(&databases[*index].target_path),
        )
    });
    for index in ordered_indexes {
        for (thread_id, values) in read_thread_rows(&databases[index].staged_path)? {
            source_rows
                .entry(thread_id)
                .or_insert(SourceThreadRow { values });
        }
    }
    for session in &active_sessions {
        if !source_rows.contains_key(&session.thread_id) {
            return Err("a canonical migration session has no runtime database row".to_string());
        }
    }

    for database in databases.iter() {
        let mut connection = Connection::open(&database.staged_path)
            .map_err(|_| "failed to open staged migration database".to_string())?;
        connection
            .busy_timeout(std::time::Duration::from_secs(2))
            .map_err(|_| "failed to configure staged migration database".to_string())?;
        let schema = table_schema(&connection, "threads")?;
        if !schema.iter().any(|column| column.name == "id")
            || !schema.iter().any(|column| column.name == "rollout_path")
        {
            return Err("staged migration threads schema is incompatible".to_string());
        }
        let existing = existing_thread_ids(&connection)?;
        let transaction = connection
            .transaction()
            .map_err(|_| "failed to begin staged migration database update".to_string())?;
        for session in &active_sessions {
            if existing.contains(&session.thread_id) {
                update_thread_path(&transaction, &session.thread_id, &session.target_path)?;
            } else {
                let source = source_rows
                    .get(&session.thread_id)
                    .ok_or_else(|| "migration source database row disappeared".to_string())?;
                insert_thread_row(
                    &transaction,
                    &schema,
                    &source.values,
                    session,
                    database.role,
                )?;
            }
        }
        transaction
            .commit()
            .map_err(|_| "failed to commit staged migration database update".to_string())?;
        drop(connection);
        quick_check_sqlite(&database.staged_path)?;
    }

    let dependent_sources = databases
        .iter()
        .map(|database| database.staged_path.clone())
        .collect::<Vec<_>>();
    for session in &active_sessions {
        copy_dependent_rows_for_thread_from_sources(
            &dependent_sources,
            databases,
            &session.thread_id,
        )?;
    }
    finalize_staged_databases(databases)
}

pub(crate) fn merge_goals_database_views(
    databases: &mut [MigrationDatabaseApplyEntry],
) -> Result<(), String> {
    if databases.is_empty() {
        return Err("migration has no goals database view to repair".to_string());
    }
    if databases
        .iter()
        .any(|database| !is_goals_database_id(&database.database_id))
    {
        return Err("migration goals database plan is invalid".to_string());
    }
    let mut participant_paths = databases
        .iter()
        .map(|database| database.staged_path.clone())
        .collect::<Vec<_>>();
    participant_paths.sort_by_key(|path| path_key(path));
    participant_paths.dedup_by(|left, right| path_key(left) == path_key(right));
    if participant_paths.is_empty() {
        return Err("migration has no physical goals database source".to_string());
    }
    let mut expected_schema_digest = None::<String>;
    for source_path in &participant_paths {
        let source = Connection::open_with_flags(
            source_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|_| "failed to open goals database stage".to_string())?;
        let (schema_digest, _, _) = goals_database_digest(&source)?;
        if expected_schema_digest
            .as_ref()
            .is_some_and(|expected| expected != &schema_digest)
        {
            return Err("goals database schemas do not match".to_string());
        }
        expected_schema_digest.get_or_insert(schema_digest);
    }

    let tables = [
        DependentTable::Single("thread_goals", "thread_id"),
        DependentTable::Single("thread_goal_continuation_deferrals", "thread_id"),
    ];
    let unions = tables
        .into_iter()
        .map(|table| gather_required_full_table_union(&participant_paths, table))
        .collect::<Result<Vec<_>, _>>()?;
    let canonical_index = databases
        .iter()
        .enumerate()
        .min_by_key(|(_, database)| {
            (
                database_role_rank(database.role),
                path_key(&database.target_path),
            )
        })
        .map(|(index, _)| index)
        .ok_or_else(|| "migration canonical goals database is missing".to_string())?;
    let canonical_stage = databases[canonical_index].staged_path.clone();
    preflight_full_table_target(&canonical_stage, &unions)?;
    apply_full_table_unions_to_target(&canonical_stage, &unions)?;
    let canonical_connection = Connection::open_with_flags(
        &canonical_stage,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| "failed to reopen merged goals database".to_string())?;
    goals_database_digest(&canonical_connection)?;
    drop(canonical_connection);

    let mut canonical_entry = databases[canonical_index].clone();
    finalize_staged_databases(std::slice::from_mut(&mut canonical_entry))?;
    for (index, database) in databases.iter_mut().enumerate() {
        if index == canonical_index {
            database.staged_path = canonical_entry.staged_path.clone();
        } else {
            let copy_path = canonical_entry
                .staged_path
                .parent()
                .ok_or_else(|| "migration goals staging path has no parent".to_string())?
                .join(format!("goals-view-{index:04}.sqlite.stage"));
            atomic_copy(&canonical_entry.staged_path, &copy_path)?;
            quick_check_sqlite(&copy_path).map_err(|error| {
                format!(
                    "{error} while copying merged goals database {}",
                    database.database_id
                )
            })?;
            remove_sqlite_sidecars(&copy_path)?;
            let (bytes, sha256) = stable_file_digest(&copy_path)?;
            if bytes != canonical_entry.staged_bytes
                || sha256 != canonical_entry.staged_sha256
                || same_regular_file_identity(&canonical_entry.staged_path, &copy_path).map_err(
                    |_| "goals database stage identity could not be verified".to_string(),
                )?
            {
                return Err(
                    "goals database publication stage is not an independent exact copy".to_string(),
                );
            }
            database.staged_path = copy_path;
        }
        database.staged_sha256 = canonical_entry.staged_sha256.clone();
        database.staged_bytes = canonical_entry.staged_bytes;
    }
    Ok(())
}

fn gather_required_full_table_union(
    source_paths: &[PathBuf],
    table: DependentTable,
) -> Result<DependentTableUnion, String> {
    let mut expected_schema = None::<Vec<TableColumn>>;
    let mut primary_key_indexes = Vec::new();
    let mut rows = BTreeMap::<Vec<u8>, Vec<Value>>::new();
    let mut primary_keys = BTreeMap::<Vec<u8>, Vec<u8>>::new();
    for source_path in source_paths {
        let source = Connection::open_with_flags(
            source_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|_| "failed to open goals union source".to_string())?;
        if !table_exists(&source, table.name())? {
            return Err(format!(
                "goals database is missing required table {}",
                table.name()
            ));
        }
        if table_has_triggers(&source, table.name())? {
            return Err("goals database table has unsupported triggers".to_string());
        }
        let schema = table_schema(&source, table.name())?;
        let indexes = validate_dependent_schema(&schema, table)?;
        if let Some(expected) = &expected_schema {
            if expected != &schema || primary_key_indexes != indexes {
                return Err("goals database schemas do not match".to_string());
            }
        } else {
            primary_key_indexes = indexes;
            expected_schema = Some(schema.clone());
        }
        let source_rows = read_dependent_rows(&source, table, &schema, None)?;
        validate_dependent_row_primary_keys(&source_rows, &primary_key_indexes)?;
        for row in source_rows {
            let row_key = encode_dependent_values(&row)?;
            let primary_key = dependent_primary_key(&row, &primary_key_indexes)?;
            if primary_keys
                .get(&primary_key)
                .is_some_and(|existing| existing != &row_key)
            {
                return Err("goals database rows conflict on the same primary key".to_string());
            }
            primary_keys
                .entry(primary_key)
                .or_insert_with(|| row_key.clone());
            rows.entry(row_key).or_insert(row);
        }
    }
    Ok(DependentTableUnion {
        table,
        schema: expected_schema,
        primary_key_indexes,
        rows,
    })
}

fn preflight_full_table_target(
    target_path: &Path,
    unions: &[DependentTableUnion],
) -> Result<(), String> {
    let target = Connection::open_with_flags(
        target_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| "failed to open goals union target".to_string())?;
    for union in unions {
        let schema = union
            .schema
            .as_ref()
            .ok_or_else(|| "goals union schema is missing".to_string())?;
        verify_dependent_target_schema(&target, union, schema)?;
        let current = read_dependent_rows(&target, union.table, schema, None)?;
        validate_dependent_row_primary_keys(&current, &union.primary_key_indexes)?;
        ensure_dependent_union_primary_keys(&current, union)?;
        if current
            .iter()
            .map(|values| encode_dependent_values(values))
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .any(|row| !union.rows.contains_key(row))
        {
            return Err("goals union target changed during planning".to_string());
        }
    }
    Ok(())
}

fn apply_full_table_unions_to_target(
    target_path: &Path,
    unions: &[DependentTableUnion],
) -> Result<(), String> {
    let mut target = Connection::open(target_path)
        .map_err(|_| "failed to open goals union target".to_string())?;
    target
        .busy_timeout(std::time::Duration::from_secs(2))
        .map_err(|_| "failed to configure goals union target".to_string())?;
    let transaction = target
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| "failed to begin goals database union".to_string())?;
    for union in unions {
        let schema = union
            .schema
            .as_ref()
            .ok_or_else(|| "goals union schema is missing".to_string())?;
        verify_dependent_target_schema(&transaction, union, schema)?;
        let before = read_dependent_rows(&transaction, union.table, schema, None)?;
        validate_dependent_row_primary_keys(&before, &union.primary_key_indexes)?;
        ensure_dependent_union_primary_keys(&before, union)?;
        let before = dependent_row_multiset(&before)?;
        let columns = schema
            .iter()
            .map(|column| quote_identifier(&column.name))
            .collect::<Vec<_>>();
        let insert = format!(
            "INSERT INTO {} ({}) VALUES ({})",
            quote_identifier(union.table.name()),
            columns.join(", "),
            (1..=columns.len())
                .map(|index| format!("?{index}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        for (row_key, row) in &union.rows {
            if before.contains_key(row_key) {
                continue;
            }
            if transaction
                .execute(&insert, rusqlite::params_from_iter(row.iter()))
                .map_err(|_| "failed to insert goals union row".to_string())?
                != 1
            {
                return Err("goals union row insert was not exact".to_string());
            }
        }
        let after = dependent_row_multiset(&read_dependent_rows(
            &transaction,
            union.table,
            schema,
            None,
        )?)?;
        let expected = union
            .rows
            .keys()
            .cloned()
            .map(|row| (row, 1_u64))
            .collect::<BTreeMap<_, _>>();
        if after != expected {
            return Err("goals database union readback was not exact".to_string());
        }
    }
    transaction
        .commit()
        .map_err(|_| "failed to commit goals database union".to_string())
}

pub(crate) fn merge_restore_import_database_views(
    sessions: &[MigrationSessionApplyEntry],
    databases: &mut [MigrationDatabaseApplyEntry],
    source_databases: &[PathBuf],
    synthetic_thread_ids: &BTreeSet<String>,
) -> Result<(), String> {
    if sessions.is_empty() {
        return finalize_staged_databases(databases);
    }
    if databases.is_empty() || source_databases.is_empty() {
        return Err("restore import requires source and runtime database views".to_string());
    }
    let session_thread_ids = sessions
        .iter()
        .map(|session| session.thread_id.as_str())
        .collect::<BTreeSet<_>>();
    if synthetic_thread_ids
        .iter()
        .any(|thread_id| !session_thread_ids.contains(thread_id.as_str()))
    {
        return Err("restore import synthetic thread set is not plan-bound".to_string());
    }

    let mut source_rows = BTreeMap::<String, BTreeMap<String, Value>>::new();
    for source_path in source_databases {
        for (thread_id, values) in read_thread_rows(source_path)? {
            source_rows.entry(thread_id).or_insert(values);
        }
    }
    for session in sessions {
        if !source_rows.contains_key(&session.thread_id) {
            if synthetic_thread_ids.contains(&session.thread_id) {
                // A unique semantic orphan explicitly classified by the restore
                // planner may synthesize only its database view. Unsupported
                // required columns still fail closed in `insert_thread_row`.
                source_rows.insert(session.thread_id.clone(), BTreeMap::new());
            } else {
                return Err("a restore import session has no downgrade database row".to_string());
            }
        }
    }

    for database in databases.iter() {
        let mut connection = Connection::open(&database.staged_path)
            .map_err(|_| "failed to open staged restore import database".to_string())?;
        connection
            .busy_timeout(std::time::Duration::from_secs(2))
            .map_err(|_| "failed to configure staged restore import database".to_string())?;
        let schema = table_schema(&connection, "threads")?;
        if !schema.iter().any(|column| column.name == "id")
            || !schema.iter().any(|column| column.name == "rollout_path")
        {
            return Err("staged restore import threads schema is incompatible".to_string());
        }
        let existing = existing_thread_ids(&connection)?;
        let transaction = connection
            .transaction()
            .map_err(|_| "failed to begin staged restore import database update".to_string())?;
        for session in sessions {
            if existing.contains(&session.thread_id) {
                update_thread_path(&transaction, &session.thread_id, &session.target_path)?;
            } else {
                let source = source_rows
                    .get(&session.thread_id)
                    .ok_or_else(|| "restore import source database row disappeared".to_string())?;
                insert_thread_row(&transaction, &schema, source, session, database.role)?;
            }
        }
        transaction
            .commit()
            .map_err(|_| "failed to commit staged restore import database update".to_string())?;
        drop(connection);
        quick_check_sqlite(&database.staged_path).map_err(|error| {
            format!(
                "{error} while finalizing migration database {}",
                database.database_id
            )
        })?;
    }

    let mut dependent_sources = source_databases.to_vec();
    dependent_sources.extend(
        databases
            .iter()
            .map(|database| database.staged_path.clone()),
    );
    dependent_sources.sort_by_key(|path| path_key(path));
    dependent_sources.dedup_by(|left, right| path_key(left) == path_key(right));
    for session in sessions {
        copy_dependent_rows_for_thread_from_sources(
            &dependent_sources,
            databases,
            &session.thread_id,
        )?;
    }
    finalize_staged_databases(databases)
}

fn read_thread_rows(path: &Path) -> Result<BTreeMap<String, BTreeMap<String, Value>>, String> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| "failed to open staged migration database".to_string())?;
    if !table_exists(&connection, "threads")? {
        return Err("staged migration database has no threads table".to_string());
    }
    let columns = table_schema(&connection, "threads")?
        .into_iter()
        .map(|column| column.name)
        .collect::<Vec<_>>();
    let id_index = columns
        .iter()
        .position(|column| column == "id")
        .ok_or_else(|| "staged migration threads schema has no id".to_string())?;
    let select = format!(
        "SELECT {} FROM {}",
        columns
            .iter()
            .map(|column| quote_identifier(column))
            .collect::<Vec<_>>()
            .join(", "),
        quote_identifier("threads")
    );
    let mut statement = connection
        .prepare(&select)
        .map_err(|_| "failed to query staged migration threads".to_string())?;
    let rows = statement
        .query_map([], |row| {
            let mut values = BTreeMap::new();
            for (index, column) in columns.iter().enumerate() {
                values.insert(column.clone(), row.get::<usize, Value>(index)?);
            }
            let id = match row.get::<usize, Value>(id_index)? {
                Value::Text(value) => value,
                _ => String::new(),
            };
            Ok((id, values))
        })
        .map_err(|_| "failed to query staged migration threads".to_string())?;
    let mut output = BTreeMap::new();
    for row in rows {
        let (id, values) = row.map_err(|_| "failed to read staged migration thread".to_string())?;
        if !id.is_empty() {
            output.insert(id, values);
        }
    }
    Ok(output)
}

fn existing_thread_ids(connection: &Connection) -> Result<BTreeSet<String>, String> {
    let mut statement = connection
        .prepare("SELECT id FROM threads")
        .map_err(|_| "failed to query staged migration thread identities".to_string())?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|_| "failed to query staged migration thread identities".to_string())?;
    let mut output = BTreeSet::new();
    for row in rows {
        output.insert(
            row.map_err(|_| "failed to read staged migration thread identity".to_string())?,
        );
    }
    Ok(output)
}

fn update_thread_path(
    transaction: &Transaction<'_>,
    thread_id: &str,
    rollout_path: &Path,
) -> Result<(), String> {
    let updated = transaction
        .execute(
            "UPDATE threads SET rollout_path = ?1 WHERE id = ?2",
            (rollout_path.to_string_lossy().to_string(), thread_id),
        )
        .map_err(|_| "failed to update staged migration thread path".to_string())?;
    if updated == 1 {
        Ok(())
    } else {
        Err("staged migration thread path update was not exact".to_string())
    }
}

fn insert_thread_row(
    transaction: &Transaction<'_>,
    schema: &[TableColumn],
    source: &BTreeMap<String, Value>,
    session: &MigrationSessionApplyEntry,
    role: DatabaseRole,
) -> Result<(), String> {
    let mut columns = Vec::new();
    let mut values = Vec::new();
    for column in schema {
        let value = if column.name == "id" {
            Value::Text(session.thread_id.clone())
        } else if column.name == "rollout_path" {
            Value::Text(session.target_path.to_string_lossy().to_string())
        } else if column.name == "model_provider" {
            role_provider(role)
                .map(|provider| Value::Text(provider.to_string()))
                .or_else(|| source.get(&column.name).cloned())
                .unwrap_or_else(|| Value::Text("openai".to_string()))
        } else if let Some(value) = source.get(&column.name) {
            value.clone()
        } else if column.default_value.is_some() {
            continue;
        } else if !column.not_null {
            Value::Null
        } else {
            return Err(
                "staged migration target schema has an unsupported required column".to_string(),
            );
        };
        columns.push(column.name.clone());
        values.push(value);
    }
    let sql = format!(
        "INSERT INTO {} ({}) VALUES ({})",
        quote_identifier("threads"),
        columns
            .iter()
            .map(|column| quote_identifier(column))
            .collect::<Vec<_>>()
            .join(", "),
        (1..=values.len())
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let inserted = transaction
        .execute(&sql, rusqlite::params_from_iter(values))
        .map_err(|_| "failed to insert staged migration thread".to_string())?;
    if inserted == 1 {
        Ok(())
    } else {
        Err("staged migration thread insert was not exact".to_string())
    }
}

fn copy_dependent_rows_for_thread_from_sources(
    source_paths: &[PathBuf],
    databases: &[MigrationDatabaseApplyEntry],
    thread_id: &str,
) -> Result<(), String> {
    let tables = [
        DependentTable::Single("thread_dynamic_tools", "thread_id"),
        DependentTable::Either("thread_spawn_edges", "parent_thread_id", "child_thread_id"),
    ];
    let target_keys = databases
        .iter()
        .map(|database| path_key(&database.staged_path))
        .collect::<BTreeSet<_>>();
    for source_path in source_paths {
        if target_keys.contains(&path_key(source_path)) {
            continue;
        }
        let source = Connection::open_with_flags(
            source_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|_| "failed to open migration dependent-row source".to_string())?;
        for table in tables {
            if table_exists(&source, table.name())? && table_has_triggers(&source, table.name())? {
                return Err("migration dependent-row table has unsupported triggers".to_string());
            }
        }
    }
    let mut participant_paths = source_paths.to_vec();
    participant_paths.extend(
        databases
            .iter()
            .map(|database| database.staged_path.clone()),
    );
    participant_paths.sort_by_key(|path| path_key(path));
    participant_paths.dedup_by(|left, right| path_key(left) == path_key(right));

    // Gather every participant before opening any target for mutation. This
    // prevents pairwise copy order from deciding which dependent rows survive.
    let unions = tables
        .into_iter()
        .map(|table| gather_dependent_table_union(&participant_paths, table, thread_id))
        .collect::<Result<Vec<_>, _>>()?;

    let mut target_paths = databases
        .iter()
        .map(|database| database.staged_path.clone())
        .collect::<Vec<_>>();
    target_paths.sort_by_key(|path| path_key(path));
    target_paths.dedup_by(|left, right| path_key(left) == path_key(right));

    // Preflight all targets before committing the first independent target
    // transaction. The transaction itself repeats the checks to close the
    // gather-to-write race on the staged database.
    for target_path in &target_paths {
        preflight_dependent_target(target_path, &unions, thread_id)?;
    }
    for target_path in target_paths {
        apply_dependent_unions_to_target(&target_path, &unions, thread_id)?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum DependentTable {
    Single(&'static str, &'static str),
    Either(&'static str, &'static str, &'static str),
}

impl DependentTable {
    fn name(self) -> &'static str {
        match self {
            Self::Single(table, _) | Self::Either(table, _, _) => table,
        }
    }

    fn filter_columns(self) -> Vec<&'static str> {
        match self {
            Self::Single(_, column) => vec![column],
            Self::Either(_, left, right) => vec![left, right],
        }
    }
}

#[derive(Debug, Clone)]
struct DependentTableUnion {
    table: DependentTable,
    schema: Option<Vec<TableColumn>>,
    primary_key_indexes: Vec<usize>,
    rows: BTreeMap<Vec<u8>, Vec<Value>>,
}

fn gather_dependent_table_union(
    source_paths: &[PathBuf],
    table: DependentTable,
    thread_id: &str,
) -> Result<DependentTableUnion, String> {
    let table_name = table.name();
    let mut expected_schema = None::<Vec<TableColumn>>;
    let mut primary_key_indexes = Vec::new();
    let mut union_rows = BTreeMap::<Vec<u8>, Vec<Value>>::new();
    let mut primary_keys = BTreeMap::<Vec<u8>, Vec<u8>>::new();

    for source_path in source_paths {
        let source = Connection::open_with_flags(
            source_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|_| "failed to open migration dependent-row source".to_string())?;
        if !table_exists(&source, table_name)? {
            continue;
        }
        if table_has_triggers(&source, table_name)? {
            return Err("migration dependent-row table has unsupported triggers".to_string());
        }
        let schema = table_schema(&source, table_name)?;
        let source_primary_key_indexes = validate_dependent_schema(&schema, table)?;
        if let Some(expected) = &expected_schema {
            if expected != &schema {
                return Err("migration dependent-row schemas do not match".to_string());
            }
        } else {
            primary_key_indexes = source_primary_key_indexes;
            expected_schema = Some(schema.clone());
        }

        let rows = read_dependent_rows(&source, table, &schema, Some(thread_id))?;
        let mut source_rows = BTreeSet::new();
        for row in rows {
            let row_key = encode_dependent_values(&row)?;
            if !source_rows.insert(row_key.clone()) {
                return Err("migration dependent-row source contains duplicate rows".to_string());
            }
            let primary_key = dependent_primary_key(&row, &primary_key_indexes)?;
            if let Some(existing_row_key) = primary_keys.get(&primary_key) {
                if existing_row_key != &row_key {
                    return Err(
                        "migration dependent rows conflict on the same primary key".to_string()
                    );
                }
            } else {
                primary_keys.insert(primary_key, row_key.clone());
            }
            union_rows.entry(row_key).or_insert(row);
        }
    }

    Ok(DependentTableUnion {
        table,
        schema: expected_schema,
        primary_key_indexes,
        rows: union_rows,
    })
}

fn preflight_dependent_target(
    target_path: &Path,
    unions: &[DependentTableUnion],
    thread_id: &str,
) -> Result<(), String> {
    let target = Connection::open_with_flags(
        target_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| "failed to open migration dependent-row target".to_string())?;
    for union in unions {
        let Some(expected_schema) = &union.schema else {
            if table_exists(&target, union.table.name())? {
                return Err("migration dependent-row schema changed during union".to_string());
            }
            continue;
        };
        verify_dependent_target_schema(&target, union, expected_schema)?;
        let all = read_dependent_rows(&target, union.table, expected_schema, None)?;
        validate_dependent_row_primary_keys(&all, &union.primary_key_indexes)?;
        ensure_dependent_union_primary_keys(&all, union)?;
        let current = read_dependent_rows(&target, union.table, expected_schema, Some(thread_id))?;
        validate_dependent_row_primary_keys(&current, &union.primary_key_indexes)?;
        let current = dependent_row_multiset(&current)?;
        ensure_dependent_rows_unique(&current)?;
        if current.keys().any(|row| !union.rows.contains_key(row)) {
            return Err("migration dependent-row target changed during union".to_string());
        }
    }
    Ok(())
}

fn apply_dependent_unions_to_target(
    target_path: &Path,
    unions: &[DependentTableUnion],
    thread_id: &str,
) -> Result<(), String> {
    let mut target = Connection::open(target_path)
        .map_err(|_| "failed to open migration dependent-row target".to_string())?;
    target
        .busy_timeout(std::time::Duration::from_secs(2))
        .map_err(|_| "failed to configure migration dependent-row target".to_string())?;
    let transaction = target
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| "failed to begin migration dependent-row union".to_string())?;
    for union in unions {
        apply_dependent_union_in_transaction(&transaction, union, thread_id)?;
    }
    transaction
        .commit()
        .map_err(|_| "failed to commit migration dependent-row union".to_string())
}

fn apply_dependent_union_in_transaction(
    transaction: &Transaction<'_>,
    union: &DependentTableUnion,
    thread_id: &str,
) -> Result<(), String> {
    let Some(expected_schema) = &union.schema else {
        if table_exists(transaction, union.table.name())? {
            return Err("migration dependent-row schema changed during union".to_string());
        }
        return Ok(());
    };
    verify_dependent_target_schema(transaction, union, expected_schema)?;

    let all_before = read_dependent_rows(transaction, union.table, expected_schema, None)?;
    let matching_before =
        read_dependent_rows(transaction, union.table, expected_schema, Some(thread_id))?;
    validate_dependent_row_primary_keys(&all_before, &union.primary_key_indexes)?;
    ensure_dependent_union_primary_keys(&all_before, union)?;
    let all_before = dependent_row_multiset(&all_before)?;
    let matching_before = dependent_row_multiset(&matching_before)?;
    ensure_dependent_rows_unique(&all_before)?;
    ensure_dependent_rows_unique(&matching_before)?;
    if matching_before
        .keys()
        .any(|row| !union.rows.contains_key(row))
    {
        return Err("migration dependent-row target changed during union".to_string());
    }
    let unrelated_before = subtract_dependent_multisets(&all_before, &matching_before)?;

    let columns = expected_schema
        .iter()
        .map(|column| quote_identifier(&column.name))
        .collect::<Vec<_>>();
    let insert = format!(
        "INSERT INTO {} ({}) VALUES ({})",
        quote_identifier(union.table.name()),
        columns.join(", "),
        (1..=columns.len())
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    for (row_key, row) in &union.rows {
        if matching_before.contains_key(row_key) {
            continue;
        }
        let inserted = transaction
            .execute(&insert, rusqlite::params_from_iter(row.iter()))
            .map_err(|_| "failed to insert migration dependent row".to_string())?;
        if inserted != 1 {
            return Err("migration dependent-row insert was not exact".to_string());
        }
    }

    let all_after = read_dependent_rows(transaction, union.table, expected_schema, None)?;
    let matching_after =
        read_dependent_rows(transaction, union.table, expected_schema, Some(thread_id))?;
    validate_dependent_row_primary_keys(&all_after, &union.primary_key_indexes)?;
    let all_after = dependent_row_multiset(&all_after)?;
    let matching_after = dependent_row_multiset(&matching_after)?;
    ensure_dependent_rows_unique(&all_after)?;
    ensure_dependent_rows_unique(&matching_after)?;
    let expected = union
        .rows
        .keys()
        .cloned()
        .map(|row| (row, 1_u64))
        .collect::<BTreeMap<_, _>>();
    if matching_after != expected {
        return Err("migration dependent-row union readback was not exact".to_string());
    }
    let unrelated_after = subtract_dependent_multisets(&all_after, &matching_after)?;
    if unrelated_after != unrelated_before {
        return Err("migration dependent-row union changed unrelated rows".to_string());
    }
    Ok(())
}

fn verify_dependent_target_schema(
    connection: &Connection,
    union: &DependentTableUnion,
    expected_schema: &[TableColumn],
) -> Result<(), String> {
    let table_name = union.table.name();
    if !table_exists(connection, table_name)? {
        return Err("migration dependent-row target table is missing".to_string());
    }
    if table_has_triggers(connection, table_name)? {
        return Err("migration dependent-row table has unsupported triggers".to_string());
    }
    let target_schema = table_schema(connection, table_name)?;
    let target_primary_key_indexes = validate_dependent_schema(&target_schema, union.table)?;
    if target_schema != expected_schema || target_primary_key_indexes != union.primary_key_indexes {
        return Err("migration dependent-row schemas do not match".to_string());
    }
    Ok(())
}

fn validate_dependent_schema(
    schema: &[TableColumn],
    table: DependentTable,
) -> Result<Vec<usize>, String> {
    if schema.is_empty() || schema.iter().any(|column| column.hidden != 0) {
        return Err("migration dependent-row schema is not safely writable".to_string());
    }
    let mut column_names = BTreeSet::new();
    for column in schema {
        if !column_names.insert(column.name.to_ascii_lowercase()) {
            return Err("migration dependent-row schema has duplicate columns".to_string());
        }
    }
    if table
        .filter_columns()
        .iter()
        .any(|filter| !schema.iter().any(|column| column.name == *filter))
    {
        return Err("migration dependent-row schema is incompatible".to_string());
    }
    let mut primary_key = schema
        .iter()
        .enumerate()
        .filter(|(_, column)| column.primary_key_ordinal > 0)
        .map(|(index, column)| (column.primary_key_ordinal, index))
        .collect::<Vec<_>>();
    primary_key.sort_by_key(|(ordinal, _)| *ordinal);
    if primary_key.is_empty()
        || primary_key
            .iter()
            .enumerate()
            .any(|(index, (ordinal, _))| *ordinal != i64::try_from(index + 1).unwrap_or(i64::MAX))
    {
        return Err("migration dependent-row schema has no reliable primary key".to_string());
    }
    Ok(primary_key.into_iter().map(|(_, index)| index).collect())
}

fn read_dependent_rows(
    connection: &Connection,
    table: DependentTable,
    schema: &[TableColumn],
    thread_id: Option<&str>,
) -> Result<Vec<Vec<Value>>, String> {
    let filters = table.filter_columns();
    let columns = schema
        .iter()
        .map(|column| quote_identifier(&column.name))
        .collect::<Vec<_>>();
    let mut select = format!(
        "SELECT {} FROM {}",
        columns.join(", "),
        quote_identifier(table.name())
    );
    let parameters = if let Some(thread_id) = thread_id {
        let where_clause = filters
            .iter()
            .enumerate()
            .map(|(index, column)| format!("{} = ?{}", quote_identifier(column), index + 1))
            .collect::<Vec<_>>()
            .join(" OR ");
        select.push_str(&format!(" WHERE {where_clause}"));
        (0..filters.len())
            .map(|_| Value::Text(thread_id.to_string()))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let filter_indexes = filters
        .iter()
        .map(|filter| {
            schema
                .iter()
                .position(|column| column.name == *filter)
                .ok_or_else(|| "migration dependent-row schema is incompatible".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut statement = connection
        .prepare(&select)
        .map_err(|_| "failed to query migration dependent rows".to_string())?;
    let rows = statement
        .query_map(rusqlite::params_from_iter(parameters), |row| {
            let mut values = Vec::with_capacity(schema.len());
            for index in 0..schema.len() {
                values.push(row.get::<usize, Value>(index)?);
            }
            Ok(values)
        })
        .map_err(|_| "failed to query migration dependent rows".to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "failed to read migration dependent rows".to_string())?;
    if rows.len() > MAX_DEPENDENT_ROWS_PER_READ {
        return Err("migration dependent-row table reached its safety limit".to_string());
    }
    if let Some(thread_id) = thread_id {
        let expected = Value::Text(thread_id.to_string());
        if rows.iter().any(|row| {
            !filter_indexes
                .iter()
                .any(|index| row.get(*index) == Some(&expected))
        }) {
            return Err("migration dependent-row filter matched an inexact value".to_string());
        }
    }
    Ok(rows)
}

fn dependent_primary_key(row: &[Value], indexes: &[usize]) -> Result<Vec<u8>, String> {
    let mut values = Vec::with_capacity(indexes.len());
    for index in indexes {
        let value = row
            .get(*index)
            .ok_or_else(|| "migration dependent-row primary key is invalid".to_string())?;
        if matches!(value, Value::Null) {
            return Err("migration dependent-row primary key contains NULL".to_string());
        }
        values.push(value.clone());
    }
    encode_dependent_values(&values)
}

fn validate_dependent_row_primary_keys(
    rows: &[Vec<Value>],
    indexes: &[usize],
) -> Result<(), String> {
    let mut primary_keys = BTreeMap::<Vec<u8>, Vec<u8>>::new();
    for row in rows {
        let primary_key = dependent_primary_key(row, indexes)?;
        let row_key = encode_dependent_values(row)?;
        if let Some(existing) = primary_keys.insert(primary_key, row_key.clone()) {
            if existing != row_key {
                return Err("migration dependent rows conflict on the same primary key".to_string());
            }
            return Err("migration dependent-row table contains duplicate rows".to_string());
        }
    }
    Ok(())
}

fn ensure_dependent_union_primary_keys(
    existing_rows: &[Vec<Value>],
    union: &DependentTableUnion,
) -> Result<(), String> {
    let mut existing = BTreeMap::<Vec<u8>, Vec<u8>>::new();
    for row in existing_rows {
        existing.insert(
            dependent_primary_key(row, &union.primary_key_indexes)?,
            encode_dependent_values(row)?,
        );
    }
    for (row_key, row) in &union.rows {
        let primary_key = dependent_primary_key(row, &union.primary_key_indexes)?;
        if existing
            .get(&primary_key)
            .is_some_and(|existing_row| existing_row != row_key)
        {
            return Err("migration dependent rows conflict on the same primary key".to_string());
        }
    }
    Ok(())
}

fn dependent_row_multiset(rows: &[Vec<Value>]) -> Result<BTreeMap<Vec<u8>, u64>, String> {
    let mut output = BTreeMap::new();
    for row in rows {
        let key = encode_dependent_values(row)?;
        let count = output.entry(key).or_insert(0_u64);
        *count = count
            .checked_add(1)
            .ok_or_else(|| "migration dependent-row count overflowed".to_string())?;
    }
    Ok(output)
}

fn ensure_dependent_rows_unique(rows: &BTreeMap<Vec<u8>, u64>) -> Result<(), String> {
    if rows.values().any(|count| *count != 1) {
        Err("migration dependent-row table contains duplicate rows".to_string())
    } else {
        Ok(())
    }
}

fn subtract_dependent_multisets(
    all: &BTreeMap<Vec<u8>, u64>,
    selected: &BTreeMap<Vec<u8>, u64>,
) -> Result<BTreeMap<Vec<u8>, u64>, String> {
    let mut output = all.clone();
    for (row, selected_count) in selected {
        let Some(all_count) = output.get_mut(row) else {
            return Err("migration dependent-row readback is inconsistent".to_string());
        };
        if *all_count < *selected_count {
            return Err("migration dependent-row readback is inconsistent".to_string());
        }
        *all_count -= *selected_count;
        if *all_count == 0 {
            output.remove(row);
        }
    }
    Ok(output)
}

fn encode_dependent_values(values: &[Value]) -> Result<Vec<u8>, String> {
    let mut output = Vec::new();
    append_dependent_length(&mut output, values.len())?;
    for value in values {
        match value {
            Value::Null => output.push(0),
            Value::Integer(value) => {
                output.push(1);
                output.extend_from_slice(&value.to_le_bytes());
            }
            Value::Real(value) if value.is_nan() => {
                return Err("migration dependent-row REAL value is NaN".to_string());
            }
            Value::Real(value) => {
                output.push(2);
                output.extend_from_slice(&value.to_bits().to_le_bytes());
            }
            Value::Text(value) => {
                output.push(3);
                append_dependent_length(&mut output, value.len())?;
                output.extend_from_slice(value.as_bytes());
            }
            Value::Blob(value) => {
                output.push(4);
                append_dependent_length(&mut output, value.len())?;
                output.extend_from_slice(value);
            }
        }
    }
    Ok(output)
}

fn append_dependent_length(output: &mut Vec<u8>, length: usize) -> Result<(), String> {
    let length = u64::try_from(length)
        .map_err(|_| "migration dependent-row value is too large".to_string())?;
    output.extend_from_slice(&length.to_le_bytes());
    Ok(())
}

fn table_has_triggers(connection: &Connection, table: &str) -> Result<bool, String> {
    let count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'trigger' AND tbl_name = ?1",
            [table],
            |row| row.get(0),
        )
        .map_err(|_| "failed to inspect migration dependent-row triggers".to_string())?;
    Ok(count != 0)
}

fn finalize_staged_databases(databases: &mut [MigrationDatabaseApplyEntry]) -> Result<(), String> {
    for database in databases {
        let connection = Connection::open(&database.staged_path)
            .map_err(|_| "failed to finalize staged migration database".to_string())?;
        connection
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .map_err(|_| "failed to checkpoint staged migration database".to_string())?;
        let journal_mode: String = connection
            .query_row("PRAGMA journal_mode=DELETE", [], |row| row.get(0))
            .map_err(|_| "failed to finalize staged migration database journal".to_string())?;
        if !journal_mode.eq_ignore_ascii_case("delete") {
            return Err("staged migration database journal mode is unsafe".to_string());
        }
        drop(connection);
        quick_check_sqlite(&database.staged_path)?;
        remove_sqlite_sidecars(&database.staged_path)?;
        let (bytes, sha256) = stable_file_digest(&database.staged_path)?;
        database.staged_bytes = bytes;
        database.staged_sha256 = sha256;
    }
    Ok(())
}

fn role_provider(role: DatabaseRole) -> Option<&'static str> {
    match role {
        DatabaseRole::CanonicalAccount | DatabaseRole::AccountView => Some("openai"),
        DatabaseRole::Relay => Some("openai_custom"),
        DatabaseRole::Shared
        | DatabaseRole::LegacyOrRelocated
        | DatabaseRole::UnknownRuntime
        | DatabaseRole::Backup
        | DatabaseRole::RecoveryPackage
        | DatabaseRole::DowngradeExport => None,
    }
}

fn database_role_rank(role: DatabaseRole) -> u8 {
    match role {
        DatabaseRole::CanonicalAccount => 0,
        DatabaseRole::AccountView => 1,
        DatabaseRole::Relay => 2,
        DatabaseRole::Shared => 3,
        DatabaseRole::LegacyOrRelocated => 4,
        DatabaseRole::UnknownRuntime => 5,
        DatabaseRole::Backup => 6,
        DatabaseRole::RecoveryPackage => 7,
        DatabaseRole::DowngradeExport => 8,
    }
}

fn rollback_steps(plan: &MigrationApplyPlan) -> Vec<LedgerRollbackStep> {
    let mut steps = plan
        .databases
        .iter()
        .map(|database| LedgerRollbackStep {
            action: RollbackActionKind::RestoreDatabase,
            source_path: database.original_backup_payload.clone(),
            target_path: database.target_path.clone(),
            expected_sha256: Some(database.original_sha256.clone()),
            applied_sha256: Some(database.staged_sha256.clone()),
            completed: false,
        })
        .collect::<Vec<_>>();
    for session in &plan.sessions {
        match session.action {
            MigrationSessionAction::CopyToCanonical => steps.push(LedgerRollbackStep {
                action: RollbackActionKind::RemoveCreatedFile,
                source_path: session.target_path.clone(),
                target_path: session.target_path.clone(),
                expected_sha256: Some(session.expected_sha256.clone()),
                applied_sha256: Some(session.expected_sha256.clone()),
                completed: false,
            }),
            MigrationSessionAction::ReplaceCanonicalWithExtension => {
                if let (Some(source_path), Some(expected_sha256)) = (
                    session.target_backup_payload.clone(),
                    session.target_before_sha256.clone(),
                ) {
                    steps.push(LedgerRollbackStep {
                        action: RollbackActionKind::RestoreFile,
                        source_path,
                        target_path: session.target_path.clone(),
                        expected_sha256: Some(expected_sha256),
                        applied_sha256: Some(session.expected_sha256.clone()),
                        completed: false,
                    });
                }
            }
            MigrationSessionAction::KeepCanonical | MigrationSessionAction::Conflict => {}
        }
    }
    steps
}

fn rollback_step_plan_matches(
    actual: &[LedgerRollbackStep],
    expected: &[LedgerRollbackStep],
) -> bool {
    actual.len() == expected.len()
        && actual.iter().zip(expected).all(|(actual, expected)| {
            actual.action == expected.action
                && actual.source_path == expected.source_path
                && actual.target_path == expected.target_path
                && actual.expected_sha256 == expected.expected_sha256
                && actual.applied_sha256 == expected.applied_sha256
        })
}

fn validate_prepared_plan(
    plan: &MigrationApplyPlan,
    report: &MigrationPreflightReport,
    backup: &MigrationBackupManifest,
) -> Result<(), String> {
    validate_plan_shape(plan)?;
    if plan.operation_id != report.operation_id
        || plan.canonical_root != report.plan.canonical_root
        || plan.inventory_fingerprint != report.plan.inventory_fingerprint
        || plan.backup_dir != backup.backup_dir
        || plan.conflict_count != report.plan.conflicts.len()
        || plan.sessions.len() != report.plan.sessions.len()
        || plan.databases.len() != report.plan.databases.len()
    {
        return Err("prepared migration plan does not match preflight".to_string());
    }
    for (prepared, expected) in plan.sessions.iter().zip(&report.plan.sessions) {
        if prepared.thread_id != expected.thread_id
            || prepared.action != expected.action
            || prepared.source_path != expected.retained_path
            || prepared.target_path != expected.canonical_path
            || prepared.expected_sha256 != expected.retained_sha256
        {
            return Err("prepared migration session plan changed".to_string());
        }
        if let Some(staged) = &prepared.staged_path {
            let semantic = read_semantic_session(staged)
                .map_err(|_| "prepared migration session is invalid".to_string())?;
            let (_, sha256) = stable_file_digest(staged)?;
            if semantic.thread_id != prepared.thread_id || sha256 != prepared.expected_sha256 {
                return Err("prepared migration session checksum changed".to_string());
            }
        }
    }
    for (prepared, expected) in plan.databases.iter().zip(&report.plan.databases) {
        if prepared.database_id != expected.database_id
            || prepared.role != expected.role
            || prepared.target_path != expected.path
        {
            return Err("prepared migration database plan changed".to_string());
        }
        quick_check_sqlite(&prepared.staged_path)?;
        let (bytes, sha256) = stable_file_digest(&prepared.staged_path)?;
        if bytes != prepared.staged_bytes || sha256 != prepared.staged_sha256 {
            return Err("prepared migration database checksum changed".to_string());
        }
    }
    Ok(())
}

fn validate_plan_shape(plan: &MigrationApplyPlan) -> Result<(), String> {
    if plan.schema_version != APPLY_PLAN_SCHEMA_VERSION {
        return Err("migration apply plan version is unsupported".to_string());
    }
    validate_operation_id(&plan.operation_id)?;
    for path in [&plan.canonical_root, &plan.backup_dir, &plan.staging_root] {
        if !path.is_absolute() {
            return Err("migration apply plan path is invalid".to_string());
        }
    }
    validate_sha256(&plan.inventory_fingerprint)?;
    let mut thread_ids = BTreeSet::new();
    let mut target_paths = BTreeSet::new();
    for session in &plan.sessions {
        if session.thread_id.trim().is_empty()
            || !thread_ids.insert(session.thread_id.clone())
            || !session.source_path.is_absolute()
            || !session.target_path.is_absolute()
            || !target_paths.insert(path_key(&session.target_path))
        {
            return Err("migration apply session plan is invalid".to_string());
        }
        validate_sha256(&session.expected_sha256)?;
        if let Some(value) = &session.target_before_sha256 {
            validate_sha256(value)?;
        }
        if let Some(path) = &session.staged_path {
            if !path.is_absolute() || !path.starts_with(&plan.staging_root) {
                return Err("migration apply session staging path is invalid".to_string());
            }
        }
        if session
            .target_backup_payload
            .as_ref()
            .is_some_and(|path| !path.is_absolute() || !path.starts_with(&plan.backup_dir))
        {
            return Err("migration apply session backup path is invalid".to_string());
        }
    }
    let mut database_ids = BTreeSet::new();
    let mut database_targets = BTreeSet::new();
    for database in &plan.databases {
        if database.database_id.trim().is_empty()
            || database.database_id.len() > 64
            || !database
                .database_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
            || !database_ids.insert(database.database_id.clone())
            || !database.target_path.is_absolute()
            || !database_targets.insert(path_key(&database.target_path))
            || !database.staged_path.is_absolute()
            || !database.staged_path.starts_with(&plan.staging_root)
            || !database.original_backup_payload.is_absolute()
            || !database
                .original_backup_payload
                .starts_with(&plan.backup_dir)
        {
            return Err("migration apply database plan is invalid".to_string());
        }
        validate_sha256(&database.original_sha256)?;
        validate_sha256(&database.staged_sha256)?;
    }
    Ok(())
}

fn write_apply_plan(path: &Path, plan: &MigrationApplyPlan) -> Result<(), String> {
    validate_plan_shape(plan)?;
    let replacement_paths = replacement_recovery_bindings(plan)?;
    let envelope = MigrationApplyPlanEnvelope {
        plan: plan.clone(),
        integrity_sha256: plan_envelope_digest(plan, &replacement_paths)?,
        replacement_paths,
    };
    let bytes = serde_json::to_vec_pretty(&envelope)
        .map_err(|_| "failed to serialize migration apply plan".to_string())?;
    if bytes.len() as u64 > MAX_APPLY_PLAN_BYTES {
        return Err("migration apply plan reached its size limit".to_string());
    }
    atomic_write(path, &bytes)?;
    let verified = load_apply_plan_from_path(path)?;
    if &verified == plan {
        Ok(())
    } else {
        Err("migration apply plan verification failed".to_string())
    }
}

fn load_apply_plan_from_path(path: &Path) -> Result<MigrationApplyPlan, String> {
    let bytes = read_regular_file_bounded(path, MAX_APPLY_PLAN_BYTES)
        .map_err(|_| "migration apply plan is unreadable".to_string())?;
    let envelope = serde_json::from_slice::<MigrationApplyPlanEnvelope>(&bytes)
        .map_err(|_| "migration apply plan is invalid".to_string())?;
    validate_plan_shape(&envelope.plan)?;
    validate_recovery_bindings(&envelope.plan, &envelope.replacement_paths)?;
    if envelope.integrity_sha256
        != plan_envelope_digest(&envelope.plan, &envelope.replacement_paths)?
    {
        return Err("migration apply plan integrity check failed".to_string());
    }
    Ok(envelope.plan)
}

fn plan_envelope_digest(
    plan: &MigrationApplyPlan,
    recovery_paths: &BTreeMap<String, PersistedHandleReplacePaths>,
) -> Result<String, String> {
    let bytes = serde_json::to_vec(&(plan, recovery_paths))
        .map_err(|_| "failed to serialize migration recovery bindings".to_string())?;
    Ok(hex_digest(Sha256::digest(bytes)))
}

fn durable_migration_path_key(path: &Path) -> String {
    let normalized = path.to_string_lossy().replace('/', "\\");
    if cfg!(windows) {
        normalized.to_ascii_lowercase()
    } else {
        normalized
    }
}

fn replacement_recovery_bindings(
    plan: &MigrationApplyPlan,
) -> Result<BTreeMap<String, PersistedHandleReplacePaths>, String> {
    let mut bindings = BTreeMap::new();
    for session in &plan.sessions {
        if session.action != MigrationSessionAction::ReplaceCanonicalWithExtension {
            continue;
        }
        insert_recovery_binding(
            &mut bindings,
            plan,
            &session.target_path,
            &format!("session:{}", session.thread_id),
        )?;
    }
    for database in &plan.databases {
        insert_recovery_binding(
            &mut bindings,
            plan,
            &database.target_path,
            &format!("database:{}", database.database_id),
        )?;
    }
    Ok(bindings)
}

fn insert_recovery_binding(
    bindings: &mut BTreeMap<String, PersistedHandleReplacePaths>,
    plan: &MigrationApplyPlan,
    target: &Path,
    label: &str,
) -> Result<(), String> {
    let key = durable_migration_path_key(target);
    let paths = PersistedHandleReplacePaths {
        target_path: target.to_path_buf(),
        recovery_path: deterministic_recovery_path(plan, target, label, "original")?,
        staging_path: deterministic_recovery_path(plan, target, label, "staging")?,
        rollback_tombstone_path: deterministic_recovery_path(
            plan,
            target,
            label,
            "rollback-tombstone",
        )?,
    };
    paths.typed()?;
    if bindings.insert(key, paths).is_some() {
        return Err("migration recovery target is duplicated".to_string());
    }
    Ok(())
}

fn validate_recovery_bindings(
    plan: &MigrationApplyPlan,
    actual: &BTreeMap<String, PersistedHandleReplacePaths>,
) -> Result<(), String> {
    if actual != &replacement_recovery_bindings(plan)? {
        return Err("migration recovery path bindings changed".to_string());
    }
    Ok(())
}

fn deterministic_recovery_path(
    plan: &MigrationApplyPlan,
    target: &Path,
    label: &str,
    phase: &str,
) -> Result<PathBuf, String> {
    let parent = target
        .parent()
        .ok_or_else(|| "migration replacement target has no parent".to_string())?;
    let mut hasher = Sha256::new();
    hasher.update(b"codex-switch-migration-recovery-v1\0");
    hasher.update(plan.operation_id.as_bytes());
    hasher.update([0]);
    hasher.update(durable_migration_path_key(target).as_bytes());
    hasher.update([0]);
    hasher.update(label.as_bytes());
    hasher.update([0]);
    hasher.update(phase.as_bytes());
    let digest = hex_digest(hasher.finalize());
    Ok(parent.join(format!(
        ".codex-switch-migration-{phase}-{}.recovery",
        &digest[..32]
    )))
}

fn replacement_paths_for_target(
    plan: &MigrationApplyPlan,
    target: &Path,
) -> Result<PersistedHandleReplacePaths, String> {
    replacement_recovery_bindings(plan)?
        .remove(&durable_migration_path_key(target))
        .ok_or_else(|| "migration replacement recovery binding is missing".to_string())
}

fn migration_replacement_phase_path(plan: &MigrationApplyPlan) -> Result<PathBuf, String> {
    Ok(
        operation_root(&migration_data_root(plan)?, &plan.operation_id)?
            .join("migration-replacement-phases.bin"),
    )
}

fn migration_data_root(plan: &MigrationApplyPlan) -> Result<PathBuf, String> {
    plan.staging_root
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .ok_or_else(|| "migration operation root is invalid".to_string())
}

fn migration_replacement_plan_digest(plan: &MigrationApplyPlan) -> Result<String, String> {
    let bindings = replacement_recovery_bindings(plan)?;
    plan_envelope_digest(plan, &bindings)
}

fn migration_replacement_expected_sha256(
    plan: &MigrationApplyPlan,
    target: &Path,
) -> Result<String, String> {
    let key = durable_migration_path_key(target);
    plan.sessions
        .iter()
        .find(|session| {
            session.action == MigrationSessionAction::ReplaceCanonicalWithExtension
                && durable_migration_path_key(&session.target_path) == key
        })
        .map(|session| session.expected_sha256.clone())
        .or_else(|| {
            plan.databases
                .iter()
                .find(|database| durable_migration_path_key(&database.target_path) == key)
                .map(|database| database.staged_sha256.clone())
        })
        .ok_or_else(|| "migration replacement target has no planned checksum".to_string())
}

fn initial_migration_replacement_phase_record(
    plan: &MigrationApplyPlan,
) -> Result<MigrationHandleReplacePhaseRecord, String> {
    let replacements = replacement_recovery_bindings(plan)?
        .into_values()
        .map(|paths| {
            Ok(MigrationHandleReplacePhaseEntry {
                expected_replacement_sha256: migration_replacement_expected_sha256(
                    plan,
                    &paths.target_path,
                )?,
                active_replacement_sha256: None,
                rollback_replacement: false,
                target_path: paths.target_path.clone(),
                paths,
                expected_original_sha256: None,
                identities: None,
                phase: MigrationHandleReplacePhase::Planned,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(MigrationHandleReplacePhaseRecord {
        schema_version: APPLY_PLAN_SCHEMA_VERSION,
        operation_id: plan.operation_id.clone(),
        plan_integrity_sha256: migration_replacement_plan_digest(plan)?,
        updated_at_ms: timestamp_millis()?,
        replacements,
    })
}

fn migration_replacement_phase_digest(
    record: &MigrationHandleReplacePhaseRecord,
) -> Result<String, String> {
    let bytes = serde_json::to_vec(record)
        .map_err(|_| "failed to serialize migration replacement phase".to_string())?;
    Ok(hex_digest(Sha256::digest(bytes)))
}

fn persist_initial_migration_replacement_phases(plan: &MigrationApplyPlan) -> Result<(), String> {
    let expected = initial_migration_replacement_phase_record(plan)?;
    if expected.replacements.is_empty() {
        return Ok(());
    }
    let path = migration_replacement_phase_path(plan)?;
    if path.exists() {
        load_migration_replacement_phases(plan).map(|_| ())
    } else {
        write_migration_replacement_phases(plan, &expected)
    }
}

fn load_migration_replacement_phases(
    plan: &MigrationApplyPlan,
) -> Result<MigrationHandleReplacePhaseRecord, String> {
    let expected = initial_migration_replacement_phase_record(plan)?;
    if expected.replacements.is_empty() {
        return Ok(expected);
    }
    let protected = read_regular_file_bounded(
        &migration_replacement_phase_path(plan)?,
        MAX_REPLACEMENT_PHASE_BYTES,
    )
    .map_err(|_| "migration replacement phase record is unreadable".to_string())?;
    let plaintext =
        if let Some(ciphertext) = protected.strip_prefix(REPLACEMENT_PHASE_CIPHERTEXT_MAGIC) {
            crate::crypto::unprotect(ciphertext)
                .map_err(|_| "migration replacement phase record is unreadable".to_string())?
        } else {
            #[cfg(windows)]
            {
                return Err("migration replacement phase record is not protected".to_string());
            }
            #[cfg(not(windows))]
            {
                protected
            }
        };
    if plaintext.len() as u64 > MAX_REPLACEMENT_PHASE_BYTES {
        return Err("migration replacement phase record reached its size limit".to_string());
    }
    let envelope = serde_json::from_slice::<MigrationHandleReplacePhaseEnvelope>(&plaintext)
        .map_err(|_| "migration replacement phase record is invalid".to_string())?;
    if envelope.integrity_sha256 != migration_replacement_phase_digest(&envelope.record)? {
        return Err("migration replacement phase record checksum changed".to_string());
    }
    if envelope.record.schema_version != APPLY_PLAN_SCHEMA_VERSION {
        return Err("migration replacement phase record version changed".to_string());
    }
    if envelope.record.operation_id != plan.operation_id {
        return Err("migration replacement phase operation identity changed".to_string());
    }
    if envelope.record.plan_integrity_sha256 != migration_replacement_plan_digest(plan)? {
        return Err("migration replacement phase plan identity changed".to_string());
    }
    if envelope.record.replacements.len() != expected.replacements.len() {
        return Err("migration replacement phase target set changed".to_string());
    }
    for (actual, planned) in envelope
        .record
        .replacements
        .iter()
        .zip(&expected.replacements)
    {
        if actual.target_path != planned.target_path
            || actual.paths != planned.paths
            || actual.expected_replacement_sha256 != planned.expected_replacement_sha256
        {
            return Err(
                "migration replacement phase record no longer matches its plan".to_string(),
            );
        }
        validate_sha256(&actual.expected_replacement_sha256)?;
        if let Some(original) = &actual.expected_original_sha256 {
            validate_sha256(original)?;
        }
        if let Some(replacement) = &actual.active_replacement_sha256 {
            validate_sha256(replacement)?;
        }
        let identities_required = !matches!(
            actual.phase,
            MigrationHandleReplacePhase::Planned | MigrationHandleReplacePhase::Skipped
        );
        if identities_required
            != (actual.identities.is_some()
                && actual.expected_original_sha256.is_some()
                && actual.active_replacement_sha256.is_some())
            || (!identities_required
                && (actual.identities.is_some()
                    || actual.expected_original_sha256.is_some()
                    || actual.active_replacement_sha256.is_some()
                    || actual.rollback_replacement))
        {
            return Err("migration replacement phase identity binding is invalid".to_string());
        }
    }
    Ok(envelope.record)
}

fn write_migration_replacement_phases(
    plan: &MigrationApplyPlan,
    record: &MigrationHandleReplacePhaseRecord,
) -> Result<(), String> {
    let envelope = MigrationHandleReplacePhaseEnvelope {
        integrity_sha256: migration_replacement_phase_digest(record)?,
        record: record.clone(),
    };
    let plaintext = serde_json::to_vec_pretty(&envelope)
        .map_err(|_| "failed to serialize migration replacement phase".to_string())?;
    if plaintext.len() as u64 > MAX_REPLACEMENT_PHASE_BYTES {
        return Err("migration replacement phase record reached its size limit".to_string());
    }
    #[cfg(windows)]
    let bytes = {
        let ciphertext = crate::crypto::protect(&plaintext)
            .map_err(|_| "failed to protect migration replacement phase".to_string())?;
        let mut protected =
            Vec::with_capacity(REPLACEMENT_PHASE_CIPHERTEXT_MAGIC.len() + ciphertext.len());
        protected.extend_from_slice(REPLACEMENT_PHASE_CIPHERTEXT_MAGIC);
        protected.extend_from_slice(&ciphertext);
        protected
    };
    #[cfg(not(windows))]
    let bytes = plaintext;
    atomic_write(&migration_replacement_phase_path(plan)?, &bytes)
}

fn migration_replacement_phase_entry(
    plan: &MigrationApplyPlan,
    target: &Path,
) -> Result<MigrationHandleReplacePhaseEntry, String> {
    let key = durable_migration_path_key(target);
    load_migration_replacement_phases(plan)?
        .replacements
        .into_iter()
        .find(|entry| durable_migration_path_key(&entry.target_path) == key)
        .ok_or_else(|| "migration replacement phase entry is missing".to_string())
}

fn record_staged_migration_replacement(
    plan: &MigrationApplyPlan,
    target: &Path,
    expected_original_sha256: &str,
    active_replacement_sha256: &str,
    rollback_replacement: bool,
    identities: HandleReplaceIdentityBindings,
) -> Result<(), String> {
    validate_sha256(expected_original_sha256)?;
    validate_sha256(active_replacement_sha256)?;
    let key = durable_migration_path_key(target);
    let mut record = load_migration_replacement_phases(plan)?;
    let entry = record
        .replacements
        .iter_mut()
        .find(|entry| durable_migration_path_key(&entry.target_path) == key)
        .ok_or_else(|| "migration replacement phase entry is missing".to_string())?;
    if entry.phase != MigrationHandleReplacePhase::Planned
        || entry.expected_original_sha256.is_some()
        || entry.identities.is_some()
    {
        return Err("migration replacement staging phase is invalid".to_string());
    }
    entry.expected_original_sha256 = Some(expected_original_sha256.to_string());
    entry.active_replacement_sha256 = Some(active_replacement_sha256.to_string());
    entry.rollback_replacement = rollback_replacement;
    entry.identities = Some(identities);
    entry.phase = MigrationHandleReplacePhase::Staged;
    if classify_migration_replacement_entry(entry)? != HandleReplaceCrashState::Staged {
        return Err("migration replacement staging identity changed".to_string());
    }
    record.updated_at_ms = timestamp_millis()?;
    write_migration_replacement_phases(plan, &record)
}

fn transition_migration_replacement_phase(
    plan: &MigrationApplyPlan,
    target: &Path,
    allowed: &[MigrationHandleReplacePhase],
    next: MigrationHandleReplacePhase,
) -> Result<(), String> {
    let key = durable_migration_path_key(target);
    let mut record = load_migration_replacement_phases(plan)?;
    let entry = record
        .replacements
        .iter_mut()
        .find(|entry| durable_migration_path_key(&entry.target_path) == key)
        .ok_or_else(|| "migration replacement phase entry is missing".to_string())?;
    if entry.phase == next {
        validate_migration_replacement_phase_layout(entry).map_err(|error| {
            format!(
                "{error}; migration replacement phase replay={:?}",
                entry.phase
            )
        })?;
        return Ok(());
    }
    if !allowed.contains(&entry.phase) {
        return Err("migration replacement phase transition is invalid".to_string());
    }
    validate_migration_replacement_phase_layout(entry).map_err(|error| {
        format!(
            "{error}; migration replacement phase before={:?}, next={next:?}",
            entry.phase
        )
    })?;
    entry.phase = next;
    validate_migration_replacement_phase_layout(entry).map_err(|error| {
        format!(
            "{error}; migration replacement phase after={:?}",
            entry.phase
        )
    })?;
    record.updated_at_ms = timestamp_millis()?;
    write_migration_replacement_phases(plan, &record)
}

fn finish_recovered_migration_replacement_phase(
    plan: &MigrationApplyPlan,
    target: &Path,
    allowed: &[MigrationHandleReplacePhase],
    next: MigrationHandleReplacePhase,
) -> Result<(), String> {
    let key = durable_migration_path_key(target);
    let mut record = load_migration_replacement_phases(plan)?;
    let entry = record
        .replacements
        .iter_mut()
        .find(|entry| durable_migration_path_key(&entry.target_path) == key)
        .ok_or_else(|| "migration replacement phase entry is missing".to_string())?;
    if !allowed.contains(&entry.phase) && entry.phase != next {
        return Err("migration recovered replacement phase transition is invalid".to_string());
    }
    entry.phase = next;
    validate_migration_replacement_phase_layout(entry)?;
    record.updated_at_ms = timestamp_millis()?;
    write_migration_replacement_phases(plan, &record)
}

fn classify_migration_replacement_entry(
    entry: &MigrationHandleReplacePhaseEntry,
) -> Result<HandleReplaceCrashState, String> {
    classify_handle_replace_crash_state(
        &entry.paths.typed()?,
        entry
            .identities
            .ok_or_else(|| "migration replacement identity is missing".to_string())?,
        entry
            .expected_original_sha256
            .as_deref()
            .ok_or_else(|| "migration replacement original checksum is missing".to_string())?,
        entry
            .active_replacement_sha256
            .as_deref()
            .ok_or_else(|| "migration active replacement checksum is missing".to_string())?,
    )
}

fn active_migration_replacement_sha256(
    entry: &MigrationHandleReplacePhaseEntry,
) -> Result<&str, String> {
    entry
        .active_replacement_sha256
        .as_deref()
        .ok_or_else(|| "migration active replacement checksum is missing".to_string())
}

fn validate_migration_replacement_phase_layout(
    entry: &MigrationHandleReplacePhaseEntry,
) -> Result<(), String> {
    if matches!(
        entry.phase,
        MigrationHandleReplacePhase::Planned | MigrationHandleReplacePhase::Skipped
    ) {
        for path in [
            &entry.paths.recovery_path,
            &entry.paths.staging_path,
            &entry.paths.rollback_tombstone_path,
        ] {
            match fs::symlink_metadata(path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Ok(_) => {
                    return Err(
                        "migration replacement has an unbound deterministic artifact".to_string(),
                    )
                }
                Err(_) => return Err("migration replacement artifact is unavailable".to_string()),
            }
        }
        return Ok(());
    }
    let state = classify_migration_replacement_entry(entry)?;
    let valid = match entry.phase {
        MigrationHandleReplacePhase::Staged => state == HandleReplaceCrashState::Staged,
        MigrationHandleReplacePhase::Preparing => matches!(
            state,
            HandleReplaceCrashState::Staged | HandleReplaceCrashState::Prepared
        ),
        MigrationHandleReplacePhase::Prepared => state == HandleReplaceCrashState::Prepared,
        MigrationHandleReplacePhase::Publishing => matches!(
            state,
            HandleReplaceCrashState::Prepared | HandleReplaceCrashState::ReplacementWithRecovery
        ),
        MigrationHandleReplacePhase::Published
        | MigrationHandleReplacePhase::Committing
        | MigrationHandleReplacePhase::CommittedWithRecovery => {
            state == HandleReplaceCrashState::ReplacementWithRecovery
        }
        MigrationHandleReplacePhase::CommitCleaning => matches!(
            state,
            HandleReplaceCrashState::ReplacementWithRecovery
                | HandleReplaceCrashState::ReplacementOnly
        ),
        MigrationHandleReplacePhase::CommittedCleaned => {
            state == HandleReplaceCrashState::ReplacementOnly
        }
        MigrationHandleReplacePhase::RollbackPreparing => matches!(
            state,
            HandleReplaceCrashState::Staged
                | HandleReplaceCrashState::Prepared
                | HandleReplaceCrashState::ReplacementWithRecovery
                | HandleReplaceCrashState::RollbackPrepared
                | HandleReplaceCrashState::RolledBack
                | HandleReplaceCrashState::Original
        ),
        MigrationHandleReplacePhase::RolledBackWithTombstone => matches!(
            state,
            HandleReplaceCrashState::Staged
                | HandleReplaceCrashState::ReplacementWithRecovery
                | HandleReplaceCrashState::RolledBack
                | HandleReplaceCrashState::Original
        ),
        MigrationHandleReplacePhase::RollbackCleaning => {
            if entry.rollback_replacement {
                matches!(
                    state,
                    HandleReplaceCrashState::ReplacementWithRecovery
                        | HandleReplaceCrashState::ReplacementOnly
                )
            } else {
                matches!(
                    state,
                    HandleReplaceCrashState::Staged
                        | HandleReplaceCrashState::RolledBack
                        | HandleReplaceCrashState::Original
                )
            }
        }
        MigrationHandleReplacePhase::RolledBackCleaned => {
            if entry.rollback_replacement {
                state == HandleReplaceCrashState::ReplacementOnly
            } else {
                state == HandleReplaceCrashState::Original
            }
        }
        MigrationHandleReplacePhase::Planned | MigrationHandleReplacePhase::Skipped => true,
    };
    if valid {
        Ok(())
    } else {
        Err("migration replacement durable phase disagrees with its physical layout".to_string())
    }
}

fn operation_root(data_root: &Path, operation_id: &str) -> Result<PathBuf, String> {
    validate_operation_id(operation_id)?;
    if !data_root.is_absolute() {
        return Err("migration managed data root is invalid".to_string());
    }
    let root = data_root
        .join("session-storage-v1/operations")
        .join(operation_id);
    validate_safe_directory(&root, "operation root")?;
    Ok(root)
}

fn validate_session_plan(
    session: &MigrationSessionPlan,
    sessions_root: &Path,
) -> Result<(), String> {
    if session.thread_id.trim().is_empty()
        || !session.retained_path.is_absolute()
        || !session.canonical_path.is_absolute()
        || !matches!(session.action, MigrationSessionAction::Conflict)
            && !session.canonical_path.starts_with(sessions_root)
    {
        return Err("migration session target is invalid".to_string());
    }
    validate_sha256(&session.retained_sha256)
}

fn backup_entry_for_source<'a>(
    backup: &'a MigrationBackupManifest,
    source_path: &Path,
    kind: MigrationBackupEntryKind,
) -> Result<&'a MigrationBackupEntry, String> {
    let key = path_key(source_path);
    backup
        .entries
        .iter()
        .find(|entry| entry.kind == kind && path_key(&entry.source_path) == key)
        .ok_or_else(|| "migration backup does not contain a required source".to_string())
}

fn backup_payload_path(
    backup: &MigrationBackupManifest,
    entry: &MigrationBackupEntry,
) -> Result<PathBuf, String> {
    validate_relative_path(&entry.payload_relative_path)?;
    let path = backup
        .backup_dir
        .join("payload")
        .join(&entry.payload_relative_path);
    let (_, sha256) = stable_file_digest(&path)?;
    if sha256 == entry.sha256 {
        Ok(path)
    } else {
        Err("migration backup payload checksum changed".to_string())
    }
}

fn create_safe_directory(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path)
        .map_err(|_| "failed to create migration staging directory".to_string())?;
    validate_safe_directory(path, "staging directory")
}

fn validate_safe_directory(path: &Path, label: &str) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(format!("migration {label} path is invalid"));
    }
    let metadata =
        fs::symlink_metadata(path).map_err(|_| format!("migration {label} is unavailable"))?;
    if !metadata.is_dir() || metadata_is_link_or_reparse(&metadata) {
        return Err(format!("migration {label} is unsafe"));
    }
    Ok(())
}

fn remove_owned_staging_tree(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    validate_safe_directory(path, "staging directory")?;
    validate_tree_without_links(path)?;
    fs::remove_dir_all(path).map_err(|_| "failed to remove migration staging directory".to_string())
}

fn validate_tree_without_links(root: &Path) -> Result<(), String> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = fs::symlink_metadata(&path)
            .map_err(|_| "migration staging tree is unreadable".to_string())?;
        if metadata_is_link_or_reparse(&metadata) {
            return Err("migration staging tree contains a link or reparse point".to_string());
        }
        if metadata.is_dir() {
            for entry in fs::read_dir(&path)
                .map_err(|_| "migration staging tree is unreadable".to_string())?
            {
                pending.push(
                    entry
                        .map_err(|_| "migration staging tree is unreadable".to_string())?
                        .path(),
                );
            }
        } else if !metadata.is_file() {
            return Err("migration staging tree contains an unsupported entry".to_string());
        }
    }
    Ok(())
}

fn table_exists(connection: &Connection, table: &str) -> Result<bool, String> {
    let count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |row| row.get(0),
        )
        .map_err(|_| "failed to inspect migration database schema".to_string())?;
    Ok(count == 1)
}

fn table_schema(connection: &Connection, table: &str) -> Result<Vec<TableColumn>, String> {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_xinfo({})", quote_identifier(table)))
        .map_err(|_| "failed to inspect migration database table".to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok(TableColumn {
                name: row.get::<_, String>(1)?,
                declared_type: row.get::<_, String>(2)?,
                not_null: row.get::<_, i64>(3)? != 0,
                default_value: row.get::<_, Option<String>>(4)?,
                primary_key_ordinal: row.get::<_, i64>(5)?,
                hidden: row.get::<_, i64>(6)?,
            })
        })
        .map_err(|_| "failed to inspect migration database table".to_string())?;
    let mut output = Vec::new();
    for row in rows {
        output.push(row.map_err(|_| "failed to read migration database schema".to_string())?);
    }
    Ok(output)
}

fn quote_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn quick_check_sqlite(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| "failed to inspect migration SQLite database".to_string())?;
    if !metadata.is_file() || metadata_is_link_or_reparse(&metadata) {
        return Err("migration SQLite database path is unsafe".to_string());
    }
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| format!("failed to open migration SQLite database: {error}"))?;
    let result: String = connection
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(|_| "failed to verify migration SQLite database".to_string())?;
    if result == "ok" {
        Ok(())
    } else {
        Err("migration SQLite database failed quick_check".to_string())
    }
}

fn remove_sqlite_sidecars(path: &Path) -> Result<(), String> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let candidate = PathBuf::from(format!("{}{}", path.to_string_lossy(), suffix));
        match fs::symlink_metadata(&candidate) {
            Ok(metadata) => {
                if !metadata.is_file() || metadata_is_link_or_reparse(&metadata) {
                    return Err("migration SQLite sidecar is unsafe".to_string());
                }
                fs::remove_file(&candidate)
                    .map_err(|_| "failed to remove migration SQLite sidecar".to_string())?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("migration SQLite sidecar is unavailable".to_string()),
        }
    }
    Ok(())
}

fn ensure_sqlite_sidecars_absent(path: &Path) -> Result<(), String> {
    if sqlite_sidecars_absent(path)? {
        Ok(())
    } else {
        Err("applied migration database has an active sidecar".to_string())
    }
}

pub(crate) fn sqlite_sidecars_absent(path: &Path) -> Result<bool, String> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let candidate = PathBuf::from(format!("{}{}", path.to_string_lossy(), suffix));
        match fs::symlink_metadata(&candidate) {
            Ok(_) => return Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("applied migration database sidecar is unavailable".to_string()),
        }
    }
    Ok(true)
}

pub(crate) fn stable_file_digest(path: &Path) -> Result<(u64, String), String> {
    let before_path = fs::symlink_metadata(path)
        .map_err(|_| "migration staged file is unavailable".to_string())?;
    if !before_path.is_file() || metadata_is_link_or_reparse(&before_path) {
        return Err("migration staged file is unsafe".to_string());
    }
    let mut file = open_regular_file(path)?;
    let before_handle = file
        .metadata()
        .map_err(|_| "migration staged file is unreadable".to_string())?;
    if !before_handle.is_file()
        || metadata_is_link_or_reparse(&before_handle)
        || file_stamp(&before_handle) != file_stamp(&before_path)
    {
        return Err("migration staged file changed before hashing".to_string());
    }
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|_| "failed to hash migration staged file".to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let after_handle = file
        .metadata()
        .map_err(|_| "migration staged file is unreadable".to_string())?;
    let after_path = fs::symlink_metadata(path)
        .map_err(|_| "migration staged file is unavailable".to_string())?;
    if !after_path.is_file()
        || metadata_is_link_or_reparse(&after_path)
        || file_stamp(&before_handle) != file_stamp(&after_handle)
        || file_stamp(&before_handle) != file_stamp(&after_path)
    {
        return Err("migration staged file changed while hashing".to_string());
    }
    Ok((before_handle.len(), hex_digest(hasher.finalize())))
}

fn open_regular_file(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options
        .open(path)
        .map_err(|_| "failed to open migration staged file".to_string())
}

fn file_stamp(metadata: &fs::Metadata) -> (u64, Option<std::time::SystemTime>) {
    (metadata.len(), metadata.modified().ok())
}

fn validate_relative_path(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        Err("migration backup payload path is invalid".to_string())
    } else {
        Ok(())
    }
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
        Err("migration operation ID is invalid".to_string())
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
        Err("migration checksum is invalid".to_string())
    }
}

fn hex_digest(digest: impl AsRef<[u8]>) -> String {
    digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
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
    use std::{
        fs::{self, OpenOptions},
        path::{Path, PathBuf},
    };

    use rusqlite::Connection;
    use tempfile::tempdir;

    use super::{
        apply_prepared_migration, prepare_migration_apply_plan, recover_interrupted_migration,
        validate_applied_migration, verify_applied_migration_with_runtime, MigrationRecoveryStatus,
    };
    use crate::session_storage::{
        codex_runtime_verifier::NativeCodexBackupVerifier,
        migration::{
            CanonicalMigrationPlan, MigrationDatabasePlan, MigrationPreflightReport,
            MigrationSessionAction, MigrationSessionPlan,
        },
        migration_backup::{
            create_migration_backup, verify_migration_backup_with_runtime,
            MigrationBackupEntryKind, MigrationBackupManifest, MigrationBackupRuntimeVerifier,
            MigrationBackupSource, MigrationRuntimeBinaryIdentity,
            MigrationRuntimeCapabilityConflictProof, MigrationRuntimeVerification,
        },
        model::DatabaseRole,
        operation_ledger::{
            OperationLedgerStore, SessionStorageOperationKind, SessionStorageOperationPhase,
        },
        semantic::read_semantic_session,
    };

    struct PassingVerifier;

    impl MigrationBackupRuntimeVerifier for PassingVerifier {
        fn verify(
            &self,
            _isolated_root: &Path,
            manifest: &MigrationBackupManifest,
        ) -> Result<MigrationRuntimeVerification, String> {
            Ok(MigrationRuntimeVerification {
                expected_session_count: 2,
                listed_session_count: 2,
                resumed_session_count: 2,
                continued_session_count: 1,
                tool_session_count: 1,
                tool_round_trip_verified: true,
                available_categories: ["ordinary", "long", "subagent", "conflictCanonical", "tool"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
                continued_categories: ["ordinary", "long", "subagent", "conflictCanonical", "tool"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
                conflict_payload_count: 0,
                conflict_payloads_verified: true,
                conflict_proofs: Vec::new(),
                capability_conflict_proof: Some(MigrationRuntimeCapabilityConflictProof {
                    fixture_thread_id_sha256: "0".repeat(64),
                    canonical_bytes: 1,
                    canonical_sha256: "1".repeat(64),
                    recycle_bytes: 1,
                    recycle_sha256: "2".repeat(64),
                    relation: "divergent".to_string(),
                }),
                runtime_binary_identity: Some(MigrationRuntimeBinaryIdentity {
                    version: "codex-cli test".to_string(),
                    bytes: 1,
                    sha256: "3".repeat(64),
                }),
                verified_at_ms: manifest.created_at_ms,
            })
        }
    }

    #[test]
    #[ignore = "requires CODEX_SWITCH_RUNTIME_FIXTURE_HOME and a native codex.exe"]
    fn verifies_applied_fixture_with_the_native_codex_app_server() {
        let fixture_home = PathBuf::from(
            std::env::var_os("CODEX_SWITCH_RUNTIME_FIXTURE_HOME")
                .expect("CODEX_SWITCH_RUNTIME_FIXTURE_HOME must be set"),
        );
        let database = fixture_home.join("state_5.sqlite");
        let connection = Connection::open(&database).unwrap();
        let mut statement = connection
            .prepare("SELECT id, rollout_path FROM threads ORDER BY id")
            .unwrap();
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        drop(statement);
        drop(connection);
        assert!(!rows.is_empty());

        let root = tempdir().unwrap();
        let staging_root = root.path().join("staging");
        let backup_dir = root.path().join("backup");
        fs::create_dir(&staging_root).unwrap();
        fs::create_dir(&backup_dir).unwrap();
        let sessions = rows
            .into_iter()
            .map(|(thread_id, rollout_path)| {
                let target_path = PathBuf::from(rollout_path);
                let semantic = read_semantic_session(&target_path).unwrap();
                assert_eq!(semantic.thread_id, thread_id);
                let sha256 = super::hex_digest(semantic.raw_sha256);
                super::MigrationSessionApplyEntry {
                    thread_id,
                    action: MigrationSessionAction::KeepCanonical,
                    source_path: target_path.clone(),
                    target_path,
                    staged_path: None,
                    expected_sha256: sha256.clone(),
                    target_before_sha256: Some(sha256),
                    target_backup_payload: None,
                }
            })
            .collect::<Vec<_>>();
        let (_, database_sha256) = super::stable_file_digest(&database).unwrap();
        let plan = super::MigrationApplyPlan {
            schema_version: super::APPLY_PLAN_SCHEMA_VERSION,
            operation_id: "native-post-apply-test".to_string(),
            generated_at_ms: 1,
            canonical_root: fixture_home,
            inventory_fingerprint: "a".repeat(64),
            backup_dir: backup_dir.clone(),
            staging_root: staging_root.clone(),
            sessions,
            databases: vec![super::MigrationDatabaseApplyEntry {
                database_id: "canonical".to_string(),
                role: DatabaseRole::CanonicalAccount,
                target_path: database,
                staged_path: staging_root.join("unused.sqlite.stage"),
                original_backup_payload: backup_dir.join("unused.sqlite"),
                original_sha256: database_sha256.clone(),
                staged_sha256: database_sha256,
                staged_bytes: 0,
            }],
            conflict_count: 0,
        };
        let verifier = NativeCodexBackupVerifier::discover().unwrap();
        let runtime = verify_applied_migration_with_runtime(&plan, &verifier).unwrap();
        assert_eq!(runtime.expected_session_count, plan.sessions.len());
        assert_eq!(runtime.listed_session_count, plan.sessions.len());
        assert!(runtime.resumed_session_count > 0);
    }

    fn write_session(path: &Path, thread_id: &str, provider: &str, messages: &[&str]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut lines = vec![serde_json::json!({
            "type": "session_meta",
            "payload": {"id": thread_id, "model_provider": provider}
        })
        .to_string()];
        lines.extend(messages.iter().map(|message| {
            serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "user_message", "message": message}
            })
            .to_string()
        }));
        fs::write(path, lines.join("\n") + "\n").unwrap();
    }

    #[test]
    fn precommit_cleanup_removes_only_the_owned_staging_tree() {
        let root = tempdir().unwrap();
        let data_root = root.path().join("data");
        let operation_root = data_root
            .join("session-storage-v1/operations")
            .join("migration-cancel-1");
        let staging_root = operation_root.join("migration-staging");
        fs::create_dir_all(&staging_root).unwrap();
        fs::write(staging_root.join("staged.jsonl"), b"staged").unwrap();
        fs::write(operation_root.join("ledger.json"), b"ledger").unwrap();

        assert!(
            super::cleanup_migration_staging_for_operation(&data_root, "migration-cancel-1")
                .unwrap()
        );
        assert!(!staging_root.exists());
        assert!(operation_root.join("ledger.json").is_file());
        assert!(
            !super::cleanup_migration_staging_for_operation(&data_root, "migration-cancel-1")
                .unwrap()
        );
    }

    fn create_database(path: &Path, rows: &[(&str, &Path, &str)]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE threads (
                    id TEXT PRIMARY KEY,
                    rollout_path TEXT NOT NULL,
                    model_provider TEXT NOT NULL,
                    source TEXT NOT NULL DEFAULT 'cli'
                 );
                 CREATE TABLE thread_spawn_edges (
                    parent_thread_id TEXT NOT NULL,
                    child_thread_id TEXT NOT NULL,
                    PRIMARY KEY (parent_thread_id, child_thread_id)
                 );",
            )
            .unwrap();
        for (thread_id, rollout_path, provider) in rows {
            connection
                .execute(
                    "INSERT INTO threads (id, rollout_path, model_provider) VALUES (?1, ?2, ?3)",
                    (
                        thread_id,
                        rollout_path.to_string_lossy().to_string(),
                        provider,
                    ),
                )
                .unwrap();
        }
    }

    fn create_strict_dependent_database(path: &Path, payload_type: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch(&format!(
                "CREATE TABLE threads (
                    id TEXT PRIMARY KEY,
                    rollout_path TEXT NOT NULL,
                    model_provider TEXT NOT NULL,
                    source TEXT NOT NULL DEFAULT 'cli'
                 );
                 CREATE TABLE thread_dynamic_tools (
                    thread_id TEXT NOT NULL,
                    position INTEGER NOT NULL,
                    name TEXT NOT NULL,
                    namespace TEXT NOT NULL,
                    payload {payload_type},
                    score REAL,
                    PRIMARY KEY (thread_id, position, name, namespace)
                 );
                 CREATE TABLE thread_spawn_edges (
                    parent_thread_id TEXT NOT NULL,
                    child_thread_id TEXT NOT NULL,
                    relation TEXT NOT NULL DEFAULT 'spawn',
                    PRIMARY KEY (parent_thread_id, child_thread_id)
                 );"
            ))
            .unwrap();
    }

    fn dependent_database_entry(
        database_id: &str,
        path: &Path,
        role: DatabaseRole,
    ) -> super::MigrationDatabaseApplyEntry {
        super::MigrationDatabaseApplyEntry {
            database_id: database_id.to_string(),
            role,
            target_path: path.with_extension("live.sqlite"),
            staged_path: path.to_path_buf(),
            original_backup_payload: path.with_extension("backup.sqlite"),
            original_sha256: "a".repeat(64),
            staged_sha256: "b".repeat(64),
            staged_bytes: 0,
        }
    }

    fn dependent_database_snapshot(
        path: &Path,
    ) -> std::collections::BTreeMap<String, Vec<Vec<u8>>> {
        let connection = Connection::open(path).unwrap();
        let mut output = std::collections::BTreeMap::new();
        for table in [
            super::DependentTable::Single("thread_dynamic_tools", "thread_id"),
            super::DependentTable::Either(
                "thread_spawn_edges",
                "parent_thread_id",
                "child_thread_id",
            ),
        ] {
            let schema = super::table_schema(&connection, table.name()).unwrap();
            let mut rows = super::read_dependent_rows(&connection, table, &schema, None)
                .unwrap()
                .into_iter()
                .map(|row| super::encode_dependent_values(&row).unwrap())
                .collect::<Vec<_>>();
            rows.sort();
            output.insert(table.name().to_string(), rows);
        }
        output
    }

    fn seed_dependent_union_fixture(root: &Path, label: &str) -> Vec<PathBuf> {
        let paths = ["account", "relay", "shared"]
            .into_iter()
            .map(|name| root.join(format!("{label}-{name}.sqlite")))
            .collect::<Vec<_>>();
        for path in &paths {
            create_strict_dependent_database(path, "BLOB");
        }
        Connection::open(&paths[0])
            .unwrap()
            .execute_batch(
                "INSERT INTO thread_dynamic_tools VALUES
                    ('thread-x', 1, 'search', 'tools', X'00FF', 1.5),
                    ('unrelated-a', 2, 'keep-a', 'tools', X'01', 2.5);",
            )
            .unwrap();
        Connection::open(&paths[2])
            .unwrap()
            .execute_batch(
                "INSERT INTO thread_spawn_edges VALUES
                    ('thread-x', 'child-x', 'spawn'),
                    ('unrelated-c', 'keep-c', 'spawn');",
            )
            .unwrap();
        paths
    }

    #[test]
    fn dependent_rows_gather_three_database_union_is_order_independent_and_idempotent() {
        let root = tempdir().unwrap();
        let first = seed_dependent_union_fixture(root.path(), "first");
        let second = seed_dependent_union_fixture(root.path(), "second");

        let first_targets = vec![
            dependent_database_entry("shared", &first[2], DatabaseRole::Shared),
            dependent_database_entry("account", &first[0], DatabaseRole::CanonicalAccount),
            dependent_database_entry("relay", &first[1], DatabaseRole::Relay),
        ];
        let second_targets = vec![
            dependent_database_entry("relay", &second[1], DatabaseRole::Relay),
            dependent_database_entry("shared", &second[2], DatabaseRole::Shared),
            dependent_database_entry("account", &second[0], DatabaseRole::CanonicalAccount),
        ];
        super::copy_dependent_rows_for_thread_from_sources(
            &[first[2].clone(), first[0].clone(), first[1].clone()],
            &first_targets,
            "thread-x",
        )
        .unwrap();
        super::copy_dependent_rows_for_thread_from_sources(
            &[second[1].clone(), second[2].clone(), second[0].clone()],
            &second_targets,
            "thread-x",
        )
        .unwrap();

        for index in 0..3 {
            assert_eq!(
                dependent_database_snapshot(&first[index]),
                dependent_database_snapshot(&second[index])
            );
            let connection = Connection::open(&first[index]).unwrap();
            assert_eq!(
                connection
                    .query_row(
                        "SELECT COUNT(*) FROM thread_dynamic_tools WHERE thread_id = 'thread-x'",
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap(),
                1
            );
            assert_eq!(
                connection
                    .query_row(
                        "SELECT COUNT(*) FROM thread_spawn_edges
                         WHERE parent_thread_id = 'thread-x' OR child_thread_id = 'thread-x'",
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap(),
                1
            );
        }
        assert_eq!(
            Connection::open(&first[0])
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM thread_dynamic_tools WHERE thread_id = 'unrelated-a'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        assert_eq!(
            Connection::open(&first[2])
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM thread_spawn_edges
                     WHERE parent_thread_id = 'unrelated-c' AND child_thread_id = 'keep-c'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );

        let before = first
            .iter()
            .map(|path| dependent_database_snapshot(path))
            .collect::<Vec<_>>();
        super::copy_dependent_rows_for_thread_from_sources(
            &[first[1].clone(), first[0].clone(), first[2].clone()],
            &first_targets,
            "thread-x",
        )
        .unwrap();
        assert_eq!(
            before,
            first
                .iter()
                .map(|path| dependent_database_snapshot(path))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn dependent_rows_reject_same_primary_key_with_different_typed_values_before_writes() {
        let root = tempdir().unwrap();
        let account = root.path().join("account.sqlite");
        let relay = root.path().join("relay.sqlite");
        create_strict_dependent_database(&account, "BLOB");
        create_strict_dependent_database(&relay, "BLOB");
        Connection::open(&account)
            .unwrap()
            .execute(
                "INSERT INTO thread_dynamic_tools VALUES
                 ('thread-x', 1, 'search', 'tools', X'01', 1.0)",
                [],
            )
            .unwrap();
        Connection::open(&relay)
            .unwrap()
            .execute(
                "INSERT INTO thread_dynamic_tools VALUES
                 ('thread-x', 1, 'search', 'tools', X'02', 1.0)",
                [],
            )
            .unwrap();
        let before_account = fs::read(&account).unwrap();
        let before_relay = fs::read(&relay).unwrap();
        let targets = vec![
            dependent_database_entry("account", &account, DatabaseRole::CanonicalAccount),
            dependent_database_entry("relay", &relay, DatabaseRole::Relay),
        ];

        let error = super::copy_dependent_rows_for_thread_from_sources(
            &[account.clone(), relay.clone()],
            &targets,
            "thread-x",
        )
        .unwrap_err();
        assert!(error.contains("same primary key"), "{error}");
        assert_eq!(fs::read(&account).unwrap(), before_account);
        assert_eq!(fs::read(&relay).unwrap(), before_relay);
    }

    #[test]
    fn dependent_rows_reject_full_table_xinfo_schema_drift_before_writes() {
        let root = tempdir().unwrap();
        let account = root.path().join("account.sqlite");
        let relay = root.path().join("relay.sqlite");
        create_strict_dependent_database(&account, "BLOB");
        create_strict_dependent_database(&relay, "TEXT");
        let before_account = fs::read(&account).unwrap();
        let before_relay = fs::read(&relay).unwrap();
        let targets = vec![
            dependent_database_entry("account", &account, DatabaseRole::CanonicalAccount),
            dependent_database_entry("relay", &relay, DatabaseRole::Relay),
        ];

        let error = super::copy_dependent_rows_for_thread_from_sources(
            &[relay.clone(), account.clone()],
            &targets,
            "thread-x",
        )
        .unwrap_err();
        assert!(error.contains("schemas do not match"), "{error}");
        assert_eq!(fs::read(&account).unwrap(), before_account);
        assert_eq!(fs::read(&relay).unwrap(), before_relay);
    }

    #[test]
    fn dependent_rows_fail_closed_for_missing_primary_key_and_triggers() {
        let root = tempdir().unwrap();
        let no_pk = root.path().join("no-pk.sqlite");
        create_strict_dependent_database(&no_pk, "BLOB");
        Connection::open(&no_pk)
            .unwrap()
            .execute_batch(
                "DROP TABLE thread_dynamic_tools;
                 CREATE TABLE thread_dynamic_tools (
                    thread_id TEXT NOT NULL,
                    name TEXT NOT NULL
                 );
                 INSERT INTO thread_dynamic_tools VALUES ('thread-x', 'unsafe');",
            )
            .unwrap();
        let no_pk_targets = vec![dependent_database_entry(
            "no-pk",
            &no_pk,
            DatabaseRole::CanonicalAccount,
        )];
        let error = super::copy_dependent_rows_for_thread_from_sources(
            std::slice::from_ref(&no_pk),
            &no_pk_targets,
            "thread-x",
        )
        .unwrap_err();
        assert!(error.contains("no reliable primary key"), "{error}");

        let triggered = root.path().join("triggered.sqlite");
        create_strict_dependent_database(&triggered, "BLOB");
        Connection::open(&triggered)
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER dynamic_tools_audit
                 AFTER INSERT ON thread_dynamic_tools BEGIN SELECT 1; END;",
            )
            .unwrap();
        let triggered_targets = vec![dependent_database_entry(
            "triggered",
            &triggered,
            DatabaseRole::CanonicalAccount,
        )];
        let error = super::copy_dependent_rows_for_thread_from_sources(
            std::slice::from_ref(&triggered),
            &triggered_targets,
            "thread-x",
        )
        .unwrap_err();
        assert!(error.contains("unsupported triggers"), "{error}");
    }

    #[test]
    fn dependent_rows_reject_triggered_external_restore_source_even_when_target_is_clean() {
        let root = tempdir().unwrap();
        let package = root.path().join("package.sqlite");
        let account = root.path().join("account.sqlite");
        create_strict_dependent_database(&package, "BLOB");
        create_strict_dependent_database(&account, "BLOB");
        Connection::open(&package)
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER dynamic_tools_audit
                 AFTER INSERT ON thread_dynamic_tools BEGIN SELECT 1; END;",
            )
            .unwrap();
        let targets = vec![dependent_database_entry(
            "account",
            &account,
            DatabaseRole::CanonicalAccount,
        )];

        let error = super::copy_dependent_rows_for_thread_from_sources(
            &[package, account],
            &targets,
            "thread-x",
        )
        .unwrap_err();
        assert!(error.contains("unsupported triggers"), "{error}");
    }

    #[test]
    fn restore_import_unions_package_and_all_runtime_dependent_rows() {
        let root = tempdir().unwrap();
        let package = root.path().join("package.sqlite");
        let account = root.path().join("account.sqlite");
        let relay = root.path().join("relay.sqlite");
        for path in [&package, &account, &relay] {
            create_strict_dependent_database(path, "BLOB");
            Connection::open(path)
                .unwrap()
                .execute(
                    "INSERT INTO threads VALUES ('thread-x', 'old.jsonl', 'openai', 'cli')",
                    [],
                )
                .unwrap();
        }
        Connection::open(&package)
            .unwrap()
            .execute(
                "INSERT INTO thread_dynamic_tools VALUES
                 ('thread-x', 1, 'search', 'tools', X'01', 1.0)",
                [],
            )
            .unwrap();
        Connection::open(&relay)
            .unwrap()
            .execute(
                "INSERT INTO thread_spawn_edges VALUES ('thread-x', 'child-x', 'spawn')",
                [],
            )
            .unwrap();

        let canonical = root.path().join("canonical/thread-x.jsonl");
        let sessions = vec![super::MigrationSessionApplyEntry {
            thread_id: "thread-x".to_string(),
            action: MigrationSessionAction::KeepCanonical,
            source_path: canonical.clone(),
            target_path: canonical,
            staged_path: None,
            expected_sha256: "a".repeat(64),
            target_before_sha256: Some("a".repeat(64)),
            target_backup_payload: None,
        }];
        let mut databases = vec![
            dependent_database_entry("account", &account, DatabaseRole::CanonicalAccount),
            dependent_database_entry("relay", &relay, DatabaseRole::Relay),
        ];
        super::merge_restore_import_database_views(
            &sessions,
            &mut databases,
            std::slice::from_ref(&package),
            &std::collections::BTreeSet::new(),
        )
        .unwrap();

        let account_snapshot = dependent_database_snapshot(&account);
        assert_eq!(account_snapshot, dependent_database_snapshot(&relay));
        for table in ["thread_dynamic_tools", "thread_spawn_edges"] {
            assert_eq!(account_snapshot.get(table).unwrap().len(), 1);
        }
    }

    fn create_real_goals_database(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        Connection::open(path)
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
    }

    fn goals_entry(
        id: &str,
        path: &Path,
        role: DatabaseRole,
    ) -> super::MigrationDatabaseApplyEntry {
        let (bytes, sha256) = super::stable_file_digest(path).unwrap();
        super::MigrationDatabaseApplyEntry {
            database_id: format!("{id}-view-0000"),
            role,
            target_path: path.with_extension("live.sqlite"),
            staged_path: path.to_path_buf(),
            original_backup_payload: path.with_extension("backup.sqlite"),
            original_sha256: sha256.clone(),
            staged_sha256: sha256,
            staged_bytes: bytes,
        }
    }

    #[test]
    fn split_goals_three_database_union_preserves_goals_deferrals_and_unrelated_rows() {
        let root = tempdir().unwrap();
        let account = root.path().join("account-goals.sqlite");
        let relay = root.path().join("relay-goals.sqlite");
        let shared = root.path().join("shared-goals.sqlite");
        for path in [&account, &relay, &shared] {
            create_real_goals_database(path);
        }
        Connection::open(&account)
            .unwrap()
            .execute_batch(
                "INSERT INTO thread_goals VALUES
                 ('account-only','goal-a','account','active',NULL,1,2,3,4);",
            )
            .unwrap();
        Connection::open(&relay)
            .unwrap()
            .execute_batch(
                "INSERT INTO thread_goals VALUES
                 ('relay-only','goal-r','relay','blocked',5,6,7,8,9);",
            )
            .unwrap();
        Connection::open(&shared)
            .unwrap()
            .execute_batch(
                "INSERT INTO thread_goals VALUES
                 ('shared-only','goal-s','shared','active',NULL,0,0,10,11);
                 INSERT INTO thread_goal_continuation_deferrals VALUES ('shared-only');",
            )
            .unwrap();
        let mut databases = vec![
            goals_entry("goals-db-0000", &account, DatabaseRole::CanonicalAccount),
            goals_entry("goals-db-0001", &relay, DatabaseRole::Relay),
            goals_entry("goals-db-0002", &shared, DatabaseRole::Shared),
        ];

        super::merge_goals_database_views(&mut databases).unwrap();
        let merged = Connection::open(&databases[0].staged_path).unwrap();
        assert_eq!(
            merged
                .query_row("SELECT COUNT(*) FROM thread_goals", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            3
        );
        assert_eq!(
            merged
                .query_row(
                    "SELECT COUNT(*) FROM thread_goal_continuation_deferrals",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        assert!(databases
            .iter()
            .all(|database| database.staged_sha256 == databases[0].staged_sha256));
        for database in databases.iter().skip(1) {
            assert!(
                database
                    .staged_path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .len()
                    <= 32,
                "derived goals staging names must leave headroom for Windows paths"
            );
        }
        for left in 0..databases.len() {
            for right in (left + 1)..databases.len() {
                assert_ne!(databases[left].staged_path, databases[right].staged_path);
                assert!(
                    !crate::session_storage::bounded_file::same_regular_file_identity(
                        &databases[left].staged_path,
                        &databases[right].staged_path,
                    )
                    .unwrap()
                );
            }
        }
    }

    #[test]
    fn split_goals_same_primary_key_different_whole_row_fails_closed() {
        let root = tempdir().unwrap();
        let account = root.path().join("account-goals.sqlite");
        let relay = root.path().join("relay-goals.sqlite");
        for path in [&account, &relay] {
            create_real_goals_database(path);
        }
        Connection::open(&account)
            .unwrap()
            .execute_batch(
                "INSERT INTO thread_goals VALUES
                 ('same','goal','left','active',NULL,0,0,1,2);",
            )
            .unwrap();
        Connection::open(&relay)
            .unwrap()
            .execute_batch(
                "INSERT INTO thread_goals VALUES
                 ('same','goal','right','active',NULL,0,0,1,99);",
            )
            .unwrap();
        let mut databases = vec![
            goals_entry("goals-db-0000", &account, DatabaseRole::CanonicalAccount),
            goals_entry("goals-db-0001", &relay, DatabaseRole::Relay),
        ];

        let error = super::merge_goals_database_views(&mut databases).unwrap_err();
        assert!(error.contains("same primary key"), "{error}");
    }

    #[test]
    fn rollback_never_deletes_same_hash_file_with_different_identity() {
        let root = tempdir().unwrap();
        let target = root.path().join("thread.jsonl");
        fs::write(&target, b"same bytes").unwrap();
        let operation_id = "identity-test";
        let witness = crate::file_ops::ownership_witness_path(&target, operation_id).unwrap();
        fs::write(&witness, b"same bytes").unwrap();
        let (_, sha256) = super::stable_file_digest(&target).unwrap();
        let plan = super::MigrationApplyPlan {
            schema_version: super::APPLY_PLAN_SCHEMA_VERSION,
            operation_id: operation_id.to_string(),
            generated_at_ms: 1,
            canonical_root: root.path().to_path_buf(),
            inventory_fingerprint: "a".repeat(64),
            backup_dir: root.path().join("backup"),
            staging_root: root.path().join("staging"),
            sessions: Vec::new(),
            databases: Vec::new(),
            conflict_count: 0,
        };
        let step = crate::session_storage::operation_ledger::LedgerRollbackStep {
            action: crate::session_storage::operation_ledger::RollbackActionKind::RemoveCreatedFile,
            source_path: target.clone(),
            target_path: target.clone(),
            expected_sha256: Some(sha256.clone()),
            applied_sha256: Some(sha256),
            completed: false,
        };

        let error = super::remove_created_session_step(&plan, &step).unwrap_err();
        assert!(error.contains("different file"), "{error}");
        assert!(target.is_file());
        assert!(witness.is_file());
    }

    #[test]
    #[cfg(windows)]
    fn replacement_binding_digest_is_stable_across_target_existence() {
        let root = tempdir().unwrap();
        let canonical_root = root.path().join("canonical");
        let target = canonical_root.join("state_5.sqlite");
        let staging_root = root
            .path()
            .join("data/session-storage/operations/binding-stability/migration-staging");
        let plan = super::MigrationApplyPlan {
            schema_version: super::APPLY_PLAN_SCHEMA_VERSION,
            operation_id: "binding-stability".to_string(),
            generated_at_ms: 1,
            canonical_root,
            inventory_fingerprint: "a".repeat(64),
            backup_dir: root.path().join("backup"),
            staging_root: staging_root.clone(),
            sessions: Vec::new(),
            databases: vec![super::MigrationDatabaseApplyEntry {
                database_id: "account".to_string(),
                role: DatabaseRole::CanonicalAccount,
                target_path: target.clone(),
                staged_path: staging_root.join("account.sqlite.stage"),
                original_backup_payload: root.path().join("backup/account.sqlite"),
                original_sha256: "b".repeat(64),
                staged_sha256: "c".repeat(64),
                staged_bytes: 1,
            }],
            conflict_count: 0,
        };

        fs::create_dir_all(target.parent().unwrap()).unwrap();
        let bindings_before = super::replacement_recovery_bindings(&plan).unwrap();
        let digest_before = super::migration_replacement_plan_digest(&plan).unwrap();
        fs::write(&target, b"now present").unwrap();
        let bindings_after = super::replacement_recovery_bindings(&plan).unwrap();
        let digest_after = super::migration_replacement_plan_digest(&plan).unwrap();

        assert_eq!(
            bindings_before, bindings_after,
            "replacement bindings changed across absent->present: before={bindings_before:?}; after={bindings_after:?}"
        );
        assert_eq!(
            digest_before, digest_after,
            "replacement plan digest changed across absent->present: before={digest_before}; after={digest_after}"
        );
    }

    #[test]
    #[cfg(windows)]
    fn committed_cleanup_removes_only_the_exact_ownership_witness() {
        let root = tempdir().unwrap();
        let target = root.path().join("sessions/thread.jsonl");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, b"canonical session").unwrap();
        let operation_id = "committed-identity-test";
        let witness = crate::file_ops::ownership_witness_path(&target, operation_id).unwrap();
        fs::hard_link(&target, &witness).unwrap();
        let (_, sha256) = super::stable_file_digest(&target).unwrap();
        let staging_root = root.path().join("staging");
        let plan = super::MigrationApplyPlan {
            schema_version: super::APPLY_PLAN_SCHEMA_VERSION,
            operation_id: operation_id.to_string(),
            generated_at_ms: 1,
            canonical_root: root.path().to_path_buf(),
            inventory_fingerprint: "a".repeat(64),
            backup_dir: root.path().join("backup"),
            staging_root: staging_root.clone(),
            sessions: vec![super::MigrationSessionApplyEntry {
                thread_id: "thread-x".to_string(),
                action: MigrationSessionAction::CopyToCanonical,
                source_path: root.path().join("source.jsonl"),
                target_path: target.clone(),
                staged_path: Some(staging_root.join("thread.stage")),
                expected_sha256: sha256,
                target_before_sha256: None,
                target_backup_payload: None,
            }],
            databases: Vec::new(),
            conflict_count: 0,
        };

        super::cleanup_committed_migration_ownership_witnesses(&plan).unwrap();
        assert!(target.is_file());
        assert!(!witness.exists());
        super::cleanup_committed_migration_ownership_witnesses(&plan).unwrap();
        assert!(target.is_file());
    }

    #[test]
    fn plan_ready_prepares_only_staged_sessions_and_database_views() {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let data = root.path().join("data");
        let backups = root.path().join("backups");
        let operation_id = "migration-prepare-1";
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&backups).unwrap();
        let store = OperationLedgerStore::new(&data);
        store
            .create(operation_id, SessionStorageOperationKind::Migration, &home)
            .unwrap();

        let canonical_a = home.join("sessions/a.jsonl");
        let shared_a = data.join("shared-sessions/sessions/a.jsonl");
        let shared_b = data.join("shared-sessions/sessions/b.jsonl");
        let canonical_b = home.join("sessions/migrated/b.jsonl");
        write_session(&canonical_a, "thread-a", "openai", &["one"]);
        write_session(&shared_a, "thread-a", "openai_custom", &["one", "two"]);
        write_session(&shared_b, "thread-b", "openai_custom", &["only"]);
        let canonical_a_before = fs::read(&canonical_a).unwrap();

        let account_db = home.join("state_5.sqlite");
        let shared_db = data.join("shared-sessions/state_5.sqlite");
        let account_goals_db = home.join("goals_1.sqlite");
        let shared_goals_db = data.join("shared-sessions/goals_1.sqlite");
        create_database(&account_db, &[("thread-a", &canonical_a, "openai")]);
        Connection::open(&account_db)
            .unwrap()
            .execute(
                "INSERT INTO thread_spawn_edges VALUES ('thread-b', 'thread-a')",
                [],
            )
            .unwrap();
        create_database(
            &shared_db,
            &[
                ("thread-a", &shared_a, "openai_custom"),
                ("thread-b", &shared_b, "openai_custom"),
            ],
        );
        Connection::open(&shared_db)
            .unwrap()
            .execute(
                "INSERT INTO thread_spawn_edges VALUES ('thread-a', 'thread-b')",
                [],
            )
            .unwrap();
        create_real_goals_database(&account_goals_db);
        create_real_goals_database(&shared_goals_db);
        Connection::open(&account_goals_db)
            .unwrap()
            .execute_batch(
                "INSERT INTO thread_goals VALUES
                 ('thread-a','goal-account','account original','active',NULL,1,2,3,4);",
            )
            .unwrap();
        Connection::open(&shared_goals_db)
            .unwrap()
            .execute_batch(
                "INSERT INTO thread_goals VALUES
                 ('thread-b','goal-shared','shared original','blocked',5,6,7,8,9);
                 INSERT INTO thread_goal_continuation_deferrals VALUES ('thread-b');",
            )
            .unwrap();
        let account_goals_before = fs::read(&account_goals_db).unwrap();
        let shared_goals_before = fs::read(&shared_goals_db).unwrap();

        let semantic_a = read_semantic_session(&shared_a).unwrap();
        let semantic_b = read_semantic_session(&shared_b).unwrap();
        let report = MigrationPreflightReport {
            schema_version: 1,
            operation_id: operation_id.to_string(),
            generated_at_ms: 1,
            canonical_session_count: 2,
            session_file_count: 3,
            provider_copy_count: 0,
            conflict_count: 0,
            anomaly_count: 0,
            estimated_reclaim_bytes: 0,
            backup_source_bytes: 0,
            required_backup_bytes: 0,
            available_backup_bytes: u64::MAX,
            backup_destination: backups.clone(),
            blockers: Vec::new(),
            ready_for_backup: true,
            plan: CanonicalMigrationPlan {
                schema_version: 1,
                operation_id: operation_id.to_string(),
                generated_at_ms: 1,
                canonical_root: home.clone(),
                inventory_fingerprint: "a".repeat(64),
                sessions: vec![
                    MigrationSessionPlan {
                        thread_id: "thread-a".to_string(),
                        action: MigrationSessionAction::ReplaceCanonicalWithExtension,
                        retained_path: shared_a.clone(),
                        canonical_path: canonical_a.clone(),
                        retained_bytes: semantic_a.bytes,
                        retained_sha256: super::hex_digest(semantic_a.raw_sha256),
                        retained_message_count: semantic_a.message_count,
                        last_valid_message_at: None,
                        duplicates: Vec::new(),
                    },
                    MigrationSessionPlan {
                        thread_id: "thread-b".to_string(),
                        action: MigrationSessionAction::CopyToCanonical,
                        retained_path: shared_b.clone(),
                        canonical_path: canonical_b.clone(),
                        retained_bytes: semantic_b.bytes,
                        retained_sha256: super::hex_digest(semantic_b.raw_sha256),
                        retained_message_count: semantic_b.message_count,
                        last_valid_message_at: None,
                        duplicates: Vec::new(),
                    },
                ],
                conflicts: Vec::new(),
                databases: vec![
                    MigrationDatabasePlan {
                        database_id: "account".to_string(),
                        path: account_db.clone(),
                        role: DatabaseRole::CanonicalAccount,
                        reference_count: 1,
                    },
                    MigrationDatabasePlan {
                        database_id: "goals-db-main-view-0000".to_string(),
                        path: account_goals_db.clone(),
                        role: DatabaseRole::CanonicalAccount,
                        reference_count: 0,
                    },
                    MigrationDatabasePlan {
                        database_id: "goals-db-main-view-0001".to_string(),
                        path: shared_goals_db.clone(),
                        role: DatabaseRole::Shared,
                        reference_count: 0,
                    },
                    MigrationDatabasePlan {
                        database_id: "shared".to_string(),
                        path: shared_db.clone(),
                        role: DatabaseRole::Shared,
                        reference_count: 2,
                    },
                ],
                unclassified_file_count: 0,
                invalid_marker_count: 0,
                missing_runtime_reference_count: 0,
                mismatched_runtime_reference_count: 0,
            },
        };
        let sources = vec![
            MigrationBackupSource {
                source_path: canonical_a.clone(),
                payload_relative_path: "canonical/sessions/a.jsonl".into(),
                kind: MigrationBackupEntryKind::Session,
                expected_sha256: None,
                logical_thread_id: Some("thread-a".to_string()),
            },
            MigrationBackupSource {
                source_path: shared_a.clone(),
                payload_relative_path: "external-sessions/a.jsonl".into(),
                kind: MigrationBackupEntryKind::Session,
                expected_sha256: None,
                logical_thread_id: Some("thread-a".to_string()),
            },
            MigrationBackupSource {
                source_path: shared_b.clone(),
                payload_relative_path: "external-sessions/b.jsonl".into(),
                kind: MigrationBackupEntryKind::Session,
                expected_sha256: None,
                logical_thread_id: Some("thread-b".to_string()),
            },
            MigrationBackupSource {
                source_path: account_db.clone(),
                payload_relative_path: "databases/canonical-state_5.sqlite".into(),
                kind: MigrationBackupEntryKind::Database,
                expected_sha256: None,
                logical_thread_id: None,
            },
            MigrationBackupSource {
                source_path: shared_db.clone(),
                payload_relative_path: "databases/shared-state_5.sqlite".into(),
                kind: MigrationBackupEntryKind::Database,
                expected_sha256: None,
                logical_thread_id: None,
            },
            MigrationBackupSource {
                source_path: account_goals_db.clone(),
                payload_relative_path: "databases/canonical-goals_1.sqlite".into(),
                kind: MigrationBackupEntryKind::Database,
                expected_sha256: None,
                logical_thread_id: None,
            },
            MigrationBackupSource {
                source_path: shared_goals_db.clone(),
                payload_relative_path: "databases/shared-goals_1.sqlite".into(),
                kind: MigrationBackupEntryKind::Database,
                expected_sha256: None,
                logical_thread_id: None,
            },
        ];
        let backup = create_migration_backup(&backups, operation_id, &sources).unwrap();
        let backup = verify_migration_backup_with_runtime(
            &backup.backup_dir,
            &root.path().join("isolated"),
            &PassingVerifier,
        )
        .unwrap();

        let prepared = prepare_migration_apply_plan(&home, &data, &report, &backup).unwrap();
        let repeated = prepare_migration_apply_plan(&home, &data, &report, &backup).unwrap();

        assert_eq!(prepared.plan, repeated.plan);
        assert_eq!(prepared.receipt, repeated.receipt);
        let prepared_goals = prepared
            .plan
            .databases
            .iter()
            .filter(|database| super::is_goals_database_id(&database.database_id))
            .collect::<Vec<_>>();
        assert_eq!(prepared_goals.len(), 2);
        assert_ne!(
            prepared_goals[0].original_backup_payload,
            prepared_goals[1].original_backup_payload
        );
        assert!(
            !crate::session_storage::bounded_file::same_regular_file_identity(
                &prepared_goals[0].original_backup_payload,
                &prepared_goals[1].original_backup_payload,
            )
            .unwrap()
        );
        assert_eq!(
            prepared_goals[0].staged_sha256,
            prepared_goals[1].staged_sha256
        );
        assert_ne!(prepared_goals[0].staged_path, prepared_goals[1].staged_path);
        assert!(
            !crate::session_storage::bounded_file::same_regular_file_identity(
                &prepared_goals[0].staged_path,
                &prepared_goals[1].staged_path,
            )
            .unwrap()
        );
        assert_eq!(fs::read(&canonical_a).unwrap(), canonical_a_before);
        assert!(!canonical_b.exists());
        let account = Connection::open(&account_db).unwrap();
        assert_eq!(
            account
                .query_row("SELECT COUNT(*) FROM threads", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        drop(account);

        let staged_account = prepared
            .plan
            .databases
            .iter()
            .find(|database| database.database_id == "account")
            .unwrap();
        let staged = Connection::open(&staged_account.staged_path).unwrap();
        let rows = staged
            .prepare("SELECT id, rollout_path, model_provider FROM threads ORDER BY id")
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].1, canonical_a.to_string_lossy());
        assert_eq!(rows[1].1, canonical_b.to_string_lossy());
        assert_eq!(rows[1].2, "openai");
        assert_eq!(
            staged
                .query_row("SELECT COUNT(*) FROM thread_spawn_edges", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            2
        );
        assert_eq!(prepared.rollback_steps.len(), 6);
        drop(staged);

        for phase in [
            SessionStorageOperationPhase::Preflight,
            SessionStorageOperationPhase::Backup,
            SessionStorageOperationPhase::BackupVerified,
            SessionStorageOperationPhase::PlanReady,
        ] {
            store.transition(operation_id, phase).unwrap();
        }
        store
            .update(operation_id, |ledger| {
                ledger.backup_root = Some(backup.backup_dir.clone());
                ledger.rollback_steps = prepared.rollback_steps.clone();
                Ok(())
            })
            .unwrap();
        store
            .transition(operation_id, SessionStorageOperationPhase::Applying)
            .unwrap();

        let rejected = apply_prepared_migration(&prepared.plan, || {
            Err("writer appeared before live write".to_string())
        })
        .unwrap_err();
        assert_eq!(rejected, "writer appeared before live write");
        assert_eq!(fs::read(&canonical_a).unwrap(), canonical_a_before);
        assert!(!canonical_b.exists());
        assert_eq!(
            Connection::open(&account_db)
                .unwrap()
                .query_row("SELECT COUNT(*) FROM threads", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );

        let staged_b = prepared
            .plan
            .sessions
            .iter()
            .find(|session| session.thread_id == "thread-b")
            .and_then(|session| session.staged_path.as_ref())
            .unwrap();
        fs::create_dir_all(canonical_b.parent().unwrap()).unwrap();
        fs::copy(staged_b, &canonical_b).unwrap();
        let appeared = apply_prepared_migration(&prepared.plan, || Ok(())).unwrap_err();
        assert!(appeared.contains("appeared after planning"), "{appeared}");
        assert_eq!(fs::read(&canonical_a).unwrap(), canonical_a_before);
        assert_eq!(fs::read(&canonical_b).unwrap(), fs::read(staged_b).unwrap());
        fs::remove_file(&canonical_b).unwrap();

        let mut guarded_write_count = 0_usize;
        let guarded_apply = apply_prepared_migration(&prepared.plan, || {
            guarded_write_count = guarded_write_count.saturating_add(1);
            if guarded_write_count > 1 {
                for guarded_path in [
                    &canonical_a,
                    &account_db,
                    &shared_db,
                    &account_goals_db,
                    &shared_goals_db,
                ] {
                    assert!(
                        OpenOptions::new().write(true).open(guarded_path).is_err(),
                        "every live migration target must retain its writer barrier"
                    );
                }
            }
            Ok(())
        });
        assert!(
            guarded_apply.is_ok(),
            "migration apply failed after {guarded_write_count} writer-gate calls: {guarded_apply:?}"
        );
        assert_eq!(guarded_write_count, 7);
        store
            .update(operation_id, |ledger| {
                ledger.live_mutation_started = true;
                Ok(())
            })
            .unwrap();
        let canonical_b_witness =
            crate::file_ops::ownership_witness_path(&canonical_b, operation_id).unwrap();
        assert!(
            crate::session_storage::bounded_file::same_regular_file_identity(
                &canonical_b,
                &canonical_b_witness,
            )
            .unwrap()
        );
        store
            .transition(operation_id, SessionStorageOperationPhase::Validating)
            .unwrap();
        let applied = validate_applied_migration(&prepared.plan).unwrap();
        assert!(applied.validated);
        assert_eq!(applied.canonical_created_count, 1);
        assert_eq!(applied.canonical_replaced_count, 1);
        assert!(applied.runtime_verification.is_none());
        let runtime =
            verify_applied_migration_with_runtime(&prepared.plan, &PassingVerifier).unwrap();
        assert_eq!(runtime.expected_session_count, 2);
        assert_eq!(runtime.listed_session_count, 2);
        assert_eq!(
            fs::read(&canonical_a).unwrap(),
            fs::read(&shared_a).unwrap()
        );
        assert_eq!(
            fs::read(&canonical_b).unwrap(),
            fs::read(&shared_b).unwrap()
        );
        assert_eq!(
            fs::read(&account_goals_db).unwrap(),
            fs::read(&shared_goals_db).unwrap()
        );
        assert!(
            !crate::session_storage::bounded_file::same_regular_file_identity(
                &account_goals_db,
                &shared_goals_db,
            )
            .unwrap()
        );
        for goals_path in [&account_goals_db, &shared_goals_db] {
            let goals = Connection::open(goals_path).unwrap();
            assert_eq!(
                goals
                    .query_row("SELECT COUNT(*) FROM thread_goals", [], |row| {
                        row.get::<_, i64>(0)
                    })
                    .unwrap(),
                2
            );
            assert_eq!(
                goals
                    .query_row(
                        "SELECT COUNT(*) FROM thread_goal_continuation_deferrals",
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap(),
                1
            );
        }
        let applied_account = Connection::open(&account_db).unwrap();
        assert_eq!(
            applied_account
                .query_row("SELECT COUNT(*) FROM threads", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            2
        );
        drop(applied_account);

        apply_prepared_migration(&prepared.plan, || Ok(())).unwrap();
        let repeated_applied = validate_applied_migration(&prepared.plan).unwrap();
        assert_eq!(repeated_applied, applied);
        assert_eq!(
            fs::read(&canonical_a).unwrap(),
            fs::read(&shared_a).unwrap()
        );
        assert_eq!(
            fs::read(&canonical_b).unwrap(),
            fs::read(&shared_b).unwrap()
        );

        super::preflight_rollback_migration(
            &prepared.plan,
            &store.load(operation_id).unwrap().rollback_steps,
        )
        .unwrap();
        let deferred = recover_interrupted_migration(&store, &data, operation_id, || {
            Err("writer is active".to_string())
        })
        .unwrap();
        assert_eq!(
            deferred.status,
            MigrationRecoveryStatus::DeferredByLiveWriter
        );
        assert_eq!(
            store.load(operation_id).unwrap().phase,
            SessionStorageOperationPhase::RollingBack
        );
        assert_eq!(
            fs::read(&canonical_a).unwrap(),
            fs::read(&shared_a).unwrap()
        );

        let applied_canonical_a = fs::read(&canonical_a).unwrap();
        let applied_canonical_b = fs::read(&canonical_b).unwrap();
        let applied_account_database = fs::read(&account_db).unwrap();
        let replacement_phase_path =
            super::migration_replacement_phase_path(&prepared.plan).unwrap();
        let replacement_phase_before_late_writer = fs::read(&replacement_phase_path).unwrap();
        let rollback_steps_before_late_writer = store.load(operation_id).unwrap().rollback_steps;

        let late_session = b"late writer session content".to_vec();
        fs::write(&canonical_a, &late_session).unwrap();
        let session_late_writer =
            recover_interrupted_migration(&store, &data, operation_id, || Ok(())).unwrap();
        assert_eq!(session_late_writer.status, MigrationRecoveryStatus::Failed);
        assert_eq!(fs::read(&canonical_a).unwrap(), late_session);
        assert_eq!(fs::read(&canonical_b).unwrap(), applied_canonical_b);
        assert_eq!(fs::read(&account_db).unwrap(), applied_account_database);
        assert_eq!(
            fs::read(&replacement_phase_path).unwrap(),
            replacement_phase_before_late_writer
        );
        assert_eq!(
            store.load(operation_id).unwrap().rollback_steps,
            rollback_steps_before_late_writer
        );
        fs::write(&canonical_a, &applied_canonical_a).unwrap();

        let applied_shared_database = fs::read(&shared_db).unwrap();
        Connection::open(&shared_db)
            .unwrap()
            .execute_batch(
                "CREATE TABLE late_writer(value TEXT NOT NULL); \
                 INSERT INTO late_writer VALUES ('preserve me');",
            )
            .unwrap();
        let database_late_writer =
            recover_interrupted_migration(&store, &data, operation_id, || Ok(())).unwrap();
        assert_eq!(database_late_writer.status, MigrationRecoveryStatus::Failed);
        assert_eq!(fs::read(&canonical_a).unwrap(), applied_canonical_a);
        assert_eq!(fs::read(&canonical_b).unwrap(), applied_canonical_b);
        assert_eq!(fs::read(&account_db).unwrap(), applied_account_database);
        assert_eq!(
            Connection::open(&shared_db)
                .unwrap()
                .query_row("SELECT value FROM late_writer", [], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap(),
            "preserve me"
        );
        assert_eq!(
            fs::read(&replacement_phase_path).unwrap(),
            replacement_phase_before_late_writer
        );
        assert_eq!(
            store.load(operation_id).unwrap().rollback_steps,
            rollback_steps_before_late_writer
        );
        super::remove_sqlite_sidecars(&shared_db).unwrap();
        fs::write(&shared_db, &applied_shared_database).unwrap();

        super::preflight_rollback_migration(
            &prepared.plan,
            &store.load(operation_id).unwrap().rollback_steps,
        )
        .unwrap();
        let recovered =
            recover_interrupted_migration(&store, &data, operation_id, || Ok(())).unwrap();
        assert_eq!(recovered.status, MigrationRecoveryStatus::RolledBack);
        let rolled_back = recovered.rollback.unwrap();
        assert_eq!(rolled_back.completed_step_count, 6);
        assert_eq!(
            store.load(operation_id).unwrap().phase,
            SessionStorageOperationPhase::RolledBack
        );
        assert_eq!(fs::read(&canonical_a).unwrap(), canonical_a_before);
        assert!(!canonical_b.exists());
        assert!(!canonical_b_witness.exists());
        assert!(!prepared.plan.staging_root.exists());
        assert_eq!(fs::read(&account_goals_db).unwrap(), account_goals_before);
        assert_eq!(fs::read(&shared_goals_db).unwrap(), shared_goals_before);
        let restored_account = Connection::open(&account_db).unwrap();
        assert_eq!(
            restored_account
                .query_row("SELECT COUNT(*) FROM threads", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
}

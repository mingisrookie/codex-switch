use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Read},
    path::{Component, Path, PathBuf},
    thread,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    file_ops::{atomic_create, atomic_write},
    operation_log::timestamp_millis,
};

use super::{
    bounded_file::read_regular_file_bounded,
    catalog::{
        discover_database_catalog, snapshot_database_catalog, CatalogSnapshot, DatabaseDescriptor,
    },
    marker::{inspect_provider_marker, provider_marker_path},
    migration::{collect_inventory_with_supplemental_paths, MigrationInventory},
    migration_backup::{
        create_migration_backup, delete_owned_migration_backup, verify_migration_backup,
        MigrationBackupEntry, MigrationBackupEntryKind, MigrationBackupManifest,
        MigrationBackupSource,
    },
    model::{FileOrigin, MarkerStatus, SessionRelation},
    operation_ledger::{
        LedgerFileSnapshot, LedgerRollbackStep, OperationLedgerStore, RollbackActionKind,
        SessionStorageOperationKind, SessionStorageOperationPhase,
    },
    reference_graph::{path_key, SessionFileNode},
    relation::compare_sessions,
    semantic::read_semantic_session,
    storage_state::load_committed_canonical_storage_state,
    write_barrier::{DestructiveFileGuard, WriteExclusionGuard},
};

const OFFLINE_GC_SCHEMA_VERSION: u32 = 1;
const MAX_OFFLINE_GC_PLAN_BYTES: u64 = 64 * 1024 * 1024;
const OBSERVATION_DELAY: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OfflineGcCandidate {
    pub candidate_path: PathBuf,
    pub marker_path: PathBuf,
    pub canonical_path: PathBuf,
    pub body_quarantine_path: PathBuf,
    pub marker_quarantine_path: PathBuf,
    pub body_backup_payload: PathBuf,
    pub marker_backup_payload: PathBuf,
    pub body_bytes: u64,
    pub body_sha256: String,
    pub marker_bytes: u64,
    pub marker_sha256: String,
    pub canonical_sha256_at_plan: String,
    pub relation: SessionRelation,
    pub origin: FileOrigin,
    pub stable_observations: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OfflineGcPlan {
    pub schema_version: u32,
    pub operation_id: String,
    pub migration_operation_id: String,
    pub created_at_ms: u128,
    pub canonical_root: PathBuf,
    pub data_root: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_dir: Option<PathBuf>,
    pub inventory_fingerprint: String,
    pub candidates: Vec<OfflineGcCandidate>,
}

#[derive(Debug, Clone)]
pub struct PreparedOfflineGc {
    pub plan: OfflineGcPlan,
    pub plan_snapshot: LedgerFileSnapshot,
    pub rollback_steps: Vec<LedgerRollbackStep>,
}

#[derive(Debug, Clone)]
struct OfflineGcCandidateDraft {
    candidate_path: PathBuf,
    marker_path: PathBuf,
    canonical_path: PathBuf,
    body_quarantine_path: PathBuf,
    marker_quarantine_path: PathBuf,
    body_bytes: u64,
    body_sha256: String,
    marker_bytes: u64,
    marker_sha256: String,
    canonical_sha256_at_plan: String,
    relation: SessionRelation,
    origin: FileOrigin,
    thread_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeDatabaseDescriptorIdentity {
    id: String,
    path_key: String,
    role: super::model::DatabaseRole,
    rollout_root_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeDatabaseFileDigest {
    path: PathBuf,
    bytes: u64,
    sha256: String,
}

#[derive(Debug)]
struct GuardedRuntimeDatabaseFile {
    expected: RuntimeDatabaseFileDigest,
    guard: WriteExclusionGuard,
}

#[derive(Debug)]
struct GuardedRuntimeReferenceSnapshot {
    descriptor_identities: Vec<RuntimeDatabaseDescriptorIdentity>,
    catalog: CatalogSnapshot,
    database_files: Vec<GuardedRuntimeDatabaseFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OfflineGcReceipt {
    pub operation_id: String,
    pub candidate_count: usize,
    pub deleted_count: usize,
    pub reclaimed_bytes: u64,
    pub validated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OfflineGcPlanEnvelope {
    plan: OfflineGcPlan,
    integrity_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OfflineGcFailure {
    LiveWriteGuard(String),
    Operation(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfflineGcRecoveryStatus {
    RolledBack,
    DeferredByLiveWriter,
    Failed,
}

impl OfflineGcFailure {
    pub fn message(&self) -> &str {
        match self {
            Self::LiveWriteGuard(message) | Self::Operation(message) => message,
        }
    }
}

pub fn prepare_offline_gc_plan(
    codex_home: &Path,
    data_root: &Path,
    operation_id: &str,
    migration_operation_id: &str,
) -> Result<PreparedOfflineGc, String> {
    validate_operation_id(operation_id)?;
    validate_operation_id(migration_operation_id)?;
    validate_safe_directory(codex_home, "canonical root")?;
    validate_safe_directory(data_root, "managed data root")?;
    let operation_root = operation_root(data_root, operation_id)?;
    let plan_path = operation_root.join("offline-gc-plan.json");
    if plan_path.exists() {
        let plan = load_offline_gc_plan(data_root, operation_id)?;
        if plan.canonical_root != codex_home
            || plan.migration_operation_id != migration_operation_id
        {
            return Err("offline GC plan does not match the migration".to_string());
        }
        return prepared_result(plan, &plan_path);
    }

    let supplemental_paths =
        migration_gc_discovery_paths(codex_home, data_root, migration_operation_id)?;
    let first =
        collect_inventory_with_supplemental_paths(codex_home, data_root, &supplemental_paths)?;
    thread::sleep(OBSERVATION_DELAY);
    let second =
        collect_inventory_with_supplemental_paths(codex_home, data_root, &supplemental_paths)?;
    validate_inventory_pair(&first, &second)?;
    let drafts = build_candidate_drafts(operation_id, &first, &second)?;
    let (backup, candidates) = prepare_candidate_recovery(data_root, operation_id, drafts)?;
    let plan = OfflineGcPlan {
        schema_version: OFFLINE_GC_SCHEMA_VERSION,
        operation_id: operation_id.to_string(),
        migration_operation_id: migration_operation_id.to_string(),
        created_at_ms: timestamp_millis()?,
        canonical_root: codex_home.to_path_buf(),
        data_root: data_root.to_path_buf(),
        backup_dir: backup.as_ref().map(|manifest| manifest.backup_dir.clone()),
        inventory_fingerprint: second.fingerprint,
        candidates,
    };
    let result = (|| {
        write_plan(&plan_path, &plan)?;
        prepared_result(plan, &plan_path)
    })();
    if result.is_err() {
        if let Some(backup) = backup {
            let _ = delete_owned_migration_backup(&backup.backup_dir, operation_id);
        }
    }
    result
}

fn migration_gc_discovery_paths(
    codex_home: &Path,
    data_root: &Path,
    migration_operation_id: &str,
) -> Result<Vec<PathBuf>, String> {
    let state = load_committed_canonical_storage_state(data_root, codex_home)?
        .ok_or_else(|| "offline GC requires committed canonical storage state".to_string())?;
    if state.migration_operation_id != migration_operation_id {
        return Err("offline GC canonical storage identity changed".to_string());
    }
    Ok(state.gc_discovery_paths().to_vec())
}

pub fn load_offline_gc_plan(data_root: &Path, operation_id: &str) -> Result<OfflineGcPlan, String> {
    let path = operation_root(data_root, operation_id)?.join("offline-gc-plan.json");
    let bytes = read_regular_file_bounded(&path, MAX_OFFLINE_GC_PLAN_BYTES)
        .map_err(|_| "offline GC plan is unreadable".to_string())?;
    let envelope = serde_json::from_slice::<OfflineGcPlanEnvelope>(&bytes)
        .map_err(|_| "offline GC plan is invalid".to_string())?;
    validate_plan(&envelope.plan)?;
    if envelope.plan.operation_id != operation_id
        || envelope.integrity_sha256 != plan_digest(&envelope.plan)?
    {
        return Err("offline GC plan integrity check failed".to_string());
    }
    Ok(envelope.plan)
}

pub fn rollback_unapplied_offline_gc(
    store: &OperationLedgerStore,
    data_root: &Path,
    operation_id: &str,
    error_code: &str,
) -> Result<(), String> {
    let ledger = store.load(operation_id)?;
    if ledger.kind != SessionStorageOperationKind::OfflineGc || ledger.phase.is_terminal() {
        return Ok(());
    }
    if !matches!(
        ledger.phase,
        SessionStorageOperationPhase::Available
            | SessionStorageOperationPhase::Preflight
            | SessionStorageOperationPhase::Backup
            | SessionStorageOperationPhase::BackupVerified
            | SessionStorageOperationPhase::PlanReady
    ) {
        return Err("offline GC operation has already entered live mutation".to_string());
    }
    let mut residual_preserved = false;
    match load_offline_gc_plan(data_root, operation_id) {
        Ok(plan) => {
            if plan.operation_id != ledger.operation_id
                || path_key(&plan.data_root) != path_key(data_root)
                || path_key(&plan.canonical_root) != path_key(&ledger.canonical_root)
                || ledger
                    .backup_root
                    .as_ref()
                    .is_some_and(|backup_root| plan.backup_dir.as_ref() != Some(backup_root))
            {
                return Err("offline GC pre-apply plan does not match its ledger".to_string());
            }
            if let Some(backup_dir) = &plan.backup_dir {
                if delete_owned_migration_backup(backup_dir, operation_id).is_err() {
                    residual_preserved = true;
                }
            }
        }
        Err(_) => {
            // No live mutation began, so an unreadable/missing plan is safe to
            // terminalize. Preserve any unverified residue for retention and
            // diagnostics instead of guessing ownership or blocking switching.
            residual_preserved = true;
            if let Some(backup_dir) = &ledger.backup_root {
                let _ = delete_owned_migration_backup(backup_dir, operation_id);
            }
        }
    }
    store.update(operation_id, |ledger| {
        ledger.last_error_code = Some(if residual_preserved {
            format!("{error_code}ResidualPreserved")
        } else {
            error_code.to_string()
        });
        Ok(())
    })?;
    store.transition(operation_id, SessionStorageOperationPhase::RollingBack)?;
    store.transition(operation_id, SessionStorageOperationPhase::RolledBack)?;
    Ok(())
}

pub fn execute_offline_gc<Guard>(
    plan: &OfflineGcPlan,
    mut before_live_write: Guard,
) -> Result<OfflineGcReceipt, OfflineGcFailure>
where
    Guard: FnMut() -> Result<(), String>,
{
    validate_plan(plan).map_err(OfflineGcFailure::Operation)?;
    let mut runtime_references =
        acquire_runtime_reference_snapshot(plan).map_err(OfflineGcFailure::Operation)?;
    let mut deleted_count = 0_usize;
    let mut reclaimed_bytes = 0_u64;
    for candidate in &plan.candidates {
        let state = inspect_candidate_state(candidate).map_err(OfflineGcFailure::Operation)?;
        match state {
            CandidateState::Original => {
                validate_original_candidate(plan, candidate, &mut runtime_references)
                    .map_err(OfflineGcFailure::Operation)?;
                before_live_write().map_err(OfflineGcFailure::LiveWriteGuard)?;
                let mut marker_guard = DestructiveFileGuard::acquire(&candidate.marker_path)
                    .map_err(OfflineGcFailure::Operation)?;
                marker_guard
                    .verify_current_path(Some(&candidate.marker_sha256))
                    .map_err(OfflineGcFailure::Operation)?;
                let mut body_guard = DestructiveFileGuard::acquire(&candidate.candidate_path)
                    .map_err(OfflineGcFailure::Operation)?;
                body_guard
                    .verify_current_path(Some(&candidate.body_sha256))
                    .map_err(OfflineGcFailure::Operation)?;
                marker_guard
                    .rename_no_replace(&candidate.marker_quarantine_path)
                    .map_err(OfflineGcFailure::Operation)?;
                if let Err(error) = before_live_write() {
                    let _ = marker_guard.rename_no_replace(&candidate.marker_path);
                    return Err(OfflineGcFailure::LiveWriteGuard(error));
                }
                if let Err(error) = body_guard.verify_current_path(Some(&candidate.body_sha256)) {
                    let _ = marker_guard.rename_no_replace(&candidate.marker_path);
                    return Err(OfflineGcFailure::Operation(error));
                }
                if let Err(error) = body_guard.rename_no_replace(&candidate.body_quarantine_path) {
                    let _ = marker_guard.rename_no_replace(&candidate.marker_path);
                    return Err(OfflineGcFailure::Operation(error));
                }
                drop(body_guard);
                drop(marker_guard);
            }
            CandidateState::MarkerQuarantined => {
                validate_marker_quarantine(candidate).map_err(OfflineGcFailure::Operation)?;
                validate_candidate_body_and_peer(plan, candidate, &candidate.candidate_path)
                    .map_err(OfflineGcFailure::Operation)?;
                ensure_zero_runtime_references(plan, candidate, &mut runtime_references)
                    .map_err(OfflineGcFailure::Operation)?;
                before_live_write().map_err(OfflineGcFailure::LiveWriteGuard)?;
                let mut body_guard = DestructiveFileGuard::acquire(&candidate.candidate_path)
                    .map_err(OfflineGcFailure::Operation)?;
                body_guard
                    .verify_current_path(Some(&candidate.body_sha256))
                    .map_err(OfflineGcFailure::Operation)?;
                body_guard
                    .rename_no_replace(&candidate.body_quarantine_path)
                    .map_err(OfflineGcFailure::Operation)?;
                drop(body_guard);
            }
            CandidateState::Quarantined => {}
            CandidateState::Deleted => {
                validate_deleted_candidate(plan, candidate, &mut runtime_references)
                    .map_err(OfflineGcFailure::Operation)?;
                deleted_count = deleted_count.saturating_add(1);
                reclaimed_bytes = reclaimed_bytes.saturating_add(candidate.body_bytes);
                continue;
            }
        }

        validate_quarantined_candidate(plan, candidate, &mut runtime_references)
            .map_err(OfflineGcFailure::Operation)?;
        before_live_write().map_err(OfflineGcFailure::LiveWriteGuard)?;
        if candidate.marker_quarantine_path.exists() {
            let mut marker_guard = DestructiveFileGuard::acquire(&candidate.marker_quarantine_path)
                .map_err(OfflineGcFailure::Operation)?;
            marker_guard
                .verify_current_path(Some(&candidate.marker_sha256))
                .map_err(OfflineGcFailure::Operation)?;
            marker_guard.delete().map_err(OfflineGcFailure::Operation)?;
        }
        validate_quarantined_candidate(plan, candidate, &mut runtime_references)
            .map_err(OfflineGcFailure::Operation)?;
        before_live_write().map_err(OfflineGcFailure::LiveWriteGuard)?;
        let mut body_guard = DestructiveFileGuard::acquire(&candidate.body_quarantine_path)
            .map_err(OfflineGcFailure::Operation)?;
        body_guard
            .verify_current_path(Some(&candidate.body_sha256))
            .map_err(OfflineGcFailure::Operation)?;
        body_guard.delete().map_err(OfflineGcFailure::Operation)?;
        validate_deleted_candidate(plan, candidate, &mut runtime_references)
            .map_err(OfflineGcFailure::Operation)?;
        deleted_count = deleted_count.saturating_add(1);
        reclaimed_bytes = reclaimed_bytes.saturating_add(candidate.body_bytes);
    }
    Ok(OfflineGcReceipt {
        operation_id: plan.operation_id.clone(),
        candidate_count: plan.candidates.len(),
        deleted_count,
        reclaimed_bytes,
        validated: false,
    })
}

fn acquire_runtime_reference_snapshot(
    plan: &OfflineGcPlan,
) -> Result<GuardedRuntimeReferenceSnapshot, String> {
    let first = discover_database_catalog(&plan.canonical_root, &plan.data_root);
    validate_runtime_database_discovery(&first)?;
    let descriptor_identities = runtime_database_descriptor_identities(&first.descriptors);
    let before_files = snapshot_runtime_database_files(&first.descriptors)?;
    let catalog = snapshot_runtime_reference_catalog(plan, &first.descriptors)?;
    if catalog.database_errors > 0 || catalog.rows_missing_rollout_path > 0 {
        return Err("offline GC could not verify every runtime database".to_string());
    }
    validate_catalog_coverage(&first.descriptors, &catalog)?;

    let second = discover_database_catalog(&plan.canonical_root, &plan.data_root);
    validate_runtime_database_discovery(&second)?;
    if runtime_database_descriptor_identities(&second.descriptors) != descriptor_identities {
        return Err(
            "offline GC runtime database set changed while snapshotting references".to_string(),
        );
    }
    let after_files = snapshot_runtime_database_files(&second.descriptors)?;
    if after_files != before_files {
        return Err(
            "offline GC runtime database files changed while snapshotting references".to_string(),
        );
    }

    let mut database_files = Vec::with_capacity(after_files.len());
    for expected in after_files {
        let mut guard = WriteExclusionGuard::acquire(&expected.path)?;
        let actual = guard.verify_current_path(Some(&expected.sha256))?;
        if actual.0 != expected.bytes {
            return Err(
                "offline GC runtime database changed during barrier acquisition".to_string(),
            );
        }
        database_files.push(GuardedRuntimeDatabaseFile { expected, guard });
    }

    let mut snapshot = GuardedRuntimeReferenceSnapshot {
        descriptor_identities,
        catalog,
        database_files,
    };
    verify_runtime_reference_snapshot(plan, &mut snapshot)?;
    Ok(snapshot)
}

fn validate_runtime_database_discovery(
    discovery: &super::catalog::CatalogDiscovery,
) -> Result<(), String> {
    if discovery.errors > 0 || discovery.goals_errors > 0 {
        Err("offline GC runtime database discovery is incomplete".to_string())
    } else {
        Ok(())
    }
}

fn runtime_database_descriptor_identities(
    descriptors: &[DatabaseDescriptor],
) -> Vec<RuntimeDatabaseDescriptorIdentity> {
    descriptors
        .iter()
        .map(|descriptor| RuntimeDatabaseDescriptorIdentity {
            id: descriptor.id.clone(),
            path_key: path_key(&descriptor.path),
            role: descriptor.role,
            rollout_root_key: path_key(&descriptor.rollout_root),
        })
        .collect()
}

fn validate_catalog_coverage(
    descriptors: &[DatabaseDescriptor],
    catalog: &CatalogSnapshot,
) -> Result<(), String> {
    let expected = descriptors
        .iter()
        .map(|descriptor| {
            (
                descriptor.id.clone(),
                path_key(&descriptor.path),
                descriptor.role,
            )
        })
        .collect::<Vec<_>>();
    let actual = catalog
        .databases
        .iter()
        .filter_map(|database| {
            database
                .path
                .as_ref()
                .map(|path| (database.id.clone(), path_key(path), database.role))
        })
        .collect::<Vec<_>>();
    if actual == expected {
        Ok(())
    } else {
        Err("offline GC runtime database catalog is incomplete".to_string())
    }
}

fn snapshot_runtime_reference_catalog(
    plan: &OfflineGcPlan,
    descriptors: &[DatabaseDescriptor],
) -> Result<CatalogSnapshot, String> {
    let staging_root =
        operation_root(&plan.data_root, &plan.operation_id)?.join("runtime-reference-snapshot");
    match fs::create_dir(&staging_root) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return Err("offline GC runtime reference staging already exists".to_string())
        }
        Err(_) => return Err("failed to create offline GC runtime reference staging".to_string()),
    }
    validate_safe_directory(&staging_root, "runtime reference staging")?;
    let result = snapshot_database_catalog(descriptors, &staging_root);
    let cleanup = fs::remove_dir(&staging_root);
    match cleanup {
        Ok(()) => Ok(result),
        Err(_) => Err("offline GC runtime reference staging cleanup failed".to_string()),
    }
}

fn snapshot_runtime_database_files(
    descriptors: &[DatabaseDescriptor],
) -> Result<Vec<RuntimeDatabaseFileDigest>, String> {
    runtime_database_file_paths(descriptors)?
        .into_iter()
        .map(|path| {
            let (bytes, sha256) = stable_file_digest(&path)?;
            Ok(RuntimeDatabaseFileDigest {
                path,
                bytes,
                sha256,
            })
        })
        .collect()
}

fn runtime_database_file_paths(descriptors: &[DatabaseDescriptor]) -> Result<Vec<PathBuf>, String> {
    let mut paths = BTreeMap::<String, PathBuf>::new();
    for descriptor in descriptors {
        add_runtime_database_file(&mut paths, descriptor.path.clone(), true)?;
        for suffix in ["-wal", "-journal"] {
            let mut sidecar = descriptor.path.as_os_str().to_os_string();
            sidecar.push(suffix);
            add_runtime_database_file(&mut paths, PathBuf::from(sidecar), false)?;
        }
    }
    Ok(paths.into_values().collect())
}

fn add_runtime_database_file(
    paths: &mut BTreeMap<String, PathBuf>,
    path: PathBuf,
    required: bool,
) -> Result<(), String> {
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() && !metadata_is_link_or_reparse(&metadata) => {
            paths.entry(path_key(&path)).or_insert(path);
            Ok(())
        }
        Ok(_) => Err("offline GC runtime database file is unsafe".to_string()),
        Err(error) if error.kind() == io::ErrorKind::NotFound && !required => Ok(()),
        Err(_) => Err("offline GC runtime database file is unavailable".to_string()),
    }
}

fn verify_runtime_reference_snapshot(
    plan: &OfflineGcPlan,
    snapshot: &mut GuardedRuntimeReferenceSnapshot,
) -> Result<(), String> {
    let discovery = discover_database_catalog(&plan.canonical_root, &plan.data_root);
    validate_runtime_database_discovery(&discovery)?;
    if runtime_database_descriptor_identities(&discovery.descriptors)
        != snapshot.descriptor_identities
    {
        return Err("offline GC runtime database set changed during the delete window".to_string());
    }
    let current_paths = runtime_database_file_paths(&discovery.descriptors)?;
    let expected_paths = snapshot
        .database_files
        .iter()
        .map(|database| path_key(&database.expected.path))
        .collect::<Vec<_>>();
    let current_paths = current_paths
        .iter()
        .map(|path| path_key(path))
        .collect::<Vec<_>>();
    if current_paths != expected_paths {
        return Err(
            "offline GC runtime database files changed during the delete window".to_string(),
        );
    }
    for database in &mut snapshot.database_files {
        let actual = database
            .guard
            .verify_current_path(Some(&database.expected.sha256))?;
        if actual.0 != database.expected.bytes {
            return Err("offline GC runtime database changed during the delete window".to_string());
        }
    }
    Ok(())
}

pub fn validate_offline_gc(
    plan: &OfflineGcPlan,
    mut receipt: OfflineGcReceipt,
) -> Result<OfflineGcReceipt, String> {
    validate_plan(plan)?;
    if receipt.operation_id != plan.operation_id
        || receipt.candidate_count != plan.candidates.len()
        || receipt.deleted_count != plan.candidates.len()
    {
        return Err("offline GC receipt does not match its plan".to_string());
    }
    let mut runtime_references = acquire_runtime_reference_snapshot(plan)?;
    for candidate in &plan.candidates {
        validate_deleted_candidate(plan, candidate, &mut runtime_references)?;
    }
    receipt.validated = true;
    Ok(receipt)
}

pub fn rollback_offline_gc<Guard>(
    store: &OperationLedgerStore,
    plan: &OfflineGcPlan,
    mut before_live_write: Guard,
) -> Result<(), OfflineGcFailure>
where
    Guard: FnMut() -> Result<(), String>,
{
    validate_plan(plan).map_err(OfflineGcFailure::Operation)?;
    let ledger = store
        .load(&plan.operation_id)
        .map_err(OfflineGcFailure::Operation)?;
    if ledger.phase != SessionStorageOperationPhase::RollingBack
        || !rollback_plan_matches(&ledger.rollback_steps, &rollback_steps(plan))
    {
        return Err(OfflineGcFailure::Operation(
            "offline GC rollback plan changed".to_string(),
        ));
    }
    for (index, step) in ledger.rollback_steps.iter().cloned().enumerate() {
        if step.completed {
            continue;
        }
        before_live_write().map_err(OfflineGcFailure::LiveWriteGuard)?;
        restore_gc_file(plan, &step).map_err(OfflineGcFailure::Operation)?;
        store
            .update(&plan.operation_id, |ledger| {
                let current = ledger
                    .rollback_steps
                    .get_mut(index)
                    .ok_or_else(|| "offline GC rollback ledger changed".to_string())?;
                if current != &step {
                    return Err("offline GC rollback ledger changed".to_string());
                }
                current.completed = true;
                Ok(())
            })
            .map_err(OfflineGcFailure::Operation)?;
    }
    for candidate in &plan.candidates {
        validate_restored_candidate(candidate).map_err(OfflineGcFailure::Operation)?;
    }
    Ok(())
}

pub fn recover_interrupted_offline_gc<Guard>(
    store: &OperationLedgerStore,
    data_root: &Path,
    operation_id: &str,
    before_live_write: Guard,
) -> Result<OfflineGcRecoveryStatus, String>
where
    Guard: FnMut() -> Result<(), String>,
{
    let mut ledger = store.load(operation_id)?;
    if ledger.kind != SessionStorageOperationKind::OfflineGc
        || !matches!(
            ledger.phase,
            SessionStorageOperationPhase::Applying
                | SessionStorageOperationPhase::Validating
                | SessionStorageOperationPhase::RollingBack
        )
    {
        return Err("session storage operation is not an interrupted offline GC".to_string());
    }
    if matches!(
        ledger.phase,
        SessionStorageOperationPhase::Applying | SessionStorageOperationPhase::Validating
    ) {
        ledger = store.transition(operation_id, SessionStorageOperationPhase::RollingBack)?;
    }
    let plan = match load_offline_gc_plan(data_root, operation_id) {
        Ok(plan)
            if plan.canonical_root == ledger.canonical_root
                && ledger.backup_root == plan.backup_dir
                && rollback_plan_matches(&ledger.rollback_steps, &rollback_steps(&plan)) =>
        {
            plan
        }
        Ok(_) | Err(_) => {
            mark_offline_gc_recovery_retry(store, operation_id, "offlineGcRollbackPlanInvalid")?;
            return Ok(OfflineGcRecoveryStatus::Failed);
        }
    };
    match rollback_offline_gc(store, &plan, before_live_write) {
        Ok(()) => {
            store.transition(operation_id, SessionStorageOperationPhase::RolledBack)?;
            Ok(OfflineGcRecoveryStatus::RolledBack)
        }
        Err(OfflineGcFailure::LiveWriteGuard(_)) => {
            store.update(operation_id, |ledger| {
                ledger.last_error_code = Some("offlineGcRollbackWriterActive".to_string());
                Ok(())
            })?;
            Ok(OfflineGcRecoveryStatus::DeferredByLiveWriter)
        }
        Err(OfflineGcFailure::Operation(_)) => {
            mark_offline_gc_recovery_retry(store, operation_id, "offlineGcRollbackRetryRequired")?;
            Ok(OfflineGcRecoveryStatus::Failed)
        }
    }
}

fn mark_offline_gc_recovery_retry(
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

fn prepared_result(plan: OfflineGcPlan, plan_path: &Path) -> Result<PreparedOfflineGc, String> {
    validate_plan(&plan)?;
    let (bytes, sha256) = stable_file_digest(plan_path)?;
    Ok(PreparedOfflineGc {
        rollback_steps: rollback_steps(&plan),
        plan,
        plan_snapshot: LedgerFileSnapshot {
            path: plan_path.to_path_buf(),
            bytes,
            sha256,
            created_by_operation: true,
            logical_thread_id: None,
        },
    })
}

fn validate_inventory_pair(
    first: &MigrationInventory,
    second: &MigrationInventory,
) -> Result<(), String> {
    if first.fingerprint != second.fingerprint {
        return Err("offline GC inventory changed between observations".to_string());
    }
    for inventory in [first, second] {
        if inventory.database_discovery_errors > 0
            || inventory.session_discovery_errors > 0
            || inventory.catalog.database_errors > 0
            || inventory.catalog.rows_missing_rollout_path > 0
        {
            return Err("offline GC inventory is incomplete".to_string());
        }
    }
    Ok(())
}

fn build_candidate_drafts(
    operation_id: &str,
    first: &MigrationInventory,
    second: &MigrationInventory,
) -> Result<Vec<OfflineGcCandidateDraft>, String> {
    let first_nodes = first
        .graph
        .files
        .iter()
        .map(|node| (node.path_key.clone(), node))
        .collect::<BTreeMap<_, _>>();
    let retained_by_thread = second
        .graph
        .files
        .iter()
        .filter_map(|node| {
            if node.retained_candidate && node.is_canonical {
                node.thread_id
                    .as_ref()
                    .map(|thread_id| (thread_id.clone(), node))
            } else {
                None
            }
        })
        .collect::<BTreeMap<_, _>>();
    let mut candidates = Vec::new();
    for node in &second.graph.files {
        if !is_safe_candidate_node(node) {
            continue;
        }
        let Some(thread_id) = node.thread_id.as_ref() else {
            continue;
        };
        let Some(canonical) = retained_by_thread.get(thread_id).copied() else {
            continue;
        };
        if path_key(&node.path) == path_key(&canonical.path) {
            continue;
        }
        let Some(first_node) = first_nodes.get(&node.path_key).copied() else {
            continue;
        };
        if !same_stable_candidate(first_node, node) {
            continue;
        }
        let body_bytes = node.observed_bytes.unwrap_or(0);
        let body_sha256 = node
            .raw_sha256
            .map(hex_digest)
            .ok_or_else(|| "offline GC candidate has no checksum".to_string())?;
        if body_bytes == 0 {
            continue;
        }
        let marker_path = provider_marker_path(&node.path)
            .map_err(|_| "offline GC candidate marker path is invalid".to_string())?;
        let (marker_bytes, marker_sha256) = stable_file_digest(&marker_path)?;
        if marker_bytes == 0 {
            continue;
        }
        let canonical_sha256 = canonical
            .raw_sha256
            .map(hex_digest)
            .ok_or_else(|| "offline GC canonical peer has no checksum".to_string())?;
        let (body_quarantine_path, marker_quarantine_path) =
            quarantine_paths(operation_id, &node.path, &marker_path)?;
        if body_quarantine_path.exists() || marker_quarantine_path.exists() {
            return Err("offline GC quarantine path already exists".to_string());
        }
        candidates.push(OfflineGcCandidateDraft {
            candidate_path: node.path.clone(),
            marker_path,
            canonical_path: canonical.path.clone(),
            body_quarantine_path,
            marker_quarantine_path,
            body_bytes,
            body_sha256,
            marker_bytes,
            marker_sha256,
            canonical_sha256_at_plan: canonical_sha256,
            relation: node
                .relation_to_retained
                .ok_or_else(|| "offline GC candidate relation is missing".to_string())?,
            origin: node.origin,
            thread_id: thread_id.clone(),
        });
    }
    candidates.sort_by_key(|candidate| path_key(&candidate.candidate_path));
    Ok(candidates)
}

fn prepare_candidate_recovery(
    data_root: &Path,
    operation_id: &str,
    drafts: Vec<OfflineGcCandidateDraft>,
) -> Result<(Option<MigrationBackupManifest>, Vec<OfflineGcCandidate>), String> {
    if drafts.is_empty() {
        return Ok((None, Vec::new()));
    }
    let recovery_parent = data_root.join("session-storage-v1/offline-gc-recovery");
    fs::create_dir_all(&recovery_parent)
        .map_err(|_| "failed to create offline GC recovery directory".to_string())?;
    validate_safe_directory(&recovery_parent, "recovery directory")?;
    let mut sources = Vec::with_capacity(drafts.len().saturating_mul(2));
    for (index, draft) in drafts.iter().enumerate() {
        sources.push(MigrationBackupSource {
            source_path: draft.candidate_path.clone(),
            payload_relative_path: PathBuf::from(format!("sessions/{index:04}.jsonl")),
            kind: MigrationBackupEntryKind::Session,
            expected_sha256: Some(draft.body_sha256.clone()),
            logical_thread_id: Some(draft.thread_id.clone()),
        });
        sources.push(MigrationBackupSource {
            source_path: draft.marker_path.clone(),
            payload_relative_path: PathBuf::from(format!("markers/{index:04}.json")),
            kind: MigrationBackupEntryKind::StorageMetadata,
            expected_sha256: Some(draft.marker_sha256.clone()),
            logical_thread_id: None,
        });
    }
    let backup = create_migration_backup(&recovery_parent, operation_id, &sources)?;
    let verified = verify_migration_backup(&backup.backup_dir)?;
    if verified != backup {
        let _ = delete_owned_migration_backup(&backup.backup_dir, operation_id);
        return Err("offline GC recovery backup identity changed".to_string());
    }
    let candidates = drafts
        .into_iter()
        .map(|draft| {
            let body_entry = backup_entry(
                &backup,
                &draft.candidate_path,
                MigrationBackupEntryKind::Session,
            )?;
            let marker_entry = backup_entry(
                &backup,
                &draft.marker_path,
                MigrationBackupEntryKind::StorageMetadata,
            )?;
            if body_entry.bytes != draft.body_bytes
                || body_entry.sha256 != draft.body_sha256
                || marker_entry.bytes != draft.marker_bytes
                || marker_entry.sha256 != draft.marker_sha256
            {
                return Err("offline GC recovery backup does not match its candidate".to_string());
            }
            Ok(OfflineGcCandidate {
                candidate_path: draft.candidate_path,
                marker_path: draft.marker_path,
                canonical_path: draft.canonical_path,
                body_quarantine_path: draft.body_quarantine_path,
                marker_quarantine_path: draft.marker_quarantine_path,
                body_backup_payload: backup_payload_path(&backup, body_entry)?,
                marker_backup_payload: backup_payload_path(&backup, marker_entry)?,
                body_bytes: draft.body_bytes,
                body_sha256: draft.body_sha256,
                marker_bytes: draft.marker_bytes,
                marker_sha256: draft.marker_sha256,
                canonical_sha256_at_plan: draft.canonical_sha256_at_plan,
                relation: draft.relation,
                origin: draft.origin,
                stable_observations: 2,
            })
        })
        .collect::<Result<Vec<_>, String>>();
    match candidates {
        Ok(candidates) => Ok((Some(backup), candidates)),
        Err(error) => {
            let _ = delete_owned_migration_backup(&backup.backup_dir, operation_id);
            Err(error)
        }
    }
}

fn is_safe_candidate_node(node: &SessionFileNode) -> bool {
    node.marker_status == MarkerStatus::Valid
        && node.is_switch_provider_slot
        && !node.retained_candidate
        && node.runtime_database_ids.is_empty()
        && matches!(
            node.relation_to_retained,
            Some(
                SessionRelation::Equal
                    | SessionRelation::EqualExceptProvider
                    | SessionRelation::LeftPrefix
            )
        )
        && matches!(
            node.origin,
            FileOrigin::CanonicalHome | FileOrigin::Shared | FileOrigin::ReferencedExternal
        )
}

fn same_stable_candidate(first: &SessionFileNode, second: &SessionFileNode) -> bool {
    first.thread_id == second.thread_id
        && first.marker_status == MarkerStatus::Valid
        && first.runtime_database_ids.is_empty()
        && first.observed_bytes == second.observed_bytes
        && first.raw_sha256 == second.raw_sha256
        && first.relation_to_retained == second.relation_to_retained
}

fn validate_original_candidate(
    plan: &OfflineGcPlan,
    candidate: &OfflineGcCandidate,
    runtime_references: &mut GuardedRuntimeReferenceSnapshot,
) -> Result<(), String> {
    let semantic = read_semantic_session(&candidate.candidate_path)
        .map_err(|_| "offline GC candidate session is invalid".to_string())?;
    let (bytes, sha256) = stable_file_digest(&candidate.candidate_path)?;
    if bytes != candidate.body_bytes
        || sha256 != candidate.body_sha256
        || inspect_provider_marker(&candidate.candidate_path, Some(&semantic))
            != MarkerStatus::Valid
    {
        return Err("offline GC candidate changed after planning".to_string());
    }
    let (marker_bytes, marker_sha256) = stable_file_digest(&candidate.marker_path)?;
    if marker_bytes != candidate.marker_bytes || marker_sha256 != candidate.marker_sha256 {
        return Err("offline GC candidate marker changed after planning".to_string());
    }
    validate_candidate_body_and_peer(plan, candidate, &candidate.candidate_path)?;
    ensure_zero_runtime_references(plan, candidate, runtime_references)
}

fn validate_marker_quarantine(candidate: &OfflineGcCandidate) -> Result<(), String> {
    let (bytes, sha256) = stable_file_digest(&candidate.marker_quarantine_path)?;
    if bytes == candidate.marker_bytes && sha256 == candidate.marker_sha256 {
        Ok(())
    } else {
        Err("offline GC quarantined marker changed".to_string())
    }
}

fn validate_quarantined_candidate(
    plan: &OfflineGcPlan,
    candidate: &OfflineGcCandidate,
    runtime_references: &mut GuardedRuntimeReferenceSnapshot,
) -> Result<(), String> {
    if candidate.candidate_path.exists() || candidate.marker_path.exists() {
        return Err("offline GC candidate quarantine is incomplete".to_string());
    }
    validate_candidate_body_and_peer(plan, candidate, &candidate.body_quarantine_path)?;
    if candidate.marker_quarantine_path.exists() {
        validate_marker_quarantine(candidate)?;
    }
    ensure_zero_runtime_references(plan, candidate, runtime_references)
}

fn validate_deleted_candidate(
    plan: &OfflineGcPlan,
    candidate: &OfflineGcCandidate,
    runtime_references: &mut GuardedRuntimeReferenceSnapshot,
) -> Result<(), String> {
    for path in [
        &candidate.candidate_path,
        &candidate.marker_path,
        &candidate.body_quarantine_path,
        &candidate.marker_quarantine_path,
    ] {
        if path.exists() {
            return Err("offline GC candidate deletion is incomplete".to_string());
        }
    }
    validate_canonical_peer(candidate, None)?;
    ensure_zero_runtime_references(plan, candidate, runtime_references)
}

fn validate_candidate_body_and_peer(
    _plan: &OfflineGcPlan,
    candidate: &OfflineGcCandidate,
    body_path: &Path,
) -> Result<(), String> {
    let (bytes, sha256) = stable_file_digest(body_path)?;
    if bytes != candidate.body_bytes || sha256 != candidate.body_sha256 {
        return Err("offline GC candidate body changed".to_string());
    }
    let candidate_semantic = read_semantic_session(body_path)
        .map_err(|_| "offline GC candidate body is invalid".to_string())?;
    validate_canonical_peer(candidate, Some(&candidate_semantic))
}

fn validate_canonical_peer(
    candidate: &OfflineGcCandidate,
    candidate_semantic: Option<&super::semantic::SemanticSession>,
) -> Result<(), String> {
    let canonical = read_semantic_session(&candidate.canonical_path)
        .map_err(|_| "offline GC canonical peer is invalid".to_string())?;
    let owned_candidate;
    let candidate_semantic = match candidate_semantic {
        Some(semantic) => semantic,
        None => {
            let backup = read_semantic_session(&candidate.body_backup_payload)
                .map_err(|_| "offline GC backup candidate is invalid".to_string())?;
            owned_candidate = backup;
            &owned_candidate
        }
    };
    if candidate_semantic.thread_id != canonical.thread_id
        || !matches!(
            compare_sessions(candidate_semantic, &canonical),
            SessionRelation::Equal
                | SessionRelation::EqualExceptProvider
                | SessionRelation::LeftPrefix
        )
    {
        return Err("offline GC canonical peer no longer preserves the candidate".to_string());
    }
    Ok(())
}

fn ensure_zero_runtime_references(
    plan: &OfflineGcPlan,
    candidate: &OfflineGcCandidate,
    runtime_references: &mut GuardedRuntimeReferenceSnapshot,
) -> Result<(), String> {
    verify_runtime_reference_snapshot(plan, runtime_references)?;
    let candidate_key = path_key(&candidate.candidate_path);
    let quarantine_key = path_key(&candidate.body_quarantine_path);
    let referenced = runtime_references
        .catalog
        .databases
        .iter()
        .filter(|database| database.role.is_runtime())
        .flat_map(|database| &database.references)
        .any(|reference| {
            let key = path_key(&reference.rollout_path);
            key == candidate_key || key == quarantine_key
        });
    if referenced {
        Err("offline GC candidate gained a runtime database reference".to_string())
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateState {
    Original,
    MarkerQuarantined,
    Quarantined,
    Deleted,
}

fn inspect_candidate_state(candidate: &OfflineGcCandidate) -> Result<CandidateState, String> {
    let body = candidate.candidate_path.exists();
    let marker = candidate.marker_path.exists();
    let body_quarantine = candidate.body_quarantine_path.exists();
    let marker_quarantine = candidate.marker_quarantine_path.exists();
    match (body, marker, body_quarantine, marker_quarantine) {
        (true, true, false, false) => Ok(CandidateState::Original),
        (true, false, false, true) => Ok(CandidateState::MarkerQuarantined),
        (false, false, true, true) | (false, false, true, false) => Ok(CandidateState::Quarantined),
        (false, false, false, false) => Ok(CandidateState::Deleted),
        _ => Err("offline GC candidate has an inconsistent filesystem state".to_string()),
    }
}

fn rollback_steps(plan: &OfflineGcPlan) -> Vec<LedgerRollbackStep> {
    let mut steps = Vec::with_capacity(plan.candidates.len().saturating_mul(2));
    for candidate in &plan.candidates {
        steps.push(LedgerRollbackStep {
            action: RollbackActionKind::RestoreFile,
            source_path: candidate.body_backup_payload.clone(),
            target_path: candidate.candidate_path.clone(),
            expected_sha256: Some(candidate.body_sha256.clone()),
            applied_sha256: None,
            completed: false,
        });
        steps.push(LedgerRollbackStep {
            action: RollbackActionKind::RestoreFile,
            source_path: candidate.marker_backup_payload.clone(),
            target_path: candidate.marker_path.clone(),
            expected_sha256: Some(candidate.marker_sha256.clone()),
            applied_sha256: None,
            completed: false,
        });
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

fn restore_gc_file(plan: &OfflineGcPlan, step: &LedgerRollbackStep) -> Result<(), String> {
    let expected = step
        .expected_sha256
        .as_deref()
        .ok_or_else(|| "offline GC rollback checksum is missing".to_string())?;
    let (_, source_sha256) = stable_file_digest(&step.source_path)?;
    if source_sha256 != expected {
        return Err("offline GC rollback backup changed".to_string());
    }
    let quarantine = quarantine_for_target(plan, &step.target_path)?;
    let mut restored_guard = match DestructiveFileGuard::acquire(&step.target_path) {
        Ok(mut guard) => {
            guard.verify_current_path(Some(expected))?;
            Some(guard)
        }
        Err(_) => None,
    };

    if restored_guard.is_none() {
        match DestructiveFileGuard::acquire(quarantine) {
            Ok(mut quarantine_guard) => {
                quarantine_guard.verify_current_path(Some(expected))?;
                quarantine_guard.rename_no_replace(&step.target_path)?;
                restored_guard = Some(quarantine_guard);
            }
            Err(_) => {
                let created = atomic_create(&step.target_path, |target| {
                    let mut source = File::open(&step.source_path)
                        .map_err(|_| "offline GC rollback backup is unavailable".to_string())?;
                    io::copy(&mut source, target)
                        .map_err(|_| "offline GC rollback copy failed".to_string())?;
                    Ok(())
                })?;
                let mut guard = DestructiveFileGuard::acquire(&step.target_path)
                    .map_err(|_| "offline GC rollback target could not be guarded".to_string())?;
                guard.verify_current_path(Some(expected)).map_err(|_| {
                    if created {
                        "offline GC rollback copy verification failed".to_string()
                    } else {
                        "offline GC rollback target changed".to_string()
                    }
                })?;
                restored_guard = Some(guard);
            }
        }
    }

    if let Ok(mut quarantine_guard) = DestructiveFileGuard::acquire(quarantine) {
        quarantine_guard.verify_current_path(Some(expected))?;
        quarantine_guard.delete()?;
    }
    restored_guard
        .as_mut()
        .ok_or_else(|| "offline GC rollback target is unavailable".to_string())?
        .verify_current_path(Some(expected))?;
    Ok(())
}

fn quarantine_for_target<'a>(plan: &'a OfflineGcPlan, target: &Path) -> Result<&'a Path, String> {
    for candidate in &plan.candidates {
        if path_key(&candidate.candidate_path) == path_key(target) {
            return Ok(&candidate.body_quarantine_path);
        }
        if path_key(&candidate.marker_path) == path_key(target) {
            return Ok(&candidate.marker_quarantine_path);
        }
    }
    Err("offline GC rollback target is not in the plan".to_string())
}

fn validate_restored_candidate(candidate: &OfflineGcCandidate) -> Result<(), String> {
    for (path, bytes, sha256) in [
        (
            &candidate.candidate_path,
            candidate.body_bytes,
            &candidate.body_sha256,
        ),
        (
            &candidate.marker_path,
            candidate.marker_bytes,
            &candidate.marker_sha256,
        ),
    ] {
        let (actual_bytes, actual_sha256) = stable_file_digest(path)?;
        if actual_bytes != bytes || &actual_sha256 != sha256 {
            return Err("offline GC rollback did not restore the original file".to_string());
        }
    }
    if candidate.body_quarantine_path.exists() || candidate.marker_quarantine_path.exists() {
        return Err("offline GC rollback left a quarantine file".to_string());
    }
    let semantic = read_semantic_session(&candidate.candidate_path)
        .map_err(|_| "offline GC rollback session is invalid".to_string())?;
    if inspect_provider_marker(&candidate.candidate_path, Some(&semantic)) != MarkerStatus::Valid {
        return Err("offline GC rollback marker is invalid".to_string());
    }
    Ok(())
}

fn backup_entry<'a>(
    backup: &'a MigrationBackupManifest,
    source: &Path,
    kind: MigrationBackupEntryKind,
) -> Result<&'a MigrationBackupEntry, String> {
    let key = path_key(source);
    backup
        .entries
        .iter()
        .find(|entry| entry.kind == kind && path_key(&entry.source_path) == key)
        .ok_or_else(|| "migration backup does not contain an offline GC source".to_string())
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
        Err("offline GC backup payload changed".to_string())
    }
}

fn quarantine_paths(
    operation_id: &str,
    candidate_path: &Path,
    marker_path: &Path,
) -> Result<(PathBuf, PathBuf), String> {
    let parent = candidate_path
        .parent()
        .ok_or_else(|| "offline GC candidate has no parent".to_string())?;
    if marker_path.parent() != Some(parent) {
        return Err("offline GC marker is not beside its candidate".to_string());
    }
    let digest = hex_digest(Sha256::digest(
        format!("{}\0{}", operation_id, path_key(candidate_path)).as_bytes(),
    ));
    Ok((
        parent.join(format!(".codex-switch-gc-{}.body", &digest[..24])),
        parent.join(format!(".codex-switch-gc-{}.marker", &digest[..24])),
    ))
}

fn write_plan(path: &Path, plan: &OfflineGcPlan) -> Result<(), String> {
    validate_plan(plan)?;
    let envelope = OfflineGcPlanEnvelope {
        plan: plan.clone(),
        integrity_sha256: plan_digest(plan)?,
    };
    let bytes = serde_json::to_vec_pretty(&envelope)
        .map_err(|_| "failed to serialize offline GC plan".to_string())?;
    if bytes.len() as u64 > MAX_OFFLINE_GC_PLAN_BYTES {
        return Err("offline GC plan reached its size limit".to_string());
    }
    atomic_write(path, &bytes)?;
    if load_plan_from_path(path)? == *plan {
        Ok(())
    } else {
        Err("offline GC plan verification failed".to_string())
    }
}

fn load_plan_from_path(path: &Path) -> Result<OfflineGcPlan, String> {
    let bytes = read_regular_file_bounded(path, MAX_OFFLINE_GC_PLAN_BYTES)
        .map_err(|_| "offline GC plan is unreadable".to_string())?;
    let envelope = serde_json::from_slice::<OfflineGcPlanEnvelope>(&bytes)
        .map_err(|_| "offline GC plan is invalid".to_string())?;
    validate_plan(&envelope.plan)?;
    if envelope.integrity_sha256 != plan_digest(&envelope.plan)? {
        return Err("offline GC plan integrity check failed".to_string());
    }
    Ok(envelope.plan)
}

fn validate_plan(plan: &OfflineGcPlan) -> Result<(), String> {
    if plan.schema_version != OFFLINE_GC_SCHEMA_VERSION {
        return Err("offline GC plan version is unsupported".to_string());
    }
    validate_operation_id(&plan.operation_id)?;
    validate_operation_id(&plan.migration_operation_id)?;
    for path in [&plan.canonical_root, &plan.data_root] {
        if !path.is_absolute() {
            return Err("offline GC plan root is invalid".to_string());
        }
    }
    if plan.candidates.is_empty() != plan.backup_dir.is_none() {
        return Err("offline GC recovery backup shape is invalid".to_string());
    }
    if let Some(backup_dir) = &plan.backup_dir {
        if !backup_dir.is_absolute() {
            return Err("offline GC recovery backup path is invalid".to_string());
        }
        let backup = verify_migration_backup(backup_dir)?;
        if backup.operation_id != plan.operation_id {
            return Err("offline GC recovery backup identity changed".to_string());
        }
    }
    validate_sha256(&plan.inventory_fingerprint)?;
    for candidate in &plan.candidates {
        for path in [
            &candidate.candidate_path,
            &candidate.marker_path,
            &candidate.canonical_path,
            &candidate.body_quarantine_path,
            &candidate.marker_quarantine_path,
            &candidate.body_backup_payload,
            &candidate.marker_backup_payload,
        ] {
            if !path.is_absolute() {
                return Err("offline GC candidate path is invalid".to_string());
            }
        }
        if provider_marker_path(&candidate.candidate_path)
            .ok()
            .as_ref()
            != Some(&candidate.marker_path)
            || candidate.body_quarantine_path.parent() != candidate.candidate_path.parent()
            || candidate.marker_quarantine_path.parent() != candidate.candidate_path.parent()
            || !candidate.canonical_path.starts_with(&plan.canonical_root)
            || path_key(&candidate.candidate_path) == path_key(&candidate.canonical_path)
            || plan.backup_dir.as_ref().is_none_or(|backup_dir| {
                !candidate.body_backup_payload.starts_with(backup_dir)
                    || !candidate.marker_backup_payload.starts_with(backup_dir)
            })
            || candidate.body_bytes == 0
            || candidate.marker_bytes == 0
            || candidate.stable_observations < 2
            || !matches!(
                candidate.relation,
                SessionRelation::Equal
                    | SessionRelation::EqualExceptProvider
                    | SessionRelation::LeftPrefix
            )
            || !matches!(
                candidate.origin,
                FileOrigin::CanonicalHome | FileOrigin::Shared | FileOrigin::ReferencedExternal
            )
        {
            return Err("offline GC candidate is invalid".to_string());
        }
        for sha256 in [
            &candidate.body_sha256,
            &candidate.marker_sha256,
            &candidate.canonical_sha256_at_plan,
        ] {
            validate_sha256(sha256)?;
        }
    }
    Ok(())
}

fn plan_digest(plan: &OfflineGcPlan) -> Result<String, String> {
    serde_json::to_vec(plan)
        .map(Sha256::digest)
        .map(hex_digest)
        .map_err(|_| "failed to serialize offline GC plan".to_string())
}

fn operation_root(data_root: &Path, operation_id: &str) -> Result<PathBuf, String> {
    validate_operation_id(operation_id)?;
    if !data_root.is_absolute() {
        return Err("offline GC managed data root is invalid".to_string());
    }
    let root = data_root
        .join("session-storage-v1/operations")
        .join(operation_id);
    validate_safe_directory(&root, "operation root")?;
    Ok(root)
}

fn validate_operation_id(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 160
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        Err("offline GC operation identity is invalid".to_string())
    } else {
        Ok(())
    }
}

fn validate_relative_path(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || !path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        Err("offline GC backup path is invalid".to_string())
    } else {
        Ok(())
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
        Err("offline GC checksum is invalid".to_string())
    }
}

fn validate_safe_directory(path: &Path, label: &str) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(format!("offline GC {label} path is invalid"));
    }
    let metadata =
        fs::symlink_metadata(path).map_err(|_| format!("offline GC {label} is unavailable"))?;
    if !metadata.is_dir() || metadata_is_link_or_reparse(&metadata) {
        return Err(format!("offline GC {label} is unsafe"));
    }
    Ok(())
}

fn stable_file_digest(path: &Path) -> Result<(u64, String), String> {
    let before_path =
        fs::symlink_metadata(path).map_err(|_| "offline GC file is unavailable".to_string())?;
    if !before_path.is_file() || metadata_is_link_or_reparse(&before_path) {
        return Err("offline GC file is unsafe".to_string());
    }
    let mut file = open_regular_file(path)?;
    let before_handle = file
        .metadata()
        .map_err(|_| "offline GC file is unreadable".to_string())?;
    if file_stamp(&before_handle) != file_stamp(&before_path) {
        return Err("offline GC file changed before hashing".to_string());
    }
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|_| "failed to hash offline GC file".to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let after_handle = file
        .metadata()
        .map_err(|_| "offline GC file is unreadable".to_string())?;
    let after_path =
        fs::symlink_metadata(path).map_err(|_| "offline GC file is unavailable".to_string())?;
    if file_stamp(&before_handle) != file_stamp(&after_handle)
        || file_stamp(&before_handle) != file_stamp(&after_path)
    {
        return Err("offline GC file changed while hashing".to_string());
    }
    Ok((after_handle.len(), hex_digest(hasher.finalize())))
}

fn open_regular_file(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options
        .open(path)
        .map_err(|_| "offline GC file is unreadable".to_string())
}

fn file_stamp(metadata: &fs::Metadata) -> (u64, Option<std::time::SystemTime>) {
    (metadata.len(), metadata.modified().ok())
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
        fs,
        fs::{File, OpenOptions},
        path::Path,
    };

    use rusqlite::Connection;
    use sha2::{Digest, Sha256};
    use tempfile::{tempdir, TempDir};

    use super::{
        execute_offline_gc, prepare_offline_gc_plan, recover_interrupted_offline_gc,
        validate_offline_gc, OfflineGcFailure, OfflineGcRecoveryStatus,
    };
    use crate::session_storage::{
        marker::{inspect_provider_marker, provider_marker_path},
        migration::{persist_migration_preflight, run_migration_preflight},
        model::MarkerStatus,
        operation_ledger::{
            OperationLedgerStore, SessionStorageOperationKind, SessionStorageOperationPhase,
        },
        semantic::read_semantic_session,
        storage_state::{
            finalize_canonical_storage_state, load_committed_canonical_storage_state,
            prepare_canonical_storage_state,
        },
    };

    const MIGRATION_OPERATION_ID: &str = "migration-source-1";
    const GC_OPERATION_ID: &str = "offline-gc-1";

    struct Fixture {
        _root: TempDir,
        home: std::path::PathBuf,
        data: std::path::PathBuf,
        canonical: std::path::PathBuf,
        slot: std::path::PathBuf,
        marker: std::path::PathBuf,
        database: std::path::PathBuf,
    }

    fn fixture() -> Fixture {
        fixture_with_finalized_state(true)
    }

    fn fixture_with_finalized_state(finalize_state: bool) -> Fixture {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let data = root.path().join("data");
        let canonical = home.join("sessions/2026/08/12/canonical.jsonl");
        let slot = data.join("account-a/sessions/2026/08/12/provider.jsonl");
        write_session(&canonical, "openai", &["one", "two"]);
        write_session(&slot, "openai_custom", &["one"]);
        let marker = write_marker(&slot, "openai_custom");
        fs::write(
            home.join("config.toml"),
            format!("sqlite_home = {:?}\n", home.to_string_lossy()),
        )
        .unwrap();
        let database = home.join("state_5.sqlite");
        create_database(&database, &canonical);
        create_goals_database(&home.join("goals_1.sqlite"));
        let relay_database = data.join("relay-sqlite/state_5.sqlite");
        create_database(&relay_database, &slot);
        create_goals_database(&data.join("relay-sqlite/goals_1.sqlite"));
        let migration_store = OperationLedgerStore::new(&data);
        migration_store
            .create(
                MIGRATION_OPERATION_ID,
                SessionStorageOperationKind::Migration,
                &home,
            )
            .unwrap();
        let backup_destination = root.path().join("migration-backup");
        fs::create_dir_all(&backup_destination).unwrap();
        let preflight =
            run_migration_preflight(&home, &data, MIGRATION_OPERATION_ID, &backup_destination)
                .unwrap();
        assert!(preflight.ready_for_backup, "{:?}", preflight.blockers);
        persist_migration_preflight(&data, &preflight).unwrap();
        migration_store
            .update(MIGRATION_OPERATION_ID, |ledger| {
                ledger.backup_root =
                    Some(preflight.backup_destination.join(MIGRATION_OPERATION_ID));
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
            migration_store
                .transition(MIGRATION_OPERATION_ID, phase)
                .unwrap();
        }
        prepare_canonical_storage_state(
            &data,
            &home,
            MIGRATION_OPERATION_ID,
            &preflight.plan.inventory_fingerprint,
        )
        .unwrap();
        migration_store
            .transition(
                MIGRATION_OPERATION_ID,
                SessionStorageOperationPhase::Committed,
            )
            .unwrap();
        if finalize_state {
            let finalized =
                finalize_canonical_storage_state(&data, &home, MIGRATION_OPERATION_ID).unwrap();
            assert_eq!(
                finalize_canonical_storage_state(&data, &home, MIGRATION_OPERATION_ID).unwrap(),
                finalized
            );
        }
        Connection::open(&relay_database)
            .unwrap()
            .execute(
                "UPDATE threads SET rollout_path = ?1 WHERE id = 'thread-a'",
                [canonical.to_string_lossy().to_string()],
            )
            .unwrap();
        Fixture {
            _root: root,
            home,
            data,
            canonical,
            slot,
            marker,
            database,
        }
    }

    fn prepare(fixture: &Fixture) -> (OperationLedgerStore, super::PreparedOfflineGc) {
        let store = OperationLedgerStore::new(&fixture.data);
        store
            .create(
                GC_OPERATION_ID,
                SessionStorageOperationKind::OfflineGc,
                &fixture.home,
            )
            .unwrap();
        let prepared = prepare_offline_gc_plan(
            &fixture.home,
            &fixture.data,
            GC_OPERATION_ID,
            MIGRATION_OPERATION_ID,
        )
        .unwrap();
        (store, prepared)
    }

    fn arm_ledger(store: &OperationLedgerStore, prepared: &super::PreparedOfflineGc) {
        for phase in [
            SessionStorageOperationPhase::Preflight,
            SessionStorageOperationPhase::Backup,
            SessionStorageOperationPhase::BackupVerified,
            SessionStorageOperationPhase::PlanReady,
        ] {
            store.transition(GC_OPERATION_ID, phase).unwrap();
        }
        store
            .update(GC_OPERATION_ID, |ledger| {
                ledger.backup_root = prepared.plan.backup_dir.clone();
                ledger.created_files.push(prepared.plan_snapshot.clone());
                ledger.rollback_steps = prepared.rollback_steps.clone();
                Ok(())
            })
            .unwrap();
        store
            .transition(GC_OPERATION_ID, SessionStorageOperationPhase::Applying)
            .unwrap();
    }

    fn write_session(path: &Path, provider: &str, messages: &[&str]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut lines = vec![serde_json::json!({
            "type": "session_meta",
            "payload": {"id": "thread-a", "model_provider": provider}
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

    fn write_marker(slot: &Path, provider: &str) -> std::path::PathBuf {
        let body = fs::read(slot).unwrap();
        let marker = provider_marker_path(slot).unwrap();
        fs::write(
            &marker,
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "threadId": "thread-a",
                "providerId": provider,
                "slotFileName": slot.file_name().unwrap().to_string_lossy(),
                "originRelativePath": null,
                "originProvider": "openai",
                "createdBytes": body.len(),
                "createdSha256": format!("{:x}", Sha256::digest(&body)),
            }))
            .unwrap(),
        )
        .unwrap();
        marker
    }

    fn create_database(path: &Path, rollout_path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE threads (
                    id TEXT PRIMARY KEY,
                    rollout_path TEXT NOT NULL,
                    model_provider TEXT NOT NULL
                 );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO threads VALUES ('thread-a', ?1, 'openai')",
                [rollout_path.to_string_lossy().to_string()],
            )
            .unwrap();
    }

    fn create_goals_database(path: &Path) {
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

    #[test]
    fn deletes_only_a_twice_stable_unreferenced_marked_prefix() {
        let fixture = fixture();
        let (store, prepared) = prepare(&fixture);
        assert_eq!(prepared.plan.candidates.len(), 1);
        assert_eq!(prepared.plan.candidates[0].stable_observations, 2);
        arm_ledger(&store, &prepared);

        let receipt = execute_offline_gc(&prepared.plan, || Ok(())).unwrap();
        store
            .transition(GC_OPERATION_ID, SessionStorageOperationPhase::Validating)
            .unwrap();
        let receipt = validate_offline_gc(&prepared.plan, receipt).unwrap();
        store
            .transition(GC_OPERATION_ID, SessionStorageOperationPhase::Committed)
            .unwrap();

        assert!(receipt.validated);
        assert_eq!(receipt.deleted_count, 1);
        assert_eq!(
            receipt.reclaimed_bytes,
            prepared.plan.candidates[0].body_bytes
        );
        assert!(!fixture.slot.exists());
        assert!(!fixture.marker.exists());
        assert!(fixture.canonical.is_file());
    }

    #[test]
    fn committed_state_keeps_gc_discovery_working_after_migration_proof_expiry() {
        let fixture = fixture();
        fs::remove_dir_all(
            fixture
                .data
                .join("session-storage-v1/operations")
                .join(MIGRATION_OPERATION_ID),
        )
        .unwrap();

        let (_store, prepared) = prepare(&fixture);
        assert_eq!(prepared.plan.migration_operation_id, MIGRATION_OPERATION_ID);
        assert_eq!(prepared.plan.candidates.len(), 1);
        assert_eq!(prepared.plan.candidates[0].candidate_path, fixture.slot);
    }

    #[test]
    fn committed_prepared_state_is_finalized_idempotently_before_gc() {
        // Simulate a crash after the ledger commit but before the command can
        // replace the Prepared state with its long-lived v2 certificate.
        let fixture = fixture_with_finalized_state(false);

        // No explicit finalize call is made here: the first v2 consumer must
        // recover Prepared + Committed proof and publish the certificate.
        let (_store, prepared) = prepare(&fixture);
        assert_eq!(prepared.plan.candidates.len(), 1);
        let recovered = load_committed_canonical_storage_state(&fixture.data, &fixture.home)
            .unwrap()
            .unwrap();
        assert_eq!(recovered.migration_operation_id, MIGRATION_OPERATION_ID);
        assert_eq!(
            finalize_canonical_storage_state(&fixture.data, &fixture.home, MIGRATION_OPERATION_ID,)
                .unwrap(),
            recovered
        );
    }

    #[test]
    fn a_live_write_guard_failure_rolls_quarantine_back_from_the_verified_backup() {
        let fixture = fixture();
        let slot_before = fs::read(&fixture.slot).unwrap();
        let marker_before = fs::read(&fixture.marker).unwrap();
        let (store, prepared) = prepare(&fixture);
        arm_ledger(&store, &prepared);
        let mut guard_count = 0_usize;

        let failure = execute_offline_gc(&prepared.plan, || {
            guard_count = guard_count.saturating_add(1);
            if guard_count == 3 {
                Err("writer appeared".to_string())
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        assert!(matches!(failure, OfflineGcFailure::LiveWriteGuard(_)));
        assert!(!fixture.slot.exists());
        let deferred =
            recover_interrupted_offline_gc(&store, &fixture.data, GC_OPERATION_ID, || {
                Err("writer is active".to_string())
            })
            .unwrap();
        assert_eq!(deferred, OfflineGcRecoveryStatus::DeferredByLiveWriter);
        assert_eq!(
            store.load(GC_OPERATION_ID).unwrap().phase,
            SessionStorageOperationPhase::RollingBack
        );
        let recovered =
            recover_interrupted_offline_gc(&store, &fixture.data, GC_OPERATION_ID, || Ok(()))
                .unwrap();
        assert_eq!(recovered, OfflineGcRecoveryStatus::RolledBack);

        assert_eq!(fs::read(&fixture.slot).unwrap(), slot_before);
        assert_eq!(fs::read(&fixture.marker).unwrap(), marker_before);
        let semantic = read_semantic_session(&fixture.slot).unwrap();
        assert_eq!(
            inspect_provider_marker(&fixture.slot, Some(&semantic)),
            MarkerStatus::Valid
        );
    }

    #[test]
    fn a_reference_added_after_planning_blocks_deletion() {
        let fixture = fixture();
        let (_store, prepared) = prepare(&fixture);
        Connection::open(&fixture.database)
            .unwrap()
            .execute(
                "UPDATE threads SET rollout_path = ?1 WHERE id = 'thread-a'",
                [fixture.slot.to_string_lossy().to_string()],
            )
            .unwrap();

        let failure = execute_offline_gc(&prepared.plan, || Ok(())).unwrap_err();
        assert!(matches!(failure, OfflineGcFailure::Operation(_)));
        assert!(fixture.slot.is_file());
        assert!(fixture.marker.is_file());
    }

    #[test]
    fn an_offline_known_runtime_database_blocks_planning_before_any_delete() {
        let fixture = fixture();
        fs::remove_file(fixture.data.join("relay-sqlite/state_5.sqlite")).unwrap();
        let store = OperationLedgerStore::new(&fixture.data);
        store
            .create(
                GC_OPERATION_ID,
                SessionStorageOperationKind::OfflineGc,
                &fixture.home,
            )
            .unwrap();

        let error = prepare_offline_gc_plan(
            &fixture.home,
            &fixture.data,
            GC_OPERATION_ID,
            MIGRATION_OPERATION_ID,
        )
        .unwrap_err();

        assert!(
            error.contains("inventory") || error.contains("database"),
            "unexpected fail-closed error: {error}"
        );
        assert!(fixture.slot.is_file());
        assert!(fixture.marker.is_file());
    }

    #[test]
    fn a_candidate_changed_after_planning_blocks_deletion() {
        let fixture = fixture();
        let (_store, prepared) = prepare(&fixture);
        write_session(&fixture.slot, "openai_custom", &["one", "different"]);

        let failure = execute_offline_gc(&prepared.plan, || Ok(())).unwrap_err();
        assert!(matches!(failure, OfflineGcFailure::Operation(_)));
        assert!(fixture.slot.is_file());
    }

    #[cfg(windows)]
    #[test]
    fn runtime_database_barriers_reject_writers_through_the_delete_window() {
        let fixture = fixture();
        let (_store, prepared) = prepare(&fixture);
        let mut guard_checks = 0_usize;

        let receipt = execute_offline_gc(&prepared.plan, || {
            guard_checks = guard_checks.saturating_add(1);
            let writer = OpenOptions::new().write(true).open(&fixture.database);
            assert!(
                writer.is_err(),
                "the runtime database barrier must refuse a concurrent writer"
            );
            Ok(())
        })
        .unwrap();

        assert!(guard_checks > 0);
        assert_eq!(receipt.deleted_count, 1);
        assert!(!fixture.slot.exists());
    }

    #[cfg(windows)]
    #[test]
    fn an_active_file_handle_blocks_deletion() {
        let fixture = fixture();
        let (_store, prepared) = prepare(&fixture);
        let _active = File::open(&fixture.slot).unwrap();

        let failure = execute_offline_gc(&prepared.plan, || Ok(())).unwrap_err();
        assert!(matches!(failure, OfflineGcFailure::Operation(_)));
        assert!(fixture.slot.is_file());
        assert!(fixture.marker.is_file());
    }
}

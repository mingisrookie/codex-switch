use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Component, Path, PathBuf},
    thread,
    time::Duration,
};

use rusqlite::{Connection, OpenFlags, MAIN_DB};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::{
    file_ops::{atomic_copy, atomic_create_with_witness, atomic_write, ownership_witness_path},
    operation_log::timestamp_millis,
};

use super::{
    bounded_file::{read_regular_file_bounded, same_regular_file_identity},
    catalog::{discover_database_catalog, goals_database_digest},
    downgrade::{
        load_downgrade_manifest_baseline, DowngradeExportManifest, DowngradePackageFileKind,
    },
    legacy_backup::PendingRecoverySource,
    migration::{
        collect_inventory, MigrationInventory, MigrationInventoryFile, MigrationSessionAction,
    },
    migration_apply::{
        merge_goals_database_views, merge_restore_import_database_views, snapshot_sqlite_database,
        sqlite_sidecars_absent, stable_file_digest, MigrationApplyPlan,
        MigrationDatabaseApplyEntry, MigrationSessionApplyEntry,
    },
    migration_backup::MigrationRuntimeVerification,
    model::{DatabaseRole, FileOrigin, SessionRelation, SESSION_STORAGE_SCHEMA_VERSION},
    operation_ledger::{
        LedgerDatabaseSnapshot, LedgerFileSnapshot, LedgerRollbackStep, OperationLedgerStore,
        RollbackActionKind, SessionStorageOperationKind, SessionStorageOperationPhase,
    },
    reference_graph::path_key,
    relation::compare_sessions,
    semantic::{read_semantic_session, SemanticSession},
    write_barrier::{
        classify_handle_replace_crash_state, parent_directory_identity_at_path,
        recover_handle_replace, DestructiveFileGuard, HandleReplaceCrashState,
        HandleReplaceIdentityBindings, HandleReplacePaths, HandleReplaceRecoveryDecision,
        RegularFileIdentity, WriteExclusionGuard,
    },
};

const RESTORE_IMPORT_SCHEMA_VERSION: u32 = 1;
const MAX_PLAN_BYTES: u64 = 64 * 1024 * 1024;
const MAX_REPLACEMENT_PHASE_BYTES: u64 = 1024 * 1024;
const REPLACEMENT_PHASE_CIPHERTEXT_MAGIC: &[u8] = b"CS-RESTORE-REPLACE-PHASE-1\0";
const OBSERVATION_DELAY: Duration = Duration::from_millis(250);
const RECOVERY_RETENTION_MS: u128 = 7 * 24 * 60 * 60 * 1000;
const WORK_MARKER_NAME: &str = ".codex-switch-restore-import-v1";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RestoreImportSessionAction {
    Unchanged,
    KeepCanonical,
    ImportNew,
    ImportExtension,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RestoreImportSourceKind {
    #[default]
    DowngradePackage,
    PendingRecovery,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RestoreImportSessionPlan {
    pub thread_id: String,
    pub action: RestoreImportSessionAction,
    pub source_path: PathBuf,
    pub source_staged_path: Option<PathBuf>,
    pub source_database_snapshot: PathBuf,
    pub source_bytes: u64,
    pub source_sha256: String,
    pub source_message_count: usize,
    pub source_last_message_at: Option<String>,
    pub source_provider: Option<String>,
    pub baseline_sha256: Option<String>,
    pub canonical_path: PathBuf,
    pub canonical_before_sha256: Option<String>,
    pub canonical_backup_payload: Option<PathBuf>,
    pub relation: Option<SessionRelation>,
    #[serde(default)]
    pub synthesize_database_row: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RestoreImportConflictPlan {
    pub thread_id: String,
    pub current_path: Option<PathBuf>,
    pub current_sha256: Option<String>,
    pub candidate_paths: Vec<PathBuf>,
    pub candidate_sha256: Vec<String>,
    pub recovery_paths: Vec<PathBuf>,
    pub relation: SessionRelation,
    pub reason: String,
    pub default_overwrite: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RestoreImportUnclassifiedReason {
    RecoveryPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RestoreImportUnclassifiedPlan {
    pub source_path: PathBuf,
    pub source_bytes: u64,
    pub source_sha256: String,
    pub recovery_path: PathBuf,
    pub reason: RestoreImportUnclassifiedReason,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RestoreImportSourceDatabase {
    pub source_path: PathBuf,
    pub snapshot_path: PathBuf,
    pub sha256: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RestoreImportReplacementKind {
    SessionExtension,
    RuntimeDatabase,
}

/// A durable, target-local replacement identity.
///
/// `replacement_witness_path` is created no-clobber immediately before the
/// two-phase handle replacement. The published target is a hard link to that
/// exact object, so startup recovery can distinguish this operation's bytes
/// from an equal-hash contender. `staging_path` is the deterministic name
/// from which the exact witness object is published. `recovery_path` owns the
/// exact previous target object and `tombstone_path` receives the exact
/// published object during rollback. `original_witness_path` is a hard link
/// to the planned original target and binds rollback to that exact identity.
/// All five names are deterministic functions of
/// `operation_id + target_path` and are persisted before live mutation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RestoreImportReplacementPlan {
    pub kind: RestoreImportReplacementKind,
    pub target_path: PathBuf,
    pub source_path: PathBuf,
    /// Exact raw bytes named by `target_path` before the operation. This is
    /// intentionally distinct from a SQLite online-backup digest: SQLite may
    /// emit a semantically equivalent snapshot with different page bytes.
    pub live_original_sha256: String,
    /// Rollback payload digest. For session files it equals
    /// `live_original_sha256`; for runtime SQLite it binds the online snapshot.
    pub rollback_snapshot_sha256: String,
    pub replacement_sha256: String,
    pub original_witness_path: PathBuf,
    pub replacement_witness_path: PathBuf,
    pub staging_path: PathBuf,
    pub recovery_path: PathBuf,
    pub tombstone_path: PathBuf,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum RestoreImportReplacementPhase {
    Planned,
    Staging,
    Staged,
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RestoreImportFileIdentity {
    volume_id: u64,
    file_id: u64,
}

impl From<RegularFileIdentity> for RestoreImportFileIdentity {
    fn from(identity: RegularFileIdentity) -> Self {
        Self {
            volume_id: identity.volume_serial_number,
            file_id: identity.file_index,
        }
    }
}

impl From<RestoreImportFileIdentity> for RegularFileIdentity {
    fn from(identity: RestoreImportFileIdentity) -> Self {
        Self {
            volume_serial_number: identity.volume_id,
            file_index: identity.file_id,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RestoreImportReplacementPhaseEntry {
    replacement: RestoreImportReplacementPlan,
    parent_identity: RestoreImportFileIdentity,
    original_identity: RestoreImportFileIdentity,
    replacement_identity: Option<RestoreImportFileIdentity>,
    phase: RestoreImportReplacementPhase,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RestoreImportReplacementPhaseRecord {
    schema_version: u32,
    operation_id: String,
    plan_integrity_sha256: String,
    updated_at_ms: u128,
    replacements: Vec<RestoreImportReplacementPhaseEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RestoreImportReplacementPhaseEnvelope {
    record: RestoreImportReplacementPhaseRecord,
    integrity_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RestoreImportPlan {
    pub schema_version: u32,
    pub operation_id: String,
    pub generated_at_ms: u128,
    pub package_operation_id: String,
    pub target_version: String,
    #[serde(default)]
    pub source_kind: RestoreImportSourceKind,
    pub package_dir: PathBuf,
    pub canonical_root: PathBuf,
    pub data_root: PathBuf,
    pub source_fingerprint: String,
    pub work_root: PathBuf,
    pub staging_root: PathBuf,
    pub recovery_root: PathBuf,
    pub recovery_expires_at_ms: u128,
    pub sessions: Vec<RestoreImportSessionPlan>,
    pub conflicts: Vec<RestoreImportConflictPlan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unclassified_payloads: Vec<RestoreImportUnclassifiedPlan>,
    pub source_databases: Vec<RestoreImportSourceDatabase>,
    pub databases: Vec<MigrationDatabaseApplyEntry>,
    pub anomaly_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RestoreImportPlanEnvelope {
    plan: RestoreImportPlan,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    replacements: Vec<RestoreImportReplacementPlan>,
    integrity_sha256: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RestoreImportReceipt {
    pub operation_id: String,
    pub package_operation_id: String,
    pub target_version: String,
    pub package_dir: PathBuf,
    pub scanned_session_count: usize,
    pub unchanged_session_count: usize,
    pub current_ahead_session_count: usize,
    pub imported_new_session_count: usize,
    pub imported_extension_count: usize,
    pub conflict_count: usize,
    pub unclassified_recovery_count: usize,
    pub unclassified_recovery_bytes: u64,
    /// Paths relative to `data_root`; never serialize absolute user paths here.
    pub unclassified_recovery_paths: Vec<PathBuf>,
    pub anomaly_count: usize,
    pub database_view_count: usize,
    pub imported_bytes: u64,
    pub recovery_expires_at_ms: u128,
    pub validated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_verification: Option<MigrationRuntimeVerification>,
}

#[derive(Debug, Clone)]
pub struct PreparedRestoreImport {
    pub plan: RestoreImportPlan,
    pub created_files: Vec<LedgerFileSnapshot>,
    pub database_snapshots: Vec<LedgerDatabaseSnapshot>,
    pub rollback_steps: Vec<LedgerRollbackStep>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreImportRecoveryStatus {
    RolledBack,
    DeferredByLiveWriter,
    Failed,
}

#[derive(Debug)]
pub enum RestoreImportApplyFailure {
    Precondition(String),
    Operation(String),
}

impl RestoreImportApplyFailure {
    pub fn message(&self) -> &str {
        match self {
            Self::Precondition(message) | Self::Operation(message) => message,
        }
    }
}

#[derive(Debug)]
struct PackageCandidate {
    path: PathBuf,
    semantic: SemanticSession,
    source_database_snapshot: PathBuf,
    synthesize_database_row: bool,
}

#[derive(Debug)]
struct PackageUnknownCandidate {
    path: PathBuf,
    bytes: u64,
    sha256: String,
    semantic: bool,
    fallback_source_database_snapshot: Option<PathBuf>,
}

#[derive(Debug)]
struct PackageUnclassifiedCandidate {
    path: PathBuf,
    bytes: u64,
    sha256: String,
    reason: RestoreImportUnclassifiedReason,
}

#[derive(Debug)]
struct SourceScan {
    candidates: BTreeMap<String, Vec<PackageCandidate>>,
    unknown_candidates: BTreeMap<String, Vec<PackageUnknownCandidate>>,
    unclassified_candidates: Vec<PackageUnclassifiedCandidate>,
    anomaly_count: usize,
    fingerprint: String,
}

pub fn prepare_restore_import(
    codex_home: &Path,
    data_root: &Path,
    package_dir: &Path,
    operation_id: &str,
) -> Result<PreparedRestoreImport, String> {
    validate_absolute_directory(codex_home, "canonical root")?;
    validate_absolute_directory(data_root, "data root")?;
    validate_absolute_directory(package_dir, "downgrade package")?;
    if package_dir.starts_with(codex_home) || codex_home.starts_with(package_dir) {
        return Err("downgrade package overlaps the canonical root".to_string());
    }
    validate_operation_id(operation_id)?;
    let manifest = load_downgrade_manifest_baseline(package_dir)?;
    let work_root = operation_root(data_root, operation_id)?;
    let staging_root = work_root.join("restore-import-staging");
    let recovery_root = data_root
        .join("session-storage-v1/restore-import-recovery")
        .join(operation_id);
    if staging_root.exists() || recovery_root.exists() {
        return Err("an interrupted restore import must be recovered first".to_string());
    }
    create_safe_directory(&staging_root)?;
    create_safe_directory(&recovery_root)?;
    write_work_marker(&staging_root, operation_id, package_dir)?;
    write_work_marker(&recovery_root, operation_id, package_dir)?;

    let result = (|| {
        let first_source_databases = snapshot_source_databases(
            package_dir,
            &manifest,
            &staging_root.join("source-databases-first"),
        )?;
        let first = scan_package(package_dir, &manifest, &first_source_databases)?;
        thread::sleep(OBSERVATION_DELAY);
        let source_databases = snapshot_source_databases(
            package_dir,
            &manifest,
            &staging_root.join("source-databases"),
        )?;
        let second = scan_package(package_dir, &manifest, &source_databases)?;
        if first.fingerprint != second.fingerprint {
            return Err("downgrade package changed during restore import preflight".to_string());
        }
        let inventory = collect_inventory(codex_home, data_root)?;
        if inventory
            .database_discovery_errors
            .saturating_sub(inventory.goals_database_discovery_errors)
            > 0
            || inventory.session_discovery_errors > 0
        {
            return Err("canonical inventory is incomplete".to_string());
        }
        let generated_at_ms = timestamp_millis()?;
        let recovery_expires_at_ms = generated_at_ms
            .checked_add(RECOVERY_RETENTION_MS)
            .ok_or_else(|| "restore import retention timestamp overflowed".to_string())?;
        let baseline = baseline_hashes_by_thread(&manifest);
        let (mut sessions, mut conflicts, unclassified_payloads, anomaly_count) =
            classify_sessions(
                codex_home,
                &baseline,
                &inventory,
                second,
                &staging_root,
                &recovery_root,
            )?;
        let source_goals = source_databases
            .iter()
            .filter(|database| {
                database
                    .source_path
                    .file_name()
                    .is_some_and(|name| name == "goals_1.sqlite")
            })
            .map(|database| database.snapshot_path.clone())
            .collect::<Vec<_>>();
        let (mut databases, database_snapshots) = prepare_runtime_databases(
            codex_home,
            data_root,
            &staging_root,
            &recovery_root,
            &source_goals,
        )?;
        let migration_sessions = sessions
            .iter()
            .filter_map(to_migration_session_entry)
            .collect::<Result<Vec<_>, _>>()?;
        let source_paths = source_databases
            .iter()
            .filter(|database| {
                database
                    .source_path
                    .file_name()
                    .is_some_and(|name| name == "state_5.sqlite")
            })
            .map(|database| database.snapshot_path.clone())
            .collect::<Vec<_>>();
        let synthetic_thread_ids = sessions
            .iter()
            .filter(|session| session.synthesize_database_row)
            .map(|session| session.thread_id.clone())
            .collect::<BTreeSet<_>>();
        merge_restore_import_runtime_views(
            &migration_sessions,
            &mut databases,
            &source_paths,
            &source_goals,
            &synthetic_thread_ids,
        )?;
        let replacements = build_restore_import_replacements(operation_id, &sessions, &databases)?;
        validate_replacement_paths_unoccupied(&replacements)?;
        sessions.sort_by(|left, right| left.thread_id.cmp(&right.thread_id));
        conflicts.sort_by(|left, right| left.thread_id.cmp(&right.thread_id));
        let plan = RestoreImportPlan {
            schema_version: RESTORE_IMPORT_SCHEMA_VERSION,
            operation_id: operation_id.to_string(),
            generated_at_ms,
            package_operation_id: manifest.operation_id,
            target_version: manifest.target.version,
            source_kind: RestoreImportSourceKind::DowngradePackage,
            package_dir: package_dir.to_path_buf(),
            canonical_root: codex_home.to_path_buf(),
            data_root: data_root.to_path_buf(),
            source_fingerprint: first.fingerprint,
            work_root: work_root.clone(),
            staging_root: staging_root.clone(),
            recovery_root: recovery_root.clone(),
            recovery_expires_at_ms,
            sessions,
            conflicts,
            unclassified_payloads,
            source_databases,
            databases,
            anomaly_count,
        };
        validate_plan(&plan)?;
        persist_restore_import_plan(data_root, &plan)?;
        prepared_result(plan, database_snapshots)
    })();
    if result.is_err() {
        let _ = remove_owned_work_tree(&staging_root, &work_root, operation_id);
        let _ = remove_owned_work_tree(
            &recovery_root,
            &data_root.join("session-storage-v1/restore-import-recovery"),
            operation_id,
        );
    }
    result
}

pub fn prepare_pending_recovery_import(
    codex_home: &Path,
    data_root: &Path,
    source: &PendingRecoverySource,
    operation_id: &str,
) -> Result<PreparedRestoreImport, String> {
    validate_absolute_directory(codex_home, "canonical root")?;
    validate_absolute_directory(data_root, "data root")?;
    validate_absolute_directory(&source.package_dir, "pending recovery package")?;
    validate_operation_id(operation_id)?;
    if !matches!(
        source.relation,
        super::legacy_backup::PendingRecoveryRelation::MissingFromCanonical
            | super::legacy_backup::PendingRecoveryRelation::ExtendsCanonical
    ) {
        return Err("pending recovery entry requires conflict review".to_string());
    }
    if !source.payload_path.starts_with(&source.package_dir)
        || !source.source_database_path.starts_with(&source.package_dir)
        || source.source_database_path != source.package_dir.join("source-state.sqlite")
        || source.payload_sha256.len() != 64
        || !source.payload_sha256.bytes().all(is_lower_hex)
    {
        return Err("pending recovery source shape is invalid".to_string());
    }
    let semantic = read_semantic_session(&source.payload_path)
        .map_err(|_| "pending recovery session is invalid".to_string())?;
    if semantic.thread_id != source.thread_id
        || hex_sha256(semantic.raw_sha256) != source.payload_sha256
    {
        return Err("pending recovery session identity changed".to_string());
    }

    let work_root = operation_root(data_root, operation_id)?;
    let staging_root = work_root.join("restore-import-staging");
    let recovery_root = data_root
        .join("session-storage-v1/restore-import-recovery")
        .join(operation_id);
    if staging_root.exists() || recovery_root.exists() {
        return Err("an interrupted restore import must be recovered first".to_string());
    }
    create_safe_directory(&staging_root)?;
    create_safe_directory(&recovery_root)?;
    write_work_marker(&staging_root, operation_id, &source.package_dir)?;
    write_work_marker(&recovery_root, operation_id, &source.package_dir)?;

    let result = (|| {
        let first_source_databases = snapshot_pending_source_database(
            &source.source_database_path,
            &staging_root.join("source-databases-first"),
        )?;
        let first = scan_pending_source(source, &first_source_databases)?;
        thread::sleep(OBSERVATION_DELAY);
        let source_databases = snapshot_pending_source_database(
            &source.source_database_path,
            &staging_root.join("source-databases"),
        )?;
        let second = scan_pending_source(source, &source_databases)?;
        if first.fingerprint != second.fingerprint {
            return Err("pending recovery source changed during import preflight".to_string());
        }
        let inventory = collect_inventory(codex_home, data_root)?;
        if inventory
            .database_discovery_errors
            .saturating_sub(inventory.goals_database_discovery_errors)
            > 0
            || inventory.session_discovery_errors > 0
        {
            return Err("canonical inventory is incomplete".to_string());
        }
        let generated_at_ms = timestamp_millis()?;
        let recovery_expires_at_ms = generated_at_ms
            .checked_add(RECOVERY_RETENTION_MS)
            .ok_or_else(|| "restore import retention timestamp overflowed".to_string())?;
        let baseline = BTreeMap::new();
        let (mut sessions, mut conflicts, unclassified_payloads, anomaly_count) =
            classify_sessions(
                codex_home,
                &baseline,
                &inventory,
                second,
                &staging_root,
                &recovery_root,
            )?;
        let source_goals = source_databases
            .iter()
            .filter(|database| {
                database
                    .source_path
                    .file_name()
                    .is_some_and(|name| name == "goals_1.sqlite")
            })
            .map(|database| database.snapshot_path.clone())
            .collect::<Vec<_>>();
        let (mut databases, database_snapshots) = prepare_runtime_databases(
            codex_home,
            data_root,
            &staging_root,
            &recovery_root,
            &source_goals,
        )?;
        let migration_sessions = sessions
            .iter()
            .filter_map(to_migration_session_entry)
            .collect::<Result<Vec<_>, _>>()?;
        // Pending recovery binds exactly one declared legacy database above;
        // unlike a downgrade package its safe package-local basename is not
        // `state_5.sqlite`, so pass that bound snapshot directly.
        let source_paths = source_databases
            .iter()
            .map(|database| database.snapshot_path.clone())
            .collect::<Vec<_>>();
        let synthetic_thread_ids = sessions
            .iter()
            .filter(|session| session.synthesize_database_row)
            .map(|session| session.thread_id.clone())
            .collect::<BTreeSet<_>>();
        merge_restore_import_runtime_views(
            &migration_sessions,
            &mut databases,
            &source_paths,
            &source_goals,
            &synthetic_thread_ids,
        )?;
        let replacements = build_restore_import_replacements(operation_id, &sessions, &databases)?;
        validate_replacement_paths_unoccupied(&replacements)?;
        sessions.sort_by(|left, right| left.thread_id.cmp(&right.thread_id));
        conflicts.sort_by(|left, right| left.thread_id.cmp(&right.thread_id));
        let plan = RestoreImportPlan {
            schema_version: RESTORE_IMPORT_SCHEMA_VERSION,
            operation_id: operation_id.to_string(),
            generated_at_ms,
            package_operation_id: source.entry_id.clone(),
            target_version: "legacy-backup-v1".to_string(),
            source_kind: RestoreImportSourceKind::PendingRecovery,
            package_dir: source.package_dir.clone(),
            canonical_root: codex_home.to_path_buf(),
            data_root: data_root.to_path_buf(),
            source_fingerprint: first.fingerprint,
            work_root: work_root.clone(),
            staging_root: staging_root.clone(),
            recovery_root: recovery_root.clone(),
            recovery_expires_at_ms,
            sessions,
            conflicts,
            unclassified_payloads,
            source_databases,
            databases,
            anomaly_count,
        };
        validate_plan(&plan)?;
        persist_restore_import_plan(data_root, &plan)?;
        prepared_result(plan, database_snapshots)
    })();
    if result.is_err() {
        let _ = remove_owned_work_tree(&staging_root, &work_root, operation_id);
        let _ = remove_owned_work_tree(
            &recovery_root,
            &data_root.join("session-storage-v1/restore-import-recovery"),
            operation_id,
        );
    }
    result
}

fn snapshot_pending_source_database(
    source_path: &Path,
    snapshot_root: &Path,
) -> Result<Vec<RestoreImportSourceDatabase>, String> {
    create_safe_directory(snapshot_root)?;
    let snapshot_path = snapshot_root.join("source-00.sqlite");
    snapshot_sqlite(source_path, &snapshot_path)?;
    let (_, sha256) = stable_file_digest(&snapshot_path)?;
    Ok(vec![RestoreImportSourceDatabase {
        source_path: source_path.to_path_buf(),
        snapshot_path,
        sha256,
    }])
}

fn scan_pending_source(
    source: &PendingRecoverySource,
    databases: &[RestoreImportSourceDatabase],
) -> Result<SourceScan, String> {
    let semantic = read_semantic_session(&source.payload_path)
        .map_err(|_| "pending recovery session is invalid".to_string())?;
    let sha256 = hex_sha256(semantic.raw_sha256);
    if semantic.thread_id != source.thread_id || sha256 != source.payload_sha256 {
        return Err("pending recovery session identity changed".to_string());
    }
    let source_database_snapshot = pending_source_database_for_thread(
        databases,
        &semantic.thread_id,
        &source.payload_path,
        &source.package_dir,
        &source.source_database_path,
        &source.entry_id,
    )?
    .ok_or_else(|| "pending recovery database is missing the session row".to_string())?;
    let mut fingerprint_parts = vec![
        source.package_id.clone(),
        source.entry_id.clone(),
        semantic.thread_id.clone(),
        semantic.bytes.to_string(),
        sha256,
    ];
    fingerprint_parts.extend(databases.iter().map(|database| database.sha256.clone()));
    let fingerprint = hex_sha256(Sha256::digest(fingerprint_parts.join("\n").as_bytes()));
    Ok(SourceScan {
        candidates: BTreeMap::from([(
            semantic.thread_id.clone(),
            vec![PackageCandidate {
                path: source.payload_path.clone(),
                semantic,
                source_database_snapshot,
                synthesize_database_row: false,
            }],
        )]),
        unknown_candidates: BTreeMap::new(),
        unclassified_candidates: Vec::new(),
        anomaly_count: 0,
        fingerprint,
    })
}

fn pending_source_database_for_thread(
    databases: &[RestoreImportSourceDatabase],
    thread_id: &str,
    candidate_path: &Path,
    package_dir: &Path,
    declared_source_database: &Path,
    entry_id: &str,
) -> Result<Option<PathBuf>, String> {
    if databases.len() != 1
        || databases[0].source_path != declared_source_database
        || declared_source_database != package_dir.join("source-state.sqlite")
        || entry_id.len() != 64
        || !entry_id.bytes().all(is_lower_hex)
        || candidate_path
            != package_dir
                .join("payloads")
                .join(format!("{entry_id}.jsonl"))
    {
        return Err("pending recovery database snapshot identity changed".to_string());
    }
    if stable_file_digest(&databases[0].snapshot_path)?.1 != databases[0].sha256 {
        return Err("pending recovery database snapshot identity changed".to_string());
    }
    let connection = Connection::open_with_flags(
        &databases[0].snapshot_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| "failed to open pending recovery database snapshot".to_string())?;
    let rollout_path = match connection.query_row(
        "SELECT rollout_path FROM threads WHERE id = ?1",
        [thread_id],
        |row| row.get::<_, Option<String>>(0),
    ) {
        Ok(path) => path,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(_) => return Err("failed to read pending recovery database thread".to_string()),
    };
    let path_matches = rollout_path.as_deref().is_some_and(|value| {
        let stored = Path::new(value);
        path_key(stored) == path_key(candidate_path)
            || relocated_package_path(package_dir, stored)
                .is_some_and(|path| path_key(&path) == path_key(candidate_path))
            || trusted_legacy_rollout_path(stored, thread_id)
    });
    if !path_matches {
        return Err("pending recovery database session path changed".to_string());
    }
    Ok(Some(databases[0].snapshot_path.clone()))
}

fn trusted_legacy_rollout_path(stored: &Path, thread_id: &str) -> bool {
    if thread_id.trim().is_empty()
        || stored
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return false;
    }
    let components = stored.components().collect::<Vec<_>>();
    let Some(session_root) = components.iter().position(|component| {
        matches!(
            component,
            Component::Normal(value)
                if value.to_string_lossy().eq_ignore_ascii_case("sessions")
                    || value
                        .to_string_lossy()
                        .eq_ignore_ascii_case("archived_sessions")
        )
    }) else {
        return false;
    };
    if components[session_root..]
        .iter()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return false;
    }
    let Some(file_stem) = stored.file_stem().and_then(|name| name.to_str()) else {
        return false;
    };
    let normalized_stem = file_stem.to_ascii_lowercase();
    let normalized_thread = thread_id.to_ascii_lowercase();
    let exact_thread_token = normalized_stem == normalized_thread
        || normalized_stem
            .strip_suffix(&normalized_thread)
            .is_some_and(|prefix| prefix.ends_with('-'));
    stored
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
        && exact_thread_token
}

pub fn persist_restore_import_plan(
    data_root: &Path,
    plan: &RestoreImportPlan,
) -> Result<(), String> {
    validate_plan(plan)?;
    if plan.data_root != data_root {
        return Err("restore import plan data root changed".to_string());
    }
    let replacements =
        build_restore_import_replacements(&plan.operation_id, &plan.sessions, &plan.databases)?;
    validate_restore_import_replacement_plan_paths(plan, &replacements)?;
    validate_restore_import_replacement_sources(&replacements)?;
    validate_replacement_paths_unoccupied(&replacements)?;
    let envelope = RestoreImportPlanEnvelope {
        integrity_sha256: plan_digest(plan, &replacements)?,
        plan: plan.clone(),
        replacements: replacements.clone(),
    };
    let bytes = serde_json::to_vec_pretty(&envelope)
        .map_err(|_| "failed to serialize restore import plan".to_string())?;
    if bytes.len() as u64 > MAX_PLAN_BYTES {
        return Err("restore import plan reached its size limit".to_string());
    }
    let persisted_plan_path = plan_path(data_root, &plan.operation_id)?;
    atomic_write(&persisted_plan_path, &bytes)?;
    if let Err(error) = persist_initial_restore_import_replacement_phases(plan, &replacements) {
        let cleanup = cleanup_unapplied_restore_import_replacement_artifacts(&replacements)
            .and_then(|()| {
                remove_restore_import_replacement_phase_record(
                    plan,
                    &replacements,
                    &[RestoreImportReplacementPhase::Planned],
                )
            })
            .and_then(|()| {
                remove_exact_replacement_artifact(
                    &persisted_plan_path,
                    &hex_sha256(Sha256::digest(&bytes)),
                )
            });
        if cleanup.is_err() {
            return Err(format!(
                "{error}; restore import failed plan cleanup requires recovery"
            ));
        }
        return Err(error);
    }
    Ok(())
}

pub fn load_restore_import_plan(
    data_root: &Path,
    operation_id: &str,
) -> Result<RestoreImportPlan, String> {
    Ok(load_restore_import_plan_envelope(data_root, operation_id)?.plan)
}

fn load_restore_import_plan_envelope(
    data_root: &Path,
    operation_id: &str,
) -> Result<RestoreImportPlanEnvelope, String> {
    let bytes = read_regular_file_bounded(&plan_path(data_root, operation_id)?, MAX_PLAN_BYTES)
        .map_err(|_| "restore import plan is unreadable".to_string())?;
    let envelope = serde_json::from_slice::<RestoreImportPlanEnvelope>(&bytes)
        .map_err(|_| "restore import plan is invalid".to_string())?;
    if envelope.plan.operation_id != operation_id || envelope.plan.data_root != data_root {
        return Err("restore import plan integrity check failed".to_string());
    }
    validate_plan(&envelope.plan)?;
    validate_persisted_restore_import_replacements(&envelope.plan, &envelope.replacements)?;
    validate_restore_import_replacement_plan_paths(&envelope.plan, &envelope.replacements)?;
    if plan_digest(&envelope.plan, &envelope.replacements)? != envelope.integrity_sha256 {
        return Err("restore import plan integrity check failed".to_string());
    }
    Ok(envelope)
}

fn load_restore_import_replacements(
    plan: &RestoreImportPlan,
) -> Result<Vec<RestoreImportReplacementPlan>, String> {
    let envelope = load_restore_import_plan_envelope(&plan.data_root, &plan.operation_id)?;
    if envelope.plan != *plan {
        return Err("restore import plan integrity check failed".to_string());
    }
    Ok(envelope.replacements)
}

fn snapshot_source_databases(
    package_dir: &Path,
    manifest: &DowngradeExportManifest,
    snapshot_root: &Path,
) -> Result<Vec<RestoreImportSourceDatabase>, String> {
    let candidates = [
        PathBuf::from("codex-home/state_5.sqlite"),
        PathBuf::from("appdata/codex-switch/relay-sqlite/state_5.sqlite"),
        PathBuf::from("appdata/codex-switch/shared-sessions/state_5.sqlite"),
        PathBuf::from("codex-home/goals_1.sqlite"),
        PathBuf::from("appdata/codex-switch/relay-sqlite/goals_1.sqlite"),
        PathBuf::from("appdata/codex-switch/shared-sessions/goals_1.sqlite"),
    ];
    create_safe_directory(snapshot_root)?;
    let mut output = Vec::new();
    for (index, relative_path) in candidates.into_iter().enumerate() {
        let source_path = package_dir.join(&relative_path);
        let declared = manifest
            .entries
            .iter()
            .find(|entry| entry.relative_path == relative_path);
        let present = match fs::symlink_metadata(&source_path) {
            Ok(metadata) if metadata.is_file() && !metadata_is_link_or_reparse(&metadata) => true,
            Ok(_) => return Err("downgrade package database is unsafe".to_string()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(_) => return Err("downgrade package database is unavailable".to_string()),
        };
        let is_post_export_relay_state =
            relative_path == Path::new("appdata/codex-switch/relay-sqlite/state_5.sqlite");
        match (declared, present) {
            (None, false) => continue,
            // The isolated v0.2 runtime can create its Relay state database
            // after the immutable export manifest was written. Accept only
            // that exact bounded state-database name; it is snapshotted and
            // schema-validated below. Newly appeared goals/auxiliary databases
            // remain fail-closed because they lack an export provenance entry.
            (None, true) if is_post_export_relay_state => {}
            (None, true) => {
                return Err("downgrade package contains an undeclared database".to_string())
            }
            (Some(_), false) => {
                return Err("downgrade package declared database is missing".to_string())
            }
            (Some(entry), true)
                if !matches!(
                    entry.kind,
                    DowngradePackageFileKind::StateDatabase
                        | DowngradePackageFileKind::AuxiliaryDatabase
                        | DowngradePackageFileKind::SharedView
                        | DowngradePackageFileKind::Bootstrap
                ) =>
            {
                return Err("downgrade package database manifest kind is invalid".to_string())
            }
            (Some(_), true) => {}
        }
        if source_path
            .file_name()
            .is_some_and(|name| name == "goals_1.sqlite")
            && !manifest.entries.iter().any(|entry| {
                entry.relative_path == relative_path && entry.logical_thread_id.is_none()
            })
        {
            return Err("downgrade goals database declaration is invalid".to_string());
        }
        if !present {
            continue;
        }
        let snapshot_path = snapshot_root.join(format!("source-{index:02}.sqlite"));
        snapshot_sqlite(&source_path, &snapshot_path)?;
        if source_path
            .file_name()
            .is_some_and(|name| name == "goals_1.sqlite")
        {
            let connection = Connection::open_with_flags(
                &snapshot_path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .map_err(|_| "failed to open downgrade goals database snapshot".to_string())?;
            goals_database_digest(&connection)?;
        }
        let (_, sha256) = stable_file_digest(&snapshot_path)?;
        output.push(RestoreImportSourceDatabase {
            source_path,
            snapshot_path,
            sha256,
        });
    }
    if output.is_empty()
        || !output
            .iter()
            .any(|database| database.source_path == package_dir.join("codex-home/state_5.sqlite"))
    {
        return Err("downgrade package Account database is missing".to_string());
    }
    Ok(output)
}

fn scan_package(
    package_dir: &Path,
    manifest: &DowngradeExportManifest,
    source_databases: &[RestoreImportSourceDatabase],
) -> Result<SourceScan, String> {
    let mut manifest_entries = BTreeMap::new();
    for entry in &manifest.entries {
        let key = path_key(&package_dir.join(&entry.relative_path));
        if manifest_entries
            .insert(
                key,
                (
                    entry.kind,
                    entry.logical_thread_id.clone(),
                    entry.sha256.clone(),
                ),
            )
            .is_some()
        {
            return Err("downgrade package manifest has duplicate entries".to_string());
        }
    }
    let roots = [
        package_dir.join("codex-home/sessions"),
        package_dir.join("codex-home/archived_sessions"),
        package_dir.join("appdata/codex-switch/relay-sqlite/sessions"),
        package_dir.join("appdata/codex-switch/relay-sqlite/archived_sessions"),
        package_dir.join("appdata/codex-switch/shared-sessions/sessions"),
        package_dir.join("appdata/codex-switch/shared-sessions/archived_sessions"),
        package_dir.join("recovery"),
    ];
    let recovery_root = package_dir.join("recovery");
    let mut files = BTreeMap::<String, PathBuf>::new();
    for root in roots {
        if !root.exists() {
            continue;
        }
        validate_contained_directory(package_dir, &root, "downgrade session root")?;
        for entry in WalkDir::new(&root).follow_links(false) {
            let entry = entry.map_err(|_| "failed to scan downgrade session root".to_string())?;
            let metadata = fs::symlink_metadata(entry.path())
                .map_err(|_| "failed to inspect downgrade session entry".to_string())?;
            if metadata_is_link_or_reparse(&metadata) || (!metadata.is_file() && !metadata.is_dir())
            {
                return Err("downgrade session root contains an unsafe entry".to_string());
            }
            if metadata.is_file()
                && entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
            {
                files.insert(path_key(entry.path()), entry.path().to_path_buf());
            }
        }
    }
    for (key, (kind, _, _)) in &manifest_entries {
        if matches!(
            kind,
            DowngradePackageFileKind::ConflictBranch | DowngradePackageFileKind::RecoveryPayload
        ) && !files.contains_key(key)
        {
            return Err("declared downgrade recovery payload is unavailable".to_string());
        }
    }

    let mut candidates = BTreeMap::<String, Vec<PackageCandidate>>::new();
    let mut unknown_candidates = BTreeMap::<String, Vec<PackageUnknownCandidate>>::new();
    let mut unclassified_candidates = Vec::<PackageUnclassifiedCandidate>::new();
    let mut anomaly_count = 0_usize;
    let mut fingerprint_parts = vec![
        manifest.operation_id.clone(),
        manifest.target.version.clone(),
        manifest.source_inventory_fingerprint.clone(),
    ];
    for path in files.into_values() {
        let key = path_key(&path);
        let manifest_entry = manifest_entries.get(&key).cloned();
        if path.starts_with(&recovery_root) && manifest_entry.is_none() {
            return Err(
                "unmanifested downgrade recovery payload cannot be safely imported".to_string(),
            );
        }
        if path.starts_with(&recovery_root)
            && manifest_entry.as_ref().is_some_and(|(kind, _, _)| {
                !matches!(
                    kind,
                    DowngradePackageFileKind::ConflictBranch
                        | DowngradePackageFileKind::RecoveryPayload
                )
            })
        {
            return Err("downgrade recovery payload kind cannot be safely imported".to_string());
        }
        let semantic = match read_semantic_session(&path) {
            Ok(semantic) => semantic,
            Err(_) => {
                anomaly_count = anomaly_count.saturating_add(1);
                let (bytes, sha256) = stable_file_digest(&path)?;
                fingerprint_parts.push(format!("invalid|{key}|{bytes}|{sha256}"));
                if let Some((kind, declared_thread_id, _)) = manifest_entry.as_ref() {
                    match kind {
                        DowngradePackageFileKind::RecoveryPayload => {
                            unclassified_candidates.push(PackageUnclassifiedCandidate {
                                path,
                                bytes,
                                sha256,
                                reason: RestoreImportUnclassifiedReason::RecoveryPayload,
                            });
                            continue;
                        }
                        DowngradePackageFileKind::ConflictBranch => {
                            let thread_id = declared_thread_id.clone().ok_or_else(|| {
                                "downgrade conflict payload has no declared logical thread"
                                    .to_string()
                            })?;
                            unknown_candidates.entry(thread_id).or_default().push(
                                PackageUnknownCandidate {
                                    path,
                                    bytes,
                                    sha256,
                                    semantic: false,
                                    fallback_source_database_snapshot: None,
                                },
                            );
                            continue;
                        }
                        _ if path.starts_with(&recovery_root) => {
                            return Err(
                                "downgrade recovery payload kind cannot be safely imported"
                                    .to_string(),
                            );
                        }
                        _ => {}
                    }
                }
                let declared_thread_id = manifest_entry
                    .as_ref()
                    .and_then(|(_, thread_id, _)| thread_id.clone());
                if let Some(thread_id) = declared_thread_id.or(source_thread_for_path(
                    source_databases,
                    &path,
                    package_dir,
                )?) {
                    unknown_candidates.entry(thread_id).or_default().push(
                        PackageUnknownCandidate {
                            path,
                            bytes,
                            sha256,
                            semantic: false,
                            fallback_source_database_snapshot: None,
                        },
                    );
                }
                continue;
            }
        };
        if let Some((kind, declared_thread_id, _)) = manifest_entry.as_ref() {
            let actual_sha256 = hex_sha256(semantic.raw_sha256);
            match kind {
                DowngradePackageFileKind::ActiveSession
                | DowngradePackageFileKind::ArchivedSession => {
                    if declared_thread_id.as_deref() != Some(semantic.thread_id.as_str()) {
                        return Err("downgrade session identity changed".to_string());
                    }
                }
                DowngradePackageFileKind::ConflictBranch => {
                    let thread_id = declared_thread_id.clone().ok_or_else(|| {
                        "downgrade conflict payload has no declared logical thread".to_string()
                    })?;
                    anomaly_count = anomaly_count.saturating_add(1);
                    fingerprint_parts.push(format!(
                        "conflict|{key}|{thread_id}|{}|{}|{}",
                        semantic.thread_id, semantic.bytes, actual_sha256
                    ));
                    unknown_candidates
                        .entry(thread_id.clone())
                        .or_default()
                        .push(PackageUnknownCandidate {
                            path,
                            bytes: semantic.bytes,
                            sha256: actual_sha256,
                            semantic: thread_id == semantic.thread_id,
                            fallback_source_database_snapshot: None,
                        });
                    continue;
                }
                DowngradePackageFileKind::RecoveryPayload => {
                    anomaly_count = anomaly_count.saturating_add(1);
                    fingerprint_parts.push(format!(
                        "recovery|{key}|{}|{}",
                        semantic.bytes, actual_sha256
                    ));
                    unclassified_candidates.push(PackageUnclassifiedCandidate {
                        path,
                        bytes: semantic.bytes,
                        sha256: actual_sha256,
                        reason: RestoreImportUnclassifiedReason::RecoveryPayload,
                    });
                    continue;
                }
                _ if path.starts_with(&recovery_root) => {
                    return Err(
                        "downgrade recovery payload kind cannot be safely imported".to_string()
                    );
                }
                _ => {}
            }
        }
        let Some(source_database_snapshot) =
            source_database_for_thread(source_databases, &semantic.thread_id, &path, package_dir)?
        else {
            if path.starts_with(&recovery_root) {
                return Err(
                    "downgrade recovery payload requires explicit conflict review".to_string(),
                );
            }
            anomaly_count = anomaly_count.saturating_add(1);
            let sha256 = hex_sha256(semantic.raw_sha256);
            fingerprint_parts.push(format!(
                "orphan|{}|{}|{}",
                path_key(&path),
                semantic.bytes,
                sha256
            ));
            unknown_candidates
                .entry(semantic.thread_id.clone())
                .or_default()
                .push(PackageUnknownCandidate {
                    path,
                    bytes: semantic.bytes,
                    sha256,
                    semantic: true,
                    fallback_source_database_snapshot: source_databases
                        .first()
                        .map(|database| database.snapshot_path.clone()),
                });
            continue;
        };
        fingerprint_parts.push(format!(
            "session|{}|{}|{}",
            path_key(&path),
            semantic.bytes,
            hex_sha256(semantic.raw_sha256)
        ));
        candidates
            .entry(semantic.thread_id.clone())
            .or_default()
            .push(PackageCandidate {
                path,
                semantic,
                source_database_snapshot,
                synthesize_database_row: false,
            });
    }
    for database in source_databases {
        fingerprint_parts.push(format!(
            "database|{}|{}",
            path_key(&database.source_path),
            database.sha256
        ));
    }
    fingerprint_parts.sort();
    let fingerprint = hex_sha256(Sha256::digest(fingerprint_parts.join("\n").as_bytes()));
    Ok(SourceScan {
        candidates,
        unknown_candidates,
        unclassified_candidates,
        anomaly_count,
        fingerprint,
    })
}

fn source_thread_for_path(
    databases: &[RestoreImportSourceDatabase],
    candidate_path: &Path,
    package_dir: &Path,
) -> Result<Option<String>, String> {
    let candidate_key = path_key(candidate_path);
    let mut thread_ids = BTreeSet::new();
    for database in databases.iter().filter(|database| {
        database
            .source_path
            .file_name()
            .is_some_and(|name| name == "state_5.sqlite")
    }) {
        let connection = Connection::open_with_flags(
            &database.snapshot_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|_| "failed to open downgrade database snapshot".to_string())?;
        let mut statement = connection
            .prepare("SELECT id, rollout_path FROM threads WHERE rollout_path IS NOT NULL")
            .map_err(|_| "failed to inspect downgrade database thread paths".to_string())?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|_| "failed to inspect downgrade database thread paths".to_string())?;
        for row in rows {
            let (thread_id, rollout_path) =
                row.map_err(|_| "failed to inspect downgrade database thread paths".to_string())?;
            let stored = Path::new(&rollout_path);
            let exact = path_key(stored) == candidate_key;
            let relocated = relocated_package_path(package_dir, stored)
                .is_some_and(|path| path_key(&path) == candidate_key);
            if exact || relocated {
                thread_ids.insert(thread_id);
            }
        }
    }
    if thread_ids.len() == 1 {
        Ok(thread_ids.into_iter().next())
    } else {
        Ok(None)
    }
}

fn source_database_for_thread(
    databases: &[RestoreImportSourceDatabase],
    thread_id: &str,
    candidate_path: &Path,
    package_dir: &Path,
) -> Result<Option<PathBuf>, String> {
    source_database_for_thread_candidates(
        databases.iter().filter(|database| {
            database
                .source_path
                .file_name()
                .is_some_and(|name| name == "state_5.sqlite")
        }),
        thread_id,
        candidate_path,
        package_dir,
    )
}

fn source_database_for_thread_candidates<'a>(
    databases: impl Iterator<Item = &'a RestoreImportSourceDatabase>,
    thread_id: &str,
    candidate_path: &Path,
    package_dir: &Path,
) -> Result<Option<PathBuf>, String> {
    let mut matches = Vec::<(u8, String, PathBuf)>::new();
    for database in databases {
        let connection = Connection::open_with_flags(
            &database.snapshot_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|_| "failed to open downgrade database snapshot".to_string())?;
        let row = connection.query_row(
            "SELECT rollout_path FROM threads WHERE id = ?1",
            [thread_id],
            |row| row.get::<_, Option<String>>(0),
        );
        let rollout_path = match row {
            Ok(path) => path,
            Err(rusqlite::Error::QueryReturnedNoRows) => continue,
            Err(_) => return Err("failed to read downgrade database thread".to_string()),
        };
        let exact = rollout_path
            .as_deref()
            .map(Path::new)
            .is_some_and(|path| path_key(path) == path_key(candidate_path));
        let source_relative_match = rollout_path.as_deref().is_some_and(|value| {
            relocated_package_path(package_dir, Path::new(value))
                .is_some_and(|path| path_key(&path) == path_key(candidate_path))
        });
        let role_rank = if database.source_path == package_dir.join("codex-home/state_5.sqlite") {
            0
        } else if database
            .source_path
            .ends_with(Path::new("relay-sqlite/state_5.sqlite"))
        {
            1
        } else {
            2
        };
        let rank = if exact || source_relative_match {
            role_rank
        } else {
            10 + role_rank
        };
        matches.push((
            rank,
            path_key(&database.snapshot_path),
            database.snapshot_path.clone(),
        ));
    }
    matches.sort();
    Ok(matches.into_iter().next().map(|(_, _, path)| path))
}

fn relocated_package_path(package_dir: &Path, stored: &Path) -> Option<PathBuf> {
    let components = stored.components().collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(value) = component else {
            continue;
        };
        if value.to_string_lossy().eq_ignore_ascii_case("codex-home")
            || value.to_string_lossy().eq_ignore_ascii_case("appdata")
        {
            let mut relative = PathBuf::new();
            for component in &components[index..] {
                if let Component::Normal(value) = component {
                    relative.push(value);
                } else {
                    return None;
                }
            }
            return Some(package_dir.join(relative));
        }
    }
    None
}

type ClassifiedSessions = (
    Vec<RestoreImportSessionPlan>,
    Vec<RestoreImportConflictPlan>,
    Vec<RestoreImportUnclassifiedPlan>,
    usize,
);

fn classify_sessions(
    codex_home: &Path,
    baseline: &BTreeMap<String, Vec<String>>,
    inventory: &MigrationInventory,
    source: SourceScan,
    staging_root: &Path,
    recovery_root: &Path,
) -> Result<ClassifiedSessions, String> {
    let canonical = canonical_sessions_by_thread(inventory);
    let SourceScan {
        mut candidates,
        unknown_candidates,
        unclassified_candidates,
        anomaly_count,
        fingerprint: _,
    } = source;
    let mut sessions = Vec::new();
    let mut conflicts = Vec::new();
    let mut anomaly_count = anomaly_count;
    let unclassified_payloads =
        stage_unclassified_payloads(&unclassified_candidates, recovery_root)?;

    for (thread_id, unknowns) in unknown_candidates {
        let mut semantic_candidates = candidates.remove(&thread_id).unwrap_or_default();
        let current = canonical.get(&thread_id).copied();
        let recoverable_orphan = current.is_none()
            && semantic_candidates.is_empty()
            && unknowns.len() == 1
            && unknowns[0].semantic
            && !baseline.contains_key(&thread_id);
        if recoverable_orphan {
            let unknown = &unknowns[0];
            let semantic = read_semantic_session(&unknown.path).map_err(|_| {
                "restore import orphan session changed during classification".to_string()
            })?;
            semantic_candidates.push(PackageCandidate {
                path: unknown.path.clone(),
                semantic,
                source_database_snapshot: unknown
                    .fallback_source_database_snapshot
                    .clone()
                    .ok_or_else(|| {
                        "restore import orphan session has no database schema snapshot".to_string()
                    })?,
                synthesize_database_row: true,
            });
            candidates.insert(thread_id, semantic_candidates);
            anomaly_count = anomaly_count.saturating_add(1);
            continue;
        }
        conflicts.push(stage_unknown_conflict(
            &thread_id,
            current,
            &semantic_candidates,
            &unknowns,
            recovery_root,
        )?);
    }

    for (thread_id, candidates) in candidates {
        let Some(selected_index) = unique_maximal_candidate(&candidates) else {
            conflicts.push(stage_conflict(
                &thread_id,
                canonical.get(&thread_id).copied(),
                &candidates,
                SessionRelation::Divergent,
                "downgrade package contains multiple incomparable branches",
                recovery_root,
            )?);
            continue;
        };
        let selected = &candidates[selected_index];
        let baseline_hash = baseline
            .get(&thread_id)
            .and_then(|hashes| {
                hashes
                    .iter()
                    .find(|hash| **hash == hex_sha256(selected.semantic.raw_sha256))
            })
            .cloned()
            .or_else(|| {
                baseline
                    .get(&thread_id)
                    .and_then(|hashes| hashes.first().cloned())
            });
        let source_changed_since_export = baseline
            .get(&thread_id)
            .is_none_or(|hashes| !hashes.contains(&hex_sha256(selected.semantic.raw_sha256)));
        let current = canonical.get(&thread_id).copied();

        if current.is_none() && baseline.contains_key(&thread_id) {
            conflicts.push(stage_conflict(
                &thread_id,
                None,
                &candidates,
                SessionRelation::Unknown,
                "canonical baseline session is missing",
                recovery_root,
            )?);
            anomaly_count = anomaly_count.saturating_add(1);
            continue;
        }

        let (action, canonical_path, canonical_before_sha256, relation) = match current {
            None => (
                RestoreImportSessionAction::ImportNew,
                generated_canonical_path(codex_home, &thread_id),
                None,
                None,
            ),
            Some(current) => {
                let current_semantic = current
                    .semantic
                    .as_ref()
                    .expect("canonical session map only contains semantic files");
                let relation = compare_sessions(current_semantic, &selected.semantic);
                match relation {
                    SessionRelation::Equal | SessionRelation::EqualExceptProvider => (
                        RestoreImportSessionAction::Unchanged,
                        current.path.clone(),
                        Some(hex_sha256(current_semantic.raw_sha256)),
                        Some(relation),
                    ),
                    SessionRelation::RightPrefix => (
                        RestoreImportSessionAction::KeepCanonical,
                        current.path.clone(),
                        Some(hex_sha256(current_semantic.raw_sha256)),
                        Some(relation),
                    ),
                    SessionRelation::LeftPrefix if source_changed_since_export => (
                        RestoreImportSessionAction::ImportExtension,
                        current.path.clone(),
                        Some(hex_sha256(current_semantic.raw_sha256)),
                        Some(relation),
                    ),
                    SessionRelation::LeftPrefix => {
                        conflicts.push(stage_conflict(
                            &thread_id,
                            Some(current),
                            &candidates,
                            SessionRelation::Unknown,
                            "canonical session is shorter than the immutable downgrade baseline",
                            recovery_root,
                        )?);
                        anomaly_count = anomaly_count.saturating_add(1);
                        continue;
                    }
                    SessionRelation::Divergent | SessionRelation::Unknown => {
                        conflicts.push(stage_conflict(
                            &thread_id,
                            Some(current),
                            &candidates,
                            relation,
                            "canonical and downgrade histories diverged",
                            recovery_root,
                        )?);
                        continue;
                    }
                }
            }
        };

        if action == RestoreImportSessionAction::ImportNew && canonical_path.exists() {
            conflicts.push(stage_conflict(
                &thread_id,
                None,
                &candidates,
                SessionRelation::Unknown,
                "generated canonical import target is occupied",
                recovery_root,
            )?);
            continue;
        }
        let source_staged_path = if matches!(
            action,
            RestoreImportSessionAction::ImportNew | RestoreImportSessionAction::ImportExtension
        ) {
            let path = staging_root
                .join("sessions")
                .join(format!("{:06}.jsonl", sessions.len()));
            atomic_copy(&selected.path, &path)?;
            verify_session_identity(&path, &thread_id, &hex_sha256(selected.semantic.raw_sha256))?;
            Some(path)
        } else {
            None
        };
        let canonical_backup_payload = if action == RestoreImportSessionAction::ImportExtension {
            let path = recovery_root
                .join("canonical-files")
                .join(format!("{:06}.jsonl", sessions.len()));
            atomic_copy(&canonical_path, &path)?;
            if stable_file_digest(&path)?.1 != canonical_before_sha256.clone().unwrap_or_default() {
                return Err("canonical restore import backup verification failed".to_string());
            }
            Some(path)
        } else {
            None
        };
        sessions.push(RestoreImportSessionPlan {
            thread_id,
            action,
            source_path: selected.path.clone(),
            source_staged_path,
            source_database_snapshot: selected.source_database_snapshot.clone(),
            source_bytes: selected.semantic.bytes,
            source_sha256: hex_sha256(selected.semantic.raw_sha256),
            source_message_count: selected.semantic.message_count,
            source_last_message_at: selected.semantic.last_message_timestamp.clone(),
            source_provider: selected.semantic.initial_provider.clone(),
            baseline_sha256: baseline_hash,
            canonical_path,
            canonical_before_sha256,
            canonical_backup_payload,
            relation,
            synthesize_database_row: selected.synthesize_database_row,
        });
    }
    Ok((sessions, conflicts, unclassified_payloads, anomaly_count))
}

fn canonical_sessions_by_thread(
    inventory: &MigrationInventory,
) -> BTreeMap<String, &MigrationInventoryFile> {
    let mut output = BTreeMap::new();
    for (index, file) in inventory.files.iter().enumerate() {
        let Ok(semantic) = &file.semantic else {
            continue;
        };
        if file.origin != FileOrigin::CanonicalHome {
            continue;
        }
        let retained = inventory
            .graph
            .files
            .get(index)
            .is_some_and(|node| node.retained_candidate);
        match output.get(&semantic.thread_id) {
            None => {
                output.insert(semantic.thread_id.clone(), file);
            }
            Some(existing) if retained => {
                let existing_retained = inventory
                    .files
                    .iter()
                    .position(|candidate| std::ptr::eq(candidate, *existing))
                    .and_then(|existing_index| inventory.graph.files.get(existing_index))
                    .is_some_and(|node| node.retained_candidate);
                if !existing_retained {
                    output.insert(semantic.thread_id.clone(), file);
                }
            }
            Some(_) => {}
        }
    }
    output
}

fn baseline_hashes_by_thread(manifest: &DowngradeExportManifest) -> BTreeMap<String, Vec<String>> {
    let mut output = BTreeMap::<String, Vec<String>>::new();
    for entry in &manifest.entries {
        if !matches!(
            entry.kind,
            DowngradePackageFileKind::ActiveSession
                | DowngradePackageFileKind::ArchivedSession
                | DowngradePackageFileKind::ConflictBranch
        ) {
            continue;
        }
        let Some(thread_id) = entry.logical_thread_id.as_ref() else {
            continue;
        };
        output
            .entry(thread_id.clone())
            .or_default()
            .push(entry.sha256.clone());
    }
    for hashes in output.values_mut() {
        hashes.sort();
        hashes.dedup();
    }
    output
}

fn unique_maximal_candidate(candidates: &[PackageCandidate]) -> Option<usize> {
    let mut maximal = Vec::new();
    for (index, candidate) in candidates.iter().enumerate() {
        let dominates_all = candidates.iter().enumerate().all(|(other_index, other)| {
            index == other_index
                || matches!(
                    compare_sessions(&candidate.semantic, &other.semantic),
                    SessionRelation::Equal
                        | SessionRelation::EqualExceptProvider
                        | SessionRelation::RightPrefix
                )
        });
        if dominates_all {
            maximal.push(index);
        }
    }
    maximal.into_iter().min_by_key(|index| {
        let candidate = &candidates[*index];
        (source_path_rank(&candidate.path), path_key(&candidate.path))
    })
}

fn source_path_rank(path: &Path) -> u8 {
    let key = path_key(path);
    if key.contains("/codex-home/sessions/") || key.contains("\\codex-home\\sessions\\") {
        0
    } else if key.contains("/codex-home/archived_sessions/")
        || key.contains("\\codex-home\\archived_sessions\\")
    {
        1
    } else if key.contains("relay-sqlite") {
        2
    } else {
        3
    }
}

fn stage_conflict(
    thread_id: &str,
    current: Option<&MigrationInventoryFile>,
    candidates: &[PackageCandidate],
    relation: SessionRelation,
    reason: &str,
    recovery_root: &Path,
) -> Result<RestoreImportConflictPlan, String> {
    let conflict_root = recovery_root
        .join("conflicts")
        .join(safe_path_component(thread_id));
    create_safe_directory(&conflict_root)?;
    let mut ordered = candidates.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|candidate| path_key(&candidate.path));
    let mut candidate_paths = Vec::new();
    let mut candidate_sha256 = Vec::new();
    let mut recovery_paths = Vec::new();
    let mut seen_hashes = BTreeSet::new();
    for candidate in ordered {
        let sha256 = hex_sha256(candidate.semantic.raw_sha256);
        if !seen_hashes.insert(sha256.clone()) {
            continue;
        }
        let recovery = conflict_root.join(format!("{sha256}.jsonl"));
        atomic_copy(&candidate.path, &recovery)?;
        verify_session_identity(&recovery, thread_id, &sha256)?;
        candidate_paths.push(candidate.path.clone());
        candidate_sha256.push(sha256);
        recovery_paths.push(recovery);
    }
    Ok(RestoreImportConflictPlan {
        thread_id: thread_id.to_string(),
        current_path: current.map(|file| file.path.clone()),
        current_sha256: current
            .and_then(|file| file.semantic.as_ref().ok())
            .map(|semantic| hex_sha256(semantic.raw_sha256)),
        candidate_paths,
        candidate_sha256,
        recovery_paths,
        relation,
        reason: reason.to_string(),
        default_overwrite: false,
    })
}

fn stage_unknown_conflict(
    thread_id: &str,
    current: Option<&MigrationInventoryFile>,
    candidates: &[PackageCandidate],
    unknowns: &[PackageUnknownCandidate],
    recovery_root: &Path,
) -> Result<RestoreImportConflictPlan, String> {
    let conflict_root = recovery_root
        .join("conflicts")
        .join(safe_path_component(thread_id));
    create_safe_directory(&conflict_root)?;
    let mut payloads = candidates
        .iter()
        .map(|candidate| {
            (
                candidate.path.clone(),
                candidate.semantic.bytes,
                hex_sha256(candidate.semantic.raw_sha256),
                true,
            )
        })
        .chain(unknowns.iter().map(|candidate| {
            (
                candidate.path.clone(),
                candidate.bytes,
                candidate.sha256.clone(),
                candidate.semantic,
            )
        }))
        .collect::<Vec<_>>();
    payloads.sort_by_key(|(path, _, _, _)| path_key(path));
    let mut candidate_paths = Vec::new();
    let mut candidate_sha256 = Vec::new();
    let mut recovery_paths = Vec::new();
    let mut seen_hashes = BTreeSet::new();
    for (path, bytes, sha256, semantic) in payloads {
        if !seen_hashes.insert(sha256.clone()) {
            continue;
        }
        let recovery = conflict_root.join(format!("{sha256}.jsonl"));
        atomic_copy(&path, &recovery)?;
        if stable_file_digest(&recovery)? != (bytes, sha256.clone()) {
            return Err("restore import unknown conflict copy changed".to_string());
        }
        if semantic {
            verify_session_identity(&recovery, thread_id, &sha256)?;
        }
        candidate_paths.push(path);
        candidate_sha256.push(sha256);
        recovery_paths.push(recovery);
    }
    Ok(RestoreImportConflictPlan {
        thread_id: thread_id.to_string(),
        current_path: current.map(|file| file.path.clone()),
        current_sha256: current
            .and_then(|file| file.semantic.as_ref().ok())
            .map(|semantic| hex_sha256(semantic.raw_sha256)),
        candidate_paths,
        candidate_sha256,
        recovery_paths,
        relation: SessionRelation::Unknown,
        reason: "downgrade package contains invalid or incomplete session history".to_string(),
        default_overwrite: false,
    })
}

fn stage_unclassified_payloads(
    candidates: &[PackageUnclassifiedCandidate],
    recovery_root: &Path,
) -> Result<Vec<RestoreImportUnclassifiedPlan>, String> {
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let root = recovery_root.join("unclassified");
    create_safe_directory(&root)?;
    let mut ordered = candidates.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|candidate| path_key(&candidate.path));
    let mut output = Vec::with_capacity(ordered.len());
    for (index, candidate) in ordered.into_iter().enumerate() {
        let recovery_path = root.join(format!("{index:06}-{}.jsonl", candidate.sha256));
        atomic_copy(&candidate.path, &recovery_path)?;
        if stable_file_digest(&recovery_path)? != (candidate.bytes, candidate.sha256.clone()) {
            return Err("restore import unclassified payload copy changed".to_string());
        }
        output.push(RestoreImportUnclassifiedPlan {
            source_path: candidate.path.clone(),
            source_bytes: candidate.bytes,
            source_sha256: candidate.sha256.clone(),
            recovery_path,
            reason: candidate.reason,
        });
    }
    Ok(output)
}

fn generated_canonical_path(codex_home: &Path, thread_id: &str) -> PathBuf {
    let digest = hex_sha256(Sha256::digest(thread_id.as_bytes()));
    codex_home
        .join("sessions/reimported")
        .join(format!("rollout-{digest}.jsonl"))
}

fn build_restore_import_replacements(
    operation_id: &str,
    sessions: &[RestoreImportSessionPlan],
    databases: &[MigrationDatabaseApplyEntry],
) -> Result<Vec<RestoreImportReplacementPlan>, String> {
    build_restore_import_replacements_inner(operation_id, sessions, databases, None)
}

fn build_restore_import_replacements_inner(
    operation_id: &str,
    sessions: &[RestoreImportSessionPlan],
    databases: &[MigrationDatabaseApplyEntry],
    persisted_live_hashes: Option<&BTreeMap<String, String>>,
) -> Result<Vec<RestoreImportReplacementPlan>, String> {
    validate_operation_id(operation_id)?;
    let canonical_goals_target = databases
        .iter()
        .filter(|database| {
            database
                .target_path
                .file_name()
                .is_some_and(|name| name == "goals_1.sqlite")
        })
        .min_by_key(|database| {
            (
                restore_database_role_rank(database.role),
                path_key(&database.target_path),
            )
        })
        .map(|database| path_key(&database.target_path));
    let mut replacements = Vec::new();
    for session in sessions
        .iter()
        .filter(|session| session.action == RestoreImportSessionAction::ImportExtension)
    {
        let source_path = session
            .source_staged_path
            .clone()
            .ok_or_else(|| "restore import extension staging is missing".to_string())?;
        let original_sha256 = session
            .canonical_before_sha256
            .clone()
            .ok_or_else(|| "restore import extension original checksum is missing".to_string())?;
        replacements.push(build_restore_import_replacement(
            operation_id,
            RestoreImportReplacementKind::SessionExtension,
            &session.canonical_path,
            &source_path,
            &original_sha256,
            &original_sha256,
            &session.source_sha256,
        )?);
    }
    for database in databases.iter().filter(|database| {
        !is_created_goals_database(database)
            && (persisted_live_hashes
                .is_some_and(|hashes| hashes.contains_key(&path_key(&database.target_path)))
                || (persisted_live_hashes.is_none()
                    && (database.original_sha256 != database.staged_sha256
                        || (database
                            .target_path
                            .file_name()
                            .is_some_and(|name| name == "goals_1.sqlite")
                            && canonical_goals_target.as_ref().is_some_and(|canonical| {
                                path_key(&database.target_path) != *canonical
                                    && !databases
                                        .iter()
                                        .find(|candidate| {
                                            path_key(&candidate.target_path) == *canonical
                                        })
                                        .is_some_and(|canonical_database| {
                                            same_regular_file_identity(
                                                &database.target_path,
                                                &canonical_database.target_path,
                                            )
                                            .unwrap_or(false)
                                        })
                            })))))
    }) {
        let live_target_sha256 = match persisted_live_hashes {
            Some(hashes) => hashes
                .get(&path_key(&database.target_path))
                .filter(|hash| hash.len() == 64 && hash.bytes().all(is_lower_hex))
                .cloned()
                .ok_or_else(|| {
                    "restore import database live original checksum is missing".to_string()
                })?,
            None => stable_file_digest(&database.target_path)?.1,
        };
        replacements.push(build_restore_import_replacement(
            operation_id,
            RestoreImportReplacementKind::RuntimeDatabase,
            &database.target_path,
            &database.staged_path,
            &live_target_sha256,
            &database.original_sha256,
            &database.staged_sha256,
        )?);
    }
    if let Some(canonical_target) = canonical_goals_target {
        let canonical_database = databases
            .iter()
            .find(|database| path_key(&database.target_path) == canonical_target)
            .ok_or_else(|| "restore import canonical goals database is missing".to_string())?;
        let canonical_stage = canonical_database.staged_path.clone();
        for replacement in replacements.iter_mut().filter(|replacement| {
            replacement.kind == RestoreImportReplacementKind::RuntimeDatabase
                && replacement
                    .target_path
                    .file_name()
                    .is_some_and(|name| name == "goals_1.sqlite")
                && path_key(&replacement.target_path) != canonical_target
        }) {
            replacement.source_path = canonical_stage.clone();
        }
    }
    replacements.sort_by(|left, right| {
        replacement_kind_rank(left.kind)
            .cmp(&replacement_kind_rank(right.kind))
            .then_with(|| {
                u8::from(
                    left.kind == RestoreImportReplacementKind::RuntimeDatabase
                        && left.source_path.file_name().is_some_and(|name| {
                            name == "goals_1.sqlite"
                                || name.to_string_lossy().ends_with(".replacement")
                                || name.to_string_lossy().ends_with(".witness")
                        }),
                )
                .cmp(&u8::from(
                    right.kind == RestoreImportReplacementKind::RuntimeDatabase
                        && right.source_path.file_name().is_some_and(|name| {
                            name == "goals_1.sqlite"
                                || name.to_string_lossy().ends_with(".replacement")
                                || name.to_string_lossy().ends_with(".witness")
                        }),
                ))
            })
            .then_with(|| path_key(&left.target_path).cmp(&path_key(&right.target_path)))
    });
    let mut targets = BTreeSet::new();
    if replacements
        .iter()
        .any(|replacement| !targets.insert(path_key(&replacement.target_path)))
    {
        return Err("restore import replacement target is duplicated".to_string());
    }
    Ok(replacements)
}

fn is_created_goals_database(database: &MigrationDatabaseApplyEntry) -> bool {
    database.database_id.starts_with("goals-db-created-")
}

fn goals_creation_witness(
    operation_id: &str,
    database: &MigrationDatabaseApplyEntry,
) -> Result<PathBuf, String> {
    ownership_witness_path(&database.target_path, operation_id)
}

fn restore_database_role_rank(role: DatabaseRole) -> u8 {
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

fn build_restore_import_replacement(
    operation_id: &str,
    kind: RestoreImportReplacementKind,
    target_path: &Path,
    source_path: &Path,
    live_original_sha256: &str,
    rollback_snapshot_sha256: &str,
    replacement_sha256: &str,
) -> Result<RestoreImportReplacementPlan, String> {
    if !target_path.is_absolute() || !source_path.is_absolute() {
        return Err("restore import replacement paths must be absolute".to_string());
    }
    let target_parent = target_path
        .parent()
        .ok_or_else(|| "restore import replacement target has no parent".to_string())?;
    let target_name = target_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "restore import replacement target name is invalid".to_string())?;
    let digest = hex_sha256(Sha256::digest(
        format!("{operation_id}\0{}", path_key(target_path)).as_bytes(),
    ));
    let prefix = format!(
        ".{target_name}.codex-switch-restore-import-{}",
        &digest[..32]
    );
    Ok(RestoreImportReplacementPlan {
        kind,
        target_path: target_path.to_path_buf(),
        source_path: source_path.to_path_buf(),
        live_original_sha256: live_original_sha256.to_string(),
        rollback_snapshot_sha256: rollback_snapshot_sha256.to_string(),
        replacement_sha256: replacement_sha256.to_string(),
        original_witness_path: target_parent.join(format!("{prefix}.original")),
        replacement_witness_path: target_parent.join(format!("{prefix}.replacement")),
        staging_path: target_parent.join(format!("{prefix}.staging")),
        recovery_path: target_parent.join(format!("{prefix}.recovery")),
        tombstone_path: target_parent.join(format!("{prefix}.tombstone")),
    })
}

fn validate_persisted_restore_import_replacements(
    plan: &RestoreImportPlan,
    replacements: &[RestoreImportReplacementPlan],
) -> Result<(), String> {
    let persisted_live_hashes = replacements
        .iter()
        .map(|replacement| {
            (
                path_key(&replacement.target_path),
                replacement.live_original_sha256.clone(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    if persisted_live_hashes.len() != replacements.len() {
        return Err("restore import replacement target is duplicated".to_string());
    }
    let expected = build_restore_import_replacements_with_live_hashes(
        &plan.operation_id,
        &plan.sessions,
        &plan.databases,
        &persisted_live_hashes,
    )?;
    if expected != replacements {
        return Err("restore import replacement plan integrity check failed".to_string());
    }
    Ok(())
}

fn build_restore_import_replacements_with_live_hashes(
    operation_id: &str,
    sessions: &[RestoreImportSessionPlan],
    databases: &[MigrationDatabaseApplyEntry],
    persisted_live_hashes: &BTreeMap<String, String>,
) -> Result<Vec<RestoreImportReplacementPlan>, String> {
    build_restore_import_replacements_inner(
        operation_id,
        sessions,
        databases,
        Some(persisted_live_hashes),
    )
}

fn replacement_kind_rank(kind: RestoreImportReplacementKind) -> u8 {
    match kind {
        RestoreImportReplacementKind::SessionExtension => 0,
        RestoreImportReplacementKind::RuntimeDatabase => 1,
    }
}

fn validate_replacement_paths_unoccupied(
    replacements: &[RestoreImportReplacementPlan],
) -> Result<(), String> {
    let mut reserved = BTreeSet::new();
    for replacement in replacements {
        for path in [
            &replacement.original_witness_path,
            &replacement.replacement_witness_path,
            &replacement.staging_path,
            &replacement.recovery_path,
            &replacement.tombstone_path,
        ] {
            let key = path_key(path);
            if key == path_key(&replacement.target_path)
                || key == path_key(&replacement.source_path)
                || !reserved.insert(key)
            {
                return Err("restore import replacement path collides with the plan".to_string());
            }
            match fs::symlink_metadata(path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Ok(_) => {
                    return Err(
                        "restore import replacement recovery path is already occupied".to_string(),
                    )
                }
                Err(_) => {
                    return Err(
                        "restore import replacement recovery path is unavailable".to_string()
                    )
                }
            }
        }
    }
    Ok(())
}

fn validate_restore_import_replacement_plan_paths(
    plan: &RestoreImportPlan,
    replacements: &[RestoreImportReplacementPlan],
) -> Result<(), String> {
    let plan_paths = plan
        .sessions
        .iter()
        .flat_map(|session| {
            [
                Some(&session.source_path),
                session.source_staged_path.as_ref(),
                Some(&session.source_database_snapshot),
                Some(&session.canonical_path),
                session.canonical_backup_payload.as_ref(),
            ]
        })
        .chain(
            plan.source_databases
                .iter()
                .flat_map(|database| [Some(&database.source_path), Some(&database.snapshot_path)]),
        )
        .chain(plan.databases.iter().flat_map(|database| {
            [
                Some(&database.target_path),
                Some(&database.staged_path),
                Some(&database.original_backup_payload),
            ]
        }))
        .flatten()
        .map(|path| path_key(path))
        .collect::<BTreeSet<_>>();
    let mut replacement_paths = BTreeSet::new();
    for replacement in replacements {
        if !replacement.target_path.is_absolute()
            || !replacement.source_path.is_absolute()
            || replacement.live_original_sha256.len() != 64
            || !replacement.live_original_sha256.bytes().all(is_lower_hex)
            || replacement.rollback_snapshot_sha256.len() != 64
            || !replacement
                .rollback_snapshot_sha256
                .bytes()
                .all(is_lower_hex)
            || replacement.replacement_sha256.len() != 64
            || !replacement.replacement_sha256.bytes().all(is_lower_hex)
        {
            return Err("restore import replacement checksum is invalid".to_string());
        }
        let expected = build_restore_import_replacement(
            &plan.operation_id,
            replacement.kind,
            &replacement.target_path,
            &replacement.source_path,
            &replacement.live_original_sha256,
            &replacement.rollback_snapshot_sha256,
            &replacement.replacement_sha256,
        )?;
        if expected != *replacement {
            return Err("restore import replacement paths are not operation-bound".to_string());
        }
        validate_existing_path_ancestors(&replacement.target_path, "replacement target")?;
        let target_parent = replacement
            .target_path
            .parent()
            .ok_or_else(|| "restore import replacement target has no parent".to_string())?;
        if replacement.original_witness_path.parent() != Some(target_parent)
            || replacement.replacement_witness_path.parent() != Some(target_parent)
            || replacement.staging_path.parent() != Some(target_parent)
            || replacement.recovery_path.parent() != Some(target_parent)
            || replacement.tombstone_path.parent() != Some(target_parent)
        {
            return Err(
                "restore import replacement artifacts must stay beside the target".to_string(),
            );
        }
        for path in [
            &replacement.original_witness_path,
            &replacement.replacement_witness_path,
            &replacement.staging_path,
            &replacement.recovery_path,
            &replacement.tombstone_path,
        ] {
            let key = path_key(path);
            if plan_paths.contains(&key) || !replacement_paths.insert(key) {
                return Err("restore import replacement path collides with the plan".to_string());
            }
        }
    }
    for database in plan
        .databases
        .iter()
        .filter(|database| is_created_goals_database(database))
    {
        let witness = goals_creation_witness(&plan.operation_id, database)?;
        if witness.parent() != database.target_path.parent() {
            return Err("restore import created goals witness is not target-local".to_string());
        }
        let key = path_key(&witness);
        if plan_paths.contains(&key) || !replacement_paths.insert(key) {
            return Err("restore import created goals witness collides with the plan".to_string());
        }
    }
    Ok(())
}

fn validate_restore_import_replacement_sources(
    replacements: &[RestoreImportReplacementPlan],
) -> Result<(), String> {
    for replacement in replacements {
        validate_existing_path_ancestors(&replacement.source_path, "replacement source")?;
        let source_metadata = fs::symlink_metadata(&replacement.source_path)
            .map_err(|_| "restore import replacement source is unavailable".to_string())?;
        if !source_metadata.is_file() || metadata_is_link_or_reparse(&source_metadata) {
            return Err("restore import replacement source is unsafe".to_string());
        }
        if stable_file_digest(&replacement.source_path)?.1 != replacement.replacement_sha256 {
            return Err("restore import replacement source changed".to_string());
        }
    }
    Ok(())
}

fn validate_existing_path_ancestors(path: &Path, label: &str) -> Result<(), String> {
    let mut current = path
        .parent()
        .ok_or_else(|| format!("restore import {label} has no parent"))?;
    loop {
        let metadata = fs::symlink_metadata(current)
            .map_err(|_| format!("restore import {label} parent is unavailable"))?;
        if !metadata.is_dir() || metadata_is_link_or_reparse(&metadata) {
            return Err(format!("restore import {label} parent is unsafe"));
        }
        let Some(parent) = current.parent() else {
            break;
        };
        if parent == current {
            break;
        }
        current = parent;
    }
    Ok(())
}

fn prepare_runtime_databases(
    codex_home: &Path,
    data_root: &Path,
    staging_root: &Path,
    recovery_root: &Path,
    source_goals: &[PathBuf],
) -> Result<
    (
        Vec<MigrationDatabaseApplyEntry>,
        Vec<LedgerDatabaseSnapshot>,
    ),
    String,
> {
    let discovery = discover_database_catalog(codex_home, data_root);
    if discovery.errors > 0 {
        return Err("runtime database discovery is incomplete".to_string());
    }
    let mut missing_goals = BTreeMap::<String, (PathBuf, DatabaseRole)>::new();
    for descriptor in discovery
        .descriptors
        .iter()
        .filter(|descriptor| descriptor.role.is_runtime())
    {
        let path = descriptor
            .path
            .parent()
            .ok_or_else(|| "runtime database has no SQLite home".to_string())?
            .join("goals_1.sqlite");
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() && !metadata_is_link_or_reparse(&metadata) => {}
            Ok(_) => return Err("runtime goals database is unsafe".to_string()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let key = path_key(&path);
                match missing_goals.get_mut(&key) {
                    Some((_, role))
                        if restore_database_role_rank(descriptor.role)
                            < restore_database_role_rank(*role) =>
                    {
                        *role = descriptor.role;
                    }
                    Some(_) => {}
                    None => {
                        missing_goals.insert(key, (path, descriptor.role));
                    }
                }
            }
            Err(_) => return Err("runtime goals database is unavailable".to_string()),
        }
    }
    if discovery.goals_errors > missing_goals.len() {
        return Err("runtime goals database identity discovery is incomplete".to_string());
    }
    let mut descriptors = discovery
        .descriptors
        .into_iter()
        .filter(|database| database.role.is_runtime())
        .collect::<Vec<_>>();
    descriptors.sort_by_key(|database| (database.role, path_key(&database.path)));
    descriptors.dedup_by(|left, right| path_key(&left.path) == path_key(&right.path));
    if !descriptors
        .iter()
        .any(|database| database.role == DatabaseRole::CanonicalAccount)
    {
        return Err("canonical Account database is missing".to_string());
    }
    let backup_root = recovery_root.join("databases");
    let stage_root = staging_root.join("databases");
    create_safe_directory(&backup_root)?;
    create_safe_directory(&stage_root)?;
    let mut entries = Vec::new();
    let mut snapshots = Vec::new();
    for (index, descriptor) in descriptors.iter().enumerate() {
        let backup = backup_root.join(format!("{index:04}.sqlite"));
        let staged = stage_root.join(format!("{index:04}.sqlite"));
        snapshot_sqlite(&descriptor.path, &backup)?;
        atomic_copy(&backup, &staged)?;
        let (bytes, sha256) = stable_file_digest(&backup)?;
        if stable_file_digest(&staged)? != (bytes, sha256.clone()) {
            return Err("restore import database staging verification failed".to_string());
        }
        entries.push(MigrationDatabaseApplyEntry {
            database_id: descriptor.id.clone(),
            role: descriptor.role,
            target_path: descriptor.path.clone(),
            staged_path: staged,
            original_backup_payload: backup.clone(),
            original_sha256: sha256.clone(),
            staged_sha256: sha256.clone(),
            staged_bytes: bytes,
        });
        snapshots.push(LedgerDatabaseSnapshot {
            source_path: descriptor.path.clone(),
            snapshot_path: backup,
            bytes,
            sha256,
        });
    }
    let mut goals_descriptors = discovery.goals_descriptors;
    goals_descriptors.sort_by_key(|descriptor| path_key(&descriptor.source_path));
    for descriptor in goals_descriptors {
        for (view_index, view) in descriptor.views.iter().enumerate() {
            let index = entries.len();
            let backup = backup_root.join(format!("{index:04}-goals.sqlite"));
            let staged = stage_root.join(format!("{index:04}-goals.sqlite"));
            snapshot_sqlite(&view.path, &backup)?;
            atomic_copy(&backup, &staged)?;
            let (bytes, sha256) = stable_file_digest(&backup)?;
            if stable_file_digest(&staged)? != (bytes, sha256.clone()) {
                return Err("restore import goals staging verification failed".to_string());
            }
            entries.push(MigrationDatabaseApplyEntry {
                database_id: format!("{}-view-{view_index:04}", descriptor.id),
                role: view.role,
                target_path: view.path.clone(),
                staged_path: staged,
                original_backup_payload: backup.clone(),
                original_sha256: sha256.clone(),
                staged_sha256: sha256.clone(),
                staged_bytes: bytes,
            });
            snapshots.push(LedgerDatabaseSnapshot {
                source_path: view.path.clone(),
                snapshot_path: backup,
                bytes,
                sha256,
            });
        }
    }
    let create_missing_goals = !source_goals.is_empty()
        || entries
            .iter()
            .any(|database| database.database_id.starts_with("goals-db-"));
    if create_missing_goals {
        let template = source_goals.first().cloned().or_else(|| {
            entries
                .iter()
                .find(|database| database.database_id.starts_with("goals-db-"))
                .map(|database| database.staged_path.clone())
        });
        let template = template
            .ok_or_else(|| "restore import goals creation template is missing".to_string())?;
        for (created_index, (_, (target_path, role))) in missing_goals.into_iter().enumerate() {
            let index = entries.len();
            let staged = stage_root.join(format!("{index:04}-goals-created.sqlite"));
            snapshot_sqlite(&template, &staged)?;
            let (bytes, sha256) = stable_file_digest(&staged)?;
            entries.push(MigrationDatabaseApplyEntry {
                database_id: format!("goals-db-created-{created_index:04}-view-0000"),
                role,
                target_path,
                staged_path: staged,
                original_backup_payload: backup_root
                    .join(format!("{index:04}-goals-absent.marker")),
                original_sha256: "0".repeat(64),
                staged_sha256: sha256,
                staged_bytes: bytes,
            });
        }
    }
    Ok((entries, snapshots))
}

fn merge_restore_import_goals_views(
    source_goals: &[PathBuf],
    databases: &mut [MigrationDatabaseApplyEntry],
) -> Result<(), String> {
    let mut goals = databases
        .iter()
        .filter(|database| database.database_id.starts_with("goals-db-"))
        .cloned()
        .collect::<Vec<_>>();
    if goals.is_empty() {
        return if source_goals.is_empty() {
            Ok(())
        } else {
            Err("restore import goals database inventory is incomplete".to_string())
        };
    }
    let source_root = goals[0]
        .staged_path
        .parent()
        .ok_or_else(|| "restore import goals staging root is invalid".to_string())?
        .join("source-goals");
    create_safe_directory(&source_root)?;
    for (index, source) in source_goals.iter().enumerate() {
        let staged = source_root.join(format!("source-{index:04}.sqlite"));
        atomic_copy(source, &staged)?;
        let (bytes, sha256) = stable_file_digest(&staged)?;
        goals.push(MigrationDatabaseApplyEntry {
            database_id: format!("goals-db-source-{index:04}-view-0000"),
            role: DatabaseRole::DowngradeExport,
            target_path: source.clone(),
            staged_path: staged,
            original_backup_payload: source.clone(),
            original_sha256: sha256.clone(),
            staged_sha256: sha256,
            staged_bytes: bytes,
        });
    }
    merge_goals_database_views(&mut goals)?;
    let merged = goals
        .first()
        .ok_or_else(|| "restore import merged goals database is missing".to_string())?;
    for database in databases
        .iter_mut()
        .filter(|database| database.database_id.starts_with("goals-db-"))
    {
        database.staged_path = merged.staged_path.clone();
        database.staged_sha256 = merged.staged_sha256.clone();
        database.staged_bytes = merged.staged_bytes;
    }
    Ok(())
}

fn merge_restore_import_runtime_views(
    sessions: &[MigrationSessionApplyEntry],
    databases: &mut Vec<MigrationDatabaseApplyEntry>,
    source_state: &[PathBuf],
    source_goals: &[PathBuf],
    synthetic_thread_ids: &BTreeSet<String>,
) -> Result<(), String> {
    let (mut goals, mut state): (Vec<_>, Vec<_>) = std::mem::take(databases)
        .into_iter()
        .partition(|database| database.database_id.starts_with("goals-db-"));
    merge_restore_import_database_views(sessions, &mut state, source_state, synthetic_thread_ids)?;
    merge_restore_import_goals_views(source_goals, &mut goals)?;
    state.extend(goals);
    state.sort_by(|left, right| left.database_id.cmp(&right.database_id));
    *databases = state;
    Ok(())
}

fn to_migration_session_entry(
    session: &RestoreImportSessionPlan,
) -> Option<Result<MigrationSessionApplyEntry, String>> {
    let action = match session.action {
        RestoreImportSessionAction::ImportNew => MigrationSessionAction::CopyToCanonical,
        RestoreImportSessionAction::ImportExtension => {
            MigrationSessionAction::ReplaceCanonicalWithExtension
        }
        RestoreImportSessionAction::Unchanged | RestoreImportSessionAction::KeepCanonical => {
            return None;
        }
    };
    Some(Ok(MigrationSessionApplyEntry {
        thread_id: session.thread_id.clone(),
        action,
        source_path: session.source_path.clone(),
        target_path: session.canonical_path.clone(),
        staged_path: session.source_staged_path.clone(),
        expected_sha256: session.source_sha256.clone(),
        target_before_sha256: session.canonical_before_sha256.clone(),
        target_backup_payload: session.canonical_backup_payload.clone(),
    }))
}

fn prepared_result(
    plan: RestoreImportPlan,
    database_snapshots: Vec<LedgerDatabaseSnapshot>,
) -> Result<PreparedRestoreImport, String> {
    let mut paths = BTreeMap::<String, PathBuf>::new();
    let persisted_plan_path = plan_path(&plan.data_root, &plan.operation_id)?;
    paths.insert(path_key(&persisted_plan_path), persisted_plan_path);
    let persisted_phase_path = replacement_phase_path(&plan.data_root, &plan.operation_id)?;
    if persisted_phase_path.is_file() {
        paths.insert(path_key(&persisted_phase_path), persisted_phase_path);
    }
    for root in [&plan.staging_root, &plan.recovery_root] {
        for entry in WalkDir::new(root).follow_links(false) {
            let entry = entry
                .map_err(|_| "failed to inventory restore import operation files".to_string())?;
            let metadata = fs::symlink_metadata(entry.path())
                .map_err(|_| "failed to inspect restore import operation file".to_string())?;
            if metadata_is_link_or_reparse(&metadata) || (!metadata.is_file() && !metadata.is_dir())
            {
                return Err("restore import operation contains an unsafe entry".to_string());
            }
            if metadata.is_file() {
                paths.insert(path_key(entry.path()), entry.path().to_path_buf());
            }
        }
    }
    let mut created_files = Vec::with_capacity(paths.len());
    for path in paths.into_values() {
        let (bytes, sha256) = stable_file_digest(&path)?;
        created_files.push(LedgerFileSnapshot {
            path,
            bytes,
            sha256,
            created_by_operation: true,
            logical_thread_id: None,
        });
    }
    let mut rollback_steps = Vec::new();
    for database in &plan.databases {
        rollback_steps.push(LedgerRollbackStep {
            action: if is_created_goals_database(database) {
                RollbackActionKind::RemoveCreatedFile
            } else {
                RollbackActionKind::RestoreDatabase
            },
            source_path: database.original_backup_payload.clone(),
            target_path: database.target_path.clone(),
            expected_sha256: Some(if is_created_goals_database(database) {
                database.staged_sha256.clone()
            } else {
                database.original_sha256.clone()
            }),
            applied_sha256: Some(database.staged_sha256.clone()),
            completed: false,
        });
    }
    for session in &plan.sessions {
        match session.action {
            RestoreImportSessionAction::ImportNew => rollback_steps.push(LedgerRollbackStep {
                action: RollbackActionKind::RemoveCreatedFile,
                source_path: session.canonical_path.clone(),
                target_path: session.canonical_path.clone(),
                expected_sha256: Some(session.source_sha256.clone()),
                applied_sha256: Some(session.source_sha256.clone()),
                completed: false,
            }),
            RestoreImportSessionAction::ImportExtension => {
                rollback_steps.push(LedgerRollbackStep {
                    action: RollbackActionKind::RestoreFile,
                    source_path: session
                        .canonical_backup_payload
                        .clone()
                        .ok_or_else(|| "restore import canonical backup is missing".to_string())?,
                    target_path: session.canonical_path.clone(),
                    expected_sha256: session.canonical_before_sha256.clone(),
                    applied_sha256: Some(session.source_sha256.clone()),
                    completed: false,
                })
            }
            RestoreImportSessionAction::Unchanged | RestoreImportSessionAction::KeepCanonical => {}
        }
    }
    Ok(PreparedRestoreImport {
        plan,
        created_files,
        database_snapshots,
        rollback_steps,
    })
}

pub fn execute_restore_import<Guard>(
    plan: &RestoreImportPlan,
    before_live_write: Guard,
) -> Result<RestoreImportReceipt, String>
where
    Guard: FnMut() -> Result<(), String>,
{
    validate_plan(plan)?;
    load_restore_import_replacements(plan)?;
    let mut barriers = acquire_restore_write_barriers(plan)?;
    validate_restore_import_preconditions(plan, &mut barriers)?;
    execute_restore_import_after_preconditions(plan, before_live_write, &mut barriers)
}

pub fn execute_restore_import_classified<Guard>(
    plan: &RestoreImportPlan,
    mut mark_live_mutation_started: impl FnMut() -> Result<(), String>,
    before_live_write: Guard,
) -> Result<RestoreImportReceipt, RestoreImportApplyFailure>
where
    Guard: FnMut() -> Result<(), String>,
{
    validate_plan(plan).map_err(RestoreImportApplyFailure::Precondition)?;
    load_restore_import_replacements(plan).map_err(RestoreImportApplyFailure::Precondition)?;
    let mut barriers =
        acquire_restore_write_barriers(plan).map_err(RestoreImportApplyFailure::Precondition)?;
    validate_restore_import_preconditions(plan, &mut barriers)
        .map_err(RestoreImportApplyFailure::Precondition)?;
    mark_live_mutation_started().map_err(RestoreImportApplyFailure::Precondition)?;
    execute_restore_import_after_preconditions(plan, before_live_write, &mut barriers)
        .map_err(RestoreImportApplyFailure::Operation)
}

fn execute_restore_import_after_preconditions<Guard>(
    plan: &RestoreImportPlan,
    mut before_live_write: Guard,
    barriers: &mut BTreeMap<String, WriteExclusionGuard>,
) -> Result<RestoreImportReceipt, String>
where
    Guard: FnMut() -> Result<(), String>,
{
    let imported = imported_sessions(plan).collect::<Vec<_>>();
    let replacements = load_restore_import_replacements(plan)?;
    validate_restore_import_replacement_sources(&replacements)?;
    if !replacements.is_empty() {
        load_restore_import_replacement_phases(plan, &replacements)?;
    }
    let replacements_by_target = replacements
        .iter()
        .map(|replacement| (path_key(&replacement.target_path), replacement))
        .collect::<BTreeMap<_, _>>();
    for session in &imported {
        before_live_write()?;
        let staged = session
            .source_staged_path
            .as_ref()
            .ok_or_else(|| "restore import session staging is missing".to_string())?;
        match session.action {
            RestoreImportSessionAction::ImportNew => {
                let witness = import_new_ownership_witness(plan, session)?;
                let created =
                    atomic_create_with_witness(&session.canonical_path, &witness, |target| {
                        let mut source = fs::File::open(staged).map_err(|_| {
                            "failed to open staged restore import session".to_string()
                        })?;
                        io::copy(&mut source, target)
                            .map(|_| ())
                            .map_err(|_| "failed to publish restored session".to_string())
                    })?;
                if !created {
                    return Err("restore import canonical target appeared concurrently".to_string());
                }
                barriers.insert(
                    path_key(&session.canonical_path),
                    WriteExclusionGuard::acquire(&session.canonical_path)?,
                );
            }
            RestoreImportSessionAction::ImportExtension => {
                let replacement = replacements_by_target
                    .get(&path_key(&session.canonical_path))
                    .copied()
                    .ok_or_else(|| {
                        "restore import canonical replacement plan is missing".to_string()
                    })?;
                publish_restore_import_replacement(plan, &replacements, replacement, barriers)?;
            }
            RestoreImportSessionAction::Unchanged | RestoreImportSessionAction::KeepCanonical => {
                unreachable!("filtered above")
            }
        }
        verify_session_identity(
            &session.canonical_path,
            &session.thread_id,
            &session.source_sha256,
        )?;
    }
    {
        let canonical_goals_target = plan
            .databases
            .iter()
            .filter(|database| {
                database
                    .target_path
                    .file_name()
                    .is_some_and(|name| name == "goals_1.sqlite")
            })
            .min_by_key(|database| {
                (
                    restore_database_role_rank(database.role),
                    path_key(&database.target_path),
                )
            })
            .map(|database| database.target_path.clone());
        let mut ordered_databases = plan.databases.iter().collect::<Vec<_>>();
        ordered_databases.sort_by_key(|database| {
            (
                database
                    .target_path
                    .file_name()
                    .is_some_and(|name| name == "goals_1.sqlite"),
                canonical_goals_target.as_ref().is_some_and(|canonical| {
                    path_key(canonical) != path_key(&database.target_path)
                }),
                database.database_id.clone(),
            )
        });
        for database in ordered_databases {
            before_live_write()?;
            if is_created_goals_database(database) {
                publish_created_goals_database(
                    plan,
                    database,
                    canonical_goals_target.as_deref(),
                    barriers,
                )?;
                continue;
            }
            let replacement = replacements_by_target
                .get(&path_key(&database.target_path))
                .copied();
            let live = guarded_restore_database_digest(plan, database, replacement, barriers)
                .map_err(|error| {
                    format!(
                        "restore import database apply guard failed for {}: {error}",
                        database.database_id
                    )
                })?;
            if live != database.original_sha256 && live != database.staged_sha256 {
                return Err("restore import database changed after planning".to_string());
            }
            let needs_identity_convergence = database
                .target_path
                .file_name()
                .is_some_and(|name| name == "goals_1.sqlite")
                && replacement.is_some();
            if live != database.staged_sha256 || needs_identity_convergence {
                let replacement = replacement.ok_or_else(|| {
                    "restore import database replacement plan is missing".to_string()
                })?;
                quick_check_sqlite(&replacement.source_path)
                    .map_err(|error| format!("staged replacement database is invalid: {error}"))?;
                if !sqlite_sidecars_absent(&database.target_path)? {
                    return Err(
                        "restore import database has an active sidecar before replacement"
                            .to_string(),
                    );
                }
                publish_restore_import_replacement(plan, &replacements, replacement, barriers)?;
                if !sqlite_sidecars_absent(&database.target_path)? {
                    return Err(
                        "restore import database gained a sidecar during replacement".to_string(),
                    );
                }
                guarded_restore_database_quick_check(
                    plan,
                    database,
                    "published",
                    &database.staged_sha256,
                    barriers,
                )
                .map_err(|error| format!("published replacement database is invalid: {error}"))?;
            }
            if replacement.is_some() {
                let sha256 = barriers
                    .get_mut(&path_key(&database.target_path))
                    .ok_or_else(|| "restore import database writer barrier is missing".to_string())?
                    .verify_current_path(Some(&database.staged_sha256))?
                    .1;
                if sha256 != database.staged_sha256 {
                    return Err(
                        "restore import database replacement verification failed".to_string()
                    );
                }
            }
        }
    }
    validate_applied_restore_import_with_barriers(plan, receipt_from_plan(plan, false), barriers)
}

fn publish_created_goals_database(
    plan: &RestoreImportPlan,
    database: &MigrationDatabaseApplyEntry,
    canonical_goals_target: Option<&Path>,
    barriers: &mut BTreeMap<String, WriteExclusionGuard>,
) -> Result<(), String> {
    if !is_created_goals_database(database)
        || database
            .target_path
            .file_name()
            .is_none_or(|name| name != "goals_1.sqlite")
    {
        return Err("restore import created goals plan is invalid".to_string());
    }
    let witness = goals_creation_witness(&plan.operation_id, database)?;
    for path in [&database.target_path, &witness] {
        match fs::symlink_metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(_) => return Err("restore import created goals target appeared".to_string()),
            Err(_) => return Err("restore import created goals target is unavailable".to_string()),
        }
    }
    let canonical = canonical_goals_target
        .ok_or_else(|| "restore import canonical goals target is missing".to_string())?;
    let is_canonical = path_key(canonical) == path_key(&database.target_path);
    if is_canonical {
        let created = atomic_create_with_witness(&database.target_path, &witness, |target| {
            let mut source = fs::File::open(&database.staged_path)
                .map_err(|_| "failed to open staged goals database".to_string())?;
            io::copy(&mut source, target)
                .map(|_| ())
                .map_err(|_| "failed to publish created goals database".to_string())
        })?;
        if !created {
            return Err("restore import created goals target appeared concurrently".to_string());
        }
    } else {
        barriers
            .get_mut(&path_key(canonical))
            .ok_or_else(|| "restore import canonical goals writer barrier is missing".to_string())?
            .verify_current_path(Some(&database.staged_sha256))?;
        fs::hard_link(canonical, &witness)
            .map_err(|_| "restore import goals views cannot converge across volumes".to_string())?;
        if let Err(error) = fs::hard_link(canonical, &database.target_path) {
            let mut guard = DestructiveFileGuard::acquire(&witness)?;
            if same_regular_file_identity(canonical, &witness).unwrap_or(false) {
                guard.verify_current_path(Some(&database.staged_sha256))?;
                guard.delete()?;
            }
            return Err(format!(
                "failed to publish created goals hardlink without replacement: {error}"
            ));
        }
    }
    let mut guard = WriteExclusionGuard::acquire(&database.target_path)?;
    guard.verify_current_path(Some(&database.staged_sha256))?;
    if !same_regular_file_identity(&database.target_path, &witness).unwrap_or(false)
        || (!is_canonical
            && !same_regular_file_identity(canonical, &database.target_path).unwrap_or(false))
    {
        return Err("restore import created goals identity verification failed".to_string());
    }
    let target_key = path_key(&database.target_path);
    if barriers.insert(target_key, guard).is_some() {
        return Err("restore import created goals writer barrier already exists".to_string());
    }
    with_guarded_restore_database_copy(
        plan,
        database,
        "created-goals",
        Some(&database.staged_sha256),
        barriers,
        |copy| {
            quick_check_sqlite(copy)?;
            let connection = Connection::open_with_flags(
                copy,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .map_err(|_| "failed to open created goals database".to_string())?;
            goals_database_digest(&connection).map(|_| ())
        },
    )
}

fn publish_restore_import_replacement(
    plan: &RestoreImportPlan,
    replacements: &[RestoreImportReplacementPlan],
    replacement: &RestoreImportReplacementPlan,
    barriers: &mut BTreeMap<String, WriteExclusionGuard>,
) -> Result<(), String> {
    let phase_entry = load_restore_import_replacement_phases(plan, replacements)?
        .replacements
        .into_iter()
        .find(|entry| entry.replacement == *replacement)
        .ok_or_else(|| "restore import replacement phase entry is missing".to_string())?;
    if phase_entry.phase != RestoreImportReplacementPhase::Planned
        || phase_entry.replacement_identity.is_some()
        || phase_entry.parent_identity
            != RestoreImportFileIdentity::from(parent_directory_identity_at_path(
                &replacement.target_path,
            )?)
        || stable_regular_file_identity(&replacement.original_witness_path)?
            != phase_entry.original_identity
    {
        return Err("restore import replacement phase is not ready for apply".to_string());
    }
    if !replacement_artifact_exists(&replacement.original_witness_path, "original witness")?
        || stable_file_digest(&replacement.original_witness_path)?.1
            != replacement.live_original_sha256
        || !same_regular_file_identity(&replacement.target_path, &replacement.original_witness_path)
            .unwrap_or(false)
    {
        return Err("restore import original target identity changed before apply".to_string());
    }
    for path in [
        &replacement.replacement_witness_path,
        &replacement.staging_path,
        &replacement.recovery_path,
        &replacement.tombstone_path,
    ] {
        match fs::symlink_metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(_) => {
                return Err("restore import replacement artifact appeared before apply".to_string())
            }
            Err(_) => {
                return Err(
                    "restore import replacement artifact is unavailable before apply".to_string(),
                )
            }
        }
    }
    if stable_file_digest(&replacement.source_path)?.1 != replacement.replacement_sha256 {
        return Err("restore import replacement source changed".to_string());
    }
    let hardlink_goals_identity = replacement.kind == RestoreImportReplacementKind::RuntimeDatabase
        && replacement
            .target_path
            .file_name()
            .is_some_and(|name| name == "goals_1.sqlite");
    let key = path_key(&replacement.target_path);
    let mut barrier = barriers
        .remove(&key)
        .ok_or_else(|| "restore import replacement writer barrier is missing".to_string())?;
    barrier.verify_current_path(Some(&replacement.live_original_sha256))?;
    let replace_paths = HandleReplacePaths::from_persisted_plan(
        replacement.target_path.clone(),
        replacement.recovery_path.clone(),
        replacement.staging_path.clone(),
        replacement.tombstone_path.clone(),
    )?;
    transition_restore_import_replacement_phase(
        plan,
        replacements,
        replacement,
        &[RestoreImportReplacementPhase::Planned],
        RestoreImportReplacementPhase::Staging,
    )?;
    let staged = if hardlink_goals_identity {
        barrier
            .stage_handle_hardlink_replace(
                &replacement.source_path,
                &replacement.replacement_sha256,
                &replace_paths,
            )
            .map_err(|error| format!("restore import goals views cannot converge: {error}"))?
    } else {
        barrier.stage_handle_replace(
            &replacement.source_path,
            &replacement.replacement_sha256,
            &replace_paths,
        )?
    };
    if staged.paths() != &replace_paths {
        return Err("restore import staged replacement identity changed".to_string());
    }
    let identities = staged.identity_bindings()?;
    record_restore_import_replacement_staged(plan, replacements, replacement, identities)?;
    fs::hard_link(
        &replacement.staging_path,
        &replacement.replacement_witness_path,
    )
    .map_err(|_| "failed to bind restore import replacement identity witness".to_string())?;
    if stable_file_digest(&replacement.replacement_witness_path)?.1
        != replacement.replacement_sha256
        || stable_regular_file_identity(&replacement.replacement_witness_path)?
            != identities.replacement_identity.into()
        || !same_regular_file_identity(
            &replacement.staging_path,
            &replacement.replacement_witness_path,
        )
        .unwrap_or(false)
    {
        return Err("restore import replacement witness verification failed".to_string());
    }
    transition_restore_import_replacement_phase(
        plan,
        replacements,
        replacement,
        &[RestoreImportReplacementPhase::Staged],
        RestoreImportReplacementPhase::Preparing,
    )?;
    let prepared = staged.prepare().map_err(|(error, _staged)| error)?;
    if prepared.paths() != &replace_paths {
        return Err("restore import replacement recovery identity changed".to_string());
    }
    transition_restore_import_replacement_phase(
        plan,
        replacements,
        replacement,
        &[RestoreImportReplacementPhase::Preparing],
        RestoreImportReplacementPhase::Prepared,
    )?;
    transition_restore_import_replacement_phase(
        plan,
        replacements,
        replacement,
        &[RestoreImportReplacementPhase::Prepared],
        RestoreImportReplacementPhase::Publishing,
    )?;
    let published = prepared.publish().map_err(|(error, _prepared)| error)?;
    if published.paths() != &replace_paths {
        return Err("restore import published recovery identity changed".to_string());
    }
    transition_restore_import_replacement_phase(
        plan,
        replacements,
        replacement,
        &[RestoreImportReplacementPhase::Publishing],
        RestoreImportReplacementPhase::Published,
    )?;
    transition_restore_import_replacement_phase(
        plan,
        replacements,
        replacement,
        &[RestoreImportReplacementPhase::Published],
        RestoreImportReplacementPhase::Committing,
    )?;
    let mut resolved = published.commit().map_err(|(error, _published)| error)?;
    if resolved.paths() != &replace_paths {
        return Err("restore import committed replacement identity changed".to_string());
    }
    resolved
        .guard_mut()
        .verify_current_path(Some(&replacement.replacement_sha256))?;
    transition_restore_import_replacement_phase(
        plan,
        replacements,
        replacement,
        &[RestoreImportReplacementPhase::Committing],
        RestoreImportReplacementPhase::CommittedWithRecovery,
    )?;
    let mut barrier = resolved.retain_for_recovery();
    barrier.verify_current_path(Some(&replacement.replacement_sha256))?;
    if !same_regular_file_identity(
        &replacement.target_path,
        &replacement.replacement_witness_path,
    )
    .unwrap_or(false)
    {
        return Err("restore import published replacement identity changed".to_string());
    }
    barriers.insert(key, barrier);
    Ok(())
}

pub fn validate_applied_restore_import(
    plan: &RestoreImportPlan,
    receipt: RestoreImportReceipt,
) -> Result<RestoreImportReceipt, String> {
    let mut barriers = acquire_restore_write_barriers(plan)?;
    validate_applied_restore_import_with_barriers(plan, receipt, &mut barriers)
}

fn validate_applied_restore_import_with_barriers(
    plan: &RestoreImportPlan,
    mut receipt: RestoreImportReceipt,
    barriers: &mut BTreeMap<String, WriteExclusionGuard>,
) -> Result<RestoreImportReceipt, String> {
    validate_plan(plan)?;
    let imported = imported_sessions(plan).collect::<Vec<_>>();
    let replacements = load_restore_import_replacements(plan)?;
    let replacements_by_target = replacements
        .iter()
        .map(|replacement| (path_key(&replacement.target_path), replacement))
        .collect::<BTreeMap<_, _>>();
    for session in &plan.sessions {
        let expected = if matches!(
            session.action,
            RestoreImportSessionAction::ImportNew | RestoreImportSessionAction::ImportExtension
        ) {
            session.source_sha256.as_str()
        } else {
            session.canonical_before_sha256.as_deref().ok_or_else(|| {
                "restore import retained canonical checksum is missing".to_string()
            })?
        };
        barriers
            .get_mut(&path_key(&session.canonical_path))
            .ok_or_else(|| "restore import validation writer barrier is missing".to_string())?
            .verify_current_path(Some(expected))?;
        verify_session_identity(&session.canonical_path, &session.thread_id, expected)?;
    }
    for database in &plan.databases {
        let replacement = replacements_by_target.get(&path_key(&database.target_path));
        let created_goals = is_created_goals_database(database);
        if replacement.is_none() && !created_goals {
            if database.original_sha256 != database.staged_sha256 {
                return Err("restore import database replacement plan is missing".to_string());
            }
            let logical_sha256 = guarded_restore_database_logical_digest(
                plan,
                database,
                "validation-retained",
                None,
                barriers,
            )?;
            if logical_sha256 != database.staged_sha256 {
                return Err("retained restore import database changed".to_string());
            }
        }
        with_guarded_restore_database_copy(
            plan,
            database,
            "validation",
            if created_goals {
                Some(database.staged_sha256.as_str())
            } else {
                replacement.map(|replacement| replacement.replacement_sha256.as_str())
            },
            barriers,
            |copy| {
                quick_check_sqlite(copy)?;
                let is_goals = database
                    .target_path
                    .file_name()
                    .is_some_and(|name| name == "goals_1.sqlite");
                if is_goals {
                    let connection = Connection::open_with_flags(
                        copy,
                        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                    )
                    .map_err(|_| "failed to open restored runtime database".to_string())?;
                    goals_database_digest(&connection)?;
                } else if !imported.is_empty() {
                    let connection = Connection::open_with_flags(
                        copy,
                        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                    )
                    .map_err(|_| "failed to open restored runtime database".to_string())?;
                    for session in &imported {
                        let path = connection
                            .query_row(
                                "SELECT rollout_path FROM threads WHERE id = ?1",
                                [&session.thread_id],
                                |row| row.get::<_, Option<String>>(0),
                            )
                            .map_err(|_| {
                                "restored runtime database is missing a thread row".to_string()
                            })?
                            .ok_or_else(|| "restored runtime thread path is missing".to_string())?;
                        if path_key(Path::new(&path)) != path_key(&session.canonical_path) {
                            return Err("restored runtime thread path is not canonical".to_string());
                        }
                    }
                }
                Ok(())
            },
        )
        .map_err(|error| {
            format!(
                "restore import database validation guard failed for {}: {error}",
                database.database_id
            )
        })?;
    }
    let goals_targets = plan
        .databases
        .iter()
        .filter(|database| {
            database
                .target_path
                .file_name()
                .is_some_and(|name| name == "goals_1.sqlite")
        })
        .map(|database| &database.target_path)
        .collect::<Vec<_>>();
    if let Some(canonical) = goals_targets.first() {
        for target in goals_targets.iter().skip(1) {
            if !same_regular_file_identity(canonical, target).unwrap_or(false) {
                return Err("restore import goals views did not converge".to_string());
            }
        }
    }
    for database in plan
        .databases
        .iter()
        .filter(|database| is_created_goals_database(database))
    {
        let witness = goals_creation_witness(&plan.operation_id, database)?;
        if stable_file_digest(&database.target_path)?.1 != database.staged_sha256
            || stable_file_digest(&witness)?.1 != database.staged_sha256
            || !same_regular_file_identity(&database.target_path, &witness).unwrap_or(false)
        {
            return Err("restore import created goals ownership changed".to_string());
        }
    }
    for replacement in &replacements {
        if stable_file_digest(&replacement.target_path)?.1 != replacement.replacement_sha256
            || stable_file_digest(&replacement.replacement_witness_path)?.1
                != replacement.replacement_sha256
            || !same_regular_file_identity(
                &replacement.target_path,
                &replacement.replacement_witness_path,
            )
            .unwrap_or(false)
            || stable_file_digest(&replacement.recovery_path)?.1 != replacement.live_original_sha256
            || stable_file_digest(&replacement.original_witness_path)?.1
                != replacement.live_original_sha256
            || !same_regular_file_identity(
                &replacement.recovery_path,
                &replacement.original_witness_path,
            )
            .unwrap_or(false)
            || replacement_artifact_exists(&replacement.staging_path, "staging")?
            || replacement_artifact_exists(&replacement.tombstone_path, "tombstone")?
        {
            return Err("restore import replacement ownership verification failed".to_string());
        }
    }
    validate_unclassified_payloads(plan)?;
    receipt.validated = true;
    Ok(receipt)
}

pub fn cleanup_restore_import_staging(plan: &RestoreImportPlan) -> Result<(), String> {
    if !plan.staging_root.exists() {
        return Ok(());
    }
    remove_owned_work_tree(&plan.staging_root, &plan.work_root, &plan.operation_id)
}

pub fn abort_unapplied_restore_import(
    store: &OperationLedgerStore,
    plan: &RestoreImportPlan,
    error_code: &str,
) -> Result<(), String> {
    validate_plan(plan)?;
    let ledger = store.load(&plan.operation_id)?;
    if ledger.kind != SessionStorageOperationKind::RestoreImport
        || ledger.phase != SessionStorageOperationPhase::Applying
        || ledger.canonical_root != plan.canonical_root
    {
        return Err("restore import precondition abort identity is invalid".to_string());
    }
    store.update(&plan.operation_id, |ledger| {
        ledger.last_error_code = Some(error_code.to_string());
        Ok(())
    })?;
    store.transition(
        &plan.operation_id,
        SessionStorageOperationPhase::RollingBack,
    )?;
    cleanup_unapplied_restore_import(plan)?;
    store.transition(&plan.operation_id, SessionStorageOperationPhase::RolledBack)?;
    Ok(())
}

pub fn restore_import_runtime_apply_plan(
    plan: &RestoreImportPlan,
) -> Result<Option<MigrationApplyPlan>, String> {
    let sessions = imported_sessions(plan)
        .map(|session| {
            to_migration_session_entry(session)
                .ok_or_else(|| "restore import runtime session action is invalid".to_string())?
        })
        .collect::<Result<Vec<_>, _>>()?;
    if sessions.is_empty() {
        return Ok(None);
    }
    Ok(Some(MigrationApplyPlan {
        schema_version: SESSION_STORAGE_SCHEMA_VERSION,
        operation_id: plan.operation_id.clone(),
        generated_at_ms: plan.generated_at_ms,
        canonical_root: plan.canonical_root.clone(),
        inventory_fingerprint: plan.source_fingerprint.clone(),
        backup_dir: plan.recovery_root.clone(),
        staging_root: plan.staging_root.clone(),
        sessions,
        databases: plan.databases.clone(),
        conflict_count: plan.conflicts.len(),
    }))
}

fn rollback_changed_after_apply(label: &str) -> String {
    format!("restore import {label} changed after apply; rollback deferred")
}

fn acquire_restore_rollback_write_barriers(
    plan: &RestoreImportPlan,
) -> Result<BTreeMap<String, WriteExclusionGuard>, String> {
    let mut paths = BTreeMap::<String, PathBuf>::new();
    for path in plan
        .sessions
        .iter()
        .map(|session| &session.canonical_path)
        .chain(plan.databases.iter().map(|database| &database.target_path))
    {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_file() && !metadata_is_link_or_reparse(&metadata) => {
                paths.insert(path_key(path), path.clone());
            }
            Ok(_) => return Err("restore import rollback target is unsafe".to_string()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return Err("restore import rollback target is unavailable".to_string()),
        }
    }
    let mut barriers = BTreeMap::new();
    for (key, path) in paths {
        barriers.insert(key, WriteExclusionGuard::acquire(&path)?);
    }
    Ok(barriers)
}

fn preflight_live_target(
    barriers: &mut BTreeMap<String, WriteExclusionGuard>,
    expected_targets: &mut BTreeMap<String, (String, RegularFileIdentity, String)>,
    path: &Path,
    expected_sha256: Option<&str>,
    expected_identity: Option<RegularFileIdentity>,
    label: &str,
) -> Result<(String, RegularFileIdentity), String> {
    let key = path_key(path);
    let barrier = barriers
        .get_mut(&key)
        .ok_or_else(|| rollback_changed_after_apply(label))?;
    let (_, sha256) = barrier
        .verify_current_path(expected_sha256)
        .map_err(|_| rollback_changed_after_apply(label))?;
    let identity = barrier
        .identity()
        .map_err(|_| rollback_changed_after_apply(label))?;
    if expected_identity.is_some_and(|expected| expected != identity) {
        return Err(rollback_changed_after_apply(label));
    }
    if let Some((existing_sha256, existing_identity, _)) = expected_targets.get(&key) {
        if existing_sha256 != &sha256 || *existing_identity != identity {
            return Err("restore import rollback target expectations conflict".to_string());
        }
    } else {
        expected_targets.insert(key, (sha256.clone(), identity, label.to_string()));
    }
    Ok((sha256, identity))
}

fn preflight_optional_identity_artifact(
    path: &Path,
    expected_sha256: &str,
    expected_identity: RestoreImportFileIdentity,
    label: &str,
) -> Result<(), String> {
    if !replacement_artifact_exists(path, label)? {
        return Ok(());
    }
    if stable_file_digest(path)?.1 != expected_sha256
        || stable_regular_file_identity(path)? != expected_identity
    {
        return Err(format!("restore import {label} identity changed"));
    }
    Ok(())
}

fn sqlite_logical_content_digest(path: &Path) -> Result<String, String> {
    use rusqlite::types::ValueRef;

    fn digest_field(hasher: &mut Sha256, bytes: &[u8]) {
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }

    quick_check_sqlite(path)?;
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| "failed to open restore import SQLite payload".to_string())?;
    let mut hasher = Sha256::new();
    hasher.update(b"codex-switch-sqlite-logical-v1\0");
    let tables = {
        let mut statement = connection
            .prepare(
                "SELECT type, name, tbl_name, COALESCE(sql, '') \
                 FROM sqlite_schema ORDER BY type, name, tbl_name, sql",
            )
            .map_err(|_| "failed to inspect restore import SQLite schema".to_string())?;
        let mut rows = statement
            .query([])
            .map_err(|_| "failed to inspect restore import SQLite schema".to_string())?;
        let mut tables = Vec::new();
        while let Some(row) = rows
            .next()
            .map_err(|_| "failed to inspect restore import SQLite schema".to_string())?
        {
            let object_type = row
                .get::<_, String>(0)
                .map_err(|_| "restore import SQLite schema is invalid".to_string())?;
            let name = row
                .get::<_, String>(1)
                .map_err(|_| "restore import SQLite schema is invalid".to_string())?;
            let table_name = row
                .get::<_, String>(2)
                .map_err(|_| "restore import SQLite schema is invalid".to_string())?;
            let sql = row
                .get::<_, String>(3)
                .map_err(|_| "restore import SQLite schema is invalid".to_string())?;
            digest_field(&mut hasher, object_type.as_bytes());
            digest_field(&mut hasher, name.as_bytes());
            digest_field(&mut hasher, table_name.as_bytes());
            digest_field(&mut hasher, sql.as_bytes());
            if object_type == "table" {
                tables.push(name);
            }
        }
        tables
    };
    for table in tables {
        digest_field(&mut hasher, table.as_bytes());
        let quoted = table.replace('"', "\"\"");
        let mut statement = connection
            .prepare(&format!("SELECT * FROM \"{quoted}\""))
            .map_err(|_| "failed to inspect restore import SQLite table".to_string())?;
        let column_count = statement.column_count();
        if column_count == 0 {
            return Err("restore import SQLite table has no columns".to_string());
        }
        let mut rows = statement
            .query([])
            .map_err(|_| "failed to inspect restore import SQLite table".to_string())?;
        let mut row_digests = Vec::<[u8; 32]>::new();
        while let Some(row) = rows
            .next()
            .map_err(|_| "failed to inspect restore import SQLite table".to_string())?
        {
            let mut row_hasher = Sha256::new();
            row_hasher.update((column_count as u64).to_le_bytes());
            for index in 0..column_count {
                match row
                    .get_ref(index)
                    .map_err(|_| "restore import SQLite row is invalid".to_string())?
                {
                    ValueRef::Null => row_hasher.update([0]),
                    ValueRef::Integer(value) => {
                        row_hasher.update([1]);
                        row_hasher.update(value.to_le_bytes());
                    }
                    ValueRef::Real(value) => {
                        row_hasher.update([2]);
                        row_hasher.update(value.to_bits().to_le_bytes());
                    }
                    ValueRef::Text(value) => {
                        row_hasher.update([3]);
                        digest_field(&mut row_hasher, value);
                    }
                    ValueRef::Blob(value) => {
                        row_hasher.update([4]);
                        digest_field(&mut row_hasher, value);
                    }
                }
            }
            row_digests.push(row_hasher.finalize().into());
        }
        row_digests.sort_unstable();
        hasher.update((row_digests.len() as u64).to_le_bytes());
        for digest in row_digests {
            hasher.update(digest);
        }
    }
    Ok(hex_sha256(hasher.finalize()))
}

fn preflight_rollback_restore_import(
    plan: &RestoreImportPlan,
    replacements: &[RestoreImportReplacementPlan],
    replacement_phases: &RestoreImportReplacementPhaseRecord,
) -> Result<(), String> {
    validate_restore_import_replacement_sources(replacements)?;
    let mut barriers = acquire_restore_rollback_write_barriers(plan)?;
    let mut expected_targets = BTreeMap::<String, (String, RegularFileIdentity, String)>::new();
    let mut replacement_states = BTreeMap::<String, HandleReplaceCrashState>::new();

    for replacement in replacements {
        let phase_entry = replacement_phases
            .replacements
            .iter()
            .find(|entry| entry.replacement == *replacement)
            .ok_or_else(|| "restore import replacement phase entry is missing".to_string())?;
        let replacement_label = match replacement.kind {
            RestoreImportReplacementKind::SessionExtension => "session replacement target",
            RestoreImportReplacementKind::RuntimeDatabase => "database replacement target",
        };
        match replacement.kind {
            RestoreImportReplacementKind::SessionExtension => {
                let session = plan
                    .sessions
                    .iter()
                    .find(|session| {
                        session.action == RestoreImportSessionAction::ImportExtension
                            && path_key(&session.canonical_path)
                                == path_key(&replacement.target_path)
                    })
                    .ok_or_else(|| {
                        "restore import extension rollback plan is missing".to_string()
                    })?;
                let backup = session.canonical_backup_payload.as_ref().ok_or_else(|| {
                    "restore import canonical rollback backup is missing".to_string()
                })?;
                if stable_file_digest(backup)?.1 != replacement.rollback_snapshot_sha256 {
                    return Err("restore import canonical rollback backup changed".to_string());
                }
                verify_session_identity(
                    backup,
                    &session.thread_id,
                    &replacement.rollback_snapshot_sha256,
                )?;
            }
            RestoreImportReplacementKind::RuntimeDatabase => {
                let database = plan
                    .databases
                    .iter()
                    .find(|database| {
                        path_key(&database.target_path) == path_key(&replacement.target_path)
                    })
                    .ok_or_else(|| {
                        "restore import database rollback plan is missing".to_string()
                    })?;
                if stable_file_digest(&database.original_backup_payload)?.1
                    != replacement.rollback_snapshot_sha256
                    || sqlite_logical_content_digest(&database.original_backup_payload).is_err()
                {
                    return Err("restore import database rollback backup changed".to_string());
                }
                if !sqlite_sidecars_absent(&replacement.target_path)? {
                    return Err(rollback_changed_after_apply("database target"));
                }
            }
        }

        if phase_entry.phase == RestoreImportReplacementPhase::Cleaned {
            preflight_live_target(
                &mut barriers,
                &mut expected_targets,
                &replacement.target_path,
                Some(&replacement.live_original_sha256),
                Some(phase_entry.original_identity.into()),
                replacement_label,
            )?;
            for path in [
                &replacement.original_witness_path,
                &replacement.replacement_witness_path,
                &replacement.staging_path,
                &replacement.recovery_path,
                &replacement.tombstone_path,
            ] {
                if replacement_artifact_exists(path, "cleaned rollback artifact")? {
                    return Err("restore import cleaned rollback artifact reappeared".to_string());
                }
            }
            replacement_states.insert(
                path_key(&replacement.target_path),
                HandleReplaceCrashState::Original,
            );
            continue;
        }

        let Some(replacement_identity) = phase_entry.replacement_identity else {
            if !matches!(
                phase_entry.phase,
                RestoreImportReplacementPhase::Planned | RestoreImportReplacementPhase::Staging
            ) {
                return Err("restore import replacement identity was not persisted".to_string());
            }
            preflight_live_target(
                &mut barriers,
                &mut expected_targets,
                &replacement.target_path,
                Some(&replacement.live_original_sha256),
                Some(phase_entry.original_identity.into()),
                replacement_label,
            )?;
            if parent_directory_identity_at_path(&replacement.target_path)?
                != phase_entry.parent_identity.into()
            {
                return Err("restore import replacement parent identity changed".to_string());
            }
            for path in [
                &replacement.replacement_witness_path,
                &replacement.staging_path,
                &replacement.recovery_path,
                &replacement.tombstone_path,
            ] {
                if replacement_artifact_exists(path, "planned rollback artifact")? {
                    return Err(
                        "restore import planned layout contains an unbound artifact".to_string()
                    );
                }
            }
            preflight_optional_identity_artifact(
                &replacement.original_witness_path,
                &replacement.live_original_sha256,
                phase_entry.original_identity,
                "original witness",
            )?;
            replacement_states.insert(
                path_key(&replacement.target_path),
                HandleReplaceCrashState::Original,
            );
            continue;
        };

        let identities = HandleReplaceIdentityBindings {
            parent_identity: phase_entry.parent_identity.into(),
            original_identity: phase_entry.original_identity.into(),
            replacement_identity: replacement_identity.into(),
        };
        if matches!(
            phase_entry.phase,
            RestoreImportReplacementPhase::Published
                | RestoreImportReplacementPhase::Committing
                | RestoreImportReplacementPhase::CommittedWithRecovery
        ) {
            preflight_live_target(
                &mut barriers,
                &mut expected_targets,
                &replacement.target_path,
                Some(&replacement.replacement_sha256),
                Some(identities.replacement_identity),
                replacement_label,
            )?;
        }
        let paths = HandleReplacePaths::from_persisted_plan(
            replacement.target_path.clone(),
            replacement.recovery_path.clone(),
            replacement.staging_path.clone(),
            replacement.tombstone_path.clone(),
        )?;
        let crash_state = classify_handle_replace_crash_state(
            &paths,
            identities,
            &replacement.live_original_sha256,
            &replacement.replacement_sha256,
        )
        .map_err(|_| {
            if matches!(
                phase_entry.phase,
                RestoreImportReplacementPhase::Published
                    | RestoreImportReplacementPhase::Committing
                    | RestoreImportReplacementPhase::CommittedWithRecovery
            ) {
                rollback_changed_after_apply(replacement_label)
            } else {
                replacement_rollback_deferred("physical replacement layout is unknown")
            }
        })?;
        if !restore_import_phase_accepts_crash_state(phase_entry.phase, crash_state) {
            return Err(replacement_rollback_deferred(
                "logical phase does not match the physical replacement layout",
            ));
        }
        match crash_state {
            HandleReplaceCrashState::Original
            | HandleReplaceCrashState::Staged
            | HandleReplaceCrashState::RolledBack => {
                preflight_live_target(
                    &mut barriers,
                    &mut expected_targets,
                    &replacement.target_path,
                    Some(&replacement.live_original_sha256),
                    Some(identities.original_identity),
                    replacement_label,
                )?;
            }
            HandleReplaceCrashState::ReplacementWithRecovery
            | HandleReplaceCrashState::ReplacementOnly => {
                preflight_live_target(
                    &mut barriers,
                    &mut expected_targets,
                    &replacement.target_path,
                    Some(&replacement.replacement_sha256),
                    Some(identities.replacement_identity),
                    replacement_label,
                )?;
            }
            HandleReplaceCrashState::Prepared | HandleReplaceCrashState::RollbackPrepared => {
                if barriers.contains_key(&path_key(&replacement.target_path)) {
                    return Err(rollback_changed_after_apply(replacement_label));
                }
            }
        }
        preflight_optional_identity_artifact(
            &replacement.original_witness_path,
            &replacement.live_original_sha256,
            phase_entry.original_identity,
            "original witness",
        )?;
        preflight_optional_identity_artifact(
            &replacement.replacement_witness_path,
            &replacement.replacement_sha256,
            replacement_identity,
            "replacement witness",
        )?;
        replacement_states.insert(path_key(&replacement.target_path), crash_state);
    }

    for session in &plan.sessions {
        match session.action {
            RestoreImportSessionAction::ImportNew => {
                let witness = import_new_ownership_witness(plan, session)?;
                let target_exists = barriers.contains_key(&path_key(&session.canonical_path));
                let witness_exists = replacement_artifact_exists(&witness, "session witness")?;
                match (target_exists, witness_exists) {
                    (false, false) => {}
                    (true, true) => {
                        let witness_identity = stable_regular_file_identity(&witness)?;
                        if stable_file_digest(&witness)?.1 != session.source_sha256 {
                            return Err(rollback_changed_after_apply("session target"));
                        }
                        preflight_live_target(
                            &mut barriers,
                            &mut expected_targets,
                            &session.canonical_path,
                            Some(&session.source_sha256),
                            Some(witness_identity.into()),
                            "session target",
                        )?;
                        verify_session_identity(
                            &session.canonical_path,
                            &session.thread_id,
                            &session.source_sha256,
                        )
                        .map_err(|_| rollback_changed_after_apply("session target"))?;
                    }
                    _ => return Err(rollback_changed_after_apply("session target")),
                }
            }
            RestoreImportSessionAction::ImportExtension => {
                let state = replacement_states
                    .get(&path_key(&session.canonical_path))
                    .copied()
                    .ok_or_else(|| {
                        "restore import extension rollback state is missing".to_string()
                    })?;
                let expected = match state {
                    HandleReplaceCrashState::ReplacementWithRecovery
                    | HandleReplaceCrashState::ReplacementOnly => {
                        Some(session.source_sha256.as_str())
                    }
                    HandleReplaceCrashState::Original
                    | HandleReplaceCrashState::Staged
                    | HandleReplaceCrashState::RolledBack => {
                        session.canonical_before_sha256.as_deref()
                    }
                    HandleReplaceCrashState::Prepared
                    | HandleReplaceCrashState::RollbackPrepared => None,
                };
                if let Some(expected) = expected {
                    verify_session_identity(&session.canonical_path, &session.thread_id, expected)
                        .map_err(|_| rollback_changed_after_apply("session target"))?;
                }
            }
            RestoreImportSessionAction::Unchanged | RestoreImportSessionAction::KeepCanonical => {
                let expected = session.canonical_before_sha256.as_deref().ok_or_else(|| {
                    "restore import retained canonical checksum is missing".to_string()
                })?;
                preflight_live_target(
                    &mut barriers,
                    &mut expected_targets,
                    &session.canonical_path,
                    Some(expected),
                    None,
                    "retained session target",
                )?;
                verify_session_identity(&session.canonical_path, &session.thread_id, expected)
                    .map_err(|_| rollback_changed_after_apply("retained session target"))?;
            }
        }
    }

    for database in &plan.databases {
        quick_check_sqlite(&database.staged_path)?;
        if stable_file_digest(&database.staged_path)?
            != (database.staged_bytes, database.staged_sha256.clone())
        {
            return Err("restore import staged database changed before rollback".to_string());
        }
        let expected_logical = sqlite_logical_content_digest(&database.staged_path)?;
        if is_created_goals_database(database) {
            let witness = goals_creation_witness(&plan.operation_id, database)?;
            let target_exists = barriers.contains_key(&path_key(&database.target_path));
            let witness_exists = replacement_artifact_exists(&witness, "goals witness")?;
            match (target_exists, witness_exists) {
                (false, false) => continue,
                (true, true) => {
                    let witness_identity = stable_regular_file_identity(&witness)?;
                    if stable_file_digest(&witness)?.1 != database.staged_sha256 {
                        return Err(rollback_changed_after_apply("database target"));
                    }
                    preflight_live_target(
                        &mut barriers,
                        &mut expected_targets,
                        &database.target_path,
                        Some(&database.staged_sha256),
                        Some(witness_identity.into()),
                        "database target",
                    )?;
                }
                _ => return Err(rollback_changed_after_apply("database target")),
            }
        } else if !replacement_states.contains_key(&path_key(&database.target_path)) {
            preflight_live_target(
                &mut barriers,
                &mut expected_targets,
                &database.target_path,
                None,
                None,
                "database target",
            )?;
        }
        if barriers.contains_key(&path_key(&database.target_path)) {
            if !sqlite_sidecars_absent(&database.target_path)? {
                return Err(rollback_changed_after_apply("database target"));
            }
            let current_logical = with_guarded_restore_database_copy(
                plan,
                database,
                "rollback-preflight",
                expected_targets
                    .get(&path_key(&database.target_path))
                    .map(|(sha256, _, _)| sha256.as_str()),
                &mut barriers,
                sqlite_logical_content_digest,
            )
            .map_err(|_| rollback_changed_after_apply("database target"))?;
            let expected = if replacement_states
                .get(&path_key(&database.target_path))
                .is_some_and(|state| {
                    matches!(
                        state,
                        HandleReplaceCrashState::Original
                            | HandleReplaceCrashState::Staged
                            | HandleReplaceCrashState::RolledBack
                    )
                }) {
                sqlite_logical_content_digest(&database.original_backup_payload)?
            } else {
                expected_logical
            };
            if current_logical != expected {
                return Err(rollback_changed_after_apply("database target"));
            }
        }
    }

    for (key, (expected_sha256, expected_identity, label)) in &expected_targets {
        let barrier = barriers
            .get_mut(key)
            .ok_or_else(|| rollback_changed_after_apply(label))?;
        barrier
            .verify_current_path(Some(expected_sha256))
            .map_err(|_| rollback_changed_after_apply(label))?;
        if barrier
            .identity()
            .map_err(|_| rollback_changed_after_apply(label))?
            != *expected_identity
        {
            return Err(rollback_changed_after_apply(label));
        }
    }
    if load_restore_import_replacement_phases(plan, replacements)? != *replacement_phases {
        return Err(
            "restore import replacement phase changed during rollback preflight".to_string(),
        );
    }
    Ok(())
}

pub fn rollback_restore_import<Guard>(
    plan: &RestoreImportPlan,
    mut before_live_write: Guard,
) -> Result<(), String>
where
    Guard: FnMut() -> Result<(), String>,
{
    validate_plan(plan)?;
    let imported = imported_sessions(plan).collect::<Vec<_>>();
    let replacements = load_restore_import_replacements(plan)?;
    let replacement_phases = load_restore_import_replacement_phases(plan, &replacements)?;
    preflight_rollback_restore_import(plan, &replacements, &replacement_phases)?;
    for replacement in replacements.iter().rev() {
        before_live_write()?;
        let phase_entry = replacement_phases
            .replacements
            .iter()
            .find(|entry| entry.replacement == *replacement)
            .cloned()
            .ok_or_else(|| "restore import replacement phase entry is missing".to_string())?;
        match replacement.kind {
            RestoreImportReplacementKind::SessionExtension => {
                let session = plan
                    .sessions
                    .iter()
                    .find(|session| {
                        session.action == RestoreImportSessionAction::ImportExtension
                            && path_key(&session.canonical_path)
                                == path_key(&replacement.target_path)
                    })
                    .ok_or_else(|| {
                        "restore import extension rollback plan is missing".to_string()
                    })?;
                let backup = session.canonical_backup_payload.as_ref().ok_or_else(|| {
                    "restore import canonical rollback backup is missing".to_string()
                })?;
                if stable_file_digest(backup)?.1 != replacement.rollback_snapshot_sha256 {
                    return Err("restore import canonical rollback backup changed".to_string());
                }
            }
            RestoreImportReplacementKind::RuntimeDatabase => {
                let database = plan
                    .databases
                    .iter()
                    .find(|database| {
                        path_key(&database.target_path) == path_key(&replacement.target_path)
                    })
                    .ok_or_else(|| {
                        "restore import database rollback plan is missing".to_string()
                    })?;
                if stable_file_digest(&database.original_backup_payload)?.1
                    != replacement.rollback_snapshot_sha256
                {
                    return Err("restore import database rollback backup changed".to_string());
                }
                if !sqlite_sidecars_absent(&replacement.target_path)? {
                    return Err(
                        "restore import database rollback encountered an active sidecar"
                            .to_string(),
                    );
                }
            }
        }
        rollback_restore_import_replacement(plan, &replacements, replacement, &phase_entry)?;
        match replacement.kind {
            RestoreImportReplacementKind::SessionExtension => {
                let session = plan
                    .sessions
                    .iter()
                    .find(|session| {
                        path_key(&session.canonical_path) == path_key(&replacement.target_path)
                    })
                    .expect("validated restore import extension replacement");
                verify_session_identity(
                    &replacement.target_path,
                    &session.thread_id,
                    &replacement.live_original_sha256,
                )?;
            }
            RestoreImportReplacementKind::RuntimeDatabase => {
                if !sqlite_sidecars_absent(&replacement.target_path)? {
                    return Err(
                        "restore import database rollback gained an active sidecar".to_string()
                    );
                }
                quick_check_sqlite(&replacement.target_path)?;
            }
        }
    }
    for database in plan
        .databases
        .iter()
        .filter(|database| is_created_goals_database(database))
        .rev()
    {
        before_live_write()?;
        rollback_created_goals_database(plan, database)?;
    }
    for session in imported.into_iter().rev() {
        before_live_write()?;
        match session.action {
            RestoreImportSessionAction::ImportNew => {
                let witness = import_new_ownership_witness(plan, session)?;
                if session.canonical_path.exists() {
                    if !witness.is_file()
                        || !same_regular_file_identity(&session.canonical_path, &witness)
                            .unwrap_or(false)
                    {
                        return Err(
                            "restore import created session ownership changed; rollback deferred"
                                .to_string(),
                        );
                    }
                    let mut destructive = DestructiveFileGuard::acquire(&session.canonical_path)?;
                    destructive.verify_current_path(Some(&session.source_sha256))?;
                    destructive.delete()?;
                    remove_import_new_ownership_witness(session, &witness)?;
                } else if witness.exists() {
                    return Err(
                        "restore import ownership witness has no provable created session"
                            .to_string(),
                    );
                }
            }
            RestoreImportSessionAction::ImportExtension => {}
            RestoreImportSessionAction::Unchanged | RestoreImportSessionAction::KeepCanonical => {}
        }
    }
    cleanup_restore_import_staging(plan)
}

fn rollback_created_goals_database(
    plan: &RestoreImportPlan,
    database: &MigrationDatabaseApplyEntry,
) -> Result<(), String> {
    let witness = goals_creation_witness(&plan.operation_id, database)?;
    let target_exists = match fs::symlink_metadata(&database.target_path) {
        Ok(metadata) if metadata.is_file() && !metadata_is_link_or_reparse(&metadata) => true,
        Ok(_) => return Err("restore import created goals target is unsafe".to_string()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(_) => return Err("restore import created goals target is unavailable".to_string()),
    };
    let witness_exists = match fs::symlink_metadata(&witness) {
        Ok(metadata) if metadata.is_file() && !metadata_is_link_or_reparse(&metadata) => true,
        Ok(_) => return Err("restore import created goals witness is unsafe".to_string()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(_) => return Err("restore import created goals witness is unavailable".to_string()),
    };
    match (target_exists, witness_exists) {
        (false, false) => return Ok(()),
        (true, true) => {}
        _ => return Err("restore import created goals ownership is incomplete".to_string()),
    }
    if stable_file_digest(&database.target_path)?.1 != database.staged_sha256
        || stable_file_digest(&witness)?.1 != database.staged_sha256
        || !same_regular_file_identity(&database.target_path, &witness).unwrap_or(false)
        || !sqlite_sidecars_absent(&database.target_path)?
    {
        return Err("restore import created goals ownership changed".to_string());
    }
    let mut target = DestructiveFileGuard::acquire(&database.target_path)?;
    target.verify_current_path(Some(&database.staged_sha256))?;
    target.delete()?;
    remove_exact_replacement_artifact(&witness, &database.staged_sha256)
}

fn rollback_restore_import_replacement(
    plan: &RestoreImportPlan,
    replacements: &[RestoreImportReplacementPlan],
    replacement: &RestoreImportReplacementPlan,
    phase_entry: &RestoreImportReplacementPhaseEntry,
) -> Result<(), String> {
    let phase = phase_entry.phase;
    if matches!(
        phase,
        RestoreImportReplacementPhase::Planned | RestoreImportReplacementPhase::Staging
    ) && phase_entry.replacement_identity.is_none()
    {
        let mut target = WriteExclusionGuard::acquire(&replacement.target_path)?;
        target.verify_current_path(Some(&replacement.live_original_sha256))?;
        if target.identity()? != phase_entry.original_identity.into()
            || parent_directory_identity_at_path(&replacement.target_path)?
                != phase_entry.parent_identity.into()
        {
            return Err(replacement_rollback_deferred(
                "planned original identity changed",
            ));
        }
        for path in [
            &replacement.replacement_witness_path,
            &replacement.staging_path,
            &replacement.recovery_path,
            &replacement.tombstone_path,
        ] {
            if replacement_artifact_exists(path, "planned rollback artifact")? {
                return Err(replacement_rollback_deferred(
                    "planned layout contains an unbound replacement artifact",
                ));
            }
        }
        drop(target);
        if replacement_artifact_exists(&replacement.original_witness_path, "original witness")? {
            remove_exact_identity_replacement_artifact(
                &replacement.original_witness_path,
                &replacement.live_original_sha256,
                phase_entry.original_identity,
            )?;
        }
        transition_restore_import_replacement_phase(
            plan,
            replacements,
            replacement,
            &[
                RestoreImportReplacementPhase::Planned,
                RestoreImportReplacementPhase::Staging,
            ],
            RestoreImportReplacementPhase::Cleaned,
        )?;
        return Ok(());
    }
    if phase == RestoreImportReplacementPhase::Cleaned {
        let mut target = WriteExclusionGuard::acquire(&replacement.target_path)?;
        target.verify_current_path(Some(&replacement.live_original_sha256))?;
        if target.identity()? != phase_entry.original_identity.into() {
            return Err(replacement_rollback_deferred(
                "cleaned rollback target identity changed",
            ));
        }
        for path in [
            &replacement.original_witness_path,
            &replacement.replacement_witness_path,
            &replacement.staging_path,
            &replacement.recovery_path,
            &replacement.tombstone_path,
        ] {
            if replacement_artifact_exists(path, "cleaned rollback artifact")? {
                return Err(replacement_rollback_deferred(
                    "cleaned rollback artifact reappeared",
                ));
            }
        }
        return Ok(());
    }
    let replacement_identity = phase_entry.replacement_identity.ok_or_else(|| {
        replacement_rollback_deferred("replacement has no durable identity binding")
    })?;
    let identities = HandleReplaceIdentityBindings {
        parent_identity: phase_entry.parent_identity.into(),
        original_identity: phase_entry.original_identity.into(),
        replacement_identity: replacement_identity.into(),
    };
    let paths = HandleReplacePaths::from_persisted_plan(
        replacement.target_path.clone(),
        replacement.recovery_path.clone(),
        replacement.staging_path.clone(),
        replacement.tombstone_path.clone(),
    )
    .map_err(|_| replacement_rollback_deferred("replacement paths are invalid"))?;
    let crash_state = classify_handle_replace_crash_state(
        &paths,
        identities,
        &replacement.live_original_sha256,
        &replacement.replacement_sha256,
    )
    .map_err(|_| replacement_rollback_deferred("physical replacement layout is unknown"))?;
    if !restore_import_phase_accepts_crash_state(phase, crash_state) {
        return Err(replacement_rollback_deferred(
            "logical phase does not match the physical replacement layout",
        ));
    }
    transition_restore_import_replacement_phase(
        plan,
        replacements,
        replacement,
        &[
            RestoreImportReplacementPhase::Staging,
            RestoreImportReplacementPhase::Staged,
            RestoreImportReplacementPhase::Preparing,
            RestoreImportReplacementPhase::Prepared,
            RestoreImportReplacementPhase::Publishing,
            RestoreImportReplacementPhase::Published,
            RestoreImportReplacementPhase::Committing,
            RestoreImportReplacementPhase::CommittedWithRecovery,
            RestoreImportReplacementPhase::RollbackPreparing,
            RestoreImportReplacementPhase::RollbackPrepared,
            RestoreImportReplacementPhase::RolledBack,
        ],
        RestoreImportReplacementPhase::RollbackPreparing,
    )?;
    let mut resolved = recover_handle_replace(
        &paths,
        identities,
        &replacement.live_original_sha256,
        &replacement.replacement_sha256,
        HandleReplaceRecoveryDecision::Restore,
    )
    .map_err(|_| replacement_rollback_deferred("exact replacement restore was contested"))?;
    resolved
        .guard_mut()
        .verify_current_path(Some(&replacement.live_original_sha256))
        .map_err(|_| replacement_rollback_deferred("restored target verification failed"))?;
    transition_restore_import_replacement_phase(
        plan,
        replacements,
        replacement,
        &[RestoreImportReplacementPhase::RollbackPreparing],
        RestoreImportReplacementPhase::RolledBack,
    )?;
    let mut guard = resolved
        .cleanup_after_durable_terminal()
        .map_err(|(error, _resolved)| {
            replacement_rollback_deferred(&format!("replacement tombstone cleanup failed: {error}"))
        })?;
    guard
        .verify_current_path(Some(&replacement.live_original_sha256))
        .map_err(|_| replacement_rollback_deferred("restored target verification failed"))?;
    drop(guard);
    cleanup_rolled_back_replacement_artifacts(replacement, phase_entry)?;
    transition_restore_import_replacement_phase(
        plan,
        replacements,
        replacement,
        &[RestoreImportReplacementPhase::RolledBack],
        RestoreImportReplacementPhase::Cleaned,
    )?;
    Ok(())
}

fn restore_import_phase_accepts_crash_state(
    phase: RestoreImportReplacementPhase,
    state: HandleReplaceCrashState,
) -> bool {
    match state {
        HandleReplaceCrashState::Original => matches!(
            phase,
            RestoreImportReplacementPhase::Staging
                | RestoreImportReplacementPhase::Staged
                | RestoreImportReplacementPhase::Preparing
                | RestoreImportReplacementPhase::RollbackPreparing
                | RestoreImportReplacementPhase::RolledBack
        ),
        HandleReplaceCrashState::Staged => matches!(
            phase,
            RestoreImportReplacementPhase::Staging
                | RestoreImportReplacementPhase::Staged
                | RestoreImportReplacementPhase::Preparing
                | RestoreImportReplacementPhase::RollbackPreparing
        ),
        HandleReplaceCrashState::Prepared => matches!(
            phase,
            RestoreImportReplacementPhase::Preparing
                | RestoreImportReplacementPhase::Prepared
                | RestoreImportReplacementPhase::Publishing
                | RestoreImportReplacementPhase::RollbackPreparing
        ),
        HandleReplaceCrashState::ReplacementWithRecovery => matches!(
            phase,
            RestoreImportReplacementPhase::Publishing
                | RestoreImportReplacementPhase::Published
                | RestoreImportReplacementPhase::Committing
                | RestoreImportReplacementPhase::CommittedWithRecovery
                | RestoreImportReplacementPhase::RollbackPreparing
        ),
        HandleReplaceCrashState::RollbackPrepared => matches!(
            phase,
            RestoreImportReplacementPhase::RollbackPreparing
                | RestoreImportReplacementPhase::RollbackPrepared
        ),
        HandleReplaceCrashState::RolledBack => matches!(
            phase,
            RestoreImportReplacementPhase::RollbackPreparing
                | RestoreImportReplacementPhase::RollbackPrepared
                | RestoreImportReplacementPhase::RolledBack
        ),
        // The committed cleanup path is forward-only. Rollback may not invent
        // a deleted exact recovery object after its durable terminal cleanup.
        HandleReplaceCrashState::ReplacementOnly => false,
    }
}

fn replacement_rollback_deferred(reason: &str) -> String {
    format!("restore import replacement rollback deferred: {reason}")
}

fn cleanup_rolled_back_replacement_artifacts(
    replacement: &RestoreImportReplacementPlan,
    phase_entry: &RestoreImportReplacementPhaseEntry,
) -> Result<(), String> {
    if stable_file_digest(&replacement.target_path)
        .map_err(|_| replacement_rollback_deferred("restored target is unreadable"))?
        .1
        != replacement.live_original_sha256
    {
        return Err(replacement_rollback_deferred(
            "restored target verification failed",
        ));
    }
    let original_witness_exists =
        replacement_artifact_exists(&replacement.original_witness_path, "original witness")
            .map_err(|_| replacement_rollback_deferred("original witness state is unsafe"))?;
    if stable_regular_file_identity(&replacement.target_path)
        .map(|identity| identity != phase_entry.original_identity)
        .unwrap_or(true)
        || (original_witness_exists
            && (stable_file_digest(&replacement.original_witness_path)
                .map(|(_, sha256)| sha256 != replacement.live_original_sha256)
                .unwrap_or(true)
                || stable_regular_file_identity(&replacement.original_witness_path)
                    .map(|identity| identity != phase_entry.original_identity)
                    .unwrap_or(true)
                || !same_regular_file_identity(
                    &replacement.target_path,
                    &replacement.original_witness_path,
                )
                .unwrap_or(false)))
    {
        return Err(replacement_rollback_deferred(
            "restored original identity changed",
        ));
    }
    if replacement_artifact_exists(&replacement.recovery_path, "recovery")
        .map_err(|_| replacement_rollback_deferred("recovery state is unsafe"))?
    {
        return Err(replacement_rollback_deferred(
            "recovery path remained after restore",
        ));
    }
    let staging_exists = replacement_artifact_exists(&replacement.staging_path, "staging")
        .map_err(|_| replacement_rollback_deferred("staging state is unsafe"))?;
    let tombstone_exists = replacement_artifact_exists(&replacement.tombstone_path, "tombstone")
        .map_err(|_| replacement_rollback_deferred("tombstone state is unsafe"))?;
    let witness_exists =
        replacement_artifact_exists(&replacement.replacement_witness_path, "witness")
            .map_err(|_| replacement_rollback_deferred("witness state is unsafe"))?;
    if staging_exists && tombstone_exists {
        return Err(replacement_rollback_deferred(
            "staging and tombstone both exist",
        ));
    }
    if (staging_exists || tombstone_exists) && phase_entry.replacement_identity.is_none() {
        return Err(replacement_rollback_deferred(
            "replacement artifact has no durable identity",
        ));
    }
    for (exists, path, label) in [
        (staging_exists, &replacement.staging_path, "staging"),
        (tombstone_exists, &replacement.tombstone_path, "tombstone"),
    ] {
        if !exists {
            continue;
        }
        if stable_file_digest(path)
            .map(|(_, sha256)| sha256 != replacement.replacement_sha256)
            .unwrap_or(true)
            || stable_regular_file_identity(path).ok() != phase_entry.replacement_identity
        {
            return Err(replacement_rollback_deferred(&format!(
                "{label} ownership changed"
            )));
        }
        remove_exact_identity_replacement_artifact(
            path,
            &replacement.replacement_sha256,
            phase_entry
                .replacement_identity
                .expect("replacement artifact durable identity"),
        )
        .map_err(|_| replacement_rollback_deferred(&format!("{label} cleanup failed")))?;
    }
    if witness_exists {
        let replacement_identity = phase_entry.replacement_identity.ok_or_else(|| {
            replacement_rollback_deferred("replacement witness has no durable identity")
        })?;
        remove_exact_identity_replacement_artifact(
            &replacement.replacement_witness_path,
            &replacement.replacement_sha256,
            replacement_identity,
        )
        .map_err(|_| replacement_rollback_deferred("witness cleanup failed"))?;
    }
    if original_witness_exists {
        remove_exact_identity_replacement_artifact(
            &replacement.original_witness_path,
            &replacement.live_original_sha256,
            phase_entry.original_identity,
        )
        .map_err(|_| replacement_rollback_deferred("original witness cleanup failed"))?;
    }
    Ok(())
}

fn replacement_artifact_exists(path: &Path, label: &str) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Ok(metadata) if metadata.is_file() && !metadata_is_link_or_reparse(&metadata) => Ok(true),
        Ok(_) => Err(format!("restore import replacement {label} is unsafe")),
        Err(_) => Err(format!("restore import replacement {label} is unavailable")),
    }
}

fn remove_exact_replacement_artifact(path: &Path, expected_sha256: &str) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Ok(metadata) if metadata.is_file() && !metadata_is_link_or_reparse(&metadata) => {}
        Ok(_) => return Err("restore import replacement cleanup target is unsafe".to_string()),
        Err(_) => {
            return Err("restore import replacement cleanup target is unavailable".to_string())
        }
    }
    let mut destructive = DestructiveFileGuard::acquire(path)?;
    destructive.verify_current_path(Some(expected_sha256))?;
    destructive.delete()
}

fn remove_exact_identity_replacement_artifact(
    path: &Path,
    expected_sha256: &str,
    expected_identity: RestoreImportFileIdentity,
) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Ok(metadata) if metadata.is_file() && !metadata_is_link_or_reparse(&metadata) => {}
        Ok(_) => return Err("restore import replacement cleanup target is unsafe".to_string()),
        Err(_) => {
            return Err("restore import replacement cleanup target is unavailable".to_string())
        }
    }
    let mut destructive = DestructiveFileGuard::acquire(path)?;
    destructive.verify_current_path(Some(expected_sha256))?;
    if RestoreImportFileIdentity::from(destructive.identity()?) != expected_identity {
        return Err("restore import replacement cleanup identity changed".to_string());
    }
    destructive.delete()
}

pub fn cleanup_committed_restore_import_ownership_witnesses(
    plan: &RestoreImportPlan,
) -> Result<(), String> {
    validate_plan(plan)?;
    let replacements = load_restore_import_replacements(plan)?;
    let replacement_phases = load_restore_import_replacement_phases(plan, &replacements)?;
    for replacement in &replacements {
        let phase_entry = replacement_phases
            .replacements
            .iter()
            .find(|entry| entry.replacement == *replacement)
            .cloned()
            .ok_or_else(|| "restore import replacement phase entry is missing".to_string())?;
        let phase = phase_entry.phase;
        if !matches!(
            phase,
            RestoreImportReplacementPhase::CommittedWithRecovery
                | RestoreImportReplacementPhase::Cleaned
        ) {
            return Err(
                "restore import committed cleanup phase does not match the durable plan"
                    .to_string(),
            );
        }
        let replacement_identity = phase_entry.replacement_identity.ok_or_else(|| {
            "restore import committed replacement identity is missing".to_string()
        })?;
        let identities = HandleReplaceIdentityBindings {
            parent_identity: phase_entry.parent_identity.into(),
            original_identity: phase_entry.original_identity.into(),
            replacement_identity: replacement_identity.into(),
        };
        let paths = HandleReplacePaths::from_persisted_plan(
            replacement.target_path.clone(),
            replacement.recovery_path.clone(),
            replacement.staging_path.clone(),
            replacement.tombstone_path.clone(),
        )?;
        let crash_state = classify_handle_replace_crash_state(
            &paths,
            identities,
            &replacement.live_original_sha256,
            &replacement.replacement_sha256,
        )?;
        if !matches!(
            (phase, crash_state),
            (
                RestoreImportReplacementPhase::CommittedWithRecovery,
                HandleReplaceCrashState::ReplacementWithRecovery
                    | HandleReplaceCrashState::ReplacementOnly
            ) | (
                RestoreImportReplacementPhase::Cleaned,
                HandleReplaceCrashState::ReplacementOnly
            )
        ) {
            return Err(
                "restore import committed cleanup phase does not match the physical layout"
                    .to_string(),
            );
        }
        if phase == RestoreImportReplacementPhase::Cleaned {
            let mut resolved = recover_handle_replace(
                &paths,
                identities,
                &replacement.live_original_sha256,
                &replacement.replacement_sha256,
                HandleReplaceRecoveryDecision::Commit,
            )?;
            resolved
                .guard_mut()
                .verify_current_path(Some(&replacement.replacement_sha256))?;
            let mut target = resolved.retain_for_recovery();
            target.verify_current_path(Some(&replacement.replacement_sha256))?;
            for path in [
                &replacement.original_witness_path,
                &replacement.replacement_witness_path,
                &replacement.staging_path,
                &replacement.recovery_path,
                &replacement.tombstone_path,
            ] {
                if replacement_artifact_exists(path, "cleaned committed artifact")? {
                    return Err(
                        "restore import cleaned replacement artifact reappeared".to_string()
                    );
                }
            }
            continue;
        }

        let witness_exists = replacement_artifact_exists(
            &replacement.replacement_witness_path,
            "committed witness",
        )?;
        if witness_exists
            && (stable_file_digest(&replacement.replacement_witness_path)?.1
                != replacement.replacement_sha256
                || stable_regular_file_identity(&replacement.replacement_witness_path)?
                    != replacement_identity
                || !same_regular_file_identity(
                    &replacement.target_path,
                    &replacement.replacement_witness_path,
                )
                .unwrap_or(false))
        {
            return Err(
                "restore import committed replacement ownership changed before cleanup".to_string(),
            );
        }

        let original_witness_exists = replacement_artifact_exists(
            &replacement.original_witness_path,
            "committed original witness",
        )?;
        if original_witness_exists
            && (stable_file_digest(&replacement.original_witness_path)?.1
                != replacement.live_original_sha256
                || stable_regular_file_identity(&replacement.original_witness_path)?
                    != phase_entry.original_identity)
        {
            return Err("restore import committed original identity witness changed".to_string());
        }

        let staging_exists =
            replacement_artifact_exists(&replacement.staging_path, "committed staging")?;
        let tombstone_exists =
            replacement_artifact_exists(&replacement.tombstone_path, "committed tombstone")?;
        if staging_exists && tombstone_exists {
            return Err(
                "restore import committed replacement has conflicting terminal artifacts"
                    .to_string(),
            );
        }
        for (exists, path, label) in [
            (staging_exists, &replacement.staging_path, "staging"),
            (tombstone_exists, &replacement.tombstone_path, "tombstone"),
        ] {
            if exists
                && (stable_file_digest(path)?.1 != replacement.replacement_sha256
                    || stable_regular_file_identity(path)? != replacement_identity
                    || (witness_exists
                        && !same_regular_file_identity(
                            path,
                            &replacement.replacement_witness_path,
                        )
                        .unwrap_or(false)))
            {
                return Err(format!(
                    "restore import committed replacement {label} ownership changed"
                ));
            }
        }

        let recovery_exists =
            replacement_artifact_exists(&replacement.recovery_path, "committed recovery")?;
        if crash_state == HandleReplaceCrashState::ReplacementWithRecovery {
            if !recovery_exists
                || stable_file_digest(&replacement.recovery_path)?.1
                    != replacement.live_original_sha256
                || stable_regular_file_identity(&replacement.recovery_path)?
                    != phase_entry.original_identity
                || !same_regular_file_identity(
                    &replacement.recovery_path,
                    &replacement.original_witness_path,
                )
                .unwrap_or(false)
            {
                return Err(
                    "restore import committed replacement recovery ownership changed".to_string(),
                );
            }
        } else if recovery_exists {
            return Err("restore import cleaned recovery artifact reappeared".to_string());
        }
        let mut resolved = recover_handle_replace(
            &paths,
            identities,
            &replacement.live_original_sha256,
            &replacement.replacement_sha256,
            HandleReplaceRecoveryDecision::Commit,
        )?;
        resolved
            .guard_mut()
            .verify_current_path(Some(&replacement.replacement_sha256))?;
        let mut target = resolved
            .cleanup_after_durable_terminal()
            .map_err(|(error, _resolved)| error)?;
        target.verify_current_path(Some(&replacement.replacement_sha256))?;
        drop(target);

        // `recover_handle_replace(..., Commit)` deletes the exact recovery
        // object. The two witnesses are caller-owned links and are cleaned only
        // after that typed terminal succeeds.
        if original_witness_exists {
            remove_exact_identity_replacement_artifact(
                &replacement.original_witness_path,
                &replacement.live_original_sha256,
                phase_entry.original_identity,
            )?;
        }
        if witness_exists {
            remove_exact_identity_replacement_artifact(
                &replacement.replacement_witness_path,
                &replacement.replacement_sha256,
                replacement_identity,
            )?;
        }
        transition_restore_import_replacement_phase(
            plan,
            &replacements,
            replacement,
            &[RestoreImportReplacementPhase::CommittedWithRecovery],
            RestoreImportReplacementPhase::Cleaned,
        )?;
    }
    for database in plan
        .databases
        .iter()
        .filter(|database| is_created_goals_database(database))
    {
        let witness = goals_creation_witness(&plan.operation_id, database)?;
        if !witness.exists()
            || !database.target_path.is_file()
            || stable_file_digest(&database.target_path)?.1 != database.staged_sha256
            || stable_file_digest(&witness)?.1 != database.staged_sha256
            || !same_regular_file_identity(&database.target_path, &witness).unwrap_or(false)
        {
            return Err(
                "restore import created goals witness no longer identifies the committed database"
                    .to_string(),
            );
        }
        remove_exact_replacement_artifact(&witness, &database.staged_sha256)?;
    }
    for session in imported_sessions(plan)
        .filter(|session| session.action == RestoreImportSessionAction::ImportNew)
    {
        let witness = import_new_ownership_witness(plan, session)?;
        if !witness.exists() {
            continue;
        }
        if !session.canonical_path.is_file()
            || stable_file_digest(&session.canonical_path)?.1 != session.source_sha256
            || stable_file_digest(&witness)?.1 != session.source_sha256
            || !same_regular_file_identity(&session.canonical_path, &witness).unwrap_or(false)
        {
            return Err(
                "restore import ownership witness no longer identifies the committed session"
                    .to_string(),
            );
        }
        remove_import_new_ownership_witness(session, &witness)?;
    }
    Ok(())
}

fn import_new_ownership_witness(
    plan: &RestoreImportPlan,
    session: &RestoreImportSessionPlan,
) -> Result<PathBuf, String> {
    ownership_witness_path(&session.canonical_path, &plan.operation_id)
}

fn remove_import_new_ownership_witness(
    session: &RestoreImportSessionPlan,
    witness: &Path,
) -> Result<(), String> {
    if !witness.exists() {
        return Ok(());
    }
    let (_, sha256) = stable_file_digest(witness)?;
    if sha256 != session.source_sha256 {
        return Err("restore import ownership witness changed".to_string());
    }
    let mut destructive = DestructiveFileGuard::acquire(witness)?;
    destructive.verify_current_path(Some(&session.source_sha256))?;
    destructive.delete()
}

pub fn recover_interrupted_restore_import<Guard>(
    store: &OperationLedgerStore,
    data_root: &Path,
    operation_id: &str,
    mut before_live_write: Guard,
) -> Result<RestoreImportRecoveryStatus, String>
where
    Guard: FnMut() -> Result<(), String>,
{
    let ledger = store.load(operation_id)?;
    if ledger.kind != SessionStorageOperationKind::RestoreImport
        || ledger.canonical_root.as_os_str().is_empty()
    {
        return Err("restore import recovery ledger identity is invalid".to_string());
    }
    if ledger.phase == SessionStorageOperationPhase::Committed {
        return Err("committed restore import does not require recovery".to_string());
    }
    if ledger.phase == SessionStorageOperationPhase::RolledBack {
        return Ok(RestoreImportRecoveryStatus::RolledBack);
    }
    if ledger.phase == SessionStorageOperationPhase::Failed {
        return Ok(RestoreImportRecoveryStatus::Failed);
    }
    let restore_plan_path = plan_path(data_root, operation_id)?;
    let plan = match load_restore_import_plan(data_root, operation_id) {
        Ok(plan) => Some(plan),
        Err(_)
            if matches!(
                ledger.phase,
                SessionStorageOperationPhase::Available
                    | SessionStorageOperationPhase::Preflight
                    | SessionStorageOperationPhase::Backup
            ) && !restore_plan_path.exists() =>
        {
            None
        }
        Err(error) => return Err(error),
    };
    if !matches!(
        ledger.phase,
        SessionStorageOperationPhase::Applying
            | SessionStorageOperationPhase::Validating
            | SessionStorageOperationPhase::RollingBack
    ) {
        let cleanup = if let Some(plan) = plan.as_ref() {
            cleanup_unapplied_restore_import(plan)
        } else {
            cleanup_unplanned_restore_import(data_root, operation_id)
        };
        if cleanup.is_err() {
            return Ok(RestoreImportRecoveryStatus::Failed);
        }
        if ledger.phase == SessionStorageOperationPhase::Available {
            store.transition(operation_id, SessionStorageOperationPhase::Preflight)?;
        }
        store.transition(operation_id, SessionStorageOperationPhase::RollingBack)?;
        store.transition(operation_id, SessionStorageOperationPhase::RolledBack)?;
        return Ok(RestoreImportRecoveryStatus::RolledBack);
    }
    if !ledger.live_mutation_started {
        let cleanup = if let Some(plan) = plan.as_ref() {
            cleanup_unapplied_restore_import(plan)
        } else {
            cleanup_unplanned_restore_import(data_root, operation_id)
        };
        if cleanup.is_err() {
            return Ok(RestoreImportRecoveryStatus::Failed);
        }
        if ledger.phase != SessionStorageOperationPhase::RollingBack {
            store.transition(operation_id, SessionStorageOperationPhase::RollingBack)?;
        }
        store.transition(operation_id, SessionStorageOperationPhase::RolledBack)?;
        return Ok(RestoreImportRecoveryStatus::RolledBack);
    }
    let Some(plan) = plan else {
        return Ok(RestoreImportRecoveryStatus::Failed);
    };
    if before_live_write().is_err() {
        return Ok(RestoreImportRecoveryStatus::DeferredByLiveWriter);
    }
    if ledger.phase != SessionStorageOperationPhase::RollingBack {
        store.transition(operation_id, SessionStorageOperationPhase::RollingBack)?;
    }
    match rollback_restore_import(&plan, before_live_write) {
        Ok(()) => {
            store.transition(operation_id, SessionStorageOperationPhase::RolledBack)?;
            Ok(RestoreImportRecoveryStatus::RolledBack)
        }
        Err(error) if restore_import_rollback_is_deferred(&error) => {
            Ok(RestoreImportRecoveryStatus::DeferredByLiveWriter)
        }
        Err(_) => Ok(RestoreImportRecoveryStatus::Failed),
    }
}

fn restore_import_rollback_is_deferred(error: &str) -> bool {
    error.starts_with("restore import replacement rollback deferred:")
        || error.ends_with("rollback deferred")
        || error == "restore import ownership witness has no provable created session"
}

fn cleanup_unapplied_restore_import(plan: &RestoreImportPlan) -> Result<(), String> {
    let replacements = load_restore_import_replacements(plan)?;
    if !replacements.is_empty()
        && load_restore_import_replacement_phases(plan, &replacements)?
            .replacements
            .iter()
            .any(|entry| entry.phase != RestoreImportReplacementPhase::Planned)
    {
        return Err(
            "restore import unapplied ledger disagrees with the replacement phase".to_string(),
        );
    }
    cleanup_unapplied_restore_import_replacement_artifacts(&replacements)?;
    cleanup_restore_import_staging(plan)?;
    if plan.recovery_root.exists() {
        remove_owned_work_tree(
            &plan.recovery_root,
            &plan
                .data_root
                .join("session-storage-v1/restore-import-recovery"),
            &plan.operation_id,
        )?;
    }
    Ok(())
}

fn cleanup_unapplied_restore_import_replacement_artifacts(
    replacements: &[RestoreImportReplacementPlan],
) -> Result<(), String> {
    for replacement in replacements.iter().rev() {
        for path in [
            &replacement.replacement_witness_path,
            &replacement.staging_path,
            &replacement.recovery_path,
            &replacement.tombstone_path,
        ] {
            if replacement_artifact_exists(path, "unapplied artifact")? {
                return Err(
                    "restore import unapplied cleanup found a live replacement artifact"
                        .to_string(),
                );
            }
        }
        if replacement_artifact_exists(&replacement.original_witness_path, "original witness")? {
            if stable_file_digest(&replacement.target_path)?.1 != replacement.live_original_sha256
                || stable_file_digest(&replacement.original_witness_path)?.1
                    != replacement.live_original_sha256
                || !same_regular_file_identity(
                    &replacement.target_path,
                    &replacement.original_witness_path,
                )
                .unwrap_or(false)
            {
                return Err(
                    "restore import unapplied original identity witness changed".to_string()
                );
            }
            remove_exact_replacement_artifact(
                &replacement.original_witness_path,
                &replacement.live_original_sha256,
            )?;
        }
    }
    Ok(())
}

fn cleanup_unplanned_restore_import(data_root: &Path, operation_id: &str) -> Result<(), String> {
    let work_root = operation_root(data_root, operation_id)?;
    let staging_root = work_root.join("restore-import-staging");
    if staging_root.exists() {
        remove_owned_work_tree(&staging_root, &work_root, operation_id)?;
    }
    let recovery_parent = data_root.join("session-storage-v1/restore-import-recovery");
    let recovery_root = recovery_parent.join(operation_id);
    if recovery_root.exists() {
        remove_owned_work_tree(&recovery_root, &recovery_parent, operation_id)?;
    }
    Ok(())
}

fn validate_restore_import_preconditions(
    plan: &RestoreImportPlan,
    barriers: &mut BTreeMap<String, WriteExclusionGuard>,
) -> Result<(), String> {
    let replacements = load_restore_import_replacements(plan)?;
    let replacements_by_target = replacements
        .iter()
        .map(|replacement| (path_key(&replacement.target_path), replacement))
        .collect::<BTreeMap<_, _>>();
    match plan.source_kind {
        RestoreImportSourceKind::DowngradePackage => {
            let manifest = load_downgrade_manifest_baseline(&plan.package_dir)?;
            if manifest.operation_id != plan.package_operation_id
                || manifest.target.version != plan.target_version
            {
                return Err("restore import downgrade package identity changed".to_string());
            }
        }
        RestoreImportSourceKind::PendingRecovery => {
            if plan.target_version != "legacy-backup-v1" {
                return Err("pending recovery import identity changed".to_string());
            }
        }
    }
    validate_source_databases_unchanged(plan)?;
    for session in &plan.sessions {
        let (_, source_sha256) = stable_file_digest(&session.source_path)?;
        if source_sha256 != session.source_sha256 {
            return Err("restore import source session changed after planning".to_string());
        }
        if let Some(staged) = session.source_staged_path.as_ref() {
            verify_session_identity(staged, &session.thread_id, &session.source_sha256)?;
        }
        if session.action == RestoreImportSessionAction::ImportNew
            && import_new_ownership_witness(plan, session)?.exists()
        {
            return Err("restore import ownership witness appeared before apply".to_string());
        }
        match session.action {
            RestoreImportSessionAction::ImportNew if session.canonical_path.exists() => {
                return Err("restore import canonical target appeared after planning".to_string());
            }
            RestoreImportSessionAction::ImportExtension => {
                let expected = session.canonical_before_sha256.as_deref().ok_or_else(|| {
                    "restore import canonical precondition is missing".to_string()
                })?;
                if barriers
                    .get_mut(&path_key(&session.canonical_path))
                    .ok_or_else(|| {
                        "restore import canonical writer barrier is missing".to_string()
                    })?
                    .verify_current_path(Some(expected))?
                    .1
                    != expected
                {
                    return Err(
                        "restore import canonical session changed after planning".to_string()
                    );
                }
            }
            RestoreImportSessionAction::Unchanged | RestoreImportSessionAction::KeepCanonical => {
                let expected = session.canonical_before_sha256.as_deref().ok_or_else(|| {
                    "restore import retained canonical checksum is missing".to_string()
                })?;
                barriers
                    .get_mut(&path_key(&session.canonical_path))
                    .ok_or_else(|| {
                        "restore import retained canonical writer barrier is missing".to_string()
                    })?
                    .verify_current_path(Some(expected))?;
            }
            RestoreImportSessionAction::ImportNew => {}
        }
    }
    for conflict in &plan.conflicts {
        for ((source, recovery), expected_sha256) in conflict
            .candidate_paths
            .iter()
            .zip(&conflict.recovery_paths)
            .zip(&conflict.candidate_sha256)
        {
            if stable_file_digest(source)?.1 != *expected_sha256 {
                return Err("restore import conflict source changed after planning".to_string());
            }
            if stable_file_digest(recovery)?.1 != *expected_sha256 {
                return Err("restore import conflict recovery payload changed".to_string());
            }
        }
    }
    validate_unclassified_payloads(plan)?;
    for database in &plan.databases {
        quick_check_sqlite(&database.staged_path)
            .map_err(|error| format!("restore import staged database is invalid: {error}"))?;
        if stable_file_digest(&database.staged_path)?.1 != database.staged_sha256 {
            return Err("restore import staged database changed".to_string());
        }
        if is_created_goals_database(database) {
            let witness = goals_creation_witness(&plan.operation_id, database)?;
            if database.target_path.exists() || witness.exists() {
                return Err(
                    "restore import created goals target appeared after planning".to_string(),
                );
            }
        } else if let Some(replacement) =
            replacements_by_target.get(&path_key(&database.target_path))
        {
            let live_raw_sha256 = barriers
                .get_mut(&path_key(&database.target_path))
                .ok_or_else(|| "restore import database writer barrier is missing".to_string())?
                .verify_current_path(Some(&replacement.live_original_sha256))?
                .1;
            if live_raw_sha256 != replacement.live_original_sha256
                || replacement.rollback_snapshot_sha256 != database.original_sha256
                || replacement.replacement_sha256 != database.staged_sha256
            {
                return Err("restore import database digest binding changed".to_string());
            }
            let recheck_sha256 = guarded_restore_database_logical_digest(
                plan,
                database,
                "precondition",
                Some(&replacement.live_original_sha256),
                barriers,
            )
            .map_err(|error| format!("restore import live database recheck failed: {error}"))?;
            if recheck_sha256 != replacement.rollback_snapshot_sha256 {
                return Err("runtime database changed after restore import planning".to_string());
            }
        } else {
            if database.original_sha256 != database.staged_sha256 {
                return Err("restore import database replacement plan is missing".to_string());
            }
            let recheck_sha256 = guarded_restore_database_logical_digest(
                plan,
                database,
                "precondition-retained",
                None,
                barriers,
            )
            .map_err(|error| format!("restore import live database recheck failed: {error}"))?;
            if recheck_sha256 != database.staged_sha256 {
                return Err("runtime database changed after restore import planning".to_string());
            }
        }
    }
    Ok(())
}

fn validate_unclassified_payloads(plan: &RestoreImportPlan) -> Result<(), String> {
    for payload in &plan.unclassified_payloads {
        if stable_file_digest(&payload.source_path)?
            != (payload.source_bytes, payload.source_sha256.clone())
        {
            return Err("restore import unclassified source changed after planning".to_string());
        }
        if stable_file_digest(&payload.recovery_path)?
            != (payload.source_bytes, payload.source_sha256.clone())
        {
            return Err("restore import unclassified recovery payload changed".to_string());
        }
    }
    Ok(())
}

fn acquire_restore_write_barriers(
    plan: &RestoreImportPlan,
) -> Result<BTreeMap<String, WriteExclusionGuard>, String> {
    let mut paths = BTreeMap::<String, PathBuf>::new();
    for session in &plan.sessions {
        if session.canonical_path.exists() {
            paths.insert(
                path_key(&session.canonical_path),
                session.canonical_path.clone(),
            );
        }
    }
    for database in &plan.databases {
        match fs::symlink_metadata(&database.target_path) {
            Ok(metadata) if metadata.is_file() && !metadata_is_link_or_reparse(&metadata) => {
                paths.insert(
                    path_key(&database.target_path),
                    database.target_path.clone(),
                );
            }
            Ok(_) => return Err("restore import database barrier target is unsafe".to_string()),
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    && is_created_goals_database(database) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err("restore import database barrier target is missing".to_string())
            }
            Err(_) => {
                return Err("restore import database barrier target is unavailable".to_string())
            }
        }
    }
    let mut barriers = BTreeMap::new();
    for (key, path) in paths {
        barriers.insert(key, WriteExclusionGuard::acquire(&path)?);
    }
    Ok(barriers)
}

fn guarded_restore_database_digest(
    plan: &RestoreImportPlan,
    database: &MigrationDatabaseApplyEntry,
    replacement: Option<&RestoreImportReplacementPlan>,
    barriers: &mut BTreeMap<String, WriteExclusionGuard>,
) -> Result<String, String> {
    let expected_raw_sha256 =
        replacement.map(|replacement| replacement.live_original_sha256.as_str());
    let raw_sha256 = barriers
        .get_mut(&path_key(&database.target_path))
        .ok_or_else(|| "restore import database writer barrier is missing".to_string())?
        .verify_current_path(expected_raw_sha256)?
        .1;
    if let Some(replacement) = replacement {
        if raw_sha256 != replacement.live_original_sha256
            || replacement.rollback_snapshot_sha256 != database.original_sha256
            || replacement.replacement_sha256 != database.staged_sha256
        {
            return Err("restore import database digest binding changed".to_string());
        }
    } else if database.original_sha256 != database.staged_sha256 {
        return Err("restore import database replacement plan is missing".to_string());
    }
    let logical_sha256 = guarded_restore_database_logical_digest(
        plan,
        database,
        "apply",
        expected_raw_sha256,
        barriers,
    )?;
    match replacement {
        Some(replacement) if logical_sha256 != replacement.rollback_snapshot_sha256 => {
            return Err("restore import database logical snapshot changed".to_string())
        }
        None if logical_sha256 != database.staged_sha256 => {
            return Err("retained restore import database changed".to_string())
        }
        _ => {}
    }
    Ok(logical_sha256)
}

fn with_guarded_restore_database_copy<T>(
    plan: &RestoreImportPlan,
    database: &MigrationDatabaseApplyEntry,
    label: &str,
    expected_raw_sha256: Option<&str>,
    barriers: &mut BTreeMap<String, WriteExclusionGuard>,
    inspect: impl FnOnce(&Path) -> Result<T, String>,
) -> Result<T, String> {
    if !sqlite_sidecars_absent(&database.target_path)? {
        return Err("restore import database has an active sidecar".to_string());
    }
    let copy = plan.staging_root.join(format!(
        ".guarded-{label}-{}.sqlite",
        safe_path_component(&database.database_id)
    ));
    let barrier = barriers
        .get_mut(&path_key(&database.target_path))
        .ok_or_else(|| "restore import database writer barrier is missing".to_string())?;
    let source_digest = barrier.copy_current_to_new_file(&copy, expected_raw_sha256)?;
    let inspected = inspect(&copy);
    let copy_unchanged = stable_file_digest(&copy).and_then(|digest| {
        if digest == source_digest {
            Ok(())
        } else {
            Err("restore import guarded database copy changed".to_string())
        }
    });
    let cleanup = remove_exact_replacement_artifact(&copy, &source_digest.1);
    let live_unchanged = barrier
        .verify_current_path(Some(&source_digest.1))
        .and_then(|digest| {
            if digest == source_digest && sqlite_sidecars_absent(&database.target_path)? {
                Ok(())
            } else {
                Err("restore import database changed during guarded validation".to_string())
            }
        });
    match (inspected, copy_unchanged, cleanup, live_unchanged) {
        (Ok(value), Ok(()), Ok(()), Ok(())) => Ok(value),
        (Err(error), _, Ok(()), Ok(())) => Err(error),
        (_, Err(error), Ok(()), Ok(())) => Err(error),
        (_, _, Err(error), Ok(())) => Err(error),
        (_, _, _, Err(error)) => Err(error),
    }
}

fn guarded_restore_database_logical_digest(
    plan: &RestoreImportPlan,
    database: &MigrationDatabaseApplyEntry,
    label: &str,
    expected_raw_sha256: Option<&str>,
    barriers: &mut BTreeMap<String, WriteExclusionGuard>,
) -> Result<String, String> {
    with_guarded_restore_database_copy(
        plan,
        database,
        label,
        expected_raw_sha256,
        barriers,
        |copy| {
            quick_check_sqlite(copy)?;
            let snapshot = plan.staging_root.join(format!(
                ".guarded-{label}-{}-logical.sqlite",
                safe_path_component(&database.database_id)
            ));
            if snapshot.exists() {
                return Err("restore import guarded logical snapshot already exists".to_string());
            }
            snapshot_sqlite_database(copy, &snapshot)?;
            let logical_sha256 = stable_file_digest(&snapshot)?.1;
            remove_exact_replacement_artifact(&snapshot, &logical_sha256)?;
            Ok(logical_sha256)
        },
    )
}

fn guarded_restore_database_quick_check(
    plan: &RestoreImportPlan,
    database: &MigrationDatabaseApplyEntry,
    label: &str,
    expected_raw_sha256: &str,
    barriers: &mut BTreeMap<String, WriteExclusionGuard>,
) -> Result<(), String> {
    with_guarded_restore_database_copy(
        plan,
        database,
        label,
        Some(expected_raw_sha256),
        barriers,
        quick_check_sqlite,
    )
}

fn validate_source_databases_unchanged(plan: &RestoreImportPlan) -> Result<(), String> {
    for (index, database) in plan.source_databases.iter().enumerate() {
        quick_check_sqlite(&database.snapshot_path)?;
        if database
            .source_path
            .file_name()
            .is_some_and(|name| name == "goals_1.sqlite")
        {
            let connection = Connection::open_with_flags(
                &database.snapshot_path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .map_err(|_| "failed to open restore import goals source".to_string())?;
            goals_database_digest(&connection)?;
        }
        if stable_file_digest(&database.snapshot_path)?.1 != database.sha256 {
            return Err("restore import source database snapshot changed".to_string());
        }
        let recheck = plan
            .staging_root
            .join(format!(".source-recheck-{index:02}.sqlite"));
        if recheck.exists() {
            fs::remove_file(&recheck).map_err(|_| {
                "failed to reset restore import source database recheck".to_string()
            })?;
        }
        snapshot_sqlite(&database.source_path, &recheck)?;
        let recheck_sha256 = stable_file_digest(&recheck)?.1;
        fs::remove_file(&recheck)
            .map_err(|_| "failed to remove restore import source database recheck".to_string())?;
        if recheck_sha256 != database.sha256 {
            return Err("source database changed after restore import planning".to_string());
        }
    }
    Ok(())
}

fn receipt_from_plan(plan: &RestoreImportPlan, validated: bool) -> RestoreImportReceipt {
    let unchanged_session_count = plan
        .sessions
        .iter()
        .filter(|session| session.action == RestoreImportSessionAction::Unchanged)
        .count();
    let current_ahead_session_count = plan
        .sessions
        .iter()
        .filter(|session| session.action == RestoreImportSessionAction::KeepCanonical)
        .count();
    let imported_new_session_count = plan
        .sessions
        .iter()
        .filter(|session| session.action == RestoreImportSessionAction::ImportNew)
        .count();
    let imported_extension_count = plan
        .sessions
        .iter()
        .filter(|session| session.action == RestoreImportSessionAction::ImportExtension)
        .count();
    let imported_bytes = imported_sessions(plan)
        .try_fold(0_u64, |total, session| {
            total.checked_add(session.source_bytes)
        })
        .unwrap_or(u64::MAX);
    let unclassified_recovery_bytes = plan
        .unclassified_payloads
        .iter()
        .try_fold(0_u64, |total, payload| {
            total.checked_add(payload.source_bytes)
        })
        .unwrap_or(u64::MAX);
    RestoreImportReceipt {
        operation_id: plan.operation_id.clone(),
        package_operation_id: plan.package_operation_id.clone(),
        target_version: plan.target_version.clone(),
        package_dir: plan.package_dir.clone(),
        scanned_session_count: plan
            .sessions
            .len()
            .saturating_add(
                plan.conflicts
                    .iter()
                    .map(|conflict| conflict.candidate_paths.len())
                    .sum::<usize>(),
            )
            .saturating_add(plan.unclassified_payloads.len()),
        unchanged_session_count,
        current_ahead_session_count,
        imported_new_session_count,
        imported_extension_count,
        conflict_count: plan.conflicts.len(),
        unclassified_recovery_count: plan.unclassified_payloads.len(),
        unclassified_recovery_bytes,
        unclassified_recovery_paths: plan
            .unclassified_payloads
            .iter()
            .filter_map(|payload| {
                payload
                    .recovery_path
                    .strip_prefix(&plan.data_root)
                    .ok()
                    .map(Path::to_path_buf)
            })
            .collect(),
        anomaly_count: plan.anomaly_count,
        database_view_count: if imported_new_session_count + imported_extension_count > 0 {
            plan.databases.len()
        } else {
            0
        },
        imported_bytes,
        recovery_expires_at_ms: plan.recovery_expires_at_ms,
        validated,
        runtime_verification: None,
    }
}

fn imported_sessions(plan: &RestoreImportPlan) -> impl Iterator<Item = &RestoreImportSessionPlan> {
    plan.sessions.iter().filter(|session| {
        matches!(
            session.action,
            RestoreImportSessionAction::ImportNew | RestoreImportSessionAction::ImportExtension
        )
    })
}

fn validate_plan(plan: &RestoreImportPlan) -> Result<(), String> {
    validate_operation_id(&plan.operation_id)?;
    validate_absolute_directory(&plan.canonical_root, "canonical root")?;
    validate_absolute_directory(&plan.data_root, "data root")?;
    validate_absolute_directory(&plan.package_dir, "downgrade package")?;
    if plan.schema_version != RESTORE_IMPORT_SCHEMA_VERSION
        || plan.package_operation_id.trim().is_empty()
        || plan.target_version.trim().is_empty()
        || plan.work_root != operation_root(&plan.data_root, &plan.operation_id)?
        || plan.staging_root != plan.work_root.join("restore-import-staging")
        || plan.recovery_root
            != plan
                .data_root
                .join("session-storage-v1/restore-import-recovery")
                .join(&plan.operation_id)
        || plan.recovery_expires_at_ms <= plan.generated_at_ms
        || plan.source_fingerprint.len() != 64
        || !plan.source_fingerprint.bytes().all(is_lower_hex)
    {
        return Err("restore import plan shape is invalid".to_string());
    }
    validate_contained_directory(
        &plan.work_root,
        &plan.staging_root,
        "restore import staging",
    )?;
    validate_contained_directory(
        &plan
            .data_root
            .join("session-storage-v1/restore-import-recovery"),
        &plan.recovery_root,
        "restore import recovery",
    )?;
    let source_database_paths = plan
        .source_databases
        .iter()
        .map(|database| path_key(&database.snapshot_path))
        .collect::<BTreeSet<_>>();
    if source_database_paths.len() != plan.source_databases.len() {
        return Err("restore import source database plan is duplicated".to_string());
    }
    for database in &plan.source_databases {
        if !database.source_path.starts_with(&plan.package_dir)
            || !database.snapshot_path.starts_with(&plan.staging_root)
            || database.sha256.len() != 64
            || !database.sha256.bytes().all(is_lower_hex)
        {
            return Err("restore import source database plan is invalid".to_string());
        }
    }
    let mut thread_ids = BTreeSet::new();
    for session in &plan.sessions {
        if session.thread_id.trim().is_empty()
            || !thread_ids.insert(session.thread_id.clone())
            || !session.source_path.starts_with(&plan.package_dir)
            || !source_database_paths.contains(&path_key(&session.source_database_snapshot))
            || session.source_sha256.len() != 64
            || !session.source_sha256.bytes().all(is_lower_hex)
            || session.source_bytes == 0
            || !session
                .canonical_path
                .starts_with(plan.canonical_root.join("sessions"))
        {
            return Err("restore import session plan is invalid".to_string());
        }
        if session.synthesize_database_row
            && (session.action != RestoreImportSessionAction::ImportNew
                || session.baseline_sha256.is_some()
                || session.canonical_before_sha256.is_some()
                || session.relation.is_some())
        {
            return Err("synthetic restore import database row plan is invalid".to_string());
        }
        match session.action {
            RestoreImportSessionAction::ImportNew => {
                if session
                    .source_staged_path
                    .as_ref()
                    .is_none_or(|path| !path.starts_with(&plan.staging_root))
                    || session.canonical_before_sha256.is_some()
                    || session.canonical_backup_payload.is_some()
                {
                    return Err("restore import new session plan is invalid".to_string());
                }
            }
            RestoreImportSessionAction::ImportExtension => {
                if session
                    .source_staged_path
                    .as_ref()
                    .is_none_or(|path| !path.starts_with(&plan.staging_root))
                    || session
                        .canonical_backup_payload
                        .as_ref()
                        .is_none_or(|path| !path.starts_with(&plan.recovery_root))
                    || session
                        .canonical_before_sha256
                        .as_ref()
                        .is_none_or(|hash| hash.len() != 64 || !hash.bytes().all(is_lower_hex))
                {
                    return Err("restore import extension plan is invalid".to_string());
                }
            }
            RestoreImportSessionAction::Unchanged | RestoreImportSessionAction::KeepCanonical => {
                if session.source_staged_path.is_some()
                    || session.canonical_backup_payload.is_some()
                    || session.canonical_before_sha256.is_none()
                {
                    return Err("restore import retained session plan is invalid".to_string());
                }
            }
        }
    }
    for conflict in &plan.conflicts {
        if conflict.thread_id.trim().is_empty()
            || !thread_ids.insert(conflict.thread_id.clone())
            || conflict.default_overwrite
            || conflict.candidate_paths.is_empty()
            || conflict.candidate_paths.len() != conflict.candidate_sha256.len()
            || conflict.candidate_paths.len() != conflict.recovery_paths.len()
            || conflict
                .candidate_paths
                .iter()
                .any(|path| !path.starts_with(&plan.package_dir))
            || conflict
                .recovery_paths
                .iter()
                .any(|path| !path.starts_with(&plan.recovery_root))
            || conflict
                .candidate_sha256
                .iter()
                .any(|hash| hash.len() != 64 || !hash.bytes().all(is_lower_hex))
        {
            return Err("restore import conflict plan is invalid".to_string());
        }
    }
    if plan.source_kind != RestoreImportSourceKind::DowngradePackage
        && !plan.unclassified_payloads.is_empty()
    {
        return Err("pending recovery import cannot contain unclassified payloads".to_string());
    }
    let package_recovery_root = plan.package_dir.join("recovery");
    let retained_unclassified_root = plan.recovery_root.join("unclassified");
    let mut unclassified_sources = BTreeSet::new();
    let mut unclassified_recoveries = BTreeSet::new();
    for payload in &plan.unclassified_payloads {
        if !payload.source_path.starts_with(&package_recovery_root)
            || !payload
                .recovery_path
                .starts_with(&retained_unclassified_root)
            || payload.source_bytes == 0
            || payload.source_sha256.len() != 64
            || !payload.source_sha256.bytes().all(is_lower_hex)
            || !unclassified_sources.insert(path_key(&payload.source_path))
            || !unclassified_recoveries.insert(path_key(&payload.recovery_path))
        {
            return Err("restore import unclassified payload plan is invalid".to_string());
        }
    }
    let mut database_targets = BTreeSet::new();
    for database in &plan.databases {
        if !database.role.is_runtime()
            || !database_targets.insert(path_key(&database.target_path))
            || !database.staged_path.starts_with(&plan.staging_root)
            || !database
                .original_backup_payload
                .starts_with(&plan.recovery_root)
            || database.original_sha256.len() != 64
            || database.staged_sha256.len() != 64
            || !database.original_sha256.bytes().all(is_lower_hex)
            || !database.staged_sha256.bytes().all(is_lower_hex)
        {
            return Err("restore import database plan is invalid".to_string());
        }
        if is_created_goals_database(database)
            && (database
                .target_path
                .file_name()
                .is_none_or(|name| name != "goals_1.sqlite")
                || database.original_sha256 != "0".repeat(64))
        {
            return Err("restore import created goals database plan is invalid".to_string());
        }
    }
    if imported_sessions(plan).next().is_some()
        && !plan
            .databases
            .iter()
            .any(|database| database.role == DatabaseRole::CanonicalAccount)
    {
        return Err("restore import has no canonical Account database".to_string());
    }
    Ok(())
}

fn operation_root(data_root: &Path, operation_id: &str) -> Result<PathBuf, String> {
    validate_operation_id(operation_id)?;
    Ok(data_root
        .join("session-storage-v1/operations")
        .join(operation_id))
}

fn plan_path(data_root: &Path, operation_id: &str) -> Result<PathBuf, String> {
    Ok(operation_root(data_root, operation_id)?.join("restore-import-plan.json"))
}

fn replacement_phase_path(data_root: &Path, operation_id: &str) -> Result<PathBuf, String> {
    Ok(operation_root(data_root, operation_id)?.join("restore-import-replacement-phases.bin"))
}

fn replacement_phase_digest(
    record: &RestoreImportReplacementPhaseRecord,
) -> Result<String, String> {
    let bytes = serde_json::to_vec(record)
        .map_err(|_| "failed to serialize restore import replacement phase".to_string())?;
    Ok(hex_sha256(Sha256::digest(bytes)))
}

fn persist_initial_restore_import_replacement_phases(
    plan: &RestoreImportPlan,
    replacements: &[RestoreImportReplacementPlan],
) -> Result<(), String> {
    if replacements.is_empty() {
        return Ok(());
    }
    let path = replacement_phase_path(&plan.data_root, &plan.operation_id)?;
    if path.exists() {
        let record = load_restore_import_replacement_phases(plan, replacements)?;
        if record
            .replacements
            .iter()
            .any(|entry| entry.phase != RestoreImportReplacementPhase::Planned)
        {
            return Err("restore import replacement phase record already advanced".to_string());
        }
        return ensure_restore_import_original_witnesses(plan, replacements);
    }
    let mut barriers = Vec::with_capacity(replacements.len());
    let mut entries = Vec::with_capacity(replacements.len());
    for replacement in replacements {
        let mut barrier = WriteExclusionGuard::acquire(&replacement.target_path)?;
        barrier.verify_current_path(Some(&replacement.live_original_sha256))?;
        entries.push(RestoreImportReplacementPhaseEntry {
            parent_identity: parent_directory_identity_at_path(&replacement.target_path)?.into(),
            original_identity: barrier.identity()?.into(),
            replacement: replacement.clone(),
            replacement_identity: None,
            phase: RestoreImportReplacementPhase::Planned,
        });
        barriers.push(barrier);
    }
    let record = RestoreImportReplacementPhaseRecord {
        schema_version: RESTORE_IMPORT_SCHEMA_VERSION,
        operation_id: plan.operation_id.clone(),
        plan_integrity_sha256: plan_digest(plan, replacements)?,
        updated_at_ms: timestamp_millis()?,
        replacements: entries,
    };
    write_restore_import_replacement_phases(plan, &record)?;
    drop(barriers);
    // Reacquire through the normal witness path after the durable identity
    // record exists. Keeping the first exclusive handles alive here would make
    // the second acquisition contend with this operation itself; any external
    // writer that wins the intentional reacquire gap is rejected by the
    // persisted hash and file-identity checks before a live mutation.
    ensure_restore_import_original_witnesses(plan, replacements)
}

fn ensure_restore_import_original_witnesses(
    plan: &RestoreImportPlan,
    replacements: &[RestoreImportReplacementPlan],
) -> Result<(), String> {
    let record = load_restore_import_replacement_phases(plan, replacements)?;
    let mut barriers = Vec::with_capacity(replacements.len());
    for (replacement, entry) in replacements.iter().zip(&record.replacements) {
        let mut barrier = WriteExclusionGuard::acquire(&replacement.target_path)?;
        barrier.verify_current_path(Some(&replacement.live_original_sha256))?;
        if barrier.identity()? != entry.original_identity.into()
            || parent_directory_identity_at_path(&replacement.target_path)?
                != entry.parent_identity.into()
        {
            return Err("restore import planned original identity changed".to_string());
        }
        barriers.push(barrier);
    }
    let mut created = Vec::<&RestoreImportReplacementPlan>::new();
    let result = (|| {
        for replacement in replacements {
            match fs::hard_link(&replacement.target_path, &replacement.original_witness_path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let entry = record
                        .replacements
                        .iter()
                        .find(|entry| entry.replacement == *replacement)
                        .expect("validated replacement phase entry");
                    if stable_regular_file_identity(&replacement.original_witness_path)?
                        != entry.original_identity
                        || stable_file_digest(&replacement.original_witness_path)?.1
                            != replacement.live_original_sha256
                    {
                        return Err(
                            "restore import original identity witness appeared concurrently"
                                .to_string(),
                        );
                    }
                    continue;
                }
                Err(_) => {
                    return Err(
                        "failed to bind restore import original identity witness".to_string()
                    )
                }
            }
            created.push(replacement);
            if stable_file_digest(&replacement.original_witness_path)?.1
                != replacement.live_original_sha256
                || !same_regular_file_identity(
                    &replacement.target_path,
                    &replacement.original_witness_path,
                )
                .unwrap_or(false)
            {
                return Err(
                    "restore import original identity witness verification failed".to_string(),
                );
            }
        }
        Ok(())
    })();
    if result.is_err() {
        for replacement in created.into_iter().rev() {
            if same_regular_file_identity(
                &replacement.target_path,
                &replacement.original_witness_path,
            )
            .unwrap_or(false)
            {
                let _ = remove_exact_replacement_artifact(
                    &replacement.original_witness_path,
                    &replacement.live_original_sha256,
                );
            }
        }
    }
    drop(barriers);
    result
}

fn load_restore_import_replacement_phases(
    plan: &RestoreImportPlan,
    replacements: &[RestoreImportReplacementPlan],
) -> Result<RestoreImportReplacementPhaseRecord, String> {
    if replacements.is_empty() {
        return Ok(RestoreImportReplacementPhaseRecord {
            schema_version: RESTORE_IMPORT_SCHEMA_VERSION,
            operation_id: plan.operation_id.clone(),
            plan_integrity_sha256: plan_digest(plan, replacements)?,
            updated_at_ms: plan.generated_at_ms,
            replacements: Vec::new(),
        });
    }
    let path = replacement_phase_path(&plan.data_root, &plan.operation_id)?;
    let protected = read_regular_file_bounded(&path, MAX_REPLACEMENT_PHASE_BYTES)
        .map_err(|_| "restore import replacement phase record is unreadable".to_string())?;
    let plaintext =
        if let Some(ciphertext) = protected.strip_prefix(REPLACEMENT_PHASE_CIPHERTEXT_MAGIC) {
            crate::crypto::unprotect(ciphertext)
                .map_err(|_| "restore import replacement phase record is unreadable".to_string())?
        } else {
            #[cfg(windows)]
            {
                return Err("restore import replacement phase record is not protected".to_string());
            }
            #[cfg(not(windows))]
            {
                protected
            }
        };
    if plaintext.len() as u64 > MAX_REPLACEMENT_PHASE_BYTES {
        return Err("restore import replacement phase record reached its size limit".to_string());
    }
    let envelope = serde_json::from_slice::<RestoreImportReplacementPhaseEnvelope>(&plaintext)
        .map_err(|_| "restore import replacement phase record is invalid".to_string())?;
    if replacement_phase_digest(&envelope.record)? != envelope.integrity_sha256
        || envelope.record.schema_version != RESTORE_IMPORT_SCHEMA_VERSION
        || envelope.record.operation_id != plan.operation_id
        || envelope.record.plan_integrity_sha256 != plan_digest(plan, replacements)?
        || envelope.record.replacements.len() != replacements.len()
        || envelope
            .record
            .replacements
            .iter()
            .zip(replacements)
            .any(|(entry, replacement)| entry.replacement != *replacement)
    {
        return Err("restore import replacement phase record integrity check failed".to_string());
    }
    Ok(envelope.record)
}

fn write_restore_import_replacement_phases(
    plan: &RestoreImportPlan,
    record: &RestoreImportReplacementPhaseRecord,
) -> Result<(), String> {
    let envelope = RestoreImportReplacementPhaseEnvelope {
        integrity_sha256: replacement_phase_digest(record)?,
        record: record.clone(),
    };
    let plaintext = serde_json::to_vec_pretty(&envelope)
        .map_err(|_| "failed to serialize restore import replacement phase".to_string())?;
    if plaintext.len() as u64 > MAX_REPLACEMENT_PHASE_BYTES {
        return Err("restore import replacement phase record reached its size limit".to_string());
    }
    #[cfg(windows)]
    let bytes = {
        let ciphertext = crate::crypto::protect(&plaintext)
            .map_err(|_| "failed to protect restore import replacement phase".to_string())?;
        let mut protected =
            Vec::with_capacity(REPLACEMENT_PHASE_CIPHERTEXT_MAGIC.len() + ciphertext.len());
        protected.extend_from_slice(REPLACEMENT_PHASE_CIPHERTEXT_MAGIC);
        protected.extend_from_slice(&ciphertext);
        protected
    };
    #[cfg(not(windows))]
    let bytes = plaintext;
    atomic_write(
        &replacement_phase_path(&plan.data_root, &plan.operation_id)?,
        &bytes,
    )
}

fn remove_restore_import_replacement_phase_record(
    plan: &RestoreImportPlan,
    replacements: &[RestoreImportReplacementPlan],
    allowed: &[RestoreImportReplacementPhase],
) -> Result<(), String> {
    if replacements.is_empty() {
        return Ok(());
    }
    let path = replacement_phase_path(&plan.data_root, &plan.operation_id)?;
    if !replacement_artifact_exists(&path, "phase record")? {
        return Ok(());
    }
    let record = load_restore_import_replacement_phases(plan, replacements)?;
    if record
        .replacements
        .iter()
        .any(|entry| !allowed.contains(&entry.phase))
    {
        return Err(
            "restore import replacement phase record is not terminal for cleanup".to_string(),
        );
    }
    let expected_sha256 = stable_file_digest(&path)?.1;
    remove_exact_replacement_artifact(&path, &expected_sha256)
}

fn transition_restore_import_replacement_phase(
    plan: &RestoreImportPlan,
    replacements: &[RestoreImportReplacementPlan],
    replacement: &RestoreImportReplacementPlan,
    allowed: &[RestoreImportReplacementPhase],
    next: RestoreImportReplacementPhase,
) -> Result<(), String> {
    let mut record = load_restore_import_replacement_phases(plan, replacements)?;
    let entry = record
        .replacements
        .iter_mut()
        .find(|entry| entry.replacement == *replacement)
        .ok_or_else(|| "restore import replacement phase entry is missing".to_string())?;
    if entry.phase == next {
        return Ok(());
    }
    if !allowed.contains(&entry.phase) {
        return Err("restore import replacement phase transition is invalid".to_string());
    }
    if !matches!(
        next,
        RestoreImportReplacementPhase::Planned
            | RestoreImportReplacementPhase::Staging
            | RestoreImportReplacementPhase::Cleaned
    ) && entry.replacement_identity.is_none()
    {
        return Err("restore import replacement identity was not persisted".to_string());
    }
    entry.phase = next;
    record.updated_at_ms = timestamp_millis()?;
    write_restore_import_replacement_phases(plan, &record)
}

fn record_restore_import_replacement_staged(
    plan: &RestoreImportPlan,
    replacements: &[RestoreImportReplacementPlan],
    replacement: &RestoreImportReplacementPlan,
    identities: HandleReplaceIdentityBindings,
) -> Result<(), String> {
    let mut record = load_restore_import_replacement_phases(plan, replacements)?;
    let entry = record
        .replacements
        .iter_mut()
        .find(|entry| entry.replacement == *replacement)
        .ok_or_else(|| "restore import replacement phase entry is missing".to_string())?;
    if entry.phase == RestoreImportReplacementPhase::Staged
        && entry.parent_identity == identities.parent_identity.into()
        && entry.original_identity == identities.original_identity.into()
        && entry.replacement_identity == Some(identities.replacement_identity.into())
    {
        return Ok(());
    }
    if entry.phase != RestoreImportReplacementPhase::Staging
        || entry.parent_identity != identities.parent_identity.into()
        || entry.original_identity != identities.original_identity.into()
        || entry.replacement_identity.is_some()
    {
        return Err("restore import staged replacement identity changed".to_string());
    }
    entry.replacement_identity = Some(identities.replacement_identity.into());
    entry.phase = RestoreImportReplacementPhase::Staged;
    record.updated_at_ms = timestamp_millis()?;
    write_restore_import_replacement_phases(plan, &record)
}

fn plan_digest(
    plan: &RestoreImportPlan,
    replacements: &[RestoreImportReplacementPlan],
) -> Result<String, String> {
    let bytes = serde_json::to_vec(&(plan, replacements))
        .map_err(|_| "failed to serialize restore import plan".to_string())?;
    Ok(hex_sha256(Sha256::digest(bytes)))
}

fn snapshot_sqlite(source: &Path, target: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(source)
        .map_err(|_| "restore import database source is unavailable".to_string())?;
    if !metadata.is_file() || metadata_is_link_or_reparse(&metadata) {
        return Err("restore import database source is unsafe".to_string());
    }
    if let Some(parent) = target.parent() {
        create_safe_directory(parent)?;
    }
    let source_connection = Connection::open_with_flags(
        source,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| "failed to open restore import database source".to_string())?;
    source_connection
        .backup(MAIN_DB, target, None)
        .map_err(|_| "failed to snapshot restore import database".to_string())?;
    quick_check_sqlite(target)
}

fn quick_check_sqlite(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| "failed to inspect restore import SQLite payload".to_string())?;
    if !metadata.is_file() || metadata_is_link_or_reparse(&metadata) {
        return Err("restore import SQLite payload path is unsafe".to_string());
    }
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| "failed to open restore import SQLite payload".to_string())?;
    let result: String = connection
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(|_| "restore import SQLite quick_check failed".to_string())?;
    if result != "ok" {
        return Err("restore import SQLite payload is invalid".to_string());
    }
    Ok(())
}

fn verify_session_identity(path: &Path, thread_id: &str, sha256: &str) -> Result<(), String> {
    let semantic = read_semantic_session(path)
        .map_err(|_| "restore import session payload is invalid".to_string())?;
    if semantic.thread_id != thread_id || hex_sha256(semantic.raw_sha256) != sha256 {
        return Err("restore import session payload identity changed".to_string());
    }
    Ok(())
}

fn write_work_marker(root: &Path, operation_id: &str, package_dir: &Path) -> Result<(), String> {
    let marker = serde_json::json!({
        "schemaVersion": RESTORE_IMPORT_SCHEMA_VERSION,
        "operationId": operation_id,
        "packageDirHash": hex_sha256(Sha256::digest(path_key(package_dir).as_bytes())),
    });
    let bytes = serde_json::to_vec_pretty(&marker)
        .map_err(|_| "failed to serialize restore import marker".to_string())?;
    atomic_write(&root.join(WORK_MARKER_NAME), &bytes)
}

fn validate_work_marker(root: &Path, operation_id: &str) -> Result<(), String> {
    let bytes = read_regular_file_bounded(&root.join(WORK_MARKER_NAME), 64 * 1024)
        .map_err(|_| "restore import work marker is unavailable".to_string())?;
    let marker = serde_json::from_slice::<serde_json::Value>(&bytes)
        .map_err(|_| "restore import work marker is invalid".to_string())?;
    if marker
        .get("schemaVersion")
        .and_then(serde_json::Value::as_u64)
        != Some(RESTORE_IMPORT_SCHEMA_VERSION as u64)
        || marker
            .get("operationId")
            .and_then(serde_json::Value::as_str)
            != Some(operation_id)
    {
        return Err("restore import work marker identity changed".to_string());
    }
    Ok(())
}

fn remove_owned_work_tree(root: &Path, parent: &Path, operation_id: &str) -> Result<(), String> {
    if root.parent() != Some(parent) || !root.is_absolute() {
        return Err("restore import cleanup root is invalid".to_string());
    }
    validate_work_marker(root, operation_id)?;
    let mut entries = Vec::new();
    for entry in WalkDir::new(root).follow_links(false).contents_first(true) {
        let entry =
            entry.map_err(|_| "failed to inspect restore import cleanup tree".to_string())?;
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|_| "failed to inspect restore import cleanup entry".to_string())?;
        if metadata_is_link_or_reparse(&metadata) || (!metadata.is_file() && !metadata.is_dir()) {
            return Err("restore import cleanup tree contains an unsafe entry".to_string());
        }
        entries.push((entry.path().to_path_buf(), metadata.is_dir()));
    }
    for (path, is_directory) in entries {
        if is_directory {
            fs::remove_dir(&path)
                .map_err(|_| "failed to remove restore import cleanup directory".to_string())?;
        } else {
            fs::remove_file(&path)
                .map_err(|_| "failed to remove restore import cleanup file".to_string())?;
        }
    }
    Ok(())
}

fn create_safe_directory(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path)
        .map_err(|_| "failed to create restore import directory".to_string())?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| "failed to inspect restore import directory".to_string())?;
    if !metadata.is_dir() || metadata_is_link_or_reparse(&metadata) {
        return Err("restore import directory is unsafe".to_string());
    }
    Ok(())
}

fn validate_absolute_directory(path: &Path, label: &str) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(format!("{label} must be absolute"));
    }
    let metadata = fs::symlink_metadata(path).map_err(|_| format!("{label} is unavailable"))?;
    if !metadata.is_dir() || metadata_is_link_or_reparse(&metadata) {
        return Err(format!("{label} is unsafe"));
    }
    Ok(())
}

fn validate_contained_directory(root: &Path, path: &Path, label: &str) -> Result<(), String> {
    if !path.starts_with(root) || path == root {
        return Err(format!("{label} escaped its root"));
    }
    let relative = path
        .strip_prefix(root)
        .map_err(|_| format!("{label} escaped its root"))?;
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err(format!("{label} path is invalid"));
    }
    validate_absolute_directory(path, label)
}

fn validate_operation_id(operation_id: &str) -> Result<(), String> {
    if operation_id.is_empty()
        || operation_id.len() > 160
        || !operation_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("restore import operation id is invalid".to_string());
    }
    Ok(())
}

fn safe_path_component(value: &str) -> String {
    let output = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if output.is_empty() {
        "unknown".to_string()
    } else {
        output
    }
}

fn hex_sha256(value: impl AsRef<[u8]>) -> String {
    value
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn is_lower_hex(byte: u8) -> bool {
    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
}

#[cfg(windows)]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn stable_regular_file_identity(path: &Path) -> Result<RestoreImportFileIdentity, String> {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    fn identity(file: &fs::File) -> Result<RestoreImportFileIdentity, String> {
        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        if unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), &mut information) } == 0
        {
            return Err("restore import file identity is unavailable".to_string());
        }
        Ok(RestoreImportFileIdentity {
            volume_id: u64::from(information.dwVolumeSerialNumber),
            file_id: (u64::from(information.nFileIndexHigh) << 32)
                | u64::from(information.nFileIndexLow),
        })
    }

    let metadata = fs::symlink_metadata(path)
        .map_err(|_| "restore import identity path is unavailable".to_string())?;
    if !metadata.is_file() || metadata_is_link_or_reparse(&metadata) {
        return Err("restore import identity path is unsafe".to_string());
    }
    let open = || {
        let mut options = fs::OpenOptions::new();
        options
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .map_err(|_| "restore import identity path is unavailable".to_string())
    };
    let first = open()?;
    let first_identity = identity(&first)?;
    let second = open()?;
    if identity(&second)? != first_identity {
        return Err("restore import identity path changed during observation".to_string());
    }
    Ok(first_identity)
}

#[cfg(unix)]
fn stable_regular_file_identity(path: &Path) -> Result<RestoreImportFileIdentity, String> {
    use std::os::unix::fs::MetadataExt;

    let metadata = fs::symlink_metadata(path)
        .map_err(|_| "restore import identity path is unavailable".to_string())?;
    if !metadata.is_file() || metadata_is_link_or_reparse(&metadata) {
        return Err("restore import identity path is unsafe".to_string());
    }
    Ok(RestoreImportFileIdentity {
        volume_id: metadata.dev(),
        file_id: metadata.ino(),
    })
}

#[cfg(not(any(windows, unix)))]
fn stable_regular_file_identity(_path: &Path) -> Result<RestoreImportFileIdentity, String> {
    Err("restore import file identity is unsupported".to_string())
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use rusqlite::Connection;
    use sha2::{Digest, Sha256};
    use tempfile::tempdir;

    use super::{
        build_restore_import_replacement, build_restore_import_replacements,
        cleanup_committed_restore_import_ownership_witnesses, create_safe_directory,
        execute_restore_import, operation_root, prepare_restore_import, read_semantic_session,
        recover_interrupted_restore_import, rollback_restore_import,
        rollback_restore_import_replacement, write_work_marker, HandleReplacePaths,
        RestoreImportRecoveryStatus, RestoreImportReplacementKind, RestoreImportReplacementPhase,
        RestoreImportReplacementPhaseEntry, RestoreImportReplacementPhaseRecord,
        RestoreImportReplacementPlan, RestoreImportSessionAction, WriteExclusionGuard,
    };
    use crate::file_ops::ownership_witness_path;
    use crate::session_storage::bounded_file::same_regular_file_identity;
    use crate::session_storage::downgrade::{
        execute_downgrade_export, prepare_downgrade_export, DowngradeExportManifest,
        DowngradePackageEntry, DowngradePackageFileKind,
    };
    use crate::session_storage::migration::{persist_migration_preflight, run_migration_preflight};
    use crate::session_storage::operation_ledger::{
        OperationLedgerStore, SessionStorageOperationKind, SessionStorageOperationPhase,
    };
    use crate::session_storage::reference_graph::path_key;
    use crate::session_storage::storage_state::{
        finalize_canonical_storage_state, prepare_canonical_storage_state,
    };

    const BASE_THREAD: &str = "11111111-1111-4111-8111-111111111111";
    const NEW_THREAD: &str = "22222222-2222-4222-8222-222222222222";
    const SHARED_GOAL_THREAD: &str = "33333333-3333-4333-8333-333333333333";

    struct Fixture {
        _root: tempfile::TempDir,
        home: std::path::PathBuf,
        data: std::path::PathBuf,
        package: std::path::PathBuf,
    }

    fn raw_replacement(
        root: &Path,
        _operation_id: &str,
    ) -> (RestoreImportReplacementPlan, Vec<u8>, Vec<u8>) {
        let target = root.join("live.jsonl");
        let source = root.join("staged.jsonl");
        let before = b"before replacement\n".to_vec();
        let after = b"after replacement\n".to_vec();
        fs::write(&target, &before).unwrap();
        fs::write(&source, &after).unwrap();
        let original_sha256 = super::stable_file_digest(&target).unwrap().1;
        let replacement_sha256 = super::stable_file_digest(&source).unwrap().1;
        let replacement = build_restore_import_replacement(
            "raw-replacement-test",
            RestoreImportReplacementKind::SessionExtension,
            &target,
            &source,
            &original_sha256,
            &original_sha256,
            &replacement_sha256,
        )
        .unwrap();
        fs::hard_link(&target, &replacement.original_witness_path).unwrap();
        (replacement, before, after)
    }

    fn typed_replacement_paths(replacement: &RestoreImportReplacementPlan) -> HandleReplacePaths {
        HandleReplacePaths::from_persisted_plan(
            replacement.target_path.clone(),
            replacement.recovery_path.clone(),
            replacement.staging_path.clone(),
            replacement.tombstone_path.clone(),
        )
        .unwrap()
    }

    fn create_replacement_witness(replacement: &RestoreImportReplacementPlan) {
        fs::copy(
            &replacement.source_path,
            &replacement.replacement_witness_path,
        )
        .unwrap();
    }

    fn assert_replacement_artifacts_absent(replacement: &RestoreImportReplacementPlan) {
        for path in [
            &replacement.original_witness_path,
            &replacement.replacement_witness_path,
            &replacement.staging_path,
            &replacement.recovery_path,
            &replacement.tombstone_path,
        ] {
            assert!(!path.exists(), "replacement artifact remained: {path:?}");
        }
    }

    fn rollback_raw_replacement(
        replacement: &RestoreImportReplacementPlan,
        phase: RestoreImportReplacementPhase,
    ) -> Result<(), String> {
        let root = replacement.target_path.parent().unwrap();
        let plan = super::RestoreImportPlan {
            schema_version: super::RESTORE_IMPORT_SCHEMA_VERSION,
            operation_id: "raw-replacement-test".to_string(),
            generated_at_ms: 1,
            package_operation_id: "raw-package".to_string(),
            target_version: "test".to_string(),
            source_kind: super::RestoreImportSourceKind::DowngradePackage,
            package_dir: root.to_path_buf(),
            canonical_root: root.to_path_buf(),
            data_root: root.to_path_buf(),
            source_fingerprint: "0".repeat(64),
            work_root: root.to_path_buf(),
            staging_root: root.to_path_buf(),
            recovery_root: root.to_path_buf(),
            recovery_expires_at_ms: 2,
            sessions: Vec::new(),
            conflicts: Vec::new(),
            unclassified_payloads: Vec::new(),
            source_databases: Vec::new(),
            databases: Vec::new(),
            anomaly_count: 0,
        };
        let replacements = vec![replacement.clone()];
        let envelope = super::RestoreImportPlanEnvelope {
            integrity_sha256: super::plan_digest(&plan, &replacements)?,
            plan: plan.clone(),
            replacements: replacements.clone(),
        };
        fs::create_dir_all(super::operation_root(&plan.data_root, &plan.operation_id)?).unwrap();
        super::atomic_write(
            &super::plan_path(&plan.data_root, &plan.operation_id)?,
            &serde_json::to_vec_pretty(&envelope).unwrap(),
        )?;
        let original_identity =
            super::stable_regular_file_identity(&replacement.original_witness_path)?;
        let replacement_identity = match phase {
            RestoreImportReplacementPhase::Planned => None,
            _ => {
                let artifact = if replacement.staging_path.is_file() {
                    &replacement.staging_path
                } else if replacement.target_path.is_file()
                    && super::stable_file_digest(&replacement.target_path)?.1
                        == replacement.replacement_sha256
                {
                    &replacement.target_path
                } else {
                    &replacement.tombstone_path
                };
                Some(super::stable_regular_file_identity(artifact)?)
            }
        };
        let record = RestoreImportReplacementPhaseRecord {
            schema_version: super::RESTORE_IMPORT_SCHEMA_VERSION,
            operation_id: plan.operation_id.clone(),
            plan_integrity_sha256: super::plan_digest(&plan, &replacements)?,
            updated_at_ms: 1,
            replacements: vec![RestoreImportReplacementPhaseEntry {
                replacement: replacement.clone(),
                parent_identity: super::parent_directory_identity_at_path(
                    &replacement.target_path,
                )?
                .into(),
                original_identity,
                replacement_identity,
                phase,
            }],
        };
        super::write_restore_import_replacement_phases(&plan, &record)?;
        let durable_entry = super::load_restore_import_replacement_phases(&plan, &replacements)?
            .replacements
            .remove(0);
        rollback_restore_import_replacement(&plan, &replacements, replacement, &durable_entry)
    }

    fn fixture() -> Fixture {
        fixture_with_setup(|_, _| {})
    }

    fn conflict_fixture() -> Fixture {
        fixture_with_setup(|_, data| {
            let shared = data.join("shared-sessions");
            let branch = shared.join("sessions/2026/08/12/rollout-conflict.jsonl");
            write_session(&branch, BASE_THREAD, &["shared divergent branch"]);
            create_state_db(&shared.join("state_5.sqlite"), &[(BASE_THREAD, &branch)]);
            fs::write(shared.join("session_index.jsonl"), b"").unwrap();
        })
    }

    fn goals_fixture() -> Fixture {
        fixture_with_setup(|home, data| {
            let canonical_goals = home.join("goals_1.sqlite");
            create_goals_db(&canonical_goals);
            insert_goal(&canonical_goals, BASE_THREAD, "canonical-goal", false);

            let shared = data.join("shared-sessions");
            fs::create_dir_all(shared.join("sessions")).unwrap();
            create_state_db(&shared.join("state_5.sqlite"), &[]);
            let shared_goals = shared.join("goals_1.sqlite");
            create_goals_db(&shared_goals);
            insert_goal(&shared_goals, SHARED_GOAL_THREAD, "shared-goal", false);
            fs::write(shared.join("session_index.jsonl"), b"").unwrap();
        })
    }

    fn fixture_with_setup(setup: impl FnOnce(&Path, &Path)) -> Fixture {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let data = root.path().join("data");
        let destination = root.path().join("exports");
        fs::create_dir_all(home.join("sessions/2026/08/12")).unwrap();
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&destination).unwrap();
        fs::write(
            home.join("auth.json"),
            br#"{"auth_mode":"chatgpt","tokens":{"access_token":"fixture-token"}}"#,
        )
        .unwrap();
        fs::write(
            home.join("config.toml"),
            format!(
                "model = \"gpt-test\"\nsqlite_home = \"{}\"\n",
                home.to_string_lossy().replace('\\', "\\\\")
            ),
        )
        .unwrap();
        fs::write(
            home.join("session_index.jsonl"),
            format!("{{\"id\":\"{BASE_THREAD}\",\"thread_name\":\"fixture\"}}\n"),
        )
        .unwrap();
        let baseline = home.join("sessions/2026/08/12/rollout-base.jsonl");
        write_session(&baseline, BASE_THREAD, &["baseline"]);
        create_state_db(&home.join("state_5.sqlite"), &[(BASE_THREAD, &baseline)]);
        setup(&home, &data);
        create_committed_downgrade_certificate(&home, &data);
        let export =
            prepare_downgrade_export(&home, &data, &destination, "downgrade-fixture", "v0.2.7")
                .unwrap();
        let receipt = execute_downgrade_export(&export, || Ok(())).unwrap();
        Fixture {
            _root: root,
            home,
            data,
            package: receipt.package_dir,
        }
    }

    fn create_committed_downgrade_certificate(canonical_root: &Path, data_root: &Path) {
        let operation_id = "migration-restore-import-fixture";
        let backup = tempdir().unwrap();
        let store = OperationLedgerStore::new(data_root);
        store
            .create(
                operation_id,
                SessionStorageOperationKind::Migration,
                canonical_root,
            )
            .unwrap();
        let report =
            run_migration_preflight(canonical_root, data_root, operation_id, backup.path())
                .unwrap();
        assert!(report.ready_for_backup, "{:?}", report.blockers);
        persist_migration_preflight(data_root, &report).unwrap();
        store
            .update(operation_id, |ledger| {
                ledger.backup_root = Some(report.backup_destination.join(operation_id));
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
            data_root,
            canonical_root,
            operation_id,
            &report.plan.inventory_fingerprint,
        )
        .unwrap();
        store
            .transition(operation_id, SessionStorageOperationPhase::Committed)
            .unwrap();
        finalize_canonical_storage_state(data_root, canonical_root, operation_id).unwrap();
    }

    #[test]
    fn imports_new_old_version_session_and_second_run_is_idempotent() {
        let fixture = fixture();
        let package_session = fixture
            .package
            .join("codex-home/sessions/2026/08/13/rollout-new.jsonl");
        fs::create_dir_all(package_session.parent().unwrap()).unwrap();
        write_session(&package_session, NEW_THREAD, &["created on v0.2"]);
        insert_thread(
            &fixture.package.join("codex-home/state_5.sqlite"),
            NEW_THREAD,
            &package_session,
        );

        let prepared = prepare_restore_import(
            &fixture.home,
            &fixture.data,
            &fixture.package,
            "restore-new",
        )
        .unwrap();
        assert_eq!(
            prepared
                .plan
                .sessions
                .iter()
                .find(|session| session.thread_id == NEW_THREAD)
                .unwrap()
                .action,
            RestoreImportSessionAction::ImportNew
        );
        let receipt = execute_restore_import(&prepared.plan, || Ok(())).unwrap();
        assert_eq!(receipt.imported_new_session_count, 1);
        assert!(receipt.validated);
        let imported = thread_rollout(&fixture.home.join("state_5.sqlite"), NEW_THREAD);
        assert!(Path::new(&imported).starts_with(fixture.home.join("sessions")));
        assert!(Path::new(&imported).is_file());

        let second = prepare_restore_import(
            &fixture.home,
            &fixture.data,
            &fixture.package,
            "restore-new-again",
        )
        .unwrap();
        let second_receipt = execute_restore_import(&second.plan, || Ok(())).unwrap();
        assert_eq!(second_receipt.imported_new_session_count, 0);
        assert_eq!(second_receipt.imported_extension_count, 0);
        assert_eq!(second_receipt.unchanged_session_count, 2);
    }

    #[test]
    fn goals_restore_strictly_unions_package_and_runtime_views_then_rolls_back() {
        let fixture = goals_fixture();
        let package_goals = fixture.package.join("codex-home/goals_1.sqlite");
        insert_goal(&package_goals, NEW_THREAD, "package-goal", true);
        let canonical_goals = fixture.home.join("goals_1.sqlite");
        let shared_goals = fixture.data.join("shared-sessions/goals_1.sqlite");

        let prepared = prepare_restore_import(
            &fixture.home,
            &fixture.data,
            &fixture.package,
            "restore-goals-union",
        )
        .unwrap();
        execute_restore_import(&prepared.plan, || Ok(())).unwrap();

        assert!(same_regular_file_identity(&canonical_goals, &shared_goals).unwrap());
        let merged = Connection::open(&canonical_goals).unwrap();
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
                    "SELECT COUNT(*) FROM thread_goal_continuation_deferrals WHERE thread_id = ?1",
                    [NEW_THREAD],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        drop(merged);

        rollback_restore_import(&prepared.plan, || Ok(())).unwrap();
        assert_eq!(goal_count(&canonical_goals), 1);
        assert_eq!(goal_count(&shared_goals), 1);
        assert!(!same_regular_file_identity(&canonical_goals, &shared_goals).unwrap());
    }

    #[test]
    fn declared_package_goals_create_a_missing_runtime_target_and_rollback_removes_it() {
        let fixture = fixture_with_setup(|home, _| {
            let goals = home.join("goals_1.sqlite");
            create_goals_db(&goals);
            insert_goal(&goals, BASE_THREAD, "exported-goal", true);
        });
        let runtime_goals = fixture.home.join("goals_1.sqlite");
        fs::remove_file(&runtime_goals).unwrap();

        let prepared = prepare_restore_import(
            &fixture.home,
            &fixture.data,
            &fixture.package,
            "restore-create-goals",
        )
        .unwrap();
        assert!(prepared
            .plan
            .databases
            .iter()
            .any(super::is_created_goals_database));
        execute_restore_import(&prepared.plan, || Ok(())).unwrap();
        assert_eq!(goal_count(&runtime_goals), 1);
        rollback_restore_import(&prepared.plan, || Ok(())).unwrap();
        assert!(!runtime_goals.exists());
    }

    #[test]
    fn package_without_goals_is_legal_and_does_not_create_a_placeholder() {
        let fixture = fixture();
        let runtime_goals = fixture.home.join("goals_1.sqlite");
        let prepared = prepare_restore_import(
            &fixture.home,
            &fixture.data,
            &fixture.package,
            "restore-goals-absent",
        )
        .unwrap();
        assert!(prepared
            .plan
            .databases
            .iter()
            .all(|database| !database.database_id.starts_with("goals-db-")));
        execute_restore_import(&prepared.plan, || Ok(())).unwrap();
        assert!(!runtime_goals.exists());
    }

    #[test]
    fn package_without_goals_preserves_existing_runtime_goals_exactly() {
        let fixture = fixture();
        let runtime_goals = fixture.home.join("goals_1.sqlite");
        create_goals_db(&runtime_goals);
        insert_goal(&runtime_goals, BASE_THREAD, "runtime-only-goal", true);
        let before = {
            let connection = Connection::open(&runtime_goals).unwrap();
            crate::session_storage::catalog::goals_database_digest(&connection).unwrap()
        };

        let prepared = prepare_restore_import(
            &fixture.home,
            &fixture.data,
            &fixture.package,
            "restore-goals-package-absent-runtime-present",
        )
        .unwrap();
        assert!(prepared
            .plan
            .databases
            .iter()
            .any(|database| database.database_id.starts_with("goals-db-")));
        execute_restore_import(&prepared.plan, || Ok(())).unwrap();
        let after = {
            let connection = Connection::open(&runtime_goals).unwrap();
            crate::session_storage::catalog::goals_database_digest(&connection).unwrap()
        };
        assert_eq!(after, before);
    }

    #[test]
    fn undeclared_package_goals_database_is_rejected() {
        let fixture = fixture();
        let package_goals = fixture.package.join("codex-home/goals_1.sqlite");
        create_goals_db(&package_goals);
        insert_goal(&package_goals, BASE_THREAD, "undeclared-goal", false);
        let error = prepare_restore_import(
            &fixture.home,
            &fixture.data,
            &fixture.package,
            "restore-undeclared-goals",
        )
        .unwrap_err();
        assert!(error.contains("undeclared database"), "{error}");
    }

    #[test]
    fn goals_restore_rejects_same_primary_key_with_different_rows() {
        let fixture = fixture_with_setup(|home, _| {
            let goals = home.join("goals_1.sqlite");
            create_goals_db(&goals);
            insert_goal(&goals, BASE_THREAD, "runtime-goal", false);
        });
        Connection::open(fixture.package.join("codex-home/goals_1.sqlite"))
            .unwrap()
            .execute(
                "UPDATE thread_goals SET objective = 'v0.2 changed the same primary key' WHERE thread_id = ?1",
                [BASE_THREAD],
            )
            .unwrap();
        let error = prepare_restore_import(
            &fixture.home,
            &fixture.data,
            &fixture.package,
            "restore-goals-conflict",
        )
        .unwrap_err();
        assert!(
            error.contains("conflict on the same primary key"),
            "{error}"
        );
    }

    #[test]
    fn declared_package_goals_with_schema_drift_is_rejected() {
        let fixture = fixture_with_setup(|home, _| {
            let goals = home.join("goals_1.sqlite");
            create_goals_db(&goals);
            insert_goal(&goals, BASE_THREAD, "runtime-goal", false);
        });
        let package_goals = fixture.package.join("codex-home/goals_1.sqlite");
        let connection = Connection::open(&package_goals).unwrap();
        connection
            .execute_batch(
                "ALTER TABLE thread_goals RENAME TO drifted_thread_goals;
                 CREATE TABLE thread_goals (thread_id TEXT);",
            )
            .unwrap();
        drop(connection);

        let error = prepare_restore_import(
            &fixture.home,
            &fixture.data,
            &fixture.package,
            "restore-goals-schema-drift",
        )
        .unwrap_err();
        assert!(error.contains("goals"), "{error}");
    }

    #[test]
    fn import_new_uses_identity_witness_until_committed_cleanup() {
        let fixture = fixture();
        let package_session = fixture
            .package
            .join("codex-home/sessions/2026/08/13/rollout-new.jsonl");
        write_session(&package_session, NEW_THREAD, &["created on v0.2"]);
        insert_thread(
            &fixture.package.join("codex-home/state_5.sqlite"),
            NEW_THREAD,
            &package_session,
        );
        let prepared = prepare_restore_import(
            &fixture.home,
            &fixture.data,
            &fixture.package,
            "restore-new-witness",
        )
        .unwrap();
        let imported = prepared
            .plan
            .sessions
            .iter()
            .find(|session| session.thread_id == NEW_THREAD)
            .unwrap();
        let target = imported.canonical_path.clone();
        let witness = ownership_witness_path(&target, &prepared.plan.operation_id).unwrap();

        execute_restore_import(&prepared.plan, || Ok(())).unwrap();
        assert!(target.is_file());
        assert!(witness.is_file());
        assert!(same_regular_file_identity(&target, &witness).unwrap());

        cleanup_committed_restore_import_ownership_witnesses(&prepared.plan).unwrap();
        assert!(target.is_file());
        assert!(!witness.exists());
        cleanup_committed_restore_import_ownership_witnesses(&prepared.plan).unwrap();
    }

    #[test]
    fn preexisting_ownership_witness_blocks_apply_without_touching_it() {
        let fixture = fixture();
        let package_session = fixture
            .package
            .join("codex-home/sessions/2026/08/13/rollout-new.jsonl");
        write_session(&package_session, NEW_THREAD, &["created on v0.2"]);
        insert_thread(
            &fixture.package.join("codex-home/state_5.sqlite"),
            NEW_THREAD,
            &package_session,
        );
        let prepared = prepare_restore_import(
            &fixture.home,
            &fixture.data,
            &fixture.package,
            "restore-witness-precondition",
        )
        .unwrap();
        let imported = prepared
            .plan
            .sessions
            .iter()
            .find(|session| session.thread_id == NEW_THREAD)
            .unwrap();
        let witness =
            ownership_witness_path(&imported.canonical_path, &prepared.plan.operation_id).unwrap();
        fs::create_dir_all(witness.parent().unwrap()).unwrap();
        fs::copy(imported.source_staged_path.as_ref().unwrap(), &witness).unwrap();
        let original = fs::read(&witness).unwrap();
        let mut live_write_called = false;

        let error = execute_restore_import(&prepared.plan, || {
            live_write_called = true;
            Ok(())
        })
        .unwrap_err();
        assert!(error.contains("ownership witness appeared"), "{error}");
        assert!(!live_write_called);
        assert_eq!(fs::read(witness).unwrap(), original);
        assert!(!imported.canonical_path.exists());
    }

    #[test]
    fn atomic_import_new_race_preserves_competing_same_hash_target() {
        let fixture = fixture();
        let package_session = fixture
            .package
            .join("codex-home/sessions/2026/08/13/rollout-new.jsonl");
        write_session(&package_session, NEW_THREAD, &["created on v0.2"]);
        insert_thread(
            &fixture.package.join("codex-home/state_5.sqlite"),
            NEW_THREAD,
            &package_session,
        );
        let prepared = prepare_restore_import(
            &fixture.home,
            &fixture.data,
            &fixture.package,
            "restore-new-atomic-race",
        )
        .unwrap();
        let imported = prepared
            .plan
            .sessions
            .iter()
            .find(|session| session.thread_id == NEW_THREAD)
            .unwrap();
        let target = imported.canonical_path.clone();
        let staged = imported.source_staged_path.clone().unwrap();
        let witness = ownership_witness_path(&target, &prepared.plan.operation_id).unwrap();

        let error = execute_restore_import(&prepared.plan, || {
            if !target.exists() {
                fs::create_dir_all(target.parent().unwrap()).unwrap();
                fs::copy(&staged, &target).unwrap();
            }
            Ok(())
        })
        .unwrap_err();
        assert!(error.contains("appeared concurrently"), "{error}");
        assert_eq!(fs::read(&target).unwrap(), fs::read(&staged).unwrap());
        assert!(!witness.exists());
        assert!(
            thread_rollout_optional(&fixture.home.join("state_5.sqlite"), NEW_THREAD).is_none()
        );
    }

    #[test]
    fn rollback_preserves_same_hash_target_with_different_file_identity() {
        let fixture = fixture();
        let package_session = fixture
            .package
            .join("codex-home/sessions/2026/08/13/rollout-new.jsonl");
        write_session(&package_session, NEW_THREAD, &["created on v0.2"]);
        insert_thread(
            &fixture.package.join("codex-home/state_5.sqlite"),
            NEW_THREAD,
            &package_session,
        );
        let prepared = prepare_restore_import(
            &fixture.home,
            &fixture.data,
            &fixture.package,
            "restore-new-identity-race",
        )
        .unwrap();
        execute_restore_import(&prepared.plan, || Ok(())).unwrap();
        let imported = prepared
            .plan
            .sessions
            .iter()
            .find(|session| session.thread_id == NEW_THREAD)
            .unwrap();
        let target = imported.canonical_path.clone();
        let witness = ownership_witness_path(&target, &prepared.plan.operation_id).unwrap();
        let bytes = fs::read(&target).unwrap();
        fs::remove_file(&target).unwrap();
        fs::write(&target, &bytes).unwrap();
        assert_eq!(
            super::stable_file_digest(&target).unwrap().1,
            imported.source_sha256
        );
        assert!(!same_regular_file_identity(&target, &witness).unwrap());

        let error = rollback_restore_import(&prepared.plan, || Ok(())).unwrap_err();
        assert!(error.contains("changed after apply"), "{error}");
        assert_eq!(fs::read(&target).unwrap(), bytes);
        assert!(witness.is_file());
        assert!(
            thread_rollout_optional(&fixture.home.join("state_5.sqlite"), NEW_THREAD).is_some()
        );
    }

    #[test]
    fn imports_only_a_complete_old_version_extension() {
        let fixture = fixture();
        let package_session = fixture
            .package
            .join("codex-home/sessions/2026/08/12/rollout-base.jsonl");
        append_session_message(&package_session, "continued on v0.2");
        let prepared = prepare_restore_import(
            &fixture.home,
            &fixture.data,
            &fixture.package,
            "restore-extension",
        )
        .unwrap();
        let session = prepared
            .plan
            .sessions
            .iter()
            .find(|session| session.thread_id == BASE_THREAD)
            .unwrap();
        assert_eq!(session.action, RestoreImportSessionAction::ImportExtension);
        execute_restore_import(&prepared.plan, || Ok(())).unwrap();
        assert_eq!(
            fs::read_to_string(&session.canonical_path)
                .unwrap()
                .matches("user_message")
                .count(),
            2
        );
    }

    #[test]
    fn replacement_plan_persists_deterministic_target_local_paths() {
        let fixture = fixture();
        let package_session = fixture
            .package
            .join("codex-home/sessions/2026/08/12/rollout-base.jsonl");
        append_session_message(&package_session, "continued on v0.2");
        let prepared = prepare_restore_import(
            &fixture.home,
            &fixture.data,
            &fixture.package,
            "restore-deterministic-replacement-paths",
        )
        .unwrap();
        let expected = build_restore_import_replacements(
            &prepared.plan.operation_id,
            &prepared.plan.sessions,
            &prepared.plan.databases,
        )
        .unwrap();
        assert!(!expected.is_empty());
        for replacement in expected
            .iter()
            .filter(|replacement| replacement.kind == RestoreImportReplacementKind::RuntimeDatabase)
        {
            let database = prepared
                .plan
                .databases
                .iter()
                .find(|database| {
                    path_key(&database.target_path) == path_key(&replacement.target_path)
                })
                .unwrap();
            assert_eq!(
                replacement.live_original_sha256,
                super::stable_file_digest(&database.target_path).unwrap().1
            );
            assert_eq!(
                replacement.rollback_snapshot_sha256,
                database.original_sha256
            );
            assert_eq!(
                super::stable_file_digest(&database.original_backup_payload)
                    .unwrap()
                    .1,
                replacement.rollback_snapshot_sha256
            );
        }
        let loaded_phases =
            super::load_restore_import_replacement_phases(&prepared.plan, &expected).unwrap();
        assert!(loaded_phases
            .replacements
            .iter()
            .all(|entry| entry.phase == RestoreImportReplacementPhase::Planned));
        assert_eq!(
            expected,
            build_restore_import_replacements(
                &prepared.plan.operation_id,
                &prepared.plan.sessions,
                &prepared.plan.databases,
            )
            .unwrap()
        );
        let envelope: serde_json::Value = serde_json::from_slice(
            &fs::read(super::plan_path(&fixture.data, &prepared.plan.operation_id).unwrap())
                .unwrap(),
        )
        .unwrap();
        let persisted = serde_json::from_value::<Vec<RestoreImportReplacementPlan>>(
            envelope.get("replacements").cloned().unwrap(),
        )
        .unwrap();
        assert_eq!(persisted, expected);

        let mut paths = std::collections::BTreeSet::new();
        for replacement in &persisted {
            {
                let path = &replacement.original_witness_path;
                assert_eq!(path.parent(), replacement.target_path.parent());
                assert!(paths.insert(path_key(path)));
                assert!(path.is_file());
                assert!(same_regular_file_identity(path, &replacement.target_path).unwrap());
            }
            for path in [
                &replacement.replacement_witness_path,
                &replacement.staging_path,
                &replacement.recovery_path,
                &replacement.tombstone_path,
            ] {
                assert_eq!(path.parent(), replacement.target_path.parent());
                assert!(paths.insert(path_key(path)));
                assert!(!path.exists());
            }
        }
    }

    #[test]
    fn pending_source_database_selection_is_exact_and_does_not_relax_package_scans() {
        let root = tempdir().unwrap();
        let package = root.path().join("pending-package");
        let entry_id = "a".repeat(64);
        let payload = package.join("payloads").join(format!("{entry_id}.jsonl"));
        write_session(&payload, NEW_THREAD, &["pending legacy session"]);
        let declared_database = package.join("source-state.sqlite");
        let original_rollout = root
            .path()
            .join("old-profile/sessions/2026/08/12")
            .join(format!("rollout-{NEW_THREAD}.jsonl"));
        create_state_db(&declared_database, &[(NEW_THREAD, &original_rollout)]);
        let databases = super::snapshot_pending_source_database(
            &declared_database,
            &root.path().join("snapshot"),
        )
        .unwrap();

        assert_eq!(
            super::pending_source_database_for_thread(
                &databases,
                NEW_THREAD,
                &payload,
                &package,
                &declared_database,
                &entry_id,
            )
            .unwrap(),
            Some(databases[0].snapshot_path.clone())
        );
        assert!(
            super::source_database_for_thread(&databases, NEW_THREAD, &payload, &package,)
                .unwrap()
                .is_none()
        );

        let undeclared = package.join("other.sqlite");
        fs::copy(&declared_database, &undeclared).unwrap();
        let mut wrong_shape = databases.clone();
        wrong_shape[0].source_path = undeclared.clone();
        assert!(super::pending_source_database_for_thread(
            &wrong_shape,
            NEW_THREAD,
            &payload,
            &package,
            &undeclared,
            &entry_id,
        )
        .is_err());

        let wrong_snapshot = root.path().join("wrong-rollout.sqlite");
        fs::copy(&databases[0].snapshot_path, &wrong_snapshot).unwrap();
        Connection::open(&wrong_snapshot)
            .unwrap()
            .execute(
                "UPDATE threads SET rollout_path = 'unrelated.jsonl' WHERE id = ?1",
                [NEW_THREAD],
            )
            .unwrap();
        let mut wrong_rollout = databases.clone();
        wrong_rollout[0].snapshot_path = wrong_snapshot.clone();
        wrong_rollout[0].sha256 = super::stable_file_digest(&wrong_snapshot).unwrap().1;
        let error = super::pending_source_database_for_thread(
            &wrong_rollout,
            NEW_THREAD,
            &payload,
            &package,
            &declared_database,
            &entry_id,
        )
        .unwrap_err();
        assert!(error.contains("session path changed"), "{error}");

        fs::write(&databases[0].snapshot_path, b"tampered snapshot").unwrap();
        assert!(super::pending_source_database_for_thread(
            &databases,
            NEW_THREAD,
            &payload,
            &package,
            &declared_database,
            &entry_id,
        )
        .is_err());

        let traversal_snapshot = root.path().join("traversal-rollout.sqlite");
        fs::copy(&wrong_snapshot, &traversal_snapshot).unwrap();
        Connection::open(&traversal_snapshot)
            .unwrap()
            .execute(
                "UPDATE threads SET rollout_path = ?1 WHERE id = ?2",
                (
                    format!("sessions/../rollout-{NEW_THREAD}.jsonl"),
                    NEW_THREAD,
                ),
            )
            .unwrap();
        let mut traversal_rollout = wrong_rollout;
        traversal_rollout[0].snapshot_path = traversal_snapshot.clone();
        traversal_rollout[0].sha256 = super::stable_file_digest(&traversal_snapshot).unwrap().1;
        assert!(super::pending_source_database_for_thread(
            &traversal_rollout,
            NEW_THREAD,
            &payload,
            &package,
            &declared_database,
            &entry_id,
        )
        .is_err());

        let prefix_collision_snapshot = root.path().join("prefix-collision-rollout.sqlite");
        fs::copy(&wrong_snapshot, &prefix_collision_snapshot).unwrap();
        Connection::open(&prefix_collision_snapshot)
            .unwrap()
            .execute(
                "UPDATE threads SET rollout_path = ?1 WHERE id = ?2",
                (
                    format!("sessions/rollout-{NEW_THREAD}-evil.jsonl"),
                    NEW_THREAD,
                ),
            )
            .unwrap();
        let mut prefix_collision = traversal_rollout;
        prefix_collision[0].snapshot_path = prefix_collision_snapshot.clone();
        prefix_collision[0].sha256 = super::stable_file_digest(&prefix_collision_snapshot)
            .unwrap()
            .1;
        assert!(super::pending_source_database_for_thread(
            &prefix_collision,
            NEW_THREAD,
            &payload,
            &package,
            &declared_database,
            &entry_id,
        )
        .is_err());
    }

    #[cfg(windows)]
    #[test]
    fn tampered_or_missing_replacement_phase_record_fails_closed() {
        let fixture = fixture();
        let package_session = fixture
            .package
            .join("codex-home/sessions/2026/08/12/rollout-base.jsonl");
        append_session_message(&package_session, "continued on v0.2");
        let prepared = prepare_restore_import(
            &fixture.home,
            &fixture.data,
            &fixture.package,
            "restore-phase-record-tamper",
        )
        .unwrap();
        let replacements = build_restore_import_replacements(
            &prepared.plan.operation_id,
            &prepared.plan.sessions,
            &prepared.plan.databases,
        )
        .unwrap();
        let phase_path =
            super::replacement_phase_path(&fixture.data, &prepared.plan.operation_id).unwrap();
        let original = fs::read(&phase_path).unwrap();

        fs::remove_file(&phase_path).unwrap();
        let missing = execute_restore_import(&prepared.plan, || Ok(())).unwrap_err();
        assert!(missing.contains("phase record"), "{missing}");
        for replacement in &replacements {
            assert_eq!(
                super::stable_file_digest(&replacement.target_path)
                    .unwrap()
                    .1,
                replacement.live_original_sha256
            );
        }

        fs::write(&phase_path, &original).unwrap();
        let mut tampered = original;
        let last = tampered.last_mut().unwrap();
        *last ^= 0x01;
        fs::write(&phase_path, tampered).unwrap();
        let tampered = execute_restore_import(&prepared.plan, || Ok(())).unwrap_err();
        assert!(tampered.contains("phase record"), "{tampered}");
        for replacement in &replacements {
            assert_eq!(
                super::stable_file_digest(&replacement.target_path)
                    .unwrap()
                    .1,
                replacement.live_original_sha256
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn rollback_rejects_equal_hash_recovery_with_a_different_identity() {
        let root = tempdir().unwrap();
        let (replacement, before, _) =
            raw_replacement(root.path(), "restore-old-identity-contender");
        create_replacement_witness(&replacement);
        let guard = WriteExclusionGuard::acquire(&replacement.target_path).unwrap();
        let prepared = guard
            .prepare_handle_hardlink_replace(
                &replacement.replacement_witness_path,
                &replacement.replacement_sha256,
                &typed_replacement_paths(&replacement),
            )
            .unwrap();
        drop(prepared);
        fs::remove_file(&replacement.recovery_path).unwrap();
        fs::write(&replacement.recovery_path, &before).unwrap();
        assert!(!same_regular_file_identity(
            &replacement.recovery_path,
            &replacement.original_witness_path
        )
        .unwrap());

        let error = rollback_raw_replacement(&replacement, RestoreImportReplacementPhase::Prepared)
            .unwrap_err();

        assert!(
            error.contains("physical replacement layout is unknown"),
            "{error}"
        );
        assert!(!replacement.target_path.exists());
        assert_eq!(fs::read(&replacement.recovery_path).unwrap(), before);
    }

    #[cfg(windows)]
    #[test]
    fn startup_rollback_recovers_the_prepared_replacement_window() {
        let root = tempdir().unwrap();
        let (replacement, before, _) = raw_replacement(root.path(), "restore-prepared-window");
        create_replacement_witness(&replacement);
        let guard = WriteExclusionGuard::acquire(&replacement.target_path).unwrap();
        let prepared = guard
            .prepare_handle_hardlink_replace(
                &replacement.replacement_witness_path,
                &replacement.replacement_sha256,
                &typed_replacement_paths(&replacement),
            )
            .unwrap();
        drop(prepared);
        assert!(!replacement.target_path.exists());
        assert!(replacement.recovery_path.is_file());
        assert!(replacement.staging_path.is_file());

        rollback_raw_replacement(&replacement, RestoreImportReplacementPhase::Prepared).unwrap();

        assert_eq!(fs::read(&replacement.target_path).unwrap(), before);
        assert_replacement_artifacts_absent(&replacement);
    }

    #[cfg(windows)]
    #[test]
    fn startup_rollback_recovers_the_staged_replacement_window() {
        let root = tempdir().unwrap();
        let (replacement, before, _) = raw_replacement(root.path(), "restore-staged-window");
        let staged = WriteExclusionGuard::acquire(&replacement.target_path)
            .unwrap()
            .stage_handle_replace(
                &replacement.source_path,
                &replacement.replacement_sha256,
                &typed_replacement_paths(&replacement),
            )
            .unwrap();
        drop(staged);
        assert!(replacement.target_path.is_file());
        assert!(replacement.staging_path.is_file());
        assert!(!replacement.recovery_path.exists());

        rollback_raw_replacement(&replacement, RestoreImportReplacementPhase::Staged).unwrap();

        assert_eq!(fs::read(&replacement.target_path).unwrap(), before);
        assert_replacement_artifacts_absent(&replacement);
    }

    #[cfg(windows)]
    #[test]
    fn startup_rollback_recovers_the_published_replacement_window() {
        let root = tempdir().unwrap();
        let (replacement, before, after) = raw_replacement(root.path(), "restore-published-window");
        create_replacement_witness(&replacement);
        let guard = WriteExclusionGuard::acquire(&replacement.target_path).unwrap();
        let published = guard
            .prepare_handle_hardlink_replace(
                &replacement.replacement_witness_path,
                &replacement.replacement_sha256,
                &typed_replacement_paths(&replacement),
            )
            .unwrap()
            .publish()
            .unwrap();
        drop(published);
        assert_eq!(fs::read(&replacement.target_path).unwrap(), after);
        assert!(replacement.recovery_path.is_file());
        assert!(!replacement.staging_path.exists());

        rollback_raw_replacement(&replacement, RestoreImportReplacementPhase::Published).unwrap();

        assert_eq!(fs::read(&replacement.target_path).unwrap(), before);
        assert_replacement_artifacts_absent(&replacement);
    }

    #[cfg(windows)]
    #[test]
    fn startup_rollback_recovers_the_committed_with_recovery_window() {
        let root = tempdir().unwrap();
        let (replacement, before, after) = raw_replacement(root.path(), "restore-committed-window");
        create_replacement_witness(&replacement);
        let guard = WriteExclusionGuard::acquire(&replacement.target_path).unwrap();
        let resolved = guard
            .prepare_handle_hardlink_replace(
                &replacement.replacement_witness_path,
                &replacement.replacement_sha256,
                &typed_replacement_paths(&replacement),
            )
            .unwrap()
            .publish()
            .unwrap()
            .commit()
            .unwrap();
        drop(resolved.retain_for_recovery());
        assert_eq!(fs::read(&replacement.target_path).unwrap(), after);
        assert!(replacement.recovery_path.is_file());

        rollback_raw_replacement(
            &replacement,
            RestoreImportReplacementPhase::CommittedWithRecovery,
        )
        .unwrap();

        assert_eq!(fs::read(&replacement.target_path).unwrap(), before);
        assert_replacement_artifacts_absent(&replacement);
    }

    #[cfg(windows)]
    #[test]
    fn delete_access_contender_makes_publish_fail_without_overwrite() {
        use std::{fs::OpenOptions, io::Write, os::windows::fs::OpenOptionsExt};
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };

        let root = tempdir().unwrap();
        let (replacement, before, _) = raw_replacement(root.path(), "restore-delete-contender");
        create_replacement_witness(&replacement);
        let guard = WriteExclusionGuard::acquire(&replacement.target_path).unwrap();
        let prepared = guard
            .prepare_handle_hardlink_replace(
                &replacement.replacement_witness_path,
                &replacement.replacement_sha256,
                &typed_replacement_paths(&replacement),
            )
            .unwrap();
        let contender_bytes = b"delete-access contender\n";
        let mut contender = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(&replacement.target_path)
            .unwrap();
        contender.write_all(contender_bytes).unwrap();
        contender.sync_all().unwrap();

        let (error, prepared) = prepared.publish().unwrap_err();
        assert!(error.contains("quarantine"), "{error}");
        assert_eq!(fs::read(&replacement.target_path).unwrap(), contender_bytes);
        drop(prepared);
        drop(contender);
        fs::remove_file(&replacement.target_path).unwrap();

        rollback_raw_replacement(&replacement, RestoreImportReplacementPhase::Prepared).unwrap();
        assert_eq!(fs::read(&replacement.target_path).unwrap(), before);
        assert_replacement_artifacts_absent(&replacement);
    }

    #[cfg(windows)]
    #[test]
    fn committed_replacement_cleanup_is_exact_and_idempotent() {
        let fixture = fixture();
        let package_session = fixture
            .package
            .join("codex-home/sessions/2026/08/12/rollout-base.jsonl");
        append_session_message(&package_session, "continued on v0.2");
        let prepared = prepare_restore_import(
            &fixture.home,
            &fixture.data,
            &fixture.package,
            "restore-committed-replacement-cleanup",
        )
        .unwrap();
        let replacements = build_restore_import_replacements(
            &prepared.plan.operation_id,
            &prepared.plan.sessions,
            &prepared.plan.databases,
        )
        .unwrap();
        execute_restore_import(&prepared.plan, || Ok(())).unwrap();
        for replacement in &replacements {
            assert_eq!(
                super::stable_file_digest(&replacement.target_path)
                    .unwrap()
                    .1,
                replacement.replacement_sha256
            );
            assert!(replacement.replacement_witness_path.is_file());
            assert!(replacement.recovery_path.is_file());
        }

        cleanup_committed_restore_import_ownership_witnesses(&prepared.plan).unwrap();
        cleanup_committed_restore_import_ownership_witnesses(&prepared.plan).unwrap();

        for replacement in &replacements {
            assert_eq!(
                super::stable_file_digest(&replacement.target_path)
                    .unwrap()
                    .1,
                replacement.replacement_sha256
            );
            assert_replacement_artifacts_absent(replacement);
        }
    }

    #[test]
    fn active_session_hash_may_advance_but_logical_thread_may_not_change() {
        let fixture = fixture();
        let package_session = fixture
            .package
            .join("codex-home/sessions/2026/08/12/rollout-base.jsonl");
        write_session(&package_session, NEW_THREAD, &["forged active thread"]);

        let error = prepare_restore_import(
            &fixture.home,
            &fixture.data,
            &fixture.package,
            "restore-active-thread-drift",
        )
        .unwrap_err();
        assert!(error.contains("session identity changed"), "{error}");
    }

    #[test]
    fn keeps_current_extension_and_preserves_divergence_as_conflict() {
        let current_ahead = fixture();
        let current_session = current_ahead
            .home
            .join("sessions/2026/08/12/rollout-base.jsonl");
        append_session_message(&current_session, "continued on v0.3");
        let keep = prepare_restore_import(
            &current_ahead.home,
            &current_ahead.data,
            &current_ahead.package,
            "restore-current-ahead",
        )
        .unwrap();
        assert_eq!(
            keep.plan.sessions[0].action,
            RestoreImportSessionAction::KeepCanonical
        );

        let divergent = fixture();
        let package_session = divergent
            .package
            .join("codex-home/sessions/2026/08/12/rollout-base.jsonl");
        write_session(&package_session, BASE_THREAD, &["rewritten on v0.2"]);
        let conflict = prepare_restore_import(
            &divergent.home,
            &divergent.data,
            &divergent.package,
            "restore-divergent",
        )
        .unwrap();
        assert_eq!(conflict.plan.conflicts.len(), 1);
        assert!(conflict.plan.conflicts[0]
            .recovery_paths
            .iter()
            .all(|path| path.is_file()));
        let before = fs::read(
            divergent
                .home
                .join("sessions/2026/08/12/rollout-base.jsonl"),
        )
        .unwrap();
        let receipt = execute_restore_import(&conflict.plan, || Ok(())).unwrap();
        assert_eq!(receipt.conflict_count, 1);
        assert_eq!(
            fs::read(
                divergent
                    .home
                    .join("sessions/2026/08/12/rollout-base.jsonl")
            )
            .unwrap(),
            before
        );
    }

    #[test]
    fn retained_sessions_changed_after_planning_are_never_reported_as_validated() {
        for keep_current_ahead in [false, true] {
            let fixture = fixture();
            let canonical = fixture.home.join("sessions/2026/08/12/rollout-base.jsonl");
            if keep_current_ahead {
                append_session_message(&canonical, "current ahead before plan");
            }
            let operation_id = if keep_current_ahead {
                "restore-retained-keep-race"
            } else {
                "restore-retained-unchanged-race"
            };
            let prepared = prepare_restore_import(
                &fixture.home,
                &fixture.data,
                &fixture.package,
                operation_id,
            )
            .unwrap();
            assert!(matches!(
                prepared.plan.sessions[0].action,
                RestoreImportSessionAction::Unchanged | RestoreImportSessionAction::KeepCanonical
            ));
            append_session_message(&canonical, "writer changed retained session after plan");
            let changed = fs::read(&canonical).unwrap();

            let error = execute_restore_import(&prepared.plan, || Ok(())).unwrap_err();
            assert!(
                error.contains("changed") || error.contains("barrier"),
                "{error}"
            );
            assert_eq!(fs::read(&canonical).unwrap(), changed);
        }
    }

    #[test]
    fn source_change_after_plan_is_rejected_before_canonical_write() {
        let fixture = fixture();
        let package_session = fixture
            .package
            .join("codex-home/sessions/2026/08/13/rollout-new.jsonl");
        fs::create_dir_all(package_session.parent().unwrap()).unwrap();
        write_session(&package_session, NEW_THREAD, &["first"]);
        insert_thread(
            &fixture.package.join("codex-home/state_5.sqlite"),
            NEW_THREAD,
            &package_session,
        );
        let prepared = prepare_restore_import(
            &fixture.home,
            &fixture.data,
            &fixture.package,
            "restore-source-race",
        )
        .unwrap();
        write_session(&package_session, NEW_THREAD, &["first", "changed"]);

        let error = execute_restore_import(&prepared.plan, || Ok(())).unwrap_err();
        assert!(error.contains("source session changed"), "{error}");
        assert!(!prepared
            .plan
            .sessions
            .iter()
            .find(|session| session.thread_id == NEW_THREAD)
            .unwrap()
            .canonical_path
            .exists());
    }

    #[test]
    fn rollback_restores_extension_and_database_snapshot() {
        let fixture = fixture();
        let current_session = fixture.home.join("sessions/2026/08/12/rollout-base.jsonl");
        let before = fs::read(&current_session).unwrap();
        let package_session = fixture
            .package
            .join("codex-home/sessions/2026/08/12/rollout-base.jsonl");
        append_session_message(&package_session, "continued on v0.2");
        let prepared = prepare_restore_import(
            &fixture.home,
            &fixture.data,
            &fixture.package,
            "restore-rollback",
        )
        .unwrap();
        execute_restore_import(&prepared.plan, || Ok(())).unwrap();
        rollback_restore_import(&prepared.plan, || Ok(())).unwrap();
        assert_eq!(fs::read(&current_session).unwrap(), before);
        assert_eq!(
            thread_rollout(&fixture.home.join("state_5.sqlite"), BASE_THREAD),
            current_session.to_string_lossy()
        );
    }

    #[test]
    fn three_incomparable_package_branches_are_preserved_as_one_conflict() {
        let fixture = fixture();
        let account_session = fixture
            .package
            .join("codex-home/sessions/2026/08/12/rollout-base.jsonl");
        let relay_session = fixture
            .package
            .join("appdata/codex-switch/relay-sqlite/sessions/rollout-relay.jsonl");
        let shared_session = fixture
            .package
            .join("appdata/codex-switch/shared-sessions/sessions/rollout-shared.jsonl");
        fs::create_dir_all(relay_session.parent().unwrap()).unwrap();
        fs::create_dir_all(shared_session.parent().unwrap()).unwrap();
        fs::copy(&account_session, &relay_session).unwrap();
        fs::copy(&account_session, &shared_session).unwrap();
        append_session_message(&relay_session, "relay branch");
        append_session_message(&shared_session, "shared branch");
        let relay_database = fixture
            .package
            .join("appdata/codex-switch/relay-sqlite/state_5.sqlite");
        create_state_db(&relay_database, &[(BASE_THREAD, &relay_session)]);
        upsert_thread(
            &fixture
                .package
                .join("appdata/codex-switch/shared-sessions/state_5.sqlite"),
            BASE_THREAD,
            &shared_session,
        );

        let prepared = prepare_restore_import(
            &fixture.home,
            &fixture.data,
            &fixture.package,
            "restore-three-branches",
        )
        .unwrap();
        assert!(prepared.plan.sessions.is_empty());
        assert_eq!(prepared.plan.conflicts.len(), 1);
        assert_eq!(prepared.plan.conflicts[0].candidate_sha256.len(), 3);
        assert!(prepared.plan.conflicts[0]
            .recovery_paths
            .iter()
            .all(|path| path.is_file()));
    }

    #[test]
    fn referenced_invalid_tool_history_is_preserved_as_unknown_conflict() {
        let fixture = fixture();
        let package_session = fixture
            .package
            .join("codex-home/sessions/2026/08/12/rollout-base.jsonl");
        write_unpaired_tool_session(&package_session, BASE_THREAD);
        let canonical = fixture.home.join("sessions/2026/08/12/rollout-base.jsonl");
        let before = fs::read(&canonical).unwrap();

        let prepared = prepare_restore_import(
            &fixture.home,
            &fixture.data,
            &fixture.package,
            "restore-invalid-tool",
        )
        .unwrap();
        assert!(prepared.plan.sessions.is_empty());
        assert_eq!(prepared.plan.conflicts.len(), 1);
        assert_eq!(
            prepared.plan.conflicts[0].relation,
            crate::session_storage::model::SessionRelation::Unknown
        );
        assert!(prepared.plan.anomaly_count >= 1);
        assert!(prepared.plan.conflicts[0]
            .recovery_paths
            .iter()
            .all(|path| path.is_file()));
        let receipt = execute_restore_import(&prepared.plan, || Ok(())).unwrap();
        assert_eq!(receipt.conflict_count, 1);
        assert_eq!(fs::read(canonical).unwrap(), before);
    }

    #[test]
    fn valid_jsonl_without_any_database_reference_is_recovered_as_a_new_canonical_session() {
        let fixture = fixture();
        let orphan = fixture
            .package
            .join("codex-home/sessions/2026/08/13/rollout-unreferenced.jsonl");
        fs::create_dir_all(orphan.parent().unwrap()).unwrap();
        write_session(&orphan, NEW_THREAD, &["valid but database row is missing"]);

        let prepared = prepare_restore_import(
            &fixture.home,
            &fixture.data,
            &fixture.package,
            "restore-valid-unreferenced",
        )
        .unwrap();
        let recovered = prepared
            .plan
            .sessions
            .iter()
            .find(|session| session.thread_id == NEW_THREAD)
            .expect("a unique valid orphan must be recoverable without manual SQLite edits");
        assert_eq!(recovered.action, RestoreImportSessionAction::ImportNew);
        assert!(recovered
            .source_staged_path
            .as_ref()
            .is_some_and(|path| path.is_file()));
        assert!(prepared
            .plan
            .conflicts
            .iter()
            .all(|conflict| conflict.thread_id != NEW_THREAD));
        assert!(prepared.plan.anomaly_count >= 1);

        let receipt = execute_restore_import(&prepared.plan, || Ok(())).unwrap();
        assert!(receipt.validated);
        assert_eq!(receipt.imported_new_session_count, 1);
        let canonical = prepared
            .plan
            .sessions
            .iter()
            .find(|session| session.thread_id == NEW_THREAD)
            .unwrap()
            .canonical_path
            .clone();
        let semantic = read_semantic_session(&canonical).unwrap();
        assert_eq!(semantic.thread_id, NEW_THREAD);
        for database in &prepared.plan.databases {
            let connection = Connection::open(&database.target_path).unwrap();
            let rollout_path = connection
                .query_row(
                    "SELECT rollout_path FROM threads WHERE id = ?1",
                    [NEW_THREAD],
                    |row| row.get::<_, String>(0),
                )
                .unwrap();
            assert_eq!(path_key(Path::new(&rollout_path)), path_key(&canonical));
        }
        assert!(orphan.is_file());
    }

    #[test]
    fn downgrade_conflict_branch_round_trip_preserves_declared_branch_bytes() {
        let fixture = conflict_fixture();
        let branch = manifest_conflict_path(&fixture.package);
        let branch_bytes = fs::read(&branch).unwrap();
        let branch_sha256 = super::hex_sha256(Sha256::digest(&branch_bytes));

        let prepared = prepare_restore_import(
            &fixture.home,
            &fixture.data,
            &fixture.package,
            "restore-real-conflict-roundtrip",
        )
        .unwrap();
        let conflict = prepared
            .plan
            .conflicts
            .iter()
            .find(|conflict| conflict.thread_id == BASE_THREAD)
            .unwrap();
        let retained = conflict
            .candidate_sha256
            .iter()
            .position(|sha256| sha256 == &branch_sha256)
            .map(|index| &conflict.recovery_paths[index])
            .unwrap();
        assert_eq!(fs::read(retained).unwrap(), branch_bytes);

        let receipt = execute_restore_import(&prepared.plan, || Ok(())).unwrap();
        assert!(receipt.validated);
        assert_eq!(receipt.conflict_count, 1);
        assert!(retained.is_file());
    }

    #[test]
    fn damaged_manifest_conflict_branch_is_retained_under_declared_thread() {
        let fixture = conflict_fixture();
        let branch = manifest_conflict_path(&fixture.package);
        let damaged = b"{this is not complete jsonl\n".to_vec();
        fs::write(&branch, &damaged).unwrap();

        let prepared = prepare_restore_import(
            &fixture.home,
            &fixture.data,
            &fixture.package,
            "restore-damaged-conflict",
        )
        .unwrap();
        assert!(prepared.plan.sessions.is_empty());
        let conflict = prepared
            .plan
            .conflicts
            .iter()
            .find(|conflict| conflict.thread_id == BASE_THREAD)
            .unwrap();
        let retained = conflict
            .candidate_paths
            .iter()
            .position(|path| path == &branch)
            .map(|index| &conflict.recovery_paths[index])
            .unwrap();
        assert_eq!(fs::read(retained).unwrap(), damaged);
        let receipt = execute_restore_import(&prepared.plan, || Ok(())).unwrap();
        assert_eq!(receipt.conflict_count, 1);
    }

    #[test]
    fn receipt_counts_every_valid_unknown_payload_for_one_conflict_thread() {
        let fixture = fixture();
        let first_relative = Path::new("recovery/conflicts/first-valid-unreferenced.jsonl");
        let first = fixture.package.join(first_relative);
        write_session(&first, NEW_THREAD, &["valid unreferenced branch one"]);
        let first_bytes = fs::read(&first).unwrap();

        let second_relative = Path::new("recovery/conflicts/second-valid-unreferenced.jsonl");
        let second = fixture.package.join(second_relative);
        write_session(&second, NEW_THREAD, &["valid unreferenced branch two"]);
        let second_bytes = fs::read(&second).unwrap();

        let mut manifest = downgrade_manifest(&fixture.package);
        manifest.package_bytes = manifest
            .package_bytes
            .saturating_add(first_bytes.len() as u64)
            .saturating_add(second_bytes.len() as u64);
        for (relative_path, bytes) in [
            (first_relative, first_bytes.as_slice()),
            (second_relative, second_bytes.as_slice()),
        ] {
            manifest.entries.push(DowngradePackageEntry {
                relative_path: relative_path.to_path_buf(),
                kind: DowngradePackageFileKind::ConflictBranch,
                bytes: bytes.len() as u64,
                sha256: super::hex_sha256(Sha256::digest(bytes)),
                logical_thread_id: Some(NEW_THREAD.to_string()),
            });
        }
        manifest.conflict_branch_count = manifest.conflict_branch_count.saturating_add(2);
        manifest
            .entries
            .sort_by_key(|entry| entry.relative_path.clone());
        write_downgrade_manifest(&fixture.package, &manifest);

        let prepared = prepare_restore_import(
            &fixture.home,
            &fixture.data,
            &fixture.package,
            "restore-multiple-unknown-payloads",
        )
        .unwrap();
        let conflict = prepared
            .plan
            .conflicts
            .iter()
            .find(|conflict| conflict.thread_id == NEW_THREAD)
            .unwrap();
        assert_eq!(conflict.candidate_paths.len(), 2);
        for (candidate, recovery, sha256) in conflict
            .candidate_paths
            .iter()
            .zip(&conflict.recovery_paths)
            .zip(&conflict.candidate_sha256)
            .map(|((candidate, recovery), sha256)| (candidate, recovery, sha256))
        {
            assert_eq!(fs::read(candidate).unwrap(), fs::read(recovery).unwrap());
            assert_eq!(
                super::stable_file_digest(recovery).unwrap().1,
                sha256.clone()
            );
            assert_eq!(
                super::read_semantic_session(recovery).unwrap().thread_id,
                NEW_THREAD
            );
        }

        let receipt = execute_restore_import(&prepared.plan, || Ok(())).unwrap();
        assert_eq!(receipt.conflict_count, 1);
        assert_eq!(
            receipt
                .scanned_session_count
                .saturating_sub(prepared.plan.sessions.len()),
            2
        );
    }

    #[test]
    fn conflict_payload_semantic_thread_drift_never_imports_the_fake_thread() {
        let fixture = conflict_fixture();
        let branch = manifest_conflict_path(&fixture.package);
        write_session(&branch, NEW_THREAD, &["forged recovery thread"]);
        let forged = fs::read(&branch).unwrap();

        let prepared = prepare_restore_import(
            &fixture.home,
            &fixture.data,
            &fixture.package,
            "restore-conflict-thread-drift",
        )
        .unwrap();
        assert!(prepared
            .plan
            .sessions
            .iter()
            .all(|session| session.thread_id != NEW_THREAD));
        let conflict = prepared
            .plan
            .conflicts
            .iter()
            .find(|conflict| conflict.thread_id == BASE_THREAD)
            .unwrap();
        let retained = conflict
            .candidate_paths
            .iter()
            .position(|path| path == &branch)
            .map(|index| &conflict.recovery_paths[index])
            .unwrap();
        assert_eq!(fs::read(retained).unwrap(), forged);
    }

    #[test]
    fn declared_recovery_payloads_are_explicitly_retained_without_importing_fake_threads() {
        let fixture = fixture();
        let parseable_relative = Path::new("recovery/unclassified/parseable.jsonl");
        let parseable = fixture._root.path().join("parseable-source.jsonl");
        write_session(&parseable, NEW_THREAD, &["must remain unclassified"]);
        let parseable_bytes = fs::read(&parseable).unwrap();
        let parseable_source =
            add_recovery_payload(&fixture.package, parseable_relative, &parseable_bytes);
        let damaged_relative = Path::new("recovery/unclassified/damaged.jsonl");
        let damaged_bytes = b"invalid recovery payload\n";
        let damaged_source =
            add_recovery_payload(&fixture.package, damaged_relative, damaged_bytes);

        let prepared = prepare_restore_import(
            &fixture.home,
            &fixture.data,
            &fixture.package,
            "restore-unclassified-payloads",
        )
        .unwrap();
        assert!(prepared
            .plan
            .sessions
            .iter()
            .all(|session| session.thread_id != NEW_THREAD));
        assert_eq!(prepared.plan.unclassified_payloads.len(), 2);
        for payload in &prepared.plan.unclassified_payloads {
            assert_eq!(
                fs::read(&payload.source_path).unwrap(),
                fs::read(&payload.recovery_path).unwrap()
            );
        }
        assert!(prepared
            .plan
            .unclassified_payloads
            .iter()
            .any(|payload| payload.source_path == parseable_source));
        assert!(prepared
            .plan
            .unclassified_payloads
            .iter()
            .any(|payload| payload.source_path == damaged_source));

        let receipt = execute_restore_import(&prepared.plan, || Ok(())).unwrap();
        assert!(receipt.validated);
        assert_eq!(receipt.unclassified_recovery_count, 2);
        assert_eq!(
            receipt.unclassified_recovery_bytes,
            (parseable_bytes.len() + damaged_bytes.len()) as u64
        );
        assert_eq!(receipt.unclassified_recovery_paths.len(), 2);
        assert!(receipt
            .unclassified_recovery_paths
            .iter()
            .all(|path| !path.is_absolute()));
        super::cleanup_restore_import_staging(&prepared.plan).unwrap();
        assert!(prepared
            .plan
            .unclassified_payloads
            .iter()
            .all(|payload| payload.recovery_path.is_file()));
    }

    #[test]
    fn unmanifested_recovery_jsonl_fails_closed_instead_of_synthetic_import() {
        let fixture = fixture();
        let payload = fixture
            .package
            .join("recovery/unclassified/unmanifested.jsonl");
        write_session(&payload, NEW_THREAD, &["must not become canonical"]);

        let error = prepare_restore_import(
            &fixture.home,
            &fixture.data,
            &fixture.package,
            "restore-unmanifested-recovery",
        )
        .unwrap_err();
        assert!(error.contains("unmanifested downgrade recovery"), "{error}");
        assert!(
            thread_rollout_optional(&fixture.home.join("state_5.sqlite"), NEW_THREAD).is_none()
        );
    }

    #[test]
    fn missing_manifest_recovery_payload_fails_closed() {
        let fixture = fixture();
        let payload = add_recovery_payload(
            &fixture.package,
            Path::new("recovery/unclassified/missing.jsonl"),
            b"declared then removed\n",
        );
        fs::remove_file(payload).unwrap();

        let error = prepare_restore_import(
            &fixture.home,
            &fixture.data,
            &fixture.package,
            "restore-missing-declared-recovery",
        )
        .unwrap_err();
        assert!(
            error.contains("declared downgrade recovery payload"),
            "{error}"
        );
    }

    #[test]
    fn rollback_refuses_to_overwrite_session_or_database_changes_after_apply() {
        let session_fixture = fixture();
        let package_session = session_fixture
            .package
            .join("codex-home/sessions/2026/08/12/rollout-base.jsonl");
        append_session_message(&package_session, "continued on v0.2");
        let session_prepared = prepare_restore_import(
            &session_fixture.home,
            &session_fixture.data,
            &session_fixture.package,
            "restore-session-late-write",
        )
        .unwrap();
        execute_restore_import(&session_prepared.plan, || Ok(())).unwrap();
        let canonical = session_fixture
            .home
            .join("sessions/2026/08/12/rollout-base.jsonl");
        append_session_message(&canonical, "writer appended after apply");
        let late_session = fs::read(&canonical).unwrap();
        let session_database = session_fixture.home.join("state_5.sqlite");
        let applied_database = fs::read(&session_database).unwrap();
        let session_phase_path = super::replacement_phase_path(
            &session_prepared.plan.data_root,
            &session_prepared.plan.operation_id,
        )
        .unwrap();
        let session_phase_before = fs::read(&session_phase_path).unwrap();
        let error = rollback_restore_import(&session_prepared.plan, || Ok(())).unwrap_err();
        assert!(error.contains("changed after apply"), "{error}");
        assert_eq!(fs::read(&canonical).unwrap(), late_session);
        assert_eq!(fs::read(&session_database).unwrap(), applied_database);
        assert_eq!(fs::read(&session_phase_path).unwrap(), session_phase_before);

        let database_fixture = fixture();
        let package_session = database_fixture
            .package
            .join("codex-home/sessions/2026/08/12/rollout-base.jsonl");
        append_session_message(&package_session, "continued on v0.2");
        let database_prepared = prepare_restore_import(
            &database_fixture.home,
            &database_fixture.data,
            &database_fixture.package,
            "restore-database-late-write",
        )
        .unwrap();
        execute_restore_import(&database_prepared.plan, || Ok(())).unwrap();
        let database = database_fixture.home.join("state_5.sqlite");
        Connection::open(&database)
            .unwrap()
            .execute(
                "UPDATE threads SET model_provider = 'writer-after-apply' WHERE id = ?1",
                [BASE_THREAD],
            )
            .unwrap();
        let late_database = fs::read(&database).unwrap();
        let applied_session = fs::read(
            database_fixture
                .home
                .join("sessions/2026/08/12/rollout-base.jsonl"),
        )
        .unwrap();
        let database_phase_path = super::replacement_phase_path(
            &database_prepared.plan.data_root,
            &database_prepared.plan.operation_id,
        )
        .unwrap();
        let database_phase_before = fs::read(&database_phase_path).unwrap();
        let error = rollback_restore_import(&database_prepared.plan, || Ok(())).unwrap_err();
        assert!(error.contains("changed after apply"), "{error}");
        assert_eq!(fs::read(&database).unwrap(), late_database);
        assert_eq!(
            fs::read(
                database_fixture
                    .home
                    .join("sessions/2026/08/12/rollout-base.jsonl")
            )
            .unwrap(),
            applied_session
        );
        assert_eq!(
            fs::read(&database_phase_path).unwrap(),
            database_phase_before
        );
        assert_eq!(
            Connection::open(&database)
                .unwrap()
                .query_row(
                    "SELECT model_provider FROM threads WHERE id = ?1",
                    [BASE_THREAD],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "writer-after-apply"
        );
    }

    #[test]
    fn source_database_change_after_plan_is_rejected_before_canonical_write() {
        let fixture = fixture();
        let package_session = fixture
            .package
            .join("codex-home/sessions/2026/08/13/rollout-new.jsonl");
        fs::create_dir_all(package_session.parent().unwrap()).unwrap();
        write_session(&package_session, NEW_THREAD, &["created on v0.2"]);
        let package_database = fixture.package.join("codex-home/state_5.sqlite");
        insert_thread(&package_database, NEW_THREAD, &package_session);
        let prepared = prepare_restore_import(
            &fixture.home,
            &fixture.data,
            &fixture.package,
            "restore-source-database-race",
        )
        .unwrap();
        Connection::open(&package_database)
            .unwrap()
            .execute(
                "UPDATE threads SET model_provider = 'openai_custom' WHERE id = ?1",
                [NEW_THREAD],
            )
            .unwrap();

        let error = execute_restore_import(&prepared.plan, || Ok(())).unwrap_err();
        assert!(error.contains("source database changed"), "{error}");
        let target = prepared
            .plan
            .sessions
            .iter()
            .find(|session| session.thread_id == NEW_THREAD)
            .unwrap()
            .canonical_path
            .clone();
        assert!(!target.exists());
    }

    #[test]
    fn startup_recovery_defers_for_writer_then_rolls_back_applied_import() {
        let fixture = fixture();
        let operation_id = "restore-startup-recovery";
        let store = OperationLedgerStore::new(&fixture.data);
        store
            .create(
                operation_id,
                SessionStorageOperationKind::RestoreImport,
                &fixture.home,
            )
            .unwrap();
        store
            .transition(operation_id, SessionStorageOperationPhase::Preflight)
            .unwrap();
        store
            .transition(operation_id, SessionStorageOperationPhase::Backup)
            .unwrap();
        let current_session = fixture.home.join("sessions/2026/08/12/rollout-base.jsonl");
        let before = fs::read(&current_session).unwrap();
        let package_session = fixture
            .package
            .join("codex-home/sessions/2026/08/12/rollout-base.jsonl");
        append_session_message(&package_session, "continued before crash");
        let prepared =
            prepare_restore_import(&fixture.home, &fixture.data, &fixture.package, operation_id)
                .unwrap();
        store
            .transition(operation_id, SessionStorageOperationPhase::BackupVerified)
            .unwrap();
        store
            .transition(operation_id, SessionStorageOperationPhase::PlanReady)
            .unwrap();
        store
            .transition(operation_id, SessionStorageOperationPhase::Applying)
            .unwrap();
        store
            .update(operation_id, |ledger| {
                ledger.live_mutation_started = true;
                Ok(())
            })
            .unwrap();
        execute_restore_import(&prepared.plan, || Ok(())).unwrap();
        assert_ne!(fs::read(&current_session).unwrap(), before);

        let deferred =
            recover_interrupted_restore_import(&store, &fixture.data, operation_id, || {
                Err("writer active".to_string())
            })
            .unwrap();
        assert_eq!(deferred, RestoreImportRecoveryStatus::DeferredByLiveWriter);
        let recovered =
            recover_interrupted_restore_import(&store, &fixture.data, operation_id, || Ok(()))
                .unwrap();
        assert_eq!(recovered, RestoreImportRecoveryStatus::RolledBack);
        assert_eq!(fs::read(&current_session).unwrap(), before);
        assert_eq!(
            store.load(operation_id).unwrap().phase,
            SessionStorageOperationPhase::RolledBack
        );
        assert!(!prepared.plan.staging_root.exists());
    }

    #[test]
    fn deferred_startup_recovery_never_overwrites_a_late_writer() {
        let fixture = fixture();
        let operation_id = "restore-deferred-late-writer";
        let store = OperationLedgerStore::new(&fixture.data);
        store
            .create(
                operation_id,
                SessionStorageOperationKind::RestoreImport,
                &fixture.home,
            )
            .unwrap();
        for phase in [
            SessionStorageOperationPhase::Preflight,
            SessionStorageOperationPhase::Backup,
        ] {
            store.transition(operation_id, phase).unwrap();
        }
        let package_session = fixture
            .package
            .join("codex-home/sessions/2026/08/12/rollout-base.jsonl");
        append_session_message(&package_session, "continued before crash");
        let prepared =
            prepare_restore_import(&fixture.home, &fixture.data, &fixture.package, operation_id)
                .unwrap();
        for phase in [
            SessionStorageOperationPhase::BackupVerified,
            SessionStorageOperationPhase::PlanReady,
            SessionStorageOperationPhase::Applying,
        ] {
            store.transition(operation_id, phase).unwrap();
        }
        store
            .update(operation_id, |ledger| {
                ledger.live_mutation_started = true;
                Ok(())
            })
            .unwrap();
        execute_restore_import(&prepared.plan, || Ok(())).unwrap();

        let deferred =
            recover_interrupted_restore_import(&store, &fixture.data, operation_id, || {
                Err("writer active".to_string())
            })
            .unwrap();
        assert_eq!(deferred, RestoreImportRecoveryStatus::DeferredByLiveWriter);
        let canonical = fixture.home.join("sessions/2026/08/12/rollout-base.jsonl");
        append_session_message(&canonical, "late writer after deferred recovery");
        let late_write = fs::read(&canonical).unwrap();

        let deferred_for_changed_identity =
            recover_interrupted_restore_import(&store, &fixture.data, operation_id, || Ok(()))
                .unwrap();

        assert_eq!(
            deferred_for_changed_identity,
            RestoreImportRecoveryStatus::DeferredByLiveWriter
        );
        assert_eq!(fs::read(&canonical).unwrap(), late_write);
        assert_eq!(
            store.load(operation_id).unwrap().phase,
            SessionStorageOperationPhase::RollingBack
        );
    }

    #[test]
    fn startup_recovery_removes_owned_preplan_roots_without_a_plan() {
        let fixture = fixture();
        let operation_id = "restore-preplan-recovery";
        let store = OperationLedgerStore::new(&fixture.data);
        store
            .create(
                operation_id,
                SessionStorageOperationKind::RestoreImport,
                &fixture.home,
            )
            .unwrap();
        store
            .transition(operation_id, SessionStorageOperationPhase::Preflight)
            .unwrap();
        store
            .transition(operation_id, SessionStorageOperationPhase::Backup)
            .unwrap();
        let work_root = operation_root(&fixture.data, operation_id).unwrap();
        let staging_root = work_root.join("restore-import-staging");
        let recovery_root = fixture
            .data
            .join("session-storage-v1/restore-import-recovery")
            .join(operation_id);
        create_safe_directory(&staging_root).unwrap();
        create_safe_directory(&recovery_root).unwrap();
        write_work_marker(&staging_root, operation_id, &fixture.package).unwrap();
        write_work_marker(&recovery_root, operation_id, &fixture.package).unwrap();
        fs::write(staging_root.join("partial.tmp"), b"partial").unwrap();
        fs::write(recovery_root.join("partial.tmp"), b"partial").unwrap();

        let recovered =
            recover_interrupted_restore_import(&store, &fixture.data, operation_id, || Ok(()))
                .unwrap();
        assert_eq!(recovered, RestoreImportRecoveryStatus::RolledBack);
        assert!(!staging_root.exists());
        assert!(!recovery_root.exists());
    }

    #[test]
    fn startup_recovery_preserves_unmarked_preplan_residual() {
        let fixture = fixture();
        let operation_id = "restore-preplan-unmarked";
        let store = OperationLedgerStore::new(&fixture.data);
        store
            .create(
                operation_id,
                SessionStorageOperationKind::RestoreImport,
                &fixture.home,
            )
            .unwrap();
        store
            .transition(operation_id, SessionStorageOperationPhase::Preflight)
            .unwrap();
        store
            .transition(operation_id, SessionStorageOperationPhase::Backup)
            .unwrap();
        let staging_root = operation_root(&fixture.data, operation_id)
            .unwrap()
            .join("restore-import-staging");
        create_safe_directory(&staging_root).unwrap();
        fs::write(staging_root.join("unknown.tmp"), b"unknown").unwrap();

        let recovered =
            recover_interrupted_restore_import(&store, &fixture.data, operation_id, || Ok(()))
                .unwrap();
        assert_eq!(recovered, RestoreImportRecoveryStatus::Failed);
        assert!(staging_root.exists());
        assert_eq!(
            store.load(operation_id).unwrap().phase,
            SessionStorageOperationPhase::Backup
        );
    }

    fn downgrade_manifest(package: &Path) -> DowngradeExportManifest {
        super::load_downgrade_manifest_baseline(package).unwrap()
    }

    fn write_downgrade_manifest(package: &Path, manifest: &DowngradeExportManifest) {
        let encoded = serde_json::to_vec(manifest).unwrap();
        let integrity_sha256 = super::hex_sha256(Sha256::digest(encoded));
        let envelope = serde_json::json!({
            "manifest": manifest,
            "integritySha256": integrity_sha256,
        });
        fs::write(
            package.join("manifest.json"),
            serde_json::to_vec_pretty(&envelope).unwrap(),
        )
        .unwrap();
    }

    fn add_recovery_payload(package: &Path, relative: &Path, bytes: &[u8]) -> std::path::PathBuf {
        let path = package.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        let mut manifest = downgrade_manifest(package);
        let sha256 = super::hex_sha256(Sha256::digest(bytes));
        manifest.entries.push(DowngradePackageEntry {
            relative_path: relative.to_path_buf(),
            kind: DowngradePackageFileKind::RecoveryPayload,
            bytes: bytes.len() as u64,
            sha256,
            logical_thread_id: None,
        });
        manifest.recovery_payload_count = manifest.recovery_payload_count.saturating_add(1);
        manifest.package_bytes = manifest
            .package_bytes
            .checked_add(bytes.len() as u64)
            .unwrap();
        manifest
            .entries
            .sort_by_key(|entry| entry.relative_path.clone());
        write_downgrade_manifest(package, &manifest);
        path
    }

    fn manifest_conflict_path(package: &Path) -> std::path::PathBuf {
        let manifest = downgrade_manifest(package);
        package.join(
            &manifest
                .entries
                .iter()
                .find(|entry| entry.kind == DowngradePackageFileKind::ConflictBranch)
                .expect("fixture must export a conflict branch")
                .relative_path,
        )
    }

    fn write_session(path: &Path, thread_id: &str, messages: &[&str]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut lines = vec![serde_json::json!({
            "type":"session_meta",
            "timestamp":"2026-08-12T00:00:00Z",
            "payload":{"id":thread_id,"model_provider":"openai"}
        })];
        lines.extend(messages.iter().enumerate().map(|(index, message)| {
            serde_json::json!({
                "type":"event_msg",
                "timestamp":format!("2026-08-12T00:00:{:02}Z", index + 1),
                "payload":{"type":"user_message","message":message}
            })
        }));
        lines.push(serde_json::json!({
            "type":"response_item",
            "timestamp":"2026-08-12T00:01:00Z",
            "payload":{"type":"message","role":"assistant","content":[]}
        }));
        let body = lines
            .iter()
            .map(|line| serde_json::to_string(line).unwrap())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(path, body).unwrap();
    }

    fn append_session_message(path: &Path, message: &str) {
        let line = serde_json::json!({
            "type":"event_msg",
            "timestamp":"2026-08-12T00:02:00Z",
            "payload":{"type":"user_message","message":message}
        });
        let mut body = fs::read_to_string(path).unwrap();
        body.push_str(&serde_json::to_string(&line).unwrap());
        body.push('\n');
        fs::write(path, body).unwrap();
    }

    fn write_unpaired_tool_session(path: &Path, thread_id: &str) {
        let body = [
            serde_json::json!({
                "type":"session_meta",
                "timestamp":"2026-08-12T00:00:00Z",
                "payload":{"id":thread_id,"model_provider":"openai"}
            }),
            serde_json::json!({
                "type":"response_item",
                "timestamp":"2026-08-12T00:00:01Z",
                "payload":{
                    "type":"function_call",
                    "call_id":"call-without-result",
                    "name":"fixture",
                    "arguments":"{}"
                }
            }),
        ]
        .iter()
        .map(|line| serde_json::to_string(line).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
            + "\n";
        fs::write(path, body).unwrap();
    }

    fn create_state_db(path: &Path, rows: &[(&str, &Path)]) {
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE threads (\
                    id TEXT PRIMARY KEY,\
                    rollout_path TEXT,\
                    model_provider TEXT,\
                    archived INTEGER NOT NULL DEFAULT 0,\
                    updated_at INTEGER,\
                    updated_at_ms INTEGER\
                );",
            )
            .unwrap();
        for (thread_id, rollout) in rows {
            connection
                .execute(
                    "INSERT INTO threads (id, rollout_path, model_provider, archived, updated_at, updated_at_ms) \
                     VALUES (?1, ?2, 'openai', 0, 1, 1000)",
                    (*thread_id, rollout.to_string_lossy().to_string()),
                )
                .unwrap();
        }
    }

    fn create_goals_db(path: &Path) {
        let connection = Connection::open(path).unwrap();
        connection
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

    fn insert_goal(path: &Path, thread_id: &str, goal_id: &str, deferred: bool) {
        let connection = Connection::open(path).unwrap();
        connection
            .execute(
                "INSERT INTO thread_goals
                 (thread_id, goal_id, objective, status, token_budget, tokens_used,
                  time_used_seconds, created_at_ms, updated_at_ms)
                 VALUES (?1, ?2, ?3, 'active', NULL, 0, 0, 1, 2)",
                (thread_id, goal_id, format!("objective-{goal_id}")),
            )
            .unwrap();
        if deferred {
            connection
                .execute(
                    "INSERT INTO thread_goal_continuation_deferrals (thread_id) VALUES (?1)",
                    [thread_id],
                )
                .unwrap();
        }
    }

    fn goal_count(path: &Path) -> i64 {
        Connection::open(path)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM thread_goals", [], |row| row.get(0))
            .unwrap()
    }

    fn insert_thread(database: &Path, thread_id: &str, rollout: &Path) {
        Connection::open(database)
            .unwrap()
            .execute(
                "INSERT INTO threads (id, rollout_path, model_provider, archived, updated_at, updated_at_ms) \
                 VALUES (?1, ?2, 'openai', 0, 1, 1000)",
                (thread_id, rollout.to_string_lossy().to_string()),
            )
            .unwrap();
    }

    fn upsert_thread(database: &Path, thread_id: &str, rollout: &Path) {
        Connection::open(database)
            .unwrap()
            .execute(
                "INSERT INTO threads (id, rollout_path, model_provider, archived, updated_at, updated_at_ms) \
                 VALUES (?1, ?2, 'openai', 0, 1, 1000) \
                 ON CONFLICT(id) DO UPDATE SET rollout_path = excluded.rollout_path",
                (thread_id, rollout.to_string_lossy().to_string()),
            )
            .unwrap();
    }

    fn thread_rollout(database: &Path, thread_id: &str) -> String {
        Connection::open(database)
            .unwrap()
            .query_row(
                "SELECT rollout_path FROM threads WHERE id = ?1",
                [thread_id],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn thread_rollout_optional(database: &Path, thread_id: &str) -> Option<String> {
        Connection::open(database)
            .unwrap()
            .query_row(
                "SELECT rollout_path FROM threads WHERE id = ?1",
                [thread_id],
                |row| row.get(0),
            )
            .ok()
    }
}

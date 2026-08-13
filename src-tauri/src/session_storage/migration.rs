use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

use rusqlite::{Connection, OpenFlags, MAIN_DB};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    codex_paths::resolve_user_codex_paths,
    file_ops::atomic_write,
    operation_log::{operation_id, timestamp_millis},
};

use super::{
    bounded_file::read_regular_file_bounded,
    catalog::{
        discover_database_catalog, snapshot_database_catalog, snapshot_goals_database_catalog,
        CatalogSnapshot, GoalsCatalogSnapshot,
    },
    marker::{inspect_provider_marker, provider_marker_path},
    migration_backup::{MigrationBackupEntryKind, MigrationBackupSource},
    model::{
        DatabaseInput, DatabaseRole, FileObservation, FileObservationState, FileOrigin,
        MarkerStatus, SessionFileInput, SessionRelation, SESSION_STORAGE_SCHEMA_VERSION,
    },
    reference_graph::{
        build_reference_graph, path_key, ReferenceGraphInput, SessionReferenceGraph,
    },
    relation::compare_sessions,
    semantic::{read_semantic_session, SemanticError, SemanticSession},
    shadow_scan::discover_session_files,
};

const PREFLIGHT_OBSERVATION_DELAY: Duration = Duration::from_millis(250);
const BACKUP_RESERVE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const BACKUP_RESERVE_PERCENT: u64 = 15;
const MAX_PREFLIGHT_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const GOALS_DATABASE_PLAN_PREFIX: &str = "goals-db-";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub enum MigrationSafetyBlocker {
    InventoryChanged,
    DatabaseDiscoveryFailed,
    DatabaseSnapshotFailed,
    SessionDiscoveryFailed,
    BackupDestinationUnsafe,
    InsufficientBackupSpace,
    CanonicalTargetCollision,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum MigrationSessionAction {
    KeepCanonical,
    CopyToCanonical,
    ReplaceCanonicalWithExtension,
    Conflict,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MigrationDuplicatePlan {
    pub path: PathBuf,
    pub bytes: u64,
    pub sha256: String,
    pub relation_to_retained: SessionRelation,
    pub marker_status: MarkerStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MigrationSessionPlan {
    pub thread_id: String,
    pub action: MigrationSessionAction,
    pub retained_path: PathBuf,
    pub canonical_path: PathBuf,
    pub retained_bytes: u64,
    pub retained_sha256: String,
    pub retained_message_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_valid_message_at: Option<String>,
    pub duplicates: Vec<MigrationDuplicatePlan>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MigrationConflictPlan {
    pub thread_id: String,
    pub current_path: PathBuf,
    pub candidate_path: PathBuf,
    pub canonical_path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_sha256: Option<String>,
    pub candidate_sha256: String,
    pub current_origin: FileOrigin,
    pub candidate_origin: FileOrigin,
    pub current_marker_status: MarkerStatus,
    pub candidate_marker_status: MarkerStatus,
    pub current_message_count: usize,
    pub candidate_message_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_last_message_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_last_message_at: Option<String>,
    pub current_provider: Option<String>,
    pub candidate_provider: Option<String>,
    pub relation: SessionRelation,
    pub default_overwrite: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MigrationDatabasePlan {
    pub database_id: String,
    pub path: PathBuf,
    pub role: DatabaseRole,
    pub reference_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CanonicalMigrationPlan {
    pub schema_version: u32,
    pub operation_id: String,
    pub generated_at_ms: u128,
    pub canonical_root: PathBuf,
    pub inventory_fingerprint: String,
    pub sessions: Vec<MigrationSessionPlan>,
    pub conflicts: Vec<MigrationConflictPlan>,
    pub databases: Vec<MigrationDatabasePlan>,
    pub unclassified_file_count: usize,
    pub invalid_marker_count: usize,
    pub missing_runtime_reference_count: usize,
    pub mismatched_runtime_reference_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MigrationPreflightReport {
    pub schema_version: u32,
    pub operation_id: String,
    pub generated_at_ms: u128,
    pub canonical_session_count: usize,
    pub session_file_count: usize,
    pub provider_copy_count: usize,
    pub conflict_count: usize,
    pub anomaly_count: usize,
    pub estimated_reclaim_bytes: u64,
    pub backup_source_bytes: u64,
    pub required_backup_bytes: u64,
    pub available_backup_bytes: u64,
    pub backup_destination: PathBuf,
    pub blockers: Vec<MigrationSafetyBlocker>,
    pub ready_for_backup: bool,
    pub plan: CanonicalMigrationPlan,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MigrationPreflightEnvelope {
    report: MigrationPreflightReport,
    integrity_sha256: String,
}

#[derive(Debug, Clone)]
pub(crate) struct MigrationInventoryFile {
    pub(crate) path: PathBuf,
    pub(crate) origin: FileOrigin,
    pub(crate) marker_status: MarkerStatus,
    pub(crate) bytes: u64,
    pub(crate) raw_sha256: String,
    pub(crate) semantic: Result<SemanticSession, SemanticError>,
}

#[derive(Debug, Clone)]
pub(crate) struct MigrationInventory {
    pub(crate) fingerprint: String,
    pub(crate) files: Vec<MigrationInventoryFile>,
    pub(crate) graph: SessionReferenceGraph,
    pub(crate) catalog: CatalogSnapshot,
    pub(crate) goals_catalog: GoalsCatalogSnapshot,
    pub(crate) database_discovery_errors: usize,
    pub(crate) goals_database_discovery_errors: usize,
    pub(crate) session_discovery_errors: usize,
}

pub fn run_migration_preflight(
    codex_home: &Path,
    data_root: &Path,
    operation_id_value: &str,
    backup_destination: &Path,
) -> Result<MigrationPreflightReport, String> {
    validate_operation_id(operation_id_value)?;
    if !codex_home.is_absolute() || !data_root.is_absolute() || !backup_destination.is_absolute() {
        return Err("migration preflight roots must be absolute".to_string());
    }
    let first = collect_inventory(codex_home, data_root)?;
    thread::sleep(PREFLIGHT_OBSERVATION_DELAY);
    let second = collect_inventory(codex_home, data_root)?;
    build_preflight_report(
        codex_home,
        data_root,
        operation_id_value,
        backup_destination,
        &first,
        &second,
    )
}

pub(crate) fn scan_stable_migration_plan(
    codex_home: &Path,
    data_root: &Path,
    operation_id_value: &str,
) -> Result<CanonicalMigrationPlan, String> {
    validate_operation_id(operation_id_value)?;
    if !codex_home.is_absolute() || !data_root.is_absolute() {
        return Err("session conflict scan roots must be absolute".to_string());
    }
    let first = collect_inventory(codex_home, data_root)?;
    thread::sleep(PREFLIGHT_OBSERVATION_DELAY);
    let second = collect_inventory(codex_home, data_root)?;
    if first.fingerprint != second.fingerprint {
        return Err("session inventory changed during conflict scan".to_string());
    }
    if second.database_discovery_errors != 0
        || second.catalog.database_errors != 0
        || second.goals_catalog.errors != 0
        || second.session_discovery_errors != 0
    {
        return Err("session conflict scan could not establish a complete inventory".to_string());
    }
    build_plan(codex_home, operation_id_value, &second).map(|(plan, _)| plan)
}

pub fn persist_migration_preflight(
    data_root: &Path,
    report: &MigrationPreflightReport,
) -> Result<(), String> {
    validate_preflight_report(report)?;
    let path = preflight_path(data_root, &report.operation_id)?;
    let envelope = MigrationPreflightEnvelope {
        report: report.clone(),
        integrity_sha256: preflight_digest(report)?,
    };
    let bytes = serde_json::to_vec_pretty(&envelope)
        .map_err(|_| "failed to serialize migration preflight".to_string())?;
    if bytes.len() as u64 > MAX_PREFLIGHT_BYTES {
        return Err("migration preflight reached its size limit".to_string());
    }
    atomic_write(&path, &bytes)
}

pub fn load_migration_preflight(
    data_root: &Path,
    operation_id_value: &str,
) -> Result<MigrationPreflightReport, String> {
    let path = preflight_path(data_root, operation_id_value)?;
    let metadata = fs::symlink_metadata(&path)
        .map_err(|_| "migration preflight is unavailable".to_string())?;
    if !metadata.is_file()
        || metadata_is_link_or_reparse(&metadata)
        || metadata.len() == 0
        || metadata.len() > MAX_PREFLIGHT_BYTES
    {
        return Err("migration preflight is invalid".to_string());
    }
    let bytes = read_regular_file_bounded(&path, MAX_PREFLIGHT_BYTES)
        .map_err(|_| "migration preflight is unreadable".to_string())?;
    let envelope = serde_json::from_slice::<MigrationPreflightEnvelope>(&bytes)
        .map_err(|_| "migration preflight is invalid".to_string())?;
    validate_preflight_report(&envelope.report)?;
    if envelope.integrity_sha256 != preflight_digest(&envelope.report)? {
        return Err("migration preflight integrity check failed".to_string());
    }
    Ok(envelope.report)
}

pub fn migration_backup_sources_for_preflight(
    codex_home: &Path,
    data_root: &Path,
    report: &MigrationPreflightReport,
) -> Result<Vec<MigrationBackupSource>, String> {
    validate_preflight_report(report)?;
    if report.plan.canonical_root != codex_home {
        return Err("migration canonical root changed after preflight".to_string());
    }
    let current = collect_inventory(codex_home, data_root)?;
    if current.fingerprint != report.plan.inventory_fingerprint {
        return Err("migration inventory changed after preflight".to_string());
    }
    build_backup_sources(codex_home, data_root, &current)
}

fn build_preflight_report(
    codex_home: &Path,
    data_root: &Path,
    operation_id_value: &str,
    backup_destination: &Path,
    first: &MigrationInventory,
    second: &MigrationInventory,
) -> Result<MigrationPreflightReport, String> {
    let mut blockers = BTreeSet::new();
    if first.fingerprint != second.fingerprint {
        blockers.insert(MigrationSafetyBlocker::InventoryChanged);
    }
    if second.database_discovery_errors > 0 {
        blockers.insert(MigrationSafetyBlocker::DatabaseDiscoveryFailed);
    }
    if second.catalog.database_errors > 0 || second.goals_catalog.errors > 0 {
        blockers.insert(MigrationSafetyBlocker::DatabaseSnapshotFailed);
    }
    if second.session_discovery_errors > 0 {
        blockers.insert(MigrationSafetyBlocker::SessionDiscoveryFailed);
    }
    let (plan, target_collision) = build_plan(codex_home, operation_id_value, second)?;
    if target_collision {
        blockers.insert(MigrationSafetyBlocker::CanonicalTargetCollision);
    }
    let sources = build_backup_sources(codex_home, data_root, second)?;
    let backup_source_bytes = estimate_source_bytes(&sources)?;
    let required_backup_bytes = required_backup_bytes(backup_source_bytes);
    let available_backup_bytes =
        match validate_backup_destination(backup_destination, &sources, codex_home, data_root) {
            Ok(()) => available_backup_bytes(backup_destination).unwrap_or(0),
            Err(_) => {
                blockers.insert(MigrationSafetyBlocker::BackupDestinationUnsafe);
                0
            }
        };
    if available_backup_bytes < required_backup_bytes {
        blockers.insert(MigrationSafetyBlocker::InsufficientBackupSpace);
    }

    let provider_copy_count = second
        .files
        .iter()
        .filter(|file| file.marker_status == MarkerStatus::Valid)
        .count();
    let estimated_reclaim_bytes = plan
        .sessions
        .iter()
        .flat_map(|session| &session.duplicates)
        .filter(|duplicate| {
            matches!(
                duplicate.relation_to_retained,
                SessionRelation::Equal
                    | SessionRelation::EqualExceptProvider
                    | SessionRelation::LeftPrefix
            )
        })
        .map(|duplicate| duplicate.bytes)
        .sum();
    let anomaly_count = second
        .files
        .iter()
        .filter(|file| file.semantic.is_err() || file.marker_status == MarkerStatus::Invalid)
        .count()
        .saturating_add(second.catalog.rows_missing_rollout_path)
        .saturating_add(second.graph.summary.missing_runtime_reference_count)
        .saturating_add(second.graph.summary.mismatched_runtime_reference_count);
    let blockers = blockers.into_iter().collect::<Vec<_>>();
    Ok(MigrationPreflightReport {
        schema_version: SESSION_STORAGE_SCHEMA_VERSION,
        operation_id: operation_id_value.to_string(),
        generated_at_ms: timestamp_millis()?,
        canonical_session_count: plan
            .sessions
            .iter()
            .filter(|session| session.action != MigrationSessionAction::Conflict)
            .count(),
        session_file_count: second.files.len(),
        provider_copy_count,
        conflict_count: plan.conflicts.len(),
        anomaly_count,
        estimated_reclaim_bytes,
        backup_source_bytes,
        required_backup_bytes,
        available_backup_bytes,
        backup_destination: backup_destination.to_path_buf(),
        ready_for_backup: blockers.is_empty(),
        blockers,
        plan,
    })
}

pub(crate) fn collect_inventory(
    codex_home: &Path,
    data_root: &Path,
) -> Result<MigrationInventory, String> {
    collect_inventory_with_supplemental_paths(codex_home, data_root, &[])
}

pub(crate) fn collect_inventory_with_supplemental_paths(
    codex_home: &Path,
    data_root: &Path,
    supplemental_paths: &[PathBuf],
) -> Result<MigrationInventory, String> {
    let scan_id = operation_id("session-storage-preflight")?;
    let staging_root = data_root
        .join("session-storage-v1/preflight")
        .join(&scan_id);
    fs::create_dir_all(&staging_root)
        .map_err(|_| "failed to create migration preflight staging".to_string())?;
    let result = collect_inventory_inner(codex_home, data_root, &staging_root, supplemental_paths);
    let cleanup = fs::remove_dir_all(&staging_root);
    match (result, cleanup) {
        (Ok(inventory), Ok(())) => Ok(inventory),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(format!("migration preflight cleanup failed: {error}")),
        (Err(error), Err(cleanup_error)) => Err(format!(
            "{error}; migration preflight cleanup failed: {cleanup_error}"
        )),
    }
}

fn collect_inventory_inner(
    codex_home: &Path,
    data_root: &Path,
    staging_root: &Path,
    supplemental_paths: &[PathBuf],
) -> Result<MigrationInventory, String> {
    let discovery = discover_database_catalog(codex_home, data_root);
    let mut catalog = snapshot_database_catalog(&discovery.descriptors, staging_root);
    let goals_catalog = snapshot_goals_database_catalog(&discovery.goals_descriptors, staging_root);
    let database_digests = match snapshot_database_digests(
        codex_home,
        &discovery.descriptors,
        &discovery.goals_descriptors,
        staging_root,
    ) {
        Ok(digests) => digests,
        Err(_) => {
            catalog.database_errors = catalog.database_errors.saturating_add(1);
            Vec::new()
        }
    };
    let (discovered_files, session_discovery_errors) = discover_session_files(
        codex_home,
        data_root,
        catalog
            .referenced_paths
            .iter()
            .chain(supplemental_paths.iter())
            .map(PathBuf::as_path),
    );
    let mut files = Vec::with_capacity(discovered_files.len());
    for (path, origin) in discovered_files {
        let semantic = read_semantic_session(&path);
        let (bytes, raw_sha256) = match &semantic {
            Ok(semantic) => (semantic.bytes, hex_digest(semantic.raw_sha256)),
            Err(_) => stable_file_digest(&path)?,
        };
        let marker_status = inspect_provider_marker(&path, semantic.as_ref().ok());
        files.push(MigrationInventoryFile {
            path,
            origin,
            marker_status,
            bytes,
            raw_sha256,
            semantic,
        });
    }
    files.sort_by_key(|file| path_key(&file.path));
    let graph_input = ReferenceGraphInput {
        files: files
            .iter()
            .map(|file| SessionFileInput {
                path: file.path.clone(),
                origin: file.origin,
                marker_status: file.marker_status,
                observation: FileObservation {
                    state: FileObservationState::Stable,
                    stable_observations: 1,
                    observed_bytes: Some(file.bytes),
                    last_verified_at_ms: 0,
                },
                semantic: file.semantic.clone(),
            })
            .collect(),
        databases: catalog.databases.clone(),
    };
    let graph = build_reference_graph(&graph_input);
    let fingerprint = inventory_fingerprint(
        &files,
        &catalog,
        &goals_catalog,
        &database_digests,
        discovery.errors.saturating_add(discovery.goals_errors),
        session_discovery_errors,
    )?;
    Ok(MigrationInventory {
        fingerprint,
        files,
        graph,
        catalog,
        goals_catalog,
        database_discovery_errors: discovery.errors.saturating_add(discovery.goals_errors),
        goals_database_discovery_errors: discovery.goals_errors,
        session_discovery_errors,
    })
}

fn build_plan(
    codex_home: &Path,
    operation_id_value: &str,
    inventory: &MigrationInventory,
) -> Result<(CanonicalMigrationPlan, bool), String> {
    let paths = resolve_user_codex_paths(codex_home)?;
    let mut by_thread = BTreeMap::<String, Vec<usize>>::new();
    for (index, file) in inventory.files.iter().enumerate() {
        if canonical_merge_source(file.origin) {
            if let Ok(semantic) = &file.semantic {
                by_thread
                    .entry(semantic.thread_id.clone())
                    .or_default()
                    .push(index);
            }
        }
    }
    let mut sessions = Vec::with_capacity(by_thread.len());
    let mut conflicts = Vec::new();
    let mut target_collision = false;
    for (thread_id, indexes) in by_thread {
        let retained_index = select_merge_retained_candidate(&indexes, inventory);
        let Some(retained_index) = retained_index else {
            continue;
        };
        let retained = inventory.files[retained_index]
            .semantic
            .as_ref()
            .expect("retained inventory file must be semantic");
        let conflicting_indexes = indexes
            .iter()
            .copied()
            .filter(|index| {
                matches!(
                    merge_file_relation(inventory, *index, retained_index),
                    SessionRelation::Divergent
                        | SessionRelation::Unknown
                        | SessionRelation::RightPrefix
                )
            })
            .collect::<Vec<_>>();
        if !conflicting_indexes.is_empty() {
            let current_index = select_unmarked_canonical_index(&indexes, inventory)
                .or_else(|| {
                    indexes
                        .iter()
                        .copied()
                        .filter(|index| inventory.files[*index].origin == FileOrigin::CanonicalHome)
                        .min_by_key(|index| path_key(&inventory.files[*index].path))
                })
                .unwrap_or(retained_index);
            let current = inventory.files[current_index]
                .semantic
                .as_ref()
                .expect("thread group file must be semantic");
            let canonical_path =
                if inventory.files[current_index].origin == FileOrigin::CanonicalHome {
                    current.path.clone()
                } else {
                    generated_canonical_path(&paths.sessions_dir, &thread_id)
                };
            for candidate_index in conflicting_indexes {
                if candidate_index == current_index {
                    continue;
                }
                let candidate = inventory.files[candidate_index]
                    .semantic
                    .as_ref()
                    .expect("thread group file must be semantic");
                conflicts.push(MigrationConflictPlan {
                    thread_id: thread_id.clone(),
                    current_path: current.path.clone(),
                    candidate_path: candidate.path.clone(),
                    canonical_path: canonical_path.clone(),
                    current_sha256: Some(hex_digest(current.raw_sha256)),
                    candidate_sha256: hex_digest(candidate.raw_sha256),
                    current_origin: inventory.files[current_index].origin,
                    candidate_origin: inventory.files[candidate_index].origin,
                    current_marker_status: inventory.files[current_index].marker_status,
                    candidate_marker_status: inventory.files[candidate_index].marker_status,
                    current_message_count: current.message_count,
                    candidate_message_count: candidate.message_count,
                    current_last_message_at: current.last_message_timestamp.clone(),
                    candidate_last_message_at: candidate.last_message_timestamp.clone(),
                    current_provider: current.initial_provider.clone(),
                    candidate_provider: candidate.initial_provider.clone(),
                    relation: compare_sessions(current, candidate),
                    default_overwrite: false,
                });
            }
            sessions.push(MigrationSessionPlan {
                thread_id,
                action: MigrationSessionAction::Conflict,
                retained_path: retained.path.clone(),
                canonical_path,
                retained_bytes: retained.bytes,
                retained_sha256: hex_digest(retained.raw_sha256),
                retained_message_count: retained.message_count,
                last_valid_message_at: retained.last_message_timestamp.clone(),
                duplicates: duplicate_plans(inventory, &indexes, retained_index),
            });
            continue;
        }

        let canonical_index = select_unmarked_canonical_index(&indexes, inventory);
        let canonical_path = match canonical_index {
            Some(index) => inventory.files[index].path.clone(),
            None => generated_canonical_path(&paths.sessions_dir, &thread_id),
        };
        let collision_semantic = if canonical_index.is_none() && canonical_path.exists() {
            read_semantic_session(&canonical_path).ok()
        } else {
            None
        };
        let local_target_collision = canonical_index.is_none()
            && canonical_path.exists()
            && collision_semantic
                .as_ref()
                .is_none_or(|existing| existing.thread_id != thread_id);
        if local_target_collision {
            target_collision = true;
            conflicts.push(MigrationConflictPlan {
                thread_id: thread_id.clone(),
                current_path: canonical_path.clone(),
                candidate_path: retained.path.clone(),
                canonical_path: canonical_path.clone(),
                current_sha256: collision_semantic
                    .as_ref()
                    .map(|existing| hex_digest(existing.raw_sha256)),
                candidate_sha256: hex_digest(retained.raw_sha256),
                current_origin: FileOrigin::CanonicalHome,
                candidate_origin: inventory.files[retained_index].origin,
                current_marker_status: inspect_provider_marker(
                    &canonical_path,
                    collision_semantic.as_ref(),
                ),
                candidate_marker_status: inventory.files[retained_index].marker_status,
                current_message_count: collision_semantic
                    .as_ref()
                    .map_or(0, |existing| existing.message_count),
                candidate_message_count: retained.message_count,
                current_last_message_at: collision_semantic
                    .as_ref()
                    .and_then(|existing| existing.last_message_timestamp.clone()),
                candidate_last_message_at: retained.last_message_timestamp.clone(),
                current_provider: collision_semantic
                    .as_ref()
                    .and_then(|existing| existing.initial_provider.clone()),
                candidate_provider: retained.initial_provider.clone(),
                relation: SessionRelation::Unknown,
                default_overwrite: false,
            });
        }
        let action = if local_target_collision {
            MigrationSessionAction::Conflict
        } else if retained.path == canonical_path {
            MigrationSessionAction::KeepCanonical
        } else if canonical_path.exists() {
            MigrationSessionAction::ReplaceCanonicalWithExtension
        } else {
            MigrationSessionAction::CopyToCanonical
        };
        sessions.push(MigrationSessionPlan {
            thread_id,
            action,
            retained_path: retained.path.clone(),
            canonical_path,
            retained_bytes: retained.bytes,
            retained_sha256: hex_digest(retained.raw_sha256),
            retained_message_count: retained.message_count,
            last_valid_message_at: retained.last_message_timestamp.clone(),
            duplicates: duplicate_plans(inventory, &indexes, retained_index),
        });
    }
    sessions.sort_by(|left, right| left.thread_id.cmp(&right.thread_id));
    conflicts.sort_by(|left, right| left.thread_id.cmp(&right.thread_id));
    let mut databases = inventory
        .graph
        .databases
        .iter()
        .filter(|database| database.role.is_runtime())
        .filter_map(database_plan)
        .collect::<Vec<_>>();
    for goals in &inventory.goals_catalog.databases {
        for (view_index, view) in goals.descriptor.views.iter().enumerate() {
            databases.push(MigrationDatabasePlan {
                database_id: format!("{}-view-{view_index:04}", goals.descriptor.id),
                path: view.path.clone(),
                role: view.role,
                reference_count: goals.row_count,
            });
        }
    }
    databases.sort_by(|left, right| left.database_id.cmp(&right.database_id));
    Ok((
        CanonicalMigrationPlan {
            schema_version: SESSION_STORAGE_SCHEMA_VERSION,
            operation_id: operation_id_value.to_string(),
            generated_at_ms: timestamp_millis()?,
            canonical_root: paths.codex_home,
            inventory_fingerprint: inventory.fingerprint.clone(),
            sessions,
            conflicts,
            databases,
            unclassified_file_count: inventory
                .files
                .iter()
                .filter(|file| file.semantic.is_err())
                .count(),
            invalid_marker_count: inventory
                .files
                .iter()
                .filter(|file| file.marker_status == MarkerStatus::Invalid)
                .count(),
            missing_runtime_reference_count: inventory
                .graph
                .summary
                .missing_runtime_reference_count,
            mismatched_runtime_reference_count: inventory
                .graph
                .summary
                .mismatched_runtime_reference_count,
        },
        target_collision,
    ))
}

fn canonical_merge_source(origin: FileOrigin) -> bool {
    matches!(
        origin,
        FileOrigin::CanonicalHome
            | FileOrigin::Shared
            | FileOrigin::ReferencedExternal
            | FileOrigin::TemporaryAdapter
    )
}

fn select_unmarked_canonical_index(
    indexes: &[usize],
    inventory: &MigrationInventory,
) -> Option<usize> {
    indexes
        .iter()
        .copied()
        .filter(|index| {
            inventory.files[*index].origin == FileOrigin::CanonicalHome
                && inventory.files[*index].marker_status == MarkerStatus::Absent
        })
        .min_by_key(|index| path_key(&inventory.files[*index].path))
}

fn select_merge_retained_candidate(
    indexes: &[usize],
    inventory: &MigrationInventory,
) -> Option<usize> {
    let mut candidates = indexes
        .iter()
        .copied()
        .filter(|index| inventory.files[*index].semantic.is_ok())
        .collect::<Vec<_>>();
    candidates.sort_by_key(|index| {
        let semantic = inventory.files[*index]
            .semantic
            .as_ref()
            .expect("filtered semantic");
        (
            Reverse(semantic.normalized_line_sha256.len()),
            merge_origin_rank(inventory.files[*index].origin),
            path_key(&inventory.files[*index].path),
        )
    });
    let complete = candidates.iter().copied().find(|candidate_index| {
        let candidate = inventory.files[*candidate_index]
            .semantic
            .as_ref()
            .expect("filtered semantic");
        indexes.iter().all(|other_index| {
            if other_index == candidate_index {
                return true;
            }
            match inventory.files[*other_index].semantic.as_ref() {
                Ok(other) => matches!(
                    compare_sessions(other, candidate),
                    SessionRelation::Equal
                        | SessionRelation::EqualExceptProvider
                        | SessionRelation::LeftPrefix
                ),
                Err(_) => false,
            }
        })
    });
    complete.or_else(|| {
        candidates.sort_by_key(|index| {
            let semantic = inventory.files[*index]
                .semantic
                .as_ref()
                .expect("filtered semantic");
            (
                merge_origin_rank(inventory.files[*index].origin),
                Reverse(semantic.normalized_line_sha256.len()),
                path_key(&inventory.files[*index].path),
            )
        });
        candidates.into_iter().next()
    })
}

fn merge_origin_rank(origin: FileOrigin) -> u8 {
    match origin {
        FileOrigin::CanonicalHome => 0,
        FileOrigin::Shared => 1,
        FileOrigin::ReferencedExternal => 2,
        FileOrigin::TemporaryAdapter => 3,
        FileOrigin::BackupInventory
        | FileOrigin::ConflictRecycle
        | FileOrigin::RecoveryPackage
        | FileOrigin::DowngradeExport
        | FileOrigin::Unknown => 4,
    }
}

fn merge_file_relation(
    inventory: &MigrationInventory,
    index: usize,
    retained_index: usize,
) -> SessionRelation {
    if index == retained_index {
        return SessionRelation::Equal;
    }
    match (
        inventory.files[index].semantic.as_ref(),
        inventory.files[retained_index].semantic.as_ref(),
    ) {
        (Ok(candidate), Ok(retained)) => compare_sessions(candidate, retained),
        _ => SessionRelation::Unknown,
    }
}

fn duplicate_plans(
    inventory: &MigrationInventory,
    indexes: &[usize],
    retained_index: usize,
) -> Vec<MigrationDuplicatePlan> {
    let mut duplicates = indexes
        .iter()
        .copied()
        .filter(|index| *index != retained_index)
        .map(|index| {
            let file = &inventory.files[index];
            MigrationDuplicatePlan {
                path: file.path.clone(),
                bytes: file.bytes,
                sha256: file.raw_sha256.clone(),
                relation_to_retained: merge_file_relation(inventory, index, retained_index),
                marker_status: file.marker_status,
            }
        })
        .collect::<Vec<_>>();
    duplicates.sort_by_key(|duplicate| path_key(&duplicate.path));
    duplicates
}

fn database_plan(database: &DatabaseInput) -> Option<MigrationDatabasePlan> {
    Some(MigrationDatabasePlan {
        database_id: database.id.clone(),
        path: database.path.clone()?,
        role: database.role,
        reference_count: database.references.len(),
    })
}

fn generated_canonical_path(sessions_dir: &Path, thread_id: &str) -> PathBuf {
    let digest = hex_digest(Sha256::digest(thread_id.as_bytes()));
    sessions_dir
        .join("migrated")
        .join(format!("rollout-{digest}.jsonl"))
}

fn build_backup_sources(
    codex_home: &Path,
    data_root: &Path,
    inventory: &MigrationInventory,
) -> Result<Vec<MigrationBackupSource>, String> {
    let paths = resolve_user_codex_paths(codex_home)?;
    let mut sources = Vec::new();
    let mut source_keys = BTreeSet::new();
    for (index, file) in inventory.files.iter().enumerate() {
        let relative = if file.path.starts_with(codex_home) {
            PathBuf::from("canonical").join(
                file.path
                    .strip_prefix(codex_home)
                    .map_err(|_| "failed to map canonical backup source".to_string())?,
            )
        } else {
            let file_name = file
                .path
                .file_name()
                .ok_or_else(|| "migration session source has no file name".to_string())?;
            PathBuf::from("external-sessions")
                .join(format!("{index:06}"))
                .join(file_name)
        };
        add_backup_source(
            &mut sources,
            &mut source_keys,
            MigrationBackupSource {
                source_path: file.path.clone(),
                payload_relative_path: relative,
                kind: MigrationBackupEntryKind::Session,
                expected_sha256: Some(file.raw_sha256.clone()),
                logical_thread_id: file
                    .semantic
                    .as_ref()
                    .ok()
                    .map(|semantic| semantic.thread_id.clone()),
            },
        );
        if let Ok(marker) = provider_marker_path(&file.path) {
            if marker.is_file() {
                let file_name = marker
                    .file_name()
                    .ok_or_else(|| "provider marker has no file name".to_string())?
                    .to_os_string();
                add_backup_source(
                    &mut sources,
                    &mut source_keys,
                    MigrationBackupSource {
                        source_path: marker,
                        payload_relative_path: PathBuf::from("provider-markers")
                            .join(format!("{index:06}"))
                            .join(&file_name),
                        kind: MigrationBackupEntryKind::StorageMetadata,
                        expected_sha256: None,
                        logical_thread_id: None,
                    },
                );
            }
        }
    }
    if paths.session_index.is_file() {
        add_backup_source(
            &mut sources,
            &mut source_keys,
            MigrationBackupSource {
                source_path: paths.session_index.clone(),
                payload_relative_path: "canonical/session_index.jsonl".into(),
                kind: MigrationBackupEntryKind::SessionIndex,
                expected_sha256: None,
                logical_thread_id: None,
            },
        );
    }
    for (label, database) in [
        ("canonical-state_5.sqlite", &paths.state_db),
        ("canonical-logs_2.sqlite", &paths.logs_db),
        ("canonical-memories_1.sqlite", &paths.memories_db),
    ] {
        if database.is_file() {
            add_backup_source(
                &mut sources,
                &mut source_keys,
                MigrationBackupSource {
                    source_path: database.clone(),
                    payload_relative_path: PathBuf::from("databases").join(label),
                    kind: MigrationBackupEntryKind::Database,
                    expected_sha256: None,
                    logical_thread_id: None,
                },
            );
        }
    }
    for database in &inventory.graph.databases {
        let Some(path) = &database.path else {
            continue;
        };
        if path.is_file() {
            let name = path
                .file_name()
                .ok_or_else(|| "migration database source has no file name".to_string())?;
            add_backup_source(
                &mut sources,
                &mut source_keys,
                MigrationBackupSource {
                    source_path: path.clone(),
                    payload_relative_path: PathBuf::from("databases").join(format!(
                        "{}-{}",
                        database.id,
                        name.to_string_lossy()
                    )),
                    kind: MigrationBackupEntryKind::Database,
                    expected_sha256: None,
                    logical_thread_id: None,
                },
            );
        }
    }
    let canonical_goals_id = inventory
        .goals_catalog
        .databases
        .iter()
        .min_by_key(|goals| {
            (
                goals
                    .descriptor
                    .views
                    .iter()
                    .map(|view| view.role)
                    .min()
                    .unwrap_or(DatabaseRole::UnknownRuntime),
                path_key(&goals.descriptor.source_path),
            )
        })
        .map(|goals| goals.descriptor.id.as_str());
    for goals in &inventory.goals_catalog.databases {
        let label = if canonical_goals_id == Some(goals.descriptor.id.as_str()) {
            "canonical-goals_1.sqlite".to_string()
        } else {
            format!("{}-goals_1.sqlite", goals.descriptor.id)
        };
        add_backup_source(
            &mut sources,
            &mut source_keys,
            MigrationBackupSource {
                source_path: goals.descriptor.source_path.clone(),
                payload_relative_path: PathBuf::from("databases").join(label),
                kind: MigrationBackupEntryKind::Database,
                expected_sha256: None,
                logical_thread_id: None,
            },
        );
    }
    for (source, relative) in [
        (
            data_root.join("session-storage-v1/route-epochs.jsonl"),
            PathBuf::from("metadata/route-epochs.jsonl"),
        ),
        (
            data_root.join("request-route-session-view-v2.json"),
            PathBuf::from("metadata/request-route-session-view-v2.json"),
        ),
        (
            data_root.join("request-route-session-view-v1.json"),
            PathBuf::from("metadata/request-route-session-view-v1.json"),
        ),
    ] {
        if source.is_file() {
            add_backup_source(
                &mut sources,
                &mut source_keys,
                MigrationBackupSource {
                    source_path: source,
                    payload_relative_path: relative,
                    kind: MigrationBackupEntryKind::StorageMetadata,
                    expected_sha256: None,
                    logical_thread_id: None,
                },
            );
        }
    }
    sources.sort_by(|left, right| left.payload_relative_path.cmp(&right.payload_relative_path));
    Ok(sources)
}

fn add_backup_source(
    sources: &mut Vec<MigrationBackupSource>,
    keys: &mut BTreeSet<String>,
    source: MigrationBackupSource,
) {
    if keys.insert(path_key(&source.source_path)) {
        sources.push(source);
    }
}

fn inventory_fingerprint(
    files: &[MigrationInventoryFile],
    catalog: &CatalogSnapshot,
    goals_catalog: &GoalsCatalogSnapshot,
    database_digests: &[(PathBuf, String)],
    database_discovery_errors: usize,
    session_discovery_errors: usize,
) -> Result<String, String> {
    let mut hasher = Sha256::new();
    hasher.update(b"codex-switch-migration-inventory-v1\0");
    hasher.update(database_discovery_errors.to_le_bytes());
    hasher.update(session_discovery_errors.to_le_bytes());
    hasher.update(catalog.database_errors.to_le_bytes());
    hasher.update(catalog.rows_missing_rollout_path.to_le_bytes());
    hasher.update(goals_catalog.errors.to_le_bytes());
    for file in files {
        hash_field(&mut hasher, path_key(&file.path).as_bytes());
        hash_field(&mut hasher, file.raw_sha256.as_bytes());
        hasher.update(file.bytes.to_le_bytes());
        hash_field(
            &mut hasher,
            serde_json::to_string(&file.origin)
                .map_err(|_| "failed to fingerprint migration origin".to_string())?
                .as_bytes(),
        );
        hash_field(
            &mut hasher,
            serde_json::to_string(&file.marker_status)
                .map_err(|_| "failed to fingerprint migration marker".to_string())?
                .as_bytes(),
        );
    }
    for (path, digest) in database_digests {
        hash_field(&mut hasher, path_key(path).as_bytes());
        hash_field(&mut hasher, digest.as_bytes());
    }
    let mut databases = catalog.databases.clone();
    databases.sort_by(|left, right| left.id.cmp(&right.id));
    for database in databases {
        hash_field(&mut hasher, database.id.as_bytes());
        if let Some(path) = database.path {
            hash_field(&mut hasher, path_key(&path).as_bytes());
        }
        hash_field(
            &mut hasher,
            serde_json::to_string(&database.role)
                .map_err(|_| "failed to fingerprint migration database".to_string())?
                .as_bytes(),
        );
        let mut references = database.references;
        references.sort_by(|left, right| {
            left.thread_id
                .cmp(&right.thread_id)
                .then_with(|| path_key(&left.rollout_path).cmp(&path_key(&right.rollout_path)))
        });
        for reference in references {
            hash_field(&mut hasher, reference.thread_id.as_bytes());
            hash_field(&mut hasher, path_key(&reference.rollout_path).as_bytes());
            hash_field(
                &mut hasher,
                reference.model_provider.as_deref().unwrap_or("").as_bytes(),
            );
        }
    }
    for goals in &goals_catalog.databases {
        hash_field(&mut hasher, goals.descriptor.id.as_bytes());
        hash_field(
            &mut hasher,
            path_key(&goals.descriptor.source_path).as_bytes(),
        );
        hash_field(&mut hasher, goals.schema_sha256.as_bytes());
        hash_field(&mut hasher, goals.rows_sha256.as_bytes());
        hasher.update(goals.row_count.to_le_bytes());
        for view in &goals.descriptor.views {
            hash_field(&mut hasher, path_key(&view.path).as_bytes());
            hash_field(
                &mut hasher,
                serde_json::to_string(&view.role)
                    .map_err(|_| "failed to fingerprint goals database view".to_string())?
                    .as_bytes(),
            );
        }
    }
    Ok(hex_digest(hasher.finalize()))
}

fn snapshot_database_digests(
    codex_home: &Path,
    descriptors: &[super::catalog::DatabaseDescriptor],
    goals_descriptors: &[super::catalog::GoalsDatabaseDescriptor],
    staging_root: &Path,
) -> Result<Vec<(PathBuf, String)>, String> {
    let paths = resolve_user_codex_paths(codex_home)?;
    let mut databases = BTreeMap::<String, PathBuf>::new();
    for descriptor in descriptors {
        if descriptor.path.is_file() {
            databases.insert(path_key(&descriptor.path), descriptor.path.clone());
        }
    }
    for descriptor in goals_descriptors {
        if descriptor.source_path.is_file() {
            databases.insert(
                path_key(&descriptor.source_path),
                descriptor.source_path.clone(),
            );
        }
    }
    for path in [paths.state_db, paths.logs_db, paths.memories_db] {
        if path.is_file() {
            databases.entry(path_key(&path)).or_insert(path);
        }
    }
    let mut digests = Vec::with_capacity(databases.len());
    for (index, path) in databases.into_values().enumerate() {
        let snapshot = staging_root.join(format!("database-digest-{index:04}.sqlite"));
        let result = (|| {
            let source = Connection::open_with_flags(
                &path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .map_err(|_| "failed to open migration database for snapshot".to_string())?;
            source
                .backup(MAIN_DB, &snapshot, None)
                .map_err(|_| "failed to create migration database snapshot".to_string())?;
            drop(source);
            let snapshot_connection = Connection::open_with_flags(
                &snapshot,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .map_err(|_| "failed to open migration database snapshot".to_string())?;
            let quick_check: String = snapshot_connection
                .query_row("PRAGMA quick_check", [], |row| row.get(0))
                .map_err(|_| "failed to verify migration database snapshot".to_string())?;
            if quick_check != "ok" {
                return Err("migration database snapshot failed quick_check".to_string());
            }
            drop(snapshot_connection);
            let (_, digest) = stable_file_digest(&snapshot)?;
            Ok((path.clone(), digest))
        })();
        let cleanup = match fs::remove_file(&snapshot) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        };
        match (result, cleanup) {
            (Ok(digest), Ok(())) => digests.push(digest),
            (Err(error), Ok(())) => return Err(error),
            (Ok(_), Err(error)) => {
                return Err(format!(
                    "migration database snapshot cleanup failed: {error}"
                ))
            }
            (Err(error), Err(cleanup_error)) => {
                return Err(format!(
                    "{error}; migration database snapshot cleanup failed: {cleanup_error}"
                ))
            }
        }
    }
    digests.sort_by(|left, right| path_key(&left.0).cmp(&path_key(&right.0)));
    Ok(digests)
}

fn stable_file_digest(path: &Path) -> Result<(u64, String), String> {
    let before = fs::symlink_metadata(path)
        .map_err(|_| "migration inventory file is unavailable".to_string())?;
    if !before.is_file() || metadata_is_link_or_reparse(&before) {
        return Err("migration inventory file is unsafe".to_string());
    }
    let mut file =
        fs::File::open(path).map_err(|_| "migration inventory file is unreadable".to_string())?;
    let mut hasher = Sha256::new();
    let mut read_bytes = 0_u64;
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|_| "migration inventory file is unreadable".to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        read_bytes = read_bytes
            .checked_add(read as u64)
            .ok_or_else(|| "migration inventory file size overflowed".to_string())?;
    }
    let after = fs::symlink_metadata(path)
        .map_err(|_| "migration inventory file is unavailable".to_string())?;
    if before.len() != after.len()
        || before.modified().ok() != after.modified().ok()
        || read_bytes != before.len()
    {
        return Err("migration inventory file changed during read".to_string());
    }
    Ok((before.len(), hex_digest(hasher.finalize())))
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value);
}

fn estimate_source_bytes(sources: &[MigrationBackupSource]) -> Result<u64, String> {
    sources.iter().try_fold(0_u64, |total, source| {
        let mut bytes = fs::metadata(&source.source_path)
            .map_err(|_| "failed to inspect migration backup source".to_string())?
            .len();
        if source.kind == MigrationBackupEntryKind::Database {
            let wal = PathBuf::from(format!("{}-wal", source.source_path.to_string_lossy()));
            if let Ok(metadata) = fs::metadata(wal) {
                bytes = bytes.saturating_add(metadata.len());
            }
        }
        total
            .checked_add(bytes)
            .ok_or_else(|| "migration backup size overflowed".to_string())
    })
}

fn required_backup_bytes(source_bytes: u64) -> u64 {
    let backup_and_isolated_restore = source_bytes.saturating_mul(2);
    let percentage = backup_and_isolated_restore.saturating_mul(BACKUP_RESERVE_PERCENT) / 100;
    backup_and_isolated_restore.saturating_add(percentage.max(BACKUP_RESERVE_BYTES))
}

fn validate_backup_destination(
    destination: &Path,
    sources: &[MigrationBackupSource],
    codex_home: &Path,
    data_root: &Path,
) -> Result<(), String> {
    let metadata = fs::symlink_metadata(destination)
        .map_err(|_| "migration backup destination is unavailable".to_string())?;
    if !metadata.is_dir() || metadata_is_link_or_reparse(&metadata) {
        return Err("migration backup destination is unsafe".to_string());
    }
    let destination = fs::canonicalize(destination)
        .map_err(|_| "migration backup destination is unavailable".to_string())?;
    for managed_root in [codex_home, data_root] {
        let managed_root = fs::canonicalize(managed_root)
            .map_err(|_| "migration managed storage root is unavailable".to_string())?;
        if managed_root.starts_with(&destination) || destination.starts_with(&managed_root) {
            return Err("migration backup destination overlaps managed storage".to_string());
        }
    }
    for source in sources {
        let source = fs::canonicalize(&source.source_path)
            .map_err(|_| "migration backup source is unavailable".to_string())?;
        if source.starts_with(&destination) || destination.starts_with(&source) {
            return Err("migration backup source and destination overlap".to_string());
        }
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) fn available_backup_bytes(destination: &Path) -> Result<u64, String> {
    use std::{os::windows::ffi::OsStrExt, ptr::null_mut};
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let canonical = fs::canonicalize(destination)
        .map_err(|_| "migration backup destination is unavailable".to_string())?;
    let wide = canonical
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut available = 0_u64;
    let result =
        unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, null_mut(), null_mut()) };
    if result == 0 {
        Err("migration backup capacity is unavailable".to_string())
    } else {
        Ok(available)
    }
}

#[cfg(not(windows))]
pub(crate) fn available_backup_bytes(_destination: &Path) -> Result<u64, String> {
    Err("migration backup capacity is unsupported on this platform".to_string())
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

fn preflight_path(data_root: &Path, operation_id_value: &str) -> Result<PathBuf, String> {
    if !data_root.is_absolute() {
        return Err("migration data root must be absolute".to_string());
    }
    validate_operation_id(operation_id_value)?;
    Ok(data_root
        .join("session-storage-v1/operations")
        .join(operation_id_value)
        .join("preflight.json"))
}

fn validate_preflight_report(report: &MigrationPreflightReport) -> Result<(), String> {
    if report.schema_version != SESSION_STORAGE_SCHEMA_VERSION
        || report.plan.schema_version != SESSION_STORAGE_SCHEMA_VERSION
        || report.operation_id != report.plan.operation_id
        || report.plan.inventory_fingerprint.len() != 64
        || !report
            .plan
            .inventory_fingerprint
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || !report.plan.canonical_root.is_absolute()
        || !report.backup_destination.is_absolute()
        || report.ready_for_backup != report.blockers.is_empty()
        || report.conflict_count != report.plan.conflicts.len()
    {
        return Err("migration preflight contract is invalid".to_string());
    }
    validate_operation_id(&report.operation_id)?;
    let mut blockers = report.blockers.clone();
    blockers.sort_unstable();
    blockers.dedup();
    if blockers != report.blockers {
        return Err("migration preflight blockers are invalid".to_string());
    }
    for conflict in &report.plan.conflicts {
        if conflict.thread_id.trim().is_empty()
            || !conflict.current_path.is_absolute()
            || !conflict.candidate_path.is_absolute()
            || !conflict.canonical_path.is_absolute()
            || conflict.current_path == conflict.candidate_path
            || !valid_sha256(&conflict.candidate_sha256)
            || conflict
                .current_sha256
                .as_deref()
                .is_some_and(|digest| !valid_sha256(digest))
            || (report.ready_for_backup && conflict.current_sha256.is_none())
            || conflict.default_overwrite
        {
            return Err("migration conflict preflight contract is invalid".to_string());
        }
    }
    Ok(())
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn preflight_digest(report: &MigrationPreflightReport) -> Result<String, String> {
    let bytes = serde_json::to_vec(report)
        .map_err(|_| "failed to serialize migration preflight".to_string())?;
    Ok(hex_digest(Sha256::digest(bytes)))
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
    use std::fs;

    use rusqlite::Connection;
    use sha2::{Digest, Sha256};
    use tempfile::tempdir;

    use super::{
        build_plan, collect_inventory, generated_canonical_path, provider_marker_path,
        required_backup_bytes, scan_stable_migration_plan, MigrationSafetyBlocker,
        MigrationSessionAction, SessionRelation, BACKUP_RESERVE_BYTES,
    };

    fn write_session(path: &std::path::Path, provider: &str, messages: &[&str]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut lines = vec![format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"thread-a\",\"model_provider\":\"{provider}\"}}}}"
        )];
        lines.extend(messages.iter().map(|message| {
            serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "user_message", "message": message}
            })
            .to_string()
        }));
        fs::write(path, lines.join("\n") + "\n").unwrap();
    }

    fn write_marker(path: &std::path::Path, provider: &str) {
        let body = fs::read(path).unwrap();
        fs::write(
            provider_marker_path(path).unwrap(),
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "threadId": "thread-a",
                "providerId": provider,
                "slotFileName": path.file_name().unwrap().to_string_lossy(),
                "originRelativePath": null,
                "originProvider": "openai",
                "createdBytes": body.len(),
                "createdSha256": format!("{:x}", Sha256::digest(&body)),
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn setup() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let data = root.path().join("data");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(data.join("shared-sessions/sessions/2026/08/11")).unwrap();
        fs::write(
            home.join("config.toml"),
            format!("sqlite_home = {:?}\n", home.to_string_lossy()),
        )
        .unwrap();
        let database = Connection::open(home.join("state_5.sqlite")).unwrap();
        database
            .execute_batch(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, model_provider TEXT);",
            )
            .unwrap();
        drop(database);
        let goals = Connection::open(home.join("goals_1.sqlite")).unwrap();
        goals
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
        drop(goals);
        (root, home, data)
    }

    #[test]
    fn plans_provider_equal_and_prefix_copies_without_using_file_name_time() {
        let (_root, home, data) = setup();
        let canonical = home.join("sessions/2099/late-name.jsonl");
        let shared = data.join("shared-sessions/sessions/2020/early-name.jsonl");
        write_session(&canonical, "openai", &["one"]);
        write_session(&shared, "openai_custom", &["one", "two"]);
        let database = Connection::open(home.join("state_5.sqlite")).unwrap();
        database
            .execute(
                "INSERT INTO threads VALUES (?1, ?2, 'openai')",
                ("thread-a", canonical.to_string_lossy().to_string()),
            )
            .unwrap();
        drop(database);

        let inventory = collect_inventory(&home, &data).unwrap();
        let (plan, collision) = build_plan(&home, "migration-1", &inventory).unwrap();

        assert!(!collision);
        assert_eq!(plan.sessions.len(), 1);
        assert_eq!(
            plan.sessions[0].action,
            MigrationSessionAction::ReplaceCanonicalWithExtension
        );
        assert_eq!(plan.sessions[0].retained_path, shared);
        assert_eq!(plan.sessions[0].canonical_path, canonical);
        assert_eq!(
            plan.sessions[0].duplicates[0].relation_to_retained,
            SessionRelation::LeftPrefix
        );
    }

    #[test]
    fn a_switch_provider_slot_is_never_selected_as_the_canonical_target() {
        let (_root, home, data) = setup();
        let provider_slot = home.join("sessions/a-provider-slot.jsonl");
        let canonical = home.join("sessions/z-canonical.jsonl");
        write_session(&provider_slot, "openai_custom", &["one"]);
        write_marker(&provider_slot, "openai_custom");
        write_session(&canonical, "openai", &["one"]);

        let inventory = collect_inventory(&home, &data).unwrap();
        let (plan, collision) = build_plan(&home, "migration-1", &inventory).unwrap();

        assert!(!collision);
        assert_eq!(plan.sessions.len(), 1);
        assert_eq!(plan.sessions[0].canonical_path, canonical);
        assert_ne!(plan.sessions[0].canonical_path, provider_slot);
        assert_eq!(
            plan.sessions[0].action,
            MigrationSessionAction::ReplaceCanonicalWithExtension
        );
    }

    #[test]
    fn a_lone_switch_provider_slot_is_moved_to_a_generated_canonical_path() {
        let (_root, home, data) = setup();
        let provider_slot = home.join("sessions/provider-slot.jsonl");
        write_session(&provider_slot, "openai_custom", &["one"]);
        write_marker(&provider_slot, "openai_custom");

        let inventory = collect_inventory(&home, &data).unwrap();
        let (plan, collision) = build_plan(&home, "migration-1", &inventory).unwrap();

        assert!(!collision);
        assert_eq!(plan.sessions.len(), 1);
        assert_eq!(
            plan.sessions[0].canonical_path,
            generated_canonical_path(&home.join("sessions"), "thread-a")
        );
        assert_ne!(plan.sessions[0].canonical_path, provider_slot);
        assert_eq!(
            plan.sessions[0].action,
            MigrationSessionAction::CopyToCanonical
        );
    }

    #[test]
    fn divergent_tail_is_conflict_and_default_never_overwrites() {
        let (_root, home, data) = setup();
        let canonical = home.join("sessions/canonical.jsonl");
        let shared = data.join("shared-sessions/sessions/2026/08/11/shared.jsonl");
        write_session(&canonical, "openai", &["one", "left"]);
        write_session(&shared, "openai_custom", &["one", "right"]);

        let inventory = collect_inventory(&home, &data).unwrap();
        let (plan, _) = build_plan(&home, "migration-1", &inventory).unwrap();

        assert_eq!(plan.sessions[0].action, MigrationSessionAction::Conflict);
        assert_eq!(plan.conflicts.len(), 1);
        assert!(!plan.conflicts[0].default_overwrite);
        assert_eq!(plan.conflicts[0].relation, SessionRelation::Divergent);
    }

    #[test]
    fn stable_inventory_surfaces_every_divergent_branch_without_operation_reports() {
        let (_root, home, data) = setup();
        // A shared runtime root is a required catalog source once present. Give
        // this stable-scan fixture the complete state/goals pair rather than
        // relying on a sessions-only directory that production rejects.
        fs::copy(
            home.join("state_5.sqlite"),
            data.join("shared-sessions/state_5.sqlite"),
        )
        .unwrap();
        fs::copy(
            home.join("goals_1.sqlite"),
            data.join("shared-sessions/goals_1.sqlite"),
        )
        .unwrap();
        let canonical = home.join("sessions/canonical.jsonl");
        let shared_one = data.join("shared-sessions/sessions/2026/08/11/branch-one.jsonl");
        let shared_two = data.join("shared-sessions/sessions/2026/08/11/branch-two.jsonl");
        write_session(&canonical, "openai", &["root", "canonical"]);
        write_session(&shared_one, "openai_custom", &["root", "branch-one"]);
        write_session(&shared_two, "openai_custom", &["root", "branch-two"]);

        let plan = scan_stable_migration_plan(&home, &data, "migration-live-conflicts").unwrap();

        assert_eq!(plan.sessions.len(), 1);
        assert_eq!(plan.sessions[0].action, MigrationSessionAction::Conflict);
        assert_eq!(plan.conflicts.len(), 2);
        assert!(plan.conflicts.iter().all(|conflict| {
            conflict.current_path == canonical
                && conflict.current_path != conflict.candidate_path
                && conflict.relation == SessionRelation::Divergent
                && !conflict.default_overwrite
        }));
        assert_eq!(
            plan.conflicts
                .iter()
                .map(|conflict| conflict.candidate_path.clone())
                .collect::<std::collections::BTreeSet<_>>(),
            std::collections::BTreeSet::from([shared_one, shared_two])
        );
        assert!(!data
            .join("session-storage-v1/operations/migration-live-conflicts")
            .exists());
    }

    #[test]
    fn occupied_generated_target_is_reported_as_an_explicit_conflict() {
        let (_root, home, data) = setup();
        let candidate = data.join("shared-sessions/sessions/2026/08/11/shared.jsonl");
        write_session(&candidate, "openai_custom", &["candidate"]);
        let collision = generated_canonical_path(&home.join("sessions"), "thread-a");
        fs::create_dir_all(collision.parent().unwrap()).unwrap();
        fs::write(
            &collision,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"thread-b\",\"model_provider\":\"openai\"}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"occupied\"}}\n"
            ),
        )
        .unwrap();

        let inventory = collect_inventory(&home, &data).unwrap();
        let (plan, target_collision) = build_plan(&home, "migration-1", &inventory).unwrap();
        let session = plan
            .sessions
            .iter()
            .find(|session| session.thread_id == "thread-a")
            .unwrap();
        let conflict = plan
            .conflicts
            .iter()
            .find(|conflict| conflict.thread_id == "thread-a")
            .unwrap();

        assert!(target_collision);
        assert_eq!(session.action, MigrationSessionAction::Conflict);
        assert_eq!(conflict.current_path, collision);
        assert_eq!(conflict.candidate_path, candidate);
        assert_eq!(conflict.relation, SessionRelation::Unknown);
        assert!(!conflict.default_overwrite);
    }

    #[test]
    fn backup_inventory_is_reported_but_never_auto_restored_into_canonical() {
        let (_root, home, data) = setup();
        let canonical = home.join("sessions/canonical.jsonl");
        let backup_session = data.join("backups/old/sessions/thread-a.jsonl");
        let backup_only = data.join("backups/old/sessions/thread-backup-only.jsonl");
        write_session(&canonical, "openai", &["current"]);
        write_session(
            &backup_session,
            "openai_custom",
            &["current", "historical-extension"],
        );
        fs::write(
            &backup_only,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"thread-backup-only\",\"model_provider\":\"openai_custom\"}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"backup only\"}}\n"
            ),
        )
        .unwrap();
        let current = Connection::open(home.join("state_5.sqlite")).unwrap();
        current
            .execute(
                "INSERT INTO threads VALUES (?1, ?2, 'openai')",
                ("thread-a", canonical.to_string_lossy().to_string()),
            )
            .unwrap();
        drop(current);
        let backup_db = data.join("backups/old/state_5.sqlite");
        let backup = Connection::open(&backup_db).unwrap();
        backup
            .execute_batch(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, model_provider TEXT);",
            )
            .unwrap();
        backup
            .execute(
                "INSERT INTO threads VALUES (?1, ?2, 'openai_custom')",
                ("thread-a", backup_session.to_string_lossy().to_string()),
            )
            .unwrap();
        backup
            .execute(
                "INSERT INTO threads VALUES (?1, ?2, 'openai_custom')",
                (
                    "thread-backup-only",
                    backup_only.to_string_lossy().to_string(),
                ),
            )
            .unwrap();
        drop(backup);

        let inventory = collect_inventory(&home, &data).unwrap();
        assert!(inventory.files.iter().any(|file| {
            file.origin == super::FileOrigin::BackupInventory && file.path == backup_session
        }));
        let (plan, collision) = build_plan(&home, "migration-1", &inventory).unwrap();

        assert!(!collision);
        assert_eq!(plan.sessions.len(), 1);
        assert_eq!(plan.sessions[0].thread_id, "thread-a");
        assert_eq!(
            plan.sessions[0].action,
            MigrationSessionAction::KeepCanonical
        );
        assert_eq!(plan.sessions[0].retained_path, canonical);
        assert!(plan.conflicts.is_empty());
    }

    #[test]
    fn blocker_enum_is_stably_serialized_for_frontend_contract() {
        assert_eq!(
            serde_json::to_string(&MigrationSafetyBlocker::InventoryChanged).unwrap(),
            "\"inventoryChanged\""
        );
    }

    #[test]
    fn backup_capacity_covers_the_backup_and_its_isolated_restore() {
        let source_bytes = 1_024_u64;
        assert_eq!(
            required_backup_bytes(source_bytes),
            source_bytes * 2 + BACKUP_RESERVE_BYTES
        );
    }
}

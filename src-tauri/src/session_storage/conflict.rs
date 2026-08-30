use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{file_ops::atomic_write, operation_log::timestamp_millis};

use super::{
    bounded_file::read_regular_file_bounded,
    legacy_backup::{
        load_pending_recovery_source, PendingRecoveryList, PendingRecoveryRelation,
        PendingRecoveryStatus,
    },
    marker::inspect_provider_marker,
    migration::{
        scan_stable_migration_plan, CanonicalMigrationPlan, MigrationConflictPlan,
        MigrationInventory, MigrationInventoryFile, MigrationPreflightReport,
    },
    model::{FileOrigin, MarkerStatus},
    provenance::parse_rfc3339_millis,
    reference_graph::path_key,
    relation::compare_sessions,
    restore_import::RestoreImportPlan,
    semantic::{read_semantic_session, SemanticSession},
    SessionRelation,
};

const RESOLVED_CONFLICT_SCHEMA_VERSION: u32 = 1;
const MAX_RESOLVED_CONFLICT_BYTES: u64 = 32 * 1024 * 1024;
const RESOLVED_CONFLICT_NAME: &str = "resolved-conflicts-v1.json";
const DEFERRED_CONFLICT_SCHEMA_VERSION: u32 = 1;
const MAX_DEFERRED_CONFLICT_BYTES: u64 = 32 * 1024 * 1024;
const MAX_PROTECTED_DEFERRED_CONFLICT_BYTES: u64 = MAX_DEFERRED_CONFLICT_BYTES * 2 + 64 * 1024;
const DEFERRED_CONFLICT_NAME: &str = "deferred-conflicts-v1.dpapi";
const DEFERRED_CONFLICT_CIPHERTEXT_MAGIC: &[u8] = b"CSDEFERRED1\0";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ConflictVersion {
    Current,
    Candidate,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionConflictSummary {
    pub conflict_id: String,
    #[serde(default)]
    pub deferred: bool,
    pub current_message_count: usize,
    pub candidate_message_count: usize,
    pub current_added_message_count: usize,
    pub candidate_added_message_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_last_message_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_last_message_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_provider: Option<String>,
    pub current_origin: FileOrigin,
    pub candidate_origin: FileOrigin,
    pub relation: SessionRelation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newer_version: Option<ConflictVersion>,
    pub default_overwrite: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionConflictList {
    pub migration_operation_id: String,
    pub conflicts: Vec<SessionConflictSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ResolvedConflictRegistry {
    schema_version: u32,
    migration_operation_id: String,
    canonical_root_sha256: String,
    resolved_conflict_ids: Vec<String>,
    updated_at_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ResolvedConflictRegistryEnvelope {
    registry: ResolvedConflictRegistry,
    integrity_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DeferredConflictRegistry {
    schema_version: u32,
    migration_operation_id: String,
    canonical_root_sha256: String,
    deferred_conflict_ids: Vec<String>,
    updated_at_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DeferredConflictRegistryEnvelope {
    registry: DeferredConflictRegistry,
    integrity_sha256: String,
}

#[derive(Debug, Clone)]
pub(crate) struct SessionConflictCandidate {
    pub summary: SessionConflictSummary,
    pub resolution_report: Option<MigrationPreflightReport>,
    pub pending_recovery_entry_ids: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct PendingRecoveryConflictScan {
    pub candidates: Vec<SessionConflictCandidate>,
    pub resolved_by_content_entry_ids: Vec<String>,
}

enum CandidateBuildResult {
    Conflict(Box<SessionConflictCandidate>),
    CanonicalContainsCandidate,
}

struct ConflictVersionSource {
    path: PathBuf,
    expected_sha256: Option<String>,
    origin: FileOrigin,
    marker_status: MarkerStatus,
}

pub fn list_migration_conflicts(
    report: &MigrationPreflightReport,
) -> Result<SessionConflictList, String> {
    list_migration_conflicts_excluding(report, &BTreeSet::new())
}

pub(crate) fn load_resolved_conflict_ids(
    data_root: &Path,
    canonical_root: &Path,
    migration_operation_id: &str,
) -> Result<BTreeSet<String>, String> {
    validate_safe_id(migration_operation_id, "migration operation ID")?;
    let path = resolved_conflict_registry_path(data_root);
    let bytes = match read_regular_file_bounded(&path, MAX_RESOLVED_CONFLICT_BYTES) {
        Ok(bytes) => bytes,
        Err(_) if !path.exists() => return Ok(BTreeSet::new()),
        Err(_) => return Err("resolved conflict registry is unreadable".to_string()),
    };
    let envelope = serde_json::from_slice::<ResolvedConflictRegistryEnvelope>(&bytes)
        .map_err(|_| "resolved conflict registry is invalid".to_string())?;
    validate_resolved_conflict_registry(&envelope.registry)?;
    if envelope.integrity_sha256 != resolved_conflict_registry_digest(&envelope.registry)? {
        return Err("resolved conflict registry integrity check failed".to_string());
    }
    if envelope.registry.migration_operation_id != migration_operation_id
        || envelope.registry.canonical_root_sha256 != canonical_root_digest(canonical_root)
    {
        return Ok(BTreeSet::new());
    }
    Ok(envelope
        .registry
        .resolved_conflict_ids
        .into_iter()
        .collect())
}

pub(crate) fn record_resolved_conflict(
    data_root: &Path,
    canonical_root: &Path,
    migration_operation_id: &str,
    conflict_id: &str,
) -> Result<(), String> {
    validate_safe_id(migration_operation_id, "migration operation ID")?;
    validate_conflict_id(conflict_id)?;
    let mut resolved =
        load_resolved_conflict_ids(data_root, canonical_root, migration_operation_id)?;
    resolved.insert(conflict_id.to_string());
    let registry = ResolvedConflictRegistry {
        schema_version: RESOLVED_CONFLICT_SCHEMA_VERSION,
        migration_operation_id: migration_operation_id.to_string(),
        canonical_root_sha256: canonical_root_digest(canonical_root),
        resolved_conflict_ids: resolved.into_iter().collect(),
        updated_at_ms: timestamp_millis()?,
    };
    validate_resolved_conflict_registry(&registry)?;
    let envelope = ResolvedConflictRegistryEnvelope {
        integrity_sha256: resolved_conflict_registry_digest(&registry)?,
        registry,
    };
    let bytes = serde_json::to_vec_pretty(&envelope)
        .map_err(|_| "failed to serialize resolved conflict registry".to_string())?;
    if bytes.len() as u64 > MAX_RESOLVED_CONFLICT_BYTES {
        return Err("resolved conflict registry reached its size limit".to_string());
    }
    atomic_write(&resolved_conflict_registry_path(data_root), &bytes)?;
    let verified = load_resolved_conflict_ids(data_root, canonical_root, migration_operation_id)?;
    if !verified.contains(conflict_id) {
        return Err("resolved conflict registry verification failed".to_string());
    }
    clear_deferred_conflict(
        data_root,
        canonical_root,
        migration_operation_id,
        conflict_id,
    )
}

/// Loads the content-bound choices that the user explicitly deferred.
///
/// A conflict ID includes the migration namespace plus the current/candidate
/// content hashes, so a changed branch receives a new ID and is not silently
/// treated as deferred. The registry never owns or deletes either branch.
pub(crate) fn load_deferred_conflict_ids(
    data_root: &Path,
    canonical_root: &Path,
    migration_operation_id: &str,
) -> Result<BTreeSet<String>, String> {
    validate_safe_id(migration_operation_id, "migration operation ID")?;
    let path = deferred_conflict_registry_path(data_root);
    let protected = match read_regular_file_bounded(&path, MAX_PROTECTED_DEFERRED_CONFLICT_BYTES) {
        Ok(bytes) => bytes,
        Err(_) if !path.exists() => return Ok(BTreeSet::new()),
        Err(_) => return Err("deferred conflict registry is unreadable".to_string()),
    };
    let bytes = unprotect_deferred_conflict_registry(&protected)?;
    let envelope = serde_json::from_slice::<DeferredConflictRegistryEnvelope>(&bytes)
        .map_err(|_| "deferred conflict registry is invalid".to_string())?;
    validate_deferred_conflict_registry(&envelope.registry)?;
    if envelope.integrity_sha256 != deferred_conflict_registry_digest(&envelope.registry)? {
        return Err("deferred conflict registry integrity check failed".to_string());
    }
    if envelope.registry.migration_operation_id != migration_operation_id
        || envelope.registry.canonical_root_sha256 != canonical_root_digest(canonical_root)
    {
        return Ok(BTreeSet::new());
    }
    Ok(envelope
        .registry
        .deferred_conflict_ids
        .into_iter()
        .collect())
}

pub(crate) fn record_deferred_conflict(
    data_root: &Path,
    canonical_root: &Path,
    migration_operation_id: &str,
    conflict_id: &str,
) -> Result<(), String> {
    validate_safe_id(migration_operation_id, "migration operation ID")?;
    validate_conflict_id(conflict_id)?;
    let mut deferred =
        load_deferred_conflict_ids(data_root, canonical_root, migration_operation_id)?;
    deferred.insert(conflict_id.to_string());
    persist_deferred_conflict_registry(
        data_root,
        canonical_root,
        migration_operation_id,
        deferred,
    )?;
    let verified = load_deferred_conflict_ids(data_root, canonical_root, migration_operation_id)?;
    if !verified.contains(conflict_id) {
        return Err("deferred conflict registry verification failed".to_string());
    }
    Ok(())
}

/// Clears an explicit defer after `UseNewer` has committed. Calling this for a
/// changed conflict ID is idempotent and never removes a conflict payload.
pub(crate) fn clear_deferred_conflict(
    data_root: &Path,
    canonical_root: &Path,
    migration_operation_id: &str,
    conflict_id: &str,
) -> Result<(), String> {
    validate_safe_id(migration_operation_id, "migration operation ID")?;
    validate_conflict_id(conflict_id)?;
    let path = deferred_conflict_registry_path(data_root);
    if !path.exists() {
        return Ok(());
    }
    let mut deferred =
        load_deferred_conflict_ids(data_root, canonical_root, migration_operation_id)?;
    if !deferred.remove(conflict_id) {
        return Ok(());
    }
    persist_deferred_conflict_registry(data_root, canonical_root, migration_operation_id, deferred)
}

fn persist_deferred_conflict_registry(
    data_root: &Path,
    canonical_root: &Path,
    migration_operation_id: &str,
    deferred_conflict_ids: BTreeSet<String>,
) -> Result<(), String> {
    let registry = DeferredConflictRegistry {
        schema_version: DEFERRED_CONFLICT_SCHEMA_VERSION,
        migration_operation_id: migration_operation_id.to_string(),
        canonical_root_sha256: canonical_root_digest(canonical_root),
        deferred_conflict_ids: deferred_conflict_ids.into_iter().collect(),
        updated_at_ms: timestamp_millis()?,
    };
    validate_deferred_conflict_registry(&registry)?;
    let envelope = DeferredConflictRegistryEnvelope {
        integrity_sha256: deferred_conflict_registry_digest(&registry)?,
        registry,
    };
    let plaintext = serde_json::to_vec_pretty(&envelope)
        .map_err(|_| "failed to serialize deferred conflict registry".to_string())?;
    if plaintext.len() as u64 > MAX_DEFERRED_CONFLICT_BYTES {
        return Err("deferred conflict registry reached its size limit".to_string());
    }
    let protected = protect_deferred_conflict_registry(&plaintext)?;
    if protected.len() as u64 > MAX_PROTECTED_DEFERRED_CONFLICT_BYTES {
        return Err("deferred conflict registry reached its size limit".to_string());
    }
    atomic_write(&deferred_conflict_registry_path(data_root), &protected)
}

#[cfg(test)]
pub(crate) fn migration_conflict_candidates_excluding(
    report: &MigrationPreflightReport,
    excluded_conflict_ids: &BTreeSet<String>,
) -> Result<Vec<SessionConflictCandidate>, String> {
    migration_conflict_candidates_for_namespace(report, &report.operation_id, excluded_conflict_ids)
}

pub(crate) fn stable_migration_conflict_candidates(
    canonical_root: &Path,
    data_root: &Path,
    migration_operation_id: &str,
    excluded_conflict_ids: &BTreeSet<String>,
) -> Result<Vec<SessionConflictCandidate>, String> {
    let plan = scan_stable_migration_plan(canonical_root, data_root, migration_operation_id)?;
    let report = conflict_report_from_plan(plan);
    migration_conflict_candidates_for_namespace(
        &report,
        migration_operation_id,
        excluded_conflict_ids,
    )
}

pub(crate) fn migration_conflict_candidates_for_namespace(
    report: &MigrationPreflightReport,
    migration_operation_id: &str,
    excluded_conflict_ids: &BTreeSet<String>,
) -> Result<Vec<SessionConflictCandidate>, String> {
    report
        .plan
        .conflicts
        .iter()
        .filter_map(|conflict| {
            let planned_conflict_id = match planned_conflict_id(migration_operation_id, conflict) {
                Ok(conflict_id) => conflict_id,
                Err(error) => return Some(Err(error)),
            };
            let refreshed = match refresh_planned_conflict(conflict) {
                Ok(Some(refreshed)) => refreshed,
                Ok(None) => return None,
                // A committed resolution may remove its candidate before the
                // migration proof expires. Suppress that expected disappearance
                // only for the exact content-bound identity that was resolved.
                // If the candidate still exists but either branch is invalid or
                // changed, fail closed or produce a new non-deferred identity.
                Err(_)
                    if excluded_conflict_ids.contains(&planned_conflict_id)
                        && !conflict.candidate_path.exists() =>
                {
                    return None;
                }
                Err(error) => return Some(Err(error)),
            };
            let summary = match summarize_conflict(migration_operation_id, &refreshed) {
                Ok(summary) => summary,
                Err(error) => return Some(Err(error)),
            };
            if excluded_conflict_ids.contains(&summary.conflict_id) {
                return None;
            }
            Some(Ok(SessionConflictCandidate {
                summary,
                resolution_report: Some(single_conflict_report(
                    migration_operation_id,
                    &report.plan.canonical_root,
                    refreshed,
                )),
                pending_recovery_entry_ids: Vec::new(),
            }))
        })
        .collect()
}

pub(crate) fn restore_import_conflict_candidates(
    migration_operation_id: &str,
    plan: &RestoreImportPlan,
) -> Result<Vec<SessionConflictCandidate>, String> {
    let mut candidates = Vec::new();
    for conflict in &plan.conflicts {
        for ((candidate_path, candidate_sha256), recovery_path) in conflict
            .candidate_paths
            .iter()
            .zip(&conflict.candidate_sha256)
            .zip(&conflict.recovery_paths)
        {
            if !candidate_path.starts_with(&plan.package_dir)
                || !recovery_path.starts_with(&plan.recovery_root)
            {
                return Err(
                    "restore import conflict source is outside its managed roots".to_string(),
                );
            }
            let current = conflict
                .current_path
                .as_ref()
                .map(|path| ConflictVersionSource {
                    path: path.clone(),
                    expected_sha256: conflict.current_sha256.clone(),
                    origin: FileOrigin::CanonicalHome,
                    marker_status: MarkerStatus::Absent,
                });
            let candidate = ConflictVersionSource {
                path: recovery_path.clone(),
                expected_sha256: Some(candidate_sha256.clone()),
                origin: FileOrigin::RecoveryPackage,
                marker_status: MarkerStatus::Absent,
            };
            match build_candidate(
                migration_operation_id,
                &plan.canonical_root,
                &conflict.thread_id,
                current,
                candidate,
                conflict.relation,
                Vec::new(),
            )? {
                CandidateBuildResult::Conflict(candidate) => candidates.push(*candidate),
                CandidateBuildResult::CanonicalContainsCandidate => {}
            }
        }
    }
    Ok(candidates)
}

pub(crate) fn pending_recovery_conflict_candidates(
    canonical_root: &Path,
    data_root: &Path,
    migration_operation_id: &str,
    pending: &PendingRecoveryList,
    inventory: &MigrationInventory,
    now_ms: u128,
) -> Result<PendingRecoveryConflictScan, String> {
    let mut scan = PendingRecoveryConflictScan::default();
    for entry in pending.entries.iter().filter(|entry| {
        entry.status == PendingRecoveryStatus::Pending
            && entry.expires_at_ms > now_ms
            && matches!(
                entry.relation,
                PendingRecoveryRelation::Divergent | PendingRecoveryRelation::Unknown
            )
    }) {
        let source =
            load_pending_recovery_source(data_root, migration_operation_id, &entry.entry_id)?;
        let current = select_current_version(inventory, &source.thread_id).map(|file| {
            ConflictVersionSource {
                path: file.path.clone(),
                expected_sha256: Some(file.raw_sha256.clone()),
                origin: file.origin,
                marker_status: file.marker_status,
            }
        });
        let declared_relation = match source.relation {
            PendingRecoveryRelation::Divergent => SessionRelation::Divergent,
            PendingRecoveryRelation::Unknown => SessionRelation::Unknown,
            PendingRecoveryRelation::MissingFromCanonical
            | PendingRecoveryRelation::ExtendsCanonical => continue,
        };
        let candidate = ConflictVersionSource {
            path: source.payload_path,
            expected_sha256: Some(source.payload_sha256),
            origin: FileOrigin::RecoveryPackage,
            marker_status: MarkerStatus::Absent,
        };
        match build_candidate(
            migration_operation_id,
            canonical_root,
            &source.thread_id,
            current,
            candidate,
            declared_relation,
            vec![source.entry_id],
        )? {
            CandidateBuildResult::Conflict(candidate) => scan.candidates.push(*candidate),
            CandidateBuildResult::CanonicalContainsCandidate => {
                scan.resolved_by_content_entry_ids
                    .push(entry.entry_id.clone());
            }
        }
    }
    Ok(scan)
}

pub(crate) fn list_migration_conflicts_excluding(
    report: &MigrationPreflightReport,
    resolved_conflict_ids: &BTreeSet<String>,
) -> Result<SessionConflictList, String> {
    let mut conflicts = report
        .plan
        .conflicts
        .iter()
        .filter_map(
            |conflict| match planned_conflict_id(&report.operation_id, conflict) {
                Ok(conflict_id) if resolved_conflict_ids.contains(&conflict_id) => None,
                Ok(_) => Some(summarize_conflict(&report.operation_id, conflict)),
                Err(error) => Some(Err(error)),
            },
        )
        .collect::<Result<Vec<_>, _>>()?;
    conflicts.sort_by(|left, right| left.conflict_id.cmp(&right.conflict_id));
    Ok(SessionConflictList {
        migration_operation_id: report.operation_id.clone(),
        conflicts,
    })
}

pub(crate) fn resolve_conflict_by_id<'a>(
    report: &'a MigrationPreflightReport,
    conflict_id: &str,
) -> Result<(&'a MigrationConflictPlan, SessionConflictSummary), String> {
    for conflict in &report.plan.conflicts {
        if planned_conflict_id(&report.operation_id, conflict)? == conflict_id {
            let summary = summarize_conflict(&report.operation_id, conflict)?;
            return Ok((conflict, summary));
        }
    }
    Err("session conflict identity is unavailable".to_string())
}

fn summarize_conflict(
    operation_id: &str,
    conflict: &MigrationConflictPlan,
) -> Result<SessionConflictSummary, String> {
    let current = validated_version(
        &conflict.current_path,
        &conflict.thread_id,
        conflict.current_sha256.as_deref(),
    )?;
    let candidate = validated_version(
        &conflict.candidate_path,
        &conflict.thread_id,
        Some(&conflict.candidate_sha256),
    )?;
    let relation = compare_sessions(&current, &candidate);
    if !matches!(
        relation,
        SessionRelation::Divergent
            | SessionRelation::Unknown
            | SessionRelation::LeftPrefix
            | SessionRelation::RightPrefix
    ) {
        return Err("session conflict content no longer requires a decision".to_string());
    }
    let common_prefix = current
        .message_line_sha256
        .iter()
        .zip(&candidate.message_line_sha256)
        .take_while(|(left, right)| left == right)
        .count();
    let newer_version = reliable_newer_version(
        current.last_message_timestamp.as_deref(),
        candidate.last_message_timestamp.as_deref(),
    );
    Ok(SessionConflictSummary {
        conflict_id: planned_conflict_id(operation_id, conflict)?,
        deferred: false,
        current_message_count: current.message_count,
        candidate_message_count: candidate.message_count,
        current_added_message_count: current.message_count.saturating_sub(common_prefix),
        candidate_added_message_count: candidate.message_count.saturating_sub(common_prefix),
        current_last_message_at: current.last_message_timestamp.clone(),
        candidate_last_message_at: candidate.last_message_timestamp.clone(),
        current_provider: current.initial_provider.clone(),
        candidate_provider: candidate.initial_provider.clone(),
        current_origin: conflict.current_origin,
        candidate_origin: conflict.candidate_origin,
        relation,
        newer_version,
        default_overwrite: false,
    })
}

fn refresh_planned_conflict(
    conflict: &MigrationConflictPlan,
) -> Result<Option<MigrationConflictPlan>, String> {
    let current = validated_version(&conflict.current_path, &conflict.thread_id, None)?;
    let candidate = validated_version(&conflict.candidate_path, &conflict.thread_id, None)?;
    let relation = compare_sessions(&current, &candidate);
    if matches!(
        relation,
        SessionRelation::Equal
            | SessionRelation::EqualExceptProvider
            | SessionRelation::RightPrefix
    ) {
        return Ok(None);
    }
    if !matches!(
        relation,
        SessionRelation::Divergent | SessionRelation::Unknown | SessionRelation::LeftPrefix
    ) {
        return Err("session conflict content relation is invalid".to_string());
    }
    Ok(Some(MigrationConflictPlan {
        thread_id: conflict.thread_id.clone(),
        current_path: conflict.current_path.clone(),
        candidate_path: conflict.candidate_path.clone(),
        canonical_path: conflict.canonical_path.clone(),
        current_sha256: Some(hex_digest(current.raw_sha256)),
        candidate_sha256: hex_digest(candidate.raw_sha256),
        current_origin: conflict.current_origin,
        candidate_origin: conflict.candidate_origin,
        current_marker_status: inspect_provider_marker(&conflict.current_path, Some(&current)),
        candidate_marker_status: inspect_provider_marker(
            &conflict.candidate_path,
            Some(&candidate),
        ),
        current_message_count: current.message_count,
        candidate_message_count: candidate.message_count,
        current_last_message_at: current.last_message_timestamp.clone(),
        candidate_last_message_at: candidate.last_message_timestamp.clone(),
        current_provider: current.initial_provider.clone(),
        candidate_provider: candidate.initial_provider.clone(),
        relation,
        default_overwrite: false,
    }))
}

fn build_candidate(
    migration_operation_id: &str,
    canonical_root: &Path,
    thread_id: &str,
    current: Option<ConflictVersionSource>,
    candidate: ConflictVersionSource,
    declared_relation: SessionRelation,
    pending_recovery_entry_ids: Vec<String>,
) -> Result<CandidateBuildResult, String> {
    let candidate_sha256 = candidate
        .expected_sha256
        .as_deref()
        .ok_or_else(|| "session conflict candidate checksum is unavailable".to_string())?;
    decode_sha256(candidate_sha256)?;
    let current_expected_sha256 = current
        .as_ref()
        .and_then(|source| source.expected_sha256.as_deref());
    if let Some(sha256) = current_expected_sha256 {
        decode_sha256(sha256)?;
    }

    let candidate_semantic = read_semantic_session(&candidate.path)
        .ok()
        .filter(|semantic| {
            semantic.thread_id == thread_id && hex_digest(semantic.raw_sha256) == candidate_sha256
        });
    let current_semantic = current.as_ref().and_then(|source| {
        read_semantic_session(&source.path).ok().filter(|semantic| {
            semantic.thread_id == thread_id
                && source
                    .expected_sha256
                    .as_deref()
                    .is_none_or(|expected| hex_digest(semantic.raw_sha256) == expected)
        })
    });
    let current_sha256 = current_semantic
        .as_ref()
        .map(|semantic| hex_digest(semantic.raw_sha256))
        .or_else(|| current_expected_sha256.map(str::to_string));
    let conflict_id = conflict_id(
        migration_operation_id,
        thread_id,
        current_sha256.as_deref(),
        candidate_sha256,
    )?;

    let Some(candidate_semantic) = candidate_semantic else {
        return Ok(CandidateBuildResult::Conflict(Box::new(
            SessionConflictCandidate {
                summary: unknown_summary(
                    conflict_id,
                    current_semantic.as_ref(),
                    None,
                    current
                        .as_ref()
                        .map(|source| source.origin)
                        .unwrap_or(FileOrigin::CanonicalHome),
                    candidate.origin,
                ),
                resolution_report: None,
                pending_recovery_entry_ids,
            },
        )));
    };
    let Some(current_semantic) = current_semantic else {
        return Ok(CandidateBuildResult::Conflict(Box::new(
            SessionConflictCandidate {
                summary: unknown_summary(
                    conflict_id,
                    None,
                    Some(&candidate_semantic),
                    current
                        .as_ref()
                        .map(|source| source.origin)
                        .unwrap_or(FileOrigin::CanonicalHome),
                    candidate.origin,
                ),
                resolution_report: None,
                pending_recovery_entry_ids,
            },
        )));
    };
    let actual_relation = compare_sessions(&current_semantic, &candidate_semantic);
    if matches!(
        actual_relation,
        SessionRelation::Equal
            | SessionRelation::EqualExceptProvider
            | SessionRelation::RightPrefix
    ) {
        return Ok(CandidateBuildResult::CanonicalContainsCandidate);
    }
    let current = current.expect("a semantic current version has a source");
    let current_sha256 = hex_digest(current_semantic.raw_sha256);
    let current_marker_status = if current.marker_status == MarkerStatus::Absent {
        inspect_provider_marker(&current.path, Some(&current_semantic))
    } else {
        current.marker_status
    };
    let resolvable = declared_relation != SessionRelation::Unknown
        && matches!(
            actual_relation,
            SessionRelation::LeftPrefix | SessionRelation::Divergent
        );
    if !resolvable {
        return Ok(CandidateBuildResult::Conflict(Box::new(
            SessionConflictCandidate {
                summary: unknown_summary(
                    conflict_id,
                    Some(&current_semantic),
                    Some(&candidate_semantic),
                    current.origin,
                    candidate.origin,
                ),
                resolution_report: None,
                pending_recovery_entry_ids,
            },
        )));
    }
    let conflict = MigrationConflictPlan {
        thread_id: thread_id.to_string(),
        current_path: current.path.clone(),
        candidate_path: candidate.path.clone(),
        canonical_path: current.path,
        current_sha256: Some(current_sha256),
        candidate_sha256: candidate_sha256.to_string(),
        current_origin: current.origin,
        candidate_origin: candidate.origin,
        current_marker_status,
        candidate_marker_status: candidate.marker_status,
        current_message_count: current_semantic.message_count,
        candidate_message_count: candidate_semantic.message_count,
        current_last_message_at: current_semantic.last_message_timestamp.clone(),
        candidate_last_message_at: candidate_semantic.last_message_timestamp.clone(),
        current_provider: current_semantic.initial_provider.clone(),
        candidate_provider: candidate_semantic.initial_provider.clone(),
        relation: actual_relation,
        default_overwrite: false,
    };
    let summary = summarize_conflict(migration_operation_id, &conflict)?;
    Ok(CandidateBuildResult::Conflict(Box::new(
        SessionConflictCandidate {
            summary,
            resolution_report: Some(single_conflict_report(
                migration_operation_id,
                canonical_root,
                conflict,
            )),
            pending_recovery_entry_ids,
        },
    )))
}

fn unknown_summary(
    conflict_id: String,
    current: Option<&SemanticSession>,
    candidate: Option<&SemanticSession>,
    current_origin: FileOrigin,
    candidate_origin: FileOrigin,
) -> SessionConflictSummary {
    let common_prefix = current
        .zip(candidate)
        .map(|(current, candidate)| {
            current
                .message_line_sha256
                .iter()
                .zip(&candidate.message_line_sha256)
                .take_while(|(left, right)| left == right)
                .count()
        })
        .unwrap_or(0);
    SessionConflictSummary {
        conflict_id,
        deferred: false,
        current_message_count: current.map(|value| value.message_count).unwrap_or(0),
        candidate_message_count: candidate.map(|value| value.message_count).unwrap_or(0),
        current_added_message_count: current
            .map(|value| value.message_count.saturating_sub(common_prefix))
            .unwrap_or(0),
        candidate_added_message_count: candidate
            .map(|value| value.message_count.saturating_sub(common_prefix))
            .unwrap_or(0),
        current_last_message_at: current.and_then(|value| value.last_message_timestamp.clone()),
        candidate_last_message_at: candidate.and_then(|value| value.last_message_timestamp.clone()),
        current_provider: current.and_then(|value| value.initial_provider.clone()),
        candidate_provider: candidate.and_then(|value| value.initial_provider.clone()),
        current_origin,
        candidate_origin,
        relation: SessionRelation::Unknown,
        newer_version: None,
        default_overwrite: false,
    }
}

fn single_conflict_report(
    operation_id: &str,
    canonical_root: &Path,
    conflict: MigrationConflictPlan,
) -> MigrationPreflightReport {
    MigrationPreflightReport {
        schema_version: 1,
        operation_id: operation_id.to_string(),
        generated_at_ms: 0,
        canonical_session_count: 0,
        session_file_count: 2,
        provider_copy_count: 0,
        conflict_count: 1,
        anomaly_count: 0,
        estimated_reclaim_bytes: 0,
        backup_source_bytes: 0,
        required_backup_bytes: 0,
        available_backup_bytes: u64::MAX,
        backup_destination: canonical_root.to_path_buf(),
        blockers: Vec::new(),
        ready_for_backup: true,
        plan: CanonicalMigrationPlan {
            schema_version: 1,
            operation_id: operation_id.to_string(),
            generated_at_ms: 0,
            canonical_root: canonical_root.to_path_buf(),
            inventory_fingerprint: "0".repeat(64),
            sessions: Vec::new(),
            conflicts: vec![conflict],
            databases: Vec::new(),
            unclassified_file_count: 0,
            invalid_marker_count: 0,
            missing_runtime_reference_count: 0,
            mismatched_runtime_reference_count: 0,
        },
    }
}

fn conflict_report_from_plan(plan: CanonicalMigrationPlan) -> MigrationPreflightReport {
    let operation_id = plan.operation_id.clone();
    let generated_at_ms = plan.generated_at_ms;
    let canonical_session_count = plan.sessions.len();
    let conflict_count = plan.conflicts.len();
    let anomaly_count = plan
        .unclassified_file_count
        .saturating_add(plan.invalid_marker_count)
        .saturating_add(plan.missing_runtime_reference_count)
        .saturating_add(plan.mismatched_runtime_reference_count);
    let backup_destination = plan.canonical_root.clone();
    MigrationPreflightReport {
        schema_version: plan.schema_version,
        operation_id,
        generated_at_ms,
        canonical_session_count,
        session_file_count: 0,
        provider_copy_count: 0,
        conflict_count,
        anomaly_count,
        estimated_reclaim_bytes: 0,
        backup_source_bytes: 0,
        required_backup_bytes: 0,
        available_backup_bytes: 0,
        backup_destination,
        blockers: Vec::new(),
        ready_for_backup: false,
        plan,
    }
}

fn select_current_version<'a>(
    inventory: &'a MigrationInventory,
    thread_id: &str,
) -> Option<&'a MigrationInventoryFile> {
    let referenced_paths = inventory
        .graph
        .files
        .iter()
        .filter(|file| {
            file.origin == FileOrigin::CanonicalHome
                && file.thread_id.as_deref() == Some(thread_id)
                && !file.runtime_database_ids.is_empty()
        })
        .map(|file| file.path_key.clone())
        .collect::<BTreeSet<_>>();
    let candidates = inventory
        .files
        .iter()
        .filter(|file| {
            file.origin == FileOrigin::CanonicalHome
                && file
                    .semantic
                    .as_ref()
                    .is_ok_and(|semantic| semantic.thread_id == thread_id)
        })
        .collect::<Vec<_>>();
    if referenced_paths.len() == 1 {
        let referenced = referenced_paths.iter().next()?;
        return candidates
            .into_iter()
            .find(|file| path_key(&file.path) == *referenced);
    }
    if referenced_paths.is_empty() && candidates.len() == 1 {
        candidates.into_iter().next()
    } else {
        None
    }
}

fn validated_version(
    path: &Path,
    thread_id: &str,
    expected_sha256: Option<&str>,
) -> Result<SemanticSession, String> {
    let semantic = read_semantic_session(path)
        .map_err(|_| "session conflict version is invalid".to_string())?;
    let actual_sha256 = hex_digest(semantic.raw_sha256);
    if semantic.thread_id == thread_id
        && expected_sha256.is_none_or(|expected| expected == actual_sha256)
    {
        Ok(semantic)
    } else {
        Err("session conflict identity or content changed".to_string())
    }
}

fn reliable_newer_version(
    current_timestamp: Option<&str>,
    candidate_timestamp: Option<&str>,
) -> Option<ConflictVersion> {
    let current = parse_rfc3339_millis(current_timestamp?)?;
    let candidate = parse_rfc3339_millis(candidate_timestamp?)?;
    match current.cmp(&candidate) {
        std::cmp::Ordering::Greater => Some(ConflictVersion::Current),
        std::cmp::Ordering::Less => Some(ConflictVersion::Candidate),
        std::cmp::Ordering::Equal => None,
    }
}

pub(crate) fn planned_conflict_id(
    operation_id: &str,
    conflict: &MigrationConflictPlan,
) -> Result<String, String> {
    conflict_id(
        operation_id,
        &conflict.thread_id,
        conflict.current_sha256.as_deref(),
        &conflict.candidate_sha256,
    )
}

fn conflict_id(
    operation_id: &str,
    thread_id: &str,
    current_sha256: Option<&str>,
    candidate_sha256: &str,
) -> Result<String, String> {
    let current = current_sha256.map(decode_sha256).transpose()?;
    let candidate = decode_sha256(candidate_sha256)?;
    let mut hasher = Sha256::new();
    let current_bytes = current
        .as_ref()
        .map(|value| value.as_slice())
        .unwrap_or(&[]);
    for value in [
        operation_id.as_bytes(),
        thread_id.as_bytes(),
        current_bytes,
        candidate.as_slice(),
    ] {
        hasher.update((value.len() as u64).to_le_bytes());
        hasher.update(value);
    }
    Ok(format!("conflict-{:x}", hasher.finalize()))
}

fn decode_sha256(value: &str) -> Result<[u8; 32], String> {
    if value.len() != 64 {
        return Err("session conflict checksum is invalid".to_string());
    }
    let mut output = [0_u8; 32];
    for (index, chunk) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let encoded = std::str::from_utf8(chunk)
            .map_err(|_| "session conflict checksum is invalid".to_string())?;
        output[index] = u8::from_str_radix(encoded, 16)
            .map_err(|_| "session conflict checksum is invalid".to_string())?;
    }
    Ok(output)
}

fn hex_digest(digest: impl AsRef<[u8]>) -> String {
    digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn resolved_conflict_registry_path(data_root: &Path) -> PathBuf {
    data_root
        .join("session-storage-v1")
        .join(RESOLVED_CONFLICT_NAME)
}

fn deferred_conflict_registry_path(data_root: &Path) -> PathBuf {
    data_root
        .join("session-storage-v1")
        .join(DEFERRED_CONFLICT_NAME)
}

fn canonical_root_digest(canonical_root: &Path) -> String {
    hex_digest(Sha256::digest(path_key(canonical_root).as_bytes()))
}

fn resolved_conflict_registry_digest(
    registry: &ResolvedConflictRegistry,
) -> Result<String, String> {
    let bytes = serde_json::to_vec(registry)
        .map_err(|_| "failed to fingerprint resolved conflict registry".to_string())?;
    Ok(hex_digest(Sha256::digest(bytes)))
}

fn deferred_conflict_registry_digest(
    registry: &DeferredConflictRegistry,
) -> Result<String, String> {
    let bytes = serde_json::to_vec(registry)
        .map_err(|_| "failed to fingerprint deferred conflict registry".to_string())?;
    Ok(hex_digest(Sha256::digest(bytes)))
}

fn protect_deferred_conflict_registry(plaintext: &[u8]) -> Result<Vec<u8>, String> {
    #[cfg(windows)]
    {
        let ciphertext = crate::crypto::protect(plaintext)
            .map_err(|_| "failed to protect deferred conflict registry".to_string())?;
        let mut protected =
            Vec::with_capacity(DEFERRED_CONFLICT_CIPHERTEXT_MAGIC.len() + ciphertext.len());
        protected.extend_from_slice(DEFERRED_CONFLICT_CIPHERTEXT_MAGIC);
        protected.extend_from_slice(&ciphertext);
        Ok(protected)
    }
    #[cfg(not(windows))]
    {
        Ok(plaintext.to_vec())
    }
}

fn unprotect_deferred_conflict_registry(protected: &[u8]) -> Result<Vec<u8>, String> {
    #[cfg(windows)]
    {
        let ciphertext = protected
            .strip_prefix(DEFERRED_CONFLICT_CIPHERTEXT_MAGIC)
            .ok_or_else(|| "deferred conflict registry is invalid".to_string())?;
        let plaintext = crate::crypto::unprotect(ciphertext)
            .map_err(|_| "deferred conflict registry is unreadable".to_string())?;
        if plaintext.len() as u64 > MAX_DEFERRED_CONFLICT_BYTES {
            return Err("deferred conflict registry reached its size limit".to_string());
        }
        Ok(plaintext)
    }
    #[cfg(not(windows))]
    {
        if protected.len() as u64 > MAX_DEFERRED_CONFLICT_BYTES {
            return Err("deferred conflict registry reached its size limit".to_string());
        }
        Ok(protected.to_vec())
    }
}

fn validate_resolved_conflict_registry(registry: &ResolvedConflictRegistry) -> Result<(), String> {
    validate_safe_id(&registry.migration_operation_id, "migration operation ID")?;
    decode_sha256(&registry.canonical_root_sha256)?;
    if registry.schema_version != RESOLVED_CONFLICT_SCHEMA_VERSION
        || registry.updated_at_ms == 0
        || registry
            .resolved_conflict_ids
            .windows(2)
            .any(|items| items[0] >= items[1])
    {
        return Err("resolved conflict registry shape is invalid".to_string());
    }
    for conflict_id in &registry.resolved_conflict_ids {
        validate_conflict_id(conflict_id)?;
    }
    Ok(())
}

fn validate_deferred_conflict_registry(registry: &DeferredConflictRegistry) -> Result<(), String> {
    validate_safe_id(&registry.migration_operation_id, "migration operation ID")?;
    decode_sha256(&registry.canonical_root_sha256)?;
    if registry.schema_version != DEFERRED_CONFLICT_SCHEMA_VERSION
        || registry.updated_at_ms == 0
        || registry
            .deferred_conflict_ids
            .windows(2)
            .any(|items| items[0] >= items[1])
    {
        return Err("deferred conflict registry shape is invalid".to_string());
    }
    for conflict_id in &registry.deferred_conflict_ids {
        validate_conflict_id(conflict_id)?;
    }
    Ok(())
}

fn validate_conflict_id(conflict_id: &str) -> Result<(), String> {
    let Some(digest) = conflict_id.strip_prefix("conflict-") else {
        return Err("session conflict identity is invalid".to_string());
    };
    if digest.len() != 64 || !digest.bytes().all(is_lower_hex) {
        return Err("session conflict identity is invalid".to_string());
    }
    Ok(())
}

fn validate_safe_id(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 160
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        Err(format!("{label} is invalid"))
    } else {
        Ok(())
    }
}

fn is_lower_hex(byte: u8) -> bool {
    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, fs, path::Path};

    use tempfile::tempdir;

    use super::{
        clear_deferred_conflict, list_migration_conflicts, load_deferred_conflict_ids,
        load_resolved_conflict_ids, migration_conflict_candidates_excluding,
        migration_conflict_candidates_for_namespace, record_deferred_conflict,
        record_resolved_conflict, restore_import_conflict_candidates, ConflictVersion,
    };
    use crate::session_storage::{
        migration::{CanonicalMigrationPlan, MigrationConflictPlan, MigrationPreflightReport},
        restore_import::{RestoreImportConflictPlan, RestoreImportPlan, RestoreImportSourceKind},
        FileOrigin, SessionRelation,
    };

    fn write_session(path: &Path, provider: &str, timestamp: &str, messages: &[&str]) {
        let mut lines = vec![serde_json::json!({
            "type": "session_meta",
            "timestamp": "2026-08-12T00:00:00Z",
            "payload": {"id": "thread-a", "model_provider": provider}
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
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, lines.join("\n") + "\n").unwrap();
    }

    #[test]
    fn summary_is_path_free_default_no_overwrite_and_recommends_only_reliable_time() {
        let root = tempdir().unwrap();
        let current = root.path().join("home/sessions/current.jsonl");
        let candidate = root.path().join("shared/candidate.jsonl");
        write_session(
            &current,
            "openai",
            "2026-08-12T00:00:01Z",
            &["common", "left"],
        );
        write_session(
            &candidate,
            "openai_custom",
            "2026-08-12T00:00:02Z",
            &["common", "right", "new"],
        );
        let report = MigrationPreflightReport {
            schema_version: 1,
            operation_id: "migration-1".to_string(),
            generated_at_ms: 1,
            canonical_session_count: 0,
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
                canonical_root: root.path().join("home"),
                inventory_fingerprint: "a".repeat(64),
                sessions: Vec::new(),
                conflicts: vec![MigrationConflictPlan {
                    thread_id: "thread-a".to_string(),
                    current_path: current,
                    candidate_path: candidate,
                    canonical_path: root.path().join("home/sessions/current.jsonl"),
                    current_sha256: Some(super::hex_digest(
                        crate::session_storage::semantic::read_semantic_session(
                            &root.path().join("home/sessions/current.jsonl"),
                        )
                        .unwrap()
                        .raw_sha256,
                    )),
                    candidate_sha256: super::hex_digest(
                        crate::session_storage::semantic::read_semantic_session(
                            &root.path().join("shared/candidate.jsonl"),
                        )
                        .unwrap()
                        .raw_sha256,
                    ),
                    current_origin: crate::session_storage::model::FileOrigin::CanonicalHome,
                    candidate_origin: crate::session_storage::model::FileOrigin::Shared,
                    current_marker_status: crate::session_storage::model::MarkerStatus::Absent,
                    candidate_marker_status: crate::session_storage::model::MarkerStatus::Absent,
                    current_message_count: 2,
                    candidate_message_count: 3,
                    current_last_message_at: None,
                    candidate_last_message_at: None,
                    current_provider: None,
                    candidate_provider: None,
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

        let list = list_migration_conflicts(&report).unwrap();
        let summary = &list.conflicts[0];
        assert!(summary.conflict_id.starts_with("conflict-"));
        assert!(!summary.deferred);
        assert!(!summary.default_overwrite);
        assert_eq!(summary.newer_version, Some(ConflictVersion::Candidate));
        assert_eq!(summary.current_added_message_count, 1);
        assert_eq!(summary.candidate_added_message_count, 2);
        let encoded = serde_json::to_string(summary).unwrap();
        assert!(!encoded.contains(root.path().to_string_lossy().as_ref()));
        assert!(!encoded.contains("thread-a"));
        let conflict_id = summary.conflict_id.clone();

        let namespaced = migration_conflict_candidates_for_namespace(
            &report,
            "migration-canonical",
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(namespaced.len(), 1);
        assert_ne!(namespaced[0].summary.conflict_id, conflict_id);
        assert_eq!(
            namespaced[0]
                .resolution_report
                .as_ref()
                .unwrap()
                .operation_id,
            "migration-canonical"
        );

        assert!(migration_conflict_candidates_excluding(
            &report,
            &BTreeSet::from([conflict_id.clone()]),
        )
        .unwrap()
        .is_empty());
        write_session(
            &report.plan.conflicts[0].candidate_path,
            "openai_custom",
            "2026-08-12T00:00:03Z",
            &["common", "changed"],
        );
        let changed = migration_conflict_candidates_excluding(
            &report,
            &BTreeSet::from([conflict_id.clone()]),
        )
        .unwrap();
        assert_eq!(changed.len(), 1);
        assert_ne!(changed[0].summary.conflict_id, conflict_id);

        write_session(
            &report.plan.conflicts[0].current_path,
            "openai",
            // A real continuation retains the exact earlier message lines.
            // Keep their timestamps aligned with the candidate so this fixture
            // proves canonical containment rather than a timestamp divergence.
            "2026-08-12T00:00:03Z",
            &["common", "changed", "continued"],
        );
        assert!(migration_conflict_candidates_for_namespace(
            &report,
            "migration-canonical",
            &BTreeSet::new(),
        )
        .unwrap()
        .is_empty());

        fs::remove_file(&report.plan.conflicts[0].candidate_path).unwrap();
        assert!(migration_conflict_candidates_excluding(&report, &BTreeSet::new()).is_err());
        assert!(
            migration_conflict_candidates_excluding(&report, &BTreeSet::from([conflict_id]),)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn restore_import_divergence_uses_managed_recovery_copy_without_exposing_paths() {
        let root = tempdir().unwrap();
        let canonical = root.path().join("home/sessions/current.jsonl");
        let package_dir = root.path().join("downgrade");
        let package_candidate = package_dir.join("codex-home/sessions/candidate.jsonl");
        let recovery_root = root.path().join("data/recovery/restore-1");
        let recovery_candidate = recovery_root.join("conflicts/thread-a/candidate.jsonl");
        write_session(
            &canonical,
            "openai",
            "2026-08-12T00:00:01Z",
            &["common", "left"],
        );
        write_session(
            &package_candidate,
            "openai_custom",
            "2026-08-12T00:00:02Z",
            &["common", "right", "new"],
        );
        fs::create_dir_all(recovery_candidate.parent().unwrap()).unwrap();
        fs::copy(&package_candidate, &recovery_candidate).unwrap();
        let current_sha256 = super::hex_digest(
            crate::session_storage::semantic::read_semantic_session(&canonical)
                .unwrap()
                .raw_sha256,
        );
        let candidate_sha256 = super::hex_digest(
            crate::session_storage::semantic::read_semantic_session(&recovery_candidate)
                .unwrap()
                .raw_sha256,
        );
        let plan = RestoreImportPlan {
            schema_version: 1,
            operation_id: "restore-1".to_string(),
            generated_at_ms: 1,
            package_operation_id: "downgrade-1".to_string(),
            target_version: "v0.2.7".to_string(),
            source_kind: RestoreImportSourceKind::DowngradePackage,
            package_dir,
            canonical_root: root.path().join("home"),
            data_root: root.path().join("data"),
            source_fingerprint: "a".repeat(64),
            work_root: root.path().join("data/operations/restore-1"),
            staging_root: root.path().join("data/operations/restore-1/staging"),
            recovery_root,
            recovery_expires_at_ms: u128::MAX,
            sessions: Vec::new(),
            conflicts: vec![RestoreImportConflictPlan {
                thread_id: "thread-a".to_string(),
                current_path: Some(canonical),
                current_sha256: Some(current_sha256),
                candidate_paths: vec![package_candidate],
                candidate_sha256: vec![candidate_sha256],
                recovery_paths: vec![recovery_candidate.clone()],
                relation: SessionRelation::Divergent,
                reason: "histories diverged".to_string(),
                default_overwrite: false,
            }],
            unclassified_payloads: Vec::new(),
            source_databases: Vec::new(),
            databases: Vec::new(),
            anomaly_count: 0,
        };

        let candidates = restore_import_conflict_candidates("migration-1", &plan).unwrap();
        assert_eq!(candidates.len(), 1);
        let candidate = &candidates[0];
        assert_eq!(
            candidate.summary.candidate_origin,
            FileOrigin::RecoveryPackage
        );
        assert_eq!(
            candidate.summary.newer_version,
            Some(ConflictVersion::Candidate)
        );
        let report = candidate.resolution_report.as_ref().unwrap();
        assert_eq!(report.plan.conflicts[0].candidate_path, recovery_candidate);
        let encoded = serde_json::to_string(&candidate.summary).unwrap();
        assert!(!encoded.contains(root.path().to_string_lossy().as_ref()));
        assert!(!encoded.contains("thread-a"));
        assert!(!encoded.contains("common"));
    }

    #[test]
    fn resolved_conflict_registry_is_integrity_bound_and_scoped_to_migration() {
        let root = tempdir().unwrap();
        let data = root.path().join("data");
        let canonical = root.path().join("home");
        fs::create_dir_all(data.join("session-storage-v1")).unwrap();
        fs::create_dir_all(&canonical).unwrap();
        let conflict_id = format!("conflict-{}", "a".repeat(64));

        record_resolved_conflict(&data, &canonical, "migration-1", &conflict_id).unwrap();
        assert!(load_resolved_conflict_ids(&data, &canonical, "migration-1")
            .unwrap()
            .contains(&conflict_id));
        assert!(load_resolved_conflict_ids(&data, &canonical, "migration-2")
            .unwrap()
            .is_empty());

        let path = data.join("session-storage-v1/resolved-conflicts-v1.json");
        let mut bytes = fs::read(&path).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        fs::write(path, bytes).unwrap();
        assert!(load_resolved_conflict_ids(&data, &canonical, "migration-1").is_err());
    }

    #[test]
    fn deferred_conflict_registry_is_protected_content_scoped_and_clearable() {
        let root = tempdir().unwrap();
        let data = root.path().join("data");
        let canonical = root.path().join("home");
        let other_canonical = root.path().join("other-home");
        fs::create_dir_all(data.join("session-storage-v1")).unwrap();
        fs::create_dir_all(&canonical).unwrap();
        fs::create_dir_all(&other_canonical).unwrap();
        let conflict_id = format!("conflict-{}", "a".repeat(64));
        let changed_content_id = format!("conflict-{}", "b".repeat(64));

        record_deferred_conflict(&data, &canonical, "migration-1", &conflict_id).unwrap();
        record_deferred_conflict(&data, &canonical, "migration-1", &conflict_id).unwrap();
        let deferred = load_deferred_conflict_ids(&data, &canonical, "migration-1").unwrap();
        assert_eq!(deferred, BTreeSet::from([conflict_id.clone()]));
        assert!(!deferred.contains(&changed_content_id));
        assert!(load_deferred_conflict_ids(&data, &canonical, "migration-2")
            .unwrap()
            .is_empty());
        assert!(
            load_deferred_conflict_ids(&data, &other_canonical, "migration-1")
                .unwrap()
                .is_empty()
        );

        let path = data.join("session-storage-v1/deferred-conflicts-v1.dpapi");
        let protected = fs::read(&path).unwrap();
        #[cfg(windows)]
        {
            assert!(protected.starts_with(super::DEFERRED_CONFLICT_CIPHERTEXT_MAGIC));
            assert!(!protected
                .windows(conflict_id.len())
                .any(|window| window == conflict_id.as_bytes()));
        }

        clear_deferred_conflict(&data, &canonical, "migration-1", &conflict_id).unwrap();
        assert!(load_deferred_conflict_ids(&data, &canonical, "migration-1")
            .unwrap()
            .is_empty());

        let mut damaged = fs::read(&path).unwrap();
        *damaged.last_mut().unwrap() ^= 1;
        fs::write(path, damaged).unwrap();
        assert!(load_deferred_conflict_ids(&data, &canonical, "migration-1").is_err());
    }
}

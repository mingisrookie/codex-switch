use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
    thread,
    time::Duration,
};

use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    backup::{
        delete_verified_full_backup, extract_backup_manifest_file,
        inspect_corrupt_managed_full_backup, verified_full_backup_reclaim_bytes, verify_backup,
        BackupFile, BackupManifest, BackupScope,
    },
    file_ops::atomic_write,
    operation_log::timestamp_millis,
};

use super::{
    bounded_file::read_regular_file_bounded,
    migration::collect_inventory,
    migration_apply::stable_file_digest,
    model::{FileOrigin, SessionRelation},
    operation_ledger::{
        LedgerFileSnapshot, OperationLedgerStore, SessionStorageOperationKind,
        SessionStorageOperationPhase,
    },
    reference_graph::path_key,
    relation::compare_sessions,
    semantic::{read_semantic_session, SemanticSession},
};

const LEGACY_BACKUP_SCHEMA_VERSION: u32 = 1;
const MAX_PLAN_BYTES: u64 = 64 * 1024 * 1024;
const MAX_INBOX_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;
const OBSERVATION_DELAY: Duration = Duration::from_millis(250);
const RECOVERY_RETENTION_MS: u128 = 7 * 24 * 60 * 60 * 1_000;
const INBOX_ROOT: &str = "session-storage-v1/pending-recovery";
const STAGING_NAME: &str = "legacy-backup-staging";
const PLAN_NAME: &str = "legacy-backup-plan.json";
const MARKER_NAME: &str = ".codex-switch-pending-recovery-v1";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PendingRecoveryRelation {
    MissingFromCanonical,
    ExtendsCanonical,
    Divergent,
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PendingRecoveryStatus {
    Pending,
    Restored,
    Deferred,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PendingRecoveryEntryRecord {
    entry_id: String,
    thread_id: String,
    relation: PendingRecoveryRelation,
    status: PendingRecoveryStatus,
    payload_relative_path: PathBuf,
    source_database_relative_path: PathBuf,
    payload_bytes: u64,
    payload_sha256: String,
    source_database_sha256: String,
    candidate_message_count: usize,
    current_message_count: usize,
    candidate_added_message_count: usize,
    current_added_message_count: usize,
    candidate_last_message_at: Option<String>,
    current_last_message_at: Option<String>,
    candidate_provider: Option<String>,
    current_provider: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PendingRecoveryManifest {
    schema_version: u32,
    package_id: String,
    reconciliation_operation_id: String,
    migration_operation_id: String,
    source_backup_id: String,
    source_backup_created_at_ms: u128,
    created_at_ms: u128,
    expires_at_ms: u128,
    entries: Vec<PendingRecoveryEntryRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PendingRecoveryEnvelope {
    manifest: PendingRecoveryManifest,
    integrity_sha256: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PendingRecoverySummary {
    pub entry_id: String,
    pub thread_id: String,
    pub relation: PendingRecoveryRelation,
    pub status: PendingRecoveryStatus,
    pub source_backup_id: String,
    pub source_backup_created_at_ms: u128,
    pub candidate_message_count: usize,
    pub current_message_count: usize,
    pub candidate_added_message_count: usize,
    pub current_added_message_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_last_message_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_last_message_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_provider: Option<String>,
    pub payload_bytes: u64,
    pub expires_at_ms: u128,
    pub restore_allowed: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PendingRecoveryList {
    pub migration_operation_id: String,
    pub entries: Vec<PendingRecoverySummary>,
    pub expired_package_count: usize,
    pub invalid_package_count: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PendingRecoveryRetentionReceipt {
    pub deleted_package_count: usize,
    pub retained_package_count: usize,
    pub invalid_package_count: usize,
    pub reclaimed_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct PendingRecoverySource {
    pub entry_id: String,
    pub package_id: String,
    pub package_dir: PathBuf,
    pub payload_path: PathBuf,
    pub source_database_path: PathBuf,
    pub thread_id: String,
    pub payload_sha256: String,
    pub relation: PendingRecoveryRelation,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum LegacyBackupAction {
    PublishAndDelete,
    PublishAndRetain,
    DeleteDuplicate,
    Retain,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LegacyBackupPlanEntry {
    source_backup_dir: PathBuf,
    source_backup_id: String,
    source_manifest_sha256: String,
    source_created_at_ms: u128,
    source_reclaim_bytes: u64,
    source_tree_sha256: Option<String>,
    action: LegacyBackupAction,
    staging_package_dir: Option<PathBuf>,
    final_package_dir: Option<PathBuf>,
    pending_count: usize,
    conflict_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending_manifest_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LegacyBackupReconciliationPlan {
    pub schema_version: u32,
    pub operation_id: String,
    pub migration_operation_id: String,
    pub generated_at_ms: u128,
    pub canonical_root: PathBuf,
    pub data_root: PathBuf,
    pub backup_root: PathBuf,
    pub cutoff_created_at_ms: u128,
    pub canonical_inventory_fingerprint: String,
    pub staging_root: PathBuf,
    entries: Vec<LegacyBackupPlanEntry>,
    pub unreadable_backup_count: usize,
    pub retained_unreadable_backup_count: usize,
    pub ignored_backup_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LegacyBackupPlanEnvelope {
    plan: LegacyBackupReconciliationPlan,
    integrity_sha256: String,
}

#[derive(Debug, Clone)]
pub struct PreparedLegacyBackupReconciliation {
    pub plan: LegacyBackupReconciliationPlan,
    pub created_files: Vec<LedgerFileSnapshot>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LegacyBackupReconciliationReceipt {
    pub operation_id: String,
    pub migration_operation_id: String,
    pub scanned_backup_count: usize,
    pub deleted_backup_count: usize,
    pub retained_backup_count: usize,
    pub unreadable_backup_count: usize,
    pub pending_recovery_count: usize,
    pub conflict_count: usize,
    pub reclaimed_bytes: u64,
    pub validated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyBackupRecoveryStatus {
    Committed,
    RolledBack,
    DeferredByLiveWriter,
    Failed,
}

pub fn prepare_legacy_backup_reconciliation(
    canonical_root: &Path,
    data_root: &Path,
    backup_root: &Path,
    migration_operation_id: &str,
    cutoff_created_at_ms: u128,
    operation_id: &str,
) -> Result<PreparedLegacyBackupReconciliation, String> {
    prepare_legacy_backup_reconciliation_with_corrupt_observer(
        canonical_root,
        data_root,
        backup_root,
        migration_operation_id,
        cutoff_created_at_ms,
        operation_id,
        |_| Ok(()),
    )
}

#[allow(clippy::too_many_arguments)]
fn prepare_legacy_backup_reconciliation_with_corrupt_observer<Observe>(
    canonical_root: &Path,
    data_root: &Path,
    backup_root: &Path,
    migration_operation_id: &str,
    cutoff_created_at_ms: u128,
    operation_id: &str,
    mut after_first_corrupt_observation: Observe,
) -> Result<PreparedLegacyBackupReconciliation, String>
where
    Observe: FnMut(&Path) -> Result<(), String>,
{
    validate_absolute_directory(canonical_root, "canonical root")?;
    validate_absolute_directory(data_root, "data root")?;
    validate_absolute_directory(backup_root, "legacy backup root")?;
    validate_id(migration_operation_id, "migration operation ID")?;
    validate_id(operation_id, "legacy backup operation ID")?;

    let operation_root = operation_root(data_root, operation_id)?;
    let staging_root = operation_root.join(STAGING_NAME);
    if staging_root.exists() {
        return Err(
            "an interrupted legacy backup reconciliation must be recovered first".to_string(),
        );
    }
    create_safe_directory(&staging_root)?;
    write_marker(&staging_root, operation_id, migration_operation_id)?;

    let result = (|| {
        let first_inventory = collect_inventory(canonical_root, data_root)?;
        thread::sleep(OBSERVATION_DELAY);
        let second_inventory = collect_inventory(canonical_root, data_root)?;
        if first_inventory.fingerprint != second_inventory.fingerprint
            || second_inventory.database_discovery_errors > 0
            || second_inventory.session_discovery_errors > 0
        {
            return Err(
                "canonical inventory changed or was incomplete during backup review".to_string(),
            );
        }
        let canonical = canonical_sessions(&second_inventory);
        let generated_at_ms = timestamp_millis()?;
        let expires_at_ms = generated_at_ms
            .checked_add(RECOVERY_RETENTION_MS)
            .ok_or_else(|| "pending recovery retention timestamp overflowed".to_string())?;
        let final_parent = data_root.join(INBOX_ROOT);
        create_safe_directory(&final_parent)?;

        let mut entries = Vec::new();
        let mut unreadable_backup_count = 0_usize;
        let mut retained_unreadable_backup_count = 0_usize;
        let mut ignored_backup_count = 0_usize;
        let mut candidates = backup_directories(backup_root)?;
        candidates.sort_by_key(|path| path_key(path));
        for source_backup_dir in candidates {
            let manifest = match verify_backup(&source_backup_dir) {
                Ok(manifest) => manifest,
                Err(_) => {
                    let first = match inspect_corrupt_managed_full_backup(
                        backup_root,
                        &source_backup_dir,
                    ) {
                        Ok(snapshot) => snapshot,
                        Err(_) => {
                            unreadable_backup_count = unreadable_backup_count.saturating_add(1);
                            retained_unreadable_backup_count =
                                retained_unreadable_backup_count.saturating_add(1);
                            continue;
                        }
                    };
                    if !is_legacy_full_backup(&first.manifest, cutoff_created_at_ms) {
                        ignored_backup_count = ignored_backup_count.saturating_add(1);
                        continue;
                    }
                    after_first_corrupt_observation(&source_backup_dir)?;
                    thread::sleep(OBSERVATION_DELAY);
                    let second = match inspect_corrupt_managed_full_backup(
                        backup_root,
                        &source_backup_dir,
                    ) {
                        Ok(snapshot) => snapshot,
                        Err(_) => {
                            unreadable_backup_count = unreadable_backup_count.saturating_add(1);
                            retained_unreadable_backup_count =
                                retained_unreadable_backup_count.saturating_add(1);
                            continue;
                        }
                    };
                    unreadable_backup_count = unreadable_backup_count.saturating_add(1);
                    if first != second {
                        retained_unreadable_backup_count =
                            retained_unreadable_backup_count.saturating_add(1);
                        continue;
                    }
                    let manifest_path = source_backup_dir.join("manifest.json");
                    let (_, source_manifest_sha256) = stable_file_digest(&manifest_path)?;
                    let source_backup_id = backup_id(&first.manifest)?;
                    let staging_package_dir = staging_root.join(&source_backup_id);
                    let final_package_dir = final_parent.join(&source_backup_id);
                    if final_package_dir.exists() {
                        let existing = read_pending_manifest(&final_package_dir)?;
                        if existing.source_backup_id != source_backup_id
                            || existing.migration_operation_id != migration_operation_id
                        {
                            return Err("pending recovery package identity collided".to_string());
                        }
                    }
                    let analysis = analyze_backup(
                        &first.manifest,
                        &source_backup_id,
                        migration_operation_id,
                        operation_id,
                        &canonical,
                        &staging_package_dir,
                        generated_at_ms,
                        expires_at_ms,
                        true,
                    )?;
                    let action = if analysis.manifest.entries.is_empty() {
                        LegacyBackupAction::Retain
                    } else {
                        write_pending_package(&staging_package_dir, &analysis.manifest)?;
                        LegacyBackupAction::PublishAndRetain
                    };
                    entries.push(LegacyBackupPlanEntry {
                        source_backup_dir,
                        source_backup_id,
                        source_manifest_sha256,
                        source_created_at_ms: first.manifest.created_at_ms,
                        source_reclaim_bytes: first.reclaimed_bytes,
                        source_tree_sha256: Some(first.tree_sha256),
                        action,
                        staging_package_dir: (action == LegacyBackupAction::PublishAndRetain)
                            .then_some(staging_package_dir),
                        final_package_dir: (action == LegacyBackupAction::PublishAndRetain)
                            .then_some(final_package_dir),
                        pending_count: analysis.manifest.entries.len(),
                        conflict_count: analysis
                            .manifest
                            .entries
                            .iter()
                            .filter(|entry| entry.relation == PendingRecoveryRelation::Divergent)
                            .count(),
                        pending_manifest_sha256: (action == LegacyBackupAction::PublishAndRetain)
                            .then(|| pending_manifest_digest(&analysis.manifest))
                            .transpose()?,
                    });
                    continue;
                }
            };
            if !is_legacy_full_backup(&manifest, cutoff_created_at_ms) {
                ignored_backup_count = ignored_backup_count.saturating_add(1);
                continue;
            }
            let manifest_path = source_backup_dir.join("manifest.json");
            let (_, source_manifest_sha256) = stable_file_digest(&manifest_path)?;
            let source_backup_id = backup_id(&manifest)?;
            let source_reclaim_bytes =
                verified_full_backup_reclaim_bytes(backup_root, &source_backup_dir)?;
            let staging_package_dir = staging_root.join(&source_backup_id);
            let final_package_dir = final_parent.join(&source_backup_id);
            if final_package_dir.exists() {
                let existing = read_pending_manifest(&final_package_dir)?;
                if existing.source_backup_id != source_backup_id
                    || existing.migration_operation_id != migration_operation_id
                {
                    return Err("pending recovery package identity collided".to_string());
                }
            }

            let analysis = analyze_backup(
                &manifest,
                &source_backup_id,
                migration_operation_id,
                operation_id,
                &canonical,
                &staging_package_dir,
                generated_at_ms,
                expires_at_ms,
                false,
            )?;
            let action = if !analysis.complete && !analysis.manifest.entries.is_empty() {
                write_pending_package(&staging_package_dir, &analysis.manifest)?;
                LegacyBackupAction::PublishAndRetain
            } else if !analysis.complete {
                LegacyBackupAction::Retain
            } else if analysis.manifest.entries.is_empty() {
                LegacyBackupAction::DeleteDuplicate
            } else {
                write_pending_package(&staging_package_dir, &analysis.manifest)?;
                LegacyBackupAction::PublishAndDelete
            };
            entries.push(LegacyBackupPlanEntry {
                source_backup_dir,
                source_backup_id,
                source_manifest_sha256,
                source_created_at_ms: manifest.created_at_ms,
                source_reclaim_bytes,
                source_tree_sha256: None,
                action,
                staging_package_dir: matches!(
                    action,
                    LegacyBackupAction::PublishAndDelete | LegacyBackupAction::PublishAndRetain
                )
                .then_some(staging_package_dir),
                final_package_dir: matches!(
                    action,
                    LegacyBackupAction::PublishAndDelete | LegacyBackupAction::PublishAndRetain
                )
                .then_some(final_package_dir),
                pending_count: analysis.manifest.entries.len(),
                conflict_count: analysis
                    .manifest
                    .entries
                    .iter()
                    .filter(|entry| entry.relation == PendingRecoveryRelation::Divergent)
                    .count(),
                pending_manifest_sha256: matches!(
                    action,
                    LegacyBackupAction::PublishAndDelete | LegacyBackupAction::PublishAndRetain
                )
                .then(|| pending_manifest_digest(&analysis.manifest))
                .transpose()?,
            });
        }
        let plan = LegacyBackupReconciliationPlan {
            schema_version: LEGACY_BACKUP_SCHEMA_VERSION,
            operation_id: operation_id.to_string(),
            migration_operation_id: migration_operation_id.to_string(),
            generated_at_ms,
            canonical_root: canonical_root.to_path_buf(),
            data_root: data_root.to_path_buf(),
            backup_root: backup_root.to_path_buf(),
            cutoff_created_at_ms,
            canonical_inventory_fingerprint: second_inventory.fingerprint,
            staging_root: staging_root.clone(),
            entries,
            unreadable_backup_count,
            retained_unreadable_backup_count,
            ignored_backup_count,
        };
        validate_plan(&plan)?;
        persist_plan(&plan)?;
        let created_files = reconciliation_created_files(&plan)?;
        Ok(PreparedLegacyBackupReconciliation {
            plan,
            created_files,
        })
    })();
    if result.is_err() {
        let _ = remove_owned_tree(&staging_root, &operation_root, operation_id);
    }
    result
}

pub fn execute_legacy_backup_reconciliation<Guard>(
    plan: &LegacyBackupReconciliationPlan,
    mut before_mutation: Guard,
) -> Result<LegacyBackupReconciliationReceipt, String>
where
    Guard: FnMut() -> Result<(), String>,
{
    validate_plan(plan)?;
    let inventory = collect_inventory(&plan.canonical_root, &plan.data_root)?;
    if inventory.fingerprint != plan.canonical_inventory_fingerprint
        || inventory.database_discovery_errors > 0
        || inventory.session_discovery_errors > 0
    {
        return Err("canonical inventory changed before legacy backup reconciliation".to_string());
    }

    let receipt = planned_reconciliation_receipt(plan)?;
    for entry in &plan.entries {
        match entry.action {
            LegacyBackupAction::Retain => {
                revalidate_source_backup(entry)?;
            }
            LegacyBackupAction::DeleteDuplicate => {
                if entry.source_backup_dir.exists() {
                    before_mutation()?;
                    revalidate_source_backup(entry)?;
                    let deleted =
                        delete_verified_full_backup(&plan.backup_root, &entry.source_backup_dir)?;
                    if deleted.reclaimed_bytes != entry.source_reclaim_bytes {
                        return Err("legacy backup reclaimed byte count changed".to_string());
                    }
                }
            }
            LegacyBackupAction::PublishAndDelete | LegacyBackupAction::PublishAndRetain => {
                let staging = entry
                    .staging_package_dir
                    .as_ref()
                    .ok_or_else(|| "legacy backup staging package is missing".to_string())?;
                let final_dir = entry
                    .final_package_dir
                    .as_ref()
                    .ok_or_else(|| "legacy backup final package is missing".to_string())?;
                before_mutation()?;
                revalidate_source_backup(entry)?;
                if !final_dir.exists() {
                    fs::rename(staging, final_dir)
                        .map_err(|_| "failed to publish pending recovery package".to_string())?;
                }
                let published = read_pending_manifest(final_dir)?;
                if published.source_backup_id != entry.source_backup_id
                    || published.reconciliation_operation_id != plan.operation_id
                    || published.migration_operation_id != plan.migration_operation_id
                    || published.source_backup_created_at_ms != entry.source_created_at_ms
                    || published.entries.len() != entry.pending_count
                    || published
                        .entries
                        .iter()
                        .filter(|candidate| {
                            candidate.relation == PendingRecoveryRelation::Divergent
                        })
                        .count()
                        != entry.conflict_count
                    || pending_manifest_digest(&published)?
                        != *entry.pending_manifest_sha256.as_ref().ok_or_else(|| {
                            "legacy backup pending manifest hash is missing".to_string()
                        })?
                {
                    return Err("published pending recovery package identity changed".to_string());
                }
                if entry.action == LegacyBackupAction::PublishAndDelete {
                    before_mutation()?;
                    revalidate_source_backup(entry)?;
                    let deleted =
                        delete_verified_full_backup(&plan.backup_root, &entry.source_backup_dir)?;
                    if deleted.reclaimed_bytes != entry.source_reclaim_bytes {
                        return Err("legacy backup reclaimed byte count changed".to_string());
                    }
                }
            }
        }
    }
    validate_applied_reconciliation(plan, receipt)
}

fn planned_reconciliation_receipt(
    plan: &LegacyBackupReconciliationPlan,
) -> Result<LegacyBackupReconciliationReceipt, String> {
    let mut receipt = LegacyBackupReconciliationReceipt {
        operation_id: plan.operation_id.clone(),
        migration_operation_id: plan.migration_operation_id.clone(),
        scanned_backup_count: plan
            .entries
            .len()
            .checked_add(plan.retained_unreadable_backup_count)
            .and_then(|count| count.checked_add(plan.ignored_backup_count))
            .ok_or_else(|| "legacy backup scan count overflowed".to_string())?,
        deleted_backup_count: 0,
        retained_backup_count: plan
            .retained_unreadable_backup_count
            .checked_add(plan.ignored_backup_count)
            .ok_or_else(|| "legacy backup retained count overflowed".to_string())?,
        unreadable_backup_count: plan.unreadable_backup_count,
        pending_recovery_count: 0,
        conflict_count: 0,
        reclaimed_bytes: 0,
        validated: false,
    };
    for entry in &plan.entries {
        match entry.action {
            LegacyBackupAction::Retain => {
                receipt.retained_backup_count = receipt
                    .retained_backup_count
                    .checked_add(1)
                    .ok_or_else(|| "legacy backup retained count overflowed".to_string())?;
            }
            LegacyBackupAction::DeleteDuplicate => {
                receipt.deleted_backup_count = receipt
                    .deleted_backup_count
                    .checked_add(1)
                    .ok_or_else(|| "legacy backup deletion count overflowed".to_string())?;
                receipt.reclaimed_bytes = receipt
                    .reclaimed_bytes
                    .checked_add(entry.source_reclaim_bytes)
                    .ok_or_else(|| "legacy backup reclaimed byte count overflowed".to_string())?;
            }
            LegacyBackupAction::PublishAndDelete | LegacyBackupAction::PublishAndRetain => {
                if entry.action == LegacyBackupAction::PublishAndDelete {
                    receipt.deleted_backup_count = receipt
                        .deleted_backup_count
                        .checked_add(1)
                        .ok_or_else(|| "legacy backup deletion count overflowed".to_string())?;
                    receipt.reclaimed_bytes = receipt
                        .reclaimed_bytes
                        .checked_add(entry.source_reclaim_bytes)
                        .ok_or_else(|| {
                            "legacy backup reclaimed byte count overflowed".to_string()
                        })?;
                } else {
                    receipt.retained_backup_count = receipt
                        .retained_backup_count
                        .checked_add(1)
                        .ok_or_else(|| "legacy backup retained count overflowed".to_string())?;
                }
                receipt.pending_recovery_count = receipt
                    .pending_recovery_count
                    .checked_add(entry.pending_count)
                    .ok_or_else(|| "pending recovery count overflowed".to_string())?;
                receipt.conflict_count =
                    receipt
                        .conflict_count
                        .checked_add(entry.conflict_count)
                        .ok_or_else(|| "pending recovery conflict count overflowed".to_string())?;
            }
        }
    }
    Ok(receipt)
}

pub fn reconciliation_receipt_from_applied_state(
    plan: &LegacyBackupReconciliationPlan,
) -> Result<LegacyBackupReconciliationReceipt, String> {
    validate_applied_reconciliation(plan, planned_reconciliation_receipt(plan)?)
}

pub fn validate_applied_reconciliation(
    plan: &LegacyBackupReconciliationPlan,
    mut receipt: LegacyBackupReconciliationReceipt,
) -> Result<LegacyBackupReconciliationReceipt, String> {
    validate_plan(plan)?;
    let expected = planned_reconciliation_receipt(plan)?;
    if receipt.operation_id != expected.operation_id
        || receipt.migration_operation_id != expected.migration_operation_id
        || receipt.scanned_backup_count != expected.scanned_backup_count
        || receipt.deleted_backup_count != expected.deleted_backup_count
        || receipt.retained_backup_count != expected.retained_backup_count
        || receipt.unreadable_backup_count != expected.unreadable_backup_count
        || receipt.pending_recovery_count != expected.pending_recovery_count
        || receipt.conflict_count != expected.conflict_count
        || receipt.reclaimed_bytes != expected.reclaimed_bytes
    {
        return Err("legacy backup reconciliation receipt does not match its plan".to_string());
    }
    for entry in &plan.entries {
        match entry.action {
            LegacyBackupAction::Retain => {
                revalidate_source_backup(entry)?;
            }
            LegacyBackupAction::DeleteDuplicate => {
                if entry.source_backup_dir.exists() {
                    return Err("reclaimable legacy backup was not deleted".to_string());
                }
            }
            LegacyBackupAction::PublishAndDelete | LegacyBackupAction::PublishAndRetain => {
                if entry.action == LegacyBackupAction::PublishAndDelete {
                    if entry.source_backup_dir.exists() {
                        return Err("legacy backup remained after recovery extraction".to_string());
                    }
                } else {
                    revalidate_source_backup(entry)?;
                }
                let final_dir = entry
                    .final_package_dir
                    .as_ref()
                    .ok_or_else(|| "pending recovery package path is missing".to_string())?;
                let manifest = read_pending_manifest(final_dir)?;
                if manifest.source_backup_id != entry.source_backup_id
                    || manifest.reconciliation_operation_id != plan.operation_id
                    || manifest.migration_operation_id != plan.migration_operation_id
                    || manifest.source_backup_created_at_ms != entry.source_created_at_ms
                    || manifest.entries.len() != entry.pending_count
                    || manifest
                        .entries
                        .iter()
                        .filter(|candidate| {
                            candidate.relation == PendingRecoveryRelation::Divergent
                        })
                        .count()
                        != entry.conflict_count
                    || pending_manifest_digest(&manifest)?
                        != *entry.pending_manifest_sha256.as_ref().ok_or_else(|| {
                            "legacy backup pending manifest hash is missing".to_string()
                        })?
                {
                    return Err("pending recovery package counts changed".to_string());
                }
            }
        }
    }
    receipt.validated = true;
    Ok(receipt)
}

pub fn cleanup_reconciliation_staging(plan: &LegacyBackupReconciliationPlan) -> Result<(), String> {
    if !plan.staging_root.exists() {
        return Ok(());
    }
    remove_owned_tree(
        &plan.staging_root,
        &operation_root(&plan.data_root, &plan.operation_id)?,
        &plan.operation_id,
    )
}

pub fn recover_interrupted_legacy_backup_reconciliation<Guard>(
    store: &OperationLedgerStore,
    data_root: &Path,
    operation_id: &str,
    mut before_mutation: Guard,
) -> Result<LegacyBackupRecoveryStatus, String>
where
    Guard: FnMut() -> Result<(), String>,
{
    let ledger = store.load(operation_id)?;
    if ledger.kind != SessionStorageOperationKind::LegacyBackupReconciliation {
        return Err("legacy backup recovery ledger identity is invalid".to_string());
    }
    match ledger.phase {
        SessionStorageOperationPhase::Committed => {
            return Ok(LegacyBackupRecoveryStatus::Committed)
        }
        SessionStorageOperationPhase::RolledBack => {
            return Ok(LegacyBackupRecoveryStatus::RolledBack)
        }
        SessionStorageOperationPhase::Failed => return Ok(LegacyBackupRecoveryStatus::Failed),
        _ => {}
    }
    let plan = match load_legacy_backup_plan(data_root, operation_id) {
        Ok(plan) => Some(plan),
        Err(_)
            if matches!(
                ledger.phase,
                SessionStorageOperationPhase::Available
                    | SessionStorageOperationPhase::Preflight
                    | SessionStorageOperationPhase::Backup
            ) =>
        {
            None
        }
        Err(error) => return Err(error),
    };
    if !matches!(
        ledger.phase,
        SessionStorageOperationPhase::Applying | SessionStorageOperationPhase::Validating
    ) {
        if let Some(plan) = plan.as_ref() {
            cleanup_reconciliation_staging(plan)?;
        } else {
            let operation_root = operation_root(data_root, operation_id)?;
            let staging_root = operation_root.join(STAGING_NAME);
            if staging_root.exists() {
                remove_owned_tree(&staging_root, &operation_root, operation_id)?;
            }
        }
        if ledger.phase == SessionStorageOperationPhase::Available {
            store.transition(operation_id, SessionStorageOperationPhase::Preflight)?;
        }
        store.transition(operation_id, SessionStorageOperationPhase::RollingBack)?;
        store.transition(operation_id, SessionStorageOperationPhase::RolledBack)?;
        return Ok(LegacyBackupRecoveryStatus::RolledBack);
    }
    let Some(plan) = plan else {
        return Ok(LegacyBackupRecoveryStatus::Failed);
    };
    if before_mutation().is_err() {
        return Ok(LegacyBackupRecoveryStatus::DeferredByLiveWriter);
    }
    let receipt = match execute_legacy_backup_reconciliation(&plan, &mut before_mutation) {
        Ok(receipt) => receipt,
        Err(_) => match reconciliation_receipt_from_applied_state(&plan) {
            Ok(receipt) => receipt,
            Err(_) => return Ok(LegacyBackupRecoveryStatus::Failed),
        },
    };
    if ledger.phase == SessionStorageOperationPhase::Applying {
        if let Err(error) = store.transition(operation_id, SessionStorageOperationPhase::Validating)
        {
            let phase = store.load(operation_id)?.phase;
            if phase == SessionStorageOperationPhase::Committed {
                return Ok(LegacyBackupRecoveryStatus::Committed);
            }
            if phase != SessionStorageOperationPhase::Validating {
                return Err(error);
            }
        }
    }
    if validate_applied_reconciliation(&plan, receipt).is_err()
        || cleanup_reconciliation_staging(&plan).is_err()
    {
        return Ok(LegacyBackupRecoveryStatus::Failed);
    }
    if let Err(error) = store.transition(operation_id, SessionStorageOperationPhase::Committed) {
        if store.load(operation_id)?.phase != SessionStorageOperationPhase::Committed {
            return Err(error);
        }
    }
    Ok(LegacyBackupRecoveryStatus::Committed)
}

pub fn load_legacy_backup_plan(
    data_root: &Path,
    operation_id: &str,
) -> Result<LegacyBackupReconciliationPlan, String> {
    let path = plan_path(data_root, operation_id)?;
    let bytes = read_regular_file_bounded(&path, MAX_PLAN_BYTES)
        .map_err(|_| "legacy backup reconciliation plan is unreadable".to_string())?;
    let envelope = serde_json::from_slice::<LegacyBackupPlanEnvelope>(&bytes)
        .map_err(|_| "legacy backup reconciliation plan is invalid".to_string())?;
    validate_plan(&envelope.plan)?;
    if envelope.plan.operation_id != operation_id
        || envelope.plan.data_root != data_root
        || plan_digest(&envelope.plan)? != envelope.integrity_sha256
    {
        return Err("legacy backup reconciliation plan integrity check failed".to_string());
    }
    Ok(envelope.plan)
}

pub fn list_pending_recovery(
    data_root: &Path,
    migration_operation_id: &str,
) -> Result<PendingRecoveryList, String> {
    validate_absolute_directory(data_root, "data root")?;
    validate_id(migration_operation_id, "migration operation ID")?;
    let root = data_root.join(INBOX_ROOT);
    if !root.exists() {
        return Ok(PendingRecoveryList {
            migration_operation_id: migration_operation_id.to_string(),
            entries: Vec::new(),
            expired_package_count: 0,
            invalid_package_count: 0,
        });
    }
    validate_absolute_directory(&root, "pending recovery root")?;
    let now = timestamp_millis()?;
    let mut entries = Vec::new();
    let mut expired_package_count = 0_usize;
    let mut invalid_package_count = 0_usize;
    for package_dir in backup_directories(&root)? {
        let manifest = match read_pending_manifest(&package_dir) {
            Ok(manifest) => manifest,
            Err(_) => {
                invalid_package_count = invalid_package_count.saturating_add(1);
                continue;
            }
        };
        if manifest.migration_operation_id != migration_operation_id {
            continue;
        }
        if manifest.expires_at_ms <= now {
            expired_package_count = expired_package_count.saturating_add(1);
        }
        entries.extend(
            manifest
                .entries
                .into_iter()
                .map(|entry| PendingRecoverySummary {
                    entry_id: entry.entry_id,
                    thread_id: entry.thread_id,
                    relation: entry.relation,
                    status: entry.status,
                    source_backup_id: manifest.source_backup_id.clone(),
                    source_backup_created_at_ms: manifest.source_backup_created_at_ms,
                    candidate_message_count: entry.candidate_message_count,
                    current_message_count: entry.current_message_count,
                    candidate_added_message_count: entry.candidate_added_message_count,
                    current_added_message_count: entry.current_added_message_count,
                    candidate_last_message_at: entry.candidate_last_message_at,
                    current_last_message_at: entry.current_last_message_at,
                    candidate_provider: entry.candidate_provider,
                    current_provider: entry.current_provider,
                    payload_bytes: entry.payload_bytes,
                    expires_at_ms: manifest.expires_at_ms,
                    restore_allowed: manifest.expires_at_ms > now
                        && entry.status == PendingRecoveryStatus::Pending
                        && matches!(
                            entry.relation,
                            PendingRecoveryRelation::MissingFromCanonical
                                | PendingRecoveryRelation::ExtendsCanonical
                        ),
                }),
        );
    }
    entries.sort_by(|left, right| {
        left.source_backup_created_at_ms
            .cmp(&right.source_backup_created_at_ms)
            .then_with(|| left.thread_id.cmp(&right.thread_id))
            .then_with(|| left.entry_id.cmp(&right.entry_id))
    });
    Ok(PendingRecoveryList {
        migration_operation_id: migration_operation_id.to_string(),
        entries,
        expired_package_count,
        invalid_package_count,
    })
}

pub fn cleanup_expired_pending_recovery(
    data_root: &Path,
    now_ms: u128,
) -> Result<PendingRecoveryRetentionReceipt, String> {
    validate_absolute_directory(data_root, "data root")?;
    let root = data_root.join(INBOX_ROOT);
    if !root.exists() {
        return Ok(PendingRecoveryRetentionReceipt {
            deleted_package_count: 0,
            retained_package_count: 0,
            invalid_package_count: 0,
            reclaimed_bytes: 0,
        });
    }
    validate_absolute_directory(&root, "pending recovery root")?;
    let mut receipt = PendingRecoveryRetentionReceipt {
        deleted_package_count: 0,
        retained_package_count: 0,
        invalid_package_count: 0,
        reclaimed_bytes: 0,
    };
    for package_dir in backup_directories(&root)? {
        let first = match read_pending_manifest(&package_dir) {
            Ok(manifest) => manifest,
            Err(_) => {
                receipt.invalid_package_count = receipt.invalid_package_count.saturating_add(1);
                receipt.retained_package_count = receipt.retained_package_count.saturating_add(1);
                continue;
            }
        };
        if first.expires_at_ms > now_ms {
            receipt.retained_package_count = receipt.retained_package_count.saturating_add(1);
            continue;
        }
        let second = read_pending_manifest(&package_dir)?;
        if first != second {
            return Err("pending recovery package changed during retention check".to_string());
        }
        let bytes = managed_tree_bytes(&package_dir)?;
        remove_owned_tree(&package_dir, &root, &first.reconciliation_operation_id)?;
        receipt.deleted_package_count = receipt.deleted_package_count.saturating_add(1);
        receipt.reclaimed_bytes = receipt
            .reclaimed_bytes
            .checked_add(bytes)
            .ok_or_else(|| "pending recovery reclaimed byte count overflowed".to_string())?;
    }
    Ok(receipt)
}

pub fn load_pending_recovery_source(
    data_root: &Path,
    migration_operation_id: &str,
    entry_id: &str,
) -> Result<PendingRecoverySource, String> {
    validate_id(entry_id, "pending recovery entry ID")?;
    let root = data_root.join(INBOX_ROOT);
    for package_dir in backup_directories(&root)? {
        let manifest = match read_pending_manifest(&package_dir) {
            Ok(manifest) if manifest.migration_operation_id == migration_operation_id => manifest,
            Ok(_) | Err(_) => continue,
        };
        let Some(entry) = manifest
            .entries
            .iter()
            .find(|entry| entry.entry_id == entry_id)
        else {
            continue;
        };
        if manifest.expires_at_ms <= timestamp_millis()? {
            return Err("pending recovery package has expired".to_string());
        }
        if entry.status != PendingRecoveryStatus::Pending {
            return Err("pending recovery entry is not pending".to_string());
        }
        let payload_path = package_dir.join(&entry.payload_relative_path);
        let source_database_path = package_dir.join(&entry.source_database_relative_path);
        verify_payload(&payload_path, entry.payload_bytes, &entry.payload_sha256)?;
        if stable_file_digest(&source_database_path)?.1 != entry.source_database_sha256 {
            return Err("pending recovery source database changed".to_string());
        }
        return Ok(PendingRecoverySource {
            entry_id: entry.entry_id.clone(),
            package_id: manifest.package_id,
            package_dir,
            payload_path,
            source_database_path,
            thread_id: entry.thread_id.clone(),
            payload_sha256: entry.payload_sha256.clone(),
            relation: entry.relation,
        });
    }
    Err("pending recovery entry was not found".to_string())
}

pub fn update_pending_recovery_status(
    data_root: &Path,
    migration_operation_id: &str,
    entry_id: &str,
    status: PendingRecoveryStatus,
) -> Result<(), String> {
    let source = load_pending_recovery_source(data_root, migration_operation_id, entry_id)?;
    let mut manifest = read_pending_manifest(&source.package_dir)?;
    let entry = manifest
        .entries
        .iter_mut()
        .find(|entry| entry.entry_id == entry_id)
        .ok_or_else(|| "pending recovery entry was not found".to_string())?;
    entry.status = status;
    write_pending_manifest(&source.package_dir, &manifest)?;
    read_pending_manifest(&source.package_dir).map(|_| ())
}

struct BackupAnalysis {
    manifest: PendingRecoveryManifest,
    complete: bool,
}

#[allow(clippy::too_many_arguments)]
fn analyze_backup(
    backup: &BackupManifest,
    source_backup_id: &str,
    migration_operation_id: &str,
    operation_id: &str,
    canonical: &BTreeMap<String, Vec<SemanticSession>>,
    staging_package_dir: &Path,
    generated_at_ms: u128,
    expires_at_ms: u128,
    tolerate_invalid_payloads: bool,
) -> Result<BackupAnalysis, String> {
    let state_file = backup
        .files
        .iter()
        .find(|file| file.relative_path == Path::new("state_5.sqlite"));
    let Some(state_file) = state_file else {
        return Ok(BackupAnalysis {
            manifest: empty_pending_manifest(
                source_backup_id,
                backup.created_at_ms,
                migration_operation_id,
                operation_id,
                generated_at_ms,
                expires_at_ms,
            ),
            complete: false,
        });
    };
    create_safe_directory(staging_package_dir)?;
    write_marker(staging_package_dir, operation_id, migration_operation_id)?;
    let database_relative_path = PathBuf::from("source-state.sqlite");
    let database_path = staging_package_dir.join(&database_relative_path);
    let (_, source_database_sha256) =
        match extract_backup_manifest_file(backup, &state_file.relative_path, &database_path) {
            Ok(extracted) => extracted,
            Err(_) if tolerate_invalid_payloads => {
                return Ok(BackupAnalysis {
                    manifest: empty_pending_manifest(
                        source_backup_id,
                        backup.created_at_ms,
                        migration_operation_id,
                        operation_id,
                        generated_at_ms,
                        expires_at_ms,
                    ),
                    complete: false,
                });
            }
            Err(error) => return Err(error),
        };
    if let Err(error) = quick_check_sqlite(&database_path) {
        if tolerate_invalid_payloads {
            return Ok(BackupAnalysis {
                manifest: empty_pending_manifest(
                    source_backup_id,
                    backup.created_at_ms,
                    migration_operation_id,
                    operation_id,
                    generated_at_ms,
                    expires_at_ms,
                ),
                complete: false,
            });
        }
        return Err(error);
    }
    let database_threads = match database_thread_paths(&database_path) {
        Ok(threads) => threads,
        Err(_) if tolerate_invalid_payloads => {
            return Ok(BackupAnalysis {
                manifest: empty_pending_manifest(
                    source_backup_id,
                    backup.created_at_ms,
                    migration_operation_id,
                    operation_id,
                    generated_at_ms,
                    expires_at_ms,
                ),
                complete: false,
            });
        }
        Err(error) => return Err(error),
    };

    let mut entries = Vec::new();
    let mut complete = true;
    let mut seen = BTreeSet::new();
    for file in backup.files.iter().filter(is_session_file) {
        let temporary = staging_package_dir
            .join("analysis")
            .join(format!("{:04}.jsonl", entries.len()));
        let (bytes, sha256) =
            match extract_backup_manifest_file(backup, &file.relative_path, &temporary) {
                Ok(extracted) => extracted,
                Err(_) if tolerate_invalid_payloads => {
                    complete = false;
                    let _ = fs::remove_file(&temporary);
                    continue;
                }
                Err(error) => return Err(error),
            };
        let semantic = match read_semantic_session(&temporary) {
            Ok(semantic) => semantic,
            Err(_) => {
                complete = false;
                let _ = fs::remove_file(&temporary);
                continue;
            }
        };
        if !database_threads.contains_key(&semantic.thread_id) {
            complete = false;
            let _ = fs::remove_file(&temporary);
            continue;
        }
        let relation = classify_candidate(canonical.get(&semantic.thread_id), &semantic);
        let Some(relation) = relation else {
            let _ = fs::remove_file(&temporary);
            continue;
        };
        let dedupe_key = format!("{}|{sha256}", semantic.thread_id);
        if !seen.insert(dedupe_key) {
            let _ = fs::remove_file(&temporary);
            continue;
        }
        let entry_id = hex_digest(Sha256::digest(
            format!("{source_backup_id}|{}|{sha256}", semantic.thread_id).as_bytes(),
        ));
        let payload_relative_path = PathBuf::from("payloads").join(format!("{entry_id}.jsonl"));
        let payload_path = staging_package_dir.join(&payload_relative_path);
        if let Some(parent) = payload_path.parent() {
            create_safe_directory(parent)?;
        }
        fs::rename(&temporary, &payload_path)
            .map_err(|_| "failed to stage pending recovery payload".to_string())?;
        verify_payload(&payload_path, bytes, &sha256)?;
        let current = select_current(canonical.get(&semantic.thread_id), &semantic);
        let common = current
            .map(|current| common_message_prefix(current, &semantic))
            .unwrap_or(0);
        entries.push(PendingRecoveryEntryRecord {
            entry_id,
            thread_id: semantic.thread_id.clone(),
            relation,
            status: PendingRecoveryStatus::Pending,
            payload_relative_path,
            source_database_relative_path: database_relative_path.clone(),
            payload_bytes: bytes,
            payload_sha256: sha256,
            source_database_sha256: source_database_sha256.clone(),
            candidate_message_count: semantic.message_count,
            current_message_count: current.map(|item| item.message_count).unwrap_or(0),
            candidate_added_message_count: semantic.message_count.saturating_sub(common),
            current_added_message_count: current
                .map(|item| item.message_count.saturating_sub(common))
                .unwrap_or(0),
            candidate_last_message_at: semantic.last_message_timestamp.clone(),
            current_last_message_at: current.and_then(|item| item.last_message_timestamp.clone()),
            candidate_provider: semantic.initial_provider.clone(),
            current_provider: current.and_then(|item| item.initial_provider.clone()),
        });
    }
    let analysis_dir = staging_package_dir.join("analysis");
    if analysis_dir.exists() {
        let _ = fs::remove_dir(&analysis_dir);
    }
    entries.sort_by(|left, right| {
        left.thread_id
            .cmp(&right.thread_id)
            .then_with(|| left.entry_id.cmp(&right.entry_id))
    });
    if entries.is_empty() {
        let _ = fs::remove_file(&database_path);
    }
    Ok(BackupAnalysis {
        manifest: PendingRecoveryManifest {
            schema_version: LEGACY_BACKUP_SCHEMA_VERSION,
            package_id: source_backup_id.to_string(),
            reconciliation_operation_id: operation_id.to_string(),
            migration_operation_id: migration_operation_id.to_string(),
            source_backup_id: source_backup_id.to_string(),
            source_backup_created_at_ms: backup.created_at_ms,
            created_at_ms: generated_at_ms,
            expires_at_ms,
            entries,
        },
        complete,
    })
}

fn classify_candidate(
    current: Option<&Vec<SemanticSession>>,
    candidate: &SemanticSession,
) -> Option<PendingRecoveryRelation> {
    let Some(current) = current else {
        return Some(PendingRecoveryRelation::MissingFromCanonical);
    };
    let relations = current
        .iter()
        .map(|item| compare_sessions(item, candidate))
        .collect::<Vec<_>>();
    if relations.iter().any(|relation| {
        matches!(
            relation,
            SessionRelation::Equal
                | SessionRelation::EqualExceptProvider
                | SessionRelation::RightPrefix
        )
    }) {
        return None;
    }
    if relations
        .iter()
        .all(|relation| *relation == SessionRelation::LeftPrefix)
    {
        return Some(PendingRecoveryRelation::ExtendsCanonical);
    }
    if relations.contains(&SessionRelation::Unknown) {
        Some(PendingRecoveryRelation::Unknown)
    } else {
        Some(PendingRecoveryRelation::Divergent)
    }
}

fn select_current<'a>(
    current: Option<&'a Vec<SemanticSession>>,
    candidate: &SemanticSession,
) -> Option<&'a SemanticSession> {
    current.and_then(|items| {
        items.iter().max_by(|left, right| {
            let left_relation = compare_sessions(left, candidate);
            let right_relation = compare_sessions(right, candidate);
            relation_rank(left_relation)
                .cmp(&relation_rank(right_relation))
                .then_with(|| left.message_count.cmp(&right.message_count))
                .then_with(|| left.raw_sha256.cmp(&right.raw_sha256))
        })
    })
}

fn relation_rank(relation: SessionRelation) -> u8 {
    match relation {
        SessionRelation::Equal | SessionRelation::EqualExceptProvider => 5,
        SessionRelation::LeftPrefix => 4,
        SessionRelation::RightPrefix => 3,
        SessionRelation::Divergent => 2,
        SessionRelation::Unknown => 1,
    }
}

fn common_message_prefix(left: &SemanticSession, right: &SemanticSession) -> usize {
    left.message_line_sha256
        .iter()
        .zip(&right.message_line_sha256)
        .take_while(|(left, right)| left == right)
        .count()
}

fn canonical_sessions(
    inventory: &super::migration::MigrationInventory,
) -> BTreeMap<String, Vec<SemanticSession>> {
    let mut sessions = BTreeMap::<String, Vec<SemanticSession>>::new();
    for file in &inventory.files {
        if file.origin != FileOrigin::CanonicalHome {
            continue;
        }
        if let Ok(semantic) = &file.semantic {
            sessions
                .entry(semantic.thread_id.clone())
                .or_default()
                .push(semantic.clone());
        }
    }
    sessions
}

fn empty_pending_manifest(
    source_backup_id: &str,
    source_backup_created_at_ms: u128,
    migration_operation_id: &str,
    operation_id: &str,
    created_at_ms: u128,
    expires_at_ms: u128,
) -> PendingRecoveryManifest {
    PendingRecoveryManifest {
        schema_version: LEGACY_BACKUP_SCHEMA_VERSION,
        package_id: source_backup_id.to_string(),
        reconciliation_operation_id: operation_id.to_string(),
        migration_operation_id: migration_operation_id.to_string(),
        source_backup_id: source_backup_id.to_string(),
        source_backup_created_at_ms,
        created_at_ms,
        expires_at_ms,
        entries: Vec::new(),
    }
}

fn write_pending_package(
    package_dir: &Path,
    manifest: &PendingRecoveryManifest,
) -> Result<(), String> {
    write_marker(
        package_dir,
        &manifest.reconciliation_operation_id,
        &manifest.migration_operation_id,
    )?;
    write_pending_manifest(package_dir, manifest)?;
    let verified = read_pending_manifest(package_dir)?;
    if &verified != manifest {
        return Err("pending recovery package verification identity changed".to_string());
    }
    Ok(())
}

fn write_pending_manifest(
    package_dir: &Path,
    manifest: &PendingRecoveryManifest,
) -> Result<(), String> {
    validate_pending_manifest(package_dir, manifest)?;
    let envelope = PendingRecoveryEnvelope {
        manifest: manifest.clone(),
        integrity_sha256: pending_manifest_digest(manifest)?,
    };
    let bytes = serde_json::to_vec_pretty(&envelope)
        .map_err(|_| "failed to serialize pending recovery manifest".to_string())?;
    if bytes.len() as u64 > MAX_INBOX_MANIFEST_BYTES {
        return Err("pending recovery manifest reached its size limit".to_string());
    }
    atomic_write(&package_dir.join("manifest.json"), &bytes)
}

fn read_pending_manifest(package_dir: &Path) -> Result<PendingRecoveryManifest, String> {
    validate_absolute_directory(package_dir, "pending recovery package")?;
    let bytes =
        read_regular_file_bounded(&package_dir.join("manifest.json"), MAX_INBOX_MANIFEST_BYTES)
            .map_err(|_| "pending recovery manifest is unreadable".to_string())?;
    let envelope = serde_json::from_slice::<PendingRecoveryEnvelope>(&bytes)
        .map_err(|_| "pending recovery manifest is invalid".to_string())?;
    validate_pending_manifest(package_dir, &envelope.manifest)?;
    if pending_manifest_digest(&envelope.manifest)? != envelope.integrity_sha256 {
        return Err("pending recovery manifest integrity check failed".to_string());
    }
    validate_marker(
        package_dir,
        &envelope.manifest.reconciliation_operation_id,
        &envelope.manifest.migration_operation_id,
    )?;
    let mut expected = BTreeSet::from([
        path_key(&package_dir.join("manifest.json")),
        path_key(&package_dir.join(MARKER_NAME)),
    ]);
    for entry in &envelope.manifest.entries {
        let payload = package_dir.join(&entry.payload_relative_path);
        let database = package_dir.join(&entry.source_database_relative_path);
        verify_payload(&payload, entry.payload_bytes, &entry.payload_sha256)?;
        if stable_file_digest(&database)?.1 != entry.source_database_sha256 {
            return Err("pending recovery source database integrity check failed".to_string());
        }
        expected.insert(path_key(&payload));
        expected.insert(path_key(&database));
    }
    let actual = regular_files(package_dir)?;
    if actual != expected {
        return Err("pending recovery package contains an undeclared payload".to_string());
    }
    Ok(envelope.manifest)
}

fn validate_pending_manifest(
    package_dir: &Path,
    manifest: &PendingRecoveryManifest,
) -> Result<(), String> {
    if manifest.schema_version != LEGACY_BACKUP_SCHEMA_VERSION
        || manifest.package_id.len() != 64
        || !manifest.package_id.bytes().all(is_lower_hex)
        || manifest.source_backup_id != manifest.package_id
        || package_dir.file_name().and_then(|name| name.to_str())
            != Some(manifest.package_id.as_str())
        || manifest.created_at_ms >= manifest.expires_at_ms
        || manifest.entries.is_empty()
    {
        return Err("pending recovery manifest shape is invalid".to_string());
    }
    validate_id(
        &manifest.reconciliation_operation_id,
        "reconciliation operation ID",
    )?;
    validate_id(&manifest.migration_operation_id, "migration operation ID")?;
    let mut entry_ids = BTreeSet::new();
    for entry in &manifest.entries {
        if entry.entry_id.len() != 64
            || !entry.entry_id.bytes().all(is_lower_hex)
            || !entry_ids.insert(entry.entry_id.clone())
            || entry.thread_id.trim().is_empty()
            || entry.payload_bytes == 0
            || entry.payload_sha256.len() != 64
            || entry.source_database_sha256.len() != 64
            || !entry.payload_sha256.bytes().all(is_lower_hex)
            || !entry.source_database_sha256.bytes().all(is_lower_hex)
            || !is_safe_relative_path(&entry.payload_relative_path)
            || !is_safe_relative_path(&entry.source_database_relative_path)
            || !package_dir
                .join(&entry.payload_relative_path)
                .starts_with(package_dir)
            || !package_dir
                .join(&entry.source_database_relative_path)
                .starts_with(package_dir)
        {
            return Err("pending recovery entry shape is invalid".to_string());
        }
    }
    Ok(())
}

fn managed_tree_bytes(root: &Path) -> Result<u64, String> {
    let mut total = 0_u64;
    for (path, is_directory) in walk_tree_contents_first(root)? {
        if is_directory {
            continue;
        }
        let metadata = fs::symlink_metadata(path)
            .map_err(|_| "pending recovery file is unavailable".to_string())?;
        total = total
            .checked_add(metadata.len())
            .ok_or_else(|| "pending recovery package byte count overflowed".to_string())?;
    }
    Ok(total)
}

fn persist_plan(plan: &LegacyBackupReconciliationPlan) -> Result<(), String> {
    let envelope = LegacyBackupPlanEnvelope {
        plan: plan.clone(),
        integrity_sha256: plan_digest(plan)?,
    };
    let bytes = serde_json::to_vec_pretty(&envelope)
        .map_err(|_| "failed to serialize legacy backup reconciliation plan".to_string())?;
    if bytes.len() as u64 > MAX_PLAN_BYTES {
        return Err("legacy backup reconciliation plan reached its size limit".to_string());
    }
    atomic_write(&plan_path(&plan.data_root, &plan.operation_id)?, &bytes)
}

fn reconciliation_created_files(
    plan: &LegacyBackupReconciliationPlan,
) -> Result<Vec<LedgerFileSnapshot>, String> {
    let mut paths = BTreeMap::<String, PathBuf>::new();
    let persisted_plan = plan_path(&plan.data_root, &plan.operation_id)?;
    paths.insert(path_key(&persisted_plan), persisted_plan);
    for (path, is_directory) in walk_tree_contents_first(&plan.staging_root)? {
        if !is_directory {
            paths.insert(path_key(&path), path);
        }
    }
    let mut output = Vec::with_capacity(paths.len());
    for path in paths.into_values() {
        let (bytes, sha256) = stable_file_digest(&path)?;
        output.push(LedgerFileSnapshot {
            path,
            bytes,
            sha256,
            created_by_operation: true,
            logical_thread_id: None,
        });
    }
    Ok(output)
}

fn validate_plan(plan: &LegacyBackupReconciliationPlan) -> Result<(), String> {
    validate_id(&plan.operation_id, "legacy backup operation ID")?;
    validate_id(&plan.migration_operation_id, "migration operation ID")?;
    validate_absolute_directory(&plan.canonical_root, "canonical root")?;
    validate_absolute_directory(&plan.data_root, "data root")?;
    validate_absolute_directory(&plan.backup_root, "legacy backup root")?;
    if plan.schema_version != LEGACY_BACKUP_SCHEMA_VERSION
        || plan.canonical_inventory_fingerprint.len() != 64
        || !plan
            .canonical_inventory_fingerprint
            .bytes()
            .all(is_lower_hex)
        || plan.staging_root
            != operation_root(&plan.data_root, &plan.operation_id)?.join(STAGING_NAME)
        || plan.retained_unreadable_backup_count > plan.unreadable_backup_count
    {
        return Err("legacy backup reconciliation plan shape is invalid".to_string());
    }
    let final_parent = plan.data_root.join(INBOX_ROOT);
    let tracked_corrupt_backup_count = plan
        .entries
        .iter()
        .filter(|entry| entry.source_tree_sha256.is_some())
        .count();
    if plan
        .retained_unreadable_backup_count
        .checked_add(tracked_corrupt_backup_count)
        != Some(plan.unreadable_backup_count)
    {
        return Err("legacy backup unreadable counts are inconsistent".to_string());
    }
    let mut backup_ids = BTreeSet::new();
    for entry in &plan.entries {
        if !backup_ids.insert(entry.source_backup_id.clone())
            || entry.source_backup_id.len() != 64
            || !entry.source_backup_id.bytes().all(is_lower_hex)
            || entry.source_manifest_sha256.len() != 64
            || !entry.source_manifest_sha256.bytes().all(is_lower_hex)
            || entry
                .source_tree_sha256
                .as_ref()
                .is_some_and(|sha256| sha256.len() != 64 || !sha256.bytes().all(is_lower_hex))
            || entry.source_reclaim_bytes == 0
            || entry.source_backup_dir.parent() != Some(plan.backup_root.as_path())
            || entry.source_created_at_ms > plan.cutoff_created_at_ms
        {
            return Err("legacy backup reconciliation source plan is invalid".to_string());
        }
        match entry.action {
            LegacyBackupAction::PublishAndDelete | LegacyBackupAction::PublishAndRetain => {
                if (entry.action == LegacyBackupAction::PublishAndDelete
                    && entry.source_tree_sha256.is_some())
                    || entry.pending_manifest_sha256.as_ref().is_none_or(|sha256| {
                        sha256.len() != 64 || !sha256.bytes().all(is_lower_hex)
                    })
                    || entry
                        .staging_package_dir
                        .as_ref()
                        .is_none_or(|path| path.parent() != Some(plan.staging_root.as_path()))
                    || entry
                        .final_package_dir
                        .as_ref()
                        .is_none_or(|path| path.parent() != Some(final_parent.as_path()))
                    || entry.pending_count == 0
                {
                    return Err("legacy backup recovery package plan is invalid".to_string());
                }
            }
            LegacyBackupAction::DeleteDuplicate => {
                if entry.source_tree_sha256.is_some()
                    || entry.pending_manifest_sha256.is_some()
                    || entry.staging_package_dir.is_some()
                    || entry.final_package_dir.is_some()
                    || entry.pending_count != 0
                    || entry.conflict_count != 0
                {
                    return Err("legacy backup non-package plan is invalid".to_string());
                }
            }
            LegacyBackupAction::Retain => {
                if entry.pending_manifest_sha256.is_some()
                    || entry.staging_package_dir.is_some()
                    || entry.final_package_dir.is_some()
                    || entry.pending_count != 0
                    || entry.conflict_count != 0
                {
                    return Err("legacy backup retained source plan is invalid".to_string());
                }
            }
        }
    }
    Ok(())
}

fn revalidate_source_backup(entry: &LegacyBackupPlanEntry) -> Result<(), String> {
    if let Some(expected_tree_sha256) = entry.source_tree_sha256.as_ref() {
        let backup_root = entry
            .source_backup_dir
            .parent()
            .ok_or_else(|| "legacy backup source root is missing".to_string())?;
        let snapshot = inspect_corrupt_managed_full_backup(backup_root, &entry.source_backup_dir)?;
        if backup_id(&snapshot.manifest)? != entry.source_backup_id
            || snapshot.manifest.created_at_ms != entry.source_created_at_ms
            || snapshot.tree_sha256 != *expected_tree_sha256
            || snapshot.reclaimed_bytes != entry.source_reclaim_bytes
            || stable_file_digest(&entry.source_backup_dir.join("manifest.json"))?.1
                != entry.source_manifest_sha256
        {
            return Err("corrupt legacy backup changed after reconciliation planning".to_string());
        }
        return Ok(());
    }
    let manifest = verify_backup(&entry.source_backup_dir)?;
    let backup_root = entry
        .source_backup_dir
        .parent()
        .ok_or_else(|| "legacy backup source root is missing".to_string())?;
    let current_reclaim_bytes =
        verified_full_backup_reclaim_bytes(backup_root, &entry.source_backup_dir)?;
    if backup_id(&manifest)? != entry.source_backup_id
        || manifest.created_at_ms != entry.source_created_at_ms
        || current_reclaim_bytes != entry.source_reclaim_bytes
        || stable_file_digest(&entry.source_backup_dir.join("manifest.json"))?.1
            != entry.source_manifest_sha256
    {
        return Err("legacy backup changed after reconciliation planning".to_string());
    }
    Ok(())
}

fn is_legacy_full_backup(manifest: &BackupManifest, cutoff: u128) -> bool {
    manifest.complete_sessions
        && manifest.created_at_ms <= cutoff
        && manifest.operation_id.is_none()
        && (manifest.version == 2 || manifest.scope == BackupScope::Full)
}

fn is_session_file(file: &&BackupFile) -> bool {
    (file.relative_path.starts_with("sessions")
        || file.relative_path.starts_with("archived_sessions"))
        && file
            .relative_path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
}

fn database_thread_paths(path: &Path) -> Result<BTreeMap<String, Option<String>>, String> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| "failed to open isolated legacy backup database".to_string())?;
    let mut statement = connection
        .prepare("SELECT id, rollout_path FROM threads")
        .map_err(|_| "legacy backup database threads schema is unsupported".to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })
        .map_err(|_| "failed to inspect legacy backup thread rows".to_string())?;
    let mut output = BTreeMap::new();
    for row in rows {
        let (thread_id, rollout_path) =
            row.map_err(|_| "failed to inspect legacy backup thread row".to_string())?;
        output.insert(thread_id, rollout_path);
    }
    Ok(output)
}

fn quick_check_sqlite(path: &Path) -> Result<(), String> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| "failed to open isolated legacy backup database".to_string())?;
    let result: String = connection
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(|_| "failed to verify isolated legacy backup database".to_string())?;
    if result.eq_ignore_ascii_case("ok") {
        Ok(())
    } else {
        Err("isolated legacy backup database failed quick_check".to_string())
    }
}

fn verify_payload(path: &Path, expected_bytes: u64, expected_sha256: &str) -> Result<(), String> {
    let (bytes, sha256) = stable_file_digest(path)?;
    if bytes != expected_bytes || sha256 != expected_sha256 {
        return Err("pending recovery payload integrity check failed".to_string());
    }
    Ok(())
}

fn backup_id(manifest: &BackupManifest) -> Result<String, String> {
    let bytes = serde_json::to_vec(manifest)
        .map_err(|_| "failed to encode legacy backup identity".to_string())?;
    Ok(hex_digest(Sha256::digest(bytes)))
}

fn pending_manifest_digest(manifest: &PendingRecoveryManifest) -> Result<String, String> {
    let bytes = serde_json::to_vec(manifest)
        .map_err(|_| "failed to encode pending recovery manifest".to_string())?;
    Ok(hex_digest(Sha256::digest(bytes)))
}

fn plan_digest(plan: &LegacyBackupReconciliationPlan) -> Result<String, String> {
    let bytes = serde_json::to_vec(plan)
        .map_err(|_| "failed to encode legacy backup reconciliation plan".to_string())?;
    Ok(hex_digest(Sha256::digest(bytes)))
}

fn write_marker(
    root: &Path,
    operation_id: &str,
    migration_operation_id: &str,
) -> Result<(), String> {
    let marker = serde_json::json!({
        "schemaVersion": LEGACY_BACKUP_SCHEMA_VERSION,
        "operationId": operation_id,
        "migrationOperationId": migration_operation_id,
    });
    let bytes = serde_json::to_vec_pretty(&marker)
        .map_err(|_| "failed to serialize pending recovery marker".to_string())?;
    atomic_write(&root.join(MARKER_NAME), &bytes)
}

fn validate_marker(
    root: &Path,
    operation_id: &str,
    migration_operation_id: &str,
) -> Result<(), String> {
    let bytes = read_regular_file_bounded(&root.join(MARKER_NAME), 64 * 1024)
        .map_err(|_| "pending recovery marker is unavailable".to_string())?;
    let marker = serde_json::from_slice::<serde_json::Value>(&bytes)
        .map_err(|_| "pending recovery marker is invalid".to_string())?;
    if marker.get("schemaVersion").and_then(|value| value.as_u64())
        != Some(LEGACY_BACKUP_SCHEMA_VERSION as u64)
        || marker.get("operationId").and_then(|value| value.as_str()) != Some(operation_id)
        || marker
            .get("migrationOperationId")
            .and_then(|value| value.as_str())
            != Some(migration_operation_id)
    {
        return Err("pending recovery marker identity changed".to_string());
    }
    Ok(())
}

fn regular_files(root: &Path) -> Result<BTreeSet<String>, String> {
    let mut files = BTreeSet::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)
            .map_err(|_| "failed to inspect pending recovery package".to_string())?
        {
            let entry = entry
                .map_err(|_| "failed to inspect pending recovery package entry".to_string())?;
            let metadata = fs::symlink_metadata(entry.path())
                .map_err(|_| "failed to inspect pending recovery package entry".to_string())?;
            if metadata_is_link_or_reparse(&metadata) {
                return Err("pending recovery package contains a link".to_string());
            }
            if metadata.is_dir() {
                pending.push(entry.path());
            } else if metadata.is_file() {
                files.insert(path_key(&entry.path()));
            } else {
                return Err("pending recovery package contains an unsupported entry".to_string());
            }
        }
    }
    Ok(files)
}

fn backup_directories(root: &Path) -> Result<Vec<PathBuf>, String> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut directories = Vec::new();
    for entry in fs::read_dir(root).map_err(|_| "failed to list backup inventory".to_string())? {
        let entry = entry.map_err(|_| "failed to inspect backup inventory entry".to_string())?;
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|_| "failed to inspect backup inventory entry".to_string())?;
        if metadata_is_link_or_reparse(&metadata) {
            continue;
        }
        if metadata.is_dir() {
            directories.push(entry.path());
        }
    }
    Ok(directories)
}

fn create_safe_directory(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path)
        .map_err(|_| "failed to create session recovery directory".to_string())?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| "failed to inspect session recovery directory".to_string())?;
    if !metadata.is_dir() || metadata_is_link_or_reparse(&metadata) {
        return Err("session recovery directory is unsafe".to_string());
    }
    Ok(())
}

fn remove_owned_tree(root: &Path, parent: &Path, operation_id: &str) -> Result<(), String> {
    if root.parent() != Some(parent) || !root.is_absolute() {
        return Err("session recovery cleanup root is invalid".to_string());
    }
    validate_marker(root, operation_id, operation_id).or_else(|_| {
        let bytes = read_regular_file_bounded(&root.join(MARKER_NAME), 64 * 1024)
            .map_err(|_| "session recovery cleanup marker is unavailable".to_string())?;
        let marker = serde_json::from_slice::<serde_json::Value>(&bytes)
            .map_err(|_| "session recovery cleanup marker is invalid".to_string())?;
        if marker.get("operationId").and_then(|value| value.as_str()) == Some(operation_id) {
            Ok(())
        } else {
            Err("session recovery cleanup marker identity changed".to_string())
        }
    })?;
    let mut entries = Vec::new();
    for entry in walk_tree_contents_first(root)? {
        entries.push(entry);
    }
    for (path, is_dir) in entries {
        if is_dir {
            fs::remove_dir(path)
                .map_err(|_| "failed to remove session recovery directory".to_string())?;
        } else {
            fs::remove_file(path)
                .map_err(|_| "failed to remove session recovery file".to_string())?;
        }
    }
    Ok(())
}

fn walk_tree_contents_first(root: &Path) -> Result<Vec<(PathBuf, bool)>, String> {
    fn visit(path: &Path, output: &mut Vec<(PathBuf, bool)>) -> Result<(), String> {
        let metadata = fs::symlink_metadata(path)
            .map_err(|_| "failed to inspect session recovery cleanup entry".to_string())?;
        if metadata_is_link_or_reparse(&metadata) {
            return Err("session recovery cleanup contains a link".to_string());
        }
        if metadata.is_dir() {
            for entry in fs::read_dir(path)
                .map_err(|_| "failed to inspect session recovery cleanup directory".to_string())?
            {
                let entry = entry
                    .map_err(|_| "failed to inspect session recovery cleanup entry".to_string())?;
                visit(&entry.path(), output)?;
            }
            output.push((path.to_path_buf(), true));
        } else if metadata.is_file() {
            output.push((path.to_path_buf(), false));
        } else {
            return Err("session recovery cleanup contains an unsupported entry".to_string());
        }
        Ok(())
    }
    let mut output = Vec::new();
    visit(root, &mut output)?;
    Ok(output)
}

fn operation_root(data_root: &Path, operation_id: &str) -> Result<PathBuf, String> {
    validate_id(operation_id, "legacy backup operation ID")?;
    Ok(data_root
        .join("session-storage-v1/operations")
        .join(operation_id))
}

fn plan_path(data_root: &Path, operation_id: &str) -> Result<PathBuf, String> {
    Ok(operation_root(data_root, operation_id)?.join(PLAN_NAME))
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

fn validate_id(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 160
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(format!("{label} is invalid"));
    }
    Ok(())
}

fn is_safe_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn hex_digest(digest: impl AsRef<[u8]>) -> String {
    digest
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

    use rusqlite::Connection;
    use tempfile::tempdir;

    use super::{
        cleanup_expired_pending_recovery, cleanup_reconciliation_staging,
        execute_legacy_backup_reconciliation, list_pending_recovery, load_pending_recovery_source,
        planned_reconciliation_receipt, prepare_legacy_backup_reconciliation,
        prepare_legacy_backup_reconciliation_with_corrupt_observer,
        reconciliation_receipt_from_applied_state,
        recover_interrupted_legacy_backup_reconciliation, update_pending_recovery_status,
        LegacyBackupReconciliationReceipt, LegacyBackupRecoveryStatus, PendingRecoveryRelation,
        PendingRecoveryStatus,
    };
    use crate::backup::{create_local_backup, BackupFile, BackupManifest};
    use crate::session_storage::operation_ledger::{
        OperationLedgerStore, SessionStorageOperationKind, SessionStorageOperationPhase,
    };
    use crate::session_storage::restore_import::{
        cleanup_restore_import_staging, execute_restore_import, prepare_pending_recovery_import,
        validate_applied_restore_import,
    };

    fn create_profile(home: &Path, sessions: &[(&str, &[&str])]) {
        fs::create_dir_all(home).unwrap();
        fs::write(home.join("auth.json"), "{}").unwrap();
        fs::write(
            home.join("config.toml"),
            format!("sqlite_home = {:?}\n", home.to_string_lossy()),
        )
        .unwrap();
        let database = Connection::open(home.join("state_5.sqlite")).unwrap();
        database
            .execute_batch(
                "CREATE TABLE threads (
                    id TEXT PRIMARY KEY,
                    rollout_path TEXT,
                    model_provider TEXT
                );",
            )
            .unwrap();
        for (index, (thread_id, messages)) in sessions.iter().enumerate() {
            let path = home
                .join("sessions/2026/08/12")
                .join(format!("rollout-{index}-{thread_id}.jsonl"));
            write_session(&path, thread_id, messages);
            database
                .execute(
                    "INSERT INTO threads VALUES (?1, ?2, 'openai')",
                    (*thread_id, path.to_string_lossy().to_string()),
                )
                .unwrap();
        }
    }

    fn write_session(path: &Path, thread_id: &str, messages: &[&str]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut lines = vec![serde_json::json!({
            "type": "session_meta",
            "payload": {"id": thread_id, "model_provider": "openai"}
        })
        .to_string()];
        lines.extend(messages.iter().enumerate().map(|(index, message)| {
            serde_json::json!({
                "timestamp": format!("2026-08-12T00:00:{index:02}Z"),
                "type": "event_msg",
                "payload": {"type": "user_message", "message": message}
            })
            .to_string()
        }));
        fs::write(path, lines.join("\n") + "\n").unwrap();
    }

    fn session_backup_file<'a>(manifest: &'a BackupManifest, thread_id: &str) -> &'a BackupFile {
        manifest
            .files
            .iter()
            .find(|file| {
                file.relative_path.starts_with("sessions")
                    && file
                        .relative_path
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().contains(thread_id))
            })
            .unwrap()
    }

    fn corrupt_payload(file: &BackupFile) {
        let mut bytes = fs::read(&file.backup_path).unwrap();
        bytes[0] ^= 0x5a;
        fs::write(&file.backup_path, bytes).unwrap();
    }

    fn assert_exact_retained_receipt(
        receipt: &LegacyBackupReconciliationReceipt,
        operation_id: &str,
        scanned_backup_count: usize,
        unreadable_backup_count: usize,
    ) {
        assert_eq!(receipt.operation_id, operation_id);
        assert_eq!(receipt.migration_operation_id, "migration-1");
        assert_eq!(receipt.scanned_backup_count, scanned_backup_count);
        assert_eq!(receipt.deleted_backup_count, 0);
        assert_eq!(receipt.retained_backup_count, scanned_backup_count);
        assert_eq!(receipt.unreadable_backup_count, unreadable_backup_count);
        assert_eq!(receipt.pending_recovery_count, 0);
        assert_eq!(receipt.conflict_count, 0);
        assert_eq!(receipt.reclaimed_bytes, 0);
        assert!(receipt.validated);
    }

    #[cfg(windows)]
    fn create_directory_symlink(target: &Path, link: &Path) -> bool {
        std::os::windows::fs::symlink_dir(target, link).is_ok()
    }

    #[cfg(unix)]
    fn create_directory_symlink(target: &Path, link: &Path) -> bool {
        std::os::unix::fs::symlink(target, link).is_ok()
    }

    #[cfg(not(any(windows, unix)))]
    fn create_directory_symlink(_target: &Path, _link: &Path) -> bool {
        false
    }

    fn enter_reconciliation_applying(
        store: &OperationLedgerStore,
        operation_id: &str,
        canonical_root: &Path,
    ) {
        store
            .create(
                operation_id,
                SessionStorageOperationKind::LegacyBackupReconciliation,
                canonical_root,
            )
            .unwrap();
        for phase in [
            SessionStorageOperationPhase::Preflight,
            SessionStorageOperationPhase::Backup,
            SessionStorageOperationPhase::BackupVerified,
            SessionStorageOperationPhase::PlanReady,
            SessionStorageOperationPhase::Applying,
        ] {
            store.transition(operation_id, phase).unwrap();
        }
    }

    #[test]
    fn extracts_missing_and_extended_sessions_before_deleting_the_old_backup() {
        let root = tempdir().unwrap();
        let current = root.path().join("current");
        let old = root.path().join("old");
        let data = root.path().join("data");
        let backups = root.path().join("backups");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&backups).unwrap();
        create_profile(&current, &[("thread-a", &["one"])]);
        create_profile(
            &old,
            &[("thread-a", &["one", "two"]), ("thread-b", &["new"])],
        );
        let old_backup = create_local_backup(&old, &backups, "manual-full-old").unwrap();

        let prepared = prepare_legacy_backup_reconciliation(
            &current,
            &data,
            &backups,
            "migration-1",
            u128::MAX,
            "legacy-reconcile-1",
        )
        .unwrap();
        assert!(old_backup.backup_dir.exists());

        let receipt = execute_legacy_backup_reconciliation(&prepared.plan, || Ok(())).unwrap();
        cleanup_reconciliation_staging(&prepared.plan).unwrap();
        let pending = list_pending_recovery(&data, "migration-1").unwrap();

        assert!(receipt.validated);
        assert_eq!(receipt.deleted_backup_count, 1);
        assert_eq!(receipt.pending_recovery_count, 2);
        assert!(!old_backup.backup_dir.exists());
        assert_eq!(pending.entries.len(), 2);
        assert!(pending.entries.iter().any(|entry| {
            entry.thread_id == "thread-a"
                && entry.relation == PendingRecoveryRelation::ExtendsCanonical
                && entry.restore_allowed
        }));
        assert!(pending.entries.iter().any(|entry| {
            entry.thread_id == "thread-b"
                && entry.relation == PendingRecoveryRelation::MissingFromCanonical
                && entry.restore_allowed
        }));

        let missing = pending
            .entries
            .iter()
            .find(|entry| entry.thread_id == "thread-b")
            .unwrap();
        let source = load_pending_recovery_source(&data, "migration-1", &missing.entry_id).unwrap();
        let prepared_import =
            prepare_pending_recovery_import(&current, &data, &source, "pending-restore-1").unwrap();
        let restored = execute_restore_import(&prepared_import.plan, || Ok(())).unwrap();
        let restored = validate_applied_restore_import(&prepared_import.plan, restored).unwrap();
        cleanup_restore_import_staging(&prepared_import.plan).unwrap();
        update_pending_recovery_status(
            &data,
            "migration-1",
            &missing.entry_id,
            PendingRecoveryStatus::Restored,
        )
        .unwrap();

        assert_eq!(restored.imported_new_session_count, 1);
        assert!(restored.validated);
        let database = Connection::open(current.join("state_5.sqlite")).unwrap();
        let restored_path: String = database
            .query_row(
                "SELECT rollout_path FROM threads WHERE id = 'thread-b'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(Path::new(&restored_path).is_file());
        assert_eq!(
            list_pending_recovery(&data, "migration-1")
                .unwrap()
                .entries
                .iter()
                .find(|entry| entry.entry_id == missing.entry_id)
                .unwrap()
                .status,
            PendingRecoveryStatus::Restored
        );
        let retention = cleanup_expired_pending_recovery(&data, u128::MAX).unwrap();
        assert_eq!(retention.deleted_package_count, 1);
        assert!(retention.reclaimed_bytes > 0);
        assert!(list_pending_recovery(&data, "migration-1")
            .unwrap()
            .entries
            .is_empty());
    }

    #[test]
    fn deletes_a_fully_duplicate_old_backup_without_creating_an_inbox_payload() {
        let root = tempdir().unwrap();
        let current = root.path().join("current");
        let old = root.path().join("old");
        let data = root.path().join("data");
        let backups = root.path().join("backups");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&backups).unwrap();
        create_profile(&current, &[("thread-a", &["same"])]);
        create_profile(&old, &[("thread-a", &["same"])]);
        let old_backup = create_local_backup(&old, &backups, "manual-full-old").unwrap();

        let prepared = prepare_legacy_backup_reconciliation(
            &current,
            &data,
            &backups,
            "migration-1",
            u128::MAX,
            "legacy-reconcile-2",
        )
        .unwrap();
        let receipt = execute_legacy_backup_reconciliation(&prepared.plan, || Ok(())).unwrap();
        cleanup_reconciliation_staging(&prepared.plan).unwrap();

        assert_eq!(receipt.deleted_backup_count, 1);
        assert_eq!(receipt.pending_recovery_count, 0);
        assert!(!old_backup.backup_dir.exists());
        assert!(list_pending_recovery(&data, "migration-1")
            .unwrap()
            .entries
            .is_empty());
    }

    #[test]
    fn retains_a_twice_stable_corrupt_switch_backup_even_when_a_replacement_exists() {
        let root = tempdir().unwrap();
        let current = root.path().join("current");
        let old = root.path().join("old");
        let data = root.path().join("data");
        let backups = root.path().join("backups");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&backups).unwrap();
        create_profile(&current, &[("thread-a", &["current"])]);
        create_profile(
            &old,
            &[
                ("thread-a", &["old"]),
                ("thread-unique", &["preserve this unique body"]),
            ],
        );
        let expected_unique_body =
            fs::read(old.join("sessions/2026/08/12/rollout-1-thread-unique.jsonl")).unwrap();
        let old_backup = create_local_backup(&old, &backups, "manual-full-old").unwrap();
        let unique_backup_file = session_backup_file(&old_backup, "thread-unique").clone();
        let unique_encrypted_before = fs::read(&unique_backup_file.backup_path).unwrap();
        corrupt_payload(session_backup_file(&old_backup, "thread-a"));
        crate::backup::inspect_corrupt_managed_full_backup(&backups, &old_backup.backup_dir)
            .unwrap();

        let prepared = prepare_legacy_backup_reconciliation(
            &current,
            &data,
            &backups,
            "migration-1",
            u128::MAX,
            "legacy-reconcile-corrupt",
        )
        .unwrap();
        let receipt = execute_legacy_backup_reconciliation(&prepared.plan, || Ok(())).unwrap();

        assert_eq!(receipt.scanned_backup_count, 1);
        assert_eq!(receipt.deleted_backup_count, 0);
        assert_eq!(receipt.retained_backup_count, 1);
        assert_eq!(receipt.unreadable_backup_count, 1);
        assert_eq!(receipt.pending_recovery_count, 1);
        assert_eq!(receipt.conflict_count, 0);
        assert_eq!(receipt.reclaimed_bytes, 0);
        assert!(receipt.validated);
        assert!(old_backup.backup_dir.exists());
        // The corrupt tree cannot be decrypted as a whole, so byte-for-byte retention of the
        // independently intact declared payload is the losslessness proof for this branch.
        assert_eq!(
            fs::read(&unique_backup_file.backup_path).unwrap(),
            unique_encrypted_before
        );
        let pending = list_pending_recovery(&data, "migration-1").unwrap();
        assert_eq!(pending.entries.len(), 1);
        assert_eq!(pending.entries[0].thread_id, "thread-unique");
        let source =
            load_pending_recovery_source(&data, "migration-1", &pending.entries[0].entry_id)
                .unwrap();
        assert_eq!(fs::read(source.payload_path).unwrap(), expected_unique_body);
    }

    #[test]
    fn retains_a_verified_mixed_backup_when_one_session_is_invalid_and_another_is_unique() {
        let root = tempdir().unwrap();
        let current = root.path().join("current");
        let old = root.path().join("old");
        let data = root.path().join("data");
        let backups = root.path().join("backups");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&backups).unwrap();
        create_profile(&current, &[]);
        create_profile(
            &old,
            &[
                ("thread-invalid", &["invalid"]),
                ("thread-unique", &["preserve this unique body"]),
            ],
        );
        fs::write(
            old.join("sessions/2026/08/12/rollout-0-thread-invalid.jsonl"),
            b"{not-json}\n",
        )
        .unwrap();
        let unique_source = old.join("sessions/2026/08/12/rollout-1-thread-unique.jsonl");
        let expected_unique_body = fs::read(&unique_source).unwrap();
        let old_backup = create_local_backup(&old, &backups, "manual-full-mixed").unwrap();
        let unique_backup_file = session_backup_file(&old_backup, "thread-unique").clone();

        let operation_id = "legacy-reconcile-mixed-invalid";
        let prepared = prepare_legacy_backup_reconciliation(
            &current,
            &data,
            &backups,
            "migration-1",
            u128::MAX,
            operation_id,
        )
        .unwrap();
        let receipt = execute_legacy_backup_reconciliation(&prepared.plan, || Ok(())).unwrap();

        assert_eq!(receipt.operation_id, operation_id);
        assert_eq!(receipt.migration_operation_id, "migration-1");
        assert_eq!(receipt.scanned_backup_count, 1);
        assert_eq!(receipt.deleted_backup_count, 0);
        assert_eq!(receipt.retained_backup_count, 1);
        assert_eq!(receipt.unreadable_backup_count, 0);
        assert_eq!(receipt.pending_recovery_count, 1);
        assert_eq!(receipt.conflict_count, 0);
        assert_eq!(receipt.reclaimed_bytes, 0);
        assert!(receipt.validated);
        assert!(old_backup.backup_dir.exists());
        let extracted = root.path().join("extracted-thread-unique.jsonl");
        crate::backup::extract_backup_manifest_file(
            &old_backup,
            &unique_backup_file.relative_path,
            &extracted,
        )
        .unwrap();
        assert_eq!(fs::read(extracted).unwrap(), expected_unique_body);
        let pending = list_pending_recovery(&data, "migration-1").unwrap();
        assert_eq!(pending.entries.len(), 1);
        assert_eq!(pending.entries[0].thread_id, "thread-unique");
        assert_eq!(
            pending.entries[0].relation,
            PendingRecoveryRelation::MissingFromCanonical
        );
        let source =
            load_pending_recovery_source(&data, "migration-1", &pending.entries[0].entry_id)
                .unwrap();
        assert_eq!(fs::read(source.payload_path).unwrap(), expected_unique_body);
    }

    #[test]
    fn retains_unverifiable_backups_with_manifest_extra_reparse_or_hash_drift() {
        let root = tempdir().unwrap();
        let current = root.path().join("current");
        let old = root.path().join("old");
        let data = root.path().join("data");
        let backups = root.path().join("backups");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&backups).unwrap();
        create_profile(&current, &[]);
        create_profile(
            &old,
            &[
                ("thread-damaged", &["damage only this entry"]),
                ("thread-unique", &["preserve this unique body"]),
            ],
        );
        let expected_unique_body =
            fs::read(old.join("sessions/2026/08/12/rollout-1-thread-unique.jsonl")).unwrap();

        let manifest_drift =
            create_local_backup(&old, &backups, "manual-full-manifest-drift").unwrap();
        fs::write(
            manifest_drift.backup_dir.join("manifest.json"),
            b"{invalid-manifest\n",
        )
        .unwrap();

        let extra_entry = create_local_backup(&old, &backups, "manual-full-extra").unwrap();
        corrupt_payload(session_backup_file(&extra_entry, "thread-damaged"));
        let undeclared = extra_entry.backup_dir.join("undeclared.txt");
        fs::write(&undeclared, b"preserve this undeclared entry").unwrap();

        let reparse_entry = create_local_backup(&old, &backups, "manual-full-reparse").unwrap();
        corrupt_payload(session_backup_file(&reparse_entry, "thread-damaged"));
        let reparse_target = root.path().join("reparse-target");
        fs::create_dir(&reparse_target).unwrap();
        fs::write(reparse_target.join("outside.txt"), b"outside body").unwrap();
        let reparse = reparse_entry.backup_dir.join("undeclared-link");
        let reparse_created = create_directory_symlink(&reparse_target, &reparse);
        if !reparse_created {
            fs::write(
                reparse_entry.backup_dir.join("undeclared-reparse-fallback"),
                b"retain when this host cannot create a reparse fixture",
            )
            .unwrap();
        }

        let backups_under_test = [&manifest_drift, &extra_entry, &reparse_entry];
        let preserved_unique_payloads = backups_under_test
            .iter()
            .map(|backup| {
                let path = session_backup_file(backup, "thread-unique")
                    .backup_path
                    .clone();
                let bytes = fs::read(&path).unwrap();
                (path, bytes)
            })
            .collect::<Vec<_>>();

        let operation_id = "legacy-reconcile-unverifiable-shapes";
        let prepared = prepare_legacy_backup_reconciliation(
            &current,
            &data,
            &backups,
            "migration-1",
            u128::MAX,
            operation_id,
        )
        .unwrap();
        let receipt = execute_legacy_backup_reconciliation(&prepared.plan, || Ok(())).unwrap();

        assert_exact_retained_receipt(
            &receipt,
            operation_id,
            backups_under_test.len(),
            backups_under_test.len(),
        );
        for (index, backup) in backups_under_test.iter().enumerate() {
            assert!(backup.backup_dir.exists());
            let unique_file = session_backup_file(backup, "thread-unique");
            let extracted = root.path().join(format!("extracted-unique-{index}.jsonl"));
            crate::backup::extract_backup_manifest_file(
                backup,
                &unique_file.relative_path,
                &extracted,
            )
            .unwrap();
            assert_eq!(fs::read(extracted).unwrap(), expected_unique_body);
        }
        for (path, expected) in preserved_unique_payloads {
            assert_eq!(fs::read(path).unwrap(), expected);
        }
        assert_eq!(
            fs::read(undeclared).unwrap(),
            b"preserve this undeclared entry"
        );
        if reparse_created {
            assert!(fs::symlink_metadata(reparse)
                .map(|metadata| super::metadata_is_link_or_reparse(&metadata))
                .unwrap_or(false));
            assert_eq!(
                fs::read(reparse_target.join("outside.txt")).unwrap(),
                b"outside body"
            );
        }
    }

    #[test]
    fn source_hash_drift_between_plan_and_delete_never_yields_a_validated_receipt() {
        let root = tempdir().unwrap();
        let current = root.path().join("current");
        let old = root.path().join("old");
        let data = root.path().join("data");
        let backups = root.path().join("backups");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&backups).unwrap();
        create_profile(&current, &[]);
        create_profile(&old, &[("thread-unique", &["preserve across late drift"])]);
        let unique_source = old.join("sessions/2026/08/12/rollout-0-thread-unique.jsonl");
        let expected_unique_body = fs::read(&unique_source).unwrap();
        let old_backup = create_local_backup(&old, &backups, "manual-full-late-drift").unwrap();
        let unique_payload = session_backup_file(&old_backup, "thread-unique")
            .backup_path
            .clone();
        let prepared = prepare_legacy_backup_reconciliation(
            &current,
            &data,
            &backups,
            "migration-1",
            u128::MAX,
            "legacy-reconcile-late-drift",
        )
        .unwrap();
        let planned = planned_reconciliation_receipt(&prepared.plan).unwrap();
        assert!(!planned.validated);
        assert_eq!(planned.deleted_backup_count, 1);
        assert_eq!(planned.pending_recovery_count, 1);

        let mut mutation_count = 0_usize;
        let error = execute_legacy_backup_reconciliation(&prepared.plan, || {
            mutation_count = mutation_count.saturating_add(1);
            if mutation_count == 2 {
                let mut bytes = fs::read(&unique_payload).unwrap();
                bytes[0] ^= 0x5a;
                fs::write(&unique_payload, bytes).unwrap();
            }
            Ok(())
        })
        .unwrap_err();

        assert!(error.contains("checksum mismatch"));
        assert!(old_backup.backup_dir.exists());
        assert!(reconciliation_receipt_from_applied_state(&prepared.plan).is_err());
        let pending = list_pending_recovery(&data, "migration-1").unwrap();
        assert_eq!(pending.entries.len(), 1);
        let source =
            load_pending_recovery_source(&data, "migration-1", &pending.entries[0].entry_id)
                .unwrap();
        assert_eq!(fs::read(source.payload_path).unwrap(), expected_unique_body);
    }

    #[test]
    fn retained_mixed_source_drift_before_publish_fails_closed_without_a_receipt() {
        let root = tempdir().unwrap();
        let current = root.path().join("current");
        let old = root.path().join("old");
        let data = root.path().join("data");
        let backups = root.path().join("backups");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&backups).unwrap();
        create_profile(&current, &[]);
        create_profile(
            &old,
            &[
                ("thread-invalid", &["invalid"]),
                ("thread-unique", &["preserve across retained-source drift"]),
            ],
        );
        fs::write(
            old.join("sessions/2026/08/12/rollout-0-thread-invalid.jsonl"),
            b"{not-json}\n",
        )
        .unwrap();
        let old_backup = create_local_backup(&old, &backups, "manual-full-mixed-drift").unwrap();
        let unique_payload = session_backup_file(&old_backup, "thread-unique")
            .backup_path
            .clone();
        let original_unique_payload = fs::read(&unique_payload).unwrap();
        let prepared = prepare_legacy_backup_reconciliation(
            &current,
            &data,
            &backups,
            "migration-1",
            u128::MAX,
            "legacy-reconcile-retained-source-drift",
        )
        .unwrap();
        let planned = planned_reconciliation_receipt(&prepared.plan).unwrap();
        assert_eq!(planned.deleted_backup_count, 0);
        assert_eq!(planned.retained_backup_count, 1);
        assert_eq!(planned.pending_recovery_count, 1);
        assert!(!planned.validated);

        let appeared_after_plan = old_backup.backup_dir.join("appeared-after-plan.txt");
        let error = execute_legacy_backup_reconciliation(&prepared.plan, || {
            fs::write(&appeared_after_plan, b"preserve this late entry").unwrap();
            Ok(())
        })
        .unwrap_err();

        assert!(error.contains("undeclared file"));
        assert!(old_backup.backup_dir.exists());
        assert_eq!(fs::read(&unique_payload).unwrap(), original_unique_payload);
        assert_eq!(
            fs::read(&appeared_after_plan).unwrap(),
            b"preserve this late entry"
        );
        assert!(reconciliation_receipt_from_applied_state(&prepared.plan).is_err());
        assert!(list_pending_recovery(&data, "migration-1")
            .unwrap()
            .entries
            .is_empty());
    }

    #[test]
    fn corrupt_tree_change_between_stability_checks_is_retained_without_extraction() {
        let root = tempdir().unwrap();
        let current = root.path().join("current");
        let old = root.path().join("old");
        let data = root.path().join("data");
        let backups = root.path().join("backups");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&backups).unwrap();
        create_profile(&current, &[]);
        create_profile(
            &old,
            &[
                ("thread-damaged", &["damage only this entry"]),
                ("thread-unique", &["preserve across tree drift"]),
            ],
        );
        let old_backup = create_local_backup(&old, &backups, "manual-full-corrupt-drift").unwrap();
        corrupt_payload(session_backup_file(&old_backup, "thread-damaged"));
        let unique_payload = session_backup_file(&old_backup, "thread-unique")
            .backup_path
            .clone();
        let expected_unique_payload = fs::read(&unique_payload).unwrap();
        let extra = old_backup.backup_dir.join("appeared-between-checks.txt");
        let prepared = prepare_legacy_backup_reconciliation_with_corrupt_observer(
            &current,
            &data,
            &backups,
            "migration-1",
            u128::MAX,
            "legacy-reconcile-corrupt-tree-drift",
            |observed_backup| {
                assert_eq!(observed_backup, old_backup.backup_dir);
                fs::write(&extra, b"preserve this between-check entry").unwrap();
                Ok(())
            },
        )
        .unwrap();
        let receipt = execute_legacy_backup_reconciliation(&prepared.plan, || Ok(())).unwrap();

        assert_exact_retained_receipt(&receipt, "legacy-reconcile-corrupt-tree-drift", 1, 1);
        assert!(old_backup.backup_dir.exists());
        assert_eq!(fs::read(unique_payload).unwrap(), expected_unique_payload);
        assert_eq!(
            fs::read(extra).unwrap(),
            b"preserve this between-check entry"
        );
        assert!(list_pending_recovery(&data, "migration-1")
            .unwrap()
            .entries
            .is_empty());
    }

    #[test]
    fn retains_a_corrupt_backup_when_its_directory_contains_an_undeclared_file() {
        let root = tempdir().unwrap();
        let current = root.path().join("current");
        let old = root.path().join("old");
        let data = root.path().join("data");
        let backups = root.path().join("backups");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&backups).unwrap();
        create_profile(&current, &[]);
        create_profile(&old, &[("thread-a", &["old"])]);
        let old_backup = create_local_backup(&old, &backups, "manual-full-old").unwrap();
        let payload = old_backup
            .files
            .iter()
            .find(|file| file.relative_path.starts_with("sessions"))
            .unwrap()
            .backup_path
            .clone();
        let mut bytes = fs::read(&payload).unwrap();
        bytes[0] ^= 0x5a;
        fs::write(&payload, bytes).unwrap();
        fs::write(old_backup.backup_dir.join("undeclared.txt"), b"preserve").unwrap();

        let prepared = prepare_legacy_backup_reconciliation(
            &current,
            &data,
            &backups,
            "migration-1",
            u128::MAX,
            "legacy-reconcile-corrupt-unknown",
        )
        .unwrap();
        let receipt = execute_legacy_backup_reconciliation(&prepared.plan, || Ok(())).unwrap();

        assert_exact_retained_receipt(&receipt, "legacy-reconcile-corrupt-unknown", 1, 1);
        assert!(old_backup.backup_dir.join("undeclared.txt").exists());
    }

    #[test]
    fn retains_an_orphan_session_backup_instead_of_silently_losing_it() {
        let root = tempdir().unwrap();
        let current = root.path().join("current");
        let old = root.path().join("old");
        let data = root.path().join("data");
        let backups = root.path().join("backups");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&backups).unwrap();
        create_profile(&current, &[]);
        create_profile(&old, &[]);
        write_session(
            &old.join("sessions/2026/08/12/orphan.jsonl"),
            "thread-orphan",
            &["preserve"],
        );
        let old_backup = create_local_backup(&old, &backups, "manual-full-old").unwrap();

        let prepared = prepare_legacy_backup_reconciliation(
            &current,
            &data,
            &backups,
            "migration-1",
            u128::MAX,
            "legacy-reconcile-3",
        )
        .unwrap();
        let receipt = execute_legacy_backup_reconciliation(&prepared.plan, || Ok(())).unwrap();
        cleanup_reconciliation_staging(&prepared.plan).unwrap();

        assert_eq!(receipt.deleted_backup_count, 0);
        assert_eq!(receipt.retained_backup_count, 1);
        assert!(old_backup.backup_dir.exists());
    }

    #[test]
    fn recovery_finishes_a_publish_interrupted_before_source_backup_deletion() {
        let root = tempdir().unwrap();
        let current = root.path().join("current");
        let old = root.path().join("old");
        let data = root.path().join("data");
        let backups = root.path().join("backups");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&backups).unwrap();
        create_profile(&current, &[]);
        create_profile(&old, &[("thread-recovery", &["preserve"])]);
        let old_backup = create_local_backup(&old, &backups, "manual-full-old").unwrap();
        let store = OperationLedgerStore::new(&data);
        let operation_id = "legacy-reconcile-recovery-applying";
        enter_reconciliation_applying(&store, operation_id, &current);
        let prepared = prepare_legacy_backup_reconciliation(
            &current,
            &data,
            &backups,
            "migration-1",
            u128::MAX,
            operation_id,
        )
        .unwrap();

        let mut mutation_count = 0_usize;
        let interrupted = execute_legacy_backup_reconciliation(&prepared.plan, || {
            mutation_count = mutation_count.saturating_add(1);
            if mutation_count == 2 {
                Err("injected interruption before backup deletion".to_string())
            } else {
                Ok(())
            }
        });
        assert!(interrupted.is_err());
        assert!(old_backup.backup_dir.exists());
        assert_eq!(
            list_pending_recovery(&data, "migration-1")
                .unwrap()
                .entries
                .len(),
            1
        );

        let recovery = recover_interrupted_legacy_backup_reconciliation(
            &store,
            &data,
            operation_id,
            || Ok(()),
        )
        .unwrap();
        let receipt = reconciliation_receipt_from_applied_state(&prepared.plan).unwrap();

        assert_eq!(recovery, LegacyBackupRecoveryStatus::Committed);
        assert_eq!(
            store.load(operation_id).unwrap().phase,
            SessionStorageOperationPhase::Committed
        );
        assert!(!old_backup.backup_dir.exists());
        assert_eq!(receipt.deleted_backup_count, 1);
        assert_eq!(receipt.pending_recovery_count, 1);
        assert!(receipt.reclaimed_bytes > 0);
        assert!(!prepared.plan.staging_root.exists());
    }

    #[test]
    fn recovery_commits_an_already_applied_validating_reconciliation_idempotently() {
        let root = tempdir().unwrap();
        let current = root.path().join("current");
        let old = root.path().join("old");
        let data = root.path().join("data");
        let backups = root.path().join("backups");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&backups).unwrap();
        create_profile(&current, &[("thread-same", &["same"])]);
        create_profile(&old, &[("thread-same", &["same"])]);
        let old_backup = create_local_backup(&old, &backups, "manual-full-old").unwrap();
        let store = OperationLedgerStore::new(&data);
        let operation_id = "legacy-reconcile-recovery-validating";
        enter_reconciliation_applying(&store, operation_id, &current);
        let prepared = prepare_legacy_backup_reconciliation(
            &current,
            &data,
            &backups,
            "migration-1",
            u128::MAX,
            operation_id,
        )
        .unwrap();
        let applied = execute_legacy_backup_reconciliation(&prepared.plan, || Ok(())).unwrap();
        store
            .transition(operation_id, SessionStorageOperationPhase::Validating)
            .unwrap();

        let recovery = recover_interrupted_legacy_backup_reconciliation(
            &store,
            &data,
            operation_id,
            || Ok(()),
        )
        .unwrap();
        let recovered = reconciliation_receipt_from_applied_state(&prepared.plan).unwrap();

        assert_eq!(recovery, LegacyBackupRecoveryStatus::Committed);
        assert_eq!(applied, recovered);
        assert!(!old_backup.backup_dir.exists());
        assert!(!prepared.plan.staging_root.exists());
    }

    #[test]
    fn retention_never_deletes_an_invalid_pending_recovery_directory() {
        let root = tempdir().unwrap();
        let data = root.path().join("data");
        let invalid = data
            .join("session-storage-v1/pending-recovery")
            .join("untrusted");
        fs::create_dir_all(&invalid).unwrap();
        fs::write(invalid.join("unknown.jsonl"), b"do not delete").unwrap();

        let receipt = cleanup_expired_pending_recovery(&data, u128::MAX).unwrap();

        assert_eq!(receipt.deleted_package_count, 0);
        assert_eq!(receipt.invalid_package_count, 1);
        assert!(invalid.exists());
    }
}

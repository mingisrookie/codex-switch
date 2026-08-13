use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use serde::Serialize;

use super::{
    conflict::{
        load_deferred_conflict_ids, load_resolved_conflict_ids,
        migration_conflict_candidates_for_namespace, record_resolved_conflict,
        restore_import_conflict_candidates, stable_migration_conflict_candidates,
    },
    conflict_resolution::load_conflict_resolution_plan,
    investigation::prune_expired_investigation_tasks,
    legacy_backup::{
        cleanup_expired_pending_recovery, list_pending_recovery, PendingRecoveryRelation,
        PendingRecoveryStatus,
    },
    migration::load_migration_preflight,
    migration_apply::stable_file_digest,
    migration_backup::{delete_expired_migration_backup, verify_migration_backup},
    operation_ledger::{
        OperationLedgerStore, SessionStorageOperationKind, SessionStorageOperationLedger,
        SessionStorageOperationPhase,
    },
    reference_graph::path_key,
    restore_import::load_restore_import_plan,
    storage_state::load_committed_canonical_storage_state,
};

pub const SESSION_STORAGE_RETENTION_MS: u128 = 7 * 24 * 60 * 60 * 1_000;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionStorageRetentionReceipt {
    pub deleted_recovery_package_count: usize,
    pub deleted_pending_recovery_package_count: usize,
    pub deleted_operation_count: usize,
    pub deleted_investigation_task_count: usize,
    pub retained_artifact_count: usize,
    pub blocked_artifact_count: usize,
    pub reclaimed_bytes: u64,
}

pub fn run_session_storage_retention(
    data_root: &Path,
    active_migration_operation_id: Option<&str>,
    now_ms: u128,
) -> Result<SessionStorageRetentionReceipt, String> {
    validate_data_root(data_root)?;
    let store = OperationLedgerStore::new(data_root);
    let ledgers = store.all()?;
    let mut receipt = SessionStorageRetentionReceipt {
        deleted_recovery_package_count: 0,
        deleted_pending_recovery_package_count: 0,
        deleted_operation_count: 0,
        deleted_investigation_task_count: 0,
        retained_artifact_count: 0,
        blocked_artifact_count: 0,
        reclaimed_bytes: 0,
    };
    let mut blocked_operation_ids = BTreeSet::new();
    let primary_context = active_migration_operation_id.and_then(|active| {
        ledgers.iter().find_map(|ledger| {
            load_committed_canonical_storage_state(data_root, &ledger.canonical_root)
                .ok()
                .flatten()
                .filter(|state| state.migration_operation_id == active)
                .map(|state| (active, state.canonical_root))
        })
    });
    let unresolved_pending_recovery = match primary_context.as_ref() {
        Some((active, _)) => list_pending_recovery(data_root, active)
            .map(|pending| {
                pending.invalid_package_count > 0
                    || pending.entries.iter().any(|entry| {
                        entry.status == PendingRecoveryStatus::Pending
                            && matches!(
                                entry.relation,
                                PendingRecoveryRelation::Divergent
                                    | PendingRecoveryRelation::Unknown
                            )
                    })
            })
            .unwrap_or(true),
        None => data_root
            .join("session-storage-v1/pending-recovery")
            .exists(),
    };
    let pending_operations = store.cleanup_abandoned_pending_operations(
        now_ms.saturating_sub(SESSION_STORAGE_RETENTION_MS),
    )?;
    receipt.retained_artifact_count = receipt.retained_artifact_count.saturating_add(
        usize::try_from(pending_operations.retained_pending_operation_count).unwrap_or(usize::MAX),
    );
    receipt.blocked_artifact_count = receipt.blocked_artifact_count.saturating_add(
        usize::try_from(pending_operations.blocked_pending_operation_count).unwrap_or(usize::MAX),
    );
    add_reclaimed(&mut receipt, pending_operations.reclaimed_bytes)?;

    let investigation = prune_expired_investigation_tasks(data_root, now_ms)?;
    receipt.deleted_investigation_task_count = investigation.deleted_task_count;
    receipt.retained_artifact_count = receipt
        .retained_artifact_count
        .saturating_add(investigation.retained_task_count);
    receipt.blocked_artifact_count = receipt
        .blocked_artifact_count
        .saturating_add(investigation.blocked_task_count);
    add_reclaimed(&mut receipt, investigation.reclaimed_bytes)?;

    for ledger in &ledgers {
        if !matches!(
            ledger.phase,
            SessionStorageOperationPhase::Committed
                | SessionStorageOperationPhase::RolledBack
                | SessionStorageOperationPhase::Failed
        ) {
            receipt.blocked_artifact_count = receipt.blocked_artifact_count.saturating_add(1);
            continue;
        }
        if ledger.kind == SessionStorageOperationKind::Migration
            && ledger.phase == SessionStorageOperationPhase::Committed
        {
            let may_expire =
                committed_migration_may_expire(data_root, ledger, active_migration_operation_id);
            if !may_expire {
                blocked_operation_ids.insert(ledger.operation_id.clone());
                receipt.blocked_artifact_count = receipt.blocked_artifact_count.saturating_add(1);
                continue;
            }
        }
        if ledger.kind == SessionStorageOperationKind::ConflictResolution
            && ledger.phase == SessionStorageOperationPhase::Committed
        {
            let plan = load_conflict_resolution_plan(data_root, &ledger.operation_id);
            let durable = match (active_migration_operation_id, plan) {
                (Some(active), Ok(plan)) if plan.migration_operation_id == active => {
                    record_resolved_conflict(
                        data_root,
                        &plan.canonical_root,
                        active,
                        &plan.conflict_id,
                    )
                }
                (None, _) | (Some(_), Ok(_)) | (_, Err(_)) => {
                    Err("conflict resolution cannot expire without durable state".to_string())
                }
            };
            if durable.is_err() {
                blocked_operation_ids.insert(ledger.operation_id.clone());
                receipt.blocked_artifact_count = receipt.blocked_artifact_count.saturating_add(1);
                continue;
            }
        }
        match ledger.kind {
            SessionStorageOperationKind::Migration
            | SessionStorageOperationKind::OfflineGc
            | SessionStorageOperationKind::ConflictResolution => {
                cleanup_migration_format_recovery(data_root, ledger, now_ms, &mut receipt);
            }
            SessionStorageOperationKind::RestoreImport => {
                match load_restore_import_plan(data_root, &ledger.operation_id) {
                    Ok(plan) if plan.unclassified_payloads.is_empty() => {
                        let conflicts_resolved =
                            primary_context
                                .as_ref()
                                .is_some_and(|(active, canonical_root)| {
                                    restore_import_conflicts_are_resolved(
                                        data_root,
                                        canonical_root.as_path(),
                                        active,
                                        &plan,
                                    )
                                });
                        if conflicts_resolved {
                            let recovery_clean = cleanup_restore_import_recovery(
                                data_root,
                                ledger,
                                now_ms,
                                &mut receipt,
                            );
                            let staging_clean = recovery_clean
                                && cleanup_restore_import_staging(data_root, ledger, &plan);
                            if !staging_clean {
                                blocked_operation_ids.insert(ledger.operation_id.clone());
                                receipt.blocked_artifact_count =
                                    receipt.blocked_artifact_count.saturating_add(1);
                            }
                        } else {
                            blocked_operation_ids.insert(ledger.operation_id.clone());
                            receipt.blocked_artifact_count =
                                receipt.blocked_artifact_count.saturating_add(1);
                        }
                    }
                    Ok(_) | Err(_) => {
                        blocked_operation_ids.insert(ledger.operation_id.clone());
                        receipt.blocked_artifact_count =
                            receipt.blocked_artifact_count.saturating_add(1);
                    }
                }
            }
            SessionStorageOperationKind::DowngradeExport
            | SessionStorageOperationKind::LegacyBackupReconciliation => {}
        }
    }

    if unresolved_pending_recovery {
        receipt.blocked_artifact_count = receipt.blocked_artifact_count.saturating_add(1);
    } else {
        let pending = cleanup_expired_pending_recovery(data_root, now_ms)?;
        receipt.deleted_pending_recovery_package_count = pending.deleted_package_count;
        receipt.retained_artifact_count = receipt
            .retained_artifact_count
            .saturating_add(pending.retained_package_count);
        receipt.blocked_artifact_count = receipt
            .blocked_artifact_count
            .saturating_add(pending.invalid_package_count);
        add_reclaimed(&mut receipt, pending.reclaimed_bytes)?;
    }

    for ledger in ledgers {
        if blocked_operation_ids.contains(&ledger.operation_id)
            || !matches!(
                ledger.phase,
                SessionStorageOperationPhase::Committed
                    | SessionStorageOperationPhase::RolledBack
                    | SessionStorageOperationPhase::Failed
            )
            || !retention_elapsed(ledger.updated_at_ms, now_ms)
            || operation_recovery_still_exists(&ledger)
        {
            continue;
        }
        match store.remove_terminal_operation(&ledger.operation_id) {
            Ok(bytes) => {
                receipt.deleted_operation_count = receipt.deleted_operation_count.saturating_add(1);
                add_reclaimed(&mut receipt, bytes)?;
            }
            Err(_) => {
                receipt.blocked_artifact_count = receipt.blocked_artifact_count.saturating_add(1);
            }
        }
    }
    Ok(receipt)
}

fn committed_migration_may_expire(
    data_root: &Path,
    ledger: &SessionStorageOperationLedger,
    active_migration_operation_id: Option<&str>,
) -> bool {
    let Some(active) = active_migration_operation_id else {
        // Without the canonical identity, a committed Migration ledger could
        // be the only proof for a cutover or later repair. Preserve it.
        return false;
    };
    let state = match load_committed_canonical_storage_state(data_root, &ledger.canonical_root) {
        Ok(Some(state)) if state.migration_operation_id == active => state,
        Ok(_) | Err(_) => return false,
    };
    if path_key(&state.canonical_root) != path_key(&ledger.canonical_root) {
        return false;
    }
    let resolved = match load_resolved_conflict_ids(data_root, &ledger.canonical_root, active) {
        Ok(resolved) => resolved,
        Err(_) => return false,
    };
    // The deferred registry is independent proof after migration preflights
    // expire. A corrupt/unreadable registry must therefore block retention.
    if load_deferred_conflict_ids(data_root, &ledger.canonical_root, active).is_err() {
        return false;
    }
    // Each retained migration preflight is the durable inventory for conflicts
    // discovered during cutover or repair, including candidates outside managed
    // roots. Do not expire that proof until every still-applicable conflict is
    // durably resolved. Once this check succeeds the preflight and ledger may
    // expire together; subsequent consumers rely on the committed v2 state.
    let preflight = match load_migration_preflight(data_root, &ledger.operation_id) {
        Ok(preflight)
            if preflight.operation_id == ledger.operation_id
                && path_key(&preflight.plan.canonical_root) == path_key(&ledger.canonical_root) =>
        {
            preflight
        }
        Ok(_) | Err(_) => return false,
    };
    if !migration_conflict_candidates_for_namespace(&preflight, active, &resolved)
        .is_ok_and(|conflicts| conflicts.is_empty())
    {
        return false;
    }
    stable_migration_conflict_candidates(&ledger.canonical_root, data_root, active, &resolved)
        .is_ok_and(|conflicts| conflicts.is_empty())
}

fn restore_import_conflicts_are_resolved(
    data_root: &Path,
    canonical_root: &Path,
    migration_operation_id: &str,
    plan: &super::restore_import::RestoreImportPlan,
) -> bool {
    if plan.conflicts.is_empty() {
        return true;
    }
    let resolved =
        match load_resolved_conflict_ids(data_root, canonical_root, migration_operation_id) {
            Ok(resolved) => resolved,
            Err(_) => return false,
        };
    if load_deferred_conflict_ids(data_root, canonical_root, migration_operation_id).is_err() {
        return false;
    }
    restore_import_conflict_candidates(migration_operation_id, plan).is_ok_and(|candidates| {
        candidates
            .iter()
            .all(|candidate| resolved.contains(&candidate.summary.conflict_id))
    })
}

fn cleanup_migration_format_recovery(
    data_root: &Path,
    ledger: &SessionStorageOperationLedger,
    now_ms: u128,
    receipt: &mut SessionStorageRetentionReceipt,
) {
    let Some(backup_root) = ledger.backup_root.as_ref() else {
        return;
    };
    if !backup_root.exists() {
        return;
    }
    if !migration_recovery_path_is_expected(data_root, ledger, backup_root) {
        receipt.blocked_artifact_count = receipt.blocked_artifact_count.saturating_add(1);
        return;
    }
    let manifest = match verify_migration_backup(backup_root) {
        Ok(manifest) if manifest.operation_id == ledger.operation_id => manifest,
        Ok(_) | Err(_) => {
            receipt.blocked_artifact_count = receipt.blocked_artifact_count.saturating_add(1);
            return;
        }
    };
    if manifest.expires_at_ms > now_ms {
        receipt.retained_artifact_count = receipt.retained_artifact_count.saturating_add(1);
        return;
    }
    match delete_expired_migration_backup(backup_root, &ledger.operation_id, now_ms) {
        Ok(bytes) => {
            receipt.deleted_recovery_package_count =
                receipt.deleted_recovery_package_count.saturating_add(1);
            if add_reclaimed(receipt, bytes).is_err() {
                receipt.blocked_artifact_count = receipt.blocked_artifact_count.saturating_add(1);
            }
        }
        Err(_) => {
            receipt.blocked_artifact_count = receipt.blocked_artifact_count.saturating_add(1);
        }
    }
}

fn cleanup_restore_import_recovery(
    data_root: &Path,
    ledger: &SessionStorageOperationLedger,
    now_ms: u128,
    receipt: &mut SessionStorageRetentionReceipt,
) -> bool {
    let Some(recovery_root) = ledger.backup_root.as_ref() else {
        return true;
    };
    if !recovery_root.exists() {
        return true;
    }
    let expected = data_root
        .join("session-storage-v1/restore-import-recovery")
        .join(&ledger.operation_id);
    if recovery_root != &expected || !retention_elapsed(ledger.updated_at_ms, now_ms) {
        receipt.retained_artifact_count = receipt.retained_artifact_count.saturating_add(1);
        return false;
    }
    match remove_snapshot_bound_tree(recovery_root, ledger) {
        Ok(bytes) => {
            receipt.deleted_recovery_package_count =
                receipt.deleted_recovery_package_count.saturating_add(1);
            if add_reclaimed(receipt, bytes).is_err() {
                receipt.blocked_artifact_count = receipt.blocked_artifact_count.saturating_add(1);
                false
            } else {
                true
            }
        }
        Err(_) => {
            receipt.blocked_artifact_count = receipt.blocked_artifact_count.saturating_add(1);
            false
        }
    }
}

fn cleanup_restore_import_staging(
    data_root: &Path,
    ledger: &SessionStorageOperationLedger,
    plan: &super::restore_import::RestoreImportPlan,
) -> bool {
    let expected = data_root
        .join("session-storage-v1/operations")
        .join(&ledger.operation_id)
        .join("restore-import-staging");
    if plan.staging_root != expected {
        return false;
    }
    let metadata = match fs::symlink_metadata(&expected) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return true,
        Err(_) => return false,
    };
    if !metadata.is_dir() || metadata_is_link_or_reparse(&metadata) {
        return false;
    }
    let mut entries = match fs::read_dir(&expected) {
        Ok(entries) => entries,
        Err(_) => return false,
    };
    if entries.next().is_some() {
        false
    } else {
        fs::remove_dir(&expected).is_ok()
    }
}

fn migration_recovery_path_is_expected(
    data_root: &Path,
    ledger: &SessionStorageOperationLedger,
    backup_root: &Path,
) -> bool {
    match ledger.kind {
        SessionStorageOperationKind::Migration => true,
        SessionStorageOperationKind::OfflineGc => {
            backup_root
                == data_root
                    .join("session-storage-v1/offline-gc-recovery")
                    .join(&ledger.operation_id)
        }
        SessionStorageOperationKind::ConflictResolution => {
            backup_root
                == data_root
                    .join("session-storage-v1/conflict-recovery")
                    .join(&ledger.operation_id)
        }
        _ => false,
    }
}

fn operation_recovery_still_exists(ledger: &SessionStorageOperationLedger) -> bool {
    match ledger.kind {
        SessionStorageOperationKind::Migration
        | SessionStorageOperationKind::OfflineGc
        | SessionStorageOperationKind::ConflictResolution
        | SessionStorageOperationKind::RestoreImport => ledger
            .backup_root
            .as_ref()
            .is_some_and(|path| path.exists()),
        SessionStorageOperationKind::DowngradeExport
        | SessionStorageOperationKind::LegacyBackupReconciliation => false,
    }
}

fn remove_snapshot_bound_tree(
    root: &Path,
    ledger: &SessionStorageOperationLedger,
) -> Result<u64, String> {
    let metadata = fs::symlink_metadata(root)
        .map_err(|_| "session recovery package is unavailable".to_string())?;
    if !root.is_absolute() || !metadata.is_dir() || metadata_is_link_or_reparse(&metadata) {
        return Err("session recovery package is unsafe".to_string());
    }
    let expected = ledger
        .created_files
        .iter()
        .filter(|snapshot| snapshot.created_by_operation && snapshot.path.starts_with(root))
        .map(|snapshot| (path_key(&snapshot.path), snapshot))
        .collect::<std::collections::BTreeMap<_, _>>();
    if expected.is_empty() {
        return Err("session recovery package has no ledger inventory".to_string());
    }
    let mut files = Vec::new();
    let mut directories = Vec::new();
    collect_tree(root, &mut files, &mut directories)?;
    let actual = files
        .iter()
        .map(|path| path_key(path))
        .collect::<BTreeSet<_>>();
    if actual != expected.keys().cloned().collect::<BTreeSet<_>>() {
        return Err("session recovery package file inventory changed".to_string());
    }
    let mut bytes = 0_u64;
    for path in &files {
        let snapshot = expected
            .get(&path_key(path))
            .ok_or_else(|| "session recovery package contains an undeclared file".to_string())?;
        let (actual_bytes, sha256) = stable_file_digest(path)?;
        if actual_bytes != snapshot.bytes || sha256 != snapshot.sha256 {
            return Err("session recovery package content changed".to_string());
        }
        bytes = bytes
            .checked_add(actual_bytes)
            .ok_or_else(|| "session recovery package size overflowed".to_string())?;
    }
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for path in files {
        fs::remove_file(path)
            .map_err(|_| "failed to remove expired session recovery file".to_string())?;
    }
    for path in directories {
        fs::remove_dir(path)
            .map_err(|_| "failed to remove expired session recovery directory".to_string())?;
    }
    Ok(bytes)
}

fn collect_tree(
    root: &Path,
    files: &mut Vec<PathBuf>,
    directories: &mut Vec<PathBuf>,
) -> Result<(), String> {
    let metadata = fs::symlink_metadata(root)
        .map_err(|_| "session recovery tree is unavailable".to_string())?;
    if !metadata.is_dir() || metadata_is_link_or_reparse(&metadata) {
        return Err("session recovery tree is unsafe".to_string());
    }
    directories.push(root.to_path_buf());
    for entry in
        fs::read_dir(root).map_err(|_| "session recovery tree is unreadable".to_string())?
    {
        let entry = entry.map_err(|_| "session recovery entry is unreadable".to_string())?;
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|_| "session recovery entry is unreadable".to_string())?;
        if metadata_is_link_or_reparse(&metadata) {
            return Err("session recovery tree contains a link".to_string());
        }
        if metadata.is_dir() {
            collect_tree(&entry.path(), files, directories)?;
        } else if metadata.is_file() {
            files.push(entry.path());
        } else {
            return Err("session recovery tree contains an unsupported entry".to_string());
        }
    }
    Ok(())
}

fn retention_elapsed(updated_at_ms: u128, now_ms: u128) -> bool {
    updated_at_ms
        .checked_add(SESSION_STORAGE_RETENTION_MS)
        .is_some_and(|expires_at_ms| expires_at_ms <= now_ms)
}

fn add_reclaimed(receipt: &mut SessionStorageRetentionReceipt, bytes: u64) -> Result<(), String> {
    receipt.reclaimed_bytes = receipt
        .reclaimed_bytes
        .checked_add(bytes)
        .ok_or_else(|| "session storage retention byte count overflowed".to_string())?;
    Ok(())
}

fn validate_data_root(data_root: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(data_root)
        .map_err(|_| "session storage retention root is unavailable".to_string())?;
    if !data_root.is_absolute() || !metadata.is_dir() || metadata_is_link_or_reparse(&metadata) {
        Err("session storage retention root is unsafe".to_string())
    } else {
        Ok(())
    }
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
    use tempfile::tempdir;

    use super::run_session_storage_retention;
    use crate::session_storage::{
        conflict::{
            migration_conflict_candidates_for_namespace, record_deferred_conflict,
            record_resolved_conflict, restore_import_conflict_candidates,
        },
        migration::{
            load_migration_preflight, persist_migration_preflight, run_migration_preflight,
        },
        migration_apply::stable_file_digest,
        migration_backup::{
            create_migration_backup, MigrationBackupEntryKind, MigrationBackupSource,
        },
        operation_ledger::{
            LedgerFileSnapshot, OperationLedgerStore, SessionStorageOperationKind,
            SessionStorageOperationPhase,
        },
        restore_import::{
            persist_restore_import_plan, RestoreImportConflictPlan, RestoreImportPlan,
            RestoreImportSourceKind, RestoreImportUnclassifiedPlan,
            RestoreImportUnclassifiedReason,
        },
        storage_state::{
            finalize_canonical_storage_state, load_committed_canonical_storage_state,
            prepare_canonical_storage_state, set_automatic_cleanup_enabled,
        },
        SessionRelation,
    };

    fn commit_existing_ledger(
        store: &OperationLedgerStore,
        operation_id: &str,
        kind: SessionStorageOperationKind,
        canonical_root: &std::path::Path,
        backup_root: Option<std::path::PathBuf>,
        created_files: Vec<LedgerFileSnapshot>,
    ) {
        let existing = store.load(operation_id).unwrap();
        assert_eq!(existing.kind, kind);
        assert_eq!(existing.canonical_root, canonical_root);
        store
            .transition(operation_id, SessionStorageOperationPhase::Preflight)
            .unwrap();
        store
            .update(operation_id, |ledger| {
                ledger.backup_root = backup_root;
                ledger.created_files = created_files;
                Ok(())
            })
            .unwrap();
        for phase in [
            SessionStorageOperationPhase::Backup,
            SessionStorageOperationPhase::BackupVerified,
            SessionStorageOperationPhase::PlanReady,
            SessionStorageOperationPhase::Applying,
            SessionStorageOperationPhase::Validating,
            SessionStorageOperationPhase::Committed,
        ] {
            store.transition(operation_id, phase).unwrap();
        }
    }

    fn migration_fixture(
        add_undeclared_file: bool,
    ) -> (
        tempfile::TempDir,
        std::path::PathBuf,
        std::path::PathBuf,
        OperationLedgerStore,
    ) {
        migration_fixture_inner(add_undeclared_file, false)
    }

    fn migration_fixture_with_unresolved_external_conflict() -> (
        tempfile::TempDir,
        std::path::PathBuf,
        std::path::PathBuf,
        OperationLedgerStore,
    ) {
        migration_fixture_inner(false, true)
    }

    fn migration_fixture_inner(
        add_undeclared_file: bool,
        add_external_conflict: bool,
    ) -> (
        tempfile::TempDir,
        std::path::PathBuf,
        std::path::PathBuf,
        OperationLedgerStore,
    ) {
        let root = tempdir().unwrap();
        let data = root.path().join("data");
        let canonical = root.path().join("canonical");
        let backups = root.path().join("backups");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&canonical).unwrap();
        fs::create_dir_all(&backups).unwrap();
        fs::write(
            canonical.join("config.toml"),
            format!("sqlite_home = {:?}\n", canonical.to_string_lossy()),
        )
        .unwrap();
        let state_database = Connection::open(canonical.join("state_5.sqlite")).unwrap();
        state_database
            .execute_batch(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, model_provider TEXT);",
            )
            .unwrap();
        let canonical_conflict_path = canonical.join("sessions/canonical-conflict.jsonl");
        let external_conflict_path = root.path().join("external/candidate-conflict.jsonl");
        if add_external_conflict {
            write_session(&canonical_conflict_path, "openai", &["root", "canonical"]);
            write_session(
                &external_conflict_path,
                "openai_custom",
                &["root", "external"],
            );
            state_database
                .execute(
                    "INSERT INTO threads VALUES (?1, ?2, 'openai_custom')",
                    (
                        "thread-retention-conflict",
                        external_conflict_path.to_string_lossy().to_string(),
                    ),
                )
                .unwrap();
        }
        drop(state_database);
        create_goals_database(&canonical.join("goals_1.sqlite"));
        let operation_id = "migration-retention-1";
        let store = OperationLedgerStore::new(&data);
        store
            .create(
                operation_id,
                SessionStorageOperationKind::Migration,
                &canonical,
            )
            .unwrap();
        let preflight = run_migration_preflight(&canonical, &data, operation_id, &backups).unwrap();
        assert!(preflight.ready_for_backup, "{:?}", preflight.blockers);
        if add_external_conflict {
            assert_eq!(preflight.plan.conflicts.len(), 1);
        }
        persist_migration_preflight(&data, &preflight).unwrap();
        if add_external_conflict {
            // Simulate the post-cutover catalog no longer referencing an
            // external divergent branch. It must remain protected by the
            // primary preflight even though a fresh scan cannot rediscover it.
            Connection::open(canonical.join("state_5.sqlite"))
                .unwrap()
                .execute(
                    "UPDATE threads SET rollout_path = ?1, model_provider = 'openai' WHERE id = ?2",
                    (
                        canonical_conflict_path.to_string_lossy().to_string(),
                        "thread-retention-conflict",
                    ),
                )
                .unwrap();
        }
        let preflight_path = data
            .join("session-storage-v1/operations")
            .join(operation_id)
            .join("preflight.json");
        let (preflight_bytes, preflight_sha256) = stable_file_digest(&preflight_path).unwrap();
        let backup = create_migration_backup(
            &backups,
            operation_id,
            &[MigrationBackupSource {
                source_path: canonical.join("config.toml"),
                payload_relative_path: "metadata/storage-state.json".into(),
                kind: MigrationBackupEntryKind::StorageMetadata,
                expected_sha256: None,
                logical_thread_id: None,
            }],
        )
        .unwrap();
        if add_undeclared_file {
            fs::write(backup.backup_dir.join("untrusted.txt"), b"preserve").unwrap();
        }
        store
            .update(operation_id, |ledger| {
                ledger.backup_root = Some(backup.backup_dir.clone());
                ledger.created_files = vec![LedgerFileSnapshot {
                    path: preflight_path.clone(),
                    bytes: preflight_bytes,
                    sha256: preflight_sha256.clone(),
                    created_by_operation: true,
                    logical_thread_id: None,
                }];
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
        finalize_canonical_storage_state(&data, &canonical, operation_id).unwrap();
        (root, data, backup.backup_dir, store)
    }

    fn write_session(path: &std::path::Path, provider: &str, messages: &[&str]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut lines = vec![serde_json::json!({
            "type": "session_meta",
            "payload": {
                "id": "thread-retention-conflict",
                "model_provider": provider,
            }
        })
        .to_string()];
        lines.extend(messages.iter().map(|message| {
            serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "user_message", "message": message},
            })
            .to_string()
        }));
        fs::write(path, lines.join("\n") + "\n").unwrap();
    }

    fn create_goals_database(path: &std::path::Path) {
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

    fn add_restore_conflict(
        root: &std::path::Path,
        data: &std::path::Path,
        canonical: &std::path::Path,
        store: &OperationLedgerStore,
        operation_id: &str,
        thread_id: &str,
    ) -> (std::path::PathBuf, String) {
        let package = root.join(format!("package-{operation_id}"));
        let current = canonical
            .join("sessions")
            .join(format!("{thread_id}-current.jsonl"));
        let candidate = package
            .join("sessions")
            .join(format!("{thread_id}-candidate.jsonl"));
        fs::create_dir_all(current.parent().unwrap()).unwrap();
        fs::create_dir_all(candidate.parent().unwrap()).unwrap();
        let session = |provider: &str, messages: &[&str]| {
            let mut lines = vec![serde_json::json!({
                "type": "session_meta",
                "timestamp": "2026-08-12T00:00:00Z",
                "payload": {"id": thread_id, "model_provider": provider}
            })
            .to_string()];
            lines.extend(messages.iter().enumerate().map(|(index, message)| {
                serde_json::json!({
                    "type": "event_msg",
                    "timestamp": format!("2026-08-12T00:00:0{}Z", index + 1),
                    "payload": {"type": "user_message", "message": message}
                })
                .to_string()
            }));
            lines.join("\n") + "\n"
        };
        fs::write(&current, session("openai", &["common", "left"])).unwrap();
        fs::write(&candidate, session("openai_custom", &["common", "right"])).unwrap();
        let (_, current_sha256) = stable_file_digest(&current).unwrap();
        let (_, candidate_sha256) = stable_file_digest(&candidate).unwrap();

        store
            .create(
                operation_id,
                SessionStorageOperationKind::RestoreImport,
                canonical,
            )
            .unwrap();
        let work_root = data
            .join("session-storage-v1/operations")
            .join(operation_id);
        let staging_root = work_root.join("restore-import-staging");
        let recovery_root = data
            .join("session-storage-v1/restore-import-recovery")
            .join(operation_id);
        let recovery = recovery_root
            .join("conflicts")
            .join(format!("{thread_id}-candidate.jsonl"));
        fs::create_dir_all(&staging_root).unwrap();
        fs::create_dir_all(recovery.parent().unwrap()).unwrap();
        fs::copy(&candidate, &recovery).unwrap();
        let plan = RestoreImportPlan {
            schema_version: 1,
            operation_id: operation_id.to_string(),
            generated_at_ms: 1,
            package_operation_id: format!("source-{operation_id}"),
            target_version: "v0.2.7".to_string(),
            source_kind: RestoreImportSourceKind::DowngradePackage,
            package_dir: package,
            canonical_root: canonical.to_path_buf(),
            data_root: data.to_path_buf(),
            source_fingerprint: "b".repeat(64),
            work_root: work_root.clone(),
            staging_root,
            recovery_root: recovery_root.clone(),
            recovery_expires_at_ms: 2,
            sessions: Vec::new(),
            conflicts: vec![RestoreImportConflictPlan {
                thread_id: thread_id.to_string(),
                current_path: Some(current),
                current_sha256: Some(current_sha256),
                candidate_paths: vec![candidate],
                candidate_sha256: vec![candidate_sha256],
                recovery_paths: vec![recovery.clone()],
                relation: SessionRelation::Divergent,
                reason: "histories diverged".to_string(),
                default_overwrite: false,
            }],
            unclassified_payloads: Vec::new(),
            source_databases: Vec::new(),
            databases: Vec::new(),
            anomaly_count: 0,
        };
        persist_restore_import_plan(data, &plan).unwrap();
        let conflict_id = restore_import_conflict_candidates("migration-retention-1", &plan)
            .unwrap()
            .remove(0)
            .summary
            .conflict_id;
        let plan_path = work_root.join("restore-import-plan.json");
        let (plan_bytes, plan_sha256) = stable_file_digest(&plan_path).unwrap();
        let (recovery_bytes, recovery_sha256) = stable_file_digest(&recovery).unwrap();
        store
            .update(operation_id, |ledger| {
                ledger.backup_root = Some(recovery_root.clone());
                ledger.created_files = vec![
                    LedgerFileSnapshot {
                        path: plan_path.clone(),
                        bytes: plan_bytes,
                        sha256: plan_sha256.clone(),
                        created_by_operation: true,
                        logical_thread_id: None,
                    },
                    LedgerFileSnapshot {
                        path: recovery.clone(),
                        bytes: recovery_bytes,
                        sha256: recovery_sha256.clone(),
                        created_by_operation: true,
                        logical_thread_id: Some(thread_id.to_string()),
                    },
                ];
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
            SessionStorageOperationPhase::Committed,
        ] {
            store.transition(operation_id, phase).unwrap();
        }
        (recovery_root, conflict_id)
    }

    #[test]
    fn committed_v2_certificate_allows_primary_backup_preflight_and_ledger_expiry() {
        let (root, data, backup_dir, store) = migration_fixture(false);
        let operation_root = data
            .join("session-storage-v1/operations")
            .join("migration-retention-1");
        assert!(operation_root.join("preflight.json").is_file());

        let receipt =
            run_session_storage_retention(&data, Some("migration-retention-1"), u128::MAX).unwrap();

        assert_eq!(receipt.deleted_recovery_package_count, 1);
        assert_eq!(receipt.deleted_operation_count, 1);
        assert!(receipt.reclaimed_bytes > 0);
        assert!(!backup_dir.exists());
        assert!(!operation_root.exists());
        assert!(store.load("migration-retention-1").is_err());
        assert!(
            load_committed_canonical_storage_state(&data, &root.path().join("canonical"))
                .unwrap()
                .is_some()
        );
        let canonical = root.path().join("canonical");
        let (recovery, conflict_id) = add_restore_conflict(
            root.path(),
            &data,
            &canonical,
            &store,
            "restore-after-primary-proof-expiry",
            "thread-after-proof-expiry",
        );
        record_resolved_conflict(&data, &canonical, "migration-retention-1", &conflict_id).unwrap();
        let after_proof_expiry =
            run_session_storage_retention(&data, Some("migration-retention-1"), u128::MAX).unwrap();
        assert_eq!(after_proof_expiry.deleted_recovery_package_count, 1);
        assert_eq!(after_proof_expiry.deleted_operation_count, 1);
        assert!(!recovery.exists());
        assert!(store.load("restore-after-primary-proof-expiry").is_err());
    }

    #[test]
    fn missing_confirmed_primary_identity_preserves_migration_proof() {
        let (_root, data, backup_dir, store) = migration_fixture(false);

        let receipt = run_session_storage_retention(&data, None, u128::MAX).unwrap();

        assert_eq!(receipt.deleted_recovery_package_count, 0);
        assert_eq!(receipt.deleted_operation_count, 0);
        assert!(receipt.blocked_artifact_count > 0);
        assert!(backup_dir.exists());
        assert!(store.load("migration-retention-1").is_ok());
    }

    #[test]
    fn unresolved_external_migration_conflict_preserves_primary_proof_until_resolved() {
        let (root, data, backup_dir, store) = migration_fixture_with_unresolved_external_conflict();
        let canonical = root.path().join("canonical");
        let report = load_migration_preflight(&data, "migration-retention-1").unwrap();
        let candidates = migration_conflict_candidates_for_namespace(
            &report,
            "migration-retention-1",
            &Default::default(),
        )
        .unwrap();
        assert_eq!(candidates.len(), 1);
        let conflict_id = candidates[0].summary.conflict_id.clone();

        let unresolved =
            run_session_storage_retention(&data, Some("migration-retention-1"), u128::MAX).unwrap();

        assert_eq!(unresolved.deleted_recovery_package_count, 0);
        assert_eq!(unresolved.deleted_operation_count, 0);
        assert!(unresolved.blocked_artifact_count > 0);
        assert!(backup_dir.exists());
        assert!(store.load("migration-retention-1").is_ok());

        record_resolved_conflict(&data, &canonical, "migration-retention-1", &conflict_id).unwrap();
        let resolved =
            run_session_storage_retention(&data, Some("migration-retention-1"), u128::MAX).unwrap();

        assert_eq!(resolved.deleted_recovery_package_count, 1);
        assert_eq!(resolved.deleted_operation_count, 1);
        assert!(!backup_dir.exists());
        assert!(store.load("migration-retention-1").is_err());
    }

    #[test]
    fn an_undeclared_backup_file_blocks_retention_deletion() {
        let (_root, data, backup_dir, store) = migration_fixture(true);

        let receipt =
            run_session_storage_retention(&data, Some("migration-retention-1"), u128::MAX).unwrap();

        assert_eq!(receipt.deleted_recovery_package_count, 0);
        assert!(receipt.blocked_artifact_count > 0);
        assert!(backup_dir.join("untrusted.txt").exists());
        assert!(store.load("migration-retention-1").is_ok());
    }

    #[test]
    fn unclassified_restore_payload_never_expires_automatically() {
        let root = tempdir().unwrap();
        let data = root.path().join("data");
        let canonical = root.path().join("canonical");
        let package = root.path().join("package");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&canonical).unwrap();
        fs::create_dir_all(package.join("recovery")).unwrap();
        let operation_id = "restore-unclassified-1";
        let store = OperationLedgerStore::new(&data);
        store
            .create(
                operation_id,
                SessionStorageOperationKind::RestoreImport,
                &canonical,
            )
            .unwrap();
        let work_root = data
            .join("session-storage-v1/operations")
            .join(operation_id);
        let staging_root = work_root.join("restore-import-staging");
        let recovery_root = data
            .join("session-storage-v1/restore-import-recovery")
            .join(operation_id);
        fs::create_dir_all(&staging_root).unwrap();
        fs::create_dir_all(recovery_root.join("unclassified")).unwrap();
        let source = package.join("recovery/unknown.bin");
        let recovery = recovery_root.join("unclassified/unknown.bin");
        fs::write(&source, b"unclassified recovery payload").unwrap();
        fs::copy(&source, &recovery).unwrap();
        let (source_bytes, source_sha256) = stable_file_digest(&source).unwrap();
        let plan = RestoreImportPlan {
            schema_version: 1,
            operation_id: operation_id.to_string(),
            generated_at_ms: 1,
            package_operation_id: "downgrade-source-1".to_string(),
            target_version: "v0.2.7".to_string(),
            source_kind: RestoreImportSourceKind::DowngradePackage,
            package_dir: package,
            canonical_root: canonical.clone(),
            data_root: data.clone(),
            source_fingerprint: "a".repeat(64),
            work_root: work_root.clone(),
            staging_root,
            recovery_root: recovery_root.clone(),
            recovery_expires_at_ms: u128::MAX,
            sessions: Vec::new(),
            conflicts: Vec::new(),
            unclassified_payloads: vec![RestoreImportUnclassifiedPlan {
                source_path: source,
                source_bytes,
                source_sha256,
                recovery_path: recovery.clone(),
                reason: RestoreImportUnclassifiedReason::RecoveryPayload,
            }],
            source_databases: Vec::new(),
            databases: Vec::new(),
            anomaly_count: 1,
        };
        persist_restore_import_plan(&data, &plan).unwrap();
        let plan_path = work_root.join("restore-import-plan.json");
        let (plan_bytes, plan_sha256) = stable_file_digest(&plan_path).unwrap();
        let (recovery_bytes, recovery_sha256) = stable_file_digest(&recovery).unwrap();
        commit_existing_ledger(
            &store,
            operation_id,
            SessionStorageOperationKind::RestoreImport,
            &canonical,
            Some(recovery_root.clone()),
            vec![
                LedgerFileSnapshot {
                    path: plan_path,
                    bytes: plan_bytes,
                    sha256: plan_sha256,
                    created_by_operation: true,
                    logical_thread_id: None,
                },
                LedgerFileSnapshot {
                    path: recovery.clone(),
                    bytes: recovery_bytes,
                    sha256: recovery_sha256,
                    created_by_operation: true,
                    logical_thread_id: None,
                },
            ],
        );

        let receipt = run_session_storage_retention(&data, None, u128::MAX).unwrap();

        assert_eq!(receipt.deleted_recovery_package_count, 0);
        assert_eq!(receipt.deleted_operation_count, 0);
        assert!(receipt.blocked_artifact_count > 0);
        assert!(recovery.is_file());
        assert!(store.load(operation_id).is_ok());
    }

    #[test]
    fn resolved_conflict_recovery_expires_but_deferred_payload_is_retained() {
        let (root, data, _migration_backup, store) = migration_fixture(false);
        let canonical = root.path().join("canonical");
        let (resolved_recovery, resolved_id) = add_restore_conflict(
            root.path(),
            &data,
            &canonical,
            &store,
            "restore-resolved-1",
            "thread-resolved",
        );
        let (deferred_recovery, deferred_id) = add_restore_conflict(
            root.path(),
            &data,
            &canonical,
            &store,
            "restore-deferred-1",
            "thread-deferred",
        );
        record_resolved_conflict(&data, &canonical, "migration-retention-1", &resolved_id).unwrap();
        record_deferred_conflict(&data, &canonical, "migration-retention-1", &deferred_id).unwrap();

        let receipt =
            run_session_storage_retention(&data, Some("migration-retention-1"), u128::MAX).unwrap();

        assert!(receipt.deleted_recovery_package_count >= 1);
        assert!(!resolved_recovery.exists());
        assert!(store.load("restore-resolved-1").is_err());
        assert!(deferred_recovery.exists());
        assert!(store.load("restore-deferred-1").is_ok());
        assert!(receipt.blocked_artifact_count > 0);

        record_resolved_conflict(&data, &canonical, "migration-retention-1", &deferred_id).unwrap();
        let after_resolution =
            run_session_storage_retention(&data, Some("migration-retention-1"), u128::MAX).unwrap();
        assert!(after_resolution.deleted_recovery_package_count >= 1);
        assert!(!deferred_recovery.exists());
        assert!(store.load("restore-deferred-1").is_err());
    }

    #[test]
    fn expires_only_hash_bound_terminal_operation_files() {
        let root = tempdir().unwrap();
        let data = root.path().join("data");
        let canonical = root.path().join("canonical");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&canonical).unwrap();
        let store = OperationLedgerStore::new(&data);
        let operation_id = "downgrade-retention-1";
        store
            .create(
                operation_id,
                SessionStorageOperationKind::DowngradeExport,
                &canonical,
            )
            .unwrap();
        let operation_root = data
            .join("session-storage-v1/operations")
            .join(operation_id);
        let plan = operation_root.join("downgrade-export-plan.json");
        fs::write(&plan, b"integrity-bound-plan").unwrap();
        let (bytes, sha256) = stable_file_digest(&plan).unwrap();
        store
            .update(operation_id, |ledger| {
                ledger.created_files = vec![LedgerFileSnapshot {
                    path: plan.clone(),
                    bytes,
                    sha256,
                    created_by_operation: true,
                    logical_thread_id: None,
                }];
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
            SessionStorageOperationPhase::Committed,
        ] {
            store.transition(operation_id, phase).unwrap();
        }

        let receipt = run_session_storage_retention(&data, None, u128::MAX).unwrap();

        assert_eq!(receipt.deleted_operation_count, 1);
        assert!(!operation_root.exists());
    }

    #[test]
    fn disabled_provider_gc_does_not_disable_privacy_lifecycle_retention() {
        let root = tempdir().unwrap();
        let data = root.path().join("data");
        let canonical = root.path().join("canonical");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&canonical).unwrap();
        set_automatic_cleanup_enabled(&data, false).unwrap();
        let store = OperationLedgerStore::new(&data);
        let terminal_id = "disabled-retention-terminal-1";
        store
            .create(
                terminal_id,
                SessionStorageOperationKind::DowngradeExport,
                &canonical,
            )
            .unwrap();
        store
            .transition(terminal_id, SessionStorageOperationPhase::Failed)
            .unwrap();
        let provider_candidate = data.join("account-a/sessions/provider-copy.jsonl");
        fs::create_dir_all(provider_candidate.parent().unwrap()).unwrap();
        fs::write(&provider_candidate, b"provider payload remains GC-owned").unwrap();

        let receipt = run_session_storage_retention(&data, None, u128::MAX).unwrap();

        assert_eq!(receipt.deleted_operation_count, 1);
        assert!(store.load(terminal_id).is_err());
        assert!(provider_candidate.is_file());
    }

    #[test]
    fn expires_a_hash_bound_failed_operation_after_seven_days() {
        let root = tempdir().unwrap();
        let data = root.path().join("data");
        let canonical = root.path().join("canonical");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&canonical).unwrap();
        let store = OperationLedgerStore::new(&data);
        let operation_id = "failed-retention-1";
        store
            .create(
                operation_id,
                SessionStorageOperationKind::DowngradeExport,
                &canonical,
            )
            .unwrap();
        let operation_root = data
            .join("session-storage-v1/operations")
            .join(operation_id);
        let plan = operation_root.join("failed-plan.json");
        fs::write(&plan, b"failed-but-terminal").unwrap();
        let (bytes, sha256) = stable_file_digest(&plan).unwrap();
        store
            .update(operation_id, |ledger| {
                ledger.created_files = vec![LedgerFileSnapshot {
                    path: plan.clone(),
                    bytes,
                    sha256,
                    created_by_operation: true,
                    logical_thread_id: None,
                }];
                Ok(())
            })
            .unwrap();
        store
            .transition(operation_id, SessionStorageOperationPhase::Failed)
            .unwrap();

        let receipt = run_session_storage_retention(&data, None, u128::MAX).unwrap();

        assert_eq!(receipt.deleted_operation_count, 1);
        assert!(!operation_root.exists());
        assert!(store.load(operation_id).is_err());
    }

    #[test]
    fn an_unfinished_operation_does_not_block_unrelated_terminal_retention() {
        let root = tempdir().unwrap();
        let data = root.path().join("data");
        let canonical = root.path().join("canonical");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&canonical).unwrap();
        let store = OperationLedgerStore::new(&data);
        store
            .create(
                "terminal-retention-1",
                SessionStorageOperationKind::DowngradeExport,
                &canonical,
            )
            .unwrap();
        store
            .transition("terminal-retention-1", SessionStorageOperationPhase::Failed)
            .unwrap();
        store
            .create(
                "unfinished-retention-1",
                SessionStorageOperationKind::Migration,
                &canonical,
            )
            .unwrap();
        store
            .transition(
                "unfinished-retention-1",
                SessionStorageOperationPhase::Preflight,
            )
            .unwrap();

        let receipt = run_session_storage_retention(&data, None, u128::MAX).unwrap();

        assert_eq!(receipt.deleted_operation_count, 1);
        assert!(receipt.blocked_artifact_count >= 1);
        assert!(store.load("terminal-retention-1").is_err());
        assert_eq!(
            store.load("unfinished-retention-1").unwrap().phase,
            SessionStorageOperationPhase::Preflight
        );
    }
}

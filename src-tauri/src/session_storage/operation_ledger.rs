use std::{
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    time::SystemTime,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{file_ops::atomic_write, operation_log::timestamp_millis};

use super::bounded_file::read_regular_file_bounded;
#[cfg(windows)]
use super::write_barrier::DestructiveFileGuard;

const LEDGER_SCHEMA_VERSION: u32 = 1;
const MAX_LEDGER_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ENCRYPTED_LEDGER_BYTES: u64 = MAX_LEDGER_BYTES * 2 + 64 * 1024;
const LEDGER_CIPHERTEXT_MAGIC: &[u8] = b"CSLEDGER1\0";
const MAX_OPERATION_ID_BYTES: usize = 160;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SessionStorageOperationKind {
    Migration,
    OfflineGc,
    ConflictResolution,
    DowngradeExport,
    RestoreImport,
    LegacyBackupReconciliation,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SessionStorageOperationPhase {
    Available,
    Preflight,
    Backup,
    BackupVerified,
    PlanReady,
    Applying,
    Validating,
    Committed,
    RollingBack,
    RolledBack,
    Failed,
}

impl SessionStorageOperationPhase {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Committed | Self::RolledBack | Self::Failed)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LedgerFileSnapshot {
    pub path: PathBuf,
    pub bytes: u64,
    pub sha256: String,
    pub created_by_operation: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logical_thread_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LedgerDatabaseSnapshot {
    pub source_path: PathBuf,
    pub snapshot_path: PathBuf,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RollbackActionKind {
    RestoreDatabase,
    RestoreFile,
    RemoveCreatedFile,
    RestoreConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LedgerRollbackStep {
    pub action: RollbackActionKind,
    pub source_path: PathBuf,
    pub target_path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_sha256: Option<String>,
    /// Hash of the target state produced by this operation. Destructive
    /// rollback must refuse to overwrite a target that no longer matches it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_sha256: Option<String>,
    pub completed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionStorageOperationLedger {
    pub schema_version: u32,
    pub revision: u64,
    pub operation_id: String,
    pub kind: SessionStorageOperationKind,
    pub phase: SessionStorageOperationPhase,
    pub started_at_ms: u128,
    pub updated_at_ms: u128,
    pub canonical_root: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_root: Option<PathBuf>,
    #[serde(default)]
    pub created_files: Vec<LedgerFileSnapshot>,
    #[serde(default)]
    pub database_snapshots: Vec<LedgerDatabaseSnapshot>,
    #[serde(default)]
    pub rollback_steps: Vec<LedgerRollbackStep>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error_code: Option<String>,
    /// Becomes durable immediately before the first live session/database
    /// mutation. Recovery may use cleanup-only abort while this remains false.
    #[serde(default)]
    pub live_mutation_started: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LedgerEnvelope {
    ledger: SessionStorageOperationLedger,
    integrity_sha256: String,
}

#[derive(Debug, Clone)]
pub struct OperationLedgerStore {
    data_root: PathBuf,
}

/// Result of the fail-closed cleanup for unpublished operation directories.
///
/// Retained candidates are intentionally distinct from blocked candidates:
/// retained means the publisher may still be alive (or the age boundary has
/// not elapsed), while blocked means ownership or immutability could not be
/// proven. Only `deleted_pending_operation_count` contributes reclaimed
/// storage.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PendingOperationCleanupReceipt {
    pub deleted_pending_operation_count: u64,
    pub retained_pending_operation_count: u64,
    pub blocked_pending_operation_count: u64,
    pub reclaimed_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingOperationName {
    operation_id: String,
    process_id: u32,
    created_at_ms: u128,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MetadataStamp {
    bytes: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    platform_identity: MetadataPlatformIdentity,
}

#[cfg(windows)]
type MetadataPlatformIdentity = (Option<(u32, u64)>, u32, u64);

#[cfg(unix)]
type MetadataPlatformIdentity = (u64, u64, u32, u64);

#[cfg(not(any(windows, unix)))]
type MetadataPlatformIdentity = ();

// The larger variant is an immutable, bounded cleanup snapshot. Keeping it
// inline avoids a second allocation in the crash-sensitive pending-ledger scan.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingOperationSnapshot {
    Empty {
        directory: MetadataStamp,
    },
    InitialLedger {
        directory: MetadataStamp,
        ledger: SessionStorageOperationLedger,
        ledger_metadata: MetadataStamp,
        ledger_bytes: u64,
        ledger_sha256: String,
    },
}

impl OperationLedgerStore {
    pub fn new(data_root: &Path) -> Self {
        Self {
            data_root: data_root.to_path_buf(),
        }
    }

    pub fn create(
        &self,
        operation_id: &str,
        kind: SessionStorageOperationKind,
        canonical_root: &Path,
    ) -> Result<SessionStorageOperationLedger, String> {
        validate_operation_id(operation_id)?;
        validate_absolute_path(canonical_root, "canonical root")?;
        let path = self.ledger_path(operation_id)?;
        if path.exists() {
            return Err("session storage operation already exists".to_string());
        }
        let operation_root = self.ensure_operation_parents(operation_id)?;
        let now = timestamp_millis()?;
        let ledger = SessionStorageOperationLedger {
            schema_version: LEDGER_SCHEMA_VERSION,
            revision: 0,
            operation_id: operation_id.to_string(),
            kind,
            phase: SessionStorageOperationPhase::Available,
            started_at_ms: now,
            updated_at_ms: now,
            canonical_root: canonical_root.to_path_buf(),
            backup_root: None,
            created_files: Vec::new(),
            database_snapshots: Vec::new(),
            rollback_steps: Vec::new(),
            last_error_code: None,
            live_mutation_started: false,
        };
        validate_ledger(&ledger)?;
        let pending_root = operation_root.with_file_name(format!(
            ".pending-{operation_id}-{}-{now}",
            std::process::id()
        ));
        fs::create_dir(&pending_root)
            .map_err(|_| "failed to create pending session storage operation".to_string())?;
        let pending_path = pending_root.join("ledger.json");
        let publish = (|| {
            write_ledger(&pending_path, &ledger)?;
            fs::rename(&pending_root, &operation_root)
                .map_err(|_| "failed to publish session storage operation".to_string())
        })();
        if publish.is_err() {
            let _ = fs::remove_file(&pending_path);
            let _ = fs::remove_dir(&pending_root);
        }
        publish?;
        self.load(operation_id)
    }

    pub fn load(&self, operation_id: &str) -> Result<SessionStorageOperationLedger, String> {
        let path = self.ledger_path(operation_id)?;
        read_ledger(&path)
    }

    pub fn try_load(
        &self,
        operation_id: &str,
    ) -> Result<Option<SessionStorageOperationLedger>, String> {
        let path = self.ledger_path(operation_id)?;
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if !metadata.is_file() || metadata_is_link_or_reparse(&metadata) {
                    return Err("session storage ledger path is unsafe".to_string());
                }
                read_ledger(&path).map(Some)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err("session storage ledger is unavailable".to_string()),
        }
    }

    pub fn update<F>(
        &self,
        operation_id: &str,
        mutate: F,
    ) -> Result<SessionStorageOperationLedger, String>
    where
        F: FnOnce(&mut SessionStorageOperationLedger) -> Result<(), String>,
    {
        let path = self.ledger_path(operation_id)?;
        let current = read_ledger(&path)?;
        if current.phase.is_terminal() {
            return Err("terminal session storage operation cannot be changed".to_string());
        }
        let mut next = current.clone();
        mutate(&mut next)?;
        if next.operation_id != current.operation_id
            || next.kind != current.kind
            || next.canonical_root != current.canonical_root
            || next.started_at_ms != current.started_at_ms
            || next.schema_version != current.schema_version
            || next.revision != current.revision
        {
            return Err("immutable session storage ledger identity changed".to_string());
        }
        validate_transition(current.phase, next.phase)?;
        next.revision = next
            .revision
            .checked_add(1)
            .ok_or_else(|| "session storage ledger revision overflowed".to_string())?;
        next.updated_at_ms = timestamp_millis()?.max(current.updated_at_ms);
        validate_ledger(&next)?;

        let persisted = read_ledger(&path)?;
        if persisted != current {
            return Err("session storage ledger changed concurrently".to_string());
        }
        write_ledger(&path, &next)?;
        let verified = read_ledger(&path)?;
        if verified != next {
            return Err("session storage ledger verification failed".to_string());
        }
        Ok(verified)
    }

    pub fn transition(
        &self,
        operation_id: &str,
        phase: SessionStorageOperationPhase,
    ) -> Result<SessionStorageOperationLedger, String> {
        self.update(operation_id, |ledger| {
            ledger.phase = phase;
            Ok(())
        })
    }

    pub fn unfinished(&self) -> Result<Vec<SessionStorageOperationLedger>, String> {
        Ok(self
            .all()?
            .into_iter()
            .filter(|ledger| !ledger.phase.is_terminal())
            .collect())
    }

    pub fn all(&self) -> Result<Vec<SessionStorageOperationLedger>, String> {
        let operations_root = self.operations_root();
        let metadata = match fs::symlink_metadata(&operations_root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(_) => return Err("session storage operation inventory is unavailable".to_string()),
        };
        if !metadata.is_dir() || metadata_is_link_or_reparse(&metadata) {
            return Err("session storage operation inventory is unsafe".to_string());
        }
        let mut ledgers = Vec::new();
        for entry in fs::read_dir(&operations_root)
            .map_err(|_| "session storage operation inventory is unreadable".to_string())?
        {
            let entry = entry
                .map_err(|_| "session storage operation inventory is unreadable".to_string())?;
            let name = entry.file_name();
            let Some(operation_id) = name.to_str() else {
                return Err(
                    "session storage operation inventory contains an invalid entry".to_string(),
                );
            };
            if operation_id.starts_with(".pending-") {
                continue;
            }
            validate_operation_id(operation_id)?;
            let metadata = fs::symlink_metadata(entry.path())
                .map_err(|_| "session storage operation entry is unreadable".to_string())?;
            if !metadata.is_dir() || metadata_is_link_or_reparse(&metadata) {
                return Err("session storage operation entry is unsafe".to_string());
            }
            let ledger_path = entry.path().join("ledger.json");
            if !ledger_path.exists() {
                if fs::read_dir(entry.path())
                    .map_err(|_| "session storage operation entry is unreadable".to_string())?
                    .next()
                    .is_none()
                {
                    continue;
                }
                return Err("session storage operation entry has no ledger".to_string());
            }
            let ledger = self.load(operation_id)?;
            ledgers.push(ledger);
        }
        ledgers.sort_by(|left, right| {
            left.started_at_ms
                .cmp(&right.started_at_ms)
                .then_with(|| left.operation_id.cmp(&right.operation_id))
        });
        Ok(ledgers)
    }

    pub fn remove_terminal_operation(&self, operation_id: &str) -> Result<u64, String> {
        let ledger = self.load(operation_id)?;
        if !ledger.phase.is_terminal() {
            return Err("only completed session storage operations can expire".to_string());
        }
        let root = self.operations_root().join(operation_id);
        if root.parent() != Some(self.operations_root().as_path()) || !root.is_absolute() {
            return Err("session storage operation cleanup root is invalid".to_string());
        }
        let ledger_path = root.join("ledger.json");
        let mut files = Vec::new();
        let mut directories = Vec::new();
        collect_operation_tree(&root, &mut files, &mut directories)?;
        let mut reclaimed_bytes = 0_u64;
        for path in &files {
            let metadata = fs::symlink_metadata(path)
                .map_err(|_| "session storage operation cleanup file is unavailable".to_string())?;
            reclaimed_bytes = reclaimed_bytes
                .checked_add(metadata.len())
                .ok_or_else(|| "session storage operation cleanup size overflowed".to_string())?;
            if path == &ledger_path {
                continue;
            }
            let snapshot = ledger
                .created_files
                .iter()
                .find(|snapshot| snapshot.created_by_operation && snapshot.path == *path)
                .ok_or_else(|| {
                    "session storage operation contains an undeclared file".to_string()
                })?;
            let (bytes, sha256) = stable_regular_file_digest(path)?;
            if bytes != snapshot.bytes || sha256 != snapshot.sha256 {
                return Err("session storage operation file changed before expiry".to_string());
            }
        }
        if !files.iter().any(|path| path == &ledger_path) {
            return Err("session storage operation ledger disappeared".to_string());
        }
        for directory in &directories {
            if directory == &root {
                continue;
            }
            if !files.iter().any(|path| path.starts_with(directory)) {
                return Err(
                    "session storage operation contains an undeclared directory".to_string()
                );
            }
        }
        directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
        for path in files {
            fs::remove_file(path).map_err(|_| {
                "failed to remove expired session storage operation file".to_string()
            })?;
        }
        for path in directories {
            fs::remove_dir(path).map_err(|_| {
                "failed to remove expired session storage operation directory".to_string()
            })?;
        }
        Ok(reclaimed_bytes)
    }

    /// Compatibility wrapper retained for the existing retention caller.
    /// New callers should use `cleanup_abandoned_pending_operations` so
    /// retained and blocked artifacts remain observable.
    pub fn remove_abandoned_pending_operations(&self, cutoff_ms: u128) -> Result<u64, String> {
        Ok(self
            .cleanup_abandoned_pending_operations(cutoff_ms)?
            .reclaimed_bytes)
    }

    pub fn cleanup_abandoned_pending_operations(
        &self,
        cutoff_ms: u128,
    ) -> Result<PendingOperationCleanupReceipt, String> {
        let root = self.operations_root();
        let metadata = match fs::symlink_metadata(&root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(PendingOperationCleanupReceipt::default());
            }
            Err(_) => return Err("pending operation inventory is unavailable".to_string()),
        };
        if !metadata.is_dir() || metadata_is_link_or_reparse(&metadata) {
            return Err("pending operation inventory is unsafe".to_string());
        }
        validate_directory_ancestry(&root)
            .map_err(|_| "pending operation inventory ancestry is unsafe".to_string())?;

        let mut receipt = PendingOperationCleanupReceipt::default();
        for entry in fs::read_dir(&root)
            .map_err(|_| "pending operation inventory is unreadable".to_string())?
        {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    receipt.blocked_pending_operation_count =
                        receipt.blocked_pending_operation_count.saturating_add(1);
                    continue;
                }
            };
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if !name.starts_with(".pending-") {
                continue;
            };

            let Some(identity) = parse_pending_operation_name(&name) else {
                receipt.blocked_pending_operation_count =
                    receipt.blocked_pending_operation_count.saturating_add(1);
                continue;
            };
            let directory = entry.path();
            if directory.parent() != Some(root.as_path())
                || validate_directory_ancestry(&directory).is_err()
            {
                receipt.blocked_pending_operation_count =
                    receipt.blocked_pending_operation_count.saturating_add(1);
                continue;
            }

            if identity.created_at_ms >= cutoff_ms || process_is_running(identity.process_id) {
                receipt.retained_pending_operation_count =
                    receipt.retained_pending_operation_count.saturating_add(1);
                continue;
            }

            let snapshot = match inspect_pending_operation(&directory, &identity) {
                Ok(snapshot) => snapshot,
                Err(()) => {
                    receipt.blocked_pending_operation_count =
                        receipt.blocked_pending_operation_count.saturating_add(1);
                    continue;
                }
            };
            match delete_pending_operation(&directory, &identity, &snapshot) {
                PendingDeleteResult::Deleted { reclaimed_bytes } => {
                    receipt.deleted_pending_operation_count =
                        receipt.deleted_pending_operation_count.saturating_add(1);
                    receipt.reclaimed_bytes = receipt
                        .reclaimed_bytes
                        .checked_add(reclaimed_bytes)
                        .ok_or_else(|| "pending operation cleanup size overflowed".to_string())?;
                }
                PendingDeleteResult::Blocked { reclaimed_bytes } => {
                    receipt.blocked_pending_operation_count =
                        receipt.blocked_pending_operation_count.saturating_add(1);
                    receipt.reclaimed_bytes = receipt
                        .reclaimed_bytes
                        .checked_add(reclaimed_bytes)
                        .ok_or_else(|| "pending operation cleanup size overflowed".to_string())?;
                }
            }
        }
        Ok(receipt)
    }

    fn operations_root(&self) -> PathBuf {
        self.data_root.join("session-storage-v1/operations")
    }

    fn ledger_path(&self, operation_id: &str) -> Result<PathBuf, String> {
        validate_operation_id(operation_id)?;
        Ok(self
            .operations_root()
            .join(operation_id)
            .join("ledger.json"))
    }

    fn ensure_operation_parents(&self, operation_id: &str) -> Result<PathBuf, String> {
        validate_operation_id(operation_id)?;
        let storage_root = self.data_root.join("session-storage-v1");
        let operations_root = self.operations_root();
        let operation_root = operations_root.join(operation_id);
        for root in [&storage_root, &operations_root] {
            fs::create_dir_all(root)
                .map_err(|_| "failed to create session storage operation directory".to_string())?;
            let metadata = fs::symlink_metadata(root)
                .map_err(|_| "failed to inspect session storage operation directory".to_string())?;
            if !metadata.is_dir() || metadata_is_link_or_reparse(&metadata) {
                return Err("session storage operation directory is unsafe".to_string());
            }
        }
        if operation_root.exists() {
            return Err("session storage operation already exists".to_string());
        }
        Ok(operation_root)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingDeleteResult {
    Deleted {
        reclaimed_bytes: u64,
    },
    /// A file can be removed before a concurrent directory change is noticed.
    /// Report those bytes without claiming the candidate itself was deleted.
    Blocked {
        reclaimed_bytes: u64,
    },
}

fn parse_pending_operation_name(name: &str) -> Option<PendingOperationName> {
    let rest = name.strip_prefix(".pending-")?;
    let (rest, created_at) = rest.rsplit_once('-')?;
    let (operation_id, process_id_text) = rest.rsplit_once('-')?;
    validate_operation_id(operation_id).ok()?;
    let process_id = process_id_text.parse::<u32>().ok()?;
    if process_id == 0 || process_id.to_string() != process_id_text {
        return None;
    }
    let created_at_ms = created_at.parse::<u128>().ok()?;
    if created_at_ms == 0 || created_at_ms.to_string() != created_at {
        return None;
    }
    Some(PendingOperationName {
        operation_id: operation_id.to_string(),
        process_id,
        created_at_ms,
    })
}

fn inspect_pending_operation(
    directory: &Path,
    identity: &PendingOperationName,
) -> Result<PendingOperationSnapshot, ()> {
    let directory_metadata = fs::symlink_metadata(directory).map_err(|_| ())?;
    if !directory_metadata.is_dir() || metadata_is_link_or_reparse(&directory_metadata) {
        return Err(());
    }
    let directory_stamp = metadata_stamp(directory, &directory_metadata);
    if !metadata_stamp_has_identity(&directory_stamp) {
        return Err(());
    }
    let mut children = fs::read_dir(directory)
        .map_err(|_| ())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ())?;
    match children.len() {
        0 => Ok(PendingOperationSnapshot::Empty {
            directory: directory_stamp,
        }),
        1 => {
            let child = children.pop().ok_or(())?;
            if child.file_name() != "ledger.json" || child.path().parent() != Some(directory) {
                return Err(());
            }
            let ledger_path = child.path();
            let ledger_metadata = fs::symlink_metadata(&ledger_path).map_err(|_| ())?;
            if !ledger_metadata.is_file() || metadata_is_link_or_reparse(&ledger_metadata) {
                return Err(());
            }
            let ledger_stamp = metadata_stamp(&ledger_path, &ledger_metadata);
            if !metadata_stamp_has_identity(&ledger_stamp) {
                return Err(());
            }
            if !pending_ledger_has_expected_storage_format(&ledger_path) {
                return Err(());
            }
            let ledger = read_ledger(&ledger_path).map_err(|_| ())?;
            if !is_initial_pending_ledger(&ledger, identity) {
                return Err(());
            }
            let (ledger_bytes, ledger_sha256) =
                stable_regular_file_digest(&ledger_path).map_err(|_| ())?;
            if metadata_stamp(
                &ledger_path,
                &fs::symlink_metadata(&ledger_path).map_err(|_| ())?,
            ) != ledger_stamp
            {
                return Err(());
            }
            Ok(PendingOperationSnapshot::InitialLedger {
                directory: directory_stamp,
                ledger,
                ledger_metadata: ledger_stamp,
                ledger_bytes,
                ledger_sha256,
            })
        }
        _ => Err(()),
    }
}

fn is_initial_pending_ledger(
    ledger: &SessionStorageOperationLedger,
    identity: &PendingOperationName,
) -> bool {
    ledger.operation_id == identity.operation_id
        && ledger.started_at_ms == identity.created_at_ms
        && ledger.updated_at_ms == identity.created_at_ms
        && ledger.revision == 0
        && ledger.phase == SessionStorageOperationPhase::Available
        && !ledger.live_mutation_started
        && ledger.backup_root.is_none()
        && ledger.created_files.is_empty()
        && ledger.database_snapshots.is_empty()
        && ledger.rollback_steps.is_empty()
        && ledger.last_error_code.is_none()
}

fn delete_pending_operation(
    directory: &Path,
    identity: &PendingOperationName,
    snapshot: &PendingOperationSnapshot,
) -> PendingDeleteResult {
    if directory
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(parse_pending_operation_name)
        .as_ref()
        != Some(identity)
        || validate_directory_ancestry(directory).is_err()
    {
        return PendingDeleteResult::Blocked { reclaimed_bytes: 0 };
    }

    let expected_directory = match snapshot {
        PendingOperationSnapshot::Empty { directory }
        | PendingOperationSnapshot::InitialLedger { directory, .. } => directory,
    };
    let Ok(current_directory) = fs::symlink_metadata(directory) else {
        return PendingDeleteResult::Blocked { reclaimed_bytes: 0 };
    };
    if !current_directory.is_dir()
        || metadata_is_link_or_reparse(&current_directory)
        || metadata_stamp(directory, &current_directory) != *expected_directory
    {
        return PendingDeleteResult::Blocked { reclaimed_bytes: 0 };
    }

    let mut reclaimed_bytes = 0_u64;
    match snapshot {
        PendingOperationSnapshot::Empty { .. } => {
            let Ok(mut children) = fs::read_dir(directory) else {
                return PendingDeleteResult::Blocked { reclaimed_bytes };
            };
            if children.next().is_some() {
                return PendingDeleteResult::Blocked { reclaimed_bytes };
            }
        }
        PendingOperationSnapshot::InitialLedger {
            ledger,
            ledger_metadata,
            ledger_bytes,
            ledger_sha256,
            ..
        } => {
            let ledger_path = directory.join("ledger.json");
            let Ok(mut children) = fs::read_dir(directory) else {
                return PendingDeleteResult::Blocked { reclaimed_bytes };
            };
            let Some(Ok(child)) = children.next() else {
                return PendingDeleteResult::Blocked { reclaimed_bytes };
            };
            if children.next().is_some()
                || child.file_name() != "ledger.json"
                || child.path() != ledger_path
            {
                return PendingDeleteResult::Blocked { reclaimed_bytes };
            }
            let Ok(current_metadata) = fs::symlink_metadata(&ledger_path) else {
                return PendingDeleteResult::Blocked { reclaimed_bytes };
            };
            if metadata_stamp(&ledger_path, &current_metadata) != *ledger_metadata
                || !pending_ledger_has_expected_storage_format(&ledger_path)
                || read_ledger(&ledger_path).ok().as_ref() != Some(ledger)
                || stable_regular_file_digest(&ledger_path).ok().as_ref()
                    != Some(&(*ledger_bytes, ledger_sha256.clone()))
                || fs::symlink_metadata(&ledger_path)
                    .ok()
                    .as_ref()
                    .map(|metadata| metadata_stamp(&ledger_path, metadata))
                    .as_ref()
                    != Some(ledger_metadata)
            {
                return PendingDeleteResult::Blocked { reclaimed_bytes };
            }
            if delete_verified_pending_ledger(&ledger_path, ledger_sha256).is_err() {
                return PendingDeleteResult::Blocked { reclaimed_bytes };
            }
            reclaimed_bytes = *ledger_bytes;
        }
    }

    match fs::remove_dir(directory) {
        Ok(()) => PendingDeleteResult::Deleted { reclaimed_bytes },
        Err(_) => PendingDeleteResult::Blocked { reclaimed_bytes },
    }
}

#[cfg(windows)]
fn delete_verified_pending_ledger(path: &Path, expected_sha256: &str) -> Result<(), ()> {
    let mut guard = DestructiveFileGuard::acquire(path).map_err(|_| ())?;
    guard
        .verify_current_path(Some(expected_sha256))
        .map_err(|_| ())?;
    guard.delete().map_err(|_| ())
}

#[cfg(not(windows))]
fn delete_verified_pending_ledger(path: &Path, expected_sha256: &str) -> Result<(), ()> {
    if stable_regular_file_digest(path)
        .ok()
        .as_ref()
        .is_none_or(|(_, digest)| digest != expected_sha256)
    {
        return Err(());
    }
    fs::remove_file(path).map_err(|_| ())
}

// The explicit cfg return makes each platform implementation self-contained.
#[allow(clippy::needless_return)]
fn pending_ledger_has_expected_storage_format(path: &Path) -> bool {
    #[cfg(windows)]
    {
        return read_regular_file_bounded(path, MAX_ENCRYPTED_LEDGER_BYTES).is_ok_and(|bytes| {
            bytes.starts_with(LEDGER_CIPHERTEXT_MAGIC)
                && bytes.len() > LEDGER_CIPHERTEXT_MAGIC.len()
        });
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        true
    }
}

fn validate_directory_ancestry(path: &Path) -> Result<(), ()> {
    if !path.is_absolute() {
        return Err(());
    }
    for ancestor in path.ancestors() {
        let metadata = fs::symlink_metadata(ancestor).map_err(|_| ())?;
        if !metadata.is_dir() || metadata_is_link_or_reparse(&metadata) {
            return Err(());
        }
    }
    Ok(())
}

fn metadata_stamp(path: &Path, metadata: &fs::Metadata) -> MetadataStamp {
    MetadataStamp {
        bytes: metadata.len(),
        modified: metadata.modified().ok(),
        created: metadata.created().ok(),
        platform_identity: metadata_platform_identity(path, metadata),
    }
}

#[cfg(windows)]
fn metadata_stamp_has_identity(stamp: &MetadataStamp) -> bool {
    stamp.platform_identity.0.is_some()
}

#[cfg(not(windows))]
fn metadata_stamp_has_identity(_stamp: &MetadataStamp) -> bool {
    true
}

#[cfg(windows)]
fn metadata_platform_identity(path: &Path, metadata: &fs::Metadata) -> MetadataPlatformIdentity {
    use std::os::windows::fs::MetadataExt;
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS,
            FILE_FLAG_OPEN_REPARSE_POINT,
        },
    };

    let identity = fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .ok()
        .and_then(|file| {
            let mut information = BY_HANDLE_FILE_INFORMATION::default();
            let result = unsafe {
                GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut information)
            };
            (result != 0).then_some((
                information.dwVolumeSerialNumber,
                (u64::from(information.nFileIndexHigh) << 32)
                    | u64::from(information.nFileIndexLow),
            ))
        });
    (
        identity,
        metadata.file_attributes(),
        metadata.last_write_time(),
    )
}

#[cfg(unix)]
fn metadata_platform_identity(_path: &Path, metadata: &fs::Metadata) -> MetadataPlatformIdentity {
    use std::os::unix::fs::MetadataExt;
    (
        metadata.dev(),
        metadata.ino(),
        metadata.mode(),
        metadata.ctime() as u64,
    )
}

#[cfg(not(any(windows, unix)))]
fn metadata_platform_identity(_path: &Path, _metadata: &fs::Metadata) -> MetadataPlatformIdentity {}

#[cfg(windows)]
fn process_is_running(process_id: u32) -> bool {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, GetLastError, ERROR_INVALID_PARAMETER, WAIT_TIMEOUT},
        System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
    };

    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, process_id) };
    if handle.is_null() {
        return unsafe { GetLastError() } != ERROR_INVALID_PARAMETER;
    }
    let running = unsafe { WaitForSingleObject(handle, 0) } == WAIT_TIMEOUT;
    unsafe {
        CloseHandle(handle);
    }
    running
}

#[cfg(not(windows))]
fn process_is_running(process_id: u32) -> bool {
    process_id == std::process::id()
}

fn collect_operation_tree(
    root: &Path,
    files: &mut Vec<PathBuf>,
    directories: &mut Vec<PathBuf>,
) -> Result<(), String> {
    let metadata = fs::symlink_metadata(root)
        .map_err(|_| "session storage operation cleanup tree is unavailable".to_string())?;
    if !metadata.is_dir() || metadata_is_link_or_reparse(&metadata) {
        return Err("session storage operation cleanup tree is unsafe".to_string());
    }
    directories.push(root.to_path_buf());
    for entry in fs::read_dir(root)
        .map_err(|_| "session storage operation cleanup tree is unreadable".to_string())?
    {
        let entry = entry
            .map_err(|_| "session storage operation cleanup entry is unreadable".to_string())?;
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|_| "session storage operation cleanup entry is unreadable".to_string())?;
        if metadata_is_link_or_reparse(&metadata) {
            return Err("session storage operation cleanup tree contains a link".to_string());
        }
        if metadata.is_dir() {
            collect_operation_tree(&entry.path(), files, directories)?;
        } else if metadata.is_file() {
            files.push(entry.path());
        } else {
            return Err(
                "session storage operation cleanup tree contains an unsupported entry".to_string(),
            );
        }
    }
    Ok(())
}

fn stable_regular_file_digest(path: &Path) -> Result<(u64, String), String> {
    let before = fs::symlink_metadata(path)
        .map_err(|_| "session storage operation file is unavailable".to_string())?;
    if !before.is_file() || metadata_is_link_or_reparse(&before) {
        return Err("session storage operation file is unsafe".to_string());
    }
    let mut file =
        File::open(path).map_err(|_| "session storage operation file is unreadable".to_string())?;
    let mut hasher = Sha256::new();
    let mut bytes = 0_u64;
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|_| "session storage operation file is unreadable".to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        bytes = bytes
            .checked_add(read as u64)
            .ok_or_else(|| "session storage operation file size overflowed".to_string())?;
    }
    let after = fs::symlink_metadata(path)
        .map_err(|_| "session storage operation file is unavailable".to_string())?;
    if before.len() != after.len() || bytes != before.len() {
        return Err("session storage operation file changed while hashing".to_string());
    }
    Ok((bytes, format!("{:x}", hasher.finalize())))
}

fn validate_transition(
    current: SessionStorageOperationPhase,
    next: SessionStorageOperationPhase,
) -> Result<(), String> {
    if current == next {
        return Ok(());
    }
    use SessionStorageOperationPhase as Phase;
    let valid = match current {
        Phase::Available => matches!(next, Phase::Preflight | Phase::RollingBack | Phase::Failed),
        Phase::Preflight => matches!(next, Phase::Backup | Phase::RollingBack | Phase::Failed),
        Phase::Backup => matches!(
            next,
            Phase::BackupVerified | Phase::RollingBack | Phase::Failed
        ),
        Phase::BackupVerified => {
            matches!(next, Phase::PlanReady | Phase::RollingBack | Phase::Failed)
        }
        Phase::PlanReady => matches!(next, Phase::Applying | Phase::RollingBack | Phase::Failed),
        Phase::Applying => matches!(next, Phase::Validating | Phase::RollingBack),
        Phase::Validating => matches!(next, Phase::Committed | Phase::RollingBack),
        Phase::RollingBack => matches!(next, Phase::RolledBack | Phase::Failed),
        Phase::Committed | Phase::RolledBack | Phase::Failed => false,
    };
    if valid {
        Ok(())
    } else {
        Err("invalid session storage operation phase transition".to_string())
    }
}

fn validate_ledger(ledger: &SessionStorageOperationLedger) -> Result<(), String> {
    if ledger.schema_version != LEDGER_SCHEMA_VERSION {
        return Err("session storage ledger version is unsupported".to_string());
    }
    validate_operation_id(&ledger.operation_id)?;
    validate_absolute_path(&ledger.canonical_root, "canonical root")?;
    if ledger.updated_at_ms < ledger.started_at_ms {
        return Err("session storage ledger timestamps are invalid".to_string());
    }
    if let Some(path) = &ledger.backup_root {
        validate_absolute_path(path, "backup root")?;
    }
    for file in &ledger.created_files {
        validate_absolute_path(&file.path, "ledger file")?;
        validate_sha256(&file.sha256)?;
        if file
            .logical_thread_id
            .as_ref()
            .is_some_and(|thread_id| thread_id.trim().is_empty() || thread_id.len() > 256)
        {
            return Err("session storage ledger thread identity is invalid".to_string());
        }
    }
    for database in &ledger.database_snapshots {
        validate_absolute_path(&database.source_path, "database source")?;
        validate_absolute_path(&database.snapshot_path, "database snapshot")?;
        validate_sha256(&database.sha256)?;
    }
    for step in &ledger.rollback_steps {
        validate_absolute_path(&step.source_path, "rollback source")?;
        validate_absolute_path(&step.target_path, "rollback target")?;
        if let Some(digest) = &step.expected_sha256 {
            validate_sha256(digest)?;
        }
        if let Some(digest) = &step.applied_sha256 {
            validate_sha256(digest)?;
        }
    }
    if ledger
        .last_error_code
        .as_ref()
        .is_some_and(|code| !is_safe_identifier(code, 128))
    {
        return Err("session storage ledger error code is invalid".to_string());
    }
    Ok(())
}

fn read_ledger(path: &Path) -> Result<SessionStorageOperationLedger, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| "session storage ledger is unavailable".to_string())?;
    if !metadata.is_file()
        || metadata_is_link_or_reparse(&metadata)
        || metadata.len() == 0
        || metadata.len() > MAX_ENCRYPTED_LEDGER_BYTES
    {
        return Err("session storage ledger is invalid".to_string());
    }
    let bytes = read_regular_file_bounded(path, MAX_ENCRYPTED_LEDGER_BYTES)
        .map_err(|_| "session storage ledger is unreadable".to_string())?;
    let bytes = if let Some(ciphertext) = bytes.strip_prefix(LEDGER_CIPHERTEXT_MAGIC) {
        crate::crypto::unprotect(ciphertext)
            .map_err(|_| "session storage ledger is unreadable".to_string())?
    } else {
        bytes
    };
    if bytes.len() as u64 > MAX_LEDGER_BYTES {
        return Err("session storage ledger is invalid".to_string());
    }
    let envelope = serde_json::from_slice::<LedgerEnvelope>(&bytes)
        .map_err(|_| "session storage ledger is invalid".to_string())?;
    validate_ledger(&envelope.ledger)?;
    let expected = ledger_digest(&envelope.ledger)?;
    if envelope.integrity_sha256 != expected {
        return Err("session storage ledger integrity check failed".to_string());
    }
    Ok(envelope.ledger)
}

fn write_ledger(path: &Path, ledger: &SessionStorageOperationLedger) -> Result<(), String> {
    validate_ledger(ledger)?;
    let envelope = LedgerEnvelope {
        ledger: ledger.clone(),
        integrity_sha256: ledger_digest(ledger)?,
    };
    let bytes = serde_json::to_vec_pretty(&envelope)
        .map_err(|_| "failed to serialize session storage ledger".to_string())?;
    if bytes.len() as u64 > MAX_LEDGER_BYTES {
        return Err("session storage ledger reached its size limit".to_string());
    }
    #[cfg(windows)]
    let bytes = {
        let ciphertext = crate::crypto::protect(&bytes)
            .map_err(|_| "failed to protect session storage ledger".to_string())?;
        let mut protected = Vec::with_capacity(LEDGER_CIPHERTEXT_MAGIC.len() + ciphertext.len());
        protected.extend_from_slice(LEDGER_CIPHERTEXT_MAGIC);
        protected.extend_from_slice(&ciphertext);
        protected
    };
    atomic_write(path, &bytes)
}

fn ledger_digest(ledger: &SessionStorageOperationLedger) -> Result<String, String> {
    let bytes = serde_json::to_vec(ledger)
        .map_err(|_| "failed to serialize session storage ledger".to_string())?;
    Ok(hex_digest(Sha256::digest(bytes)))
}

fn validate_operation_id(value: &str) -> Result<(), String> {
    if is_safe_identifier(value, MAX_OPERATION_ID_BYTES) {
        Ok(())
    } else {
        Err("session storage operation ID is invalid".to_string())
    }
}

fn is_safe_identifier(value: &str, max_bytes: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_bytes
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

fn validate_absolute_path(path: &Path, label: &str) -> Result<(), String> {
    if path.is_absolute() && !path.as_os_str().is_empty() {
        Ok(())
    } else {
        Err(format!("session storage {label} path is invalid"))
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
        Err("session storage ledger checksum is invalid".to_string())
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
    use std::fs;

    use tempfile::tempdir;

    use super::{
        write_ledger, LedgerFileSnapshot, OperationLedgerStore, SessionStorageOperationKind,
        SessionStorageOperationLedger, SessionStorageOperationPhase,
    };

    fn initial_pending_ledger(
        root: &std::path::Path,
        operation_id: &str,
        created_at_ms: u128,
    ) -> SessionStorageOperationLedger {
        SessionStorageOperationLedger {
            schema_version: 1,
            revision: 0,
            operation_id: operation_id.to_string(),
            kind: SessionStorageOperationKind::Migration,
            phase: SessionStorageOperationPhase::Available,
            started_at_ms: created_at_ms,
            updated_at_ms: created_at_ms,
            canonical_root: root.join("canonical"),
            backup_root: None,
            created_files: Vec::new(),
            database_snapshots: Vec::new(),
            rollback_steps: Vec::new(),
            last_error_code: None,
            live_mutation_started: false,
        }
    }

    fn pending_directory(
        root: &std::path::Path,
        operation_id: &str,
        process_id: u32,
        created_at_ms: u128,
    ) -> std::path::PathBuf {
        root.join("session-storage-v1/operations").join(format!(
            ".pending-{operation_id}-{process_id}-{created_at_ms}"
        ))
    }

    #[test]
    fn persists_verified_revisions_and_lists_unfinished_operations() {
        let root = tempdir().unwrap();
        let canonical = root.path().join("canonical");
        let store = OperationLedgerStore::new(root.path());
        assert!(store.try_load("session-migration-1").unwrap().is_none());
        let created = store
            .create(
                "session-migration-1",
                SessionStorageOperationKind::Migration,
                &canonical,
            )
            .unwrap();
        assert_eq!(created.revision, 0);
        assert_eq!(created.phase, SessionStorageOperationPhase::Available);
        assert_eq!(
            store.try_load("session-migration-1").unwrap(),
            Some(created.clone())
        );

        let preflight = store
            .transition(
                "session-migration-1",
                SessionStorageOperationPhase::Preflight,
            )
            .unwrap();
        assert_eq!(preflight.revision, 1);
        let updated = store
            .update("session-migration-1", |ledger| {
                ledger.created_files.push(LedgerFileSnapshot {
                    path: canonical.join("session.jsonl"),
                    bytes: 10,
                    sha256: "a".repeat(64),
                    created_by_operation: true,
                    logical_thread_id: Some("thread-a".to_string()),
                });
                Ok(())
            })
            .unwrap();
        assert_eq!(updated.revision, 2);
        assert_eq!(store.unfinished().unwrap(), vec![updated]);
    }

    #[test]
    fn rejects_invalid_transitions_and_terminal_mutation() {
        let root = tempdir().unwrap();
        let store = OperationLedgerStore::new(root.path());
        store
            .create(
                "session-migration-1",
                SessionStorageOperationKind::Migration,
                &root.path().join("canonical"),
            )
            .unwrap();
        assert!(store
            .transition(
                "session-migration-1",
                SessionStorageOperationPhase::Applying,
            )
            .unwrap_err()
            .contains("phase transition"));
        store
            .transition("session-migration-1", SessionStorageOperationPhase::Failed)
            .unwrap();
        assert!(store
            .update("session-migration-1", |_| Ok(()))
            .unwrap_err()
            .contains("terminal"));
        assert!(store.unfinished().unwrap().is_empty());
    }

    #[test]
    fn precommit_operation_can_cancel_through_rollback() {
        let root = tempdir().unwrap();
        let store = OperationLedgerStore::new(root.path());
        store
            .create(
                "session-migration-1",
                SessionStorageOperationKind::Migration,
                &root.path().join("canonical"),
            )
            .unwrap();
        store
            .transition(
                "session-migration-1",
                SessionStorageOperationPhase::Preflight,
            )
            .unwrap();
        store
            .transition(
                "session-migration-1",
                SessionStorageOperationPhase::RollingBack,
            )
            .unwrap();
        store
            .transition(
                "session-migration-1",
                SessionStorageOperationPhase::RolledBack,
            )
            .unwrap();
        assert!(store.unfinished().unwrap().is_empty());
    }

    #[test]
    fn detects_truncation_tampering_and_unsafe_operation_ids() {
        let root = tempdir().unwrap();
        let store = OperationLedgerStore::new(root.path());
        store
            .create(
                "session-migration-1",
                SessionStorageOperationKind::Migration,
                &root.path().join("canonical"),
            )
            .unwrap();
        let path = root
            .path()
            .join("session-storage-v1/operations/session-migration-1/ledger.json");
        let mut bytes = fs::read(&path).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("canonical"));
        let last = bytes.last_mut().unwrap();
        *last ^= 0x5a;
        fs::write(&path, bytes).unwrap();
        let error = store.load("session-migration-1").unwrap_err();
        assert!(
            error.contains("integrity") || error.contains("unreadable"),
            "{error}"
        );
        assert!(store
            .create(
                "../escape",
                SessionStorageOperationKind::Migration,
                &root.path().join("canonical"),
            )
            .unwrap_err()
            .contains("ID"));
    }

    #[test]
    fn empty_legacy_orphans_and_unpublished_pending_directories_do_not_block_inventory() {
        let root = tempdir().unwrap();
        let operations = root.path().join("session-storage-v1/operations");
        fs::create_dir_all(operations.join("orphan-before-ledger")).unwrap();
        fs::create_dir_all(operations.join(".pending-interrupted-create")).unwrap();
        fs::write(
            operations
                .join(".pending-interrupted-create")
                .join("ledger.json"),
            b"unpublished",
        )
        .unwrap();
        let store = OperationLedgerStore::new(root.path());
        assert!(store.all().unwrap().is_empty());
        store
            .create(
                "session-migration-1",
                SessionStorageOperationKind::Migration,
                &root.path().join("canonical"),
            )
            .unwrap();
        assert_eq!(store.all().unwrap().len(), 1);
    }

    #[test]
    fn pending_cleanup_retains_live_publishers_and_honors_the_age_boundary() {
        let root = tempdir().unwrap();
        let operations = root.path().join("session-storage-v1/operations");
        fs::create_dir_all(&operations).unwrap();
        let dead_pid = u32::MAX;
        let boundary = pending_directory(root.path(), "boundary", dead_pid, 100);
        let live = pending_directory(root.path(), "live", std::process::id(), 1);
        fs::create_dir(&boundary).unwrap();
        fs::create_dir(&live).unwrap();

        let store = OperationLedgerStore::new(root.path());
        let first = store.cleanup_abandoned_pending_operations(100).unwrap();
        assert_eq!(first.deleted_pending_operation_count, 0);
        assert_eq!(first.retained_pending_operation_count, 2);
        assert_eq!(first.blocked_pending_operation_count, 0);
        assert!(boundary.exists());
        assert!(live.exists());

        let second = store.cleanup_abandoned_pending_operations(101).unwrap();
        assert_eq!(second.deleted_pending_operation_count, 1);
        assert_eq!(second.retained_pending_operation_count, 1);
        assert_eq!(second.blocked_pending_operation_count, 0);
        assert!(!boundary.exists());
        assert!(live.exists());
    }

    #[test]
    fn pending_cleanup_deletes_only_a_verified_initial_ledger_and_is_idempotent() {
        let root = tempdir().unwrap();
        let operation_id = "verified-initial";
        let pending = pending_directory(root.path(), operation_id, u32::MAX, 1);
        fs::create_dir_all(&pending).unwrap();
        write_ledger(
            &pending.join("ledger.json"),
            &initial_pending_ledger(root.path(), operation_id, 1),
        )
        .unwrap();

        let store = OperationLedgerStore::new(root.path());
        let first = store.cleanup_abandoned_pending_operations(2).unwrap();
        assert_eq!(first.deleted_pending_operation_count, 1);
        assert_eq!(first.retained_pending_operation_count, 0);
        assert_eq!(first.blocked_pending_operation_count, 0);
        assert!(first.reclaimed_bytes > 0);
        assert!(!pending.exists());

        let second = store.cleanup_abandoned_pending_operations(2).unwrap();
        assert_eq!(second, Default::default());
    }

    #[test]
    fn pending_cleanup_blocks_corrupt_extra_and_noninitial_candidates() {
        let root = tempdir().unwrap();
        let operations = root.path().join("session-storage-v1/operations");
        fs::create_dir_all(&operations).unwrap();

        let corrupt = pending_directory(root.path(), "corrupt", u32::MAX, 1);
        fs::create_dir(&corrupt).unwrap();
        fs::write(corrupt.join("ledger.json"), b"corrupt").unwrap();

        let extra = pending_directory(root.path(), "extra", u32::MAX, 1);
        fs::create_dir(&extra).unwrap();
        write_ledger(
            &extra.join("ledger.json"),
            &initial_pending_ledger(root.path(), "extra", 1),
        )
        .unwrap();
        fs::write(extra.join("unexpected.bin"), b"do not delete").unwrap();

        let noninitial = pending_directory(root.path(), "noninitial", u32::MAX, 1);
        fs::create_dir(&noninitial).unwrap();
        let mut noninitial_ledger = initial_pending_ledger(root.path(), "noninitial", 1);
        noninitial_ledger.revision = 1;
        noninitial_ledger.phase = SessionStorageOperationPhase::Preflight;
        noninitial_ledger.updated_at_ms = 2;
        write_ledger(&noninitial.join("ledger.json"), &noninitial_ledger).unwrap();

        let identity_mismatch = pending_directory(root.path(), "identity-in-name", u32::MAX, 1);
        fs::create_dir(&identity_mismatch).unwrap();
        write_ledger(
            &identity_mismatch.join("ledger.json"),
            &initial_pending_ledger(root.path(), "different-ledger-id", 2),
        )
        .unwrap();

        let malformed = operations.join(".pending-invalid-name");
        fs::create_dir(&malformed).unwrap();

        let store = OperationLedgerStore::new(root.path());
        let receipt = store.cleanup_abandoned_pending_operations(3).unwrap();
        assert_eq!(receipt.deleted_pending_operation_count, 0);
        assert_eq!(receipt.retained_pending_operation_count, 0);
        assert_eq!(receipt.blocked_pending_operation_count, 5);
        assert_eq!(receipt.reclaimed_bytes, 0);
        for path in [corrupt, extra, noninitial, identity_mismatch, malformed] {
            assert!(path.exists(), "blocked candidate disappeared: {path:?}");
        }
    }
}

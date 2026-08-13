use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use rusqlite::{Connection, OpenFlags, MAIN_DB};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{file_ops::atomic_write, operation_log::timestamp_millis};

use super::{model::SessionRelation, relation::compare_sessions, semantic::read_semantic_session};

const BACKUP_SCHEMA_VERSION: u32 = 1;
const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;
const MAX_MARKER_BYTES: u64 = 64 * 1024;
const RETENTION_MS: u128 = 7 * 24 * 60 * 60 * 1_000;
const ISOLATED_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
const BACKUP_MARKER_NAME: &str = ".codex-switch-migration-backup-v1";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum MigrationBackupEntryKind {
    Session,
    SessionIndex,
    Database,
    StorageMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationBackupSource {
    pub source_path: PathBuf,
    pub payload_relative_path: PathBuf,
    pub kind: MigrationBackupEntryKind,
    pub expected_sha256: Option<String>,
    pub logical_thread_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MigrationBackupEntry {
    pub source_path: PathBuf,
    pub payload_relative_path: PathBuf,
    pub kind: MigrationBackupEntryKind,
    pub bytes: u64,
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logical_thread_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum MigrationBackupStatus {
    IntegrityVerified,
    IsolatedRestoreVerified,
    RuntimeVerified,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MigrationRuntimeVerification {
    pub expected_session_count: usize,
    pub listed_session_count: usize,
    pub resumed_session_count: usize,
    #[serde(default)]
    pub continued_session_count: usize,
    pub tool_session_count: usize,
    pub tool_round_trip_verified: bool,
    #[serde(default)]
    pub available_categories: Vec<String>,
    #[serde(default)]
    pub continued_categories: Vec<String>,
    #[serde(default)]
    pub conflict_payload_count: usize,
    #[serde(default)]
    pub conflict_payloads_verified: bool,
    #[serde(default)]
    pub conflict_proofs: Vec<MigrationRuntimeConflictProof>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_conflict_proof: Option<MigrationRuntimeCapabilityConflictProof>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_binary_identity: Option<MigrationRuntimeBinaryIdentity>,
    pub verified_at_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MigrationRuntimeBinaryIdentity {
    pub version: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MigrationRuntimeConflictProof {
    pub thread_id_sha256: String,
    pub canonical_payload_relative_path: PathBuf,
    pub canonical_sha256: String,
    pub recycle_payload_relative_path: PathBuf,
    pub recycle_payload_sha256: String,
    pub relation: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MigrationRuntimeCapabilityConflictProof {
    pub fixture_thread_id_sha256: String,
    pub canonical_bytes: u64,
    pub canonical_sha256: String,
    pub recycle_bytes: u64,
    pub recycle_sha256: String,
    pub relation: String,
}

const REQUIRED_RUNTIME_CATEGORIES: [&str; 5] =
    ["ordinary", "long", "subagent", "conflictCanonical", "tool"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MigrationBackupManifest {
    pub schema_version: u32,
    pub operation_id: String,
    pub created_at_ms: u128,
    pub expires_at_ms: u128,
    pub backup_dir: PathBuf,
    pub status: MigrationBackupStatus,
    pub entries: Vec<MigrationBackupEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolated_restore_verified_at_ms: Option<u128>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_verification: Option<MigrationRuntimeVerification>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MigrationBackupEnvelope {
    manifest: MigrationBackupManifest,
    integrity_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MigrationBackupMarker {
    schema_version: u32,
    operation_id: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct IsolatedRestoreReceipt {
    pub backup_dir: PathBuf,
    pub isolated_root: PathBuf,
    pub restored_file_count: usize,
    pub restored_bytes: u64,
    pub session_count: usize,
    pub database_count: usize,
    pub verified: bool,
}

pub trait MigrationBackupRuntimeVerifier {
    fn verify(
        &self,
        isolated_root: &Path,
        manifest: &MigrationBackupManifest,
    ) -> Result<MigrationRuntimeVerification, String>;
}

pub fn create_migration_backup(
    destination_root: &Path,
    operation_id: &str,
    sources: &[MigrationBackupSource],
) -> Result<MigrationBackupManifest, String> {
    validate_operation_id(operation_id)?;
    validate_absolute_directory(destination_root, "backup destination")?;
    if sources.is_empty() {
        return Err("migration backup has no source files".to_string());
    }
    validate_sources(destination_root, sources)?;

    let backup_dir = destination_root.join(operation_id);
    if backup_dir.exists() {
        return Err("migration backup directory already exists".to_string());
    }
    fs::create_dir(&backup_dir)
        .map_err(|_| "failed to create migration backup directory".to_string())?;
    write_backup_marker(&backup_dir, operation_id)?;
    let result = create_migration_backup_inner(&backup_dir, operation_id, sources);
    if result.is_err() {
        let _ = fs::remove_dir_all(&backup_dir);
    }
    result
}

fn create_migration_backup_inner(
    backup_dir: &Path,
    operation_id: &str,
    sources: &[MigrationBackupSource],
) -> Result<MigrationBackupManifest, String> {
    let mut entries = Vec::with_capacity(sources.len());
    let mut sorted_sources = sources.to_vec();
    sorted_sources
        .sort_by(|left, right| left.payload_relative_path.cmp(&right.payload_relative_path));
    let mut database_source_index = 0_usize;
    for source in &sorted_sources {
        let target = backup_dir
            .join("payload")
            .join(&source.payload_relative_path);
        let (bytes, sha256) = match source.kind {
            MigrationBackupEntryKind::Database => {
                let index = database_source_index;
                database_source_index = database_source_index.saturating_add(1);
                snapshot_sqlite(&source.source_path, &target)
                    .map_err(|error| format!("{error}; database source index {index}"))?
            }
            _ => copy_regular_file_verified(
                &source.source_path,
                &target,
                source.expected_sha256.as_deref(),
            )?,
        };
        if source.kind == MigrationBackupEntryKind::Session {
            let semantic = read_semantic_session(&target)
                .map_err(|error| format!("backup session validation failed: {:?}", error.kind))?;
            if source
                .logical_thread_id
                .as_ref()
                .is_some_and(|thread_id| thread_id != &semantic.thread_id)
            {
                return Err("backup session thread identity changed".to_string());
            }
        }
        entries.push(MigrationBackupEntry {
            source_path: source.source_path.clone(),
            payload_relative_path: source.payload_relative_path.clone(),
            kind: source.kind,
            bytes,
            sha256,
            logical_thread_id: source.logical_thread_id.clone(),
        });
    }

    let created_at_ms = timestamp_millis()?;
    let manifest = MigrationBackupManifest {
        schema_version: BACKUP_SCHEMA_VERSION,
        operation_id: operation_id.to_string(),
        created_at_ms,
        expires_at_ms: created_at_ms.saturating_add(RETENTION_MS),
        backup_dir: backup_dir.to_path_buf(),
        status: MigrationBackupStatus::IntegrityVerified,
        entries,
        isolated_restore_verified_at_ms: None,
        runtime_verification: None,
    };
    write_manifest(&manifest)?;
    verify_migration_backup(backup_dir)
}

pub fn verify_migration_backup(backup_dir: &Path) -> Result<MigrationBackupManifest, String> {
    validate_absolute_directory(backup_dir, "migration backup")?;
    let manifest = read_manifest(backup_dir)?;
    if manifest.backup_dir != backup_dir {
        return Err("migration backup manifest path does not match its directory".to_string());
    }
    verify_backup_marker(backup_dir, &manifest.operation_id)?;
    for entry in &manifest.entries {
        validate_relative_path(&entry.payload_relative_path)?;
        validate_sha256(&entry.sha256)?;
        let payload = backup_dir
            .join("payload")
            .join(&entry.payload_relative_path);
        let metadata = safe_file_metadata(&payload, "migration backup payload")?;
        if metadata.len() != entry.bytes || sha256_regular_file(&payload)? != entry.sha256 {
            return Err("migration backup payload integrity check failed".to_string());
        }
        match entry.kind {
            MigrationBackupEntryKind::Database => quick_check_sqlite(&payload)?,
            MigrationBackupEntryKind::Session => {
                let semantic = read_semantic_session(&payload).map_err(|error| {
                    format!("backup session validation failed: {:?}", error.kind)
                })?;
                if entry
                    .logical_thread_id
                    .as_ref()
                    .is_some_and(|thread_id| thread_id != &semantic.thread_id)
                {
                    return Err("migration backup session identity is invalid".to_string());
                }
            }
            MigrationBackupEntryKind::SessionIndex | MigrationBackupEntryKind::StorageMetadata => {}
        }
    }
    Ok(manifest)
}

pub fn delete_expired_migration_backup(
    backup_dir: &Path,
    expected_operation_id: &str,
    now_ms: u128,
) -> Result<u64, String> {
    delete_verified_migration_backup(backup_dir, expected_operation_id, Some(now_ms))
}

pub(crate) fn delete_owned_migration_backup(
    backup_dir: &Path,
    expected_operation_id: &str,
) -> Result<u64, String> {
    delete_verified_migration_backup(backup_dir, expected_operation_id, None)
}

pub fn verify_migration_backup_sources(
    manifest: &MigrationBackupManifest,
    sources: &[MigrationBackupSource],
) -> Result<(), String> {
    if manifest.entries.len() != sources.len() {
        return Err("migration backup source inventory does not match preflight".to_string());
    }
    let mut expected = sources.to_vec();
    expected.sort_by(|left, right| left.payload_relative_path.cmp(&right.payload_relative_path));
    let mut actual = manifest.entries.iter().collect::<Vec<_>>();
    actual.sort_by(|left, right| left.payload_relative_path.cmp(&right.payload_relative_path));
    for (source, entry) in expected.iter().zip(actual) {
        if source.source_path != entry.source_path
            || source.payload_relative_path != entry.payload_relative_path
            || source.kind != entry.kind
            || source.logical_thread_id != entry.logical_thread_id
            || source
                .expected_sha256
                .as_ref()
                .is_some_and(|digest| digest != &entry.sha256)
        {
            return Err("migration backup source inventory does not match preflight".to_string());
        }
    }
    Ok(())
}

pub fn restore_migration_backup_to_isolated(
    backup_dir: &Path,
    isolated_root: &Path,
) -> Result<IsolatedRestoreReceipt, String> {
    let mut manifest = verify_migration_backup(backup_dir)?;
    validate_empty_restore_root(isolated_root)?;
    fs::create_dir_all(isolated_root)
        .map_err(|_| "failed to create isolated restore directory".to_string())?;

    let result = (|| {
        let mut restored_bytes = 0_u64;
        let mut session_count = 0_usize;
        let mut database_count = 0_usize;
        for entry in &manifest.entries {
            let source = backup_dir
                .join("payload")
                .join(&entry.payload_relative_path);
            let target = isolated_root.join(&entry.payload_relative_path);
            let (bytes, sha256) =
                copy_regular_file_verified(&source, &target, Some(&entry.sha256))?;
            if bytes != entry.bytes || sha256 != entry.sha256 {
                return Err("isolated restore payload changed during copy".to_string());
            }
            match entry.kind {
                MigrationBackupEntryKind::Database => {
                    quick_check_sqlite(&target)?;
                    database_count = database_count.saturating_add(1);
                }
                MigrationBackupEntryKind::Session => {
                    let semantic = read_semantic_session(&target).map_err(|error| {
                        format!("isolated session validation failed: {:?}", error.kind)
                    })?;
                    if entry
                        .logical_thread_id
                        .as_ref()
                        .is_some_and(|thread_id| thread_id != &semantic.thread_id)
                    {
                        return Err("isolated restore session identity is invalid".to_string());
                    }
                    session_count = session_count.saturating_add(1);
                }
                MigrationBackupEntryKind::SessionIndex
                | MigrationBackupEntryKind::StorageMetadata => {}
            }
            restored_bytes = restored_bytes.saturating_add(bytes);
        }
        Ok(IsolatedRestoreReceipt {
            backup_dir: backup_dir.to_path_buf(),
            isolated_root: isolated_root.to_path_buf(),
            restored_file_count: manifest.entries.len(),
            restored_bytes,
            session_count,
            database_count,
            verified: true,
        })
    })();

    match result {
        Ok(receipt) => {
            let verified_at_ms = timestamp_millis()?;
            manifest.status = MigrationBackupStatus::IsolatedRestoreVerified;
            manifest.isolated_restore_verified_at_ms = Some(verified_at_ms);
            write_manifest(&manifest)?;
            verify_migration_backup(backup_dir)?;
            Ok(receipt)
        }
        Err(error) => {
            let cleanup = cleanup_isolated_root(isolated_root);
            match cleanup {
                Ok(()) => Err(error),
                Err(cleanup_error) => Err(format!(
                    "{error}; isolated restore cleanup failed: {cleanup_error}"
                )),
            }
        }
    }
}

pub fn verify_migration_backup_with_runtime<V: MigrationBackupRuntimeVerifier>(
    backup_dir: &Path,
    isolated_root: &Path,
    verifier: &V,
) -> Result<MigrationBackupManifest, String> {
    let existing = verify_migration_backup(backup_dir)?;
    if existing.status == MigrationBackupStatus::RuntimeVerified {
        return Ok(existing);
    }
    let receipt = restore_migration_backup_to_isolated(backup_dir, isolated_root)?;
    let mut manifest = verify_migration_backup(backup_dir)?;
    let runtime_result = verifier.verify(isolated_root, &manifest);
    let cleanup_result = cleanup_isolated_root(isolated_root);
    let runtime = match (runtime_result, cleanup_result) {
        (Ok(runtime), Ok(())) => runtime,
        (Err(error), Ok(())) => return Err(error),
        (Ok(_), Err(error)) => {
            return Err(format!(
                "isolated runtime verification cleanup failed: {error}"
            ))
        }
        (Err(runtime_error), Err(cleanup_error)) => {
            return Err(format!(
                "{runtime_error}; isolated runtime verification cleanup failed: {cleanup_error}"
            ))
        }
    };
    let backup_sessions = manifest
        .entries
        .iter()
        .filter(|entry| entry.kind == MigrationBackupEntryKind::Session)
        .count();
    if !receipt.verified
        || runtime.expected_session_count == 0
        || runtime.listed_session_count != runtime.expected_session_count
        || (backup_sessions > 0 && runtime.resumed_session_count != runtime.expected_session_count)
        || runtime.continued_session_count == 0
        || runtime.verified_at_ms < manifest.created_at_ms
        || validate_complete_runtime_contract(&manifest, &runtime).is_err()
    {
        return Err("Codex runtime did not verify the complete migration backup".to_string());
    }
    manifest.status = MigrationBackupStatus::RuntimeVerified;
    manifest.runtime_verification = Some(runtime);
    write_manifest(&manifest)?;
    verify_migration_backup(backup_dir)
}

pub fn verify_migration_backup_with_isolated_restore(
    backup_dir: &Path,
    isolated_root: &Path,
) -> Result<MigrationBackupManifest, String> {
    let existing = verify_migration_backup(backup_dir)?;
    if matches!(
        existing.status,
        MigrationBackupStatus::IsolatedRestoreVerified | MigrationBackupStatus::RuntimeVerified
    ) {
        return Ok(existing);
    }
    let receipt = restore_migration_backup_to_isolated(backup_dir, isolated_root)?;
    let cleanup = cleanup_isolated_root(isolated_root);
    match cleanup {
        Ok(()) if receipt.verified => verify_migration_backup(backup_dir),
        Ok(()) => Err("isolated migration backup restore was not verified".to_string()),
        Err(error) => Err(format!(
            "isolated migration backup restore cleanup failed: {error}"
        )),
    }
}

fn validate_sources(
    destination_root: &Path,
    sources: &[MigrationBackupSource],
) -> Result<(), String> {
    let destination = fs::canonicalize(destination_root)
        .map_err(|_| "migration backup destination is unavailable".to_string())?;
    let mut source_paths = BTreeSet::new();
    let mut payload_paths = BTreeSet::new();
    for source in sources {
        if !source.source_path.is_absolute() {
            return Err("migration backup source path is invalid".to_string());
        }
        validate_relative_path(&source.payload_relative_path)?;
        let canonical_source = fs::canonicalize(&source.source_path)
            .map_err(|_| "migration backup source is unavailable".to_string())?;
        if canonical_source.starts_with(&destination) || destination.starts_with(&canonical_source)
        {
            return Err("migration backup source and destination overlap".to_string());
        }
        safe_file_metadata(&canonical_source, "migration backup source")?;
        if !source_paths.insert(path_key(&canonical_source))
            || !payload_paths.insert(path_key(&source.payload_relative_path))
        {
            return Err("migration backup sources contain a duplicate path".to_string());
        }
        if let Some(digest) = &source.expected_sha256 {
            validate_sha256(digest)?;
        }
        if source
            .logical_thread_id
            .as_ref()
            .is_some_and(|thread_id| thread_id.trim().is_empty() || thread_id.len() > 256)
        {
            return Err("migration backup thread identity is invalid".to_string());
        }
    }
    Ok(())
}

fn snapshot_sqlite(source: &Path, target: &Path) -> Result<(u64, String), String> {
    safe_file_metadata(source, "migration SQLite source")?;
    let canonical_source = fs::canonicalize(source)
        .map_err(|_| "failed to resolve migration SQLite source".to_string())?;
    ensure_new_parent(target)?;
    if target.exists() {
        return Err("migration backup payload already exists".to_string());
    }
    let target_parent = target
        .parent()
        .ok_or_else(|| "migration backup payload has no parent".to_string())?;
    let target_name = target
        .file_name()
        .ok_or_else(|| "migration backup payload has no file name".to_string())?;
    let canonical_target = fs::canonicalize(target_parent)
        .map_err(|_| "failed to resolve migration backup payload directory".to_string())?
        .join(target_name);
    let connection = Connection::open_with_flags(
        &canonical_source,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| sqlite_failure("failed to open migration SQLite source", &error))?;
    connection
        .backup(MAIN_DB, &canonical_target, None)
        .map_err(|error| sqlite_failure("failed to snapshot migration SQLite source", &error))?;
    drop(connection);
    quick_check_sqlite(target)?;
    let metadata = safe_file_metadata(target, "migration SQLite snapshot")?;
    Ok((metadata.len(), sha256_regular_file(target)?))
}

fn sqlite_failure(context: &str, error: &rusqlite::Error) -> String {
    match error.sqlite_error() {
        Some(sqlite) => format!(
            "{context} (SQLite {:?}, extended code {})",
            sqlite.code, sqlite.extended_code
        ),
        None => context.to_string(),
    }
}

fn quick_check_sqlite(path: &Path) -> Result<(), String> {
    let canonical_path = fs::canonicalize(path)
        .map_err(|_| "failed to resolve migration SQLite snapshot".to_string())?;
    let connection = Connection::open_with_flags(
        &canonical_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| "failed to open migration SQLite snapshot".to_string())?;
    let result: String = connection
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(|_| "failed to verify migration SQLite snapshot".to_string())?;
    if result == "ok" {
        Ok(())
    } else {
        Err("migration SQLite snapshot failed quick_check".to_string())
    }
}

fn copy_regular_file_verified(
    source: &Path,
    target: &Path,
    expected_sha256: Option<&str>,
) -> Result<(u64, String), String> {
    let before = safe_file_metadata(source, "migration backup source")?;
    let source_sha256_before = sha256_regular_file(source)?;
    if expected_sha256.is_some_and(|expected| expected != source_sha256_before) {
        return Err("migration backup source changed after planning".to_string());
    }
    ensure_new_parent(target)?;
    if target.exists() {
        return Err("migration backup payload already exists".to_string());
    }
    let mut source_file = open_regular_file(source)?;
    let mut target_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)
        .map_err(|_| "failed to create migration backup payload".to_string())?;
    let mut hasher = Sha256::new();
    let mut bytes = 0_u64;
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = source_file
            .read(&mut buffer)
            .map_err(|_| "failed to read migration backup source".to_string())?;
        if read == 0 {
            break;
        }
        target_file
            .write_all(&buffer[..read])
            .map_err(|_| "failed to write migration backup payload".to_string())?;
        hasher.update(&buffer[..read]);
        bytes = bytes
            .checked_add(read as u64)
            .ok_or_else(|| "migration backup size overflowed".to_string())?;
    }
    target_file
        .sync_all()
        .map_err(|_| "failed to flush migration backup payload".to_string())?;
    drop(target_file);
    let after = safe_file_metadata(source, "migration backup source")?;
    if file_stamp(&before) != file_stamp(&after) || bytes != before.len() {
        return Err("migration backup source changed while it was copied".to_string());
    }
    let source_sha256_after = sha256_regular_file(source)?;
    let copied_sha256 = hex_digest(hasher.finalize());
    if source_sha256_before != source_sha256_after || source_sha256_after != copied_sha256 {
        return Err("migration backup source changed while it was copied".to_string());
    }
    let target_metadata = safe_file_metadata(target, "migration backup payload")?;
    if target_metadata.len() != bytes || sha256_regular_file(target)? != copied_sha256 {
        return Err("migration backup payload verification failed".to_string());
    }
    Ok((bytes, copied_sha256))
}

fn sha256_regular_file(path: &Path) -> Result<String, String> {
    let before = safe_file_metadata(path, "checksum source")?;
    let mut file = open_regular_file(path)?;
    let mut hasher = Sha256::new();
    let mut bytes = 0_u64;
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|_| "failed to read checksum source".to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        bytes = bytes
            .checked_add(read as u64)
            .ok_or_else(|| "checksum source size overflowed".to_string())?;
    }
    let after = safe_file_metadata(path, "checksum source")?;
    if file_stamp(&before) != file_stamp(&after) || bytes != before.len() {
        return Err("checksum source changed while it was read".to_string());
    }
    Ok(hex_digest(hasher.finalize()))
}

fn write_manifest(manifest: &MigrationBackupManifest) -> Result<(), String> {
    validate_manifest(manifest)?;
    let envelope = MigrationBackupEnvelope {
        integrity_sha256: manifest_digest(manifest)?,
        manifest: manifest.clone(),
    };
    let bytes = serde_json::to_vec_pretty(&envelope)
        .map_err(|_| "failed to serialize migration backup manifest".to_string())?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err("migration backup manifest reached its size limit".to_string());
    }
    atomic_write(&manifest.backup_dir.join("manifest.json"), &bytes)
}

fn write_backup_marker(backup_dir: &Path, operation_id: &str) -> Result<(), String> {
    validate_operation_id(operation_id)?;
    let marker = MigrationBackupMarker {
        schema_version: BACKUP_SCHEMA_VERSION,
        operation_id: operation_id.to_string(),
    };
    let bytes = serde_json::to_vec(&marker)
        .map_err(|_| "failed to serialize migration backup marker".to_string())?;
    atomic_write(&backup_dir.join(BACKUP_MARKER_NAME), &bytes)
        .map_err(|_| "failed to persist migration backup marker".to_string())
}

fn verify_backup_marker(backup_dir: &Path, operation_id: &str) -> Result<(), String> {
    let path = backup_dir.join(BACKUP_MARKER_NAME);
    let metadata = safe_file_metadata(&path, "migration backup marker")?;
    if metadata.len() == 0 || metadata.len() > MAX_MARKER_BYTES {
        return Err("migration backup marker is invalid".to_string());
    }
    let bytes = fs::read(path).map_err(|_| "migration backup marker is unreadable".to_string())?;
    let marker = serde_json::from_slice::<MigrationBackupMarker>(&bytes)
        .map_err(|_| "migration backup marker is invalid".to_string())?;
    if marker.schema_version != BACKUP_SCHEMA_VERSION || marker.operation_id != operation_id {
        return Err("migration backup marker identity changed".to_string());
    }
    Ok(())
}

fn delete_verified_migration_backup(
    backup_dir: &Path,
    expected_operation_id: &str,
    expiry_check_ms: Option<u128>,
) -> Result<u64, String> {
    validate_operation_id(expected_operation_id)?;
    let first = verify_migration_backup(backup_dir)?;
    let first_bytes = verify_exact_backup_tree(backup_dir, &first)?;
    let second = verify_migration_backup(backup_dir)?;
    let second_bytes = verify_exact_backup_tree(backup_dir, &second)?;
    if first != second || first_bytes != second_bytes {
        return Err("migration backup changed during deletion verification".to_string());
    }
    if first.operation_id != expected_operation_id
        || backup_dir.file_name().and_then(|name| name.to_str()) != Some(expected_operation_id)
    {
        return Err("migration backup deletion identity changed".to_string());
    }
    if expiry_check_ms.is_some_and(|now_ms| first.expires_at_ms > now_ms) {
        return Err("migration backup retention has not expired".to_string());
    }
    fs::remove_dir_all(backup_dir)
        .map_err(|_| "failed to remove verified migration backup".to_string())?;
    Ok(first_bytes)
}

fn verify_exact_backup_tree(
    backup_dir: &Path,
    manifest: &MigrationBackupManifest,
) -> Result<u64, String> {
    let root = fs::canonicalize(backup_dir)
        .map_err(|_| "failed to resolve migration backup directory".to_string())?;
    let mut allowed_files = BTreeSet::new();
    let mut allowed_directories = BTreeSet::from([root.clone()]);
    for path in std::iter::once(root.join("manifest.json"))
        .chain(std::iter::once(root.join(BACKUP_MARKER_NAME)))
        .chain(
            manifest
                .entries
                .iter()
                .map(|entry| root.join("payload").join(&entry.payload_relative_path)),
        )
    {
        safe_file_metadata(&path, "migration backup declared file")?;
        let resolved = fs::canonicalize(&path)
            .map_err(|_| "failed to resolve migration backup declared file".to_string())?;
        if !resolved.starts_with(&root) {
            return Err("migration backup declared file escaped its directory".to_string());
        }
        let mut parent = resolved.parent();
        while let Some(directory) = parent {
            if !directory.starts_with(&root) {
                break;
            }
            allowed_directories.insert(directory.to_path_buf());
            if directory == root {
                break;
            }
            parent = directory.parent();
        }
        allowed_files.insert(resolved);
    }

    let mut observed_files = BTreeSet::new();
    let mut pending = vec![root.clone()];
    let mut total_bytes = 0_u64;
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)
            .map_err(|_| "migration backup directory is unreadable".to_string())?
        {
            let entry =
                entry.map_err(|_| "migration backup directory entry is unreadable".to_string())?;
            let metadata = fs::symlink_metadata(entry.path())
                .map_err(|_| "migration backup directory entry is unreadable".to_string())?;
            if metadata_is_link_or_reparse(&metadata) {
                return Err("migration backup contains a link".to_string());
            }
            let path = fs::canonicalize(entry.path())
                .map_err(|_| "failed to resolve migration backup entry".to_string())?;
            if metadata.is_dir() {
                if !allowed_directories.contains(&path) {
                    return Err("migration backup contains an undeclared directory".to_string());
                }
                pending.push(path);
            } else if metadata.is_file() {
                if !allowed_files.contains(&path) {
                    return Err("migration backup contains an undeclared file".to_string());
                }
                observed_files.insert(path);
                total_bytes = total_bytes
                    .checked_add(metadata.len())
                    .ok_or_else(|| "migration backup byte count overflowed".to_string())?;
            } else {
                return Err("migration backup contains an unsupported entry".to_string());
            }
        }
    }
    if observed_files != allowed_files {
        return Err("migration backup declared file set changed".to_string());
    }
    Ok(total_bytes)
}

fn read_manifest(backup_dir: &Path) -> Result<MigrationBackupManifest, String> {
    let path = backup_dir.join("manifest.json");
    let metadata = safe_file_metadata(&path, "migration backup manifest")?;
    if metadata.len() == 0 || metadata.len() > MAX_MANIFEST_BYTES {
        return Err("migration backup manifest is invalid".to_string());
    }
    let bytes =
        fs::read(&path).map_err(|_| "migration backup manifest is unreadable".to_string())?;
    let envelope = serde_json::from_slice::<MigrationBackupEnvelope>(&bytes)
        .map_err(|_| "migration backup manifest is invalid".to_string())?;
    validate_manifest(&envelope.manifest)?;
    if envelope.integrity_sha256 != manifest_digest(&envelope.manifest)? {
        return Err("migration backup manifest integrity check failed".to_string());
    }
    Ok(envelope.manifest)
}

fn validate_manifest(manifest: &MigrationBackupManifest) -> Result<(), String> {
    if manifest.schema_version != BACKUP_SCHEMA_VERSION
        || manifest.expires_at_ms < manifest.created_at_ms
    {
        return Err("migration backup manifest version is unsupported".to_string());
    }
    validate_operation_id(&manifest.operation_id)?;
    if !manifest.backup_dir.is_absolute() || manifest.entries.is_empty() {
        return Err("migration backup manifest paths are invalid".to_string());
    }
    let mut relative_paths = BTreeSet::new();
    for entry in &manifest.entries {
        if !entry.source_path.is_absolute() {
            return Err("migration backup manifest source path is invalid".to_string());
        }
        validate_relative_path(&entry.payload_relative_path)?;
        validate_sha256(&entry.sha256)?;
        if !relative_paths.insert(path_key(&entry.payload_relative_path)) {
            return Err("migration backup manifest contains duplicate payloads".to_string());
        }
    }
    match manifest.status {
        MigrationBackupStatus::IntegrityVerified => {
            if manifest.isolated_restore_verified_at_ms.is_some()
                || manifest.runtime_verification.is_some()
            {
                return Err("migration backup verification state is invalid".to_string());
            }
        }
        MigrationBackupStatus::IsolatedRestoreVerified => {
            if manifest.isolated_restore_verified_at_ms.is_none()
                || manifest.runtime_verification.is_some()
            {
                return Err("migration backup verification state is invalid".to_string());
            }
        }
        MigrationBackupStatus::RuntimeVerified => {
            let Some(runtime) = &manifest.runtime_verification else {
                return Err("migration backup verification state is invalid".to_string());
            };
            if manifest.isolated_restore_verified_at_ms.is_none()
                || runtime.expected_session_count == 0
                || runtime.listed_session_count != runtime.expected_session_count
                || runtime.resumed_session_count != runtime.expected_session_count
                || runtime.continued_session_count == 0
                || runtime.verified_at_ms < manifest.created_at_ms
                || validate_complete_runtime_contract(manifest, runtime).is_err()
            {
                return Err("migration backup verification state is invalid".to_string());
            }
        }
    }
    Ok(())
}

fn validate_complete_runtime_contract(
    manifest: &MigrationBackupManifest,
    runtime: &MigrationRuntimeVerification,
) -> Result<(), String> {
    let available = runtime
        .available_categories
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let continued = runtime
        .continued_categories
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let required = REQUIRED_RUNTIME_CATEGORIES
        .into_iter()
        .collect::<BTreeSet<_>>();
    if available != required
        || continued != required
        || runtime.tool_session_count == 0
        || runtime.tool_session_count > runtime.expected_session_count
        || !runtime.tool_round_trip_verified
        || !runtime.conflict_payloads_verified
        || runtime.conflict_payload_count != runtime.conflict_proofs.len()
    {
        return Err("migration runtime category coverage is incomplete".to_string());
    }
    let capability = runtime
        .capability_conflict_proof
        .as_ref()
        .ok_or_else(|| "migration runtime conflict capability proof is missing".to_string())?;
    validate_sha256(&capability.fixture_thread_id_sha256)?;
    validate_sha256(&capability.canonical_sha256)?;
    validate_sha256(&capability.recycle_sha256)?;
    if capability.canonical_bytes == 0
        || capability.recycle_bytes == 0
        || capability.canonical_sha256 == capability.recycle_sha256
        || capability.relation != "divergent"
    {
        return Err("migration runtime conflict capability proof is invalid".to_string());
    }
    let binary = runtime
        .runtime_binary_identity
        .as_ref()
        .ok_or_else(|| "migration runtime binary identity is missing".to_string())?;
    validate_sha256(&binary.sha256)?;
    if binary.bytes == 0
        || binary.version.is_empty()
        || binary.version.len() > 256
        || !binary.version.to_ascii_lowercase().contains("codex")
        || binary.version.chars().any(char::is_control)
    {
        return Err("migration runtime binary identity is invalid".to_string());
    }

    let entries = manifest
        .entries
        .iter()
        .map(|entry| (path_key(&entry.payload_relative_path), entry))
        .collect::<BTreeMap<_, _>>();
    let mut unique = BTreeSet::new();
    for proof in &runtime.conflict_proofs {
        validate_sha256(&proof.thread_id_sha256)?;
        validate_sha256(&proof.canonical_sha256)?;
        validate_sha256(&proof.recycle_payload_sha256)?;
        validate_relative_path(&proof.canonical_payload_relative_path)?;
        validate_relative_path(&proof.recycle_payload_relative_path)?;
        if !matches!(proof.relation.as_str(), "divergent" | "unknown")
            || !unique.insert((
                proof.thread_id_sha256.clone(),
                path_key(&proof.recycle_payload_relative_path),
            ))
        {
            return Err("migration runtime conflict proof is invalid".to_string());
        }
        let canonical = entries
            .get(&path_key(&proof.canonical_payload_relative_path))
            .ok_or_else(|| "migration runtime canonical proof is unbound".to_string())?;
        let recycle = entries
            .get(&path_key(&proof.recycle_payload_relative_path))
            .ok_or_else(|| "migration runtime recycle proof is unbound".to_string())?;
        let canonical_thread = canonical
            .logical_thread_id
            .as_deref()
            .ok_or_else(|| "migration runtime canonical thread is missing".to_string())?;
        if recycle.logical_thread_id.as_deref() != Some(canonical_thread)
            || hex_digest(Sha256::digest(canonical_thread.as_bytes())) != proof.thread_id_sha256
            || canonical.sha256 != proof.canonical_sha256
            || recycle.sha256 != proof.recycle_payload_sha256
        {
            return Err("migration runtime conflict proof identity changed".to_string());
        }
        let canonical_semantic = read_semantic_session(
            &manifest
                .backup_dir
                .join("payload")
                .join(&canonical.payload_relative_path),
        )
        .map_err(|_| "migration runtime canonical proof is unreadable".to_string())?;
        let recycle_semantic = read_semantic_session(
            &manifest
                .backup_dir
                .join("payload")
                .join(&recycle.payload_relative_path),
        )
        .map_err(|_| "migration runtime recycle proof is unreadable".to_string())?;
        let relation = match compare_sessions(&canonical_semantic, &recycle_semantic) {
            SessionRelation::Divergent => "divergent",
            SessionRelation::Unknown => "unknown",
            _ => return Err("migration runtime conflict proof is not a conflict".to_string()),
        };
        if relation != proof.relation {
            return Err("migration runtime conflict proof relation changed".to_string());
        }
    }
    Ok(())
}

fn manifest_digest(manifest: &MigrationBackupManifest) -> Result<String, String> {
    let bytes = serde_json::to_vec(manifest)
        .map_err(|_| "failed to serialize migration backup manifest".to_string())?;
    Ok(hex_digest(Sha256::digest(bytes)))
}

fn validate_empty_restore_root(path: &Path) -> Result<(), String> {
    if !path.is_absolute() {
        return Err("isolated restore path is invalid".to_string());
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_dir() || metadata_is_link_or_reparse(&metadata) {
                return Err("isolated restore path is unsafe".to_string());
            }
            if fs::read_dir(path)
                .map_err(|_| "isolated restore path is unreadable".to_string())?
                .next()
                .is_some()
            {
                return Err("isolated restore directory must be empty".to_string());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err("isolated restore path is unavailable".to_string()),
    }
    Ok(())
}

pub(crate) fn cleanup_isolated_root(path: &Path) -> std::io::Result<()> {
    let deadline = Instant::now() + ISOLATED_CLEANUP_TIMEOUT;
    loop {
        match fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
            Ok(metadata) if !metadata.is_dir() || metadata_is_link_or_reparse(&metadata) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "isolated restore root is unsafe",
                ));
            }
            Ok(_) => {}
        }
        match fs::remove_dir_all(path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error),
        }
    }
}

fn validate_absolute_directory(path: &Path, label: &str) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(format!("{label} path is invalid"));
    }
    let metadata = fs::symlink_metadata(path).map_err(|_| format!("{label} is unavailable"))?;
    if !metadata.is_dir() || metadata_is_link_or_reparse(&metadata) {
        return Err(format!("{label} is unsafe"));
    }
    Ok(())
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
        return Err("migration backup relative path is invalid".to_string());
    }
    Ok(())
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
        Err("migration backup operation ID is invalid".to_string())
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
        Err("migration backup checksum is invalid".to_string())
    }
}

fn ensure_new_parent(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "migration backup payload has no parent".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|_| "failed to create migration backup payload directory".to_string())?;
    let metadata = fs::symlink_metadata(parent)
        .map_err(|_| "failed to inspect migration backup payload directory".to_string())?;
    if !metadata.is_dir() || metadata_is_link_or_reparse(&metadata) {
        return Err("migration backup payload directory is unsafe".to_string());
    }
    Ok(())
}

fn safe_file_metadata(path: &Path, label: &str) -> Result<fs::Metadata, String> {
    let metadata = fs::symlink_metadata(path).map_err(|_| format!("{label} is unavailable"))?;
    if !metadata.is_file() || metadata_is_link_or_reparse(&metadata) {
        return Err(format!("{label} is unsafe"));
    }
    Ok(metadata)
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
        .map_err(|_| "failed to open regular migration backup file".to_string())
}

fn file_stamp(metadata: &fs::Metadata) -> (u64, Option<std::time::SystemTime>) {
    (metadata.len(), metadata.modified().ok())
}

fn path_key(path: &Path) -> String {
    let normalized = path.to_string_lossy().replace('/', "\\");
    if cfg!(windows) {
        normalized.to_ascii_lowercase()
    } else {
        normalized
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
    use std::{fs, path::Path};

    use rusqlite::Connection;
    use tempfile::tempdir;

    use super::{
        create_migration_backup, quick_check_sqlite, restore_migration_backup_to_isolated,
        snapshot_sqlite, verify_migration_backup, verify_migration_backup_with_runtime,
        MigrationBackupEntryKind, MigrationBackupRuntimeVerifier, MigrationBackupSource,
        MigrationBackupStatus, MigrationRuntimeBinaryIdentity,
        MigrationRuntimeCapabilityConflictProof, MigrationRuntimeVerification,
        REQUIRED_RUNTIME_CATEGORIES,
    };

    struct PassingRuntimeVerifier;

    impl MigrationBackupRuntimeVerifier for PassingRuntimeVerifier {
        fn verify(
            &self,
            _isolated_root: &Path,
            manifest: &super::MigrationBackupManifest,
        ) -> Result<MigrationRuntimeVerification, String> {
            let sessions = manifest
                .entries
                .iter()
                .filter(|entry| entry.kind == MigrationBackupEntryKind::Session)
                .count();
            Ok(MigrationRuntimeVerification {
                expected_session_count: sessions,
                listed_session_count: sessions,
                resumed_session_count: sessions,
                continued_session_count: usize::from(sessions > 0),
                tool_session_count: 1,
                tool_round_trip_verified: true,
                available_categories: REQUIRED_RUNTIME_CATEGORIES
                    .iter()
                    .map(|category| (*category).to_string())
                    .collect(),
                continued_categories: REQUIRED_RUNTIME_CATEGORIES
                    .iter()
                    .map(|category| (*category).to_string())
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

    fn fixture_sources(root: &Path) -> Vec<MigrationBackupSource> {
        let session = root.join("session.jsonl");
        fs::write(
            &session,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"thread-a\",\"model_provider\":\"openai\"}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"hello\"}}\n"
            ),
        )
        .unwrap();
        let database = root.join("state_5.sqlite");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, model_provider TEXT);
                 INSERT INTO threads VALUES ('thread-a', 'C:/canonical/session.jsonl', 'openai');",
            )
            .unwrap();
        drop(connection);
        vec![
            MigrationBackupSource {
                source_path: session,
                payload_relative_path: "canonical/sessions/session.jsonl".into(),
                kind: MigrationBackupEntryKind::Session,
                expected_sha256: None,
                logical_thread_id: Some("thread-a".to_string()),
            },
            MigrationBackupSource {
                source_path: database,
                payload_relative_path: "databases/state_5.sqlite".into(),
                kind: MigrationBackupEntryKind::Database,
                expected_sha256: None,
                logical_thread_id: None,
            },
        ]
    }

    #[cfg(windows)]
    #[test]
    fn snapshots_sqlite_to_an_extended_length_windows_path() {
        let root = tempdir().unwrap();
        let source = root.path().join("source.sqlite");
        let connection = Connection::open(&source).unwrap();
        connection
            .execute_batch("CREATE TABLE probe (value TEXT); INSERT INTO probe VALUES ('ok');")
            .unwrap();
        drop(connection);

        let mut target_parent = root.path().join("long-path");
        while target_parent
            .join("snapshot.sqlite")
            .to_string_lossy()
            .encode_utf16()
            .count()
            <= 300
        {
            target_parent = target_parent.join("0123456789abcdef");
        }
        fs::create_dir_all(&target_parent).unwrap();
        let target = target_parent.join("snapshot.sqlite");

        let (bytes, sha256) = snapshot_sqlite(&source, &target).unwrap();
        assert!(bytes > 0);
        assert_eq!(sha256.len(), 64);
        assert!(target.is_file());
        quick_check_sqlite(&target).unwrap();
    }

    #[test]
    fn creates_unencrypted_backup_and_verifies_isolated_restore_and_runtime() {
        let root = tempdir().unwrap();
        let source_root = root.path().join("source");
        let backups = root.path().join("backups");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&backups).unwrap();
        let sources = fixture_sources(&source_root);

        let created = create_migration_backup(&backups, "migration-1", &sources).unwrap();
        assert_eq!(created.status, MigrationBackupStatus::IntegrityVerified);
        assert_eq!(
            fs::read(backups.join("migration-1/payload/canonical/sessions/session.jsonl")).unwrap(),
            fs::read(&sources[0].source_path).unwrap()
        );

        let isolated = root.path().join("isolated");
        let verified = verify_migration_backup_with_runtime(
            &created.backup_dir,
            &isolated,
            &PassingRuntimeVerifier,
        )
        .unwrap();
        assert_eq!(verified.status, MigrationBackupStatus::RuntimeVerified);
        assert!(verified.runtime_verification.is_some());
        assert!(!isolated.exists());
        let repeated = verify_migration_backup_with_runtime(
            &created.backup_dir,
            &isolated,
            &PassingRuntimeVerifier,
        )
        .unwrap();
        assert_eq!(repeated, verified);
        assert!(!isolated.exists());
    }

    #[test]
    fn corrupted_payload_is_rejected_and_failed_restore_is_cleaned() {
        let root = tempdir().unwrap();
        let source_root = root.path().join("source");
        let backups = root.path().join("backups");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&backups).unwrap();
        let created =
            create_migration_backup(&backups, "migration-1", &fixture_sources(&source_root))
                .unwrap();
        fs::write(
            created
                .backup_dir
                .join("payload/canonical/sessions/session.jsonl"),
            b"corrupt",
        )
        .unwrap();
        assert!(verify_migration_backup(&created.backup_dir)
            .unwrap_err()
            .contains("integrity"));
        let isolated = root.path().join("isolated");
        assert!(restore_migration_backup_to_isolated(&created.backup_dir, &isolated).is_err());
        assert!(!isolated.exists());
    }

    #[test]
    fn rejects_duplicate_payloads_and_path_escape() {
        let root = tempdir().unwrap();
        let source_root = root.path().join("source");
        let backups = root.path().join("backups");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&backups).unwrap();
        let mut sources = fixture_sources(&source_root);
        sources[1].payload_relative_path = sources[0].payload_relative_path.clone();
        assert!(create_migration_backup(&backups, "migration-1", &sources)
            .unwrap_err()
            .contains("duplicate"));
        sources[1].payload_relative_path = "../escape.sqlite".into();
        assert!(create_migration_backup(&backups, "migration-2", &sources)
            .unwrap_err()
            .contains("relative path"));
    }
}

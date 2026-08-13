use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    str::FromStr,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(test)]
use std::io::{Seek, SeekFrom};
#[cfg(all(test, windows))]
use std::os::windows::fs::OpenOptionsExt;
#[cfg(all(test, windows))]
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;

#[cfg(test)]
use crate::{
    chat_process_state::validate_snapshot_bytes as validate_chat_process_state_bytes,
    file_ops::atomic_rewrite,
};

use rusqlite::{Connection, OpenFlags, MAIN_DB};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use toml_edit::DocumentMut;

use crate::{
    chat_process_state::{
        backup_source as chat_process_state_backup_source,
        existing_restore_target as existing_chat_process_state_restore_target,
        read_snapshot as read_chat_process_state_snapshot,
        restore_target as chat_process_state_restore_target, CHAT_PROCESS_STATE_RELATIVE_PATH,
    },
    codex_paths::{
        codex_paths_with_sqlite_home, local_codex_paths, resolve_user_codex_paths,
        validate_absolute_root, CodexPaths,
    },
    crypto::{protect, unprotect},
    file_ops::{atomic_write, walk_jsonl_files},
    operation_log::{OperationAction, OperationPhase, OperationRecord, OperationStatus},
    session_storage::write_barrier::{
        parent_directory_identity_at_path, recover_handle_create, recover_handle_delete,
        recover_handle_replace, stage_handle_delete, stage_handle_hardlink_create,
        HandleCreateIdentityBindings, HandleCreatePaths, HandleCreateRecoveryDecision,
        HandleDeleteIdentityBindings, HandleDeletePaths, HandleDeleteRecoveryDecision,
        HandleReplaceIdentityBindings, HandleReplacePaths, HandleReplaceRecoveryDecision,
        RegularFileIdentity, ResolvedHandleCreate, ResolvedHandleDelete, ResolvedHandleReplace,
        WriteExclusionGuard,
    },
};

static BACKUP_COUNTER: AtomicU64 = AtomicU64::new(0);
const SCOPED_BACKUP_MANIFEST_VERSION: u32 = 3;
const BACKUP_MANIFEST_VERSION: u32 = 4;
const BACKUP_RESTORE_JOURNAL_SCHEMA_VERSION: u32 = 1;
const BACKUP_RESTORE_OPERATION_DIRECTORY: &str = "backup-restore-operations";
const BACKUP_RESTORE_JOURNAL_FILE: &str = "journal.dpapi";
const BACKUP_RESTORE_JOURNAL_MAGIC: &[u8] = b"CSBRESTORE1\0";
const BACKUP_RESTORE_MAX_JOURNAL_BYTES: u64 = 64 * 1024 * 1024;
const BACKUP_RESTORE_MAX_CIPHERTEXT_BYTES: u64 = BACKUP_RESTORE_MAX_JOURNAL_BYTES * 2 + 64 * 1024;
const BACKUP_RESTORE_MAX_OPERATION_ID_BYTES: usize = 160;

const BACKUP_FILE_OVERHEAD_BYTES: u64 = 64 * 1024;
const MANIFEST_BASE_OVERHEAD_BYTES: u64 = 1024 * 1024;
const MANIFEST_ENTRY_OVERHEAD_BYTES: u64 = 4 * 1024;
const MIN_CAPACITY_RESERVE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const CAPACITY_RESERVE_PERCENT: u64 = 15;
const MAX_DPAPI_PAYLOAD_BYTES: u64 = u32::MAX as u64;
const STATE_DATABASE: &str = "state_5.sqlite";
const MANAGED_DATABASES: [&str; 4] = [
    STATE_DATABASE,
    "goals_1.sqlite",
    "memories_1.sqlite",
    "logs_2.sqlite",
];

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum BackupScope {
    #[default]
    Full,
    Runtime,
    RuntimeState,
    Sessions,
    StateOnly,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub enum CheckpointRole {
    Current,
    Shared,
    Visibility,
}

impl BackupScope {
    fn tracks_runtime_files(self) -> bool {
        matches!(self, Self::Full | Self::Runtime | Self::RuntimeState)
    }

    fn tracks_sessions(self) -> bool {
        !matches!(self, Self::RuntimeState | Self::StateOnly)
    }

    fn tracks_archived_sessions(self) -> bool {
        matches!(self, Self::Full | Self::Sessions)
    }

    fn tracks_process_state(self) -> bool {
        matches!(self, Self::RuntimeState)
    }

    fn tracked_databases(self) -> &'static [&'static str] {
        match self {
            Self::Full => &MANAGED_DATABASES,
            Self::Runtime | Self::RuntimeState | Self::Sessions | Self::StateOnly => {
                &[STATE_DATABASE]
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BackupManifest {
    pub version: u32,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<CheckpointRole>,
    pub created_at_ms: u128,
    pub source_root: PathBuf,
    pub root_existed: bool,
    #[serde(default)]
    pub scope: BackupScope,
    #[serde(default)]
    pub tracked_databases: Vec<String>,
    #[serde(default)]
    pub state_db_is_local: bool,
    #[serde(default)]
    pub tracked_process_state: bool,
    pub complete_sessions: bool,
    pub backup_dir: PathBuf,
    pub files: Vec<BackupFile>,
}

#[derive(Debug, Clone, Copy)]
struct CheckpointBinding<'a> {
    operation_id: &'a str,
    role: CheckpointRole,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BackupFile {
    pub source: PathBuf,
    pub relative_path: PathBuf,
    pub backup_path: PathBuf,
    pub bytes: u64,
    pub sha256: String,
    pub encrypted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RestoreResult {
    pub backup_dir: PathBuf,
    pub target_root: PathBuf,
    pub restored_files: usize,
    pub verified: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DeleteBackupResult {
    pub backup_dir: PathBuf,
    pub reclaimed_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BackupSummary {
    pub backup_dir: PathBuf,
    pub source_root: PathBuf,
    pub reason: String,
    pub created_at_ms: u128,
    pub file_count: usize,
    pub total_bytes: u64,
    pub verified: bool,
    pub complete_sessions: bool,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointCleanupSummary {
    pub attempted_count: usize,
    pub failed_count: usize,
    pub reclaimed_count: usize,
    pub reclaimed_bytes: u64,
    pub retained_count: usize,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointCleanupReceipt {
    pub operation_id: String,
    pub attempted_count: usize,
    pub failed_count: usize,
    pub reclaimed_count: usize,
    pub reclaimed_bytes: u64,
    pub retained_count: usize,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointStorageStatus {
    pub total_count: usize,
    pub total_bytes: u64,
    pub reclaimable_count: usize,
    pub reclaimable_bytes: u64,
    pub retained_count: usize,
    pub warnings: Vec<String>,
    pub last_cleanup: Option<CheckpointCleanupReceipt>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackupCapacityPreflight {
    pub required_bytes: u64,
    pub available_bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(not(test), derive(Copy))]
struct BackupSourceCapacityMetadata {
    plaintext_payload_bytes: u64,
    file_count: u64,
    sqlite_logical_bytes: u64,
    #[cfg(test)]
    relative_paths: std::collections::BTreeSet<PathBuf>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct BackupCapacitySource<'a> {
    pub home: &'a Path,
    pub paths: &'a CodexPaths,
    pub scope: BackupScope,
}

pub fn preflight_backup_capacity(
    destination_root: &Path,
    current_home: &Path,
    shared_home: &Path,
) -> Result<BackupCapacityPreflight, String> {
    preflight_scoped_backup_capacity(
        destination_root,
        current_home,
        BackupScope::Full,
        shared_home,
        BackupScope::Full,
    )
}

pub fn preflight_runtime_backup_capacity(
    destination_root: &Path,
    current_home: &Path,
    shared_home: &Path,
) -> Result<BackupCapacityPreflight, String> {
    preflight_scoped_backup_capacity(
        destination_root,
        current_home,
        BackupScope::Runtime,
        shared_home,
        BackupScope::Sessions,
    )
}

pub fn preflight_session_backup_capacity(
    destination_root: &Path,
    current_home: &Path,
    shared_home: &Path,
) -> Result<BackupCapacityPreflight, String> {
    preflight_scoped_backup_capacity(
        destination_root,
        current_home,
        BackupScope::Sessions,
        shared_home,
        BackupScope::Sessions,
    )
}

pub(crate) fn preflight_backup_capacity_with_paths(
    destination_root: &Path,
    home: &Path,
    paths: &CodexPaths,
    scope: BackupScope,
) -> Result<BackupCapacityPreflight, String> {
    preflight_backup_capacity_for_sources(
        destination_root,
        &[BackupCapacitySource { home, paths, scope }],
    )
}

fn preflight_scoped_backup_capacity(
    destination_root: &Path,
    current_home: &Path,
    current_scope: BackupScope,
    shared_home: &Path,
    shared_scope: BackupScope,
) -> Result<BackupCapacityPreflight, String> {
    let current_paths =
        resolve_user_codex_paths(current_home).map_err(|_| capacity_preflight_error())?;
    let shared_paths = local_codex_paths(shared_home);
    preflight_backup_capacity_for_sources(
        destination_root,
        &[
            BackupCapacitySource {
                home: current_home,
                paths: &current_paths,
                scope: current_scope,
            },
            BackupCapacitySource {
                home: shared_home,
                paths: &shared_paths,
                scope: shared_scope,
            },
        ],
    )
}

pub(crate) fn preflight_backup_capacity_for_sources(
    destination_root: &Path,
    sources: &[BackupCapacitySource<'_>],
) -> Result<BackupCapacityPreflight, String> {
    let destination_root = validate_absolute_root(destination_root, "backup destination root")
        .map_err(|_| capacity_preflight_error())?;
    if sources.is_empty() || validate_capacity_sources(&destination_root, sources).is_err() {
        return Err(capacity_preflight_error());
    }
    let capacity = sources
        .iter()
        .map(|source| {
            collect_backup_capacity_metadata(source.home, source.paths, source.scope)
                .map_err(|_| capacity_preflight_error())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let required_bytes = estimate_backup_peak(&capacity).map_err(|_| capacity_preflight_error())?;
    let available_bytes = available_backup_bytes(&destination_root)?;

    finish_capacity_preflight(required_bytes, available_bytes)
}

fn validate_capacity_sources(
    destination_root: &Path,
    sources: &[BackupCapacitySource<'_>],
) -> Result<(), String> {
    for source in sources {
        let (home, sqlite_home) = validate_resolved_backup_paths(source.home, source.paths)?;
        ensure_roots_disjoint(
            destination_root,
            "backup destination root",
            &home,
            "backup source root",
        )?;
        ensure_roots_disjoint(
            destination_root,
            "backup destination root",
            &sqlite_home,
            "SQLite root",
        )?;
    }
    for left_index in 0..sources.len() {
        for right_index in (left_index + 1)..sources.len() {
            let left = sources[left_index];
            let right = sources[right_index];
            for (left_root, left_label, right_root, right_label) in [
                (
                    left.home,
                    "backup source root",
                    right.home,
                    "backup source root",
                ),
                (
                    left.home,
                    "backup source root",
                    &right.paths.sqlite_home,
                    "SQLite root",
                ),
                (
                    &left.paths.sqlite_home,
                    "SQLite root",
                    right.home,
                    "backup source root",
                ),
                (
                    &left.paths.sqlite_home,
                    "SQLite root",
                    &right.paths.sqlite_home,
                    "SQLite root",
                ),
            ] {
                ensure_roots_disjoint(left_root, left_label, right_root, right_label)?;
            }
        }
    }
    Ok(())
}

fn validate_resolved_backup_paths(
    home: &Path,
    paths: &CodexPaths,
) -> Result<(PathBuf, PathBuf), String> {
    let home = validate_absolute_root(home, "backup source root")?;
    let paths_home = validate_absolute_root(&paths.codex_home, "resolved source root")?;
    let sqlite_home = validate_absolute_root(&paths.sqlite_home, "SQLite root")?;
    if home != paths_home
        || paths.state_db != sqlite_home.join(STATE_DATABASE)
        || paths.goals_db != sqlite_home.join("goals_1.sqlite")
        || paths.memories_db != sqlite_home.join("memories_1.sqlite")
        || paths.logs_db != sqlite_home.join("logs_2.sqlite")
        || paths.sessions_dir != home.join("sessions")
        || paths.archived_sessions_dir != home.join("archived_sessions")
        || paths.session_index != home.join("session_index.jsonl")
    {
        return Err("resolved backup paths do not match the source root".to_string());
    }
    Ok((home, sqlite_home))
}

pub(crate) fn ensure_roots_disjoint(
    left: &Path,
    left_label: &str,
    right: &Path,
    right_label: &str,
) -> Result<(), String> {
    let left = resolve_root_for_overlap(left)?;
    let right = resolve_root_for_overlap(right)?;
    if roots_overlap(&left, &right) {
        return Err(format!("{left_label} and {right_label} must not overlap"));
    }
    Ok(())
}

#[cfg(windows)]
fn roots_overlap(left: &Path, right: &Path) -> bool {
    let left = windows_path_components(left);
    let right = windows_path_components(right);
    left.len() <= right.len()
        && left
            .iter()
            .zip(&right)
            .all(|(left, right)| windows_components_equal(left, right))
        || right.len() <= left.len()
            && right
                .iter()
                .zip(&left)
                .all(|(right, left)| windows_components_equal(right, left))
}

#[cfg(windows)]
fn windows_path_components(path: &Path) -> Vec<std::ffi::OsString> {
    path.components()
        .map(|component| component.as_os_str().to_os_string())
        .collect()
}

#[cfg(windows)]
fn windows_components_equal(left: &std::ffi::OsStr, right: &std::ffi::OsStr) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Globalization::{CompareStringOrdinal, CSTR_EQUAL};

    let left = left.encode_wide().collect::<Vec<_>>();
    let right = right.encode_wide().collect::<Vec<_>>();
    let (Ok(left_len), Ok(right_len)) = (i32::try_from(left.len()), i32::try_from(right.len()))
    else {
        return false;
    };
    // The buffers remain alive for the duration of this ordinal Win32 comparison.
    unsafe {
        CompareStringOrdinal(left.as_ptr(), left_len, right.as_ptr(), right_len, 1) == CSTR_EQUAL
    }
}

#[cfg(not(windows))]
fn roots_overlap(left: &Path, right: &Path) -> bool {
    left == right || left.starts_with(right) || right.starts_with(left)
}

fn resolve_root_for_overlap(path: &Path) -> Result<PathBuf, String> {
    let mut existing = path;
    let mut missing = Vec::new();
    loop {
        match fs::canonicalize(existing) {
            Ok(mut resolved) => {
                for component in missing.iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = existing
                    .file_name()
                    .ok_or_else(|| format!("failed to resolve root {}: {error}", path.display()))?;
                missing.push(name.to_os_string());
                existing = existing
                    .parent()
                    .ok_or_else(|| format!("failed to resolve root {}: {error}", path.display()))?;
            }
            Err(error) => {
                return Err(format!(
                    "failed to resolve root {}: {error}",
                    path.display()
                ));
            }
        }
    }
}

fn finish_capacity_preflight(
    required_bytes: u64,
    available_bytes: u64,
) -> Result<BackupCapacityPreflight, String> {
    if available_bytes < required_bytes {
        return Err(format!(
            "insufficient backup capacity: required_bytes={required_bytes}, available_bytes={available_bytes}"
        ));
    }

    Ok(BackupCapacityPreflight {
        required_bytes,
        available_bytes,
    })
}

fn collect_backup_capacity_metadata(
    home: &Path,
    paths: &CodexPaths,
    scope: BackupScope,
) -> Result<BackupSourceCapacityMetadata, ()> {
    let mut capacity = BackupSourceCapacityMetadata::default();
    let mut sources = Vec::new();
    if scope.tracks_runtime_files() {
        sources.push((home.join("auth.json"), PathBuf::from("auth.json")));
        sources.push((home.join("config.toml"), PathBuf::from("config.toml")));
    }
    if scope.tracks_process_state() {
        if let Some((_source, bytes)) = chat_process_state_backup_source(home).map_err(|_| ())? {
            add_capacity_file_at(
                &mut capacity,
                bytes,
                PathBuf::from(CHAT_PROCESS_STATE_RELATIVE_PATH),
            )?;
        }
    }
    if scope.tracks_sessions() {
        sources.push((
            paths.session_index.clone(),
            PathBuf::from("session_index.jsonl"),
        ));
    }
    for (source, relative) in sources {
        if let Some(bytes) = regular_file_len(&source)? {
            add_capacity_file_at(&mut capacity, bytes, relative)?;
        }
    }

    for (database_name, database) in managed_sqlite_paths(paths) {
        if !scope.tracked_databases().contains(&database_name) {
            continue;
        }
        if regular_file_len(database)?.is_some() {
            let logical_bytes = sqlite_logical_bytes(database)?;
            add_capacity_file_at(&mut capacity, logical_bytes, PathBuf::from(database_name))?;
            capacity.sqlite_logical_bytes = capacity.sqlite_logical_bytes.max(logical_bytes);
        }
    }

    if !scope.tracks_sessions() {
        return Ok(capacity);
    }
    let mut session_roots = vec![&paths.sessions_dir];
    if scope.tracks_archived_sessions() {
        session_roots.push(&paths.archived_sessions_dir);
    }
    for session_root in session_roots {
        match fs::metadata(session_root) {
            Ok(metadata) if metadata.is_dir() => {
                for path in walk_jsonl_files(session_root).map_err(|_| ())? {
                    let bytes = regular_file_len(&path)?.ok_or(())?;
                    add_capacity_file(&mut capacity, bytes)?;
                    #[cfg(test)]
                    capacity
                        .relative_paths
                        .insert(path.strip_prefix(home).map_err(|_| ())?.to_path_buf());
                }
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(()),
        }
    }

    Ok(capacity)
}

fn add_capacity_file_at(
    capacity: &mut BackupSourceCapacityMetadata,
    plaintext_bytes: u64,
    relative_path: PathBuf,
) -> Result<(), ()> {
    add_capacity_file(capacity, plaintext_bytes)?;
    #[cfg(test)]
    capacity.relative_paths.insert(relative_path);
    #[cfg(not(test))]
    let _ = relative_path;
    Ok(())
}

fn regular_file_len(path: &Path) -> Result<Option<u64>, ()> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(Some(metadata.len())),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(()),
    }
}

fn add_capacity_file(
    capacity: &mut BackupSourceCapacityMetadata,
    plaintext_bytes: u64,
) -> Result<(), ()> {
    ensure_encryptable_payload_size(plaintext_bytes)?;
    capacity.plaintext_payload_bytes = capacity
        .plaintext_payload_bytes
        .checked_add(plaintext_bytes)
        .ok_or(())?;
    capacity.file_count = capacity.file_count.checked_add(1).ok_or(())?;
    Ok(())
}

fn ensure_encryptable_payload_size(bytes: u64) -> Result<(), ()> {
    if bytes > MAX_DPAPI_PAYLOAD_BYTES {
        Err(())
    } else {
        Ok(())
    }
}

fn sqlite_logical_bytes(database: &Path) -> Result<u64, ()> {
    let connection =
        Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|_| ())?;
    let page_size: i64 = connection
        .query_row("PRAGMA page_size", [], |row| row.get(0))
        .map_err(|_| ())?;
    let page_count: i64 = connection
        .query_row("PRAGMA page_count", [], |row| row.get(0))
        .map_err(|_| ())?;
    let page_size = u64::try_from(page_size).map_err(|_| ())?;
    let page_count = u64::try_from(page_count).map_err(|_| ())?;
    page_count.checked_mul(page_size).ok_or(())
}

fn estimate_backup_peak(sources: &[BackupSourceCapacityMetadata]) -> Result<u64, ()> {
    let source_count = u64::try_from(sources.len()).map_err(|_| ())?;
    estimate_backup_peak_with_source_count(sources, source_count)
}

fn estimate_backup_peak_with_source_count(
    sources: &[BackupSourceCapacityMetadata],
    source_count: u64,
) -> Result<u64, ()> {
    let peak_without_reserve =
        estimate_backup_peak_without_reserve_with_source_count(sources, source_count)?;
    required_capacity_with_reserve(peak_without_reserve)
}

fn estimate_backup_peak_without_reserve_with_source_count(
    sources: &[BackupSourceCapacityMetadata],
    source_count: u64,
) -> Result<u64, ()> {
    if sources.is_empty() || source_count == 0 {
        return Err(());
    }
    let plaintext_payload_bytes = sources.iter().try_fold(0_u64, |total, source| {
        total.checked_add(source.plaintext_payload_bytes).ok_or(())
    })?;
    let file_count = sources.iter().try_fold(0_u64, |total, source| {
        total.checked_add(source.file_count).ok_or(())
    })?;
    let encrypted_payload_overhead = file_count
        .checked_mul(BACKUP_FILE_OVERHEAD_BYTES)
        .ok_or(())?;
    let manifest_overhead = source_count
        .checked_mul(MANIFEST_BASE_OVERHEAD_BYTES)
        .and_then(|base| {
            file_count
                .checked_mul(MANIFEST_ENTRY_OVERHEAD_BYTES)
                .and_then(|entries| base.checked_add(entries))
        })
        .ok_or(())?;
    let sqlite_workspace = sources
        .iter()
        .map(|source| source.sqlite_logical_bytes)
        .max()
        .ok_or(())?;
    let peak_without_reserve = plaintext_payload_bytes
        .checked_add(encrypted_payload_overhead)
        .and_then(|value| value.checked_add(manifest_overhead))
        .and_then(|value| value.checked_add(sqlite_workspace))
        .ok_or(())?;
    Ok(peak_without_reserve)
}

fn required_capacity_with_reserve(peak_without_reserve: u64) -> Result<u64, ()> {
    let percentage_reserve = percentage_ceil(peak_without_reserve, CAPACITY_RESERVE_PERCENT)?;
    let reserve = MIN_CAPACITY_RESERVE_BYTES.max(percentage_reserve);
    peak_without_reserve.checked_add(reserve).ok_or(())
}

#[cfg(test)]
fn existing_capacity_ancestor(path: &Path) -> Result<PathBuf, String> {
    let mut candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        return Err(capacity_preflight_error());
    };
    loop {
        match fs::metadata(&candidate) {
            Ok(metadata) => {
                let resolved =
                    fs::canonicalize(&candidate).map_err(|_| capacity_preflight_error())?;
                if metadata.is_dir() {
                    return Ok(resolved);
                }
                return resolved
                    .parent()
                    .map(Path::to_path_buf)
                    .ok_or_else(capacity_preflight_error);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if !candidate.pop() {
                    return Err(capacity_preflight_error());
                }
            }
            Err(_) => return Err(capacity_preflight_error()),
        }
    }
}

fn percentage_ceil(value: u64, percent: u64) -> Result<u64, ()> {
    let numerator = u128::from(value)
        .checked_mul(u128::from(percent))
        .and_then(|product| product.checked_add(99))
        .ok_or(())?;
    u64::try_from(numerator / 100).map_err(|_| ())
}

fn capacity_preflight_error() -> String {
    "backup capacity preflight failed".to_string()
}

#[cfg(windows)]
fn available_backup_bytes(destination_root: &Path) -> Result<u64, String> {
    use std::{os::windows::ffi::OsStrExt, ptr::null_mut};

    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let mut query_path = if destination_root.is_absolute() {
        destination_root.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|_| capacity_preflight_error())?
            .join(destination_root)
    };
    loop {
        match fs::metadata(&query_path) {
            Ok(metadata) if metadata.is_dir() => break,
            Ok(_) => return Err(capacity_preflight_error()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if !query_path.pop() {
                    return Err(capacity_preflight_error());
                }
            }
            Err(_) => return Err(capacity_preflight_error()),
        }
    }
    let query_path = fs::canonicalize(query_path).map_err(|_| capacity_preflight_error())?;
    let wide = query_path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut available_bytes = 0_u64;
    let result =
        unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut available_bytes, null_mut(), null_mut()) };
    if result == 0 {
        return Err(capacity_preflight_error());
    }
    Ok(available_bytes)
}

#[cfg(not(windows))]
fn available_backup_bytes(_destination_root: &Path) -> Result<u64, String> {
    Err("backup capacity preflight is unsupported on this platform".to_string())
}

pub fn create_backup(
    home: &Path,
    destination_root: &Path,
    reason: &str,
) -> Result<BackupManifest, String> {
    let paths = resolve_user_codex_paths(home)?;
    create_scoped_backup_with_paths(
        home,
        destination_root,
        reason,
        paths,
        BackupScope::Full,
        None,
    )
}

pub fn create_local_backup(
    home: &Path,
    destination_root: &Path,
    reason: &str,
) -> Result<BackupManifest, String> {
    create_scoped_backup_with_paths(
        home,
        destination_root,
        reason,
        local_codex_paths(home),
        BackupScope::Full,
        None,
    )
}

pub fn create_runtime_backup(
    home: &Path,
    destination_root: &Path,
    reason: &str,
) -> Result<BackupManifest, String> {
    let paths = resolve_user_codex_paths(home)?;
    create_runtime_backup_with_paths(home, destination_root, reason, paths)
}

pub(crate) fn create_runtime_backup_with_paths(
    home: &Path,
    destination_root: &Path,
    reason: &str,
    paths: CodexPaths,
) -> Result<BackupManifest, String> {
    create_scoped_backup_with_paths(
        home,
        destination_root,
        reason,
        paths,
        BackupScope::Runtime,
        None,
    )
}

#[cfg(test)]
pub(crate) fn create_runtime_state_backup_with_paths(
    home: &Path,
    destination_root: &Path,
    reason: &str,
    paths: CodexPaths,
) -> Result<BackupManifest, String> {
    create_scoped_backup_with_paths(
        home,
        destination_root,
        reason,
        paths,
        BackupScope::RuntimeState,
        None,
    )
}

#[cfg(test)]
pub(crate) fn create_runtime_state_checkpoint_with_paths(
    home: &Path,
    destination_root: &Path,
    reason: &str,
    paths: CodexPaths,
    operation_id: &str,
    role: CheckpointRole,
) -> Result<BackupManifest, String> {
    create_scoped_backup_with_paths(
        home,
        destination_root,
        reason,
        paths,
        BackupScope::RuntimeState,
        Some(checkpoint_binding(operation_id, role)?),
    )
}

pub fn create_session_backup(
    home: &Path,
    destination_root: &Path,
    reason: &str,
) -> Result<BackupManifest, String> {
    let paths = resolve_user_codex_paths(home)?;
    create_session_backup_with_paths(home, destination_root, reason, paths)
}

pub(crate) fn create_session_backup_with_paths(
    home: &Path,
    destination_root: &Path,
    reason: &str,
    paths: CodexPaths,
) -> Result<BackupManifest, String> {
    create_scoped_backup_with_paths(
        home,
        destination_root,
        reason,
        paths,
        BackupScope::Sessions,
        None,
    )
}

pub fn create_local_session_backup(
    home: &Path,
    destination_root: &Path,
    reason: &str,
) -> Result<BackupManifest, String> {
    create_scoped_backup_with_paths(
        home,
        destination_root,
        reason,
        local_codex_paths(home),
        BackupScope::Sessions,
        None,
    )
}

pub fn create_state_backup(
    home: &Path,
    destination_root: &Path,
    reason: &str,
) -> Result<BackupManifest, String> {
    let paths = resolve_user_codex_paths(home)?;
    create_scoped_backup_with_paths(
        home,
        destination_root,
        reason,
        paths,
        BackupScope::StateOnly,
        None,
    )
}

pub(crate) fn create_backup_with_paths(
    home: &Path,
    destination_root: &Path,
    reason: &str,
    paths: CodexPaths,
) -> Result<BackupManifest, String> {
    create_scoped_backup_with_paths(
        home,
        destination_root,
        reason,
        paths,
        BackupScope::Full,
        None,
    )
}

pub(crate) fn create_state_checkpoint_with_paths(
    home: &Path,
    destination_root: &Path,
    reason: &str,
    paths: CodexPaths,
    operation_id: &str,
    role: CheckpointRole,
) -> Result<BackupManifest, String> {
    create_scoped_backup_with_paths(
        home,
        destination_root,
        reason,
        paths,
        BackupScope::StateOnly,
        Some(checkpoint_binding(operation_id, role)?),
    )
}

fn checkpoint_binding(
    operation_id: &str,
    role: CheckpointRole,
) -> Result<CheckpointBinding<'_>, String> {
    if operation_id.trim().is_empty() {
        return Err("checkpoint operation ID is required".to_string());
    }
    Ok(CheckpointBinding { operation_id, role })
}

fn create_scoped_backup_with_paths(
    home: &Path,
    destination_root: &Path,
    reason: &str,
    paths: CodexPaths,
    scope: BackupScope,
    binding: Option<CheckpointBinding<'_>>,
) -> Result<BackupManifest, String> {
    let (home, sqlite_home) = validate_resolved_backup_paths(home, &paths)?;
    let destination_root = validate_absolute_root(destination_root, "backup destination root")?;
    ensure_roots_disjoint(
        &home,
        "backup source root",
        &destination_root,
        "backup destination root",
    )?;
    ensure_roots_disjoint(
        &sqlite_home,
        "SQLite root",
        &destination_root,
        "backup destination root",
    )?;
    let created_at_ms = timestamp_millis()?;
    let backup_dir = destination_root.join(format!(
        "{}-{}-{}-{}",
        created_at_ms,
        std::process::id(),
        BACKUP_COUNTER.fetch_add(1, Ordering::Relaxed),
        safe_reason(reason)
    ));
    fs::create_dir_all(&backup_dir)
        .map_err(|error| format!("failed to create backup dir: {error}"))?;

    let result = create_backup_in_dir(
        &home,
        &backup_dir,
        reason,
        created_at_ms,
        paths,
        scope,
        binding,
    );
    finish_backup_creation_with_cleanup(&backup_dir, result, |path| fs::remove_dir_all(path))
}

fn finish_backup_creation_with_cleanup<Cleanup>(
    backup_dir: &Path,
    result: Result<BackupManifest, String>,
    cleanup: Cleanup,
) -> Result<BackupManifest, String>
where
    Cleanup: FnOnce(&Path) -> std::io::Result<()>,
{
    match result {
        Ok(manifest) => Ok(manifest),
        Err(backup_error) => match cleanup(backup_dir) {
            Ok(()) => Err(backup_error),
            Err(cleanup_error) => Err(format!(
                "{backup_error}; incomplete_dir={}: cleanup failed: {cleanup_error}",
                backup_dir.display()
            )),
        },
    }
}

pub fn delete_verified_full_backup(
    destination_root: &Path,
    backup_dir: &Path,
) -> Result<DeleteBackupResult, String> {
    let initial = verify_managed_full_backup(destination_root, backup_dir)?;
    let rechecked = verify_managed_full_backup(destination_root, backup_dir)?;
    if rechecked.manifest != initial.manifest
        || rechecked.reclaimed_bytes != initial.reclaimed_bytes
        || rechecked.directory != initial.directory
    {
        return Err("backup changed during deletion verification".to_string());
    }
    fs::remove_dir_all(&initial.directory)
        .map_err(|error| format!("failed to remove verified full backup: {error}"))?;
    Ok(DeleteBackupResult {
        backup_dir: initial.backup_dir,
        reclaimed_bytes: initial.reclaimed_bytes,
    })
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackupRestoreRecoveryReceipt {
    pub discovered_operation_count: usize,
    pub rolled_back_operation_count: usize,
    pub committed_cleanup_count: usize,
    pub already_terminal_count: usize,
    pub blocked_operation_count: usize,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum BackupRestoreMutationKind {
    Create,
    Replace,
    Delete,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum BackupRestoreOperationPhase {
    Planned,
    Applying,
    Validating,
    Committing,
    Committed,
    RollingBack,
    RolledBack,
    CommittedCleanupComplete,
    RolledBackCleanupComplete,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum BackupRestoreMutationPhase {
    Planned,
    WitnessCreating,
    WitnessReady,
    Preparing,
    Prepared,
    Publishing,
    Published,
    CommittedWithRecovery,
    RollbackPreparing,
    RollbackPrepared,
    RolledBack,
    Cleaned,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BackupRestoreArtifactPaths {
    original_witness_path: PathBuf,
    replacement_witness_path: PathBuf,
    staging_path: PathBuf,
    recovery_path: PathBuf,
    rollback_tombstone_path: PathBuf,
}

impl BackupRestoreArtifactPaths {
    fn replace_paths(&self, target_path: &Path) -> Result<HandleReplacePaths, String> {
        HandleReplacePaths::from_persisted_plan(
            target_path.to_path_buf(),
            self.recovery_path.clone(),
            self.staging_path.clone(),
            self.rollback_tombstone_path.clone(),
        )
    }

    fn create_paths(&self, target_path: &Path) -> Result<HandleCreatePaths, String> {
        HandleCreatePaths::from_persisted_plan(
            target_path.to_path_buf(),
            self.staging_path.clone(),
            self.rollback_tombstone_path.clone(),
        )
    }

    fn delete_paths(&self, target_path: &Path) -> Result<HandleDeletePaths, String> {
        HandleDeletePaths::from_persisted_plan(
            target_path.to_path_buf(),
            self.recovery_path.clone(),
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BackupRestoreMutationPlan {
    kind: BackupRestoreMutationKind,
    logical_path: PathBuf,
    target_path: PathBuf,
    original_sha256: Option<String>,
    replacement_sha256: Option<String>,
    artifacts: BackupRestoreArtifactPaths,
    sqlite: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BackupRestoreMutationState {
    phase: BackupRestoreMutationPhase,
    parent_identity: Option<RegularFileIdentity>,
    original_identity: Option<RegularFileIdentity>,
    replacement_identity: Option<RegularFileIdentity>,
}

impl BackupRestoreMutationState {
    fn replace_bindings(&self) -> Result<HandleReplaceIdentityBindings, String> {
        Ok(HandleReplaceIdentityBindings {
            parent_identity: self
                .parent_identity
                .ok_or_else(|| "backup restore parent identity is missing".to_string())?,
            original_identity: self
                .original_identity
                .ok_or_else(|| "backup restore original identity is missing".to_string())?,
            replacement_identity: self
                .replacement_identity
                .ok_or_else(|| "backup restore replacement identity is missing".to_string())?,
        })
    }

    fn create_bindings(&self) -> Result<HandleCreateIdentityBindings, String> {
        Ok(HandleCreateIdentityBindings {
            parent_identity: self
                .parent_identity
                .ok_or_else(|| "backup restore parent identity is missing".to_string())?,
            created_identity: self
                .replacement_identity
                .ok_or_else(|| "backup restore replacement identity is missing".to_string())?,
        })
    }

    fn delete_bindings(&self) -> Result<HandleDeleteIdentityBindings, String> {
        Ok(HandleDeleteIdentityBindings {
            parent_identity: self
                .parent_identity
                .ok_or_else(|| "backup restore parent identity is missing".to_string())?,
            deleted_identity: self
                .original_identity
                .ok_or_else(|| "backup restore original identity is missing".to_string())?,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BackupRestorePlan {
    operation_id: String,
    created_at_ms: u128,
    backup_dir: PathBuf,
    backup_manifest_sha256: String,
    target_root: PathBuf,
    allowed_roots: Vec<PathBuf>,
    restored_file_count: usize,
    mutations: Vec<BackupRestoreMutationPlan>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BackupRestoreJournal {
    schema_version: u32,
    revision: u64,
    updated_at_ms: u128,
    phase: BackupRestoreOperationPhase,
    plan_integrity_sha256: String,
    plan: BackupRestorePlan,
    mutation_states: Vec<BackupRestoreMutationState>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BackupRestoreJournalEnvelope {
    journal: BackupRestoreJournal,
    integrity_sha256: String,
}

#[derive(Debug, Clone)]
struct BackupRestorePreparedPlan {
    manifest: BackupManifest,
    journal: BackupRestoreJournal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CorruptManagedBackupSnapshot {
    pub manifest: BackupManifest,
    pub tree_sha256: String,
    pub reclaimed_bytes: u64,
}

pub(crate) fn verified_full_backup_reclaim_bytes(
    destination_root: &Path,
    backup_dir: &Path,
) -> Result<u64, String> {
    Ok(verify_managed_full_backup(destination_root, backup_dir)?.reclaimed_bytes)
}

pub(crate) fn inspect_corrupt_managed_full_backup(
    destination_root: &Path,
    backup_dir: &Path,
) -> Result<CorruptManagedBackupSnapshot, String> {
    let destination_root = validate_absolute_root(destination_root, "backup destination root")?;
    let backup_dir = validate_absolute_root(backup_dir, "backup directory")?;
    let root = fs::canonicalize(&destination_root)
        .map_err(|_| "failed to resolve backup destination root".to_string())?;
    let directory = fs::canonicalize(&backup_dir)
        .map_err(|_| "failed to resolve corrupt backup directory".to_string())?;
    if directory.parent() != Some(root.as_path()) {
        return Err("corrupt backup directory is outside the managed root".to_string());
    }
    let manifest = read_backup_manifest(&directory)?;
    if manifest.backup_dir != backup_dir
        || fs::canonicalize(&manifest.backup_dir)
            .map_err(|_| "failed to resolve corrupt backup manifest directory".to_string())?
            != directory
        || !managed_backup_directory_name_matches(&directory, &manifest)
    {
        return Err("corrupt backup identity is not managed".to_string());
    }
    if verify_backup(&directory).is_ok() {
        return Err("backup is still fully verifiable".to_string());
    }

    let mut allowed_files = BTreeSet::from([backup_path_key(&directory.join("manifest.json"))]);
    let mut allowed_directories = BTreeSet::from([backup_path_key(&directory)]);
    for file in &manifest.files {
        validate_relative_path(&file.relative_path)?;
        let expected = encrypted_payload_path(&manifest.backup_dir, &file.relative_path)?;
        if file.backup_path != expected {
            return Err("corrupt backup payload path is not managed".to_string());
        }
        let relative = expected
            .strip_prefix(&manifest.backup_dir)
            .map_err(|_| "corrupt backup payload escaped its directory".to_string())?;
        let managed_path = directory.join(relative);
        allowed_files.insert(backup_path_key(&managed_path));
        let mut parent = managed_path.parent();
        while let Some(path) = parent {
            if !path.starts_with(&directory) {
                break;
            }
            allowed_directories.insert(backup_path_key(path));
            if path == directory {
                break;
            }
            parent = path.parent();
        }
    }

    let mut observed_files = Vec::new();
    let mut pending = vec![directory.clone()];
    while let Some(current) = pending.pop() {
        for entry in fs::read_dir(&current)
            .map_err(|_| "corrupt backup directory is unreadable".to_string())?
        {
            let entry = entry.map_err(|_| "corrupt backup entry is unreadable".to_string())?;
            let metadata = fs::symlink_metadata(entry.path())
                .map_err(|_| "corrupt backup entry is unreadable".to_string())?;
            if backup_metadata_is_link_or_reparse(&metadata) {
                return Err("corrupt backup contains a link".to_string());
            }
            let key = backup_path_key(&entry.path());
            if metadata.is_dir() {
                if !allowed_directories.contains(&key) {
                    return Err("corrupt backup contains an undeclared directory".to_string());
                }
                pending.push(entry.path());
            } else if metadata.is_file() {
                if !allowed_files.contains(&key) {
                    return Err("corrupt backup contains an undeclared file".to_string());
                }
                observed_files.push((key, metadata.len(), sha256_file(&entry.path())?));
            } else {
                return Err("corrupt backup contains an unsupported entry".to_string());
            }
        }
    }
    if !observed_files
        .iter()
        .any(|(path, _, _)| path == &backup_path_key(&directory.join("manifest.json")))
    {
        return Err("corrupt backup manifest disappeared".to_string());
    }
    observed_files.sort_by(|left, right| left.0.cmp(&right.0));
    let reclaimed_bytes = observed_files.iter().try_fold(0_u64, |total, entry| {
        total
            .checked_add(entry.1)
            .ok_or_else(|| "corrupt backup size overflowed".to_string())
    })?;
    let tree_sha256 = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&observed_files)
                .map_err(|_| "failed to encode corrupt backup inventory".to_string())?
        )
    );
    Ok(CorruptManagedBackupSnapshot {
        manifest,
        tree_sha256,
        reclaimed_bytes,
    })
}

#[derive(Debug)]
struct VerifiedFullBackup {
    backup_dir: PathBuf,
    directory: PathBuf,
    manifest: BackupManifest,
    reclaimed_bytes: u64,
}

fn verify_managed_full_backup(
    destination_root: &Path,
    backup_dir: &Path,
) -> Result<VerifiedFullBackup, String> {
    let destination_root = validate_absolute_root(destination_root, "backup destination root")?;
    let backup_dir = validate_absolute_root(backup_dir, "backup directory")?;
    let root_metadata = fs::symlink_metadata(&destination_root)
        .map_err(|error| format!("failed to inspect backup destination root: {error}"))?;
    validate_directory_entry(
        root_metadata.is_dir(),
        root_metadata.file_type().is_symlink(),
        "backup destination root",
    )?;
    let backup_metadata = fs::symlink_metadata(&backup_dir)
        .map_err(|error| format!("failed to inspect backup directory: {error}"))?;
    validate_directory_entry(
        backup_metadata.is_dir(),
        backup_metadata.file_type().is_symlink(),
        "backup directory",
    )?;
    let root = fs::canonicalize(&destination_root)
        .map_err(|error| format!("failed to resolve backup destination root: {error}"))?;
    let directory = fs::canonicalize(&backup_dir)
        .map_err(|error| format!("failed to resolve backup directory: {error}"))?;
    if directory.parent() != Some(root.as_path()) {
        return Err("backup directory is outside the managed backup root".to_string());
    }
    let manifest = verify_backup(&directory)?;
    if manifest.backup_dir != backup_dir {
        return Err("backup manifest directory does not match the requested directory".to_string());
    }
    let manifest_directory = fs::canonicalize(&manifest.backup_dir)
        .map_err(|error| format!("failed to resolve backup manifest directory: {error}"))?;
    if manifest_directory != directory {
        return Err("backup manifest directory does not match its container".to_string());
    }
    let is_full = manifest.version == 2
        || (matches!(
            manifest.version,
            SCOPED_BACKUP_MANIFEST_VERSION | BACKUP_MANIFEST_VERSION
        ) && manifest.scope == BackupScope::Full);
    if !is_full {
        return Err("only persistent full backups can be deleted".to_string());
    }
    let reclaimed_bytes = managed_checkpoint_directory_size(&directory, &manifest)?;
    Ok(VerifiedFullBackup {
        backup_dir,
        directory,
        manifest,
        reclaimed_bytes,
    })
}

fn validate_directory_entry(
    is_directory: bool,
    is_symlink: bool,
    label: &str,
) -> Result<(), String> {
    if !is_directory || is_symlink {
        Err(format!("{label} is not a regular directory"))
    } else {
        Ok(())
    }
}

pub(crate) fn cleanup_transient_checkpoints(
    destination_root: &Path,
    terminal_record: &OperationRecord,
    manifests: &[BackupManifest],
) -> CheckpointCleanupSummary {
    let transient = manifests
        .iter()
        .filter(|manifest| is_transient_checkpoint(manifest))
        .collect::<Vec<_>>();
    let mut summary = CheckpointCleanupSummary {
        attempted_count: transient.len(),
        retained_count: manifests.len(),
        ..CheckpointCleanupSummary::default()
    };
    if transient.is_empty() {
        return summary;
    }
    let root = match fs::canonicalize(destination_root) {
        Ok(root) => root,
        Err(_) => {
            summary.failed_count = summary.attempted_count;
            summary
                .warnings
                .push("automatic checkpoints could not be resolved and were retained".to_string());
            return summary;
        }
    };
    let selected = (|| {
        if !is_automatic_checkpoint_terminal(terminal_record)
            || terminal_record.operation_id.trim().is_empty()
            || !automatic_checkpoint_count_matches(terminal_record, transient.len())
            || terminal_record.backup_dirs.len() != transient.len()
        {
            return Err("terminal record does not match the checkpoint set".to_string());
        }
        let mut selected = Vec::with_capacity(transient.len());
        for manifest in &transient {
            if terminal_record
                .backup_dirs
                .iter()
                .filter(|path| *path == &manifest.backup_dir)
                .count()
                != 1
            {
                return Err("terminal record does not reference the checkpoint exactly".to_string());
            }
            let metadata = fs::symlink_metadata(&manifest.backup_dir)
                .map_err(|_| "checkpoint directory is unavailable".to_string())?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err("checkpoint directory is not a regular directory".to_string());
            }
            let directory = fs::canonicalize(&manifest.backup_dir)
                .map_err(|_| "checkpoint directory could not be resolved".to_string())?;
            if directory.parent() != Some(root.as_path()) {
                return Err("checkpoint directory is outside the managed backup root".to_string());
            }
            let mut stored = read_managed_checkpoint(&directory)?;
            if stored.raw_version != BACKUP_MANIFEST_VERSION || stored.manifest != **manifest {
                return Err("checkpoint manifest changed after creation".to_string());
            }
            stored.bytes = managed_checkpoint_directory_size(&directory, &stored.manifest)?;
            selected.push(stored);
        }
        let selected_refs = selected.iter().collect::<Vec<_>>();
        if !checkpoint_selection_matches(terminal_record, &selected_refs) {
            return Err("checkpoint binding does not match the terminal record".to_string());
        }
        Ok(selected)
    })();
    let selected = match selected {
        Ok(selected) => selected,
        Err(error) => {
            summary.failed_count = transient.len();
            summary
                .warnings
                .push(format!("automatic checkpoints were retained: {error}"));
            return summary;
        }
    };

    for checkpoint in selected {
        let cleanup = revalidate_managed_checkpoint(&root, &checkpoint).and_then(|current| {
            if current.raw_version != checkpoint.raw_version
                || current.manifest != checkpoint.manifest
                || current.bytes != checkpoint.bytes
            {
                return Err("checkpoint changed during cleanup".to_string());
            }
            fs::remove_dir_all(&current.path)
                .map_err(|error| format!("failed to remove checkpoint directory: {error}"))?;
            Ok(current.bytes)
        });
        match cleanup {
            Ok(bytes) => {
                summary.reclaimed_count += 1;
                summary.reclaimed_bytes = summary.reclaimed_bytes.saturating_add(bytes);
                summary.retained_count = summary.retained_count.saturating_sub(1);
            }
            Err(error) => {
                summary.failed_count += 1;
                summary
                    .warnings
                    .push(format!("automatic checkpoint was retained: {error}"));
            }
        }
    }
    summary
}

fn is_transient_checkpoint(manifest: &BackupManifest) -> bool {
    manifest.version == BACKUP_MANIFEST_VERSION
        && manifest
            .operation_id
            .as_deref()
            .is_some_and(|operation_id| !operation_id.trim().is_empty())
        && matches!(
            (&*manifest.reason, manifest.scope, manifest.role),
            (
                "switch-runtime-current",
                BackupScope::RuntimeState,
                Some(CheckpointRole::Current)
            ) | (
                "switch-runtime-shared",
                BackupScope::StateOnly,
                Some(CheckpointRole::Shared)
            ) | (
                "incremental-current",
                BackupScope::StateOnly,
                Some(CheckpointRole::Current)
            ) | (
                "incremental-shared",
                BackupScope::StateOnly,
                Some(CheckpointRole::Shared)
            ) | (
                "sync-current",
                BackupScope::StateOnly,
                Some(CheckpointRole::Current)
            ) | (
                "sync-shared",
                BackupScope::StateOnly,
                Some(CheckpointRole::Shared)
            ) | (
                "restore-sessions-visible",
                BackupScope::StateOnly,
                Some(CheckpointRole::Visibility)
            )
        )
}

fn directory_size_without_links(root: &Path) -> Result<u64, String> {
    let mut total = 0_u64;
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)
            .map_err(|error| format!("failed to inspect checkpoint directory: {error}"))?
        {
            let entry =
                entry.map_err(|error| format!("failed to inspect checkpoint entry: {error}"))?;
            let metadata = fs::symlink_metadata(entry.path())
                .map_err(|error| format!("failed to inspect checkpoint entry: {error}"))?;
            if metadata.file_type().is_symlink() {
                return Err("checkpoint contains a link".to_string());
            }
            if metadata.is_dir() {
                pending.push(entry.path());
            } else if metadata.is_file() {
                total = total
                    .checked_add(metadata.len())
                    .ok_or_else(|| "checkpoint size overflowed".to_string())?;
            } else {
                return Err("checkpoint contains an unsupported entry".to_string());
            }
        }
    }
    Ok(total)
}

fn managed_checkpoint_directory_size(
    root: &Path,
    manifest: &BackupManifest,
) -> Result<u64, String> {
    let root = fs::canonicalize(root)
        .map_err(|error| format!("failed to resolve checkpoint directory: {error}"))?;
    let mut allowed_files = HashSet::new();
    let mut allowed_directories = HashSet::from([root.clone()]);
    for path in std::iter::once(root.join("manifest.json"))
        .chain(manifest.files.iter().map(|file| file.backup_path.clone()))
    {
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("checkpoint payload is unavailable: {error}"))?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err("checkpoint payload is not a regular file".to_string());
        }
        let path = fs::canonicalize(path)
            .map_err(|error| format!("failed to resolve checkpoint payload: {error}"))?;
        if !path.starts_with(&root) {
            return Err("checkpoint payload escaped the managed directory".to_string());
        }
        let mut parent = path.parent();
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
        allowed_files.insert(path);
    }

    let mut total = 0_u64;
    let mut observed_files = HashSet::new();
    let mut pending = vec![root.clone()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)
            .map_err(|error| format!("failed to inspect checkpoint directory: {error}"))?
        {
            let entry =
                entry.map_err(|error| format!("failed to inspect checkpoint entry: {error}"))?;
            let metadata = fs::symlink_metadata(entry.path())
                .map_err(|error| format!("failed to inspect checkpoint entry: {error}"))?;
            if metadata.file_type().is_symlink() {
                return Err("checkpoint contains a link".to_string());
            }
            let path = fs::canonicalize(entry.path())
                .map_err(|error| format!("failed to resolve checkpoint entry: {error}"))?;
            if metadata.is_dir() {
                if !allowed_directories.contains(&path) {
                    return Err("checkpoint contains an undeclared directory".to_string());
                }
                pending.push(path);
            } else if metadata.is_file() {
                if !allowed_files.contains(&path) {
                    return Err("checkpoint contains an undeclared file".to_string());
                }
                observed_files.insert(path);
                total = total
                    .checked_add(metadata.len())
                    .ok_or_else(|| "checkpoint size overflowed".to_string())?;
            } else {
                return Err("checkpoint contains an unsupported entry".to_string());
            }
        }
    }
    if observed_files != allowed_files {
        return Err("checkpoint payload set changed during inspection".to_string());
    }
    Ok(total)
}

#[derive(Debug, Clone)]
struct ManagedCheckpointDirectory {
    path: PathBuf,
    bytes: u64,
    raw_version: u32,
    manifest: BackupManifest,
}

#[derive(Debug, Clone)]
struct CheckpointCleanupPlan {
    status: CheckpointStorageStatus,
    reclaimable: Vec<ManagedCheckpointDirectory>,
}

pub fn inspect_checkpoint_storage(
    destination_root: &Path,
    records: &[OperationRecord],
) -> Result<CheckpointStorageStatus, String> {
    Ok(plan_checkpoint_cleanup(destination_root, records)?.status)
}

pub fn cleanup_automatic_checkpoints(
    destination_root: &Path,
    records: &[OperationRecord],
) -> Result<CheckpointCleanupSummary, String> {
    cleanup_automatic_checkpoints_with_remove(destination_root, records, |path| {
        fs::remove_dir_all(path)
    })
}

fn cleanup_automatic_checkpoints_with_remove<Remove>(
    destination_root: &Path,
    records: &[OperationRecord],
    mut remove_checkpoint: Remove,
) -> Result<CheckpointCleanupSummary, String>
where
    Remove: FnMut(&Path) -> std::io::Result<()>,
{
    let plan = plan_checkpoint_cleanup(destination_root, records)?;
    let attempted_count = plan.reclaimable.len();
    let root = if plan.reclaimable.is_empty() {
        None
    } else {
        Some(
            fs::canonicalize(destination_root)
                .map_err(|error| format!("failed to resolve backup root: {error}"))?,
        )
    };
    let mut summary = CheckpointCleanupSummary {
        attempted_count,
        retained_count: plan.status.retained_count,
        warnings: plan.status.warnings.clone(),
        ..CheckpointCleanupSummary::default()
    };
    for checkpoint in plan.reclaimable {
        let current = match revalidate_managed_checkpoint(
            root.as_deref()
                .ok_or_else(|| "backup root is unavailable during cleanup".to_string())?,
            &checkpoint,
        ) {
            Ok(current)
                if current.raw_version == checkpoint.raw_version
                    && current.manifest == checkpoint.manifest
                    && current.bytes == checkpoint.bytes =>
            {
                current
            }
            _ => {
                summary.failed_count += 1;
                summary.retained_count += 1;
                summary.warnings.push(
                    "an automatic checkpoint changed during cleanup and was retained".to_string(),
                );
                continue;
            }
        };
        match remove_checkpoint(&current.path) {
            Ok(()) => {
                summary.reclaimed_count += 1;
                summary.reclaimed_bytes = summary
                    .reclaimed_bytes
                    .checked_add(current.bytes)
                    .ok_or_else(|| "checkpoint cleanup byte count overflowed".to_string())?;
            }
            Err(_) => {
                summary.failed_count += 1;
                summary.retained_count += 1;
                summary.warnings.push(
                    "an automatic checkpoint could not be removed and was retained".to_string(),
                );
            }
        }
    }
    Ok(summary)
}

fn revalidate_managed_checkpoint(
    root: &Path,
    checkpoint: &ManagedCheckpointDirectory,
) -> Result<ManagedCheckpointDirectory, String> {
    let metadata = fs::symlink_metadata(&checkpoint.path)
        .map_err(|error| format!("failed to inspect checkpoint directory: {error}"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("checkpoint directory is not a regular directory".to_string());
    }
    let path = fs::canonicalize(&checkpoint.path)
        .map_err(|error| format!("failed to resolve checkpoint directory: {error}"))?;
    if path != checkpoint.path || path.parent() != Some(root) {
        return Err("checkpoint directory changed or escaped the managed backup root".to_string());
    }
    let mut current = read_managed_checkpoint(&path)?;
    current.bytes = managed_checkpoint_directory_size(&path, &current.manifest)?;
    Ok(current)
}

fn plan_checkpoint_cleanup(
    destination_root: &Path,
    records: &[OperationRecord],
) -> Result<CheckpointCleanupPlan, String> {
    let last_cleanup = latest_checkpoint_cleanup(records);
    if !destination_root.exists() {
        return Ok(CheckpointCleanupPlan {
            status: CheckpointStorageStatus {
                last_cleanup,
                ..CheckpointStorageStatus::default()
            },
            reclaimable: Vec::new(),
        });
    }
    let root_metadata = fs::symlink_metadata(destination_root)
        .map_err(|error| format!("failed to inspect backup root: {error}"))?;
    if !root_metadata.is_dir() || root_metadata.file_type().is_symlink() {
        return Err("backup root is not a regular directory".to_string());
    }
    let root = fs::canonicalize(destination_root)
        .map_err(|error| format!("failed to resolve backup root: {error}"))?;
    let mut total_count = 0_usize;
    let mut total_bytes = 0_u64;
    let mut unclassified = 0_usize;
    let mut directories = BTreeMap::new();
    for entry in
        fs::read_dir(&root).map_err(|error| format!("failed to scan backup root: {error}"))?
    {
        let entry = entry.map_err(|error| format!("failed to scan backup entry: {error}"))?;
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|error| format!("failed to inspect backup entry: {error}"))?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            unclassified += 1;
            continue;
        }
        total_count += 1;
        let path = fs::canonicalize(entry.path())
            .map_err(|error| format!("failed to resolve backup entry: {error}"))?;
        if path.parent() != Some(root.as_path()) {
            return Err("backup entry escaped the managed backup root".to_string());
        }
        let bytes = match directory_size_without_links(&path) {
            Ok(bytes) => bytes,
            Err(_) => {
                unclassified += 1;
                continue;
            }
        };
        total_bytes = total_bytes
            .checked_add(bytes)
            .ok_or_else(|| "backup storage byte count overflowed".to_string())?;
        match read_managed_checkpoint(&path) {
            Ok(mut checkpoint) => {
                match managed_checkpoint_directory_size(&path, &checkpoint.manifest) {
                    Ok(exact_bytes) if exact_bytes == bytes => {
                        checkpoint.bytes = exact_bytes;
                        directories.insert(path, checkpoint);
                    }
                    _ => unclassified += 1,
                }
            }
            Err(_) => unclassified += 1,
        }
    }

    let mut references = HashMap::<PathBuf, usize>::new();
    let operation_id_counts = records.iter().fold(HashMap::new(), |mut counts, record| {
        *counts
            .entry(record.operation_id.as_str())
            .or_insert(0_usize) += 1;
        counts
    });
    for record in records {
        for selected in &record.backup_dirs {
            if !selected.exists() {
                continue;
            }
            let resolved = fs::canonicalize(selected).map_err(|error| {
                format!("failed to resolve operation backup reference: {error}")
            })?;
            if directories.contains_key(&resolved) {
                *references.entry(resolved).or_default() += 1;
            } else if is_automatic_checkpoint_terminal(record)
                && resolved.parent() != Some(root.as_path())
            {
                return Err(
                    "automatic checkpoint operation referenced a path outside the managed backup root"
                        .to_string(),
                );
            }
        }
    }

    let mut reclaimable_paths = HashSet::new();
    for record in records.iter().filter(|record| {
        is_automatic_checkpoint_terminal(record)
            && !record.operation_id.trim().is_empty()
            && operation_id_counts
                .get(record.operation_id.as_str())
                .copied()
                == Some(1)
    }) {
        if !automatic_checkpoint_count_matches(record, record.backup_dirs.len()) {
            continue;
        }
        let mut selected = Vec::with_capacity(record.backup_dirs.len());
        let mut complete = true;
        for backup_dir in &record.backup_dirs {
            let resolved = match fs::canonicalize(backup_dir) {
                Ok(resolved) => resolved,
                Err(_) => {
                    complete = false;
                    break;
                }
            };
            if resolved.parent() != Some(root.as_path()) {
                return Err(
                    "automatic checkpoint operation referenced a path outside the managed backup root"
                        .to_string(),
                );
            }
            let Some(checkpoint) = directories.get(&resolved) else {
                complete = false;
                break;
            };
            if backup_dir != &checkpoint.manifest.backup_dir {
                complete = false;
                break;
            }
            if references.get(&resolved).copied() != Some(1) {
                complete = false;
                break;
            }
            selected.push(checkpoint);
        }
        if !complete
            || selected
                .iter()
                .map(|checkpoint| checkpoint.path.as_path())
                .collect::<HashSet<_>>()
                .len()
                != selected.len()
            || !checkpoint_selection_matches(record, &selected)
        {
            continue;
        }
        reclaimable_paths.extend(
            selected
                .into_iter()
                .map(|checkpoint| checkpoint.path.clone()),
        );
    }

    let reclaimable = reclaimable_paths
        .into_iter()
        .filter_map(|path| directories.get(&path).cloned())
        .collect::<Vec<_>>();
    let reclaimable_bytes = reclaimable.iter().try_fold(0_u64, |total, checkpoint| {
        total
            .checked_add(checkpoint.bytes)
            .ok_or_else(|| "reclaimable checkpoint byte count overflowed".to_string())
    })?;
    let reclaimable_count = reclaimable.len();
    let retained_count = total_count.saturating_sub(reclaimable_count);
    let mut warnings = Vec::new();
    if unclassified > 0 {
        warnings.push(format!(
            "{unclassified} backup entries could not be proven safe to reclaim and were retained"
        ));
    }
    Ok(CheckpointCleanupPlan {
        status: CheckpointStorageStatus {
            total_count,
            total_bytes,
            reclaimable_count,
            reclaimable_bytes,
            retained_count,
            warnings,
            last_cleanup,
        },
        reclaimable,
    })
}

fn read_managed_checkpoint(path: &Path) -> Result<ManagedCheckpointDirectory, String> {
    let raw = fs::read(path.join("manifest.json"))
        .map_err(|error| format!("failed to read checkpoint manifest: {error}"))?;
    let value: serde_json::Value = serde_json::from_slice(&raw)
        .map_err(|error| format!("failed to parse checkpoint manifest: {error}"))?;
    let raw_version = value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .and_then(|version| u32::try_from(version).ok())
        .ok_or_else(|| "checkpoint manifest version is invalid".to_string())?;
    let initial = read_backup_manifest(path)?;
    let manifest = verify_backup(path)?;
    if manifest != initial || manifest.version != raw_version {
        return Err("checkpoint manifest changed during verification".to_string());
    }
    let manifest_dir = fs::canonicalize(&manifest.backup_dir)
        .map_err(|error| format!("failed to resolve checkpoint manifest path: {error}"))?;
    if manifest_dir != path {
        return Err("checkpoint manifest directory does not match its container".to_string());
    }
    Ok(ManagedCheckpointDirectory {
        path: path.to_path_buf(),
        bytes: 0,
        raw_version,
        manifest,
    })
}

fn is_automatic_checkpoint_terminal(record: &OperationRecord) -> bool {
    matches!(
        (record.action, record.status, record.phase),
        (
            OperationAction::SwitchRuntime
                | OperationAction::IncrementalSync
                | OperationAction::SyncSessions,
            OperationStatus::Succeeded,
            OperationPhase::Complete
        ) | (
            OperationAction::SwitchRuntime
                | OperationAction::IncrementalSync
                | OperationAction::SyncSessions,
            OperationStatus::RolledBack,
            OperationPhase::Rollback
        ) | (
            OperationAction::SwitchRuntime
                | OperationAction::IncrementalSync
                | OperationAction::SyncSessions,
            OperationStatus::Failed,
            OperationPhase::Backup
        ) | (
            OperationAction::RestoreVisibility,
            OperationStatus::Succeeded,
            OperationPhase::Complete
        )
    )
}

fn automatic_checkpoint_count_matches(record: &OperationRecord, count: usize) -> bool {
    match (record.action, record.status, record.phase) {
        (
            OperationAction::SwitchRuntime
            | OperationAction::IncrementalSync
            | OperationAction::SyncSessions,
            OperationStatus::Succeeded,
            OperationPhase::Complete,
        )
        | (
            OperationAction::SwitchRuntime
            | OperationAction::IncrementalSync
            | OperationAction::SyncSessions,
            OperationStatus::RolledBack,
            OperationPhase::Rollback,
        ) => count == 2,
        (
            OperationAction::SwitchRuntime
            | OperationAction::IncrementalSync
            | OperationAction::SyncSessions,
            OperationStatus::Failed,
            OperationPhase::Backup,
        ) => matches!(count, 1 | 2),
        (
            OperationAction::RestoreVisibility,
            OperationStatus::Succeeded,
            OperationPhase::Complete,
        ) => count == 1,
        _ => false,
    }
}

fn checkpoint_selection_matches(
    record: &OperationRecord,
    checkpoints: &[&ManagedCheckpointDirectory],
) -> bool {
    if checkpoints.is_empty()
        || checkpoints.iter().any(|checkpoint| {
            checkpoint.raw_version != BACKUP_MANIFEST_VERSION
                || !is_transient_checkpoint(&checkpoint.manifest)
                || checkpoint.manifest.operation_id.as_deref() != Some(record.operation_id.as_str())
        })
        || record.completed_at_ms < record.started_at_ms
        || checkpoints.iter().any(|checkpoint| {
            checkpoint.manifest.created_at_ms < record.started_at_ms
                || checkpoint.manifest.created_at_ms > record.completed_at_ms
        })
        || (checkpoints.len() == 2
            && ensure_roots_disjoint(
                &checkpoints[0].manifest.source_root,
                "checkpoint source roots",
                &checkpoints[1].manifest.source_root,
                "checkpoint source roots",
            )
            .is_err())
    {
        return false;
    }
    match (record.action, record.status, record.phase) {
        (
            OperationAction::SwitchRuntime
            | OperationAction::IncrementalSync
            | OperationAction::SyncSessions,
            OperationStatus::Failed,
            OperationPhase::Backup,
        ) => prewrite_failure_checkpoints_match(record.action, checkpoints),
        (
            OperationAction::RestoreVisibility,
            OperationStatus::Succeeded,
            OperationPhase::Complete,
        ) => {
            checkpoints.len() == 1
                && checkpoints[0].manifest.reason == "restore-sessions-visible"
                && checkpoints[0].manifest.scope == BackupScope::StateOnly
                && checkpoints[0].manifest.role == Some(CheckpointRole::Visibility)
        }
        _ => checkpoint_pair_matches(record, checkpoints),
    }
}

fn prewrite_failure_checkpoints_match(
    action: OperationAction,
    checkpoints: &[&ManagedCheckpointDirectory],
) -> bool {
    if !matches!(checkpoints.len(), 1 | 2) {
        return false;
    }
    let mut roles = HashSet::new();
    checkpoints.iter().all(|checkpoint| {
        checkpoint
            .manifest
            .role
            .is_some_and(|role| roles.insert(role))
            && matches!(
                (
                    action,
                    checkpoint.manifest.reason.as_str(),
                    checkpoint.manifest.scope,
                    checkpoint.manifest.role,
                ),
                (
                    OperationAction::SwitchRuntime,
                    "switch-runtime-current",
                    BackupScope::RuntimeState,
                    Some(CheckpointRole::Current),
                ) | (
                    OperationAction::SwitchRuntime,
                    "switch-runtime-shared",
                    BackupScope::StateOnly,
                    Some(CheckpointRole::Shared),
                ) | (
                    OperationAction::IncrementalSync,
                    "incremental-current",
                    BackupScope::StateOnly,
                    Some(CheckpointRole::Current),
                ) | (
                    OperationAction::IncrementalSync,
                    "incremental-shared",
                    BackupScope::StateOnly,
                    Some(CheckpointRole::Shared),
                ) | (
                    OperationAction::SyncSessions,
                    "sync-current",
                    BackupScope::StateOnly,
                    Some(CheckpointRole::Current),
                ) | (
                    OperationAction::SyncSessions,
                    "sync-shared",
                    BackupScope::StateOnly,
                    Some(CheckpointRole::Shared),
                )
            )
    })
}

fn checkpoint_pair_matches(
    record: &OperationRecord,
    checkpoints: &[&ManagedCheckpointDirectory],
) -> bool {
    if checkpoints.len() != 2
        || record.completed_at_ms < record.started_at_ms
        || checkpoints.iter().any(|checkpoint| {
            checkpoint.manifest.created_at_ms < record.started_at_ms
                || checkpoint.manifest.created_at_ms > record.completed_at_ms
        })
        || ensure_roots_disjoint(
            &checkpoints[0].manifest.source_root,
            "checkpoint source roots",
            &checkpoints[1].manifest.source_root,
            "checkpoint source roots",
        )
        .is_err()
    {
        return false;
    }
    let mut by_role = checkpoints
        .iter()
        .filter_map(|checkpoint| checkpoint.manifest.role.map(|role| (role, *checkpoint)))
        .collect::<HashMap<_, _>>();
    let (Some(current), Some(shared)) = (
        by_role.remove(&CheckpointRole::Current),
        by_role.remove(&CheckpointRole::Shared),
    ) else {
        return false;
    };
    if !by_role.is_empty() {
        return false;
    }
    match record.action {
        OperationAction::SwitchRuntime => {
            current.manifest.reason == "switch-runtime-current"
                && current.manifest.scope == BackupScope::RuntimeState
                && shared.manifest.reason == "switch-runtime-shared"
                && shared.manifest.scope == BackupScope::StateOnly
        }
        OperationAction::SyncSessions => {
            current.manifest.reason == "sync-current"
                && current.manifest.scope == BackupScope::StateOnly
                && shared.manifest.reason == "sync-shared"
                && shared.manifest.scope == BackupScope::StateOnly
        }
        OperationAction::IncrementalSync => {
            current.manifest.reason == "incremental-current"
                && current.manifest.scope == BackupScope::StateOnly
                && shared.manifest.reason == "incremental-shared"
                && shared.manifest.scope == BackupScope::StateOnly
        }
        _ => false,
    }
}

fn latest_checkpoint_cleanup(records: &[OperationRecord]) -> Option<CheckpointCleanupReceipt> {
    records
        .iter()
        .find(|record| {
            record.action == OperationAction::CleanupCheckpoints
                && matches!(
                    (record.status, record.phase),
                    (OperationStatus::Succeeded, OperationPhase::Complete)
                        | (OperationStatus::Failed, OperationPhase::Apply)
                )
        })
        .map(|record| {
            let reclaimed_count = record
                .counts
                .get("reclaimedCount")
                .copied()
                .unwrap_or_default();
            let failed_count = record
                .counts
                .get("failedCount")
                .copied()
                .unwrap_or_default();
            CheckpointCleanupReceipt {
                operation_id: record.operation_id.clone(),
                attempted_count: record
                    .counts
                    .get("attemptedCount")
                    .copied()
                    .unwrap_or_else(|| reclaimed_count.saturating_add(failed_count)),
                failed_count,
                reclaimed_count,
                reclaimed_bytes: record
                    .counts
                    .get("reclaimedBytes")
                    .copied()
                    .and_then(|value| u64::try_from(value).ok())
                    .unwrap_or_default(),
                retained_count: record
                    .counts
                    .get("retainedCount")
                    .copied()
                    .unwrap_or_default(),
                warnings: Vec::new(),
            }
        })
}

fn create_backup_in_dir(
    home: &Path,
    backup_dir: &Path,
    reason: &str,
    created_at_ms: u128,
    paths: CodexPaths,
    scope: BackupScope,
    binding: Option<CheckpointBinding<'_>>,
) -> Result<BackupManifest, String> {
    let root_existed = home.exists();
    let mut files = Vec::new();

    let mut sources = Vec::new();
    if scope.tracks_runtime_files() {
        sources.push((home.join("auth.json"), PathBuf::from("auth.json")));
        sources.push((home.join("config.toml"), PathBuf::from("config.toml")));
    }
    if scope.tracks_process_state() {
        if let Some(snapshot) = read_chat_process_state_snapshot(home)? {
            files.push(encrypt_payload_bytes(
                &snapshot.bytes,
                &snapshot.path,
                backup_dir,
                Path::new(CHAT_PROCESS_STATE_RELATIVE_PATH),
            )?);
        }
    }
    if scope.tracks_sessions() {
        sources.push((
            paths.session_index.clone(),
            PathBuf::from("session_index.jsonl"),
        ));
    }
    for (source, relative) in sources {
        if source.is_file() {
            files.push(encrypt_payload(&source, backup_dir, &relative)?);
        }
    }

    for (relative, database) in managed_sqlite_paths(&paths) {
        if !scope.tracked_databases().contains(&relative) {
            continue;
        }
        if database.is_file() {
            files.push(snapshot_sqlite(database, backup_dir, Path::new(relative))?);
        }
    }

    if scope.tracks_sessions() {
        let mut session_roots = vec![&paths.sessions_dir];
        if scope.tracks_archived_sessions() {
            session_roots.push(&paths.archived_sessions_dir);
        }
        for session_root in session_roots {
            if !session_root.is_dir() {
                continue;
            }
            for path in walk_jsonl_files(session_root)? {
                let relative = path
                    .strip_prefix(home)
                    .map_err(|error| format!("failed to map session backup path: {error}"))?
                    .to_path_buf();
                files.push(encrypt_payload(&path, backup_dir, &relative)?);
            }
        }
    }
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));

    let manifest = BackupManifest {
        version: BACKUP_MANIFEST_VERSION,
        reason: reason.to_string(),
        operation_id: binding.map(|binding| binding.operation_id.to_string()),
        role: binding.map(|binding| binding.role),
        created_at_ms,
        source_root: home.to_path_buf(),
        root_existed,
        scope,
        tracked_databases: scope
            .tracked_databases()
            .iter()
            .map(|database| (*database).to_string())
            .collect(),
        state_db_is_local: paths.state_db == home.join("state_5.sqlite"),
        tracked_process_state: scope.tracks_process_state(),
        complete_sessions: scope.tracks_sessions(),
        backup_dir: backup_dir.to_path_buf(),
        files,
    };
    let encoded = serde_json::to_vec_pretty(&manifest)
        .map_err(|error| format!("failed to serialize backup manifest: {error}"))?;
    atomic_write(&backup_dir.join("manifest.json"), &encoded)?;
    verify_backup(backup_dir)
}

pub fn verify_backup(backup_dir: &Path) -> Result<BackupManifest, String> {
    let manifest = read_backup_manifest(backup_dir)?;
    let canonical_root = fs::canonicalize(backup_dir)
        .map_err(|error| format!("failed to resolve backup directory: {error}"))?;
    for file in &manifest.files {
        validate_relative_path(&file.relative_path)?;
        let canonical_payload = fs::canonicalize(&file.backup_path)
            .map_err(|error| format!("backup payload is missing: {error}"))?;
        if !canonical_payload.starts_with(&canonical_root) {
            return Err("backup payload escaped the backup directory".to_string());
        }
        let metadata = fs::metadata(&canonical_payload)
            .map_err(|error| format!("failed to inspect backup payload: {error}"))?;
        if metadata.len() != file.bytes {
            return Err(format!(
                "backup payload size mismatch: {}",
                file.relative_path.display()
            ));
        }
        if sha256_file(&canonical_payload)? != file.sha256 {
            return Err(format!(
                "backup payload checksum mismatch: {}",
                file.relative_path.display()
            ));
        }
        if !file.encrypted {
            return Err("unencrypted payloads are not restorable".to_string());
        }
    }
    Ok(manifest)
}

/// Extracts one payload from an already managed backup without interpreting the
/// backed-up config or resolving its historical SQLite root. This is used by
/// the v0.3 legacy-backup inventory so an old `sqlite_home` value can never
/// redirect an isolated inspection into a live profile.
#[cfg(test)]
pub(crate) fn extract_verified_backup_file(
    backup_dir: &Path,
    relative_path: &Path,
    target: &Path,
) -> Result<(u64, String), String> {
    validate_absolute_root(backup_dir, "backup directory")?;
    if !target.is_absolute() {
        return Err("backup extraction target must be absolute".to_string());
    }
    validate_relative_path(relative_path)?;
    let manifest = verify_backup(backup_dir)?;
    extract_backup_manifest_file(&manifest, relative_path, target)
}

pub(crate) fn extract_backup_manifest_file(
    manifest: &BackupManifest,
    relative_path: &Path,
    target: &Path,
) -> Result<(u64, String), String> {
    if !manifest.backup_dir.is_absolute() || !target.is_absolute() {
        return Err("backup extraction paths must be absolute".to_string());
    }
    validate_relative_path(relative_path)?;
    let file = manifest
        .files
        .iter()
        .find(|file| file.relative_path == relative_path)
        .ok_or_else(|| "backup payload is not declared by the manifest".to_string())?;
    let encrypted_before = fs::read(&file.backup_path)
        .map_err(|_| "backup payload is unavailable during extraction".to_string())?;
    if encrypted_before.len() as u64 != file.bytes
        || format!("{:x}", Sha256::digest(&encrypted_before)) != file.sha256
    {
        return Err("backup payload changed before extraction".to_string());
    }
    let plaintext = unprotect(&encrypted_before)
        .map_err(|_| "backup payload could not be decrypted for isolated inspection".to_string())?;
    atomic_write(target, &plaintext)
        .map_err(|_| "failed to write isolated backup payload".to_string())?;
    let encrypted_after = fs::read(&file.backup_path)
        .map_err(|_| "backup payload is unavailable after extraction".to_string())?;
    if encrypted_after != encrypted_before {
        return Err("backup payload changed during extraction".to_string());
    }
    let bytes = u64::try_from(plaintext.len())
        .map_err(|_| "isolated backup payload size overflowed".to_string())?;
    let sha256 = format!("{:x}", Sha256::digest(&plaintext));
    let written =
        fs::read(target).map_err(|_| "isolated backup payload is unavailable".to_string())?;
    if written.len() as u64 != bytes || format!("{:x}", Sha256::digest(&written)) != sha256 {
        return Err("isolated backup payload verification failed".to_string());
    }
    Ok((bytes, sha256))
}

#[cfg(test)]
pub(crate) fn load_process_state_checkpoint(
    manifest: &BackupManifest,
    codex_home: &Path,
) -> Result<Option<Vec<u8>>, String> {
    if manifest.scope != BackupScope::RuntimeState || !manifest.tracked_process_state {
        return Err("runtime checkpoint does not track ChatGPT process state".to_string());
    }
    let verified = verify_backup(&manifest.backup_dir)?;
    if verified != *manifest {
        return Err("runtime checkpoint changed after creation".to_string());
    }
    let checkpoint_file = manifest
        .files
        .iter()
        .find(|file| file.relative_path == Path::new(CHAT_PROCESS_STATE_RELATIVE_PATH));
    let current = read_chat_process_state_snapshot(codex_home)?;
    match (checkpoint_file, current) {
        (None, None) => Ok(None),
        (Some(file), Some(current)) => {
            let encrypted = fs::read(&file.backup_path)
                .map_err(|_| "runtime checkpoint process state is unavailable".to_string())?;
            let checkpoint_bytes = unprotect(&encrypted)?;
            if current.bytes != checkpoint_bytes {
                return Err(
                    "ChatGPT process state changed after the runtime checkpoint".to_string()
                );
            }
            Ok(Some(checkpoint_bytes))
        }
        _ => Err("ChatGPT process state changed after the runtime checkpoint".to_string()),
    }
}

fn read_backup_manifest(backup_dir: &Path) -> Result<BackupManifest, String> {
    let manifest_path = backup_dir.join("manifest.json");
    let raw = fs::read(&manifest_path)
        .map_err(|error| format!("failed to read backup manifest: {error}"))?;
    let value: serde_json::Value = serde_json::from_slice(&raw)
        .map_err(|error| format!("failed to parse backup manifest: {error}"))?;
    let mut manifest: BackupManifest = serde_json::from_value(value.clone())
        .map_err(|error| format!("failed to parse backup manifest: {error}"))?;
    if !matches!(
        manifest.version,
        2 | SCOPED_BACKUP_MANIFEST_VERSION | BACKUP_MANIFEST_VERSION
    ) {
        return Err(format!(
            "unsupported backup manifest version: {}",
            manifest.version
        ));
    }
    validate_manifest_contract(&manifest, &value)?;
    if manifest.version == 2 {
        manifest.scope = BackupScope::Full;
        manifest.tracked_databases = vec![STATE_DATABASE.to_string()];
    }
    Ok(manifest)
}

fn validate_manifest_contract(
    manifest: &BackupManifest,
    raw: &serde_json::Value,
) -> Result<(), String> {
    if !manifest.root_existed && !manifest.files.is_empty() {
        return Err(
            "backup manifest cannot contain payloads when the source root did not exist"
                .to_string(),
        );
    }
    if manifest.version >= SCOPED_BACKUP_MANIFEST_VERSION {
        let object = raw
            .as_object()
            .ok_or_else(|| "backup manifest must be a JSON object".to_string())?;
        if !object.contains_key("scope") || !object.contains_key("trackedDatabases") {
            return Err("scoped backup manifest is missing scope metadata".to_string());
        }
        let expected = manifest.scope.tracked_databases();
        let actual = manifest
            .tracked_databases
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        if actual.len() != manifest.tracked_databases.len()
            || actual.len() != expected.len()
            || !expected.iter().all(|database| actual.contains(database))
        {
            return Err("backup manifest database scope is invalid".to_string());
        }
        if manifest.complete_sessions != manifest.scope.tracks_sessions() {
            return Err("backup manifest session scope is inconsistent".to_string());
        }
    }
    if manifest.version >= BACKUP_MANIFEST_VERSION {
        let object = raw
            .as_object()
            .ok_or_else(|| "backup manifest must be a JSON object".to_string())?;
        if !object.contains_key("trackedProcessState")
            || manifest.tracked_process_state != manifest.scope.tracks_process_state()
        {
            return Err("backup manifest process-state scope is invalid".to_string());
        }
        match (&manifest.operation_id, manifest.role) {
            (Some(operation_id), Some(_)) if !operation_id.trim().is_empty() => {}
            (None, None) => {}
            _ => return Err("checkpoint binding metadata is incomplete".to_string()),
        }
    } else if manifest.operation_id.is_some()
        || manifest.role.is_some()
        || manifest.tracked_process_state
    {
        return Err("legacy backup manifest contains unsupported checkpoint metadata".to_string());
    }

    let mut relative_paths = HashSet::new();
    for file in &manifest.files {
        if !relative_paths.insert(&file.relative_path) {
            return Err(format!(
                "backup manifest contains a duplicate payload: {}",
                file.relative_path.display()
            ));
        }
        if !manifest_allows_file(manifest, &file.relative_path) {
            return Err(format!(
                "backup payload is outside the declared scope: {}",
                file.relative_path.display()
            ));
        }
    }
    Ok(())
}

fn manifest_allows_file(manifest: &BackupManifest, relative_path: &Path) -> bool {
    let scope = manifest_scope(manifest);
    let Some(relative) = relative_path.to_str() else {
        return false;
    };
    if MANAGED_DATABASES.contains(&relative) {
        return manifest_tracks_database(manifest, relative);
    }
    if matches!(relative, "auth.json" | "config.toml") {
        return scope.tracks_runtime_files();
    }
    if relative == CHAT_PROCESS_STATE_RELATIVE_PATH {
        return manifest.tracked_process_state;
    }
    if relative == "session_index.jsonl" {
        return scope.tracks_sessions();
    }
    let is_jsonl = relative_path.extension().and_then(|value| value.to_str()) == Some("jsonl");
    if relative_path.starts_with(Path::new("sessions")) {
        return scope.tracks_sessions() && is_jsonl;
    }
    manifest.version >= SCOPED_BACKUP_MANIFEST_VERSION
        && scope.tracks_archived_sessions()
        && relative_path.starts_with(Path::new("archived_sessions"))
        && is_jsonl
}

fn manifest_scope(manifest: &BackupManifest) -> BackupScope {
    if manifest.version == 2 {
        BackupScope::Full
    } else {
        manifest.scope
    }
}

fn prepare_backup_restore_plan(
    backup_dir: &Path,
    target_home: &Path,
    operation_id: &str,
) -> Result<BackupRestorePreparedPlan, String> {
    let backup_dir = validate_absolute_root(backup_dir, "backup directory")?;
    let target_home = validate_absolute_root(target_home, "restore target root")?;
    validate_backup_restore_operation_id(operation_id)?;
    ensure_roots_disjoint(
        &backup_dir,
        "backup directory",
        &target_home,
        "restore target root",
    )?;
    let manifest = verify_backup(&backup_dir)?;
    let backup_metadata = fs::symlink_metadata(&backup_dir)
        .map_err(|_| "backup restore directory is unavailable".to_string())?;
    let manifest_metadata = fs::symlink_metadata(backup_dir.join("manifest.json"))
        .map_err(|_| "backup restore manifest is unavailable".to_string())?;
    if !backup_metadata.is_dir()
        || backup_metadata_is_link_or_reparse(&backup_metadata)
        || !manifest_metadata.is_file()
        || backup_metadata_is_link_or_reparse(&manifest_metadata)
    {
        return Err("backup restore directory is unsafe".to_string());
    }
    if manifest.backup_dir != backup_dir
        || fs::canonicalize(&manifest.backup_dir)
            .map_err(|_| "backup restore directory is unavailable".to_string())?
            != fs::canonicalize(&backup_dir)
                .map_err(|_| "backup restore directory is unavailable".to_string())?
        || manifest.source_root != target_home
    {
        return Err("backup restore target does not match the backed-up root".to_string());
    }
    let (_, backup_manifest_sha256) = backup_restore_existing_file_digest(
        &backup_dir.join("manifest.json"),
        "backup restore manifest",
    )?;

    let old_paths = if manifest.state_db_is_local {
        local_codex_paths(&target_home)
    } else {
        resolve_user_codex_paths(&target_home)?
    };
    let new_paths = resolve_backed_up_codex_paths(&manifest, &target_home, &old_paths)?;
    for root in [&old_paths.sqlite_home, &new_paths.sqlite_home] {
        ensure_roots_disjoint(
            &backup_dir,
            "backup directory",
            root,
            "backup restore SQLite root",
        )?;
    }

    let mut allowed_roots = vec![target_home.clone()];
    for root in [&old_paths.sqlite_home, &new_paths.sqlite_home] {
        if !allowed_roots
            .iter()
            .any(|candidate| backup_restore_path_key(candidate) == backup_restore_path_key(root))
        {
            allowed_roots.push(root.clone());
        }
    }
    allowed_roots.sort_by_key(|path| backup_restore_path_key(path));
    for root in &allowed_roots {
        validate_backup_restore_allowed_root(root)?;
    }

    let mut planned = BTreeMap::<String, BackupRestoreMutationPlan>::new();
    if manifest.root_existed {
        for file in &manifest.files {
            let target = restore_target(&new_paths, &target_home, &file.relative_path)?;
            validate_backup_restore_target_ancestry(&allowed_roots, &target)?;
            let plaintext = read_verified_backup_plaintext(&manifest, file)?;
            let replacement_sha256 = format!("{:x}", Sha256::digest(&plaintext));
            let expected_sqlite =
                sqlite_restore_target(&new_paths, file.relative_path.to_string_lossy().as_ref())
                    .is_some();
            if expected_sqlite {
                validate_sqlite_snapshot_header(&plaintext)?;
            }
            let existing = backup_restore_optional_file_digest(&target, "backup restore target")?;
            let kind = if existing.is_some() {
                BackupRestoreMutationKind::Replace
            } else {
                BackupRestoreMutationKind::Create
            };
            insert_backup_restore_mutation(
                &mut planned,
                operation_id,
                BackupRestoreMutationPlan {
                    kind,
                    logical_path: file.relative_path.clone(),
                    target_path: target,
                    original_sha256: existing.map(|(_, sha256)| sha256),
                    replacement_sha256: Some(replacement_sha256),
                    artifacts: BackupRestoreArtifactPaths {
                        original_witness_path: PathBuf::new(),
                        replacement_witness_path: PathBuf::new(),
                        staging_path: PathBuf::new(),
                        recovery_path: PathBuf::new(),
                        rollback_tombstone_path: PathBuf::new(),
                    },
                    sqlite: expected_sqlite,
                },
            )?;
        }
    }

    collect_backup_restore_deletions(
        &manifest,
        &target_home,
        &old_paths,
        &new_paths,
        &allowed_roots,
        operation_id,
        &mut planned,
    )?;
    let mutations = planned.into_values().collect::<Vec<_>>();
    let created_at_ms = timestamp_millis()?;
    let plan = BackupRestorePlan {
        operation_id: operation_id.to_string(),
        created_at_ms,
        backup_dir: backup_dir.clone(),
        backup_manifest_sha256,
        target_root: target_home,
        allowed_roots,
        restored_file_count: manifest.files.len(),
        mutations,
    };
    let journal = new_backup_restore_journal(plan)?;
    Ok(BackupRestorePreparedPlan { manifest, journal })
}

fn insert_backup_restore_mutation(
    planned: &mut BTreeMap<String, BackupRestoreMutationPlan>,
    operation_id: &str,
    mut mutation: BackupRestoreMutationPlan,
) -> Result<(), String> {
    mutation.artifacts = build_backup_restore_artifact_paths(operation_id, &mutation.target_path)?;
    let key = backup_restore_path_key(&mutation.target_path);
    match planned.entry(key) {
        std::collections::btree_map::Entry::Vacant(entry) => {
            entry.insert(mutation);
            Ok(())
        }
        std::collections::btree_map::Entry::Occupied(entry)
            if entry.get().replacement_sha256.is_some()
                && mutation.replacement_sha256.is_none() =>
        {
            Ok(())
        }
        std::collections::btree_map::Entry::Occupied(entry)
            if entry.get().kind == BackupRestoreMutationKind::Delete
                && mutation.kind == BackupRestoreMutationKind::Delete
                && entry.get().original_sha256 == mutation.original_sha256 =>
        {
            Ok(())
        }
        std::collections::btree_map::Entry::Occupied(_) => {
            Err("backup restore plan contains conflicting target actions".to_string())
        }
    }
}

fn collect_backup_restore_deletions(
    manifest: &BackupManifest,
    target_home: &Path,
    old_paths: &CodexPaths,
    new_paths: &CodexPaths,
    allowed_roots: &[PathBuf],
    operation_id: &str,
    planned: &mut BTreeMap<String, BackupRestoreMutationPlan>,
) -> Result<(), String> {
    let scope = manifest_scope(manifest);
    let desired = manifest
        .files
        .iter()
        .map(|file| file.relative_path.clone())
        .collect::<HashSet<_>>();
    let mut candidates = Vec::<(PathBuf, PathBuf, bool)>::new();
    if scope.tracks_runtime_files() {
        for relative in [Path::new("auth.json"), Path::new("config.toml")] {
            if !desired.contains(relative) {
                candidates.push((
                    PathBuf::from(format!("absent/{}", relative.display())),
                    target_home.join(relative),
                    false,
                ));
            }
        }
    }
    if manifest.tracked_process_state
        && !desired.contains(Path::new(CHAT_PROCESS_STATE_RELATIVE_PATH))
    {
        if let Some(target) = existing_chat_process_state_restore_target(target_home)? {
            candidates.push((
                PathBuf::from(format!("absent/{CHAT_PROCESS_STATE_RELATIVE_PATH}")),
                target,
                false,
            ));
        }
    }
    if scope.tracks_sessions() && !desired.contains(Path::new("session_index.jsonl")) {
        candidates.push((
            PathBuf::from("absent/session_index.jsonl"),
            new_paths.session_index.clone(),
            false,
        ));
    }

    for (relative, new_database) in managed_sqlite_paths(new_paths) {
        if !manifest_tracks_database(manifest, relative) {
            continue;
        }
        let old_database = sqlite_restore_target(old_paths, relative)
            .expect("managed SQLite relative paths must be routable");
        let database_is_desired = desired.contains(Path::new(relative));
        if !database_is_desired {
            candidates.push((
                PathBuf::from(format!("absent/{relative}")),
                new_database.to_path_buf(),
                true,
            ));
        }
        if old_database != new_database {
            candidates.push((
                PathBuf::from(format!("previous-sqlite/{relative}")),
                old_database.to_path_buf(),
                true,
            ));
        }
        for database in [new_database, old_database] {
            for suffix in ["-wal", "-shm"] {
                let sidecar = PathBuf::from(format!("{}{suffix}", database.to_string_lossy()));
                candidates.push((
                    PathBuf::from(format!(
                        "sqlite-sidecars/{}/{}{}",
                        backup_restore_root_digest(
                            database.parent().unwrap_or_else(|| Path::new("."))
                        ),
                        relative,
                        suffix
                    )),
                    sidecar,
                    true,
                ));
            }
        }
    }

    if scope.tracks_sessions() && manifest.complete_sessions {
        let expected = manifest
            .files
            .iter()
            .filter(|file| {
                file.relative_path.starts_with(Path::new("sessions"))
                    || file
                        .relative_path
                        .starts_with(Path::new("archived_sessions"))
            })
            .map(|file| target_home.join(&file.relative_path))
            .map(|path| backup_restore_path_key(&path))
            .collect::<HashSet<_>>();
        for (directory, tracked) in [
            (&new_paths.sessions_dir, true),
            (
                &new_paths.archived_sessions_dir,
                scope.tracks_archived_sessions(),
            ),
        ] {
            if !tracked || !directory.is_dir() {
                continue;
            }
            for path in walk_jsonl_files(directory)? {
                if !expected.contains(&backup_restore_path_key(&path)) {
                    let relative = path
                        .strip_prefix(target_home)
                        .map_err(|_| "backup restore session escaped its root".to_string())?;
                    candidates.push((
                        PathBuf::from(format!("extra/{}", relative.display())),
                        path,
                        false,
                    ));
                }
            }
        }
    }

    for (logical_path, target_path, sqlite) in candidates {
        validate_backup_restore_target_ancestry(allowed_roots, &target_path)?;
        let Some((_, original_sha256)) =
            backup_restore_optional_file_digest(&target_path, "backup restore deletion target")?
        else {
            continue;
        };
        insert_backup_restore_mutation(
            planned,
            operation_id,
            BackupRestoreMutationPlan {
                kind: BackupRestoreMutationKind::Delete,
                logical_path,
                target_path,
                original_sha256: Some(original_sha256),
                replacement_sha256: None,
                artifacts: BackupRestoreArtifactPaths {
                    original_witness_path: PathBuf::new(),
                    replacement_witness_path: PathBuf::new(),
                    staging_path: PathBuf::new(),
                    recovery_path: PathBuf::new(),
                    rollback_tombstone_path: PathBuf::new(),
                },
                sqlite,
            },
        )?;
    }
    Ok(())
}

fn resolve_backed_up_codex_paths(
    manifest: &BackupManifest,
    target_home: &Path,
    current_paths: &CodexPaths,
) -> Result<CodexPaths, String> {
    if manifest.state_db_is_local {
        return Ok(local_codex_paths(target_home));
    }
    let configured_sqlite_home = match manifest
        .files
        .iter()
        .find(|file| file.relative_path == Path::new("config.toml"))
    {
        Some(file) => {
            let plaintext = read_verified_backup_plaintext(manifest, file)?;
            let text = std::str::from_utf8(&plaintext)
                .map_err(|_| "backed-up config.toml is not UTF-8".to_string())?;
            let document = DocumentMut::from_str(text)
                .map_err(|_| "backed-up config.toml is invalid".to_string())?;
            match document.get("sqlite_home") {
                Some(value) => {
                    let raw = value
                        .as_str()
                        .ok_or_else(|| {
                            "backed-up config.toml sqlite_home must be a string".to_string()
                        })?
                        .trim();
                    if raw.is_empty() {
                        None
                    } else {
                        Some(validate_absolute_root(
                            &PathBuf::from(raw),
                            "backed-up config.toml sqlite_home",
                        )?)
                    }
                }
                None => None,
            }
        }
        None => None,
    };
    let mut declared_sqlite_roots = manifest
        .files
        .iter()
        .filter(|file| {
            file.relative_path
                .to_str()
                .is_some_and(|relative| MANAGED_DATABASES.contains(&relative))
        })
        .map(|file| {
            file.source
                .parent()
                .map(Path::to_path_buf)
                .ok_or_else(|| "backed-up SQLite payload has no parent".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    declared_sqlite_roots.sort_by_key(|path| backup_restore_path_key(path));
    declared_sqlite_roots
        .dedup_by(|left, right| backup_restore_path_key(left) == backup_restore_path_key(right));
    if declared_sqlite_roots.len() > 1 {
        return Err("backed-up SQLite payloads disagree on their storage root".to_string());
    }
    if let (Some(configured), Some(source)) = (
        configured_sqlite_home.as_ref(),
        declared_sqlite_roots.first(),
    ) {
        if backup_restore_path_key(configured) != backup_restore_path_key(source) {
            return Err("backed-up config and SQLite payload roots disagree".to_string());
        }
    }
    let sqlite_home = configured_sqlite_home
        .or_else(|| {
            (current_paths.sqlite_home != target_home).then(|| current_paths.sqlite_home.clone())
        })
        .ok_or_else(|| "external backup SQLite root cannot be proven".to_string())?;
    codex_paths_with_sqlite_home(target_home, &sqlite_home)
}

fn read_verified_backup_plaintext(
    manifest: &BackupManifest,
    file: &BackupFile,
) -> Result<Vec<u8>, String> {
    let expected = encrypted_payload_path(&manifest.backup_dir, &file.relative_path)?;
    if file.backup_path != expected || !file.encrypted {
        return Err("backup restore payload identity is invalid".to_string());
    }
    let metadata = fs::symlink_metadata(&file.backup_path)
        .map_err(|_| "backup restore payload is unavailable".to_string())?;
    if !metadata.is_file() || backup_metadata_is_link_or_reparse(&metadata) {
        return Err("backup restore payload is unsafe".to_string());
    }
    let encrypted_before = fs::read(&file.backup_path)
        .map_err(|_| "backup restore payload is unavailable".to_string())?;
    if encrypted_before.len() as u64 != file.bytes
        || format!("{:x}", Sha256::digest(&encrypted_before)) != file.sha256
    {
        return Err("backup restore payload changed before staging".to_string());
    }
    let plaintext = unprotect(&encrypted_before)
        .map_err(|_| "backup restore payload is unreadable".to_string())?;
    let encrypted_after = fs::read(&file.backup_path)
        .map_err(|_| "backup restore payload is unavailable".to_string())?;
    if encrypted_after != encrypted_before {
        return Err("backup restore payload changed during staging".to_string());
    }
    Ok(plaintext)
}

fn validate_sqlite_snapshot_header(bytes: &[u8]) -> Result<(), String> {
    const SQLITE_HEADER: &[u8] = b"SQLite format 3\0";
    if bytes.len() < 100 || !bytes.starts_with(SQLITE_HEADER) {
        return Err("backed-up SQLite payload header is invalid".to_string());
    }
    let encoded_page_size = u16::from_be_bytes([bytes[16], bytes[17]]);
    let page_size = if encoded_page_size == 1 {
        65_536_usize
    } else {
        usize::from(encoded_page_size)
    };
    if !(512..=65_536).contains(&page_size)
        || !page_size.is_power_of_two()
        || !bytes.len().is_multiple_of(page_size)
    {
        return Err("backed-up SQLite payload page layout is invalid".to_string());
    }
    Ok(())
}

fn backup_restore_optional_file_digest(
    path: &Path,
    label: &str,
) -> Result<Option<(u64, String)>, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_file() || backup_metadata_is_link_or_reparse(&metadata) {
                return Err(format!("{label} is not a regular file"));
            }
            backup_restore_existing_file_digest(path, label).map(Some)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(format!("{label} is unavailable")),
    }
}

fn backup_restore_existing_file_digest(path: &Path, label: &str) -> Result<(u64, String), String> {
    let before = fs::symlink_metadata(path).map_err(|_| format!("{label} is unavailable"))?;
    if !before.is_file() || backup_metadata_is_link_or_reparse(&before) {
        return Err(format!("{label} is not a regular file"));
    }
    let sha256 = sha256_file(path)?;
    let after = fs::symlink_metadata(path).map_err(|_| format!("{label} is unavailable"))?;
    if before.len() != after.len()
        || before.modified().ok() != after.modified().ok()
        || backup_restore_metadata_identity(path, &before)
            != backup_restore_metadata_identity(path, &after)
    {
        return Err(format!("{label} changed during inspection"));
    }
    Ok((before.len(), sha256))
}

#[cfg(windows)]
fn backup_restore_metadata_identity(
    path: &Path,
    metadata: &fs::Metadata,
) -> (Option<(u32, u64)>, u32, u64) {
    use std::os::windows::{fs::MetadataExt, io::AsRawHandle};
    use windows_sys::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION},
    };
    let identity = fs::File::open(path).ok().and_then(|file| {
        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        let ok =
            unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut information) };
        (ok != 0).then_some((
            information.dwVolumeSerialNumber,
            (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow),
        ))
    });
    (
        identity,
        metadata.file_attributes(),
        metadata.last_write_time(),
    )
}

#[cfg(unix)]
fn backup_restore_metadata_identity(_path: &Path, metadata: &fs::Metadata) -> (u64, u64, u32, i64) {
    use std::os::unix::fs::MetadataExt;
    (
        metadata.dev(),
        metadata.ino(),
        metadata.mode(),
        metadata.ctime(),
    )
}

#[cfg(not(any(windows, unix)))]
fn backup_restore_metadata_identity(
    _path: &Path,
    metadata: &fs::Metadata,
) -> (u64, Option<SystemTime>) {
    (metadata.len(), metadata.modified().ok())
}

fn backup_restore_root_digest(path: &Path) -> String {
    let digest = format!(
        "{:x}",
        Sha256::digest(backup_restore_path_key(path).as_bytes())
    );
    digest[..16].to_string()
}

fn build_backup_restore_artifact_paths(
    operation_id: &str,
    target_path: &Path,
) -> Result<BackupRestoreArtifactPaths, String> {
    validate_backup_restore_operation_id(operation_id)?;
    validate_absolute_root(
        target_path
            .parent()
            .ok_or_else(|| "backup restore target has no parent".to_string())?,
        "backup restore target parent",
    )?;
    let target_name = target_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "backup restore target name is invalid".to_string())?;
    let digest = format!(
        "{:x}",
        Sha256::digest(
            format!(
                "codex-switch-backup-restore-artifact-v1\0{operation_id}\0{}",
                backup_restore_path_key(target_path)
            )
            .as_bytes()
        )
    );
    let prefix = format!(
        ".{target_name}.codex-switch-backup-restore-{}",
        &digest[..32]
    );
    let parent = target_path
        .parent()
        .expect("validated backup restore targets have a parent");
    Ok(BackupRestoreArtifactPaths {
        original_witness_path: parent.join(format!("{prefix}.original")),
        replacement_witness_path: parent.join(format!("{prefix}.replacement")),
        staging_path: parent.join(format!("{prefix}.staging")),
        recovery_path: parent.join(format!("{prefix}.recovery")),
        rollback_tombstone_path: parent.join(format!("{prefix}.tombstone")),
    })
}

fn new_backup_restore_journal(plan: BackupRestorePlan) -> Result<BackupRestoreJournal, String> {
    validate_backup_restore_plan(&plan)?;
    let mutation_states = plan
        .mutations
        .iter()
        .map(|_| BackupRestoreMutationState {
            phase: BackupRestoreMutationPhase::Planned,
            parent_identity: None,
            original_identity: None,
            replacement_identity: None,
        })
        .collect::<Vec<_>>();
    let journal = BackupRestoreJournal {
        schema_version: BACKUP_RESTORE_JOURNAL_SCHEMA_VERSION,
        revision: 0,
        updated_at_ms: plan.created_at_ms,
        phase: BackupRestoreOperationPhase::Planned,
        plan_integrity_sha256: backup_restore_plan_digest(&plan)?,
        plan,
        mutation_states,
    };
    validate_backup_restore_journal(&journal)?;
    Ok(journal)
}

fn validate_backup_restore_journal(journal: &BackupRestoreJournal) -> Result<(), String> {
    if journal.schema_version != BACKUP_RESTORE_JOURNAL_SCHEMA_VERSION {
        return Err("backup restore journal version is unsupported".to_string());
    }
    validate_backup_restore_plan(&journal.plan)?;
    if journal.plan_integrity_sha256 != backup_restore_plan_digest(&journal.plan)? {
        return Err("backup restore plan integrity check failed".to_string());
    }
    if journal.updated_at_ms < journal.plan.created_at_ms {
        return Err("backup restore journal timestamp moved backwards".to_string());
    }
    if journal.mutation_states.len() != journal.plan.mutations.len() {
        return Err("backup restore journal mutation state is incomplete".to_string());
    }
    if journal.phase == BackupRestoreOperationPhase::Planned
        && (journal.revision != 0
            || journal
                .mutation_states
                .iter()
                .any(|state| state.phase != BackupRestoreMutationPhase::Planned))
    {
        return Err("backup restore planned journal contains applied state".to_string());
    }
    for (mutation, state) in journal.plan.mutations.iter().zip(&journal.mutation_states) {
        validate_backup_restore_mutation_state(mutation, state)?;
    }
    match journal.phase {
        BackupRestoreOperationPhase::Committed => {
            if journal.mutation_states.iter().any(|state| {
                !matches!(
                    state.phase,
                    BackupRestoreMutationPhase::CommittedWithRecovery
                        | BackupRestoreMutationPhase::Cleaned
                )
            }) {
                return Err("committed backup restore has unfinished mutations".to_string());
            }
        }
        BackupRestoreOperationPhase::RolledBack => {
            if journal.mutation_states.iter().any(|state| {
                !matches!(
                    state.phase,
                    BackupRestoreMutationPhase::RolledBack | BackupRestoreMutationPhase::Cleaned
                )
            }) {
                return Err("rolled back backup restore has unfinished mutations".to_string());
            }
        }
        BackupRestoreOperationPhase::CommittedCleanupComplete
        | BackupRestoreOperationPhase::RolledBackCleanupComplete
            if journal
                .mutation_states
                .iter()
                .any(|state| state.phase != BackupRestoreMutationPhase::Cleaned) =>
        {
            return Err("completed backup restore cleanup has retained artifacts".to_string());
        }
        _ => {}
    }
    Ok(())
}

fn validate_backup_restore_plan(plan: &BackupRestorePlan) -> Result<(), String> {
    validate_backup_restore_operation_id(&plan.operation_id)?;
    if plan.created_at_ms == 0 {
        return Err("backup restore plan timestamp is invalid".to_string());
    }
    let backup_dir = validate_absolute_root(&plan.backup_dir, "backup restore backup directory")?;
    let target_root = validate_absolute_root(&plan.target_root, "backup restore target root")?;
    validate_backup_restore_sha256(&plan.backup_manifest_sha256)?;
    if plan.allowed_roots.is_empty() {
        return Err("backup restore plan has no allowed roots".to_string());
    }
    let mut allowed_root_keys = BTreeSet::new();
    for root in &plan.allowed_roots {
        let root = validate_absolute_root(root, "backup restore allowed root")?;
        validate_backup_restore_allowed_root(&root)?;
        if !allowed_root_keys.insert(backup_restore_path_key(&root)) {
            return Err("backup restore plan contains a duplicate allowed root".to_string());
        }
        ensure_roots_disjoint(
            &backup_dir,
            "backup restore backup directory",
            &root,
            "backup restore allowed root",
        )?;
    }
    if !allowed_root_keys.contains(&backup_restore_path_key(&target_root)) {
        return Err("backup restore target root is not allowed".to_string());
    }
    let mut target_keys = BTreeSet::new();
    let mut reserved_keys = BTreeSet::new();
    for mutation in &plan.mutations {
        validate_relative_path(&mutation.logical_path)?;
        validate_absolute_root(
            mutation
                .target_path
                .parent()
                .ok_or_else(|| "backup restore mutation target has no parent".to_string())?,
            "backup restore mutation parent",
        )?;
        if mutation.target_path.file_name().is_none()
            || !plan
                .allowed_roots
                .iter()
                .any(|root| mutation.target_path != *root && mutation.target_path.starts_with(root))
        {
            return Err("backup restore mutation escaped its allowed roots".to_string());
        }
        let target_key = backup_restore_path_key(&mutation.target_path);
        if !target_keys.insert(target_key.clone()) || !reserved_keys.insert(target_key) {
            return Err("backup restore plan contains a duplicate target".to_string());
        }
        let expected =
            build_backup_restore_artifact_paths(&plan.operation_id, &mutation.target_path)?;
        if mutation.artifacts != expected {
            return Err("backup restore artifact paths are not deterministic".to_string());
        }
        for artifact in [
            &mutation.artifacts.original_witness_path,
            &mutation.artifacts.replacement_witness_path,
            &mutation.artifacts.staging_path,
            &mutation.artifacts.recovery_path,
            &mutation.artifacts.rollback_tombstone_path,
        ] {
            let key = backup_restore_path_key(artifact);
            if !reserved_keys.insert(key) {
                return Err("backup restore plan contains a duplicate artifact".to_string());
            }
        }
        match mutation.kind {
            BackupRestoreMutationKind::Create => {
                if mutation.original_sha256.is_some() || mutation.replacement_sha256.is_none() {
                    return Err("backup restore create mutation hashes are invalid".to_string());
                }
            }
            BackupRestoreMutationKind::Replace => {
                if mutation.original_sha256.is_none() || mutation.replacement_sha256.is_none() {
                    return Err("backup restore replace mutation hashes are invalid".to_string());
                }
            }
            BackupRestoreMutationKind::Delete => {
                if mutation.original_sha256.is_none() || mutation.replacement_sha256.is_some() {
                    return Err("backup restore delete mutation hashes are invalid".to_string());
                }
            }
        }
        if let Some(sha256) = &mutation.original_sha256 {
            validate_backup_restore_sha256(sha256)?;
        }
        if let Some(sha256) = &mutation.replacement_sha256 {
            validate_backup_restore_sha256(sha256)?;
        }
    }
    Ok(())
}

fn validate_backup_restore_mutation_state(
    mutation: &BackupRestoreMutationPlan,
    state: &BackupRestoreMutationState,
) -> Result<(), String> {
    if state.phase == BackupRestoreMutationPhase::Planned
        && (state.parent_identity.is_some()
            || state.original_identity.is_some()
            || state.replacement_identity.is_some())
    {
        return Err("planned backup restore mutation contains file identity".to_string());
    }
    if mutation.original_sha256.is_none() && state.original_identity.is_some() {
        return Err("backup restore mutation has an unexpected original identity".to_string());
    }
    if mutation.replacement_sha256.is_none() && state.replacement_identity.is_some() {
        return Err("backup restore mutation has an unexpected replacement identity".to_string());
    }
    if !matches!(
        state.phase,
        BackupRestoreMutationPhase::Planned
            | BackupRestoreMutationPhase::WitnessCreating
            | BackupRestoreMutationPhase::RollbackPreparing
    ) {
        if state.parent_identity.is_none() {
            return Err("backup restore mutation parent identity is missing".to_string());
        }
        if mutation.original_sha256.is_some() && state.original_identity.is_none() {
            return Err("backup restore mutation original identity is missing".to_string());
        }
        if mutation.replacement_sha256.is_some() && state.replacement_identity.is_none() {
            return Err("backup restore mutation replacement identity is missing".to_string());
        }
    }
    Ok(())
}

fn validate_backup_restore_operation_transition(
    current: BackupRestoreOperationPhase,
    next: BackupRestoreOperationPhase,
) -> Result<(), String> {
    let allowed = current == next
        || matches!(
            (current, next),
            (
                BackupRestoreOperationPhase::Planned,
                BackupRestoreOperationPhase::Applying | BackupRestoreOperationPhase::RollingBack
            ) | (
                BackupRestoreOperationPhase::Applying,
                BackupRestoreOperationPhase::Validating | BackupRestoreOperationPhase::RollingBack
            ) | (
                BackupRestoreOperationPhase::Validating,
                BackupRestoreOperationPhase::Committing | BackupRestoreOperationPhase::RollingBack
            ) | (
                BackupRestoreOperationPhase::Committing,
                BackupRestoreOperationPhase::Committed | BackupRestoreOperationPhase::RollingBack
            ) | (
                BackupRestoreOperationPhase::Committed,
                BackupRestoreOperationPhase::CommittedCleanupComplete
            ) | (
                BackupRestoreOperationPhase::RollingBack,
                BackupRestoreOperationPhase::RolledBack
            ) | (
                BackupRestoreOperationPhase::RolledBack,
                BackupRestoreOperationPhase::RolledBackCleanupComplete
            )
        );
    if allowed {
        Ok(())
    } else {
        Err("backup restore journal phase transition is invalid".to_string())
    }
}

fn validate_backup_restore_mutation_transition(
    current: BackupRestoreMutationPhase,
    next: BackupRestoreMutationPhase,
) -> Result<(), String> {
    let allowed = current == next
        || matches!(
            (current, next),
            (
                BackupRestoreMutationPhase::Planned,
                BackupRestoreMutationPhase::WitnessCreating
                    | BackupRestoreMutationPhase::RollbackPreparing
                    | BackupRestoreMutationPhase::Cleaned
            ) | (
                BackupRestoreMutationPhase::WitnessCreating,
                BackupRestoreMutationPhase::WitnessReady
                    | BackupRestoreMutationPhase::RollbackPreparing
            ) | (
                BackupRestoreMutationPhase::WitnessReady,
                BackupRestoreMutationPhase::Preparing
                    | BackupRestoreMutationPhase::Publishing
                    | BackupRestoreMutationPhase::RollbackPreparing
            ) | (
                BackupRestoreMutationPhase::Preparing,
                BackupRestoreMutationPhase::Prepared
                    | BackupRestoreMutationPhase::RollbackPreparing
            ) | (
                BackupRestoreMutationPhase::Prepared,
                BackupRestoreMutationPhase::Publishing
                    | BackupRestoreMutationPhase::CommittedWithRecovery
                    | BackupRestoreMutationPhase::RollbackPreparing
            ) | (
                BackupRestoreMutationPhase::Publishing,
                BackupRestoreMutationPhase::Published
                    | BackupRestoreMutationPhase::RollbackPreparing
            ) | (
                BackupRestoreMutationPhase::Published,
                BackupRestoreMutationPhase::CommittedWithRecovery
                    | BackupRestoreMutationPhase::RollbackPreparing
            ) | (
                BackupRestoreMutationPhase::CommittedWithRecovery,
                BackupRestoreMutationPhase::RollbackPreparing | BackupRestoreMutationPhase::Cleaned
            ) | (
                BackupRestoreMutationPhase::RollbackPreparing,
                BackupRestoreMutationPhase::RollbackPrepared
                    | BackupRestoreMutationPhase::RolledBack
            ) | (
                BackupRestoreMutationPhase::RollbackPrepared,
                BackupRestoreMutationPhase::RolledBack
            ) | (
                BackupRestoreMutationPhase::RolledBack,
                BackupRestoreMutationPhase::Cleaned
            )
        );
    if allowed {
        Ok(())
    } else {
        Err("backup restore mutation phase transition is invalid".to_string())
    }
}

fn validate_backup_restore_journal_update(
    current: &BackupRestoreJournal,
    next: &BackupRestoreJournal,
) -> Result<(), String> {
    validate_backup_restore_journal(current)?;
    validate_backup_restore_journal(next)?;
    if (matches!(
        current.phase,
        BackupRestoreOperationPhase::CommittedCleanupComplete
            | BackupRestoreOperationPhase::RolledBackCleanupComplete
    ) && current.phase != next.phase)
        || (current.phase == BackupRestoreOperationPhase::Committed
            && !matches!(
                next.phase,
                BackupRestoreOperationPhase::Committed
                    | BackupRestoreOperationPhase::CommittedCleanupComplete
            ))
        || (current.phase == BackupRestoreOperationPhase::RolledBack
            && !matches!(
                next.phase,
                BackupRestoreOperationPhase::RolledBack
                    | BackupRestoreOperationPhase::RolledBackCleanupComplete
            ))
    {
        return Err("terminal backup restore journal cannot change outcome".to_string());
    }
    if next.schema_version != current.schema_version
        || next.plan != current.plan
        || next.plan_integrity_sha256 != current.plan_integrity_sha256
        || next.revision != current.revision.saturating_add(1)
        || next.updated_at_ms < current.updated_at_ms
        || next.mutation_states.len() != current.mutation_states.len()
    {
        return Err("backup restore journal immutable identity changed".to_string());
    }
    validate_backup_restore_operation_transition(current.phase, next.phase)?;
    for (current, next) in current.mutation_states.iter().zip(&next.mutation_states) {
        validate_backup_restore_mutation_transition(current.phase, next.phase)?;
        if current.original_identity.is_some()
            && current.original_identity != next.original_identity
            || current.parent_identity.is_some() && current.parent_identity != next.parent_identity
            || current.replacement_identity.is_some()
                && current.replacement_identity != next.replacement_identity
        {
            return Err("backup restore mutation identity changed".to_string());
        }
    }
    Ok(())
}

fn create_backup_restore_journal(
    data_root: &Path,
    journal: &BackupRestoreJournal,
) -> Result<PathBuf, String> {
    validate_backup_restore_journal(journal)?;
    if journal.revision != 0 || journal.phase != BackupRestoreOperationPhase::Planned {
        return Err("backup restore journal must begin in the planned phase".to_string());
    }
    let operations_root = ensure_backup_restore_operations_root(data_root)?;
    let operation_root = operations_root.join(&journal.plan.operation_id);
    match fs::symlink_metadata(&operation_root) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Ok(_) => return Err("backup restore operation already exists".to_string()),
        Err(_) => return Err("backup restore operation inventory is unavailable".to_string()),
    }
    let pending_root = operations_root.join(format!(
        ".pending-{}-{}-{}",
        journal.plan.operation_id,
        std::process::id(),
        journal.plan.created_at_ms
    ));
    match fs::create_dir(&pending_root) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return Err("backup restore pending operation already exists".to_string())
        }
        Err(_) => return Err("failed to create pending backup restore operation".to_string()),
    }
    let pending_journal_path = pending_root.join(BACKUP_RESTORE_JOURNAL_FILE);
    let bytes = encode_backup_restore_journal(journal)?;
    let publish = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&pending_journal_path)
            .map_err(|_| "failed to create backup restore journal".to_string())?;
        file.write_all(&bytes)
            .map_err(|_| "failed to write backup restore journal".to_string())?;
        file.sync_all()
            .map_err(|_| "failed to flush backup restore journal".to_string())?;
        drop(file);
        let persisted = fs::read(&pending_journal_path)
            .map_err(|_| "backup restore journal is unavailable".to_string())?;
        if decode_backup_restore_journal_bytes(&persisted)?
            != decode_backup_restore_journal_bytes(&bytes)?
        {
            return Err("backup restore journal verification failed".to_string());
        }
        backup_restore_publish_directory(&pending_root, &operation_root)
    })();
    if let Err(error) = publish {
        let _ = fs::remove_file(&pending_journal_path);
        let _ = fs::remove_dir(&pending_root);
        return Err(error);
    }
    let journal_path = operation_root.join(BACKUP_RESTORE_JOURNAL_FILE);
    let persisted = load_backup_restore_journal(data_root, &journal.plan.operation_id)?;
    if persisted != *journal {
        return Err("backup restore journal verification failed".to_string());
    }
    Ok(journal_path)
}

#[cfg(windows)]
fn backup_restore_publish_directory(source: &Path, target: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_WRITE_THROUGH};

    let source = fs::canonicalize(source)
        .map_err(|_| "pending backup restore operation is unavailable".to_string())?;
    let target_parent = target
        .parent()
        .ok_or_else(|| "backup restore operation has no parent".to_string())?;
    let target_parent = fs::canonicalize(target_parent)
        .map_err(|_| "backup restore operation inventory is unavailable".to_string())?;
    let target_name = target
        .file_name()
        .ok_or_else(|| "backup restore operation name is invalid".to_string())?;
    let target = target_parent.join(target_name);
    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let target = target
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let ok = unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), MOVEFILE_WRITE_THROUGH) };
    if ok == 0 {
        Err("failed to publish backup restore operation".to_string())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn backup_restore_publish_directory(source: &Path, target: &Path) -> Result<(), String> {
    fs::rename(source, target).map_err(|_| "failed to publish backup restore operation".to_string())
}

fn persist_backup_restore_journal(
    data_root: &Path,
    current: &BackupRestoreJournal,
    next: &BackupRestoreJournal,
) -> Result<BackupRestoreJournal, String> {
    validate_backup_restore_journal_update(current, next)?;
    let persisted = load_backup_restore_journal(data_root, &current.plan.operation_id)?;
    if persisted != *current {
        return Err("backup restore journal changed concurrently".to_string());
    }
    let journal_path = backup_restore_journal_path(data_root, &current.plan.operation_id)?;
    atomic_write(&journal_path, &encode_backup_restore_journal(next)?)?;
    let persisted = load_backup_restore_journal(data_root, &current.plan.operation_id)?;
    if persisted != *next {
        return Err("backup restore journal verification failed".to_string());
    }
    Ok(persisted)
}

fn transition_backup_restore_operation(
    data_root: &Path,
    journal: &mut BackupRestoreJournal,
    phase: BackupRestoreOperationPhase,
) -> Result<(), String> {
    let current = journal.clone();
    let mut next = current.clone();
    next.revision = next
        .revision
        .checked_add(1)
        .ok_or_else(|| "backup restore journal revision overflowed".to_string())?;
    next.updated_at_ms = timestamp_millis()?.max(current.updated_at_ms);
    next.phase = phase;
    *journal = persist_backup_restore_journal(data_root, &current, &next)?;
    Ok(())
}

fn transition_backup_restore_mutation(
    data_root: &Path,
    journal: &mut BackupRestoreJournal,
    index: usize,
    phase: BackupRestoreMutationPhase,
    parent_identity: Option<RegularFileIdentity>,
    original_identity: Option<RegularFileIdentity>,
    replacement_identity: Option<RegularFileIdentity>,
) -> Result<(), String> {
    let current = journal.clone();
    let mut next = current.clone();
    let state = next
        .mutation_states
        .get_mut(index)
        .ok_or_else(|| "backup restore mutation index is invalid".to_string())?;
    state.phase = phase;
    if parent_identity.is_some() {
        state.parent_identity = parent_identity;
    }
    if original_identity.is_some() {
        state.original_identity = original_identity;
    }
    if replacement_identity.is_some() {
        state.replacement_identity = replacement_identity;
    }
    next.revision = next
        .revision
        .checked_add(1)
        .ok_or_else(|| "backup restore journal revision overflowed".to_string())?;
    next.updated_at_ms = timestamp_millis()?.max(current.updated_at_ms);
    *journal = persist_backup_restore_journal(data_root, &current, &next)?;
    Ok(())
}

fn stage_backup_restore_witnesses(
    data_root: &Path,
    manifest: &BackupManifest,
    journal: &mut BackupRestoreJournal,
) -> Result<(), String> {
    if journal.phase == BackupRestoreOperationPhase::Planned {
        transition_backup_restore_operation(
            data_root,
            journal,
            BackupRestoreOperationPhase::Applying,
        )?;
    }
    if journal.phase != BackupRestoreOperationPhase::Applying {
        return Err("backup restore journal is not ready for staging".to_string());
    }
    let verified = verify_backup(&journal.plan.backup_dir)?;
    if verified != *manifest
        || backup_restore_existing_file_digest(
            &journal.plan.backup_dir.join("manifest.json"),
            "backup restore manifest",
        )?
        .1 != journal.plan.backup_manifest_sha256
    {
        return Err("backup restore source changed before staging".to_string());
    }

    for index in 0..journal.plan.mutations.len() {
        if journal.mutation_states[index].phase != BackupRestoreMutationPhase::Planned {
            return Err("backup restore mutation was already staged".to_string());
        }
        let mutation = journal.plan.mutations[index].clone();
        transition_backup_restore_mutation(
            data_root,
            journal,
            index,
            BackupRestoreMutationPhase::WitnessCreating,
            None,
            None,
            None,
        )?;
        for artifact in [
            &mutation.artifacts.original_witness_path,
            &mutation.artifacts.replacement_witness_path,
            &mutation.artifacts.staging_path,
            &mutation.artifacts.recovery_path,
            &mutation.artifacts.rollback_tombstone_path,
        ] {
            match fs::symlink_metadata(artifact) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Ok(_) => {
                    return Err(
                        "backup restore operation artifact appeared before staging".to_string()
                    )
                }
                Err(_) => {
                    return Err(
                        "backup restore operation artifact is unavailable before staging"
                            .to_string(),
                    )
                }
            }
        }
        ensure_backup_restore_target_parent(&journal.plan, &mutation.target_path)?;

        let original_identity = if let Some(expected) = mutation.original_sha256.as_deref() {
            let (_, actual) = backup_restore_existing_file_digest(
                &mutation.target_path,
                "backup restore target",
            )?;
            if actual != expected {
                return Err("backup restore target changed after planning".to_string());
            }
            create_backup_restore_hardlink(
                &mutation.target_path,
                &mutation.artifacts.original_witness_path,
            )?;
            if !backup_restore_same_file_identity(
                &mutation.target_path,
                &mutation.artifacts.original_witness_path,
            )? {
                return Err("backup restore original witness identity is invalid".to_string());
            }
            Some(backup_restore_file_identity(&mutation.target_path)?)
        } else {
            if backup_restore_optional_file_digest(
                &mutation.target_path,
                "backup restore create target",
            )?
            .is_some()
            {
                return Err("backup restore create target appeared after planning".to_string());
            }
            None
        };

        let replacement_identity = if let Some(expected) = mutation.replacement_sha256.as_deref() {
            let backup_file = manifest
                .files
                .iter()
                .find(|file| file.relative_path == mutation.logical_path)
                .ok_or_else(|| "backup restore replacement payload is not declared".to_string())?;
            let plaintext = read_verified_backup_plaintext(manifest, backup_file)?;
            if format!("{:x}", Sha256::digest(&plaintext)) != expected {
                return Err("backup restore replacement bytes changed".to_string());
            }
            let mut target = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&mutation.artifacts.replacement_witness_path)
                .map_err(|error| match error.kind() {
                    io::ErrorKind::AlreadyExists => {
                        "backup restore replacement witness appeared concurrently".to_string()
                    }
                    _ => "failed to create backup restore replacement witness".to_string(),
                })?;
            target
                .write_all(&plaintext)
                .map_err(|_| "failed to write backup restore replacement witness".to_string())?;
            target
                .sync_all()
                .map_err(|_| "failed to flush backup restore replacement witness".to_string())?;
            drop(target);
            if backup_restore_existing_file_digest(
                &mutation.artifacts.replacement_witness_path,
                "backup restore replacement witness",
            )?
            .1 != expected
            {
                return Err("backup restore replacement witness verification failed".to_string());
            }
            if mutation.sqlite {
                quick_check_sqlite(
                    &mutation.artifacts.replacement_witness_path,
                    "staged backup restore SQLite",
                )?;
            }
            Some(backup_restore_file_identity(
                &mutation.artifacts.replacement_witness_path,
            )?)
        } else {
            None
        };
        transition_backup_restore_mutation(
            data_root,
            journal,
            index,
            BackupRestoreMutationPhase::WitnessReady,
            Some(parent_directory_identity_at_path(&mutation.target_path)?),
            original_identity,
            replacement_identity,
        )?;
    }
    Ok(())
}

fn ensure_backup_restore_target_parent(
    plan: &BackupRestorePlan,
    target_path: &Path,
) -> Result<(), String> {
    let parent = target_path
        .parent()
        .ok_or_else(|| "backup restore target has no parent".to_string())?;
    let root = plan
        .allowed_roots
        .iter()
        .filter(|root| parent.starts_with(root))
        .max_by_key(|root| root.components().count())
        .ok_or_else(|| "backup restore target parent escaped its allowed roots".to_string())?;
    let root_metadata = fs::symlink_metadata(root)
        .map_err(|_| "backup restore allowed root is unavailable".to_string())?;
    if !root_metadata.is_dir() || backup_metadata_is_link_or_reparse(&root_metadata) {
        return Err("backup restore allowed root is unsafe".to_string());
    }
    let relative = parent
        .strip_prefix(root)
        .map_err(|_| "backup restore target parent escaped its allowed root".to_string())?;
    let mut current = fs::canonicalize(root)
        .map_err(|_| "backup restore allowed root is unavailable".to_string())?;
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err("backup restore target parent is invalid".to_string());
        };
        current.push(component);
        match fs::create_dir(&current) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(_) => return Err("failed to create backup restore target parent".to_string()),
        }
        let metadata = fs::symlink_metadata(&current)
            .map_err(|_| "backup restore target parent is unavailable".to_string())?;
        if !metadata.is_dir() || backup_metadata_is_link_or_reparse(&metadata) {
            return Err("backup restore target parent is unsafe".to_string());
        }
    }
    Ok(())
}

fn validate_backup_restore_allowed_root(root: &Path) -> Result<(), String> {
    validate_absolute_root(root, "backup restore allowed root")?;
    match fs::symlink_metadata(root) {
        Ok(metadata) => {
            if !metadata.is_dir() || backup_metadata_is_link_or_reparse(&metadata) {
                return Err("backup restore allowed root is unsafe".to_string());
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err("backup restore allowed root is unavailable".to_string())
        }
        Err(_) => return Err("backup restore allowed root is unavailable".to_string()),
    }
    Ok(())
}

fn validate_backup_restore_target_ancestry(
    allowed_roots: &[PathBuf],
    target: &Path,
) -> Result<(), String> {
    let parent = target
        .parent()
        .ok_or_else(|| "backup restore target has no parent".to_string())?;
    let root = allowed_roots
        .iter()
        .filter(|root| target != root.as_path() && target.starts_with(root))
        .max_by_key(|root| root.components().count())
        .ok_or_else(|| "backup restore target escaped its allowed roots".to_string())?;
    validate_backup_restore_allowed_root(root)?;
    let relative = parent
        .strip_prefix(root)
        .map_err(|_| "backup restore target parent escaped its allowed root".to_string())?;
    let mut current = root.clone();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err("backup restore target parent is invalid".to_string());
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if !metadata.is_dir() || backup_metadata_is_link_or_reparse(&metadata) {
                    return Err("backup restore target parent is unsafe".to_string());
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(_) => return Err("backup restore target parent is unavailable".to_string()),
        }
    }
    Ok(())
}

// Each cfg branch is the complete platform implementation, so the explicit
// return keeps the mutually exclusive bodies readable.
#[allow(clippy::needless_return)]
fn backup_restore_file_identity(path: &Path) -> Result<RegularFileIdentity, String> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::{
            Foundation::HANDLE,
            Storage::FileSystem::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION},
        };
        let file = fs::File::open(path)
            .map_err(|_| "backup restore file identity is unavailable".to_string())?;
        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        let ok =
            unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut information) };
        if ok == 0 {
            return Err("backup restore file identity is unavailable".to_string());
        }
        return Ok(RegularFileIdentity {
            volume_serial_number: u64::from(information.dwVolumeSerialNumber),
            file_index: (u64::from(information.nFileIndexHigh) << 32)
                | u64::from(information.nFileIndexLow),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = fs::metadata(path)
            .map_err(|_| "backup restore file identity is unavailable".to_string())?;
        return Ok(RegularFileIdentity {
            volume_serial_number: metadata.dev(),
            file_index: metadata.ino(),
        });
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = path;
        Err("backup restore file identity is unsupported".to_string())
    }
}

fn backup_restore_same_file_identity(left: &Path, right: &Path) -> Result<bool, String> {
    Ok(backup_restore_file_identity(left)? == backup_restore_file_identity(right)?)
}

#[cfg(windows)]
fn create_backup_restore_hardlink(source: &Path, witness: &Path) -> Result<(), String> {
    use std::os::windows::{ffi::OsStrExt, io::FromRawHandle};
    use windows_sys::Win32::{
        Foundation::INVALID_HANDLE_VALUE,
        Storage::FileSystem::{
            CreateFileW, CreateHardLinkW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE,
            FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        },
    };

    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_READ_AND_WRITE_AND_DELETE: u32 = 0xC001_0000;
    if source.parent() != witness.parent() {
        return Err("backup restore witness must share the target parent".to_string());
    }
    let canonical_parent = fs::canonicalize(
        source
            .parent()
            .ok_or_else(|| "backup restore witness parent is invalid".to_string())?,
    )
    .map_err(|_| "backup restore witness parent is unavailable".to_string())?;
    let source = canonical_parent.join(
        source
            .file_name()
            .ok_or_else(|| "backup restore source name is invalid".to_string())?,
    );
    let witness = canonical_parent.join(
        witness
            .file_name()
            .ok_or_else(|| "backup restore witness name is invalid".to_string())?,
    );
    let source_wide = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let witness_wide = witness
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let source_handle = unsafe {
        CreateFileW(
            source_wide.as_ptr(),
            GENERIC_READ_AND_WRITE_AND_DELETE,
            FILE_SHARE_READ | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if source_handle == INVALID_HANDLE_VALUE {
        return Err("backup restore target writer barrier is unavailable".to_string());
    }
    let source_guard = unsafe { fs::File::from_raw_handle(source_handle as _) };
    let before = backup_restore_file_identity_from_handle(&source_guard)?;
    if unsafe {
        CreateHardLinkW(
            witness_wide.as_ptr(),
            source_wide.as_ptr(),
            std::ptr::null(),
        )
    } == 0
    {
        return Err("failed to create backup restore original witness".to_string());
    }
    let witness_handle = unsafe {
        CreateFileW(
            witness_wide.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if witness_handle == INVALID_HANDLE_VALUE {
        return Err("backup restore original witness is unavailable".to_string());
    }
    let witness_guard = unsafe { fs::File::from_raw_handle(witness_handle as _) };
    if before != backup_restore_file_identity_from_handle(&source_guard)?
        || before != backup_restore_file_identity_from_handle(&witness_guard)?
    {
        return Err("backup restore target changed during witness creation".to_string());
    }
    Ok(())
}

#[cfg(not(windows))]
fn create_backup_restore_hardlink(source: &Path, witness: &Path) -> Result<(), String> {
    fs::hard_link(source, witness).map_err(|error| match error.kind() {
        io::ErrorKind::AlreadyExists => {
            "backup restore original witness appeared concurrently".to_string()
        }
        _ => "failed to create backup restore original witness".to_string(),
    })
}

#[cfg(windows)]
fn backup_restore_file_identity_from_handle(
    file: &fs::File,
) -> Result<RegularFileIdentity, String> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION},
    };
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    let ok =
        unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut information) };
    if ok == 0 {
        return Err("backup restore file identity is unavailable".to_string());
    }
    Ok(RegularFileIdentity {
        volume_serial_number: u64::from(information.dwVolumeSerialNumber),
        file_index: (u64::from(information.nFileIndexHigh) << 32)
            | u64::from(information.nFileIndexLow),
    })
}

fn load_backup_restore_journal(
    data_root: &Path,
    operation_id: &str,
) -> Result<BackupRestoreJournal, String> {
    validate_backup_restore_operation_id(operation_id)?;
    let path = backup_restore_journal_path(data_root, operation_id)?;
    let operation_root = path
        .parent()
        .ok_or_else(|| "backup restore operation directory is invalid".to_string())?;
    let mut entries = fs::read_dir(operation_root)
        .map_err(|_| "backup restore operation inventory is unavailable".to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "backup restore operation inventory is unavailable".to_string())?;
    if entries.len() != 1 || entries.pop().is_none_or(|entry| entry.path() != path) {
        return Err("backup restore operation contains undeclared artifacts".to_string());
    }
    let metadata = fs::symlink_metadata(&path)
        .map_err(|_| "backup restore journal is unavailable".to_string())?;
    if !metadata.is_file()
        || backup_metadata_is_link_or_reparse(&metadata)
        || metadata.len() == 0
        || metadata.len() > BACKUP_RESTORE_MAX_CIPHERTEXT_BYTES
    {
        return Err("backup restore journal is unsafe".to_string());
    }
    let file =
        fs::File::open(&path).map_err(|_| "backup restore journal is unavailable".to_string())?;
    let mut bytes = Vec::new();
    file.take(BACKUP_RESTORE_MAX_CIPHERTEXT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "backup restore journal is unreadable".to_string())?;
    if bytes.is_empty() || bytes.len() as u64 > BACKUP_RESTORE_MAX_CIPHERTEXT_BYTES {
        return Err("backup restore journal is invalid".to_string());
    }
    let plaintext = decode_backup_restore_journal_bytes(&bytes)?;
    if plaintext.len() as u64 > BACKUP_RESTORE_MAX_JOURNAL_BYTES {
        return Err("backup restore journal is invalid".to_string());
    }
    let envelope = serde_json::from_slice::<BackupRestoreJournalEnvelope>(&plaintext)
        .map_err(|_| "backup restore journal is invalid".to_string())?;
    validate_backup_restore_journal(&envelope.journal)?;
    if envelope.integrity_sha256 != backup_restore_journal_digest(&envelope.journal)? {
        return Err("backup restore journal integrity check failed".to_string());
    }
    if envelope.journal.plan.operation_id != operation_id {
        return Err("backup restore journal operation identity is invalid".to_string());
    }
    Ok(envelope.journal)
}

fn encode_backup_restore_journal(journal: &BackupRestoreJournal) -> Result<Vec<u8>, String> {
    validate_backup_restore_journal(journal)?;
    let envelope = BackupRestoreJournalEnvelope {
        journal: journal.clone(),
        integrity_sha256: backup_restore_journal_digest(journal)?,
    };
    let plaintext = serde_json::to_vec(&envelope)
        .map_err(|_| "failed to serialize backup restore journal".to_string())?;
    if plaintext.len() as u64 > BACKUP_RESTORE_MAX_JOURNAL_BYTES {
        return Err("backup restore journal reached its size limit".to_string());
    }
    #[cfg(windows)]
    {
        let ciphertext = protect(&plaintext)
            .map_err(|_| "failed to protect backup restore journal".to_string())?;
        let mut encoded = Vec::with_capacity(BACKUP_RESTORE_JOURNAL_MAGIC.len() + ciphertext.len());
        encoded.extend_from_slice(BACKUP_RESTORE_JOURNAL_MAGIC);
        encoded.extend_from_slice(&ciphertext);
        if encoded.len() as u64 > BACKUP_RESTORE_MAX_CIPHERTEXT_BYTES {
            return Err("protected backup restore journal reached its size limit".to_string());
        }
        Ok(encoded)
    }
    #[cfg(not(windows))]
    {
        Ok(plaintext)
    }
}

fn decode_backup_restore_journal_bytes(bytes: &[u8]) -> Result<Vec<u8>, String> {
    #[cfg(windows)]
    {
        let ciphertext = bytes
            .strip_prefix(BACKUP_RESTORE_JOURNAL_MAGIC)
            .ok_or_else(|| "backup restore journal is not DPAPI protected".to_string())?;
        if ciphertext.is_empty() {
            return Err("backup restore journal is invalid".to_string());
        }
        unprotect(ciphertext).map_err(|_| "backup restore journal is unreadable".to_string())
    }
    #[cfg(not(windows))]
    {
        let _ = BACKUP_RESTORE_JOURNAL_MAGIC;
        Ok(bytes.to_vec())
    }
}

fn backup_restore_journal_path(data_root: &Path, operation_id: &str) -> Result<PathBuf, String> {
    validate_backup_restore_operation_id(operation_id)?;
    let data_root = validate_backup_restore_data_root(data_root)?;
    let operations_root = data_root.join(BACKUP_RESTORE_OPERATION_DIRECTORY);
    let operation_root = operations_root.join(operation_id);
    validate_backup_restore_operation_root(&operations_root, &operation_root)?;
    Ok(operation_root.join(BACKUP_RESTORE_JOURNAL_FILE))
}

fn validate_backup_restore_data_root(data_root: &Path) -> Result<PathBuf, String> {
    let data_root = validate_absolute_root(data_root, "backup restore data root")?;
    let metadata = fs::symlink_metadata(&data_root)
        .map_err(|_| "backup restore data root is unavailable".to_string())?;
    if !metadata.is_dir() || backup_metadata_is_link_or_reparse(&metadata) {
        return Err("backup restore data root is unsafe".to_string());
    }
    let canonical = fs::canonicalize(&data_root)
        .map_err(|_| "backup restore data root is unavailable".to_string())?;
    if canonical
        .file_name()
        .and_then(|name| name.to_str())
        .is_none_or(|name| !name.eq_ignore_ascii_case("codex-switch"))
    {
        return Err("backup restore data root identity is invalid".to_string());
    }
    for ancestor in canonical.ancestors() {
        let metadata = fs::symlink_metadata(ancestor)
            .map_err(|_| "backup restore data root ancestry is unavailable".to_string())?;
        if !metadata.is_dir() || backup_metadata_is_link_or_reparse(&metadata) {
            return Err("backup restore data root ancestry is unsafe".to_string());
        }
    }
    Ok(canonical)
}

fn ensure_backup_restore_operations_root(data_root: &Path) -> Result<PathBuf, String> {
    let data_root = validate_backup_restore_data_root(data_root)?;
    let operations_root = data_root.join(BACKUP_RESTORE_OPERATION_DIRECTORY);
    match fs::create_dir(&operations_root) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err("failed to create backup restore operation inventory".to_string()),
    }
    let metadata = fs::symlink_metadata(&operations_root)
        .map_err(|_| "backup restore operation inventory is unavailable".to_string())?;
    if !metadata.is_dir() || backup_metadata_is_link_or_reparse(&metadata) {
        return Err("backup restore operation inventory is unsafe".to_string());
    }
    let canonical = fs::canonicalize(&operations_root)
        .map_err(|_| "backup restore operation inventory is unavailable".to_string())?;
    if canonical.parent() != Some(data_root.as_path()) {
        return Err("backup restore operation inventory escaped its data root".to_string());
    }
    Ok(canonical)
}

fn validate_backup_restore_operation_root(
    operations_root: &Path,
    operation_root: &Path,
) -> Result<(), String> {
    let operations_metadata = fs::symlink_metadata(operations_root)
        .map_err(|_| "backup restore operation inventory is unavailable".to_string())?;
    if !operations_metadata.is_dir() || backup_metadata_is_link_or_reparse(&operations_metadata) {
        return Err("backup restore operation inventory is unsafe".to_string());
    }
    let operations_root = fs::canonicalize(operations_root)
        .map_err(|_| "backup restore operation inventory is unavailable".to_string())?;
    let metadata = fs::symlink_metadata(operation_root)
        .map_err(|_| "backup restore operation is unavailable".to_string())?;
    if !metadata.is_dir() || backup_metadata_is_link_or_reparse(&metadata) {
        return Err("backup restore operation directory is unsafe".to_string());
    }
    let operation_root = fs::canonicalize(operation_root)
        .map_err(|_| "backup restore operation is unavailable".to_string())?;
    if operation_root.parent() != Some(operations_root.as_path()) {
        return Err("backup restore operation escaped its inventory".to_string());
    }
    Ok(())
}

fn backup_restore_plan_digest(plan: &BackupRestorePlan) -> Result<String, String> {
    let bytes = serde_json::to_vec(plan)
        .map_err(|_| "failed to serialize backup restore plan".to_string())?;
    let mut hasher = Sha256::new();
    hasher.update(b"codex-switch-backup-restore-plan-v1\0");
    hasher.update(bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

fn backup_restore_journal_digest(journal: &BackupRestoreJournal) -> Result<String, String> {
    let bytes = serde_json::to_vec(journal)
        .map_err(|_| "failed to serialize backup restore journal".to_string())?;
    let mut hasher = Sha256::new();
    hasher.update(b"codex-switch-backup-restore-journal-v1\0");
    hasher.update(bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

fn validate_backup_restore_operation_id(value: &str) -> Result<(), String> {
    if !value.is_empty()
        && value.len() <= BACKUP_RESTORE_MAX_OPERATION_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        Ok(())
    } else {
        Err("backup restore operation ID is invalid".to_string())
    }
}

fn validate_backup_restore_sha256(value: &str) -> Result<(), String> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        Ok(())
    } else {
        Err("backup restore SHA-256 is invalid".to_string())
    }
}

fn backup_restore_path_key(path: &Path) -> String {
    let path = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        path.to_ascii_lowercase()
    } else {
        path
    }
}

enum BackupRestoreResolvedMutation {
    Replace(ResolvedHandleReplace),
    Create(ResolvedHandleCreate),
    Delete(ResolvedHandleDelete),
}

impl BackupRestoreResolvedMutation {
    fn cleanup_after_durable_terminal(self) -> Result<(), String> {
        match self {
            Self::Replace(value) => {
                drop(
                    value
                        .cleanup_after_durable_terminal()
                        .map_err(|(error, _)| error)?,
                );
            }
            Self::Create(value) => {
                drop(
                    value
                        .cleanup_after_durable_terminal()
                        .map_err(|(error, _)| error)?,
                );
            }
            Self::Delete(value) => {
                drop(
                    value
                        .cleanup_after_durable_terminal()
                        .map_err(|(error, _)| error)?,
                );
            }
        }
        Ok(())
    }
}

fn transition_backup_restore_mutation_phase(
    data_root: &Path,
    journal: &mut BackupRestoreJournal,
    index: usize,
    phase: BackupRestoreMutationPhase,
) -> Result<(), String> {
    transition_backup_restore_mutation(data_root, journal, index, phase, None, None, None)
}

/// Restores one verified managed backup through a durable, identity-bound
/// operation journal. Every live create/replace/delete remains recoverable
/// until the committed or rolled-back terminal has been persisted.
pub fn restore_backup_with_recovery(
    backup_dir: &Path,
    target_home: &Path,
    data_root: &Path,
    operation_id: &str,
) -> Result<RestoreResult, String> {
    let data_root = validate_backup_restore_data_root(data_root)?;
    let mut prepared = prepare_backup_restore_plan(backup_dir, target_home, operation_id)?;
    create_backup_restore_journal(&data_root, &prepared.journal)?;

    if let Err(error) =
        stage_backup_restore_witnesses(&data_root, &prepared.manifest, &mut prepared.journal)
    {
        let rollback = rollback_backup_restore_operation(&data_root, &mut prepared.journal);
        return match rollback {
            Ok(()) => Err(format!("{error}; backup restore rolled back")),
            Err(rollback_error) => Err(format!(
                "{error}; backup restore rollback remains pending: {rollback_error}"
            )),
        };
    }

    let resolved = match apply_backup_restore_mutations(&data_root, &mut prepared.journal) {
        Ok(resolved) => resolved,
        Err(error) => {
            let rollback = rollback_backup_restore_operation(&data_root, &mut prepared.journal);
            return match rollback {
                Ok(()) => Err(format!("{error}; backup restore rolled back")),
                Err(rollback_error) => Err(format!(
                    "{error}; backup restore rollback remains pending: {rollback_error}"
                )),
            };
        }
    };

    transition_backup_restore_operation(
        &data_root,
        &mut prepared.journal,
        BackupRestoreOperationPhase::Validating,
    )?;
    if let Err(error) = validate_backup_restore_applied(&prepared.manifest, &prepared.journal) {
        drop(resolved);
        let rollback = rollback_backup_restore_operation(&data_root, &mut prepared.journal);
        return match rollback {
            Ok(()) => Err(format!("{error}; backup restore rolled back")),
            Err(rollback_error) => Err(format!(
                "{error}; backup restore rollback remains pending: {rollback_error}"
            )),
        };
    }
    transition_backup_restore_operation(
        &data_root,
        &mut prepared.journal,
        BackupRestoreOperationPhase::Committing,
    )?;
    transition_backup_restore_operation(
        &data_root,
        &mut prepared.journal,
        BackupRestoreOperationPhase::Committed,
    )?;
    cleanup_backup_restore_terminal(
        &data_root,
        &mut prepared.journal,
        Some(resolved.into_iter().enumerate().collect()),
        true,
    )?;

    Ok(RestoreResult {
        backup_dir: prepared.journal.plan.backup_dir.clone(),
        target_root: prepared.journal.plan.target_root.clone(),
        restored_files: prepared.journal.plan.restored_file_count,
        verified: true,
    })
}

fn apply_backup_restore_mutations(
    data_root: &Path,
    journal: &mut BackupRestoreJournal,
) -> Result<Vec<BackupRestoreResolvedMutation>, String> {
    if journal.phase != BackupRestoreOperationPhase::Applying {
        return Err("backup restore journal is not ready to apply".to_string());
    }
    let mut resolved = Vec::new();
    for index in 0..journal.plan.mutations.len() {
        let mutation = journal.plan.mutations[index].clone();
        let state = journal.mutation_states[index].clone();
        if state.phase != BackupRestoreMutationPhase::WitnessReady {
            return Err("backup restore mutation is not ready to apply".to_string());
        }
        transition_backup_restore_mutation_phase(
            data_root,
            journal,
            index,
            BackupRestoreMutationPhase::Preparing,
        )?;
        let value = match mutation.kind {
            BackupRestoreMutationKind::Replace => {
                let original_sha256 = mutation
                    .original_sha256
                    .as_deref()
                    .ok_or_else(|| "backup restore original hash is missing".to_string())?;
                let replacement_sha256 = mutation
                    .replacement_sha256
                    .as_deref()
                    .ok_or_else(|| "backup restore replacement hash is missing".to_string())?;
                let expected = state.replace_bindings()?;
                let paths = mutation.artifacts.replace_paths(&mutation.target_path)?;
                let mut guard = WriteExclusionGuard::acquire(&mutation.target_path)?;
                guard.verify_current_path(Some(original_sha256))?;
                if guard.identity()? != expected.original_identity {
                    return Err("backup restore original identity changed".to_string());
                }
                let staged = guard.stage_handle_hardlink_replace(
                    &mutation.artifacts.replacement_witness_path,
                    replacement_sha256,
                    &paths,
                )?;
                if staged.identity_bindings()? != expected {
                    return Err("backup restore replacement identity changed".to_string());
                }
                transition_backup_restore_mutation_phase(
                    data_root,
                    journal,
                    index,
                    BackupRestoreMutationPhase::Prepared,
                )?;
                let prepared = staged.prepare().map_err(|(error, _)| error)?;
                transition_backup_restore_mutation_phase(
                    data_root,
                    journal,
                    index,
                    BackupRestoreMutationPhase::Publishing,
                )?;
                let published = prepared.publish().map_err(|(error, _)| error)?;
                transition_backup_restore_mutation_phase(
                    data_root,
                    journal,
                    index,
                    BackupRestoreMutationPhase::Published,
                )?;
                BackupRestoreResolvedMutation::Replace(
                    published.commit().map_err(|(error, _)| error)?,
                )
            }
            BackupRestoreMutationKind::Create => {
                let replacement_sha256 = mutation
                    .replacement_sha256
                    .as_deref()
                    .ok_or_else(|| "backup restore replacement hash is missing".to_string())?;
                let expected = state.create_bindings()?;
                let paths = mutation.artifacts.create_paths(&mutation.target_path)?;
                let staged = stage_handle_hardlink_create(
                    &mutation.artifacts.replacement_witness_path,
                    replacement_sha256,
                    &paths,
                )?;
                if staged.identity_bindings()? != expected {
                    return Err("backup restore created identity changed".to_string());
                }
                transition_backup_restore_mutation_phase(
                    data_root,
                    journal,
                    index,
                    BackupRestoreMutationPhase::Prepared,
                )?;
                transition_backup_restore_mutation_phase(
                    data_root,
                    journal,
                    index,
                    BackupRestoreMutationPhase::Publishing,
                )?;
                let published = staged.publish().map_err(|(error, _)| error)?;
                transition_backup_restore_mutation_phase(
                    data_root,
                    journal,
                    index,
                    BackupRestoreMutationPhase::Published,
                )?;
                BackupRestoreResolvedMutation::Create(
                    published.commit().map_err(|(error, _)| error)?,
                )
            }
            BackupRestoreMutationKind::Delete => {
                let original_sha256 = mutation
                    .original_sha256
                    .as_deref()
                    .ok_or_else(|| "backup restore original hash is missing".to_string())?;
                let expected = state.delete_bindings()?;
                let paths = mutation.artifacts.delete_paths(&mutation.target_path)?;
                let staged = stage_handle_delete(&paths, original_sha256)?;
                if staged.identity_bindings()? != expected {
                    return Err("backup restore deleted identity changed".to_string());
                }
                transition_backup_restore_mutation_phase(
                    data_root,
                    journal,
                    index,
                    BackupRestoreMutationPhase::Prepared,
                )?;
                transition_backup_restore_mutation_phase(
                    data_root,
                    journal,
                    index,
                    BackupRestoreMutationPhase::Publishing,
                )?;
                let prepared = staged.prepare().map_err(|(error, _)| error)?;
                transition_backup_restore_mutation_phase(
                    data_root,
                    journal,
                    index,
                    BackupRestoreMutationPhase::Published,
                )?;
                BackupRestoreResolvedMutation::Delete(
                    prepared.commit().map_err(|(error, _)| error)?,
                )
            }
        };
        transition_backup_restore_mutation_phase(
            data_root,
            journal,
            index,
            BackupRestoreMutationPhase::CommittedWithRecovery,
        )?;
        resolved.push(value);
    }
    Ok(resolved)
}

fn validate_backup_restore_applied(
    manifest: &BackupManifest,
    journal: &BackupRestoreJournal,
) -> Result<(), String> {
    if verify_backup(&journal.plan.backup_dir)? != *manifest
        || backup_restore_existing_file_digest(
            &journal.plan.backup_dir.join("manifest.json"),
            "backup restore manifest",
        )?
        .1 != journal.plan.backup_manifest_sha256
    {
        return Err("backup restore source changed during apply".to_string());
    }
    for (mutation, state) in journal.plan.mutations.iter().zip(&journal.mutation_states) {
        match mutation.kind {
            BackupRestoreMutationKind::Create | BackupRestoreMutationKind::Replace => {
                let expected = mutation
                    .replacement_sha256
                    .as_deref()
                    .ok_or_else(|| "backup restore replacement hash is missing".to_string())?;
                let (_, actual) = backup_restore_existing_file_digest(
                    &mutation.target_path,
                    "applied backup restore target",
                )?;
                if actual != expected
                    || backup_restore_file_identity(&mutation.target_path)?
                        != state.replacement_identity.ok_or_else(|| {
                            "backup restore replacement identity is missing".to_string()
                        })?
                {
                    return Err("applied backup restore target changed".to_string());
                }
                validate_backup_restore_readback(mutation)?;
            }
            BackupRestoreMutationKind::Delete => {
                if backup_restore_optional_file_digest(
                    &mutation.target_path,
                    "deleted backup restore target",
                )?
                .is_some()
                {
                    return Err("deleted backup restore target reappeared".to_string());
                }
                let expected = mutation
                    .original_sha256
                    .as_deref()
                    .ok_or_else(|| "backup restore original hash is missing".to_string())?;
                let (_, actual) = backup_restore_existing_file_digest(
                    &mutation.artifacts.recovery_path,
                    "backup restore delete recovery",
                )?;
                if actual != expected
                    || backup_restore_file_identity(&mutation.artifacts.recovery_path)?
                        != state.original_identity.ok_or_else(|| {
                            "backup restore original identity is missing".to_string()
                        })?
                {
                    return Err("backup restore delete recovery changed".to_string());
                }
            }
        }
    }
    validate_backup_restore_session_inventory(manifest, journal)
}

fn validate_backup_restore_readback(mutation: &BackupRestoreMutationPlan) -> Result<(), String> {
    if mutation.sqlite {
        quick_check_sqlite(&mutation.target_path, "restored SQLite")?;
    }
    if mutation.logical_path == Path::new("config.toml") {
        let bytes = fs::read(&mutation.target_path)
            .map_err(|_| "restored config.toml is unavailable".to_string())?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| "restored config.toml is not UTF-8".to_string())?;
        DocumentMut::from_str(text).map_err(|_| "restored config.toml is invalid".to_string())?;
    } else if mutation.logical_path == Path::new("auth.json") {
        let bytes = fs::read(&mutation.target_path)
            .map_err(|_| "restored auth.json is unavailable".to_string())?;
        serde_json::from_slice::<serde_json::Value>(&bytes)
            .map_err(|_| "restored auth.json is invalid".to_string())?;
    }
    Ok(())
}

fn validate_backup_restore_session_inventory(
    manifest: &BackupManifest,
    journal: &BackupRestoreJournal,
) -> Result<(), String> {
    let scope = manifest_scope(manifest);
    for (relative_root, tracked) in [
        (
            "sessions",
            scope.tracks_sessions() && manifest.complete_sessions,
        ),
        (
            "archived_sessions",
            scope.tracks_archived_sessions() && manifest.complete_sessions,
        ),
    ] {
        if !tracked {
            continue;
        }
        let root = journal.plan.target_root.join(relative_root);
        let actual = if root.exists() {
            walk_jsonl_files(&root)?
                .into_iter()
                .map(|path| backup_restore_path_key(&path))
                .collect::<BTreeSet<_>>()
        } else {
            BTreeSet::new()
        };
        let expected = journal
            .plan
            .mutations
            .iter()
            .filter(|mutation| {
                mutation.replacement_sha256.is_some()
                    && mutation.logical_path.starts_with(relative_root)
                    && mutation
                        .logical_path
                        .extension()
                        .and_then(|value| value.to_str())
                        == Some("jsonl")
            })
            .map(|mutation| backup_restore_path_key(&mutation.target_path))
            .collect::<BTreeSet<_>>();
        if actual != expected {
            return Err("backup restore session inventory changed during apply".to_string());
        }
    }
    Ok(())
}

fn rollback_backup_restore_operation(
    data_root: &Path,
    journal: &mut BackupRestoreJournal,
) -> Result<(), String> {
    if !matches!(
        journal.phase,
        BackupRestoreOperationPhase::RollingBack | BackupRestoreOperationPhase::RolledBack
    ) {
        transition_backup_restore_operation(
            data_root,
            journal,
            BackupRestoreOperationPhase::RollingBack,
        )?;
    }
    let mut resolved = BTreeMap::new();
    for index in (0..journal.plan.mutations.len()).rev() {
        let mutation = journal.plan.mutations[index].clone();
        let state = journal.mutation_states[index].clone();
        if matches!(
            state.phase,
            BackupRestoreMutationPhase::RolledBack | BackupRestoreMutationPhase::Cleaned
        ) {
            continue;
        }
        if state.parent_identity.is_none() {
            ensure_unowned_backup_restore_mutation_unchanged(&mutation)?;
            transition_backup_restore_mutation_phase(
                data_root,
                journal,
                index,
                BackupRestoreMutationPhase::RollbackPreparing,
            )?;
            transition_backup_restore_mutation_phase(
                data_root,
                journal,
                index,
                BackupRestoreMutationPhase::RolledBack,
            )?;
            continue;
        }
        transition_backup_restore_mutation_phase(
            data_root,
            journal,
            index,
            BackupRestoreMutationPhase::RollbackPreparing,
        )?;
        let value = recover_backup_restore_mutation(&mutation, &state, false)?;
        transition_backup_restore_mutation_phase(
            data_root,
            journal,
            index,
            BackupRestoreMutationPhase::RolledBack,
        )?;
        resolved.insert(index, value);
    }
    if journal.phase != BackupRestoreOperationPhase::RolledBack {
        transition_backup_restore_operation(
            data_root,
            journal,
            BackupRestoreOperationPhase::RolledBack,
        )?;
    }
    cleanup_backup_restore_terminal(data_root, journal, Some(resolved), false)
}

fn ensure_unowned_backup_restore_mutation_unchanged(
    mutation: &BackupRestoreMutationPlan,
) -> Result<(), String> {
    for artifact in [
        &mutation.artifacts.original_witness_path,
        &mutation.artifacts.replacement_witness_path,
        &mutation.artifacts.staging_path,
        &mutation.artifacts.recovery_path,
        &mutation.artifacts.rollback_tombstone_path,
    ] {
        if fs::symlink_metadata(artifact).is_ok() {
            return Err("backup restore witness creation is incomplete".to_string());
        }
    }
    match mutation.original_sha256.as_deref() {
        Some(expected) => {
            if backup_restore_existing_file_digest(
                &mutation.target_path,
                "backup restore rollback target",
            )?
            .1 != expected
            {
                return Err("backup restore rollback target changed".to_string());
            }
        }
        None => {
            if backup_restore_optional_file_digest(
                &mutation.target_path,
                "backup restore rollback target",
            )?
            .is_some()
            {
                return Err("backup restore rollback target appeared".to_string());
            }
        }
    }
    Ok(())
}

fn recover_backup_restore_mutation(
    mutation: &BackupRestoreMutationPlan,
    state: &BackupRestoreMutationState,
    commit: bool,
) -> Result<BackupRestoreResolvedMutation, String> {
    match mutation.kind {
        BackupRestoreMutationKind::Replace => Ok(BackupRestoreResolvedMutation::Replace(
            recover_handle_replace(
                &mutation.artifacts.replace_paths(&mutation.target_path)?,
                state.replace_bindings()?,
                mutation
                    .original_sha256
                    .as_deref()
                    .ok_or_else(|| "backup restore original hash is missing".to_string())?,
                mutation
                    .replacement_sha256
                    .as_deref()
                    .ok_or_else(|| "backup restore replacement hash is missing".to_string())?,
                if commit {
                    HandleReplaceRecoveryDecision::Commit
                } else {
                    HandleReplaceRecoveryDecision::Restore
                },
            )?,
        )),
        BackupRestoreMutationKind::Create => Ok(BackupRestoreResolvedMutation::Create(
            recover_handle_create(
                &mutation.artifacts.create_paths(&mutation.target_path)?,
                state.create_bindings()?,
                mutation
                    .replacement_sha256
                    .as_deref()
                    .ok_or_else(|| "backup restore replacement hash is missing".to_string())?,
                if commit {
                    HandleCreateRecoveryDecision::Commit
                } else {
                    HandleCreateRecoveryDecision::Restore
                },
            )?,
        )),
        BackupRestoreMutationKind::Delete => Ok(BackupRestoreResolvedMutation::Delete(
            recover_handle_delete(
                &mutation.artifacts.delete_paths(&mutation.target_path)?,
                state.delete_bindings()?,
                mutation
                    .original_sha256
                    .as_deref()
                    .ok_or_else(|| "backup restore original hash is missing".to_string())?,
                if commit {
                    HandleDeleteRecoveryDecision::Commit
                } else {
                    HandleDeleteRecoveryDecision::Restore
                },
            )?,
        )),
    }
}

fn cleanup_backup_restore_terminal(
    data_root: &Path,
    journal: &mut BackupRestoreJournal,
    resolved: Option<BTreeMap<usize, BackupRestoreResolvedMutation>>,
    committed: bool,
) -> Result<(), String> {
    let mut resolved = resolved.unwrap_or_default();
    for index in 0..journal.plan.mutations.len() {
        if journal.mutation_states[index].phase == BackupRestoreMutationPhase::Cleaned {
            continue;
        }
        let mutation = journal.plan.mutations[index].clone();
        let state = journal.mutation_states[index].clone();
        let value = match resolved.remove(&index) {
            Some(value) => Some(value),
            None => match recover_backup_restore_mutation(&mutation, &state, committed) {
                Ok(value) => Some(value),
                Err(_error)
                    if committed
                        && mutation.kind == BackupRestoreMutationKind::Replace
                        && backup_restore_committed_replace_was_already_cleaned(
                            &mutation, &state,
                        )? =>
                {
                    None
                }
                Err(error) => return Err(error),
            },
        };
        if let Some(value) = value {
            value.cleanup_after_durable_terminal()?;
        }
        cleanup_backup_restore_witnesses(&mutation, &state)?;
        transition_backup_restore_mutation_phase(
            data_root,
            journal,
            index,
            BackupRestoreMutationPhase::Cleaned,
        )?;
    }
    transition_backup_restore_operation(
        data_root,
        journal,
        if committed {
            BackupRestoreOperationPhase::CommittedCleanupComplete
        } else {
            BackupRestoreOperationPhase::RolledBackCleanupComplete
        },
    )
}

fn backup_restore_committed_replace_was_already_cleaned(
    mutation: &BackupRestoreMutationPlan,
    state: &BackupRestoreMutationState,
) -> Result<bool, String> {
    let replacement_sha256 = mutation
        .replacement_sha256
        .as_deref()
        .ok_or_else(|| "backup restore replacement hash is missing".to_string())?;
    let target = backup_restore_optional_file_digest(
        &mutation.target_path,
        "committed backup restore target",
    )?;
    if target.as_ref().map(|(_, digest)| digest.as_str()) != Some(replacement_sha256)
        || backup_restore_file_identity(&mutation.target_path)?
            != state
                .replacement_identity
                .ok_or_else(|| "backup restore replacement identity is missing".to_string())?
    {
        return Ok(false);
    }
    for path in [
        &mutation.artifacts.staging_path,
        &mutation.artifacts.recovery_path,
        &mutation.artifacts.rollback_tombstone_path,
    ] {
        if fs::symlink_metadata(path).is_ok() {
            return Ok(false);
        }
    }
    Ok(true)
}

fn cleanup_backup_restore_witnesses(
    mutation: &BackupRestoreMutationPlan,
    state: &BackupRestoreMutationState,
) -> Result<(), String> {
    if let (Some(expected_sha256), Some(identity)) =
        (mutation.original_sha256.as_deref(), state.original_identity)
    {
        cleanup_backup_restore_witness(
            &mutation.artifacts.original_witness_path,
            expected_sha256,
            identity,
        )?;
    }
    if let (Some(expected_sha256), Some(identity)) = (
        mutation.replacement_sha256.as_deref(),
        state.replacement_identity,
    ) {
        cleanup_backup_restore_witness(
            &mutation.artifacts.replacement_witness_path,
            expected_sha256,
            identity,
        )?;
    }
    Ok(())
}

fn cleanup_backup_restore_witness(
    path: &Path,
    expected_sha256: &str,
    expected_identity: RegularFileIdentity,
) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err("backup restore witness is unavailable".to_string()),
        Ok(metadata) if !metadata.is_file() || backup_metadata_is_link_or_reparse(&metadata) => {
            return Err("backup restore witness is unsafe".to_string())
        }
        Ok(_) => {}
    }
    let mut guard = crate::session_storage::write_barrier::DestructiveFileGuard::acquire(path)?;
    if guard.identity()? != expected_identity {
        return Err("backup restore witness identity changed".to_string());
    }
    guard.verify_current_path(Some(expected_sha256))?;
    guard.delete()
}

/// Reconciles every direct-child restore journal for the current/shared
/// managed roots. Pre-commit work always rolls back; durably committed work
/// only completes exact artifact cleanup.
pub fn recover_pending_backup_restores(
    data_root: &Path,
    current_home: &Path,
    shared_home: &Path,
) -> Result<BackupRestoreRecoveryReceipt, String> {
    let data_root = validate_backup_restore_data_root(data_root)?;
    let current_home = validate_absolute_root(current_home, "current restore root")?;
    let shared_home = validate_absolute_root(shared_home, "shared restore root")?;
    let operations_root = data_root.join(BACKUP_RESTORE_OPERATION_DIRECTORY);
    match fs::symlink_metadata(&operations_root) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(BackupRestoreRecoveryReceipt::default())
        }
        Err(_) => return Err("backup restore operation inventory is unavailable".to_string()),
        Ok(metadata) if !metadata.is_dir() || backup_metadata_is_link_or_reparse(&metadata) => {
            return Err("backup restore operation inventory is unsafe".to_string())
        }
        Ok(_) => {}
    }
    let mut entries = fs::read_dir(&operations_root)
        .map_err(|_| "backup restore operation inventory is unavailable".to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "backup restore operation inventory is unavailable".to_string())?;
    entries.sort_by_key(|entry| entry.file_name());
    let mut receipt = BackupRestoreRecoveryReceipt::default();
    for entry in entries {
        receipt.discovered_operation_count += 1;
        let operation_id = match entry.file_name().to_str() {
            Some(value) if validate_backup_restore_operation_id(value).is_ok() => value.to_string(),
            _ => {
                receipt.blocked_operation_count += 1;
                continue;
            }
        };
        let metadata = match fs::symlink_metadata(entry.path()) {
            Ok(metadata) if metadata.is_dir() && !backup_metadata_is_link_or_reparse(&metadata) => {
                metadata
            }
            _ => {
                receipt.blocked_operation_count += 1;
                continue;
            }
        };
        let _ = metadata;
        let mut journal = match load_backup_restore_journal(&data_root, &operation_id) {
            Ok(journal) => journal,
            Err(_) => {
                receipt.blocked_operation_count += 1;
                continue;
            }
        };
        let target_key = backup_restore_path_key(&journal.plan.target_root);
        if target_key != backup_restore_path_key(&current_home)
            && target_key != backup_restore_path_key(&shared_home)
        {
            receipt.blocked_operation_count += 1;
            continue;
        }
        let recovered = match journal.phase {
            BackupRestoreOperationPhase::CommittedCleanupComplete
            | BackupRestoreOperationPhase::RolledBackCleanupComplete => {
                receipt.already_terminal_count += 1;
                Ok(())
            }
            BackupRestoreOperationPhase::Committed => {
                cleanup_backup_restore_terminal(&data_root, &mut journal, None, true).map(|()| {
                    receipt.committed_cleanup_count += 1;
                })
            }
            BackupRestoreOperationPhase::RolledBack => {
                cleanup_backup_restore_terminal(&data_root, &mut journal, None, false).map(|()| {
                    receipt.rolled_back_operation_count += 1;
                })
            }
            _ => rollback_backup_restore_operation(&data_root, &mut journal).map(|()| {
                receipt.rolled_back_operation_count += 1;
            }),
        };
        if recovered.is_err() {
            receipt.blocked_operation_count += 1;
        }
    }
    Ok(receipt)
}

#[cfg(test)]
pub fn restore_backup(backup_dir: &Path, target_home: &Path) -> Result<RestoreResult, String> {
    validate_absolute_root(backup_dir, "backup directory")?;
    validate_absolute_root(target_home, "restore target root")?;
    ensure_roots_disjoint(
        backup_dir,
        "backup directory",
        target_home,
        "restore target root",
    )?;
    let manifest = verify_backup(backup_dir)?;
    restore_verified_backup(backup_dir, target_home, &manifest)
}

#[cfg(test)]
fn restore_verified_backup(
    backup_dir: &Path,
    target_home: &Path,
    manifest: &BackupManifest,
) -> Result<RestoreResult, String> {
    let old_paths = if manifest.state_db_is_local {
        local_codex_paths(target_home)
    } else {
        resolve_user_codex_paths(target_home)?
    };
    ensure_roots_disjoint(
        backup_dir,
        "backup directory",
        &old_paths.sqlite_home,
        "SQLite root",
    )?;
    if !manifest.root_existed {
        clear_known_codex_state(target_home, &old_paths, manifest)?;
        return Ok(RestoreResult {
            backup_dir: backup_dir.to_path_buf(),
            target_root: target_home.to_path_buf(),
            restored_files: 0,
            verified: true,
        });
    }

    let mut staged = stage_backup_payloads(backup_dir, target_home, manifest)?;
    restore_staged_backup(backup_dir, target_home, manifest, &old_paths, &mut staged)
}

#[cfg(test)]
fn restore_staged_backup(
    backup_dir: &Path,
    target_home: &Path,
    manifest: &BackupManifest,
    old_paths: &CodexPaths,
    staged: &mut RestoreStage,
) -> Result<RestoreResult, String> {
    fs::create_dir_all(target_home)
        .map_err(|error| format!("failed to create restore target: {error}"))?;
    remove_absent_core_files(manifest, target_home, old_paths)?;
    if manifest.complete_sessions && manifest_scope(manifest).tracks_sessions() {
        remove_extra_session_files(manifest, target_home)?;
    }

    let mut restored_files = 0;
    if let Some((index, _)) = manifest
        .files
        .iter()
        .enumerate()
        .find(|(_, file)| file.relative_path == Path::new("config.toml"))
    {
        restore_staged_file(
            staged,
            index,
            Path::new("config.toml"),
            &target_home.join("config.toml"),
        )?;
        restored_files += 1;
    }

    let paths = if manifest.state_db_is_local {
        local_codex_paths(target_home)
    } else {
        resolve_user_codex_paths(target_home)?
    };
    ensure_roots_disjoint(
        backup_dir,
        "backup directory",
        &paths.sqlite_home,
        "SQLite root",
    )?;
    remove_absent_core_files(manifest, target_home, &paths)?;
    for (relative, old_database) in managed_sqlite_paths(old_paths) {
        if !manifest_tracks_database(manifest, relative) {
            continue;
        }
        let new_database = sqlite_restore_target(&paths, relative)
            .expect("managed SQLite relative paths must be routable");
        if old_database != new_database {
            remove_sqlite_files(old_database)?;
        }
    }
    for (index, file) in manifest.files.iter().enumerate() {
        if file.relative_path == Path::new("config.toml") {
            continue;
        }
        let target = restore_target(&paths, target_home, &file.relative_path)?;
        restore_staged_file(staged, index, &file.relative_path, &target)?;
        restored_files += 1;
    }
    for (relative, database) in managed_sqlite_paths(&paths) {
        if !manifest_tracks_database(manifest, relative) {
            continue;
        }
        remove_sqlite_sidecars(database)?;
        if database.exists() {
            quick_check_sqlite(database, &format!("restored {relative}"))?;
        }
    }

    Ok(RestoreResult {
        backup_dir: backup_dir.to_path_buf(),
        target_root: target_home.to_path_buf(),
        restored_files,
        verified: true,
    })
}

#[cfg(test)]
struct RestoreStage {
    root: PathBuf,
    files: Vec<StagedBackupFile>,
}

#[cfg(test)]
impl RestoreStage {
    fn create(target_home: &Path) -> Result<Self, String> {
        let parent = target_home
            .parent()
            .filter(|path| path.is_dir())
            .map(Path::to_path_buf)
            .unwrap_or_else(std::env::temp_dir);
        let parent = fs::canonicalize(&parent)
            .map_err(|error| format!("failed to resolve restore staging parent: {error}"))?;
        let created_at_ms = timestamp_millis()?;
        for _ in 0..64 {
            let candidate = parent.join(format!(
                ".codex-switch-restore-{}-{}-{}",
                created_at_ms,
                std::process::id(),
                BACKUP_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&candidate) {
                Ok(()) => {
                    return Ok(Self {
                        root: candidate,
                        files: Vec::new(),
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(format!(
                        "failed to create restore staging directory: {error}"
                    ));
                }
            }
        }
        Err("failed to allocate a unique restore staging directory".to_string())
    }
}

#[cfg(test)]
impl Drop for RestoreStage {
    fn drop(&mut self) {
        self.files.clear();
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[cfg(test)]
struct StagedBackupFile {
    relative_path: PathBuf,
    plaintext_bytes: u64,
    plaintext_sha256: String,
    handle: fs::File,
}

#[cfg(test)]
fn stage_backup_payloads(
    backup_dir: &Path,
    target_home: &Path,
    manifest: &BackupManifest,
) -> Result<RestoreStage, String> {
    let canonical_root = fs::canonicalize(backup_dir)
        .map_err(|error| format!("failed to resolve backup directory: {error}"))?;
    let mut stage = RestoreStage::create(target_home)?;
    for (index, file) in manifest.files.iter().enumerate() {
        let canonical_payload = fs::canonicalize(&file.backup_path)
            .map_err(|error| format!("backup payload is missing: {error}"))?;
        if !canonical_payload.starts_with(&canonical_root) {
            return Err("backup payload escaped the backup directory".to_string());
        }
        let mut source = open_restore_file(&canonical_payload, false)?;
        let metadata = source
            .metadata()
            .map_err(|error| format!("failed to inspect backup payload: {error}"))?;
        if !metadata.is_file() || metadata.len() != file.bytes {
            return Err(format!(
                "backup payload size mismatch: {}",
                file.relative_path.display()
            ));
        }
        let mut encrypted = Vec::new();
        source
            .read_to_end(&mut encrypted)
            .map_err(|error| format!("failed to read backup payload: {error}"))?;
        let encrypted_bytes = u64::try_from(encrypted.len())
            .map_err(|_| "backup payload size overflow".to_string())?;
        if encrypted_bytes != file.bytes
            || format!("{:x}", Sha256::digest(&encrypted)) != file.sha256
        {
            return Err(format!(
                "backup payload checksum mismatch: {}",
                file.relative_path.display()
            ));
        }
        if !file.encrypted {
            return Err("unencrypted payloads are not restorable".to_string());
        }
        let plaintext = unprotect(&encrypted)?;
        let plaintext_bytes = u64::try_from(plaintext.len())
            .map_err(|_| "staged backup payload size overflow".to_string())?;
        if file.relative_path == Path::new(CHAT_PROCESS_STATE_RELATIVE_PATH) {
            validate_chat_process_state_bytes(plaintext_bytes)?;
        }
        let plaintext_sha256 = format!("{:x}", Sha256::digest(&plaintext));
        let stage_path = stage.root.join(format!("{index:08}.payload"));
        let mut handle = open_restore_file(&stage_path, true)?;
        handle
            .write_all(&plaintext)
            .map_err(|error| format!("failed to stage backup payload: {error}"))?;
        handle
            .sync_all()
            .map_err(|error| format!("failed to flush staged backup payload: {error}"))?;
        handle
            .seek(SeekFrom::Start(0))
            .map_err(|error| format!("failed to rewind staged backup payload: {error}"))?;
        fs::remove_file(&stage_path)
            .map_err(|error| format!("failed to unlink staged backup payload: {error}"))?;
        stage.files.push(StagedBackupFile {
            relative_path: file.relative_path.clone(),
            plaintext_bytes,
            plaintext_sha256,
            handle,
        });
    }
    Ok(stage)
}

#[cfg(test)]
fn open_restore_file(path: &Path, create_new: bool) -> Result<fs::File, String> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    if create_new {
        options.write(true).create_new(true);
    }
    #[cfg(windows)]
    options.share_mode(if create_new { FILE_SHARE_DELETE } else { 0 });
    options.open(path).map_err(|error| {
        if create_new {
            format!("failed to create staged backup payload: {error}")
        } else {
            format!("failed to open backup payload for staging: {error}")
        }
    })
}

#[cfg(test)]
fn restore_staged_file(
    stage: &mut RestoreStage,
    index: usize,
    relative_path: &Path,
    target: &Path,
) -> Result<(), String> {
    let file = stage
        .files
        .get_mut(index)
        .ok_or_else(|| "staged backup payload set is incomplete".to_string())?;
    if file.relative_path != relative_path {
        return Err("staged backup payload order changed".to_string());
    }
    file.handle
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("failed to rewind staged backup payload: {error}"))?;
    let expected_bytes = file.plaintext_bytes;
    let expected_sha256 = file.plaintext_sha256.clone();
    atomic_rewrite(target, |target_file| {
        let mut bytes = 0_u64;
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file
                .handle
                .read(&mut buffer)
                .map_err(|error| format!("failed to read staged backup payload: {error}"))?;
            if read == 0 {
                break;
            }
            target_file
                .write_all(&buffer[..read])
                .map_err(|error| format!("failed to restore staged backup payload: {error}"))?;
            hasher.update(&buffer[..read]);
            bytes = bytes
                .checked_add(
                    u64::try_from(read)
                        .map_err(|_| "staged backup payload size overflow".to_string())?,
                )
                .ok_or_else(|| "staged backup payload size overflow".to_string())?;
        }
        if bytes != expected_bytes || format!("{:x}", hasher.finalize()) != expected_sha256 {
            return Err(format!(
                "staged backup payload changed: {}",
                relative_path.display()
            ));
        }
        Ok(())
    })
}

#[cfg(test)]
fn sqlite_sidecar(database: &Path, suffix: &str) -> PathBuf {
    let mut name = database.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    database.with_file_name(name)
}

#[cfg(test)]
fn remove_sqlite_sidecars(database: &Path) -> Result<(), String> {
    for suffix in ["-wal", "-shm"] {
        let sidecar = sqlite_sidecar(database, suffix);
        if sidecar.is_file() {
            fs::remove_file(&sidecar).map_err(|error| {
                format!(
                    "failed to remove stale SQLite sidecar {}: {error}",
                    sidecar.display()
                )
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
fn remove_sqlite_files(database: &Path) -> Result<(), String> {
    if database.is_file() {
        fs::remove_file(database).map_err(|error| {
            format!(
                "failed to remove previous SQLite database {}: {error}",
                database.display()
            )
        })?;
    }
    remove_sqlite_sidecars(database)
}

#[cfg(test)]
fn remove_absent_core_files(
    manifest: &BackupManifest,
    target_home: &Path,
    paths: &CodexPaths,
) -> Result<(), String> {
    let expected = manifest
        .files
        .iter()
        .map(|file| file.relative_path.as_path())
        .collect::<HashSet<_>>();
    let scope = manifest_scope(manifest);
    let mut tracked_files = Vec::new();
    if scope.tracks_runtime_files() {
        tracked_files.push((Path::new("auth.json"), target_home.join("auth.json")));
        tracked_files.push((Path::new("config.toml"), target_home.join("config.toml")));
    }
    if manifest.tracked_process_state {
        if let Some(target) = existing_chat_process_state_restore_target(target_home)? {
            tracked_files.push((Path::new(CHAT_PROCESS_STATE_RELATIVE_PATH), target));
        }
    }
    if scope.tracks_sessions() {
        tracked_files.push((
            Path::new("session_index.jsonl"),
            target_home.join("session_index.jsonl"),
        ));
    }
    for (relative, target) in tracked_files {
        if !expected.contains(relative) && target.is_file() {
            fs::remove_file(&target).map_err(|error| {
                format!(
                    "failed to remove post-backup file {}: {error}",
                    target.display()
                )
            })?;
        }
    }
    for (relative, database) in managed_sqlite_paths(paths) {
        if manifest_tracks_database(manifest, relative) && !expected.contains(Path::new(relative)) {
            remove_sqlite_files(database)?;
        }
    }
    Ok(())
}

pub fn list_recent_backups(
    destination_root: &Path,
    verification_limit: usize,
) -> Result<Vec<BackupSummary>, String> {
    if !destination_root.exists() || verification_limit == 0 {
        return Ok(Vec::new());
    }
    let mut candidates = Vec::new();
    for entry in fs::read_dir(destination_root)
        .map_err(|error| format!("failed to list backup directory: {error}"))?
    {
        let entry = entry.map_err(|error| format!("failed to read backup entry: {error}"))?;
        if !entry
            .file_type()
            .map_err(|error| format!("failed to inspect backup entry: {error}"))?
            .is_dir()
        {
            continue;
        }
        let backup_dir = entry.path();
        let Ok(manifest) = read_backup_manifest(&backup_dir) else {
            continue;
        };
        if manifest.version != 2 && manifest.scope != BackupScope::Full {
            continue;
        }
        candidates.push((backup_dir, manifest.created_at_ms));
    }
    candidates.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| right.0.cmp(&left.0)));

    let mut summaries = Vec::new();
    for (backup_dir, _) in candidates {
        let Ok(verified) = verify_managed_full_backup(destination_root, &backup_dir) else {
            continue;
        };
        let manifest = verified.manifest;
        summaries.push(BackupSummary {
            backup_dir,
            source_root: manifest.source_root,
            reason: manifest.reason,
            created_at_ms: manifest.created_at_ms,
            file_count: manifest.files.len(),
            total_bytes: manifest.files.iter().map(|file| file.bytes).sum(),
            verified: true,
            complete_sessions: manifest.complete_sessions,
        });
        if summaries.len() == verification_limit {
            break;
        }
    }
    Ok(summaries)
}

fn snapshot_sqlite(
    source: &Path,
    backup_dir: &Path,
    relative_path: &Path,
) -> Result<BackupFile, String> {
    let file_name = relative_path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| "SQLite backup path is invalid".to_string())?;
    let snapshot = backup_dir.join(format!(".{file_name}.snapshot"));
    let source_conn = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| format!("failed to open {file_name} for backup: {error}"))?;
    source_conn
        .backup(MAIN_DB, &snapshot, None)
        .map_err(|error| format!("failed to create SQLite backup snapshot: {error}"))?;
    let result = encrypt_payload(&snapshot, backup_dir, relative_path);
    let _ = fs::remove_file(snapshot);
    result.map(|mut file| {
        file.source = source.to_path_buf();
        file
    })
}

fn encrypt_payload(
    source: &Path,
    backup_dir: &Path,
    relative_path: &Path,
) -> Result<BackupFile, String> {
    validate_relative_path(relative_path)?;
    let source_bytes = fs::metadata(source)
        .map_err(|error| format!("failed to inspect backup source file: {error}"))?
        .len();
    ensure_encryptable_payload_size(source_bytes)
        .map_err(|_| "backup payload exceeds the DPAPI size limit".to_string())?;
    let plaintext =
        fs::read(source).map_err(|error| format!("failed to read backup source file: {error}"))?;
    let plaintext_bytes = u64::try_from(plaintext.len())
        .map_err(|_| "backup payload exceeds the DPAPI size limit".to_string())?;
    ensure_encryptable_payload_size(plaintext_bytes)
        .map_err(|_| "backup payload exceeds the DPAPI size limit".to_string())?;
    encrypt_payload_bytes(&plaintext, source, backup_dir, relative_path)
}

fn encrypt_payload_bytes(
    plaintext: &[u8],
    source: &Path,
    backup_dir: &Path,
    relative_path: &Path,
) -> Result<BackupFile, String> {
    validate_relative_path(relative_path)?;
    let plaintext_bytes = u64::try_from(plaintext.len())
        .map_err(|_| "backup payload exceeds the DPAPI size limit".to_string())?;
    ensure_encryptable_payload_size(plaintext_bytes)
        .map_err(|_| "backup payload exceeds the DPAPI size limit".to_string())?;
    let encrypted = protect(plaintext)?;
    let backup_path = encrypted_payload_path(backup_dir, relative_path)?;
    atomic_write(&backup_path, &encrypted)?;
    let bytes = fs::metadata(&backup_path)
        .map_err(|error| format!("failed to inspect encrypted backup payload: {error}"))?
        .len();
    Ok(BackupFile {
        source: source.to_path_buf(),
        relative_path: relative_path.to_path_buf(),
        backup_path: backup_path.clone(),
        bytes,
        sha256: sha256_file(&backup_path)?,
        encrypted: true,
    })
}

fn encrypted_payload_path(backup_dir: &Path, relative_path: &Path) -> Result<PathBuf, String> {
    let file_name = relative_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "backup relative path must include a UTF-8 file name".to_string())?;
    let parent = relative_path.parent().unwrap_or_else(|| Path::new(""));
    Ok(backup_dir
        .join("payload")
        .join(parent)
        .join(format!("{file_name}.enc")))
}

fn restore_target(
    paths: &crate::codex_paths::CodexPaths,
    target_home: &Path,
    relative_path: &Path,
) -> Result<PathBuf, String> {
    validate_relative_path(relative_path)?;
    if let Some(target) = sqlite_restore_target(paths, relative_path.to_string_lossy().as_ref()) {
        return Ok(target.to_path_buf());
    }
    if relative_path == Path::new(CHAT_PROCESS_STATE_RELATIVE_PATH) {
        return chat_process_state_restore_target(target_home);
    }
    Ok(target_home.join(relative_path))
}

#[cfg(test)]
fn remove_extra_session_files(manifest: &BackupManifest, target_home: &Path) -> Result<(), String> {
    let scope = manifest_scope(manifest);
    let mut roots = vec![Path::new("sessions")];
    if manifest.version >= SCOPED_BACKUP_MANIFEST_VERSION && scope.tracks_archived_sessions() {
        roots.push(Path::new("archived_sessions"));
    }
    for relative_root in roots {
        let expected = manifest
            .files
            .iter()
            .filter(|file| file.relative_path.starts_with(relative_root))
            .map(|file| target_home.join(&file.relative_path))
            .collect::<HashSet<_>>();
        let root = target_home.join(relative_root);
        if !root.exists() {
            continue;
        }
        for path in walk_jsonl_files(&root)? {
            if !expected.contains(&path) {
                fs::remove_file(&path).map_err(|error| {
                    format!("failed to remove post-backup session file: {error}")
                })?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
fn clear_known_codex_state(
    target_home: &Path,
    paths: &CodexPaths,
    manifest: &BackupManifest,
) -> Result<(), String> {
    let scope = manifest_scope(manifest);
    let mut tracked_files = Vec::new();
    if scope.tracks_runtime_files() {
        tracked_files.push(target_home.join("auth.json"));
        tracked_files.push(target_home.join("config.toml"));
    }
    if manifest.tracked_process_state {
        if let Some(target) = existing_chat_process_state_restore_target(target_home)? {
            tracked_files.push(target);
        }
    }
    if scope.tracks_sessions() {
        tracked_files.push(paths.session_index.clone());
    }
    for path in tracked_files {
        if path.is_file() {
            fs::remove_file(&path).map_err(|error| {
                format!("failed to clear restored file {}: {error}", path.display())
            })?;
        }
    }
    for (relative, database) in managed_sqlite_paths(paths) {
        if manifest_tracks_database(manifest, relative) {
            remove_sqlite_files(database)?;
        }
    }
    if scope.tracks_sessions() {
        let sessions = target_home.join("sessions");
        if sessions.is_dir() {
            fs::remove_dir_all(&sessions)
                .map_err(|error| format!("failed to clear restored sessions directory: {error}"))?;
        }
        if manifest.version >= SCOPED_BACKUP_MANIFEST_VERSION && scope.tracks_archived_sessions() {
            let archived = target_home.join("archived_sessions");
            if archived.is_dir() {
                for path in walk_jsonl_files(&archived)? {
                    fs::remove_file(&path).map_err(|error| {
                        format!("failed to clear restored archived session file: {error}")
                    })?;
                }
            }
        }
    }
    Ok(())
}

fn managed_sqlite_paths(paths: &CodexPaths) -> [(&'static str, &Path); 4] {
    [
        ("state_5.sqlite", paths.state_db.as_path()),
        ("goals_1.sqlite", paths.goals_db.as_path()),
        ("memories_1.sqlite", paths.memories_db.as_path()),
        ("logs_2.sqlite", paths.logs_db.as_path()),
    ]
}

fn sqlite_restore_target<'a>(paths: &'a CodexPaths, relative: &str) -> Option<&'a Path> {
    match relative {
        "state_5.sqlite" => Some(&paths.state_db),
        "goals_1.sqlite" => Some(&paths.goals_db),
        "memories_1.sqlite" => Some(&paths.memories_db),
        "logs_2.sqlite" => Some(&paths.logs_db),
        _ => None,
    }
}

fn manifest_tracks_database(manifest: &BackupManifest, relative: &str) -> bool {
    if manifest.version == 2 {
        relative == STATE_DATABASE
    } else {
        manifest
            .tracked_databases
            .iter()
            .any(|database| database == relative)
    }
}

fn quick_check_sqlite(database: &Path, label: &str) -> Result<(), String> {
    let connection = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| format!("{label} cannot be opened: {error}"))?;
    let result: String = connection
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(|error| format!("failed to verify {label}: {error}"))?;
    if result != "ok" {
        return Err(format!("{label} failed quick_check: {result}"));
    }
    Ok(())
}

pub(crate) fn migrate_legacy_plaintext_auth(destination_root: &Path) -> Result<(), String> {
    if !destination_root.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(destination_root)
        .map_err(|error| format!("failed to inspect legacy backups: {error}"))?
    {
        let entry =
            entry.map_err(|error| format!("failed to read legacy backup entry: {error}"))?;
        if !entry
            .file_type()
            .map_err(|error| format!("failed to inspect legacy backup entry: {error}"))?
            .is_dir()
        {
            continue;
        }
        let plaintext = entry.path().join("auth.json");
        if !plaintext.is_file() {
            continue;
        }
        let encrypted = protect(
            &fs::read(&plaintext)
                .map_err(|error| format!("failed to read legacy auth backup: {error}"))?,
        )?;
        atomic_write(&entry.path().join("auth.json.enc"), &encrypted)?;
        fs::remove_file(&plaintext)
            .map_err(|error| format!("failed to remove legacy plaintext auth backup: {error}"))?;
    }
    Ok(())
}

fn validate_relative_path(path: &Path) -> Result<(), String> {
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err("backup relative path is unsafe".to_string());
    }
    Ok(())
}

fn safe_reason(reason: &str) -> String {
    let value = reason
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    if value.trim_matches('-').is_empty() {
        "backup".to_string()
    } else {
        value
    }
}

fn managed_backup_directory_name_matches(path: &Path, manifest: &BackupManifest) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let mut parts = name.splitn(4, '-');
    parts.next() == Some(manifest.created_at_ms.to_string().as_str())
        && parts.next().is_some_and(|value| {
            !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
        })
        && parts.next().is_some_and(|value| {
            !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
        })
        && parts.next() == Some(safe_reason(&manifest.reason).as_str())
}

fn backup_path_key(path: &Path) -> String {
    let value = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        value.to_ascii_lowercase()
    } else {
        value
    }
}

#[cfg(windows)]
fn backup_metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn backup_metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file =
        fs::File::open(path).map_err(|error| format!("failed to hash backup file: {error}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("failed to read backup file for hashing: {error}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn timestamp_millis() -> Result<u128, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .map_err(|error| format!("system clock before unix epoch: {error}"))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        fs,
        path::Path,
    };

    use rusqlite::Connection;
    use sha2::{Digest, Sha256};
    use tempfile::tempdir;

    use crate::operation_log::{OperationAction, OperationPhase, OperationRecord, OperationStatus};

    use super::{
        add_capacity_file, available_backup_bytes, cleanup_automatic_checkpoints,
        cleanup_automatic_checkpoints_with_remove, cleanup_transient_checkpoints,
        collect_backup_capacity_metadata, create_backup, create_local_backup,
        create_runtime_backup, create_runtime_backup_with_paths,
        create_runtime_state_backup_with_paths, create_runtime_state_checkpoint_with_paths,
        create_session_backup, create_session_backup_with_paths, create_state_backup,
        create_state_checkpoint_with_paths, delete_verified_full_backup,
        ensure_encryptable_payload_size, ensure_roots_disjoint, estimate_backup_peak,
        estimate_backup_peak_with_source_count, existing_capacity_ancestor,
        extract_verified_backup_file, finish_backup_creation_with_cleanup,
        finish_capacity_preflight, inspect_checkpoint_storage, list_recent_backups,
        load_process_state_checkpoint, migrate_legacy_plaintext_auth, percentage_ceil,
        preflight_backup_capacity, preflight_backup_capacity_for_sources,
        preflight_backup_capacity_with_paths, prepare_backup_restore_plan, restore_backup,
        restore_staged_backup, restore_verified_backup, sqlite_logical_bytes,
        stage_backup_payloads, stage_backup_restore_witnesses,
        validate_backup_restore_journal_update, validate_backup_restore_plan,
        validate_directory_entry, verify_backup, BackupCapacitySource, BackupManifest,
        BackupRestoreMutationKind, BackupRestoreMutationPhase, BackupRestoreMutationPlan,
        BackupRestoreOperationPhase, BackupRestorePlan, BackupScope, BackupSourceCapacityMetadata,
        CheckpointRole, RegularFileIdentity, BACKUP_FILE_OVERHEAD_BYTES,
        BACKUP_RESTORE_JOURNAL_MAGIC, CHAT_PROCESS_STATE_RELATIVE_PATH,
        MANIFEST_BASE_OVERHEAD_BYTES, MANIFEST_ENTRY_OVERHEAD_BYTES, MAX_DPAPI_PAYLOAD_BYTES,
        MIN_CAPACITY_RESERVE_BYTES, SCOPED_BACKUP_MANIFEST_VERSION,
    };

    fn seed_home(home: &std::path::Path) -> std::path::PathBuf {
        fs::write(
            home.join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"fake-secret-token"}}"#,
        )
        .unwrap();
        fs::write(
            home.join("config.toml"),
            "model = \"gpt-5.5\"\nmodel_instructions_file = \"global\"\n",
        )
        .unwrap();
        let conn = Connection::open(home.join("state_5.sqlite")).unwrap();
        conn.execute(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT)",
            [],
        )
        .unwrap();
        let rollout = home.join("sessions/2026/07/13/rollout-thread-a.jsonl");
        fs::create_dir_all(rollout.parent().unwrap()).unwrap();
        fs::write(
            &rollout,
            concat!(
                r#"{"type":"session_meta","payload":{"id":"thread-a"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"text":"private session body"}}"#,
                "\n",
            ),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO threads (id, rollout_path) VALUES ('thread-a', ?1)",
            [rollout.to_string_lossy().to_string()],
        )
        .unwrap();
        drop(conn);
        for (database, table) in [
            ("goals_1.sqlite", "thread_goals"),
            ("memories_1.sqlite", "stage1_outputs"),
            ("logs_2.sqlite", "logs"),
        ] {
            let conn = Connection::open(home.join(database)).unwrap();
            conn.execute(
                &format!("CREATE TABLE {table} (thread_id TEXT PRIMARY KEY, value TEXT)"),
                [],
            )
            .unwrap();
            conn.execute(
                &format!("INSERT INTO {table} VALUES ('thread-a', 'original')"),
                [],
            )
            .unwrap();
        }
        fs::write(
            home.join("session_index.jsonl"),
            "{\"id\":\"thread-a\",\"thread_name\":\"Private\"}\n",
        )
        .unwrap();
        rollout
    }

    fn seed_archived_session(home: &Path, id: &str, body: &[u8]) -> std::path::PathBuf {
        let rollout = home.join(format!("archived_sessions/2026/07/26/rollout-{id}.jsonl"));
        fs::create_dir_all(rollout.parent().unwrap()).unwrap();
        fs::write(&rollout, body).unwrap();
        rollout
    }

    fn operation_for_checkpoints(
        operation_id: &str,
        action: OperationAction,
        status: OperationStatus,
        phase: OperationPhase,
        checkpoints: &[&BackupManifest],
    ) -> OperationRecord {
        OperationRecord {
            operation_id: operation_id.to_string(),
            action,
            status,
            phase,
            started_at_ms: checkpoints
                .iter()
                .map(|checkpoint| checkpoint.created_at_ms)
                .min()
                .unwrap()
                .saturating_sub(1),
            completed_at_ms: checkpoints
                .iter()
                .map(|checkpoint| checkpoint.created_at_ms)
                .max()
                .unwrap()
                .saturating_add(1),
            backup_dirs: checkpoints
                .iter()
                .map(|checkpoint| checkpoint.backup_dir.clone())
                .collect(),
            counts: Default::default(),
        }
    }

    fn state_checkpoint(
        home: &Path,
        backup_root: &Path,
        reason: &str,
        operation_id: &str,
        role: CheckpointRole,
    ) -> BackupManifest {
        create_state_checkpoint_with_paths(
            home,
            backup_root,
            reason,
            crate::codex_paths::local_codex_paths(home),
            operation_id,
            role,
        )
        .unwrap()
    }

    fn runtime_state_checkpoint(
        home: &Path,
        backup_root: &Path,
        reason: &str,
        operation_id: &str,
        role: CheckpointRole,
    ) -> BackupManifest {
        create_runtime_state_checkpoint_with_paths(
            home,
            backup_root,
            reason,
            crate::codex_paths::local_codex_paths(home),
            operation_id,
            role,
        )
        .unwrap()
    }

    #[cfg(windows)]
    fn create_directory_symlink(target: &Path, link: &Path) -> Result<(), std::io::Error> {
        std::os::windows::fs::symlink_dir(target, link)
    }

    #[cfg(unix)]
    fn create_directory_symlink(target: &Path, link: &Path) -> Result<(), std::io::Error> {
        std::os::unix::fs::symlink(target, link)
    }

    fn backup_restore_plan_fixture(root: &Path, operation_id: &str) -> BackupRestorePlan {
        let backup_dir = root.join("backup");
        let target_root = root.join("target");
        fs::create_dir_all(&backup_dir).unwrap();
        fs::create_dir_all(&target_root).unwrap();
        let target_path = target_root.join("config.toml");
        let artifacts = super::build_backup_restore_artifact_paths(operation_id, &target_path)
            .expect("fixture artifact paths");
        BackupRestorePlan {
            operation_id: operation_id.to_string(),
            created_at_ms: 123,
            backup_dir,
            backup_manifest_sha256: "a".repeat(64),
            target_root: target_root.clone(),
            allowed_roots: vec![target_root],
            restored_file_count: 1,
            mutations: vec![BackupRestoreMutationPlan {
                kind: BackupRestoreMutationKind::Replace,
                logical_path: "config.toml".into(),
                target_path,
                original_sha256: Some("b".repeat(64)),
                replacement_sha256: Some("c".repeat(64)),
                artifacts,
                sqlite: false,
            }],
        }
    }

    fn backup_restore_data_root_fixture(root: &Path) -> std::path::PathBuf {
        let data_root = root.join("codex-switch");
        fs::create_dir_all(&data_root).unwrap();
        data_root
    }

    fn applying_backup_restore_journal(
        current: &super::BackupRestoreJournal,
    ) -> super::BackupRestoreJournal {
        let mut next = current.clone();
        next.revision += 1;
        next.updated_at_ms += 1;
        next.phase = BackupRestoreOperationPhase::Applying;
        next.mutation_states[0].phase = BackupRestoreMutationPhase::WitnessCreating;
        next
    }

    #[test]
    fn backup_restore_journal_is_integrity_bound_and_update_is_cas_verified() {
        let root = tempdir().unwrap();
        let data_root = backup_restore_data_root_fixture(root.path());
        let plan = backup_restore_plan_fixture(root.path(), "restore-op-1");
        let journal = super::new_backup_restore_journal(plan).unwrap();

        let journal_path = super::create_backup_restore_journal(&data_root, &journal).unwrap();
        let encoded = fs::read(&journal_path).unwrap();
        #[cfg(windows)]
        {
            assert!(encoded.starts_with(BACKUP_RESTORE_JOURNAL_MAGIC));
            assert!(!encoded
                .windows(journal.plan.target_root.as_os_str().len())
                .any(|window| window == journal.plan.target_root.to_string_lossy().as_bytes()));
        }
        let loaded =
            super::load_backup_restore_journal(&data_root, &journal.plan.operation_id).unwrap();
        assert_eq!(loaded, journal);

        let next = applying_backup_restore_journal(&journal);
        let persisted = super::persist_backup_restore_journal(&data_root, &journal, &next).unwrap();
        assert_eq!(persisted, next);
        assert_eq!(
            super::load_backup_restore_journal(&data_root, &journal.plan.operation_id).unwrap(),
            next
        );

        let bytes_before_duplicate = fs::read(&journal_path).unwrap();
        let error = super::create_backup_restore_journal(&data_root, &journal).unwrap_err();
        assert!(error.contains("already exists"), "{error}");
        assert_eq!(fs::read(&journal_path).unwrap(), bytes_before_duplicate);
    }

    #[test]
    fn backup_restore_journal_tampering_and_extra_files_fail_closed() {
        let root = tempdir().unwrap();
        let data_root = backup_restore_data_root_fixture(root.path());
        let plan = backup_restore_plan_fixture(root.path(), "restore-op-tamper");
        let journal = super::new_backup_restore_journal(plan).unwrap();
        let journal_path = super::create_backup_restore_journal(&data_root, &journal).unwrap();

        let extra = journal_path.parent().unwrap().join("extra.bin");
        fs::write(&extra, b"contender").unwrap();
        let error =
            super::load_backup_restore_journal(&data_root, "restore-op-tamper").unwrap_err();
        assert!(error.contains("undeclared artifacts"), "{error}");
        fs::remove_file(extra).unwrap();

        let mut bytes = fs::read(&journal_path).unwrap();
        let last = bytes.last_mut().unwrap();
        *last ^= 0x5a;
        fs::write(&journal_path, bytes).unwrap();
        let error =
            super::load_backup_restore_journal(&data_root, "restore-op-tamper").unwrap_err();
        assert!(
            error.contains("unreadable")
                || error.contains("invalid")
                || error.contains("integrity"),
            "{error}"
        );
    }

    #[test]
    fn backup_restore_journal_rejects_identity_and_phase_rewrites() {
        let root = tempdir().unwrap();
        let plan = backup_restore_plan_fixture(root.path(), "restore-op-transition");
        let journal = super::new_backup_restore_journal(plan).unwrap();
        let applying = applying_backup_restore_journal(&journal);
        validate_backup_restore_journal_update(&journal, &applying).unwrap();

        let mut ready = applying.clone();
        ready.revision += 1;
        ready.updated_at_ms += 1;
        ready.mutation_states[0].phase = BackupRestoreMutationPhase::WitnessReady;
        ready.mutation_states[0].parent_identity = Some(RegularFileIdentity {
            volume_serial_number: 1,
            file_index: 1,
        });
        ready.mutation_states[0].original_identity = Some(RegularFileIdentity {
            volume_serial_number: 1,
            file_index: 2,
        });
        ready.mutation_states[0].replacement_identity = Some(RegularFileIdentity {
            volume_serial_number: 1,
            file_index: 3,
        });
        validate_backup_restore_journal_update(&applying, &ready).unwrap();

        let mut changed_identity = ready.clone();
        changed_identity.revision += 1;
        changed_identity.updated_at_ms += 1;
        changed_identity.mutation_states[0].original_identity = Some(RegularFileIdentity {
            volume_serial_number: 9,
            file_index: 9,
        });
        let error = validate_backup_restore_journal_update(&ready, &changed_identity).unwrap_err();
        assert!(error.contains("identity changed"), "{error}");

        let mut skipped_phase = ready.clone();
        skipped_phase.revision += 1;
        skipped_phase.updated_at_ms += 1;
        skipped_phase.phase = BackupRestoreOperationPhase::Committed;
        skipped_phase.mutation_states[0].phase = BackupRestoreMutationPhase::CommittedWithRecovery;
        let error = validate_backup_restore_journal_update(&ready, &skipped_phase).unwrap_err();
        assert!(error.contains("phase transition"), "{error}");
    }

    #[test]
    fn backup_restore_plan_rejects_non_deterministic_or_escaping_identity() {
        let root = tempdir().unwrap();
        let mut plan = backup_restore_plan_fixture(root.path(), "restore-op-plan");
        validate_backup_restore_plan(&plan).unwrap();

        plan.mutations[0].artifacts.recovery_path = root.path().join("attacker.recovery");
        let error = validate_backup_restore_plan(&plan).unwrap_err();
        assert!(error.contains("not deterministic"), "{error}");

        assert!(super::build_backup_restore_artifact_paths(
            "../escape",
            &root.path().join("target/config.toml")
        )
        .unwrap_err()
        .contains("operation ID"));

        let unsafe_data_root = root.path().join("somewhere-else");
        fs::create_dir_all(&unsafe_data_root).unwrap();
        let error = super::validate_backup_restore_data_root(&unsafe_data_root).unwrap_err();
        assert!(error.contains("identity is invalid"), "{error}");
    }

    #[test]
    fn backup_restore_preflight_builds_complete_plan_without_mutating_live_bytes() {
        let home = tempdir().unwrap();
        seed_home(home.path());
        let backup_root = tempdir().unwrap();
        let manifest = create_backup(home.path(), backup_root.path(), "journal-plan").unwrap();

        fs::write(home.path().join("config.toml"), b"model = \"later\"\n").unwrap();
        fs::write(home.path().join("auth.json"), b"later-auth").unwrap();
        fs::write(home.path().join("state_5.sqlite-wal"), b"late-wal").unwrap();
        let extra = home.path().join("sessions/2026/08/12/rollout-extra.jsonl");
        fs::create_dir_all(extra.parent().unwrap()).unwrap();
        fs::write(&extra, b"late-session\n").unwrap();
        let before = [
            home.path().join("config.toml"),
            home.path().join("auth.json"),
            home.path().join("state_5.sqlite"),
            home.path().join("state_5.sqlite-wal"),
            extra.clone(),
        ]
        .into_iter()
        .map(|path| (path.clone(), fs::read(&path).unwrap()))
        .collect::<Vec<_>>();

        let prepared =
            prepare_backup_restore_plan(&manifest.backup_dir, home.path(), "restore-plan-complete")
                .unwrap();

        assert_eq!(prepared.manifest, manifest);
        assert_eq!(
            prepared.journal.plan.restored_file_count,
            manifest.files.len()
        );
        assert!(prepared.journal.plan.mutations.iter().any(|mutation| {
            mutation.target_path == home.path().join("config.toml")
                && mutation.kind == BackupRestoreMutationKind::Replace
        }));
        assert!(prepared.journal.plan.mutations.iter().any(|mutation| {
            mutation.target_path == home.path().join("state_5.sqlite-wal")
                && mutation.kind == BackupRestoreMutationKind::Delete
        }));
        assert!(prepared.journal.plan.mutations.iter().any(|mutation| {
            mutation.target_path == extra && mutation.kind == BackupRestoreMutationKind::Delete
        }));
        for mutation in &prepared.journal.plan.mutations {
            assert_eq!(
                mutation.artifacts,
                super::build_backup_restore_artifact_paths(
                    &prepared.journal.plan.operation_id,
                    &mutation.target_path
                )
                .unwrap()
            );
        }
        for (path, bytes) in before {
            assert_eq!(fs::read(path).unwrap(), bytes);
        }
    }

    #[test]
    fn backup_restore_preflight_rejects_corrupt_sqlite_before_journal_or_live_mutation() {
        let home = tempdir().unwrap();
        seed_home(home.path());
        let backup_root = tempdir().unwrap();
        let manifest = create_backup(home.path(), backup_root.path(), "invalid-sqlite").unwrap();
        let state = manifest
            .files
            .iter()
            .find(|file| file.relative_path == Path::new("state_5.sqlite"))
            .unwrap();
        let invalid = super::protect(b"not a SQLite database").unwrap();
        fs::write(&state.backup_path, &invalid).unwrap();
        let mut raw: serde_json::Value =
            serde_json::from_slice(&fs::read(manifest.backup_dir.join("manifest.json")).unwrap())
                .unwrap();
        let file = raw["files"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|file| file["relativePath"] == "state_5.sqlite")
            .unwrap();
        file["bytes"] = serde_json::json!(invalid.len() as u64);
        file["sha256"] = serde_json::json!(format!("{:x}", Sha256::digest(&invalid)));
        fs::write(
            manifest.backup_dir.join("manifest.json"),
            serde_json::to_vec_pretty(&raw).unwrap(),
        )
        .unwrap();
        let live_before = fs::read(home.path().join("state_5.sqlite")).unwrap();

        let error = prepare_backup_restore_plan(
            &manifest.backup_dir,
            home.path(),
            "restore-corrupt-sqlite",
        )
        .unwrap_err();

        assert!(
            error.contains("SQLite") || error.contains("state_5.sqlite"),
            "{error}"
        );
        assert_eq!(
            fs::read(home.path().join("state_5.sqlite")).unwrap(),
            live_before
        );
    }

    #[test]
    fn backup_restore_staging_is_durable_and_leaves_live_targets_untouched() {
        let root = tempdir().unwrap();
        let data_root = backup_restore_data_root_fixture(root.path());
        let home = tempdir().unwrap();
        seed_home(home.path());
        let backup_root = tempdir().unwrap();
        let manifest = create_backup(home.path(), backup_root.path(), "stage-witnesses").unwrap();
        fs::write(home.path().join("config.toml"), b"model = \"later\"\n").unwrap();
        fs::write(home.path().join("auth.json"), b"later-auth").unwrap();
        fs::write(home.path().join("state_5.sqlite-wal"), b"late-wal").unwrap();
        let live_before = [
            home.path().join("config.toml"),
            home.path().join("auth.json"),
            home.path().join("state_5.sqlite"),
            home.path().join("state_5.sqlite-wal"),
        ]
        .into_iter()
        .map(|path| (path.clone(), fs::read(&path).unwrap()))
        .collect::<Vec<_>>();
        let prepared =
            prepare_backup_restore_plan(&manifest.backup_dir, home.path(), "restore-stage-durable")
                .unwrap();
        let mut journal = prepared.journal;
        super::create_backup_restore_journal(&data_root, &journal).unwrap();

        stage_backup_restore_witnesses(&data_root, &manifest, &mut journal).unwrap();

        assert_eq!(journal.phase, BackupRestoreOperationPhase::Applying);
        assert!(journal
            .mutation_states
            .iter()
            .all(|state| state.phase == BackupRestoreMutationPhase::WitnessReady));
        for (mutation, state) in journal.plan.mutations.iter().zip(&journal.mutation_states) {
            if mutation.original_sha256.is_some() {
                assert!(super::backup_restore_same_file_identity(
                    &mutation.target_path,
                    &mutation.artifacts.original_witness_path
                )
                .unwrap());
                assert_eq!(
                    state.original_identity,
                    Some(super::backup_restore_file_identity(&mutation.target_path).unwrap())
                );
            }
            if mutation.replacement_sha256.is_some() {
                assert_eq!(
                    state.replacement_identity,
                    Some(
                        super::backup_restore_file_identity(
                            &mutation.artifacts.replacement_witness_path
                        )
                        .unwrap()
                    )
                );
                if mutation.sqlite {
                    super::quick_check_sqlite(
                        &mutation.artifacts.replacement_witness_path,
                        "staged test SQLite",
                    )
                    .unwrap();
                }
            }
        }
        assert_eq!(
            super::load_backup_restore_journal(&data_root, "restore-stage-durable").unwrap(),
            journal
        );
        for (path, bytes) in live_before {
            assert_eq!(fs::read(path).unwrap(), bytes);
        }
    }

    #[test]
    fn backup_restore_staging_refuses_artifact_contender_without_touching_live_target() {
        let root = tempdir().unwrap();
        let data_root = backup_restore_data_root_fixture(root.path());
        let home = tempdir().unwrap();
        seed_home(home.path());
        let backup_root = tempdir().unwrap();
        let manifest = create_backup(home.path(), backup_root.path(), "stage-contender").unwrap();
        fs::write(home.path().join("config.toml"), b"model = \"later\"\n").unwrap();
        let live_before = fs::read(home.path().join("config.toml")).unwrap();
        let prepared = prepare_backup_restore_plan(
            &manifest.backup_dir,
            home.path(),
            "restore-stage-contender",
        )
        .unwrap();
        let mut journal = prepared.journal;
        super::create_backup_restore_journal(&data_root, &journal).unwrap();
        let contender = journal.plan.mutations[0]
            .artifacts
            .replacement_witness_path
            .clone();
        fs::write(&contender, b"contender").unwrap();

        let error =
            stage_backup_restore_witnesses(&data_root, &manifest, &mut journal).unwrap_err();

        assert!(error.contains("appeared before staging"), "{error}");
        assert_eq!(
            fs::read(home.path().join("config.toml")).unwrap(),
            live_before
        );
        assert_eq!(fs::read(contender).unwrap(), b"contender");
    }

    #[test]
    fn single_source_capacity_counts_one_manifest_and_the_minimum_reserve() {
        let source = BackupSourceCapacityMetadata {
            plaintext_payload_bytes: 100,
            file_count: 2,
            sqlite_logical_bytes: 80,
            ..BackupSourceCapacityMetadata::default()
        };
        let peak_without_reserve = source.plaintext_payload_bytes
            + source.file_count * BACKUP_FILE_OVERHEAD_BYTES
            + MANIFEST_BASE_OVERHEAD_BYTES
            + source.file_count * MANIFEST_ENTRY_OVERHEAD_BYTES
            + source.sqlite_logical_bytes;

        let required = estimate_backup_peak(&[source]).unwrap();

        assert_eq!(required, peak_without_reserve + MIN_CAPACITY_RESERVE_BYTES);
    }

    #[test]
    fn two_source_capacity_sums_payloads_and_uses_the_largest_sqlite_workspace() {
        let current = BackupSourceCapacityMetadata {
            plaintext_payload_bytes: 100,
            file_count: 2,
            sqlite_logical_bytes: 80,
            ..BackupSourceCapacityMetadata::default()
        };
        let shared = BackupSourceCapacityMetadata {
            plaintext_payload_bytes: 200,
            file_count: 3,
            sqlite_logical_bytes: 120,
            ..BackupSourceCapacityMetadata::default()
        };
        let file_count = current.file_count + shared.file_count;
        let peak_without_reserve = current.plaintext_payload_bytes
            + shared.plaintext_payload_bytes
            + file_count * BACKUP_FILE_OVERHEAD_BYTES
            + 2 * MANIFEST_BASE_OVERHEAD_BYTES
            + file_count * MANIFEST_ENTRY_OVERHEAD_BYTES
            + shared.sqlite_logical_bytes;

        let required = estimate_backup_peak(&[current, shared]).unwrap();

        assert_eq!(required, peak_without_reserve + MIN_CAPACITY_RESERVE_BYTES);
    }

    #[test]
    fn dynamic_source_count_controls_manifest_overhead_and_rounded_reserve() {
        let large = BackupSourceCapacityMetadata {
            plaintext_payload_bytes: 20 * 1024 * 1024 * 1024,
            file_count: 0,
            sqlite_logical_bytes: 1,
            ..BackupSourceCapacityMetadata::default()
        };
        let sources = [
            large.clone(),
            BackupSourceCapacityMetadata::default(),
            BackupSourceCapacityMetadata {
                sqlite_logical_bytes: 2,
                ..BackupSourceCapacityMetadata::default()
            },
        ];
        let peak_without_reserve =
            large.plaintext_payload_bytes + 3 * MANIFEST_BASE_OVERHEAD_BYTES + 2;
        let percentage_reserve = percentage_ceil(peak_without_reserve, 15).unwrap();

        let required = estimate_backup_peak(&sources).unwrap();

        assert!(percentage_reserve > MIN_CAPACITY_RESERVE_BYTES);
        assert_eq!(required, peak_without_reserve + percentage_reserve);
        assert_eq!(percentage_ceil(1, 15).unwrap(), 1);
    }

    #[test]
    fn capacity_estimation_fails_closed_on_arithmetic_overflow() {
        let current = BackupSourceCapacityMetadata {
            plaintext_payload_bytes: u64::MAX,
            file_count: 1,
            sqlite_logical_bytes: 0,
            ..BackupSourceCapacityMetadata::default()
        };

        assert!(estimate_backup_peak(&[current]).is_err());
        assert!(estimate_backup_peak(&[]).is_err());
        assert!(estimate_backup_peak_with_source_count(
            &[BackupSourceCapacityMetadata::default()],
            u64::MAX
        )
        .is_err());

        let mut capacity = BackupSourceCapacityMetadata {
            plaintext_payload_bytes: u64::MAX,
            ..BackupSourceCapacityMetadata::default()
        };
        assert!(add_capacity_file(&mut capacity, 1).is_err());
    }

    #[test]
    fn payload_size_limit_accepts_u32_max_and_rejects_the_next_byte() {
        assert!(ensure_encryptable_payload_size(MAX_DPAPI_PAYLOAD_BYTES).is_ok());
        assert!(ensure_encryptable_payload_size(MAX_DPAPI_PAYLOAD_BYTES + 1).is_err());

        let mut accepted = BackupSourceCapacityMetadata::default();
        add_capacity_file(&mut accepted, MAX_DPAPI_PAYLOAD_BYTES).unwrap();
        assert_eq!(accepted.file_count, 1);
        assert_eq!(accepted.plaintext_payload_bytes, MAX_DPAPI_PAYLOAD_BYTES);

        let mut rejected = BackupSourceCapacityMetadata::default();
        assert!(add_capacity_file(&mut rejected, MAX_DPAPI_PAYLOAD_BYTES + 1).is_err());
        assert_eq!(rejected, BackupSourceCapacityMetadata::default());
    }

    #[test]
    fn insufficient_capacity_error_contains_only_required_and_available_counts() {
        let error = finish_capacity_preflight(20, 10).unwrap_err();

        assert_eq!(
            error,
            "insufficient backup capacity: required_bytes=20, available_bytes=10"
        );
        assert_eq!(
            finish_capacity_preflight(20, 20).unwrap(),
            super::BackupCapacityPreflight {
                required_bytes: 20,
                available_bytes: 20,
            }
        );
    }

    #[test]
    fn capacity_metadata_uses_file_lengths_and_sqlite_logical_pages() {
        let home = tempdir().unwrap();
        fs::write(home.path().join("auth.json"), b"auth").unwrap();
        fs::write(home.path().join("config.toml"), b"model = \"test\"\n").unwrap();
        fs::write(home.path().join("session_index.jsonl"), b"index\n").unwrap();
        let sessions = home.path().join("sessions/2026/07/25");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(sessions.join("rollout-a.jsonl"), b"session\n").unwrap();
        fs::write(sessions.join("ignored.txt"), b"ignored").unwrap();
        let archived = home.path().join("archived_sessions/2026/07/24");
        fs::create_dir_all(&archived).unwrap();
        fs::write(archived.join("rollout-archived.jsonl"), b"archived\n").unwrap();
        fs::write(archived.join("ignored.txt"), b"ignored").unwrap();
        let state_db = home.path().join("state_5.sqlite");
        Connection::open(&state_db)
            .unwrap()
            .execute("CREATE TABLE marker (value TEXT)", [])
            .unwrap();
        let paths = crate::codex_paths::local_codex_paths(home.path());
        let sqlite_bytes = sqlite_logical_bytes(&state_db).unwrap();

        let capacity =
            collect_backup_capacity_metadata(home.path(), &paths, BackupScope::Full).unwrap();

        assert_eq!(capacity.file_count, 6);
        assert_eq!(capacity.sqlite_logical_bytes, sqlite_bytes);
        assert_eq!(
            capacity.plaintext_payload_bytes,
            4 + 15 + 6 + 8 + 9 + sqlite_bytes
        );
    }

    #[test]
    fn capacity_metadata_counts_all_managed_sqlite_databases() {
        let home = tempdir().unwrap();
        let paths = crate::codex_paths::local_codex_paths(home.path());
        for database in [
            &paths.state_db,
            &paths.goals_db,
            &paths.memories_db,
            &paths.logs_db,
        ] {
            Connection::open(database)
                .unwrap()
                .execute("CREATE TABLE marker (value TEXT)", [])
                .unwrap();
        }

        let capacity =
            collect_backup_capacity_metadata(home.path(), &paths, BackupScope::Full).unwrap();

        assert_eq!(capacity.file_count, 4);
        assert_eq!(
            capacity.plaintext_payload_bytes,
            [
                &paths.state_db,
                &paths.goals_db,
                &paths.memories_db,
                &paths.logs_db
            ]
            .into_iter()
            .map(|database| sqlite_logical_bytes(database).unwrap())
            .sum::<u64>()
        );
    }

    #[test]
    fn runtime_capacity_excludes_auxiliary_databases_while_full_keeps_them() {
        let home = tempdir().unwrap();
        let paths = crate::codex_paths::local_codex_paths(home.path());
        Connection::open(&paths.state_db)
            .unwrap()
            .execute("CREATE TABLE state_marker (value TEXT)", [])
            .unwrap();
        let logs = Connection::open(&paths.logs_db).unwrap();
        logs.execute("CREATE TABLE payload (value BLOB)", [])
            .unwrap();
        logs.execute("INSERT INTO payload VALUES (?1)", [vec![7_u8; 512 * 1024]])
            .unwrap();
        drop(logs);

        let runtime =
            collect_backup_capacity_metadata(home.path(), &paths, BackupScope::Runtime).unwrap();
        let full =
            collect_backup_capacity_metadata(home.path(), &paths, BackupScope::Full).unwrap();

        assert_eq!(runtime.file_count, 1);
        assert_eq!(
            runtime.plaintext_payload_bytes,
            sqlite_logical_bytes(&paths.state_db).unwrap()
        );
        assert_eq!(full.file_count, 2);
        assert!(full.plaintext_payload_bytes > runtime.plaintext_payload_bytes);
        assert!(full.sqlite_logical_bytes > runtime.sqlite_logical_bytes);
    }

    #[test]
    fn capacity_metadata_matches_all_scope_write_sets() {
        let home = tempdir().unwrap();
        seed_home(home.path());
        seed_archived_session(home.path(), "thread-archived", b"archived\n");
        let process_state = home.path().join(CHAT_PROCESS_STATE_RELATIVE_PATH);
        fs::create_dir_all(process_state.parent().unwrap()).unwrap();
        fs::write(process_state, b"not-json").unwrap();
        let paths = crate::codex_paths::local_codex_paths(home.path());

        let full =
            collect_backup_capacity_metadata(home.path(), &paths, BackupScope::Full).unwrap();
        let runtime =
            collect_backup_capacity_metadata(home.path(), &paths, BackupScope::Runtime).unwrap();
        let runtime_state =
            collect_backup_capacity_metadata(home.path(), &paths, BackupScope::RuntimeState)
                .unwrap();
        let sessions =
            collect_backup_capacity_metadata(home.path(), &paths, BackupScope::Sessions).unwrap();
        let state =
            collect_backup_capacity_metadata(home.path(), &paths, BackupScope::StateOnly).unwrap();
        let relative_paths = |capacity: &BackupSourceCapacityMetadata| {
            capacity
                .relative_paths
                .iter()
                .map(|path| path.to_string_lossy().replace('\\', "/"))
                .collect::<BTreeSet<_>>()
        };

        assert_eq!(
            relative_paths(&full),
            BTreeSet::from([
                "auth.json".to_string(),
                "archived_sessions/2026/07/26/rollout-thread-archived.jsonl".to_string(),
                "config.toml".to_string(),
                "goals_1.sqlite".to_string(),
                "logs_2.sqlite".to_string(),
                "memories_1.sqlite".to_string(),
                "session_index.jsonl".to_string(),
                "sessions/2026/07/13/rollout-thread-a.jsonl".to_string(),
                "state_5.sqlite".to_string(),
            ])
        );
        assert_eq!(
            relative_paths(&runtime),
            BTreeSet::from([
                "auth.json".to_string(),
                "config.toml".to_string(),
                "session_index.jsonl".to_string(),
                "sessions/2026/07/13/rollout-thread-a.jsonl".to_string(),
                "state_5.sqlite".to_string(),
            ])
        );
        assert_eq!(
            relative_paths(&runtime_state),
            BTreeSet::from([
                "auth.json".to_string(),
                "config.toml".to_string(),
                "process_manager/chat_processes.json".to_string(),
                "state_5.sqlite".to_string(),
            ])
        );
        assert_eq!(
            relative_paths(&sessions),
            BTreeSet::from([
                "archived_sessions/2026/07/26/rollout-thread-archived.jsonl".to_string(),
                "session_index.jsonl".to_string(),
                "sessions/2026/07/13/rollout-thread-a.jsonl".to_string(),
                "state_5.sqlite".to_string(),
            ])
        );
        assert_eq!(
            relative_paths(&state),
            BTreeSet::from(["state_5.sqlite".to_string()])
        );
        assert_eq!(state.file_count, 1);
        assert_eq!(runtime_state.file_count, state.file_count + 3);
        assert_eq!(sessions.file_count, state.file_count + 3);
        assert_eq!(runtime.file_count, sessions.file_count + 1);
        assert_eq!(full.file_count, runtime.file_count + 4);
        assert_eq!(
            runtime_state.sqlite_logical_bytes,
            state.sqlite_logical_bytes
        );
        assert_eq!(state.sqlite_logical_bytes, sessions.sqlite_logical_bytes);
        assert_eq!(runtime.sqlite_logical_bytes, sessions.sqlite_logical_bytes);
        assert!(full.sqlite_logical_bytes >= runtime.sqlite_logical_bytes);
        assert!(state.plaintext_payload_bytes < sessions.plaintext_payload_bytes);
        assert!(sessions.plaintext_payload_bytes < runtime.plaintext_payload_bytes);
        assert!(runtime.plaintext_payload_bytes < full.plaintext_payload_bytes);
    }

    #[test]
    fn runtime_state_preflight_is_smaller_than_the_legacy_runtime_scope() {
        let home = tempdir().unwrap();
        seed_home(home.path());
        let backup_root = tempdir().unwrap();
        let paths = crate::codex_paths::local_codex_paths(home.path());

        let runtime_state = preflight_backup_capacity_with_paths(
            backup_root.path(),
            home.path(),
            &paths,
            BackupScope::RuntimeState,
        )
        .unwrap();
        let runtime = preflight_backup_capacity_with_paths(
            backup_root.path(),
            home.path(),
            &paths,
            BackupScope::Runtime,
        )
        .unwrap();

        assert!(runtime_state.required_bytes < runtime.required_bytes);
    }

    #[test]
    fn sqlite_capacity_uses_the_wal_logical_view() {
        let home = tempdir().unwrap();
        let state_db = home.path().join("state_5.sqlite");
        let connection = Connection::open(&state_db).unwrap();
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA wal_autocheckpoint = 0;
                 CREATE TABLE payload (value BLOB);
                 PRAGMA wal_checkpoint(TRUNCATE);",
            )
            .unwrap();
        connection
            .execute("INSERT INTO payload VALUES (?1)", [vec![7_u8; 512 * 1024]])
            .unwrap();
        let main_file_bytes = fs::metadata(&state_db).unwrap().len();

        let logical_bytes = sqlite_logical_bytes(&state_db).unwrap();

        assert!(logical_bytes > main_file_bytes);
        assert!(state_db.with_file_name("state_5.sqlite-wal").is_file());
    }

    #[cfg(windows)]
    #[test]
    fn windows_capacity_query_accepts_a_missing_child_without_creating_it() {
        let destination = tempdir().unwrap();
        let missing_child = destination.path().join("backups/not-created");

        let _available = available_backup_bytes(&missing_child).unwrap();

        assert!(!missing_child.exists());
    }

    #[cfg(windows)]
    #[test]
    fn windows_capacity_query_uses_the_parent_of_an_existing_file() {
        let destination = tempdir().unwrap();
        let existing_file = destination.path().join("session_index.jsonl");
        fs::write(&existing_file, b"index").unwrap();

        let query_path = existing_capacity_ancestor(&existing_file).unwrap();
        let expected_parent = fs::canonicalize(destination.path()).unwrap();

        assert_eq!(query_path, expected_parent);
        assert!(query_path.is_dir());
        let _available = available_backup_bytes(&query_path).unwrap();
    }

    #[test]
    fn backup_source_and_destination_must_not_overlap() {
        let home = tempdir().unwrap();
        seed_home(home.path());
        let nested_backup = home.path().join("backups");

        let error = create_backup(home.path(), &nested_backup, "overlap").unwrap_err();

        assert!(error.contains("must not overlap"));
        assert!(!nested_backup.exists());
    }

    #[cfg(windows)]
    #[test]
    fn windows_overlap_rejects_case_only_aliases_with_missing_tail_components() {
        let root = tempdir().unwrap();
        let upper = root.path().join("Missing/Backups");
        let lower = root.path().join("missing/backups");

        let error = ensure_roots_disjoint(&upper, "upper", &lower, "lower").unwrap_err();

        assert!(error.contains("must not overlap"), "{error}");
        assert!(!upper.exists());
        assert!(!lower.exists());
    }

    #[cfg(windows)]
    #[test]
    fn windows_overlap_rejects_case_only_nested_missing_tail_components() {
        let root = tempdir().unwrap();
        let parent = root.path().join("Missing/Backups");
        let nested = root.path().join("missing/backups/next");

        let error = ensure_roots_disjoint(&parent, "parent", &nested, "nested").unwrap_err();

        assert!(error.contains("must not overlap"), "{error}");
        assert!(!parent.exists());
        assert!(!nested.exists());
    }

    #[cfg(windows)]
    #[test]
    fn windows_overlap_rejects_non_ascii_case_aliases() {
        let root = tempdir().unwrap();
        let upper = root.path().join("Missing/Äpfel");
        let lower = root.path().join("missing/äPFEL");

        let error = ensure_roots_disjoint(&upper, "upper", &lower, "lower").unwrap_err();

        assert!(error.contains("must not overlap"), "{error}");
        assert!(!upper.exists());
        assert!(!lower.exists());
    }

    #[test]
    fn capacity_preflight_rejects_an_external_sqlite_root_inside_the_shared_root() {
        let current = tempdir().unwrap();
        let shared = tempdir().unwrap();
        let backup = tempdir().unwrap();
        fs::write(
            current.path().join("config.toml"),
            format!("sqlite_home = \"{}\"\n", shared.path().display()).replace('\\', "\\\\"),
        )
        .unwrap();

        let error =
            preflight_backup_capacity(backup.path(), current.path(), shared.path()).unwrap_err();

        assert_eq!(error, "backup capacity preflight failed");
    }

    #[test]
    fn dynamic_capacity_preflight_rejects_shared_external_sqlite_roots() {
        let current = tempdir().unwrap();
        let shared = tempdir().unwrap();
        let sqlite_root = tempdir().unwrap();
        let backup = tempdir().unwrap();
        let mut current_paths = crate::codex_paths::local_codex_paths(current.path());
        let mut shared_paths = crate::codex_paths::local_codex_paths(shared.path());
        for paths in [&mut current_paths, &mut shared_paths] {
            paths.sqlite_home = sqlite_root.path().to_path_buf();
            paths.state_db = sqlite_root.path().join("state_5.sqlite");
            paths.goals_db = sqlite_root.path().join("goals_1.sqlite");
            paths.memories_db = sqlite_root.path().join("memories_1.sqlite");
            paths.logs_db = sqlite_root.path().join("logs_2.sqlite");
        }

        let error = preflight_backup_capacity_for_sources(
            backup.path(),
            &[
                BackupCapacitySource {
                    home: current.path(),
                    paths: &current_paths,
                    scope: BackupScope::Full,
                },
                BackupCapacitySource {
                    home: shared.path(),
                    paths: &shared_paths,
                    scope: BackupScope::Sessions,
                },
            ],
        )
        .unwrap_err();

        assert_eq!(error, "backup capacity preflight failed");
    }

    #[test]
    fn single_scope_capacity_preflight_rejects_mismatched_resolved_paths() {
        let home = tempdir().unwrap();
        let backup = tempdir().unwrap();
        let mut paths = crate::codex_paths::local_codex_paths(home.path());
        paths.session_index = home.path().join("other-index.jsonl");

        let error = preflight_backup_capacity_with_paths(
            backup.path(),
            home.path(),
            &paths,
            BackupScope::StateOnly,
        )
        .unwrap_err();

        assert_eq!(error, "backup capacity preflight failed");
    }

    #[test]
    fn scoped_backup_creation_rejects_mismatched_resolved_paths_before_creating_a_directory() {
        let home = tempdir().unwrap();
        let backup = tempdir().unwrap();
        let mut paths = crate::codex_paths::local_codex_paths(home.path());
        paths.session_index = home.path().join("other-index.jsonl");

        let error =
            create_session_backup_with_paths(home.path(), backup.path(), "mismatched-paths", paths)
                .unwrap_err();

        assert!(error.contains("resolved backup paths"));
        assert_eq!(fs::read_dir(backup.path()).unwrap().count(), 0);
    }

    #[test]
    fn backup_creation_reports_the_incomplete_directory_when_cleanup_fails() {
        let backup_root = tempdir().unwrap();
        let incomplete = backup_root.path().join("incomplete-backup");
        fs::create_dir(&incomplete).unwrap();
        fs::write(incomplete.join("partial.enc"), b"partial").unwrap();
        let original_error = "failed to encrypt backup payload".to_string();

        let error =
            finish_backup_creation_with_cleanup(&incomplete, Err(original_error.clone()), |_| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "injected cleanup denial",
                ))
            })
            .unwrap_err();

        assert!(error.contains(&original_error));
        assert!(error.contains(&format!("incomplete_dir={}", incomplete.display())));
        assert!(error.contains("injected cleanup denial"));
        assert!(incomplete.join("partial.enc").exists());
    }

    #[test]
    fn scoped_backup_with_paths_keeps_the_preflight_sqlite_root_frozen() {
        let home = tempdir().unwrap();
        let first_sqlite = tempdir().unwrap();
        let second_sqlite = tempdir().unwrap();
        let backup = tempdir().unwrap();
        fs::write(
            home.path().join("config.toml"),
            format!("sqlite_home = \"{}\"\n", first_sqlite.path().display()).replace('\\', "\\\\"),
        )
        .unwrap();
        Connection::open(first_sqlite.path().join("state_5.sqlite"))
            .unwrap()
            .execute("CREATE TABLE first_root (value TEXT)", [])
            .unwrap();
        Connection::open(second_sqlite.path().join("state_5.sqlite"))
            .unwrap()
            .execute("CREATE TABLE second_root (value TEXT)", [])
            .unwrap();
        let frozen_paths = crate::codex_paths::resolve_user_codex_paths(home.path()).unwrap();
        fs::write(
            home.path().join("config.toml"),
            format!("sqlite_home = \"{}\"\n", second_sqlite.path().display()).replace('\\', "\\\\"),
        )
        .unwrap();

        let runtime = create_runtime_backup_with_paths(
            home.path(),
            backup.path(),
            "frozen-runtime",
            frozen_paths.clone(),
        )
        .unwrap();
        let runtime_state = create_runtime_state_backup_with_paths(
            home.path(),
            backup.path(),
            "frozen-runtime-state",
            frozen_paths.clone(),
        )
        .unwrap();
        let sessions = create_session_backup_with_paths(
            home.path(),
            backup.path(),
            "frozen-sessions",
            frozen_paths,
        )
        .unwrap();

        for manifest in [runtime, runtime_state, sessions] {
            let state = manifest
                .files
                .iter()
                .find(|file| file.relative_path == Path::new("state_5.sqlite"))
                .unwrap();
            assert_eq!(state.source, first_sqlite.path().join("state_5.sqlite"));
            assert_ne!(state.source, second_sqlite.path().join("state_5.sqlite"));
        }
    }

    #[test]
    fn creates_verified_encrypted_snapshot_with_session_payloads() {
        let home = tempdir().unwrap();
        let rollout = seed_home(home.path());
        let backup_root = tempdir().unwrap();

        let manifest = create_backup(home.path(), backup_root.path(), "switch").unwrap();
        let verified = verify_backup(&manifest.backup_dir).unwrap();

        assert_eq!(manifest.reason, "switch");
        assert_eq!(verified.files, manifest.files);
        assert!(manifest.backup_dir.join("manifest.json").exists());
        assert!(!manifest.backup_dir.join("auth.json").exists());
        assert!(!manifest.backup_dir.join("state_5.sqlite").exists());
        assert!(!manifest
            .backup_dir
            .join("sessions/2026/07/13/rollout-thread-a.jsonl")
            .exists());
        assert!(manifest.files.iter().any(|file| file.source == rollout));

        for file in &manifest.files {
            let bytes = fs::read(&file.backup_path).unwrap();
            let visible = String::from_utf8_lossy(&bytes);
            assert!(!visible.contains("fake-secret-token"));
            assert!(!visible.contains("private session body"));
        }
    }

    #[test]
    fn scoped_backups_capture_only_the_files_the_operation_can_mutate() {
        let home = tempdir().unwrap();
        seed_home(home.path());
        seed_archived_session(home.path(), "thread-archived", b"archived\n");
        let process_state = home.path().join(CHAT_PROCESS_STATE_RELATIVE_PATH);
        fs::create_dir_all(process_state.parent().unwrap()).unwrap();
        fs::write(process_state, b"not-json").unwrap();
        let backup_root = tempdir().unwrap();

        let full = create_backup(home.path(), backup_root.path(), "full-scope").unwrap();
        let runtime =
            create_runtime_backup(home.path(), backup_root.path(), "runtime-scope").unwrap();
        let runtime_state = create_runtime_state_backup_with_paths(
            home.path(),
            backup_root.path(),
            "runtime-state-scope",
            crate::codex_paths::local_codex_paths(home.path()),
        )
        .unwrap();
        let sessions =
            create_session_backup(home.path(), backup_root.path(), "session-scope").unwrap();
        let state = create_state_backup(home.path(), backup_root.path(), "state-scope").unwrap();

        let relative_paths = |manifest: &BackupManifest| {
            manifest
                .files
                .iter()
                .map(|file| file.relative_path.to_string_lossy().replace('\\', "/"))
                .collect::<BTreeSet<_>>()
        };
        assert_eq!(full.scope, BackupScope::Full);
        assert_eq!(
            full.tracked_databases,
            vec![
                "state_5.sqlite",
                "goals_1.sqlite",
                "memories_1.sqlite",
                "logs_2.sqlite"
            ]
        );
        assert_eq!(
            relative_paths(&full),
            BTreeSet::from([
                "auth.json".to_string(),
                "archived_sessions/2026/07/26/rollout-thread-archived.jsonl".to_string(),
                "config.toml".to_string(),
                "goals_1.sqlite".to_string(),
                "logs_2.sqlite".to_string(),
                "memories_1.sqlite".to_string(),
                "session_index.jsonl".to_string(),
                "sessions/2026/07/13/rollout-thread-a.jsonl".to_string(),
                "state_5.sqlite".to_string(),
            ])
        );
        assert_eq!(runtime.scope, BackupScope::Runtime);
        assert!(runtime.complete_sessions);
        assert_eq!(
            relative_paths(&runtime),
            BTreeSet::from([
                "auth.json".to_string(),
                "config.toml".to_string(),
                "session_index.jsonl".to_string(),
                "sessions/2026/07/13/rollout-thread-a.jsonl".to_string(),
                "state_5.sqlite".to_string(),
            ])
        );
        assert_eq!(runtime_state.scope, BackupScope::RuntimeState);
        assert!(runtime_state.tracked_process_state);
        assert!(!runtime_state.complete_sessions);
        assert_eq!(
            serde_json::to_string(&runtime_state.scope).unwrap(),
            "\"runtimeState\""
        );
        assert_eq!(
            relative_paths(&runtime_state),
            BTreeSet::from([
                "auth.json".to_string(),
                "config.toml".to_string(),
                "process_manager/chat_processes.json".to_string(),
                "state_5.sqlite".to_string(),
            ])
        );
        assert!(!full.tracked_process_state);
        assert!(!runtime.tracked_process_state);
        assert!(!sessions.tracked_process_state);
        assert!(!state.tracked_process_state);
        assert_eq!(sessions.scope, BackupScope::Sessions);
        assert_eq!(
            relative_paths(&sessions),
            BTreeSet::from([
                "archived_sessions/2026/07/26/rollout-thread-archived.jsonl".to_string(),
                "session_index.jsonl".to_string(),
                "sessions/2026/07/13/rollout-thread-a.jsonl".to_string(),
                "state_5.sqlite".to_string(),
            ])
        );
        assert_eq!(state.scope, BackupScope::StateOnly);
        assert_eq!(
            relative_paths(&state),
            BTreeSet::from(["state_5.sqlite".to_string()])
        );
        for manifest in [&runtime, &runtime_state, &sessions, &state] {
            assert_eq!(manifest.tracked_databases, vec!["state_5.sqlite"]);
        }
    }

    #[test]
    fn version_three_manifest_requires_explicit_scope_metadata() {
        let home = tempdir().unwrap();
        seed_home(home.path());
        let backup_root = tempdir().unwrap();
        let manifest =
            create_runtime_backup(home.path(), backup_root.path(), "missing-scope").unwrap();
        let manifest_path = manifest.backup_dir.join("manifest.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        value.as_object_mut().unwrap().remove("trackedDatabases");
        fs::write(&manifest_path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

        let error = verify_backup(&manifest.backup_dir).unwrap_err();

        assert!(error.contains("missing scope metadata"), "{error}");
    }

    #[test]
    fn current_manifest_requires_explicit_process_state_scope_metadata() {
        let home = tempdir().unwrap();
        seed_home(home.path());
        let backup_root = tempdir().unwrap();
        let manifest = create_runtime_state_backup_with_paths(
            home.path(),
            backup_root.path(),
            "missing-process-state-scope",
            crate::codex_paths::local_codex_paths(home.path()),
        )
        .unwrap();
        let manifest_path = manifest.backup_dir.join("manifest.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        value.as_object_mut().unwrap().remove("trackedProcessState");
        fs::write(&manifest_path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

        let error = verify_backup(&manifest.backup_dir).unwrap_err();

        assert!(error.contains("process-state scope is invalid"), "{error}");
    }

    #[test]
    fn runtime_manifest_rejects_archived_session_payloads_outside_its_scope() {
        let home = tempdir().unwrap();
        seed_home(home.path());
        let backup_root = tempdir().unwrap();
        let manifest =
            create_runtime_backup(home.path(), backup_root.path(), "runtime-scope").unwrap();
        let manifest_path = manifest.backup_dir.join("manifest.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        value["files"][0]["relativePath"] =
            serde_json::json!("archived_sessions/rollout-unexpected.jsonl");
        fs::write(&manifest_path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

        let error = verify_backup(&manifest.backup_dir).unwrap_err();

        assert!(error.contains("outside the declared scope"), "{error}");
    }

    #[test]
    fn runtime_state_restore_restores_process_state_and_leaves_sessions_untouched() {
        let home = tempdir().unwrap();
        let rollout = seed_home(home.path());
        let backup_root = tempdir().unwrap();
        let process_state = home.path().join(CHAT_PROCESS_STATE_RELATIVE_PATH);
        fs::create_dir_all(process_state.parent().unwrap()).unwrap();
        let original_process_state = vec![0_u8; 1024];
        fs::write(&process_state, &original_process_state).unwrap();
        let original_auth = fs::read(home.path().join("auth.json")).unwrap();
        let original_config = fs::read(home.path().join("config.toml")).unwrap();
        let manifest = create_runtime_state_backup_with_paths(
            home.path(),
            backup_root.path(),
            "runtime-state-rollback",
            crate::codex_paths::local_codex_paths(home.path()),
        )
        .unwrap();
        verify_backup(&manifest.backup_dir).unwrap();

        fs::write(home.path().join("auth.json"), "{}\n").unwrap();
        fs::write(home.path().join("config.toml"), "model = \"changed\"\n").unwrap();
        fs::write(&process_state, b"[]").unwrap();
        fs::write(home.path().join("session_index.jsonl"), "changed-index\n").unwrap();
        fs::write(&rollout, "changed-session\n").unwrap();
        let extra_session = home.path().join("sessions/extra.jsonl");
        fs::write(&extra_session, "late-session\n").unwrap();
        Connection::open(home.path().join("state_5.sqlite"))
            .unwrap()
            .execute("DELETE FROM threads", [])
            .unwrap();

        restore_backup(&manifest.backup_dir, home.path()).unwrap();

        assert_eq!(
            fs::read(home.path().join("auth.json")).unwrap(),
            original_auth
        );
        assert_eq!(
            fs::read(home.path().join("config.toml")).unwrap(),
            original_config
        );
        assert_eq!(fs::read(&process_state).unwrap(), original_process_state);
        assert_eq!(
            fs::read(home.path().join("session_index.jsonl")).unwrap(),
            b"changed-index\n"
        );
        assert_eq!(fs::read(&rollout).unwrap(), b"changed-session\n");
        assert_eq!(fs::read(&extra_session).unwrap(), b"late-session\n");
        let state_count: i64 = Connection::open(home.path().join("state_5.sqlite"))
            .unwrap()
            .query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0))
            .unwrap();
        assert_eq!(state_count, 1);
    }

    #[test]
    fn runtime_state_restore_removes_process_state_created_after_checkpoint() {
        let home = tempdir().unwrap();
        seed_home(home.path());
        let backup_root = tempdir().unwrap();
        let manifest = create_runtime_state_backup_with_paths(
            home.path(),
            backup_root.path(),
            "runtime-state-rollback",
            crate::codex_paths::local_codex_paths(home.path()),
        )
        .unwrap();
        let process_state = home.path().join(CHAT_PROCESS_STATE_RELATIVE_PATH);
        fs::create_dir_all(process_state.parent().unwrap()).unwrap();
        fs::write(&process_state, b"[]").unwrap();

        restore_backup(&manifest.backup_dir, home.path()).unwrap();

        assert!(!process_state.exists());
    }

    #[test]
    fn process_state_revalidation_rejects_same_length_checkpoint_drift() {
        let home = tempdir().unwrap();
        seed_home(home.path());
        let process_state = home.path().join(CHAT_PROCESS_STATE_RELATIVE_PATH);
        fs::create_dir_all(process_state.parent().unwrap()).unwrap();
        fs::write(&process_state, b"not-json").unwrap();
        let backup_root = tempdir().unwrap();
        let manifest = create_runtime_state_backup_with_paths(
            home.path(),
            backup_root.path(),
            "runtime-state-revalidate",
            crate::codex_paths::local_codex_paths(home.path()),
        )
        .unwrap();
        load_process_state_checkpoint(&manifest, home.path()).unwrap();

        fs::write(&process_state, b"bad-json").unwrap();

        let error = load_process_state_checkpoint(&manifest, home.path()).unwrap_err();
        assert!(error.contains("changed after"), "unexpected error: {error}");
    }

    #[test]
    fn process_state_revalidation_rejects_a_file_created_after_checkpoint() {
        let home = tempdir().unwrap();
        seed_home(home.path());
        let backup_root = tempdir().unwrap();
        let manifest = create_runtime_state_backup_with_paths(
            home.path(),
            backup_root.path(),
            "runtime-state-revalidate",
            crate::codex_paths::local_codex_paths(home.path()),
        )
        .unwrap();
        let process_state = home.path().join(CHAT_PROCESS_STATE_RELATIVE_PATH);
        fs::create_dir_all(process_state.parent().unwrap()).unwrap();
        fs::write(&process_state, b"[]").unwrap();

        let error = load_process_state_checkpoint(&manifest, home.path()).unwrap_err();
        assert!(error.contains("changed after"), "unexpected error: {error}");
    }

    #[test]
    fn runtime_restore_rolls_back_its_touched_set_without_overwriting_auxiliary_databases() {
        let home = tempdir().unwrap();
        let rollout = seed_home(home.path());
        let backup_root = tempdir().unwrap();
        let original_auth = fs::read(home.path().join("auth.json")).unwrap();
        let original_config = fs::read(home.path().join("config.toml")).unwrap();
        let original_index = fs::read(home.path().join("session_index.jsonl")).unwrap();
        let original_rollout = fs::read(&rollout).unwrap();
        let manifest =
            create_runtime_backup(home.path(), backup_root.path(), "runtime-rollback").unwrap();

        fs::write(home.path().join("auth.json"), "{}\n").unwrap();
        fs::write(home.path().join("config.toml"), "model = \"changed\"\n").unwrap();
        fs::write(home.path().join("session_index.jsonl"), "changed\n").unwrap();
        fs::write(&rollout, "changed\n").unwrap();
        Connection::open(home.path().join("state_5.sqlite"))
            .unwrap()
            .execute("DELETE FROM threads", [])
            .unwrap();
        for (database, table) in [
            ("goals_1.sqlite", "thread_goals"),
            ("memories_1.sqlite", "stage1_outputs"),
            ("logs_2.sqlite", "logs"),
        ] {
            Connection::open(home.path().join(database))
                .unwrap()
                .execute(
                    &format!("UPDATE {table} SET value = 'newer' WHERE thread_id = 'thread-a'"),
                    [],
                )
                .unwrap();
        }

        restore_backup(&manifest.backup_dir, home.path()).unwrap();

        assert_eq!(
            fs::read(home.path().join("auth.json")).unwrap(),
            original_auth
        );
        assert_eq!(
            fs::read(home.path().join("config.toml")).unwrap(),
            original_config
        );
        assert_eq!(
            fs::read(home.path().join("session_index.jsonl")).unwrap(),
            original_index
        );
        assert_eq!(fs::read(&rollout).unwrap(), original_rollout);
        let state_count: i64 = Connection::open(home.path().join("state_5.sqlite"))
            .unwrap()
            .query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0))
            .unwrap();
        assert_eq!(state_count, 1);
        for (database, table) in [
            ("goals_1.sqlite", "thread_goals"),
            ("memories_1.sqlite", "stage1_outputs"),
            ("logs_2.sqlite", "logs"),
        ] {
            let value: String = Connection::open(home.path().join(database))
                .unwrap()
                .query_row(
                    &format!("SELECT value FROM {table} WHERE thread_id = 'thread-a'"),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(value, "newer");
        }
    }

    #[test]
    fn tracked_database_absence_removes_a_database_created_after_the_snapshot() {
        let home = tempdir().unwrap();
        fs::write(home.path().join("config.toml"), "model = \"test\"\n").unwrap();
        let backup_root = tempdir().unwrap();
        let manifest =
            create_state_backup(home.path(), backup_root.path(), "absent-state").unwrap();
        assert_eq!(manifest.tracked_databases, vec!["state_5.sqlite"]);
        assert!(manifest.files.is_empty());

        Connection::open(home.path().join("state_5.sqlite"))
            .unwrap()
            .execute("CREATE TABLE created_later (value TEXT)", [])
            .unwrap();
        Connection::open(home.path().join("memories_1.sqlite"))
            .unwrap()
            .execute("CREATE TABLE untracked (value TEXT)", [])
            .unwrap();

        restore_backup(&manifest.backup_dir, home.path()).unwrap();

        assert!(!home.path().join("state_5.sqlite").exists());
        assert!(home.path().join("memories_1.sqlite").exists());
        assert!(home.path().join("config.toml").exists());
    }

    #[test]
    fn recent_backup_listing_replaces_a_corrupt_new_candidate_with_an_older_verified_one() {
        let home = tempdir().unwrap();
        seed_home(home.path());
        let backup_root = tempdir().unwrap();
        let mut manifests = Vec::new();
        for index in 0..6_u128 {
            let mut manifest = create_backup(
                home.path(),
                backup_root.path(),
                &format!("candidate-{index}"),
            )
            .unwrap();
            manifest.created_at_ms = index;
            fs::write(
                manifest.backup_dir.join("manifest.json"),
                serde_json::to_vec_pretty(&manifest).unwrap(),
            )
            .unwrap();
            manifests.push(manifest);
        }
        let newest_payload = &manifests[5].files[0].backup_path;
        let mut tampered = fs::read(newest_payload).unwrap();
        tampered[0] ^= 0xff;
        fs::write(newest_payload, tampered).unwrap();

        let summaries = list_recent_backups(backup_root.path(), 5).unwrap();

        assert_eq!(summaries.len(), 5);
        assert!(summaries.iter().all(|summary| summary.verified));
        assert!(summaries
            .iter()
            .all(|summary| summary.backup_dir != manifests[5].backup_dir));
        assert!(summaries
            .iter()
            .any(|summary| summary.backup_dir == manifests[0].backup_dir));
        assert_eq!(summaries[0].backup_dir, manifests[4].backup_dir);
    }

    #[test]
    fn recent_backup_listing_excludes_candidates_the_delete_contract_rejects() {
        let home = tempdir().unwrap();
        seed_home(home.path());
        let backup_root = tempdir().unwrap();
        let valid = create_backup(home.path(), backup_root.path(), "valid-full").unwrap();
        let with_extra = create_backup(home.path(), backup_root.path(), "full-with-extra").unwrap();
        fs::write(with_extra.backup_dir.join("undeclared.txt"), b"undeclared").unwrap();

        let summaries = list_recent_backups(backup_root.path(), 5).unwrap();

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].backup_dir, valid.backup_dir);
        assert!(summaries[0].verified);
        delete_verified_full_backup(backup_root.path(), &summaries[0].backup_dir).unwrap();
        assert!(!valid.backup_dir.exists());
        assert!(with_extra.backup_dir.exists());
    }

    #[test]
    fn recent_backup_listing_hides_partial_compensation_snapshots_but_keeps_v2() {
        let home = tempdir().unwrap();
        seed_home(home.path());
        let backup_root = tempdir().unwrap();
        let full = create_backup(home.path(), backup_root.path(), "manual-full").unwrap();
        let partial =
            create_runtime_backup(home.path(), backup_root.path(), "runtime-partial").unwrap();
        let runtime_state = create_runtime_state_backup_with_paths(
            home.path(),
            backup_root.path(),
            "runtime-state-partial",
            crate::codex_paths::local_codex_paths(home.path()),
        )
        .unwrap();
        let mut legacy = create_backup(home.path(), backup_root.path(), "legacy-full").unwrap();
        for file in legacy.files.iter().filter(|file| {
            matches!(
                file.relative_path.to_string_lossy().as_ref(),
                "goals_1.sqlite" | "memories_1.sqlite" | "logs_2.sqlite"
            )
        }) {
            fs::remove_file(&file.backup_path).unwrap();
        }
        legacy.version = 2;
        legacy.files.retain(|file| {
            !matches!(
                file.relative_path.to_string_lossy().as_ref(),
                "goals_1.sqlite" | "memories_1.sqlite" | "logs_2.sqlite"
            )
        });
        let mut legacy_value = serde_json::to_value(&legacy).unwrap();
        let legacy_object = legacy_value.as_object_mut().unwrap();
        legacy_object.remove("scope");
        legacy_object.remove("trackedDatabases");
        fs::write(
            legacy.backup_dir.join("manifest.json"),
            serde_json::to_vec_pretty(&legacy_value).unwrap(),
        )
        .unwrap();

        let summaries = list_recent_backups(backup_root.path(), 5).unwrap();
        let listed = summaries
            .iter()
            .map(|summary| summary.backup_dir.as_path())
            .collect::<Vec<_>>();

        assert_eq!(summaries.len(), 2);
        assert!(listed.contains(&full.backup_dir.as_path()));
        assert!(listed.contains(&legacy.backup_dir.as_path()));
        assert!(!listed.contains(&partial.backup_dir.as_path()));
        assert!(!listed.contains(&runtime_state.backup_dir.as_path()));
    }

    #[test]
    fn legacy_plaintext_auth_is_encrypted_before_the_original_is_removed() {
        let backup_root = tempdir().unwrap();
        let legacy = backup_root.path().join("legacy-backup");
        fs::create_dir_all(&legacy).unwrap();
        let plaintext = br#"{"auth_mode":"chatgpt","token":"fake-legacy-token"}"#;
        fs::write(legacy.join("auth.json"), plaintext).unwrap();

        migrate_legacy_plaintext_auth(backup_root.path()).unwrap();

        assert!(!legacy.join("auth.json").exists());
        let encrypted = fs::read(legacy.join("auth.json.enc")).unwrap();
        assert!(!encrypted
            .windows(plaintext.len())
            .any(|window| window == plaintext));
        assert_eq!(crate::crypto::unprotect(&encrypted).unwrap(), plaintext);
    }

    #[test]
    fn restores_all_payloads_and_rejects_tampering() {
        let home = tempdir().unwrap();
        let rollout = seed_home(home.path());
        let original_auth = fs::read(home.path().join("auth.json")).unwrap();
        let original_config = fs::read(home.path().join("config.toml")).unwrap();
        let original_index = fs::read(home.path().join("session_index.jsonl")).unwrap();
        let original_rollout = fs::read(&rollout).unwrap();
        let backup_root = tempdir().unwrap();
        let manifest = create_backup(home.path(), backup_root.path(), "delete").unwrap();

        fs::write(home.path().join("auth.json"), "{}\n").unwrap();
        fs::write(home.path().join("config.toml"), "model = \"broken\"\n").unwrap();
        fs::remove_file(home.path().join("state_5.sqlite")).unwrap();
        fs::remove_file(home.path().join("goals_1.sqlite")).unwrap();
        fs::remove_file(home.path().join("memories_1.sqlite")).unwrap();
        fs::remove_file(home.path().join("logs_2.sqlite")).unwrap();
        fs::remove_file(home.path().join("session_index.jsonl")).unwrap();
        fs::remove_file(&rollout).unwrap();

        let restored = restore_backup(&manifest.backup_dir, home.path()).unwrap();

        assert_eq!(restored.restored_files, manifest.files.len());
        assert_eq!(
            fs::read(home.path().join("auth.json")).unwrap(),
            original_auth
        );
        assert_eq!(
            fs::read(home.path().join("config.toml")).unwrap(),
            original_config
        );
        assert_eq!(
            fs::read(home.path().join("session_index.jsonl")).unwrap(),
            original_index
        );
        assert_eq!(fs::read(&rollout).unwrap(), original_rollout);
        let conn = Connection::open(home.path().join("state_5.sqlite")).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
        for (database, table) in [
            ("goals_1.sqlite", "thread_goals"),
            ("memories_1.sqlite", "stage1_outputs"),
            ("logs_2.sqlite", "logs"),
        ] {
            let value: String = Connection::open(home.path().join(database))
                .unwrap()
                .query_row(
                    &format!("SELECT value FROM {table} WHERE thread_id = 'thread-a'"),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(value, "original");
        }

        let payload = &manifest.files[0].backup_path;
        let mut bytes = fs::read(payload).unwrap();
        bytes[0] ^= 0xff;
        fs::write(payload, bytes).unwrap();
        assert!(verify_backup(&manifest.backup_dir).is_err());
    }

    #[test]
    fn full_backup_restores_archived_rollouts_for_hard_delete_rollback() {
        let home = tempdir().unwrap();
        seed_home(home.path());
        let archived =
            seed_archived_session(home.path(), "hard-delete", b"archived before delete\n");
        let backup_root = tempdir().unwrap();
        let manifest =
            create_backup(home.path(), backup_root.path(), "hard-delete-current").unwrap();

        fs::remove_file(&archived).unwrap();
        restore_backup(&manifest.backup_dir, home.path()).unwrap();

        assert_eq!(fs::read(&archived).unwrap(), b"archived before delete\n");
        assert!(manifest.complete_sessions);
        assert!(manifest
            .files
            .iter()
            .any(|file| file.relative_path.starts_with("archived_sessions")));
    }

    #[test]
    fn sessions_backup_restores_archived_rollouts_and_removes_extra_archived_files() {
        let home = tempdir().unwrap();
        seed_home(home.path());
        let archived =
            seed_archived_session(home.path(), "sync", b"archived before synchronization\n");
        let backup_root = tempdir().unwrap();
        let manifest =
            create_session_backup(home.path(), backup_root.path(), "sync-current").unwrap();

        fs::write(&archived, b"changed\n").unwrap();
        let extra = seed_archived_session(home.path(), "extra", b"created after snapshot\n");
        restore_backup(&manifest.backup_dir, home.path()).unwrap();

        assert_eq!(
            fs::read(&archived).unwrap(),
            b"archived before synchronization\n"
        );
        assert!(!extra.exists());
        assert!(manifest.complete_sessions);
    }

    #[test]
    fn absent_full_snapshot_clears_active_and_archived_session_roots() {
        let source_parent = tempdir().unwrap();
        let home = source_parent.path().join("missing-home");
        let backup_root = tempdir().unwrap();
        let manifest = create_local_backup(&home, backup_root.path(), "missing-full").unwrap();
        assert!(!manifest.root_existed);
        assert!(manifest.complete_sessions);

        let active = home.join("sessions/late.jsonl");
        fs::create_dir_all(active.parent().unwrap()).unwrap();
        fs::write(&active, b"late active\n").unwrap();
        let archived = seed_archived_session(&home, "late-archived", b"late archived\n");
        let untracked = home.join("archived_sessions/keep.txt");
        fs::write(&untracked, b"not part of the JSONL snapshot\n").unwrap();

        restore_backup(&manifest.backup_dir, &home).unwrap();

        assert!(!active.exists());
        assert!(!archived.exists());
        assert!(!home.join("sessions").exists());
        assert_eq!(
            fs::read(untracked).unwrap(),
            b"not part of the JSONL snapshot\n"
        );
    }

    #[test]
    fn runtime_backup_restore_does_not_touch_archived_rollouts() {
        let home = tempdir().unwrap();
        seed_home(home.path());
        let archived = seed_archived_session(home.path(), "runtime", b"before\n");
        let backup_root = tempdir().unwrap();
        let manifest =
            create_runtime_backup(home.path(), backup_root.path(), "runtime-current").unwrap();

        fs::write(&archived, b"after\n").unwrap();
        let extra = seed_archived_session(home.path(), "runtime-extra", b"extra\n");
        restore_backup(&manifest.backup_dir, home.path()).unwrap();

        assert_eq!(fs::read(&archived).unwrap(), b"after\n");
        assert!(extra.exists());
        assert!(!manifest
            .files
            .iter()
            .any(|file| file.relative_path.starts_with("archived_sessions")));
    }

    #[test]
    fn absent_root_manifests_reject_payloads_in_versions_two_and_three() {
        let home = tempdir().unwrap();
        fs::write(home.path().join("config.toml"), "model = \"source\"\n").unwrap();
        let backup_root = tempdir().unwrap();
        let manifest = create_backup(home.path(), backup_root.path(), "absent-with-files").unwrap();
        assert!(!manifest.files.is_empty());

        for version in [3, 2] {
            let mut value = serde_json::to_value(&manifest).unwrap();
            let object = value.as_object_mut().unwrap();
            object.insert("version".to_string(), serde_json::json!(version));
            object.insert("rootExisted".to_string(), serde_json::json!(false));
            if version == 2 {
                object.remove("scope");
                object.remove("trackedDatabases");
            }
            fs::write(
                manifest.backup_dir.join("manifest.json"),
                serde_json::to_vec_pretty(&value).unwrap(),
            )
            .unwrap();

            let error = verify_backup(&manifest.backup_dir).unwrap_err();
            assert!(error.contains("source root did not exist"), "{error}");
        }
    }

    #[test]
    fn preverified_payload_change_fails_before_restore_mutates_the_target() {
        let source = tempdir().unwrap();
        fs::write(source.path().join("config.toml"), "model = \"source\"\n").unwrap();
        let backup_root = tempdir().unwrap();
        let manifest =
            create_backup(source.path(), backup_root.path(), "restore-stage-race").unwrap();
        let verified = verify_backup(&manifest.backup_dir).unwrap();

        let target = tempdir().unwrap();
        fs::write(target.path().join("config.toml"), "model = \"target\"\n").unwrap();
        fs::write(target.path().join("auth.json"), b"target-auth").unwrap();
        let extra_session = target.path().join("sessions/2026/extra.jsonl");
        fs::create_dir_all(extra_session.parent().unwrap()).unwrap();
        fs::write(&extra_session, b"target-session").unwrap();

        let payload = &verified.files[0].backup_path;
        let mut bytes = fs::read(payload).unwrap();
        bytes[0] ^= 0xff;
        fs::write(payload, bytes).unwrap();

        let error =
            restore_verified_backup(&manifest.backup_dir, target.path(), &verified).unwrap_err();

        assert!(error.contains("checksum mismatch"), "{error}");
        assert_eq!(
            fs::read_to_string(target.path().join("config.toml")).unwrap(),
            "model = \"target\"\n"
        );
        assert_eq!(
            fs::read(target.path().join("auth.json")).unwrap(),
            b"target-auth"
        );
        assert_eq!(fs::read(extra_session).unwrap(), b"target-session");
    }

    #[test]
    fn staged_restore_does_not_reopen_the_original_backup_payload() {
        let source = tempdir().unwrap();
        fs::write(source.path().join("config.toml"), "model = \"source\"\n").unwrap();
        let backup_root = tempdir().unwrap();
        let manifest =
            create_backup(source.path(), backup_root.path(), "restore-stage-source").unwrap();
        let verified = verify_backup(&manifest.backup_dir).unwrap();

        let target = tempdir().unwrap();
        fs::write(target.path().join("config.toml"), "model = \"target\"\n").unwrap();
        let old_paths = super::local_codex_paths(target.path());
        let mut staged =
            stage_backup_payloads(&manifest.backup_dir, target.path(), &verified).unwrap();

        let payload = &verified.files[0].backup_path;
        let mut bytes = fs::read(payload).unwrap();
        bytes[0] ^= 0xff;
        fs::write(payload, bytes).unwrap();

        let restored = restore_staged_backup(
            &manifest.backup_dir,
            target.path(),
            &verified,
            &old_paths,
            &mut staged,
        )
        .unwrap();

        assert_eq!(restored.restored_files, verified.files.len());
        assert_eq!(
            fs::read_to_string(target.path().join("config.toml")).unwrap(),
            "model = \"source\"\n"
        );
    }

    #[test]
    fn restore_routes_state_db_using_the_backed_up_config_before_writing_sqlite() {
        let home = tempdir().unwrap();
        let original_sqlite = tempdir().unwrap();
        let later_sqlite = tempdir().unwrap();
        let config = |path: &std::path::Path| {
            format!("sqlite_home = \"{}\"\n", path.display()).replace('\\', "\\\\")
        };
        fs::write(
            home.path().join("config.toml"),
            config(original_sqlite.path()),
        )
        .unwrap();
        let conn = Connection::open(original_sqlite.path().join("state_5.sqlite")).unwrap();
        conn.execute("CREATE TABLE marker (value TEXT)", [])
            .unwrap();
        conn.execute("INSERT INTO marker VALUES ('original')", [])
            .unwrap();
        drop(conn);
        let conn = Connection::open(original_sqlite.path().join("memories_1.sqlite")).unwrap();
        conn.execute("CREATE TABLE marker (value TEXT)", [])
            .unwrap();
        conn.execute("INSERT INTO marker VALUES ('original-memory')", [])
            .unwrap();
        drop(conn);
        let backup_root = tempdir().unwrap();
        let manifest = create_backup(home.path(), backup_root.path(), "external-sqlite").unwrap();
        assert!(!manifest.state_db_is_local);

        fs::write(home.path().join("config.toml"), config(later_sqlite.path())).unwrap();
        let conn = Connection::open(later_sqlite.path().join("state_5.sqlite")).unwrap();
        conn.execute("CREATE TABLE marker (value TEXT)", [])
            .unwrap();
        conn.execute("INSERT INTO marker VALUES ('later')", [])
            .unwrap();
        drop(conn);
        let conn = Connection::open(later_sqlite.path().join("memories_1.sqlite")).unwrap();
        conn.execute("CREATE TABLE marker (value TEXT)", [])
            .unwrap();
        conn.execute("INSERT INTO marker VALUES ('later-memory')", [])
            .unwrap();
        drop(conn);
        fs::remove_file(original_sqlite.path().join("state_5.sqlite")).unwrap();
        fs::remove_file(original_sqlite.path().join("memories_1.sqlite")).unwrap();

        restore_backup(&manifest.backup_dir, home.path()).unwrap();

        let conn = Connection::open(original_sqlite.path().join("state_5.sqlite")).unwrap();
        let value: String = conn
            .query_row("SELECT value FROM marker", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, "original");
        let memory_value: String =
            Connection::open(original_sqlite.path().join("memories_1.sqlite"))
                .unwrap()
                .query_row("SELECT value FROM marker", [], |row| row.get(0))
                .unwrap();
        assert_eq!(memory_value, "original-memory");
        assert!(!later_sqlite.path().join("state_5.sqlite").exists());
        assert!(!later_sqlite.path().join("memories_1.sqlite").exists());
    }

    #[test]
    fn restore_removes_auxiliary_database_absent_from_external_sqlite_snapshot() {
        let home = tempdir().unwrap();
        let original_sqlite = tempdir().unwrap();
        let later_sqlite = tempdir().unwrap();
        let backup_root = tempdir().unwrap();
        let config = |path: &std::path::Path| {
            format!("sqlite_home = \"{}\"\n", path.display()).replace('\\', "\\\\")
        };
        fs::write(
            home.path().join("config.toml"),
            config(original_sqlite.path()),
        )
        .unwrap();
        Connection::open(original_sqlite.path().join("state_5.sqlite"))
            .unwrap()
            .execute("CREATE TABLE marker (value TEXT)", [])
            .unwrap();
        let manifest = create_backup(home.path(), backup_root.path(), "external-absence").unwrap();
        assert!(!manifest
            .files
            .iter()
            .any(|file| file.relative_path == std::path::Path::new("memories_1.sqlite")));

        Connection::open(original_sqlite.path().join("memories_1.sqlite"))
            .unwrap()
            .execute("CREATE TABLE post_backup (value TEXT)", [])
            .unwrap();
        fs::write(home.path().join("config.toml"), config(later_sqlite.path())).unwrap();
        Connection::open(later_sqlite.path().join("memories_1.sqlite"))
            .unwrap()
            .execute("CREATE TABLE later (value TEXT)", [])
            .unwrap();

        restore_backup(&manifest.backup_dir, home.path()).unwrap();

        assert!(!original_sqlite.path().join("memories_1.sqlite").exists());
        assert!(!later_sqlite.path().join("memories_1.sqlite").exists());
    }

    #[test]
    fn local_backup_ignores_a_user_sqlite_home_binding_for_shared_roots() {
        let shared = tempdir().unwrap();
        let external = tempdir().unwrap();
        fs::write(
            shared.path().join("config.toml"),
            format!("sqlite_home = \"{}\"\n", external.path().display()).replace('\\', "\\\\"),
        )
        .unwrap();
        Connection::open(shared.path().join("state_5.sqlite"))
            .unwrap()
            .execute("CREATE TABLE local_marker (value TEXT)", [])
            .unwrap();
        Connection::open(external.path().join("state_5.sqlite"))
            .unwrap()
            .execute("CREATE TABLE external_marker (value TEXT)", [])
            .unwrap();
        let backup_root = tempdir().unwrap();

        let manifest =
            create_local_backup(shared.path(), backup_root.path(), "shared-local").unwrap();

        assert!(manifest.state_db_is_local);
        let state = manifest
            .files
            .iter()
            .find(|file| file.relative_path == std::path::Path::new("state_5.sqlite"))
            .unwrap();
        assert_eq!(state.source, shared.path().join("state_5.sqlite"));
    }

    #[test]
    fn version_two_restore_does_not_touch_auxiliary_databases_it_did_not_track() {
        let home = tempdir().unwrap();
        seed_home(home.path());
        let backup_root = tempdir().unwrap();
        let mut manifest = create_backup(home.path(), backup_root.path(), "legacy-v2").unwrap();
        manifest.version = 2;
        manifest.files.retain(|file| {
            !matches!(
                file.relative_path.to_string_lossy().as_ref(),
                "goals_1.sqlite" | "memories_1.sqlite" | "logs_2.sqlite"
            )
        });
        let mut legacy_value = serde_json::to_value(&manifest).unwrap();
        let legacy_object = legacy_value.as_object_mut().unwrap();
        legacy_object.remove("scope");
        legacy_object.remove("trackedDatabases");
        fs::write(
            manifest.backup_dir.join("manifest.json"),
            serde_json::to_vec_pretty(&legacy_value).unwrap(),
        )
        .unwrap();
        let memories = Connection::open(home.path().join("memories_1.sqlite")).unwrap();
        memories
            .execute(
                "UPDATE stage1_outputs SET value = 'newer' WHERE thread_id = 'thread-a'",
                [],
            )
            .unwrap();
        drop(memories);

        let legacy = verify_backup(&manifest.backup_dir).unwrap();
        assert_eq!(legacy.scope, BackupScope::Full);
        assert_eq!(legacy.tracked_databases, vec!["state_5.sqlite"]);
        restore_backup(&manifest.backup_dir, home.path()).unwrap();

        let value: String = Connection::open(home.path().join("memories_1.sqlite"))
            .unwrap()
            .query_row(
                "SELECT value FROM stage1_outputs WHERE thread_id = 'thread-a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(value, "newer");
    }

    #[test]
    fn version_two_absent_root_restore_preserves_untracked_auxiliary_databases() {
        let home = tempdir().unwrap();
        seed_home(home.path());
        let archived = seed_archived_session(home.path(), "legacy-preserved", b"keep\n");
        let backup_root = tempdir().unwrap();
        let mut manifest =
            create_backup(home.path(), backup_root.path(), "legacy-v2-absent").unwrap();
        manifest.version = 2;
        manifest.root_existed = false;
        manifest.files.clear();
        let mut legacy_value = serde_json::to_value(&manifest).unwrap();
        let legacy_object = legacy_value.as_object_mut().unwrap();
        legacy_object.remove("scope");
        legacy_object.remove("trackedDatabases");
        fs::write(
            manifest.backup_dir.join("manifest.json"),
            serde_json::to_vec_pretty(&legacy_value).unwrap(),
        )
        .unwrap();
        let memories = Connection::open(home.path().join("memories_1.sqlite")).unwrap();
        memories
            .execute(
                "UPDATE stage1_outputs SET value = 'newer' WHERE thread_id = 'thread-a'",
                [],
            )
            .unwrap();
        drop(memories);

        restore_backup(&manifest.backup_dir, home.path()).unwrap();

        let value: String = Connection::open(home.path().join("memories_1.sqlite"))
            .unwrap()
            .query_row(
                "SELECT value FROM stage1_outputs WHERE thread_id = 'thread-a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(value, "newer");
        assert_eq!(fs::read(archived).unwrap(), b"keep\n");
    }

    #[test]
    fn auxiliary_sqlite_snapshot_includes_uncheckpointed_wal_rows() {
        let home = tempdir().unwrap();
        fs::write(home.path().join("config.toml"), "model = \"test\"\n").unwrap();
        let database = home.path().join("memories_1.sqlite");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA wal_autocheckpoint = 0;
                 CREATE TABLE stage1_outputs (thread_id TEXT PRIMARY KEY, value TEXT);
                 PRAGMA wal_checkpoint(TRUNCATE);",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO stage1_outputs VALUES ('thread-a', 'from-wal')",
                [],
            )
            .unwrap();
        let backup_root = tempdir().unwrap();

        let manifest = create_backup(home.path(), backup_root.path(), "wal").unwrap();
        connection
            .execute("UPDATE stage1_outputs SET value = 'changed'", [])
            .unwrap();
        drop(connection);
        restore_backup(&manifest.backup_dir, home.path()).unwrap();

        let value: String = Connection::open(&database)
            .unwrap()
            .query_row(
                "SELECT value FROM stage1_outputs WHERE thread_id = 'thread-a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(value, "from-wal");
    }

    #[test]
    fn verified_full_backup_deletion_returns_reclaimed_bytes_and_supports_legacy_v2() {
        let home = tempdir().unwrap();
        seed_home(home.path());
        let backup_root = tempdir().unwrap();
        let full = create_backup(home.path(), backup_root.path(), "manual-full").unwrap();

        let deleted = delete_verified_full_backup(backup_root.path(), &full.backup_dir).unwrap();

        assert_eq!(deleted.backup_dir, full.backup_dir);
        assert!(deleted.reclaimed_bytes > 0);
        assert!(!deleted.backup_dir.exists());

        let legacy_home = tempdir().unwrap();
        fs::write(
            legacy_home.path().join("config.toml"),
            "model = \"legacy\"\n",
        )
        .unwrap();
        Connection::open(legacy_home.path().join("state_5.sqlite"))
            .unwrap()
            .execute("CREATE TABLE marker (value TEXT)", [])
            .unwrap();
        let legacy = create_backup(
            legacy_home.path(),
            backup_root.path(),
            "switch-runtime-current",
        )
        .unwrap();
        let manifest_path = legacy.backup_dir.join("manifest.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        value["version"] = serde_json::Value::from(2);
        value.as_object_mut().unwrap().remove("scope");
        value.as_object_mut().unwrap().remove("trackedDatabases");
        fs::write(&manifest_path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

        let deleted = delete_verified_full_backup(backup_root.path(), &legacy.backup_dir).unwrap();

        assert_eq!(deleted.backup_dir, legacy.backup_dir);
        assert!(deleted.reclaimed_bytes > 0);
        assert!(!legacy.backup_dir.exists());
    }

    #[test]
    fn verified_full_backup_deletion_rejects_v3_scoped_backups() {
        let home = tempdir().unwrap();
        seed_home(home.path());
        let backup_root = tempdir().unwrap();
        let scoped =
            create_session_backup(home.path(), backup_root.path(), "manual-session").unwrap();
        let transient =
            create_state_backup(home.path(), backup_root.path(), "sync-current").unwrap();

        let scoped_error =
            delete_verified_full_backup(backup_root.path(), &scoped.backup_dir).unwrap_err();
        let transient_error =
            delete_verified_full_backup(backup_root.path(), &transient.backup_dir).unwrap_err();

        assert!(scoped_error.contains("persistent full"));
        assert!(transient_error.contains("persistent full"));
        assert!(scoped.backup_dir.exists());
        assert!(transient.backup_dir.exists());
    }

    #[test]
    fn v3_full_backup_remains_listable_restorable_and_deletable() {
        let home = tempdir().unwrap();
        seed_home(home.path());
        let backup_root = tempdir().unwrap();
        let backup = create_backup(home.path(), backup_root.path(), "v3-full").unwrap();
        let manifest_path = backup.backup_dir.join("manifest.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        value["version"] = serde_json::Value::from(SCOPED_BACKUP_MANIFEST_VERSION);
        value.as_object_mut().unwrap().remove("operationId");
        value.as_object_mut().unwrap().remove("role");
        fs::write(&manifest_path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

        fs::write(home.path().join("config.toml"), "model = \"changed\"\n").unwrap();
        restore_backup(&backup.backup_dir, home.path()).unwrap();
        assert!(fs::read_to_string(home.path().join("config.toml"))
            .unwrap()
            .contains("gpt-5.5"));
        let listed = list_recent_backups(backup_root.path(), 8).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].backup_dir, backup.backup_dir);

        delete_verified_full_backup(backup_root.path(), &backup.backup_dir).unwrap();
        assert!(!backup.backup_dir.exists());
    }

    #[test]
    fn verified_full_backup_deletion_rejects_an_outside_directory() {
        let home = tempdir().unwrap();
        seed_home(home.path());
        let managed_root = tempdir().unwrap();
        let outside_root = tempdir().unwrap();
        let outside = create_backup(home.path(), outside_root.path(), "outside-full").unwrap();

        let error =
            delete_verified_full_backup(managed_root.path(), &outside.backup_dir).unwrap_err();

        assert!(error.contains("outside the managed backup root"));
        assert!(outside.backup_dir.exists());
    }

    #[test]
    fn verified_full_backup_deletion_rejects_a_symlinked_directory() {
        let home = tempdir().unwrap();
        seed_home(home.path());
        let managed_root = tempdir().unwrap();
        let outside_root = tempdir().unwrap();
        let outside = create_backup(home.path(), outside_root.path(), "symlink-target").unwrap();
        let link = managed_root.path().join("linked-full");
        if let Err(error) = create_directory_symlink(&outside.backup_dir, &link) {
            if error.raw_os_error() == Some(1314) {
                let error = validate_directory_entry(true, true, "backup directory").unwrap_err();
                assert!(error.contains("not a regular directory"));
                assert!(outside.backup_dir.exists());
                return;
            }
            panic!("failed to create backup directory symlink: {error}");
        }

        let error = delete_verified_full_backup(managed_root.path(), &link).unwrap_err();

        assert!(error.contains("not a regular directory"));
        assert!(link.exists());
        assert!(outside.backup_dir.exists());
    }

    #[test]
    fn verified_full_backup_deletion_rejects_extra_files_and_payload_hash_drift() {
        let home = tempdir().unwrap();
        seed_home(home.path());
        let backup_root = tempdir().unwrap();
        let with_extra = create_backup(home.path(), backup_root.path(), "full-with-extra").unwrap();
        let extra = with_extra.backup_dir.join("unexpected.txt");
        fs::write(&extra, b"unexpected").unwrap();

        let extra_error =
            delete_verified_full_backup(backup_root.path(), &with_extra.backup_dir).unwrap_err();

        assert!(extra_error.contains("undeclared file"));
        assert!(with_extra.backup_dir.exists());
        assert!(extra.exists());

        let bitflipped = create_backup(home.path(), backup_root.path(), "full-bitflipped").unwrap();
        let payload = &bitflipped.files[0].backup_path;
        let mut bytes = fs::read(payload).unwrap();
        bytes[0] ^= 0xff;
        fs::write(payload, bytes).unwrap();

        let hash_error =
            delete_verified_full_backup(backup_root.path(), &bitflipped.backup_dir).unwrap_err();

        assert!(hash_error.contains("checksum mismatch"));
        assert!(bitflipped.backup_dir.exists());
    }

    #[test]
    fn completed_transient_checkpoints_are_removed_but_full_backups_are_retained() {
        let current = tempdir().unwrap();
        let shared = tempdir().unwrap();
        seed_home(current.path());
        seed_home(shared.path());
        let backup_root = tempdir().unwrap();
        let operation_id = "sync-transient";
        let current_checkpoint = state_checkpoint(
            current.path(),
            backup_root.path(),
            "sync-current",
            operation_id,
            CheckpointRole::Current,
        );
        let shared_checkpoint = state_checkpoint(
            shared.path(),
            backup_root.path(),
            "sync-shared",
            operation_id,
            CheckpointRole::Shared,
        );
        let full = create_backup(current.path(), backup_root.path(), "manual-full-backup").unwrap();
        let record = operation_for_checkpoints(
            operation_id,
            OperationAction::SyncSessions,
            OperationStatus::Succeeded,
            OperationPhase::Complete,
            &[&current_checkpoint, &shared_checkpoint],
        );

        let summary = cleanup_transient_checkpoints(
            backup_root.path(),
            &record,
            &[
                current_checkpoint.clone(),
                shared_checkpoint.clone(),
                full.clone(),
            ],
        );

        assert_eq!(summary.attempted_count, 2);
        assert_eq!(summary.failed_count, 0);
        assert_eq!(summary.reclaimed_count, 2);
        assert_eq!(summary.retained_count, 1);
        assert!(!current_checkpoint.backup_dir.exists());
        assert!(!shared_checkpoint.backup_dir.exists());
        assert!(full.backup_dir.exists());
        assert!(summary.warnings.is_empty());
    }

    #[test]
    fn completed_incremental_checkpoints_are_removed_as_an_exact_state_only_pair() {
        let current = tempdir().unwrap();
        let shared = tempdir().unwrap();
        seed_home(current.path());
        seed_home(shared.path());
        let backup_root = tempdir().unwrap();
        let operation_id = "incremental-transient";
        let current_checkpoint = state_checkpoint(
            current.path(),
            backup_root.path(),
            "incremental-current",
            operation_id,
            CheckpointRole::Current,
        );
        let shared_checkpoint = state_checkpoint(
            shared.path(),
            backup_root.path(),
            "incremental-shared",
            operation_id,
            CheckpointRole::Shared,
        );
        let record = operation_for_checkpoints(
            operation_id,
            OperationAction::IncrementalSync,
            OperationStatus::Succeeded,
            OperationPhase::Complete,
            &[&current_checkpoint, &shared_checkpoint],
        );

        let summary = cleanup_transient_checkpoints(
            backup_root.path(),
            &record,
            &[current_checkpoint.clone(), shared_checkpoint.clone()],
        );

        assert_eq!(summary.attempted_count, 2);
        assert_eq!(summary.reclaimed_count, 2);
        assert_eq!(summary.failed_count, 0);
        assert!(!current_checkpoint.backup_dir.exists());
        assert!(!shared_checkpoint.backup_dir.exists());
    }

    #[test]
    fn explicit_cleanup_keeps_informational_warnings_out_of_the_failure_count() {
        let current = tempdir().unwrap();
        let shared = tempdir().unwrap();
        seed_home(current.path());
        seed_home(shared.path());
        let backup_root = tempdir().unwrap();
        let operation_id = "sync-cleanup-with-retained-entries";
        let current_checkpoint = state_checkpoint(
            current.path(),
            backup_root.path(),
            "sync-current",
            operation_id,
            CheckpointRole::Current,
        );
        let shared_checkpoint = state_checkpoint(
            shared.path(),
            backup_root.path(),
            "sync-shared",
            operation_id,
            CheckpointRole::Shared,
        );
        let full = create_backup(current.path(), backup_root.path(), "manual-full-backup").unwrap();
        let unclassified = backup_root.path().join("unclassified");
        fs::create_dir(&unclassified).unwrap();
        fs::write(unclassified.join("unknown.bin"), b"retain").unwrap();
        let record = operation_for_checkpoints(
            operation_id,
            OperationAction::SyncSessions,
            OperationStatus::Succeeded,
            OperationPhase::Complete,
            &[&current_checkpoint, &shared_checkpoint],
        );

        let summary =
            cleanup_automatic_checkpoints(backup_root.path(), std::slice::from_ref(&record))
                .unwrap();

        assert_eq!(summary.attempted_count, 2);
        assert_eq!(summary.failed_count, 0);
        assert_eq!(summary.reclaimed_count, 2);
        assert_eq!(summary.retained_count, 2);
        assert!(!summary.warnings.is_empty());
        assert!(!current_checkpoint.backup_dir.exists());
        assert!(!shared_checkpoint.backup_dir.exists());
        assert!(full.backup_dir.exists());
        assert!(unclassified.exists());
    }

    #[test]
    fn explicit_cleanup_counts_real_remove_failures() {
        let current = tempdir().unwrap();
        let shared = tempdir().unwrap();
        seed_home(current.path());
        seed_home(shared.path());
        let backup_root = tempdir().unwrap();
        let operation_id = "sync-cleanup-remove-failure";
        let current_checkpoint = state_checkpoint(
            current.path(),
            backup_root.path(),
            "sync-current",
            operation_id,
            CheckpointRole::Current,
        );
        let shared_checkpoint = state_checkpoint(
            shared.path(),
            backup_root.path(),
            "sync-shared",
            operation_id,
            CheckpointRole::Shared,
        );
        let record = operation_for_checkpoints(
            operation_id,
            OperationAction::SyncSessions,
            OperationStatus::Succeeded,
            OperationPhase::Complete,
            &[&current_checkpoint, &shared_checkpoint],
        );

        let summary = cleanup_automatic_checkpoints_with_remove(
            backup_root.path(),
            std::slice::from_ref(&record),
            |_| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "injected remove failure",
                ))
            },
        )
        .unwrap();

        assert_eq!(summary.attempted_count, 2);
        assert_eq!(summary.failed_count, 2);
        assert_eq!(summary.reclaimed_count, 0);
        assert_eq!(summary.retained_count, 2);
        assert_eq!(summary.warnings.len(), 2);
        assert!(current_checkpoint.backup_dir.exists());
        assert!(shared_checkpoint.backup_dir.exists());
    }

    #[test]
    fn cleanup_retains_a_checkpoint_if_its_manifest_changes_after_creation() {
        let home = tempdir().unwrap();
        seed_home(home.path());
        let backup_root = tempdir().unwrap();
        let checkpoint = state_checkpoint(
            home.path(),
            backup_root.path(),
            "sync-current",
            "sync-manifest-drift",
            CheckpointRole::Current,
        );
        let record = operation_for_checkpoints(
            "sync-manifest-drift",
            OperationAction::SyncSessions,
            OperationStatus::Failed,
            OperationPhase::Backup,
            &[&checkpoint],
        );
        let mut stored: serde_json::Value =
            serde_json::from_slice(&fs::read(checkpoint.backup_dir.join("manifest.json")).unwrap())
                .unwrap();
        stored["reason"] = serde_json::Value::String("manual-full-backup".to_string());
        fs::write(
            checkpoint.backup_dir.join("manifest.json"),
            serde_json::to_vec_pretty(&stored).unwrap(),
        )
        .unwrap();

        let summary = cleanup_transient_checkpoints(
            backup_root.path(),
            &record,
            std::slice::from_ref(&checkpoint),
        );

        assert_eq!(summary.reclaimed_count, 0);
        assert_eq!(summary.retained_count, 1);
        assert!(checkpoint.backup_dir.exists());
        assert_eq!(summary.warnings.len(), 1);
    }

    #[test]
    fn cleanup_retains_a_checkpoint_with_an_undeclared_file() {
        let home = tempdir().unwrap();
        seed_home(home.path());
        let backup_root = tempdir().unwrap();
        let checkpoint = state_checkpoint(
            home.path(),
            backup_root.path(),
            "sync-current",
            "sync-extra-file",
            CheckpointRole::Current,
        );
        let record = operation_for_checkpoints(
            "sync-extra-file",
            OperationAction::SyncSessions,
            OperationStatus::Failed,
            OperationPhase::Backup,
            &[&checkpoint],
        );
        fs::write(checkpoint.backup_dir.join("untracked.txt"), b"keep").unwrap();

        let summary = cleanup_transient_checkpoints(
            backup_root.path(),
            &record,
            std::slice::from_ref(&checkpoint),
        );

        assert_eq!(summary.reclaimed_count, 0);
        assert_eq!(summary.retained_count, 1);
        assert!(checkpoint.backup_dir.join("untracked.txt").exists());
    }

    #[test]
    fn cleanup_retains_a_checkpoint_with_a_same_size_payload_bitflip() {
        let home = tempdir().unwrap();
        seed_home(home.path());
        let backup_root = tempdir().unwrap();
        let checkpoint = state_checkpoint(
            home.path(),
            backup_root.path(),
            "sync-current",
            "sync-payload-drift",
            CheckpointRole::Current,
        );
        let record = operation_for_checkpoints(
            "sync-payload-drift",
            OperationAction::SyncSessions,
            OperationStatus::Failed,
            OperationPhase::Backup,
            &[&checkpoint],
        );
        let payload = &checkpoint.files[0].backup_path;
        let mut bytes = fs::read(payload).unwrap();
        bytes[0] ^= 0xff;
        fs::write(payload, &bytes).unwrap();
        assert_eq!(fs::metadata(payload).unwrap().len(), bytes.len() as u64);

        let summary = cleanup_transient_checkpoints(
            backup_root.path(),
            &record,
            std::slice::from_ref(&checkpoint),
        );

        assert_eq!(summary.reclaimed_count, 0);
        assert_eq!(summary.retained_count, 1);
        assert!(checkpoint.backup_dir.exists());
        assert!(summary
            .warnings
            .iter()
            .any(|warning| warning.contains("checksum mismatch")));
    }

    #[test]
    fn explicit_cleanup_retains_a_pair_with_a_same_size_payload_bitflip() {
        let current = tempdir().unwrap();
        let shared = tempdir().unwrap();
        seed_home(current.path());
        seed_home(shared.path());
        let backup_root = tempdir().unwrap();
        let operation_id = "sync-corrupt-explicit-cleanup";
        let current_checkpoint = state_checkpoint(
            current.path(),
            backup_root.path(),
            "sync-current",
            operation_id,
            CheckpointRole::Current,
        );
        let shared_checkpoint = state_checkpoint(
            shared.path(),
            backup_root.path(),
            "sync-shared",
            operation_id,
            CheckpointRole::Shared,
        );
        let payload = &current_checkpoint.files[0].backup_path;
        let mut bytes = fs::read(payload).unwrap();
        bytes[0] ^= 0xff;
        fs::write(payload, &bytes).unwrap();
        let record = operation_for_checkpoints(
            operation_id,
            OperationAction::SyncSessions,
            OperationStatus::Succeeded,
            OperationPhase::Complete,
            &[&current_checkpoint, &shared_checkpoint],
        );

        let summary =
            cleanup_automatic_checkpoints(backup_root.path(), std::slice::from_ref(&record))
                .unwrap();

        assert_eq!(summary.reclaimed_count, 0);
        assert_eq!(summary.retained_count, 2);
        assert!(!summary.warnings.is_empty());
        assert!(current_checkpoint.backup_dir.exists());
        assert!(shared_checkpoint.backup_dir.exists());
    }

    #[test]
    fn completed_state_only_sync_checkpoints_are_transient() {
        let current = tempdir().unwrap();
        let shared = tempdir().unwrap();
        seed_home(current.path());
        seed_home(shared.path());
        let backup_root = tempdir().unwrap();
        let operation_id = "sync-state-only";
        let current_checkpoint = state_checkpoint(
            current.path(),
            backup_root.path(),
            "sync-current",
            operation_id,
            CheckpointRole::Current,
        );
        let shared_checkpoint = state_checkpoint(
            shared.path(),
            backup_root.path(),
            "sync-shared",
            operation_id,
            CheckpointRole::Shared,
        );
        let record = operation_for_checkpoints(
            operation_id,
            OperationAction::SyncSessions,
            OperationStatus::Succeeded,
            OperationPhase::Complete,
            &[&current_checkpoint, &shared_checkpoint],
        );

        let summary = cleanup_transient_checkpoints(
            backup_root.path(),
            &record,
            &[current_checkpoint.clone(), shared_checkpoint.clone()],
        );

        assert_eq!(summary.reclaimed_count, 2);
        assert_eq!(summary.retained_count, 0);
        assert!(!current_checkpoint.backup_dir.exists());
        assert!(!shared_checkpoint.backup_dir.exists());
    }

    #[test]
    fn completed_restore_visibility_checkpoint_is_transient() {
        let home = tempdir().unwrap();
        seed_home(home.path());
        let backup_root = tempdir().unwrap();
        let operation_id = "restore-visible-complete";
        let checkpoint = state_checkpoint(
            home.path(),
            backup_root.path(),
            "restore-sessions-visible",
            operation_id,
            CheckpointRole::Visibility,
        );
        let record = operation_for_checkpoints(
            operation_id,
            OperationAction::RestoreVisibility,
            OperationStatus::Succeeded,
            OperationPhase::Complete,
            &[&checkpoint],
        );

        let summary = cleanup_transient_checkpoints(
            backup_root.path(),
            &record,
            std::slice::from_ref(&checkpoint),
        );

        assert_eq!(summary.reclaimed_count, 1);
        assert_eq!(summary.retained_count, 0);
        assert!(summary.warnings.is_empty());
        assert!(!checkpoint.backup_dir.exists());
    }

    #[test]
    fn legacy_v2_switch_pair_is_retained_even_with_a_success_terminal() {
        let current = tempdir().unwrap();
        let shared = tempdir().unwrap();
        seed_home(current.path());
        seed_home(shared.path());
        let backup_root = tempdir().unwrap();
        let current_checkpoint =
            create_session_backup(current.path(), backup_root.path(), "switch-runtime-current")
                .unwrap();
        let shared_checkpoint =
            create_session_backup(shared.path(), backup_root.path(), "switch-runtime-shared")
                .unwrap();
        for checkpoint in [&current_checkpoint, &shared_checkpoint] {
            let manifest_path = checkpoint.backup_dir.join("manifest.json");
            let mut manifest: serde_json::Value =
                serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
            manifest["version"] = serde_json::Value::from(2);
            manifest.as_object_mut().unwrap().remove("scope");
            manifest.as_object_mut().unwrap().remove("trackedDatabases");
            fs::write(manifest_path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
        }
        let backup_dirs = vec![
            current_checkpoint.backup_dir.clone(),
            shared_checkpoint.backup_dir.clone(),
        ];
        let started_at_ms = current_checkpoint
            .created_at_ms
            .min(shared_checkpoint.created_at_ms)
            .saturating_sub(1);
        let completed_at_ms = current_checkpoint
            .created_at_ms
            .max(shared_checkpoint.created_at_ms)
            .saturating_add(1);
        let failed = OperationRecord {
            operation_id: "switch-failed".to_string(),
            action: OperationAction::SwitchRuntime,
            status: OperationStatus::RollbackFailed,
            phase: OperationPhase::Rollback,
            started_at_ms,
            completed_at_ms,
            backup_dirs: backup_dirs.clone(),
            counts: Default::default(),
        };
        let status =
            inspect_checkpoint_storage(backup_root.path(), std::slice::from_ref(&failed)).unwrap();
        assert_eq!(status.reclaimable_count, 0);

        let succeeded = OperationRecord {
            operation_id: "switch-succeeded".to_string(),
            status: OperationStatus::Succeeded,
            phase: OperationPhase::Complete,
            ..failed
        };
        let status =
            inspect_checkpoint_storage(backup_root.path(), std::slice::from_ref(&succeeded))
                .unwrap();
        assert_eq!(status.reclaimable_count, 0, "{status:?}");
        assert_eq!(status.retained_count, 2);

        let summary =
            cleanup_automatic_checkpoints(backup_root.path(), std::slice::from_ref(&succeeded))
                .unwrap();
        assert_eq!(summary.reclaimed_count, 0);
        assert!(backup_dirs[0].exists());
        assert!(backup_dirs[1].exists());
    }

    #[test]
    fn terminal_cleanup_never_reclaims_an_orphan_or_full_backup() {
        let current = tempdir().unwrap();
        let shared = tempdir().unwrap();
        seed_home(current.path());
        seed_home(shared.path());
        let backup_root = tempdir().unwrap();
        let operation_id = "sync-succeeded";
        let current_checkpoint = state_checkpoint(
            current.path(),
            backup_root.path(),
            "sync-current",
            operation_id,
            CheckpointRole::Current,
        );
        let shared_checkpoint = state_checkpoint(
            shared.path(),
            backup_root.path(),
            "sync-shared",
            operation_id,
            CheckpointRole::Shared,
        );
        let full = create_backup(current.path(), backup_root.path(), "restore-safety").unwrap();
        let record = OperationRecord {
            operation_id: operation_id.to_string(),
            action: OperationAction::SyncSessions,
            status: OperationStatus::Succeeded,
            phase: OperationPhase::Complete,
            started_at_ms: 1,
            completed_at_ms: 2,
            backup_dirs: vec![current_checkpoint.backup_dir.clone()],
            counts: Default::default(),
        };

        let status =
            inspect_checkpoint_storage(backup_root.path(), std::slice::from_ref(&record)).unwrap();

        assert_eq!(status.total_count, 3);
        assert_eq!(status.reclaimable_count, 0);
        assert_eq!(status.retained_count, 3);
        assert!(current_checkpoint.backup_dir.exists());
        assert!(shared_checkpoint.backup_dir.exists());
        assert!(full.backup_dir.exists());
    }

    #[test]
    fn terminal_cleanup_requires_a_unique_operation_id_and_matching_time_window() {
        let current = tempdir().unwrap();
        let shared = tempdir().unwrap();
        seed_home(current.path());
        seed_home(shared.path());
        let backup_root = tempdir().unwrap();
        let operation_id = "sync-unique";
        let current_checkpoint = state_checkpoint(
            current.path(),
            backup_root.path(),
            "sync-current",
            operation_id,
            CheckpointRole::Current,
        );
        let shared_checkpoint = state_checkpoint(
            shared.path(),
            backup_root.path(),
            "sync-shared",
            operation_id,
            CheckpointRole::Shared,
        );
        let started_at_ms = current_checkpoint
            .created_at_ms
            .min(shared_checkpoint.created_at_ms)
            .saturating_sub(1);
        let completed_at_ms = current_checkpoint
            .created_at_ms
            .max(shared_checkpoint.created_at_ms)
            .saturating_add(1);
        let record = OperationRecord {
            operation_id: operation_id.to_string(),
            action: OperationAction::SyncSessions,
            status: OperationStatus::Succeeded,
            phase: OperationPhase::Complete,
            started_at_ms,
            completed_at_ms,
            backup_dirs: vec![
                current_checkpoint.backup_dir.clone(),
                shared_checkpoint.backup_dir.clone(),
            ],
            counts: Default::default(),
        };
        let duplicate_id = OperationRecord {
            backup_dirs: Vec::new(),
            ..record.clone()
        };
        let duplicate_status =
            inspect_checkpoint_storage(backup_root.path(), &[record.clone(), duplicate_id])
                .unwrap();
        assert_eq!(duplicate_status.reclaimable_count, 0);

        let outside_window = OperationRecord {
            started_at_ms: 1,
            completed_at_ms: 2,
            ..record.clone()
        };
        let outside_status =
            inspect_checkpoint_storage(backup_root.path(), &[outside_window]).unwrap();
        assert_eq!(outside_status.reclaimable_count, 0);

        let completed_sync_rollback = OperationRecord {
            status: OperationStatus::RolledBack,
            phase: OperationPhase::Rollback,
            ..record.clone()
        };
        let rollback_status =
            inspect_checkpoint_storage(backup_root.path(), &[completed_sync_rollback]).unwrap();
        assert_eq!(rollback_status.reclaimable_count, 2);

        let valid_status = inspect_checkpoint_storage(backup_root.path(), &[record]).unwrap();
        assert_eq!(valid_status.reclaimable_count, 2);
    }

    #[test]
    fn terminal_cleanup_requires_exact_manifest_operation_ids_and_roles() {
        let current = tempdir().unwrap();
        let shared = tempdir().unwrap();
        seed_home(current.path());
        seed_home(shared.path());

        let wrong_id_root = tempdir().unwrap();
        let current_checkpoint = state_checkpoint(
            current.path(),
            wrong_id_root.path(),
            "sync-current",
            "manifest-operation",
            CheckpointRole::Current,
        );
        let shared_checkpoint = state_checkpoint(
            shared.path(),
            wrong_id_root.path(),
            "sync-shared",
            "manifest-operation",
            CheckpointRole::Shared,
        );
        let wrong_id_record = operation_for_checkpoints(
            "different-operation",
            OperationAction::SyncSessions,
            OperationStatus::Succeeded,
            OperationPhase::Complete,
            &[&current_checkpoint, &shared_checkpoint],
        );
        let wrong_id_status =
            inspect_checkpoint_storage(wrong_id_root.path(), &[wrong_id_record]).unwrap();
        assert_eq!(wrong_id_status.reclaimable_count, 0);
        assert_eq!(wrong_id_status.retained_count, 2);

        let wrong_role_root = tempdir().unwrap();
        let current_checkpoint = state_checkpoint(
            current.path(),
            wrong_role_root.path(),
            "sync-current",
            "wrong-role-operation",
            CheckpointRole::Shared,
        );
        let shared_checkpoint = state_checkpoint(
            shared.path(),
            wrong_role_root.path(),
            "sync-shared",
            "wrong-role-operation",
            CheckpointRole::Current,
        );
        let wrong_role_record = operation_for_checkpoints(
            "wrong-role-operation",
            OperationAction::SyncSessions,
            OperationStatus::Succeeded,
            OperationPhase::Complete,
            &[&current_checkpoint, &shared_checkpoint],
        );
        let wrong_role_status =
            inspect_checkpoint_storage(wrong_role_root.path(), &[wrong_role_record]).unwrap();
        assert_eq!(wrong_role_status.reclaimable_count, 0);
        assert_eq!(wrong_role_status.retained_count, 2);
    }

    #[test]
    fn unbound_v3_checkpoint_cannot_be_claimed_by_a_new_terminal_record() {
        let home = tempdir().unwrap();
        seed_home(home.path());
        let backup_root = tempdir().unwrap();
        let checkpoint =
            create_state_backup(home.path(), backup_root.path(), "sync-current").unwrap();
        let manifest_path = checkpoint.backup_dir.join("manifest.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        value["version"] = serde_json::Value::from(SCOPED_BACKUP_MANIFEST_VERSION);
        value.as_object_mut().unwrap().remove("operationId");
        value.as_object_mut().unwrap().remove("role");
        fs::write(&manifest_path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
        let record = operation_for_checkpoints(
            "arbitrary-new-terminal",
            OperationAction::SyncSessions,
            OperationStatus::Failed,
            OperationPhase::Backup,
            &[&checkpoint],
        );

        let status = inspect_checkpoint_storage(backup_root.path(), &[record]).unwrap();

        assert_eq!(status.reclaimable_count, 0);
        assert_eq!(status.retained_count, 1);
        assert!(checkpoint.backup_dir.exists());
    }

    #[test]
    fn terminal_cleanup_rejects_aliases_for_manifest_backup_paths() {
        let current = tempdir().unwrap();
        let shared = tempdir().unwrap();
        seed_home(current.path());
        seed_home(shared.path());
        let backup_root = tempdir().unwrap();
        let operation_id = "sync-path-alias";
        let current_checkpoint = state_checkpoint(
            current.path(),
            backup_root.path(),
            "sync-current",
            operation_id,
            CheckpointRole::Current,
        );
        let shared_checkpoint = state_checkpoint(
            shared.path(),
            backup_root.path(),
            "sync-shared",
            operation_id,
            CheckpointRole::Shared,
        );
        let alias = |manifest: &BackupManifest| {
            manifest
                .backup_dir
                .join("..")
                .join(manifest.backup_dir.file_name().unwrap())
        };
        let record = OperationRecord {
            operation_id: operation_id.to_string(),
            action: OperationAction::SyncSessions,
            status: OperationStatus::Succeeded,
            phase: OperationPhase::Complete,
            started_at_ms: current_checkpoint
                .created_at_ms
                .min(shared_checkpoint.created_at_ms)
                .saturating_sub(1),
            completed_at_ms: current_checkpoint
                .created_at_ms
                .max(shared_checkpoint.created_at_ms)
                .saturating_add(1),
            backup_dirs: vec![alias(&current_checkpoint), alias(&shared_checkpoint)],
            counts: Default::default(),
        };

        let status = inspect_checkpoint_storage(backup_root.path(), &[record]).unwrap();

        assert_eq!(status.reclaimable_count, 0);
        assert_eq!(status.retained_count, 2);
        assert!(current_checkpoint.backup_dir.exists());
        assert!(shared_checkpoint.backup_dir.exists());
    }

    #[test]
    fn terminal_cleanup_rejects_mixed_runtime_checkpoint_scopes() {
        let current = tempdir().unwrap();
        let shared = tempdir().unwrap();
        seed_home(current.path());
        seed_home(shared.path());
        let backup_root = tempdir().unwrap();
        let current_checkpoint =
            create_runtime_backup(current.path(), backup_root.path(), "switch-runtime-current")
                .unwrap();
        let shared_checkpoint =
            create_state_backup(shared.path(), backup_root.path(), "switch-runtime-shared")
                .unwrap();
        let record = OperationRecord {
            operation_id: "switch-mixed-scopes".to_string(),
            action: OperationAction::SwitchRuntime,
            status: OperationStatus::Succeeded,
            phase: OperationPhase::Complete,
            started_at_ms: current_checkpoint
                .created_at_ms
                .min(shared_checkpoint.created_at_ms)
                .saturating_sub(1),
            completed_at_ms: current_checkpoint
                .created_at_ms
                .max(shared_checkpoint.created_at_ms)
                .saturating_add(1),
            backup_dirs: vec![current_checkpoint.backup_dir, shared_checkpoint.backup_dir],
            counts: Default::default(),
        };

        let status = inspect_checkpoint_storage(backup_root.path(), &[record]).unwrap();

        assert_eq!(status.reclaimable_count, 0);
        assert_eq!(status.retained_count, 2);
    }

    #[test]
    fn terminal_sync_cleanup_accepts_equal_state_only_scopes_but_rejects_mixed_scopes() {
        let current = tempdir().unwrap();
        let shared = tempdir().unwrap();
        seed_home(current.path());
        seed_home(shared.path());
        let backup_root = tempdir().unwrap();
        let operation_id = "sync-state-only";
        let current_state = state_checkpoint(
            current.path(),
            backup_root.path(),
            "sync-current",
            operation_id,
            CheckpointRole::Current,
        );
        let shared_state = state_checkpoint(
            shared.path(),
            backup_root.path(),
            "sync-shared",
            operation_id,
            CheckpointRole::Shared,
        );
        let state_record = OperationRecord {
            operation_id: operation_id.to_string(),
            action: OperationAction::SyncSessions,
            status: OperationStatus::Succeeded,
            phase: OperationPhase::Complete,
            started_at_ms: current_state
                .created_at_ms
                .min(shared_state.created_at_ms)
                .saturating_sub(1),
            completed_at_ms: current_state
                .created_at_ms
                .max(shared_state.created_at_ms)
                .saturating_add(1),
            backup_dirs: vec![
                current_state.backup_dir.clone(),
                shared_state.backup_dir.clone(),
            ],
            counts: Default::default(),
        };

        let state_status = inspect_checkpoint_storage(backup_root.path(), &[state_record]).unwrap();
        assert_eq!(state_status.reclaimable_count, 2);

        let mixed_root = tempdir().unwrap();
        let current_sessions =
            create_session_backup(current.path(), mixed_root.path(), "sync-current").unwrap();
        let shared_state =
            create_state_backup(shared.path(), mixed_root.path(), "sync-shared").unwrap();
        let mixed_record = OperationRecord {
            operation_id: "sync-mixed-scopes".to_string(),
            action: OperationAction::SyncSessions,
            status: OperationStatus::Succeeded,
            phase: OperationPhase::Complete,
            started_at_ms: current_sessions
                .created_at_ms
                .min(shared_state.created_at_ms)
                .saturating_sub(1),
            completed_at_ms: current_sessions
                .created_at_ms
                .max(shared_state.created_at_ms)
                .saturating_add(1),
            backup_dirs: vec![current_sessions.backup_dir, shared_state.backup_dir],
            counts: Default::default(),
        };

        let mixed_status = inspect_checkpoint_storage(mixed_root.path(), &[mixed_record]).unwrap();
        assert_eq!(mixed_status.reclaimable_count, 0);
        assert_eq!(mixed_status.retained_count, 2);
    }

    #[test]
    fn failed_backup_phase_cleanup_accepts_one_or_two_bound_v4_checkpoints() {
        let current = tempdir().unwrap();
        let shared = tempdir().unwrap();
        seed_home(current.path());
        seed_home(shared.path());

        let single_root = tempdir().unwrap();
        let single = state_checkpoint(
            current.path(),
            single_root.path(),
            "sync-current",
            "sync-prewrite-single",
            CheckpointRole::Current,
        );
        let single_record = operation_for_checkpoints(
            "sync-prewrite-single",
            OperationAction::SyncSessions,
            OperationStatus::Failed,
            OperationPhase::Backup,
            &[&single],
        );
        let single_status =
            inspect_checkpoint_storage(single_root.path(), std::slice::from_ref(&single_record))
                .unwrap();
        assert_eq!(single_status.reclaimable_count, 1);
        let single_cleanup =
            cleanup_automatic_checkpoints(single_root.path(), std::slice::from_ref(&single_record))
                .unwrap();
        assert_eq!(single_cleanup.reclaimed_count, 1);
        assert!(!single.backup_dir.exists());

        let pair_root = tempdir().unwrap();
        let current_checkpoint = runtime_state_checkpoint(
            current.path(),
            pair_root.path(),
            "switch-runtime-current",
            "switch-prewrite-pair",
            CheckpointRole::Current,
        );
        let shared_checkpoint = state_checkpoint(
            shared.path(),
            pair_root.path(),
            "switch-runtime-shared",
            "switch-prewrite-pair",
            CheckpointRole::Shared,
        );
        let pair_record = operation_for_checkpoints(
            "switch-prewrite-pair",
            OperationAction::SwitchRuntime,
            OperationStatus::Failed,
            OperationPhase::Backup,
            &[&current_checkpoint, &shared_checkpoint],
        );
        let pair_status = inspect_checkpoint_storage(pair_root.path(), &[pair_record]).unwrap();
        assert_eq!(pair_status.reclaimable_count, 2);
    }

    #[test]
    fn failed_backup_phase_cleanup_rejects_mixed_v2_and_apply_failures() {
        let current = tempdir().unwrap();
        let shared = tempdir().unwrap();
        seed_home(current.path());
        seed_home(shared.path());

        let mixed_root = tempdir().unwrap();
        let current_checkpoint = runtime_state_checkpoint(
            current.path(),
            mixed_root.path(),
            "switch-runtime-current",
            "switch-prewrite-mixed",
            CheckpointRole::Current,
        );
        let shared_checkpoint =
            create_session_backup(shared.path(), mixed_root.path(), "switch-runtime-shared")
                .unwrap();
        let mixed_record = operation_for_checkpoints(
            "switch-prewrite-mixed",
            OperationAction::SwitchRuntime,
            OperationStatus::Failed,
            OperationPhase::Backup,
            &[&current_checkpoint, &shared_checkpoint],
        );
        let mixed_status = inspect_checkpoint_storage(mixed_root.path(), &[mixed_record]).unwrap();
        assert_eq!(mixed_status.reclaimable_count, 0);

        let legacy_root = tempdir().unwrap();
        let legacy =
            create_state_backup(current.path(), legacy_root.path(), "sync-current").unwrap();
        let manifest_path = legacy.backup_dir.join("manifest.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        value["version"] = serde_json::Value::from(2);
        value.as_object_mut().unwrap().remove("scope");
        value.as_object_mut().unwrap().remove("trackedDatabases");
        fs::write(&manifest_path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
        let legacy_record = operation_for_checkpoints(
            "sync-prewrite-v2",
            OperationAction::SyncSessions,
            OperationStatus::Failed,
            OperationPhase::Backup,
            &[&legacy],
        );
        let legacy_status =
            inspect_checkpoint_storage(legacy_root.path(), &[legacy_record]).unwrap();
        assert_eq!(legacy_status.reclaimable_count, 0);

        let apply_root = tempdir().unwrap();
        let apply = state_checkpoint(
            current.path(),
            apply_root.path(),
            "sync-current",
            "sync-apply-failed",
            CheckpointRole::Current,
        );
        let apply_record = operation_for_checkpoints(
            "sync-apply-failed",
            OperationAction::SyncSessions,
            OperationStatus::Failed,
            OperationPhase::Apply,
            &[&apply],
        );
        let apply_status = inspect_checkpoint_storage(apply_root.path(), &[apply_record]).unwrap();
        assert_eq!(apply_status.reclaimable_count, 0);
    }

    #[test]
    fn checkpoint_storage_reconstructs_the_latest_failed_cleanup_counts() {
        let backup_root = tempdir().unwrap();
        let record = OperationRecord {
            operation_id: "cleanup-partial".to_string(),
            action: OperationAction::CleanupCheckpoints,
            status: OperationStatus::Failed,
            phase: OperationPhase::Apply,
            started_at_ms: 1,
            completed_at_ms: 2,
            backup_dirs: Vec::new(),
            counts: BTreeMap::from([
                ("attemptedCount".to_string(), 4),
                ("failedCount".to_string(), 1),
                ("reclaimedCount".to_string(), 3),
                ("reclaimedBytes".to_string(), 4096),
                ("retainedCount".to_string(), 2),
            ]),
        };

        let status =
            inspect_checkpoint_storage(backup_root.path(), std::slice::from_ref(&record)).unwrap();
        let cleanup = status.last_cleanup.unwrap();

        assert_eq!(cleanup.operation_id, "cleanup-partial");
        assert_eq!(cleanup.attempted_count, 4);
        assert_eq!(cleanup.failed_count, 1);
        assert_eq!(cleanup.reclaimed_count, 3);
        assert_eq!(cleanup.reclaimed_bytes, 4096);
        assert_eq!(cleanup.retained_count, 2);
    }

    #[test]
    fn terminal_restore_visibility_cleanup_accepts_one_strict_state_checkpoint() {
        let home = tempdir().unwrap();
        seed_home(home.path());
        let backup_root = tempdir().unwrap();
        let checkpoint = state_checkpoint(
            home.path(),
            backup_root.path(),
            "restore-sessions-visible",
            "restore-visible-complete",
            CheckpointRole::Visibility,
        );
        let record = operation_for_checkpoints(
            "restore-visible-complete",
            OperationAction::RestoreVisibility,
            OperationStatus::Succeeded,
            OperationPhase::Complete,
            &[&checkpoint],
        );

        let status =
            inspect_checkpoint_storage(backup_root.path(), std::slice::from_ref(&record)).unwrap();

        assert_eq!(status.reclaimable_count, 1);
        assert_eq!(status.retained_count, 0);
        let cleanup =
            cleanup_automatic_checkpoints(backup_root.path(), std::slice::from_ref(&record))
                .unwrap();
        assert_eq!(cleanup.reclaimed_count, 1);
        assert!(!checkpoint.backup_dir.exists());
    }

    #[test]
    fn verified_payload_extraction_does_not_interpret_backed_up_config_paths() {
        let home = tempdir().unwrap();
        let rollout = seed_home(home.path());
        fs::write(
            home.path().join("config.toml"),
            "sqlite_home = \"Z:\\\\must-not-be-opened\"\n",
        )
        .unwrap();
        let backup_root = tempdir().unwrap();
        let backup = create_local_backup(home.path(), backup_root.path(), "manual-full").unwrap();
        let output_root = tempdir().unwrap();
        let output = output_root.path().join("isolated.jsonl");
        let relative = rollout.strip_prefix(home.path()).unwrap();

        let (bytes, sha256) =
            extract_verified_backup_file(&backup.backup_dir, relative, &output).unwrap();

        assert_eq!(bytes, fs::metadata(&rollout).unwrap().len());
        assert_eq!(fs::read(&output).unwrap(), fs::read(&rollout).unwrap());
        assert_eq!(sha256.len(), 64);
        assert!(!output_root.path().join("state_5.sqlite").exists());
    }
}

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    io::{Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;

use rusqlite::{Connection, OpenFlags, MAIN_DB};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    chat_process_state::{
        backup_source as chat_process_state_backup_source,
        existing_restore_target as existing_chat_process_state_restore_target,
        read_snapshot as read_chat_process_state_snapshot,
        restore_target as chat_process_state_restore_target,
        validate_snapshot_bytes as validate_chat_process_state_bytes,
        CHAT_PROCESS_STATE_RELATIVE_PATH,
    },
    codex_paths::{
        local_codex_paths, resolve_user_codex_paths, validate_absolute_root, CodexPaths,
    },
    crypto::{protect, unprotect},
    file_ops::{atomic_rewrite, atomic_write, walk_jsonl_files},
    operation_log::{OperationAction, OperationPhase, OperationRecord, OperationStatus},
};

static BACKUP_COUNTER: AtomicU64 = AtomicU64::new(0);
const SCOPED_BACKUP_MANIFEST_VERSION: u32 = 3;
const BACKUP_MANIFEST_VERSION: u32 = 4;

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

struct RestoreStage {
    root: PathBuf,
    files: Vec<StagedBackupFile>,
}

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

impl Drop for RestoreStage {
    fn drop(&mut self) {
        self.files.clear();
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct StagedBackupFile {
    relative_path: PathBuf,
    plaintext_bytes: u64,
    plaintext_sha256: String,
    handle: fs::File,
}

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

fn sqlite_sidecar(database: &Path, suffix: &str) -> PathBuf {
    let mut name = database.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    database.with_file_name(name)
}

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
        finish_backup_creation_with_cleanup, finish_capacity_preflight, inspect_checkpoint_storage,
        list_recent_backups, load_process_state_checkpoint, migrate_legacy_plaintext_auth,
        percentage_ceil, preflight_backup_capacity, preflight_backup_capacity_for_sources,
        preflight_backup_capacity_with_paths, restore_backup, restore_staged_backup,
        restore_verified_backup, sqlite_logical_bytes, stage_backup_payloads,
        validate_directory_entry, verify_backup, BackupCapacitySource, BackupManifest, BackupScope,
        BackupSourceCapacityMetadata, CheckpointRole, BACKUP_FILE_OVERHEAD_BYTES,
        CHAT_PROCESS_STATE_RELATIVE_PATH, MANIFEST_BASE_OVERHEAD_BYTES,
        MANIFEST_ENTRY_OVERHEAD_BYTES, MAX_DPAPI_PAYLOAD_BYTES, MIN_CAPACITY_RESERVE_BYTES,
        SCOPED_BACKUP_MANIFEST_VERSION,
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
}

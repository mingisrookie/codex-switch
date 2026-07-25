use std::{
    collections::HashSet,
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, OpenFlags, MAIN_DB};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    codex_paths::{
        local_codex_paths, resolve_user_codex_paths, validate_absolute_root, CodexPaths,
    },
    crypto::{protect, unprotect},
    file_ops::{atomic_write, walk_jsonl_files},
};

static BACKUP_COUNTER: AtomicU64 = AtomicU64::new(0);
const BACKUP_MANIFEST_VERSION: u32 = 3;

const BACKUP_FILE_OVERHEAD_BYTES: u64 = 64 * 1024;
const MANIFEST_BASE_OVERHEAD_BYTES: u64 = 1024 * 1024;
const MANIFEST_ENTRY_OVERHEAD_BYTES: u64 = 4 * 1024;
const MIN_CAPACITY_RESERVE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const CAPACITY_RESERVE_PERCENT: u64 = 15;
const BACKUP_ROOT_COUNT: u64 = 2;
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
    Sessions,
    StateOnly,
}

impl BackupScope {
    fn tracks_runtime_files(self) -> bool {
        matches!(self, Self::Full | Self::Runtime)
    }

    fn tracks_sessions(self) -> bool {
        !matches!(self, Self::StateOnly)
    }

    fn tracked_databases(self) -> &'static [&'static str] {
        match self {
            Self::Full => &MANAGED_DATABASES,
            Self::Runtime | Self::Sessions | Self::StateOnly => &[STATE_DATABASE],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BackupManifest {
    pub version: u32,
    pub reason: String,
    pub created_at_ms: u128,
    pub source_root: PathBuf,
    pub root_existed: bool,
    #[serde(default)]
    pub scope: BackupScope,
    #[serde(default)]
    pub tracked_databases: Vec<String>,
    #[serde(default)]
    pub state_db_is_local: bool,
    pub complete_sessions: bool,
    pub backup_dir: PathBuf,
    pub files: Vec<BackupFile>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackupCapacityPreflight {
    pub required_bytes: u64,
    pub available_bytes: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct BackupSourceCapacityMetadata {
    plaintext_payload_bytes: u64,
    file_count: u64,
    sqlite_logical_bytes: u64,
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

fn preflight_scoped_backup_capacity(
    destination_root: &Path,
    current_home: &Path,
    current_scope: BackupScope,
    shared_home: &Path,
    shared_scope: BackupScope,
) -> Result<BackupCapacityPreflight, String> {
    let destination_root = validate_absolute_root(destination_root, "backup destination root")
        .map_err(|_| capacity_preflight_error())?;
    let current_home = validate_absolute_root(current_home, "current ChatGPT home")
        .map_err(|_| capacity_preflight_error())?;
    let shared_home = validate_absolute_root(shared_home, "shared session root")
        .map_err(|_| capacity_preflight_error())?;
    let current_paths =
        resolve_user_codex_paths(&current_home).map_err(|_| capacity_preflight_error())?;
    if ensure_roots_disjoint(
        &destination_root,
        "backup destination root",
        &current_home,
        "current ChatGPT home",
    )
    .and_then(|_| {
        ensure_roots_disjoint(
            &destination_root,
            "backup destination root",
            &shared_home,
            "shared session root",
        )
    })
    .and_then(|_| {
        ensure_roots_disjoint(
            &current_home,
            "current ChatGPT home",
            &shared_home,
            "shared session root",
        )
    })
    .and_then(|_| {
        ensure_roots_disjoint(
            &current_paths.sqlite_home,
            "current SQLite root",
            &shared_home,
            "shared session root",
        )
    })
    .and_then(|_| {
        ensure_roots_disjoint(
            &current_paths.sqlite_home,
            "current SQLite root",
            &destination_root,
            "backup destination root",
        )
    })
    .is_err()
    {
        return Err(capacity_preflight_error());
    }
    let current = collect_backup_capacity_metadata(&current_home, &current_paths, current_scope)
        .map_err(|_| capacity_preflight_error())?;
    let shared = collect_backup_capacity_metadata(
        &shared_home,
        &local_codex_paths(&shared_home),
        shared_scope,
    )
    .map_err(|_| capacity_preflight_error())?;
    let required_bytes =
        estimate_two_root_peak(current, shared).map_err(|_| capacity_preflight_error())?;
    let available_bytes = available_backup_bytes(&destination_root)?;

    finish_capacity_preflight(required_bytes, available_bytes)
}

pub(crate) fn ensure_roots_disjoint(
    left: &Path,
    left_label: &str,
    right: &Path,
    right_label: &str,
) -> Result<(), String> {
    let left = resolve_root_for_overlap(left)?;
    let right = resolve_root_for_overlap(right)?;
    if left == right || left.starts_with(&right) || right.starts_with(&left) {
        return Err(format!("{left_label} and {right_label} must not overlap"));
    }
    Ok(())
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
        sources.push(home.join("auth.json"));
        sources.push(home.join("config.toml"));
    }
    if scope.tracks_sessions() {
        sources.push(paths.session_index.clone());
    }
    for source in sources {
        if let Some(bytes) = regular_file_len(&source)? {
            add_capacity_file(&mut capacity, bytes)?;
        }
    }

    for (database_name, database) in managed_sqlite_paths(paths) {
        if !scope.tracked_databases().contains(&database_name) {
            continue;
        }
        if regular_file_len(database)?.is_some() {
            let logical_bytes = sqlite_logical_bytes(database)?;
            add_capacity_file(&mut capacity, logical_bytes)?;
            capacity.sqlite_logical_bytes = capacity.sqlite_logical_bytes.max(logical_bytes);
        }
    }

    if !scope.tracks_sessions() {
        return Ok(capacity);
    }
    match fs::metadata(&paths.sessions_dir) {
        Ok(metadata) if metadata.is_dir() => {
            for path in walk_jsonl_files(&paths.sessions_dir).map_err(|_| ())? {
                let bytes = regular_file_len(&path)?.ok_or(())?;
                add_capacity_file(&mut capacity, bytes)?;
            }
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(()),
    }

    Ok(capacity)
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
    capacity.plaintext_payload_bytes = capacity
        .plaintext_payload_bytes
        .checked_add(plaintext_bytes)
        .ok_or(())?;
    capacity.file_count = capacity.file_count.checked_add(1).ok_or(())?;
    Ok(())
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

fn estimate_two_root_peak(
    current: BackupSourceCapacityMetadata,
    shared: BackupSourceCapacityMetadata,
) -> Result<u64, ()> {
    let plaintext_payload_bytes = current
        .plaintext_payload_bytes
        .checked_add(shared.plaintext_payload_bytes)
        .ok_or(())?;
    let file_count = current
        .file_count
        .checked_add(shared.file_count)
        .ok_or(())?;
    let encrypted_payload_overhead = file_count
        .checked_mul(BACKUP_FILE_OVERHEAD_BYTES)
        .ok_or(())?;
    let manifest_overhead = BACKUP_ROOT_COUNT
        .checked_mul(MANIFEST_BASE_OVERHEAD_BYTES)
        .and_then(|base| {
            file_count
                .checked_mul(MANIFEST_ENTRY_OVERHEAD_BYTES)
                .and_then(|entries| base.checked_add(entries))
        })
        .ok_or(())?;
    let sqlite_workspace = current
        .sqlite_logical_bytes
        .max(shared.sqlite_logical_bytes);
    let peak_without_reserve = plaintext_payload_bytes
        .checked_add(encrypted_payload_overhead)
        .and_then(|value| value.checked_add(manifest_overhead))
        .and_then(|value| value.checked_add(sqlite_workspace))
        .ok_or(())?;
    let percentage_reserve = percentage_ceil(peak_without_reserve, CAPACITY_RESERVE_PERCENT)?;
    let reserve = MIN_CAPACITY_RESERVE_BYTES.max(percentage_reserve);
    peak_without_reserve.checked_add(reserve).ok_or(())
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
    create_scoped_backup_with_paths(home, destination_root, reason, paths, BackupScope::Full)
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
    )
}

pub fn create_runtime_backup(
    home: &Path,
    destination_root: &Path,
    reason: &str,
) -> Result<BackupManifest, String> {
    let paths = resolve_user_codex_paths(home)?;
    create_scoped_backup_with_paths(home, destination_root, reason, paths, BackupScope::Runtime)
}

pub fn create_session_backup(
    home: &Path,
    destination_root: &Path,
    reason: &str,
) -> Result<BackupManifest, String> {
    let paths = resolve_user_codex_paths(home)?;
    create_scoped_backup_with_paths(home, destination_root, reason, paths, BackupScope::Sessions)
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
    )
}

pub(crate) fn create_backup_with_paths(
    home: &Path,
    destination_root: &Path,
    reason: &str,
    paths: CodexPaths,
) -> Result<BackupManifest, String> {
    create_scoped_backup_with_paths(home, destination_root, reason, paths, BackupScope::Full)
}

pub(crate) fn create_state_backup_with_paths(
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
        BackupScope::StateOnly,
    )
}

fn create_scoped_backup_with_paths(
    home: &Path,
    destination_root: &Path,
    reason: &str,
    paths: CodexPaths,
    scope: BackupScope,
) -> Result<BackupManifest, String> {
    let home = validate_absolute_root(home, "backup source root")?;
    let destination_root = validate_absolute_root(destination_root, "backup destination root")?;
    validate_absolute_root(&paths.sqlite_home, "SQLite root")?;
    if paths.codex_home != home {
        return Err("resolved backup paths do not match the source root".to_string());
    }
    ensure_roots_disjoint(
        &home,
        "backup source root",
        &destination_root,
        "backup destination root",
    )?;
    ensure_roots_disjoint(
        &paths.sqlite_home,
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

    let result = create_backup_in_dir(&home, &backup_dir, reason, created_at_ms, paths, scope);
    if result.is_err() {
        let _ = fs::remove_dir_all(&backup_dir);
    }
    result
}

fn create_backup_in_dir(
    home: &Path,
    backup_dir: &Path,
    reason: &str,
    created_at_ms: u128,
    paths: CodexPaths,
    scope: BackupScope,
) -> Result<BackupManifest, String> {
    let root_existed = home.exists();
    let mut files = Vec::new();

    let mut sources = Vec::new();
    if scope.tracks_runtime_files() {
        sources.push((home.join("auth.json"), PathBuf::from("auth.json")));
        sources.push((home.join("config.toml"), PathBuf::from("config.toml")));
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

    if scope.tracks_sessions() && paths.sessions_dir.is_dir() {
        for path in walk_jsonl_files(&paths.sessions_dir)? {
            let relative = path
                .strip_prefix(home)
                .map_err(|error| format!("failed to map session backup path: {error}"))?
                .to_path_buf();
            files.push(encrypt_payload(&path, backup_dir, &relative)?);
        }
    }
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));

    let manifest = BackupManifest {
        version: BACKUP_MANIFEST_VERSION,
        reason: reason.to_string(),
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

fn read_backup_manifest(backup_dir: &Path) -> Result<BackupManifest, String> {
    let manifest_path = backup_dir.join("manifest.json");
    let raw = fs::read(&manifest_path)
        .map_err(|error| format!("failed to read backup manifest: {error}"))?;
    let value: serde_json::Value = serde_json::from_slice(&raw)
        .map_err(|error| format!("failed to parse backup manifest: {error}"))?;
    let mut manifest: BackupManifest = serde_json::from_value(value.clone())
        .map_err(|error| format!("failed to parse backup manifest: {error}"))?;
    if !matches!(manifest.version, 2 | BACKUP_MANIFEST_VERSION) {
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
    if manifest.version >= BACKUP_MANIFEST_VERSION {
        let object = raw
            .as_object()
            .ok_or_else(|| "backup manifest must be a JSON object".to_string())?;
        if !object.contains_key("scope") || !object.contains_key("trackedDatabases") {
            return Err("version 3 backup manifest is missing scope metadata".to_string());
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
    if relative == "session_index.jsonl" {
        return scope.tracks_sessions();
    }
    scope.tracks_sessions()
        && relative_path.starts_with(Path::new("sessions"))
        && relative_path.extension().and_then(|value| value.to_str()) == Some("jsonl")
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
        clear_known_codex_state(target_home, &old_paths, &manifest)?;
        return Ok(RestoreResult {
            backup_dir: backup_dir.to_path_buf(),
            target_root: target_home.to_path_buf(),
            restored_files: 0,
            verified: true,
        });
    }

    fs::create_dir_all(target_home)
        .map_err(|error| format!("failed to create restore target: {error}"))?;
    remove_absent_core_files(&manifest, target_home, &old_paths)?;
    if manifest.complete_sessions && manifest_scope(&manifest).tracks_sessions() {
        remove_extra_session_files(&manifest, target_home)?;
    }

    let mut restored_files = 0;
    if let Some(config) = manifest
        .files
        .iter()
        .find(|file| file.relative_path == Path::new("config.toml"))
    {
        restore_file(config, &target_home.join("config.toml"))?;
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
    remove_absent_core_files(&manifest, target_home, &paths)?;
    for (relative, old_database) in managed_sqlite_paths(&old_paths) {
        if !manifest_tracks_database(&manifest, relative) {
            continue;
        }
        let new_database = sqlite_restore_target(&paths, relative)
            .expect("managed SQLite relative paths must be routable");
        if old_database != new_database {
            remove_sqlite_files(old_database)?;
        }
    }
    for file in &manifest.files {
        if file.relative_path == Path::new("config.toml") {
            continue;
        }
        let target = restore_target(&paths, target_home, &file.relative_path)?;
        restore_file(file, &target)?;
        restored_files += 1;
    }
    for (relative, database) in managed_sqlite_paths(&paths) {
        if !manifest_tracks_database(&manifest, relative) {
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

fn restore_file(file: &BackupFile, target: &Path) -> Result<(), String> {
    let encrypted = fs::read(&file.backup_path)
        .map_err(|error| format!("failed to read backup payload: {error}"))?;
    let plaintext = unprotect(&encrypted)?;
    atomic_write(target, &plaintext)
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
    for (backup_dir, _) in candidates.into_iter().take(verification_limit) {
        let Ok(manifest) = verify_backup(&backup_dir) else {
            continue;
        };
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
    let plaintext =
        fs::read(source).map_err(|error| format!("failed to read backup source file: {error}"))?;
    let encrypted = protect(&plaintext)?;
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
    Ok(target_home.join(relative_path))
}

fn remove_extra_session_files(manifest: &BackupManifest, target_home: &Path) -> Result<(), String> {
    let expected = manifest
        .files
        .iter()
        .filter(|file| file.relative_path.starts_with("sessions"))
        .map(|file| target_home.join(&file.relative_path))
        .collect::<HashSet<_>>();
    let sessions = target_home.join("sessions");
    if !sessions.exists() {
        return Ok(());
    }
    for path in walk_jsonl_files(&sessions)? {
        if !expected.contains(&path) {
            fs::remove_file(&path)
                .map_err(|error| format!("failed to remove post-backup session file: {error}"))?;
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
    use std::fs;

    use rusqlite::Connection;
    use tempfile::tempdir;

    use super::{
        add_capacity_file, available_backup_bytes, collect_backup_capacity_metadata, create_backup,
        create_local_backup, create_runtime_backup, create_session_backup, create_state_backup,
        estimate_two_root_peak, finish_capacity_preflight, list_recent_backups,
        migrate_legacy_plaintext_auth, percentage_ceil, preflight_backup_capacity, restore_backup,
        sqlite_logical_bytes, verify_backup, BackupScope, BackupSourceCapacityMetadata,
        BACKUP_FILE_OVERHEAD_BYTES, BACKUP_ROOT_COUNT, MANIFEST_BASE_OVERHEAD_BYTES,
        MANIFEST_ENTRY_OVERHEAD_BYTES, MIN_CAPACITY_RESERVE_BYTES,
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

    #[test]
    fn two_root_capacity_uses_the_minimum_reserve_for_small_backups() {
        let current = BackupSourceCapacityMetadata {
            plaintext_payload_bytes: 100,
            file_count: 2,
            sqlite_logical_bytes: 80,
        };
        let shared = BackupSourceCapacityMetadata {
            plaintext_payload_bytes: 200,
            file_count: 3,
            sqlite_logical_bytes: 120,
        };
        let file_count = current.file_count + shared.file_count;
        let peak_without_reserve = current.plaintext_payload_bytes
            + shared.plaintext_payload_bytes
            + file_count * BACKUP_FILE_OVERHEAD_BYTES
            + BACKUP_ROOT_COUNT * MANIFEST_BASE_OVERHEAD_BYTES
            + file_count * MANIFEST_ENTRY_OVERHEAD_BYTES
            + shared.sqlite_logical_bytes;

        let required = estimate_two_root_peak(current, shared).unwrap();

        assert_eq!(required, peak_without_reserve + MIN_CAPACITY_RESERVE_BYTES);
    }

    #[test]
    fn two_root_capacity_uses_a_rounded_up_fifteen_percent_reserve() {
        let current = BackupSourceCapacityMetadata {
            plaintext_payload_bytes: 20 * 1024 * 1024 * 1024,
            file_count: 0,
            sqlite_logical_bytes: 1,
        };
        let shared = BackupSourceCapacityMetadata::default();
        let peak_without_reserve = current.plaintext_payload_bytes
            + BACKUP_ROOT_COUNT * MANIFEST_BASE_OVERHEAD_BYTES
            + current.sqlite_logical_bytes;
        let percentage_reserve = percentage_ceil(peak_without_reserve, 15).unwrap();

        let required = estimate_two_root_peak(current, shared).unwrap();

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
        };

        assert!(estimate_two_root_peak(current, BackupSourceCapacityMetadata::default()).is_err());

        let mut capacity = BackupSourceCapacityMetadata {
            plaintext_payload_bytes: u64::MAX,
            ..BackupSourceCapacityMetadata::default()
        };
        assert!(add_capacity_file(&mut capacity, 1).is_err());
    }

    #[test]
    fn insufficient_capacity_error_contains_only_required_and_available_counts() {
        let error = finish_capacity_preflight(20, 10).unwrap_err();

        assert_eq!(
            error,
            "insufficient backup capacity: required_bytes=20, available_bytes=10"
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
        let state_db = home.path().join("state_5.sqlite");
        Connection::open(&state_db)
            .unwrap()
            .execute("CREATE TABLE marker (value TEXT)", [])
            .unwrap();
        let paths = crate::codex_paths::local_codex_paths(home.path());
        let sqlite_bytes = sqlite_logical_bytes(&state_db).unwrap();

        let capacity =
            collect_backup_capacity_metadata(home.path(), &paths, BackupScope::Full).unwrap();

        assert_eq!(capacity.file_count, 5);
        assert_eq!(capacity.sqlite_logical_bytes, sqlite_bytes);
        assert_eq!(
            capacity.plaintext_payload_bytes,
            4 + 15 + 6 + 8 + sqlite_bytes
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

    #[test]
    fn backup_source_and_destination_must_not_overlap() {
        let home = tempdir().unwrap();
        seed_home(home.path());
        let nested_backup = home.path().join("backups");

        let error = create_backup(home.path(), &nested_backup, "overlap").unwrap_err();

        assert!(error.contains("must not overlap"));
        assert!(!nested_backup.exists());
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
        let backup_root = tempdir().unwrap();

        let runtime =
            create_runtime_backup(home.path(), backup_root.path(), "runtime-scope").unwrap();
        let sessions =
            create_session_backup(home.path(), backup_root.path(), "session-scope").unwrap();
        let state = create_state_backup(home.path(), backup_root.path(), "state-scope").unwrap();

        assert_eq!(runtime.scope, BackupScope::Runtime);
        assert_eq!(sessions.scope, BackupScope::Sessions);
        assert_eq!(state.scope, BackupScope::StateOnly);
        for manifest in [&runtime, &sessions, &state] {
            assert_eq!(manifest.tracked_databases, vec!["state_5.sqlite"]);
            assert!(!manifest.files.iter().any(|file| {
                matches!(
                    file.relative_path.to_string_lossy().as_ref(),
                    "goals_1.sqlite" | "memories_1.sqlite" | "logs_2.sqlite"
                )
            }));
        }
        assert!(runtime
            .files
            .iter()
            .any(|file| file.relative_path == std::path::Path::new("auth.json")));
        assert!(runtime
            .files
            .iter()
            .any(|file| file.relative_path.starts_with("sessions")));
        assert!(!sessions.files.iter().any(|file| {
            matches!(
                file.relative_path.to_string_lossy().as_ref(),
                "auth.json" | "config.toml"
            )
        }));
        assert!(sessions
            .files
            .iter()
            .any(|file| file.relative_path == std::path::Path::new("state_5.sqlite")));
        assert_eq!(state.files.len(), 1);
        assert_eq!(
            state.files[0].relative_path,
            std::path::Path::new("state_5.sqlite")
        );
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
    fn recent_backup_listing_returns_only_the_five_newest_candidates() {
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
        let oldest_payload = &manifests[0].files[0].backup_path;
        let mut tampered = fs::read(oldest_payload).unwrap();
        tampered[0] ^= 0xff;
        fs::write(oldest_payload, tampered).unwrap();

        let summaries = list_recent_backups(backup_root.path(), 5).unwrap();

        assert_eq!(summaries.len(), 5);
        assert!(summaries.iter().all(|summary| summary.verified));
        assert!(summaries
            .iter()
            .all(|summary| summary.backup_dir != manifests[0].backup_dir));
        assert_eq!(summaries[0].backup_dir, manifests[5].backup_dir);
    }

    #[test]
    fn recent_backup_listing_hides_partial_compensation_snapshots_but_keeps_v2() {
        let home = tempdir().unwrap();
        seed_home(home.path());
        let backup_root = tempdir().unwrap();
        let full = create_backup(home.path(), backup_root.path(), "manual-full").unwrap();
        let partial =
            create_runtime_backup(home.path(), backup_root.path(), "runtime-partial").unwrap();
        let mut legacy = create_backup(home.path(), backup_root.path(), "legacy-full").unwrap();
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
}

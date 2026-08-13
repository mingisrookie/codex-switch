use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::Duration,
};

#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;

use crate::{
    codex_paths::{local_codex_paths, resolve_user_codex_paths},
    diagnostics::{global_runtime, DiagnosticEventInput, DiagnosticEventKind, DiagnosticLevel},
    file_ops::{atomic_write, walk_jsonl_files},
    operation_log::{operation_id, timestamp_millis},
};

use super::{
    bounded_file::read_regular_file_bounded,
    catalog::{discover_database_catalog, snapshot_database_catalog},
    hash_cache::HashCache,
    model::{
        FileOrigin, SessionFileInput, ShadowScanIssue, ShadowScanIssueCode, ShadowScanReport,
        StorageScanStatus, SESSION_STORAGE_SCHEMA_VERSION,
    },
    provenance::{RouteProvenanceLedger, TurnSourceStatus},
    reference_graph::{
        build_reference_graph, managed_relative_components, path_key, ReferenceGraphInput,
    },
    storage_state::load_committed_canonical_storage_state,
};

static SHADOW_SCAN_RUNNING: AtomicBool = AtomicBool::new(false);
static SHADOW_SCAN_PENDING: AtomicBool = AtomicBool::new(false);
const SHADOW_STAGING_PREFIX: &str = "session-storage-shadow-";
const STALE_STAGING_MIN_AGE_MS: u128 = 60 * 60 * 1_000;
const SHADOW_SCAN_LOCK_FILE: &str = "shadow-scan.lock";
const SHADOW_SCAN_LOCK_RETRY: Duration = Duration::from_millis(250);
const MAX_SHADOW_REPORT_BYTES: u64 = 256 * 1024;

pub fn background_shadow_scan_is_running() -> bool {
    SHADOW_SCAN_RUNNING.load(Ordering::Acquire)
}

struct ShadowScanLease {
    _file: File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShadowScanLeaseError {
    Busy,
    Unavailable,
}

pub fn run_shadow_scan(codex_home: &Path, data_root: &Path) -> Result<ShadowScanReport, String> {
    if SHADOW_SCAN_RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err("a session storage shadow scan is already running".to_string());
    }
    let result = match try_acquire_shadow_scan_lease(data_root) {
        Ok(_lease) => run_shadow_scan_inner(codex_home, data_root),
        Err(ShadowScanLeaseError::Busy) => {
            Err("a session storage shadow scan is already running".to_string())
        }
        Err(ShadowScanLeaseError::Unavailable) => {
            Err("the session storage shadow scan lock is unavailable".to_string())
        }
    };
    SHADOW_SCAN_RUNNING.store(false, Ordering::Release);
    if SHADOW_SCAN_PENDING.load(Ordering::Acquire) {
        let _ = request_background_shadow_scan(codex_home.to_path_buf(), data_root.to_path_buf());
    }
    result
}

pub fn request_background_shadow_scan(codex_home: PathBuf, data_root: PathBuf) -> bool {
    SHADOW_SCAN_PENDING.store(true, Ordering::Release);
    if SHADOW_SCAN_RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return true;
    }
    match thread::Builder::new()
        .name("session-storage-shadow".to_string())
        .spawn(move || background_scan_loop(codex_home, data_root))
    {
        Ok(_) => true,
        Err(_) => {
            SHADOW_SCAN_RUNNING.store(false, Ordering::Release);
            record_background_shadow_failure(
                "spawnSessionStorageShadowScan",
                "shadow_scan.background_spawn_failed",
                "session storage shadow scan worker could not be started",
            );
            false
        }
    }
}

fn background_scan_loop(codex_home: PathBuf, data_root: PathBuf) {
    drain_pending_scans(&SHADOW_SCAN_PENDING, || {
        run_background_shadow_scan(&codex_home, &data_root);
    });
    SHADOW_SCAN_RUNNING.store(false, Ordering::Release);
    if SHADOW_SCAN_PENDING.load(Ordering::Acquire) {
        let _ = request_background_shadow_scan(codex_home, data_root);
    }
}

fn run_background_shadow_scan(codex_home: &Path, data_root: &Path) {
    loop {
        match try_acquire_shadow_scan_lease(data_root) {
            Ok(_lease) => {
                if let Err(error) = run_shadow_scan_inner(codex_home, data_root) {
                    record_background_shadow_failure(
                        "scanSessionStorageInBackground",
                        "shadow_scan.background_scan_failed",
                        &error,
                    );
                }
                return;
            }
            Err(ShadowScanLeaseError::Busy) => thread::sleep(SHADOW_SCAN_LOCK_RETRY),
            Err(ShadowScanLeaseError::Unavailable) => {
                record_background_shadow_failure(
                    "acquireSessionStorageShadowScanLease",
                    "shadow_scan.background_lease_unavailable",
                    "session storage shadow scan lock is unavailable",
                );
                return;
            }
        }
    }
}

fn record_background_shadow_failure(action: &'static str, error_code: &'static str, error: &str) {
    let Some(runtime) = global_runtime() else {
        return;
    };
    let _ = runtime.recorder().record(
        DiagnosticEventInput::new(
            DiagnosticLevel::Error,
            "sessionStorageShadowScan",
            DiagnosticEventKind::BackgroundFailure,
        )
        .with_action(action)
        .with_phase("query")
        .with_error(error_code, error),
    );
}

fn try_acquire_shadow_scan_lease(
    data_root: &Path,
) -> Result<ShadowScanLease, ShadowScanLeaseError> {
    let lock_root = data_root.join("session-storage-v1");
    fs::create_dir_all(&lock_root).map_err(|_| ShadowScanLeaseError::Unavailable)?;
    let root_metadata =
        fs::symlink_metadata(&lock_root).map_err(|_| ShadowScanLeaseError::Unavailable)?;
    if !root_metadata.is_dir() || metadata_is_link_or_reparse(&root_metadata) {
        return Err(ShadowScanLeaseError::Unavailable);
    }
    let lock_path = lock_root.join(SHADOW_SCAN_LOCK_FILE);
    if let Ok(metadata) = fs::symlink_metadata(&lock_path) {
        if !metadata.is_file() || metadata_is_link_or_reparse(&metadata) {
            return Err(ShadowScanLeaseError::Unavailable);
        }
    }
    let file = open_shadow_scan_lock_file(&lock_path).map_err(|error| {
        if shadow_scan_lock_is_busy(&error) {
            ShadowScanLeaseError::Busy
        } else {
            ShadowScanLeaseError::Unavailable
        }
    })?;
    let metadata = file
        .metadata()
        .map_err(|_| ShadowScanLeaseError::Unavailable)?;
    if !metadata.is_file() || metadata_is_link_or_reparse(&metadata) {
        return Err(ShadowScanLeaseError::Unavailable);
    }
    Ok(ShadowScanLease { _file: file })
}

#[cfg(windows)]
fn open_shadow_scan_lock_file(path: &Path) -> std::io::Result<File> {
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(true)
        .share_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    options.open(path)
}

#[cfg(not(windows))]
fn open_shadow_scan_lock_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(path)
}

#[cfg(windows)]
fn shadow_scan_lock_is_busy(error: &std::io::Error) -> bool {
    use windows_sys::Win32::Foundation::{ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION};

    matches!(
        error.raw_os_error(),
        Some(code)
            if code == ERROR_SHARING_VIOLATION as i32 || code == ERROR_LOCK_VIOLATION as i32
    )
}

#[cfg(not(windows))]
fn shadow_scan_lock_is_busy(_error: &std::io::Error) -> bool {
    false
}

fn drain_pending_scans(pending: &AtomicBool, mut scan: impl FnMut()) {
    loop {
        pending.store(false, Ordering::Release);
        scan();
        if !pending.swap(false, Ordering::AcqRel) {
            break;
        }
    }
}

fn run_shadow_scan_inner(codex_home: &Path, data_root: &Path) -> Result<ShadowScanReport, String> {
    let scan_id = operation_id("session-storage-shadow")?;
    let shadow_root = data_root.join("session-storage-v1/shadow");
    let staging_root = shadow_root.join(safe_component(&scan_id));
    if let Ok(now_ms) = timestamp_millis() {
        cleanup_stale_shadow_staging(&shadow_root, &staging_root, now_ms);
    }
    let mut issue_counts = BTreeMap::<ShadowScanIssueCode, usize>::new();
    let (canonical_ready, storage_state_errors) =
        match load_committed_canonical_storage_state(data_root, codex_home) {
            Ok(state) => (state.is_some(), 0),
            Err(_) => (false, 1),
        };
    record_issue(
        &mut issue_counts,
        ShadowScanIssueCode::StorageStateInvalid,
        storage_state_errors,
    );
    let discovery = discover_database_catalog(codex_home, data_root);
    record_issue(
        &mut issue_counts,
        ShadowScanIssueCode::DatabaseDiscoveryFailed,
        discovery.errors,
    );
    let catalog = snapshot_database_catalog(&discovery.descriptors, &staging_root);
    record_issue(
        &mut issue_counts,
        ShadowScanIssueCode::DatabaseSnapshotFailed,
        catalog.database_errors,
    );
    record_issue(
        &mut issue_counts,
        ShadowScanIssueCode::DatabaseRowMissingRolloutPath,
        catalog.rows_missing_rollout_path,
    );

    let (files, discovery_errors) = discover_session_files(
        codex_home,
        data_root,
        catalog.referenced_paths.iter().map(PathBuf::as_path),
    );
    record_issue(
        &mut issue_counts,
        ShadowScanIssueCode::SessionDiscoveryFailed,
        discovery_errors,
    );
    let mut inputs = Vec::with_capacity(files.len());
    let mut parse_errors = 0_usize;
    let mut invalid_markers = 0_usize;
    let mut cache = match HashCache::load(data_root) {
        Ok(cache) => cache,
        Err(_) => {
            record_issue(&mut issue_counts, ShadowScanIssueCode::HashCacheInvalid, 1);
            HashCache::empty_for(data_root)
        }
    };
    for (path, origin) in files {
        let scanned = cache.scan_file(&path);
        let semantic = scanned.semantic;
        if semantic.is_err() {
            parse_errors = parse_errors.saturating_add(1);
        }
        let marker_status = scanned.marker_status;
        if marker_status == super::model::MarkerStatus::Invalid {
            invalid_markers = invalid_markers.saturating_add(1);
        }
        inputs.push(SessionFileInput {
            path,
            origin,
            marker_status,
            observation: scanned.observation,
            semantic,
        });
    }
    let cache_stats = cache.stats();
    let graph_input = ReferenceGraphInput {
        files: inputs,
        databases: catalog.databases,
    };
    let graph = build_reference_graph(&graph_input);
    let mut summary = graph.summary;
    let mut provenance_errors = 0_usize;
    match RouteProvenanceLedger::load(data_root) {
        Ok(ledger) => {
            for (node, input) in graph.files.iter().zip(&graph_input.files) {
                if !node.retained_candidate {
                    continue;
                }
                let Ok(session) = &input.semantic else {
                    continue;
                };
                summary.turn_context_count = summary
                    .turn_context_count
                    .saturating_add(session.turn_contexts.len());
                match ledger.resolve(session) {
                    Ok(turns) => {
                        for turn in turns {
                            match turn.status {
                                TurnSourceStatus::Resolved => {
                                    summary.resolved_turn_provenance_count =
                                        summary.resolved_turn_provenance_count.saturating_add(1)
                                }
                                TurnSourceStatus::HistoricalUnknown => {
                                    summary.historical_unknown_turn_count =
                                        summary.historical_unknown_turn_count.saturating_add(1)
                                }
                                TurnSourceStatus::Incomplete => {
                                    summary.incomplete_turn_provenance_count =
                                        summary.incomplete_turn_provenance_count.saturating_add(1);
                                    provenance_errors = provenance_errors.saturating_add(1);
                                }
                            }
                        }
                    }
                    Err(_) => {
                        summary.incomplete_turn_provenance_count = summary
                            .incomplete_turn_provenance_count
                            .saturating_add(session.turn_contexts.len());
                        provenance_errors = provenance_errors.saturating_add(1);
                    }
                }
            }
        }
        Err(_) => {
            provenance_errors = provenance_errors.saturating_add(1);
            for (node, input) in graph.files.iter().zip(&graph_input.files) {
                if !node.retained_candidate {
                    continue;
                }
                if let Ok(session) = &input.semantic {
                    summary.turn_context_count = summary
                        .turn_context_count
                        .saturating_add(session.turn_contexts.len());
                    summary.incomplete_turn_provenance_count = summary
                        .incomplete_turn_provenance_count
                        .saturating_add(session.turn_contexts.len());
                }
            }
        }
    }
    summary.cache_hit_count = cache_stats.hits;
    summary.cache_miss_count = cache_stats.misses;
    summary.stable_file_count = cache_stats.stable_files;
    if cache.save().is_err() {
        record_issue(
            &mut issue_counts,
            ShadowScanIssueCode::HashCachePersistenceFailed,
            1,
        );
    }
    record_issue(
        &mut issue_counts,
        ShadowScanIssueCode::SessionParseFailed,
        parse_errors,
    );
    record_issue(
        &mut issue_counts,
        ShadowScanIssueCode::InvalidProviderMarker,
        invalid_markers,
    );
    record_issue(
        &mut issue_counts,
        ShadowScanIssueCode::MissingRuntimeReference,
        summary.missing_runtime_reference_count,
    );
    record_issue(
        &mut issue_counts,
        ShadowScanIssueCode::MismatchedRuntimeReference,
        summary.mismatched_runtime_reference_count,
    );
    record_issue(
        &mut issue_counts,
        ShadowScanIssueCode::DivergentSession,
        summary.relation_counts.divergent,
    );
    record_issue(
        &mut issue_counts,
        ShadowScanIssueCode::TurnProvenanceInvalid,
        provenance_errors,
    );
    record_issue(
        &mut issue_counts,
        ShadowScanIssueCode::OnlineSnapshotNotAtomic,
        usize::from(summary.runtime_database_count > 1),
    );
    cleanup_staging(&staging_root);

    let requires_review = discovery.errors > 0
        || catalog.database_errors > 0
        || catalog.rows_missing_rollout_path > 0
        || discovery_errors > 0
        || parse_errors > 0
        || invalid_markers > 0
        || provenance_errors > 0
        || storage_state_errors > 0
        || summary.conflict_session_count > 0
        || summary.missing_runtime_reference_count > 0
        || summary.mismatched_runtime_reference_count > 0;
    let status = if requires_review {
        StorageScanStatus::ReviewRequired
    } else if summary.session_file_count == 0 {
        StorageScanStatus::NoSessions
    } else if canonical_ready {
        StorageScanStatus::CanonicalReady
    } else {
        StorageScanStatus::MigrationAvailable
    };
    let generated_at_ms = u64::try_from(timestamp_millis()?)
        .map_err(|_| "session storage scan timestamp overflowed".to_string())?;
    let mut report = ShadowScanReport {
        schema_version: SESSION_STORAGE_SCHEMA_VERSION,
        scan_id,
        generated_at_ms,
        status,
        migration_required: summary.session_file_count > 0 && !canonical_ready,
        deletion_enabled: false,
        summary,
        issues: issue_counts
            .into_iter()
            .filter(|(_, count)| *count > 0)
            .map(|(code, count)| ShadowScanIssue { code, count })
            .collect(),
    };
    if persist_last_report(data_root, &report).is_err() {
        report.status = StorageScanStatus::ReviewRequired;
        report.issues.push(ShadowScanIssue {
            code: ShadowScanIssueCode::ReportPersistenceFailed,
            count: 1,
        });
    }
    Ok(report)
}

pub fn load_last_shadow_report(data_root: &Path) -> Result<Option<ShadowScanReport>, String> {
    let path = last_report_path(data_root);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("session storage report metadata is unavailable".to_string()),
    };
    if !metadata.is_file()
        || metadata_is_link_or_reparse(&metadata)
        || metadata.len() == 0
        || metadata.len() > MAX_SHADOW_REPORT_BYTES
    {
        return Err("session storage report is invalid".to_string());
    }
    let encoded = read_regular_file_bounded(&path, MAX_SHADOW_REPORT_BYTES)
        .map_err(|_| "session storage report is unreadable".to_string())?;
    let report = serde_json::from_slice::<ShadowScanReport>(&encoded)
        .map_err(|_| "session storage report is invalid".to_string())?;
    if report.schema_version != SESSION_STORAGE_SCHEMA_VERSION
        || report.summary.schema_version != SESSION_STORAGE_SCHEMA_VERSION
        || report.deletion_enabled
        || !report.summary.online_scan_only
        || !report.summary.non_atomic_across_databases
    {
        return Err("session storage report has an unsupported safety contract".to_string());
    }
    Ok(Some(report))
}

fn persist_last_report(data_root: &Path, report: &ShadowScanReport) -> Result<(), String> {
    let encoded = serde_json::to_vec(report)
        .map_err(|_| "failed to serialize the session storage report".to_string())?;
    atomic_write(&last_report_path(data_root), &encoded)
        .map_err(|_| "failed to persist the session storage report".to_string())
}

fn last_report_path(data_root: &Path) -> PathBuf {
    data_root.join("session-storage-v1/latest-shadow-report.json")
}

pub(crate) fn discover_session_files<'a>(
    codex_home: &Path,
    data_root: &Path,
    referenced_paths: impl Iterator<Item = &'a Path>,
) -> (Vec<(PathBuf, FileOrigin)>, usize) {
    let mut files = BTreeMap::<String, (PathBuf, FileOrigin)>::new();
    let mut errors = 0_usize;
    let current = match resolve_user_codex_paths(codex_home) {
        Ok(paths) => Some(paths),
        Err(_) => {
            errors = errors.saturating_add(1);
            None
        }
    };
    let shared = local_codex_paths(&data_root.join("shared-sessions"));
    if let Some(current) = current {
        for root in [current.sessions_dir, current.archived_sessions_dir] {
            collect_root(&root, FileOrigin::CanonicalHome, &mut files, &mut errors);
        }
    }
    for root in [shared.sessions_dir, shared.archived_sessions_dir] {
        collect_root(&root, FileOrigin::Shared, &mut files, &mut errors);
    }
    for path in referenced_paths {
        if path.is_file()
            && path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
        {
            files
                .entry(path_key(path))
                .or_insert_with(|| (path.to_path_buf(), classify_external_file(path, data_root)));
        }
    }
    (files.into_values().collect(), errors)
}

fn collect_root(
    root: &Path,
    origin: FileOrigin,
    files: &mut BTreeMap<String, (PathBuf, FileOrigin)>,
    errors: &mut usize,
) {
    if !root.is_dir() {
        return;
    }
    match walk_jsonl_files(root) {
        Ok(paths) => {
            for path in paths {
                files.entry(path_key(&path)).or_insert((path, origin));
            }
        }
        Err(_) => *errors = errors.saturating_add(1),
    }
}

fn classify_external_file(path: &Path, data_root: &Path) -> FileOrigin {
    let Some(components) = managed_relative_components(path, data_root) else {
        return FileOrigin::ReferencedExternal;
    };
    if components
        .iter()
        .any(|component| component == "conflict" || component.starts_with("conflict-"))
        || components
            .iter()
            .any(|component| component == "recycle" || component.starts_with("recycle-"))
    {
        FileOrigin::ConflictRecycle
    } else if components.iter().any(|component| {
        component == "recovery"
            || component.starts_with("recovery-")
            || component == "restore"
            || component.starts_with("restore-")
    }) {
        FileOrigin::RecoveryPackage
    } else if components.iter().any(|component| {
        component == "downgrade"
            || component.starts_with("downgrade-")
            || component == "v0.2"
            || component.starts_with("v0.2-")
    }) {
        FileOrigin::DowngradeExport
    } else if components.iter().any(|component| {
        component == "backup"
            || component.starts_with("backup-")
            || component == "backups"
            || component.starts_with("backups-")
    }) {
        FileOrigin::BackupInventory
    } else if components.iter().any(|component| {
        component == "temp"
            || component.starts_with("temp-")
            || component == "adapter"
            || component.starts_with("adapter-")
    }) {
        FileOrigin::TemporaryAdapter
    } else {
        FileOrigin::ReferencedExternal
    }
}

fn record_issue(
    issues: &mut BTreeMap<ShadowScanIssueCode, usize>,
    code: ShadowScanIssueCode,
    count: usize,
) {
    if count > 0 {
        let current = issues.entry(code).or_default();
        *current = current.saturating_add(count);
    }
}

fn safe_component(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || *character == '-')
        .collect()
}

fn cleanup_staging(path: &Path) {
    if remove_managed_staging_dir(path).is_ok() {
        if let Some(parent) = path.parent() {
            let _ = fs::remove_dir(parent);
        }
    }
}

fn cleanup_stale_shadow_staging(shadow_root: &Path, current: &Path, now_ms: u128) {
    let Ok(root_metadata) = fs::symlink_metadata(shadow_root) else {
        return;
    };
    if !root_metadata.is_dir() || metadata_is_link_or_reparse(&root_metadata) {
        return;
    }
    let Ok(entries) = fs::read_dir(shadow_root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == current {
            continue;
        }
        let Some((created_at_ms, process_id)) = parse_shadow_staging_owner(&entry.file_name())
        else {
            continue;
        };
        if now_ms.saturating_sub(created_at_ms) < STALE_STAGING_MIN_AGE_MS
            || process_is_running(process_id)
        {
            continue;
        }
        let _ = remove_managed_staging_dir(&path);
    }
}

fn parse_shadow_staging_owner(name: &std::ffi::OsStr) -> Option<(u128, u32)> {
    let suffix = name.to_str()?.strip_prefix(SHADOW_STAGING_PREFIX)?;
    let mut parts = suffix.rsplitn(3, '-');
    let _counter = parts.next()?.parse::<u64>().ok()?;
    let process_id = parts.next()?.parse::<u32>().ok()?;
    let created_at_ms = parts.next()?.parse::<u128>().ok()?;
    Some((created_at_ms, process_id))
}

fn remove_managed_staging_dir(path: &Path) -> Result<(), ()> {
    let metadata = fs::symlink_metadata(path).map_err(|_| ())?;
    if !metadata.is_dir() || metadata_is_link_or_reparse(&metadata) {
        return Err(());
    }
    let mut files = Vec::new();
    for entry in fs::read_dir(path).map_err(|_| ())? {
        let entry = entry.map_err(|_| ())?;
        if !is_shadow_snapshot_file(&entry.file_name()) {
            return Err(());
        }
        let entry_path = entry.path();
        let metadata = fs::symlink_metadata(&entry_path).map_err(|_| ())?;
        if !metadata.is_file() || metadata_is_link_or_reparse(&metadata) {
            return Err(());
        }
        files.push(entry_path);
    }
    for file in files {
        fs::remove_file(file).map_err(|_| ())?;
    }
    fs::remove_dir(path).map_err(|_| ())
}

fn is_shadow_snapshot_file(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some(suffix) = name.strip_prefix("snapshot-") else {
        return false;
    };
    let Some((index, sqlite_suffix)) = suffix.split_once(".sqlite") else {
        return false;
    };
    index.len() >= 4
        && index.bytes().all(|byte| byte.is_ascii_digit())
        && matches!(sqlite_suffix, "" | "-wal" | "-shm" | "-journal")
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

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        fs,
        sync::{atomic::AtomicBool, Mutex},
        time::{Duration, Instant},
    };

    use rusqlite::Connection;
    use tempfile::tempdir;

    use super::{
        classify_external_file, cleanup_stale_shadow_staging, drain_pending_scans,
        load_last_shadow_report, request_background_shadow_scan, run_shadow_scan,
        try_acquire_shadow_scan_lease, ShadowScanLeaseError, SHADOW_SCAN_RUNNING,
        STALE_STAGING_MIN_AGE_MS,
    };
    use crate::session_storage::{
        migration::{persist_migration_preflight, run_migration_preflight},
        model::{FileOrigin, ShadowScanIssueCode, StorageScanStatus},
        operation_ledger::{
            OperationLedgerStore, SessionStorageOperationKind, SessionStorageOperationPhase,
        },
        provenance::{record_or_verify_route_epoch, RouteEpochInput},
        storage_state::{
            finalize_canonical_storage_state, load_committed_canonical_storage_state,
            load_session_storage_control_state, prepare_canonical_storage_state,
        },
    };

    static TEST_SHADOW_SCAN_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn shadow_scan_reports_duplicates_and_never_enables_deletion() {
        let _scan_guard = TEST_SHADOW_SCAN_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let data = root.path().join("data");
        let canonical = home.join("sessions/2026/08/11/canonical.jsonl");
        let shared = data.join("shared-sessions/sessions/2026/08/11/shared.jsonl");
        fs::create_dir_all(canonical.parent().unwrap()).unwrap();
        fs::create_dir_all(shared.parent().unwrap()).unwrap();
        fs::write(
            &canonical,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"thread-a\",\"model_provider\":\"openai\"}}\n",
                "{\"type\":\"turn_context\",\"timestamp\":\"1970-01-01T00:00:01Z\",\"payload\":{\"turn_id\":\"turn-a\",\"model\":\"gpt-test\"}}\n"
            ),
        )
        .unwrap();
        fs::write(
            &shared,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"thread-a\",\"model_provider\":\"openai_custom\"}}\n",
                "{\"type\":\"turn_context\",\"timestamp\":\"1970-01-01T00:00:01Z\",\"payload\":{\"turn_id\":\"turn-a\",\"model\":\"gpt-test\"}}\n"
            ),
        )
        .unwrap();
        record_or_verify_route_epoch(
            &data,
            RouteEpochInput::new(
                "switch-runtime-1",
                500,
                "plus",
                "openai",
                "plus:1",
                Some("gpt-test"),
            ),
            true,
        )
        .unwrap();
        fs::write(
            home.join("config.toml"),
            format!("sqlite_home = {:?}\n", home.to_string_lossy()),
        )
        .unwrap();
        let database = Connection::open(home.join("state_5.sqlite")).unwrap();
        database
            .execute_batch(&format!(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, model_provider TEXT);
                 INSERT INTO threads VALUES ('thread-a', '{}', 'openai');",
                canonical.to_string_lossy().replace('\'', "''")
            ))
            .unwrap();
        create_goals_database(&home.join("goals_1.sqlite"));
        let shared_database =
            Connection::open(data.join("shared-sessions/state_5.sqlite")).unwrap();
        shared_database
            .execute_batch(&format!(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, model_provider TEXT);
                 INSERT INTO threads VALUES ('thread-a', '{}', 'openai_custom');",
                shared.to_string_lossy().replace('\'', "''")
            ))
            .unwrap();
        create_goals_database(&data.join("shared-sessions/goals_1.sqlite"));

        let report = run_shadow_scan(&home, &data).unwrap();

        assert_eq!(
            report.status,
            StorageScanStatus::MigrationAvailable,
            "{report:#?}"
        );
        assert!(!report.deletion_enabled);
        assert_eq!(report.summary.logical_session_count, 1);
        assert_eq!(report.summary.high_confidence_copy_count, 1);
        assert_eq!(report.summary.relation_counts.equal_except_provider, 1);
        assert!(report.issues.iter().all(|issue| {
            issue.code != ShadowScanIssueCode::MissingRuntimeReference
                && issue.code != ShadowScanIssueCode::DivergentSession
        }));
        assert_eq!(report.summary.cache_miss_count, 2);
        assert_eq!(report.summary.cache_hit_count, 0);
        assert_eq!(report.summary.turn_context_count, 1);
        assert_eq!(report.summary.resolved_turn_provenance_count, 1);
        assert_eq!(report.summary.historical_unknown_turn_count, 0);
        assert_eq!(report.summary.incomplete_turn_provenance_count, 0);

        let cached = run_shadow_scan(&home, &data).unwrap();
        assert_eq!(cached.summary.cache_hit_count, 2);
        assert_eq!(cached.summary.cache_miss_count, 0);
        assert_eq!(cached.summary.stable_file_count, 2);
        assert_eq!(load_last_shadow_report(&data).unwrap(), Some(cached));

        let store = OperationLedgerStore::new(&data);
        store
            .create(
                "migration-shadow-ready",
                SessionStorageOperationKind::Migration,
                &home,
            )
            .unwrap();
        let backup_destination = root.path().join("migration-backup");
        fs::create_dir_all(&backup_destination).unwrap();
        let preflight =
            run_migration_preflight(&home, &data, "migration-shadow-ready", &backup_destination)
                .unwrap();
        assert!(preflight.ready_for_backup, "{:?}", preflight.blockers);
        persist_migration_preflight(&data, &preflight).unwrap();
        store
            .update("migration-shadow-ready", |ledger| {
                ledger.backup_root =
                    Some(preflight.backup_destination.join("migration-shadow-ready"));
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
            store.transition("migration-shadow-ready", phase).unwrap();
        }
        prepare_canonical_storage_state(
            &data,
            &home,
            "migration-shadow-ready",
            &preflight.plan.inventory_fingerprint,
        )
        .unwrap();
        store
            .transition(
                "migration-shadow-ready",
                SessionStorageOperationPhase::Committed,
            )
            .unwrap();
        finalize_canonical_storage_state(&data, &home, "migration-shadow-ready").unwrap();
        let canonical_ready = run_shadow_scan(&home, &data).unwrap();
        assert_eq!(canonical_ready.status, StorageScanStatus::CanonicalReady);
        assert!(!canonical_ready.migration_required);

        fs::remove_dir_all(data.join("session-storage-v1/operations/migration-shadow-ready"))
            .unwrap();
        let retained_state_ready = run_shadow_scan(&home, &data).unwrap();
        assert_eq!(
            retained_state_ready.status,
            StorageScanStatus::CanonicalReady
        );
        assert!(!retained_state_ready.migration_required);
        let retained_state = load_committed_canonical_storage_state(&data, &home)
            .unwrap()
            .unwrap();
        assert_eq!(retained_state.backup_destination, backup_destination);
        let control = load_session_storage_control_state(&data, &home).unwrap();
        assert!(control.canonical_ready);
        assert_eq!(
            control.migration_operation_id.as_deref(),
            Some("migration-shadow-ready")
        );

        let report_path = data.join("session-storage-v1/latest-shadow-report.json");
        let mut unsafe_report: serde_json::Value =
            serde_json::from_slice(&fs::read(&report_path).unwrap()).unwrap();
        unsafe_report["deletionEnabled"] = serde_json::Value::Bool(true);
        fs::write(&report_path, serde_json::to_vec(&unsafe_report).unwrap()).unwrap();
        assert!(load_last_shadow_report(&data).is_err());
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

    #[test]
    fn stale_staging_cleanup_only_removes_managed_inactive_direct_children() {
        let root = tempdir().unwrap();
        let shadow = root.path().join("session-storage-v1/shadow");
        let stale = shadow.join("session-storage-shadow-1-0-1");
        let current = shadow.join("session-storage-shadow-1-0-2");
        let recent_timestamp = STALE_STAGING_MIN_AGE_MS + 10;
        let recent = shadow.join(format!("session-storage-shadow-{recent_timestamp}-0-3"));
        let live_owner = shadow.join(format!("session-storage-shadow-1-{}-5", std::process::id()));
        let unmanaged = shadow.join("manual-folder");
        let nested_managed = unmanaged.join("session-storage-shadow-1-0-4");
        let unexpected_tree = shadow.join("session-storage-shadow-1-0-6");
        for directory in [&stale, &current, &recent, &live_owner, &nested_managed] {
            fs::create_dir_all(directory).unwrap();
            fs::write(directory.join("snapshot-0000.sqlite"), b"shadow").unwrap();
        }
        fs::create_dir_all(unexpected_tree.join("nested")).unwrap();
        fs::write(
            unexpected_tree.join("nested/snapshot-0000.sqlite"),
            b"shadow",
        )
        .unwrap();

        cleanup_stale_shadow_staging(&shadow, &current, STALE_STAGING_MIN_AGE_MS + 20);

        assert!(!stale.exists());
        assert!(current.exists());
        assert!(recent.exists());
        assert!(live_owner.exists());
        assert!(unmanaged.exists());
        assert!(nested_managed.exists());
        assert!(unexpected_tree.exists());
    }

    #[cfg(windows)]
    #[test]
    fn the_windows_scan_lease_is_exclusive_until_its_handle_is_released() {
        let root = tempdir().unwrap();
        let first = try_acquire_shadow_scan_lease(root.path()).unwrap();

        assert!(matches!(
            try_acquire_shadow_scan_lease(root.path()),
            Err(ShadowScanLeaseError::Busy)
        ));

        drop(first);
        assert!(try_acquire_shadow_scan_lease(root.path()).is_ok());
    }

    #[test]
    fn external_origin_classification_never_uses_unrelated_parent_names() {
        let root = tempdir().unwrap();
        let data = root.path().join("parent-conflict/codex-switch");

        assert_eq!(
            classify_external_file(&data.join("backups/item/session.jsonl"), &data),
            FileOrigin::BackupInventory
        );
        assert_eq!(
            classify_external_file(&root.path().join("conflict-outside/session.jsonl"), &data,),
            FileOrigin::ReferencedExternal
        );
    }

    #[test]
    fn discovery_failures_require_review_even_when_no_sessions_were_found() {
        let _scan_guard = TEST_SHADOW_SCAN_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let data = root.path().join("data");
        fs::create_dir_all(data.join("runtimes/plus")).unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::write(
            home.join("config.toml"),
            format!("sqlite_home = {:?}\n", home.to_string_lossy()),
        )
        .unwrap();
        Connection::open(home.join("state_5.sqlite"))
            .unwrap()
            .execute_batch(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, model_provider TEXT);",
            )
            .unwrap();
        fs::write(data.join("runtimes/plus/config.toml"), "not = [valid").unwrap();

        let report = run_shadow_scan(&home, &data).unwrap();

        assert_eq!(report.status, StorageScanStatus::ReviewRequired);
        assert_eq!(report.summary.session_file_count, 0);
        assert!(report.issues.iter().any(|issue| {
            issue.code == ShadowScanIssueCode::DatabaseDiscoveryFailed && issue.count == 1
        }));
        assert!(!report.deletion_enabled);
    }

    #[test]
    fn a_background_request_persists_a_scan_only_report() {
        let _scan_guard = TEST_SHADOW_SCAN_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let data = root.path().join("data");
        fs::create_dir_all(&home).unwrap();
        fs::write(
            home.join("config.toml"),
            format!("sqlite_home = {:?}\n", home.to_string_lossy()),
        )
        .unwrap();
        Connection::open(home.join("state_5.sqlite"))
            .unwrap()
            .execute_batch(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, model_provider TEXT);",
            )
            .unwrap();

        assert!(request_background_shadow_scan(home, data.clone()));
        let deadline = Instant::now() + Duration::from_secs(5);
        let report = loop {
            if let Some(report) = load_last_shadow_report(&data).unwrap() {
                break report;
            }
            assert!(
                Instant::now() < deadline,
                "background shadow scan timed out"
            );
            std::thread::sleep(Duration::from_millis(20));
        };

        assert_eq!(report.status, StorageScanStatus::NoSessions);
        assert!(report.summary.online_scan_only);
        assert!(!report.deletion_enabled);
        while SHADOW_SCAN_RUNNING.load(std::sync::atomic::Ordering::Acquire) {
            assert!(
                Instant::now() < deadline,
                "background scan did not release its lease"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn repeated_requests_during_a_scan_coalesce_into_one_final_calibration() {
        let pending = AtomicBool::new(true);
        let scans = Cell::new(0_usize);

        drain_pending_scans(&pending, || {
            scans.set(scans.get() + 1);
            if scans.get() == 1 {
                for _ in 0..10 {
                    pending.store(true, std::sync::atomic::Ordering::Release);
                }
            }
        });

        assert_eq!(scans.get(), 2);
        assert!(!pending.load(std::sync::atomic::Ordering::Acquire));
    }
}

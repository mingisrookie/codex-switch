use std::{
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;

use rusqlite::{types::Value, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};

use crate::{
    codex_paths::{local_codex_paths, resolve_user_codex_paths, CodexPaths},
    file_ops::{atomic_create, atomic_write, walk_jsonl_files},
};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionSyncResult {
    pub inserted_threads: usize,
    pub copied_session_files: usize,
    pub duplicate_threads: usize,
    pub skipped_missing_session_files: usize,
    pub skipped_archived_threads: usize,
    pub merged_session_index_entries: usize,
    pub persistent_session_bytes_added: u64,
    pub persistent_session_bytes_reclaimed: u64,
    #[serde(skip)]
    pub(crate) obsolete_provider_slots: Vec<ObsoleteProviderSlot>,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionStorageDemand {
    pub destination: PathBuf,
    pub bytes: u64,
}

#[cfg(test)]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RuntimeSessionStoragePlan {
    demands: HashMap<PathBuf, u64>,
    session_file_writes_required: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ProviderSlotGcSummary {
    pub reclaimed_count: usize,
    pub reclaimed_bytes: u64,
    pub retained_count: usize,
    pub warnings: Vec<String>,
}

#[cfg(test)]
impl RuntimeSessionStoragePlan {
    fn add(&mut self, destination: PathBuf, bytes: u64) -> Result<(), String> {
        self.session_file_writes_required = true;
        self.add_capacity(destination, bytes)
    }

    fn add_capacity(&mut self, destination: PathBuf, bytes: u64) -> Result<(), String> {
        if bytes == 0 {
            return Ok(());
        }
        let entry = self.demands.entry(destination).or_default();
        *entry = (*entry).max(bytes);
        Ok(())
    }

    pub(crate) fn demands(&self) -> Vec<SessionStorageDemand> {
        let mut demands = self
            .demands
            .iter()
            .map(|(destination, bytes)| SessionStorageDemand {
                destination: destination.clone(),
                bytes: *bytes,
            })
            .collect::<Vec<_>>();
        demands.sort_by(|left, right| left.destination.cmp(&right.destination));
        demands
    }
}

#[derive(Debug, Clone)]
struct SyncRoot {
    root: PathBuf,
    state_db: PathBuf,
    sessions_dir: PathBuf,
    session_index: PathBuf,
}

#[derive(Debug, Clone)]
struct SourceThread {
    id: String,
    values_by_column: HashMap<String, Value>,
    session_file: PathBuf,
    meta: SessionMeta,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct SessionMeta {
    id: String,
    model_provider: Option<String>,
    source: Option<String>,
    cwd: Option<String>,
    cli_version: Option<String>,
    timestamp_millis: Option<i64>,
}

#[derive(Debug, Clone)]
struct SourceRow {
    values_by_column: HashMap<String, Value>,
}

struct SourceScan {
    threads: Vec<SourceThread>,
    candidate_ids: HashSet<String>,
    skipped_archived_threads: usize,
    skipped_missing_session_files: usize,
}

struct PlannedSourceThread {
    thread: SourceThread,
}

struct PreparedSource {
    source_conn: Option<Connection>,
    threads: Vec<PlannedSourceThread>,
    candidate_ids: HashSet<String>,
    skipped_archived_threads: usize,
    skipped_missing_session_files: usize,
}

#[derive(Debug, Clone)]
struct TableColumn {
    name: String,
    not_null: bool,
    default_value: Option<String>,
}

#[derive(Debug)]
struct RolloutCopy {
    path: String,
    copied: bool,
    created_bytes: u64,
    source_meta: SessionMeta,
    obsolete_provider_slot: Option<ObsoleteProviderSlot>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ObsoleteProviderSlot {
    obsolete: PathBuf,
    successor: PathBuf,
    obsolete_version: StableSourceVersion,
    obsolete_marker: ProviderSlotMarker,
    successor_version: StableSourceVersion,
    successor_marker: ProviderSlotMarker,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OwnedProviderSlot {
    path: PathBuf,
    version: StableSourceVersion,
    marker: ProviderSlotMarker,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RolloutFileAction {
    Unchanged(PathBuf),
    Create(PathBuf),
    Import(PathBuf),
}

impl RolloutFileAction {
    fn target_path(&self) -> &Path {
        match self {
            Self::Unchanged(path) | Self::Create(path) | Self::Import(path) => path,
        }
    }

    fn writes_session_file(&self) -> bool {
        !matches!(self, Self::Unchanged(_))
    }
}

struct SessionIndexMergePlan {
    lines: Vec<String>,
    target_needs_newline: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionFileWritePolicy {
    Allow,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionIndexWritePolicy {
    MergeAtomic,
    Skip,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExistingRolloutPathPolicy {
    SelectMostComplete,
    // ChatGPT may keep appending the current rollout through an already-open handle.
    PreserveExisting,
}

#[derive(Debug, Clone, Copy)]
struct SessionSyncPolicy {
    allow_existing_replacement: bool,
    update_existing_provider: bool,
    existing_rollout_path: ExistingRolloutPathPolicy,
    session_files: SessionFileWritePolicy,
    session_index: SessionIndexWritePolicy,
    copy_dependent_rows: bool,
}

impl SessionSyncPolicy {
    fn standard(
        allow_existing_replacement: bool,
        update_existing_provider: bool,
        session_files: SessionFileWritePolicy,
    ) -> Self {
        Self {
            allow_existing_replacement,
            update_existing_provider,
            existing_rollout_path: ExistingRolloutPathPolicy::SelectMostComplete,
            session_files,
            session_index: SessionIndexWritePolicy::closed(session_files),
            copy_dependent_rows: true,
        }
    }

    fn hot_current(session_files: SessionFileWritePolicy) -> Self {
        Self {
            allow_existing_replacement: false,
            update_existing_provider: false,
            existing_rollout_path: ExistingRolloutPathPolicy::PreserveExisting,
            session_files,
            session_index: SessionIndexWritePolicy::hot(session_files),
            copy_dependent_rows: true,
        }
    }

    fn incremental(update_existing_provider: bool, preserve_existing: bool) -> Self {
        Self {
            allow_existing_replacement: !preserve_existing,
            update_existing_provider,
            existing_rollout_path: if preserve_existing {
                ExistingRolloutPathPolicy::PreserveExisting
            } else {
                ExistingRolloutPathPolicy::SelectMostComplete
            },
            session_files: SessionFileWritePolicy::Allow,
            session_index: SessionIndexWritePolicy::Skip,
            copy_dependent_rows: true,
        }
    }

    fn incremental_self_provider() -> Self {
        Self {
            allow_existing_replacement: true,
            update_existing_provider: true,
            existing_rollout_path: ExistingRolloutPathPolicy::SelectMostComplete,
            session_files: SessionFileWritePolicy::Allow,
            session_index: SessionIndexWritePolicy::Skip,
            copy_dependent_rows: false,
        }
    }
}

impl SessionIndexWritePolicy {
    fn closed(file_policy: SessionFileWritePolicy) -> Self {
        match file_policy {
            SessionFileWritePolicy::Allow => Self::MergeAtomic,
            SessionFileWritePolicy::Deny => Self::Deny,
        }
    }

    fn hot(file_policy: SessionFileWritePolicy) -> Self {
        match file_policy {
            SessionFileWritePolicy::Allow => Self::Skip,
            SessionFileWritePolicy::Deny => Self::Deny,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionFileRelation {
    Equal,
    LeftExtendsRight,
    RightExtendsLeft,
    Divergent,
}

pub fn sync_sessions(
    source_homes: &[PathBuf],
    target_home: &Path,
) -> Result<SessionSyncResult, String> {
    let sources = source_homes
        .iter()
        .map(|home| root_from_paths(local_codex_paths(home)))
        .collect::<Vec<_>>();
    sync_session_roots(
        &sources,
        root_from_paths(local_codex_paths(target_home)),
        None,
        SessionSyncPolicy::standard(true, false, SessionFileWritePolicy::Allow),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StableSourceVersion {
    length: u64,
    sha256: Vec<u8>,
    meta: SessionMeta,
}

#[derive(Debug)]
struct StableSourceData {
    version: StableSourceVersion,
    normalized_line_hashes: Vec<Vec<u8>>,
}

struct StableRolloutPlan {
    action: RolloutFileAction,
    source: StableSourceData,
    marker_origin: Option<PathBuf>,
    supersedes: Option<OwnedProviderSlot>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProviderRolloutAction {
    Unchanged(PathBuf),
    Create {
        path: PathBuf,
        supersedes: Option<Box<OwnedProviderSlot>>,
    },
}

impl ProviderRolloutAction {
    fn target_path(&self) -> &Path {
        match self {
            Self::Unchanged(path) | Self::Create { path, .. } => path,
        }
    }

    fn writes_session_file(&self) -> bool {
        !matches!(self, Self::Unchanged(_))
    }
}

struct StableProviderRolloutPlan {
    action: ProviderRolloutAction,
    source: StableSourceData,
}

const SOURCE_STABILITY_ATTEMPTS: usize = 3;
const SOURCE_CHANGED_PREFIX: &str = "source session JSONL changed";
const PROVIDER_SLOT_MARKER_VERSION: u32 = 1;
const PROVIDER_SLOT_MARKER_MAX_BYTES: u64 = 16 * 1024;
const PROVIDER_SLOT_ALLOCATION_ATTEMPTS: u32 = 32;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProviderSlotMarker {
    version: u32,
    thread_id: String,
    provider_id: Option<String>,
    slot_file_name: String,
    origin_relative_path: Option<PathBuf>,
    origin_provider: Option<String>,
    created_bytes: u64,
    created_sha256: String,
}

pub fn sync_sessions_for_provider(
    source_homes: &[PathBuf],
    target_home: &Path,
    provider_id: &str,
) -> Result<SessionSyncResult, String> {
    let sources = source_homes
        .iter()
        .map(|home| root_from_paths(local_codex_paths(home)))
        .collect::<Vec<_>>();
    sync_session_roots(
        &sources,
        root_from_paths(local_codex_paths(target_home)),
        Some(provider_id),
        SessionSyncPolicy::standard(true, true, SessionFileWritePolicy::Allow),
    )
}

pub fn sync_user_home_to_shared(
    codex_home: &Path,
    shared_home: &Path,
) -> Result<SessionSyncResult, String> {
    let current = resolve_user_codex_paths(codex_home)?;
    let shared = local_codex_paths(shared_home);
    sync_user_home_to_shared_with_paths(&current, &shared)
}

pub(crate) fn sync_user_home_to_shared_with_paths(
    current: &CodexPaths,
    shared: &CodexPaths,
) -> Result<SessionSyncResult, String> {
    sync_user_home_to_shared_with_policy(current, shared, SessionFileWritePolicy::Allow)
}

pub(crate) fn sync_user_home_to_shared_with_policy(
    current: &CodexPaths,
    shared: &CodexPaths,
    file_write_policy: SessionFileWritePolicy,
) -> Result<SessionSyncResult, String> {
    sync_session_roots(
        &[root_from_paths(current.clone())],
        root_from_paths(shared.clone()),
        None,
        SessionSyncPolicy::standard(true, false, file_write_policy),
    )
}

pub fn sync_shared_to_user_home(
    shared_home: &Path,
    codex_home: &Path,
    provider_id: &str,
) -> Result<SessionSyncResult, String> {
    let shared = local_codex_paths(shared_home);
    let current = resolve_user_codex_paths(codex_home)?;
    sync_shared_to_user_home_with_paths(&shared, &current, provider_id)
}

pub(crate) fn sync_shared_to_user_home_with_paths(
    shared: &CodexPaths,
    current: &CodexPaths,
    provider_id: &str,
) -> Result<SessionSyncResult, String> {
    sync_shared_to_user_home_with_policy(
        shared,
        current,
        provider_id,
        SessionFileWritePolicy::Allow,
    )
}

pub(crate) fn sync_shared_to_user_home_with_policy(
    shared: &CodexPaths,
    current: &CodexPaths,
    provider_id: &str,
    file_write_policy: SessionFileWritePolicy,
) -> Result<SessionSyncResult, String> {
    sync_session_roots(
        &[root_from_paths(shared.clone())],
        root_from_paths(current.clone()),
        Some(provider_id),
        SessionSyncPolicy::standard(true, true, file_write_policy),
    )
}

pub fn sync_shared_to_user_home_hot(
    shared_home: &Path,
    codex_home: &Path,
    provider_id: &str,
) -> Result<SessionSyncResult, String> {
    let shared = local_codex_paths(shared_home);
    let current = resolve_user_codex_paths(codex_home)?;
    sync_shared_to_user_home_hot_with_paths(&shared, &current, provider_id)
}

pub(crate) fn sync_shared_to_user_home_hot_with_paths(
    shared: &CodexPaths,
    current: &CodexPaths,
    provider_id: &str,
) -> Result<SessionSyncResult, String> {
    sync_shared_to_user_home_hot_with_policy(
        shared,
        current,
        provider_id,
        SessionFileWritePolicy::Allow,
    )
}

pub(crate) fn sync_shared_to_user_home_hot_with_policy(
    shared: &CodexPaths,
    current: &CodexPaths,
    provider_id: &str,
    file_write_policy: SessionFileWritePolicy,
) -> Result<SessionSyncResult, String> {
    sync_session_roots(
        &[root_from_paths(shared.clone())],
        root_from_paths(current.clone()),
        Some(provider_id),
        SessionSyncPolicy::hot_current(file_write_policy),
    )
}

pub(crate) fn sync_selected_user_home_to_shared_with_paths(
    current: &CodexPaths,
    shared: &CodexPaths,
    ids: &HashSet<String>,
) -> Result<SessionSyncResult, String> {
    sync_selected_session_roots(
        &root_from_paths(current.clone()),
        root_from_paths(shared.clone()),
        ids,
        None,
        SessionSyncPolicy::incremental(false, false),
    )
}

pub(crate) fn sync_selected_shared_to_user_home_hot_with_paths(
    shared: &CodexPaths,
    current: &CodexPaths,
    ids: &HashSet<String>,
    provider_id: &str,
) -> Result<SessionSyncResult, String> {
    sync_selected_session_roots(
        &root_from_paths(shared.clone()),
        root_from_paths(current.clone()),
        ids,
        Some(provider_id),
        SessionSyncPolicy::incremental(false, true),
    )
}

pub(crate) fn normalize_selected_user_home_provider_with_paths(
    current: &CodexPaths,
    ids: &HashSet<String>,
    provider_id: &str,
) -> Result<SessionSyncResult, String> {
    let root = root_from_paths(current.clone());
    sync_selected_session_roots(
        &root,
        root.clone(),
        ids,
        Some(provider_id),
        SessionSyncPolicy::incremental_self_provider(),
    )
}

#[cfg(test)]
pub(crate) fn plan_runtime_session_storage_with_paths(
    current: &CodexPaths,
    shared: &CodexPaths,
    target_provider: &str,
) -> Result<RuntimeSessionStoragePlan, String> {
    let current_root = root_from_paths(current.clone());
    let shared_root = root_from_paths(shared.clone());
    let mut plan = RuntimeSessionStoragePlan::default();

    if !shared_root.state_db.is_file() {
        let state_bytes = fs::metadata(&current_root.state_db)
            .map_err(|error| format!("failed to inspect current state_5.sqlite: {error}"))?
            .len();
        plan.add(shared_root.state_db.clone(), state_bytes)?;
    }

    plan_root_storage_writes(&current_root, &shared_root, &mut plan)?;
    plan_provider_storage_writes(&current_root, &shared_root, target_provider, &mut plan)?;
    plan_state_database_workspace(&current_root, &shared_root, &mut plan)?;
    Ok(plan)
}

#[cfg(test)]
fn plan_root_storage_writes(
    source_root: &SyncRoot,
    target_root: &SyncRoot,
    plan: &mut RuntimeSessionStoragePlan,
) -> Result<(), String> {
    let source_conn = open_source_conn(source_root)?;
    let source_scan = read_source_threads(source_root, source_conn.as_ref())?;
    for thread in &source_scan.threads {
        let rollout = plan_stable_rollout_file(target_root, thread, true)?;
        if rollout.action.writes_session_file() {
            let target = rollout.action.target_path().to_path_buf();
            plan.add(target.clone(), rollout.source.version.length)?;
            plan.add(
                provider_slot_marker_path(&target)?,
                PROVIDER_SLOT_MARKER_MAX_BYTES,
            )?;
        }
    }
    let index = plan_session_index_merge(source_root, target_root, &source_scan.candidate_ids)?;
    if !index.lines.is_empty() {
        let target_bytes = fs::metadata(&target_root.session_index)
            .map(|metadata| metadata.len())
            .or_else(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    Ok(0)
                } else {
                    Err(error)
                }
            })
            .map_err(|error| format!("failed to inspect target session index: {error}"))?;
        let separator = u64::from(index.target_needs_newline);
        let appended = index.lines.iter().try_fold(0_u64, |total, line| {
            let line_bytes = u64::try_from(line.len())
                .map_err(|_| "session index capacity calculation overflowed".to_string())?;
            total
                .checked_add(line_bytes)
                .and_then(|value| value.checked_add(1))
                .ok_or_else(|| "session index capacity calculation overflowed".to_string())
        })?;
        let output_bytes = target_bytes
            .checked_add(separator)
            .and_then(|value| value.checked_add(appended))
            .ok_or_else(|| "session index capacity calculation overflowed".to_string())?;
        plan.add(target_root.session_index.clone(), output_bytes)?;
    }
    Ok(())
}

#[cfg(test)]
fn plan_provider_storage_writes(
    current_root: &SyncRoot,
    shared_root: &SyncRoot,
    provider_id: &str,
    plan: &mut RuntimeSessionStoragePlan,
) -> Result<(), String> {
    let current_conn = open_source_conn(current_root)?
        .ok_or_else(|| "state_5.sqlite is required before switching runtimes".to_string())?;
    let current_scan = read_source_threads(current_root, Some(&current_conn))?;
    let current_threads = current_scan
        .threads
        .iter()
        .cloned()
        .map(|thread| (thread.id.clone(), thread))
        .collect::<HashMap<_, _>>();
    let mut selected = HashMap::new();
    if let Some(shared_conn) = open_source_conn(shared_root)? {
        let shared_scan = read_source_threads(shared_root, Some(&shared_conn))?;
        selected.extend(
            shared_scan
                .threads
                .into_iter()
                .map(|thread| (thread.id.clone(), thread)),
        );
    }
    for current_thread in current_threads.values() {
        match selected.get(&current_thread.id) {
            Some(shared_thread)
                if session_file_relation(
                    &current_thread.session_file,
                    &shared_thread.session_file,
                )? != SessionFileRelation::LeftExtendsRight => {}
            _ => {
                selected.insert(current_thread.id.clone(), current_thread.clone());
            }
        }
    }

    for thread in selected.into_values() {
        validate_remote_thread_id(&thread.id)?;
        let existing = existing_thread_rollout_path(&current_conn, current_root, &thread.id)?;
        let anchor = existing.as_deref().map(PathBuf::from).unwrap_or_else(|| {
            current_root
                .root
                .join(rollout_relative_path(&thread.session_file))
        });
        let mut source_path = thread.session_file.clone();
        if anchor.is_file() {
            match session_file_relation(&thread.session_file, &anchor)? {
                SessionFileRelation::LeftExtendsRight => {}
                SessionFileRelation::Equal | SessionFileRelation::RightExtendsLeft => {
                    source_path = anchor.clone();
                }
                SessionFileRelation::Divergent => {
                    let raw = plan_stable_rollout_file(current_root, &thread, true)?;
                    if raw.action.writes_session_file() {
                        let target = raw.action.target_path().to_path_buf();
                        plan.add(target.clone(), raw.source.version.length)?;
                        plan.add(
                            provider_slot_marker_path(&target)?,
                            PROVIDER_SLOT_MARKER_MAX_BYTES,
                        )?;
                    }
                    source_path = anchor.clone();
                }
            }
        }
        let provider_plan = plan_provider_rollout_from_source(
            current_root,
            &source_path,
            &anchor,
            &thread.id,
            provider_id,
        )?;
        if provider_plan.action.writes_session_file() {
            let output_bytes = provider_output_len(
                &source_path,
                &thread.id,
                provider_id,
                &provider_plan.source.version,
            )?;
            let target = provider_plan.action.target_path().to_path_buf();
            plan.add(target.clone(), output_bytes)?;
            plan.add(
                provider_slot_marker_path(&target)?,
                PROVIDER_SLOT_MARKER_MAX_BYTES,
            )?;
        }
    }

    if shared_root.state_db.is_file() {
        let shared_conn = open_source_conn(shared_root)?;
        let shared_scan = read_source_threads(shared_root, shared_conn.as_ref())?;
        let index =
            plan_session_index_merge(shared_root, current_root, &shared_scan.candidate_ids)?;
        add_session_index_plan(current_root, &index, plan)?;
    }
    Ok(())
}

#[cfg(test)]
fn add_session_index_plan(
    target_root: &SyncRoot,
    index: &SessionIndexMergePlan,
    plan: &mut RuntimeSessionStoragePlan,
) -> Result<(), String> {
    if index.lines.is_empty() {
        return Ok(());
    }
    let target_bytes = fs::metadata(&target_root.session_index)
        .map(|metadata| metadata.len())
        .or_else(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                Ok(0)
            } else {
                Err(error)
            }
        })
        .map_err(|error| format!("failed to inspect target session index: {error}"))?;
    let separator = u64::from(index.target_needs_newline);
    let appended = index.lines.iter().try_fold(0_u64, |total, line| {
        let line_bytes = u64::try_from(line.len())
            .map_err(|_| "session index capacity calculation overflowed".to_string())?;
        total
            .checked_add(line_bytes)
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| "session index capacity calculation overflowed".to_string())
    })?;
    let output_bytes = target_bytes
        .checked_add(separator)
        .and_then(|value| value.checked_add(appended))
        .ok_or_else(|| "session index capacity calculation overflowed".to_string())?;
    plan.add(target_root.session_index.clone(), output_bytes)
}

#[cfg(test)]
fn plan_state_database_workspace(
    current_root: &SyncRoot,
    shared_root: &SyncRoot,
    plan: &mut RuntimeSessionStoragePlan,
) -> Result<(), String> {
    let current_bytes = sqlite_workspace_bytes(&current_root.state_db)?;
    let shared_bytes = if shared_root.state_db.is_file() {
        sqlite_workspace_bytes(&shared_root.state_db)?
    } else {
        current_bytes
    };
    plan.add_capacity(current_root.state_db.clone(), current_bytes)?;
    plan.add_capacity(shared_root.state_db.clone(), shared_bytes)?;
    Ok(())
}

#[cfg(test)]
fn sqlite_workspace_bytes(database: &Path) -> Result<u64, String> {
    let mut bytes = fs::metadata(database)
        .map_err(|error| format!("failed to inspect session database: {error}"))?
        .len();
    for suffix in ["-wal", "-shm"] {
        let auxiliary = PathBuf::from(format!("{}{suffix}", database.to_string_lossy()));
        if let Ok(metadata) = fs::metadata(auxiliary) {
            bytes = bytes
                .checked_add(metadata.len())
                .ok_or_else(|| "session database capacity calculation overflowed".to_string())?;
        }
    }
    Ok(bytes)
}

fn root_from_paths(paths: CodexPaths) -> SyncRoot {
    SyncRoot {
        root: paths.codex_home,
        state_db: paths.state_db,
        sessions_dir: paths.sessions_dir,
        session_index: paths.session_index,
    }
}

fn sync_session_roots(
    source_roots: &[SyncRoot],
    target_root: SyncRoot,
    provider_id: Option<&str>,
    policy: SessionSyncPolicy,
) -> Result<SessionSyncResult, String> {
    let prepared_sources = prepare_sources(
        source_roots,
        &target_root,
        policy.session_files,
        policy.session_index,
    )?;
    sync_prepared_session_roots(
        source_roots,
        &prepared_sources,
        target_root,
        provider_id,
        policy,
    )
}

fn sync_selected_session_roots(
    source_root: &SyncRoot,
    target_root: SyncRoot,
    ids: &HashSet<String>,
    provider_id: Option<&str>,
    policy: SessionSyncPolicy,
) -> Result<SessionSyncResult, String> {
    if ids.is_empty() {
        return Ok(empty_session_sync_result());
    }
    let prepared_sources = prepare_selected_sources(std::slice::from_ref(source_root), ids)?;
    sync_prepared_session_roots(
        std::slice::from_ref(source_root),
        &prepared_sources,
        target_root,
        provider_id,
        policy,
    )
}

fn sync_prepared_session_roots(
    source_roots: &[SyncRoot],
    prepared_sources: &[PreparedSource],
    target_root: SyncRoot,
    provider_id: Option<&str>,
    policy: SessionSyncPolicy,
) -> Result<SessionSyncResult, String> {
    if policy.update_existing_provider {
        for prepared in prepared_sources {
            for planned in &prepared.threads {
                validate_remote_thread_id(&planned.thread.id)?;
            }
        }
    }
    let target_conn = Connection::open(&target_root.state_db)
        .map_err(|error| format!("failed to open target state_5.sqlite: {error}"))?;
    target_conn
        .busy_timeout(Duration::from_secs(2))
        .map_err(|error| format!("failed to set target SQLite timeout: {error}"))?;
    target_conn
        .execute_batch("BEGIN IMMEDIATE")
        .map_err(|error| format!("failed to start session sync transaction: {error}"))?;

    let result = sync_sessions_in_transaction(
        prepared_sources,
        &target_root,
        &target_conn,
        provider_id,
        policy,
    );
    match result {
        Ok(mut result) => {
            target_conn
                .execute_batch("COMMIT")
                .map_err(|error| format!("failed to commit session sync transaction: {error}"))?;
            let quick_check: String = target_conn
                .query_row("PRAGMA quick_check", [], |row| row.get(0))
                .map_err(|error| format!("failed to verify target state_5.sqlite: {error}"))?;
            if quick_check != "ok" {
                return Err(format!(
                    "target state_5.sqlite failed quick_check: {quick_check}"
                ));
            }
            for (source_root, prepared) in source_roots.iter().zip(prepared_sources) {
                result.merged_session_index_entries += merge_session_index_with_policy(
                    source_root,
                    &target_root,
                    &prepared.candidate_ids,
                    policy.session_index,
                )?;
            }
            Ok(result)
        }
        Err(error) => {
            let _ = target_conn.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

fn empty_session_sync_result() -> SessionSyncResult {
    SessionSyncResult {
        inserted_threads: 0,
        copied_session_files: 0,
        duplicate_threads: 0,
        skipped_missing_session_files: 0,
        skipped_archived_threads: 0,
        merged_session_index_entries: 0,
        persistent_session_bytes_added: 0,
        persistent_session_bytes_reclaimed: 0,
        obsolete_provider_slots: Vec::new(),
    }
}

fn prepare_sources(
    source_roots: &[SyncRoot],
    target_root: &SyncRoot,
    file_write_policy: SessionFileWritePolicy,
    index_write_policy: SessionIndexWritePolicy,
) -> Result<Vec<PreparedSource>, String> {
    let mut prepared_sources = Vec::with_capacity(source_roots.len());
    for source_root in source_roots {
        let source_conn = open_source_conn(source_root)?;
        let source_scan = read_source_threads(source_root, source_conn.as_ref())?;
        let mut threads = Vec::with_capacity(source_scan.threads.len());
        for thread in source_scan.threads {
            if file_write_policy == SessionFileWritePolicy::Deny {
                let rollout_action = plan_rollout_file(target_root, &thread)?;
                if rollout_action.writes_session_file() {
                    return Err(
                        "session JSONL changed after fast-path planning; retry the session sync"
                            .to_string(),
                    );
                }
            }
            threads.push(PlannedSourceThread { thread });
        }
        if index_write_policy == SessionIndexWritePolicy::Deny
            && !plan_session_index_merge(source_root, target_root, &source_scan.candidate_ids)?
                .lines
                .is_empty()
        {
            return Err(
                "session index changed after fast-path planning; retry the session sync"
                    .to_string(),
            );
        }
        prepared_sources.push(PreparedSource {
            source_conn,
            threads,
            candidate_ids: source_scan.candidate_ids,
            skipped_archived_threads: source_scan.skipped_archived_threads,
            skipped_missing_session_files: source_scan.skipped_missing_session_files,
        });
    }
    Ok(prepared_sources)
}

fn prepare_selected_sources(
    source_roots: &[SyncRoot],
    ids: &HashSet<String>,
) -> Result<Vec<PreparedSource>, String> {
    let mut prepared_sources = Vec::with_capacity(source_roots.len());
    for source_root in source_roots {
        let source_conn = open_source_conn(source_root)?;
        let source_scan = read_source_threads_selected(source_root, source_conn.as_ref(), ids)?;
        let threads = source_scan
            .threads
            .into_iter()
            .map(|thread| PlannedSourceThread { thread })
            .collect();
        prepared_sources.push(PreparedSource {
            source_conn,
            threads,
            candidate_ids: source_scan.candidate_ids,
            skipped_archived_threads: source_scan.skipped_archived_threads,
            skipped_missing_session_files: source_scan.skipped_missing_session_files,
        });
    }
    Ok(prepared_sources)
}

fn sync_sessions_in_transaction(
    prepared_sources: &[PreparedSource],
    target_root: &SyncRoot,
    target_conn: &Connection,
    provider_id: Option<&str>,
    policy: SessionSyncPolicy,
) -> Result<SessionSyncResult, String> {
    let mut inserted_threads = 0;
    let mut copied_session_files = 0;
    let mut duplicate_threads = 0;
    let mut skipped_missing_session_files = 0;
    let mut skipped_archived_threads = 0;
    let mut persistent_session_bytes_added = 0_u64;
    let mut obsolete_provider_slots = Vec::new();

    for prepared in prepared_sources {
        skipped_archived_threads += prepared.skipped_archived_threads;
        skipped_missing_session_files += prepared.skipped_missing_session_files;

        for planned in &prepared.threads {
            let thread = &planned.thread;
            let existing_thread = thread_exists(target_conn, &thread.id)?;
            if existing_thread
                && policy.existing_rollout_path == ExistingRolloutPathPolicy::PreserveExisting
            {
                duplicate_threads += 1;
                continue;
            }
            let existing_rollout =
                existing_thread_rollout_path(target_conn, target_root, &thread.id)?;
            if policy.update_existing_provider {
                let provider_id = provider_id.ok_or_else(|| {
                    "target provider is required for a provider-aware sync".to_string()
                })?;
                let (provider_rollout, preserved_raw) = copy_provider_rollout_for_closed_sync(
                    thread,
                    target_root,
                    existing_rollout.as_deref(),
                    provider_id,
                    policy.session_files,
                )?;
                if let Some(raw) = preserved_raw.as_ref() {
                    record_rollout_storage(
                        raw,
                        &mut persistent_session_bytes_added,
                        &mut obsolete_provider_slots,
                    )?;
                    copied_session_files += usize::from(raw.copied);
                }
                record_rollout_storage(
                    &provider_rollout,
                    &mut persistent_session_bytes_added,
                    &mut obsolete_provider_slots,
                )?;
                copied_session_files += usize::from(provider_rollout.copied);
                let mut stable_thread = thread.clone();
                stable_thread.meta = provider_rollout.source_meta.clone();
                if existing_thread {
                    duplicate_threads += 1;
                    update_existing_thread(
                        target_conn,
                        &thread.id,
                        Some(provider_rollout.path.as_str()),
                        Some(provider_id),
                    )?;
                } else {
                    insert_thread(
                        target_conn,
                        &stable_thread,
                        provider_rollout.path.as_str(),
                        Some(provider_id),
                    )?;
                    inserted_threads += 1;
                }
                continue;
            }
            if !existing_thread {
                if let Some(provider_id) = provider_id {
                    let (provider_rollout, preserved_raw) = copy_provider_rollout_for_closed_sync(
                        thread,
                        target_root,
                        None,
                        provider_id,
                        policy.session_files,
                    )?;
                    debug_assert!(preserved_raw.is_none());
                    record_rollout_storage(
                        &provider_rollout,
                        &mut persistent_session_bytes_added,
                        &mut obsolete_provider_slots,
                    )?;
                    let mut stable_thread = thread.clone();
                    stable_thread.meta = provider_rollout.source_meta.clone();
                    insert_thread(
                        target_conn,
                        &stable_thread,
                        provider_rollout.path.as_str(),
                        Some(provider_id),
                    )?;
                    inserted_threads += 1;
                    copied_session_files += usize::from(provider_rollout.copied);
                    continue;
                }
            }
            let mut copied_rollout = copy_rollout_file(
                thread,
                target_root,
                policy.allow_existing_replacement,
                policy.session_files,
                None,
            )?;
            if let Some(existing_rollout) = existing_rollout.as_deref() {
                attach_owned_predecessor(
                    target_root,
                    existing_rollout,
                    &thread.id,
                    &mut copied_rollout,
                )?;
            }
            let mut stable_thread = thread.clone();
            stable_thread.meta = copied_rollout.source_meta.clone();
            if existing_thread {
                duplicate_threads += 1;
                let selected_rollout = select_target_rollout_path(
                    existing_rollout.as_deref(),
                    copied_rollout.path.as_str(),
                )?;
                let selected_rollout = RolloutCopy {
                    path: selected_rollout,
                    copied: false,
                    created_bytes: 0,
                    source_meta: copied_rollout.source_meta.clone(),
                    obsolete_provider_slot: None,
                };
                let provider_for_thread =
                    if copied_rollout.copied && copied_rollout.path == selected_rollout.path {
                        provider_id
                    } else {
                        None
                    };
                update_existing_thread(
                    target_conn,
                    &thread.id,
                    Some(selected_rollout.path.as_str()),
                    provider_for_thread,
                )?;
                record_rollout_storage(
                    &copied_rollout,
                    &mut persistent_session_bytes_added,
                    &mut obsolete_provider_slots,
                )?;
                record_rollout_storage(
                    &selected_rollout,
                    &mut persistent_session_bytes_added,
                    &mut obsolete_provider_slots,
                )?;
                copied_session_files +=
                    usize::from(copied_rollout.copied) + usize::from(selected_rollout.copied);
                continue;
            }
            record_rollout_storage(
                &copied_rollout,
                &mut persistent_session_bytes_added,
                &mut obsolete_provider_slots,
            )?;
            insert_thread(
                target_conn,
                &stable_thread,
                copied_rollout.path.as_str(),
                None,
            )?;
            inserted_threads += 1;
            if copied_rollout.copied {
                copied_session_files += 1;
            }
        }
        if policy.copy_dependent_rows {
            let Some(source_conn) = prepared.source_conn.as_ref() else {
                continue;
            };
            copy_dependent_rows(source_conn, target_conn, &prepared.candidate_ids)?;
        }
    }

    Ok(SessionSyncResult {
        inserted_threads,
        copied_session_files,
        duplicate_threads,
        skipped_missing_session_files,
        skipped_archived_threads,
        merged_session_index_entries: 0,
        persistent_session_bytes_added,
        persistent_session_bytes_reclaimed: 0,
        obsolete_provider_slots,
    })
}

fn record_rollout_storage(
    copy: &RolloutCopy,
    added_bytes: &mut u64,
    obsolete_slots: &mut Vec<ObsoleteProviderSlot>,
) -> Result<(), String> {
    *added_bytes = added_bytes
        .checked_add(copy.created_bytes)
        .ok_or_else(|| "session storage accounting overflowed".to_string())?;
    if let Some(obsolete) = copy.obsolete_provider_slot.clone() {
        obsolete_slots.push(obsolete);
    }
    Ok(())
}

fn attach_owned_predecessor(
    target_root: &SyncRoot,
    existing_rollout: &str,
    thread_id: &str,
    successor: &mut RolloutCopy,
) -> Result<(), String> {
    if !successor.copied || successor.obsolete_provider_slot.is_some() {
        return Ok(());
    }
    let existing = PathBuf::from(existing_rollout);
    if !existing.is_file()
        || !matches!(
            session_file_relation(Path::new(&successor.path), &existing)?,
            SessionFileRelation::LeftExtendsRight
        )
    {
        return Ok(());
    }
    let Some(obsolete) = owned_provider_slot(target_root, &existing, thread_id)? else {
        return Ok(());
    };
    let successor_path = PathBuf::from(&successor.path);
    let Some(successor_marker) =
        read_provider_slot_marker(target_root, &successor_path, thread_id)?
    else {
        return Ok(());
    };
    let successor_version =
        read_stable_source(&successor_path, thread_id, |_, _, _| Ok(()))?.version;
    successor.obsolete_provider_slot = Some(ObsoleteProviderSlot {
        obsolete: obsolete.path,
        successor: successor_path,
        obsolete_version: obsolete.version,
        obsolete_marker: obsolete.marker,
        successor_version,
        successor_marker,
    });
    Ok(())
}

fn open_source_conn(source_root: &SyncRoot) -> Result<Option<Connection>, String> {
    if !source_root.state_db.exists() {
        return Ok(None);
    }
    let conn = Connection::open_with_flags(&source_root.state_db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| format!("failed to open source state_5.sqlite: {error}"))?;
    conn.busy_timeout(Duration::from_secs(2))
        .map_err(|error| format!("failed to set source SQLite timeout: {error}"))?;
    Ok(Some(conn))
}

fn read_source_threads(
    source_root: &SyncRoot,
    source_conn: Option<&Connection>,
) -> Result<SourceScan, String> {
    let source_rows = if let Some(conn) = source_conn {
        read_source_thread_rows(conn)?
    } else {
        HashMap::new()
    };
    let mut threads = Vec::new();
    let mut skipped_archived = 0;
    for (session_file, meta) in read_session_files(source_root, &source_rows)? {
        if meta.id.trim().is_empty() {
            continue;
        }
        let row = source_rows.get(&meta.id);
        if row.is_some_and(source_row_is_archived) {
            skipped_archived += 1;
            continue;
        }
        threads.push(SourceThread {
            id: meta.id.clone(),
            values_by_column: row
                .map(|row| row.values_by_column.clone())
                .unwrap_or_default(),
            session_file,
            meta,
        });
    }
    let candidate_ids = threads
        .iter()
        .map(|thread| thread.id.clone())
        .collect::<HashSet<_>>();
    let skipped_missing_session_files = source_rows
        .iter()
        .filter(|(id, row)| !source_row_is_archived(row) && !candidate_ids.contains(id.as_str()))
        .count();
    Ok(SourceScan {
        threads,
        candidate_ids,
        skipped_archived_threads: skipped_archived,
        skipped_missing_session_files,
    })
}

fn read_source_threads_selected(
    source_root: &SyncRoot,
    source_conn: Option<&Connection>,
    ids: &HashSet<String>,
) -> Result<SourceScan, String> {
    let Some(conn) = source_conn else {
        return Err("state_5.sqlite is required for incremental session sync".to_string());
    };
    let source_rows = read_source_thread_rows(conn)?;
    let mut threads = Vec::with_capacity(ids.len());
    let mut skipped_archived_threads = 0;
    let mut skipped_missing_session_files = 0;

    for id in ids {
        let Some(row) = source_rows.get(id) else {
            skipped_missing_session_files += 1;
            continue;
        };
        if source_row_is_archived(row) {
            skipped_archived_threads += 1;
            continue;
        }
        let Some(session_file) = existing_thread_rollout_path(conn, source_root, id)? else {
            skipped_missing_session_files += 1;
            continue;
        };
        let session_file = PathBuf::from(session_file);
        let Some(meta) = session_file_meta(&session_file)? else {
            skipped_missing_session_files += 1;
            continue;
        };
        if meta.id != *id {
            return Err("selected source session JSONL does not match its thread id".to_string());
        }
        threads.push(SourceThread {
            id: id.clone(),
            values_by_column: row.values_by_column.clone(),
            session_file,
            meta,
        });
    }

    threads.sort_by(|left, right| left.session_file.cmp(&right.session_file));
    let candidate_ids = threads
        .iter()
        .map(|thread| thread.id.clone())
        .collect::<HashSet<_>>();
    Ok(SourceScan {
        threads,
        candidate_ids,
        skipped_archived_threads,
        skipped_missing_session_files,
    })
}

fn source_row_is_archived(row: &SourceRow) -> bool {
    archived_value_is_true(row.values_by_column.get("archived"))
}

fn archived_value_is_true(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Integer(value)) => *value != 0,
        Some(Value::Real(value)) => *value != 0.0,
        Some(Value::Text(value)) => {
            let normalized = value.trim().to_ascii_lowercase();
            matches!(normalized.as_str(), "1" | "true" | "yes")
        }
        _ => false,
    }
}

fn read_source_thread_rows(conn: &Connection) -> Result<HashMap<String, SourceRow>, String> {
    if !table_exists(conn, "threads")? {
        return Ok(HashMap::new());
    }
    let columns = table_columns(conn, "threads")?;
    if !columns.iter().any(|column| column == "id") {
        return Ok(HashMap::new());
    }
    let id_index = columns.iter().position(|column| column == "id").unwrap();
    let select = format!("SELECT {} FROM threads", columns.join(", "));
    let mut statement = conn
        .prepare(&select)
        .map_err(|error| format!("failed to prepare source thread query: {error}"))?;
    let rows = statement
        .query_map([], |row| {
            let mut values = HashMap::new();
            let mut id = String::new();
            for (index, column) in columns.iter().enumerate() {
                let value = row.get::<usize, Value>(index)?;
                if index == id_index {
                    if let Value::Text(text) = &value {
                        id = text.clone();
                    }
                }
                values.insert(column.clone(), value);
            }
            Ok((
                id,
                SourceRow {
                    values_by_column: values,
                },
            ))
        })
        .map_err(|error| format!("failed to read source threads: {error}"))?;

    let mut output = HashMap::new();
    for row in rows {
        let (id, source_row) =
            row.map_err(|error| format!("failed to collect source threads: {error}"))?;
        if !id.is_empty() {
            output.insert(id, source_row);
        }
    }
    Ok(output)
}

fn read_session_files(
    source_root: &SyncRoot,
    source_rows: &HashMap<String, SourceRow>,
) -> Result<Vec<(PathBuf, SessionMeta)>, String> {
    if !source_root.sessions_dir.exists() {
        return Ok(Vec::new());
    }
    let files = walk_jsonl_files(&source_root.sessions_dir)?;

    let mut grouped = HashMap::<String, Vec<(PathBuf, SessionMeta)>>::new();
    for path in files {
        let Some(meta) = session_file_meta(&path)? else {
            continue;
        };
        grouped
            .entry(meta.id.clone())
            .or_default()
            .push((path, meta));
    }

    let mut output = Vec::new();
    for (id, mut candidates) in grouped {
        let preferred_relative = source_rows
            .get(&id)
            .and_then(|row| row.values_by_column.get("rollout_path"))
            .and_then(|value| match value {
                Value::Text(path) => relative_from_sessions(Path::new(path)),
                _ => None,
            });
        let mut selected = preferred_relative
            .as_ref()
            .and_then(|preferred| {
                candidates
                    .iter()
                    .position(|(path, _)| relative_from_sessions(path).as_ref() == Some(preferred))
            })
            .unwrap_or_else(|| {
                candidates
                    .iter()
                    .enumerate()
                    .max_by_key(|(_, (path, _))| {
                        let metadata = fs::metadata(path).ok();
                        (
                            metadata.as_ref().map(|value| value.len()).unwrap_or(0),
                            metadata
                                .and_then(|value| value.modified().ok())
                                .and_then(system_time_millis)
                                .unwrap_or(0),
                        )
                    })
                    .map(|(index, _)| index)
                    .unwrap_or(0)
            });
        loop {
            let mut extensions = Vec::new();
            for (index, (path, _)) in candidates.iter().enumerate() {
                if index != selected
                    && session_file_relation(path, &candidates[selected].0)?
                        == SessionFileRelation::LeftExtendsRight
                {
                    extensions.push(index);
                }
            }
            let Some(mut next) = extensions.first().copied() else {
                break;
            };
            let mut ambiguous = false;
            for index in extensions.into_iter().skip(1) {
                match session_file_relation(&candidates[index].0, &candidates[next].0)? {
                    SessionFileRelation::LeftExtendsRight => next = index,
                    SessionFileRelation::RightExtendsLeft | SessionFileRelation::Equal => {}
                    SessionFileRelation::Divergent => {
                        ambiguous = true;
                        break;
                    }
                }
            }
            if ambiguous {
                break;
            }
            selected = next;
        }
        output.push(candidates.swap_remove(selected));
    }
    output.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(output)
}

fn insert_thread(
    conn: &Connection,
    thread: &SourceThread,
    rollout_path: &str,
    provider_id: Option<&str>,
) -> Result<(), String> {
    let schema = table_schema(conn, "threads")?;
    let mut columns = Vec::with_capacity(schema.len());
    let mut values = Vec::with_capacity(schema.len());
    for column in &schema {
        let value = thread_value_for_target_column(thread, column, rollout_path, provider_id)?;
        // Binding NULL would bypass SQLite's declared default and can violate a new NOT NULL
        // column. Omit a missing/default-backed NULL, but preserve an explicit source NULL when
        // the target remains nullable.
        let use_target_default = matches!(value, Value::Null)
            && column.default_value.is_some()
            && (column.not_null || !thread.values_by_column.contains_key(&column.name));
        if use_target_default {
            continue;
        }
        columns.push(column.name.clone());
        values.push(value);
    }
    let placeholders = (0..columns.len())
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "INSERT INTO threads ({}) VALUES ({})",
        columns.join(", "),
        placeholders
    );
    let inserted = conn
        .execute(&sql, rusqlite::params_from_iter(values))
        .map_err(|error| format!("failed to insert target thread: {error}"))?;
    if inserted != 1 {
        return Err(format!("target thread insert affected {inserted} rows"));
    }
    Ok(())
}

fn thread_value_for_target_column(
    thread: &SourceThread,
    column: &TableColumn,
    rollout_path: &str,
    provider_id: Option<&str>,
) -> Result<Value, String> {
    if column.name == "rollout_path" {
        return Ok(Value::Text(rollout_path.to_string()));
    }
    if column.name == "model_provider" {
        return Ok(Value::Text(
            provider_id
                .map(ToOwned::to_owned)
                .or_else(|| thread.meta.model_provider.clone())
                .unwrap_or_else(|| "openai".to_string()),
        ));
    }
    if let Some(value) = thread.values_by_column.get(&column.name) {
        return Ok(value.clone());
    }
    let file_ms = file_modified_millis(&thread.session_file).unwrap_or_else(now_millis);
    let value = match column.name.as_str() {
        "id" => Value::Text(thread.id.clone()),
        "created_at" | "updated_at" | "recency_at" => {
            Value::Integer(thread.meta.timestamp_millis.unwrap_or(file_ms) / 1000)
        }
        "created_at_ms" | "updated_at_ms" | "recency_at_ms" => {
            Value::Integer(thread.meta.timestamp_millis.unwrap_or(file_ms))
        }
        "source" => Value::Text(
            thread
                .meta
                .source
                .clone()
                .unwrap_or_else(|| "cli".to_string()),
        ),
        "cwd" => Value::Text(thread.meta.cwd.clone().unwrap_or_default()),
        "cli_version" => Value::Text(thread.meta.cli_version.clone().unwrap_or_default()),
        "title" | "preview" | "first_user_message" | "sandbox_policy" | "approval_mode" => {
            Value::Text(String::new())
        }
        "tokens_used" | "has_user_event" | "archived" => Value::Integer(0),
        "memory_mode" => Value::Text("enabled".to_string()),
        "thread_source" | "agent_nickname" | "agent_role" | "agent_path" | "model"
        | "reasoning_effort" | "archived_at" | "git_sha" | "git_branch" | "git_origin_url" => {
            Value::Null
        }
        _ if column.default_value.is_some() || !column.not_null => Value::Null,
        _ => {
            return Err(format!(
                "unsupported threads schema: required column {} has no known value or default",
                column.name
            ))
        }
    };
    Ok(value)
}

fn thread_exists(conn: &Connection, id: &str) -> Result<bool, String> {
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM threads WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .map_err(|error| format!("failed to check target thread: {error}"))?;
    Ok(count > 0)
}

fn existing_thread_rollout_path(
    conn: &Connection,
    target_root: &SyncRoot,
    id: &str,
) -> Result<Option<String>, String> {
    if !thread_exists(conn, id)? {
        return Ok(None);
    }
    let columns = table_columns(conn, "threads")?;
    if !columns.iter().any(|column| column == "rollout_path") {
        return Ok(None);
    }
    let stored = conn
        .query_row(
            "SELECT rollout_path FROM threads WHERE id = ?1",
            [id],
            |row| row.get::<_, Option<String>>(0),
        )
        .map_err(|error| format!("failed to read target thread rollout path: {error}"))?;
    let Some(stored) = stored else {
        return Ok(None);
    };
    let stored_path = PathBuf::from(&stored);
    let candidate = if stored_path.is_absolute() {
        stored_path
    } else {
        target_root.root.join(stored_path)
    };
    let Ok(relative) = candidate.strip_prefix(&target_root.sessions_dir) else {
        return Ok(None);
    };
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Ok(None);
    }
    if !target_root.sessions_dir.is_dir() || !candidate.is_file() {
        return Ok(None);
    }
    let canonical_candidate = fs::canonicalize(&candidate)
        .map_err(|error| format!("failed to resolve target thread rollout path: {error}"))?;
    let canonical_sessions = fs::canonicalize(&target_root.sessions_dir)
        .map_err(|error| format!("failed to resolve target sessions directory: {error}"))?;
    if !canonical_candidate.starts_with(&canonical_sessions) {
        return Ok(None);
    }
    if session_file_meta(&canonical_candidate)?.is_none_or(|meta| meta.id != id) {
        return Ok(None);
    }
    Ok(Some(candidate.to_string_lossy().to_string()))
}

fn select_target_rollout_path(
    existing_rollout: Option<&str>,
    copied_rollout: &str,
) -> Result<String, String> {
    let Some(existing_rollout) = existing_rollout else {
        return Ok(copied_rollout.to_string());
    };
    let existing = Path::new(existing_rollout);
    let copied = Path::new(copied_rollout);
    if !existing.exists() {
        return Ok(copied_rollout.to_string());
    }
    if !copied.exists() || existing == copied {
        return Ok(existing_rollout.to_string());
    }
    match session_file_relation(copied, existing)? {
        SessionFileRelation::LeftExtendsRight => Ok(copied_rollout.to_string()),
        SessionFileRelation::Equal
        | SessionFileRelation::RightExtendsLeft
        | SessionFileRelation::Divergent => Ok(existing_rollout.to_string()),
    }
}

fn update_existing_thread(
    conn: &Connection,
    thread_id: &str,
    rollout_path: Option<&str>,
    provider_id: Option<&str>,
) -> Result<usize, String> {
    let columns = table_columns(conn, "threads")?;
    let mut assignments = Vec::new();
    let mut differences = Vec::new();
    let mut values = Vec::new();
    if let Some(path) = rollout_path {
        if columns.iter().any(|column| column == "rollout_path") {
            let parameter = values.len() + 1;
            assignments.push(format!("rollout_path = ?{parameter}"));
            differences.push(format!("rollout_path IS NOT ?{parameter}"));
            values.push(Value::Text(path.to_string()));
        }
    }
    if let Some(provider_id) = provider_id {
        if columns.iter().any(|column| column == "model_provider") {
            let parameter = values.len() + 1;
            assignments.push(format!("model_provider = ?{parameter}"));
            differences.push(format!("model_provider IS NOT ?{parameter}"));
            values.push(Value::Text(provider_id.to_string()));
        }
    }
    if assignments.is_empty() {
        return Ok(0);
    }
    let id_parameter = values.len() + 1;
    values.push(Value::Text(thread_id.to_string()));
    let sql = format!(
        "UPDATE threads SET {} WHERE id = ?{} AND ({})",
        assignments.join(", "),
        id_parameter,
        differences.join(" OR ")
    );
    conn.execute(&sql, rusqlite::params_from_iter(values))
        .map_err(|error| format!("failed to update target thread: {error}"))
}

fn copy_rollout_file(
    thread: &SourceThread,
    target_root: &SyncRoot,
    allow_existing_replacement: bool,
    file_write_policy: SessionFileWritePolicy,
    provider_id: Option<&str>,
) -> Result<RolloutCopy, String> {
    let mut last_source_change = None;
    let publish_extensions =
        allow_existing_replacement || file_write_policy == SessionFileWritePolicy::Deny;
    for attempt in 0..SOURCE_STABILITY_ATTEMPTS {
        let StableRolloutPlan {
            action,
            source,
            marker_origin,
            supersedes,
        } = plan_stable_rollout_file(target_root, thread, publish_extensions)?;
        if action.writes_session_file() && file_write_policy == SessionFileWritePolicy::Deny {
            return Err(
                "session JSONL changed after fast-path planning; retry the session sync"
                    .to_string(),
            );
        }

        let target_path = action.target_path().to_path_buf();
        let result = match &action {
            RolloutFileAction::Unchanged(_) => {
                let rechecked = plan_stable_rollout_file(target_root, thread, publish_extensions)?;
                if rechecked.action == action && rechecked.source.version == source.version {
                    return Ok(rollout_copy(&target_path, false, 0, &rechecked.source));
                }
                last_source_change = Some(source_changed(
                    "the source or target changed before an unchanged plan was finalized",
                ));
                Ok(None)
            }
            RolloutFileAction::Create(_) | RolloutFileAction::Import(_) => {
                match create_session_file_if_absent(
                    &thread.session_file,
                    &target_path,
                    &thread.id,
                    provider_id,
                    &source.version,
                ) {
                    Ok(true) => {
                        let output_provider =
                            provider_id.or(source.version.meta.model_provider.as_deref());
                        let (successor_marker, marker_bytes) = match write_provider_slot_marker(
                            target_root,
                            &target_path,
                            marker_origin.as_deref(),
                            &thread.id,
                            output_provider,
                        ) {
                            Ok(marker) => marker,
                            Err(error) => {
                                let _ = fs::remove_file(&target_path);
                                return Err(error);
                            }
                        };
                        let successor_version =
                            read_stable_source(&target_path, &thread.id, |_, _, _| Ok(()))?.version;
                        let created_bytes = fs::metadata(&target_path)
                            .map_err(|error| {
                                format!("failed to inspect copied session JSONL: {error}")
                            })?
                            .len()
                            .checked_add(marker_bytes)
                            .ok_or_else(|| "session storage accounting overflowed".to_string())?;
                        let mut copy = rollout_copy(&target_path, true, created_bytes, &source);
                        copy.obsolete_provider_slot =
                            supersedes.map(|obsolete| ObsoleteProviderSlot {
                                obsolete: obsolete.path,
                                successor: target_path.clone(),
                                obsolete_version: obsolete.version,
                                obsolete_marker: obsolete.marker,
                                successor_version,
                                successor_marker,
                            });
                        return Ok(copy);
                    }
                    Err(error) => Err(error),
                    Ok(false) => match stable_source_relation_to_target(&source, &target_path)? {
                        SessionFileRelation::Equal | SessionFileRelation::RightExtendsLeft => {
                            return Ok(rollout_copy(&target_path, false, 0, &source));
                        }
                        SessionFileRelation::LeftExtendsRight => {
                            if publish_extensions && matches!(&action, RolloutFileAction::Create(_))
                            {
                                Ok(None)
                            } else if publish_extensions {
                                Err("target imported session JSONL is shorter than its content hash"
                                    .to_string())
                            } else {
                                return Ok(rollout_copy(&target_path, false, 0, &source));
                            }
                        }
                        SessionFileRelation::Divergent
                            if matches!(&action, RolloutFileAction::Create(_)) =>
                        {
                            Ok(None)
                        }
                        SessionFileRelation::Divergent => {
                            Err("target session JSONL changed during no-clobber publish"
                                .to_string())
                        }
                    },
                }
            }
        };
        match result {
            Ok(Some(copy)) => return Ok(copy),
            Ok(None) => continue,
            Err(error) if is_source_changed(&error) => {
                last_source_change = Some(error);
                if attempt + 1 < SOURCE_STABILITY_ATTEMPTS {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_source_change.unwrap_or_else(|| {
        "target session JSONL kept changing during no-clobber publish".to_string()
    }))
}

fn rollout_copy(
    target: &Path,
    copied: bool,
    created_bytes: u64,
    source: &StableSourceData,
) -> RolloutCopy {
    RolloutCopy {
        path: target.to_string_lossy().to_string(),
        copied,
        created_bytes,
        source_meta: source.version.meta.clone(),
        obsolete_provider_slot: None,
    }
}

fn copy_provider_rollout_for_closed_sync(
    thread: &SourceThread,
    target_root: &SyncRoot,
    existing_rollout: Option<&str>,
    provider_id: &str,
    file_write_policy: SessionFileWritePolicy,
) -> Result<(RolloutCopy, Option<RolloutCopy>), String> {
    let existing = existing_rollout.map(PathBuf::from);
    let mut preserved_raw = None;
    let anchor = existing.clone().unwrap_or_else(|| {
        target_root
            .root
            .join(rollout_relative_path(&thread.session_file))
    });
    let source = match existing.as_deref() {
        Some(existing) => match session_file_relation(&thread.session_file, existing)? {
            SessionFileRelation::LeftExtendsRight => thread.session_file.clone(),
            SessionFileRelation::Equal | SessionFileRelation::RightExtendsLeft => {
                existing.to_path_buf()
            }
            SessionFileRelation::Divergent => {
                let raw = copy_rollout_file(thread, target_root, true, file_write_policy, None)?;
                preserved_raw = Some(raw);
                existing.to_path_buf()
            }
        },
        None => thread.session_file.clone(),
    };
    if let Some(parent) = anchor.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create provider rollout directory: {error}"))?;
    }
    let provider = ensure_provider_rollout_from_source(
        target_root,
        &source,
        &anchor,
        &thread.id,
        provider_id,
        file_write_policy,
    )?;
    Ok((provider, preserved_raw))
}

fn ensure_provider_rollout_from_source(
    target_root: &SyncRoot,
    source_rollout: &Path,
    target_anchor: &Path,
    thread_id: &str,
    provider_id: &str,
    file_write_policy: SessionFileWritePolicy,
) -> Result<RolloutCopy, String> {
    let mut last_source_change = None;
    for attempt in 0..SOURCE_STABILITY_ATTEMPTS {
        let plan = plan_provider_rollout_from_source(
            target_root,
            source_rollout,
            target_anchor,
            thread_id,
            provider_id,
        )?;
        let action = plan.action;
        let source = plan.source;
        if action.writes_session_file() && file_write_policy == SessionFileWritePolicy::Deny {
            return Err(
                "session JSONL changed after fast-path planning; retry the session sync"
                    .to_string(),
            );
        }
        let target_path = action.target_path().to_path_buf();
        match action {
            ProviderRolloutAction::Unchanged(_) => {
                let rechecked = plan_provider_rollout_from_source(
                    target_root,
                    source_rollout,
                    target_anchor,
                    thread_id,
                    provider_id,
                )?;
                if rechecked.action.target_path() == target_path
                    && rechecked.source.version == source.version
                {
                    return Ok(rollout_copy(&target_path, false, 0, &rechecked.source));
                }
                last_source_change = Some(source_changed(
                    "the selected provider rollout changed before it was finalized",
                ));
            }
            ProviderRolloutAction::Create { supersedes, .. } => {
                match create_session_file_if_absent(
                    source_rollout,
                    &target_path,
                    thread_id,
                    Some(provider_id),
                    &source.version,
                ) {
                    Ok(true) => {
                        let (successor_marker, marker_bytes) = match write_provider_slot_marker(
                            target_root,
                            &target_path,
                            target_anchor.is_file().then_some(target_anchor),
                            thread_id,
                            Some(provider_id),
                        ) {
                            Ok(bytes) => bytes,
                            Err(error) => {
                                let _ = fs::remove_file(&target_path);
                                return Err(error);
                            }
                        };
                        let successor_version =
                            read_stable_source(&target_path, thread_id, |_, _, _| Ok(()))?.version;
                        let created_bytes = fs::metadata(&target_path)
                            .map_err(|error| {
                                format!("failed to inspect provider session JSONL: {error}")
                            })?
                            .len()
                            .checked_add(marker_bytes)
                            .ok_or_else(|| {
                                "provider session storage accounting overflowed".to_string()
                            })?;
                        let mut copy = rollout_copy(&target_path, true, created_bytes, &source);
                        copy.obsolete_provider_slot = supersedes.map(|obsolete| {
                            let obsolete = *obsolete;
                            ObsoleteProviderSlot {
                                obsolete: obsolete.path,
                                successor: target_path.clone(),
                                obsolete_version: obsolete.version,
                                obsolete_marker: obsolete.marker,
                                successor_version,
                                successor_marker,
                            }
                        });
                        return Ok(copy);
                    }
                    Ok(false) => {
                        let rechecked = plan_provider_rollout_from_source(
                            target_root,
                            source_rollout,
                            target_anchor,
                            thread_id,
                            provider_id,
                        )?;
                        if rechecked.action.target_path() == target_path
                            && !rechecked.action.writes_session_file()
                            && rechecked.source.version == source.version
                        {
                            return Ok(rollout_copy(&target_path, false, 0, &rechecked.source));
                        }
                        last_source_change = Some(source_changed(
                            "the provider rollout destination changed during no-clobber publish",
                        ));
                    }
                    Err(error) if is_source_changed(&error) => {
                        last_source_change = Some(error);
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        if attempt + 1 < SOURCE_STABILITY_ATTEMPTS {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    Err(last_source_change.unwrap_or_else(|| {
        "provider-normalized session JSONL kept changing during no-clobber publish".to_string()
    }))
}

fn create_session_file_if_absent(
    source: &Path,
    target: &Path,
    expected_id: &str,
    provider_id: Option<&str>,
    expected_version: &StableSourceVersion,
) -> Result<bool, String> {
    atomic_create(target, |output| {
        write_session_file(source, output, expected_id, provider_id, expected_version)
    })
}

fn plan_rollout_file(
    target_root: &SyncRoot,
    thread: &SourceThread,
) -> Result<RolloutFileAction, String> {
    let relative = rollout_relative_path(&thread.session_file);
    let mut target_path = target_root.root.join(relative);
    if !target_path.exists() {
        return Ok(RolloutFileAction::Create(target_path));
    }
    match session_file_relation(&thread.session_file, &target_path)? {
        SessionFileRelation::Equal | SessionFileRelation::RightExtendsLeft => {
            Ok(RolloutFileAction::Unchanged(target_path))
        }
        SessionFileRelation::LeftExtendsRight | SessionFileRelation::Divergent => {
            let source_hash = sha256_file(&thread.session_file)?;
            set_imported_file_name(&mut target_path, &source_hash);
            Ok(RolloutFileAction::Import(target_path))
        }
    }
}

fn plan_stable_rollout_file(
    target_root: &SyncRoot,
    thread: &SourceThread,
    publish_extensions: bool,
) -> Result<StableRolloutPlan, String> {
    let mut last_change = None;
    for attempt in 0..SOURCE_STABILITY_ATTEMPTS {
        match plan_stable_rollout_file_once(target_root, thread, publish_extensions) {
            Ok(plan) => return Ok(plan),
            Err(error) if is_source_changed(&error) => {
                last_change = Some(error);
                if attempt + 1 < SOURCE_STABILITY_ATTEMPTS {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_change
        .unwrap_or_else(|| source_changed("the file did not become stable within the retry limit")))
}

fn plan_stable_rollout_file_once(
    target_root: &SyncRoot,
    thread: &SourceThread,
    publish_extensions: bool,
) -> Result<StableRolloutPlan, String> {
    let source = read_stable_source(&thread.session_file, &thread.id, |_, _, _| Ok(()))?;
    let relative = rollout_relative_path(&thread.session_file);
    let mut target_path = target_root.root.join(relative);
    let mut marker_origin = None;
    let mut supersedes = None;
    let action = if !target_path.exists() {
        RolloutFileAction::Create(target_path)
    } else {
        match stable_source_relation_to_target(&source, &target_path)? {
            SessionFileRelation::Equal | SessionFileRelation::RightExtendsLeft => {
                RolloutFileAction::Unchanged(target_path)
            }
            SessionFileRelation::LeftExtendsRight if !publish_extensions => {
                RolloutFileAction::Unchanged(target_path)
            }
            SessionFileRelation::LeftExtendsRight => {
                marker_origin = Some(target_path.clone());
                supersedes = owned_provider_slot(target_root, &target_path, &thread.id)?;
                set_imported_file_name(&mut target_path, &source.version.sha256);
                RolloutFileAction::Import(target_path)
            }
            SessionFileRelation::Divergent => {
                marker_origin = Some(target_path.clone());
                set_imported_file_name(&mut target_path, &source.version.sha256);
                RolloutFileAction::Import(target_path)
            }
        }
    };
    Ok(StableRolloutPlan {
        action,
        source,
        marker_origin,
        supersedes,
    })
}

fn set_imported_file_name(target_path: &mut PathBuf, source_hash: &[u8]) {
    let stem = target_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("rollout")
        .to_string();
    let stem = imported_base_stem(&stem);
    let hash_suffix = source_hash
        .iter()
        .take(6)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    target_path.set_file_name(format!("{stem}-imported-{hash_suffix}.jsonl"));
}

fn plan_provider_rollout_from_source(
    target_root: &SyncRoot,
    source_rollout: &Path,
    target_anchor: &Path,
    thread_id: &str,
    provider_id: &str,
) -> Result<StableProviderRolloutPlan, String> {
    validate_remote_thread_id(thread_id)?;
    let source = read_stable_source(source_rollout, thread_id, |_, _, _| Ok(()))?;
    let provider_matches = source.version.meta.model_provider.as_deref() == Some(provider_id);
    if provider_matches
        && is_remote_rollout_path(source_rollout, thread_id)
        && validate_contained_session_file(target_root, source_rollout, thread_id).is_ok()
    {
        return Ok(StableProviderRolloutPlan {
            action: ProviderRolloutAction::Unchanged(source_rollout.to_path_buf()),
            source,
        });
    }
    if target_anchor.is_file() {
        validate_contained_session_file(target_root, target_anchor, thread_id)?;
    } else {
        let parent = target_anchor
            .parent()
            .ok_or_else(|| "provider rollout path must have a parent".to_string())?;
        validate_provider_destination_parent(target_root, parent)?;
    }
    let action = provider_rollout_variant(target_root, target_anchor, &source, provider_id)?;
    Ok(StableProviderRolloutPlan { action, source })
}

fn provider_rollout_variant(
    target_root: &SyncRoot,
    selected_rollout: &Path,
    source: &StableSourceData,
    provider_id: &str,
) -> Result<ProviderRolloutAction, String> {
    let date = rollout_date(selected_rollout).ok_or_else(|| {
        format!(
            "session rollout path does not contain a valid date: {}",
            selected_rollout.display()
        )
    })?;
    let parent = selected_rollout
        .parent()
        .ok_or_else(|| "session rollout path must have a parent".to_string())?;
    validate_provider_destination_parent(target_root, parent)?;

    if let Some(marker) =
        read_provider_slot_marker(target_root, selected_rollout, &source.version.meta.id)?
    {
        if marker.origin_provider.as_deref() == Some(provider_id) {
            if let Some(origin) = provider_marker_origin_path(target_root, &marker)? {
                if origin != selected_rollout && origin.is_file() {
                    let canonical_origin = validate_contained_session_file(
                        target_root,
                        &origin,
                        &source.version.meta.id,
                    )?;
                    let provider_matches = session_file_meta(&canonical_origin)?
                        .and_then(|meta| meta.model_provider)
                        .as_deref()
                        == Some(provider_id);
                    if provider_matches
                        && matches!(
                            stable_source_relation_to_target(source, &canonical_origin)?,
                            SessionFileRelation::Equal | SessionFileRelation::RightExtendsLeft
                        )
                    {
                        return Ok(ProviderRolloutAction::Unchanged(origin));
                    }
                }
            }
        }
    }

    let mut supersedes = None;
    let mut first_free = None;
    for sequence in 0_u32..PROVIDER_SLOT_ALLOCATION_ATTEMPTS {
        let candidate = provider_candidate_path(
            parent,
            &date,
            &source.version.meta.id,
            provider_id,
            sequence,
        );
        validate_provider_destination_parent(
            target_root,
            candidate
                .parent()
                .ok_or_else(|| "provider rollout path must have a parent".to_string())?,
        )?;
        if candidate == selected_rollout {
            continue;
        }
        if !candidate.exists() {
            if !provider_slot_marker_path(&candidate)?.exists() && first_free.is_none() {
                first_free = Some(candidate);
            }
            continue;
        }
        let canonical_candidate =
            validate_contained_session_file(target_root, &candidate, &source.version.meta.id)?;
        let provider_matches = session_file_meta(&canonical_candidate)?
            .and_then(|meta| meta.model_provider)
            .as_deref()
            == Some(provider_id);
        if !provider_matches {
            continue;
        }
        match stable_source_relation_to_target(source, &canonical_candidate)? {
            SessionFileRelation::Equal | SessionFileRelation::RightExtendsLeft => {
                return Ok(ProviderRolloutAction::Unchanged(candidate));
            }
            SessionFileRelation::LeftExtendsRight => {
                if let Some(owned) =
                    owned_provider_slot(target_root, &canonical_candidate, &source.version.meta.id)?
                {
                    supersedes.get_or_insert(Box::new(OwnedProviderSlot {
                        path: candidate,
                        ..owned
                    }));
                }
            }
            SessionFileRelation::Divergent => {}
        }
    }
    if let Some(path) = first_free {
        return Ok(ProviderRolloutAction::Create { path, supersedes });
    }
    Err(format!(
        "failed to allocate a provider-normalized session JSONL beside {}",
        selected_rollout.display()
    ))
}

fn provider_candidate_path(
    parent: &Path,
    date: &str,
    thread_id: &str,
    provider_id: &str,
    sequence: u32,
) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(thread_id.as_bytes());
    hasher.update(provider_id.as_bytes());
    hasher.update(sequence.to_le_bytes());
    let hash = hasher.finalize();
    let seconds = u32::from_le_bytes([hash[0], hash[1], hash[2], hash[3]]) % 86_400;
    let hour = seconds / 3_600;
    let minute = (seconds % 3_600) / 60;
    let second = seconds % 60;
    parent.join(format!(
        "rollout-{date}T{hour:02}-{minute:02}-{second:02}-{thread_id}.jsonl"
    ))
}

fn provider_slot_marker_path(slot: &Path) -> Result<PathBuf, String> {
    let file_name = slot
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "provider rollout filename is not valid UTF-8".to_string())?;
    Ok(slot.with_file_name(format!(
        ".{file_name}.codex-switch-slot-v{PROVIDER_SLOT_MARKER_VERSION}.json"
    )))
}

fn write_provider_slot_marker(
    target_root: &SyncRoot,
    slot: &Path,
    origin: Option<&Path>,
    thread_id: &str,
    provider_id: Option<&str>,
) -> Result<(ProviderSlotMarker, u64), String> {
    let slot = validate_contained_session_file(target_root, slot, thread_id)?;
    let sessions_root = fs::canonicalize(&target_root.sessions_dir)
        .map_err(|error| format!("failed to resolve target sessions directory: {error}"))?;
    let (origin_relative_path, origin_provider) = match origin {
        Some(origin) => {
            let origin = validate_contained_session_file(target_root, origin, thread_id)?;
            let relative = origin
                .strip_prefix(&sessions_root)
                .map_err(|_| {
                    "provider rollout origin is outside the managed sessions root".to_string()
                })?
                .to_path_buf();
            validate_provider_marker_relative_path(&relative)?;
            (
                Some(relative),
                session_file_meta(&origin)?.and_then(|meta| meta.model_provider),
            )
        }
        None => (None, None),
    };
    let created_bytes = fs::metadata(&slot)
        .map_err(|error| format!("failed to inspect provider session JSONL: {error}"))?
        .len();
    let marker = ProviderSlotMarker {
        version: PROVIDER_SLOT_MARKER_VERSION,
        thread_id: thread_id.to_string(),
        provider_id: provider_id.map(ToOwned::to_owned),
        slot_file_name: slot
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| "provider rollout filename is not valid UTF-8".to_string())?
            .to_string(),
        origin_relative_path,
        origin_provider,
        created_bytes,
        created_sha256: encode_sha256(&sha256_file(&slot)?),
    };
    let encoded = serde_json::to_vec(&marker)
        .map_err(|error| format!("failed to serialize provider slot marker: {error}"))?;
    let encoded_len = u64::try_from(encoded.len())
        .map_err(|_| "provider slot marker is too large".to_string())?;
    if encoded_len > PROVIDER_SLOT_MARKER_MAX_BYTES {
        return Err("provider slot marker is too large".to_string());
    }
    let marker_path = provider_slot_marker_path(&slot)?;
    let created = atomic_create(&marker_path, |output| {
        output
            .write_all(&encoded)
            .map_err(|error| format!("failed to write provider slot marker: {error}"))?;
        output
            .sync_all()
            .map_err(|error| format!("failed to persist provider slot marker: {error}"))
    })?;
    if !created {
        return Err("provider slot marker destination already exists".to_string());
    }
    Ok((marker, encoded_len))
}

fn read_provider_slot_marker(
    target_root: &SyncRoot,
    slot: &Path,
    expected_thread_id: &str,
) -> Result<Option<ProviderSlotMarker>, String> {
    if !slot.is_file() {
        return Ok(None);
    }
    let slot = validate_contained_session_file(target_root, slot, expected_thread_id)?;
    let marker_path = provider_slot_marker_path(&slot)?;
    let metadata = match fs::metadata(&marker_path) {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => return Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "failed to inspect provider slot marker {}: {error}",
                marker_path.display()
            ));
        }
    };
    if metadata.len() == 0 || metadata.len() > PROVIDER_SLOT_MARKER_MAX_BYTES {
        return Ok(None);
    }
    let encoded = fs::read(&marker_path).map_err(|error| {
        format!(
            "failed to read provider slot marker {}: {error}",
            marker_path.display()
        )
    })?;
    let marker = match serde_json::from_slice::<ProviderSlotMarker>(&encoded) {
        Ok(marker) => marker,
        Err(_) => return Ok(None),
    };
    let slot_file_name = slot.file_name().and_then(|name| name.to_str());
    if marker.version != PROVIDER_SLOT_MARKER_VERSION
        || marker.thread_id != expected_thread_id
        || slot_file_name != Some(marker.slot_file_name.as_str())
        || marker
            .provider_id
            .as_deref()
            .is_some_and(|provider| provider.trim().is_empty())
        || marker.created_bytes == 0
        || marker
            .origin_relative_path
            .as_deref()
            .is_some_and(|path| validate_provider_marker_relative_path(path).is_err())
    {
        return Ok(None);
    }
    let slot_metadata = fs::metadata(&slot)
        .map_err(|error| format!("failed to inspect provider session JSONL: {error}"))?;
    if slot_metadata.len() < marker.created_bytes {
        return Ok(None);
    }
    let current_prefix = sha256_file_prefix(&slot, marker.created_bytes)?;
    if encode_sha256(&current_prefix) != marker.created_sha256 {
        return Ok(None);
    }
    let slot_provider = session_file_meta(&slot)?.and_then(|meta| meta.model_provider);
    if slot_provider.as_deref() != marker.provider_id.as_deref() {
        return Ok(None);
    }
    Ok(Some(marker))
}

fn owned_provider_slot(
    target_root: &SyncRoot,
    slot: &Path,
    expected_thread_id: &str,
) -> Result<Option<OwnedProviderSlot>, String> {
    let Some(marker) = read_provider_slot_marker(target_root, slot, expected_thread_id)? else {
        return Ok(None);
    };
    let version = read_stable_source(slot, expected_thread_id, |_, _, _| Ok(()))?.version;
    Ok(Some(OwnedProviderSlot {
        path: slot.to_path_buf(),
        version,
        marker,
    }))
}

fn provider_marker_origin_path(
    target_root: &SyncRoot,
    marker: &ProviderSlotMarker,
) -> Result<Option<PathBuf>, String> {
    let Some(relative) = marker.origin_relative_path.as_deref() else {
        return Ok(None);
    };
    validate_provider_marker_relative_path(relative)?;
    Ok(Some(target_root.sessions_dir.join(relative)))
}

fn validate_provider_marker_relative_path(relative: &Path) -> Result<(), String> {
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err("provider slot marker contains an unsafe origin path".to_string());
    }
    Ok(())
}

fn sha256_file_prefix(path: &Path, length: u64) -> Result<Vec<u8>, String> {
    let file =
        fs::File::open(path).map_err(|error| format!("failed to open file for hash: {error}"))?;
    let mut reader = file.take(length);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| format!("failed to read file for hash: {error}"))?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .ok_or_else(|| "provider slot marker hash length overflowed".to_string())?;
        hasher.update(&buffer[..read]);
    }
    if total != length {
        return Err("provider slot marker source is shorter than expected".to_string());
    }
    Ok(hasher.finalize().to_vec())
}

fn encode_sha256(digest: &[u8]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
fn provider_output_len(
    source_path: &Path,
    expected_id: &str,
    provider_id: &str,
    expected_version: &StableSourceVersion,
) -> Result<u64, String> {
    let mut output_bytes = 0_u64;
    let observed = read_stable_source(source_path, expected_id, |raw, body, value| {
        let line_bytes = if value.get("type").and_then(JsonValue::as_str) != Some("session_meta") {
            u64::try_from(raw.len())
                .map_err(|_| "provider rollout capacity calculation overflowed".to_string())?
        } else {
            let mut value = value.clone();
            let payload = value
                .get_mut("payload")
                .and_then(JsonValue::as_object_mut)
                .ok_or_else(|| "session_meta payload must be an object".to_string())?;
            payload.insert(
                "model_provider".to_string(),
                JsonValue::String(provider_id.to_string()),
            );
            let rewritten = serde_json::to_vec(&value)
                .map_err(|error| format!("failed to serialize session metadata: {error}"))?;
            let ending = raw
                .len()
                .checked_sub(body.len())
                .ok_or_else(|| "provider rollout capacity calculation overflowed".to_string())?;
            u64::try_from(rewritten.len())
                .ok()
                .and_then(|value| {
                    u64::try_from(ending)
                        .ok()
                        .and_then(|ending| value.checked_add(ending))
                })
                .ok_or_else(|| "provider rollout capacity calculation overflowed".to_string())?
        };
        output_bytes = output_bytes
            .checked_add(line_bytes)
            .ok_or_else(|| "provider rollout capacity calculation overflowed".to_string())?;
        Ok(())
    })?;
    if observed.version != *expected_version {
        return Err(source_changed(
            "the provider rollout capacity source changed while it was measured",
        ));
    }
    Ok(output_bytes)
}

fn validate_remote_thread_id(thread_id: &str) -> Result<(), String> {
    let path = Path::new(thread_id);
    if thread_id.len() != 36
        || path.file_name().and_then(|value| value.to_str()) != Some(thread_id)
        || path.components().count() != 1
        || !path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
        || !thread_id
            .bytes()
            .enumerate()
            .all(|(index, byte)| match index {
                8 | 13 | 18 | 23 => byte == b'-',
                _ => byte.is_ascii_hexdigit(),
            })
    {
        return Err("session id is not a safe UUID filename component".to_string());
    }
    Ok(())
}

fn validate_contained_session_file(
    target_root: &SyncRoot,
    candidate: &Path,
    expected_id: &str,
) -> Result<PathBuf, String> {
    let sessions_root = fs::canonicalize(&target_root.sessions_dir)
        .map_err(|error| format!("failed to resolve target sessions directory: {error}"))?;
    let candidate = fs::canonicalize(candidate)
        .map_err(|error| format!("failed to resolve selected session JSONL: {error}"))?;
    if !candidate.starts_with(&sessions_root) || !candidate.is_file() {
        return Err("selected session JSONL is outside the managed sessions root".to_string());
    }
    if session_file_meta(&candidate)?.is_none_or(|meta| meta.id != expected_id) {
        return Err("selected session JSONL does not match the expected session id".to_string());
    }
    Ok(candidate)
}

fn validate_provider_destination_parent(
    target_root: &SyncRoot,
    parent: &Path,
) -> Result<(), String> {
    let sessions_root = fs::canonicalize(&target_root.sessions_dir)
        .map_err(|error| format!("failed to resolve target sessions directory: {error}"))?;
    let relative = parent
        .strip_prefix(&target_root.sessions_dir)
        .or_else(|_| parent.strip_prefix(&sessions_root))
        .map_err(|_| {
            "provider rollout destination is outside the managed sessions root".to_string()
        })?;
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err(
            "provider rollout destination is outside the managed sessions root".to_string(),
        );
    }
    let mut existing = parent;
    loop {
        match fs::canonicalize(existing) {
            Ok(resolved) => {
                if !resolved.starts_with(&sessions_root) || !resolved.is_dir() {
                    return Err(
                        "provider rollout destination is outside the managed sessions root"
                            .to_string(),
                    );
                }
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                existing = existing.parent().ok_or_else(|| {
                    "provider rollout destination is outside the managed sessions root".to_string()
                })?;
            }
            Err(error) => {
                return Err(format!(
                    "failed to resolve provider rollout directory: {error}"
                ));
            }
        }
    }
    Ok(())
}

fn rollout_date(path: &Path) -> Option<String> {
    let file_name = path.file_name()?.to_str()?;
    file_name
        .strip_prefix("rollout-")
        .and_then(|name| name.get(..10))
        .filter(|date| valid_rollout_date(date))
        .map(ToOwned::to_owned)
        .or_else(|| rollout_date_from_parent(path))
}

fn rollout_date_from_parent(path: &Path) -> Option<String> {
    let day = path.parent()?.file_name()?.to_str()?;
    let month = path.parent()?.parent()?.file_name()?.to_str()?;
    let year = path.parent()?.parent()?.parent()?.file_name()?.to_str()?;
    let date = format!("{year}-{month}-{day}");
    valid_rollout_date(&date).then_some(date)
}

fn is_remote_rollout_path(path: &Path, thread_id: &str) -> bool {
    let file_name = path.file_name().and_then(|name| name.to_str());
    let timestamp = file_name
        .and_then(|name| name.strip_prefix("rollout-"))
        .and_then(|name| name.strip_suffix(&format!("-{thread_id}.jsonl")));
    timestamp.is_some_and(valid_rollout_timestamp)
}

fn valid_rollout_date(date: &str) -> bool {
    if date.len() != 10
        || !date.bytes().enumerate().all(|(index, byte)| match index {
            4 | 7 => byte == b'-',
            _ => byte.is_ascii_digit(),
        })
    {
        return false;
    }
    let month = date[5..7].parse::<u8>().ok();
    let day = date[8..10].parse::<u8>().ok();
    matches!(month, Some(1..=12)) && matches!(day, Some(1..=31))
}

fn valid_rollout_timestamp(timestamp: &str) -> bool {
    if timestamp.len() != 19
        || !timestamp
            .bytes()
            .enumerate()
            .all(|(index, byte)| match index {
                4 | 7 | 13 | 16 => byte == b'-',
                10 => byte == b'T',
                _ => byte.is_ascii_digit(),
            })
    {
        return false;
    }
    let hour = timestamp[11..13].parse::<u8>().ok();
    let minute = timestamp[14..16].parse::<u8>().ok();
    let second = timestamp[17..19].parse::<u8>().ok();
    valid_rollout_date(&timestamp[..10])
        && matches!(hour, Some(0..=23))
        && matches!(minute, Some(0..=59))
        && matches!(second, Some(0..=59))
}

fn stable_source_relation_to_target(
    source: &StableSourceData,
    target: &Path,
) -> Result<SessionFileRelation, String> {
    let target = fs::File::open(target)
        .map_err(|error| format!("failed to open target session candidate: {error}"))?;
    stable_source_relation_to_reader(source, &mut BufReader::new(target))
        .map(|(relation, _)| relation)
}

fn stable_source_relation_to_reader(
    source: &StableSourceData,
    target: &mut impl BufRead,
) -> Result<(SessionFileRelation, usize), String> {
    let mut target_lines = 0_usize;
    loop {
        let Some(target_line) = read_normalized_session_line(target)? else {
            let relation = if target_lines == source.normalized_line_hashes.len() {
                SessionFileRelation::Equal
            } else {
                SessionFileRelation::LeftExtendsRight
            };
            return Ok((relation, target_lines));
        };
        if target_lines >= source.normalized_line_hashes.len() {
            return Ok((SessionFileRelation::RightExtendsLeft, target_lines + 1));
        }
        if sha256_bytes(&target_line) != source.normalized_line_hashes[target_lines] {
            return Ok((SessionFileRelation::Divergent, target_lines + 1));
        }
        target_lines += 1;
    }
}

fn read_stable_source<F>(
    source_path: &Path,
    expected_id: &str,
    mut on_line: F,
) -> Result<StableSourceData, String>
where
    F: FnMut(&[u8], &[u8], &JsonValue) -> Result<(), String>,
{
    let source = fs::File::open(source_path)
        .map_err(|error| source_changed(format!("failed to open the planned file: {error}")))?;
    let before_handle = source
        .metadata()
        .map_err(|error| source_changed(format!("failed to inspect the planned file: {error}")))?;
    let before_path = fs::metadata(source_path)
        .map_err(|error| source_changed(format!("failed to inspect the planned path: {error}")))?;
    if !before_handle.is_file() || file_stamp(&before_handle) != file_stamp(&before_path) {
        return Err(source_changed(
            "the planned path changed before it was read",
        ));
    }
    let before_stamp = file_stamp(&before_handle);
    let mut source = BufReader::new(source);

    let mut raw_hash = Sha256::new();
    let mut normalized_line_hashes = Vec::new();
    let mut meta = None;
    let mut total_length = 0_u64;
    let mut raw = Vec::new();
    loop {
        raw.clear();
        let read = source
            .read_until(b'\n', &mut raw)
            .map_err(|error| source_changed(format!("failed while reading the file: {error}")))?;
        if read == 0 {
            break;
        }
        total_length = total_length
            .checked_add(read as u64)
            .ok_or_else(|| source_changed("the file length overflowed"))?;
        raw_hash.update(&raw);
        let body = trim_jsonl_ending(&raw);
        if body.is_empty() {
            return Err(source_changed("the file contains an empty JSONL entry"));
        }
        let value = serde_json::from_slice::<JsonValue>(body)
            .map_err(|_| source_changed("the file has an incomplete or invalid JSONL tail"))?;
        if meta.is_none() && value.get("type").and_then(JsonValue::as_str) == Some("session_meta") {
            let parsed = session_meta_from_value(&value).ok_or_else(|| {
                source_changed("the file contains session_meta without a valid id")
            })?;
            if parsed.id != expected_id {
                return Err(format!(
                    "source session JSONL id changed from {expected_id} to {}",
                    parsed.id
                ));
            }
            meta = Some(parsed);
        }
        normalized_line_hashes.push(sha256_bytes(&normalized_session_line(&value, body)?));
        on_line(&raw, body, &value)?;
    }

    let after_handle = source
        .get_ref()
        .metadata()
        .map_err(|error| source_changed(format!("failed to recheck the planned file: {error}")))?;
    let after_path = fs::metadata(source_path)
        .map_err(|error| source_changed(format!("failed to recheck the planned path: {error}")))?;
    if file_stamp(&after_handle) != before_stamp
        || file_stamp(&after_path) != before_stamp
        || total_length != before_stamp.length
    {
        return Err(source_changed("the file changed while it was being read"));
    }
    let meta = meta
        .ok_or_else(|| source_changed("the file does not contain a complete session_meta entry"))?;
    Ok(StableSourceData {
        version: StableSourceVersion {
            length: total_length,
            sha256: raw_hash.finalize().to_vec(),
            meta,
        },
        normalized_line_hashes,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileStamp {
    length: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
}

fn file_stamp(metadata: &fs::Metadata) -> FileStamp {
    FileStamp {
        length: metadata.len(),
        modified: metadata.modified().ok(),
        created: metadata.created().ok(),
    }
}

fn trim_jsonl_ending(mut line: &[u8]) -> &[u8] {
    while line
        .last()
        .is_some_and(|byte| matches!(byte, b'\r' | b'\n'))
    {
        line = &line[..line.len() - 1];
    }
    line
}

fn normalized_session_line(value: &JsonValue, original: &[u8]) -> Result<Vec<u8>, String> {
    if value.get("type").and_then(JsonValue::as_str) != Some("session_meta") {
        return Ok(original.to_vec());
    }
    let mut value = value.clone();
    if let Some(payload) = value.get_mut("payload").and_then(JsonValue::as_object_mut) {
        payload.remove("model_provider");
    }
    serde_json::to_vec(&value)
        .map_err(|error| format!("failed to normalize session metadata: {error}"))
}

fn sha256_bytes(bytes: &[u8]) -> Vec<u8> {
    Sha256::digest(bytes).to_vec()
}

fn source_changed(detail: impl AsRef<str>) -> String {
    format!("{SOURCE_CHANGED_PREFIX}: {}", detail.as_ref())
}

fn is_source_changed(error: &str) -> bool {
    error.starts_with(SOURCE_CHANGED_PREFIX)
}

fn imported_base_stem(stem: &str) -> &str {
    let mut base = stem;
    loop {
        if let Some(stripped) = base.strip_suffix("-imported") {
            base = stripped;
            continue;
        }
        let Some((candidate, suffix)) = base.rsplit_once("-imported-") else {
            break;
        };
        if suffix.len() == 12 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            base = candidate;
            continue;
        }
        break;
    }
    base
}

pub(crate) fn session_file_relation(
    left: &Path,
    right: &Path,
) -> Result<SessionFileRelation, String> {
    let left = fs::File::open(left)
        .map_err(|error| format!("failed to open left session candidate: {error}"))?;
    let right = fs::File::open(right)
        .map_err(|error| format!("failed to open right session candidate: {error}"))?;
    let mut left = BufReader::new(left);
    let mut right = BufReader::new(right);
    loop {
        let left_line = read_normalized_session_line(&mut left)?;
        let right_line = read_normalized_session_line(&mut right)?;
        match (left_line, right_line) {
            (None, None) => return Ok(SessionFileRelation::Equal),
            (Some(_), None) => return Ok(SessionFileRelation::LeftExtendsRight),
            (None, Some(_)) => return Ok(SessionFileRelation::RightExtendsLeft),
            (Some(left_line), Some(right_line)) if left_line != right_line => {
                return Ok(SessionFileRelation::Divergent);
            }
            _ => {}
        }
    }
}

fn read_normalized_session_line(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>, String> {
    let mut line = Vec::new();
    let read = reader
        .read_until(b'\n', &mut line)
        .map_err(|error| format!("failed to read session candidate: {error}"))?;
    if read == 0 {
        return Ok(None);
    }
    while line
        .last()
        .is_some_and(|byte| matches!(byte, b'\r' | b'\n'))
    {
        line.pop();
    }
    let Ok(mut value) = serde_json::from_slice::<JsonValue>(&line) else {
        return Ok(Some(line));
    };
    if value.get("type").and_then(JsonValue::as_str) == Some("session_meta") {
        if let Some(payload) = value.get_mut("payload").and_then(JsonValue::as_object_mut) {
            payload.remove("model_provider");
        }
        return serde_json::to_vec(&value)
            .map(Some)
            .map_err(|error| format!("failed to normalize session metadata: {error}"));
    }
    Ok(Some(line))
}

fn rollout_relative_path(source_path: &Path) -> PathBuf {
    relative_from_sessions(source_path).unwrap_or_else(|| {
        PathBuf::from("sessions").join(source_path.file_name().unwrap_or_default())
    })
}

fn relative_from_sessions(path: &Path) -> Option<PathBuf> {
    let mut relative = PathBuf::new();
    let mut found = false;
    for component in path.components() {
        let text = component.as_os_str().to_string_lossy();
        if found || text.eq_ignore_ascii_case("sessions") {
            relative.push(component.as_os_str());
            found = true;
        }
    }
    found.then_some(relative)
}

fn session_file_meta(path: &Path) -> Result<Option<SessionMeta>, String> {
    let file =
        fs::File::open(path).map_err(|error| format!("failed to open session jsonl: {error}"))?;
    for line in BufReader::new(file).lines().take(25) {
        let line = line.map_err(|error| format!("failed to read session jsonl: {error}"))?;
        let Ok(value) = serde_json::from_str::<JsonValue>(&line) else {
            continue;
        };
        if let Some(meta) = session_meta_from_value(&value) {
            return Ok(Some(meta));
        }
    }
    Ok(None)
}

fn session_meta_from_value(value: &JsonValue) -> Option<SessionMeta> {
    if value.get("type").and_then(JsonValue::as_str) != Some("session_meta") {
        return None;
    }
    let payload = value.get("payload")?;
    let id = payload.get("id").and_then(JsonValue::as_str)?;
    Some(SessionMeta {
        id: id.to_string(),
        model_provider: payload
            .get("model_provider")
            .and_then(JsonValue::as_str)
            .map(ToOwned::to_owned),
        source: payload
            .get("source")
            .and_then(JsonValue::as_str)
            .map(ToOwned::to_owned),
        cwd: payload
            .get("cwd")
            .and_then(JsonValue::as_str)
            .map(ToOwned::to_owned),
        cli_version: payload
            .get("cli_version")
            .and_then(JsonValue::as_str)
            .map(ToOwned::to_owned),
        timestamp_millis: None,
    })
}

fn write_session_file(
    source_path: &Path,
    output: &mut fs::File,
    expected_id: &str,
    provider_id: Option<&str>,
    expected_version: &StableSourceVersion,
) -> Result<(), String> {
    let mut authoritative_meta_written = false;
    let observed = read_stable_source(source_path, expected_id, |raw, body, value| {
        let Some(provider_id) = provider_id else {
            return output
                .write_all(raw)
                .map_err(|error| format!("failed to copy session JSONL: {error}"));
        };
        if authoritative_meta_written
            || value.get("type").and_then(JsonValue::as_str) != Some("session_meta")
        {
            return output
                .write_all(raw)
                .map_err(|error| format!("failed to copy session JSONL: {error}"));
        }
        let mut value = value.clone();
        let payload = value
            .get_mut("payload")
            .and_then(JsonValue::as_object_mut)
            .ok_or_else(|| "session_meta payload must be an object".to_string())?;
        payload.insert(
            "model_provider".to_string(),
            JsonValue::String(provider_id.to_string()),
        );
        authoritative_meta_written = true;
        let rewritten = serde_json::to_vec(&value)
            .map_err(|error| format!("failed to serialize session metadata: {error}"))?;
        output
            .write_all(&rewritten)
            .map_err(|error| format!("failed to write session JSONL rewrite: {error}"))?;
        output
            .write_all(&raw[body.len()..])
            .map_err(|error| format!("failed to write session JSONL ending: {error}"))
    })?;
    if observed.version != *expected_version {
        return Err(source_changed(
            "the copied version no longer matches the planned version",
        ));
    }
    Ok(())
}

#[cfg(test)]
fn merge_session_index(
    source_root: &SyncRoot,
    target_root: &SyncRoot,
    allowlist: &HashSet<String>,
) -> Result<usize, String> {
    merge_session_index_with_policy(
        source_root,
        target_root,
        allowlist,
        SessionIndexWritePolicy::MergeAtomic,
    )
}

fn merge_session_index_with_policy(
    source_root: &SyncRoot,
    target_root: &SyncRoot,
    allowlist: &HashSet<String>,
    write_policy: SessionIndexWritePolicy,
) -> Result<usize, String> {
    if write_policy == SessionIndexWritePolicy::Skip {
        return Ok(0);
    }
    let source_exclusive = write_policy == SessionIndexWritePolicy::Deny;
    let Some(mut source) =
        open_existing_session_index(&source_root.session_index, source_exclusive, "source")?
    else {
        return Ok(0);
    };
    let source_lines = read_session_index_source_lines(&mut source, allowlist)?;

    if write_policy == SessionIndexWritePolicy::Deny {
        if source_lines.is_empty() {
            return Ok(0);
        }
        let Some(mut target) =
            open_existing_session_index(&target_root.session_index, true, "target")?
        else {
            return Err(
                "session index changed after fast-path planning; retry the session sync"
                    .to_string(),
            );
        };
        let plan = plan_session_index_merge_from_lines(&source_lines, Some(&mut target))?;
        if plan.lines.is_empty() {
            return Ok(0);
        }
        return Err(
            "session index changed after fast-path planning; retry the session sync".to_string(),
        );
    }

    if source_lines.is_empty() {
        return Ok(0);
    }
    let target_bytes = read_session_index_target_bytes(&target_root.session_index, false)?;
    let plan = plan_session_index_merge_from_bytes(&source_lines, &target_bytes)?;
    if plan.lines.is_empty() {
        return Ok(0);
    }
    let mut encoded = target_bytes;
    if plan.target_needs_newline {
        encoded.push(b'\n');
    }
    for line in &plan.lines {
        encoded.extend_from_slice(line.as_bytes());
        encoded.push(b'\n');
    }
    atomic_write(&target_root.session_index, &encoded)
        .map_err(|error| format!("failed to atomically replace session_index.jsonl: {error}"))?;
    Ok(plan.lines.len())
}

fn plan_session_index_merge(
    source_root: &SyncRoot,
    target_root: &SyncRoot,
    allowlist: &HashSet<String>,
) -> Result<SessionIndexMergePlan, String> {
    let Some(mut source) =
        open_existing_session_index(&source_root.session_index, false, "source")?
    else {
        return Ok(SessionIndexMergePlan {
            lines: Vec::new(),
            target_needs_newline: false,
        });
    };
    let source_lines = read_session_index_source_lines(&mut source, allowlist)?;
    let mut target = open_existing_session_index(&target_root.session_index, false, "target")?;
    plan_session_index_merge_from_lines(&source_lines, target.as_mut())
}

fn open_existing_session_index(
    path: &Path,
    exclusive: bool,
    role: &str,
) -> Result<Option<fs::File>, String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    if exclusive {
        options.share_mode(0);
    }
    match options.open(path) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            let action = if exclusive { "lock" } else { "open" };
            Err(format!(
                "failed to {action} {role} session_index.jsonl: {error}"
            ))
        }
    }
}

fn read_session_index_source_lines(
    source: &mut fs::File,
    allowlist: &HashSet<String>,
) -> Result<Vec<String>, String> {
    source
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("failed to rewind source session_index.jsonl: {error}"))?;
    let mut lines = Vec::new();
    for line in BufReader::new(source).lines() {
        let line =
            line.map_err(|error| format!("failed to read source session_index.jsonl: {error}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let Some(id) = session_index_line_id(&line)? else {
            continue;
        };
        if allowlist.contains(&id) {
            lines.push(line);
        }
    }
    Ok(lines)
}

fn plan_session_index_merge_from_lines(
    source_lines: &[String],
    target: Option<&mut fs::File>,
) -> Result<SessionIndexMergePlan, String> {
    let mut target_bytes = Vec::new();
    if let Some(target) = target {
        target
            .seek(SeekFrom::Start(0))
            .map_err(|error| format!("failed to rewind target session_index.jsonl: {error}"))?;
        target
            .read_to_end(&mut target_bytes)
            .map_err(|error| format!("failed to read target session_index.jsonl: {error}"))?;
    }
    plan_session_index_merge_from_bytes(source_lines, &target_bytes)
}

fn plan_session_index_merge_from_bytes(
    source_lines: &[String],
    target_bytes: &[u8],
) -> Result<SessionIndexMergePlan, String> {
    let mut seen = HashSet::new();
    for raw_line in target_bytes.split(|byte| *byte == b'\n') {
        let raw_line = raw_line.strip_suffix(b"\r").unwrap_or(raw_line);
        if raw_line.is_empty() {
            continue;
        }
        let line = std::str::from_utf8(raw_line).map_err(|_| {
            "target session_index.jsonl contains an incomplete or invalid entry".to_string()
        })?;
        if serde_json::from_str::<JsonValue>(line).is_err() {
            return Err(
                "target session_index.jsonl contains an incomplete or invalid entry".to_string(),
            );
        }
        seen.insert(line.to_string());
    }
    let mut missing_lines = Vec::new();
    for line in source_lines {
        if seen.insert(line.clone()) {
            missing_lines.push(line.clone());
        }
    }
    Ok(SessionIndexMergePlan {
        lines: missing_lines,
        target_needs_newline: !target_bytes.is_empty() && !target_bytes.ends_with(b"\n"),
    })
}

fn read_session_index_target_bytes(path: &Path, exclusive: bool) -> Result<Vec<u8>, String> {
    let Some(mut target) = open_existing_session_index(path, exclusive, "target")? else {
        return Ok(Vec::new());
    };
    let mut bytes = Vec::new();
    target
        .read_to_end(&mut bytes)
        .map_err(|error| format!("failed to read target session_index.jsonl: {error}"))?;
    Ok(bytes)
}

fn session_index_line_id(line: &str) -> Result<Option<String>, String> {
    let value = serde_json::from_str::<JsonValue>(line)
        .map_err(|_| "source session_index.jsonl contains an invalid entry".to_string())?;
    Ok(value
        .get("id")
        .or_else(|| value.get("session_id"))
        .or_else(|| value.get("sessionId"))
        .and_then(JsonValue::as_str)
        .map(ToOwned::to_owned))
}

fn copy_dependent_rows(
    source: &Connection,
    target: &Connection,
    candidate_ids: &HashSet<String>,
) -> Result<(), String> {
    if candidate_ids.is_empty() {
        return Ok(());
    }
    for (table, column) in [
        ("thread_dynamic_tools", "thread_id"),
        ("thread_goals", "thread_id"),
        ("thread_spawn_edges", "parent_thread_id"),
        ("thread_spawn_edges", "child_thread_id"),
    ] {
        if table_exists(source, table)? && table_exists(target, table)? {
            copy_table_rows(source, target, table, column, candidate_ids)?;
        }
    }
    Ok(())
}

fn copy_table_rows(
    source: &Connection,
    target: &Connection,
    table: &str,
    filter_column: &str,
    candidate_ids: &HashSet<String>,
) -> Result<(), String> {
    let source_columns = table_columns(source, table)?;
    let target_columns = table_columns(target, table)?;
    let columns = source_columns
        .into_iter()
        .filter(|column| target_columns.contains(column))
        .collect::<Vec<_>>();
    let Some(filter_index) = columns.iter().position(|column| column == filter_column) else {
        return Ok(());
    };
    let select = format!("SELECT {} FROM {table}", columns.join(", "));
    let mut statement = source
        .prepare(&select)
        .map_err(|error| format!("failed to prepare dependent row query: {error}"))?;
    let rows = statement
        .query_map([], |row| {
            let mut values = Vec::new();
            for index in 0..columns.len() {
                values.push(row.get::<usize, Value>(index)?);
            }
            Ok(values)
        })
        .map_err(|error| format!("failed to query dependent rows: {error}"))?;
    let placeholders = (0..columns.len())
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(", ");
    let insert = format!(
        "INSERT OR IGNORE INTO {table} ({}) VALUES ({})",
        columns.join(", "),
        placeholders
    );
    for row in rows {
        let values = row.map_err(|error| format!("failed to read dependent row: {error}"))?;
        let include =
            matches!(values.get(filter_index), Some(Value::Text(id)) if candidate_ids.contains(id));
        if include {
            target
                .execute(&insert, rusqlite::params_from_iter(values))
                .map_err(|error| format!("failed to insert dependent row: {error}"))?;
        }
    }
    Ok(())
}

fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>, String> {
    table_schema(conn, table).map(|schema| schema.into_iter().map(|column| column.name).collect())
}

fn table_schema(conn: &Connection, table: &str) -> Result<Vec<TableColumn>, String> {
    let mut statement = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|error| format!("failed to inspect table {table}: {error}"))?;
    let rows = statement
        .query_map([], |row| {
            Ok(TableColumn {
                name: row.get::<usize, String>(1)?,
                not_null: row.get::<usize, i64>(3)? != 0,
                default_value: row.get::<usize, Option<String>>(4)?,
            })
        })
        .map_err(|error| format!("failed to read table columns: {error}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("failed to collect table columns: {error}"))
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool, String> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |row| row.get(0),
        )
        .map_err(|error| format!("failed to check table existence: {error}"))?;
    Ok(count > 0)
}

fn sha256_file(path: &Path) -> Result<Vec<u8>, String> {
    let mut file =
        fs::File::open(path).map_err(|error| format!("failed to open file for hash: {error}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("failed to read file for hash: {error}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().to_vec())
}

pub(crate) fn cleanup_obsolete_provider_slots(
    candidates: &[ObsoleteProviderSlot],
    current: &CodexPaths,
    shared: &CodexPaths,
) -> ProviderSlotGcSummary {
    let current_root = root_from_paths(current.clone());
    let shared_root = root_from_paths(shared.clone());
    let mut summary = ProviderSlotGcSummary::default();
    let mut seen = HashSet::new();
    for candidate in candidates {
        let obsolete_key = candidate.obsolete.to_string_lossy().to_ascii_lowercase();
        if !seen.insert(obsolete_key) {
            continue;
        }
        let root = if validate_contained_session_file(
            &current_root,
            &candidate.obsolete,
            &candidate.obsolete_version.meta.id,
        )
        .is_ok()
        {
            &current_root
        } else {
            &shared_root
        };
        match cleanup_obsolete_provider_slot(candidate, root, current, shared) {
            Ok((bytes, warning)) => {
                summary.reclaimed_count += 1;
                summary.reclaimed_bytes = summary.reclaimed_bytes.saturating_add(bytes);
                if let Some(warning) = warning {
                    summary.warnings.push(warning);
                }
            }
            Err(error) => {
                summary.retained_count += 1;
                summary.warnings.push(error);
            }
        }
    }
    summary
}

fn cleanup_obsolete_provider_slot(
    candidate: &ObsoleteProviderSlot,
    root: &SyncRoot,
    current: &CodexPaths,
    shared: &CodexPaths,
) -> Result<(u64, Option<String>), String> {
    let obsolete = validate_contained_session_file(
        root,
        &candidate.obsolete,
        &candidate.obsolete_version.meta.id,
    )
    .map_err(|_| "旧会话槽位校验失败，已保留".to_string())?;
    let successor = validate_contained_session_file(
        root,
        &candidate.successor,
        &candidate.successor_version.meta.id,
    )
    .map_err(|_| "新会话槽位校验失败，旧槽位已保留".to_string())?;
    let obsolete_marker =
        read_provider_slot_marker(root, &obsolete, &candidate.obsolete_version.meta.id)
            .map_err(|_| "旧会话槽位来源标记读取失败，已保留".to_string())?
            .ok_or_else(|| "旧会话槽位缺少有效来源标记，已保留".to_string())?;
    if obsolete_marker != candidate.obsolete_marker {
        return Err("旧会话槽位来源标记已变化，已保留".to_string());
    }
    let successor_marker =
        read_provider_slot_marker(root, &successor, &candidate.successor_version.meta.id)
            .map_err(|_| "新会话槽位来源标记读取失败，旧槽位已保留".to_string())?
            .ok_or_else(|| "新会话槽位缺少有效来源标记，旧槽位已保留".to_string())?;
    if successor_marker != candidate.successor_marker {
        return Err("新会话槽位来源标记已变化，旧槽位已保留".to_string());
    }
    let obsolete_source =
        read_stable_source(&obsolete, &candidate.obsolete_version.meta.id, |_, _, _| {
            Ok(())
        })
        .map_err(|_| "旧会话槽位在清理前发生变化，已保留".to_string())?;
    if obsolete_source.version != candidate.obsolete_version {
        return Err("旧会话槽位在清理前发生变化，已保留".to_string());
    }
    let successor_source = read_stable_source(
        &successor,
        &candidate.successor_version.meta.id,
        |_, _, _| Ok(()),
    )
    .map_err(|_| "新会话槽位在清理前发生变化，旧槽位已保留".to_string())?;
    if successor_source.version != candidate.successor_version
        || !matches!(
            stable_source_relation_to_target(&successor_source, &obsolete)
                .map_err(|_| "会话槽位完整性比较失败，旧槽位已保留".to_string())?,
            SessionFileRelation::Equal | SessionFileRelation::LeftExtendsRight
        )
    {
        return Err("新会话槽位未完整包含旧槽位，旧槽位已保留".to_string());
    }
    if provider_marker_origin_path(root, &obsolete_marker)
        .map_err(|_| "旧会话槽位来源路径无效，已保留".to_string())?
        .is_some_and(|origin| paths_match(&origin, &obsolete))
    {
        return Err("旧会话槽位仍是原始会话来源，已保留".to_string());
    }
    if database_references_rollout(current, &obsolete)?
        || database_references_rollout(shared, &obsolete)?
    {
        return Err("旧会话槽位仍被会话数据库引用，已保留".to_string());
    }

    let marker_path = provider_slot_marker_path(&obsolete)
        .map_err(|_| "旧会话槽位来源标记路径无效，已保留".to_string())?;
    let marker_bytes = fs::metadata(&marker_path)
        .map_err(|_| "旧会话槽位来源标记无法复核，已保留".to_string())?
        .len();
    let rollout_bytes = fs::metadata(&obsolete)
        .map_err(|_| "旧会话槽位无法复核，已保留".to_string())?
        .len();
    fs::remove_file(&obsolete)
        .map_err(|error| format!("旧会话槽位文件无法删除，来源标记已保留：{error}"))?;
    match fs::remove_file(&marker_path) {
        Ok(()) => rollout_bytes
            .checked_add(marker_bytes)
            .map(|bytes| (bytes, None))
            .ok_or_else(|| "会话槽位回收计数溢出".to_string()),
        Err(error) => Ok((
            rollout_bytes,
            Some(format!(
                "旧会话槽位已删除，但来源标记清理失败并已保留：{error}"
            )),
        )),
    }
}

fn database_references_rollout(paths: &CodexPaths, candidate: &Path) -> Result<bool, String> {
    if !paths.state_db.is_file() {
        return Ok(false);
    }
    let conn = Connection::open_with_flags(&paths.state_db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|_| "无法复核会话数据库引用，旧槽位已保留".to_string())?;
    if !table_exists(&conn, "threads")
        .map_err(|_| "无法复核会话数据库引用，旧槽位已保留".to_string())?
        || !table_columns(&conn, "threads")
            .map_err(|_| "无法复核会话数据库引用，旧槽位已保留".to_string())?
            .iter()
            .any(|column| column == "rollout_path")
    {
        return Ok(false);
    }
    let mut statement = conn
        .prepare("SELECT rollout_path FROM threads WHERE rollout_path IS NOT NULL")
        .map_err(|_| "无法复核会话数据库引用，旧槽位已保留".to_string())?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|_| "无法复核会话数据库引用，旧槽位已保留".to_string())?;
    for stored in rows {
        let stored = stored.map_err(|_| "无法复核会话数据库引用，旧槽位已保留".to_string())?;
        let stored = PathBuf::from(stored);
        let stored = if stored.is_absolute() {
            stored
        } else {
            paths.codex_home.join(stored)
        };
        if paths_match(&stored, candidate) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn paths_match(left: &Path, right: &Path) -> bool {
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => {
            #[cfg(windows)]
            {
                left.to_string_lossy()
                    .eq_ignore_ascii_case(&right.to_string_lossy())
            }
            #[cfg(not(windows))]
            {
                left == right
            }
        }
        _ => {
            #[cfg(windows)]
            {
                left.to_string_lossy()
                    .eq_ignore_ascii_case(&right.to_string_lossy())
            }
            #[cfg(not(windows))]
            {
                left == right
            }
        }
    }
}

fn file_modified_millis(path: &Path) -> Option<i64> {
    fs::metadata(path)
        .ok()?
        .modified()
        .ok()
        .and_then(system_time_millis)
}

fn now_millis() -> i64 {
    system_time_millis(SystemTime::now()).unwrap_or(0)
}

fn system_time_millis(time: SystemTime) -> Option<i64> {
    let duration = time.duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(duration.as_millis()).ok()
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, fs, io::Write};

    use rusqlite::Connection;
    use tempfile::tempdir;

    use super::{
        copy_rollout_file, merge_session_index, merge_session_index_with_policy,
        normalize_selected_user_home_provider_with_paths, open_source_conn,
        plan_session_index_merge, read_source_threads, read_stable_source, root_from_paths,
        sha256_file, sync_selected_user_home_to_shared_with_paths, sync_sessions,
        sync_sessions_for_provider, sync_shared_to_user_home, sync_shared_to_user_home_hot,
        sync_shared_to_user_home_hot_with_paths, sync_shared_to_user_home_hot_with_policy,
        sync_shared_to_user_home_with_paths, sync_user_home_to_shared,
        sync_user_home_to_shared_with_paths, write_session_file, SessionFileWritePolicy,
        SessionIndexWritePolicy,
    };
    use crate::codex_paths::local_codex_paths;

    const REMOTE_THREAD_A: &str = "019f8ced-fc55-7a93-8cc5-a18d5b96b4a6";
    const REMOTE_THREAD_B: &str = "019f8ced-fc55-7a93-8cc5-a18d5b96b4a7";

    fn create_db(path: &std::path::Path, threads: &[(&str, &str)]) {
        let conn = Connection::open(path).unwrap();
        conn.execute(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, updated_at INTEGER, updated_at_ms INTEGER)",
            [],
        )
        .unwrap();
        for (id, rollout_path) in threads {
            conn.execute(
                "INSERT INTO threads (id, rollout_path, updated_at, updated_at_ms) VALUES (?1, ?2, 1, 1000)",
                (id, rollout_path),
            )
            .unwrap();
        }
    }

    fn create_official_like_db(path: &std::path::Path, threads: &[(&str, &str)]) {
        let conn = Connection::open(path).unwrap();
        conn.execute(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                rollout_path TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                source TEXT NOT NULL,
                model_provider TEXT NOT NULL,
                cwd TEXT NOT NULL,
                title TEXT NOT NULL,
                sandbox_policy TEXT NOT NULL,
                approval_mode TEXT NOT NULL,
                tokens_used INTEGER NOT NULL DEFAULT 0,
                has_user_event INTEGER NOT NULL DEFAULT 0,
                archived INTEGER NOT NULL DEFAULT 0,
                cli_version TEXT NOT NULL DEFAULT '',
                first_user_message TEXT NOT NULL DEFAULT '',
                memory_mode TEXT NOT NULL DEFAULT 'enabled',
                model TEXT,
                reasoning_effort TEXT,
                created_at_ms INTEGER,
                updated_at_ms INTEGER,
                preview TEXT NOT NULL DEFAULT '',
                recency_at INTEGER NOT NULL DEFAULT 0,
                recency_at_ms INTEGER NOT NULL DEFAULT 0
            )",
            [],
        )
        .unwrap();
        for (id, rollout_path) in threads {
            conn.execute(
                "INSERT INTO threads (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, sandbox_policy, approval_mode, created_at_ms, updated_at_ms, recency_at, recency_at_ms) VALUES (?1, ?2, 1, 1, 'cli', 'openai', '', '', '', '', 1000, 1000, 1, 1000)",
                (id, rollout_path),
            )
            .unwrap();
        }
    }

    fn create_provider_gc_candidate() -> (
        tempfile::TempDir,
        tempfile::TempDir,
        std::path::PathBuf,
        super::SessionSyncResult,
    ) {
        let shared = tempdir().unwrap();
        let home = tempdir().unwrap();
        let relative =
            format!("sessions/2026/07/25/rollout-2026-07-25T12-00-00-{REMOTE_THREAD_A}.jsonl");
        let shared_jsonl = shared.path().join(&relative);
        let home_jsonl = home.path().join(&relative);
        fs::create_dir_all(shared_jsonl.parent().unwrap()).unwrap();
        fs::create_dir_all(home_jsonl.parent().unwrap()).unwrap();
        let base = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{REMOTE_THREAD_A}\",\"model_provider\":\"openai\"}}}}\n"
        );
        fs::write(&shared_jsonl, &base).unwrap();
        fs::write(&home_jsonl, &base).unwrap();
        create_official_like_db(
            &shared.path().join("state_5.sqlite"),
            &[(REMOTE_THREAD_A, shared_jsonl.to_str().unwrap())],
        );
        create_official_like_db(
            &home.path().join("state_5.sqlite"),
            &[(REMOTE_THREAD_A, home_jsonl.to_str().unwrap())],
        );

        sync_shared_to_user_home(shared.path(), home.path(), "openai_custom").unwrap();
        let conn = Connection::open(home.path().join("state_5.sqlite")).unwrap();
        let first_custom: String = conn
            .query_row(
                "SELECT rollout_path FROM threads WHERE id = ?1",
                [REMOTE_THREAD_A],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);
        let first_custom = std::path::PathBuf::from(first_custom);
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&first_custom)
            .unwrap();
        writeln!(
            file,
            "{{\"type\":\"response_item\",\"payload\":{{\"text\":\"growth\"}}}}"
        )
        .unwrap();
        file.sync_all().unwrap();

        sync_shared_to_user_home(shared.path(), home.path(), "openai").unwrap();
        let conn = Connection::open(home.path().join("state_5.sqlite")).unwrap();
        let grown_openai: String = conn
            .query_row(
                "SELECT rollout_path FROM threads WHERE id = ?1",
                [REMOTE_THREAD_A],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(grown_openai)
            .unwrap();
        writeln!(
            file,
            "{{\"type\":\"response_item\",\"payload\":{{\"text\":\"second-growth\"}}}}"
        )
        .unwrap();
        file.sync_all().unwrap();

        let successor_result =
            sync_shared_to_user_home(shared.path(), home.path(), "openai_custom").unwrap();
        assert_eq!(successor_result.obsolete_provider_slots.len(), 1);
        (shared, home, first_custom, successor_result)
    }

    #[test]
    fn copies_new_threads_and_jsonl_without_duplicates() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let source_jsonl = source
            .path()
            .join("sessions/2026/06/23/rollout-thread-b.jsonl");
        fs::create_dir_all(source_jsonl.parent().unwrap()).unwrap();
        fs::write(
            &source_jsonl,
            r#"{"type":"session_meta","payload":{"id":"thread-b"}}"#,
        )
        .unwrap();
        create_db(
            &source.path().join("state_5.sqlite"),
            &[
                ("thread-a", "a.jsonl"),
                ("thread-b", source_jsonl.to_str().unwrap()),
            ],
        );
        create_db(
            &target.path().join("state_5.sqlite"),
            &[("thread-a", "a.jsonl")],
        );

        let result = sync_sessions(&[source.path().to_path_buf()], target.path()).unwrap();
        let result_again = sync_sessions(&[source.path().to_path_buf()], target.path()).unwrap();

        assert_eq!(result.inserted_threads, 1);
        assert_eq!(result.copied_session_files, 1);
        assert_eq!(result.skipped_missing_session_files, 1);
        assert_eq!(result_again.inserted_threads, 0);
        let conn = Connection::open(target.path().join("state_5.sqlite")).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2);
        assert!(target
            .path()
            .join("sessions/2026/06/23/rollout-thread-b.jsonl")
            .exists());
    }

    #[test]
    fn skips_db_rows_without_rollout_jsonl() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        create_db(
            &source.path().join("state_5.sqlite"),
            &[("thread-a", "missing.jsonl")],
        );
        create_db(&target.path().join("state_5.sqlite"), &[]);

        let result = sync_sessions(&[source.path().to_path_buf()], target.path()).unwrap();

        assert_eq!(result.inserted_threads, 0);
        assert_eq!(result.skipped_missing_session_files, 1);
        let conn = Connection::open(target.path().join("state_5.sqlite")).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn inserts_jsonl_only_thread_into_official_like_schema() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let source_jsonl = source
            .path()
            .join("sessions/2026/06/23/rollout-thread-jsonl-only.jsonl");
        fs::create_dir_all(source_jsonl.parent().unwrap()).unwrap();
        fs::write(
            &source_jsonl,
            r#"{"type":"session_meta","payload":{"id":"thread-jsonl-only","model_provider":"openai","source":"cli","cwd":"C:\\repo","cli_version":"1.0.0"}}"#,
        )
        .unwrap();
        create_official_like_db(&target.path().join("state_5.sqlite"), &[]);

        let result = sync_sessions(&[source.path().to_path_buf()], target.path()).unwrap();

        assert_eq!(result.inserted_threads, 1);
        let conn = Connection::open(target.path().join("state_5.sqlite")).unwrap();
        let (provider, cwd): (String, String) = conn
            .query_row(
                "SELECT model_provider, cwd FROM threads WHERE id = 'thread-jsonl-only'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(provider, "openai");
        assert_eq!(cwd, r"C:\repo");
    }

    #[test]
    fn target_schema_default_is_used_for_a_new_required_column_missing_from_source() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let thread_id = "thread-target-default";
        let source_jsonl = source
            .path()
            .join("sessions/2026/07/27/rollout-thread-target-default.jsonl");
        fs::create_dir_all(source_jsonl.parent().unwrap()).unwrap();
        fs::write(
            &source_jsonl,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{thread_id}\",\"model_provider\":\"openai\"}}}}"
            ),
        )
        .unwrap();
        create_official_like_db(
            &source.path().join("state_5.sqlite"),
            &[(thread_id, source_jsonl.to_str().unwrap())],
        );
        let source_conn = Connection::open(source.path().join("state_5.sqlite")).unwrap();
        source_conn
            .execute(
                "ALTER TABLE threads ADD COLUMN nullable_note TEXT DEFAULT 'source'",
                [],
            )
            .unwrap();
        source_conn
            .execute(
                "UPDATE threads SET nullable_note = NULL WHERE id = ?1",
                [thread_id],
            )
            .unwrap();
        drop(source_conn);
        create_official_like_db(&target.path().join("state_5.sqlite"), &[]);
        let target_conn = Connection::open(target.path().join("state_5.sqlite")).unwrap();
        target_conn
            .execute(
                "ALTER TABLE threads ADD COLUMN history_mode TEXT NOT NULL DEFAULT 'legacy'",
                [],
            )
            .unwrap();
        target_conn
            .execute(
                "ALTER TABLE threads ADD COLUMN nullable_note TEXT DEFAULT 'target'",
                [],
            )
            .unwrap();
        drop(target_conn);

        let result = sync_sessions(&[source.path().to_path_buf()], target.path()).unwrap();

        assert_eq!(result.inserted_threads, 1);
        let target_conn = Connection::open(target.path().join("state_5.sqlite")).unwrap();
        let (history_mode, nullable_note): (String, Option<String>) = target_conn
            .query_row(
                "SELECT history_mode, nullable_note FROM threads WHERE id = ?1",
                [thread_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(history_mode, "legacy");
        assert_eq!(nullable_note, None);
    }

    #[test]
    fn repairs_duplicate_thread_rollout_and_normalizes_provider_metadata() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let source_jsonl = source.path().join(format!(
            "sessions/2026/06/23/rollout-2026-06-23T12-00-00-{REMOTE_THREAD_A}.jsonl"
        ));
        fs::create_dir_all(source_jsonl.parent().unwrap()).unwrap();
        fs::write(
            &source_jsonl,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{REMOTE_THREAD_A}\",\"model_provider\":\"openai_custom\"}}}}\n\
                 {{\"type\":\"response_item\",\"payload\":{{\"text\":\"do not rewrite openai_custom in content\"}}}}\n"
            ),
        )
        .unwrap();
        create_db(
            &source.path().join("state_5.sqlite"),
            &[(REMOTE_THREAD_A, source_jsonl.to_str().unwrap())],
        );
        set_provider(source.path(), "openai_custom");

        let missing_target_rollout = target.path().join("sessions/2026/06/23/missing.jsonl");
        create_db(
            &target.path().join("state_5.sqlite"),
            &[(REMOTE_THREAD_A, missing_target_rollout.to_str().unwrap())],
        );
        set_provider(target.path(), "openai_custom");

        let result =
            sync_sessions_for_provider(&[source.path().to_path_buf()], target.path(), "openai")
                .unwrap();

        assert_eq!(result.inserted_threads, 0);
        assert_eq!(result.duplicate_threads, 1);
        assert_eq!(result.copied_session_files, 1);
        let conn = Connection::open(target.path().join("state_5.sqlite")).unwrap();
        let (provider, rollout_path): (String, String) = conn
            .query_row(
                "SELECT model_provider, rollout_path FROM threads WHERE id = ?1",
                [REMOTE_THREAD_A],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let target_jsonl = std::path::PathBuf::from(rollout_path);
        let jsonl = fs::read_to_string(&target_jsonl).unwrap();
        assert!(jsonl.contains(r#""model_provider":"openai""#));
        assert!(jsonl.contains("do not rewrite openai_custom in content"));
        assert_eq!(provider, "openai");
        assert!(super::is_remote_rollout_path(
            &target_jsonl,
            REMOTE_THREAD_A
        ));
    }

    #[test]
    fn merges_session_index_entries() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let source_jsonl = source
            .path()
            .join("sessions/2026/06/23/rollout-thread-a.jsonl");
        fs::create_dir_all(source_jsonl.parent().unwrap()).unwrap();
        fs::write(
            &source_jsonl,
            r#"{"type":"session_meta","payload":{"id":"thread-a"}}"#,
        )
        .unwrap();
        fs::write(
            source.path().join("session_index.jsonl"),
            "{\"id\":\"thread-a\",\"thread_name\":\"A\",\"updated_at\":\"now\"}\n",
        )
        .unwrap();
        create_db(
            &source.path().join("state_5.sqlite"),
            &[("thread-a", source_jsonl.to_str().unwrap())],
        );
        create_db(&target.path().join("state_5.sqlite"), &[]);

        let result = sync_sessions(&[source.path().to_path_buf()], target.path()).unwrap();
        let result_again = sync_sessions(&[source.path().to_path_buf()], target.path()).unwrap();

        assert_eq!(result.merged_session_index_entries, 1);
        assert_eq!(result_again.merged_session_index_entries, 0);
        assert!(
            fs::read_to_string(target.path().join("session_index.jsonl"))
                .unwrap()
                .contains("thread_name")
        );
    }

    #[test]
    fn session_index_atomic_merge_repairs_a_missing_trailing_newline() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        fs::write(
            source.path().join("session_index.jsonl"),
            "{\"id\":\"thread-b\"}\n",
        )
        .unwrap();
        fs::write(
            target.path().join("session_index.jsonl"),
            "{\"id\":\"thread-a\"}",
        )
        .unwrap();
        let source_root = root_from_paths(crate::codex_paths::local_codex_paths(source.path()));
        let target_root = root_from_paths(crate::codex_paths::local_codex_paths(target.path()));

        let merged = merge_session_index(
            &source_root,
            &target_root,
            &std::collections::HashSet::from(["thread-b".to_string()]),
        )
        .unwrap();

        assert_eq!(merged, 1);
        let index = fs::read_to_string(target.path().join("session_index.jsonl")).unwrap();
        assert_eq!(index.lines().count(), 2);
        assert!(index.contains("thread-a\"}\n{\"id\":\"thread-b"));
    }

    #[test]
    fn session_index_merge_publishes_a_complete_replacement() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let source_index = source.path().join("session_index.jsonl");
        let target_index = target.path().join("session_index.jsonl");
        let original = b"{\"id\":\"thread-a\"}\n";
        fs::write(&source_index, "{\"id\":\"thread-b\"}\n").unwrap();
        fs::write(&target_index, original).unwrap();
        let source_root = root_from_paths(crate::codex_paths::local_codex_paths(source.path()));
        let target_root = root_from_paths(crate::codex_paths::local_codex_paths(target.path()));

        let merged = merge_session_index(
            &source_root,
            &target_root,
            &std::collections::HashSet::from(["thread-b".to_string()]),
        )
        .unwrap();

        assert_eq!(merged, 1);
        let published = fs::read_to_string(&target_index).unwrap();
        assert_eq!(published, "{\"id\":\"thread-a\"}\n{\"id\":\"thread-b\"}\n");
        assert_eq!(
            fs::read_dir(target.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().contains("codex-switch"))
                .count(),
            0
        );
    }

    #[test]
    fn denied_session_index_write_is_rechecked_after_planning() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        fs::write(
            source.path().join("session_index.jsonl"),
            "{\"id\":\"thread-b\"}\n",
        )
        .unwrap();
        fs::write(
            target.path().join("session_index.jsonl"),
            "{\"id\":\"thread-b\"}\n",
        )
        .unwrap();
        let source_root = root_from_paths(crate::codex_paths::local_codex_paths(source.path()));
        let target_root = root_from_paths(crate::codex_paths::local_codex_paths(target.path()));
        let allowlist = std::collections::HashSet::from(["thread-b".to_string()]);
        let initial_plan =
            plan_session_index_merge(&source_root, &target_root, &allowlist).unwrap();
        assert!(initial_plan.lines.is_empty());

        let drifted = b"{\"id\":\"thread-a\"}\n";
        fs::write(target.path().join("session_index.jsonl"), drifted).unwrap();
        let error = merge_session_index_with_policy(
            &source_root,
            &target_root,
            &allowlist,
            SessionIndexWritePolicy::Deny,
        )
        .unwrap_err();

        assert!(error.contains("changed after fast-path planning"));
        assert!(error.contains("retry the session sync"));
        assert!(!error.contains("runtime switch"));
        assert_eq!(
            fs::read(target.path().join("session_index.jsonl")).unwrap(),
            drifted
        );
    }

    #[test]
    fn session_index_atomic_merge_preserves_an_incomplete_target() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        fs::write(
            source.path().join("session_index.jsonl"),
            "{\"id\":\"thread-b\"}\n",
        )
        .unwrap();
        let target_index = target.path().join("session_index.jsonl");
        let incomplete = b"{\"id\":\"thread-a\"";
        fs::write(&target_index, incomplete).unwrap();
        let source_root = root_from_paths(crate::codex_paths::local_codex_paths(source.path()));
        let target_root = root_from_paths(crate::codex_paths::local_codex_paths(target.path()));

        let error = merge_session_index(
            &source_root,
            &target_root,
            &std::collections::HashSet::from(["thread-b".to_string()]),
        )
        .unwrap_err();

        assert!(error.contains("incomplete or invalid entry"));
        assert_eq!(fs::read(&target_index).unwrap(), incomplete);
    }

    #[cfg(windows)]
    #[test]
    fn session_index_atomic_replace_fails_closed_while_target_is_open() {
        use std::{fs::OpenOptions, os::windows::fs::OpenOptionsExt};

        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        fs::write(
            source.path().join("session_index.jsonl"),
            "{\"id\":\"thread-b\"}\n",
        )
        .unwrap();
        let target_index = target.path().join("session_index.jsonl");
        let original = b"{\"id\":\"thread-a\"}\n";
        fs::write(&target_index, original).unwrap();
        let held_target = OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0x7)
            .open(&target_index)
            .unwrap();
        let source_root = root_from_paths(crate::codex_paths::local_codex_paths(source.path()));
        let target_root = root_from_paths(crate::codex_paths::local_codex_paths(target.path()));

        let error = merge_session_index(
            &source_root,
            &target_root,
            &std::collections::HashSet::from(["thread-b".to_string()]),
        )
        .unwrap_err();

        assert!(error.contains("failed to atomically replace session_index.jsonl"));
        assert_eq!(fs::read(&target_index).unwrap(), original);
        drop(held_target);
    }

    #[test]
    fn skips_archived_threads_and_session_index_entries() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let active_jsonl = source
            .path()
            .join("sessions/2026/06/23/rollout-active.jsonl");
        let archived_jsonl = source
            .path()
            .join("sessions/2026/06/23/rollout-archived.jsonl");
        fs::create_dir_all(active_jsonl.parent().unwrap()).unwrap();
        fs::write(
            &active_jsonl,
            r#"{"type":"session_meta","payload":{"id":"thread-active"}}"#,
        )
        .unwrap();
        fs::write(
            &archived_jsonl,
            r#"{"type":"session_meta","payload":{"id":"thread-archived"}}"#,
        )
        .unwrap();
        fs::write(
            source.path().join("session_index.jsonl"),
            "{\"id\":\"thread-active\"}\n{\"id\":\"thread-archived\"}\n",
        )
        .unwrap();
        create_db(
            &source.path().join("state_5.sqlite"),
            &[
                ("thread-active", active_jsonl.to_str().unwrap()),
                ("thread-archived", archived_jsonl.to_str().unwrap()),
            ],
        );
        let conn = Connection::open(source.path().join("state_5.sqlite")).unwrap();
        conn.execute(
            "ALTER TABLE threads ADD COLUMN archived INTEGER DEFAULT 0",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE threads SET archived = 1 WHERE id = 'thread-archived'",
            [],
        )
        .unwrap();
        create_db(&target.path().join("state_5.sqlite"), &[]);

        let result = sync_sessions(&[source.path().to_path_buf()], target.path()).unwrap();

        assert_eq!(result.inserted_threads, 1);
        assert_eq!(result.skipped_archived_threads, 1);
        assert!(!target
            .path()
            .join("sessions/2026/06/23/rollout-archived.jsonl")
            .exists());
        let target_index = fs::read_to_string(target.path().join("session_index.jsonl")).unwrap();
        assert!(target_index.contains("thread-active"));
        assert!(!target_index.contains("thread-archived"));
        let target_conn = Connection::open(target.path().join("state_5.sqlite")).unwrap();
        let archived_count: i64 = target_conn
            .query_row(
                "SELECT COUNT(*) FROM threads WHERE id = 'thread-archived'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(archived_count, 0);
    }

    #[test]
    fn user_home_sync_respects_configured_sqlite_home() {
        let home = tempdir().unwrap();
        let sqlite_home = tempdir().unwrap();
        let shared = tempdir().unwrap();
        fs::write(
            home.path().join("config.toml"),
            format!("sqlite_home = \"{}\"\n", sqlite_home.path().display()).replace('\\', "\\\\"),
        )
        .unwrap();
        let source_jsonl = home.path().join(format!(
            "sessions/2026/06/23/rollout-2026-06-23T12-00-00-{REMOTE_THREAD_A}.jsonl"
        ));
        fs::create_dir_all(source_jsonl.parent().unwrap()).unwrap();
        fs::write(
            &source_jsonl,
            format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{REMOTE_THREAD_A}\"}}}}"),
        )
        .unwrap();
        create_db(
            &sqlite_home.path().join("state_5.sqlite"),
            &[(REMOTE_THREAD_A, source_jsonl.to_str().unwrap())],
        );
        create_db(&shared.path().join("state_5.sqlite"), &[]);

        let to_shared = sync_user_home_to_shared(home.path(), shared.path()).unwrap();
        let from_shared = sync_shared_to_user_home(shared.path(), home.path(), "openai").unwrap();

        assert_eq!(to_shared.inserted_threads, 1);
        assert!(from_shared.duplicate_threads >= 1);
        let conn = Connection::open(sqlite_home.path().join("state_5.sqlite")).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM threads WHERE id = ?1",
                [REMOTE_THREAD_A],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn frozen_user_paths_ignore_config_repoint_for_all_sync_directions() {
        let home = tempdir().unwrap();
        let sqlite_a = tempdir().unwrap();
        let sqlite_b = tempdir().unwrap();
        let shared = tempdir().unwrap();
        fs::write(
            home.path().join("config.toml"),
            format!("sqlite_home = \"{}\"\n", sqlite_a.path().display()).replace('\\', "\\\\"),
        )
        .unwrap();
        let thread_a = home.path().join(format!(
            "sessions/2026/07/26/rollout-2026-07-26T12-00-00-{REMOTE_THREAD_A}.jsonl"
        ));
        fs::create_dir_all(thread_a.parent().unwrap()).unwrap();
        fs::write(
            &thread_a,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{REMOTE_THREAD_A}\",\"model_provider\":\"openai\"}}}}"
            ),
        )
        .unwrap();
        create_official_like_db(
            &sqlite_a.path().join("state_5.sqlite"),
            &[(REMOTE_THREAD_A, thread_a.to_str().unwrap())],
        );
        create_official_like_db(&sqlite_b.path().join("state_5.sqlite"), &[]);
        create_official_like_db(&shared.path().join("state_5.sqlite"), &[]);
        let current_paths = crate::codex_paths::resolve_user_codex_paths(home.path()).unwrap();
        let shared_paths = crate::codex_paths::local_codex_paths(shared.path());

        fs::write(
            home.path().join("config.toml"),
            format!("sqlite_home = \"{}\"\n", sqlite_b.path().display()).replace('\\', "\\\\"),
        )
        .unwrap();

        let to_shared = sync_user_home_to_shared_with_paths(&current_paths, &shared_paths).unwrap();
        let from_shared =
            sync_shared_to_user_home_with_paths(&shared_paths, &current_paths, "relay").unwrap();
        assert_eq!(to_shared.inserted_threads, 1);
        assert_eq!(from_shared.duplicate_threads, 1);

        let thread_b = shared.path().join(format!(
            "sessions/2026/07/26/rollout-2026-07-26T12-01-00-{REMOTE_THREAD_B}.jsonl"
        ));
        fs::create_dir_all(thread_b.parent().unwrap()).unwrap();
        fs::write(
            &thread_b,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{REMOTE_THREAD_B}\",\"model_provider\":\"relay\"}}}}"
            ),
        )
        .unwrap();
        let shared_conn = Connection::open(shared.path().join("state_5.sqlite")).unwrap();
        shared_conn
            .execute(
                "INSERT INTO threads (
                    id, rollout_path, created_at, updated_at, source, model_provider, cwd, title,
                    sandbox_policy, approval_mode, created_at_ms, updated_at_ms, recency_at,
                    recency_at_ms
                ) VALUES (
                    ?1, ?2, 1, 1, 'cli', 'relay', '', '', '', '', 1000, 1000, 1, 1000
                )",
                (REMOTE_THREAD_B, thread_b.to_str().unwrap()),
            )
            .unwrap();
        drop(shared_conn);

        let hot = sync_shared_to_user_home_hot_with_paths(&shared_paths, &current_paths, "relay")
            .unwrap();
        assert_eq!(hot.inserted_threads, 1);

        let conn_a = Connection::open(sqlite_a.path().join("state_5.sqlite")).unwrap();
        let (count_a, provider_a): (i64, String) = conn_a
            .query_row(
                "SELECT COUNT(*), MAX(model_provider) FROM threads",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(count_a, 2);
        assert_eq!(provider_a, "relay");

        let conn_b = Connection::open(sqlite_b.path().join("state_5.sqlite")).unwrap();
        let count_b: i64 = conn_b
            .query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count_b, 0);
    }

    #[test]
    fn source_append_after_scan_is_replanned_from_a_stable_version() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let relative = "sessions/2026/07/26/rollout-thread-source-append.jsonl";
        let source_jsonl = source.path().join(relative);
        let target_jsonl = target.path().join(relative);
        fs::create_dir_all(source_jsonl.parent().unwrap()).unwrap();
        fs::create_dir_all(target_jsonl.parent().unwrap()).unwrap();
        let base = concat!(
            r#"{"type":"session_meta","payload":{"id":"thread-source-append"}}"#,
            "\n",
        );
        fs::write(&source_jsonl, base).unwrap();
        fs::write(&target_jsonl, base).unwrap();
        let target_hash = sha256_file(&target_jsonl).unwrap();
        let target_modified = fs::metadata(&target_jsonl).unwrap().modified().unwrap();
        create_db(
            &source.path().join("state_5.sqlite"),
            &[("thread-source-append", source_jsonl.to_str().unwrap())],
        );
        let source_root = root_from_paths(crate::codex_paths::local_codex_paths(source.path()));
        let target_root = root_from_paths(crate::codex_paths::local_codex_paths(target.path()));
        let source_conn = open_source_conn(&source_root).unwrap();
        let scan = read_source_threads(&source_root, source_conn.as_ref()).unwrap();
        assert_eq!(scan.threads.len(), 1);

        let tail = r#"{"type":"response_item","payload":{"text":"after scan"}}"#;
        let mut source_file = fs::OpenOptions::new()
            .append(true)
            .open(&source_jsonl)
            .unwrap();
        writeln!(source_file, "{tail}").unwrap();
        source_file.sync_data().unwrap();

        let copied = copy_rollout_file(
            &scan.threads[0],
            &target_root,
            true,
            SessionFileWritePolicy::Allow,
            None,
        )
        .unwrap();

        assert!(copied.copied);
        assert_ne!(std::path::Path::new(&copied.path), target_jsonl);
        assert_eq!(fs::read(&target_jsonl).unwrap(), base.as_bytes());
        assert_eq!(sha256_file(&target_jsonl).unwrap(), target_hash);
        assert_eq!(
            fs::metadata(&target_jsonl).unwrap().modified().unwrap(),
            target_modified
        );
        assert!(fs::read_to_string(&copied.path)
            .unwrap()
            .contains("after scan"));
    }

    #[test]
    fn source_replacement_after_scan_uses_the_new_complete_version() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let relative = "sessions/2026/07/26/rollout-thread-source-replace.jsonl";
        let source_jsonl = source.path().join(relative);
        let target_jsonl = target.path().join(relative);
        fs::create_dir_all(source_jsonl.parent().unwrap()).unwrap();
        fs::create_dir_all(target_jsonl.parent().unwrap()).unwrap();
        let original = concat!(
            r#"{"type":"session_meta","payload":{"id":"thread-source-replace"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"text":"original"}}"#,
            "\n",
        );
        fs::write(&source_jsonl, original).unwrap();
        fs::write(&target_jsonl, original).unwrap();
        create_db(
            &source.path().join("state_5.sqlite"),
            &[("thread-source-replace", source_jsonl.to_str().unwrap())],
        );
        let source_root = root_from_paths(crate::codex_paths::local_codex_paths(source.path()));
        let target_root = root_from_paths(crate::codex_paths::local_codex_paths(target.path()));
        let source_conn = open_source_conn(&source_root).unwrap();
        let scan = read_source_threads(&source_root, source_conn.as_ref()).unwrap();
        assert_eq!(scan.threads.len(), 1);

        fs::write(
            &source_jsonl,
            concat!(
                r#"{"type":"session_meta","payload":{"id":"thread-source-replace"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"text":"replacement"}}"#,
                "\n",
            ),
        )
        .unwrap();

        let copied = copy_rollout_file(
            &scan.threads[0],
            &target_root,
            true,
            SessionFileWritePolicy::Allow,
            None,
        )
        .unwrap();

        assert!(copied.copied);
        assert_ne!(std::path::Path::new(&copied.path), target_jsonl);
        assert!(fs::read_to_string(copied.path)
            .unwrap()
            .contains("replacement"));
        assert!(fs::read_to_string(&target_jsonl)
            .unwrap()
            .contains("original"));
    }

    #[test]
    fn incomplete_source_tail_is_never_published() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let relative = "sessions/2026/07/26/rollout-thread-incomplete-source.jsonl";
        let source_jsonl = source.path().join(relative);
        fs::create_dir_all(source_jsonl.parent().unwrap()).unwrap();
        fs::write(
            &source_jsonl,
            concat!(
                r#"{"type":"session_meta","payload":{"id":"thread-incomplete-source"}}"#,
                "\n",
                r#"{"type":"response_item","payload":"#,
            ),
        )
        .unwrap();
        create_db(
            &source.path().join("state_5.sqlite"),
            &[("thread-incomplete-source", source_jsonl.to_str().unwrap())],
        );
        let source_root = root_from_paths(crate::codex_paths::local_codex_paths(source.path()));
        let target_root = root_from_paths(crate::codex_paths::local_codex_paths(target.path()));
        let source_conn = open_source_conn(&source_root).unwrap();
        let scan = read_source_threads(&source_root, source_conn.as_ref()).unwrap();

        let error = copy_rollout_file(
            &scan.threads[0],
            &target_root,
            true,
            SessionFileWritePolicy::Allow,
            None,
        )
        .unwrap_err();

        assert!(error.contains("incomplete or invalid JSONL tail"));
        assert!(!target.path().join(relative).exists());
    }

    #[test]
    fn source_change_during_read_is_detected_before_publish() {
        let source = tempdir().unwrap();
        let source_jsonl = source.path().join("rollout-source-changing.jsonl");
        fs::write(
            &source_jsonl,
            concat!(
                r#"{"type":"session_meta","payload":{"id":"thread-changing"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"text":"first"}}"#,
                "\n",
            ),
        )
        .unwrap();
        let mut changed = false;

        let error = read_stable_source(&source_jsonl, "thread-changing", |_, _, _| {
            if !changed {
                changed = true;
                let mut file = fs::OpenOptions::new()
                    .append(true)
                    .open(&source_jsonl)
                    .unwrap();
                file.write_all(
                    concat!(
                        r#"{"type":"response_item","payload":{"text":"concurrent"}}"#,
                        "\n",
                    )
                    .as_bytes(),
                )
                .unwrap();
                file.sync_data().unwrap();
            }
            Ok(())
        })
        .unwrap_err();

        assert!(error.starts_with(super::SOURCE_CHANGED_PREFIX));
    }

    #[test]
    fn derived_rollout_keeps_only_the_first_session_meta_authoritative() {
        let source = tempdir().unwrap();
        let source_jsonl = source.path().join("rollout-derived.jsonl");
        let original = format!(
            concat!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{}\",\"model_provider\":\"openai_custom\"}}}}\n",
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{}\",\"model_provider\":\"openai\"}}}}\n",
                "{{\"type\":\"response_item\",\"payload\":{{\"text\":\"history\"}}}}\n",
            ),
            REMOTE_THREAD_A, REMOTE_THREAD_B
        );
        fs::write(&source_jsonl, original.as_bytes()).unwrap();

        let stable = read_stable_source(&source_jsonl, REMOTE_THREAD_A, |_, _, _| Ok(()))
            .expect("the first session_meta owns the derived rollout");
        assert_eq!(stable.version.meta.id, REMOTE_THREAD_A);

        let target_jsonl = source.path().join("rollout-derived-relay.jsonl");
        let mut target = fs::File::create(&target_jsonl).unwrap();
        write_session_file(
            &source_jsonl,
            &mut target,
            REMOTE_THREAD_A,
            Some("relay"),
            &stable.version,
        )
        .unwrap();
        drop(target);

        assert_eq!(fs::read(&source_jsonl).unwrap(), original.as_bytes());
        let target = fs::read_to_string(target_jsonl).unwrap();
        let lines = target.lines().collect::<Vec<_>>();
        let authoritative =
            serde_json::from_str::<serde_json::Value>(lines.first().unwrap()).unwrap();
        assert_eq!(
            authoritative["payload"]["id"].as_str(),
            Some(REMOTE_THREAD_A)
        );
        assert_eq!(
            authoritative["payload"]["model_provider"].as_str(),
            Some("relay")
        );
        let inherited = serde_json::from_str::<serde_json::Value>(lines.get(1).unwrap()).unwrap();
        assert_eq!(inherited["payload"]["id"].as_str(), Some(REMOTE_THREAD_B));
        assert_eq!(
            inherited["payload"]["model_provider"].as_str(),
            Some("openai")
        );
    }

    #[test]
    fn stable_source_rejects_a_wrong_authoritative_session_meta() {
        let source = tempdir().unwrap();
        let source_jsonl = source.path().join("rollout-wrong-authority.jsonl");
        fs::write(
            &source_jsonl,
            format!(
                concat!(
                    "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{}\"}}}}\n",
                    "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{}\"}}}}\n",
                ),
                REMOTE_THREAD_B, REMOTE_THREAD_A
            ),
        )
        .unwrap();

        let error =
            read_stable_source(&source_jsonl, REMOTE_THREAD_A, |_, _, _| Ok(())).unwrap_err();

        assert!(error.contains(&format!(
            "source session JSONL id changed from {REMOTE_THREAD_A} to {REMOTE_THREAD_B}"
        )));
    }

    #[test]
    fn hot_sync_deny_allows_db_only_when_session_files_are_unchanged() {
        let shared = tempdir().unwrap();
        let current = tempdir().unwrap();
        let relative = "sessions/2026/07/26/rollout-thread-hot-deny.jsonl";
        let shared_jsonl = shared.path().join(relative);
        let current_jsonl = current.path().join(relative);
        fs::create_dir_all(shared_jsonl.parent().unwrap()).unwrap();
        fs::create_dir_all(current_jsonl.parent().unwrap()).unwrap();
        let bytes = concat!(
            r#"{"type":"session_meta","payload":{"id":"thread-hot-deny"}}"#,
            "\n",
        );
        fs::write(&shared_jsonl, bytes).unwrap();
        fs::write(&current_jsonl, bytes).unwrap();
        create_db(
            &shared.path().join("state_5.sqlite"),
            &[("thread-hot-deny", shared_jsonl.to_str().unwrap())],
        );
        create_db(
            &current.path().join("state_5.sqlite"),
            &[("thread-hot-deny", current_jsonl.to_str().unwrap())],
        );
        let shared_paths = crate::codex_paths::local_codex_paths(shared.path());
        let current_paths = crate::codex_paths::local_codex_paths(current.path());

        let result = sync_shared_to_user_home_hot_with_policy(
            &shared_paths,
            &current_paths,
            "openai",
            SessionFileWritePolicy::Deny,
        )
        .unwrap();
        assert_eq!(result.copied_session_files, 0);

        let mut shared_file = fs::OpenOptions::new()
            .append(true)
            .open(&shared_jsonl)
            .unwrap();
        shared_file
            .write_all(
                concat!(
                    r#"{"type":"response_item","payload":{"text":"drift"}}"#,
                    "\n",
                )
                .as_bytes(),
            )
            .unwrap();
        shared_file.sync_data().unwrap();
        let error = sync_shared_to_user_home_hot_with_policy(
            &shared_paths,
            &current_paths,
            "openai",
            SessionFileWritePolicy::Deny,
        )
        .unwrap_err();

        assert!(error.contains("changed after fast-path planning"));
        assert!(error.contains("retry the session sync"));
        assert!(!error.contains("runtime switch"));
        assert_eq!(fs::read(&current_jsonl).unwrap(), bytes.as_bytes());
    }

    #[test]
    fn incremental_copy_only_reads_and_publishes_selected_threads() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let source_a = source.path().join(format!(
            "sessions/2026/07/28/rollout-{REMOTE_THREAD_A}.jsonl"
        ));
        let source_b = source.path().join(format!(
            "sessions/2026/07/28/rollout-{REMOTE_THREAD_B}.jsonl"
        ));
        fs::create_dir_all(source_a.parent().unwrap()).unwrap();
        for (path, id) in [(&source_a, REMOTE_THREAD_A), (&source_b, REMOTE_THREAD_B)] {
            fs::write(
                path,
                format!(
                    "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"model_provider\":\"openai\"}}}}\n"
                ),
            )
            .unwrap();
        }
        create_official_like_db(
            &source.path().join("state_5.sqlite"),
            &[
                (REMOTE_THREAD_A, source_a.to_str().unwrap()),
                (REMOTE_THREAD_B, source_b.to_str().unwrap()),
            ],
        );
        create_official_like_db(&target.path().join("state_5.sqlite"), &[]);

        let result = sync_selected_user_home_to_shared_with_paths(
            &local_codex_paths(source.path()),
            &local_codex_paths(target.path()),
            &HashSet::from([REMOTE_THREAD_A.to_string()]),
        )
        .unwrap();

        assert_eq!(result.inserted_threads, 1);
        let conn = Connection::open(target.path().join("state_5.sqlite")).unwrap();
        let selected: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM threads WHERE id = ?1",
                [REMOTE_THREAD_A],
                |row| row.get(0),
            )
            .unwrap();
        let unselected: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM threads WHERE id = ?1",
                [REMOTE_THREAD_B],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!((selected, unselected), (1, 0));
    }

    #[test]
    fn account_incremental_normalizes_only_selected_current_threads() {
        let home = tempdir().unwrap();
        let relay_a = home.path().join(format!(
            "sessions/2026/07/28/rollout-{REMOTE_THREAD_A}.jsonl"
        ));
        let relay_b = home.path().join(format!(
            "sessions/2026/07/28/rollout-{REMOTE_THREAD_B}.jsonl"
        ));
        fs::create_dir_all(relay_a.parent().unwrap()).unwrap();
        for (path, id) in [(&relay_a, REMOTE_THREAD_A), (&relay_b, REMOTE_THREAD_B)] {
            fs::write(
                path,
                format!(
                    "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"model_provider\":\"openai_custom\"}}}}\n"
                ),
            )
            .unwrap();
        }
        create_official_like_db(
            &home.path().join("state_5.sqlite"),
            &[
                (REMOTE_THREAD_A, relay_a.to_str().unwrap()),
                (REMOTE_THREAD_B, relay_b.to_str().unwrap()),
            ],
        );
        let conn = Connection::open(home.path().join("state_5.sqlite")).unwrap();
        conn.execute("UPDATE threads SET model_provider = 'openai_custom'", [])
            .unwrap();
        drop(conn);

        normalize_selected_user_home_provider_with_paths(
            &local_codex_paths(home.path()),
            &HashSet::from([REMOTE_THREAD_A.to_string()]),
            "openai",
        )
        .unwrap();

        let conn = Connection::open(home.path().join("state_5.sqlite")).unwrap();
        let selected: (String, String) = conn
            .query_row(
                "SELECT model_provider, rollout_path FROM threads WHERE id = ?1",
                [REMOTE_THREAD_A],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let unselected: (String, String) = conn
            .query_row(
                "SELECT model_provider, rollout_path FROM threads WHERE id = ?1",
                [REMOTE_THREAD_B],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(selected.0, "openai");
        assert!(fs::read_to_string(selected.1)
            .unwrap()
            .contains(r#""model_provider":"openai""#));
        assert_eq!(unselected.0, "openai_custom");
        assert_eq!(unselected.1, relay_b.to_string_lossy());
    }

    #[test]
    fn hot_sync_skips_the_live_session_index() {
        let shared = tempdir().unwrap();
        let current = tempdir().unwrap();
        let relative = "sessions/2026/07/26/rollout-thread-hot-index.jsonl";
        let shared_jsonl = shared.path().join(relative);
        let current_jsonl = current.path().join(relative);
        fs::create_dir_all(shared_jsonl.parent().unwrap()).unwrap();
        fs::create_dir_all(current_jsonl.parent().unwrap()).unwrap();
        let bytes = concat!(
            r#"{"type":"session_meta","payload":{"id":"thread-hot-index"}}"#,
            "\n",
        );
        fs::write(&shared_jsonl, bytes).unwrap();
        fs::write(&current_jsonl, bytes).unwrap();
        create_db(
            &shared.path().join("state_5.sqlite"),
            &[("thread-hot-index", shared_jsonl.to_str().unwrap())],
        );
        create_db(
            &current.path().join("state_5.sqlite"),
            &[("thread-hot-index", current_jsonl.to_str().unwrap())],
        );
        fs::write(
            shared.path().join("session_index.jsonl"),
            "{\"id\":\"thread-hot-index\",\"thread_name\":\"Shared\"}\n",
        )
        .unwrap();
        let current_index = current.path().join("session_index.jsonl");
        let original_index = b"{\"id\":\"thread-current\",\"thread_name\":\"Current\"}\n";
        fs::write(&current_index, original_index).unwrap();
        let original_modified = fs::metadata(&current_index).unwrap().modified().unwrap();
        let shared_paths = crate::codex_paths::local_codex_paths(shared.path());
        let current_paths = crate::codex_paths::local_codex_paths(current.path());

        let result = sync_shared_to_user_home_hot_with_policy(
            &shared_paths,
            &current_paths,
            "openai",
            SessionFileWritePolicy::Allow,
        )
        .unwrap();

        assert_eq!(result.merged_session_index_entries, 0);
        assert_eq!(fs::read(&current_index).unwrap(), original_index);
        assert_eq!(
            fs::metadata(&current_index).unwrap().modified().unwrap(),
            original_modified
        );
    }

    #[test]
    fn publishes_a_complete_import_without_mutating_a_stale_target() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let relative = "sessions/2026/07/13/rollout-thread-growing.jsonl";
        let source_jsonl = source.path().join(relative);
        let target_jsonl = target.path().join(relative);
        fs::create_dir_all(source_jsonl.parent().unwrap()).unwrap();
        fs::create_dir_all(target_jsonl.parent().unwrap()).unwrap();
        let first = concat!(
            r#"{"type":"session_meta","payload":{"id":"thread-growing"}}"#,
            "\n",
        );
        fs::write(&target_jsonl, first).unwrap();
        fs::write(
            &source_jsonl,
            format!(
                "{first}{}\n",
                r#"{"type":"response_item","payload":{"text":"new tail"}}"#
            ),
        )
        .unwrap();
        create_db(
            &source.path().join("state_5.sqlite"),
            &[("thread-growing", source_jsonl.to_str().unwrap())],
        );
        create_db(
            &target.path().join("state_5.sqlite"),
            &[("thread-growing", target_jsonl.to_str().unwrap())],
        );
        let original_hash = sha256_file(&target_jsonl).unwrap();
        let original_modified = fs::metadata(&target_jsonl).unwrap().modified().unwrap();

        let result = sync_sessions(&[source.path().to_path_buf()], target.path()).unwrap();

        assert_eq!(result.copied_session_files, 1);
        assert_eq!(fs::read(&target_jsonl).unwrap(), first.as_bytes());
        assert_eq!(sha256_file(&target_jsonl).unwrap(), original_hash);
        assert_eq!(
            fs::metadata(&target_jsonl).unwrap().modified().unwrap(),
            original_modified
        );
        let conn = Connection::open(target.path().join("state_5.sqlite")).unwrap();
        let rollout_path: String = conn
            .query_row(
                "SELECT rollout_path FROM threads WHERE id = 'thread-growing'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let published = std::path::PathBuf::from(rollout_path);
        assert_ne!(published, target_jsonl);
        assert!(fs::read_to_string(published).unwrap().contains("new tail"));
    }

    #[test]
    fn divergent_versions_use_content_hashes_instead_of_one_stale_imported_file() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let relative = "sessions/2026/07/13/rollout-thread-conflict.jsonl";
        let source_jsonl = source.path().join(relative);
        let target_jsonl = target.path().join(relative);
        fs::create_dir_all(source_jsonl.parent().unwrap()).unwrap();
        fs::create_dir_all(target_jsonl.parent().unwrap()).unwrap();
        fs::write(
            &target_jsonl,
            r#"{"type":"session_meta","payload":{"id":"thread-conflict","source":"target"}}"#,
        )
        .unwrap();
        fs::write(
            &source_jsonl,
            r#"{"type":"session_meta","payload":{"id":"thread-conflict","source":"source-one"}}"#,
        )
        .unwrap();
        create_db(
            &source.path().join("state_5.sqlite"),
            &[("thread-conflict", source_jsonl.to_str().unwrap())],
        );
        create_db(
            &target.path().join("state_5.sqlite"),
            &[("thread-conflict", target_jsonl.to_str().unwrap())],
        );

        sync_sessions(&[source.path().to_path_buf()], target.path()).unwrap();
        fs::write(
            &source_jsonl,
            r#"{"type":"session_meta","payload":{"id":"thread-conflict","source":"source-two"}}"#,
        )
        .unwrap();
        sync_sessions(&[source.path().to_path_buf()], target.path()).unwrap();

        let imported = fs::read_dir(target_jsonl.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
                    && entry.file_name().to_string_lossy().contains("-imported-")
            })
            .count();
        assert_eq!(imported, 2);
    }

    #[test]
    fn hot_sync_does_not_rewrite_an_existing_live_jsonl_for_provider_changes() {
        let shared = tempdir().unwrap();
        let home = tempdir().unwrap();
        let relative = "sessions/2026/07/13/rollout-thread-hot.jsonl";
        let shared_jsonl = shared.path().join(relative);
        let home_jsonl = home.path().join(relative);
        fs::create_dir_all(shared_jsonl.parent().unwrap()).unwrap();
        fs::create_dir_all(home_jsonl.parent().unwrap()).unwrap();
        fs::write(
            &shared_jsonl,
            r#"{"type":"session_meta","payload":{"id":"thread-hot","model_provider":"openai_custom"}}"#,
        )
        .unwrap();
        let live_bytes =
            br#"{"type":"session_meta","payload":{"id":"thread-hot","model_provider":"openai"}}"#;
        fs::write(&home_jsonl, live_bytes).unwrap();
        create_db(
            &shared.path().join("state_5.sqlite"),
            &[("thread-hot", shared_jsonl.to_str().unwrap())],
        );
        create_db(
            &home.path().join("state_5.sqlite"),
            &[("thread-hot", home_jsonl.to_str().unwrap())],
        );

        sync_shared_to_user_home_hot(shared.path(), home.path(), "openai").unwrap();

        assert_eq!(fs::read(&home_jsonl).unwrap(), live_bytes);
    }

    #[test]
    fn hot_sync_publishes_one_provider_rollout_for_a_new_remote_thread() {
        let shared = tempdir().unwrap();
        let home = tempdir().unwrap();
        let relative =
            format!("sessions/2026/07/26/rollout-2026-07-26T12-00-00-{REMOTE_THREAD_A}.jsonl");
        let shared_jsonl = shared.path().join(&relative);
        let raw_home_jsonl = home.path().join(&relative);
        fs::create_dir_all(shared_jsonl.parent().unwrap()).unwrap();
        fs::write(
            &shared_jsonl,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{REMOTE_THREAD_A}\",\"model_provider\":\"shared-provider\"}}}}\n"
            ),
        )
        .unwrap();
        create_official_like_db(
            &shared.path().join("state_5.sqlite"),
            &[(REMOTE_THREAD_A, shared_jsonl.to_str().unwrap())],
        );
        create_official_like_db(&home.path().join("state_5.sqlite"), &[]);

        let result =
            sync_shared_to_user_home_hot(shared.path(), home.path(), "current-provider").unwrap();

        assert_eq!(result.copied_session_files, 1);
        assert!(!raw_home_jsonl.exists());
        let session_dir = raw_home_jsonl.parent().unwrap();
        let jsonl_files = fs::read_dir(session_dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
            })
            .collect::<Vec<_>>();
        assert_eq!(jsonl_files.len(), 1);
        assert!(fs::read_to_string(jsonl_files[0].path())
            .unwrap()
            .contains(r#""model_provider":"current-provider""#));
    }

    #[test]
    fn provider_switch_publishes_a_stable_remote_rollout_without_rewriting_original_history() {
        let shared = tempdir().unwrap();
        let home = tempdir().unwrap();
        let relative =
            format!("sessions/2026/07/25/rollout-2026-07-25T12-00-00-{REMOTE_THREAD_A}.jsonl");
        let shared_jsonl = shared.path().join(&relative);
        let home_jsonl = home.path().join(&relative);
        fs::create_dir_all(shared_jsonl.parent().unwrap()).unwrap();
        fs::create_dir_all(home_jsonl.parent().unwrap()).unwrap();
        let existing_bytes = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{REMOTE_THREAD_A}\",\"model_provider\":\"openai\"}}}}\n\
             {{\"type\":\"response_item\",\"payload\":{{\"text\":\"unchanged history\"}}}}\n"
        );
        fs::write(&shared_jsonl, &existing_bytes).unwrap();
        fs::write(&home_jsonl, &existing_bytes).unwrap();
        create_official_like_db(
            &shared.path().join("state_5.sqlite"),
            &[(REMOTE_THREAD_A, shared_jsonl.to_str().unwrap())],
        );
        create_official_like_db(
            &home.path().join("state_5.sqlite"),
            &[(REMOTE_THREAD_A, home_jsonl.to_str().unwrap())],
        );
        let modified_before = fs::metadata(&home_jsonl).unwrap().modified().unwrap();

        let result = sync_shared_to_user_home(shared.path(), home.path(), "openai_custom").unwrap();

        assert_eq!(result.copied_session_files, 1);
        assert_eq!(fs::read(&home_jsonl).unwrap(), existing_bytes.as_bytes());
        assert_eq!(
            fs::metadata(&home_jsonl).unwrap().modified().unwrap(),
            modified_before
        );
        let conn = Connection::open(home.path().join("state_5.sqlite")).unwrap();
        let (provider, rollout_path): (String, String) = conn
            .query_row(
                "SELECT model_provider, rollout_path FROM threads WHERE id = ?1",
                [REMOTE_THREAD_A],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(provider, "openai_custom");
        let provider_rollout = std::path::PathBuf::from(rollout_path);
        assert_ne!(provider_rollout, home_jsonl);
        assert!(super::is_remote_rollout_path(
            &provider_rollout,
            REMOTE_THREAD_A
        ));
        assert!(fs::read_to_string(&provider_rollout)
            .unwrap()
            .contains(r#""model_provider":"openai_custom""#));
        drop(conn);

        let files_after_first = fs::read_dir(home_jsonl.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .count();
        let repeated =
            sync_shared_to_user_home(shared.path(), home.path(), "openai_custom").unwrap();
        assert_eq!(repeated.copied_session_files, 0);
        let conn = Connection::open(home.path().join("state_5.sqlite")).unwrap();
        let repeated_path: String = conn
            .query_row(
                "SELECT rollout_path FROM threads WHERE id = ?1",
                [REMOTE_THREAD_A],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(std::path::PathBuf::from(repeated_path), provider_rollout);
        assert_eq!(
            fs::read_dir(home_jsonl.parent().unwrap())
                .unwrap()
                .filter_map(Result::ok)
                .count(),
            files_after_first
        );
    }

    #[test]
    fn native_remote_candidate_collision_is_never_overwritten_or_marked_owned() {
        let shared = tempdir().unwrap();
        let home = tempdir().unwrap();
        let relative =
            format!("sessions/2026/07/25/rollout-2026-07-25T12-00-00-{REMOTE_THREAD_A}.jsonl");
        let shared_jsonl = shared.path().join(&relative);
        let home_jsonl = home.path().join(&relative);
        fs::create_dir_all(shared_jsonl.parent().unwrap()).unwrap();
        fs::create_dir_all(home_jsonl.parent().unwrap()).unwrap();
        let complete = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{REMOTE_THREAD_A}\",\"model_provider\":\"openai\"}}}}\n\
             {{\"type\":\"response_item\",\"payload\":{{\"text\":\"complete\"}}}}\n"
        );
        fs::write(&shared_jsonl, &complete).unwrap();
        fs::write(&home_jsonl, &complete).unwrap();
        create_official_like_db(
            &shared.path().join("state_5.sqlite"),
            &[(REMOTE_THREAD_A, shared_jsonl.to_str().unwrap())],
        );
        create_official_like_db(
            &home.path().join("state_5.sqlite"),
            &[(REMOTE_THREAD_A, home_jsonl.to_str().unwrap())],
        );
        let collision = super::provider_candidate_path(
            home_jsonl.parent().unwrap(),
            "2026-07-25",
            REMOTE_THREAD_A,
            "openai_custom",
            0,
        );
        fs::write(
            &collision,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{REMOTE_THREAD_A}\",\"model_provider\":\"openai_custom\"}}}}\n"
            ),
        )
        .unwrap();
        let collision_hash = sha256_file(&collision).unwrap();
        let collision_modified = fs::metadata(&collision).unwrap().modified().unwrap();

        sync_shared_to_user_home(shared.path(), home.path(), "openai_custom").unwrap();

        assert_eq!(sha256_file(&collision).unwrap(), collision_hash);
        assert_eq!(
            fs::metadata(&collision).unwrap().modified().unwrap(),
            collision_modified
        );
        assert!(!super::provider_slot_marker_path(&collision)
            .unwrap()
            .exists());
        let conn = Connection::open(home.path().join("state_5.sqlite")).unwrap();
        let active: String = conn
            .query_row(
                "SELECT rollout_path FROM threads WHERE id = ?1",
                [REMOTE_THREAD_A],
                |row| row.get(0),
            )
            .unwrap();
        assert_ne!(
            fs::canonicalize(active).unwrap(),
            fs::canonicalize(collision).unwrap()
        );
    }

    #[test]
    fn owned_marker_accepts_append_but_rejects_a_changed_created_prefix() {
        let shared = tempdir().unwrap();
        let home = tempdir().unwrap();
        let relative =
            format!("sessions/2026/07/25/rollout-2026-07-25T12-00-00-{REMOTE_THREAD_A}.jsonl");
        let shared_jsonl = shared.path().join(&relative);
        let home_jsonl = home.path().join(&relative);
        fs::create_dir_all(shared_jsonl.parent().unwrap()).unwrap();
        fs::create_dir_all(home_jsonl.parent().unwrap()).unwrap();
        let base = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{REMOTE_THREAD_A}\",\"model_provider\":\"openai\"}}}}\n\
             {{\"type\":\"response_item\",\"payload\":{{\"text\":\"base\"}}}}\n"
        );
        fs::write(&shared_jsonl, &base).unwrap();
        fs::write(&home_jsonl, &base).unwrap();
        create_official_like_db(
            &shared.path().join("state_5.sqlite"),
            &[(REMOTE_THREAD_A, shared_jsonl.to_str().unwrap())],
        );
        create_official_like_db(
            &home.path().join("state_5.sqlite"),
            &[(REMOTE_THREAD_A, home_jsonl.to_str().unwrap())],
        );
        sync_shared_to_user_home(shared.path(), home.path(), "openai_custom").unwrap();
        let conn = Connection::open(home.path().join("state_5.sqlite")).unwrap();
        let active: String = conn
            .query_row(
                "SELECT rollout_path FROM threads WHERE id = ?1",
                [REMOTE_THREAD_A],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);
        let active = std::path::PathBuf::from(active);
        let root = root_from_paths(crate::codex_paths::local_codex_paths(home.path()));
        assert!(
            super::read_provider_slot_marker(&root, &active, REMOTE_THREAD_A)
                .unwrap()
                .is_some()
        );

        let mut file = fs::OpenOptions::new().append(true).open(&active).unwrap();
        writeln!(
            file,
            "{{\"type\":\"response_item\",\"payload\":{{\"text\":\"append\"}}}}"
        )
        .unwrap();
        file.sync_all().unwrap();
        assert!(
            super::read_provider_slot_marker(&root, &active, REMOTE_THREAD_A)
                .unwrap()
                .is_some()
        );

        let mut changed = fs::read(&active).unwrap();
        let position = changed
            .windows(4)
            .position(|window| window == b"base")
            .unwrap();
        changed[position] = b'c';
        fs::write(&active, changed).unwrap();
        assert!(
            super::read_provider_slot_marker(&root, &active, REMOTE_THREAD_A)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn no_growth_round_trip_reuses_native_and_existing_provider_slots() {
        let shared = tempdir().unwrap();
        let home = tempdir().unwrap();
        let relative =
            format!("sessions/2026/07/25/rollout-2026-07-25T12-00-00-{REMOTE_THREAD_A}.jsonl");
        let shared_jsonl = shared.path().join(&relative);
        let home_jsonl = home.path().join(&relative);
        fs::create_dir_all(shared_jsonl.parent().unwrap()).unwrap();
        fs::create_dir_all(home_jsonl.parent().unwrap()).unwrap();
        let base = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{REMOTE_THREAD_A}\",\"model_provider\":\"openai\"}}}}\n\
             {{\"type\":\"response_item\",\"payload\":{{\"text\":\"same\"}}}}\n"
        );
        fs::write(&shared_jsonl, &base).unwrap();
        fs::write(&home_jsonl, &base).unwrap();
        create_official_like_db(
            &shared.path().join("state_5.sqlite"),
            &[(REMOTE_THREAD_A, shared_jsonl.to_str().unwrap())],
        );
        create_official_like_db(
            &home.path().join("state_5.sqlite"),
            &[(REMOTE_THREAD_A, home_jsonl.to_str().unwrap())],
        );

        let first = sync_shared_to_user_home(shared.path(), home.path(), "openai_custom").unwrap();
        assert!(first.persistent_session_bytes_added > 0);
        let back = sync_shared_to_user_home(shared.path(), home.path(), "openai").unwrap();
        assert_eq!(back.persistent_session_bytes_added, 0);
        let conn = Connection::open(home.path().join("state_5.sqlite")).unwrap();
        let native_again: String = conn
            .query_row(
                "SELECT rollout_path FROM threads WHERE id = ?1",
                [REMOTE_THREAD_A],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            fs::canonicalize(native_again).unwrap(),
            fs::canonicalize(&home_jsonl).unwrap()
        );
        drop(conn);

        let repeated =
            sync_shared_to_user_home(shared.path(), home.path(), "openai_custom").unwrap();
        assert_eq!(repeated.persistent_session_bytes_added, 0);
        assert_eq!(
            super::walk_jsonl_files(&home.path().join("sessions"))
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn shared_long_current_short_plans_and_writes_only_the_final_provider_slot() {
        let shared = tempdir().unwrap();
        let home = tempdir().unwrap();
        let relative =
            format!("sessions/2026/07/25/rollout-2026-07-25T12-00-00-{REMOTE_THREAD_A}.jsonl");
        let shared_jsonl = shared.path().join(&relative);
        let home_jsonl = home.path().join(&relative);
        fs::create_dir_all(shared_jsonl.parent().unwrap()).unwrap();
        fs::create_dir_all(home_jsonl.parent().unwrap()).unwrap();
        let base = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{REMOTE_THREAD_A}\",\"model_provider\":\"openai\"}}}}\n"
        );
        fs::write(&home_jsonl, &base).unwrap();
        fs::write(
            &shared_jsonl,
            format!(
                "{base}{{\"type\":\"response_item\",\"payload\":{{\"text\":\"{}\"}}}}\n",
                "long".repeat(4096)
            ),
        )
        .unwrap();
        create_official_like_db(
            &shared.path().join("state_5.sqlite"),
            &[(REMOTE_THREAD_A, shared_jsonl.to_str().unwrap())],
        );
        create_official_like_db(
            &home.path().join("state_5.sqlite"),
            &[(REMOTE_THREAD_A, home_jsonl.to_str().unwrap())],
        );
        let current_paths = crate::codex_paths::local_codex_paths(home.path());
        let shared_paths = crate::codex_paths::local_codex_paths(shared.path());
        let plan = super::plan_runtime_session_storage_with_paths(
            &current_paths,
            &shared_paths,
            "openai_custom",
        )
        .unwrap();
        let jsonl_demands = plan
            .demands()
            .into_iter()
            .filter(|demand| {
                demand
                    .destination
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
            })
            .collect::<Vec<_>>();
        assert_eq!(jsonl_demands.len(), 1, "{jsonl_demands:?}");
        assert!(jsonl_demands[0].bytes >= fs::metadata(&shared_jsonl).unwrap().len());
        assert!(!jsonl_demands[0]
            .destination
            .to_string_lossy()
            .contains("-imported-"));

        let result = sync_shared_to_user_home(shared.path(), home.path(), "openai_custom").unwrap();
        assert_eq!(result.copied_session_files, 1);
        assert_eq!(
            super::walk_jsonl_files(&home.path().join("sessions"))
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn owned_growth_uses_a_successor_and_gc_reclaims_only_an_unreferenced_stable_predecessor() {
        let shared = tempdir().unwrap();
        let home = tempdir().unwrap();
        let relative =
            format!("sessions/2026/07/25/rollout-2026-07-25T12-00-00-{REMOTE_THREAD_A}.jsonl");
        let shared_jsonl = shared.path().join(&relative);
        let home_jsonl = home.path().join(&relative);
        fs::create_dir_all(shared_jsonl.parent().unwrap()).unwrap();
        fs::create_dir_all(home_jsonl.parent().unwrap()).unwrap();
        let base = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{REMOTE_THREAD_A}\",\"model_provider\":\"openai\"}}}}\n"
        );
        fs::write(&shared_jsonl, &base).unwrap();
        fs::write(&home_jsonl, &base).unwrap();
        create_official_like_db(
            &shared.path().join("state_5.sqlite"),
            &[(REMOTE_THREAD_A, shared_jsonl.to_str().unwrap())],
        );
        create_official_like_db(
            &home.path().join("state_5.sqlite"),
            &[(REMOTE_THREAD_A, home_jsonl.to_str().unwrap())],
        );
        sync_shared_to_user_home(shared.path(), home.path(), "openai_custom").unwrap();
        let conn = Connection::open(home.path().join("state_5.sqlite")).unwrap();
        let first_custom: String = conn
            .query_row(
                "SELECT rollout_path FROM threads WHERE id = ?1",
                [REMOTE_THREAD_A],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);
        let first_custom = std::path::PathBuf::from(first_custom);
        let first_hash = sha256_file(&first_custom).unwrap();
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&first_custom)
            .unwrap();
        writeln!(
            file,
            "{{\"type\":\"response_item\",\"payload\":{{\"text\":\"growth\"}}}}"
        )
        .unwrap();
        file.sync_all().unwrap();

        sync_shared_to_user_home(shared.path(), home.path(), "openai").unwrap();
        let conn = Connection::open(home.path().join("state_5.sqlite")).unwrap();
        let grown_openai: String = conn
            .query_row(
                "SELECT rollout_path FROM threads WHERE id = ?1",
                [REMOTE_THREAD_A],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(grown_openai)
            .unwrap();
        writeln!(
            file,
            "{{\"type\":\"response_item\",\"payload\":{{\"text\":\"second-growth\"}}}}"
        )
        .unwrap();
        file.sync_all().unwrap();
        let successor_result =
            sync_shared_to_user_home(shared.path(), home.path(), "openai_custom").unwrap();
        assert_eq!(successor_result.obsolete_provider_slots.len(), 1);
        assert!(first_custom.exists());
        assert_ne!(sha256_file(&first_custom).unwrap(), first_hash);
        let conn = Connection::open(home.path().join("state_5.sqlite")).unwrap();
        let successor: String = conn
            .query_row(
                "SELECT rollout_path FROM threads WHERE id = ?1",
                [REMOTE_THREAD_A],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);
        let successor = std::path::PathBuf::from(successor);
        assert_ne!(
            fs::canonicalize(&successor).unwrap(),
            fs::canonicalize(&first_custom).unwrap()
        );

        let current_paths = crate::codex_paths::local_codex_paths(home.path());
        let shared_paths = crate::codex_paths::local_codex_paths(shared.path());
        let gc = super::cleanup_obsolete_provider_slots(
            &successor_result.obsolete_provider_slots,
            &current_paths,
            &shared_paths,
        );
        assert_eq!(gc.reclaimed_count, 1, "{gc:?}");
        assert_eq!(gc.retained_count, 0, "{gc:?}");
        assert!(gc.reclaimed_bytes > 0);
        assert!(!first_custom.exists());
        assert!(successor.exists());
    }

    #[test]
    fn provider_gc_retains_a_predecessor_appended_after_candidate_creation() {
        let (shared, home, first_custom, successor_result) = create_provider_gc_candidate();
        let successor = successor_result.obsolete_provider_slots[0]
            .successor
            .clone();
        let marker = super::provider_slot_marker_path(&first_custom).unwrap();

        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&first_custom)
            .unwrap();
        writeln!(
            file,
            "{{\"type\":\"response_item\",\"payload\":{{\"text\":\"late-append\"}}}}"
        )
        .unwrap();
        file.sync_all().unwrap();

        let current_paths = crate::codex_paths::local_codex_paths(home.path());
        let shared_paths = crate::codex_paths::local_codex_paths(shared.path());
        let gc = super::cleanup_obsolete_provider_slots(
            &successor_result.obsolete_provider_slots,
            &current_paths,
            &shared_paths,
        );
        assert_eq!(gc.reclaimed_count, 0, "{gc:?}");
        assert_eq!(gc.retained_count, 1, "{gc:?}");
        assert!(gc
            .warnings
            .iter()
            .any(|warning| warning.contains("发生变化")));
        assert!(first_custom.exists());
        assert!(marker.exists());
        assert!(successor.exists());
    }

    #[test]
    fn provider_gc_fails_closed_when_a_candidate_marker_is_malformed() {
        let (shared, home, first_custom, successor_result) = create_provider_gc_candidate();
        let successor = successor_result.obsolete_provider_slots[0]
            .successor
            .clone();
        let marker = super::provider_slot_marker_path(&first_custom).unwrap();
        fs::write(&marker, b"{not-valid-json").unwrap();

        let current_paths = crate::codex_paths::local_codex_paths(home.path());
        let shared_paths = crate::codex_paths::local_codex_paths(shared.path());
        let gc = super::cleanup_obsolete_provider_slots(
            &successor_result.obsolete_provider_slots,
            &current_paths,
            &shared_paths,
        );
        assert_eq!(gc.reclaimed_count, 0, "{gc:?}");
        assert_eq!(gc.retained_count, 1, "{gc:?}");
        assert!(gc
            .warnings
            .iter()
            .any(|warning| warning.contains("缺少有效来源标记")));
        assert!(first_custom.exists());
        assert!(marker.exists());
        assert!(successor.exists());
    }

    #[test]
    fn provider_gc_retains_a_predecessor_when_any_managed_database_still_references_it() {
        let shared = tempdir().unwrap();
        let home = tempdir().unwrap();
        let relative =
            format!("sessions/2026/07/25/rollout-2026-07-25T12-00-00-{REMOTE_THREAD_A}.jsonl");
        let shared_jsonl = shared.path().join(&relative);
        let home_jsonl = home.path().join(&relative);
        fs::create_dir_all(shared_jsonl.parent().unwrap()).unwrap();
        fs::create_dir_all(home_jsonl.parent().unwrap()).unwrap();
        let base = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{REMOTE_THREAD_A}\",\"model_provider\":\"openai\"}}}}\n"
        );
        fs::write(&shared_jsonl, &base).unwrap();
        fs::write(&home_jsonl, &base).unwrap();
        create_official_like_db(
            &shared.path().join("state_5.sqlite"),
            &[(REMOTE_THREAD_A, shared_jsonl.to_str().unwrap())],
        );
        create_official_like_db(
            &home.path().join("state_5.sqlite"),
            &[(REMOTE_THREAD_A, home_jsonl.to_str().unwrap())],
        );
        sync_shared_to_user_home(shared.path(), home.path(), "openai_custom").unwrap();
        let conn = Connection::open(home.path().join("state_5.sqlite")).unwrap();
        let first_custom: String = conn
            .query_row(
                "SELECT rollout_path FROM threads WHERE id = ?1",
                [REMOTE_THREAD_A],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);
        let first_custom = std::path::PathBuf::from(first_custom);
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&first_custom)
            .unwrap();
        writeln!(
            file,
            "{{\"type\":\"response_item\",\"payload\":{{\"text\":\"growth\"}}}}"
        )
        .unwrap();
        file.sync_all().unwrap();
        sync_shared_to_user_home(shared.path(), home.path(), "openai").unwrap();
        let conn = Connection::open(home.path().join("state_5.sqlite")).unwrap();
        let grown_openai: String = conn
            .query_row(
                "SELECT rollout_path FROM threads WHERE id = ?1",
                [REMOTE_THREAD_A],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(grown_openai)
            .unwrap();
        writeln!(
            file,
            "{{\"type\":\"response_item\",\"payload\":{{\"text\":\"second-growth\"}}}}"
        )
        .unwrap();
        file.sync_all().unwrap();
        let successor_result =
            sync_shared_to_user_home(shared.path(), home.path(), "openai_custom").unwrap();
        assert_eq!(successor_result.obsolete_provider_slots.len(), 1);

        let shared_conn = Connection::open(shared.path().join("state_5.sqlite")).unwrap();
        shared_conn
            .execute(
                "UPDATE threads SET rollout_path = ?1 WHERE id = ?2",
                (first_custom.to_string_lossy().as_ref(), REMOTE_THREAD_A),
            )
            .unwrap();
        drop(shared_conn);
        let current_paths = crate::codex_paths::local_codex_paths(home.path());
        let shared_paths = crate::codex_paths::local_codex_paths(shared.path());
        let gc = super::cleanup_obsolete_provider_slots(
            &successor_result.obsolete_provider_slots,
            &current_paths,
            &shared_paths,
        );
        assert_eq!(gc.reclaimed_count, 0, "{gc:?}");
        assert_eq!(gc.retained_count, 1, "{gc:?}");
        assert!(first_custom.exists());
    }

    #[test]
    fn provider_slots_stay_bounded_across_growth_and_repeated_round_trips() {
        let shared = tempdir().unwrap();
        let home = tempdir().unwrap();
        let relative =
            format!("sessions/2026/07/25/rollout-2026-07-25T12-00-00-{REMOTE_THREAD_A}.jsonl");
        let shared_jsonl = shared.path().join(&relative);
        let home_jsonl = home.path().join(&relative);
        fs::create_dir_all(shared_jsonl.parent().unwrap()).unwrap();
        fs::create_dir_all(home_jsonl.parent().unwrap()).unwrap();
        let base = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{REMOTE_THREAD_A}\",\"model_provider\":\"openai\"}}}}\n"
        );
        fs::write(&shared_jsonl, &base).unwrap();
        fs::write(&home_jsonl, &base).unwrap();
        create_official_like_db(
            &shared.path().join("state_5.sqlite"),
            &[(REMOTE_THREAD_A, shared_jsonl.to_str().unwrap())],
        );
        create_official_like_db(
            &home.path().join("state_5.sqlite"),
            &[(REMOTE_THREAD_A, home_jsonl.to_str().unwrap())],
        );

        for (iteration, provider) in [
            "openai_custom",
            "openai",
            "openai_custom",
            "openai",
            "openai_custom",
            "openai",
        ]
        .into_iter()
        .enumerate()
        {
            let current_paths = crate::codex_paths::local_codex_paths(home.path());
            let shared_paths = crate::codex_paths::local_codex_paths(shared.path());
            let to_shared = sync_user_home_to_shared(home.path(), shared.path()).unwrap();
            let to_shared_gc = super::cleanup_obsolete_provider_slots(
                &to_shared.obsolete_provider_slots,
                &current_paths,
                &shared_paths,
            );
            assert!(to_shared_gc.warnings.is_empty(), "{to_shared_gc:?}");
            let from_shared =
                sync_shared_to_user_home(shared.path(), home.path(), provider).unwrap();
            let from_shared_gc = super::cleanup_obsolete_provider_slots(
                &from_shared.obsolete_provider_slots,
                &current_paths,
                &shared_paths,
            );
            assert!(from_shared_gc.warnings.is_empty(), "{from_shared_gc:?}");
            let conn = Connection::open(home.path().join("state_5.sqlite")).unwrap();
            let active: String = conn
                .query_row(
                    "SELECT rollout_path FROM threads WHERE id = ?1",
                    [REMOTE_THREAD_A],
                    |row| row.get(0),
                )
                .unwrap();
            drop(conn);
            let mut file = fs::OpenOptions::new().append(true).open(active).unwrap();
            writeln!(
                file,
                "{{\"type\":\"response_item\",\"payload\":{{\"text\":\"tail-{iteration}\"}}}}"
            )
            .unwrap();
            file.sync_all().unwrap();
        }

        sync_user_home_to_shared(home.path(), shared.path()).unwrap();
        let current_files = super::walk_jsonl_files(&home.path().join("sessions")).unwrap();
        let shared_files = super::walk_jsonl_files(&shared.path().join("sessions")).unwrap();
        assert!(current_files.len() <= 3, "{current_files:?}");
        assert!(shared_files.len() <= 3, "{shared_files:?}");
        let conn = Connection::open(home.path().join("state_5.sqlite")).unwrap();
        let active: String = conn
            .query_row(
                "SELECT rollout_path FROM threads WHERE id = ?1",
                [REMOTE_THREAD_A],
                |row| row.get(0),
            )
            .unwrap();
        let active = fs::read_to_string(active).unwrap();
        for iteration in 0..6 {
            assert!(active.contains(&format!("tail-{iteration}")));
        }
    }

    #[test]
    fn runtime_storage_plan_includes_provider_rollout_and_session_index_outputs() {
        let current = tempdir().unwrap();
        let shared = tempdir().unwrap();
        let rollout = current.path().join(format!(
            "sessions/2026/07/25/rollout-2026-07-25T12-00-00-{REMOTE_THREAD_A}-imported-abc.jsonl"
        ));
        fs::create_dir_all(rollout.parent().unwrap()).unwrap();
        let source = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{REMOTE_THREAD_A}\",\"model_provider\":\"openai\"}}}}\n"
        );
        fs::write(&rollout, &source).unwrap();
        fs::write(
            current.path().join("session_index.jsonl"),
            format!("{{\"id\":\"{REMOTE_THREAD_A}\"}}\n"),
        )
        .unwrap();
        create_official_like_db(
            &current.path().join("state_5.sqlite"),
            &[(REMOTE_THREAD_A, rollout.to_str().unwrap())],
        );
        let current_paths = crate::codex_paths::resolve_user_codex_paths(current.path()).unwrap();
        let shared_paths = crate::codex_paths::local_codex_paths(shared.path());

        let demands = super::plan_runtime_session_storage_with_paths(
            &current_paths,
            &shared_paths,
            "openai_custom",
        )
        .unwrap()
        .demands();

        assert!(demands
            .iter()
            .any(|demand| demand.destination == shared_paths.state_db));
        assert!(demands
            .iter()
            .any(|demand| demand.destination == shared_paths.session_index));
        let provider = demands
            .iter()
            .find(|demand| {
                demand.destination != rollout
                    && super::is_remote_rollout_path(&demand.destination, REMOTE_THREAD_A)
            })
            .unwrap_or_else(|| {
                panic!("provider-normalized current rollout was not planned: {demands:?}")
            });
        assert!(provider.bytes > u64::try_from(source.len()).unwrap());
        let current_sessions = fs::canonicalize(&current_paths.sessions_dir).unwrap();
        let provider_parent = fs::canonicalize(provider.destination.parent().unwrap()).unwrap();
        assert!(provider_parent.starts_with(current_sessions));
    }

    #[test]
    fn provider_sync_rejects_unsafe_session_id_before_creating_a_target_file() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let source_jsonl = source
            .path()
            .join("sessions/2026/07/25/rollout-malicious.jsonl");
        fs::create_dir_all(source_jsonl.parent().unwrap()).unwrap();
        fs::write(
            &source_jsonl,
            r#"{"type":"session_meta","payload":{"id":"..\\escape","model_provider":"openai"}}"#,
        )
        .unwrap();
        create_db(
            &source.path().join("state_5.sqlite"),
            &[("..\\escape", source_jsonl.to_str().unwrap())],
        );
        create_official_like_db(&target.path().join("state_5.sqlite"), &[]);

        let error =
            sync_sessions_for_provider(&[source.path().to_path_buf()], target.path(), "openai")
                .unwrap_err();

        assert_eq!(error, "session id is not a safe UUID filename component");
        assert!(
            !target.path().join("sessions").exists()
                || super::walk_jsonl_files(&target.path().join("sessions"))
                    .unwrap()
                    .is_empty()
        );
    }

    #[test]
    fn source_database_rollout_wins_over_an_earlier_imported_filename() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let dir = source.path().join("sessions/2026/07/19");
        fs::create_dir_all(&dir).unwrap();
        let base = dir.join("rollout-thread-preferred.jsonl");
        let imported = dir.join("rollout-thread-preferred-imported-deadbeefcafe.jsonl");
        let first = concat!(
            r#"{"type":"session_meta","payload":{"id":"thread-preferred"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"text":"first"}}"#,
            "\n",
        );
        fs::write(&imported, first).unwrap();
        fs::write(
            &base,
            format!(
                "{first}{}\n",
                r#"{"type":"response_item","payload":{"text":"complete tail"}}"#
            ),
        )
        .unwrap();
        create_db(
            &source.path().join("state_5.sqlite"),
            &[("thread-preferred", base.to_str().unwrap())],
        );
        create_db(&target.path().join("state_5.sqlite"), &[]);

        sync_sessions(&[source.path().to_path_buf()], target.path()).unwrap();

        let target_base = target
            .path()
            .join("sessions/2026/07/19/rollout-thread-preferred.jsonl");
        assert!(fs::read_to_string(&target_base)
            .unwrap()
            .contains("complete tail"));
        let conn = Connection::open(target.path().join("state_5.sqlite")).unwrap();
        let rollout_path: String = conn
            .query_row(
                "SELECT rollout_path FROM threads WHERE id = 'thread-preferred'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(std::path::PathBuf::from(rollout_path), target_base);
    }

    #[test]
    fn source_database_rollout_advances_to_a_strictly_more_complete_candidate() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let dir = source.path().join("sessions/2026/07/19");
        fs::create_dir_all(&dir).unwrap();
        let base = dir.join("rollout-thread-heal.jsonl");
        let imported = dir.join("rollout-thread-heal-imported-deadbeefcafe.jsonl");
        let first = concat!(
            r#"{"type":"session_meta","payload":{"id":"thread-heal"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"text":"first"}}"#,
            "\n",
        );
        fs::write(&imported, first).unwrap();
        fs::write(
            &base,
            format!(
                "{first}{}\n",
                r#"{"type":"response_item","payload":{"text":"recovered tail"}}"#
            ),
        )
        .unwrap();
        create_db(
            &source.path().join("state_5.sqlite"),
            &[("thread-heal", imported.to_str().unwrap())],
        );
        create_db(&target.path().join("state_5.sqlite"), &[]);

        sync_sessions(&[source.path().to_path_buf()], target.path()).unwrap();

        let target_base = target
            .path()
            .join("sessions/2026/07/19/rollout-thread-heal.jsonl");
        assert!(fs::read_to_string(&target_base)
            .unwrap()
            .contains("recovered tail"));
        let conn = Connection::open(target.path().join("state_5.sqlite")).unwrap();
        let rollout_path: String = conn
            .query_row(
                "SELECT rollout_path FROM threads WHERE id = 'thread-heal'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(std::path::PathBuf::from(rollout_path), target_base);
    }

    #[test]
    fn divergent_larger_candidate_does_not_override_the_database_rollout() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let dir = source.path().join("sessions/2026/07/19");
        fs::create_dir_all(&dir).unwrap();
        let base = dir.join("rollout-thread-branch.jsonl");
        let imported = dir.join("rollout-thread-branch-imported-deadbeefcafe.jsonl");
        fs::write(
            &imported,
            concat!(
                r#"{"type":"session_meta","payload":{"id":"thread-branch"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"text":"active branch"}}"#,
                "\n",
            ),
        )
        .unwrap();
        fs::write(
            &base,
            concat!(
                r#"{"type":"session_meta","payload":{"id":"thread-branch"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"text":"other branch"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"text":"larger but divergent"}}"#,
                "\n",
            ),
        )
        .unwrap();
        create_db(
            &source.path().join("state_5.sqlite"),
            &[("thread-branch", imported.to_str().unwrap())],
        );
        create_db(&target.path().join("state_5.sqlite"), &[]);

        sync_sessions(&[source.path().to_path_buf()], target.path()).unwrap();

        let target_imported = target
            .path()
            .join("sessions/2026/07/19/rollout-thread-branch-imported-deadbeefcafe.jsonl");
        assert!(fs::read_to_string(&target_imported)
            .unwrap()
            .contains("active branch"));
        let conn = Connection::open(target.path().join("state_5.sqlite")).unwrap();
        let rollout_path: String = conn
            .query_row(
                "SELECT rollout_path FROM threads WHERE id = 'thread-branch'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(std::path::PathBuf::from(rollout_path), target_imported);
    }

    #[test]
    fn shorter_source_copy_does_not_hijack_a_more_complete_target_thread() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let relative = "sessions/2026/07/19";
        let source_jsonl = source
            .path()
            .join(relative)
            .join("rollout-thread-complete-imported-aaaaaaaaaaaa.jsonl");
        let target_jsonl = target
            .path()
            .join(relative)
            .join("rollout-thread-complete.jsonl");
        fs::create_dir_all(source_jsonl.parent().unwrap()).unwrap();
        fs::create_dir_all(target_jsonl.parent().unwrap()).unwrap();
        let first = concat!(
            r#"{"type":"session_meta","payload":{"id":"thread-complete"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"text":"first"}}"#,
            "\n",
        );
        fs::write(&source_jsonl, first).unwrap();
        fs::write(
            &target_jsonl,
            format!(
                "{first}{}\n",
                r#"{"type":"response_item","payload":{"text":"target-only tail"}}"#
            ),
        )
        .unwrap();
        create_db(
            &source.path().join("state_5.sqlite"),
            &[("thread-complete", source_jsonl.to_str().unwrap())],
        );
        create_db(
            &target.path().join("state_5.sqlite"),
            &[("thread-complete", target_jsonl.to_str().unwrap())],
        );

        sync_sessions(&[source.path().to_path_buf()], target.path()).unwrap();

        let conn = Connection::open(target.path().join("state_5.sqlite")).unwrap();
        let rollout_path: String = conn
            .query_row(
                "SELECT rollout_path FROM threads WHERE id = 'thread-complete'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(std::path::PathBuf::from(&rollout_path), target_jsonl);
        assert!(fs::read_to_string(&rollout_path)
            .unwrap()
            .contains("target-only tail"));
    }

    #[test]
    fn provider_only_metadata_change_does_not_create_an_imported_conflict() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let relative =
            format!("sessions/2026/07/19/rollout-2026-07-19T12-00-00-{REMOTE_THREAD_A}.jsonl");
        let source_jsonl = source.path().join(&relative);
        let target_jsonl = target.path().join(&relative);
        fs::create_dir_all(source_jsonl.parent().unwrap()).unwrap();
        fs::create_dir_all(target_jsonl.parent().unwrap()).unwrap();
        fs::write(
            &source_jsonl,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{REMOTE_THREAD_A}\",\"model_provider\":\"relay\"}}}}\n\
                 {{\"type\":\"response_item\",\"payload\":{{\"text\":\"same history\"}}}}\n"
            ),
        )
        .unwrap();
        fs::write(
            &target_jsonl,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{REMOTE_THREAD_A}\",\"model_provider\":\"openai\"}}}}\n\
                 {{\"type\":\"response_item\",\"payload\":{{\"text\":\"same history\"}}}}\n"
            ),
        )
        .unwrap();
        create_db(
            &source.path().join("state_5.sqlite"),
            &[(REMOTE_THREAD_A, source_jsonl.to_str().unwrap())],
        );
        create_db(
            &target.path().join("state_5.sqlite"),
            &[(REMOTE_THREAD_A, target_jsonl.to_str().unwrap())],
        );

        sync_sessions_for_provider(&[source.path().to_path_buf()], target.path(), "openai")
            .unwrap();

        let imported = fs::read_dir(target_jsonl.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains("-imported-"))
            .count();
        assert_eq!(imported, 0);
        let conn = Connection::open(target.path().join("state_5.sqlite")).unwrap();
        let rollout_path: String = conn
            .query_row(
                "SELECT rollout_path FROM threads WHERE id = ?1",
                [REMOTE_THREAD_A],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            fs::canonicalize(rollout_path).unwrap(),
            fs::canonicalize(target_jsonl).unwrap()
        );
    }

    #[test]
    fn divergent_imported_copy_uses_the_base_stem_instead_of_chaining_suffixes() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let relative = "sessions/2026/07/19/rollout-thread-chain-imported-aaaaaaaaaaaa.jsonl";
        let source_jsonl = source.path().join(relative);
        let target_jsonl = target.path().join(relative);
        fs::create_dir_all(source_jsonl.parent().unwrap()).unwrap();
        fs::create_dir_all(target_jsonl.parent().unwrap()).unwrap();
        fs::write(
            &source_jsonl,
            r#"{"type":"session_meta","payload":{"id":"thread-chain","source":"source"}}"#,
        )
        .unwrap();
        fs::write(
            &target_jsonl,
            r#"{"type":"session_meta","payload":{"id":"thread-chain","source":"target"}}"#,
        )
        .unwrap();
        create_db(
            &source.path().join("state_5.sqlite"),
            &[("thread-chain", source_jsonl.to_str().unwrap())],
        );
        create_db(
            &target.path().join("state_5.sqlite"),
            &[("thread-chain", target_jsonl.to_str().unwrap())],
        );

        sync_sessions(&[source.path().to_path_buf()], target.path()).unwrap();

        let names = fs::read_dir(target_jsonl.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect::<Vec<_>>();
        assert!(names.iter().any(|name| {
            name.starts_with("rollout-thread-chain-imported-")
                && !name.contains("aaaaaaaaaaaa-imported")
        }));
    }

    #[test]
    fn hot_sync_preserves_active_rollout_when_shorter_candidate_has_a_different_name() {
        let shared = tempdir().unwrap();
        let home = tempdir().unwrap();
        let shared_jsonl = shared
            .path()
            .join("sessions/2026/07/19/rollout-thread-hot-short-imported-aaaaaaaaaaaa.jsonl");
        let home_jsonl = home
            .path()
            .join("sessions/2026/07/19/rollout-thread-hot-short.jsonl");
        let copied_candidate = home
            .path()
            .join("sessions/2026/07/19/rollout-thread-hot-short-imported-aaaaaaaaaaaa.jsonl");
        fs::create_dir_all(shared_jsonl.parent().unwrap()).unwrap();
        fs::create_dir_all(home_jsonl.parent().unwrap()).unwrap();
        fs::write(
            &shared_jsonl,
            concat!(
                r#"{"type":"session_meta","payload":{"id":"thread-hot-short","model_provider":"openai_custom"}}"#,
                "\n",
            ),
        )
        .unwrap();
        let live_bytes = concat!(
            r#"{"type":"session_meta","payload":{"id":"thread-hot-short","model_provider":"live-provider"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"text":"live tail"}}"#,
            "\n",
        );
        fs::write(&home_jsonl, live_bytes).unwrap();
        create_db(
            &shared.path().join("state_5.sqlite"),
            &[("thread-hot-short", shared_jsonl.to_str().unwrap())],
        );
        create_db(
            &home.path().join("state_5.sqlite"),
            &[("thread-hot-short", home_jsonl.to_str().unwrap())],
        );
        set_provider(home.path(), "live-provider");

        let result = sync_shared_to_user_home_hot(shared.path(), home.path(), "openai").unwrap();

        assert_eq!(result.copied_session_files, 0);
        assert!(!copied_candidate.exists());
        assert_eq!(fs::read(&home_jsonl).unwrap(), live_bytes.as_bytes());
        let conn = Connection::open(home.path().join("state_5.sqlite")).unwrap();
        let (provider, rollout_path): (String, String) = conn
            .query_row(
                "SELECT model_provider, rollout_path
                 FROM threads WHERE id = 'thread-hot-short'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(provider, "live-provider");
        assert_eq!(std::path::PathBuf::from(rollout_path), home_jsonl);
    }

    #[test]
    fn hot_sync_keeps_the_live_rollout_visible_when_a_longer_different_name_arrives() {
        let shared = tempdir().unwrap();
        let home = tempdir().unwrap();
        let shared_jsonl = shared
            .path()
            .join("sessions/2026/07/19/rollout-thread-hot-long-imported-aaaaaaaaaaaa.jsonl");
        let home_jsonl = home
            .path()
            .join("sessions/2026/07/19/rollout-thread-hot-long.jsonl");
        let copied_candidate = home
            .path()
            .join("sessions/2026/07/19/rollout-thread-hot-long-imported-aaaaaaaaaaaa.jsonl");
        fs::create_dir_all(shared_jsonl.parent().unwrap()).unwrap();
        fs::create_dir_all(home_jsonl.parent().unwrap()).unwrap();
        let live_prefix = concat!(
            r#"{"type":"session_meta","payload":{"id":"thread-hot-long","model_provider":"live-provider"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"text":"visible before sync"}}"#,
            "\n",
        );
        let shared_history = concat!(
            r#"{"type":"session_meta","payload":{"id":"thread-hot-long","model_provider":"shared-provider"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"text":"visible before sync"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"text":"shared longer history"}}"#,
            "\n",
        );
        fs::write(&shared_jsonl, shared_history).unwrap();
        fs::write(&home_jsonl, live_prefix).unwrap();
        create_official_like_db(
            &shared.path().join("state_5.sqlite"),
            &[("thread-hot-long", shared_jsonl.to_str().unwrap())],
        );
        create_official_like_db(
            &home.path().join("state_5.sqlite"),
            &[("thread-hot-long", home_jsonl.to_str().unwrap())],
        );
        let home_conn = Connection::open(home.path().join("state_5.sqlite")).unwrap();
        home_conn
            .execute(
                "UPDATE threads
                 SET model_provider = 'live-provider', title = 'Visible live title'
                 WHERE id = 'thread-hot-long'",
                [],
            )
            .unwrap();
        drop(home_conn);
        let mut live_writer = fs::OpenOptions::new()
            .append(true)
            .open(&home_jsonl)
            .unwrap();

        let result =
            sync_shared_to_user_home_hot(shared.path(), home.path(), "current-provider").unwrap();
        live_writer
            .write_all(
                concat!(
                    r#"{"type":"response_item","payload":{"text":"writer after sync"}}"#,
                    "\n",
                )
                .as_bytes(),
            )
            .unwrap();
        live_writer.sync_data().unwrap();
        drop(live_writer);

        assert_eq!(result.copied_session_files, 0);
        assert!(!copied_candidate.exists());
        let conn = Connection::open(home.path().join("state_5.sqlite")).unwrap();
        let (rollout_path, provider, title): (String, String, String) = conn
            .query_row(
                "SELECT rollout_path, model_provider, title
                 FROM threads WHERE id = 'thread-hot-long'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(std::path::PathBuf::from(&rollout_path), home_jsonl);
        assert_eq!(provider, "live-provider");
        assert_eq!(title, "Visible live title");
        let visible_history = fs::read_to_string(rollout_path).unwrap();
        assert!(visible_history.contains("visible before sync"));
        assert!(visible_history.contains("writer after sync"));
    }

    #[test]
    fn duplicate_thread_repairs_an_existing_rollout_outside_the_target_home() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let external = tempdir().unwrap();
        let source_jsonl = source.path().join(format!(
            "sessions/2026/07/19/rollout-2026-07-19T12-00-00-{REMOTE_THREAD_A}.jsonl"
        ));
        let external_jsonl = external.path().join(format!(
            "rollout-2026-07-19T12-00-00-{REMOTE_THREAD_A}.jsonl"
        ));
        fs::create_dir_all(source_jsonl.parent().unwrap()).unwrap();
        let source_bytes = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{REMOTE_THREAD_A}\",\"model_provider\":\"openai_custom\"}}}}\n"
        );
        let external_bytes = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{REMOTE_THREAD_A}\",\"model_provider\":\"external-provider\"}}}}\n"
        );
        fs::write(&source_jsonl, &source_bytes).unwrap();
        fs::write(&external_jsonl, &external_bytes).unwrap();
        create_db(
            &source.path().join("state_5.sqlite"),
            &[(REMOTE_THREAD_A, source_jsonl.to_str().unwrap())],
        );
        create_db(
            &target.path().join("state_5.sqlite"),
            &[(REMOTE_THREAD_A, external_jsonl.to_str().unwrap())],
        );

        sync_sessions_for_provider(&[source.path().to_path_buf()], target.path(), "openai")
            .unwrap();

        assert_eq!(
            fs::read(&external_jsonl).unwrap(),
            external_bytes.as_bytes()
        );
        let conn = Connection::open(target.path().join("state_5.sqlite")).unwrap();
        let rollout_path: String = conn
            .query_row(
                "SELECT rollout_path FROM threads WHERE id = ?1",
                [REMOTE_THREAD_A],
                |row| row.get(0),
            )
            .unwrap();
        assert!(fs::canonicalize(&rollout_path)
            .unwrap()
            .starts_with(fs::canonicalize(target.path()).unwrap()));
    }

    #[test]
    fn source_database_rollout_matches_the_full_sessions_relative_path() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let stale = source
            .path()
            .join("sessions/2026/07/18/rollout-thread-same-name.jsonl");
        let active = source
            .path()
            .join("sessions/2026/07/19/rollout-thread-same-name.jsonl");
        fs::create_dir_all(stale.parent().unwrap()).unwrap();
        fs::create_dir_all(active.parent().unwrap()).unwrap();
        fs::write(
            &stale,
            concat!(
                r#"{"type":"session_meta","payload":{"id":"thread-same-name"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"text":"stale branch"}}"#,
                "\n",
            ),
        )
        .unwrap();
        fs::write(
            &active,
            concat!(
                r#"{"type":"session_meta","payload":{"id":"thread-same-name"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"text":"active branch"}}"#,
                "\n",
            ),
        )
        .unwrap();
        create_db(
            &source.path().join("state_5.sqlite"),
            &[("thread-same-name", active.to_str().unwrap())],
        );
        create_db(&target.path().join("state_5.sqlite"), &[]);

        sync_sessions(&[source.path().to_path_buf()], target.path()).unwrap();

        let copied = target
            .path()
            .join("sessions/2026/07/19/rollout-thread-same-name.jsonl");
        assert!(fs::read_to_string(copied)
            .unwrap()
            .contains("active branch"));
    }

    #[test]
    fn ambiguous_strict_extensions_do_not_replace_the_database_rollout() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let dir = source.path().join("sessions/2026/07/19");
        fs::create_dir_all(&dir).unwrap();
        let active = dir.join("rollout-thread-ambiguous.jsonl");
        let branch_a = dir.join("rollout-thread-ambiguous-imported-aaaaaaaaaaaa.jsonl");
        let branch_b = dir.join("rollout-thread-ambiguous-imported-bbbbbbbbbbbb.jsonl");
        let prefix = concat!(
            r#"{"type":"session_meta","payload":{"id":"thread-ambiguous"}}"#,
            "\n",
        );
        fs::write(&active, prefix).unwrap();
        fs::write(
            &branch_a,
            format!(
                "{prefix}{}\n",
                r#"{"type":"response_item","payload":{"text":"branch a"}}"#
            ),
        )
        .unwrap();
        fs::write(
            &branch_b,
            format!(
                "{prefix}{}\n",
                r#"{"type":"response_item","payload":{"text":"branch b"}}"#
            ),
        )
        .unwrap();
        create_db(
            &source.path().join("state_5.sqlite"),
            &[("thread-ambiguous", active.to_str().unwrap())],
        );
        create_db(&target.path().join("state_5.sqlite"), &[]);

        sync_sessions(&[source.path().to_path_buf()], target.path()).unwrap();

        let copied = target
            .path()
            .join("sessions/2026/07/19/rollout-thread-ambiguous.jsonl");
        assert_eq!(fs::read_to_string(copied).unwrap(), prefix);
    }

    fn set_provider(home: &std::path::Path, provider: &str) {
        let conn = Connection::open(home.join("state_5.sqlite")).unwrap();
        conn.execute("ALTER TABLE threads ADD COLUMN model_provider TEXT", [])
            .unwrap();
        conn.execute("UPDATE threads SET model_provider = ?1", [provider])
            .unwrap();
    }
}

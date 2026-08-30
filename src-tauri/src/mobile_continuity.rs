use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs,
    io::{BufRead, Read},
    path::{Component, Path, PathBuf},
    time::UNIX_EPOCH,
};

use crate::{
    codex_paths::CodexPaths,
    file_ops::atomic_write,
    operation_log::timestamp_millis,
    runtime_store::default_store_root,
    session_sync::{
        is_remote_rollout_path, publish_selected_user_home_provider_for_mobile_between_paths,
        validate_remote_thread_id,
    },
};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const STATE_VERSION: u32 = 1;
const MAX_STATE_BYTES: u64 = 8 * 1024 * 1024;
#[cfg(test)]
const MAX_AUTO_PUBLISH_THREADS: usize = 8;
const MAX_AUTO_PUBLISH_BYTES: u64 = 8 * 1024 * 1024;
const RELAY_PROVIDER: &str = "openai_custom";
const ACCOUNT_PROVIDER: &str = "openai";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum MobileContinuityItemStatus {
    Queued,
    Publishing,
    RemotePublished,
    Partial,
    Conflict,
    Retrying,
    NeedsManual,
    Paused,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MobileContinuityItem {
    pub thread_id: String,
    pub status: MobileContinuityItemStatus,
    pub attempts: u32,
    pub next_retry_at_ms: Option<u128>,
    pub updated_at_ms: u128,
    pub failure_category: Option<String>,
    #[serde(default)]
    pub source_fingerprint: Option<SourceFingerprint>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourceFingerprint {
    pub size: u64,
    pub modified_at_ms: u128,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MobileContinuityStatus {
    pub enabled: bool,
    pub notice_pending: bool,
    pub initialized_at_ms: u128,
    pub queued: usize,
    pub publishing: usize,
    pub remote_published: usize,
    pub partial: usize,
    pub conflict: usize,
    pub needs_manual: usize,
    pub items: Vec<MobileContinuityItem>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MobileContinuityPreparation {
    pub detected_threads: usize,
    pub published_threads: usize,
    pub deferred_threads: usize,
    pub partial_threads: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MobileContinuityState {
    version: u32,
    enabled: bool,
    notice_pending: bool,
    initialized_at_ms: u128,
    known_thread_ids: BTreeSet<String>,
    items: BTreeMap<String, MobileContinuityItem>,
}

#[derive(Debug, Clone)]
struct ObservedThread {
    id: String,
    provider: Option<String>,
    rollout_path: Option<PathBuf>,
    archived: bool,
}

pub fn default_state_path() -> Result<PathBuf, String> {
    Ok(default_store_root()?.join("mobile-continuity-v1.json"))
}

pub fn initialize_status(
    state_path: &Path,
    current: &CodexPaths,
) -> Result<MobileContinuityStatus, String> {
    let mut state = load_or_initialize(state_path, current)?;
    if reconcile_completed_publications(&mut state, current)? {
        save_state(state_path, &state)?;
    }
    Ok(status_from_state(&state))
}

pub fn set_enabled(
    state_path: &Path,
    current: &CodexPaths,
    enabled: bool,
) -> Result<MobileContinuityStatus, String> {
    let mut state = load_or_initialize(state_path, current)?;
    state.known_thread_ids.extend(
        observe_threads(current)?
            .into_iter()
            .map(|thread| thread.id),
    );
    state.enabled = enabled;
    save_state(state_path, &state)?;
    Ok(status_from_state(&state))
}

pub fn acknowledge_notice(
    state_path: &Path,
    current: &CodexPaths,
) -> Result<MobileContinuityStatus, String> {
    let mut state = load_or_initialize(state_path, current)?;
    state.notice_pending = false;
    save_state(state_path, &state)?;
    Ok(status_from_state(&state))
}

#[cfg(test)]
pub(crate) fn prepare_account_publication(
    state_path: &Path,
    current: &CodexPaths,
) -> Result<MobileContinuityPreparation, String> {
    prepare_account_publication_between(state_path, current, current)
}

#[cfg(test)]
pub(crate) fn prepare_account_publication_between(
    state_path: &Path,
    source: &CodexPaths,
    target: &CodexPaths,
) -> Result<MobileContinuityPreparation, String> {
    let mut state = load_existing_state(state_path)?
        .ok_or_else(|| "手机连续性尚未初始化；本次不自动处理既有会话".to_string())?;
    if !state.enabled {
        return Ok(MobileContinuityPreparation {
            detected_threads: 0,
            published_threads: 0,
            deferred_threads: 0,
            partial_threads: 0,
        });
    }

    let now = timestamp_millis()?;
    let observed = observe_threads(source)?;
    let mut newly_queued = Vec::new();
    for thread in &observed {
        if state.known_thread_ids.insert(thread.id.clone())
            && !thread.archived
            && thread.provider.as_deref() == Some(RELAY_PROVIDER)
        {
            state.items.insert(
                thread.id.clone(),
                MobileContinuityItem {
                    thread_id: thread.id.clone(),
                    status: MobileContinuityItemStatus::Queued,
                    attempts: 0,
                    next_retry_at_ms: None,
                    updated_at_ms: now,
                    failure_category: None,
                    source_fingerprint: thread
                        .rollout_path
                        .as_deref()
                        .and_then(|path| managed_rollout_path(source, path, &thread.id))
                        .and_then(|path| stable_source_fingerprint(&path).ok()),
                },
            );
            newly_queued.push(thread.id.clone());
        }
    }
    save_state(state_path, &state)?;

    let observed_paths = observed
        .iter()
        .filter_map(|thread| {
            (!thread.archived).then_some(thread).and_then(|thread| {
                thread
                    .rollout_path
                    .as_ref()
                    .and_then(|path| managed_rollout_path(source, path, &thread.id))
                    .map(|path| (thread.id.as_str(), path))
            })
        })
        .collect::<BTreeMap<_, _>>();
    let mut selected = HashSet::new();
    let mut selected_bytes = 0_u64;
    for (id, item) in &state.items {
        if !matches!(
            item.status,
            MobileContinuityItemStatus::Queued
                | MobileContinuityItemStatus::Retrying
                | MobileContinuityItemStatus::Paused
        ) {
            continue;
        }
        let Some(path) = observed_paths.get(id.as_str()) else {
            continue;
        };
        let Ok(fingerprint) = stable_source_fingerprint(path) else {
            continue;
        };
        let file_bytes = fingerprint.size;
        let Some(next_bytes) = selected_bytes.checked_add(file_bytes) else {
            continue;
        };
        if selected.len() >= MAX_AUTO_PUBLISH_THREADS || next_bytes > MAX_AUTO_PUBLISH_BYTES {
            continue;
        }
        selected.insert(id.clone());
        selected_bytes = next_bytes;
    }
    if selected.is_empty() {
        return Ok(MobileContinuityPreparation {
            detected_threads: newly_queued.len(),
            published_threads: 0,
            deferred_threads: state
                .items
                .values()
                .filter(|item| {
                    matches!(
                        item.status,
                        MobileContinuityItemStatus::Queued
                            | MobileContinuityItemStatus::Retrying
                            | MobileContinuityItemStatus::Paused
                    )
                })
                .count(),
            partial_threads: 0,
        });
    }

    for id in &selected {
        if let Some(item) = state.items.get_mut(id) {
            item.source_fingerprint = observed_paths
                .get(id.as_str())
                .and_then(|path| stable_source_fingerprint(path).ok());
            item.status = MobileContinuityItemStatus::Publishing;
            item.attempts = item.attempts.saturating_add(1);
            item.updated_at_ms = now;
        }
    }
    save_state(state_path, &state)?;

    let publish_result = publish_selected_user_home_provider_for_mobile_between_paths(
        source,
        target,
        &selected,
        ACCOUNT_PROVIDER,
    );
    let completed_at = timestamp_millis()?;
    match publish_result {
        Ok(result) => {
            let observations = observe_threads(target)?
                .into_iter()
                .filter(|thread| !thread.archived)
                .map(|thread| (thread.id.clone(), thread))
                .collect::<BTreeMap<_, _>>();
            let mut published = 0;
            for id in &selected {
                if result.preserved_divergent_thread_ids.contains(id) {
                    if let Some(item) = state.items.get_mut(id) {
                        item.status = MobileContinuityItemStatus::Conflict;
                        item.failure_category = Some("conflict".to_string());
                        item.updated_at_ms = completed_at;
                    }
                    continue;
                }
                let (status, category) = match observations.get(id) {
                    Some(thread)
                        if thread.provider.as_deref() == Some(ACCOUNT_PROVIDER)
                            && thread
                                .rollout_path
                                .as_deref()
                                .is_some_and(|path| is_remote_rollout(target, path, id)) =>
                    {
                        let remote_path = managed_rollout_path(
                            target,
                            thread.rollout_path.as_deref().expect("checked above"),
                            id,
                        )
                        .expect("remote rollout was validated above");
                        let _ = remote_path;
                        published += 1;
                        (MobileContinuityItemStatus::RemotePublished, None)
                    }
                    _ => (
                        MobileContinuityItemStatus::NeedsManual,
                        Some("remoteEnumerationMismatch".to_string()),
                    ),
                };
                if let Some(item) = state.items.get_mut(id) {
                    item.status = status;
                    item.failure_category = category;
                    item.updated_at_ms = completed_at;
                }
            }
            save_state(state_path, &state)?;
            Ok(MobileContinuityPreparation {
                detected_threads: newly_queued.len(),
                published_threads: published,
                deferred_threads: state
                    .items
                    .values()
                    .filter(|item| {
                        matches!(
                            item.status,
                            MobileContinuityItemStatus::Queued
                                | MobileContinuityItemStatus::Retrying
                                | MobileContinuityItemStatus::Paused
                        )
                    })
                    .count(),
                partial_threads: 0,
            })
        }
        Err(error) => {
            for id in &selected {
                if let Some(item) = state.items.get_mut(id) {
                    let category = classify_publish_error(&error);
                    item.status = if category == "conflict" {
                        MobileContinuityItemStatus::Conflict
                    } else {
                        MobileContinuityItemStatus::NeedsManual
                    };
                    item.failure_category = Some(category.to_string());
                    item.updated_at_ms = completed_at;
                }
            }
            save_state(state_path, &state)?;
            Err("手机连续性发布未完成；请求端切换不受影响，可稍后手动同步".to_string())
        }
    }
}

pub(crate) fn publish_single_account_session(
    state_path: &Path,
    current: &CodexPaths,
    thread_id: &str,
) -> Result<MobileContinuityStatus, String> {
    publish_single_account_session_between(state_path, current, current, thread_id)
}

pub(crate) fn publish_single_account_session_between(
    state_path: &Path,
    source: &CodexPaths,
    target: &CodexPaths,
    thread_id: &str,
) -> Result<MobileContinuityStatus, String> {
    validate_remote_thread_id(thread_id)?;
    let mut state = load_or_initialize(state_path, source)?;
    let now = timestamp_millis()?;
    state.known_thread_ids.insert(thread_id.to_string());
    let item = state
        .items
        .entry(thread_id.to_string())
        .or_insert(MobileContinuityItem {
            thread_id: thread_id.to_string(),
            status: MobileContinuityItemStatus::Queued,
            attempts: 0,
            next_retry_at_ms: None,
            updated_at_ms: now,
            failure_category: None,
            source_fingerprint: None,
        });
    let source_path = observe_threads(source)?
        .into_iter()
        .find(|thread| {
            thread.id == thread_id
                && !thread.archived
                && thread.provider.as_deref() == Some(RELAY_PROVIDER)
        })
        .and_then(|thread| {
            thread
                .rollout_path
                .as_deref()
                .and_then(|path| managed_rollout_path(source, path, thread_id))
        })
        .ok_or_else(|| "该会话没有可安全读取的受管 JSONL；请使用完全同步".to_string())?;
    item.source_fingerprint = Some(stable_source_fingerprint(&source_path)?);
    item.status = MobileContinuityItemStatus::Publishing;
    item.attempts = item.attempts.saturating_add(1);
    item.updated_at_ms = now;
    save_state(state_path, &state)?;

    let selected = HashSet::from([thread_id.to_string()]);
    let publish_result = publish_selected_user_home_provider_for_mobile_between_paths(
        source,
        target,
        &selected,
        ACCOUNT_PROVIDER,
    );
    let result = match publish_result {
        Ok(result) => result,
        Err(error) => {
            let item = state.items.get_mut(thread_id).expect("inserted above");
            item.status = if classify_publish_error(&error) == "conflict" {
                MobileContinuityItemStatus::Conflict
            } else {
                MobileContinuityItemStatus::NeedsManual
            };
            item.failure_category = Some(classify_publish_error(&error).to_string());
            item.updated_at_ms = timestamp_millis()?;
            save_state(state_path, &state)?;
            return Err(
                "该会话未能安全发布；原会话未覆盖，请使用完全同步处理冲突或异常".to_string(),
            );
        }
    };

    if result.preserved_divergent_thread_ids.contains(thread_id) {
        let item = state.items.get_mut(thread_id).expect("inserted above");
        item.status = MobileContinuityItemStatus::Conflict;
        item.failure_category = Some("conflict".to_string());
        item.updated_at_ms = timestamp_millis()?;
        save_state(state_path, &state)?;
        return Ok(status_from_state(&state));
    }

    let observed = observe_threads(target)?
        .into_iter()
        .filter(|thread| !thread.archived)
        .find(|thread| thread.id == thread_id);
    let (status, failure_category) = match observed {
        Some(thread)
            if thread.provider.as_deref() == Some(ACCOUNT_PROVIDER)
                && thread
                    .rollout_path
                    .as_deref()
                    .is_some_and(|path| is_remote_rollout(target, path, thread_id)) =>
        {
            let remote_path = managed_rollout_path(
                target,
                thread.rollout_path.as_deref().expect("checked above"),
                thread_id,
            )
            .expect("remote rollout was validated above");
            let _ = remote_path;
            (MobileContinuityItemStatus::RemotePublished, None)
        }
        _ => (
            MobileContinuityItemStatus::NeedsManual,
            Some("remoteEnumerationMismatch".to_string()),
        ),
    };
    let item = state.items.get_mut(thread_id).expect("inserted above");
    item.status = status;
    item.failure_category = failure_category;
    item.updated_at_ms = timestamp_millis()?;
    save_state(state_path, &state)?;
    Ok(status_from_state(&state))
}

fn load_or_initialize(
    state_path: &Path,
    current: &CodexPaths,
) -> Result<MobileContinuityState, String> {
    if let Some(state) = load_existing_state(state_path)? {
        return Ok(state);
    }
    let initialized_at_ms = timestamp_millis()?;
    let known_thread_ids = observe_threads(current)?
        .into_iter()
        .map(|thread| thread.id)
        .collect();
    let state = MobileContinuityState {
        version: STATE_VERSION,
        enabled: true,
        notice_pending: true,
        initialized_at_ms,
        known_thread_ids,
        items: BTreeMap::new(),
    };
    save_state(state_path, &state)?;
    Ok(state)
}

fn reconcile_completed_publications(
    state: &mut MobileContinuityState,
    current: &CodexPaths,
) -> Result<bool, String> {
    let observations = observe_threads(current)?
        .into_iter()
        .filter(|thread| !thread.archived)
        .map(|thread| (thread.id.clone(), thread))
        .collect::<BTreeMap<_, _>>();
    let mut changed = false;
    for item in state.items.values_mut() {
        if !matches!(
            item.status,
            MobileContinuityItemStatus::Queued
                | MobileContinuityItemStatus::Retrying
                | MobileContinuityItemStatus::Paused
        ) {
            continue;
        }
        let Some(thread) = observations.get(&item.thread_id) else {
            continue;
        };
        let Some(path) = thread.rollout_path.as_deref() else {
            continue;
        };
        if thread.provider.as_deref() != Some(ACCOUNT_PROVIDER)
            || !is_remote_rollout(current, path, &item.thread_id)
        {
            continue;
        }
        let Some(path) = managed_rollout_path(current, path, &item.thread_id) else {
            continue;
        };
        let _ = path;
        item.failure_category = None;
        item.status = MobileContinuityItemStatus::RemotePublished;
        item.next_retry_at_ms = None;
        item.updated_at_ms = timestamp_millis()?;
        changed = true;
    }
    Ok(changed)
}

fn load_existing_state(state_path: &Path) -> Result<Option<MobileContinuityState>, String> {
    let bytes = match fs::read(state_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("failed to read mobile continuity state: {error}")),
    };
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err("mobile continuity state exceeded the size limit".to_string());
    }
    let mut state = serde_json::from_slice::<MobileContinuityState>(&bytes)
        .map_err(|_| "mobile continuity state is invalid; use manual sync".to_string())?;
    if state.version != STATE_VERSION {
        return Err("mobile continuity state version is unsupported; use manual sync".to_string());
    }
    for item in state.items.values_mut() {
        if item.status == MobileContinuityItemStatus::Publishing {
            item.status = MobileContinuityItemStatus::Paused;
        }
    }
    Ok(Some(state))
}

fn save_state(state_path: &Path, state: &MobileContinuityState) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(state)
        .map_err(|_| "failed to serialize mobile continuity state".to_string())?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err("mobile continuity state exceeded the size limit".to_string());
    }
    atomic_write(state_path, &bytes)
}

fn observe_threads(current: &CodexPaths) -> Result<Vec<ObservedThread>, String> {
    match fs::symlink_metadata(&current.state_db) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => return Err("mobile continuity state database is not a regular file".to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err("failed to inspect mobile continuity state database".to_string()),
    }
    let connection = Connection::open_with_flags(
        &current.state_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| {
        redacted_sqlite_failure("failed to open mobile continuity state database", &error)
    })?;
    let columns = connection
        .prepare("PRAGMA table_info(threads)")
        .and_then(|mut statement| {
            statement
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<Result<Vec<_>, _>>()
        })
        .map_err(|error| {
            redacted_sqlite_failure("failed to inspect mobile continuity schema", &error)
        })?;
    if !columns.iter().any(|column| column == "id") {
        return Ok(Vec::new());
    }
    let provider = columns.iter().any(|column| column == "model_provider");
    let rollout = columns.iter().any(|column| column == "rollout_path");
    let archived = columns.iter().any(|column| column == "archived");
    let sql = format!(
        "SELECT id, {}, {}, {} FROM threads",
        if provider { "model_provider" } else { "NULL" },
        if rollout { "rollout_path" } else { "NULL" },
        if archived { "archived" } else { "0" },
    );
    let mut statement = connection.prepare(&sql).map_err(|error| {
        redacted_sqlite_failure("failed to prepare mobile continuity query", &error)
    })?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .map_err(|error| {
            redacted_sqlite_failure("failed to read mobile continuity threads", &error)
        })?;
    let mut output = Vec::new();
    for row in rows {
        let (id, provider, rollout_path, archived) = row.map_err(|error| {
            redacted_sqlite_failure("failed to collect mobile continuity threads", &error)
        })?;
        if validate_remote_thread_id(&id).is_err() {
            continue;
        }
        output.push(ObservedThread {
            id,
            provider,
            rollout_path: rollout_path.map(PathBuf::from),
            archived: archived != 0,
        });
    }
    Ok(output)
}

fn redacted_sqlite_failure(context: &str, error: &rusqlite::Error) -> String {
    match error.sqlite_error() {
        Some(sqlite) => format!(
            "{context} (SQLite code {:?}, extended {})",
            sqlite.code, sqlite.extended_code
        ),
        None => context.to_string(),
    }
}

fn is_remote_rollout(current: &CodexPaths, rollout_path: &Path, thread_id: &str) -> bool {
    let Some(candidate) = managed_rollout_path(current, rollout_path, thread_id) else {
        return false;
    };
    candidate.is_file()
        && is_remote_rollout_path(&candidate, thread_id)
        && first_meta_matches_thread(&candidate, thread_id)
}

fn managed_rollout_path(
    current: &CodexPaths,
    rollout_path: &Path,
    thread_id: &str,
) -> Option<PathBuf> {
    let candidate = if rollout_path.is_absolute() {
        rollout_path.to_path_buf()
    } else {
        current.codex_home.join(rollout_path)
    };
    let relative = candidate.strip_prefix(&current.sessions_dir).ok()?;
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return None;
    }
    let sessions_root = fs::canonicalize(&current.sessions_dir).ok()?;
    let candidate = fs::canonicalize(candidate).ok()?;
    if !candidate.starts_with(&sessions_root)
        || candidate.extension().and_then(|value| value.to_str()) != Some("jsonl")
        || !candidate
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|name| name.contains(thread_id))
    {
        return None;
    }
    Some(candidate)
}

fn stable_source_fingerprint(path: &Path) -> Result<SourceFingerprint, String> {
    let before = fs::metadata(path)
        .map_err(|_| "mobile continuity source metadata is unavailable".to_string())?;
    if before.len() > MAX_AUTO_PUBLISH_BYTES {
        return Err("mobile continuity source exceeded the automatic capacity limit".to_string());
    }
    let bytes =
        fs::read(path).map_err(|_| "mobile continuity source is unavailable".to_string())?;
    let after = fs::metadata(path)
        .map_err(|_| "mobile continuity source metadata is unavailable".to_string())?;
    let before_modified = before
        .modified()
        .map_err(|_| "mobile continuity source timestamp is unavailable".to_string())?;
    let after_modified = after
        .modified()
        .map_err(|_| "mobile continuity source timestamp is unavailable".to_string())?;
    if before.len() != after.len()
        || before.len() != bytes.len() as u64
        || before_modified != after_modified
    {
        return Err("mobile continuity source changed while fingerprinting".to_string());
    }
    let modified_at_ms = before_modified
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "mobile continuity source timestamp is invalid".to_string())?
        .as_millis();
    let digest = Sha256::digest(&bytes);
    Ok(SourceFingerprint {
        size: before.len(),
        modified_at_ms,
        sha256: digest.iter().map(|byte| format!("{byte:02x}")).collect(),
    })
}

fn first_meta_matches_thread(path: &Path, thread_id: &str) -> bool {
    let Ok(file) = fs::File::open(path) else {
        return false;
    };
    let mut reader = std::io::BufReader::new(file).take(256 * 1024 + 1);
    let mut line = Vec::new();
    let Ok(read) = reader.read_until(b'\n', &mut line) else {
        return false;
    };
    if read == 0 || line.len() > 256 * 1024 {
        return false;
    }
    while line
        .last()
        .is_some_and(|byte| matches!(byte, b'\r' | b'\n'))
    {
        line.pop();
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&line) else {
        return false;
    };
    value.get("type").and_then(serde_json::Value::as_str) == Some("session_meta")
        && value
            .get("payload")
            .and_then(|payload| payload.get("id"))
            .and_then(serde_json::Value::as_str)
            == Some(thread_id)
}

fn classify_publish_error(error: &str) -> &'static str {
    let normalized = error.to_ascii_lowercase();
    if normalized.contains("diverg") || normalized.contains("conflict") {
        "conflict"
    } else if normalized.contains("capacity") || normalized.contains("space") {
        "capacity"
    } else {
        "publishFailed"
    }
}

fn status_from_state(state: &MobileContinuityState) -> MobileContinuityStatus {
    let count = |status| {
        state
            .items
            .values()
            .filter(|item| item.status == status)
            .count()
    };
    MobileContinuityStatus {
        enabled: state.enabled,
        notice_pending: state.notice_pending,
        initialized_at_ms: state.initialized_at_ms,
        queued: count(MobileContinuityItemStatus::Queued)
            + count(MobileContinuityItemStatus::Retrying)
            + count(MobileContinuityItemStatus::Paused),
        publishing: count(MobileContinuityItemStatus::Publishing),
        remote_published: count(MobileContinuityItemStatus::RemotePublished),
        partial: count(MobileContinuityItemStatus::Partial),
        conflict: count(MobileContinuityItemStatus::Conflict),
        needs_manual: count(MobileContinuityItemStatus::NeedsManual),
        items: state.items.values().cloned().collect(),
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, fs, io::Write, path::Path};

    use rusqlite::Connection;
    use tempfile::tempdir;

    use super::{
        initialize_status, is_remote_rollout_path, load_existing_state,
        prepare_account_publication, prepare_account_publication_between, redacted_sqlite_failure,
        set_enabled, MobileContinuityItemStatus,
    };
    use crate::codex_paths::{codex_paths_with_sqlite_home, local_codex_paths};

    fn insert_thread(home: &Path, id: &str, provider: &str) {
        let sessions = home.join("sessions").join("2026").join("07").join("28");
        fs::create_dir_all(&sessions).unwrap();
        let rollout = sessions.join(format!("rollout-2026-07-28T00-00-00-{id}.jsonl"));
        fs::write(
            &rollout,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"model_provider\":\"{provider}\"}}}}\n"
            ),
        )
        .unwrap();
        let connection = Connection::open(home.join("state_5.sqlite")).unwrap();
        connection
            .execute(
                "INSERT INTO threads (id, rollout_path, model_provider, archived) VALUES (?1, ?2, ?3, 0)",
                (id, rollout.to_string_lossy().to_string(), provider),
            )
            .unwrap();
    }

    fn setup() -> (
        tempfile::TempDir,
        tempfile::TempDir,
        crate::codex_paths::CodexPaths,
    ) {
        let home = tempdir().unwrap();
        fs::create_dir_all(home.path().join("sessions")).unwrap();
        fs::write(home.path().join("session_index.jsonl"), "").unwrap();
        let connection = Connection::open(home.path().join("state_5.sqlite")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE threads (
                    id TEXT PRIMARY KEY,
                    rollout_path TEXT,
                    model_provider TEXT,
                    archived INTEGER NOT NULL DEFAULT 0
                );",
            )
            .unwrap();
        drop(connection);
        let state_root = tempdir().unwrap();
        let paths = local_codex_paths(home.path());
        (home, state_root, paths)
    }

    #[test]
    fn sqlite_failures_never_expose_embedded_windows_paths() {
        let error = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
            Some(r"unable to open database file: C:\Users\private\state_5.sqlite".to_string()),
        );

        let message =
            redacted_sqlite_failure("failed to open mobile continuity state database", &error);

        assert!(message.starts_with("failed to open mobile continuity state database (SQLite code"));
        assert!(message.contains("extended"));
        assert!(!message.contains(r"C:\"));
        assert!(!message.contains("private"));
    }

    #[test]
    fn fresh_home_without_state_database_has_empty_continuity_status() {
        let home = tempdir().unwrap();
        let state_root = tempdir().unwrap();
        let paths = local_codex_paths(home.path());
        let state_path = state_root.path().join("mobile-continuity.json");

        let first = initialize_status(&state_path, &paths).unwrap();
        let second = initialize_status(&state_path, &paths).unwrap();

        assert!(state_path.is_file());
        assert!(!paths.state_db.exists());
        for status in [first, second] {
            assert_eq!(status.queued, 0);
            assert_eq!(status.publishing, 0);
            assert_eq!(status.remote_published, 0);
            assert_eq!(status.partial, 0);
            assert_eq!(status.conflict, 0);
            assert_eq!(status.needs_manual, 0);
            assert!(status.items.is_empty());
        }
    }

    #[test]
    fn non_file_state_database_is_rejected_without_leaking_its_path() {
        let home = tempdir().unwrap();
        let state_root = tempdir().unwrap();
        let paths = local_codex_paths(home.path());
        fs::create_dir_all(&paths.state_db).unwrap();

        let message = initialize_status(&state_root.path().join("mobile-continuity.json"), &paths)
            .unwrap_err();

        assert_eq!(
            message,
            "mobile continuity state database is not a regular file"
        );
        assert!(!message.contains(&paths.state_db.display().to_string()));
    }

    #[test]
    fn cutover_excludes_existing_threads_and_queues_only_new_relay_threads() {
        let (home, state_root, paths) = setup();
        let old_id = "11111111-1111-4111-8111-111111111111".to_string();
        insert_thread(home.path(), &old_id, "openai_custom");
        let state_path = state_root.path().join("mobile-continuity.json");

        let initial = initialize_status(&state_path, &paths).unwrap();
        assert_eq!(initial.queued, 0);

        let new_id = "22222222-2222-4222-8222-222222222222".to_string();
        insert_thread(home.path(), &new_id, "openai_custom");
        let result = prepare_account_publication(&state_path, &paths).unwrap();

        assert_eq!(result.detected_threads, 1);
        assert_eq!(result.published_threads, 1);
        let state = load_existing_state(&state_path).unwrap().unwrap();
        assert!(state.known_thread_ids.contains(&old_id));
        assert!(state.known_thread_ids.contains(&new_id));
        assert_eq!(
            state.items.keys().cloned().collect::<HashSet<_>>(),
            HashSet::from([new_id])
        );
    }

    #[test]
    fn isolated_relay_view_publishes_to_account_without_changing_relay_row() {
        let (home, state_root, relay_paths) = setup();
        let account_sqlite = tempdir().unwrap();
        let account_paths =
            codex_paths_with_sqlite_home(home.path(), account_sqlite.path()).unwrap();
        Connection::open(&account_paths.state_db)
            .unwrap()
            .execute_batch(
                "CREATE TABLE threads (
                    id TEXT PRIMARY KEY,
                    rollout_path TEXT,
                    model_provider TEXT,
                    archived INTEGER NOT NULL DEFAULT 0
                );",
            )
            .unwrap();
        let state_path = state_root.path().join("mobile-continuity.json");
        initialize_status(&state_path, &relay_paths).unwrap();
        let id = "29292929-2929-4929-8929-292929292929";
        insert_thread(home.path(), id, "openai_custom");

        let result =
            prepare_account_publication_between(&state_path, &relay_paths, &account_paths).unwrap();

        assert_eq!(result.detected_threads, 1);
        assert_eq!(result.published_threads, 1);
        assert_eq!(
            Connection::open(&relay_paths.state_db)
                .unwrap()
                .query_row(
                    "SELECT model_provider FROM threads WHERE id = ?1",
                    [id],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "openai_custom"
        );
        let (provider, rollout_path): (String, String) = Connection::open(&account_paths.state_db)
            .unwrap()
            .query_row(
                "SELECT model_provider, rollout_path FROM threads WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(provider, "openai");
        assert!(is_remote_rollout_path(Path::new(&rollout_path), id));
    }

    #[test]
    fn appending_an_old_thread_never_moves_it_into_the_automatic_queue() {
        let (home, state_root, paths) = setup();
        let old_id = "33333333-3333-4333-8333-333333333333".to_string();
        insert_thread(home.path(), &old_id, "openai_custom");
        let state_path = state_root.path().join("mobile-continuity.json");
        initialize_status(&state_path, &paths).unwrap();
        let connection = Connection::open(home.path().join("state_5.sqlite")).unwrap();
        connection
            .execute(
                "UPDATE threads SET model_provider = 'openai_custom' WHERE id = ?1",
                [&old_id],
            )
            .unwrap();

        let result = prepare_account_publication(&state_path, &paths).unwrap();

        assert_eq!(result.detected_threads, 0);
        assert_eq!(result.published_threads, 0);
        assert!(load_existing_state(&state_path)
            .unwrap()
            .unwrap()
            .items
            .is_empty());
    }

    #[test]
    fn an_archived_thread_present_at_cutover_stays_legacy_after_unarchive() {
        let (home, state_root, paths) = setup();
        let old_id = "44444444-4444-4444-8444-444444444444".to_string();
        insert_thread(home.path(), &old_id, "openai_custom");
        let connection = Connection::open(home.path().join("state_5.sqlite")).unwrap();
        connection
            .execute("UPDATE threads SET archived = 1 WHERE id = ?1", [&old_id])
            .unwrap();
        let state_path = state_root.path().join("mobile-continuity.json");
        initialize_status(&state_path, &paths).unwrap();
        connection
            .execute("UPDATE threads SET archived = 0 WHERE id = ?1", [&old_id])
            .unwrap();

        let result = prepare_account_publication(&state_path, &paths).unwrap();

        assert_eq!(result.detected_threads, 0);
        assert!(load_existing_state(&state_path)
            .unwrap()
            .unwrap()
            .items
            .is_empty());
    }

    #[test]
    fn sessions_created_while_disabled_do_not_backfill_when_reenabled() {
        let (home, state_root, paths) = setup();
        let state_path = state_root.path().join("mobile-continuity.json");
        initialize_status(&state_path, &paths).unwrap();
        set_enabled(&state_path, &paths, false).unwrap();
        let id = "77777777-7777-4777-8777-777777777777".to_string();
        insert_thread(home.path(), &id, "openai_custom");

        set_enabled(&state_path, &paths, true).unwrap();
        let result = prepare_account_publication(&state_path, &paths).unwrap();

        assert_eq!(result.detected_threads, 0);
        assert!(load_existing_state(&state_path)
            .unwrap()
            .unwrap()
            .items
            .is_empty());
    }

    #[test]
    fn startup_reconciles_a_crash_paused_item_that_already_reached_remote() {
        let (home, state_root, paths) = setup();
        let state_path = state_root.path().join("mobile-continuity.json");
        initialize_status(&state_path, &paths).unwrap();
        let id = "88888888-8888-4888-8888-888888888888".to_string();
        insert_thread(home.path(), &id, "openai_custom");
        prepare_account_publication(&state_path, &paths).unwrap();
        let state = fs::read_to_string(&state_path)
            .unwrap()
            .replace("\"remotePublished\"", "\"publishing\"");
        fs::write(&state_path, state).unwrap();

        let status = initialize_status(&state_path, &paths).unwrap();

        assert_eq!(status.remote_published, 1);
        assert_eq!(status.queued, 0);
    }

    #[test]
    fn malformed_state_fails_closed_without_rebuilding_a_cutover() {
        let (home, state_root, paths) = setup();
        let state_path = state_root.path().join("mobile-continuity.json");
        let id = "99999999-9999-4999-8999-999999999999".to_string();
        insert_thread(home.path(), &id, "openai_custom");
        fs::write(&state_path, b"{not-json").unwrap();

        let error = initialize_status(&state_path, &paths).unwrap_err();

        assert!(error.contains("invalid"));
        assert_eq!(fs::read(&state_path).unwrap(), b"{not-json");
        let provider: String = Connection::open(home.path().join("state_5.sqlite"))
            .unwrap()
            .query_row(
                "SELECT model_provider FROM threads WHERE id = ?1",
                [&id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(provider, "openai_custom");
    }

    #[test]
    fn canonical_view_keeps_local_only_content_in_place_without_copying() {
        let (home, state_root, paths) = setup();
        let state_path = state_root.path().join("mobile-continuity.json");
        initialize_status(&state_path, &paths).unwrap();
        let id = "55555555-5555-4555-8555-555555555555".to_string();
        insert_thread(home.path(), &id, "openai_custom");
        let connection = Connection::open(home.path().join("state_5.sqlite")).unwrap();
        let source_path: String = connection
            .query_row(
                "SELECT rollout_path FROM threads WHERE id = ?1",
                [&id],
                |row| row.get(0),
            )
            .unwrap();
        let private_path = r"C:\private\secret.txt";
        fs::OpenOptions::new()
            .append(true)
            .open(&source_path)
            .unwrap()
            .write_all(
                format!(
                    "{}\n",
                    serde_json::json!({
                        "type": "response_item",
                        "payload": { "file_path": private_path }
                    })
                )
                .as_bytes(),
            )
            .unwrap();
        let original = fs::read(&source_path).unwrap();
        let jsonl_before = fs::read_dir(Path::new(&source_path).parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry.path().extension().and_then(|value| value.to_str()) == Some("jsonl")
            })
            .count();

        let result = prepare_account_publication(&state_path, &paths).unwrap();

        assert_eq!(result.published_threads, 1);
        assert_eq!(result.partial_threads, 0);
        assert_eq!(fs::read(&source_path).unwrap(), original);
        let (provider, canonical): (String, String) = connection
            .query_row(
                "SELECT model_provider, rollout_path FROM threads WHERE id = ?1",
                [&id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(provider, "openai");
        assert_eq!(Path::new(&canonical), Path::new(&source_path));
        assert!(!fs::read_to_string(&canonical)
            .unwrap()
            .contains("部分内容仅本机"));
        let jsonl_after = fs::read_dir(Path::new(&source_path).parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry.path().extension().and_then(|value| value.to_str()) == Some("jsonl")
            })
            .count();
        assert_eq!(jsonl_after, jsonl_before);
    }

    #[test]
    fn divergent_canonical_view_paths_are_preserved_and_reported_as_conflict() {
        let (home, state_root, relay_paths) = setup();
        let account_sqlite = tempdir().unwrap();
        let account_paths =
            codex_paths_with_sqlite_home(home.path(), account_sqlite.path()).unwrap();
        Connection::open(&account_paths.state_db)
            .unwrap()
            .execute_batch(
                "CREATE TABLE threads (
                    id TEXT PRIMARY KEY,
                    rollout_path TEXT,
                    model_provider TEXT,
                    archived INTEGER NOT NULL DEFAULT 0
                );",
            )
            .unwrap();
        let state_path = state_root.path().join("mobile-continuity.json");
        initialize_status(&state_path, &relay_paths).unwrap();
        let id = "66666666-6666-4666-8666-666666666666".to_string();
        insert_thread(home.path(), &id, "openai_custom");
        let connection = Connection::open(home.path().join("state_5.sqlite")).unwrap();
        let source_path: String = connection
            .query_row(
                "SELECT rollout_path FROM threads WHERE id = ?1",
                [&id],
                |row| row.get(0),
            )
            .unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(&source_path)
            .unwrap()
            .write_all(b"{\"type\":\"event_msg\",\"payload\":{\"message\":\"branch-a\"}}\n")
            .unwrap();
        let target_path = Path::new(&source_path)
            .with_file_name(format!("rollout-2026-07-28T00-00-01-{id}.jsonl"));
        fs::write(
            &target_path,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"model_provider\":\"openai\"}}}}\n\
                 {{\"type\":\"event_msg\",\"payload\":{{\"message\":\"branch-b\"}}}}\n"
            ),
        )
        .unwrap();
        Connection::open(&account_paths.state_db)
            .unwrap()
            .execute(
                "INSERT INTO threads (id, rollout_path, model_provider, archived)
                 VALUES (?1, ?2, 'openai', 0)",
                (&id, target_path.to_string_lossy().to_string()),
            )
            .unwrap();

        let result =
            prepare_account_publication_between(&state_path, &relay_paths, &account_paths).unwrap();
        let status = super::status_from_state(&load_existing_state(&state_path).unwrap().unwrap());

        assert_eq!(result.published_threads, 0);
        assert_eq!(status.conflict, 1);
        assert_eq!(
            status
                .items
                .iter()
                .find(|item| item.thread_id == id)
                .unwrap()
                .status,
            MobileContinuityItemStatus::Conflict
        );
        assert!(fs::read_to_string(&source_path)
            .unwrap()
            .contains("branch-a"));
        assert!(fs::read_to_string(&target_path)
            .unwrap()
            .contains("branch-b"));
    }
}

use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::{BufRead, BufReader, Read},
    path::{Component, Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use rusqlite::{types::Value, Connection, OpenFlags};
use serde::{Deserialize, Serialize};

use crate::{
    codex_paths::CodexPaths,
    file_ops::atomic_write,
    session_sync::{session_file_relation, SessionFileRelation},
};

const SYNC_INDEX_VERSION: u32 = 1;
const MAX_SYNC_INDEX_BYTES: u64 = 8 * 1024 * 1024;
pub(crate) const MAX_INCREMENTAL_THREADS: usize = 32;
pub(crate) const MAX_INCREMENTAL_PROJECTED_BYTES: u64 = 32 * 1024 * 1024;
pub(crate) const MAX_INCREMENTAL_TOTAL_DURATION: Duration = Duration::from_secs(2);
const PROVIDER_MARKER_RESERVE_BYTES: u64 = 16 * 1024;
const MAX_INCREMENTAL_INVENTORY_DURATION: Duration = Duration::from_millis(750);
const MAX_SESSION_META_LINE_BYTES: u64 = 256 * 1024;

type RolloutInspection = (bool, Option<u64>, Option<u128>, Option<String>);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SessionFingerprint {
    rollout_path: Option<String>,
    model_provider: Option<String>,
    file_provider: Option<String>,
    archived: bool,
    file_length: Option<u64>,
    modified_ns: Option<u128>,
    valid_file: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SessionSyncIndex {
    version: u32,
    completed_at_ms: u128,
    current: BTreeMap<String, SessionFingerprint>,
    shared: BTreeMap<String, SessionFingerprint>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum IncrementalSessionSyncStatus {
    Skipped,
    Unchanged,
    Applied,
    NeedsFullSync,
    Deferred,
    Failed,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct IncrementalSessionSyncReceipt {
    pub status: IncrementalSessionSyncStatus,
    pub detected_threads: usize,
    pub synced_threads: usize,
    pub projected_bytes: u64,
    pub duration_ms: u128,
    pub requires_full_sync: bool,
}

impl IncrementalSessionSyncReceipt {
    pub(crate) fn skipped() -> Self {
        Self {
            status: IncrementalSessionSyncStatus::Skipped,
            detected_threads: 0,
            synced_threads: 0,
            projected_bytes: 0,
            duration_ms: 0,
            requires_full_sync: false,
        }
    }

    pub(crate) fn needs_full_sync(duration_ms: u128) -> Self {
        Self {
            status: IncrementalSessionSyncStatus::NeedsFullSync,
            detected_threads: 0,
            synced_threads: 0,
            projected_bytes: 0,
            duration_ms,
            requires_full_sync: true,
        }
    }

    pub(crate) fn failed(detected_threads: usize, projected_bytes: u64, duration_ms: u128) -> Self {
        Self {
            status: IncrementalSessionSyncStatus::Failed,
            detected_threads,
            synced_threads: 0,
            projected_bytes,
            duration_ms,
            requires_full_sync: true,
        }
    }
}

#[derive(Debug)]
pub(crate) enum IncrementalSessionPlan {
    Unchanged,
    Ready {
        current_ids: HashSet<String>,
        shared_ids: HashSet<String>,
        normalize_current_ids: HashSet<String>,
        projected_bytes: u64,
    },
    NeedsFullSync,
    Deferred {
        detected_threads: usize,
        projected_bytes: u64,
    },
}

pub(crate) fn plan_incremental_session_sync(
    index_path: &Path,
    current: &CodexPaths,
    shared: &CodexPaths,
    target_provider: Option<&str>,
) -> Result<IncrementalSessionPlan, String> {
    let Some(previous) = load_index(index_path)? else {
        return Ok(IncrementalSessionPlan::NeedsFullSync);
    };
    let deadline = Instant::now() + MAX_INCREMENTAL_INVENTORY_DURATION;
    let Some(observed_current) = observe_root(current, Some(deadline))? else {
        return Ok(IncrementalSessionPlan::Deferred {
            detected_threads: 0,
            projected_bytes: 0,
        });
    };
    let Some(observed_shared) = observe_root(shared, Some(deadline))? else {
        return Ok(IncrementalSessionPlan::Deferred {
            detected_threads: 0,
            projected_bytes: 0,
        });
    };
    let Some(current_ids) = changed_ids(&previous.current, &observed_current) else {
        return Ok(IncrementalSessionPlan::NeedsFullSync);
    };
    let Some(shared_ids) = changed_ids(&previous.shared, &observed_shared) else {
        return Ok(IncrementalSessionPlan::NeedsFullSync);
    };

    if current_ids.iter().any(|id| shared_ids.contains(id))
        || shared_ids
            .iter()
            .any(|id| observed_current.contains_key(id))
    {
        return Ok(IncrementalSessionPlan::NeedsFullSync);
    }
    for id in &current_ids {
        let Some(shared_fingerprint) = observed_shared.get(id) else {
            continue;
        };
        let current_path = fingerprint_path(
            current,
            observed_current
                .get(id)
                .ok_or_else(|| "incremental session candidate disappeared".to_string())?,
        )?;
        let shared_path = fingerprint_path(shared, shared_fingerprint)?;
        if !matches!(
            session_file_relation(&current_path, &shared_path)?,
            SessionFileRelation::Equal | SessionFileRelation::LeftExtendsRight
        ) {
            return Ok(IncrementalSessionPlan::NeedsFullSync);
        }
    }
    let normalize_current_ids: HashSet<String> = target_provider
        .map(|provider| {
            provider_mismatch_ids(&observed_current, provider)
                .intersection(&current_ids)
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    if current_ids.is_empty() && shared_ids.is_empty() && normalize_current_ids.is_empty() {
        return Ok(IncrementalSessionPlan::Unchanged);
    }

    let detected_threads = current_ids
        .union(&normalize_current_ids)
        .count()
        .checked_add(shared_ids.len())
        .ok_or_else(|| "incremental session count overflowed".to_string())?;
    let session_bytes = projected_incremental_bytes(&observed_current, &current_ids, 1)?
        .checked_add(projected_incremental_bytes(
            &observed_current,
            &normalize_current_ids,
            1,
        )?)
        .ok_or_else(|| "incremental session capacity calculation overflowed".to_string())?
        .checked_add(projected_incremental_bytes(
            &observed_shared,
            &shared_ids,
            1,
        )?)
        .ok_or_else(|| "incremental session capacity calculation overflowed".to_string())?;
    let current_state_bytes = regular_file_length(&current.state_db)?;
    let shared_state_bytes = regular_file_length(&shared.state_db)?;
    let projected_bytes = session_bytes
        .checked_add(current_state_bytes)
        .and_then(|bytes| bytes.checked_add(shared_state_bytes))
        .ok_or_else(|| "incremental session capacity calculation overflowed".to_string())?;
    if detected_threads > MAX_INCREMENTAL_THREADS
        || projected_bytes > MAX_INCREMENTAL_PROJECTED_BYTES
    {
        return Ok(IncrementalSessionPlan::Deferred {
            detected_threads,
            projected_bytes,
        });
    }

    Ok(IncrementalSessionPlan::Ready {
        current_ids,
        shared_ids,
        normalize_current_ids,
        projected_bytes,
    })
}

fn regular_file_length(path: &Path) -> Result<u64, String> {
    fs::metadata(path)
        .map(|metadata| metadata.len())
        .map_err(|error| format!("failed to inspect incremental session state: {error}"))
}

pub(crate) fn save_session_sync_index(
    index_path: &Path,
    current: &CodexPaths,
    shared: &CodexPaths,
) -> Result<(), String> {
    save_session_sync_index_with_deadline(index_path, current, shared, None).and_then(|saved| {
        saved
            .then_some(())
            .ok_or_else(|| "session sync index inventory timed out".to_string())
    })
}

pub(crate) fn save_session_sync_index_bounded(
    index_path: &Path,
    current: &CodexPaths,
    shared: &CodexPaths,
    deadline: Instant,
) -> Result<bool, String> {
    save_session_sync_index_with_deadline(index_path, current, shared, Some(deadline))
}

fn save_session_sync_index_with_deadline(
    index_path: &Path,
    current: &CodexPaths,
    shared: &CodexPaths,
    deadline: Option<Instant>,
) -> Result<bool, String> {
    let Some(current) = observe_root(current, deadline)? else {
        return Ok(false);
    };
    let Some(shared) = observe_root(shared, deadline)? else {
        return Ok(false);
    };
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Ok(false);
    }
    let index = SessionSyncIndex {
        version: SYNC_INDEX_VERSION,
        completed_at_ms: timestamp_millis()?,
        current,
        shared,
    };
    let encoded = serde_json::to_vec(&index)
        .map_err(|_| "failed to serialize the session sync index".to_string())?;
    if encoded.len() as u64 > MAX_SYNC_INDEX_BYTES {
        return Err("session sync index exceeds its size limit".to_string());
    }
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Ok(false);
    }
    atomic_write(index_path, &encoded)
        .map_err(|error| format!("failed to persist the session sync index: {error}"))?;
    Ok(true)
}

fn load_index(path: &Path) -> Result<Option<SessionSyncIndex>, String> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("failed to inspect the session sync index: {error}")),
    };
    if !metadata.is_file() || metadata.len() > MAX_SYNC_INDEX_BYTES {
        return Ok(None);
    }
    let bytes = fs::read(path)
        .map_err(|error| format!("failed to read the session sync index: {error}"))?;
    let Ok(index) = serde_json::from_slice::<SessionSyncIndex>(&bytes) else {
        return Ok(None);
    };
    Ok((index.version == SYNC_INDEX_VERSION).then_some(index))
}

fn changed_ids(
    previous: &BTreeMap<String, SessionFingerprint>,
    observed: &BTreeMap<String, SessionFingerprint>,
) -> Option<HashSet<String>> {
    if previous.keys().any(|id| !observed.contains_key(id)) {
        return None;
    }
    let mut changed = HashSet::new();
    for (id, current) in observed {
        match previous.get(id) {
            Some(old) if old == current => {}
            Some(old)
                if old.archived != current.archived || !old.valid_file || !current.valid_file =>
            {
                return None;
            }
            Some(_) if current.archived => return None,
            Some(_) => {
                changed.insert(id.clone());
            }
            None if current.archived || !current.valid_file => return None,
            None => {
                changed.insert(id.clone());
            }
        }
    }
    Some(changed)
}

fn provider_mismatch_ids(
    observed: &BTreeMap<String, SessionFingerprint>,
    provider: &str,
) -> HashSet<String> {
    observed
        .iter()
        .filter(|(_, fingerprint)| {
            !fingerprint.archived
                && fingerprint.valid_file
                && (fingerprint.model_provider.as_deref() != Some(provider)
                    || fingerprint.file_provider.as_deref() != Some(provider))
        })
        .map(|(id, _)| id.clone())
        .collect()
}

fn fingerprint_path(
    paths: &CodexPaths,
    fingerprint: &SessionFingerprint,
) -> Result<PathBuf, String> {
    if !fingerprint.valid_file {
        return Err("incremental session candidate has no stable file".to_string());
    }
    let stored = fingerprint
        .rollout_path
        .as_deref()
        .ok_or_else(|| "incremental session candidate has no rollout path".to_string())?;
    let stored = PathBuf::from(stored);
    Ok(if stored.is_absolute() {
        stored
    } else {
        paths.codex_home.join(stored)
    })
}

fn projected_incremental_bytes(
    observed: &BTreeMap<String, SessionFingerprint>,
    ids: &HashSet<String>,
    copies: u64,
) -> Result<u64, String> {
    ids.iter().try_fold(0_u64, |total, id| {
        let fingerprint = observed
            .get(id)
            .ok_or_else(|| "incremental session candidate disappeared".to_string())?;
        let bytes = fingerprint
            .file_length
            .ok_or_else(|| "incremental session candidate has no stable file".to_string())?
            .checked_add(PROVIDER_MARKER_RESERVE_BYTES)
            .and_then(|value| value.checked_mul(copies))
            .ok_or_else(|| "incremental session capacity calculation overflowed".to_string())?;
        total
            .checked_add(bytes)
            .ok_or_else(|| "incremental session capacity calculation overflowed".to_string())
    })
}

fn observe_root(
    paths: &CodexPaths,
    deadline: Option<Instant>,
) -> Result<Option<BTreeMap<String, SessionFingerprint>>, String> {
    let conn = Connection::open_with_flags(&paths.state_db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| format!("failed to open session state for incremental sync: {error}"))?;
    conn.busy_timeout(if deadline.is_some() {
        Duration::from_millis(200)
    } else {
        Duration::from_secs(2)
    })
    .map_err(|error| format!("failed to set incremental session timeout: {error}"))?;
    let columns = table_columns(&conn, "threads")?;
    if !columns.iter().any(|column| column == "id")
        || !columns.iter().any(|column| column == "rollout_path")
    {
        return Err("threads table is missing incremental sync columns".to_string());
    }
    let provider_column = columns
        .iter()
        .any(|column| column == "model_provider")
        .then_some("model_provider");
    let archived_column = columns
        .iter()
        .any(|column| column == "archived")
        .then_some("archived");
    let mut selected = vec!["id", "rollout_path"];
    if let Some(column) = provider_column {
        selected.push(column);
    }
    if let Some(column) = archived_column {
        selected.push(column);
    }
    let mut statement = conn
        .prepare(&format!("SELECT {} FROM threads", selected.join(", ")))
        .map_err(|error| format!("failed to prepare incremental session inventory: {error}"))?;
    let rows = statement
        .query_map([], |row| {
            let id = row.get::<_, Value>(0)?;
            let rollout_path = row.get::<_, Value>(1)?;
            let mut index = 2;
            let model_provider = if provider_column.is_some() {
                let value = row.get::<_, Value>(index)?;
                index += 1;
                value_text(value)
            } else {
                None
            };
            let archived = if archived_column.is_some() {
                archived_value(row.get::<_, Value>(index)?)
            } else {
                false
            };
            Ok((
                value_text(id),
                value_text(rollout_path),
                model_provider,
                archived,
            ))
        })
        .map_err(|error| format!("failed to read incremental session inventory: {error}"))?;

    let canonical_sessions = fs::canonicalize(&paths.sessions_dir).ok();
    let mut observations = BTreeMap::new();
    for row in rows {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Ok(None);
        }
        let (id, rollout_path, model_provider, archived) = row
            .map_err(|error| format!("failed to collect incremental session inventory: {error}"))?;
        let Some(id) = id.filter(|id| !id.trim().is_empty()) else {
            continue;
        };
        let (valid_file, file_length, modified_ns, file_provider) = rollout_path
            .as_deref()
            .map(|stored| inspect_rollout(paths, canonical_sessions.as_deref(), stored))
            .transpose()?
            .unwrap_or((false, None, None, None));
        observations.insert(
            id,
            SessionFingerprint {
                rollout_path,
                model_provider,
                file_provider,
                archived,
                file_length,
                modified_ns,
                valid_file,
            },
        );
    }
    Ok(Some(observations))
}

fn inspect_rollout(
    paths: &CodexPaths,
    canonical_sessions: Option<&Path>,
    stored: &str,
) -> Result<RolloutInspection, String> {
    let stored_path = PathBuf::from(stored);
    let candidate = if stored_path.is_absolute() {
        stored_path
    } else {
        paths.codex_home.join(stored_path)
    };
    let Ok(relative) = candidate.strip_prefix(&paths.sessions_dir) else {
        return Ok((false, None, None, None));
    };
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Ok((false, None, None, None));
    }
    let Ok(metadata) = fs::metadata(&candidate) else {
        return Ok((false, None, None, None));
    };
    if !metadata.is_file() {
        return Ok((false, None, None, None));
    }
    if let Some(canonical_sessions) = canonical_sessions {
        let canonical_candidate = fs::canonicalize(&candidate)
            .map_err(|error| format!("failed to resolve an incremental session file: {error}"))?;
        if !canonical_candidate.starts_with(canonical_sessions) {
            return Ok((false, None, None, None));
        }
    } else {
        return Ok((false, None, None, None));
    }
    let modified_ns = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos());
    Ok((
        true,
        Some(metadata.len()),
        modified_ns,
        session_file_provider(&candidate)?,
    ))
}

fn session_file_provider(path: &Path) -> Result<Option<String>, String> {
    let file = fs::File::open(path)
        .map_err(|error| format!("failed to open an incremental session file: {error}"))?;
    let mut line = String::new();
    BufReader::new(file)
        .take(MAX_SESSION_META_LINE_BYTES)
        .read_line(&mut line)
        .map_err(|error| format!("failed to read incremental session metadata: {error}"))?;
    let value = serde_json::from_str::<serde_json::Value>(line.trim_end()).ok();
    Ok(value
        .as_ref()
        .filter(|value| {
            value.get("type").and_then(serde_json::Value::as_str) == Some("session_meta")
        })
        .and_then(|value| value.get("payload"))
        .and_then(|payload| payload.get("model_provider"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string))
}

fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>, String> {
    let mut statement = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|error| format!("failed to inspect incremental session schema: {error}"))?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| format!("failed to read incremental session schema: {error}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("failed to collect incremental session schema: {error}"))
}

fn value_text(value: Value) -> Option<String> {
    match value {
        Value::Text(value) => Some(value),
        _ => None,
    }
}

fn archived_value(value: Value) -> bool {
    match value {
        Value::Integer(value) => value != 0,
        Value::Real(value) => value != 0.0,
        Value::Text(value) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes"
        ),
        _ => false,
    }
}

fn timestamp_millis() -> Result<u128, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .map_err(|_| "system clock is before UNIX_EPOCH".to_string())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, HashSet},
        fs,
        io::Write,
        path::Path,
        time::Instant,
    };

    use rusqlite::Connection;
    use tempfile::tempdir;

    use super::{
        changed_ids, plan_incremental_session_sync, save_session_sync_index,
        save_session_sync_index_bounded, IncrementalSessionPlan, SessionFingerprint,
    };
    use crate::codex_paths::local_codex_paths;

    fn fingerprint(length: u64) -> SessionFingerprint {
        SessionFingerprint {
            rollout_path: Some("sessions/2026/07/28/rollout.jsonl".to_string()),
            model_provider: Some("openai".to_string()),
            file_provider: Some("openai".to_string()),
            archived: false,
            file_length: Some(length),
            modified_ns: Some(length as u128),
            valid_file: true,
        }
    }

    fn create_root(
        root: &Path,
        id: &str,
        provider: &str,
        body: &str,
    ) -> crate::codex_paths::CodexPaths {
        let paths = local_codex_paths(root);
        fs::create_dir_all(paths.sessions_dir.join("2026/07/28")).unwrap();
        let rollout = paths
            .sessions_dir
            .join("2026/07/28")
            .join(format!("rollout-2026-07-28T00-00-00-{id}.jsonl"));
        fs::write(
            &rollout,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"model_provider\":\"{provider}\"}}}}\n{body}\n"
            ),
        )
        .unwrap();
        let conn = Connection::open(&paths.state_db).unwrap();
        conn.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                rollout_path TEXT,
                model_provider TEXT,
                archived INTEGER NOT NULL DEFAULT 0
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO threads (id, rollout_path, model_provider, archived)
             VALUES (?1, ?2, ?3, 0)",
            (id, rollout.to_string_lossy().to_string(), provider),
        )
        .unwrap();
        paths
    }

    #[test]
    fn changed_ids_rejects_removal_archive_and_invalid_file() {
        let mut previous = BTreeMap::new();
        previous.insert("a".to_string(), fingerprint(1));
        assert!(changed_ids(&previous, &BTreeMap::new()).is_none());

        let mut archived = previous.clone();
        archived.get_mut("a").unwrap().archived = true;
        assert!(changed_ids(&previous, &archived).is_none());

        let mut invalid = previous.clone();
        invalid.get_mut("a").unwrap().valid_file = false;
        assert!(changed_ids(&previous, &invalid).is_none());
    }

    #[test]
    fn missing_index_requires_full_sync_without_scanning_roots() {
        let root = tempdir().unwrap();
        let current = local_codex_paths(&root.path().join("missing-current"));
        let shared = local_codex_paths(&root.path().join("missing-shared"));

        let plan = plan_incremental_session_sync(
            &root.path().join("missing-index.json"),
            &current,
            &shared,
            Some("openai"),
        )
        .unwrap();

        assert!(matches!(plan, IncrementalSessionPlan::NeedsFullSync));
    }

    #[test]
    fn saved_index_detects_only_the_changed_root() {
        let root = tempdir().unwrap();
        let id = "019fa68f-dd42-76b3-8299-84a865ab553c";
        let current = create_root(
            &root.path().join("current"),
            id,
            "openai",
            "{\"type\":\"a\"}",
        );
        let shared = create_root(
            &root.path().join("shared"),
            id,
            "openai",
            "{\"type\":\"a\"}",
        );
        let index = root.path().join("sync-index.json");
        save_session_sync_index(&index, &current, &shared).unwrap();

        let current_rollout = fs::read_dir(current.sessions_dir.join("2026/07/28"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        fs::OpenOptions::new()
            .append(true)
            .open(current_rollout)
            .unwrap()
            .write_all(b"{\"type\":\"b\"}\n")
            .unwrap();

        let plan =
            plan_incremental_session_sync(&index, &current, &shared, Some("openai")).unwrap();
        let IncrementalSessionPlan::Ready {
            current_ids,
            shared_ids,
            normalize_current_ids,
            ..
        } = plan
        else {
            panic!("expected a bounded incremental plan");
        };
        assert_eq!(current_ids, HashSet::from([id.to_string()]));
        assert!(shared_ids.is_empty());
        assert!(normalize_current_ids.is_empty());
    }

    #[test]
    fn account_target_does_not_rewrite_an_unchanged_historical_provider() {
        let root = tempdir().unwrap();
        let id = "019fa68f-dd42-76b3-8299-84a865ab553d";
        let current = create_root(
            &root.path().join("current"),
            id,
            "openai_custom",
            "{\"type\":\"a\"}",
        );
        let shared = create_root(
            &root.path().join("shared"),
            id,
            "openai_custom",
            "{\"type\":\"a\"}",
        );
        let index = root.path().join("sync-index.json");
        save_session_sync_index(&index, &current, &shared).unwrap();

        assert!(matches!(
            plan_incremental_session_sync(&index, &current, &shared, None).unwrap(),
            IncrementalSessionPlan::Unchanged
        ));
        assert!(matches!(
            plan_incremental_session_sync(&index, &current, &shared, Some("openai")).unwrap(),
            IncrementalSessionPlan::Unchanged
        ));
    }

    #[test]
    fn account_target_normalizes_only_a_changed_relay_session() {
        let root = tempdir().unwrap();
        let id = "019fa68f-dd42-76b3-8299-84a865ab5541";
        let current = create_root(
            &root.path().join("current"),
            id,
            "openai_custom",
            "{\"type\":\"a\"}",
        );
        let shared = create_root(
            &root.path().join("shared"),
            id,
            "openai_custom",
            "{\"type\":\"a\"}",
        );
        let index = root.path().join("sync-index.json");
        save_session_sync_index(&index, &current, &shared).unwrap();
        let current_rollout = fs::read_dir(current.sessions_dir.join("2026/07/28"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        fs::OpenOptions::new()
            .append(true)
            .open(current_rollout)
            .unwrap()
            .write_all(b"{\"type\":\"changed\"}\n")
            .unwrap();

        let IncrementalSessionPlan::Ready {
            current_ids,
            shared_ids,
            normalize_current_ids,
            ..
        } = plan_incremental_session_sync(&index, &current, &shared, Some("openai")).unwrap()
        else {
            panic!("expected one changed Account normalization candidate");
        };

        assert_eq!(current_ids, HashSet::from([id.to_string()]));
        assert!(shared_ids.is_empty());
        assert_eq!(normalize_current_ids, HashSet::from([id.to_string()]));
    }

    #[test]
    fn changes_to_both_copies_of_one_thread_require_full_reconciliation() {
        let root = tempdir().unwrap();
        let id = "019fa68f-dd42-76b3-8299-84a865ab553e";
        let current = create_root(
            &root.path().join("current"),
            id,
            "openai",
            "{\"type\":\"a\"}",
        );
        let shared = create_root(
            &root.path().join("shared"),
            id,
            "openai",
            "{\"type\":\"a\"}",
        );
        let index = root.path().join("sync-index.json");
        save_session_sync_index(&index, &current, &shared).unwrap();
        for paths in [&current, &shared] {
            let rollout = fs::read_dir(paths.sessions_dir.join("2026/07/28"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            fs::OpenOptions::new()
                .append(true)
                .open(rollout)
                .unwrap()
                .write_all(b"{\"type\":\"changed\"}\n")
                .unwrap();
        }

        assert!(matches!(
            plan_incremental_session_sync(&index, &current, &shared, Some("openai")).unwrap(),
            IncrementalSessionPlan::NeedsFullSync
        ));
    }

    #[test]
    fn bounded_index_save_stops_without_publishing_after_its_deadline() {
        let root = tempdir().unwrap();
        let current = create_root(
            &root.path().join("current"),
            "019fa68f-dd42-76b3-8299-84a865ab553f",
            "openai",
            "{\"type\":\"a\"}",
        );
        let shared = create_root(
            &root.path().join("shared"),
            "019fa68f-dd42-76b3-8299-84a865ab5540",
            "openai",
            "{\"type\":\"b\"}",
        );
        let index = root.path().join("sync-index.json");

        assert!(
            !save_session_sync_index_bounded(&index, &current, &shared, Instant::now(),).unwrap()
        );
        assert!(!index.exists());
    }
}

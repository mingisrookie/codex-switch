use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{file_ops::atomic_write, operation_log::timestamp_millis};

use super::bounded_file::read_regular_file_bounded;

const METRICS_SCHEMA_VERSION: u32 = 1;
const METRICS_FILE_NAME: &str = "reclaim-metrics-v1.json";
const MAX_METRICS_BYTES: u64 = 8 * 1024 * 1024;
const MAX_EVENT_ID_BYTES: usize = 192;
const RECLAIM_EVENT_RETENTION_MS: u128 = 7 * 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionStorageReclaimMetrics {
    pub schema_version: u32,
    pub reclaimed_bytes: u64,
    pub updated_at_ms: u128,
    pub processed_event_keys: BTreeSet<String>,
    pub events: BTreeMap<String, ReclaimMetricEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReclaimMetricEvent {
    pub reclaimed_bytes: u64,
    pub recorded_at_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReclaimMetricsEnvelope {
    metrics: SessionStorageReclaimMetrics,
    integrity_sha256: String,
}

pub fn load_reclaim_metrics(data_root: &Path) -> Result<SessionStorageReclaimMetrics, String> {
    let path = metrics_path(data_root);
    let bytes = match read_regular_file_bounded(&path, MAX_METRICS_BYTES) {
        Ok(bytes) => bytes,
        Err(_) if !path.exists() => return Ok(empty_metrics()),
        Err(_) => return Err("session storage reclaim metrics are unreadable".to_string()),
    };
    let envelope = serde_json::from_slice::<ReclaimMetricsEnvelope>(&bytes)
        .map_err(|_| "session storage reclaim metrics are invalid".to_string())?;
    validate_metrics(&envelope.metrics)?;
    if metrics_digest(&envelope.metrics)? != envelope.integrity_sha256 {
        return Err("session storage reclaim metrics integrity check failed".to_string());
    }
    Ok(envelope.metrics)
}

pub fn has_recorded_reclaim_event(data_root: &Path, event_id: &str) -> Result<bool, String> {
    validate_event_id(event_id)?;
    Ok(load_reclaim_metrics(data_root)?
        .processed_event_keys
        .contains(&event_key(event_id)))
}

pub fn record_reclaimed_bytes(
    data_root: &Path,
    event_id: &str,
    reclaimed_bytes: u64,
) -> Result<SessionStorageReclaimMetrics, String> {
    record_reclaimed_bytes_at(data_root, event_id, reclaimed_bytes, timestamp_millis()?)
}

fn record_reclaimed_bytes_at(
    data_root: &Path,
    event_id: &str,
    reclaimed_bytes: u64,
    recorded_at_ms: u128,
) -> Result<SessionStorageReclaimMetrics, String> {
    validate_event_id(event_id)?;
    if recorded_at_ms == 0 {
        return Err("session storage reclaim metric timestamp is invalid".to_string());
    }
    let mut metrics = load_reclaim_metrics(data_root)?;
    let cutoff = recorded_at_ms.saturating_sub(RECLAIM_EVENT_RETENTION_MS);
    let original_event_count = metrics.events.len();
    metrics
        .events
        .retain(|_, event| event.recorded_at_ms >= cutoff);
    let event_key = event_key(event_id);
    if let Some(existing_bytes) = metrics
        .events
        .get(&event_key)
        .map(|event| event.reclaimed_bytes)
    {
        if existing_bytes != reclaimed_bytes {
            return Err("session storage reclaim metric event changed".to_string());
        }
        if metrics.events.len() != original_event_count {
            metrics.updated_at_ms = recorded_at_ms.max(metrics.updated_at_ms);
            persist_metrics(data_root, &metrics)?;
            return load_reclaim_metrics(data_root);
        }
        return Ok(metrics);
    }
    if metrics.processed_event_keys.contains(&event_key) {
        if metrics.events.len() != original_event_count {
            metrics.updated_at_ms = recorded_at_ms.max(metrics.updated_at_ms);
            persist_metrics(data_root, &metrics)?;
            return load_reclaim_metrics(data_root);
        }
        return Ok(metrics);
    }
    if reclaimed_bytes == 0 {
        metrics.processed_event_keys.insert(event_key);
        metrics.updated_at_ms = recorded_at_ms.max(metrics.updated_at_ms);
        persist_metrics(data_root, &metrics)?;
        return load_reclaim_metrics(data_root);
    }
    metrics.reclaimed_bytes = metrics
        .reclaimed_bytes
        .checked_add(reclaimed_bytes)
        .ok_or_else(|| "session storage reclaimed byte count overflowed".to_string())?;
    metrics.updated_at_ms = recorded_at_ms.max(metrics.updated_at_ms);
    metrics.events.insert(
        event_key.clone(),
        ReclaimMetricEvent {
            reclaimed_bytes,
            recorded_at_ms,
        },
    );
    metrics.processed_event_keys.insert(event_key);
    persist_metrics(data_root, &metrics)?;
    let verified = load_reclaim_metrics(data_root)?;
    if verified != metrics {
        return Err("session storage reclaim metrics verification failed".to_string());
    }
    Ok(verified)
}

fn persist_metrics(data_root: &Path, metrics: &SessionStorageReclaimMetrics) -> Result<(), String> {
    validate_metrics(metrics)?;
    let envelope = ReclaimMetricsEnvelope {
        integrity_sha256: metrics_digest(metrics)?,
        metrics: metrics.clone(),
    };
    let bytes = serde_json::to_vec_pretty(&envelope)
        .map_err(|_| "failed to serialize session storage reclaim metrics".to_string())?;
    if bytes.len() as u64 > MAX_METRICS_BYTES {
        return Err("session storage reclaim metrics reached their size limit".to_string());
    }
    atomic_write(&metrics_path(data_root), &bytes)
}

fn validate_metrics(metrics: &SessionStorageReclaimMetrics) -> Result<(), String> {
    if metrics.schema_version != METRICS_SCHEMA_VERSION {
        return Err("session storage reclaim metrics version is unsupported".to_string());
    }
    let recent_total = metrics.events.values().try_fold(0_u64, |total, event| {
        total.checked_add(event.reclaimed_bytes)
    });
    if recent_total.is_none_or(|total| total > metrics.reclaimed_bytes) {
        return Err("session storage reclaim metrics total is invalid".to_string());
    }
    for event_key in &metrics.processed_event_keys {
        if !is_sha256(event_key) {
            return Err("session storage reclaim metric event is invalid".to_string());
        }
    }
    for (event_key, event) in &metrics.events {
        if !metrics.processed_event_keys.contains(event_key)
            || event.reclaimed_bytes == 0
            || event.recorded_at_ms == 0
        {
            return Err("session storage reclaim metric event is invalid".to_string());
        }
    }
    Ok(())
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_event_id(event_id: &str) -> Result<(), String> {
    if event_id.is_empty()
        || event_id.len() > MAX_EVENT_ID_BYTES
        || !event_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("session storage reclaim metric event id is invalid".to_string());
    }
    Ok(())
}

fn event_key(event_id: &str) -> String {
    hex_sha256(Sha256::digest(event_id.as_bytes()))
}

fn empty_metrics() -> SessionStorageReclaimMetrics {
    SessionStorageReclaimMetrics {
        schema_version: METRICS_SCHEMA_VERSION,
        reclaimed_bytes: 0,
        updated_at_ms: 0,
        processed_event_keys: BTreeSet::new(),
        events: BTreeMap::new(),
    }
}

fn metrics_path(data_root: &Path) -> std::path::PathBuf {
    data_root.join("session-storage-v1").join(METRICS_FILE_NAME)
}

fn metrics_digest(metrics: &SessionStorageReclaimMetrics) -> Result<String, String> {
    let bytes = serde_json::to_vec(metrics)
        .map_err(|_| "failed to fingerprint session storage reclaim metrics".to_string())?;
    Ok(hex_sha256(Sha256::digest(bytes)))
}

fn hex_sha256(value: impl AsRef<[u8]>) -> String {
    value
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::{
        load_reclaim_metrics, metrics_path, record_reclaimed_bytes, record_reclaimed_bytes_at,
        RECLAIM_EVENT_RETENTION_MS,
    };

    #[test]
    fn reclaimed_bytes_are_durable_and_event_idempotent() {
        let root = tempdir().unwrap();
        let first = record_reclaimed_bytes(root.path(), "offline-gc-1", 4096).unwrap();
        assert_eq!(first.reclaimed_bytes, 4096);
        let repeated = record_reclaimed_bytes(root.path(), "offline-gc-1", 4096).unwrap();
        assert_eq!(repeated.reclaimed_bytes, 4096);
        let second = record_reclaimed_bytes(root.path(), "retention-2", 512).unwrap();
        assert_eq!(second.reclaimed_bytes, 4608);
        assert_eq!(load_reclaim_metrics(root.path()).unwrap(), second);
        assert!(record_reclaimed_bytes(root.path(), "offline-gc-1", 1).is_err());
    }

    #[test]
    fn reclaim_event_deduplication_metadata_expires_after_seven_days() {
        let root = tempdir().unwrap();
        let first = record_reclaimed_bytes_at(root.path(), "offline-gc-1", 10, 1).unwrap();
        assert_eq!(first.events.len(), 1);
        let after_expiry = record_reclaimed_bytes_at(
            root.path(),
            "retention-prune",
            0,
            RECLAIM_EVENT_RETENTION_MS + 2,
        )
        .unwrap();
        assert!(after_expiry.events.is_empty());
        assert_eq!(after_expiry.reclaimed_bytes, 10);
        let replay = record_reclaimed_bytes_at(
            root.path(),
            "offline-gc-1",
            10,
            RECLAIM_EVENT_RETENTION_MS + 3,
        )
        .unwrap();
        assert_eq!(replay.reclaimed_bytes, 10);
    }

    #[test]
    fn reclaimed_metrics_reject_tampering() {
        let root = tempdir().unwrap();
        record_reclaimed_bytes(root.path(), "offline-gc-1", 4096).unwrap();
        let path = metrics_path(root.path());
        let mut value =
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).unwrap();
        value["metrics"]["reclaimedBytes"] = serde_json::json!(8192);
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(load_reclaim_metrics(root.path()).is_err());
    }
}

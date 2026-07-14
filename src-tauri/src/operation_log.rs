use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

static OPERATION_COUNTER: AtomicU64 = AtomicU64::new(0);
static LOG_WRITE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum OperationAction {
    ImportAccount,
    SaveRelay,
    VerifyRelay,
    SwitchRuntime,
    SyncSessions,
    DeleteSessions,
    RestoreVisibility,
    RestoreBackup,
    InstallSkill,
    ConfigureSkill,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum OperationStatus {
    Succeeded,
    Failed,
    RolledBack,
    RollbackFailed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum OperationPhase {
    Preflight,
    Backup,
    Apply,
    Verify,
    Complete,
    Rollback,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OperationRecord {
    pub operation_id: String,
    pub action: OperationAction,
    pub status: OperationStatus,
    pub phase: OperationPhase,
    pub started_at_ms: u128,
    pub completed_at_ms: u128,
    pub backup_dirs: Vec<PathBuf>,
    pub counts: BTreeMap<String, usize>,
}

#[derive(Debug, Clone)]
pub struct OperationLog {
    path: PathBuf,
}

impl OperationLog {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn from_appdata(appdata: &Path) -> Self {
        Self::new(appdata.join("codex-switch/logs/operations.jsonl"))
    }

    pub fn append(&self, record: &OperationRecord) -> Result<(), String> {
        let _guard = LOG_WRITE_LOCK
            .lock()
            .map_err(|_| "operation log lock is poisoned".to_string())?;
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("failed to create operation log directory: {error}"))?;
        }
        let mut encoded = serde_json::to_vec(record)
            .map_err(|error| format!("failed to serialize operation record: {error}"))?;
        encoded.push(b'\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|error| format!("failed to open operation log: {error}"))?;
        file.write_all(&encoded)
            .map_err(|error| format!("failed to append operation log: {error}"))?;
        file.sync_data()
            .map_err(|error| format!("failed to sync operation log: {error}"))
    }

    pub fn list(&self, limit: usize) -> Result<Vec<OperationRecord>, String> {
        if !self.path.exists() || limit == 0 {
            return Ok(Vec::new());
        }
        let file = fs::File::open(&self.path)
            .map_err(|error| format!("failed to open operation log: {error}"))?;
        let mut records = Vec::new();
        for line in BufReader::new(file).lines() {
            let line = line.map_err(|error| format!("failed to read operation log: {error}"))?;
            if line.trim().is_empty() {
                continue;
            }
            records.push(
                serde_json::from_str(&line)
                    .map_err(|error| format!("failed to parse operation record: {error}"))?,
            );
        }
        records.reverse();
        records.truncate(limit);
        Ok(records)
    }
}

pub fn operation_id(prefix: &str) -> Result<String, String> {
    Ok(format!(
        "{prefix}-{}-{}-{}",
        timestamp_millis()?,
        std::process::id(),
        OPERATION_COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

pub fn timestamp_millis() -> Result<u128, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .map_err(|error| format!("system clock before unix epoch: {error}"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use tempfile::tempdir;

    use super::{
        operation_id, OperationAction, OperationLog, OperationPhase, OperationRecord,
        OperationStatus,
    };

    #[test]
    fn appends_terminal_records_and_lists_newest_first() {
        let root = tempdir().unwrap();
        let log = OperationLog::new(root.path().join("operations.jsonl"));
        for (id, completed) in [("first", 1), ("second", 2)] {
            log.append(&OperationRecord {
                operation_id: id.to_string(),
                action: OperationAction::SyncSessions,
                status: OperationStatus::Succeeded,
                phase: OperationPhase::Complete,
                started_at_ms: 0,
                completed_at_ms: completed,
                backup_dirs: Vec::new(),
                counts: BTreeMap::from([("insertedThreads".to_string(), completed as usize)]),
            })
            .unwrap();
        }

        let records = log.list(1).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].operation_id, "second");
        assert_eq!(records[0].counts["insertedThreads"], 2);
    }

    #[test]
    fn operation_ids_are_unique_without_containing_credentials() {
        let first = operation_id("switch-runtime").unwrap();
        let second = operation_id("switch-runtime").unwrap();
        assert_ne!(first, second);
        assert!(!first.contains("token"));
        assert!(!first.contains("key"));
    }
}

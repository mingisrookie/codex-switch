use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(windows)]
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde::{Deserialize, Serialize};

use crate::file_ops::atomic_write;

static OPERATION_COUNTER: AtomicU64 = AtomicU64::new(0);
static OPERATION_LOG_LOCK: Mutex<()> = Mutex::new(());
const PROTECTED_RECORD_PREFIX: &[u8] = b"csop1:";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum OperationAction {
    ImportAccount,
    SaveRelay,
    VerifyRelay,
    SwitchRuntime,
    IncrementalSync,
    SyncSessions,
    DeleteSessions,
    RestoreVisibility,
    CreateBackup,
    DeleteBackup,
    RestoreBackup,
    CleanupCheckpoints,
    InstallSkill,
    ConfigureSkill,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum OperationStatus {
    Succeeded,
    Failed,
    RolledBack,
    RollbackFailed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
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
        self.append_with_lock(record, || {})
    }

    fn append_with_lock<F>(&self, record: &OperationRecord, on_locked: F) -> Result<(), String>
    where
        F: FnOnce(),
    {
        self.append_with_lock_and_writer(record, on_locked, atomic_write)
    }

    fn append_with_lock_and_writer<F, WriteLog>(
        &self,
        record: &OperationRecord,
        on_locked: F,
        write_log: WriteLog,
    ) -> Result<(), String>
    where
        F: FnOnce(),
        WriteLog: FnOnce(&Path, &[u8]) -> Result<(), String>,
    {
        let _guard = OPERATION_LOG_LOCK
            .lock()
            .map_err(|_| "operation log lock is poisoned".to_string())?;
        on_locked();
        let existing_payload = if self.path.exists() {
            fs::read(&self.path)
                .map_err(|error| format!("failed to read operation log before append: {error}"))?
        } else {
            Vec::new()
        };
        let existing = parse_operation_records_strict(&existing_payload)?;
        if existing
            .iter()
            .any(|existing| existing.operation_id == record.operation_id)
        {
            return Err("operation log already contains this operation ID".to_string());
        }
        let mut payload = Vec::new();
        for existing_record in &existing {
            payload.extend_from_slice(&encode_operation_record(existing_record)?);
            payload.push(b'\n');
        }
        let mut encoded = encode_operation_record(record)?;
        encoded.push(b'\n');
        payload.extend_from_slice(&encoded);
        write_log(&self.path, &payload)
    }

    pub fn list(&self, limit: usize) -> Result<Vec<OperationRecord>, String> {
        self.list_with_lock(limit, || {})
    }

    pub fn list_all_strict(&self) -> Result<Vec<OperationRecord>, String> {
        let _guard = OPERATION_LOG_LOCK
            .lock()
            .map_err(|_| "operation log lock is poisoned".to_string())?;
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let payload = fs::read(&self.path)
            .map_err(|error| format!("failed to read operation log: {error}"))?;
        let mut records = parse_operation_records_strict(&payload)?;
        records.reverse();
        Ok(records)
    }

    pub fn prune_completed_before(&self, cutoff_ms: u128) -> Result<usize, String> {
        let _guard = OPERATION_LOG_LOCK
            .lock()
            .map_err(|_| "operation log lock is poisoned".to_string())?;
        if !self.path.exists() {
            return Ok(0);
        }
        let payload = fs::read(&self.path)
            .map_err(|error| format!("failed to read operation log before retention: {error}"))?;
        let records = parse_operation_records_strict(&payload)?;
        let retained = records
            .iter()
            .filter(|record| record.completed_at_ms >= cutoff_ms)
            .collect::<Vec<_>>();
        let removed = records.len().saturating_sub(retained.len());
        let mut encoded = Vec::new();
        for record in retained {
            encoded.extend_from_slice(&encode_operation_record(record)?);
            encoded.push(b'\n');
        }
        atomic_write(&self.path, &encoded)?;
        Ok(removed)
    }

    fn list_with_lock<F>(&self, limit: usize, on_locked: F) -> Result<Vec<OperationRecord>, String>
    where
        F: FnOnce(),
    {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let _guard = OPERATION_LOG_LOCK
            .lock()
            .map_err(|_| "operation log lock is poisoned".to_string())?;
        on_locked();
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let payload = fs::read(&self.path)
            .map_err(|error| format!("failed to read operation log: {error}"))?;
        let lines = payload
            .split(|byte| *byte == b'\n')
            .filter(|line| line.iter().any(|byte| !byte.is_ascii_whitespace()))
            .collect::<Vec<_>>();
        let mut records = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            match decode_operation_record(line) {
                Ok(record) => records.push(record),
                Err(_) if index + 1 == lines.len() => break,
                Err(error) => {
                    return Err(format!("failed to parse operation record: {error}"));
                }
            }
        }
        records.reverse();
        records.truncate(limit);
        Ok(records)
    }
}

fn parse_operation_records_strict(payload: &[u8]) -> Result<Vec<OperationRecord>, String> {
    payload
        .split(|byte| *byte == b'\n')
        .filter(|line| line.iter().any(|byte| !byte.is_ascii_whitespace()))
        .map(decode_operation_record)
        .collect()
}

fn encode_operation_record(record: &OperationRecord) -> Result<Vec<u8>, String> {
    let plaintext = serde_json::to_vec(record)
        .map_err(|error| format!("failed to serialize operation record: {error}"))?;
    #[cfg(windows)]
    {
        let ciphertext = crate::crypto::protect(&plaintext)
            .map_err(|_| "failed to protect operation record".to_string())?;
        let mut output = Vec::with_capacity(PROTECTED_RECORD_PREFIX.len() + ciphertext.len() * 2);
        output.extend_from_slice(PROTECTED_RECORD_PREFIX);
        output.extend_from_slice(BASE64.encode(ciphertext).as_bytes());
        Ok(output)
    }
    #[cfg(not(windows))]
    {
        Ok(plaintext)
    }
}

fn decode_operation_record(line: &[u8]) -> Result<OperationRecord, String> {
    let plaintext = if let Some(encoded) = line.strip_prefix(PROTECTED_RECORD_PREFIX) {
        #[cfg(windows)]
        {
            let ciphertext = BASE64.decode(encoded).map_err(|_| {
                "failed to parse operation record: protected payload is invalid".to_string()
            })?;
            crate::crypto::unprotect(&ciphertext).map_err(|_| {
                "failed to parse operation record: protected payload is unreadable".to_string()
            })?
        }
        #[cfg(not(windows))]
        {
            return Err(
                "failed to parse operation record: protected payload is unsupported".to_string(),
            );
        }
    } else {
        line.to_vec()
    };
    serde_json::from_slice(&plaintext)
        .map_err(|error| format!("failed to parse operation record: {error}"))
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
    use std::{
        collections::BTreeMap,
        fs::{self, OpenOptions},
        io::Write,
        sync::mpsc,
        thread,
        time::Duration,
    };

    use tempfile::tempdir;

    use super::{
        operation_id, OperationAction, OperationLog, OperationPhase, OperationRecord,
        OperationStatus,
    };

    fn record(id: &str, completed_at_ms: u128) -> OperationRecord {
        OperationRecord {
            operation_id: id.to_string(),
            action: OperationAction::SyncSessions,
            status: OperationStatus::Succeeded,
            phase: OperationPhase::Complete,
            started_at_ms: 0,
            completed_at_ms,
            backup_dirs: Vec::new(),
            counts: BTreeMap::from([("insertedThreads".to_string(), completed_at_ms as usize)]),
        }
    }

    #[test]
    fn appends_terminal_records_and_lists_newest_first() {
        let root = tempdir().unwrap();
        let log = OperationLog::new(root.path().join("operations.jsonl"));
        for (id, completed) in [("first", 1), ("second", 2)] {
            log.append(&record(id, completed)).unwrap();
        }

        let records = log.list(1).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].operation_id, "second");
        assert_eq!(records[0].counts["insertedThreads"], 2);
    }

    #[test]
    fn ignores_one_damaged_tail_record_but_rejects_internal_corruption() {
        let root = tempdir().unwrap();
        let path = root.path().join("operations.jsonl");
        let log = OperationLog::new(path.clone());
        log.append(&record("first", 1)).unwrap();
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(br#"{"operationId":"truncated""#)
            .unwrap();

        let records = log.list(10).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].operation_id, "first");

        fs::write(
            &path,
            format!(
                "{}\nnot-json\n",
                serde_json::to_string(&record("first", 1)).unwrap(),
            ),
        )
        .unwrap();
        let records = log.list(10).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].operation_id, "first");

        fs::write(
            &path,
            format!(
                "{}\nnot-json\n{}\n",
                serde_json::to_string(&record("first", 1)).unwrap(),
                serde_json::to_string(&record("second", 2)).unwrap(),
            ),
        )
        .unwrap();
        let error = log.list(10).unwrap_err();
        assert!(
            error.contains("failed to parse operation record"),
            "{error}"
        );
    }

    #[test]
    fn retention_prunes_only_records_older_than_the_cutoff() {
        let root = tempdir().unwrap();
        let log = OperationLog::new(root.path().join("operations.jsonl"));
        for (id, completed) in [("old", 10), ("boundary", 20), ("new", 30)] {
            log.append(&record(id, completed)).unwrap();
        }

        assert_eq!(log.prune_completed_before(20).unwrap(), 1);
        let records = log.list_all_strict().unwrap();
        assert_eq!(
            records
                .iter()
                .map(|record| record.operation_id.as_str())
                .collect::<Vec<_>>(),
            ["new", "boundary"]
        );
    }

    #[cfg(windows)]
    #[test]
    fn append_and_zero_removal_retention_reprotect_legacy_plaintext_records() {
        let root = tempdir().unwrap();
        let path = root.path().join("operations.jsonl");
        let log = OperationLog::new(path.clone());
        let mut legacy = record("legacy", 20);
        legacy.backup_dirs = vec![std::path::PathBuf::from(
            r"C:\Users\private-user\sensitive-backup",
        )];
        let plaintext = format!("{}\n", serde_json::to_string(&legacy).unwrap());

        fs::write(&path, plaintext.as_bytes()).unwrap();
        log.append(&record("new", 30)).unwrap();
        assert_operation_log_is_fully_protected(&path, 2);

        fs::write(&path, plaintext.as_bytes()).unwrap();
        assert_eq!(log.prune_completed_before(0).unwrap(), 0);
        assert_operation_log_is_fully_protected(&path, 1);
    }

    #[cfg(windows)]
    fn assert_operation_log_is_fully_protected(path: &std::path::Path, expected_lines: usize) {
        let payload = fs::read(path).unwrap();
        let lines = payload
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(lines.len(), expected_lines);
        assert!(lines
            .iter()
            .all(|line| line.starts_with(super::PROTECTED_RECORD_PREFIX)));
        assert!(!payload
            .windows(b"private-user".len())
            .any(|window| window == b"private-user"));
    }

    #[test]
    fn list_waits_for_an_in_progress_append() {
        let root = tempdir().unwrap();
        let log = OperationLog::new(root.path().join("operations.jsonl"));
        log.append(&record("first", 1)).unwrap();
        let writer_log = log.clone();
        let reader_log = log.clone();
        let (locked_sender, locked_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let (reader_started_sender, reader_started_receiver) = mpsc::channel();
        let (reader_locked_sender, reader_locked_receiver) = mpsc::channel();
        let (reader_done_sender, reader_done_receiver) = mpsc::channel();

        let writer = thread::spawn(move || {
            writer_log.append_with_lock(&record("second", 2), || {
                let _ = locked_sender.send(());
                let _ = release_receiver.recv();
            })
        });
        locked_receiver
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        let reader = thread::spawn(move || {
            reader_started_sender.send(()).unwrap();
            let result = reader_log.list_with_lock(10, || {
                let _ = reader_locked_sender.send(());
            });
            reader_done_sender.send(result).unwrap();
        });
        reader_started_receiver
            .recv_timeout(Duration::from_secs(1))
            .unwrap();

        let reader_entered_while_append_was_locked = reader_locked_receiver
            .recv_timeout(Duration::from_millis(50))
            .is_ok();
        release_sender.send(()).unwrap();
        writer.join().unwrap().unwrap();
        if !reader_entered_while_append_was_locked {
            reader_locked_receiver
                .recv_timeout(Duration::from_secs(1))
                .unwrap();
        }
        let records = reader_done_receiver
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        reader.join().unwrap();

        assert!(!reader_entered_while_append_was_locked);
        assert_eq!(
            records
                .iter()
                .map(|record| record.operation_id.as_str())
                .collect::<Vec<_>>(),
            vec!["second", "first"]
        );
    }

    #[test]
    fn operation_ids_are_unique_without_containing_credentials() {
        let first = operation_id("switch-runtime").unwrap();
        let second = operation_id("switch-runtime").unwrap();
        assert_ne!(first, second);
        assert!(!first.contains("token"));
        assert!(!first.contains("key"));
    }

    #[test]
    fn create_backup_action_uses_the_public_camel_case_contract() {
        assert_eq!(
            serde_json::to_string(&OperationAction::CreateBackup).unwrap(),
            "\"createBackup\""
        );
    }

    #[test]
    fn delete_backup_action_uses_the_public_camel_case_contract() {
        assert_eq!(
            serde_json::to_string(&OperationAction::DeleteBackup).unwrap(),
            "\"deleteBackup\""
        );
    }

    #[test]
    fn strict_cleanup_reader_rejects_a_truncated_final_record() {
        let root = tempdir().unwrap();
        let path = root.path().join("operations.jsonl");
        let log = OperationLog::new(path.clone());
        log.append(&record("complete", 1)).unwrap();
        let mut file = OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(br#"{"operationId":"truncated""#).unwrap();
        file.sync_data().unwrap();

        assert!(log.list(10).is_ok());
        assert!(log.list_all_strict().is_err());
    }

    #[test]
    fn failed_atomic_publish_keeps_the_previous_log_byte_exact() {
        let root = tempdir().unwrap();
        let path = root.path().join("operations.jsonl");
        let log = OperationLog::new(path.clone());
        log.append(&record("complete", 1)).unwrap();
        let before = fs::read(&path).unwrap();

        let error = log
            .append_with_lock_and_writer(
                &record("must-not-appear", 2),
                || {},
                |_path, _payload| Err("injected pre-publish failure".to_string()),
            )
            .unwrap_err();

        assert_eq!(error, "injected pre-publish failure");
        assert_eq!(fs::read(&path).unwrap(), before);
        let records = log.list_all_strict().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].operation_id, "complete");
    }

    #[test]
    fn append_rejects_a_damaged_existing_log_without_claiming_a_new_terminal() {
        let root = tempdir().unwrap();
        let path = root.path().join("operations.jsonl");
        let log = OperationLog::new(path.clone());
        log.append(&record("complete", 1)).unwrap();
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(br#"{"operationId":"truncated""#)
            .unwrap();
        let before = fs::read(&path).unwrap();

        let error = log.append(&record("must-not-appear", 2)).unwrap_err();

        assert!(
            error.contains("failed to parse operation record"),
            "{error}"
        );
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(!String::from_utf8_lossy(&before).contains("must-not-appear"));
    }

    #[test]
    fn checkpoint_cleanup_action_uses_the_public_camel_case_contract() {
        assert_eq!(
            serde_json::to_string(&OperationAction::CleanupCheckpoints).unwrap(),
            "\"cleanupCheckpoints\""
        );
    }
}

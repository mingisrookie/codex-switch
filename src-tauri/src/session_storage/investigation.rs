use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{file_ops::atomic_write, operation_log::timestamp_millis};

use super::{
    bounded_file::read_regular_file_bounded,
    model::{
        DatabaseRole, ShadowScanIssue, ShadowScanIssueCode, ShadowScanReport, StorageScanStatus,
    },
    write_barrier::DestructiveFileGuard,
};

const TASK_SCHEMA_VERSION: u32 = 1;
const TASK_ROOT: &str = "session-storage-v1/codex-investigations";
const TASK_FILE: &str = "TASK.md";
const MANIFEST_FILE: &str = "manifest.json";
const MAX_TASK_BYTES: u64 = 1024 * 1024;
const MAX_TASK_ID_BYTES: usize = 160;
pub const INVESTIGATION_TASK_RETENTION_MS: u128 = 7 * 24 * 60 * 60 * 1_000;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InvestigationDatabaseSummary {
    pub database_id: String,
    pub role: DatabaseRole,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionStorageInvestigationReceipt {
    pub task_id: String,
    pub issue_count: usize,
    pub database_count: usize,
    pub display_path: String,
    pub task_sha256: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct InvestigationTaskPruneReceipt {
    pub deleted_task_count: usize,
    pub retained_task_count: usize,
    pub blocked_task_count: usize,
    pub reclaimed_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InvestigationManifest {
    schema_version: u32,
    task_id: String,
    generated_at_ms: u128,
    expires_at_ms: u128,
    app_version: String,
    shadow_schema_version: u32,
    issue_count: usize,
    database_count: usize,
    task_sha256: String,
}

pub fn has_investigation_issues(report: &ShadowScanReport) -> bool {
    report
        .issues
        .iter()
        .any(|issue| issue.count > 0 && is_investigation_issue(issue.code))
}

pub fn create_session_storage_investigation_task(
    data_root: &Path,
    app_version: &str,
    report: &ShadowScanReport,
    databases: &[InvestigationDatabaseSummary],
) -> Result<SessionStorageInvestigationReceipt, String> {
    if !data_root.is_absolute() || !data_root.is_dir() {
        return Err("session storage investigation data root is invalid".to_string());
    }
    if !is_safe_label(app_version, 64, true) {
        return Err("session storage investigation app version is invalid".to_string());
    }
    if report.schema_version == 0
        || report.scan_id.is_empty()
        || report.scan_id.len() > MAX_TASK_ID_BYTES
    {
        return Err("session storage investigation report identity is invalid".to_string());
    }
    let issues = report
        .issues
        .iter()
        .filter(|issue| issue.count > 0 && is_investigation_issue(issue.code))
        .cloned()
        .collect::<Vec<_>>();
    if issues.is_empty() {
        return Err(
            "no out-of-scope session storage issue requires Codex investigation".to_string(),
        );
    }
    let issue_count = checked_issue_count(&issues)?;
    validate_database_summaries(databases)?;
    let generated_at_ms = timestamp_millis()?;
    let expires_at_ms = generated_at_ms
        .checked_add(INVESTIGATION_TASK_RETENTION_MS)
        .ok_or_else(|| "session storage investigation expiry overflowed".to_string())?;
    let scan_key = hex_sha256(Sha256::digest(report.scan_id.as_bytes()));
    let task_id = format!("codex-investigation-{generated_at_ms}-{}", &scan_key[..12]);
    validate_task_id(&task_id)?;
    let task_dir = create_safe_task_directory(data_root, &task_id)?;

    let markdown = render_task(app_version, report, &issues, issue_count, databases);
    if markdown.len() as u64 > MAX_TASK_BYTES {
        return Err("session storage investigation task reached its size limit".to_string());
    }
    let task_sha256 = hex_sha256(Sha256::digest(markdown.as_bytes()));
    atomic_write(&task_dir.join(TASK_FILE), markdown.as_bytes())?;
    let manifest = InvestigationManifest {
        schema_version: TASK_SCHEMA_VERSION,
        task_id: task_id.clone(),
        generated_at_ms,
        expires_at_ms,
        app_version: app_version.to_string(),
        shadow_schema_version: report.schema_version,
        issue_count,
        database_count: databases.len(),
        task_sha256: task_sha256.clone(),
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|_| "failed to serialize session storage investigation manifest".to_string())?;
    atomic_write(&task_dir.join(MANIFEST_FILE), &manifest_bytes)?;
    verify_investigation_task(data_root, &task_id)?;
    Ok(SessionStorageInvestigationReceipt {
        task_id: task_id.clone(),
        issue_count: manifest.issue_count,
        database_count: manifest.database_count,
        display_path: format!("[app-data]/codex-switch/{TASK_ROOT}/{task_id}/{TASK_FILE}"),
        task_sha256,
    })
}

pub fn verify_investigation_task(
    data_root: &Path,
    task_id: &str,
) -> Result<std::path::PathBuf, String> {
    let task_dir = investigation_task_dir(data_root, task_id)?;
    validate_safe_task_directory(data_root, &task_dir)?;
    let task_bytes = read_regular_file_bounded(&task_dir.join(TASK_FILE), MAX_TASK_BYTES)
        .map_err(|_| "session storage investigation task is unreadable".to_string())?;
    let manifest_bytes =
        read_regular_file_bounded(&task_dir.join(MANIFEST_FILE), MAX_TASK_BYTES)
            .map_err(|_| "session storage investigation manifest is unreadable".to_string())?;
    parse_and_validate_manifest(task_id, &task_bytes, &manifest_bytes)?;
    Ok(task_dir)
}

pub fn prune_expired_investigation_tasks(
    data_root: &Path,
    now_ms: u128,
) -> Result<InvestigationTaskPruneReceipt, String> {
    validate_existing_safe_directory(data_root)?;
    if !data_root.is_absolute() {
        return Err("session storage investigation data root is invalid".to_string());
    }
    let storage_root = data_root.join("session-storage-v1");
    if !validate_optional_safe_directory(&storage_root)? {
        return Ok(InvestigationTaskPruneReceipt::default());
    }
    let investigations_root = data_root.join(TASK_ROOT);
    if !validate_optional_safe_directory(&investigations_root)? {
        return Ok(InvestigationTaskPruneReceipt::default());
    }

    let mut receipt = InvestigationTaskPruneReceipt::default();
    let entries = fs::read_dir(&investigations_root)
        .map_err(|_| "session storage investigation directory is unreadable".to_string())?;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                receipt.blocked_task_count = receipt.blocked_task_count.saturating_add(1);
                continue;
            }
        };
        match prune_investigation_task_candidate(data_root, &entry.path(), now_ms) {
            Ok(PruneCandidateOutcome::Deleted { reclaimed_bytes }) => {
                receipt.deleted_task_count = receipt.deleted_task_count.saturating_add(1);
                receipt.reclaimed_bytes = receipt
                    .reclaimed_bytes
                    .checked_add(reclaimed_bytes)
                    .ok_or_else(|| {
                        "session storage investigation reclaimed byte count overflowed".to_string()
                    })?;
            }
            Ok(PruneCandidateOutcome::Retained) => {
                receipt.retained_task_count = receipt.retained_task_count.saturating_add(1);
            }
            Err(_) => {
                receipt.blocked_task_count = receipt.blocked_task_count.saturating_add(1);
            }
        }
    }
    Ok(receipt)
}

enum PruneCandidateOutcome {
    Deleted { reclaimed_bytes: u64 },
    Retained,
}

fn prune_investigation_task_candidate(
    data_root: &Path,
    task_dir: &Path,
    now_ms: u128,
) -> Result<PruneCandidateOutcome, String> {
    let parent = task_dir
        .parent()
        .ok_or_else(|| "session storage investigation task has no parent".to_string())?;
    if parent != data_root.join(TASK_ROOT) {
        return Err("session storage investigation task is not a direct child".to_string());
    }
    let task_id = task_dir
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| "session storage investigation task id is invalid".to_string())?;
    validate_task_id(task_id)?;
    validate_safe_task_directory(data_root, task_dir)?;
    validate_exact_task_inventory(task_dir)?;

    let task_path = task_dir.join(TASK_FILE);
    let manifest_path = task_dir.join(MANIFEST_FILE);
    let task_bytes = read_regular_file_bounded(&task_path, MAX_TASK_BYTES)
        .map_err(|_| "session storage investigation task is unreadable".to_string())?;
    let manifest_bytes = read_regular_file_bounded(&manifest_path, MAX_TASK_BYTES)
        .map_err(|_| "session storage investigation manifest is unreadable".to_string())?;
    let manifest = parse_and_validate_manifest(task_id, &task_bytes, &manifest_bytes)?;
    if now_ms < manifest.expires_at_ms {
        return Ok(PruneCandidateOutcome::Retained);
    }

    let task_sha256 = hex_sha256(Sha256::digest(&task_bytes));
    let manifest_sha256 = hex_sha256(Sha256::digest(&manifest_bytes));
    let mut task_guard = DestructiveFileGuard::acquire(&task_path)?;
    let mut manifest_guard = DestructiveFileGuard::acquire(&manifest_path)?;
    let (task_size, _) = task_guard.verify_current_path(Some(&task_sha256))?;
    let (manifest_size, _) = manifest_guard.verify_current_path(Some(&manifest_sha256))?;
    validate_safe_task_directory(data_root, task_dir)?;
    validate_exact_task_inventory(task_dir)?;

    task_guard.verify_current_path(Some(&manifest.task_sha256))?;
    manifest_guard.verify_current_path(Some(&manifest_sha256))?;
    task_guard.delete()?;
    manifest_guard.delete()?;
    fs::remove_dir(task_dir)
        .map_err(|_| "failed to remove expired session storage investigation task".to_string())?;
    let reclaimed_bytes = task_size
        .checked_add(manifest_size)
        .ok_or_else(|| "session storage investigation task size overflowed".to_string())?;
    Ok(PruneCandidateOutcome::Deleted { reclaimed_bytes })
}

fn validate_optional_safe_directory(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata_is_link_or_reparse(&metadata) => Ok(true),
        Ok(_) => Err("session storage investigation directory is unsafe".to_string()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(_) => Err("session storage investigation directory is unavailable".to_string()),
    }
}

fn validate_exact_task_inventory(task_dir: &Path) -> Result<(), String> {
    let mut names = Vec::new();
    for entry in fs::read_dir(task_dir)
        .map_err(|_| "session storage investigation task is unreadable".to_string())?
    {
        let entry = entry
            .map_err(|_| "session storage investigation task entry is unreadable".to_string())?;
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|_| "session storage investigation task entry is unavailable".to_string())?;
        if !metadata.is_file() || metadata_is_link_or_reparse(&metadata) {
            return Err("session storage investigation task inventory is unsafe".to_string());
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "session storage investigation task inventory is invalid".to_string())?;
        names.push(name);
    }
    names.sort();
    if names != [TASK_FILE.to_string(), MANIFEST_FILE.to_string()] {
        return Err("session storage investigation task inventory changed".to_string());
    }
    Ok(())
}

fn parse_and_validate_manifest(
    task_id: &str,
    task_bytes: &[u8],
    manifest_bytes: &[u8],
) -> Result<InvestigationManifest, String> {
    let manifest = serde_json::from_slice::<InvestigationManifest>(manifest_bytes)
        .map_err(|_| "session storage investigation manifest is invalid".to_string())?;
    let generated_at_ms = task_generated_at_ms(task_id)?;
    let expected_expiry = manifest
        .generated_at_ms
        .checked_add(INVESTIGATION_TASK_RETENTION_MS)
        .ok_or_else(|| "session storage investigation expiry is invalid".to_string())?;
    if manifest.schema_version != TASK_SCHEMA_VERSION
        || manifest.task_id != task_id
        || manifest.generated_at_ms != generated_at_ms
        || manifest.expires_at_ms != expected_expiry
        || !is_safe_label(&manifest.app_version, 64, true)
        || manifest.shadow_schema_version == 0
        || manifest.issue_count == 0
        || manifest.task_sha256 != hex_sha256(Sha256::digest(task_bytes))
    {
        return Err("session storage investigation task integrity check failed".to_string());
    }
    Ok(manifest)
}

fn render_task(
    app_version: &str,
    report: &ShadowScanReport,
    issues: &[ShadowScanIssue],
    issue_count: usize,
    databases: &[InvestigationDatabaseSummary],
) -> String {
    let mut output = format!(
        "# Codex Switch 本地只读排查任务\n\n> 先只读排查，不修改 SQLite、JSONL、配置、运行态或备份。不要输出会话正文、凭据、完整用户路径或 threadId。\n\n## 任务元数据\n\n- task schema: `{TASK_SCHEMA_VERSION}`\n- Codex Switch version: `{app_version}`\n- Shadow schema: `{}`\n- Shadow status: `{}`\n- scan identity: `[scan:{}]`\n\n## 问题类型\n\n",
        report.schema_version,
        scan_status_name(report.status),
        &hex_sha256(Sha256::digest(report.scan_id.as_bytes()))[..12]
    );
    for issue in issues {
        output.push_str(&format!(
            "- `{}`: {}\n",
            issue_code_name(issue.code),
            issue.count
        ));
    }
    output.push_str(
        "\n## 脱敏路径与数据库区域\n\n- `[canonical-home]`\n- `[app-data]/codex-switch`\n",
    );
    for database in databases {
        output.push_str(&format!(
            "- `[database:{}:{}]`\n",
            database.database_id,
            database_role_name(database.role)
        ));
    }
    output.push_str(&format!(
        "\n## 引用与完整性摘要\n\n- runtime databases: {}\n- backup databases: {}\n- runtime references: {}\n- missing runtime references: {}\n- mismatched runtime references: {}\n- session files: {}\n- parse/discovery issue records: {}\n- persisted Shadow report: verified by the application loader\n\n## 建议只读检查步骤\n\n1. 核对上述数据库区域的 schema/version、`PRAGMA quick_check` 和 WAL 状态，只读取一致快照。\n2. 对 missing/mismatched reference 只记录脱敏数据库区域、计数和引用类型；不要打开或复制会话正文。\n3. 对 JSONL 异常只验证文件存在性、大小、结构完整性和工具调用配对；不要输出消息、回复、Prompt、附件或工具输出。\n4. 区分当前运行库、备份库存、legacy/relocated 与来源不明文件；不要把备份引用当成运行时引用。\n5. 输出只读结论、可复现检查命令和仍未知的边界；任何修复方案必须另行获得用户授权。\n",
        report.summary.runtime_database_count,
        report.summary.backup_database_count,
        report.summary.runtime_reference_count,
        report.summary.missing_runtime_reference_count,
        report.summary.mismatched_runtime_reference_count,
        report.summary.session_file_count,
        issue_count,
    ));
    output
}

fn validate_database_summaries(databases: &[InvestigationDatabaseSummary]) -> Result<(), String> {
    for database in databases {
        if database.database_id.is_empty()
            || database.database_id.len() > 64
            || !database
                .database_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err("session storage investigation database id is invalid".to_string());
        }
    }
    Ok(())
}

fn checked_issue_count(issues: &[ShadowScanIssue]) -> Result<usize, String> {
    issues.iter().try_fold(0_usize, |total, issue| {
        total
            .checked_add(issue.count)
            .ok_or_else(|| "session storage investigation issue count overflowed".to_string())
    })
}

fn create_safe_task_directory(data_root: &Path, task_id: &str) -> Result<PathBuf, String> {
    validate_existing_safe_directory(data_root)?;
    let storage_root = data_root.join("session-storage-v1");
    let investigations_root = data_root.join(TASK_ROOT);
    for directory in [&storage_root, &investigations_root] {
        match fs::create_dir(directory) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(_) => {
                return Err("failed to create session storage investigation directory".to_string())
            }
        }
        validate_existing_safe_directory(directory)?;
    }
    let task_dir = investigation_task_dir(data_root, task_id)?;
    fs::create_dir(&task_dir).map_err(|error| {
        if error.kind() == ErrorKind::AlreadyExists {
            "session storage investigation task already exists".to_string()
        } else {
            "failed to create session storage investigation task".to_string()
        }
    })?;
    validate_existing_safe_directory(&task_dir)?;
    Ok(task_dir)
}

fn validate_safe_task_directory(data_root: &Path, task_dir: &Path) -> Result<(), String> {
    validate_existing_safe_directory(data_root)?;
    validate_existing_safe_directory(&data_root.join("session-storage-v1"))?;
    validate_existing_safe_directory(&data_root.join(TASK_ROOT))?;
    validate_existing_safe_directory(task_dir)
}

fn validate_existing_safe_directory(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| "session storage investigation directory is unavailable".to_string())?;
    if !metadata.is_dir() || metadata_is_link_or_reparse(&metadata) {
        return Err("session storage investigation directory is unsafe".to_string());
    }
    Ok(())
}

fn investigation_task_dir(data_root: &Path, task_id: &str) -> Result<std::path::PathBuf, String> {
    validate_task_id(task_id)?;
    Ok(data_root.join(TASK_ROOT).join(task_id))
}

fn validate_task_id(task_id: &str) -> Result<(), String> {
    task_generated_at_ms(task_id).map(|_| ())
}

fn task_generated_at_ms(task_id: &str) -> Result<u128, String> {
    if !is_safe_label(task_id, MAX_TASK_ID_BYTES, false) {
        return Err("session storage investigation task id is invalid".to_string());
    }
    let body = task_id
        .strip_prefix("codex-investigation-")
        .ok_or_else(|| "session storage investigation task id is invalid".to_string())?;
    let (generated_at, scan_key) = body
        .rsplit_once('-')
        .ok_or_else(|| "session storage investigation task id is invalid".to_string())?;
    if generated_at.starts_with('0')
        || scan_key.len() != 12
        || !scan_key
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err("session storage investigation task id is invalid".to_string());
    }
    generated_at
        .parse::<u128>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| "session storage investigation task id is invalid".to_string())
}

fn is_safe_label(value: &str, max_bytes: usize, allow_semver_punctuation: bool) -> bool {
    !value.is_empty()
        && value.len() <= max_bytes
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'-' | b'_')
                || allow_semver_punctuation && matches!(byte, b'.' | b'+')
        })
}

fn scan_status_name(status: StorageScanStatus) -> &'static str {
    match status {
        StorageScanStatus::NoSessions => "noSessions",
        StorageScanStatus::CanonicalReady => "canonicalReady",
        StorageScanStatus::MigrationAvailable => "migrationAvailable",
        StorageScanStatus::ReviewRequired => "reviewRequired",
    }
}

fn database_role_name(role: DatabaseRole) -> &'static str {
    match role {
        DatabaseRole::CanonicalAccount => "canonicalAccount",
        DatabaseRole::AccountView => "accountView",
        DatabaseRole::Relay => "relay",
        DatabaseRole::Shared => "shared",
        DatabaseRole::LegacyOrRelocated => "legacyOrRelocated",
        DatabaseRole::Backup => "backup",
        DatabaseRole::RecoveryPackage => "recoveryPackage",
        DatabaseRole::DowngradeExport => "downgradeExport",
        DatabaseRole::UnknownRuntime => "unknownRuntime",
    }
}

fn is_investigation_issue(code: ShadowScanIssueCode) -> bool {
    matches!(
        code,
        ShadowScanIssueCode::DatabaseDiscoveryFailed
            | ShadowScanIssueCode::DatabaseSnapshotFailed
            | ShadowScanIssueCode::DatabaseRowMissingRolloutPath
            | ShadowScanIssueCode::SessionDiscoveryFailed
            | ShadowScanIssueCode::SessionParseFailed
            | ShadowScanIssueCode::MissingRuntimeReference
            | ShadowScanIssueCode::MismatchedRuntimeReference
            | ShadowScanIssueCode::TurnProvenanceInvalid
            | ShadowScanIssueCode::StorageStateInvalid
    )
}

fn issue_code_name(code: ShadowScanIssueCode) -> &'static str {
    match code {
        ShadowScanIssueCode::DatabaseDiscoveryFailed => "databaseDiscoveryFailed",
        ShadowScanIssueCode::DatabaseSnapshotFailed => "databaseSnapshotFailed",
        ShadowScanIssueCode::DatabaseRowMissingRolloutPath => "databaseRowMissingRolloutPath",
        ShadowScanIssueCode::SessionDiscoveryFailed => "sessionDiscoveryFailed",
        ShadowScanIssueCode::SessionParseFailed => "sessionParseFailed",
        ShadowScanIssueCode::MissingRuntimeReference => "missingRuntimeReference",
        ShadowScanIssueCode::MismatchedRuntimeReference => "mismatchedRuntimeReference",
        ShadowScanIssueCode::TurnProvenanceInvalid => "turnProvenanceInvalid",
        ShadowScanIssueCode::StorageStateInvalid => "storageStateInvalid",
        ShadowScanIssueCode::InvalidProviderMarker => "invalidProviderMarker",
        ShadowScanIssueCode::DivergentSession => "divergentSession",
        ShadowScanIssueCode::OnlineSnapshotNotAtomic => "onlineSnapshotNotAtomic",
        ShadowScanIssueCode::ReportPersistenceFailed => "reportPersistenceFailed",
        ShadowScanIssueCode::HashCacheInvalid => "hashCacheInvalid",
        ShadowScanIssueCode::HashCachePersistenceFailed => "hashCachePersistenceFailed",
    }
}

fn hex_sha256(value: impl AsRef<[u8]>) -> String {
    value
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
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

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::{
        create_session_storage_investigation_task, prune_expired_investigation_tasks,
        verify_investigation_task, InvestigationDatabaseSummary, InvestigationManifest,
    };
    use crate::session_storage::model::{
        DatabaseRole, RelationCounts, ShadowScanIssue, ShadowScanIssueCode, ShadowScanReport,
        ShadowScanSummary, StorageScanStatus,
    };

    #[test]
    fn investigation_task_is_read_only_sanitized_and_integrity_bound() {
        let root = tempdir().unwrap();
        let data = root.path().join("private-user/AppData/codex-switch");
        std::fs::create_dir_all(&data).unwrap();
        let report = report_with_issue();
        let receipt = create_session_storage_investigation_task(
            &data,
            "0.3.0",
            &report,
            &[InvestigationDatabaseSummary {
                database_id: "db-0000".to_string(),
                role: DatabaseRole::CanonicalAccount,
            }],
        )
        .unwrap();
        let task_dir = verify_investigation_task(&data, &receipt.task_id).unwrap();
        let markdown = std::fs::read_to_string(task_dir.join("TASK.md")).unwrap();
        assert!(markdown.contains("先只读排查"));
        assert!(markdown.contains("[canonical-home]"));
        assert!(markdown.contains("missingRuntimeReference"));
        assert!(!markdown.contains(&data.to_string_lossy().to_string()));
        assert!(!markdown.contains("fixture-token"));
        assert!(!markdown.contains("11111111-1111-4111-8111-111111111111"));
        assert!(receipt.display_path.starts_with("[app-data]/"));

        std::fs::write(task_dir.join("TASK.md"), "tampered").unwrap();
        assert!(verify_investigation_task(&data, &receipt.task_id).is_err());
    }

    #[test]
    fn handled_conflicts_do_not_generate_a_codex_investigation_task() {
        let root = tempdir().unwrap();
        let data = root.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let mut report = report_with_issue();
        report.issues = vec![ShadowScanIssue {
            code: ShadowScanIssueCode::DivergentSession,
            count: 1,
        }];
        assert!(create_session_storage_investigation_task(&data, "0.3.0", &report, &[]).is_err());
    }

    #[test]
    fn rejects_unbounded_issue_counts_and_unsafe_labels() {
        let root = tempdir().unwrap();
        let data = root.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let mut report = report_with_issue();
        report.issues.push(ShadowScanIssue {
            code: ShadowScanIssueCode::SessionParseFailed,
            count: usize::MAX,
        });
        assert!(
            create_session_storage_investigation_task(&data, "0.3.0", &report, &[])
                .unwrap_err()
                .contains("overflowed")
        );

        let report = report_with_issue();
        assert!(
            create_session_storage_investigation_task(&data, "0.3.0\nsecret", &report, &[])
                .is_err()
        );
    }

    #[test]
    fn investigation_task_retention_obeys_the_manifest_expiry_boundary_and_is_idempotent() {
        let root = tempdir().unwrap();
        let data = root.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let receipt =
            create_session_storage_investigation_task(&data, "0.3.0", &report_with_issue(), &[])
                .unwrap();
        let task_dir = verify_investigation_task(&data, &receipt.task_id).unwrap();
        let manifest: InvestigationManifest =
            serde_json::from_slice(&std::fs::read(task_dir.join("manifest.json")).unwrap())
                .unwrap();

        let retained =
            prune_expired_investigation_tasks(&data, manifest.expires_at_ms - 1).unwrap();
        assert_eq!(retained.deleted_task_count, 0);
        assert_eq!(retained.retained_task_count, 1);
        assert_eq!(retained.blocked_task_count, 0);
        assert!(task_dir.exists());

        let deleted = prune_expired_investigation_tasks(&data, manifest.expires_at_ms).unwrap();
        assert_eq!(deleted.deleted_task_count, 1);
        assert_eq!(deleted.retained_task_count, 0);
        assert_eq!(deleted.blocked_task_count, 0);
        assert!(deleted.reclaimed_bytes > 0);
        assert!(!task_dir.exists());

        let repeated = prune_expired_investigation_tasks(&data, manifest.expires_at_ms).unwrap();
        assert_eq!(repeated, Default::default());
    }

    #[test]
    fn investigation_task_retention_blocks_corrupt_or_extended_inventories() {
        let root = tempdir().unwrap();
        let data = root.path().join("data");
        std::fs::create_dir_all(&data).unwrap();

        let corrupt =
            create_session_storage_investigation_task(&data, "0.3.0", &report_with_issue(), &[])
                .unwrap();
        let corrupt_dir = data
            .join("session-storage-v1/codex-investigations")
            .join(&corrupt.task_id);
        std::fs::write(corrupt_dir.join("manifest.json"), b"{}").unwrap();

        let mut extended_report = report_with_issue();
        extended_report.scan_id = "scan-private-source-with-extra-file".to_string();
        let extended =
            create_session_storage_investigation_task(&data, "0.3.0", &extended_report, &[])
                .unwrap();
        let extended_dir = data
            .join("session-storage-v1/codex-investigations")
            .join(&extended.task_id);
        std::fs::write(extended_dir.join("unexpected.txt"), b"preserve me").unwrap();

        let result = prune_expired_investigation_tasks(&data, u128::MAX).unwrap();
        assert_eq!(result.deleted_task_count, 0);
        assert_eq!(result.retained_task_count, 0);
        assert_eq!(result.blocked_task_count, 2);
        assert_eq!(result.reclaimed_bytes, 0);
        assert!(corrupt_dir.join("TASK.md").exists());
        assert!(extended_dir.join("TASK.md").exists());
        assert!(extended_dir.join("unexpected.txt").exists());
    }

    fn report_with_issue() -> ShadowScanReport {
        ShadowScanReport {
            schema_version: 1,
            scan_id: "scan-private-source".to_string(),
            generated_at_ms: 1,
            status: StorageScanStatus::ReviewRequired,
            migration_required: true,
            deletion_enabled: false,
            summary: ShadowScanSummary {
                schema_version: 1,
                online_scan_only: true,
                non_atomic_across_databases: true,
                logical_session_count: 1,
                canonical_candidate_count: 1,
                duplicated_session_count: 0,
                conflict_session_count: 0,
                high_confidence_copy_count: 0,
                session_file_count: 1,
                session_bytes: 100,
                potential_reclaim_bytes: 0,
                marker_file_count: 0,
                runtime_database_count: 1,
                backup_database_count: 0,
                runtime_reference_count: 1,
                missing_runtime_reference_count: 1,
                mismatched_runtime_reference_count: 0,
                cache_hit_count: 0,
                cache_miss_count: 1,
                stable_file_count: 1,
                turn_context_count: 0,
                resolved_turn_provenance_count: 0,
                historical_unknown_turn_count: 0,
                incomplete_turn_provenance_count: 0,
                relation_counts: RelationCounts::default(),
            },
            issues: vec![ShadowScanIssue {
                code: ShadowScanIssueCode::MissingRuntimeReference,
                count: 1,
            }],
        }
    }
}

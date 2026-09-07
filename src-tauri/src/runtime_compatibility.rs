use std::{collections::BTreeSet, fs, path::Path, time::Duration};

use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    codex_paths::resolve_user_codex_paths,
    command_error::{CommandErrorCode, CommandFailure, CommandRecoverability},
    managed_client::{inspect_managed_client_packages, ManagedClientPackage},
    session_storage::bounded_file::observe_regular_file,
};

const REQUIRED_SESSION_VIEW_COLUMNS: [&str; 2] = ["id", "model_provider"];
const REQUIRED_ADVANCED_STORAGE_COLUMNS: [&str; 3] = ["id", "model_provider", "rollout_path"];
const COMPATIBILITY_INSPECTION_ATTEMPTS: usize = 2;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub enum CompatibilityLevel {
    Supported,
    Warning,
    Unknown,
    Blocked,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CapabilityAvailability {
    Supported,
    Unavailable,
    Blocked,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum StateDatabaseCompatibility {
    Absent,
    Compatible,
    UnsupportedSchema,
    Unavailable,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RuntimeCompatibilityIssueCode {
    StateDatabaseAbsent,
    RuntimePathUnavailable,
    StateDatabaseUnsafe,
    StateDatabaseChanged,
    StateDatabaseUnreadable,
    StateDatabaseIntegrityFailed,
    ThreadsTableMissing,
    ThreadIdColumnMissing,
    ThreadIdColumnIncompatible,
    ModelProviderColumnMissing,
    ModelProviderColumnIncompatible,
    RolloutPathColumnMissing,
    RolloutPathColumnIncompatible,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeCompatibilityIssue {
    pub code: RuntimeCompatibilityIssueCode,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeCompatibilityReport {
    pub status: CompatibilityLevel,
    pub route_config: CapabilityAvailability,
    pub session_view: CapabilityAvailability,
    pub advanced_storage: CapabilityAvailability,
    pub state_database: StateDatabaseCompatibility,
    pub schema_fingerprint: Option<String>,
    pub sqlite_schema_version: Option<i64>,
    pub managed_clients: Vec<ManagedClientPackage>,
    pub issues: Vec<RuntimeCompatibilityIssue>,
}

impl RuntimeCompatibilityReport {
    pub fn session_view_supported(&self) -> bool {
        self.session_view == CapabilityAvailability::Supported
    }

    pub fn advanced_storage_supported(&self) -> bool {
        self.advanced_storage == CapabilityAvailability::Supported
    }
}

#[derive(Debug)]
struct ThreadColumn {
    cid: i64,
    name: String,
    declared_type: String,
    not_null: bool,
    default_value: Option<String>,
    primary_key_order: i64,
    hidden: i64,
}

#[derive(Debug)]
struct DatabaseInspection {
    sqlite_schema_version: i64,
    columns: Vec<ThreadColumn>,
}

#[derive(Debug)]
struct DatabaseInspectionFailure {
    code: RuntimeCompatibilityIssueCode,
    message: &'static str,
}

pub fn inspect_runtime_compatibility(codex_home: &Path) -> RuntimeCompatibilityReport {
    inspect_runtime_compatibility_with_clients(codex_home, inspect_managed_client_packages())
}

fn inspect_runtime_compatibility_with_clients(
    codex_home: &Path,
    managed_clients: Vec<ManagedClientPackage>,
) -> RuntimeCompatibilityReport {
    inspect_runtime_capabilities(codex_home, managed_clients, true)
}

fn inspect_runtime_capabilities(
    codex_home: &Path,
    managed_clients: Vec<ManagedClientPackage>,
    check_integrity: bool,
) -> RuntimeCompatibilityReport {
    let paths = match resolve_user_codex_paths(codex_home) {
        Ok(paths) => paths,
        Err(_) => {
            return blocked_report(
                managed_clients,
                RuntimeCompatibilityIssueCode::RuntimePathUnavailable,
                "无法可靠解析当前 Codex Home 或 SQLite Home；未执行兼容性相关写操作。",
            )
        }
    };

    match fs::symlink_metadata(&paths.state_db) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return RuntimeCompatibilityReport {
                status: CompatibilityLevel::Warning,
                route_config: CapabilityAvailability::Supported,
                session_view: CapabilityAvailability::Unavailable,
                advanced_storage: CapabilityAvailability::Unavailable,
                state_database: StateDatabaseCompatibility::Absent,
                schema_fingerprint: None,
                sqlite_schema_version: None,
                managed_clients,
                issues: vec![issue(
                    RuntimeCompatibilityIssueCode::StateDatabaseAbsent,
                    "当前 Home 尚未生成 state_5.sqlite；本地请求路由仍可配置，高级存储将在数据库生成并通过兼容性检查后启用。",
                )],
            };
        }
        Err(_) => {
            return blocked_report(
                managed_clients,
                RuntimeCompatibilityIssueCode::StateDatabaseUnsafe,
                "无法安全检查 state_5.sqlite；未执行数据库视图或高级存储写操作。",
            )
        }
        Ok(_) => {}
    }

    for attempt in 0..COMPATIBILITY_INSPECTION_ATTEMPTS {
        let before = match observe_regular_file(&paths.state_db, u64::MAX) {
            Ok(observation) => observation,
            Err(_) if attempt + 1 < COMPATIBILITY_INSPECTION_ATTEMPTS => continue,
            Err(_) => {
                return blocked_report(
                    managed_clients,
                    RuntimeCompatibilityIssueCode::StateDatabaseUnsafe,
                    "state_5.sqlite 不是可稳定识别的普通文件，或其身份正在变化；未执行写操作。",
                )
            }
        };
        let inspection = match inspect_database(&paths.state_db, check_integrity) {
            Ok(inspection) => inspection,
            Err(failure) => return blocked_report(managed_clients, failure.code, failure.message),
        };
        let after = match observe_regular_file(&paths.state_db, u64::MAX) {
            Ok(observation) => observation,
            Err(_) if attempt + 1 < COMPATIBILITY_INSPECTION_ATTEMPTS => continue,
            Err(_) => return changed_report(managed_clients),
        };

        // Compatibility is a schema capability check, not a writer-quiescence check.
        // WAL/rollback traffic may change size or timestamps while the desktop is open;
        // accepting the inspection requires the path to resolve to the same regular-file
        // identity before and after SQLite inspected its schema. High-risk mutations still
        // perform their independent writer-closed and state-drift checks.
        if before.same_file_identity(&after) {
            return report_for_schema(managed_clients, inspection);
        }
        if attempt + 1 >= COMPATIBILITY_INSPECTION_ATTEMPTS {
            return changed_report(managed_clients);
        }
    }

    changed_report(managed_clients)
}

fn inspect_database(
    path: &Path,
    check_integrity: bool,
) -> Result<DatabaseInspection, DatabaseInspectionFailure> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| DatabaseInspectionFailure {
        code: RuntimeCompatibilityIssueCode::StateDatabaseUnreadable,
        message: "无法只读打开 state_5.sqlite；未执行数据库视图或高级存储写操作。",
    })?;
    connection
        .busy_timeout(Duration::from_secs(2))
        .map_err(|_| DatabaseInspectionFailure {
            code: RuntimeCompatibilityIssueCode::StateDatabaseUnreadable,
            message: "无法配置 state_5.sqlite 的只读检查超时；未执行写操作。",
        })?;

    // Keep integrity, schema version and columns on the same read snapshot.
    // A concurrent WAL writer must not splice two schema generations into one report.
    let transaction =
        connection
            .unchecked_transaction()
            .map_err(|_| DatabaseInspectionFailure {
                code: RuntimeCompatibilityIssueCode::StateDatabaseUnreadable,
                message: "无法建立 state_5.sqlite 的只读检查快照；未执行写操作。",
            })?;
    if check_integrity {
        let quick_check = transaction
            .query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
            .map_err(|_| DatabaseInspectionFailure {
                code: RuntimeCompatibilityIssueCode::StateDatabaseIntegrityFailed,
                message:
                    "state_5.sqlite 未通过 SQLite quick_check；未执行数据库视图或高级存储写操作。",
            })?;
        if quick_check != "ok" {
            return Err(DatabaseInspectionFailure {
                code: RuntimeCompatibilityIssueCode::StateDatabaseIntegrityFailed,
                message:
                    "state_5.sqlite 未通过 SQLite quick_check；未执行数据库视图或高级存储写操作。",
            });
        }
    }

    let sqlite_schema_version = transaction
        .query_row("PRAGMA schema_version", [], |row| row.get::<_, i64>(0))
        .map_err(|_| DatabaseInspectionFailure {
            code: RuntimeCompatibilityIssueCode::StateDatabaseUnreadable,
            message: "无法读取 state_5.sqlite 的 Schema 版本；未执行写操作。",
        })?;
    let columns = read_thread_columns(&transaction).map_err(|_| DatabaseInspectionFailure {
        code: RuntimeCompatibilityIssueCode::StateDatabaseUnreadable,
        message: "无法读取 threads 表结构；未执行数据库视图或高级存储写操作。",
    })?;

    Ok(DatabaseInspection {
        sqlite_schema_version,
        columns,
    })
}

fn report_for_schema(
    managed_clients: Vec<ManagedClientPackage>,
    inspection: DatabaseInspection,
) -> RuntimeCompatibilityReport {
    if inspection.columns.is_empty() {
        return unsupported_schema_report(
            managed_clients,
            inspection.sqlite_schema_version,
            None,
            RuntimeCompatibilityIssueCode::ThreadsTableMissing,
            "当前 state_5.sqlite 中不存在可识别的 threads 表；仅保留请求配置和只读排查能力。",
        );
    }

    let schema_fingerprint = Some(schema_fingerprint(
        inspection.sqlite_schema_version,
        &inspection.columns,
    ));
    let names = inspection
        .columns
        .iter()
        .map(|column| column.name.to_ascii_lowercase())
        .collect::<BTreeSet<_>>();
    let mut issues = Vec::new();
    let id_compatible = match find_column(&inspection.columns, "id") {
        None => {
            issues.push(issue(
                RuntimeCompatibilityIssueCode::ThreadIdColumnMissing,
                "threads 表缺少 id 列。",
            ));
            false
        }
        Some(column)
            if !is_text_column(column)
                || column.primary_key_order != 1
                || inspection
                    .columns
                    .iter()
                    .filter(|column| column.primary_key_order > 0)
                    .count()
                    != 1 =>
        {
            issues.push(issue(
                RuntimeCompatibilityIssueCode::ThreadIdColumnIncompatible,
                "threads.id 必须是独立的普通 TEXT 主键；复合主键或其他结构未经支持验证。",
            ));
            false
        }
        Some(_) => true,
    };
    let provider_compatible = match find_column(&inspection.columns, "model_provider") {
        None => {
            issues.push(issue(
                RuntimeCompatibilityIssueCode::ModelProviderColumnMissing,
                "threads 表缺少 model_provider 列。",
            ));
            false
        }
        Some(column) if !is_text_column(column) => {
            issues.push(issue(
                RuntimeCompatibilityIssueCode::ModelProviderColumnIncompatible,
                "threads.model_provider 必须是普通 TEXT 列；当前结构未经支持验证。",
            ));
            false
        }
        Some(_) => true,
    };
    let session_view_supported = REQUIRED_SESSION_VIEW_COLUMNS
        .iter()
        .all(|required| names.contains(*required))
        && id_compatible
        && provider_compatible;
    let rollout_compatible = match find_column(&inspection.columns, "rollout_path") {
        None => {
            issues.push(issue(
                RuntimeCompatibilityIssueCode::RolloutPathColumnMissing,
                "threads 表缺少 rollout_path 列；高级存储写操作已停用。",
            ));
            false
        }
        Some(column) if !is_text_column(column) => {
            issues.push(issue(
                RuntimeCompatibilityIssueCode::RolloutPathColumnIncompatible,
                "threads.rollout_path 必须是普通 TEXT 列；高级存储写操作已停用。",
            ));
            false
        }
        Some(_) => true,
    };
    let advanced_storage_supported = REQUIRED_ADVANCED_STORAGE_COLUMNS
        .iter()
        .all(|required| names.contains(*required))
        && session_view_supported
        && rollout_compatible;

    if !session_view_supported {
        return RuntimeCompatibilityReport {
            status: CompatibilityLevel::Unknown,
            route_config: CapabilityAvailability::Supported,
            session_view: CapabilityAvailability::Blocked,
            advanced_storage: CapabilityAvailability::Blocked,
            state_database: StateDatabaseCompatibility::UnsupportedSchema,
            schema_fingerprint,
            sqlite_schema_version: Some(inspection.sqlite_schema_version),
            managed_clients,
            issues,
        };
    }

    RuntimeCompatibilityReport {
        status: if advanced_storage_supported {
            CompatibilityLevel::Supported
        } else {
            CompatibilityLevel::Warning
        },
        route_config: CapabilityAvailability::Supported,
        session_view: CapabilityAvailability::Supported,
        advanced_storage: if advanced_storage_supported {
            CapabilityAvailability::Supported
        } else {
            CapabilityAvailability::Blocked
        },
        state_database: StateDatabaseCompatibility::Compatible,
        schema_fingerprint,
        sqlite_schema_version: Some(inspection.sqlite_schema_version),
        managed_clients,
        issues,
    }
}

pub fn require_session_view_compatible(
    codex_home: &Path,
) -> Result<RuntimeCompatibilityReport, CommandFailure> {
    let report = inspect_runtime_compatibility(codex_home);
    match report.session_view {
        CapabilityAvailability::Supported | CapabilityAvailability::Unavailable => Ok(report),
        CapabilityAvailability::Blocked => Err(compatibility_failure(&report, false)),
    }
}

pub fn require_advanced_storage_compatible(
    codex_home: &Path,
) -> Result<RuntimeCompatibilityReport, CommandFailure> {
    let report = inspect_runtime_compatibility(codex_home);
    if report.advanced_storage_supported() {
        Ok(report)
    } else {
        Err(compatibility_failure(&report, true))
    }
}

/// Revalidate structure after the operation's writer gate, without repeating an
/// entire database integrity scan at each closed-window recheck. The caller already ran
/// the full preflight, and the storage executor retains its own snapshot/hash guards.
pub(crate) fn require_advanced_storage_after_writer_check<F>(
    codex_home: &Path,
    mut ensure_writers_closed: F,
) -> Result<(), String>
where
    F: FnMut() -> Result<(), String>,
{
    ensure_writers_closed()?;
    let report = inspect_runtime_capabilities(codex_home, Vec::new(), false);
    if !report.advanced_storage_supported() {
        return Err(compatibility_failure(&report, true).encoded());
    }
    ensure_writers_closed()
}

fn compatibility_failure(report: &RuntimeCompatibilityReport, advanced: bool) -> CommandFailure {
    let unavailable = if advanced {
        report.advanced_storage == CapabilityAvailability::Unavailable
    } else {
        report.session_view == CapabilityAvailability::Unavailable
    };
    let issue_code = report.issues.first().map(|issue| issue.code);
    let recoverability = match issue_code {
        Some(RuntimeCompatibilityIssueCode::StateDatabaseChanged) => {
            CommandRecoverability::CloseWritersAndRetry
        }
        Some(RuntimeCompatibilityIssueCode::StateDatabaseAbsent) => CommandRecoverability::Retry,
        Some(
            RuntimeCompatibilityIssueCode::RuntimePathUnavailable
            | RuntimeCompatibilityIssueCode::StateDatabaseUnsafe
            | RuntimeCompatibilityIssueCode::StateDatabaseUnreadable
            | RuntimeCompatibilityIssueCode::StateDatabaseIntegrityFailed,
        ) => CommandRecoverability::ManualInvestigation,
        _ => CommandRecoverability::UnsupportedClient,
    };
    CommandFailure::new(
        if unavailable {
            CommandErrorCode::RuntimeCompatibilityUnavailable
        } else {
            CommandErrorCode::RuntimeCompatibilityBlocked
        },
        report
            .issues
            .first()
            .map(|issue| issue.message.as_str())
            .unwrap_or("当前 ChatGPT/Codex 本地存储结构未经支持验证；未执行写操作。"),
        "compatibilityPreflight",
        recoverability,
    )
}

fn read_thread_columns(connection: &Connection) -> rusqlite::Result<Vec<ThreadColumn>> {
    let mut statement = connection.prepare("PRAGMA table_xinfo(threads)")?;
    let rows = statement.query_map([], |row| {
        Ok(ThreadColumn {
            cid: row.get(0)?,
            name: row.get(1)?,
            declared_type: row.get(2)?,
            not_null: row.get::<_, i64>(3)? != 0,
            default_value: row.get(4)?,
            primary_key_order: row.get(5)?,
            hidden: row.get(6)?,
        })
    })?;
    rows.collect()
}

fn find_column<'a>(columns: &'a [ThreadColumn], name: &str) -> Option<&'a ThreadColumn> {
    columns
        .iter()
        .find(|column| column.name.eq_ignore_ascii_case(name))
}

fn is_text_column(column: &ThreadColumn) -> bool {
    column.hidden == 0 && {
        let declared = column.declared_type.trim().to_ascii_uppercase();
        !declared.contains("INT")
            && (declared.contains("CHAR") || declared.contains("CLOB") || declared.contains("TEXT"))
    }
}

fn schema_fingerprint(schema_version: i64, columns: &[ThreadColumn]) -> String {
    let mut hasher = Sha256::new();
    hash_field(&mut hasher, &schema_version.to_le_bytes());
    let mut columns = columns.iter().collect::<Vec<_>>();
    columns.sort_by_key(|column| column.cid);
    for column in columns {
        hash_field(&mut hasher, &column.cid.to_le_bytes());
        hash_field(&mut hasher, column.name.as_bytes());
        hash_field(&mut hasher, column.declared_type.as_bytes());
        hash_field(&mut hasher, &[u8::from(column.not_null)]);
        hash_field(
            &mut hasher,
            column
                .default_value
                .as_deref()
                .unwrap_or_default()
                .as_bytes(),
        );
        hash_field(&mut hasher, &column.primary_key_order.to_le_bytes());
        hash_field(&mut hasher, &column.hidden.to_le_bytes());
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_le_bytes());
    hasher.update(value);
}

fn issue(
    code: RuntimeCompatibilityIssueCode,
    message: impl Into<String>,
) -> RuntimeCompatibilityIssue {
    RuntimeCompatibilityIssue {
        code,
        message: message.into(),
    }
}

fn changed_report(managed_clients: Vec<ManagedClientPackage>) -> RuntimeCompatibilityReport {
    blocked_report(
        managed_clients,
        RuntimeCompatibilityIssueCode::StateDatabaseChanged,
        "兼容性检查期间 state_5.sqlite 持续变化或文件身份发生替换；请关闭全部 ChatGPT/Codex 写入进程后重试。",
    )
}

fn blocked_report(
    managed_clients: Vec<ManagedClientPackage>,
    code: RuntimeCompatibilityIssueCode,
    message: impl Into<String>,
) -> RuntimeCompatibilityReport {
    RuntimeCompatibilityReport {
        status: CompatibilityLevel::Blocked,
        route_config: if code == RuntimeCompatibilityIssueCode::RuntimePathUnavailable {
            CapabilityAvailability::Blocked
        } else {
            CapabilityAvailability::Supported
        },
        session_view: CapabilityAvailability::Blocked,
        advanced_storage: CapabilityAvailability::Blocked,
        state_database: StateDatabaseCompatibility::Unavailable,
        schema_fingerprint: None,
        sqlite_schema_version: None,
        managed_clients,
        issues: vec![issue(code, message)],
    }
}

fn unsupported_schema_report(
    managed_clients: Vec<ManagedClientPackage>,
    sqlite_schema_version: i64,
    schema_fingerprint: Option<String>,
    code: RuntimeCompatibilityIssueCode,
    message: impl Into<String>,
) -> RuntimeCompatibilityReport {
    RuntimeCompatibilityReport {
        status: CompatibilityLevel::Unknown,
        route_config: CapabilityAvailability::Supported,
        session_view: CapabilityAvailability::Blocked,
        advanced_storage: CapabilityAvailability::Blocked,
        state_database: StateDatabaseCompatibility::UnsupportedSchema,
        schema_fingerprint,
        sqlite_schema_version: Some(sqlite_schema_version),
        managed_clients,
        issues: vec![issue(code, message)],
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rusqlite::Connection;
    use tempfile::tempdir;

    use super::{
        compatibility_failure, inspect_runtime_compatibility,
        inspect_runtime_compatibility_with_clients, require_advanced_storage_compatible,
        require_session_view_compatible, CapabilityAvailability, CompatibilityLevel,
        RuntimeCompatibilityIssueCode, StateDatabaseCompatibility,
    };
    use crate::{
        command_error::{CommandErrorCode, CommandRecoverability},
        managed_client::ManagedClientPackage,
    };

    #[test]
    fn fresh_home_allows_route_configuration_without_inventing_a_database() {
        let home = tempdir().unwrap();

        let report = inspect_runtime_compatibility(home.path());

        assert_eq!(report.status, CompatibilityLevel::Warning);
        assert_eq!(report.route_config, CapabilityAvailability::Supported);
        assert_eq!(report.session_view, CapabilityAvailability::Unavailable);
        assert_eq!(report.advanced_storage, CapabilityAvailability::Unavailable);
        assert_eq!(report.state_database, StateDatabaseCompatibility::Absent);
        assert!(require_session_view_compatible(home.path()).is_ok());
        let failure = require_advanced_storage_compatible(home.path()).unwrap_err();
        assert_eq!(
            failure.code,
            CommandErrorCode::RuntimeCompatibilityUnavailable
        );
        assert_eq!(failure.recoverability, CommandRecoverability::Retry);
        assert!(!home.path().join("state_5.sqlite").exists());
    }

    #[test]
    fn installed_client_versions_are_reported_without_becoming_a_schema_whitelist() {
        let home = tempdir().unwrap();
        let clients = vec![ManagedClientPackage {
            aumid: "OpenAI.Codex_test!App".to_string(),
            package_name: Some("OpenAI.Codex".to_string()),
            package_family_name: Some("OpenAI.Codex_test".to_string()),
            version: Some("1.2.3.4".to_string()),
        }];

        let report = inspect_runtime_compatibility_with_clients(home.path(), clients.clone());

        assert_eq!(report.managed_clients, clients);
        assert_eq!(report.route_config, CapabilityAvailability::Supported);
    }

    #[test]
    fn supported_schema_enables_session_view_and_advanced_storage() {
        let home = tempdir().unwrap();
        Connection::open(home.path().join("state_5.sqlite"))
            .unwrap()
            .execute_batch(
                "CREATE TABLE threads (
                    id TEXT PRIMARY KEY,
                    rollout_path TEXT NOT NULL,
                    model_provider TEXT NOT NULL,
                    archived INTEGER DEFAULT 0
                );",
            )
            .unwrap();

        let report = inspect_runtime_compatibility(home.path());

        assert_eq!(report.status, CompatibilityLevel::Supported);
        assert_eq!(report.session_view, CapabilityAvailability::Supported);
        assert_eq!(report.advanced_storage, CapabilityAvailability::Supported);
        assert_eq!(
            report.state_database,
            StateDatabaseCompatibility::Compatible
        );
        assert!(report
            .schema_fingerprint
            .as_deref()
            .is_some_and(|value| value.len() == 64));
        assert!(require_session_view_compatible(home.path()).is_ok());
        assert!(require_advanced_storage_compatible(home.path()).is_ok());
    }

    #[test]
    fn session_view_schema_can_be_supported_while_advanced_storage_is_blocked() {
        let home = tempdir().unwrap();
        Connection::open(home.path().join("state_5.sqlite"))
            .unwrap()
            .execute_batch(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, model_provider TEXT NOT NULL);",
            )
            .unwrap();

        let report = inspect_runtime_compatibility(home.path());

        assert_eq!(report.status, CompatibilityLevel::Warning);
        assert_eq!(report.session_view, CapabilityAvailability::Supported);
        assert_eq!(report.advanced_storage, CapabilityAvailability::Blocked);
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.code == RuntimeCompatibilityIssueCode::RolloutPathColumnMissing));
        assert!(require_session_view_compatible(home.path()).is_ok());
        assert!(require_advanced_storage_compatible(home.path()).is_err());
    }

    #[test]
    fn unknown_threads_schema_fails_closed_before_storage_writes() {
        let home = tempdir().unwrap();
        Connection::open(home.path().join("state_5.sqlite"))
            .unwrap()
            .execute_batch("CREATE TABLE unrelated (id INTEGER PRIMARY KEY);")
            .unwrap();

        let report = inspect_runtime_compatibility(home.path());

        assert_eq!(report.status, CompatibilityLevel::Unknown);
        assert_eq!(report.session_view, CapabilityAvailability::Blocked);
        assert_eq!(report.advanced_storage, CapabilityAvailability::Blocked);
        assert_eq!(
            require_session_view_compatible(home.path())
                .unwrap_err()
                .code,
            CommandErrorCode::RuntimeCompatibilityBlocked
        );
        assert_eq!(
            compatibility_failure(&report, true).recoverability,
            CommandRecoverability::UnsupportedClient
        );
    }

    #[test]
    fn corrupt_or_non_regular_state_database_is_blocked() {
        let corrupt = tempdir().unwrap();
        fs::write(corrupt.path().join("state_5.sqlite"), b"not sqlite").unwrap();
        let corrupt_report = inspect_runtime_compatibility(corrupt.path());
        assert_eq!(corrupt_report.status, CompatibilityLevel::Blocked);
        assert_eq!(
            compatibility_failure(&corrupt_report, true).recoverability,
            CommandRecoverability::ManualInvestigation
        );

        let directory = tempdir().unwrap();
        fs::create_dir(directory.path().join("state_5.sqlite")).unwrap();
        let report = inspect_runtime_compatibility(directory.path());
        assert_eq!(report.status, CompatibilityLevel::Blocked);
        assert_eq!(
            report.issues[0].code,
            RuntimeCompatibilityIssueCode::StateDatabaseUnsafe
        );
    }

    #[test]
    fn schema_fingerprint_is_stable_for_the_same_schema() {
        let left = tempdir().unwrap();
        let right = tempdir().unwrap();
        for home in [left.path(), right.path()] {
            Connection::open(home.join("state_5.sqlite"))
                .unwrap()
                .execute_batch(
                    "CREATE TABLE threads (
                        id TEXT PRIMARY KEY,
                        rollout_path TEXT,
                        model_provider TEXT,
                        archived INTEGER DEFAULT 0
                    );",
                )
                .unwrap();
        }

        assert_eq!(
            inspect_runtime_compatibility(left.path()).schema_fingerprint,
            inspect_runtime_compatibility(right.path()).schema_fingerprint
        );
    }
    #[test]
    fn incompatible_key_column_types_fail_closed() {
        let home = tempdir().unwrap();
        Connection::open(home.path().join("state_5.sqlite"))
            .unwrap()
            .execute_batch(
                "CREATE TABLE threads (
                    id INTEGER PRIMARY KEY,
                    rollout_path TEXT,
                    model_provider TEXT
                );",
            )
            .unwrap();

        let report = inspect_runtime_compatibility(home.path());

        assert_eq!(report.status, CompatibilityLevel::Unknown);
        assert_eq!(report.session_view, CapabilityAvailability::Blocked);
        assert!(report.issues.iter().any(|issue| {
            issue.code == RuntimeCompatibilityIssueCode::ThreadIdColumnIncompatible
        }));
    }

    #[test]
    fn composite_primary_keys_cannot_authorize_thread_id_only_mutations() {
        let home = tempdir().unwrap();
        let db = home.path().join("state_5.sqlite");
        Connection::open(&db).unwrap().execute_batch(
            "CREATE TABLE threads (id TEXT, branch TEXT, model_provider TEXT, rollout_path TEXT,
             PRIMARY KEY(id, branch));
             INSERT INTO threads VALUES ('same','a','openai','one'), ('same','b','openai','two');"
        ).unwrap();
        let before = fs::read(&db).unwrap();
        let report = inspect_runtime_compatibility(home.path());
        assert_eq!(report.session_view, CapabilityAvailability::Blocked);
        assert_eq!(report.advanced_storage, CapabilityAvailability::Blocked);
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.code == RuntimeCompatibilityIssueCode::ThreadIdColumnIncompatible));
        assert_eq!(before, fs::read(&db).unwrap());
    }

    #[test]
    fn sqlite_integer_affinity_takes_precedence_over_text_substrings() {
        for declaration in ["TEXTINT", "CHARINT", "CLOBINT"] {
            let home = tempdir().unwrap();
            Connection::open(home.path().join("state_5.sqlite")).unwrap().execute_batch(
                &format!("CREATE TABLE threads (id {declaration} PRIMARY KEY, model_provider TEXT, rollout_path TEXT);")
            ).unwrap();
            let report = inspect_runtime_compatibility(home.path());
            assert_eq!(
                report.session_view,
                CapabilityAvailability::Blocked,
                "{declaration}"
            );
        }
    }

    #[test]
    fn generated_provider_column_does_not_authorize_database_updates() {
        let home = tempdir().unwrap();
        Connection::open(home.path().join("state_5.sqlite")).unwrap().execute_batch(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, model_provider TEXT AS ('openai') VIRTUAL, rollout_path TEXT);"
        ).unwrap();
        let report = inspect_runtime_compatibility(home.path());
        assert_eq!(report.session_view, CapabilityAvailability::Blocked);
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.code
                == RuntimeCompatibilityIssueCode::ModelProviderColumnIncompatible));
    }

    #[test]
    fn schema_change_during_writer_shutdown_invalidates_the_earlier_preflight() {
        let home = tempdir().unwrap();
        let db = home.path().join("state_5.sqlite");
        Connection::open(&db).unwrap().execute_batch(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, model_provider TEXT, rollout_path TEXT);"
        ).unwrap();
        require_advanced_storage_compatible(home.path()).unwrap();
        let mut checks = 0;
        let result = super::require_advanced_storage_after_writer_check(home.path(), || {
            checks += 1;
            Connection::open(&db)
                .unwrap()
                .execute_batch("DROP TABLE threads; CREATE TABLE threads (id INTEGER PRIMARY KEY);")
                .unwrap();
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(checks, 1);
        let failure = crate::command_error::decode_command_failure(&result.unwrap_err()).unwrap();
        assert_eq!(failure.code, CommandErrorCode::RuntimeCompatibilityBlocked);
    }

    #[test]
    fn writer_appearing_during_schema_inspection_blocks_the_final_write_guard() {
        let home = tempdir().unwrap();
        Connection::open(home.path().join("state_5.sqlite")).unwrap().execute_batch(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, model_provider TEXT, rollout_path TEXT);"
        ).unwrap();
        let mut checks = 0;
        let result = super::require_advanced_storage_after_writer_check(home.path(), || {
            checks += 1;
            if checks == 1 {
                Ok(())
            } else {
                Err("writer appeared".to_string())
            }
        });
        assert_eq!(checks, 2);
        assert_eq!(result.unwrap_err(), "writer appeared");
    }

    #[test]
    fn schema_recheck_respects_an_existing_executor_write_barrier() {
        for wal in [false, true] {
            let home = tempdir().unwrap();
            let path = home.path().join("state_5.sqlite");
            let conn = Connection::open(&path).unwrap();
            if wal {
                conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
            }
            conn.execute_batch(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, model_provider TEXT, rollout_path TEXT);"
            ).unwrap();
            drop(conn);
            let _barrier =
                crate::session_storage::write_barrier::WriteExclusionGuard::acquire(&path).unwrap();
            // A protected file must not be reopened through a permissive fallback.
            // Commands recheck before executor entry, then use its frozen proofs.
            assert!(
                super::require_advanced_storage_after_writer_check(home.path(), || Ok(())).is_err()
            );
        }
    }
}

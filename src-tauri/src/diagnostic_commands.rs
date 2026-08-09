use std::{
    collections::{BTreeSet, VecDeque},
    env, fs,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    codex_paths::resolve_user_codex_paths,
    diagnostics::{
        export::{
            prepare_diagnostic_archive, publish_prepared_to_directory_at, DiagnosticExportReceipt,
            ExportInputs, ExportMetadata, ExportSelection, ExportSelectionMode,
            PreparedDiagnosticArchive, RedactionContext,
        },
        global_runtime, new_diagnostic_id,
        platform::{downloads_dir, local_export_time, open_directory, open_export_location_in},
        DiagnosticEvent, DiagnosticEventInput, DiagnosticEventKind, DiagnosticLevel,
        DEFAULT_MAX_TOTAL_BYTES, DEFAULT_RETENTION, DIAGNOSTIC_SCHEMA_VERSION,
    },
    operation_log::{OperationLog, OperationRecord},
    process_control::list_codex_process_inventory,
    runtime_store::RuntimeStore,
};

const OPERATION_CONTEXT_MS: u128 = 10 * 60 * 1_000;
const MAX_REGISTERED_EXPORTS: usize = 16;
const MAX_PREPARED_EXPORTS: usize = 4;
const PREPARED_EXPORT_TTL: Duration = Duration::from_secs(10 * 60);
const REDACTION_POLICY_VERSION: u32 = 1;

static EXPORTED_ARCHIVES: OnceLock<Mutex<VecDeque<(String, PathBuf)>>> = OnceLock::new();
static PREPARED_EXPORTS: OnceLock<Mutex<PreparedExportRegistry>> = OnceLock::new();

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticStatus {
    pub available: bool,
    pub event_count: usize,
    pub total_bytes: u64,
    pub retention_days: u64,
    pub max_bytes: u64,
    pub oldest_event_at_ms: Option<u128>,
    pub newest_event_at_ms: Option<u128>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FrontendDiagnosticInput {
    level: String,
    component: String,
    event_kind: String,
    error_code: String,
    safe_message: String,
}

#[derive(Debug)]
struct SelectedEvents {
    events: Vec<DiagnosticEvent>,
    selection: ExportSelection,
}

#[derive(Debug, Clone, Copy)]
enum ExportDestination {
    Downloads,
    DiagnosticDirectory,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DiagnosticExportFailureKind {
    Preparation,
    Destination,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticExportFailure {
    pub kind: DiagnosticExportFailureKind,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_id: Option<String>,
}

#[derive(Debug)]
struct PreparedExportContext {
    retry_id: String,
    created_at: Instant,
    local_timestamp: String,
    archive: PreparedDiagnosticArchive,
}

#[derive(Debug, Default)]
struct PreparedExportRegistry {
    entries: VecDeque<PreparedExportContext>,
}

#[tauri::command]
pub async fn get_diagnostic_status() -> Result<DiagnosticStatus, String> {
    tauri::async_runtime::spawn_blocking(diagnostic_status)
        .await
        .map_err(|_| "diagnostic status worker failed".to_string())?
}

#[tauri::command]
pub async fn export_diagnostics(
    operation_id: Option<String>,
    retry_id: Option<String>,
) -> Result<DiagnosticExportReceipt, DiagnosticExportFailure> {
    if operation_id.is_some() && retry_id.is_some() {
        return Err(preparation_failure("invalid diagnostic export request"));
    }
    export_diagnostics_to(operation_id, retry_id, ExportDestination::Downloads).await
}

#[tauri::command]
pub async fn export_diagnostics_to_diagnostic_directory(
    retry_id: String,
) -> Result<DiagnosticExportReceipt, DiagnosticExportFailure> {
    export_diagnostics_to(None, Some(retry_id), ExportDestination::DiagnosticDirectory).await
}

async fn export_diagnostics_to(
    operation_id: Option<String>,
    retry_id: Option<String>,
    destination: ExportDestination,
) -> Result<DiagnosticExportReceipt, DiagnosticExportFailure> {
    let receipt = tauri::async_runtime::spawn_blocking(move || match retry_id {
        Some(retry_id) => publish_registered_export(&retry_id, destination),
        None => prepare_and_publish_export(operation_id, destination),
    })
    .await
    .map_err(|_| preparation_failure("diagnostic export worker failed"))??;
    register_export(&receipt);
    Ok(receipt)
}

#[tauri::command]
pub async fn open_diagnostic_export(export_id: String) -> Result<(), String> {
    let path = registered_export(&export_id)?;
    let directory = path
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "the diagnostic archive is unavailable".to_string())?;
    tauri::async_runtime::spawn_blocking(move || open_export_location_in(&directory, &path))
        .await
        .map_err(|_| "diagnostic archive opener failed".to_string())?
}

#[tauri::command]
pub async fn open_diagnostic_log_directory() -> Result<(), String> {
    let root = diagnostic_root()?;
    tauri::async_runtime::spawn_blocking(move || {
        fs::create_dir_all(&root)
            .map_err(|_| "the diagnostic directory is unavailable".to_string())?;
        open_directory(&root)
    })
    .await
    .map_err(|_| "diagnostic directory opener failed".to_string())?
}

#[tauri::command]
pub async fn clear_diagnostic_logs() -> Result<(), String> {
    let store = global_runtime()
        .ok_or_else(|| "diagnostic logging is unavailable".to_string())?
        .store()
        .clone();
    tauri::async_runtime::spawn_blocking(move || store.clear())
        .await
        .map_err(|_| "diagnostic cleanup worker failed".to_string())?
}

#[tauri::command]
pub fn record_frontend_diagnostic(input: FrontendDiagnosticInput) -> Result<(), String> {
    validate_frontend_input(&input)?;
    let runtime =
        global_runtime().ok_or_else(|| "diagnostic logging is unavailable".to_string())?;
    let _ = runtime.recorder().record(
        DiagnosticEventInput::new(
            DiagnosticLevel::Error,
            "frontend",
            DiagnosticEventKind::BackgroundFailure,
        )
        .with_action(input.event_kind)
        .with_error(input.error_code, input.safe_message),
    );
    Ok(())
}

fn diagnostic_status() -> Result<DiagnosticStatus, String> {
    let Some(runtime) = global_runtime() else {
        return Ok(DiagnosticStatus {
            available: false,
            event_count: 0,
            total_bytes: 0,
            retention_days: DEFAULT_RETENTION.as_secs() / 86_400,
            max_bytes: DEFAULT_MAX_TOTAL_BYTES,
            oldest_event_at_ms: None,
            newest_event_at_ms: None,
            warnings: vec!["diagnosticRuntimeUnavailable".to_string()],
        });
    };
    let status = runtime.store().status()?;
    let events = runtime.store().read_events()?;
    let (oldest_event_at_ms, newest_event_at_ms) = event_timestamp_bounds(&events);
    Ok(DiagnosticStatus {
        available: true,
        event_count: events.len(),
        total_bytes: status.total_bytes,
        retention_days: DEFAULT_RETENTION.as_secs() / 86_400,
        max_bytes: DEFAULT_MAX_TOTAL_BYTES,
        oldest_event_at_ms,
        newest_event_at_ms,
        warnings: Vec::new(),
    })
}

fn prepare_and_publish_export(
    operation_id: Option<String>,
    destination: ExportDestination,
) -> Result<DiagnosticExportReceipt, DiagnosticExportFailure> {
    let context = prepare_export_context(operation_id).map_err(preparation_failure)?;
    publish_export_context(context, destination)
}

fn prepare_export_context(operation_id: Option<String>) -> Result<PreparedExportContext, String> {
    let runtime =
        global_runtime().ok_or_else(|| "diagnostic logging is unavailable".to_string())?;
    let events = runtime.store().read_events()?;
    let now = timestamp_millis();
    let selected = select_events(events, operation_id.as_deref(), now)?;
    let diagnostics_jsonl = encode_diagnostics(&selected.events)?;

    let appdata = appdata_root()?;
    let operation_log = OperationLog::from_appdata(&appdata);
    let mut unavailable = Vec::new();
    let operations = match operation_log.list_all_strict() {
        Ok(records) => select_operations(records, &selected.selection),
        Err(_) => {
            unavailable.push("operations.jsonl".to_string());
            Vec::new()
        }
    };
    let operations_jsonl = encode_operations(&operations)?;
    let store_status = runtime.store().status()?;
    let health_json =
        build_health_json(&appdata, &store_status, operations.len(), &mut unavailable)?;
    let local_time = local_export_time()?;

    let redaction = RedactionContext {
        user_profile: env_path("USERPROFILE"),
        appdata: Some(appdata.clone()),
        codex_home: managed_codex_home_for_redaction(),
        forbidden_literals: identity_literals(),
    };
    let metadata = ExportMetadata {
        schema_version: DIAGNOSTIC_SCHEMA_VERSION,
        application_version: env!("CARGO_PKG_VERSION").to_string(),
        build_version: option_env!("GITHUB_SHA").map(str::to_string),
        exported_at: local_time.rfc3339,
        timezone_offset_minutes: local_time.timezone_offset_minutes,
        redaction_policy_version: REDACTION_POLICY_VERSION,
        selection: selected.selection,
        event_count: selected.events.len(),
        unavailable,
        warnings: Vec::new(),
    };
    let inputs = ExportInputs {
        metadata,
        redaction,
        diagnostics_jsonl,
        operations_jsonl,
        health_json,
    };
    let archive = prepare_diagnostic_archive(inputs)?;
    Ok(PreparedExportContext {
        retry_id: new_diagnostic_id("diagnostic-export-context"),
        created_at: Instant::now(),
        local_timestamp: local_time.filename_timestamp,
        archive,
    })
}

fn publish_registered_export(
    retry_id: &str,
    destination: ExportDestination,
) -> Result<DiagnosticExportReceipt, DiagnosticExportFailure> {
    let context = take_prepared_export(retry_id).ok_or_else(|| {
        preparation_failure("diagnostic export context expired; start a new export")
    })?;
    publish_export_context(context, destination)
}

fn publish_export_context(
    context: PreparedExportContext,
    destination: ExportDestination,
) -> Result<DiagnosticExportReceipt, DiagnosticExportFailure> {
    match resolve_export_destination(destination).and_then(|directory| {
        publish_prepared_to_directory_at(&context.archive, &directory, &context.local_timestamp)
    }) {
        Ok(receipt) => Ok(receipt),
        Err(message) => {
            let retry_id = context.retry_id.clone();
            register_prepared_export(context);
            Err(destination_failure(message, retry_id))
        }
    }
}

fn resolve_export_destination(destination: ExportDestination) -> Result<PathBuf, String> {
    let directory = match destination {
        ExportDestination::Downloads => downloads_dir()?,
        ExportDestination::DiagnosticDirectory => {
            let appdata = appdata_root()?;
            let directory = appdata.join("codex-switch/diagnostic-exports");
            fs::create_dir_all(&directory)
                .map_err(|_| "the diagnostic fallback directory is unavailable".to_string())?;
            directory
        }
    };
    Ok(directory)
}

fn select_events(
    events: Vec<DiagnosticEvent>,
    operation_id: Option<&str>,
    now: u128,
) -> Result<SelectedEvents, String> {
    let operation_id = operation_id
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(operation_id) = operation_id {
        let attempt_ids = events
            .iter()
            .filter(|event| event.operation_id.as_deref() == Some(operation_id))
            .filter_map(|event| event.attempt_id.clone())
            .collect::<BTreeSet<_>>();
        let related = events
            .iter()
            .filter(|event| {
                event.operation_id.as_deref() == Some(operation_id)
                    || event.attempt_id.as_deref() == Some(operation_id)
                    || event
                        .attempt_id
                        .as_ref()
                        .is_some_and(|attempt_id| attempt_ids.contains(attempt_id))
            })
            .collect::<Vec<_>>();
        let Some(first) = related.iter().map(|event| event.timestamp).min() else {
            return Err("no diagnostic events match this operation".to_string());
        };
        let last = related
            .iter()
            .map(|event| event.timestamp)
            .max()
            .unwrap_or(first);
        let from = first.saturating_sub(OPERATION_CONTEXT_MS);
        let through = last;
        let events = events
            .into_iter()
            .filter(|event| event.timestamp >= from && event.timestamp <= through)
            .collect();
        return Ok(SelectedEvents {
            events,
            selection: ExportSelection {
                mode: ExportSelectionMode::Operation,
                operation_id: Some(operation_id.to_string()),
                from_timestamp_ms: from,
                through_timestamp_ms: through,
            },
        });
    }

    let (oldest, newest) = event_timestamp_bounds(&events);
    let from = oldest.unwrap_or(now);
    let through = newest.map(|timestamp| now.max(timestamp)).unwrap_or(now);
    Ok(SelectedEvents {
        events,
        selection: ExportSelection {
            mode: ExportSelectionMode::RetainedWindow,
            operation_id: None,
            from_timestamp_ms: from,
            through_timestamp_ms: through,
        },
    })
}

fn event_timestamp_bounds(events: &[DiagnosticEvent]) -> (Option<u128>, Option<u128>) {
    events
        .iter()
        .map(|event| event.timestamp)
        .fold((None, None), |(oldest, newest), timestamp| {
            (
                Some(oldest.map_or(timestamp, |value: u128| value.min(timestamp))),
                Some(newest.map_or(timestamp, |value: u128| value.max(timestamp))),
            )
        })
}

fn select_operations(
    records: Vec<OperationRecord>,
    selection: &ExportSelection,
) -> Vec<OperationRecord> {
    records
        .into_iter()
        .filter(|record| {
            let record_from = record.started_at_ms.min(record.completed_at_ms);
            let record_through = record.started_at_ms.max(record.completed_at_ms);
            selection.operation_id.as_deref() == Some(record.operation_id.as_str())
                || (record_through >= selection.from_timestamp_ms
                    && record_from <= selection.through_timestamp_ms)
        })
        .collect()
}

fn encode_diagnostics(events: &[DiagnosticEvent]) -> Result<Vec<u8>, String> {
    encode_jsonl(events.iter())
}

fn encode_operations(records: &[OperationRecord]) -> Result<Vec<u8>, String> {
    let safe = records.iter().map(|record| {
        json!({
            "operationId": record.operation_id,
            "action": record.action,
            "status": record.status,
            "phase": record.phase,
            "startedAtMs": record.started_at_ms,
            "completedAtMs": record.completed_at_ms,
            "counts": record.counts,
        })
    });
    encode_jsonl(safe)
}

fn encode_jsonl<T, I>(items: I) -> Result<Vec<u8>, String>
where
    T: Serialize,
    I: IntoIterator<Item = T>,
{
    let mut output = Vec::new();
    for item in items {
        serde_json::to_writer(&mut output, &item)
            .map_err(|_| "failed to serialize diagnostic export data".to_string())?;
        output.push(b'\n');
    }
    Ok(output)
}

fn build_health_json(
    appdata: &Path,
    store_status: &crate::diagnostics::DiagnosticStoreStatus,
    operation_count: usize,
    unavailable: &mut Vec<String>,
) -> Result<Vec<u8>, String> {
    let codex_home = managed_codex_home_for_redaction();
    let windows_version = match windows_version() {
        Some(version) => Value::String(version),
        None => {
            unavailable.push("windowsVersion".to_string());
            Value::Null
        }
    };
    let process_health = match list_codex_process_inventory() {
        Ok((managed, standalone)) => json!({
            "available": true,
            "managedChatgptCount": managed.len(),
            "standaloneCodexCount": standalone.len(),
        }),
        Err(_) => {
            unavailable.push("managedProcessInventory".to_string());
            json!({ "available": false })
        }
    };
    let route_health = codex_home
        .as_deref()
        .and_then(|home| {
            RuntimeStore::from_default_root()
                .and_then(|store| store.detect_active_runtime(home))
                .ok()
        })
        .map(|status| {
            json!({
                "available": true,
                "activeRuntimeId": status.active_runtime_id,
                "confidence": status.confidence,
                "authMode": status.auth_mode,
                "modelProvider": status.model_provider,
            })
        })
        .unwrap_or_else(|| {
            unavailable.push("runtimeRouteHealth".to_string());
            json!({ "available": false })
        });
    let sqlite_health = sqlite_health(codex_home.as_deref(), unavailable);
    let health = json!({
        "schemaVersion": 1,
        "application": {
            "name": "ChatGPT Switch",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "platform": {
            "os": std::env::consts::OS,
            "architecture": std::env::consts::ARCH,
            "windowsVersion": windows_version,
        },
        "storage": {
            "appdataAvailable": appdata.is_dir(),
            "codexHomeAvailable": codex_home.as_deref().is_some_and(Path::is_dir),
            "diagnosticSegmentCount": store_status.segment_count,
            "diagnosticBytes": store_status.total_bytes,
            "operationRecordCount": operation_count,
        },
        "runtimeRoute": route_health,
        "processes": process_health,
        "sqlite": sqlite_health,
    });
    serde_json::to_vec_pretty(&health)
        .map_err(|_| "failed to serialize diagnostic health data".to_string())
}

fn sqlite_health(codex_home: Option<&Path>, unavailable: &mut Vec<String>) -> Value {
    let Some(codex_home) = codex_home else {
        unavailable.push("sqliteHealth".to_string());
        return json!({ "available": false });
    };
    let paths = match resolve_user_codex_paths(codex_home) {
        Ok(paths) => paths,
        Err(_) => {
            unavailable.push("sqliteHealth".to_string());
            return json!({ "available": false });
        }
    };
    let databases = [
        ("state", paths.state_db),
        ("logs", paths.logs_db),
        ("goals", paths.goals_db),
        ("memories", paths.memories_db),
    ];
    let mut result = serde_json::Map::new();
    for (name, path) in databases {
        let value = match fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => match sqlite_schema_version(&path) {
                Ok(schema_version) => json!({
                    "present": true,
                    "readable": true,
                    "bytes": metadata.len(),
                    "schemaVersion": schema_version,
                }),
                Err(_) => {
                    unavailable.push(format!("sqlite.{name}"));
                    json!({
                        "present": true,
                        "readable": false,
                        "bytes": metadata.len(),
                    })
                }
            },
            Ok(_) => {
                unavailable.push(format!("sqlite.{name}"));
                json!({ "present": true, "readable": false })
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                json!({ "present": false, "readable": false })
            }
            Err(_) => {
                unavailable.push(format!("sqlite.{name}"));
                json!({ "present": null, "readable": false })
            }
        };
        result.insert(name.to_string(), value);
    }
    json!({ "available": true, "databases": result })
}

fn sqlite_schema_version(path: &Path) -> Result<i64, String> {
    let connection = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| "SQLite database is unavailable".to_string())?;
    connection
        .query_row("PRAGMA schema_version", [], |row| row.get(0))
        .map_err(|_| "SQLite schema is unavailable".to_string())
}

#[cfg(windows)]
fn windows_version() -> Option<String> {
    use std::mem::size_of;
    use windows_sys::Win32::System::SystemInformation::{GetVersionExW, OSVERSIONINFOW};

    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32,
        ..OSVERSIONINFOW::default()
    };
    (unsafe { GetVersionExW(&mut info) } != 0).then(|| {
        format!(
            "{}.{}.{}",
            info.dwMajorVersion, info.dwMinorVersion, info.dwBuildNumber
        )
    })
}

#[cfg(not(windows))]
fn windows_version() -> Option<String> {
    None
}

fn validate_frontend_input(input: &FrontendDiagnosticInput) -> Result<(), String> {
    if input.level != "error" || input.component != "frontend" {
        return Err("invalid frontend diagnostic classification".to_string());
    }
    let expected_code = match input.event_kind.as_str() {
        "unhandledError" => "frontend.unhandled_error",
        "unhandledRejection" => "frontend.unhandled_rejection",
        _ => return Err("invalid frontend diagnostic event".to_string()),
    };
    if input.error_code != expected_code || input.safe_message.len() > 512 {
        return Err("invalid frontend diagnostic payload".to_string());
    }
    Ok(())
}

fn preparation_failure(message: impl Into<String>) -> DiagnosticExportFailure {
    DiagnosticExportFailure {
        kind: DiagnosticExportFailureKind::Preparation,
        message: message.into(),
        retry_id: None,
    }
}

fn destination_failure(message: impl Into<String>, retry_id: String) -> DiagnosticExportFailure {
    DiagnosticExportFailure {
        kind: DiagnosticExportFailureKind::Destination,
        message: message.into(),
        retry_id: Some(retry_id),
    }
}

impl PreparedExportRegistry {
    fn insert(&mut self, context: PreparedExportContext, now: Instant) {
        self.purge_expired(now);
        if now
            .checked_duration_since(context.created_at)
            .is_none_or(|age| age > PREPARED_EXPORT_TTL)
        {
            return;
        }
        self.entries
            .retain(|entry| entry.retry_id != context.retry_id);
        self.entries.push_back(context);
        while self.entries.len() > MAX_PREPARED_EXPORTS {
            self.entries.pop_front();
        }
    }

    fn take(&mut self, retry_id: &str, now: Instant) -> Option<PreparedExportContext> {
        self.purge_expired(now);
        let index = self
            .entries
            .iter()
            .position(|entry| entry.retry_id == retry_id)?;
        self.entries.remove(index)
    }

    fn purge_expired(&mut self, now: Instant) {
        self.entries.retain(|entry| {
            now.checked_duration_since(entry.created_at)
                .is_some_and(|age| age <= PREPARED_EXPORT_TTL)
        });
    }
}

fn register_prepared_export(mut context: PreparedExportContext) {
    let now = Instant::now();
    context.created_at = now;
    let registry = PREPARED_EXPORTS.get_or_init(|| Mutex::new(PreparedExportRegistry::default()));
    registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(context, now);
}

fn take_prepared_export(retry_id: &str) -> Option<PreparedExportContext> {
    if !valid_retry_id(retry_id) {
        return None;
    }
    PREPARED_EXPORTS.get().and_then(|registry| {
        registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take(retry_id, Instant::now())
    })
}

fn valid_retry_id(retry_id: &str) -> bool {
    const PREFIX: &str = "diagnostic-export-context-";
    retry_id.strip_prefix(PREFIX).is_some_and(|suffix| {
        suffix.len() == 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

fn register_export(receipt: &DiagnosticExportReceipt) {
    let registry = EXPORTED_ARCHIVES.get_or_init(|| Mutex::new(VecDeque::new()));
    let mut registry = registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    registry.retain(|(export_id, _)| export_id != &receipt.export_id);
    registry.push_back((receipt.export_id.clone(), receipt.path.clone()));
    while registry.len() > MAX_REGISTERED_EXPORTS {
        registry.pop_front();
    }
}

fn registered_export(export_id: &str) -> Result<PathBuf, String> {
    if export_id.trim().is_empty() || export_id.len() > 160 {
        return Err("invalid diagnostic export identifier".to_string());
    }
    EXPORTED_ARCHIVES
        .get()
        .and_then(|registry| {
            registry
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .iter()
                .find(|(registered, _)| registered == export_id)
                .map(|(_, path)| path.clone())
        })
        .ok_or_else(|| "the diagnostic archive is no longer available".to_string())
}

fn diagnostic_root() -> Result<PathBuf, String> {
    global_runtime()
        .map(|runtime| runtime.store().root().to_path_buf())
        .ok_or_else(|| "diagnostic logging is unavailable".to_string())
}

fn appdata_root() -> Result<PathBuf, String> {
    env_path("APPDATA")
        .filter(|path| path.is_absolute())
        .ok_or_else(|| "diagnostic storage is unavailable".to_string())
}

fn env_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.to_string_lossy().trim().is_empty())
        .map(PathBuf::from)
}

fn managed_codex_home_for_redaction() -> Option<PathBuf> {
    env_path("CODEX_HOME").or_else(|| env_path("USERPROFILE").map(|root| root.join(".codex")))
}

fn identity_literals() -> Vec<String> {
    ["USERNAME", "COMPUTERNAME"]
        .into_iter()
        .filter_map(|name| env::var(name).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect()
}

fn timestamp_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        path::PathBuf,
        time::{Duration, Instant},
    };

    use crate::{
        diagnostics::{
            export::{
                prepare_diagnostic_archive, ExportInputs, ExportMetadata,
                PreparedDiagnosticArchive, RedactionContext,
            },
            DiagnosticEvent, DiagnosticEventKind, DiagnosticLevel, DIAGNOSTIC_SCHEMA_VERSION,
        },
        operation_log::{OperationAction, OperationPhase, OperationRecord, OperationStatus},
    };

    use super::{
        destination_failure, encode_operations, event_timestamp_bounds, preparation_failure,
        select_events, select_operations, sqlite_schema_version, validate_frontend_input,
        ExportSelection, ExportSelectionMode, FrontendDiagnosticInput, PreparedExportContext,
        PreparedExportRegistry, MAX_PREPARED_EXPORTS, PREPARED_EXPORT_TTL,
    };

    fn event(timestamp: u128, attempt: &str, operation: Option<&str>) -> DiagnosticEvent {
        DiagnosticEvent {
            schema_version: DIAGNOSTIC_SCHEMA_VERSION,
            event_id: format!("event-{timestamp}"),
            session_id: "session-test".to_string(),
            sequence: timestamp as u64,
            timestamp,
            level: DiagnosticLevel::Info,
            component: "test".to_string(),
            event_kind: DiagnosticEventKind::OperationPhase,
            attempt_id: Some(attempt.to_string()),
            operation_id: operation.map(str::to_string),
            action: Some("switchRuntime".to_string()),
            phase: Some("apply".to_string()),
            terminal_status: None,
            error_code: None,
            safe_message: None,
            safe_context: BTreeMap::new(),
        }
    }

    fn prepared_archive(selection: ExportSelection) -> PreparedDiagnosticArchive {
        let inputs = ExportInputs {
            metadata: ExportMetadata {
                schema_version: DIAGNOSTIC_SCHEMA_VERSION,
                application_version: "0.2.7".to_string(),
                build_version: None,
                exported_at: "2026-08-09T15:30:12+08:00".to_string(),
                timezone_offset_minutes: 480,
                redaction_policy_version: 1,
                selection,
                event_count: 1,
                unavailable: Vec::new(),
                warnings: Vec::new(),
            },
            redaction: RedactionContext::default(),
            diagnostics_jsonl: b"{}\n".to_vec(),
            operations_jsonl: b"{}\n".to_vec(),
            health_json: b"{}".to_vec(),
        };
        prepare_diagnostic_archive(inputs).unwrap()
    }

    fn prepared_context(retry_id: &str, created_at: Instant) -> PreparedExportContext {
        let selection = ExportSelection {
            mode: ExportSelectionMode::Operation,
            operation_id: Some("operation-1".to_string()),
            from_timestamp_ms: 1,
            through_timestamp_ms: 2,
        };
        PreparedExportContext {
            retry_id: retry_id.to_string(),
            created_at,
            local_timestamp: "20260809-153012-004".to_string(),
            archive: prepared_archive(selection),
        }
    }

    fn operation_record(id: &str, started_at_ms: u128, completed_at_ms: u128) -> OperationRecord {
        OperationRecord {
            operation_id: id.to_string(),
            action: OperationAction::SyncSessions,
            status: OperationStatus::Succeeded,
            phase: OperationPhase::Complete,
            started_at_ms,
            completed_at_ms,
            backup_dirs: Vec::new(),
            counts: BTreeMap::new(),
        }
    }

    #[test]
    fn export_failures_serialize_destination_context_only() {
        let preparation = preparation_failure("prepare failed");
        let destination = destination_failure(
            "publish failed",
            "diagnostic-export-context-aabbccddeeff00112233445566778899".to_string(),
        );
        let preparation_json = serde_json::to_value(preparation).unwrap();
        let destination_json = serde_json::to_value(destination).unwrap();

        assert_eq!(preparation_json["kind"], "preparation");
        assert!(preparation_json.get("retryId").is_none());
        assert_eq!(destination_json["kind"], "destination");
        assert_eq!(
            destination_json["retryId"],
            "diagnostic-export-context-aabbccddeeff00112233445566778899"
        );
    }

    #[test]
    fn prepared_registry_is_bounded_expiring_and_one_time() {
        let now = Instant::now();
        let mut registry = PreparedExportRegistry::default();
        for index in 0..=MAX_PREPARED_EXPORTS {
            registry.insert(
                prepared_context(&format!("diagnostic-export-context-{index:032x}"), now),
                now,
            );
        }
        assert_eq!(registry.entries.len(), MAX_PREPARED_EXPORTS);
        assert!(registry
            .take(
                "diagnostic-export-context-00000000000000000000000000000000",
                now
            )
            .is_none());

        let retained = format!("diagnostic-export-context-{:032x}", MAX_PREPARED_EXPORTS);
        let context = registry.take(&retained, now).unwrap();
        let expected_hash = context.archive.sha256().to_string();
        assert_eq!(context.archive.sha256(), expected_hash);
        assert!(registry.take(&retained, now).is_none());

        registry.insert(
            prepared_context(
                "diagnostic-export-context-ffffffffffffffffffffffffffffffff",
                now - PREPARED_EXPORT_TTL - Duration::from_secs(1),
            ),
            now,
        );
        assert!(registry
            .take(
                "diagnostic-export-context-ffffffffffffffffffffffffffffffff",
                now,
            )
            .is_none());
    }

    #[test]
    fn status_timestamp_bounds_survive_wall_clock_regression() {
        let events = vec![
            event(900, "attempt-newer", None),
            event(100, "attempt-regressed", None),
            event(500, "attempt-middle", None),
        ];

        assert_eq!(event_timestamp_bounds(&events), (Some(100), Some(900)));
        assert_eq!(event_timestamp_bounds(&[]), (None, None));
    }

    #[test]
    fn retained_selection_uses_timestamp_extrema_and_survives_manifest_preparation() {
        let events = vec![
            event(900, "attempt-newer", None),
            event(100, "attempt-regressed", None),
            event(500, "attempt-middle", None),
        ];
        let selected_during_regression = select_events(events.clone(), None, 700).unwrap();
        assert_eq!(selected_during_regression.selection.from_timestamp_ms, 100);
        assert_eq!(
            selected_during_regression.selection.through_timestamp_ms,
            900
        );

        let selected_after_clock_advance = select_events(events, None, 1_100).unwrap();
        assert_eq!(
            selected_after_clock_advance.selection.from_timestamp_ms,
            100
        );
        assert_eq!(
            selected_after_clock_advance.selection.through_timestamp_ms,
            1_100
        );
        let prepared = prepared_archive(selected_after_clock_advance.selection.clone());
        assert_eq!(
            prepared.selection(),
            &selected_after_clock_advance.selection
        );
    }

    #[test]
    fn operation_export_includes_preceding_context_and_exact_operation() {
        let selected = select_events(
            vec![
                event(100, "background", None),
                event(600_100, "attempt-1", None),
                event(600_200, "attempt-1", Some("operation-1")),
                event(600_250, "later-unrelated", None),
            ],
            Some("operation-1"),
            600_300,
        )
        .unwrap();

        assert_eq!(selected.selection.mode, ExportSelectionMode::Operation);
        assert_eq!(selected.selection.from_timestamp_ms, 100);
        assert_eq!(selected.events.len(), 3);
    }

    #[test]
    fn durable_operation_selection_recovers_the_complete_long_attempt_before_bind() {
        let mut started = event(100, "attempt-long", None);
        started.event_kind = DiagnosticEventKind::OperationStarted;
        let mut early_phase = event(200, "attempt-long", None);
        early_phase.event_kind = DiagnosticEventKind::OperationPhase;
        let mut bound = event(700_200, "attempt-long", Some("operation-long"));
        bound.event_kind = DiagnosticEventKind::OperationBound;
        let mut terminal = event(900_500, "attempt-long", Some("operation-long"));
        terminal.event_kind = DiagnosticEventKind::OperationTerminal;
        let unrelated_after = event(900_600, "unrelated", None);

        let selected = select_events(
            vec![started, early_phase, bound, terminal, unrelated_after],
            Some("operation-long"),
            1_000_000,
        )
        .unwrap();

        assert_eq!(selected.selection.from_timestamp_ms, 0);
        assert_eq!(selected.selection.through_timestamp_ms, 900_500);
        assert_eq!(selected.events.len(), 4);
        assert!(selected
            .events
            .iter()
            .any(|event| event.event_kind == DiagnosticEventKind::OperationStarted));
        assert!(!selected
            .events
            .iter()
            .any(|event| event.attempt_id.as_deref() == Some("unrelated")));
    }

    #[test]
    fn missing_operation_is_not_guessed_from_latest_event() {
        let error =
            select_events(vec![event(1, "attempt-1", None)], Some("missing"), 2).unwrap_err();
        assert!(error.contains("no diagnostic events"));
    }

    #[test]
    fn operation_export_removes_backup_paths() {
        let record = OperationRecord {
            operation_id: "operation-1".to_string(),
            action: OperationAction::SyncSessions,
            status: OperationStatus::Failed,
            phase: OperationPhase::Apply,
            started_at_ms: 10,
            completed_at_ms: 20,
            backup_dirs: vec![PathBuf::from(r"C:\Users\Alice\secret")],
            counts: BTreeMap::new(),
        };
        let encoded = String::from_utf8(encode_operations(&[record]).unwrap()).unwrap();
        assert!(!encoded.contains("backup"));
        assert!(!encoded.contains("Alice"));
    }

    #[test]
    fn operation_selection_uses_true_overlap_window_even_when_clock_regresses() {
        let selection = ExportSelection {
            mode: ExportSelectionMode::RetainedWindow,
            operation_id: None,
            from_timestamp_ms: 10,
            through_timestamp_ms: 30,
        };
        let records = vec![
            operation_record("before", 1, 9),
            operation_record("ends-at-start", 1, 10),
            operation_record("encompasses", 5, 35),
            operation_record("inside", 15, 20),
            operation_record("starts-at-end", 30, 40),
            operation_record("after", 31, 40),
            operation_record("clock-regressed", 25, 8),
        ];
        let selected = select_operations(records, &selection)
            .into_iter()
            .map(|record| record.operation_id)
            .collect::<Vec<_>>();
        assert_eq!(
            selected,
            vec![
                "ends-at-start",
                "encompasses",
                "inside",
                "starts-at-end",
                "clock-regressed",
            ]
        );
    }

    #[test]
    fn frontend_events_accept_only_fixed_safe_classifications() {
        let valid = FrontendDiagnosticInput {
            level: "error".to_string(),
            component: "frontend".to_string(),
            event_kind: "unhandledError".to_string(),
            error_code: "frontend.unhandled_error".to_string(),
            safe_message: "A browser error occurred".to_string(),
        };
        assert!(validate_frontend_input(&valid).is_ok());
        let invalid = FrontendDiagnosticInput {
            safe_message: "x".repeat(513),
            ..valid
        };
        assert!(validate_frontend_input(&invalid).is_err());
    }

    #[test]
    fn sqlite_health_reads_only_schema_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state_5.sqlite");
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch("CREATE TABLE diagnostic_fixture(id INTEGER PRIMARY KEY);")
            .unwrap();
        drop(connection);

        assert!(sqlite_schema_version(&path).unwrap() > 0);
    }
}

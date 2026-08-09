use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Cursor, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use zip::{write::SimpleFileOptions, CompressionMethod, ZipArchive, ZipWriter};

#[cfg(windows)]
use std::os::windows::{fs::MetadataExt, io::AsRawHandle};
#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::HANDLE,
    Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT,
    },
};

use crate::file_ops::atomic_publish_new;

use super::platform::{
    downloads_dir, is_diagnostic_archive_name, local_filename_timestamp, validate_downloads_dir,
    validate_export_file, DIAGNOSTIC_ARCHIVE_PREFIX,
};
use super::sanitize::{
    contains_absolute_path, contains_secret_shape, contains_uuid_like_id, redact_free_form_ids,
    replace_known_path_root,
};

const README_NAME: &str = "README.txt";
const MANIFEST_NAME: &str = "manifest.json";
const DIAGNOSTICS_NAME: &str = "diagnostics.jsonl";
const OPERATIONS_NAME: &str = "operations.jsonl";
const HEALTH_NAME: &str = "health.json";
const ENTRY_NAMES: [&str; 5] = [
    README_NAME,
    MANIFEST_NAME,
    DIAGNOSTICS_NAME,
    OPERATIONS_NAME,
    HEALTH_NAME,
];
const MAX_ENTRY_BYTES: usize = 12 * 1024 * 1024;
const MAX_TOTAL_ENTRY_BYTES: usize = 16 * 1024 * 1024;
const MAX_JSON_DEPTH: usize = 16;
const MAX_OBJECT_FIELDS: usize = 256;
const MAX_ARRAY_ITEMS: usize = 512;
const MAX_STRING_CHARS: usize = 4_096;
const MAX_ARCHIVE_NAME_ATTEMPTS: usize = 100;
const README: &str = "ChatGPT Switch 诊断包\r\n\
\r\n\
此文件由 ChatGPT Switch 自动生成，供维护者定位故障。\r\n\
内容已经过结构化脱敏，不包含凭据、聊天正文、请求或响应正文。\r\n\
\r\n\
文件：\r\n\
- manifest.json：版本、选择窗口和各负载文件的大小与 SHA-256。\r\n\
- diagnostics.jsonl：脱敏后的诊断事件。\r\n\
- operations.jsonl：相关操作终态的脱敏子集。\r\n\
- health.json：只读健康摘要。\r\n\
\r\n\
manifest.json 不递归记录自身哈希；导出回执中的 ZIP SHA-256 覆盖整个压缩包。\r\n";

static STAGING_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ExportSelectionMode {
    Operation,
    RetainedWindow,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ExportSelection {
    pub mode: ExportSelectionMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    pub from_timestamp_ms: u128,
    pub through_timestamp_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ExportMetadata {
    pub schema_version: u32,
    pub application_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_version: Option<String>,
    pub exported_at: String,
    pub timezone_offset_minutes: i32,
    pub redaction_policy_version: u32,
    pub selection: ExportSelection,
    pub event_count: usize,
    #[serde(default)]
    pub unavailable: Vec<String>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RedactionContext {
    pub user_profile: Option<PathBuf>,
    pub appdata: Option<PathBuf>,
    pub codex_home: Option<PathBuf>,
    pub forbidden_literals: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportInputs {
    pub metadata: ExportMetadata,
    pub redaction: RedactionContext,
    pub diagnostics_jsonl: Vec<u8>,
    pub operations_jsonl: Vec<u8>,
    pub health_json: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticExportReceipt {
    pub export_id: String,
    pub path: PathBuf,
    pub filename: String,
    pub bytes: u64,
    pub sha256: String,
    pub event_count: usize,
    pub selection: ExportSelection,
    pub warnings: Vec<String>,
}

/// A fully built, sanitized, and self-checked archive that is safe to retain
/// briefly while the user chooses a fixed export destination. It deliberately
/// contains neither the raw export inputs nor any destination path.
#[derive(Debug)]
pub struct PreparedDiagnosticArchive {
    bytes: Vec<u8>,
    sha256: String,
    event_count: usize,
    selection: ExportSelection,
    warnings: Vec<String>,
}

impl PreparedDiagnosticArchive {
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    pub fn selection(&self) -> &ExportSelection {
        &self.selection
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct DiagnosticManifest {
    schema_version: u32,
    application_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    build_version: Option<String>,
    exported_at: String,
    timezone_offset_minutes: i32,
    timestamp_unit: String,
    platform: String,
    architecture: String,
    redaction_policy_version: u32,
    selection: ExportSelection,
    event_count: usize,
    manifest_file: String,
    files: Vec<ManifestFile>,
    unavailable: Vec<String>,
    warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct ManifestFile {
    name: String,
    bytes: usize,
    sha256: String,
}

struct SanitizedJsonl {
    bytes: Vec<u8>,
    tail_dropped: bool,
}

struct StagingFile {
    path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    first: u64,
    second: u64,
}

impl Drop for StagingFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub fn export_to_downloads(inputs: ExportInputs) -> Result<DiagnosticExportReceipt, String> {
    let prepared = prepare_diagnostic_archive(inputs)?;
    let timestamp = local_filename_timestamp()?;
    let downloads = downloads_dir()?;
    publish_prepared_to_directory_at(&prepared, &downloads, &timestamp)
}

pub fn export_to_directory_at(
    inputs: ExportInputs,
    directory: &Path,
    local_timestamp: &str,
) -> Result<DiagnosticExportReceipt, String> {
    let prepared = prepare_diagnostic_archive(inputs)?;
    publish_prepared_to_directory_at(&prepared, directory, local_timestamp)
}

pub fn prepare_diagnostic_archive(
    inputs: ExportInputs,
) -> Result<PreparedDiagnosticArchive, String> {
    let entries = build_entries(&inputs)?;
    let manifest: DiagnosticManifest = serde_json::from_slice(
        entries
            .get(MANIFEST_NAME)
            .ok_or_else(|| "the diagnostic manifest is missing".to_string())?,
    )
    .map_err(|_| "the diagnostic manifest is invalid".to_string())?;
    let bytes = zip_bytes(&entries)?;
    self_check_archive_bytes(&bytes, &entries, &inputs.redaction)?;
    let sha256 = sha256_bytes(&bytes);

    Ok(PreparedDiagnosticArchive {
        bytes,
        sha256,
        event_count: manifest.event_count,
        selection: manifest.selection,
        warnings: manifest.warnings,
    })
}

pub fn publish_prepared_to_directory_at(
    prepared: &PreparedDiagnosticArchive,
    directory: &Path,
    local_timestamp: &str,
) -> Result<DiagnosticExportReceipt, String> {
    publish_prepared_to_directory_at_with(prepared, directory, local_timestamp, |root, path| {
        let published = validate_export_file(root, path)?;
        if sha256_file(&published)? != prepared.sha256 {
            return Err("the published diagnostic archive failed verification".to_string());
        }
        let bytes = fs::metadata(&published)
            .map_err(|_| "failed to inspect the diagnostic archive".to_string())?
            .len();
        Ok((published, bytes))
    })
}

fn publish_prepared_to_directory_at_with<Verify>(
    prepared: &PreparedDiagnosticArchive,
    directory: &Path,
    local_timestamp: &str,
    verify: Verify,
) -> Result<DiagnosticExportReceipt, String>
where
    Verify: FnOnce(&Path, &Path) -> Result<(PathBuf, u64), String>,
{
    validate_filename_timestamp(local_timestamp)?;
    let directory = validate_downloads_dir(directory.to_path_buf())?;
    let staging = create_staging_file(&directory)?;
    write_prepared_archive(&staging.path, &prepared.bytes)?;
    if sha256_file(&staging.path)? != prepared.sha256 {
        return Err("the diagnostic export staging failed verification".to_string());
    }
    let staging_identity = path_identity(&staging.path)?;

    let base = format!("{DIAGNOSTIC_ARCHIVE_PREFIX}{local_timestamp}");
    let mut published = None;
    for attempt in 1..=MAX_ARCHIVE_NAME_ATTEMPTS {
        let filename = if attempt == 1 {
            format!("{base}.zip")
        } else {
            format!("{base}-{attempt}.zip")
        };
        let target = directory.join(&filename);
        if atomic_publish_new(&staging.path, &target)? {
            published = Some((filename, target));
            break;
        }
    }
    let (filename, published) = published
        .ok_or_else(|| "too many diagnostic archives use the same timestamp".to_string())?;
    let (published, bytes) = match verify(&directory, &published) {
        Ok(verified) => verified,
        Err(error) => {
            cleanup_failed_publish(&directory, &published, staging_identity);
            return Err(error);
        }
    };
    let export_id = export_id(&filename, &prepared.sha256);

    Ok(DiagnosticExportReceipt {
        export_id,
        path: published,
        filename,
        bytes,
        sha256: prepared.sha256.clone(),
        event_count: prepared.event_count,
        selection: prepared.selection.clone(),
        warnings: prepared.warnings.clone(),
    })
}

fn build_entries(inputs: &ExportInputs) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let diagnostics = sanitize_jsonl(&inputs.diagnostics_jsonl, &inputs.redaction)?;
    let operations = sanitize_jsonl(&inputs.operations_jsonl, &inputs.redaction)?;
    let health = sanitize_json(&inputs.health_json, &inputs.redaction)?;
    let mut metadata = sanitized_metadata(&inputs.metadata, &inputs.redaction)?;
    if diagnostics.tail_dropped {
        metadata
            .warnings
            .push("diagnosticsTailRecordDropped".to_string());
    }
    if operations.tail_dropped {
        metadata
            .warnings
            .push("operationsTailRecordDropped".to_string());
    }
    metadata.warnings.sort();
    metadata.warnings.dedup();

    let payloads = [
        (README_NAME, README.as_bytes().to_vec()),
        (DIAGNOSTICS_NAME, diagnostics.bytes),
        (OPERATIONS_NAME, operations.bytes),
        (HEALTH_NAME, health),
    ];
    let total = payloads.iter().try_fold(0usize, |total, (_, bytes)| {
        if bytes.len() > MAX_ENTRY_BYTES {
            return Err("a diagnostic package entry exceeds its size limit".to_string());
        }
        total
            .checked_add(bytes.len())
            .ok_or_else(|| "the diagnostic package size overflowed".to_string())
    })?;
    if total > MAX_TOTAL_ENTRY_BYTES {
        return Err("the diagnostic package exceeds its size limit".to_string());
    }

    let manifest_files = payloads
        .iter()
        .map(|(name, bytes)| ManifestFile {
            name: (*name).to_string(),
            bytes: bytes.len(),
            sha256: sha256_bytes(bytes),
        })
        .collect::<Vec<_>>();
    let manifest = DiagnosticManifest {
        schema_version: metadata.schema_version,
        application_version: metadata.application_version,
        build_version: metadata.build_version,
        exported_at: metadata.exported_at,
        timezone_offset_minutes: metadata.timezone_offset_minutes,
        timestamp_unit: "unixEpochMilliseconds".to_string(),
        platform: "windows".to_string(),
        architecture: std::env::consts::ARCH.to_string(),
        redaction_policy_version: metadata.redaction_policy_version,
        selection: metadata.selection,
        event_count: metadata.event_count,
        manifest_file: MANIFEST_NAME.to_string(),
        files: manifest_files,
        unavailable: metadata.unavailable,
        warnings: metadata.warnings,
    };
    let manifest = serde_json::to_vec_pretty(&manifest)
        .map_err(|_| "failed to encode the diagnostic manifest".to_string())?;
    if manifest.len() > MAX_ENTRY_BYTES
        || total
            .checked_add(manifest.len())
            .is_none_or(|total| total > MAX_TOTAL_ENTRY_BYTES)
    {
        return Err("the diagnostic package exceeds its size limit".to_string());
    }

    let mut entries = BTreeMap::new();
    entries.insert(README_NAME.to_string(), payloads[0].1.clone());
    entries.insert(MANIFEST_NAME.to_string(), manifest);
    entries.insert(DIAGNOSTICS_NAME.to_string(), payloads[1].1.clone());
    entries.insert(OPERATIONS_NAME.to_string(), payloads[2].1.clone());
    entries.insert(HEALTH_NAME.to_string(), payloads[3].1.clone());
    for (name, bytes) in &entries {
        scan_entry(name, bytes, &inputs.redaction)?;
    }
    Ok(entries)
}

fn sanitized_metadata(
    metadata: &ExportMetadata,
    context: &RedactionContext,
) -> Result<ExportMetadata, String> {
    let mut value = serde_json::to_value(metadata)
        .map_err(|_| "failed to encode diagnostic export metadata".to_string())?;
    sanitize_value(&mut value, context, 0)?;
    let bytes = serde_json::to_vec(&value)
        .map_err(|_| "failed to encode diagnostic export metadata".to_string())?;
    serde_json::from_slice(&bytes)
        .map_err(|_| "diagnostic export metadata is invalid after redaction".to_string())
}

fn sanitize_json(bytes: &[u8], context: &RedactionContext) -> Result<Vec<u8>, String> {
    if bytes.len() > MAX_ENTRY_BYTES {
        return Err("a diagnostic package entry exceeds its size limit".to_string());
    }
    let mut value: Value = serde_json::from_slice(bytes)
        .map_err(|_| "diagnostic health data is invalid".to_string())?;
    sanitize_value(&mut value, context, 0)?;
    serde_json::to_vec_pretty(&value)
        .map_err(|_| "failed to encode diagnostic health data".to_string())
}

fn sanitize_jsonl(bytes: &[u8], context: &RedactionContext) -> Result<SanitizedJsonl, String> {
    if bytes.len() > MAX_ENTRY_BYTES {
        return Err("a diagnostic package entry exceeds its size limit".to_string());
    }
    let lines = bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| line.iter().any(|byte| !byte.is_ascii_whitespace()))
        .collect::<Vec<_>>();
    let mut output = Vec::with_capacity(bytes.len());
    let mut tail_dropped = false;
    for (index, line) in lines.iter().enumerate() {
        let mut value: Value = match serde_json::from_slice(line) {
            Ok(value) => value,
            Err(_) if index + 1 == lines.len() && !bytes.ends_with(b"\n") => {
                tail_dropped = true;
                break;
            }
            Err(_) => return Err("diagnostic JSONL contains internal corruption".to_string()),
        };
        sanitize_value(&mut value, context, 0)?;
        serde_json::to_writer(&mut output, &value)
            .map_err(|_| "failed to encode diagnostic JSONL".to_string())?;
        output.push(b'\n');
    }
    Ok(SanitizedJsonl {
        bytes: output,
        tail_dropped,
    })
}

fn sanitize_value(
    value: &mut Value,
    context: &RedactionContext,
    depth: usize,
) -> Result<(), String> {
    sanitize_value_scoped(value, context, depth, true, true, false)
}

fn sanitize_value_scoped(
    value: &mut Value,
    context: &RedactionContext,
    depth: usize,
    allow_structured_id_fields: bool,
    allow_selection_child: bool,
    preserve_uuid: bool,
) -> Result<(), String> {
    if depth > MAX_JSON_DEPTH {
        return Err("diagnostic data exceeds the nesting limit".to_string());
    }
    match value {
        Value::Object(object) => {
            if object.len() > MAX_OBJECT_FIELDS {
                return Err("diagnostic data contains too many fields".to_string());
            }
            object.retain(|key, _| !is_forbidden_key(key));
            for (key, value) in object.iter_mut() {
                let normalized = normalize_key(key);
                let child_preserve_uuid =
                    allow_structured_id_fields && is_structured_id_key(&normalized);
                let child_allows_structured_ids =
                    allow_selection_child && normalized == "selection";
                sanitize_value_scoped(
                    value,
                    context,
                    depth + 1,
                    child_allows_structured_ids,
                    false,
                    child_preserve_uuid,
                )?;
            }
        }
        Value::Array(array) => {
            if array.len() > MAX_ARRAY_ITEMS {
                return Err("diagnostic data contains too many items".to_string());
            }
            for value in array {
                sanitize_value_scoped(value, context, depth + 1, false, false, false)?;
            }
        }
        Value::String(string) => *string = sanitize_string(string, context, preserve_uuid),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
    Ok(())
}

fn sanitize_string(value: &str, context: &RedactionContext, preserve_uuid: bool) -> String {
    let mut value = value.to_string();
    let mut roots = [
        context
            .codex_home
            .as_ref()
            .map(|path| (path.to_string_lossy().into_owned(), "%CODEX_HOME%")),
        context
            .appdata
            .as_ref()
            .map(|path| (path.to_string_lossy().into_owned(), "%APPDATA%")),
        context
            .user_profile
            .as_ref()
            .map(|path| (path.to_string_lossy().into_owned(), "%USERPROFILE%")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    roots.sort_by_key(|right| std::cmp::Reverse(right.0.len()));
    for (root, replacement) in roots {
        value = replace_known_path_root(&value, &root, replacement);
        value = replace_known_path_root(&value, &root.replace('\\', "/"), replacement);
    }
    for literal in &context.forbidden_literals {
        if !literal.is_empty() {
            value = replace_forbidden_literal(&value, literal, "[REDACTED]");
        }
    }
    if !preserve_uuid {
        value = redact_free_form_ids(&value);
    }
    if contains_secret_shape(&value) {
        return "[REDACTED]".to_string();
    }
    if contains_absolute_path(&value) {
        return "[REDACTED_PATH]".to_string();
    }
    if value.chars().count() > MAX_STRING_CHARS {
        value = value.chars().take(MAX_STRING_CHARS).collect();
    }
    value
}

fn replace_forbidden_literal(value: &str, needle: &str, replacement: &str) -> String {
    if needle.is_empty() {
        return value.to_string();
    }
    let source = value.as_bytes();
    let target = needle.as_bytes();
    let mut output = String::with_capacity(value.len());
    let mut cursor = 0usize;
    let mut index = 0usize;
    while index + target.len() <= source.len() {
        let end = index + target.len();
        if source[index..end].eq_ignore_ascii_case(target)
            && is_literal_boundary(source.get(index.wrapping_sub(1)).copied())
            && is_literal_boundary(source.get(end).copied())
        {
            output.push_str(&value[cursor..index]);
            output.push_str(replacement);
            cursor = end;
            index = end;
        } else {
            index += 1;
        }
    }
    output.push_str(&value[cursor..]);
    output
}

fn contains_forbidden_literal(value: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let source = value.as_bytes();
    let target = needle.as_bytes();
    (0..=source.len().saturating_sub(target.len())).any(|index| {
        let end = index + target.len();
        end <= source.len()
            && source[index..end].eq_ignore_ascii_case(target)
            && is_literal_boundary(source.get(index.wrapping_sub(1)).copied())
            && is_literal_boundary(source.get(end).copied())
    })
}

fn is_literal_boundary(byte: Option<u8>) -> bool {
    byte.is_none_or(|byte| !byte.is_ascii_alphanumeric() && !matches!(byte, b'_' | b'-'))
}

fn is_forbidden_key(key: &str) -> bool {
    let normalized = normalize_key(key);
    matches!(
        normalized.as_str(),
        "authorization"
            | "apikey"
            | "openaiapikey"
            | "bearer"
            | "bearertoken"
            | "token"
            | "accesstoken"
            | "refreshtoken"
            | "idtoken"
            | "password"
            | "credential"
            | "credentials"
            | "cookie"
            | "setcookie"
            | "tokens"
            | "secret"
            | "clientsecret"
            | "authjson"
            | "configtoml"
            | "requestbody"
            | "responsebody"
            | "sessionjsonl"
    ) || normalized.ends_with("apikey")
        || normalized.ends_with("accesstoken")
        || normalized.ends_with("refreshtoken")
        || normalized.ends_with("clientsecret")
}

fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn is_structured_id_key(normalized: &str) -> bool {
    matches!(
        normalized,
        "eventid" | "sessionid" | "attemptid" | "operationid"
    )
}

fn scan_entry(name: &str, bytes: &[u8], context: &RedactionContext) -> Result<(), String> {
    let text =
        std::str::from_utf8(bytes).map_err(|_| format!("diagnostic entry {name} is not UTF-8"))?;
    for literal in &context.forbidden_literals {
        if contains_forbidden_literal(text, literal) {
            return Err(format!(
                "diagnostic entry {name} contains a forbidden value"
            ));
        }
    }
    for path in [
        context.user_profile.as_ref(),
        context.appdata.as_ref(),
        context.codex_home.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        let path = path.to_string_lossy();
        if text
            .to_ascii_lowercase()
            .contains(&path.to_ascii_lowercase())
            || text
                .to_ascii_lowercase()
                .contains(&path.replace('\\', "/").to_ascii_lowercase())
        {
            return Err(format!("diagnostic entry {name} contains a private path"));
        }
    }
    if name.ends_with(".jsonl") {
        for line in bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| line.iter().any(|byte| !byte.is_ascii_whitespace()))
        {
            let value: Value = serde_json::from_slice(line)
                .map_err(|_| format!("diagnostic entry {name} contains invalid JSONL"))?;
            scan_json_value(name, &value, context)?;
        }
    } else if name.ends_with(".json") {
        let value: Value = serde_json::from_slice(bytes)
            .map_err(|_| format!("diagnostic entry {name} contains invalid JSON"))?;
        scan_json_value(name, &value, context)?;
    } else if contains_secret_shape(text)
        || contains_uuid_like_id(text)
        || contains_absolute_path(text)
    {
        return Err(format!("diagnostic entry {name} contains forbidden text"));
    }
    Ok(())
}

fn scan_json_value(name: &str, value: &Value, context: &RedactionContext) -> Result<(), String> {
    scan_json_value_scoped(name, value, context, true, true, false)
}

fn scan_json_value_scoped(
    name: &str,
    value: &Value,
    context: &RedactionContext,
    allow_structured_id_fields: bool,
    allow_selection_child: bool,
    preserve_uuid: bool,
) -> Result<(), String> {
    match value {
        Value::Object(object) => {
            if object.keys().any(|key| is_forbidden_key(key)) {
                return Err(format!(
                    "diagnostic entry {name} contains a forbidden field"
                ));
            }
            for (key, value) in object {
                let normalized = normalize_key(key);
                let child_preserve_uuid =
                    allow_structured_id_fields && is_structured_id_key(&normalized);
                scan_json_value_scoped(
                    name,
                    value,
                    context,
                    allow_selection_child && normalized == "selection",
                    false,
                    child_preserve_uuid,
                )?;
            }
        }
        Value::Array(array) => {
            for value in array {
                scan_json_value_scoped(name, value, context, false, false, false)?;
            }
        }
        Value::String(value) => {
            for literal in &context.forbidden_literals {
                if contains_forbidden_literal(value, literal) {
                    return Err(format!(
                        "diagnostic entry {name} contains a forbidden value"
                    ));
                }
            }
            for path in [
                context.user_profile.as_ref(),
                context.appdata.as_ref(),
                context.codex_home.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                let path = path.to_string_lossy();
                if contains_ascii_case_insensitive(value, &path)
                    || contains_ascii_case_insensitive(value, &path.replace('\\', "/"))
                {
                    return Err(format!("diagnostic entry {name} contains a private path"));
                }
            }
            if contains_secret_shape(value)
                || (!preserve_uuid && contains_uuid_like_id(value))
                || contains_absolute_path(value)
            {
                return Err(format!("diagnostic entry {name} contains forbidden text"));
            }
        }
        _ => {}
    }
    Ok(())
}

fn contains_ascii_case_insensitive(value: &str, needle: &str) -> bool {
    !needle.is_empty()
        && value
            .to_ascii_lowercase()
            .contains(&needle.to_ascii_lowercase())
}

fn create_staging_file(directory: &Path) -> Result<StagingFile, String> {
    for _ in 0..32 {
        let sequence = STAGING_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!(
            ".chatgpt-switch-diagnostics.{}.{}.tmp",
            std::process::id(),
            sequence
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => {
                drop(file);
                return Ok(StagingFile { path });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err("failed to create diagnostic export staging".to_string()),
        }
    }
    Err("failed to allocate diagnostic export staging".to_string())
}

fn cleanup_failed_publish(directory: &Path, published: &Path, expected: Option<FileIdentity>) {
    let Some(expected) = expected else {
        return;
    };
    if published.parent() != Some(directory)
        || !published
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_diagnostic_archive_name)
    {
        return;
    }
    let Ok(metadata) = fs::symlink_metadata(published) else {
        return;
    };
    if !metadata.file_type().is_file() || metadata_is_reparse_point(&metadata) {
        return;
    }
    if path_identity(published).ok().flatten() != Some(expected) {
        return;
    }
    let _ = fs::remove_file(published);
}

fn path_identity(path: &Path) -> Result<Option<FileIdentity>, String> {
    let file = File::open(path).map_err(|_| "failed to identify the diagnostic ZIP".to_string())?;
    file_identity(&file)
}

#[cfg(windows)]
fn file_identity(file: &File) -> Result<Option<FileIdentity>, String> {
    let mut information = unsafe { std::mem::zeroed::<BY_HANDLE_FILE_INFORMATION>() };
    let ok = unsafe {
        GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut information as *mut _)
    };
    if ok == 0 {
        return Err("failed to identify the diagnostic ZIP".to_string());
    }
    Ok(Some(FileIdentity {
        first: u64::from(information.dwVolumeSerialNumber),
        second: (u64::from(information.nFileIndexHigh) << 32)
            | u64::from(information.nFileIndexLow),
    }))
}

#[cfg(unix)]
fn file_identity(file: &File) -> Result<Option<FileIdentity>, String> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file
        .metadata()
        .map_err(|_| "failed to identify the diagnostic ZIP".to_string())?;
    Ok(Some(FileIdentity {
        first: metadata.dev(),
        second: metadata.ino(),
    }))
}

#[cfg(not(any(windows, unix)))]
fn file_identity(_file: &File) -> Result<Option<FileIdentity>, String> {
    Ok(None)
}

#[cfg(windows)]
fn metadata_is_reparse_point(metadata: &fs::Metadata) -> bool {
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_reparse_point(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(test)]
fn write_zip(path: &Path, entries: &BTreeMap<String, Vec<u8>>) -> Result<(), String> {
    let bytes = zip_bytes(entries)?;
    write_prepared_archive(path, &bytes)
}

fn zip_bytes(entries: &BTreeMap<String, Vec<u8>>) -> Result<Vec<u8>, String> {
    let cursor = Cursor::new(Vec::new());
    let mut zip = ZipWriter::new(cursor);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    for name in ENTRY_NAMES {
        let bytes = entries
            .get(name)
            .ok_or_else(|| "the diagnostic package is missing a required entry".to_string())?;
        zip.start_file(name, options)
            .map_err(|_| "failed to create a diagnostic ZIP entry".to_string())?;
        zip.write_all(bytes)
            .map_err(|_| "failed to write a diagnostic ZIP entry".to_string())?;
    }
    zip.finish()
        .map(Cursor::into_inner)
        .map_err(|_| "failed to finalize the diagnostic ZIP".to_string())
}

fn write_prepared_archive(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .map_err(|_| "failed to open diagnostic export staging".to_string())?;
    let mut file = file;
    file.write_all(bytes)
        .map_err(|_| "failed to write diagnostic export staging".to_string())?;
    file.sync_all()
        .map_err(|_| "failed to flush the diagnostic ZIP".to_string())
}

fn self_check_archive_bytes(
    bytes: &[u8],
    expected: &BTreeMap<String, Vec<u8>>,
    context: &RedactionContext,
) -> Result<(), String> {
    let mut zip = ZipArchive::new(Cursor::new(bytes))
        .map_err(|_| "the diagnostic ZIP is invalid".to_string())?;
    self_check_zip(&mut zip, expected, context)
}

#[cfg(test)]
fn self_check_archive(
    path: &Path,
    expected: &BTreeMap<String, Vec<u8>>,
    context: &RedactionContext,
) -> Result<(), String> {
    let file = File::open(path).map_err(|_| "failed to reopen the diagnostic ZIP".to_string())?;
    let mut zip = ZipArchive::new(file).map_err(|_| "the diagnostic ZIP is invalid".to_string())?;
    self_check_zip(&mut zip, expected, context)
}

fn self_check_zip<R: Read + std::io::Seek>(
    zip: &mut ZipArchive<R>,
    expected: &BTreeMap<String, Vec<u8>>,
    context: &RedactionContext,
) -> Result<(), String> {
    if zip.len() != ENTRY_NAMES.len() {
        return Err("the diagnostic ZIP contains an unexpected entry count".to_string());
    }
    let allowed = ENTRY_NAMES.into_iter().collect::<BTreeSet<_>>();
    let mut actual = BTreeMap::new();
    let mut total = 0usize;
    for index in 0..zip.len() {
        let mut entry = zip
            .by_index(index)
            .map_err(|_| "failed to inspect a diagnostic ZIP entry".to_string())?;
        let name = entry.name().to_string();
        if entry.is_dir() || !allowed.contains(name.as_str()) || actual.contains_key(&name) {
            return Err("the diagnostic ZIP contains an unsafe entry".to_string());
        }
        let declared = usize::try_from(entry.size())
            .map_err(|_| "a diagnostic ZIP entry is too large".to_string())?;
        if declared > MAX_ENTRY_BYTES {
            return Err("a diagnostic ZIP entry is too large".to_string());
        }
        let mut bytes = Vec::with_capacity(declared);
        entry
            .by_ref()
            .take((MAX_ENTRY_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| "failed to read a diagnostic ZIP entry".to_string())?;
        if bytes.len() != declared || bytes.len() > MAX_ENTRY_BYTES {
            return Err("a diagnostic ZIP entry failed its size check".to_string());
        }
        total = total
            .checked_add(bytes.len())
            .ok_or_else(|| "the diagnostic ZIP size overflowed".to_string())?;
        if total > MAX_TOTAL_ENTRY_BYTES {
            return Err("the diagnostic ZIP exceeds its size limit".to_string());
        }
        scan_entry(&name, &bytes, context)?;
        actual.insert(name, bytes);
    }
    if &actual != expected {
        return Err("the diagnostic ZIP contents failed verification".to_string());
    }
    verify_manifest(&actual)
}

fn verify_manifest(entries: &BTreeMap<String, Vec<u8>>) -> Result<(), String> {
    let manifest: DiagnosticManifest = serde_json::from_slice(
        entries
            .get(MANIFEST_NAME)
            .ok_or_else(|| "the diagnostic manifest is missing".to_string())?,
    )
    .map_err(|_| "the diagnostic manifest is invalid".to_string())?;
    if manifest.manifest_file != MANIFEST_NAME || manifest.files.len() != 4 {
        return Err("the diagnostic manifest file list is invalid".to_string());
    }
    let mut names = BTreeSet::new();
    for file in manifest.files {
        if file.name == MANIFEST_NAME || !names.insert(file.name.clone()) {
            return Err("the diagnostic manifest file list is invalid".to_string());
        }
        let bytes = entries
            .get(&file.name)
            .ok_or_else(|| "the diagnostic manifest references a missing file".to_string())?;
        if file.bytes != bytes.len() || file.sha256 != sha256_bytes(bytes) {
            return Err("the diagnostic manifest integrity check failed".to_string());
        }
    }
    Ok(())
}

fn validate_filename_timestamp(value: &str) -> Result<(), String> {
    let candidate = format!("{DIAGNOSTIC_ARCHIVE_PREFIX}{value}.zip");
    if is_diagnostic_archive_name(&candidate) && value.len() == 19 {
        Ok(())
    } else {
        Err("the diagnostic export timestamp is invalid".to_string())
    }
}

fn export_id(filename: &str, archive_sha256: &str) -> String {
    let digest = Sha256::digest(format!("{filename}\0{archive_sha256}").as_bytes());
    format!("diag-{}", hex_bytes(&digest[..12]))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    hex_bytes(&Sha256::digest(bytes))
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|_| "failed to read the diagnostic ZIP".to_string())?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|_| "failed while hashing the diagnostic ZIP".to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex_bytes(&hasher.finalize()))
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs::OpenOptions,
        io::{Read, Seek, SeekFrom, Write},
    };

    use serde_json::{json, Value};
    use tempfile::tempdir;
    use zip::ZipArchive;

    use crate::diagnostics::{DiagnosticSanitizer, SanitizerRoots};

    use super::{
        build_entries, export_to_directory_at, prepare_diagnostic_archive,
        publish_prepared_to_directory_at, publish_prepared_to_directory_at_with, scan_entry,
        self_check_archive, ExportInputs, ExportMetadata, ExportSelection, ExportSelectionMode,
        RedactionContext, DIAGNOSTICS_NAME, ENTRY_NAMES, HEALTH_NAME, MANIFEST_NAME,
        OPERATIONS_NAME,
    };

    fn inputs() -> ExportInputs {
        ExportInputs {
            metadata: ExportMetadata {
                schema_version: 1,
                application_version: "0.2.7".to_string(),
                build_version: Some("test-build".to_string()),
                exported_at: "2026-08-09T15:30:12+08:00".to_string(),
                timezone_offset_minutes: 480,
                redaction_policy_version: 1,
                selection: ExportSelection {
                    mode: ExportSelectionMode::Operation,
                    operation_id: Some("switch-123".to_string()),
                    from_timestamp_ms: 1,
                    through_timestamp_ms: 2,
                },
                event_count: 1,
                unavailable: Vec::new(),
                warnings: Vec::new(),
            },
            redaction: RedactionContext {
                user_profile: Some(r"C:\Users\alice".into()),
                appdata: Some(r"C:\Users\alice\AppData\Roaming".into()),
                codex_home: Some(r"C:\Users\alice\.codex".into()),
                forbidden_literals: vec![
                    "sk-test-super-secret-value".to_string(),
                    "private chat sentence".to_string(),
                    "ALICE-PC".to_string(),
                ],
            },
            diagnostics_jsonl: br#"{"eventKind":"failed","safeMessage":"failed at C:\\Users\\alice\\.codex\\state_5.sqlite","apiKey":"sk-test-super-secret-value","chat":"private chat sentence"}
"#
            .to_vec(),
            operations_jsonl: br#"{"operationId":"switch-123","backupDirs":["C:\\Users\\alice\\AppData\\Roaming\\codex-switch\\backups\\one"]}
"#
            .to_vec(),
            health_json: serde_json::to_vec(&json!({
                "machine": "ALICE-PC",
                "codexHome": "C:\\Users\\alice\\.codex",
                "credentialConfigured": true
            }))
            .unwrap(),
        }
    }

    #[test]
    fn exports_exact_fixed_entries_and_revalidates_manifest_hashes() {
        let root = tempdir().unwrap();
        let receipt = export_to_directory_at(inputs(), root.path(), "20260809-153012-004").unwrap();

        assert_eq!(
            receipt.filename,
            "ChatGPT-Switch-Diagnostics-20260809-153012-004.zip"
        );
        assert_eq!(receipt.event_count, 1);
        assert!(receipt.export_id.starts_with("diag-"));
        assert_eq!(receipt.sha256.len(), 64);

        let mut archive = ZipArchive::new(std::fs::File::open(receipt.path).unwrap()).unwrap();
        let mut names = (0..archive.len())
            .map(|index| archive.by_index(index).unwrap().name().to_string())
            .collect::<Vec<_>>();
        names.sort();
        let mut expected = ENTRY_NAMES.map(str::to_string).to_vec();
        expected.sort();
        assert_eq!(names, expected);
        let mut manifest = String::new();
        archive
            .by_name(MANIFEST_NAME)
            .unwrap()
            .read_to_string(&mut manifest)
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&manifest).unwrap()["timestampUnit"],
            "unixEpochMilliseconds"
        );
    }

    #[test]
    fn export_sanitizes_paths_credentials_identity_and_content_before_zip() {
        let entries = build_entries(&inputs()).unwrap();
        let all = entries
            .values()
            .flat_map(|bytes| bytes.iter().copied())
            .collect::<Vec<_>>();
        let visible = String::from_utf8_lossy(&all).to_ascii_lowercase();

        for forbidden in [
            "sk-test-super-secret-value",
            "private chat sentence",
            "alice-pc",
            r"c:\users\alice",
        ] {
            assert!(!visible.contains(forbidden), "{forbidden}");
        }
        assert!(String::from_utf8_lossy(&entries[DIAGNOSTICS_NAME]).contains("%CODEX_HOME%"));
        assert!(String::from_utf8_lossy(&entries[OPERATIONS_NAME]).contains("%APPDATA%"));
        assert!(!String::from_utf8_lossy(&entries[DIAGNOSTICS_NAME]).contains("apiKey"));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&entries[HEALTH_NAME]).unwrap()
                ["credentialConfigured"],
            true
        );
    }

    #[test]
    fn export_redacts_free_form_business_ids_but_preserves_structured_correlation_ids() {
        let structured = "123e4567-e89b-12d3-a456-426614174000";
        let expected = "123e4567-e89b-12d3-a456-426614174004";
        let actual = "123e4567-e89b-12d3-a456-426614174005";
        let mut value = inputs();
        value.metadata.selection.operation_id = Some(structured.into());
        value.diagnostics_jsonl = format!(
            "{}\n",
            json!({
                "eventId": structured,
                "sessionId": structured,
                "attemptId": structured,
                "operationId": structured,
                "safeMessage": format!(
                    "source session JSONL id changed from {expected} to {actual}"
                ),
                "safeContext": {
                    "detail": format!("provider mismatch ({expected} != {actual})"),
                    "operationId": expected,
                    "selection": {"sessionId": actual}
                }
            })
        )
        .into_bytes();
        value.operations_jsonl = format!("{}\n", json!({"operationId": structured})).into_bytes();

        let entries = build_entries(&value).unwrap();
        let diagnostic: Value = serde_json::from_slice(
            entries[DIAGNOSTICS_NAME]
                .split(|byte| *byte == b'\n')
                .next()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(diagnostic["eventId"], structured);
        assert_eq!(diagnostic["sessionId"], structured);
        assert_eq!(diagnostic["attemptId"], structured);
        assert_eq!(diagnostic["operationId"], structured);
        assert!(diagnostic["safeMessage"]
            .as_str()
            .unwrap()
            .contains("source session JSONL id changed from [REDACTED_ID] to [REDACTED_ID]"));
        assert_eq!(diagnostic["safeContext"]["operationId"], "[REDACTED_ID]");
        assert_eq!(
            diagnostic["safeContext"]["selection"]["sessionId"],
            "[REDACTED_ID]"
        );
        let manifest: Value = serde_json::from_slice(&entries[MANIFEST_NAME]).unwrap();
        assert_eq!(manifest["selection"]["operationId"], structured);
        for bytes in entries.values() {
            let text = String::from_utf8_lossy(bytes);
            assert!(!text.contains(expected), "{text}");
            assert!(!text.contains(actual), "{text}");
        }
    }

    #[test]
    fn export_path_redaction_handles_forward_unc_and_final_scan_excludes_web_urls() {
        let mut value = inputs();
        value.diagnostics_jsonl = format!(
            "{}\n",
            json!({
                "eventId": "event-1",
                "safeMessage": "path mismatch //server/share/Alice/x",
                "safeContext": {
                    "device": "//?/C:/Users/Alice/private.txt",
                    "similar": r"C:\Users\AliceOther\state.db",
                    "similarSpace": r"C:\Users\Alice Other\private.txt",
                    "similarComma": r"C:\Users\Alice,Other\private.txt",
                    "url": "https://example.test/Users/Alice/docs"
                }
            })
        )
        .into_bytes();
        let entries = build_entries(&value).unwrap();
        let diagnostics = String::from_utf8_lossy(&entries[DIAGNOSTICS_NAME]);
        for forbidden in [
            "server",
            "share",
            "AliceOther",
            "Alice Other",
            "Alice,Other",
            "private.txt",
            "state.db",
        ] {
            assert!(!diagnostics.contains(forbidden), "{diagnostics}");
        }
        assert!(!diagnostics.contains("%USERPROFILE%Other"), "{diagnostics}");
        assert!(diagnostics.contains("https://example.test/Users/Alice/docs"));

        let context = RedactionContext::default();
        for raw_path in [
            "//server/share/Alice/x",
            "//?/C:/Users/Alice/x",
            r"D:\Other User\private.txt",
            "//server/share/My Documents/private.txt",
            r"D:\Other\a%b\secret.db",
            r"D:\Other\[draft]\secret.db",
            r"D:\Other\customer,2026.db",
            r"D:\Other\customer;2026.db",
            r"D:\Other\customer)2026.db",
            r"D:\Other\O'Brien\file.db",
            r"C:\Users\Alice Other\private.txt",
            r"C:\Users\Alice,Other\private.txt",
        ] {
            let payload = serde_json::to_vec(&json!({"safeMessage": raw_path})).unwrap();
            assert!(scan_entry(HEALTH_NAME, &payload, &context).is_err());
        }
        let raw_uuid = serde_json::to_vec(&json!({
            "safeMessage": "mismatch 123e4567-e89b-12d3-a456-426614174099"
        }))
        .unwrap();
        assert!(scan_entry(HEALTH_NAME, &raw_uuid, &context).is_err());
        let nested_selection_uuid = serde_json::to_vec(&json!({
            "safeContext": {
                "selection": {
                    "sessionId": "123e4567-e89b-12d3-a456-426614174099"
                }
            }
        }))
        .unwrap();
        assert!(scan_entry(HEALTH_NAME, &nested_selection_uuid, &context).is_err());
        let structured_and_url = serde_json::to_vec(&json!({
            "operationId": "123e4567-e89b-12d3-a456-426614174099",
            "url": "https://example.test/Users/Alice/docs"
        }))
        .unwrap();
        assert!(scan_entry(HEALTH_NAME, &structured_and_url, &context).is_ok());

        let sanitizer = DiagnosticSanitizer::new(SanitizerRoots {
            user_profile: Some(r"C:\Users\Alice".into()),
            appdata: None,
            codex_home: None,
        });
        let path_cases = [
            r"D:\Other User\private.txt",
            "//server/share/My Documents/private.txt",
            r"D:\Other\a%b\secret.db",
            r"D:\Other\[draft]\secret.db",
            r"D:\Other\customer,2026.db",
            r"D:\Other\customer;2026.db",
            r"D:\Other\customer)2026.db",
            r"D:\Other\O'Brien\file.db",
            r"C:\Users\Alice Other\private.txt",
            r"C:\Users\Alice,Other\private.txt",
        ];
        let mut pipeline = inputs();
        pipeline.diagnostics_jsonl = path_cases
            .iter()
            .enumerate()
            .map(|(index, path)| {
                let message = sanitizer
                    .sanitize_text(&format!("open failed: {path} | retry-visible-{index}"));
                assert!(message.contains(&format!("retry-visible-{index}")));
                format!(
                    "{}\n",
                    json!({"eventId": format!("event-{index}"), "safeMessage": message})
                )
            })
            .collect::<String>()
            .into_bytes();
        let entries = build_entries(&pipeline).unwrap();
        let diagnostics = String::from_utf8_lossy(&entries[DIAGNOSTICS_NAME]);
        for forbidden in [
            "Other User",
            "My Documents",
            "private.txt",
            "a%b",
            "[draft]",
            "customer,2026",
            "customer;2026",
            "customer)2026",
            "O'Brien",
            "Brien",
            "file.db",
            "Alice Other",
            "Alice,Other",
            "secret.db",
        ] {
            assert!(!diagnostics.contains(forbidden), "{diagnostics}");
        }
        for index in 0..path_cases.len() {
            assert!(
                diagnostics.contains(&format!("retry-visible-{index}")),
                "{diagnostics}"
            );
        }
    }

    #[test]
    fn collisions_never_replace_an_existing_archive() {
        let root = tempdir().unwrap();
        let first = export_to_directory_at(inputs(), root.path(), "20260809-153012-004").unwrap();
        let first_bytes = std::fs::read(&first.path).unwrap();
        let second = export_to_directory_at(inputs(), root.path(), "20260809-153012-004").unwrap();

        assert_eq!(
            second.filename,
            "ChatGPT-Switch-Diagnostics-20260809-153012-004-2.zip"
        );
        assert_eq!(std::fs::read(first.path).unwrap(), first_bytes);
    }

    #[test]
    fn prepared_archive_is_identical_across_destination_retries() {
        let first_root = tempdir().unwrap();
        let second_root = tempdir().unwrap();
        let prepared = prepare_diagnostic_archive(inputs()).unwrap();
        let expected_hash = prepared.sha256().to_string();
        let expected_selection = prepared.selection().clone();

        let first =
            publish_prepared_to_directory_at(&prepared, first_root.path(), "20260809-153012-004")
                .unwrap();
        let second =
            publish_prepared_to_directory_at(&prepared, second_root.path(), "20260809-153012-004")
                .unwrap();

        assert_eq!(first.sha256, expected_hash);
        assert_eq!(second.sha256, expected_hash);
        assert_eq!(first.selection, expected_selection);
        assert_eq!(second.selection, expected_selection);
        assert_eq!(
            std::fs::read(first.path).unwrap(),
            std::fs::read(second.path).unwrap()
        );
    }

    #[test]
    fn post_publish_verification_failure_removes_only_the_published_target() {
        let root = tempdir().unwrap();
        let unrelated = root.path().join("keep.txt");
        std::fs::write(&unrelated, b"keep").unwrap();
        let prepared = prepare_diagnostic_archive(inputs()).unwrap();

        let error = publish_prepared_to_directory_at_with(
            &prepared,
            root.path(),
            "20260809-153012-004",
            |_root, _published| Err("injected post-publish verification failure".to_string()),
        )
        .unwrap_err();

        assert!(error.contains("injected"));
        assert_eq!(std::fs::read(&unrelated).unwrap(), b"keep");
        assert_eq!(
            std::fs::read_dir(root.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().ends_with(".zip"))
                .count(),
            0
        );
    }

    #[test]
    fn truncated_tail_is_dropped_but_internal_corruption_fails_closed() {
        let mut tail = inputs();
        tail.diagnostics_jsonl
            .extend_from_slice(br#"{"truncated":"#);
        let entries = build_entries(&tail).unwrap();
        assert!(String::from_utf8_lossy(&entries[MANIFEST_NAME])
            .contains("diagnosticsTailRecordDropped"));
        let root = tempdir().unwrap();
        let receipt = export_to_directory_at(tail, root.path(), "20260809-153012-004").unwrap();
        assert!(receipt
            .warnings
            .contains(&"diagnosticsTailRecordDropped".to_string()));

        let mut internal = inputs();
        internal.diagnostics_jsonl = b"{}\nnot-json\n{}\n".to_vec();
        assert!(build_entries(&internal).is_err());

        let mut terminated_corruption = inputs();
        terminated_corruption.diagnostics_jsonl = b"{}\nnot-json\n".to_vec();
        assert!(build_entries(&terminated_corruption).is_err());
    }

    #[test]
    fn zip_self_check_rejects_post_write_tampering() {
        let root = tempdir().unwrap();
        let entries = build_entries(&inputs()).unwrap();
        let path = root.path().join("tampered.zip");
        std::fs::write(&path, []).unwrap();
        super::write_zip(&path, &entries).unwrap();
        let mut file = OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(b"NO").unwrap();

        assert!(self_check_archive(&path, &entries, &inputs().redaction).is_err());
    }

    #[test]
    fn forbidden_scan_rejects_secret_shaped_values_after_redaction_boundary() {
        let context = RedactionContext::default();
        let payload = br#"{"safeMessage":"Authorization: Bearer abcdefghijklmnop"}"#;
        assert!(scan_entry(HEALTH_NAME, payload, &context).is_err());
    }

    #[test]
    fn forbidden_scan_checks_parsed_strings_not_only_json_escaping() {
        let context = inputs().redaction;
        let payload = br#"{"safeMessage":"C:\\Users\\alice\\private.txt"}"#;

        assert!(scan_entry(HEALTH_NAME, payload, &context).is_err());
    }

    #[test]
    fn short_identity_literals_do_not_match_inside_static_words() {
        let mut value = inputs();
        value.redaction.forbidden_literals = vec!["win".to_string(), "app".to_string()];

        assert!(build_entries(&value).is_ok());
    }

    #[test]
    fn short_identity_literals_are_redacted_only_at_boundaries() {
        let mut value = inputs();
        value.redaction.forbidden_literals = vec!["Al".to_string(), "PC".to_string()];
        value.diagnostics_jsonl = br#"{"safeMessage":"Al on PC; palette application stays"}
"#
        .to_vec();

        let entries = build_entries(&value).unwrap();
        let line: serde_json::Value = serde_json::from_slice(
            entries[DIAGNOSTICS_NAME]
                .split(|byte| *byte == b'\n')
                .next()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            line["safeMessage"],
            "[REDACTED] on [REDACTED]; palette application stays"
        );
    }

    #[test]
    fn self_check_rejects_unexpected_entry_maps() {
        let root = tempdir().unwrap();
        let entries = build_entries(&inputs()).unwrap();
        let path = root.path().join("diagnostics.zip");
        std::fs::write(&path, []).unwrap();
        super::write_zip(&path, &entries).unwrap();
        let mut wrong = BTreeMap::new();
        wrong.insert("unexpected".to_string(), Vec::new());
        assert!(self_check_archive(&path, &wrong, &inputs().redaction).is_err());
    }
}

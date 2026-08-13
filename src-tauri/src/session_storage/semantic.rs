use std::{
    collections::HashSet,
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    time::SystemTime,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

const MAX_JSONL_LINE_BYTES: usize = 64 * 1024 * 1024;
const MAX_JSONL_ENTRIES: usize = 1_000_000;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SemanticErrorKind {
    Io,
    UnsafePath,
    ChangedDuringRead,
    EmptyEntry,
    OversizedEntry,
    TooManyEntries,
    InvalidJson,
    MissingSessionMeta,
    InvalidSessionMeta,
    InvalidTurnContext,
    InvalidToolRelation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticError {
    pub kind: SemanticErrorKind,
    pub safe_detail: &'static str,
}

impl SemanticError {
    fn new(kind: SemanticErrorKind, safe_detail: &'static str) -> Self {
        Self { kind, safe_detail }
    }

    pub(crate) fn from_kind(kind: SemanticErrorKind) -> Self {
        let safe_detail = match kind {
            SemanticErrorKind::Io => "session file could not be read",
            SemanticErrorKind::UnsafePath => "session path is a link or reparse point",
            SemanticErrorKind::ChangedDuringRead => "session file changed while it was parsed",
            SemanticErrorKind::EmptyEntry => "session file contains an empty entry",
            SemanticErrorKind::OversizedEntry => "session entry exceeded the parser limit",
            SemanticErrorKind::TooManyEntries => "session file exceeded the entry count limit",
            SemanticErrorKind::InvalidJson => "session file contains invalid JSONL",
            SemanticErrorKind::MissingSessionMeta => {
                "session file has no complete session metadata"
            }
            SemanticErrorKind::InvalidSessionMeta => "session metadata is invalid",
            SemanticErrorKind::InvalidTurnContext => "turn provenance metadata is invalid",
            SemanticErrorKind::InvalidToolRelation => "tool calls and results could not be paired",
        };
        Self { kind, safe_detail }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticSession {
    pub path: PathBuf,
    pub thread_id: String,
    pub initial_provider: Option<String>,
    pub bytes: u64,
    pub raw_sha256: [u8; 32],
    pub normalized_line_sha256: Vec<[u8; 32]>,
    pub message_line_sha256: Vec<[u8; 32]>,
    pub message_count: usize,
    pub tool_call_count: usize,
    pub tool_result_count: usize,
    pub last_message_timestamp: Option<String>,
    pub turn_contexts: Vec<TurnContextIdentity>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TurnContextIdentity {
    pub timestamp: String,
    pub turn_id: String,
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileStamp {
    length: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
}

pub fn read_semantic_session(path: &Path) -> Result<SemanticSession, SemanticError> {
    let file = fs::File::open(path).map_err(|_| {
        SemanticError::new(SemanticErrorKind::Io, "session file could not be opened")
    })?;
    let before_handle = file.metadata().map_err(|_| {
        SemanticError::new(
            SemanticErrorKind::Io,
            "session file metadata was unavailable",
        )
    })?;
    let before_path = fs::symlink_metadata(path).map_err(|_| {
        SemanticError::new(
            SemanticErrorKind::Io,
            "session path metadata was unavailable",
        )
    })?;
    if !before_handle.is_file()
        || !before_path.is_file()
        || metadata_is_link_or_reparse(&before_path)
        || file_stamp(&before_handle) != file_stamp(&before_path)
    {
        return Err(SemanticError::new(
            if metadata_is_link_or_reparse(&before_path) {
                SemanticErrorKind::UnsafePath
            } else {
                SemanticErrorKind::ChangedDuringRead
            },
            "session path is unsafe or changed before parsing",
        ));
    }
    let before_stamp = file_stamp(&before_handle);
    let mut reader = BufReader::new(file);
    let mut raw_hasher = Sha256::new();
    let mut normalized_line_sha256 = Vec::new();
    let mut message_line_sha256 = Vec::new();
    let mut raw = Vec::new();
    let mut total_bytes = 0_u64;
    let mut thread_id = None;
    let mut initial_provider = None;
    let mut message_count = 0_usize;
    let mut last_message_timestamp = None;
    let mut turn_contexts = Vec::new();
    let mut calls = HashSet::new();
    let mut results = HashSet::new();

    loop {
        raw.clear();
        let read = (&mut reader)
            .take((MAX_JSONL_LINE_BYTES as u64).saturating_add(1))
            .read_until(b'\n', &mut raw)
            .map_err(|_| {
                SemanticError::new(SemanticErrorKind::Io, "session file could not be read")
            })?;
        if read == 0 {
            break;
        }
        if raw.len() > MAX_JSONL_LINE_BYTES {
            return Err(SemanticError::new(
                SemanticErrorKind::OversizedEntry,
                "session entry exceeded the parser limit",
            ));
        }
        total_bytes = total_bytes
            .checked_add(read as u64)
            .ok_or_else(|| SemanticError::new(SemanticErrorKind::Io, "session size overflowed"))?;
        raw_hasher.update(&raw);
        let body = trim_jsonl_ending(&raw);
        if body.is_empty() {
            return Err(SemanticError::new(
                SemanticErrorKind::EmptyEntry,
                "session file contains an empty entry",
            ));
        }
        let value = serde_json::from_slice::<Value>(body).map_err(|_| {
            SemanticError::new(
                SemanticErrorKind::InvalidJson,
                "session file contains invalid or incomplete JSONL",
            )
        })?;
        let outer_type = value.get("type").and_then(Value::as_str);
        if outer_type == Some("session_meta") {
            let payload = value
                .get("payload")
                .and_then(Value::as_object)
                .ok_or_else(|| {
                    SemanticError::new(
                        SemanticErrorKind::InvalidSessionMeta,
                        "session metadata payload is invalid",
                    )
                })?;
            let id = payload
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .ok_or_else(|| {
                    SemanticError::new(
                        SemanticErrorKind::InvalidSessionMeta,
                        "session metadata has no valid id",
                    )
                })?;
            if thread_id.is_none() {
                thread_id = Some(id.to_string());
                initial_provider = payload
                    .get("model_provider")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
            }
        }

        if is_message(&value) {
            message_count = message_count.saturating_add(1);
            message_line_sha256.push(digest(&normalized_message(&value)?));
            if let Some(timestamp) = value.get("timestamp").and_then(Value::as_str) {
                last_message_timestamp = Some(timestamp.to_string());
            }
        }
        inspect_tool_relation(&value, &mut calls, &mut results)?;
        if let Some(turn_context) = inspect_turn_context(&value)? {
            turn_contexts.push(turn_context);
        }
        if normalized_line_sha256.len() >= MAX_JSONL_ENTRIES {
            return Err(SemanticError::new(
                SemanticErrorKind::TooManyEntries,
                "session file exceeded the entry count limit",
            ));
        }
        normalized_line_sha256.push(digest(&normalized_line(value, body)?));
    }

    let after_handle = reader.get_ref().metadata().map_err(|_| {
        SemanticError::new(SemanticErrorKind::Io, "session file could not be rechecked")
    })?;
    let after_path = fs::symlink_metadata(path).map_err(|_| {
        SemanticError::new(SemanticErrorKind::Io, "session path could not be rechecked")
    })?;
    if metadata_is_link_or_reparse(&after_path)
        || !after_path.is_file()
        || file_stamp(&after_handle) != before_stamp
        || file_stamp(&after_path) != before_stamp
        || total_bytes != before_stamp.length
    {
        return Err(SemanticError::new(
            SemanticErrorKind::ChangedDuringRead,
            "session file changed while it was parsed",
        ));
    }
    if !calls.is_subset(&results) || !results.is_subset(&calls) {
        return Err(SemanticError::new(
            SemanticErrorKind::InvalidToolRelation,
            "tool calls and results could not be paired",
        ));
    }
    let thread_id = thread_id.ok_or_else(|| {
        SemanticError::new(
            SemanticErrorKind::MissingSessionMeta,
            "session file has no complete session metadata",
        )
    })?;
    Ok(SemanticSession {
        path: path.to_path_buf(),
        thread_id,
        initial_provider,
        bytes: total_bytes,
        raw_sha256: raw_hasher.finalize().into(),
        normalized_line_sha256,
        message_line_sha256,
        message_count,
        tool_call_count: calls.len(),
        tool_result_count: results.len(),
        last_message_timestamp,
        turn_contexts,
    })
}

fn inspect_turn_context(value: &Value) -> Result<Option<TurnContextIdentity>, SemanticError> {
    if value.get("type").and_then(Value::as_str) != Some("turn_context") {
        return Ok(None);
    }
    let timestamp = value
        .get("timestamp")
        .and_then(Value::as_str)
        .filter(|timestamp| valid_safe_field(timestamp, 96))
        .ok_or_else(|| {
            SemanticError::new(
                SemanticErrorKind::InvalidTurnContext,
                "turn context timestamp is invalid",
            )
        })?;
    let payload = value
        .get("payload")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            SemanticError::new(
                SemanticErrorKind::InvalidTurnContext,
                "turn context payload is invalid",
            )
        })?;
    let turn_id = payload
        .get("turn_id")
        .and_then(Value::as_str)
        .filter(|turn_id| valid_safe_field(turn_id, 160))
        .ok_or_else(|| {
            SemanticError::new(
                SemanticErrorKind::InvalidTurnContext,
                "turn context id is invalid",
            )
        })?;
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .map(|model| {
            if valid_safe_field(model, 256) {
                Ok(model.to_string())
            } else {
                Err(SemanticError::new(
                    SemanticErrorKind::InvalidTurnContext,
                    "turn context model is invalid",
                ))
            }
        })
        .transpose()?;
    if payload.get("model").is_some() && model.is_none() {
        return Err(SemanticError::new(
            SemanticErrorKind::InvalidTurnContext,
            "turn context model is invalid",
        ));
    }
    Ok(Some(TurnContextIdentity {
        timestamp: timestamp.to_string(),
        turn_id: turn_id.to_string(),
        model,
    }))
}

fn valid_safe_field(value: &str, max_bytes: usize) -> bool {
    !value.is_empty() && value.len() <= max_bytes && !value.chars().any(char::is_control)
}

fn inspect_tool_relation(
    value: &Value,
    calls: &mut HashSet<(PairedToolFamily, String)>,
    results: &mut HashSet<(PairedToolFamily, String)>,
) -> Result<(), SemanticError> {
    if value.get("type").and_then(Value::as_str) != Some("response_item") {
        return Ok(());
    }
    let Some(payload) = value.get("payload").and_then(Value::as_object) else {
        return Ok(());
    };
    let Some(item_type) = payload.get("type").and_then(Value::as_str) else {
        return Ok(());
    };
    let Some((family, is_output)) = paired_tool_item(item_type) else {
        return Ok(());
    };
    let call_id = payload
        .get("call_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            SemanticError::new(
                SemanticErrorKind::InvalidToolRelation,
                "tool item has no valid call id",
            )
        })?;
    let key = (family, call_id.to_string());
    if is_output && !calls.contains(&key) {
        return Err(SemanticError::new(
            SemanticErrorKind::InvalidToolRelation,
            "tool result appeared before its call",
        ));
    }
    let inserted = if is_output {
        results.insert(key)
    } else {
        calls.insert(key)
    };
    if !inserted {
        return Err(SemanticError::new(
            SemanticErrorKind::InvalidToolRelation,
            "tool call relation contains a duplicate id",
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum PairedToolFamily {
    Function,
    Custom,
    Computer,
    LocalShell,
    ApplyPatch,
}

fn paired_tool_item(item_type: &str) -> Option<(PairedToolFamily, bool)> {
    let item = match item_type {
        "function_call" => (PairedToolFamily::Function, false),
        "function_call_output" => (PairedToolFamily::Function, true),
        "custom_tool_call" => (PairedToolFamily::Custom, false),
        "custom_tool_call_output" => (PairedToolFamily::Custom, true),
        "computer_call" => (PairedToolFamily::Computer, false),
        "computer_call_output" => (PairedToolFamily::Computer, true),
        "local_shell_call" => (PairedToolFamily::LocalShell, false),
        "local_shell_call_output" => (PairedToolFamily::LocalShell, true),
        "apply_patch_call" => (PairedToolFamily::ApplyPatch, false),
        "apply_patch_call_output" => (PairedToolFamily::ApplyPatch, true),
        _ => return None,
    };
    Some(item)
}

fn is_message(value: &Value) -> bool {
    let outer = value.get("type").and_then(Value::as_str);
    let inner = value
        .get("payload")
        .and_then(|payload| payload.get("type"))
        .and_then(Value::as_str);
    matches!(
        (outer, inner),
        (Some("response_item"), Some("message" | "agent_message"))
            | (Some("event_msg"), Some("user_message" | "agent_message"))
    )
}

fn normalized_line(mut value: Value, original: &[u8]) -> Result<Vec<u8>, SemanticError> {
    if value.get("type").and_then(Value::as_str) != Some("session_meta") {
        return Ok(original.to_vec());
    }
    if let Some(payload) = value.get_mut("payload").and_then(Value::as_object_mut) {
        payload.remove("model_provider");
    }
    serde_json::to_vec(&value).map_err(|_| {
        SemanticError::new(
            SemanticErrorKind::InvalidJson,
            "session metadata could not be normalized",
        )
    })
}

fn normalized_message(value: &Value) -> Result<Vec<u8>, SemanticError> {
    let mut value = value.clone();
    if let Some(message) = value.as_object_mut() {
        message.remove("timestamp");
    }
    serde_json::to_vec(&value).map_err(|_| {
        SemanticError::new(
            SemanticErrorKind::InvalidJson,
            "session message could not be normalized",
        )
    })
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

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn file_stamp(metadata: &fs::Metadata) -> FileStamp {
    FileStamp {
        length: metadata.len(),
        modified: metadata.modified().ok(),
        created: metadata.created().ok(),
    }
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
    use std::fs;

    use tempfile::tempdir;

    use super::{read_semantic_session, SemanticErrorKind};

    fn write_session(lines: &[serde_json::Value]) -> (tempfile::TempDir, std::path::PathBuf) {
        let root = tempdir().unwrap();
        let path = root.path().join("session.jsonl");
        let body = lines
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .join("\n")
            + "\n";
        fs::write(&path, body).unwrap();
        (root, path)
    }

    #[test]
    fn parses_identity_messages_and_paired_tool_items() {
        let (_root, path) = write_session(&[
            serde_json::json!({"type":"session_meta","timestamp":"2026-08-11T00:00:00Z","payload":{"id":"thread-a","model_provider":"openai"}}),
            serde_json::json!({"type":"turn_context","timestamp":"2026-08-11T00:00:00.500Z","payload":{"turn_id":"turn-a","model":"gpt-test"}}),
            serde_json::json!({"type":"event_msg","timestamp":"2026-08-11T00:00:01Z","payload":{"type":"user_message","message":"hello"}}),
            serde_json::json!({"type":"response_item","timestamp":"2026-08-11T00:00:02Z","payload":{"type":"function_call","call_id":"call-a","name":"tool","arguments":"{}"}}),
            serde_json::json!({"type":"response_item","timestamp":"2026-08-11T00:00:03Z","payload":{"type":"function_call_output","call_id":"call-a","output":"ok"}}),
            serde_json::json!({"type":"response_item","timestamp":"2026-08-11T00:00:04Z","payload":{"type":"message","role":"assistant","content":[]}}),
        ]);

        let parsed = read_semantic_session(&path).unwrap();

        assert_eq!(parsed.thread_id, "thread-a");
        assert_eq!(parsed.initial_provider.as_deref(), Some("openai"));
        assert_eq!(parsed.message_count, 2);
        assert_eq!(parsed.message_line_sha256.len(), 2);
        assert_eq!(parsed.tool_call_count, 1);
        assert_eq!(parsed.tool_result_count, 1);
        assert_eq!(parsed.turn_contexts.len(), 1);
        assert_eq!(parsed.turn_contexts[0].turn_id, "turn-a");
        assert_eq!(parsed.turn_contexts[0].model.as_deref(), Some("gpt-test"));
        assert_eq!(
            parsed.last_message_timestamp.as_deref(),
            Some("2026-08-11T00:00:04Z")
        );
    }

    #[test]
    fn accepts_later_parent_session_metadata_without_changing_identity() {
        let (_root, path) = write_session(&[
            serde_json::json!({"type":"session_meta","payload":{"id":"child","model_provider":"openai"}}),
            serde_json::json!({"type":"session_meta","payload":{"id":"parent","model_provider":"openai"}}),
        ]);

        let parsed = read_semantic_session(&path).unwrap();

        assert_eq!(parsed.thread_id, "child");
    }

    #[test]
    fn accepts_standalone_call_items_that_do_not_have_separate_outputs() {
        let (_root, path) = write_session(&[
            serde_json::json!({"type":"session_meta","payload":{"id":"thread-a"}}),
            serde_json::json!({"type":"response_item","payload":{"type":"web_search_call","id":"search-a","status":"completed"}}),
            serde_json::json!({"type":"response_item","payload":{"type":"mcp_call","id":"mcp-a","name":"tool","output":"ok"}}),
        ]);

        assert!(read_semantic_session(&path).is_ok());
    }

    #[test]
    fn rejects_incomplete_turn_provenance_metadata() {
        let (_root, path) = write_session(&[
            serde_json::json!({"type":"session_meta","payload":{"id":"thread-a"}}),
            serde_json::json!({"type":"turn_context","timestamp":"2026-08-11T00:00:00Z","payload":{"model":"gpt-test"}}),
        ]);

        assert_eq!(
            read_semantic_session(&path).unwrap_err().kind,
            SemanticErrorKind::InvalidTurnContext
        );
    }

    #[test]
    fn rejects_invalid_json_and_unpaired_tool_calls() {
        let root = tempdir().unwrap();
        let invalid = root.path().join("invalid.jsonl");
        fs::write(&invalid, b"{not-json\n").unwrap();
        assert_eq!(
            read_semantic_session(&invalid).unwrap_err().kind,
            SemanticErrorKind::InvalidJson
        );

        let (_root, unpaired) = write_session(&[
            serde_json::json!({"type":"session_meta","payload":{"id":"thread-a"}}),
            serde_json::json!({"type":"response_item","payload":{"type":"custom_tool_call","call_id":"call-a","name":"tool","input":"{}"}}),
        ]);
        assert_eq!(
            read_semantic_session(&unpaired).unwrap_err().kind,
            SemanticErrorKind::InvalidToolRelation
        );

        let (_root, reversed) = write_session(&[
            serde_json::json!({"type":"session_meta","payload":{"id":"thread-a"}}),
            serde_json::json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"call-a","output":"early"}}),
            serde_json::json!({"type":"response_item","payload":{"type":"function_call","call_id":"call-a","name":"tool","arguments":"{}"}}),
        ]);
        assert_eq!(
            read_semantic_session(&reversed).unwrap_err().kind,
            SemanticErrorKind::InvalidToolRelation
        );

        let (_root, mismatched_family) = write_session(&[
            serde_json::json!({"type":"session_meta","payload":{"id":"thread-a"}}),
            serde_json::json!({"type":"response_item","payload":{"type":"function_call","call_id":"call-a","name":"tool","arguments":"{}"}}),
            serde_json::json!({"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call-a","output":"wrong family"}}),
        ]);
        assert_eq!(
            read_semantic_session(&mismatched_family).unwrap_err().kind,
            SemanticErrorKind::InvalidToolRelation
        );
    }
}

use std::{
    fs,
    io::{Read, Take},
    path::{Component, Path, PathBuf},
};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::{
    bounded_file::read_regular_file_bounded, model::MarkerStatus, semantic::SemanticSession,
};

const PROVIDER_SLOT_MARKER_VERSION: u32 = 1;
const PROVIDER_SLOT_MARKER_MAX_BYTES: u64 = 16 * 1024;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProviderSlotMarker {
    version: u32,
    thread_id: String,
    provider_id: Option<String>,
    slot_file_name: String,
    origin_relative_path: Option<PathBuf>,
    origin_provider: Option<String>,
    created_bytes: u64,
    created_sha256: String,
}

pub fn inspect_provider_marker(path: &Path, semantic: Option<&SemanticSession>) -> MarkerStatus {
    let slot_metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(_) => return MarkerStatus::Invalid,
    };
    if !slot_metadata.is_file() || metadata_is_link_or_reparse(&slot_metadata) {
        return MarkerStatus::Invalid;
    }
    let Ok(marker_path) = provider_marker_path(path) else {
        return MarkerStatus::Invalid;
    };
    let metadata = match fs::symlink_metadata(&marker_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return MarkerStatus::Absent,
        Err(_) => return MarkerStatus::Invalid,
    };
    if !metadata.is_file()
        || metadata_is_link_or_reparse(&metadata)
        || metadata.len() == 0
        || metadata.len() > PROVIDER_SLOT_MARKER_MAX_BYTES
    {
        return MarkerStatus::Invalid;
    }
    let encoded = match read_regular_file_bounded(&marker_path, PROVIDER_SLOT_MARKER_MAX_BYTES) {
        Ok(encoded) => encoded,
        Err(_) => return MarkerStatus::Invalid,
    };
    let marker = match serde_json::from_slice::<ProviderSlotMarker>(&encoded) {
        Ok(marker) => marker,
        Err(_) => return MarkerStatus::Invalid,
    };
    let Some(semantic) = semantic else {
        return MarkerStatus::Invalid;
    };
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return MarkerStatus::Invalid;
    };
    if marker.version != PROVIDER_SLOT_MARKER_VERSION
        || marker.thread_id != semantic.thread_id
        || marker.provider_id.as_deref() != semantic.initial_provider.as_deref()
        || marker.slot_file_name != file_name
        || marker.created_bytes == 0
        || marker.created_bytes > semantic.bytes
        || marker.created_sha256.len() != 64
        || !marker
            .created_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || marker
            .origin_relative_path
            .as_deref()
            .is_some_and(|path| !is_safe_relative_path(path))
        || marker
            .provider_id
            .as_deref()
            .is_some_and(|provider| provider.trim().is_empty())
        || marker
            .origin_provider
            .as_deref()
            .is_some_and(|provider| provider.trim().is_empty())
    {
        return MarkerStatus::Invalid;
    }
    match sha256_prefix(path, marker.created_bytes) {
        Ok(hash) if hash == marker.created_sha256.to_ascii_lowercase() => MarkerStatus::Valid,
        _ => MarkerStatus::Invalid,
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

pub(crate) fn provider_marker_path(slot: &Path) -> Result<PathBuf, ()> {
    let file_name = slot.file_name().and_then(|name| name.to_str()).ok_or(())?;
    Ok(slot.with_file_name(format!(
        ".{file_name}.codex-switch-slot-v{PROVIDER_SLOT_MARKER_VERSION}.json"
    )))
}

fn is_safe_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn sha256_prefix(path: &Path, bytes: u64) -> Result<String, ()> {
    let file = fs::File::open(path).map_err(|_| ())?;
    let mut reader: Take<fs::File> = file.take(bytes);
    let mut hasher = Sha256::new();
    let copied = std::io::copy(&mut reader, &mut hasher).map_err(|_| ())?;
    if copied != bytes {
        return Err(());
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use sha2::{Digest, Sha256};
    use tempfile::tempdir;

    use super::{inspect_provider_marker, provider_marker_path};
    use crate::session_storage::{model::MarkerStatus, semantic::read_semantic_session};

    #[test]
    fn validates_bound_prefix_and_rejects_tampering() {
        let root = tempdir().unwrap();
        let path = root.path().join("rollout-thread-a.jsonl");
        let body = b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"thread-a\",\"model_provider\":\"openai\"}}\n";
        fs::write(&path, body).unwrap();
        let marker = serde_json::json!({
            "version": 1,
            "threadId": "thread-a",
            "providerId": "openai",
            "slotFileName": "rollout-thread-a.jsonl",
            "originRelativePath": null,
            "originProvider": null,
            "createdBytes": body.len(),
            "createdSha256": format!("{:x}", Sha256::digest(body)),
        });
        fs::write(
            provider_marker_path(&path).unwrap(),
            serde_json::to_vec(&marker).unwrap(),
        )
        .unwrap();
        let semantic = read_semantic_session(&path).unwrap();

        assert_eq!(
            inspect_provider_marker(&path, Some(&semantic)),
            MarkerStatus::Valid
        );

        let mut forged = marker.clone();
        forged["threadId"] = serde_json::Value::String("thread-b".to_string());
        fs::write(
            provider_marker_path(&path).unwrap(),
            serde_json::to_vec(&forged).unwrap(),
        )
        .unwrap();
        assert_eq!(
            inspect_provider_marker(&path, Some(&semantic)),
            MarkerStatus::Invalid
        );
        let mut forged = marker.clone();
        forged["providerId"] = serde_json::Value::String("openai_custom".to_string());
        fs::write(
            provider_marker_path(&path).unwrap(),
            serde_json::to_vec(&forged).unwrap(),
        )
        .unwrap();
        assert_eq!(
            inspect_provider_marker(&path, Some(&semantic)),
            MarkerStatus::Invalid
        );
        fs::write(
            provider_marker_path(&path).unwrap(),
            serde_json::to_vec(&marker).unwrap(),
        )
        .unwrap();

        let mut changed = body.to_vec();
        changed[5] ^= 1;
        fs::write(&path, changed).unwrap();
        assert_eq!(
            inspect_provider_marker(&path, Some(&semantic)),
            MarkerStatus::Invalid
        );
    }
}

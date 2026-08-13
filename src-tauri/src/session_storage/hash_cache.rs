use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    bounded_file::read_regular_file_bounded,
    marker::{inspect_provider_marker, provider_marker_path},
    model::{FileObservation, FileObservationState, MarkerStatus},
    reference_graph::path_key,
    semantic::{
        read_semantic_session, SemanticError, SemanticErrorKind, SemanticSession,
        TurnContextIdentity,
    },
};
use crate::{file_ops::atomic_write, operation_log::timestamp_millis};

const CACHE_SCHEMA_VERSION: u32 = 1;
const PARSER_SCHEMA_VERSION: u32 = 6;
const MAX_CACHE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_CACHE_ENTRIES: usize = 50_000;
const MAX_CACHED_HASH_BYTES_PER_FILE: usize = 32 * 1_000_000;
const MAX_CACHED_TURN_CONTEXT_BYTES_PER_FILE: usize = 16 * 1024 * 1024;
const FINGERPRINT_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub hits: usize,
    pub misses: usize,
    pub stable_files: usize,
}

#[derive(Debug, Clone)]
pub struct CachedFileScan {
    pub semantic: Result<SemanticSession, SemanticError>,
    pub marker_status: MarkerStatus,
    pub observation: FileObservation,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CacheFile {
    schema_version: u32,
    parser_schema_version: u32,
    entries: BTreeMap<String, CacheEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CacheEntry {
    signature: FileSignature,
    semantic: CachedSemantic,
    marker_signature: Option<FileSignature>,
    marker_status: MarkerStatus,
    stable_observations: u32,
    last_seen_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FileSignature {
    bytes: u64,
    modified_ns: Option<u128>,
    created_ns: Option<u128>,
    platform_file_id: Option<String>,
    quick_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "camelCase", deny_unknown_fields)]
enum CachedSemantic {
    Valid { value: Box<CachedValidSemantic> },
    Invalid { kind: SemanticErrorKind },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CachedValidSemantic {
    thread_id: String,
    initial_provider: Option<String>,
    bytes: u64,
    raw_sha256: String,
    normalized_line_count: usize,
    normalized_line_sha256: String,
    message_line_count: usize,
    message_line_sha256: String,
    message_count: usize,
    tool_call_count: usize,
    tool_result_count: usize,
    last_message_timestamp: Option<String>,
    turn_contexts: Vec<TurnContextIdentity>,
}

#[derive(Debug, Clone)]
pub struct HashCache {
    path: PathBuf,
    entries: BTreeMap<String, CacheEntry>,
    seen: BTreeSet<String>,
    stats: CacheStats,
}

impl HashCache {
    pub fn load(data_root: &Path) -> Result<Self, String> {
        let path = cache_path(data_root);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::empty(path))
            }
            Err(_) => return Err("session hash cache metadata is unavailable".to_string()),
        };
        if !metadata.is_file()
            || metadata_is_link_or_reparse(&metadata)
            || metadata.len() == 0
            || metadata.len() > MAX_CACHE_BYTES
        {
            return Err("session hash cache is invalid".to_string());
        }
        let encoded = read_regular_file_bounded(&path, MAX_CACHE_BYTES)
            .map_err(|_| "session hash cache is unreadable".to_string())?;
        let cache = serde_json::from_slice::<CacheFile>(&encoded)
            .map_err(|_| "session hash cache is invalid".to_string())?;
        if cache.schema_version != CACHE_SCHEMA_VERSION
            || cache.parser_schema_version != PARSER_SCHEMA_VERSION
            || cache.entries.len() > MAX_CACHE_ENTRIES
        {
            return Err("session hash cache has an unsupported contract".to_string());
        }
        Ok(Self {
            path,
            entries: cache.entries,
            seen: BTreeSet::new(),
            stats: CacheStats::default(),
        })
    }

    pub fn empty_for(data_root: &Path) -> Self {
        Self::empty(cache_path(data_root))
    }

    pub fn scan_file(&mut self, path: &Path) -> CachedFileScan {
        let key = cache_key(path);
        self.seen.insert(key.clone());
        let before = file_signature(path).ok();
        let cached_semantic = before.as_ref().and_then(|signature| {
            self.entries
                .get(&key)
                .filter(|entry| entry.signature == *signature)
                .and_then(|entry| decode_semantic(path, &entry.semantic).ok())
        });
        let semantic = if let Some(semantic) = cached_semantic {
            self.stats.hits = self.stats.hits.saturating_add(1);
            semantic
        } else {
            self.stats.misses = self.stats.misses.saturating_add(1);
            read_semantic_session(path)
        };
        let after = file_signature(path).ok();
        let signature_stable = before.is_some() && before == after;
        let marker_signature_result = provider_marker_path(path)
            .map_err(|_| ())
            .and_then(|marker_path| marker_signature_for_cache(&marker_path));
        let marker_signature = marker_signature_result.clone().unwrap_or(None);
        let cached_marker = after.as_ref().and_then(|signature| {
            marker_signature_result
                .as_ref()
                .ok()
                .and_then(|marker_signature| {
                    self.entries.get(&key).filter(|entry| {
                        entry.signature == *signature
                            && entry.marker_signature.as_ref() == marker_signature.as_ref()
                    })
                })
        });
        let marker_status = cached_marker.map_or_else(
            || inspect_provider_marker(path, semantic.as_ref().ok()),
            |entry| entry.marker_status,
        );
        let mut stable_observations = 0_u32;
        if signature_stable {
            stable_observations = self
                .entries
                .get(&key)
                .filter(|entry| entry.signature == *after.as_ref().expect("stable signature"))
                .map_or(1, |entry| entry.stable_observations.saturating_add(1));
            if stable_observations >= 2 {
                self.stats.stable_files = self.stats.stable_files.saturating_add(1);
            }
            if let (Some(signature), Ok(cached)) =
                (after.clone(), encode_semantic(&semantic).map_err(|_| ()))
            {
                self.entries.insert(
                    key,
                    CacheEntry {
                        signature,
                        semantic: cached,
                        marker_signature,
                        marker_status,
                        stable_observations,
                        last_seen_at_ms: current_timestamp(),
                    },
                );
            }
        }
        CachedFileScan {
            observation: FileObservation {
                state: if semantic
                    .as_ref()
                    .is_err_and(|error| error.kind == SemanticErrorKind::ChangedDuringRead)
                    || ((before.is_some() || after.is_some()) && before != after)
                {
                    FileObservationState::ChangedDuringScan
                } else if signature_stable {
                    FileObservationState::Stable
                } else {
                    FileObservationState::Unknown
                },
                stable_observations,
                observed_bytes: after
                    .as_ref()
                    .or(before.as_ref())
                    .map(|signature| signature.bytes),
                last_verified_at_ms: current_timestamp(),
            },
            semantic,
            marker_status,
        }
    }

    pub fn stats(&self) -> CacheStats {
        self.stats.clone()
    }

    pub fn save(mut self) -> Result<(), String> {
        self.entries.retain(|key, _| self.seen.contains(key));
        if self.entries.len() > MAX_CACHE_ENTRIES {
            return Err("session hash cache entry limit was exceeded".to_string());
        }
        let encoded = serde_json::to_vec(&CacheFile {
            schema_version: CACHE_SCHEMA_VERSION,
            parser_schema_version: PARSER_SCHEMA_VERSION,
            entries: self.entries,
        })
        .map_err(|_| "failed to serialize the session hash cache".to_string())?;
        if encoded.len() as u64 > MAX_CACHE_BYTES {
            return Err("session hash cache size limit was exceeded".to_string());
        }
        atomic_write(&self.path, &encoded)
            .map_err(|_| "failed to persist the session hash cache".to_string())
    }

    fn empty(path: PathBuf) -> Self {
        Self {
            path,
            entries: BTreeMap::new(),
            seen: BTreeSet::new(),
            stats: CacheStats::default(),
        }
    }
}

fn encode_semantic(
    semantic: &Result<SemanticSession, SemanticError>,
) -> Result<CachedSemantic, String> {
    match semantic {
        Ok(semantic) => {
            let hash_bytes = semantic
                .normalized_line_sha256
                .len()
                .checked_mul(32)
                .ok_or_else(|| "session hash count overflowed".to_string())?;
            if hash_bytes > MAX_CACHED_HASH_BYTES_PER_FILE {
                return Err("session hash cache entry is too large".to_string());
            }
            let message_hash_bytes = semantic
                .message_line_sha256
                .len()
                .checked_mul(32)
                .ok_or_else(|| "session message hash count overflowed".to_string())?;
            if message_hash_bytes > MAX_CACHED_HASH_BYTES_PER_FILE {
                return Err("session message hash cache entry is too large".to_string());
            }
            let turn_context_bytes =
                semantic
                    .turn_contexts
                    .iter()
                    .try_fold(0_usize, |total, context| {
                        total
                            .checked_add(context.timestamp.len())
                            .and_then(|value| value.checked_add(context.turn_id.len()))
                            .and_then(|value| {
                                value.checked_add(context.model.as_ref().map_or(0, String::len))
                            })
                    });
            if turn_context_bytes.is_none_or(|bytes| bytes > MAX_CACHED_TURN_CONTEXT_BYTES_PER_FILE)
            {
                return Err("session provenance cache entry is too large".to_string());
            }
            let mut hashes = Vec::with_capacity(hash_bytes);
            for hash in &semantic.normalized_line_sha256 {
                hashes.extend_from_slice(hash);
            }
            let mut message_hashes = Vec::with_capacity(message_hash_bytes);
            for hash in &semantic.message_line_sha256 {
                message_hashes.extend_from_slice(hash);
            }
            Ok(CachedSemantic::Valid {
                value: Box::new(CachedValidSemantic {
                    thread_id: semantic.thread_id.clone(),
                    initial_provider: semantic.initial_provider.clone(),
                    bytes: semantic.bytes,
                    raw_sha256: BASE64.encode(semantic.raw_sha256),
                    normalized_line_count: semantic.normalized_line_sha256.len(),
                    normalized_line_sha256: BASE64.encode(hashes),
                    message_line_count: semantic.message_line_sha256.len(),
                    message_line_sha256: BASE64.encode(message_hashes),
                    message_count: semantic.message_count,
                    tool_call_count: semantic.tool_call_count,
                    tool_result_count: semantic.tool_result_count,
                    last_message_timestamp: semantic.last_message_timestamp.clone(),
                    turn_contexts: semantic.turn_contexts.clone(),
                }),
            })
        }
        Err(error) => Ok(CachedSemantic::Invalid { kind: error.kind }),
    }
}

fn decode_semantic(
    path: &Path,
    cached: &CachedSemantic,
) -> Result<Result<SemanticSession, SemanticError>, String> {
    match cached {
        CachedSemantic::Invalid { kind } => Ok(Err(SemanticError::from_kind(*kind))),
        CachedSemantic::Valid { value } => {
            let CachedValidSemantic {
                thread_id,
                initial_provider,
                bytes,
                raw_sha256,
                normalized_line_count,
                normalized_line_sha256,
                message_line_count,
                message_line_sha256,
                message_count,
                tool_call_count,
                tool_result_count,
                last_message_timestamp,
                turn_contexts,
            } = value.as_ref();
            let raw = BASE64
                .decode(raw_sha256)
                .map_err(|_| "cached session hash is invalid".to_string())?;
            let raw_sha256: [u8; 32] = raw
                .try_into()
                .map_err(|_| "cached session hash has the wrong length".to_string())?;
            let hashes = BASE64
                .decode(normalized_line_sha256)
                .map_err(|_| "cached session line hashes are invalid".to_string())?;
            if hashes.len() != normalized_line_count.saturating_mul(32)
                || hashes.len() > MAX_CACHED_HASH_BYTES_PER_FILE
            {
                return Err("cached session line hashes have the wrong length".to_string());
            }
            let normalized_line_sha256 = hashes
                .chunks_exact(32)
                .map(|chunk| {
                    chunk
                        .try_into()
                        .expect("chunks_exact always returns 32-byte chunks")
                })
                .collect();
            let message_hashes = BASE64
                .decode(message_line_sha256)
                .map_err(|_| "cached session message hashes are invalid".to_string())?;
            if message_hashes.len() != message_line_count.saturating_mul(32)
                || message_hashes.len() > MAX_CACHED_HASH_BYTES_PER_FILE
                || *message_line_count != *message_count
            {
                return Err("cached session message hashes have the wrong length".to_string());
            }
            let message_line_sha256 = message_hashes
                .chunks_exact(32)
                .map(|chunk| {
                    chunk
                        .try_into()
                        .expect("chunks_exact always returns 32-byte chunks")
                })
                .collect();
            Ok(Ok(SemanticSession {
                path: path.to_path_buf(),
                thread_id: thread_id.clone(),
                initial_provider: initial_provider.clone(),
                bytes: *bytes,
                raw_sha256,
                normalized_line_sha256,
                message_line_sha256,
                message_count: *message_count,
                tool_call_count: *tool_call_count,
                tool_result_count: *tool_result_count,
                last_message_timestamp: last_message_timestamp.clone(),
                turn_contexts: turn_contexts.clone(),
            }))
        }
    }
}

fn file_signature(path: &Path) -> Result<FileSignature, String> {
    let before =
        fs::symlink_metadata(path).map_err(|_| "file metadata is unavailable".to_string())?;
    if !before.is_file() || metadata_is_link_or_reparse(&before) {
        return Err("cache candidate is not a file".to_string());
    }
    let before_file_id = platform_file_id(path);
    let quick_sha256 = quick_fingerprint(path, before.len())?;
    let after =
        fs::symlink_metadata(path).map_err(|_| "file metadata is unavailable".to_string())?;
    if !after.is_file() || metadata_is_link_or_reparse(&after) {
        return Err("cache candidate changed file type".to_string());
    }
    let after_file_id = platform_file_id(path);
    if metadata_stamp(&before) != metadata_stamp(&after) || before_file_id != after_file_id {
        return Err("file changed while its cache signature was read".to_string());
    }
    Ok(FileSignature {
        bytes: after.len(),
        modified_ns: system_time_ns(after.modified().ok()),
        created_ns: system_time_ns(after.created().ok()),
        platform_file_id: after_file_id,
        quick_sha256,
    })
}

fn marker_signature_for_cache(path: &Path) -> Result<Option<FileSignature>, ()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(()),
    };
    if !metadata.is_file() || metadata_is_link_or_reparse(&metadata) {
        return Err(());
    }
    file_signature(path).map(Some).map_err(|_| ())
}

fn quick_fingerprint(path: &Path, bytes: u64) -> Result<String, String> {
    let mut file =
        fs::File::open(path).map_err(|_| "file could not be fingerprinted".to_string())?;
    let mut hasher = Sha256::new();
    hasher.update(bytes.to_le_bytes());
    let first_len = usize::try_from(bytes.min(FINGERPRINT_CHUNK_BYTES as u64))
        .map_err(|_| "file fingerprint length overflowed".to_string())?;
    let mut first = vec![0_u8; first_len];
    file.read_exact(&mut first)
        .map_err(|_| "file prefix could not be fingerprinted".to_string())?;
    hasher.update(&first);
    if bytes > FINGERPRINT_CHUNK_BYTES as u64 {
        let tail_len = usize::try_from(bytes.min(FINGERPRINT_CHUNK_BYTES as u64))
            .map_err(|_| "file fingerprint length overflowed".to_string())?;
        file.seek(SeekFrom::Start(bytes.saturating_sub(tail_len as u64)))
            .map_err(|_| "file suffix could not be fingerprinted".to_string())?;
        let mut tail = vec![0_u8; tail_len];
        file.read_exact(&mut tail)
            .map_err(|_| "file suffix could not be fingerprinted".to_string())?;
        hasher.update(&tail);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn metadata_stamp(metadata: &fs::Metadata) -> (u64, Option<u128>, Option<u128>) {
    (
        metadata.len(),
        system_time_ns(metadata.modified().ok()),
        system_time_ns(metadata.created().ok()),
    )
}

fn system_time_ns(value: Option<SystemTime>) -> Option<u128> {
    value?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|value| value.as_nanos())
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

#[cfg(windows)]
fn platform_file_id(path: &Path) -> Option<String> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION},
    };

    let file = fs::File::open(path).ok()?;
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    let result =
        unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut information) };
    if result == 0 {
        return None;
    }
    let file_index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
    Some(format!(
        "{:08x}:{file_index:016x}",
        information.dwVolumeSerialNumber
    ))
}

#[cfg(not(windows))]
fn platform_file_id(_path: &Path) -> Option<String> {
    None
}

fn cache_key(path: &Path) -> String {
    format!("{:x}", Sha256::digest(path_key(path).as_bytes()))
}

fn cache_path(data_root: &Path) -> PathBuf {
    data_root.join("session-storage-v1/hash-cache.json")
}

fn current_timestamp() -> u64 {
    timestamp_millis()
        .ok()
        .and_then(|value| u64::try_from(value).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{cache_path, HashCache};
    use crate::session_storage::{
        marker::provider_marker_path,
        model::{FileObservationState, MarkerStatus},
    };

    #[test]
    fn reuses_stable_semantics_without_persisting_full_paths() {
        let root = tempdir().unwrap();
        let data = root.path().join("data");
        let path = root.path().join("private/sessions/session.jsonl");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"thread-a\",\"model_provider\":\"openai\"}}\n",
        )
        .unwrap();

        let mut first = HashCache::load(&data).unwrap();
        let first_scan = first.scan_file(&path);
        assert!(first_scan.semantic.is_ok());
        assert_eq!(first_scan.observation.state, FileObservationState::Stable);
        assert_eq!(first_scan.observation.stable_observations, 1);
        assert_eq!(first.stats().misses, 1);
        first.save().unwrap();

        let encoded = fs::read_to_string(cache_path(&data)).unwrap();
        assert!(!encoded.contains(&path.to_string_lossy().to_string()));

        let mut second = HashCache::load(&data).unwrap();
        let second_scan = second.scan_file(&path);
        assert!(second_scan.semantic.is_ok());
        assert_eq!(second_scan.observation.state, FileObservationState::Stable);
        assert_eq!(second_scan.observation.stable_observations, 2);
        assert_eq!(second.stats().hits, 1);
        assert_eq!(second.stats().stable_files, 1);
    }

    #[test]
    fn same_size_content_change_invalidates_the_cache() {
        let root = tempdir().unwrap();
        let data = root.path().join("data");
        let path = root.path().join("session.jsonl");
        let first_body = "{\"type\":\"session_meta\",\"payload\":{\"id\":\"thread-a\"}}\n";
        let second_body = "{\"type\":\"session_meta\",\"payload\":{\"id\":\"thread-b\"}}\n";
        assert_eq!(first_body.len(), second_body.len());
        fs::write(&path, first_body).unwrap();
        let mut first = HashCache::load(&data).unwrap();
        first.scan_file(&path);
        first.save().unwrap();

        fs::write(&path, second_body).unwrap();
        let mut second = HashCache::load(&data).unwrap();
        let scanned = second.scan_file(&path);

        assert_eq!(second.stats().hits, 0);
        assert_eq!(second.stats().misses, 1);
        assert_eq!(scanned.semantic.unwrap().thread_id, "thread-b");
    }

    #[test]
    fn an_invalid_marker_cannot_reuse_a_cached_absent_marker_result() {
        let root = tempdir().unwrap();
        let data = root.path().join("data");
        let path = root.path().join("session.jsonl");
        fs::write(
            &path,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"thread-a\"}}\n",
        )
        .unwrap();
        let mut first = HashCache::load(&data).unwrap();
        assert_eq!(first.scan_file(&path).marker_status, MarkerStatus::Absent);
        first.save().unwrap();

        fs::create_dir(provider_marker_path(&path).unwrap()).unwrap();
        let mut second = HashCache::load(&data).unwrap();
        let scanned = second.scan_file(&path);

        assert_eq!(second.stats().hits, 1);
        assert_eq!(scanned.marker_status, MarkerStatus::Invalid);
    }
}

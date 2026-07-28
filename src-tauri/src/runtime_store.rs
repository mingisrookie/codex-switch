use std::{
    fs,
    net::IpAddr,
    path::{Path, PathBuf},
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};
use toml_edit::{value, DocumentMut, Item, Table};

use crate::{
    config_patch::MANAGED_RELAY_PROVIDER_ID,
    crypto::{protect, unprotect},
    file_ops::{atomic_copy, atomic_write},
};

pub const PLUS_RUNTIME_ID: &str = "plus";
pub const RELAY_RUNTIME_ID: &str = "relay";
const RUNTIME_BUNDLE_MANIFEST: &str = "bundle.json";
const RUNTIME_BUNDLE_MARKER: &str = ".bundle-v1";
const RUNTIME_BUNDLE_MARKER_BYTES: &[u8] = b"codex-switch-runtime-bundle-v1\n";
const RUNTIME_BUNDLE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RuntimeKind {
    Plus,
    Relay,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RuntimeConfidence {
    Exact,
    Mode,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeStatus {
    pub active_runtime_id: Option<String>,
    pub confidence: RuntimeConfidence,
    pub auth_mode: Option<String>,
    pub model_provider: Option<String>,
    pub detected_at_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeMetadata {
    pub id: String,
    pub name: String,
    pub kind: RuntimeKind,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub created_at_ms: u128,
    #[serde(default)]
    pub last_used_at_ms: Option<u128>,
    #[serde(default)]
    pub last_verified_at_ms: Option<u128>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RelayRuntimeInput {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeFiles {
    pub auth_json: Vec<u8>,
    pub config_toml: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayConnection {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct RuntimeBundleManifest {
    version: u32,
    auth_sha256: String,
    config_sha256: String,
    metadata_sha256: String,
}

#[derive(Debug, Clone)]
pub struct RuntimeStore {
    root: PathBuf,
}

impl RuntimeStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn from_default_root() -> Result<Self, String> {
        Ok(Self::new(default_store_root()?.join("runtimes")))
    }

    pub fn runtime_dir(&self, runtime_id: &str) -> PathBuf {
        self.root.join(runtime_id)
    }

    pub fn list_runtimes(&self) -> Result<Vec<RuntimeMetadata>, String> {
        let mut runtimes = Vec::new();
        for id in [PLUS_RUNTIME_ID, RELAY_RUNTIME_ID] {
            if let Some(metadata) = self.load_existing_metadata(id)? {
                runtimes.push(metadata);
            }
        }
        Ok(runtimes)
    }

    pub fn import_plus_from_home(
        &self,
        codex_home: &Path,
        confirm_overwrite: bool,
    ) -> Result<RuntimeMetadata, String> {
        let auth = fs::read(codex_home.join("auth.json"))
            .map_err(|error| format!("failed to read plus auth.json: {error}"))?;
        if auth_mode_from_bytes(&auth)?.as_deref() != Some("chatgpt") {
            return Err("当前 auth.json 不是 ChatGPT 账号登录态，不能保存到账号槽位".to_string());
        }
        let config = fs::read_to_string(codex_home.join("config.toml"))
            .map_err(|error| format!("failed to read plus config.toml: {error}"))?;
        if config_string(&config, "model_provider")?.as_deref() == Some(MANAGED_RELAY_PROVIDER_ID) {
            return Err(
                "当前请求端是 API Relay；请先切回 OpenAI 官方请求端，再更新账号配置".to_string(),
            );
        }
        let config_overlay = account_config_overlay(&config)?;
        let existing = self.load_existing_metadata(PLUS_RUNTIME_ID)?;
        if existing.is_some() && !confirm_overwrite {
            return Err("账号槽位已存在，请确认覆盖后重试".to_string());
        }
        if existing.is_some() {
            self.archive_runtime(PLUS_RUNTIME_ID)?;
        }
        let created_at_ms = existing
            .as_ref()
            .map(|metadata| metadata.created_at_ms)
            .unwrap_or(timestamp_millis()?);
        let metadata = RuntimeMetadata {
            id: PLUS_RUNTIME_ID.to_string(),
            name: "ChatGPT 账号".to_string(),
            kind: RuntimeKind::Plus,
            base_url: None,
            model: read_model_from_config(&config),
            created_at_ms,
            last_used_at_ms: None,
            last_verified_at_ms: Some(timestamp_millis()?),
        };
        self.write_runtime(&metadata, &auth, &config_overlay)?;
        Ok(metadata)
    }

    pub fn upsert_relay(
        &self,
        input: RelayRuntimeInput,
        codex_home: &Path,
    ) -> Result<RuntimeMetadata, String> {
        let normalized_base_url = normalize_base_url(&input.base_url)?;
        let model = input.model.trim();
        if model.is_empty() {
            return Err("relay model is required".to_string());
        }
        let existing = self.load_existing_metadata(RELAY_RUNTIME_ID)?;
        if input.api_key.trim().is_empty() {
            let existing_base_url = existing
                .as_ref()
                .and_then(|metadata| metadata.base_url.as_deref())
                .ok_or_else(|| "relay API key is required for the first save".to_string())?;
            if normalized_url_origin(existing_base_url)?
                != normalized_url_origin(&normalized_base_url)?
            {
                return Err("relay API key is required when changing the relay origin".to_string());
            }
        }
        let base_config = fs::read_to_string(codex_home.join("config.toml"))
            .map_err(|error| format!("failed to read live config.toml: {error}"))?;
        let config_toml = relay_config_template(&base_config, &normalized_base_url, model)?;
        let auth = if input.api_key.trim().is_empty() {
            self.load_runtime_files(RELAY_RUNTIME_ID)
                .map_err(|_| {
                    "stored relay credentials are invalid; enter a new API key".to_string()
                })?
                .auth_json
        } else {
            relay_auth_json(&input.api_key)?
        };
        if !valid_relay_auth(&auth) {
            return Err("stored relay credentials are invalid".to_string());
        }
        let created_at_ms = existing
            .as_ref()
            .map(|metadata| metadata.created_at_ms)
            .unwrap_or(timestamp_millis()?);
        let metadata = RuntimeMetadata {
            id: RELAY_RUNTIME_ID.to_string(),
            name: "API 中转站".to_string(),
            kind: RuntimeKind::Relay,
            base_url: Some(normalized_base_url),
            model: Some(model.to_string()),
            created_at_ms,
            last_used_at_ms: existing
                .as_ref()
                .and_then(|metadata| metadata.last_used_at_ms),
            last_verified_at_ms: None,
        };
        if existing.is_some() {
            self.archive_runtime(RELAY_RUNTIME_ID)?;
        }
        self.write_runtime(&metadata, &auth, &config_toml)?;
        Ok(metadata)
    }

    pub fn load_runtime_files(&self, runtime_id: &str) -> Result<RuntimeFiles, String> {
        self.load_runtime_bundle(runtime_id).map(|(_, files)| files)
    }

    pub fn load_metadata(&self, runtime_id: &str) -> Result<RuntimeMetadata, String> {
        validate_runtime_id(runtime_id)?;
        let metadata = read_metadata(&self.runtime_dir(runtime_id).join("runtime.json"))?;
        validate_runtime_metadata(runtime_id, &metadata)?;
        Ok(metadata)
    }

    fn load_existing_metadata(&self, runtime_id: &str) -> Result<Option<RuntimeMetadata>, String> {
        if !self.runtime_dir(runtime_id).exists() {
            return Ok(None);
        }
        self.load_metadata(runtime_id).map(Some)
    }

    pub fn detect_active_runtime(&self, codex_home: &Path) -> Result<RuntimeStatus, String> {
        let live_auth = fs::read(codex_home.join("auth.json"))
            .map_err(|error| format!("failed to read live auth.json: {error}"))?;
        let live_config = fs::read_to_string(codex_home.join("config.toml"))
            .map_err(|error| format!("failed to read live config.toml: {error}"))?;
        let auth_mode = auth_mode_from_bytes(&live_auth)?;
        let model_provider = config_string(&live_config, "model_provider")?;

        if auth_mode.as_deref() == Some("chatgpt") {
            for runtime in self.list_runtimes()? {
                let files = self.load_runtime_files(&runtime.id)?;
                if runtime_binding_matches(
                    &runtime,
                    &files.auth_json,
                    &files.config_toml,
                    &live_config,
                )? {
                    return Ok(RuntimeStatus {
                        active_runtime_id: Some(runtime.id),
                        confidence: RuntimeConfidence::Exact,
                        auth_mode,
                        model_provider,
                        detected_at_ms: timestamp_millis()?,
                    });
                }
            }
        }

        let active_runtime_id = match (auth_mode.as_deref(), model_provider.as_deref()) {
            (Some("chatgpt"), provider) if provider != Some(MANAGED_RELAY_PROVIDER_ID) => {
                Some(PLUS_RUNTIME_ID.to_string())
            }
            (Some("chatgpt"), Some(MANAGED_RELAY_PROVIDER_ID)) => {
                Some(RELAY_RUNTIME_ID.to_string())
            }
            _ => None,
        };
        let confidence = if active_runtime_id.is_some() {
            RuntimeConfidence::Mode
        } else {
            RuntimeConfidence::Unknown
        };
        Ok(RuntimeStatus {
            active_runtime_id,
            confidence,
            auth_mode,
            model_provider,
            detected_at_ms: timestamp_millis()?,
        })
    }

    pub fn mark_used(&self, runtime_id: &str) -> Result<RuntimeMetadata, String> {
        let (mut metadata, _) = self.load_runtime_bundle(runtime_id)?;
        let now = timestamp_millis()?;
        metadata.last_used_at_ms = Some(now);
        metadata.last_verified_at_ms = Some(now);
        self.write_metadata_and_bundle(runtime_id, &metadata)?;
        Ok(metadata)
    }

    pub fn mark_verified(&self, runtime_id: &str) -> Result<RuntimeMetadata, String> {
        let (mut metadata, _) = self.load_runtime_bundle(runtime_id)?;
        metadata.last_verified_at_ms = Some(timestamp_millis()?);
        self.write_metadata_and_bundle(runtime_id, &metadata)?;
        Ok(metadata)
    }

    pub fn load_relay_connection(&self) -> Result<RelayConnection, String> {
        let (_, files) = self.load_runtime_bundle(RELAY_RUNTIME_ID)?;
        let base_url =
            provider_config_string(&files.config_toml, MANAGED_RELAY_PROVIDER_ID, "base_url")?
                .ok_or_else(|| "relay base URL is missing".to_string())?;
        let model = config_string(&files.config_toml, "model")?
            .filter(|model| !model.trim().is_empty())
            .ok_or_else(|| "relay model is missing".to_string())?;
        let auth = serde_json::from_slice::<JsonValue>(&files.auth_json)
            .map_err(|error| format!("failed to parse stored relay auth: {error}"))?;
        let api_key = auth
            .get("OPENAI_API_KEY")
            .and_then(JsonValue::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| "stored relay API key is missing".to_string())?
            .to_string();
        Ok(RelayConnection {
            base_url,
            api_key,
            model,
        })
    }

    fn load_runtime_bundle(
        &self,
        runtime_id: &str,
    ) -> Result<(RuntimeMetadata, RuntimeFiles), String> {
        validate_runtime_id(runtime_id)?;
        let dir = self.runtime_dir(runtime_id);
        let encrypted_auth = fs::read(dir.join("auth.enc"))
            .map_err(|error| format!("failed to read runtime auth: {error}"))?;
        let config_bytes = fs::read(dir.join("config.toml"))
            .map_err(|error| format!("failed to read runtime config: {error}"))?;
        let metadata_bytes = fs::read(dir.join("runtime.json"))
            .map_err(|error| format!("failed to read runtime metadata: {error}"))?;
        let metadata = deserialize_metadata(&metadata_bytes)?;
        validate_runtime_metadata(runtime_id, &metadata)?;
        verify_runtime_bundle(&dir, &encrypted_auth, &config_bytes, &metadata_bytes)?;
        let auth_json = unprotect(&encrypted_auth)?;
        let config_toml = String::from_utf8(config_bytes)
            .map_err(|_| "stored runtime config was not valid UTF-8".to_string())?;
        validate_runtime_bundle_semantics(&metadata, &auth_json, &config_toml)?;
        Ok((
            metadata,
            RuntimeFiles {
                auth_json,
                config_toml,
            },
        ))
    }

    fn archive_runtime(&self, runtime_id: &str) -> Result<(), String> {
        self.archive_runtime_at(runtime_id, timestamp_millis()?)
    }

    fn archive_runtime_at(&self, runtime_id: &str, timestamp_ms: u128) -> Result<(), String> {
        self.archive_runtime_at_with(runtime_id, timestamp_ms, |source, target| {
            atomic_copy(source, target).map(|_| ())
        })
    }

    fn archive_runtime_at_with<F>(
        &self,
        runtime_id: &str,
        timestamp_ms: u128,
        mut copy_file: F,
    ) -> Result<(), String>
    where
        F: FnMut(&Path, &Path) -> Result<(), String>,
    {
        let runtime_dir = self.runtime_dir(runtime_id);
        if !runtime_dir.exists() {
            return Ok(());
        }
        self.load_runtime_bundle(runtime_id)?;
        let history_root = runtime_dir.join("history");
        fs::create_dir_all(&history_root)
            .map_err(|error| format!("failed to create runtime history root: {error}"))?;
        let (history_dir, staging_dir) = create_history_paths(&history_root, timestamp_ms)?;
        let result = (|| {
            for name in [
                "auth.enc",
                "config.toml",
                "runtime.json",
                RUNTIME_BUNDLE_MANIFEST,
            ] {
                let source = runtime_dir.join(name);
                if source.is_file() {
                    copy_file(&source, &staging_dir.join(name))?;
                }
            }
            self.load_runtime_bundle(runtime_id)?;
            for name in [
                "auth.enc",
                "config.toml",
                "runtime.json",
                RUNTIME_BUNDLE_MANIFEST,
            ] {
                let source = runtime_dir.join(name);
                let archived = staging_dir.join(name);
                match (fs::read(&source), fs::read(&archived)) {
                    (Ok(source), Ok(archived)) if source == archived => {}
                    (Err(source_error), Err(archive_error))
                        if source_error.kind() == std::io::ErrorKind::NotFound
                            && archive_error.kind() == std::io::ErrorKind::NotFound => {}
                    _ => return Err("runtime history copy failed verification".to_string()),
                }
            }
            fs::rename(&staging_dir, &history_dir)
                .map_err(|error| format!("failed to publish runtime history: {error}"))
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&staging_dir);
        }
        result
    }

    fn write_runtime(
        &self,
        metadata: &RuntimeMetadata,
        auth_json: &[u8],
        config_toml: &str,
    ) -> Result<(), String> {
        self.write_runtime_with(metadata, auth_json, config_toml, atomic_write)
    }

    fn write_runtime_with<F>(
        &self,
        metadata: &RuntimeMetadata,
        auth_json: &[u8],
        config_toml: &str,
        mut write_file: F,
    ) -> Result<(), String>
    where
        F: FnMut(&Path, &[u8]) -> Result<(), String>,
    {
        let dir = self.runtime_dir(&metadata.id);
        let auth_path = dir.join("auth.enc");
        let config_path = dir.join("config.toml");
        let metadata_path = dir.join("runtime.json");
        let bundle_path = dir.join(RUNTIME_BUNDLE_MANIFEST);
        let marker_path = dir.join(RUNTIME_BUNDLE_MARKER);
        let encrypted_auth = protect(auth_json)?;
        let metadata_json = serialize_metadata(metadata)?;
        let bundle_json = serialize_runtime_bundle_manifest(&runtime_bundle_manifest(
            &encrypted_auth,
            config_toml.as_bytes(),
            &metadata_json,
        ))?;
        let snapshots = [
            snapshot_runtime_file(&auth_path)?,
            snapshot_runtime_file(&config_path)?,
            snapshot_runtime_file(&metadata_path)?,
            snapshot_runtime_file(&bundle_path)?,
            snapshot_runtime_file(&marker_path)?,
        ];
        fs::create_dir_all(&dir)
            .map_err(|error| format!("failed to create runtime dir: {error}"))?;
        let result = (|| {
            atomic_write(&marker_path, RUNTIME_BUNDLE_MARKER_BYTES)?;
            write_file(&auth_path, &encrypted_auth)?;
            write_file(&config_path, config_toml.as_bytes())?;
            write_file(&metadata_path, &metadata_json)?;
            write_file(&bundle_path, &bundle_json)
        })();
        if let Err(error) = result {
            return match restore_runtime_files(&snapshots) {
                Ok(()) => Err(format!("{error}; rolled back previous runtime files")),
                Err(rollback_error) => Err(format!("{error}; rollback failed: {rollback_error}")),
            };
        }
        Ok(())
    }

    fn write_metadata_and_bundle(
        &self,
        runtime_id: &str,
        metadata: &RuntimeMetadata,
    ) -> Result<(), String> {
        self.write_metadata_and_bundle_with(runtime_id, metadata, atomic_write)
    }

    fn write_metadata_and_bundle_with<F>(
        &self,
        runtime_id: &str,
        metadata: &RuntimeMetadata,
        mut write_file: F,
    ) -> Result<(), String>
    where
        F: FnMut(&Path, &[u8]) -> Result<(), String>,
    {
        validate_runtime_metadata(runtime_id, metadata)?;
        let dir = self.runtime_dir(runtime_id);
        let auth_path = dir.join("auth.enc");
        let config_path = dir.join("config.toml");
        let metadata_path = dir.join("runtime.json");
        let bundle_path = dir.join(RUNTIME_BUNDLE_MANIFEST);
        let marker_path = dir.join(RUNTIME_BUNDLE_MARKER);
        let encrypted_auth = fs::read(&auth_path)
            .map_err(|error| format!("failed to read runtime auth: {error}"))?;
        let config = fs::read(&config_path)
            .map_err(|error| format!("failed to read runtime config: {error}"))?;
        let metadata_json = serialize_metadata(metadata)?;
        let bundle_json = serialize_runtime_bundle_manifest(&runtime_bundle_manifest(
            &encrypted_auth,
            &config,
            &metadata_json,
        ))?;
        let snapshots = [
            snapshot_runtime_file(&metadata_path)?,
            snapshot_runtime_file(&bundle_path)?,
            snapshot_runtime_file(&marker_path)?,
        ];
        let result = (|| {
            atomic_write(&marker_path, RUNTIME_BUNDLE_MARKER_BYTES)?;
            write_file(&metadata_path, &metadata_json)?;
            write_file(&bundle_path, &bundle_json)
        })();
        if let Err(error) = result {
            return match restore_runtime_files(&snapshots) {
                Ok(()) => Err(format!("{error}; rolled back previous runtime files")),
                Err(rollback_error) => Err(format!("{error}; rollback failed: {rollback_error}")),
            };
        }
        Ok(())
    }
}

fn runtime_bundle_manifest(
    encrypted_auth: &[u8],
    config: &[u8],
    metadata: &[u8],
) -> RuntimeBundleManifest {
    RuntimeBundleManifest {
        version: RUNTIME_BUNDLE_VERSION,
        auth_sha256: hash_bytes(encrypted_auth),
        config_sha256: hash_bytes(config),
        metadata_sha256: hash_bytes(metadata),
    }
}

fn serialize_runtime_bundle_manifest(manifest: &RuntimeBundleManifest) -> Result<Vec<u8>, String> {
    serde_json::to_vec_pretty(manifest)
        .map_err(|error| format!("failed to serialize runtime bundle manifest: {error}"))
}

fn verify_runtime_bundle(
    runtime_dir: &Path,
    encrypted_auth: &[u8],
    config: &[u8],
    metadata: &[u8],
) -> Result<(), String> {
    let manifest_bytes = match fs::read(runtime_dir.join(RUNTIME_BUNDLE_MANIFEST)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return verify_legacy_runtime_bundle(runtime_dir);
        }
        Err(_) => return Err(runtime_bundle_integrity_error()),
    };
    let manifest = serde_json::from_slice::<RuntimeBundleManifest>(&manifest_bytes)
        .map_err(|_| runtime_bundle_integrity_error())?;
    if manifest != runtime_bundle_manifest(encrypted_auth, config, metadata) {
        return Err(runtime_bundle_integrity_error());
    }
    Ok(())
}

fn verify_legacy_runtime_bundle(runtime_dir: &Path) -> Result<(), String> {
    if runtime_dir
        .join(RUNTIME_BUNDLE_MARKER)
        .try_exists()
        .map_err(|_| runtime_bundle_integrity_error())?
        || runtime_history_contains_bundle(runtime_dir)?
    {
        return Err(runtime_bundle_integrity_error());
    }
    Ok(())
}

fn runtime_history_contains_bundle(runtime_dir: &Path) -> Result<bool, String> {
    let history_root = runtime_dir.join("history");
    let entries = match fs::read_dir(&history_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err(runtime_bundle_integrity_error()),
    };
    for entry in entries {
        let entry = entry.map_err(|_| runtime_bundle_integrity_error())?;
        if !entry
            .file_type()
            .map_err(|_| runtime_bundle_integrity_error())?
            .is_dir()
        {
            continue;
        }
        let name = entry.file_name();
        if name.to_str().and_then(runtime_history_key).is_none() {
            continue;
        }
        match entry.path().join(RUNTIME_BUNDLE_MANIFEST).try_exists() {
            Ok(true) => return Ok(true),
            Ok(false) => {}
            Err(_) => return Err(runtime_bundle_integrity_error()),
        }
    }
    Ok(false)
}

#[cfg(test)]
fn latest_runtime_history_dir(runtime_dir: &Path) -> Result<Option<PathBuf>, String> {
    let history_root = runtime_dir.join("history");
    let entries = match fs::read_dir(&history_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(runtime_bundle_integrity_error()),
    };
    let mut latest = None;
    for entry in entries {
        let entry = entry.map_err(|_| runtime_bundle_integrity_error())?;
        if !entry
            .file_type()
            .map_err(|_| runtime_bundle_integrity_error())?
            .is_dir()
        {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(key) = runtime_history_key(name) else {
            continue;
        };
        if latest
            .as_ref()
            .is_none_or(|(latest_key, _): &((u128, u64), PathBuf)| key > *latest_key)
        {
            latest = Some((key, entry.path()));
        }
    }
    Ok(latest.map(|(_, path)| path))
}

fn runtime_history_key(name: &str) -> Option<(u128, u64)> {
    let (timestamp, sequence) = name
        .split_once('-')
        .map_or((name, "0"), |(timestamp, sequence)| (timestamp, sequence));
    Some((timestamp.parse().ok()?, sequence.parse().ok()?))
}

fn validate_runtime_bundle_semantics(
    metadata: &RuntimeMetadata,
    auth_json: &[u8],
    config_toml: &str,
) -> Result<(), String> {
    let config_model = config_string(config_toml, "model")?;
    if metadata.model.as_deref() != config_model.as_deref() {
        return Err(runtime_bundle_integrity_error());
    }
    match metadata.kind {
        RuntimeKind::Plus => {
            if auth_mode_from_bytes(auth_json)?.as_deref() != Some("chatgpt")
                || config_string(config_toml, "model_provider")?.is_some()
            {
                return Err(runtime_bundle_integrity_error());
            }
        }
        RuntimeKind::Relay => {
            let base_url =
                provider_config_string(config_toml, MANAGED_RELAY_PROVIDER_ID, "base_url")?;
            if !valid_relay_auth(auth_json)
                || config_string(config_toml, "model_provider")?.as_deref()
                    != Some(MANAGED_RELAY_PROVIDER_ID)
                || metadata.base_url.as_deref() != base_url.as_deref()
            {
                return Err(runtime_bundle_integrity_error());
            }
        }
    }
    Ok(())
}

fn runtime_bundle_integrity_error() -> String {
    "stored runtime files are inconsistent; resave the runtime slot".to_string()
}

fn hash_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Debug)]
struct RuntimeFileSnapshot {
    path: PathBuf,
    contents: Option<Vec<u8>>,
}

fn snapshot_runtime_file(path: &Path) -> Result<RuntimeFileSnapshot, String> {
    let contents = match fs::read(path) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("failed to snapshot runtime file: {error}")),
    };
    Ok(RuntimeFileSnapshot {
        path: path.to_path_buf(),
        contents,
    })
}

fn restore_runtime_files(snapshots: &[RuntimeFileSnapshot]) -> Result<(), String> {
    for snapshot in snapshots {
        let result = match &snapshot.contents {
            Some(contents) => atomic_write(&snapshot.path, contents),
            None => match fs::remove_file(&snapshot.path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(format!("failed to remove new runtime file: {error}")),
            },
        };
        let _ = result;
    }

    let mut verification_error = None;
    for snapshot in snapshots {
        let result = match &snapshot.contents {
            Some(expected) => match fs::read(&snapshot.path) {
                Ok(actual) if actual.as_slice() == expected.as_slice() => Ok(()),
                Ok(_) => Err("restored runtime file did not match its snapshot".to_string()),
                Err(error) => Err(format!("failed to verify restored runtime file: {error}")),
            },
            None => match snapshot.path.try_exists() {
                Ok(false) => Ok(()),
                Ok(true) => Err("new runtime file remained after rollback".to_string()),
                Err(error) => Err(format!("failed to verify removed runtime file: {error}")),
            },
        };
        if verification_error.is_none() {
            verification_error = result.err();
        }
    }
    verification_error.map_or(Ok(()), Err)
}

fn create_history_paths(
    history_root: &Path,
    timestamp_ms: u128,
) -> Result<(PathBuf, PathBuf), String> {
    for sequence in 0_u64.. {
        let name = if sequence == 0 {
            timestamp_ms.to_string()
        } else {
            format!("{timestamp_ms}-{sequence}")
        };
        let history_dir = history_root.join(&name);
        match history_dir.try_exists() {
            Ok(true) => continue,
            Ok(false) => {}
            Err(error) => return Err(format!("failed to inspect runtime history: {error}")),
        }
        let staging_dir = history_root.join(format!(".pending-{name}-{}", std::process::id()));
        match fs::create_dir(&staging_dir) {
            Ok(()) => return Ok((history_dir, staging_dir)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("failed to create runtime history: {error}")),
        }
    }
    unreachable!("u64 history sequence cannot be exhausted")
}

fn validate_runtime_id(runtime_id: &str) -> Result<(), String> {
    if matches!(runtime_id, PLUS_RUNTIME_ID | RELAY_RUNTIME_ID) {
        Ok(())
    } else {
        Err("unsupported runtime id; expected plus or relay".to_string())
    }
}

pub fn default_store_root() -> Result<PathBuf, String> {
    let appdata = std::env::var_os("APPDATA").ok_or_else(|| "APPDATA is not set".to_string())?;
    Ok(PathBuf::from(appdata).join("codex-switch"))
}

fn normalize_base_url(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err("relay base URL is required".to_string());
    }
    if trimmed.contains("://")
        && !trimmed.starts_with("http://")
        && !trimmed.starts_with("https://")
    {
        return Err("invalid relay base URL".to_string());
    }
    let with_scheme = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let mut parsed =
        reqwest::Url::parse(&with_scheme).map_err(|_| "invalid relay base URL".to_string())?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err("invalid relay base URL".to_string());
    }
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err("relay base URL must not contain credentials, query, or fragment".to_string());
    }
    if parsed.scheme() == "http" && !is_loopback_host(parsed.host_str()) {
        return Err("non-loopback relay URLs must use HTTPS".to_string());
    }
    let path = parsed.path().trim_end_matches('/');
    let normalized_path = if path.ends_with("/v1") {
        path.to_string()
    } else if path.is_empty() {
        "/v1".to_string()
    } else {
        format!("{path}/v1")
    };
    parsed.set_path(&normalized_path);
    Ok(parsed.to_string().trim_end_matches('/').to_string())
}

#[derive(Debug, PartialEq, Eq)]
struct NormalizedUrlOrigin {
    scheme: String,
    host: String,
    port: u16,
}

fn normalized_url_origin(value: &str) -> Result<NormalizedUrlOrigin, String> {
    let parsed =
        reqwest::Url::parse(value).map_err(|_| "stored relay base URL is invalid".to_string())?;
    let host = parsed
        .host_str()
        .ok_or_else(|| "stored relay base URL is invalid".to_string())?;
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| "stored relay base URL is invalid".to_string())?;
    Ok(NormalizedUrlOrigin {
        scheme: parsed.scheme().to_ascii_lowercase(),
        host: host.to_ascii_lowercase(),
        port,
    })
}

fn relay_auth_json(api_key: &str) -> Result<Vec<u8>, String> {
    let key = api_key.trim();
    if key.is_empty() {
        return Err("relay API key is required".to_string());
    }
    serde_json::to_vec(&serde_json::json!({
        "auth_mode": "apikey",
        "OPENAI_API_KEY": key
    }))
    .map_err(|error| format!("failed to serialize relay auth: {error}"))
}

fn valid_relay_auth(auth: &[u8]) -> bool {
    relay_api_key_from_auth(auth).is_ok()
}

pub(crate) fn relay_api_key_from_auth(auth: &[u8]) -> Result<String, String> {
    let value = serde_json::from_slice::<JsonValue>(auth)
        .map_err(|_| "stored relay credentials are invalid".to_string())?;
    if value.get("auth_mode").and_then(JsonValue::as_str) != Some("apikey") {
        return Err("stored relay credentials are invalid".to_string());
    }
    value
        .get("OPENAI_API_KEY")
        .and_then(JsonValue::as_str)
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "stored relay credentials are invalid".to_string())
}

fn relay_config_template(base_config: &str, base_url: &str, model: &str) -> Result<String, String> {
    if !base_config.trim().is_empty() {
        DocumentMut::from_str(base_config)
            .map_err(|_| "failed to parse config.toml".to_string())?;
    }
    let mut doc = DocumentMut::new();

    doc["model"] = value(model);
    doc["model_provider"] = value(MANAGED_RELAY_PROVIDER_ID);

    let provider = provider_table_mut(&mut doc, MANAGED_RELAY_PROVIDER_ID)?;
    provider["name"] = value(MANAGED_RELAY_PROVIDER_ID);
    provider["base_url"] = value(base_url);
    provider["wire_api"] = value("responses");
    provider["supports_websockets"] = value(false);
    provider["request_max_retries"] = value(6);
    provider["stream_max_retries"] = value(3);
    provider["stream_idle_timeout_ms"] = value(180_000);
    provider.remove("env_key");
    provider.remove("api_key");
    provider.remove("goal");

    Ok(doc.to_string())
}

fn account_config_overlay(config: &str) -> Result<String, String> {
    let source = DocumentMut::from_str(config)
        .map_err(|_| "failed to parse plus config.toml".to_string())?;
    let mut overlay = DocumentMut::new();
    for key in ["model", "service_tier"] {
        if let Some(value) = source.get(key).and_then(Item::as_str) {
            overlay[key] = toml_edit::value(value);
        }
    }
    Ok(overlay.to_string())
}

fn provider_table_mut<'a>(
    doc: &'a mut DocumentMut,
    provider: &str,
) -> Result<&'a mut Table, String> {
    let providers = doc
        .entry("model_providers")
        .or_insert_with(|| Item::Table(Table::new()));
    let providers_table = providers
        .as_table_mut()
        .ok_or_else(|| "model_providers must be a TOML table".to_string())?;
    let provider_item = providers_table
        .entry(provider)
        .or_insert_with(|| Item::Table(Table::new()));

    provider_item
        .as_table_mut()
        .ok_or_else(|| format!("model_providers.{provider} must be a TOML table"))
}

fn read_model_from_config(config: &str) -> Option<String> {
    config
        .parse::<toml_edit::DocumentMut>()
        .ok()
        .and_then(|doc| {
            doc.get("model")
                .and_then(toml_edit::Item::as_str)
                .map(str::to_string)
        })
}

fn serialize_metadata(metadata: &RuntimeMetadata) -> Result<Vec<u8>, String> {
    serde_json::to_vec_pretty(metadata)
        .map_err(|error| format!("failed to serialize runtime metadata: {error}"))
}

fn read_metadata(path: &Path) -> Result<RuntimeMetadata, String> {
    let raw =
        fs::read(path).map_err(|error| format!("failed to read runtime metadata: {error}"))?;
    deserialize_metadata(&raw)
}

fn deserialize_metadata(raw: &[u8]) -> Result<RuntimeMetadata, String> {
    serde_json::from_slice(raw)
        .map_err(|error| format!("failed to parse runtime metadata: {error}"))
}

fn validate_runtime_metadata(runtime_id: &str, metadata: &RuntimeMetadata) -> Result<(), String> {
    let expected_kind = if runtime_id == PLUS_RUNTIME_ID {
        RuntimeKind::Plus
    } else {
        RuntimeKind::Relay
    };
    if metadata.id != runtime_id || metadata.kind != expected_kind {
        return Err(format!(
            "runtime metadata does not match the fixed {runtime_id} slot"
        ));
    }
    Ok(())
}

fn timestamp_millis() -> Result<u128, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .map_err(|error| format!("system clock before unix epoch: {error}"))
}

pub(crate) fn is_loopback_host(host: Option<&str>) -> bool {
    host.is_some_and(|host| {
        let host = host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(host);
        host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    })
}

fn auth_mode_from_bytes(bytes: &[u8]) -> Result<Option<String>, String> {
    let value: JsonValue = serde_json::from_slice(bytes)
        .map_err(|error| format!("failed to parse auth.json: {error}"))?;
    Ok(value
        .get("auth_mode")
        .and_then(JsonValue::as_str)
        .map(str::to_string))
}

fn config_string(config: &str, key: &str) -> Result<Option<String>, String> {
    let doc =
        DocumentMut::from_str(config).map_err(|_| "failed to parse config.toml".to_string())?;
    Ok(doc.get(key).and_then(Item::as_str).map(str::to_string))
}

fn runtime_binding_matches(
    runtime: &RuntimeMetadata,
    stored_auth: &[u8],
    stored_config: &str,
    live_config: &str,
) -> Result<bool, String> {
    let stored = DocumentMut::from_str(stored_config)
        .map_err(|_| "failed to parse stored runtime config.toml".to_string())?;
    let live = DocumentMut::from_str(live_config)
        .map_err(|_| "failed to parse live config.toml".to_string())?;
    let stored_model = document_string(&stored, "model");
    let live_model = document_string(&live, "model");
    if runtime.model.as_deref() != stored_model {
        return Ok(false);
    }
    match runtime.kind {
        RuntimeKind::Plus => {
            let stored_service_tier = document_string(&stored, "service_tier");
            Ok(document_string(&live, "model_provider").is_none()
                && provider_config_table(&live, MANAGED_RELAY_PROVIDER_ID).is_none()
                && live_model == stored_model
                && document_string(&live, "service_tier") == stored_service_tier)
        }
        RuntimeKind::Relay => {
            let relay_api_key = relay_api_key_from_auth(stored_auth)?;
            let expected_provider = provider_config_fingerprint_with_bearer(
                &stored,
                MANAGED_RELAY_PROVIDER_ID,
                &relay_api_key,
            );
            let stored_provider = document_string(&stored, "model_provider");
            Ok(stored_provider == Some(MANAGED_RELAY_PROVIDER_ID)
                && document_string(&live, "model_provider") == stored_provider
                && live_model == stored_model
                && provider_config_fingerprint(&live, MANAGED_RELAY_PROVIDER_ID)
                    == expected_provider)
        }
    }
}

fn document_string<'a>(doc: &'a DocumentMut, key: &str) -> Option<&'a str> {
    doc.get(key).and_then(Item::as_str)
}

fn provider_config_table<'a>(doc: &'a DocumentMut, provider: &str) -> Option<&'a Table> {
    doc.get("model_providers")
        .and_then(Item::as_table)
        .and_then(|providers| providers.get(provider))
        .and_then(Item::as_table)
}

fn provider_config_fingerprint(
    doc: &DocumentMut,
    provider: &str,
) -> Option<Vec<(String, SemanticToml)>> {
    let mut entries = provider_config_table(doc, provider)?
        .iter()
        .map(|(key, item)| (key.to_string(), semantic_toml_item(item)))
        .collect::<Vec<_>>();
    entries.sort_unstable();
    Some(entries)
}

fn provider_config_fingerprint_with_bearer(
    doc: &DocumentMut,
    provider: &str,
    bearer_token: &str,
) -> Option<Vec<(String, SemanticToml)>> {
    let mut entries = provider_config_fingerprint(doc, provider)?;
    entries.retain(|(key, _)| {
        !matches!(
            key.as_str(),
            "api_key" | "env_key" | "goal" | "experimental_bearer_token"
        )
    });
    entries.push((
        "experimental_bearer_token".to_string(),
        SemanticToml::String(bearer_token.to_string()),
    ));
    entries.sort_unstable();
    Some(entries)
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum SemanticToml {
    None,
    String(String),
    Integer(i64),
    Float(u64),
    Boolean(bool),
    Datetime(String),
    Array(Vec<SemanticToml>),
    Table(Vec<(String, SemanticToml)>),
    ArrayOfTables(Vec<Vec<(String, SemanticToml)>>),
}

fn semantic_toml_item(item: &Item) -> SemanticToml {
    match item {
        Item::None => SemanticToml::None,
        Item::Value(value) => semantic_toml_value(value),
        Item::Table(table) => SemanticToml::Table(semantic_toml_table(table)),
        Item::ArrayOfTables(tables) => {
            SemanticToml::ArrayOfTables(tables.iter().map(semantic_toml_table).collect::<Vec<_>>())
        }
    }
}

fn semantic_toml_value(value: &toml_edit::Value) -> SemanticToml {
    match value {
        toml_edit::Value::String(value) => SemanticToml::String(value.value().clone()),
        toml_edit::Value::Integer(value) => SemanticToml::Integer(*value.value()),
        toml_edit::Value::Float(value) => SemanticToml::Float(value.value().to_bits()),
        toml_edit::Value::Boolean(value) => SemanticToml::Boolean(*value.value()),
        toml_edit::Value::Datetime(value) => SemanticToml::Datetime(value.value().to_string()),
        toml_edit::Value::Array(values) => {
            SemanticToml::Array(values.iter().map(semantic_toml_value).collect())
        }
        toml_edit::Value::InlineTable(table) => {
            let mut entries = table
                .iter()
                .map(|(key, value)| (key.to_string(), semantic_toml_value(value)))
                .collect::<Vec<_>>();
            entries.sort_unstable();
            SemanticToml::Table(entries)
        }
    }
}

fn semantic_toml_table(table: &Table) -> Vec<(String, SemanticToml)> {
    let mut entries = table
        .iter()
        .map(|(key, item)| (key.to_string(), semantic_toml_item(item)))
        .collect::<Vec<_>>();
    entries.sort_unstable();
    entries
}

fn provider_config_string(
    config: &str,
    provider: &str,
    key: &str,
) -> Result<Option<String>, String> {
    let doc =
        DocumentMut::from_str(config).map_err(|_| "failed to parse config.toml".to_string())?;
    Ok(doc
        .get("model_providers")
        .and_then(Item::as_table)
        .and_then(|providers| providers.get(provider))
        .and_then(Item::as_table)
        .and_then(|provider| provider.get(key))
        .and_then(Item::as_str)
        .map(str::to_string))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{RuntimeConfidence, RuntimeKind, RuntimeStore, PLUS_RUNTIME_ID, RELAY_RUNTIME_ID};
    use crate::config_patch::{plan_runtime_config_patch, RuntimeConfigKind};

    fn activate_relay_route(store: &RuntimeStore, home: &std::path::Path) {
        let files = store.load_runtime_files(RELAY_RUNTIME_ID).unwrap();
        let key = super::relay_api_key_from_auth(&files.auth_json).unwrap();
        let live = fs::read_to_string(home.join("config.toml")).unwrap();
        let plan = plan_runtime_config_patch(
            &live,
            &files.config_toml,
            RuntimeConfigKind::Relay,
            Some(&key),
        )
        .unwrap();
        fs::write(home.join("config.toml"), plan.patched_toml).unwrap();
    }

    #[test]
    fn imports_plus_runtime_as_encrypted_auth_and_full_config() {
        let home = tempdir().unwrap();
        fs::write(
            home.path().join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"fake-plus"}}"#,
        )
        .unwrap();
        fs::write(
            home.path().join("config.toml"),
            "model = \"gpt-5.5\"\nmodel_instructions_file = \"global\"\n",
        )
        .unwrap();
        let root = tempdir().unwrap();
        let store = RuntimeStore::new(root.path().join("runtimes"));

        let metadata = store.import_plus_from_home(home.path(), false).unwrap();

        assert_eq!(metadata.id, PLUS_RUNTIME_ID);
        assert_eq!(metadata.kind, RuntimeKind::Plus);
        assert_eq!(metadata.model.as_deref(), Some("gpt-5.5"));
        let encrypted = fs::read(store.runtime_dir(PLUS_RUNTIME_ID).join("auth.enc")).unwrap();
        assert!(!String::from_utf8_lossy(&encrypted).contains("fake-plus"));
        let files = store.load_runtime_files(PLUS_RUNTIME_ID).unwrap();
        assert!(String::from_utf8(files.auth_json)
            .unwrap()
            .contains("fake-plus"));
        assert!(!files.config_toml.contains("model_instructions_file"));
    }

    #[test]
    fn upserts_single_relay_runtime_with_normalized_url_and_encrypted_key() {
        let home = tempdir().unwrap();
        fs::write(
            home.path().join("config.toml"),
            "model = \"gpt-5.5\"\nmodel_instructions_file = \"global\"\n",
        )
        .unwrap();
        let root = tempdir().unwrap();
        let store = RuntimeStore::new(root.path().join("runtimes"));

        let metadata = store
            .upsert_relay(
                super::RelayRuntimeInput {
                    base_url: "relay.example.com".to_string(),
                    api_key: "sk-fake-relay".to_string(),
                    model: "gpt-5.5".to_string(),
                },
                home.path(),
            )
            .unwrap();

        assert_eq!(metadata.id, RELAY_RUNTIME_ID);
        assert_eq!(metadata.kind, RuntimeKind::Relay);
        assert_eq!(
            metadata.base_url.as_deref(),
            Some("https://relay.example.com/v1")
        );
        let encrypted = fs::read(store.runtime_dir(RELAY_RUNTIME_ID).join("auth.enc")).unwrap();
        assert!(!String::from_utf8_lossy(&encrypted).contains("sk-fake-relay"));
        let stored_config =
            fs::read_to_string(store.runtime_dir(RELAY_RUNTIME_ID).join("config.toml")).unwrap();
        assert!(!stored_config.contains("sk-fake-relay"));
        let files = store.load_runtime_files(RELAY_RUNTIME_ID).unwrap();
        let auth = String::from_utf8(files.auth_json).unwrap();
        assert!(auth.contains("\"auth_mode\":\"apikey\""));
        assert!(auth.contains("sk-fake-relay"));
        assert!(files
            .config_toml
            .contains("model_provider = \"openai_custom\""));
        assert!(files
            .config_toml
            .contains("[model_providers.openai_custom]"));
        assert!(files.config_toml.contains("name = \"openai_custom\""));
        assert!(files.config_toml.contains("wire_api = \"responses\""));
        assert!(files.config_toml.contains("supports_websockets = false"));
        assert!(files.config_toml.contains("request_max_retries = 6"));
        assert!(files.config_toml.contains("stream_max_retries = 3"));
        assert!(files
            .config_toml
            .contains("stream_idle_timeout_ms = 180000"));
        assert!(!files.config_toml.contains("env_key ="));
        assert!(!files.config_toml.contains("api_key ="));
        assert!(!files.config_toml.contains("goal ="));
        assert!(files
            .config_toml
            .contains("base_url = \"https://relay.example.com/v1\""));
        assert!(!files.config_toml.contains("model_instructions_file"));
        assert_eq!(store.list_runtimes().unwrap().len(), 1);
    }

    #[test]
    fn relay_update_with_blank_key_preserves_credential_for_same_origin_path_change() {
        let home = tempdir().unwrap();
        fs::write(home.path().join("config.toml"), "model = \"old\"\n").unwrap();
        let root = tempdir().unwrap();
        let store = RuntimeStore::new(root.path().join("runtimes"));
        store
            .upsert_relay(
                super::RelayRuntimeInput {
                    base_url: "https://relay.example.com/gateway".to_string(),
                    api_key: "sk-preserved".to_string(),
                    model: "old".to_string(),
                },
                home.path(),
            )
            .unwrap();

        store
            .upsert_relay(
                super::RelayRuntimeInput {
                    base_url: "https://relay.example.com/edge".to_string(),
                    api_key: String::new(),
                    model: "new".to_string(),
                },
                home.path(),
            )
            .unwrap();

        let connection = store.load_relay_connection().unwrap();
        assert_eq!(connection.api_key, "sk-preserved");
        assert_eq!(connection.base_url, "https://relay.example.com/edge/v1");
        assert_eq!(
            store
                .load_metadata(RELAY_RUNTIME_ID)
                .unwrap()
                .model
                .as_deref(),
            Some("new")
        );
    }

    #[test]
    fn relay_update_with_blank_key_rejects_origin_changes_before_writing() {
        for (old_url, new_url) in [
            (
                "https://relay.example.com/v1",
                "https://other.example.com/v1",
            ),
            (
                "https://relay.example.com/v1",
                "https://relay.example.com:8443/v1",
            ),
            ("https://localhost/v1", "http://localhost/v1"),
        ] {
            let home = tempdir().unwrap();
            fs::write(home.path().join("config.toml"), "model = \"old\"\n").unwrap();
            let root = tempdir().unwrap();
            let store = RuntimeStore::new(root.path().join("runtimes"));
            store
                .upsert_relay(
                    super::RelayRuntimeInput {
                        base_url: old_url.to_string(),
                        api_key: "sk-preserved".to_string(),
                        model: "old".to_string(),
                    },
                    home.path(),
                )
                .unwrap();
            let runtime_dir = store.runtime_dir(RELAY_RUNTIME_ID);
            let before = [
                fs::read(runtime_dir.join("auth.enc")).unwrap(),
                fs::read(runtime_dir.join("config.toml")).unwrap(),
                fs::read(runtime_dir.join("runtime.json")).unwrap(),
            ];

            let error = store
                .upsert_relay(
                    super::RelayRuntimeInput {
                        base_url: new_url.to_string(),
                        api_key: String::new(),
                        model: "new".to_string(),
                    },
                    home.path(),
                )
                .unwrap_err();

            assert!(error.contains("API key is required"), "{error}");
            assert!(!error.contains(old_url), "{error}");
            assert!(!error.contains(new_url), "{error}");
            assert_eq!(
                [
                    fs::read(runtime_dir.join("auth.enc")).unwrap(),
                    fs::read(runtime_dir.join("config.toml")).unwrap(),
                    fs::read(runtime_dir.join("runtime.json")).unwrap(),
                ],
                before
            );
            assert!(!runtime_dir.join("history").exists());
        }
    }

    #[test]
    fn relay_update_archives_the_previous_runtime_before_overwrite() {
        let home = tempdir().unwrap();
        fs::write(home.path().join("config.toml"), "model = \"old\"\n").unwrap();
        let root = tempdir().unwrap();
        let store = RuntimeStore::new(root.path().join("runtimes"));
        store
            .upsert_relay(
                super::RelayRuntimeInput {
                    base_url: "https://old.example.com/v1".to_string(),
                    api_key: "sk-old-relay".to_string(),
                    model: "old".to_string(),
                },
                home.path(),
            )
            .unwrap();
        let runtime_dir = store.runtime_dir(RELAY_RUNTIME_ID);
        let old_auth = fs::read(runtime_dir.join("auth.enc")).unwrap();
        let old_config = fs::read(runtime_dir.join("config.toml")).unwrap();
        let old_metadata = fs::read(runtime_dir.join("runtime.json")).unwrap();

        store
            .upsert_relay(
                super::RelayRuntimeInput {
                    base_url: "https://new.example.com/v1".to_string(),
                    api_key: "sk-new-relay".to_string(),
                    model: "new".to_string(),
                },
                home.path(),
            )
            .unwrap();

        let history = runtime_dir
            .join("history")
            .read_dir()
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(history.len(), 1);
        assert_eq!(fs::read(history[0].join("auth.enc")).unwrap(), old_auth);
        assert_eq!(
            fs::read(history[0].join("config.toml")).unwrap(),
            old_config
        );
        assert_eq!(
            fs::read(history[0].join("runtime.json")).unwrap(),
            old_metadata
        );
        assert_ne!(fs::read(runtime_dir.join("auth.enc")).unwrap(), old_auth);
        assert!(fs::read_to_string(runtime_dir.join("config.toml"))
            .unwrap()
            .contains("new.example.com"));
        let connection = store.load_relay_connection().unwrap();
        assert_eq!(connection.base_url, "https://new.example.com/v1");
        assert_eq!(connection.api_key, "sk-new-relay");
    }

    #[test]
    fn same_millisecond_archives_use_distinct_history_directories() {
        let home = tempdir().unwrap();
        fs::write(home.path().join("config.toml"), "model = \"old\"\n").unwrap();
        let root = tempdir().unwrap();
        let store = RuntimeStore::new(root.path().join("runtimes"));
        store
            .upsert_relay(
                super::RelayRuntimeInput {
                    base_url: "https://relay.example.com/v1".to_string(),
                    api_key: "sk-relay".to_string(),
                    model: "old".to_string(),
                },
                home.path(),
            )
            .unwrap();
        let runtime_dir = store.runtime_dir(RELAY_RUNTIME_ID);
        let first_config = fs::read(runtime_dir.join("config.toml")).unwrap();

        store.archive_runtime_at(RELAY_RUNTIME_ID, 123).unwrap();
        store.archive_runtime_at(RELAY_RUNTIME_ID, 123).unwrap();

        assert_eq!(
            fs::read(runtime_dir.join("history/123/config.toml")).unwrap(),
            first_config
        );
        assert_eq!(
            fs::read(runtime_dir.join("history/123-1/config.toml")).unwrap(),
            first_config
        );
    }

    #[test]
    fn failed_archive_copy_never_publishes_a_partial_history() {
        let home = tempdir().unwrap();
        fs::write(home.path().join("config.toml"), "model = \"old\"\n").unwrap();
        let root = tempdir().unwrap();
        let store = RuntimeStore::new(root.path().join("runtimes"));
        store
            .upsert_relay(
                super::RelayRuntimeInput {
                    base_url: "https://relay.example.com/v1".to_string(),
                    api_key: "sk-fake-relay".to_string(),
                    model: "relay-model".to_string(),
                },
                home.path(),
            )
            .unwrap();
        let runtime_dir = store.runtime_dir(RELAY_RUNTIME_ID);
        fs::remove_file(runtime_dir.join(super::RUNTIME_BUNDLE_MANIFEST)).unwrap();
        fs::remove_file(runtime_dir.join(super::RUNTIME_BUNDLE_MARKER)).unwrap();

        for (timestamp, failed_copy) in [(200, 2), (201, 3)] {
            let mut copies = 0;
            let error = store
                .archive_runtime_at_with(RELAY_RUNTIME_ID, timestamp, |source, target| {
                    copies += 1;
                    if copies == failed_copy {
                        Err("injected archive copy failure".to_string())
                    } else {
                        crate::file_ops::atomic_copy(source, target).map(|_| ())
                    }
                })
                .unwrap_err();

            assert!(error.contains("injected archive copy failure"), "{error}");
            assert!(store.load_runtime_files(RELAY_RUNTIME_ID).is_ok());
            assert!(super::latest_runtime_history_dir(&runtime_dir)
                .unwrap()
                .is_none());
            assert_eq!(
                fs::read_dir(runtime_dir.join("history")).unwrap().count(),
                0
            );
        }
    }

    #[test]
    fn runtime_overlays_do_not_persist_unrelated_live_global_sections() {
        let home = tempdir().unwrap();
        fs::write(
            home.path().join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"plus"}}"#,
        )
        .unwrap();
        fs::write(
            home.path().join("config.toml"),
            concat!(
                "model = \"gpt-5.5\"\n",
                "model_instructions_file = \"private-global\"\n",
                "[features]\nfast_mode = true\n",
                "[mcp_servers.private]\ncommand = \"private-command\"\n",
            ),
        )
        .unwrap();
        let root = tempdir().unwrap();
        let store = RuntimeStore::new(root.path().join("runtimes"));

        store.import_plus_from_home(home.path(), false).unwrap();
        store
            .upsert_relay(
                super::RelayRuntimeInput {
                    base_url: "relay.example.com".to_string(),
                    api_key: "sk-test".to_string(),
                    model: "relay-model".to_string(),
                },
                home.path(),
            )
            .unwrap();

        for runtime_id in [PLUS_RUNTIME_ID, RELAY_RUNTIME_ID] {
            let config = store.load_runtime_files(runtime_id).unwrap().config_toml;
            assert!(!config.contains("private-global"));
            assert!(!config.contains("private-command"));
            assert!(!config.contains("[features]"));
        }
    }

    #[test]
    fn relay_url_rejects_credentials_query_non_http_and_insecure_remote_schemes() {
        let home = tempdir().unwrap();
        fs::write(home.path().join("config.toml"), "model = \"old\"\n").unwrap();
        let root = tempdir().unwrap();
        let store = RuntimeStore::new(root.path().join("runtimes"));
        for url in [
            "https://user:password@relay.example.com/v1",
            "https://relay.example.com/v1?api_key=secret",
            "file:///tmp/relay",
            "http://relay.example.com/v1",
            "http://192.168.1.20:8787/v1",
        ] {
            let error = store
                .upsert_relay(
                    super::RelayRuntimeInput {
                        base_url: url.to_string(),
                        api_key: "sk-test".to_string(),
                        model: "gpt".to_string(),
                    },
                    home.path(),
                )
                .unwrap_err();
            assert!(
                error.contains("relay base URL")
                    || error.contains("invalid")
                    || error.contains("HTTPS"),
                "{error}"
            );
            assert!(!error.contains("password"));
            assert!(!error.contains("secret"));
        }
    }

    #[test]
    fn relay_url_allows_http_only_for_loopback_hosts() {
        for url in [
            "http://localhost:8787",
            "http://127.0.0.1:8787/v1",
            "http://127.20.30.40:8787",
            "http://[::1]:8787/v1",
        ] {
            let normalized = super::normalize_base_url(url).unwrap();
            assert!(normalized.starts_with("http://"), "{normalized}");
            assert!(normalized.ends_with("/v1"), "{normalized}");
        }
    }

    #[test]
    fn account_import_rejects_relay_auth_and_requires_confirmation_before_overwrite() {
        let home = tempdir().unwrap();
        fs::write(
            home.path().join("auth.json"),
            r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-fake-relay"}"#,
        )
        .unwrap();
        fs::write(home.path().join("config.toml"), "model = \"gpt-5.5\"\n").unwrap();
        let root = tempdir().unwrap();
        let store = RuntimeStore::new(root.path().join("runtimes"));

        let error = store.import_plus_from_home(home.path(), false).unwrap_err();
        assert!(error.contains("账号登录态"));

        fs::write(
            home.path().join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"first"}}"#,
        )
        .unwrap();
        store.import_plus_from_home(home.path(), false).unwrap();
        fs::write(
            home.path().join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"second"}}"#,
        )
        .unwrap();

        let error = store.import_plus_from_home(home.path(), false).unwrap_err();
        assert!(error.contains("确认覆盖"));
        store.import_plus_from_home(home.path(), true).unwrap();
        assert!(store
            .runtime_dir(PLUS_RUNTIME_ID)
            .join("history")
            .read_dir()
            .unwrap()
            .next()
            .is_some());
    }

    #[test]
    fn account_import_rejects_a_live_managed_relay_route_without_touching_the_slot() {
        let home = tempdir().unwrap();
        fs::write(
            home.path().join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"official"}}"#,
        )
        .unwrap();
        fs::write(
            home.path().join("config.toml"),
            "model = \"relay-model\"\nmodel_provider = \"openai_custom\"\n",
        )
        .unwrap();
        let root = tempdir().unwrap();
        let store = RuntimeStore::new(root.path().join("runtimes"));

        let error = store.import_plus_from_home(home.path(), false).unwrap_err();

        assert!(error.contains("先切回 OpenAI 官方请求端"), "{error}");
        assert!(!store.runtime_dir(PLUS_RUNTIME_ID).exists());
    }

    #[test]
    fn refreshed_official_auth_remains_an_exact_route_match_without_exposing_credentials() {
        let home = tempdir().unwrap();
        fs::write(
            home.path().join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"first"}}"#,
        )
        .unwrap();
        fs::write(home.path().join("config.toml"), "model = \"gpt-5.5\"\n").unwrap();
        let root = tempdir().unwrap();
        let store = RuntimeStore::new(root.path().join("runtimes"));
        store.import_plus_from_home(home.path(), false).unwrap();

        let exact = store.detect_active_runtime(home.path()).unwrap();
        assert_eq!(exact.active_runtime_id.as_deref(), Some(PLUS_RUNTIME_ID));
        assert_eq!(exact.confidence, RuntimeConfidence::Exact);

        fs::write(
            home.path().join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"refreshed"}}"#,
        )
        .unwrap();
        let refreshed = store.detect_active_runtime(home.path()).unwrap();
        assert_eq!(
            refreshed.active_runtime_id.as_deref(),
            Some(PLUS_RUNTIME_ID)
        );
        assert_eq!(refreshed.confidence, RuntimeConfidence::Exact);
    }

    #[test]
    fn changed_saved_relay_base_url_is_not_exact_until_live_config_is_applied() {
        let home = tempdir().unwrap();
        fs::write(
            home.path().join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"official"}}"#,
        )
        .unwrap();
        fs::write(home.path().join("config.toml"), "model = \"old\"\n").unwrap();
        let root = tempdir().unwrap();
        let store = RuntimeStore::new(root.path().join("runtimes"));
        store
            .upsert_relay(
                super::RelayRuntimeInput {
                    base_url: "https://relay.example.com/old/v1".to_string(),
                    api_key: "sk-fake-relay".to_string(),
                    model: "relay-model".to_string(),
                },
                home.path(),
            )
            .unwrap();
        activate_relay_route(&store, home.path());
        let exact = store.detect_active_runtime(home.path()).unwrap();
        assert_eq!(exact.active_runtime_id.as_deref(), Some(RELAY_RUNTIME_ID));
        assert_eq!(exact.confidence, RuntimeConfidence::Exact);

        store
            .upsert_relay(
                super::RelayRuntimeInput {
                    base_url: "https://relay.example.com/new/v1".to_string(),
                    api_key: String::new(),
                    model: "relay-model".to_string(),
                },
                home.path(),
            )
            .unwrap();

        let stale_live = store.detect_active_runtime(home.path()).unwrap();
        assert_eq!(
            stale_live.active_runtime_id.as_deref(),
            Some(RELAY_RUNTIME_ID)
        );
        assert_eq!(stale_live.confidence, RuntimeConfidence::Mode);
    }

    #[test]
    fn relay_provider_route_drift_is_not_reported_as_exact() {
        let home = tempdir().unwrap();
        fs::write(
            home.path().join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"official"}}"#,
        )
        .unwrap();
        fs::write(home.path().join("config.toml"), "model = \"old\"\n").unwrap();
        let root = tempdir().unwrap();
        let store = RuntimeStore::new(root.path().join("runtimes"));
        store
            .upsert_relay(
                super::RelayRuntimeInput {
                    base_url: "https://relay.example.com/v1".to_string(),
                    api_key: "sk-fake-relay".to_string(),
                    model: "relay-model".to_string(),
                },
                home.path(),
            )
            .unwrap();
        activate_relay_route(&store, home.path());
        let active_config = fs::read_to_string(home.path().join("config.toml")).unwrap();
        assert_eq!(
            store.detect_active_runtime(home.path()).unwrap().confidence,
            RuntimeConfidence::Exact
        );

        for (from, to) in [
            ("wire_api = \"responses\"", "wire_api = \"chat\""),
            ("supports_websockets = false", "supports_websockets = true"),
            ("request_max_retries = 6", "request_max_retries = 2"),
            ("stream_max_retries = 3", "stream_max_retries = 1"),
            (
                "stream_idle_timeout_ms = 180000",
                "stream_idle_timeout_ms = 1000",
            ),
        ] {
            let drifted = active_config.replace(from, to);
            assert_ne!(drifted, active_config, "{from}");
            fs::write(home.path().join("config.toml"), drifted).unwrap();

            assert_eq!(
                store.detect_active_runtime(home.path()).unwrap().confidence,
                RuntimeConfidence::Mode,
                "{from}"
            );
        }
    }

    #[test]
    fn relay_provider_formatting_drift_remains_exact() {
        let home = tempdir().unwrap();
        fs::write(
            home.path().join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"official"}}"#,
        )
        .unwrap();
        fs::write(home.path().join("config.toml"), "model = \"old\"\n").unwrap();
        let root = tempdir().unwrap();
        let store = RuntimeStore::new(root.path().join("runtimes"));
        store
            .upsert_relay(
                super::RelayRuntimeInput {
                    base_url: "https://relay.example.com/v1".to_string(),
                    api_key: "sk-fake-relay".to_string(),
                    model: "relay-model".to_string(),
                },
                home.path(),
            )
            .unwrap();
        activate_relay_route(&store, home.path());
        let active_config = fs::read_to_string(home.path().join("config.toml")).unwrap();
        let formatted = active_config
            .replace(
                "request_max_retries = 6",
                "request_max_retries = 0x6 # equivalent TOML integer",
            )
            .replace(
                "wire_api = \"responses\"",
                "wire_api = 'responses' # equivalent TOML string",
            );
        assert_ne!(formatted, active_config);
        fs::write(home.path().join("config.toml"), formatted).unwrap();

        assert_eq!(
            store.detect_active_runtime(home.path()).unwrap().confidence,
            RuntimeConfidence::Exact
        );
    }

    #[test]
    fn relay_bundle_rejects_each_uncommitted_write_boundary() {
        let home = tempdir().unwrap();
        fs::write(home.path().join("config.toml"), "model = \"old\"\n").unwrap();
        let root = tempdir().unwrap();
        let store = RuntimeStore::new(root.path().join("runtimes"));
        store
            .upsert_relay(
                super::RelayRuntimeInput {
                    base_url: "https://old.example.com/v1".to_string(),
                    api_key: "sk-old-secret".to_string(),
                    model: "old".to_string(),
                },
                home.path(),
            )
            .unwrap();
        let runtime_dir = store.runtime_dir(RELAY_RUNTIME_ID);
        let names = [
            "auth.enc",
            "config.toml",
            "runtime.json",
            super::RUNTIME_BUNDLE_MANIFEST,
        ];
        let old = names.map(|name| fs::read(runtime_dir.join(name)).unwrap());

        store
            .upsert_relay(
                super::RelayRuntimeInput {
                    base_url: "https://new.example.com/v1".to_string(),
                    api_key: "sk-new-secret".to_string(),
                    model: "new".to_string(),
                },
                home.path(),
            )
            .unwrap();
        let new = names.map(|name| fs::read(runtime_dir.join(name)).unwrap());

        for completed_writes in 1..=3 {
            for (name, contents) in names.iter().zip(&old) {
                fs::write(runtime_dir.join(name), contents).unwrap();
            }
            for (name, contents) in names.iter().zip(&new).take(completed_writes) {
                fs::write(runtime_dir.join(name), contents).unwrap();
            }

            let error = store.load_relay_connection().unwrap_err();
            assert!(
                error.contains("inconsistent"),
                "{completed_writes}: {error}"
            );
            assert!(!error.contains("sk-old-secret"), "{error}");
            assert!(!error.contains("sk-new-secret"), "{error}");
            assert!(!error.contains("old.example.com"), "{error}");
            assert!(!error.contains("new.example.com"), "{error}");
        }
    }

    #[test]
    fn complete_legacy_runtime_without_history_remains_readable() {
        let home = tempdir().unwrap();
        fs::write(home.path().join("config.toml"), "model = \"old\"\n").unwrap();
        let root = tempdir().unwrap();
        let store = RuntimeStore::new(root.path().join("runtimes"));
        store
            .upsert_relay(
                super::RelayRuntimeInput {
                    base_url: "https://relay.example.com/v1".to_string(),
                    api_key: "sk-legacy-secret".to_string(),
                    model: "legacy-model".to_string(),
                },
                home.path(),
            )
            .unwrap();
        let runtime_dir = store.runtime_dir(RELAY_RUNTIME_ID);
        fs::remove_file(runtime_dir.join(super::RUNTIME_BUNDLE_MANIFEST)).unwrap();
        fs::remove_file(runtime_dir.join(super::RUNTIME_BUNDLE_MARKER)).unwrap();

        let connection = store.load_relay_connection().unwrap();

        assert_eq!(connection.base_url, "https://relay.example.com/v1");
        assert_eq!(connection.api_key, "sk-legacy-secret");
        assert_eq!(connection.model, "legacy-model");
    }

    #[test]
    fn missing_live_manifest_cannot_downgrade_an_upgraded_slot_to_legacy() {
        let home = tempdir().unwrap();
        fs::write(home.path().join("config.toml"), "model = \"old\"\n").unwrap();
        let root = tempdir().unwrap();
        let store = RuntimeStore::new(root.path().join("runtimes"));
        store
            .upsert_relay(
                super::RelayRuntimeInput {
                    base_url: "https://old.example.com/v1".to_string(),
                    api_key: "sk-old-secret".to_string(),
                    model: "old".to_string(),
                },
                home.path(),
            )
            .unwrap();
        store
            .upsert_relay(
                super::RelayRuntimeInput {
                    base_url: "https://new.example.com/v1".to_string(),
                    api_key: "sk-new-secret".to_string(),
                    model: "new".to_string(),
                },
                home.path(),
            )
            .unwrap();
        let runtime_dir = store.runtime_dir(RELAY_RUNTIME_ID);
        fs::remove_file(runtime_dir.join(super::RUNTIME_BUNDLE_MANIFEST)).unwrap();

        let error = store.load_relay_connection().unwrap_err();

        assert!(error.contains("inconsistent"), "{error}");
        assert!(!error.contains("sk-old-secret"), "{error}");
        assert!(!error.contains("sk-new-secret"), "{error}");
        assert!(!error.contains("old.example.com"), "{error}");
        assert!(!error.contains("new.example.com"), "{error}");
    }

    #[test]
    fn complete_legacy_key_only_update_with_history_remains_readable() {
        let home = tempdir().unwrap();
        fs::write(home.path().join("config.toml"), "model = \"old\"\n").unwrap();
        let root = tempdir().unwrap();
        let store = RuntimeStore::new(root.path().join("runtimes"));
        store
            .upsert_relay(
                super::RelayRuntimeInput {
                    base_url: "https://old.example.com/v1".to_string(),
                    api_key: "sk-old-secret".to_string(),
                    model: "old".to_string(),
                },
                home.path(),
            )
            .unwrap();
        let runtime_dir = store.runtime_dir(RELAY_RUNTIME_ID);
        store.archive_runtime_at(RELAY_RUNTIME_ID, 123).unwrap();
        fs::remove_file(runtime_dir.join(super::RUNTIME_BUNDLE_MANIFEST)).unwrap();
        fs::remove_file(runtime_dir.join(super::RUNTIME_BUNDLE_MARKER)).unwrap();
        fs::remove_file(
            runtime_dir
                .join("history/123")
                .join(super::RUNTIME_BUNDLE_MANIFEST),
        )
        .unwrap();
        fs::write(
            runtime_dir.join("auth.enc"),
            crate::crypto::protect(br#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-new-secret"}"#)
                .unwrap(),
        )
        .unwrap();

        let connection = store.load_relay_connection().unwrap();

        assert_eq!(connection.base_url, "https://old.example.com/v1");
        assert_eq!(connection.api_key, "sk-new-secret");
        assert_eq!(connection.model, "old");
    }

    #[test]
    fn rejects_runtime_ids_outside_the_fixed_slots_before_reading_files() {
        let root = tempdir().unwrap();
        let store = RuntimeStore::new(root.path().join("runtimes"));
        let outside = root.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(
            outside.join("auth.enc"),
            crate::crypto::protect(br#"{"auth_mode":"chatgpt"}"#).unwrap(),
        )
        .unwrap();
        fs::write(outside.join("config.toml"), "model = \"outside\"\n").unwrap();

        let error = store.load_runtime_files("../outside").unwrap_err();

        assert!(error.contains("unsupported runtime id"), "{error}");
    }

    #[test]
    fn rejects_metadata_that_does_not_match_its_fixed_slot() {
        let home = tempdir().unwrap();
        fs::write(
            home.path().join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"fake"}}"#,
        )
        .unwrap();
        fs::write(home.path().join("config.toml"), "model = \"gpt\"\n").unwrap();
        let root = tempdir().unwrap();
        let store = RuntimeStore::new(root.path().join("runtimes"));
        store.import_plus_from_home(home.path(), false).unwrap();
        let metadata_path = store.runtime_dir(PLUS_RUNTIME_ID).join("runtime.json");
        let mut metadata: serde_json::Value =
            serde_json::from_slice(&fs::read(&metadata_path).unwrap()).unwrap();
        metadata["id"] = serde_json::Value::String(RELAY_RUNTIME_ID.to_string());
        fs::write(&metadata_path, serde_json::to_vec(&metadata).unwrap()).unwrap();

        let error = store.load_metadata(PLUS_RUNTIME_ID).unwrap_err();

        assert!(error.contains("does not match"), "{error}");
    }

    #[test]
    fn invalid_fixed_slot_metadata_blocks_relay_listing_and_overwrite() {
        for condition in ["missing", "corrupt", "mismatched"] {
            let home = tempdir().unwrap();
            fs::write(home.path().join("config.toml"), "model = \"live\"\n").unwrap();
            let root = tempdir().unwrap();
            let store = RuntimeStore::new(root.path().join("runtimes"));
            store
                .upsert_relay(
                    super::RelayRuntimeInput {
                        base_url: "https://old.example.com/v1".to_string(),
                        api_key: "sk-old-secret".to_string(),
                        model: "old".to_string(),
                    },
                    home.path(),
                )
                .unwrap();
            let runtime_dir = store.runtime_dir(RELAY_RUNTIME_ID);
            let metadata_path = runtime_dir.join("runtime.json");
            match condition {
                "missing" => fs::remove_file(&metadata_path).unwrap(),
                "corrupt" => fs::write(&metadata_path, b"not-json").unwrap(),
                "mismatched" => {
                    let mut metadata: serde_json::Value =
                        serde_json::from_slice(&fs::read(&metadata_path).unwrap()).unwrap();
                    metadata["id"] = serde_json::Value::String(PLUS_RUNTIME_ID.to_string());
                    fs::write(&metadata_path, serde_json::to_vec(&metadata).unwrap()).unwrap();
                }
                _ => unreachable!(),
            }
            let before = [
                fs::read(runtime_dir.join("auth.enc")).ok(),
                fs::read(runtime_dir.join("config.toml")).ok(),
                fs::read(&metadata_path).ok(),
            ];

            assert!(store.list_runtimes().is_err(), "{condition}");
            let error = store
                .upsert_relay(
                    super::RelayRuntimeInput {
                        base_url: "https://new.example.com/v1".to_string(),
                        api_key: "sk-new-secret".to_string(),
                        model: "new".to_string(),
                    },
                    home.path(),
                )
                .unwrap_err();

            assert!(!error.contains("sk-new-secret"), "{error}");
            assert_eq!(
                [
                    fs::read(runtime_dir.join("auth.enc")).ok(),
                    fs::read(runtime_dir.join("config.toml")).ok(),
                    fs::read(&metadata_path).ok(),
                ],
                before,
                "{condition}"
            );
            assert!(!runtime_dir.join("history").exists(), "{condition}");
        }
    }

    #[test]
    fn corrupt_plus_metadata_blocks_confirmed_overwrite() {
        let home = tempdir().unwrap();
        fs::write(
            home.path().join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"first"}}"#,
        )
        .unwrap();
        fs::write(home.path().join("config.toml"), "model = \"old\"\n").unwrap();
        let root = tempdir().unwrap();
        let store = RuntimeStore::new(root.path().join("runtimes"));
        store.import_plus_from_home(home.path(), false).unwrap();
        let runtime_dir = store.runtime_dir(PLUS_RUNTIME_ID);
        let metadata_path = runtime_dir.join("runtime.json");
        fs::write(&metadata_path, b"not-json").unwrap();
        let before = [
            fs::read(runtime_dir.join("auth.enc")).unwrap(),
            fs::read(runtime_dir.join("config.toml")).unwrap(),
            fs::read(&metadata_path).unwrap(),
        ];
        fs::write(
            home.path().join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"second"}}"#,
        )
        .unwrap();

        let error = store.import_plus_from_home(home.path(), true).unwrap_err();

        assert!(error.contains("runtime metadata"), "{error}");
        assert_eq!(fs::read(runtime_dir.join("auth.enc")).unwrap(), before[0]);
        assert_eq!(
            fs::read(runtime_dir.join("config.toml")).unwrap(),
            before[1]
        );
        assert_eq!(fs::read(&metadata_path).unwrap(), before[2]);
        assert!(!runtime_dir.join("history").exists());
    }

    #[test]
    fn runtime_write_failure_restores_all_previous_files() {
        let home = tempdir().unwrap();
        fs::write(home.path().join("config.toml"), "model = \"old\"\n").unwrap();
        let root = tempdir().unwrap();
        let store = RuntimeStore::new(root.path().join("runtimes"));
        store
            .upsert_relay(
                super::RelayRuntimeInput {
                    base_url: "https://old.example.com/v1".to_string(),
                    api_key: "sk-old-secret".to_string(),
                    model: "old".to_string(),
                },
                home.path(),
            )
            .unwrap();
        let runtime_dir = store.runtime_dir(RELAY_RUNTIME_ID);
        let before = [
            fs::read(runtime_dir.join("auth.enc")).unwrap(),
            fs::read(runtime_dir.join("config.toml")).unwrap(),
            fs::read(runtime_dir.join("runtime.json")).unwrap(),
            fs::read(runtime_dir.join(super::RUNTIME_BUNDLE_MANIFEST)).unwrap(),
        ];
        let mut metadata = store.load_metadata(RELAY_RUNTIME_ID).unwrap();
        metadata.base_url = Some("https://new.example.com/v1".to_string());
        metadata.model = Some("new".to_string());
        let mut writes = 0;

        let error = store
            .write_runtime_with(
                &metadata,
                br#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-new-secret"}"#,
                "model = \"new\"\n",
                |path, bytes| {
                    writes += 1;
                    if writes == 2 {
                        Err("injected config write failure".to_string())
                    } else {
                        crate::file_ops::atomic_write(path, bytes)
                    }
                },
            )
            .unwrap_err();

        assert!(error.contains("rolled back"), "{error}");
        assert!(!error.contains("sk-new-secret"), "{error}");
        assert_eq!(fs::read(runtime_dir.join("auth.enc")).unwrap(), before[0]);
        assert_eq!(
            fs::read(runtime_dir.join("config.toml")).unwrap(),
            before[1]
        );
        assert_eq!(
            fs::read(runtime_dir.join("runtime.json")).unwrap(),
            before[2]
        );
        assert_eq!(
            fs::read(runtime_dir.join(super::RUNTIME_BUNDLE_MANIFEST)).unwrap(),
            before[3]
        );
    }

    #[test]
    fn metadata_bundle_failure_restores_the_previous_committed_pair() {
        let home = tempdir().unwrap();
        fs::write(home.path().join("config.toml"), "model = \"old\"\n").unwrap();
        let root = tempdir().unwrap();
        let store = RuntimeStore::new(root.path().join("runtimes"));
        store
            .upsert_relay(
                super::RelayRuntimeInput {
                    base_url: "https://relay.example.com/v1".to_string(),
                    api_key: "sk-old-secret".to_string(),
                    model: "relay-model".to_string(),
                },
                home.path(),
            )
            .unwrap();
        let runtime_dir = store.runtime_dir(RELAY_RUNTIME_ID);
        let metadata_path = runtime_dir.join("runtime.json");
        let bundle_path = runtime_dir.join(super::RUNTIME_BUNDLE_MANIFEST);
        let before_metadata = fs::read(&metadata_path).unwrap();
        let before_bundle = fs::read(&bundle_path).unwrap();
        let mut metadata = store.load_metadata(RELAY_RUNTIME_ID).unwrap();
        metadata.last_verified_at_ms = Some(123);
        let mut writes = 0;

        let error = store
            .write_metadata_and_bundle_with(RELAY_RUNTIME_ID, &metadata, |path, bytes| {
                writes += 1;
                if writes == 2 {
                    Err("injected bundle write failure".to_string())
                } else {
                    crate::file_ops::atomic_write(path, bytes)
                }
            })
            .unwrap_err();

        assert!(error.contains("rolled back"), "{error}");
        assert_eq!(fs::read(&metadata_path).unwrap(), before_metadata);
        assert_eq!(fs::read(&bundle_path).unwrap(), before_bundle);
        assert!(store.load_relay_connection().is_ok());
    }

    #[test]
    fn failed_first_runtime_write_removes_every_new_file() {
        let root = tempdir().unwrap();
        let store = RuntimeStore::new(root.path().join("runtimes"));
        let metadata = super::RuntimeMetadata {
            id: RELAY_RUNTIME_ID.to_string(),
            name: "API 中转站".to_string(),
            kind: RuntimeKind::Relay,
            base_url: Some("https://relay.example.com/v1".to_string()),
            model: Some("new".to_string()),
            created_at_ms: 1,
            last_used_at_ms: None,
            last_verified_at_ms: None,
        };
        let runtime_dir = store.runtime_dir(RELAY_RUNTIME_ID);
        let mut writes = 0;

        let error = store
            .write_runtime_with(
                &metadata,
                br#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-new-secret"}"#,
                "model = \"new\"\n",
                |path, bytes| {
                    writes += 1;
                    if writes == 2 {
                        Err("injected config write failure".to_string())
                    } else {
                        crate::file_ops::atomic_write(path, bytes)
                    }
                },
            )
            .unwrap_err();

        assert!(error.contains("rolled back"), "{error}");
        for name in [
            "auth.enc",
            "config.toml",
            "runtime.json",
            super::RUNTIME_BUNDLE_MANIFEST,
            super::RUNTIME_BUNDLE_MARKER,
        ] {
            assert!(!runtime_dir.join(name).exists(), "{name}");
        }
    }
}

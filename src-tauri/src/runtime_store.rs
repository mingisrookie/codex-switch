use std::{
    fs,
    path::{Path, PathBuf},
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use toml_edit::{value, DocumentMut, Item, Table};

use crate::crypto::{protect, unprotect};

pub const PLUS_RUNTIME_ID: &str = "plus";
pub const RELAY_RUNTIME_ID: &str = "relay";
const RELAY_PROVIDER_ID: &str = "openai_custom";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RuntimeKind {
    Plus,
    Relay,
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
    pub last_used_at_ms: Option<u128>,
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

#[derive(Debug, Clone)]
pub struct RuntimeStore {
    root: PathBuf,
}

impl RuntimeStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn default() -> Result<Self, String> {
        Ok(Self::new(default_store_root()?.join("runtimes")))
    }

    pub fn runtime_dir(&self, runtime_id: &str) -> PathBuf {
        self.root.join(runtime_id)
    }

    pub fn list_runtimes(&self) -> Result<Vec<RuntimeMetadata>, String> {
        let mut runtimes = Vec::new();
        for id in [PLUS_RUNTIME_ID, RELAY_RUNTIME_ID] {
            let meta_path = self.runtime_dir(id).join("runtime.json");
            if meta_path.exists() {
                runtimes.push(read_metadata(&meta_path)?);
            }
        }
        Ok(runtimes)
    }

    pub fn import_plus_from_home(&self, codex_home: &Path) -> Result<RuntimeMetadata, String> {
        let auth = fs::read(codex_home.join("auth.json"))
            .map_err(|error| format!("failed to read plus auth.json: {error}"))?;
        let config = fs::read_to_string(codex_home.join("config.toml"))
            .map_err(|error| format!("failed to read plus config.toml: {error}"))?;
        let created_at_ms = self
            .load_metadata(PLUS_RUNTIME_ID)
            .ok()
            .map(|metadata| metadata.created_at_ms)
            .unwrap_or(timestamp_millis()?);
        let metadata = RuntimeMetadata {
            id: PLUS_RUNTIME_ID.to_string(),
            name: "Codex 账号".to_string(),
            kind: RuntimeKind::Plus,
            base_url: None,
            model: read_model_from_config(&config),
            created_at_ms,
            last_used_at_ms: None,
        };
        self.write_runtime(&metadata, &auth, &config)?;
        Ok(metadata)
    }

    pub fn upsert_relay(&self, input: RelayRuntimeInput, codex_home: &Path) -> Result<RuntimeMetadata, String> {
        let normalized_base_url = normalize_base_url(&input.base_url)?;
        let base_config = fs::read_to_string(codex_home.join("config.toml")).unwrap_or_default();
        let config_toml = relay_config_template(&base_config, &normalized_base_url, &input.model)?;
        let auth = relay_auth_json(&input.api_key)?;
        let created_at_ms = self
            .load_metadata(RELAY_RUNTIME_ID)
            .ok()
            .map(|metadata| metadata.created_at_ms)
            .unwrap_or(timestamp_millis()?);
        let metadata = RuntimeMetadata {
            id: RELAY_RUNTIME_ID.to_string(),
            name: "API 中转站".to_string(),
            kind: RuntimeKind::Relay,
            base_url: Some(normalized_base_url),
            model: Some(input.model),
            created_at_ms,
            last_used_at_ms: None,
        };
        self.write_runtime(&metadata, &auth, &config_toml)?;
        Ok(metadata)
    }

    pub fn load_runtime_files(&self, runtime_id: &str) -> Result<RuntimeFiles, String> {
        let dir = self.runtime_dir(runtime_id);
        let encrypted = fs::read(dir.join("auth.enc"))
            .map_err(|error| format!("failed to read runtime auth: {error}"))?;
        let config_toml = fs::read_to_string(dir.join("config.toml"))
            .map_err(|error| format!("failed to read runtime config: {error}"))?;
        let auth_json = unprotect(&encrypted)?;
        Ok(RuntimeFiles {
            auth_json,
            config_toml,
        })
    }

    pub fn load_metadata(&self, runtime_id: &str) -> Result<RuntimeMetadata, String> {
        read_metadata(&self.runtime_dir(runtime_id).join("runtime.json"))
    }

    fn write_runtime(&self, metadata: &RuntimeMetadata, auth_json: &[u8], config_toml: &str) -> Result<(), String> {
        let dir = self.runtime_dir(&metadata.id);
        fs::create_dir_all(&dir).map_err(|error| format!("failed to create runtime dir: {error}"))?;
        fs::write(dir.join("auth.enc"), protect(auth_json)?)
            .map_err(|error| format!("failed to write encrypted runtime auth: {error}"))?;
        fs::write(dir.join("config.toml"), config_toml)
            .map_err(|error| format!("failed to write runtime config: {error}"))?;
        write_metadata(&dir.join("runtime.json"), metadata)
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
    let with_scheme = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    if with_scheme.ends_with("/v1") {
        Ok(with_scheme)
    } else {
        Ok(format!("{with_scheme}/v1"))
    }
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

fn relay_config_template(base_config: &str, base_url: &str, model: &str) -> Result<String, String> {
    let mut doc = if base_config.trim().is_empty() {
        DocumentMut::new()
    } else {
        DocumentMut::from_str(base_config)
            .map_err(|error| format!("failed to parse config.toml: {error}"))?
    };

    doc["model"] = value(model);
    doc["model_provider"] = value(RELAY_PROVIDER_ID);

    let provider = provider_table_mut(&mut doc, RELAY_PROVIDER_ID)?;
    provider["name"] = value(RELAY_PROVIDER_ID);
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

fn provider_table_mut<'a>(doc: &'a mut DocumentMut, provider: &str) -> Result<&'a mut Table, String> {
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
        .and_then(|doc| doc.get("model").and_then(toml_edit::Item::as_str).map(str::to_string))
}

fn write_metadata(path: &Path, metadata: &RuntimeMetadata) -> Result<(), String> {
    let json = serde_json::to_string_pretty(metadata)
        .map_err(|error| format!("failed to serialize runtime metadata: {error}"))?;
    fs::write(path, json).map_err(|error| format!("failed to write runtime metadata: {error}"))
}

fn read_metadata(path: &Path) -> Result<RuntimeMetadata, String> {
    let raw = fs::read_to_string(path).map_err(|error| format!("failed to read runtime metadata: {error}"))?;
    serde_json::from_str(&raw).map_err(|error| format!("failed to parse runtime metadata: {error}"))
}

fn timestamp_millis() -> Result<u128, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .map_err(|error| format!("system clock before unix epoch: {error}"))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{RuntimeKind, RuntimeStore, PLUS_RUNTIME_ID, RELAY_RUNTIME_ID};

    #[test]
    fn imports_plus_runtime_as_encrypted_auth_and_full_config() {
        let home = tempdir().unwrap();
        fs::write(home.path().join("auth.json"), r#"{"auth_mode":"chatgpt","tokens":{"access_token":"fake-plus"}}"#).unwrap();
        fs::write(home.path().join("config.toml"), "model = \"gpt-5.5\"\nmodel_instructions_file = \"global\"\n").unwrap();
        let root = tempdir().unwrap();
        let store = RuntimeStore::new(root.path().join("runtimes"));

        let metadata = store.import_plus_from_home(home.path()).unwrap();

        assert_eq!(metadata.id, PLUS_RUNTIME_ID);
        assert_eq!(metadata.kind, RuntimeKind::Plus);
        assert_eq!(metadata.model.as_deref(), Some("gpt-5.5"));
        let encrypted = fs::read(store.runtime_dir(PLUS_RUNTIME_ID).join("auth.enc")).unwrap();
        assert!(!String::from_utf8_lossy(&encrypted).contains("fake-plus"));
        let files = store.load_runtime_files(PLUS_RUNTIME_ID).unwrap();
        assert!(String::from_utf8(files.auth_json).unwrap().contains("fake-plus"));
        assert!(files.config_toml.contains("model_instructions_file"));
    }

    #[test]
    fn upserts_single_relay_runtime_with_normalized_url_and_encrypted_key() {
        let home = tempdir().unwrap();
        fs::write(home.path().join("config.toml"), "model = \"gpt-5.5\"\nmodel_instructions_file = \"global\"\n").unwrap();
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
        assert_eq!(metadata.base_url.as_deref(), Some("https://relay.example.com/v1"));
        let encrypted = fs::read(store.runtime_dir(RELAY_RUNTIME_ID).join("auth.enc")).unwrap();
        assert!(!String::from_utf8_lossy(&encrypted).contains("sk-fake-relay"));
        let stored_config = fs::read_to_string(store.runtime_dir(RELAY_RUNTIME_ID).join("config.toml")).unwrap();
        assert!(!stored_config.contains("sk-fake-relay"));
        let files = store.load_runtime_files(RELAY_RUNTIME_ID).unwrap();
        let auth = String::from_utf8(files.auth_json).unwrap();
        assert!(auth.contains("\"auth_mode\":\"apikey\""));
        assert!(auth.contains("sk-fake-relay"));
        assert!(files.config_toml.contains("model_provider = \"openai_custom\""));
        assert!(files.config_toml.contains("[model_providers.openai_custom]"));
        assert!(files.config_toml.contains("name = \"openai_custom\""));
        assert!(files.config_toml.contains("wire_api = \"responses\""));
        assert!(files.config_toml.contains("supports_websockets = false"));
        assert!(files.config_toml.contains("request_max_retries = 6"));
        assert!(files.config_toml.contains("stream_max_retries = 3"));
        assert!(files.config_toml.contains("stream_idle_timeout_ms = 180000"));
        assert!(!files.config_toml.contains("env_key ="));
        assert!(!files.config_toml.contains("api_key ="));
        assert!(!files.config_toml.contains("goal ="));
        assert!(files.config_toml.contains("base_url = \"https://relay.example.com/v1\""));
        assert!(files.config_toml.contains("model_instructions_file"));
        assert_eq!(store.list_runtimes().unwrap().len(), 1);
    }
}

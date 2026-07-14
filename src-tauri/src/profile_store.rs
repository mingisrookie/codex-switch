use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::crypto::{protect, unprotect};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ProfileKind {
    ChatGpt,
    ApiKey,
    ApiProvider,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProfileMetadata {
    pub id: String,
    pub name: String,
    pub kind: ProfileKind,
    pub provider_label: Option<String>,
    pub auth_mode: Option<String>,
    pub source_home: Option<PathBuf>,
    pub created_at_ms: u128,
    pub last_used_at_ms: Option<u128>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ApiProfileInput {
    pub name: String,
    pub provider_label: String,
    pub base_url: String,
    pub env_key: String,
    pub model: String,
    pub api_key: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ProfileStore {
    root: PathBuf,
}

impl ProfileStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn from_default_root() -> Result<Self, String> {
        Ok(Self::new(default_store_root()?))
    }

    pub fn profile_dir(&self, profile_id: &str) -> PathBuf {
        self.root.join("profiles").join(profile_id)
    }

    pub fn list_profiles(&self) -> Result<Vec<ProfileMetadata>, String> {
        let profiles_root = self.root.join("profiles");
        if !profiles_root.exists() {
            return Ok(Vec::new());
        }

        let mut profiles = Vec::new();
        for entry in fs::read_dir(profiles_root)
            .map_err(|error| format!("failed to read profiles: {error}"))?
        {
            let entry = entry.map_err(|error| format!("failed to read profile entry: {error}"))?;
            let meta_path = entry.path().join("profile.json");
            if meta_path.exists() {
                profiles.push(read_profile_metadata(&meta_path)?);
            }
        }
        profiles.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(profiles)
    }

    pub fn import_current_profile(
        &self,
        name: &str,
        kind: ProfileKind,
        codex_home: &Path,
    ) -> Result<ProfileMetadata, String> {
        let auth_path = codex_home.join("auth.json");
        let config_path = codex_home.join("config.toml");
        let auth_bytes =
            fs::read(&auth_path).map_err(|error| format!("failed to read auth.json: {error}"))?;
        let auth_mode = serde_json::from_slice::<serde_json::Value>(&auth_bytes)
            .ok()
            .and_then(|value| {
                value
                    .get("auth_mode")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            });
        let profile = ProfileMetadata {
            id: make_profile_id(name)?,
            name: name.to_string(),
            kind,
            provider_label: None,
            auth_mode,
            source_home: Some(codex_home.to_path_buf()),
            created_at_ms: timestamp_millis()?,
            last_used_at_ms: None,
        };
        let dir = self.profile_dir(&profile.id);
        fs::create_dir_all(&dir)
            .map_err(|error| format!("failed to create profile dir: {error}"))?;
        fs::write(dir.join("auth.enc"), protect(&auth_bytes)?)
            .map_err(|error| format!("failed to write encrypted auth: {error}"))?;
        if config_path.exists() {
            fs::copy(&config_path, dir.join("config-full.toml"))
                .map_err(|error| format!("failed to copy config snapshot: {error}"))?;
        }
        write_profile_metadata(&dir.join("profile.json"), &profile)?;
        Ok(profile)
    }

    pub fn create_api_profile(&self, input: ApiProfileInput) -> Result<ProfileMetadata, String> {
        let profile = ProfileMetadata {
            id: make_profile_id(&input.name)?,
            name: input.name,
            kind: ProfileKind::ApiProvider,
            provider_label: Some(input.provider_label.clone()),
            auth_mode: Some("apikey".to_string()),
            source_home: None,
            created_at_ms: timestamp_millis()?,
            last_used_at_ms: None,
        };
        let dir = self.profile_dir(&profile.id);
        fs::create_dir_all(&dir)
            .map_err(|error| format!("failed to create profile dir: {error}"))?;
        let auth = api_auth_json(input.env_key.as_str(), input.api_key.as_deref())?;
        fs::write(dir.join("auth.enc"), protect(&auth)?)
            .map_err(|error| format!("failed to write encrypted auth: {error}"))?;
        let overlay = format!(
            "model = \"{}\"\nmodel_provider = \"{}\"\n\n[model_providers.{}]\nbase_url = \"{}\"\nenv_key = \"{}\"\nwire_api = \"responses\"\n",
            input.model, input.provider_label, input.provider_label, input.base_url, input.env_key
        );
        fs::write(dir.join("config-overlay.toml"), overlay)
            .map_err(|error| format!("failed to write config overlay: {error}"))?;
        write_profile_metadata(&dir.join("profile.json"), &profile)?;
        Ok(profile)
    }

    pub fn load_profile(&self, profile_id: &str) -> Result<ProfileMetadata, String> {
        read_profile_metadata(&self.profile_dir(profile_id).join("profile.json"))
    }

    pub fn load_auth_plaintext(&self, profile_id: &str) -> Result<Vec<u8>, String> {
        let encrypted = fs::read(self.profile_dir(profile_id).join("auth.enc"))
            .map_err(|error| format!("failed to read encrypted auth: {error}"))?;
        unprotect(&encrypted)
    }

    pub fn load_overlay(&self, profile_id: &str) -> Result<Option<String>, String> {
        let path = self.profile_dir(profile_id).join("config-overlay.toml");
        if !path.exists() {
            return Ok(None);
        }
        fs::read_to_string(path)
            .map(Some)
            .map_err(|error| format!("failed to read profile overlay: {error}"))
    }
}

fn api_auth_json(env_key: &str, api_key: Option<&str>) -> Result<Vec<u8>, String> {
    let mut auth = serde_json::Map::new();
    auth.insert(
        "auth_mode".to_string(),
        serde_json::Value::String("apikey".to_string()),
    );
    if let Some(api_key) = api_key.map(str::trim).filter(|api_key| !api_key.is_empty()) {
        auth.insert(
            "OPENAI_API_KEY".to_string(),
            serde_json::Value::String(api_key.to_string()),
        );
        if env_key != "OPENAI_API_KEY" {
            auth.insert(
                env_key.to_string(),
                serde_json::Value::String(api_key.to_string()),
            );
        }
    } else {
        auth.insert("OPENAI_API_KEY".to_string(), serde_json::Value::Null);
    }
    serde_json::to_vec(&serde_json::Value::Object(auth))
        .map_err(|error| format!("failed to serialize API auth placeholder: {error}"))
}

pub fn default_store_root() -> Result<PathBuf, String> {
    let appdata = std::env::var_os("APPDATA").ok_or_else(|| "APPDATA is not set".to_string())?;
    Ok(PathBuf::from(appdata).join("codex-switch"))
}

fn write_profile_metadata(path: &Path, profile: &ProfileMetadata) -> Result<(), String> {
    let json = serde_json::to_string_pretty(profile)
        .map_err(|error| format!("failed to serialize profile metadata: {error}"))?;
    fs::write(path, json).map_err(|error| format!("failed to write profile metadata: {error}"))
}

fn read_profile_metadata(path: &Path) -> Result<ProfileMetadata, String> {
    let raw = fs::read_to_string(path)
        .map_err(|error| format!("failed to read profile metadata: {error}"))?;
    serde_json::from_str(&raw).map_err(|error| format!("failed to parse profile metadata: {error}"))
}

fn make_profile_id(name: &str) -> Result<String, String> {
    let now = timestamp_millis()?;
    let mut hasher = Sha256::new();
    hasher.update(name.as_bytes());
    hasher.update(now.to_le_bytes());
    let hash = hasher.finalize();
    let slug = name
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_')
        .collect::<String>();
    let prefix = if slug.is_empty() { "profile" } else { &slug };
    Ok(format!(
        "{prefix}-{:02x}{:02x}{:02x}{:02x}",
        hash[0], hash[1], hash[2], hash[3]
    ))
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

    use super::{ApiProfileInput, ProfileKind, ProfileStore};

    #[test]
    fn imports_current_profile_with_encrypted_auth() {
        let home = tempdir().unwrap();
        fs::write(
            home.path().join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"fake"}}"#,
        )
        .unwrap();
        fs::write(home.path().join("config.toml"), "model = \"gpt-5.5\"\n").unwrap();
        let store_root = tempdir().unwrap();
        let store = ProfileStore::new(store_root.path().to_path_buf());

        let profile = store
            .import_current_profile("主账号", ProfileKind::ChatGpt, home.path())
            .unwrap();

        let auth_enc = fs::read(store.profile_dir(&profile.id).join("auth.enc")).unwrap();
        assert!(!String::from_utf8_lossy(&auth_enc).contains("access_token"));
        assert_eq!(
            store.load_auth_plaintext(&profile.id).unwrap(),
            fs::read(home.path().join("auth.json")).unwrap()
        );
        assert_eq!(store.list_profiles().unwrap().len(), 1);
    }

    #[test]
    fn creates_api_profile_with_provider_overlay() {
        let store_root = tempdir().unwrap();
        let store = ProfileStore::new(store_root.path().to_path_buf());

        let profile = store
            .create_api_profile(ApiProfileInput {
                name: "官方直连".to_string(),
                provider_label: "direct-account".to_string(),
                base_url: "https://api.openai.com/v1".to_string(),
                env_key: "OPENAI_API_KEY".to_string(),
                model: "gpt-5.5".to_string(),
                api_key: None,
            })
            .unwrap();

        assert_eq!(profile.kind, ProfileKind::ApiProvider);
        let overlay =
            fs::read_to_string(store.profile_dir(&profile.id).join("config-overlay.toml")).unwrap();
        assert!(overlay.contains("model_provider = \"direct-account\""));
        assert!(overlay.contains("base_url = \"https://api.openai.com/v1\""));
    }

    #[test]
    fn creates_api_profile_with_encrypted_api_key_when_provided() {
        let store_root = tempdir().unwrap();
        let store = ProfileStore::new(store_root.path().to_path_buf());

        let profile = store
            .create_api_profile(ApiProfileInput {
                name: "自定义 API".to_string(),
                provider_label: "custom-account".to_string(),
                base_url: "https://example.invalid/v1".to_string(),
                env_key: "CUSTOM_API_KEY".to_string(),
                model: "gpt-5.5".to_string(),
                api_key: Some("sk-fake-secret".to_string()),
            })
            .unwrap();

        let encrypted = fs::read(store.profile_dir(&profile.id).join("auth.enc")).unwrap();
        assert!(!String::from_utf8_lossy(&encrypted).contains("sk-fake-secret"));
        let auth = String::from_utf8(store.load_auth_plaintext(&profile.id).unwrap()).unwrap();
        assert!(auth.contains("\"auth_mode\":\"apikey\""));
        assert!(auth.contains("\"OPENAI_API_KEY\":\"sk-fake-secret\""));
        assert!(auth.contains("\"CUSTOM_API_KEY\":\"sk-fake-secret\""));
    }
}

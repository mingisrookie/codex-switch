use std::{fs, path::Path};

use serde::Serialize;

use crate::{
    backup::{create_backup, BackupManifest},
    config_patch::{plan_config_patch, ConfigOverlay},
    profile_store::ProfileStore,
};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SwitchResult {
    pub profile_id: String,
    pub backup: BackupManifest,
    pub changed_config_keys: Vec<String>,
}

pub fn switch_profile_files(
    store: &ProfileStore,
    profile_id: &str,
    codex_home: &Path,
    backup_root: &Path,
) -> Result<SwitchResult, String> {
    let backup = create_backup(codex_home, backup_root, "switch-profile")?;
    let auth = store.load_auth_plaintext(profile_id)?;
    let profile = store.load_profile(profile_id)?;

    let auth_path = codex_home.join("auth.json");
    let tmp_auth = codex_home.join("auth.json.codex-switch.tmp");
    fs::write(&tmp_auth, auth)
        .map_err(|error| format!("failed to write temporary auth.json: {error}"))?;
    fs::rename(&tmp_auth, &auth_path)
        .map_err(|error| format!("failed to replace auth.json: {error}"))?;

    let config_path = codex_home.join("config.toml");
    let mut changed_config_keys = Vec::new();
    if let Some(overlay_raw) = store.load_overlay(profile_id)? {
        let overlay = overlay_from_toml(&overlay_raw)?;
        let base = fs::read_to_string(&config_path).unwrap_or_default();
        let plan = plan_config_patch(&base, &overlay)?;
        let tmp_config = codex_home.join("config.toml.codex-switch.tmp");
        fs::write(&tmp_config, plan.patched_toml)
            .map_err(|error| format!("failed to write temporary config.toml: {error}"))?;
        fs::rename(&tmp_config, &config_path)
            .map_err(|error| format!("failed to replace config.toml: {error}"))?;
        changed_config_keys = plan.changed_keys;
    }

    Ok(SwitchResult {
        profile_id: profile.id,
        backup,
        changed_config_keys,
    })
}

fn overlay_from_toml(raw: &str) -> Result<ConfigOverlay, String> {
    let value = raw
        .parse::<toml_edit::DocumentMut>()
        .map_err(|error| format!("failed to parse config overlay: {error}"))?;
    let model = value
        .get("model")
        .and_then(toml_edit::Item::as_str)
        .map(str::to_string);
    let model_provider = value
        .get("model_provider")
        .and_then(toml_edit::Item::as_str)
        .map(str::to_string);
    let service_tier = value
        .get("service_tier")
        .and_then(toml_edit::Item::as_str)
        .map(str::to_string);

    let (provider_base_url, env_key) = if let Some(provider) = model_provider.as_deref() {
        let provider_table = value
            .get("model_providers")
            .and_then(|item| item.get(provider));
        (
            provider_table
                .and_then(|item| item.get("base_url"))
                .and_then(toml_edit::Item::as_str)
                .map(str::to_string),
            provider_table
                .and_then(|item| item.get("env_key"))
                .and_then(toml_edit::Item::as_str)
                .map(str::to_string),
        )
    } else {
        (None, None)
    };

    Ok(ConfigOverlay {
        model,
        model_provider,
        service_tier,
        env_key,
        provider_base_url,
    })
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use tempfile::tempdir;

    use crate::profile_store::{ApiProfileInput, ProfileKind, ProfileStore};

    use super::switch_profile_files;

    #[test]
    fn replaces_auth_and_patches_config_with_backup() {
        let home = tempdir().unwrap();
        fs::write(
            home.path().join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"fake-old"}}"#,
        )
        .unwrap();
        fs::write(
            home.path().join("config.toml"),
            "model = \"gpt-5.5\"\nmodel_instructions_file = \"C:\\\\Users\\\\admin\\\\.codex\\\\instruction.md\"\n",
        )
        .unwrap();
        let store_root = tempdir().unwrap();
        let store = ProfileStore::new(store_root.path().to_path_buf());
        let profile = store
            .create_api_profile(ApiProfileInput {
                name: "API".to_string(),
                provider_label: "direct-account".to_string(),
                base_url: "https://api.openai.com/v1".to_string(),
                env_key: "OPENAI_API_KEY".to_string(),
                model: "gpt-5.4-mini".to_string(),
                api_key: None,
            })
            .unwrap();
        assert_eq!(profile.kind, ProfileKind::ApiProvider);
        let backup_root = tempdir().unwrap();

        let result =
            switch_profile_files(&store, &profile.id, home.path(), backup_root.path()).unwrap();

        let auth_backup = result
            .backup
            .files
            .iter()
            .find(|file| file.relative_path == Path::new("auth.json"))
            .expect("auth.json must be present in the encrypted backup manifest");
        assert!(auth_backup.encrypted);
        assert!(auth_backup.backup_path.exists());
        assert!(!result.backup.backup_dir.join("auth.json").exists());
        assert!(fs::read_to_string(home.path().join("auth.json"))
            .unwrap()
            .contains("\"auth_mode\":\"apikey\""));
        let config = fs::read_to_string(home.path().join("config.toml")).unwrap();
        assert!(config.contains("model = \"gpt-5.4-mini\""));
        assert!(config.contains("model_instructions_file"));
        assert!(config.contains("[model_providers.direct-account]"));
    }
}

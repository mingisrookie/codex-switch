use std::str::FromStr;

use serde::Serialize;
use toml_edit::{value, DocumentMut, Item, Table};

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConfigOverlay {
    pub model: Option<String>,
    pub model_provider: Option<String>,
    pub service_tier: Option<String>,
    pub env_key: Option<String>,
    pub provider_base_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConfigPatchPlan {
    pub patched_toml: String,
    pub changed_keys: Vec<String>,
}

pub fn plan_config_patch(base_toml: &str, overlay: &ConfigOverlay) -> Result<ConfigPatchPlan, String> {
    if overlay == &ConfigOverlay::default() {
        return Ok(ConfigPatchPlan {
            patched_toml: base_toml.to_string(),
            changed_keys: Vec::new(),
        });
    }

    let mut doc = DocumentMut::from_str(base_toml)
        .map_err(|error| format!("failed to parse config.toml: {error}"))?;
    let mut changed_keys = Vec::new();

    set_top_level_string(&mut doc, "model", overlay.model.as_deref(), &mut changed_keys);
    set_top_level_string(
        &mut doc,
        "model_provider",
        overlay.model_provider.as_deref(),
        &mut changed_keys,
    );
    set_top_level_string(
        &mut doc,
        "service_tier",
        overlay.service_tier.as_deref(),
        &mut changed_keys,
    );

    if overlay.provider_base_url.is_some() || overlay.env_key.is_some() {
        let provider = overlay
            .model_provider
            .as_deref()
            .ok_or_else(|| "model_provider is required when patching provider settings".to_string())?;
        let table = provider_table_mut(&mut doc, provider)?;

        if let Some(base_url) = overlay.provider_base_url.as_deref() {
            set_table_string(table, "base_url", base_url, "model_providers.base_url", &mut changed_keys);
        }
        if let Some(env_key) = overlay.env_key.as_deref() {
            set_table_string(table, "env_key", env_key, "model_providers.env_key", &mut changed_keys);
        }
    }

    Ok(ConfigPatchPlan {
        patched_toml: doc.to_string(),
        changed_keys,
    })
}

fn set_top_level_string(
    doc: &mut DocumentMut,
    key: &str,
    next: Option<&str>,
    changed_keys: &mut Vec<String>,
) {
    let Some(next) = next else {
        return;
    };

    if doc.get(key).and_then(Item::as_str) != Some(next) {
        doc[key] = value(next);
        changed_keys.push(key.to_string());
    }
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

fn set_table_string(
    table: &mut Table,
    key: &str,
    next: &str,
    changed_name: &str,
    changed_keys: &mut Vec<String>,
) {
    if table.get(key).and_then(Item::as_str) != Some(next) {
        table[key] = value(next);
        changed_keys.push(changed_name.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::{plan_config_patch, ConfigOverlay};

    #[test]
    fn patches_login_bound_fields_while_preserving_global_config() {
        let base = r#"
model = "gpt-5.5"
model_instructions_file = "C:\\Users\\admin\\.codex\\instruction.md"

[features]
fast_mode = true
guardian_approval = true
"#;

        let plan = plan_config_patch(
            base,
            &ConfigOverlay {
                model: Some("gpt-5.4-mini".to_string()),
                model_provider: Some("direct-account".to_string()),
                service_tier: Some("fast".to_string()),
                env_key: Some("CODEX_SWITCH_API_KEY".to_string()),
                provider_base_url: Some("https://api.openai.com/v1".to_string()),
            },
        )
        .unwrap();

        assert!(plan.changed_keys.contains(&"model".to_string()));
        assert!(plan.changed_keys.contains(&"model_provider".to_string()));
        assert!(plan.patched_toml.contains("model = \"gpt-5.4-mini\""));
        assert!(plan.patched_toml.contains("model_provider = \"direct-account\""));
        assert!(plan.patched_toml.contains("model_instructions_file = \"C:\\\\Users\\\\admin\\\\.codex\\\\instruction.md\""));
        assert!(plan.patched_toml.contains("fast_mode = true"));
        assert!(plan.patched_toml.contains("[model_providers.direct-account]"));
        assert!(plan.patched_toml.contains("base_url = \"https://api.openai.com/v1\""));
        assert!(plan.patched_toml.contains("env_key = \"CODEX_SWITCH_API_KEY\""));
    }

    #[test]
    fn empty_overlay_returns_unchanged_toml_and_no_changes() {
        let base = "model = \"gpt-5.5\"\\n";
        let plan = plan_config_patch(base, &ConfigOverlay::default()).unwrap();

        assert_eq!(plan.patched_toml, base);
        assert!(plan.changed_keys.is_empty());
    }
}

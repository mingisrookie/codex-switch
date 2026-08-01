use std::str::FromStr;

use serde::Serialize;
use toml_edit::{value, DocumentMut, Item, Table};

pub const MANAGED_RELAY_PROVIDER_ID: &str = "openai_custom";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqliteHomePatch {
    Keep,
    Set(String),
    Remove,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeConfigKind {
    Account,
    Relay,
}

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

pub fn plan_config_patch(
    base_toml: &str,
    overlay: &ConfigOverlay,
) -> Result<ConfigPatchPlan, String> {
    if overlay == &ConfigOverlay::default() {
        return Ok(ConfigPatchPlan {
            patched_toml: base_toml.to_string(),
            changed_keys: Vec::new(),
        });
    }

    let mut doc =
        DocumentMut::from_str(base_toml).map_err(|_| "failed to parse config.toml".to_string())?;
    let mut changed_keys = Vec::new();

    set_top_level_string(
        &mut doc,
        "model",
        overlay.model.as_deref(),
        &mut changed_keys,
    );
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
        let provider = overlay.model_provider.as_deref().ok_or_else(|| {
            "model_provider is required when patching provider settings".to_string()
        })?;
        let table = provider_table_mut(&mut doc, provider)?;

        if let Some(base_url) = overlay.provider_base_url.as_deref() {
            set_table_string(
                table,
                "base_url",
                base_url,
                "model_providers.base_url",
                &mut changed_keys,
            );
        }
        if let Some(env_key) = overlay.env_key.as_deref() {
            set_table_string(
                table,
                "env_key",
                env_key,
                "model_providers.env_key",
                &mut changed_keys,
            );
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

pub fn plan_runtime_config_patch(
    live_toml: &str,
    runtime_toml: &str,
    kind: RuntimeConfigKind,
    relay_bearer_token: Option<&str>,
) -> Result<ConfigPatchPlan, String> {
    let mut live = DocumentMut::from_str(live_toml)
        .map_err(|_| "failed to parse live config.toml".to_string())?;
    let runtime = DocumentMut::from_str(runtime_toml)
        .map_err(|_| "failed to parse runtime config.toml".to_string())?;
    let mut changed_keys = Vec::new();

    match kind {
        RuntimeConfigKind::Account => {
            // The saved Account overlay is authoritative for route-scoped model fields.
            // Removing an absent field prevents a Relay-only model from leaking into OpenAI.
            for key in ["model", "service_tier"] {
                if let Some(next) = runtime.get(key).and_then(Item::as_str) {
                    set_top_level_string(&mut live, key, Some(next), &mut changed_keys);
                } else if live.remove(key).is_some() {
                    changed_keys.push(key.to_string());
                }
            }
            if live.remove("model_provider").is_some() {
                changed_keys.push("model_provider".to_string());
            }
            if let Some(providers) = live.get_mut("model_providers").and_then(Item::as_table_mut) {
                if providers.remove(MANAGED_RELAY_PROVIDER_ID).is_some() {
                    changed_keys.push(format!("model_providers.{MANAGED_RELAY_PROVIDER_ID}"));
                }
                if providers.is_empty() {
                    live.remove("model_providers");
                }
            }
        }
        RuntimeConfigKind::Relay => {
            for key in ["model", "service_tier"] {
                if let Some(next) = runtime.get(key).and_then(Item::as_str) {
                    set_top_level_string(&mut live, key, Some(next), &mut changed_keys);
                }
            }
            let relay_bearer_token = relay_bearer_token
                .map(str::trim)
                .filter(|token| !token.is_empty())
                .ok_or_else(|| "relay bearer token is required".to_string())?;
            let provider = runtime
                .get("model_provider")
                .and_then(Item::as_str)
                .ok_or_else(|| "relay runtime config is missing model_provider".to_string())?;
            if provider != MANAGED_RELAY_PROVIDER_ID {
                return Err("relay runtime config uses an unsupported provider".to_string());
            }
            set_top_level_string(
                &mut live,
                "model_provider",
                Some(provider),
                &mut changed_keys,
            );
            let source_table = runtime
                .get("model_providers")
                .and_then(Item::as_table)
                .and_then(|providers| providers.get(provider))
                .and_then(Item::as_table)
                .ok_or_else(|| {
                    format!("relay runtime config is missing model_providers.{provider}")
                })?;
            let target_table = provider_table_mut(&mut live, provider)?;
            target_table.clear();
            for (key, item) in source_table.iter() {
                if matches!(
                    key,
                    "api_key" | "env_key" | "goal" | "experimental_bearer_token"
                ) {
                    continue;
                }
                target_table.insert(key, item.clone());
            }
            target_table["experimental_bearer_token"] = value(relay_bearer_token);
            target_table["requires_openai_auth"] = value(true);
            target_table["supports_websockets"] = value(true);
            changed_keys.push(format!("model_providers.{provider}"));
        }
    }

    let patched_toml = live.to_string();
    DocumentMut::from_str(&patched_toml)
        .map_err(|_| "patched config.toml is invalid".to_string())?;
    Ok(ConfigPatchPlan {
        patched_toml,
        changed_keys,
    })
}

pub fn apply_sqlite_home_patch(
    plan: ConfigPatchPlan,
    patch: &SqliteHomePatch,
) -> Result<ConfigPatchPlan, String> {
    if patch == &SqliteHomePatch::Keep {
        return Ok(plan);
    }
    let mut doc = DocumentMut::from_str(&plan.patched_toml)
        .map_err(|_| "patched config.toml is invalid".to_string())?;
    let mut changed_keys = plan.changed_keys;
    match patch {
        SqliteHomePatch::Keep => {}
        SqliteHomePatch::Set(path) => {
            if doc.get("sqlite_home").and_then(Item::as_str) != Some(path) {
                doc["sqlite_home"] = value(path);
                changed_keys.push("sqlite_home".to_string());
            }
        }
        SqliteHomePatch::Remove => {
            if doc.remove("sqlite_home").is_some() {
                changed_keys.push("sqlite_home".to_string());
            }
        }
    }
    Ok(ConfigPatchPlan {
        patched_toml: doc.to_string(),
        changed_keys,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        apply_sqlite_home_patch, plan_config_patch, plan_runtime_config_patch, ConfigOverlay,
        RuntimeConfigKind, SqliteHomePatch,
    };

    #[test]
    fn patches_login_bound_fields_while_preserving_global_config() {
        let base = r#"
model = "gpt-5.5"
model_instructions_file = "C:\\Users\\alice\\.codex\\instruction.md"

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
        assert!(plan
            .patched_toml
            .contains("model_provider = \"direct-account\""));
        assert!(plan.patched_toml.contains(
            "model_instructions_file = \"C:\\\\Users\\\\alice\\\\.codex\\\\instruction.md\""
        ));
        assert!(plan.patched_toml.contains("fast_mode = true"));
        assert!(plan
            .patched_toml
            .contains("[model_providers.direct-account]"));
        assert!(plan
            .patched_toml
            .contains("base_url = \"https://api.openai.com/v1\""));
        assert!(plan
            .patched_toml
            .contains("env_key = \"CODEX_SWITCH_API_KEY\""));
    }

    #[test]
    fn empty_overlay_returns_unchanged_toml_and_no_changes() {
        let base = "model = \"gpt-5.5\"\\n";
        let plan = plan_config_patch(base, &ConfigOverlay::default()).unwrap();

        assert_eq!(plan.patched_toml, base);
        assert!(plan.changed_keys.is_empty());
    }

    #[test]
    fn account_runtime_patch_removes_relay_binding_but_preserves_live_global_sections() {
        let live = r#"
model = "relay-model"
model_provider = "openai_custom"
model_instructions_file = "new-global"

[features]
fast_mode = true

[mcp_servers.current]
command = "new-command"

[model_providers.openai_custom]
base_url = "https://relay.example.com/v1"
wire_api = "responses"
"#;
        let stored_account = r#"
model = "account-model"
model_instructions_file = "stale-global"

[features]
fast_mode = false

[mcp_servers.old]
command = "old-command"
"#;

        let plan =
            plan_runtime_config_patch(live, stored_account, RuntimeConfigKind::Account, None)
                .unwrap();

        assert!(plan.patched_toml.contains("model = \"account-model\""));
        assert!(!plan
            .patched_toml
            .contains("model_provider = \"openai_custom\""));
        assert!(plan
            .patched_toml
            .contains("model_instructions_file = \"new-global\""));
        assert!(plan.patched_toml.contains("fast_mode = true"));
        assert!(plan.patched_toml.contains("new-command"));
        assert!(!plan.patched_toml.contains("old-command"));
        assert!(!plan.patched_toml.contains("openai_custom"));
    }

    #[test]
    fn relay_runtime_patch_updates_only_runtime_binding_and_keeps_unrelated_providers() {
        let live = r#"
model = "account-model"
model_instructions_file = "global"

[projects."C:\\repo"]
trust_level = "trusted"

[model_providers.customer_owned]
base_url = "https://customer.example.com/v1"
"#;
        let stored_relay = r#"
model = "relay-model"
model_provider = "openai_custom"

[model_providers.openai_custom]
name = "openai_custom"
base_url = "https://relay.example.com/v1"
wire_api = "responses"
supports_websockets = false
"#;

        let plan = plan_runtime_config_patch(
            live,
            stored_relay,
            RuntimeConfigKind::Relay,
            Some("sk-relay-secret"),
        )
        .unwrap();

        assert!(plan.patched_toml.contains("model = \"relay-model\""));
        assert!(plan
            .patched_toml
            .contains("model_provider = \"openai_custom\""));
        assert!(plan.patched_toml.contains("https://relay.example.com/v1"));
        assert!(plan.patched_toml.contains("customer_owned"));
        assert!(plan.patched_toml.contains("trust_level = \"trusted\""));
        assert!(plan
            .patched_toml
            .contains("model_instructions_file = \"global\""));
        assert!(plan
            .patched_toml
            .contains("experimental_bearer_token = \"sk-relay-secret\""));
        assert!(plan.patched_toml.contains("requires_openai_auth = true"));
        assert!(plan.patched_toml.contains("supports_websockets = true"));
        assert!(!plan.patched_toml.contains("supports_websockets = false"));
    }

    #[test]
    fn relay_runtime_patch_adds_websocket_support_when_the_saved_field_is_missing() {
        let plan = plan_runtime_config_patch(
            "model = \"account\"\n",
            "model = \"relay\"\nmodel_provider = \"openai_custom\"\n\
             [model_providers.openai_custom]\nbase_url = \"https://relay.example.com/v1\"\n",
            RuntimeConfigKind::Relay,
            Some("sk-relay-secret"),
        )
        .unwrap();
        let patched = plan.patched_toml.parse::<toml_edit::DocumentMut>().unwrap();

        assert_eq!(
            patched
                .get("model_providers")
                .and_then(toml_edit::Item::as_table)
                .and_then(|providers| providers.get("openai_custom"))
                .and_then(toml_edit::Item::as_table)
                .and_then(|provider| provider.get("supports_websockets"))
                .and_then(toml_edit::Item::as_bool),
            Some(true)
        );
    }

    #[test]
    fn sqlite_home_patch_sets_and_restores_only_the_managed_key() {
        let base = plan_config_patch(
            "model = \"gpt-5.6-sol\"\n[features]\nfast_mode = true\n",
            &ConfigOverlay::default(),
        )
        .unwrap();
        let relay =
            apply_sqlite_home_patch(base, &SqliteHomePatch::Set(r"C:\relay-view".to_string()))
                .unwrap();
        let relay_doc = relay
            .patched_toml
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        assert_eq!(
            relay_doc
                .get("sqlite_home")
                .and_then(toml_edit::Item::as_str),
            Some(r"C:\relay-view")
        );
        assert!(relay.patched_toml.contains("fast_mode = true"));

        let account = apply_sqlite_home_patch(relay, &SqliteHomePatch::Remove).unwrap();
        assert!(!account.patched_toml.contains("sqlite_home"));
        assert!(account.patched_toml.contains("fast_mode = true"));
    }

    #[test]
    fn relay_runtime_patch_requires_a_nonempty_bearer_token_without_echoing_it() {
        let error = plan_runtime_config_patch(
            "model = \"account\"\n",
            "model = \"relay\"\nmodel_provider = \"openai_custom\"\n\
             [model_providers.openai_custom]\nbase_url = \"https://relay.example.com/v1\"\n",
            RuntimeConfigKind::Relay,
            Some("   "),
        )
        .unwrap_err();

        assert_eq!(error, "relay bearer token is required");
    }

    #[test]
    fn account_patch_removes_only_the_managed_relay_provider_and_secret() {
        let live = r#"
model = "relay"
model_provider = "openai_custom"

[model_providers.openai_custom]
base_url = "https://relay.example.com/v1"
experimental_bearer_token = "sk-secret"

[model_providers.customer_owned]
base_url = "https://customer.example.com/v1"
"#;
        let plan = plan_runtime_config_patch(
            live,
            "model = \"account\"\n",
            RuntimeConfigKind::Account,
            None,
        )
        .unwrap();

        assert!(!plan.patched_toml.contains("openai_custom"));
        assert!(!plan.patched_toml.contains("sk-secret"));
        assert!(plan.patched_toml.contains("customer_owned"));
    }

    #[test]
    fn account_patch_removes_relay_only_model_fields_absent_from_the_saved_overlay() {
        let plan = plan_runtime_config_patch(
            "model = \"relay\"\nservice_tier = \"fast\"\nmodel_provider = \"openai_custom\"\n",
            "",
            RuntimeConfigKind::Account,
            None,
        )
        .unwrap();

        assert!(!plan.patched_toml.contains("model ="));
        assert!(!plan.patched_toml.contains("service_tier"));
        assert!(!plan.patched_toml.contains("model_provider"));
    }

    #[test]
    fn relay_patch_rejects_a_provider_outside_the_managed_contract() {
        let error = plan_runtime_config_patch(
            "model = \"account\"\n",
            "model = \"relay\"\nmodel_provider = \"other\"\n\
             [model_providers.other]\nbase_url = \"https://relay.example.com/v1\"\n",
            RuntimeConfigKind::Relay,
            Some("sk-relay-secret"),
        )
        .unwrap_err();

        assert_eq!(error, "relay runtime config uses an unsupported provider");
    }
}

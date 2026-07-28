use std::{fs, path::Path};

use serde_json::Value as JsonValue;

use crate::{
    chat_process_state::{read_snapshot as read_chat_process_state, repair_after_shutdown},
    config_patch::{plan_runtime_config_patch, ConfigPatchPlan, RuntimeConfigKind},
    file_ops::atomic_write,
    operation_log::operation_id,
    runtime_store::{
        relay_api_key_from_auth, RuntimeConfidence, RuntimeKind, RuntimeMetadata, RuntimeStore,
    },
    runtime_switcher::{
        ChatGptLaunchReceipt, RuntimeSwitchFailure, RuntimeSwitchOutcome, RuntimeSwitchPhase,
        RuntimeSwitchResult,
    },
    session_incremental::IncrementalSessionSyncReceipt,
};

#[derive(Debug)]
pub(crate) struct RequestRouteSwitchPlan {
    operation_id: String,
    runtime: RuntimeMetadata,
    config_plan: ConfigPatchPlan,
    auth_snapshot: Vec<u8>,
    config_snapshot: Vec<u8>,
    requires_change: bool,
}

impl RequestRouteSwitchPlan {
    pub(crate) fn requires_change(&self) -> bool {
        self.requires_change
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestRouteFailurePoint {
    None,
    #[cfg(test)]
    AfterConfigWrite,
}

pub(crate) fn preflight_request_route_switch(
    store: &RuntimeStore,
    runtime_id: &str,
    codex_home: &Path,
) -> Result<RequestRouteSwitchPlan, String> {
    build_request_route_switch_plan(store, runtime_id, codex_home, None)
}

pub(crate) fn switch_request_route_preflighted_with_progress(
    store: &RuntimeStore,
    codex_home: &Path,
    plan: RequestRouteSwitchPlan,
    verify_processes_closed: &mut dyn FnMut() -> Result<(), String>,
    on_progress: &mut dyn FnMut(RuntimeSwitchPhase),
) -> Result<RuntimeSwitchResult, RuntimeSwitchFailure> {
    switch_request_route_from_plan(
        store,
        codex_home,
        plan,
        RequestRouteFailurePoint::None,
        verify_processes_closed,
        on_progress,
    )
}

fn switch_request_route_from_plan(
    store: &RuntimeStore,
    codex_home: &Path,
    plan: RequestRouteSwitchPlan,
    failure_point: RequestRouteFailurePoint,
    verify_processes_closed: &mut dyn FnMut() -> Result<(), String>,
    on_progress: &mut dyn FnMut(RuntimeSwitchPhase),
) -> Result<RuntimeSwitchResult, RuntimeSwitchFailure> {
    let operation_id = plan.operation_id.clone();
    let runtime_id = plan.runtime.id.clone();
    if !plan.requires_change {
        verify_requested_runtime(store, codex_home, &runtime_id)
            .map_err(|message| before_write_failure(message, &operation_id))?;
        return no_op_result(store, &runtime_id, operation_id);
    }

    verify_processes_closed().map_err(|message| before_write_failure(message, &operation_id))?;
    on_progress(RuntimeSwitchPhase::PreparingRuntime);
    let closed_plan = build_request_route_switch_plan(
        store,
        &runtime_id,
        codex_home,
        Some(operation_id.as_str()),
    )
    .map_err(|message| before_write_failure(message, &operation_id))?;
    if !closed_plan.requires_change {
        verify_processes_closed()
            .map_err(|message| before_write_failure(message, &operation_id))?;
        verify_auth_unchanged(codex_home, &closed_plan.auth_snapshot)
            .map_err(|message| before_write_failure(message, &operation_id))?;
        verify_requested_runtime(store, codex_home, &runtime_id)
            .map_err(|message| before_write_failure(message, &operation_id))?;
        return no_op_result(store, &runtime_id, operation_id);
    }

    on_progress(RuntimeSwitchPhase::RepairingAppState);
    let process_state = read_chat_process_state(codex_home)
        .map_err(|message| before_write_failure(message, &operation_id))?;
    let process_state_bytes = process_state
        .as_ref()
        .map(|snapshot| snapshot.bytes.clone());
    drop(process_state);
    verify_processes_closed().map_err(|message| before_write_failure(message, &operation_id))?;
    let chat_process_state_repaired =
        repair_after_shutdown(codex_home, process_state_bytes.as_deref())
            .map_err(|message| before_write_failure(message, &operation_id))?;

    on_progress(RuntimeSwitchPhase::ApplyingRuntime);
    atomic_write(
        &codex_home.join("config.toml"),
        closed_plan.config_plan.patched_toml.as_bytes(),
    )
    .map_err(|message| before_write_failure(message, &operation_id))?;

    let applied: Result<RuntimeMetadata, String> = (|| {
        #[cfg(test)]
        if failure_point == RequestRouteFailurePoint::AfterConfigWrite {
            return Err("injected failure after request config write".to_string());
        }
        let _ = failure_point;
        verify_processes_closed()?;
        on_progress(RuntimeSwitchPhase::Verifying);
        verify_auth_unchanged(codex_home, &closed_plan.auth_snapshot)?;
        verify_requested_runtime(store, codex_home, &runtime_id)?;
        let runtime = store.mark_used(&runtime_id)?;
        Ok(runtime)
    })();

    match applied {
        Ok(runtime) => Ok(RuntimeSwitchResult {
            operation_id,
            changed: true,
            runtime,
            warnings: Vec::new(),
            incremental_session_sync: IncrementalSessionSyncReceipt::skipped(),
            chat_process_state_repaired,
            chatgpt_launch: ChatGptLaunchReceipt::not_requested(),
        }),
        Err(error) => {
            on_progress(RuntimeSwitchPhase::RollingBack);
            if let Err(gate_error) = verify_processes_closed() {
                return Err(RuntimeSwitchFailure {
                    message: format!(
                        "{error}; config rollback was not attempted because ChatGPT/Codex activity resumed: {gate_error}"
                    ),
                    outcome: RuntimeSwitchOutcome::RollbackFailed,
                    operation_id: Some(operation_id),
                });
            }
            match restore_config_and_verify_auth(
                codex_home,
                &closed_plan.config_snapshot,
                &closed_plan.auth_snapshot,
            ) {
                Ok(()) => Err(RuntimeSwitchFailure {
                    message: format!(
                        "{error}; restored the original config.toml; official auth.json was not changed"
                    ),
                    outcome: RuntimeSwitchOutcome::RolledBack,
                    operation_id: Some(operation_id),
                }),
                Err(rollback_error) => Err(RuntimeSwitchFailure {
                    message: format!("{error}; config rollback failed: {rollback_error}"),
                    outcome: RuntimeSwitchOutcome::RollbackFailed,
                    operation_id: Some(operation_id),
                }),
            }
        }
    }
}

fn build_request_route_switch_plan(
    store: &RuntimeStore,
    runtime_id: &str,
    codex_home: &Path,
    existing_operation_id: Option<&str>,
) -> Result<RequestRouteSwitchPlan, String> {
    let operation_id = match existing_operation_id {
        Some(id) => id.to_string(),
        None => operation_id("switch-runtime")?,
    };
    let auth_snapshot = load_official_auth_snapshot(codex_home)?;
    let config_snapshot = fs::read(codex_home.join("config.toml"))
        .map_err(|error| format!("failed to read live config.toml: {error}"))?;
    let live_config = std::str::from_utf8(&config_snapshot)
        .map_err(|_| "live config.toml is not valid UTF-8".to_string())?;
    let runtime_files = store.load_runtime_files(runtime_id)?;
    let runtime = store.load_metadata(runtime_id)?;
    let (config_kind, relay_bearer_token) = match runtime.kind {
        RuntimeKind::Plus => (RuntimeConfigKind::Account, None),
        RuntimeKind::Relay => (
            RuntimeConfigKind::Relay,
            Some(relay_api_key_from_auth(&runtime_files.auth_json)?),
        ),
    };
    let config_plan = plan_runtime_config_patch(
        live_config,
        &runtime_files.config_toml,
        config_kind,
        relay_bearer_token.as_deref(),
    )?;
    let active = store.detect_active_runtime(codex_home)?;
    let requires_change = active.active_runtime_id.as_deref() != Some(runtime_id)
        || active.confidence != RuntimeConfidence::Exact;
    Ok(RequestRouteSwitchPlan {
        operation_id,
        runtime,
        config_plan,
        auth_snapshot,
        config_snapshot,
        requires_change,
    })
}

fn load_official_auth_snapshot(codex_home: &Path) -> Result<Vec<u8>, String> {
    let auth = fs::read(codex_home.join("auth.json"))
        .map_err(|error| format!("failed to read official auth.json: {error}"))?;
    let value = serde_json::from_slice::<JsonValue>(&auth)
        .map_err(|error| format!("failed to parse official auth.json: {error}"))?;
    if value.get("auth_mode").and_then(JsonValue::as_str) != Some("chatgpt") {
        return Err(
            "当前 auth.json 不是 ChatGPT 官方登录态；请求端切换已停止且不会改写登录状态"
                .to_string(),
        );
    }
    Ok(auth)
}

fn verify_auth_unchanged(codex_home: &Path, expected: &[u8]) -> Result<(), String> {
    let observed = fs::read(codex_home.join("auth.json"))
        .map_err(|error| format!("failed to re-read official auth.json: {error}"))?;
    if observed == expected {
        Ok(())
    } else {
        Err("official auth.json changed during request route switch".to_string())
    }
}

fn verify_requested_runtime(
    store: &RuntimeStore,
    codex_home: &Path,
    runtime_id: &str,
) -> Result<(), String> {
    let verified = store.detect_active_runtime(codex_home)?;
    if verified.active_runtime_id.as_deref() == Some(runtime_id)
        && verified.confidence == RuntimeConfidence::Exact
        && verified.auth_mode.as_deref() == Some("chatgpt")
    {
        Ok(())
    } else {
        Err("request route verification did not match the requested target".to_string())
    }
}

fn restore_config_and_verify_auth(
    codex_home: &Path,
    config_snapshot: &[u8],
    auth_snapshot: &[u8],
) -> Result<(), String> {
    atomic_write(&codex_home.join("config.toml"), config_snapshot)?;
    let restored = fs::read(codex_home.join("config.toml"))
        .map_err(|error| format!("failed to verify restored config.toml: {error}"))?;
    if restored != config_snapshot {
        return Err("restored config.toml did not match its snapshot".to_string());
    }
    verify_auth_unchanged(codex_home, auth_snapshot)
}

fn no_op_result(
    store: &RuntimeStore,
    runtime_id: &str,
    operation_id: String,
) -> Result<RuntimeSwitchResult, RuntimeSwitchFailure> {
    let runtime = store
        .load_metadata(runtime_id)
        .map_err(|message| before_write_failure(message, &operation_id))?;
    Ok(RuntimeSwitchResult {
        operation_id,
        changed: false,
        runtime,
        warnings: Vec::new(),
        incremental_session_sync: IncrementalSessionSyncReceipt::skipped(),
        chat_process_state_repaired: false,
        chatgpt_launch: ChatGptLaunchReceipt::not_requested(),
    })
}

fn before_write_failure(message: String, operation_id: &str) -> RuntimeSwitchFailure {
    RuntimeSwitchFailure {
        message,
        outcome: RuntimeSwitchOutcome::FailedBeforeWrite,
        operation_id: Some(operation_id.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{
        preflight_request_route_switch, switch_request_route_from_plan, RequestRouteFailurePoint,
    };
    use crate::{
        runtime_store::{RelayRuntimeInput, RuntimeStore, PLUS_RUNTIME_ID, RELAY_RUNTIME_ID},
        runtime_switcher::{RuntimeSwitchOutcome, RuntimeSwitchPhase},
    };

    fn setup() -> (tempfile::TempDir, tempfile::TempDir, RuntimeStore, Vec<u8>) {
        let home = tempdir().unwrap();
        let store_root = tempdir().unwrap();
        let auth =
            br#"{"auth_mode":"chatgpt","tokens":{"access_token":"official-token"}}"#.to_vec();
        fs::write(home.path().join("auth.json"), &auth).unwrap();
        fs::write(
            home.path().join("config.toml"),
            "model = \"account-model\"\nmodel_instructions_file = \"global\"\n",
        )
        .unwrap();
        let store = RuntimeStore::new(store_root.path().join("runtimes"));
        store.import_plus_from_home(home.path(), false).unwrap();
        store
            .upsert_relay(
                RelayRuntimeInput {
                    base_url: "https://relay.example.com/v1".to_string(),
                    api_key: "sk-relay-secret".to_string(),
                    model: "relay-model".to_string(),
                },
                home.path(),
            )
            .unwrap();
        (home, store_root, store, auth)
    }

    #[test]
    fn relay_and_account_routes_preserve_official_auth_bytes() {
        let (home, _store_root, store, auth) = setup();
        let auth_modified = fs::metadata(home.path().join("auth.json"))
            .unwrap()
            .modified()
            .unwrap();
        let mut phases = Vec::new();
        let relay_plan =
            preflight_request_route_switch(&store, RELAY_RUNTIME_ID, home.path()).unwrap();
        let relay = switch_request_route_from_plan(
            &store,
            home.path(),
            relay_plan,
            RequestRouteFailurePoint::None,
            &mut || Ok(()),
            &mut |phase| phases.push(phase),
        )
        .unwrap();

        assert!(relay.changed);
        assert_eq!(fs::read(home.path().join("auth.json")).unwrap(), auth);
        let relay_config = fs::read_to_string(home.path().join("config.toml")).unwrap();
        assert!(relay_config.contains("model_provider = \"openai_custom\""));
        assert!(relay_config.contains("experimental_bearer_token = \"sk-relay-secret\""));
        assert_eq!(
            phases,
            vec![
                RuntimeSwitchPhase::PreparingRuntime,
                RuntimeSwitchPhase::RepairingAppState,
                RuntimeSwitchPhase::ApplyingRuntime,
                RuntimeSwitchPhase::Verifying,
            ]
        );

        phases.clear();
        let account_plan =
            preflight_request_route_switch(&store, PLUS_RUNTIME_ID, home.path()).unwrap();
        let account = switch_request_route_from_plan(
            &store,
            home.path(),
            account_plan,
            RequestRouteFailurePoint::None,
            &mut || Ok(()),
            &mut |phase| phases.push(phase),
        )
        .unwrap();

        assert!(account.changed);
        assert_eq!(fs::read(home.path().join("auth.json")).unwrap(), auth);
        assert_eq!(
            fs::metadata(home.path().join("auth.json"))
                .unwrap()
                .modified()
                .unwrap(),
            auth_modified
        );
        let account_config = fs::read_to_string(home.path().join("config.toml")).unwrap();
        assert!(!account_config.contains("openai_custom"));
        assert!(!account_config.contains("sk-relay-secret"));
    }

    #[test]
    fn non_official_auth_fails_before_process_or_config_mutation() {
        let (home, _store_root, store, _) = setup();
        let config_before = fs::read(home.path().join("config.toml")).unwrap();
        fs::write(
            home.path().join("auth.json"),
            br#"{"auth_mode":"apikey","OPENAI_API_KEY":"legacy"}"#,
        )
        .unwrap();

        let error =
            preflight_request_route_switch(&store, RELAY_RUNTIME_ID, home.path()).unwrap_err();

        assert!(error.contains("不是 ChatGPT 官方登录态"), "{error}");
        assert_eq!(
            fs::read(home.path().join("config.toml")).unwrap(),
            config_before
        );
    }

    #[test]
    fn exact_account_noop_allows_a_concurrent_official_token_refresh() {
        let (home, _store_root, store, _) = setup();
        let plan = preflight_request_route_switch(&store, PLUS_RUNTIME_ID, home.path()).unwrap();
        let refreshed =
            br#"{"auth_mode":"chatgpt","tokens":{"access_token":"refreshed-official-token"}}"#;
        fs::write(home.path().join("auth.json"), refreshed).unwrap();
        let process_gate_calls = std::cell::Cell::new(0);

        let result = switch_request_route_from_plan(
            &store,
            home.path(),
            plan,
            RequestRouteFailurePoint::None,
            &mut || {
                process_gate_calls.set(process_gate_calls.get() + 1);
                Ok(())
            },
            &mut |_| {},
        )
        .unwrap();

        assert!(!result.changed);
        assert_eq!(process_gate_calls.get(), 0);
        assert_eq!(fs::read(home.path().join("auth.json")).unwrap(), refreshed);
    }

    #[test]
    fn post_write_failure_rolls_back_only_config_and_preserves_auth() {
        let (home, _store_root, store, auth) = setup();
        let config_before = fs::read(home.path().join("config.toml")).unwrap();
        let plan = preflight_request_route_switch(&store, RELAY_RUNTIME_ID, home.path()).unwrap();
        let mut phases = Vec::new();

        let failure = switch_request_route_from_plan(
            &store,
            home.path(),
            plan,
            RequestRouteFailurePoint::AfterConfigWrite,
            &mut || Ok(()),
            &mut |phase| phases.push(phase),
        )
        .unwrap_err();

        assert_eq!(failure.outcome, RuntimeSwitchOutcome::RolledBack);
        assert_eq!(fs::read(home.path().join("auth.json")).unwrap(), auth);
        assert_eq!(
            fs::read(home.path().join("config.toml")).unwrap(),
            config_before
        );
        assert!(phases.contains(&RuntimeSwitchPhase::RollingBack));
    }
}

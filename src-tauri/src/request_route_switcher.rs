use std::{fs, path::Path};

use serde_json::Value as JsonValue;

use crate::{
    chat_process_state::{read_snapshot as read_chat_process_state, repair_after_shutdown},
    config_patch::{
        apply_sqlite_home_patch, plan_runtime_config_patch, ConfigPatchPlan, RuntimeConfigKind,
    },
    file_ops::atomic_write,
    operation_log::operation_id,
    runtime_session_view::{
        commit_transition, plan_transition, prepare_transition, rollback_transition,
        PreparedViewTransition, SessionViewPlan, SessionViewTarget, SessionViewTransition,
    },
    runtime_store::{
        relay_api_key_from_auth, RuntimeConfidence, RuntimeKind, RuntimeMetadata, RuntimeStore,
    },
    runtime_switcher::{
        ChatGptLaunchReceipt, RelayValidationStatus, RuntimeSwitchFailure, RuntimeSwitchOutcome,
        RuntimeSwitchPhase, RuntimeSwitchResult,
    },
    session_incremental::IncrementalSessionSyncReceipt,
    session_storage::provenance::RouteProvenanceReceipt,
};

#[derive(Debug)]
pub(crate) struct RequestRouteSwitchPlan {
    operation_id: String,
    runtime: RuntimeMetadata,
    config_plan: ConfigPatchPlan,
    auth_snapshot: Vec<u8>,
    config_snapshot: Vec<u8>,
    session_view: SessionViewPlan,
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
    plan.session_view
        .recover_pending(codex_home)
        .map_err(|message| before_write_failure(message, &operation_id))?;
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

    let prepared_view = if matches!(
        &closed_plan.session_view.transition,
        SessionViewTransition::None
    ) {
        PreparedViewTransition::skipped(&closed_plan.session_view)
    } else {
        on_progress(RuntimeSwitchPhase::SyncingIncrementalSessions);
        prepare_transition(&closed_plan.session_view, closed_plan.operation_id.as_str())
            .map_err(|message| before_write_failure(message, &operation_id))?
    };
    let incremental_session_sync = prepared_view.receipt().clone();

    on_progress(RuntimeSwitchPhase::ApplyingRuntime);
    let config_write_error = atomic_write(
        &codex_home.join("config.toml"),
        closed_plan.config_plan.patched_toml.as_bytes(),
    )
    .err();

    let config_written = config_write_error.is_none();
    let applied: Result<RuntimeMetadata, String> = if let Some(error) = config_write_error {
        Err(error)
    } else {
        (|| {
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
        })()
    };

    match applied {
        Ok(runtime) => {
            commit_transition(prepared_view).map_err(|message| RuntimeSwitchFailure {
                message,
                outcome: RuntimeSwitchOutcome::RollbackFailed,
                operation_id: Some(operation_id.clone()),
            })?;
            Ok(RuntimeSwitchResult {
                operation_id,
                changed: true,
                runtime,
                warnings: Vec::new(),
                incremental_session_sync,
                route_provenance: RouteProvenanceReceipt::pending(),
                relay_validation: RelayValidationStatus::NotApplicable,
                chat_process_state_repaired,
                chatgpt_launch: ChatGptLaunchReceipt::not_requested(),
            })
        }
        Err(error) => {
            on_progress(RuntimeSwitchPhase::RollingBack);
            if config_written {
                if let Err(gate_error) = verify_processes_closed() {
                    return Err(RuntimeSwitchFailure {
                        message: format!(
                            "{error}; config rollback was not attempted because ChatGPT/Codex activity resumed: {gate_error}"
                        ),
                        outcome: RuntimeSwitchOutcome::RollbackFailed,
                        operation_id: Some(operation_id),
                    });
                }
            }
            if config_written {
                match restore_config_and_verify_auth(
                    codex_home,
                    &closed_plan.config_snapshot,
                    &closed_plan.auth_snapshot,
                ) {
                    Ok(()) => {}
                    Err(rollback_error) => {
                        return Err(RuntimeSwitchFailure {
                            message: format!("{error}; config rollback failed: {rollback_error}"),
                            outcome: RuntimeSwitchOutcome::RollbackFailed,
                            operation_id: Some(operation_id),
                        });
                    }
                }
            } else if let Err(gate_error) = verify_processes_closed() {
                return Err(RuntimeSwitchFailure {
                    message: format!(
                        "{error}; session view rollback was not attempted because ChatGPT/Codex activity resumed: {gate_error}"
                    ),
                    outcome: RuntimeSwitchOutcome::RollbackFailed,
                    operation_id: Some(operation_id),
                });
            }
            match rollback_transition(prepared_view) {
                Ok(()) => Err(RuntimeSwitchFailure {
                    message: format!(
                        "{error}; restored the original config.toml and session database view; official auth.json was not changed"
                    ),
                    outcome: RuntimeSwitchOutcome::RolledBack,
                    operation_id: Some(operation_id),
                }),
                Err(view_error) => Err(RuntimeSwitchFailure {
                    message: format!(
                        "{error}; restored the original config.toml, but session view rollback requires recovery: {view_error}"
                    ),
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
    let session_view = plan_transition(
        codex_home,
        live_config,
        match runtime.kind {
            RuntimeKind::Plus => SessionViewTarget::Account,
            RuntimeKind::Relay => SessionViewTarget::Relay,
        },
        &store.data_root()?,
    )?;
    let config_plan = apply_sqlite_home_patch(
        plan_runtime_config_patch(
            live_config,
            &runtime_files.config_toml,
            config_kind,
            relay_bearer_token.as_deref(),
        )?,
        &session_view.sqlite_home_patch,
    )?;
    let active = store.detect_active_runtime(codex_home)?;
    let requires_change = active.active_runtime_id.as_deref() != Some(runtime_id)
        || active.confidence != RuntimeConfidence::Exact
        || !config_plan.changed_keys.is_empty()
        || session_view.requires_work();
    Ok(RequestRouteSwitchPlan {
        operation_id,
        runtime,
        config_plan,
        auth_snapshot,
        config_snapshot,
        session_view,
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
        route_provenance: RouteProvenanceReceipt::pending(),
        relay_validation: RelayValidationStatus::NotApplicable,
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
    use std::{fs, path::Path};

    use rusqlite::Connection;
    use tempfile::tempdir;

    use super::{
        preflight_request_route_switch, switch_request_route_from_plan, RequestRouteFailurePoint,
    };
    use crate::{
        runtime_session_view::inspect_session_view_database_homes,
        runtime_store::{RelayRuntimeInput, RuntimeStore, PLUS_RUNTIME_ID, RELAY_RUNTIME_ID},
        runtime_switcher::{RuntimeSwitchOutcome, RuntimeSwitchPhase},
        session_storage::bounded_file::same_regular_file_identity,
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
        fs::create_dir_all(home.path().join("sessions")).unwrap();
        Connection::open(home.path().join("state_5.sqlite"))
            .unwrap()
            .execute_batch(
                "CREATE TABLE threads (
                    id TEXT PRIMARY KEY,
                    rollout_path TEXT,
                    model_provider TEXT,
                    archived INTEGER NOT NULL DEFAULT 0
                );
                INSERT INTO threads (
                    id, rollout_path, model_provider, archived
                ) VALUES (
                    '019fa68f-dd42-76b3-8299-84a865ab553c',
                    'sessions/rollout-2026-07-29T00-00-00-019fa68f-dd42-76b3-8299-84a865ab553c.jsonl',
                    'openai',
                    0
                );",
            )
            .unwrap();
        for (name, table) in [
            ("logs_2.sqlite", "logs"),
            ("goals_1.sqlite", "goals"),
            ("memories_1.sqlite", "memories"),
        ] {
            Connection::open(home.path().join(name))
                .unwrap()
                .execute_batch(&format!(
                    "CREATE TABLE {table} (id INTEGER PRIMARY KEY, payload BLOB);
                     INSERT INTO {table} VALUES (1, zeroblob(4096));"
                ))
                .unwrap();
        }
        fs::write(
            home.path()
                .join("sessions")
                .join(
                    "rollout-2026-07-29T00-00-00-019fa68f-dd42-76b3-8299-84a865ab553c.jsonl",
                ),
            b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"019fa68f-dd42-76b3-8299-84a865ab553c\",\"model_provider\":\"openai\"}}\n",
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

    fn count_jsonl_recursively(root: &Path) -> usize {
        let Ok(entries) = fs::read_dir(root) else {
            return 0;
        };
        entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .map(|path| {
                if path.is_dir() {
                    count_jsonl_recursively(&path)
                } else {
                    usize::from(path.extension().and_then(|value| value.to_str()) == Some("jsonl"))
                }
            })
            .sum()
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
        assert!(relay_config.contains("requires_openai_auth = true"));
        let relay_doc = relay_config.parse::<toml_edit::DocumentMut>().unwrap();
        assert_eq!(
            relay_doc
                .get("model_providers")
                .and_then(toml_edit::Item::as_table)
                .and_then(|providers| providers.get("openai_custom"))
                .and_then(toml_edit::Item::as_table)
                .and_then(|provider| provider.get("supports_websockets"))
                .and_then(toml_edit::Item::as_bool),
            Some(true)
        );
        let relay_sqlite_home = relay_doc
            .get("sqlite_home")
            .and_then(toml_edit::Item::as_str)
            .unwrap();
        assert_ne!(std::path::Path::new(relay_sqlite_home), home.path());
        assert_eq!(
            Connection::open(home.path().join("state_5.sqlite"))
                .unwrap()
                .query_row(
                    "SELECT model_provider FROM threads
                     WHERE id = '019fa68f-dd42-76b3-8299-84a865ab553c'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "openai"
        );
        assert_eq!(
            Connection::open(std::path::Path::new(relay_sqlite_home).join("state_5.sqlite"))
                .unwrap()
                .query_row(
                    "SELECT model_provider FROM threads
                     WHERE id = '019fa68f-dd42-76b3-8299-84a865ab553c'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "openai_custom"
        );
        assert_eq!(
            phases,
            vec![
                RuntimeSwitchPhase::PreparingRuntime,
                RuntimeSwitchPhase::RepairingAppState,
                RuntimeSwitchPhase::SyncingIncrementalSessions,
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
        assert!(!account_config.contains("sqlite_home"));
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

    #[test]
    fn resumed_activity_after_config_write_preserves_journal_for_next_transition_recovery() {
        let (home, _store_root, store, _auth) = setup();
        let plan = preflight_request_route_switch(&store, RELAY_RUNTIME_ID, home.path()).unwrap();
        let mut gate_calls = 0_u32;

        let failure = switch_request_route_from_plan(
            &store,
            home.path(),
            plan,
            RequestRouteFailurePoint::AfterConfigWrite,
            &mut || {
                gate_calls += 1;
                if gate_calls == 3 {
                    Err("injected resumed writer".to_string())
                } else {
                    Ok(())
                }
            },
            &mut |_| {},
        )
        .unwrap_err();

        assert_eq!(failure.outcome, RuntimeSwitchOutcome::RollbackFailed);
        let journal_path = store
            .data_root()
            .unwrap()
            .join("request-route-session-view-transition-v2.json");
        assert!(journal_path.is_file());
        assert!(fs::read_to_string(home.path().join("config.toml"))
            .unwrap()
            .contains("sqlite_home"));

        let recovery_plan =
            preflight_request_route_switch(&store, RELAY_RUNTIME_ID, home.path()).unwrap();
        assert!(recovery_plan.requires_change());
        let recovered = switch_request_route_from_plan(
            &store,
            home.path(),
            recovery_plan,
            RequestRouteFailurePoint::None,
            &mut || Ok(()),
            &mut |_| {},
        )
        .unwrap();
        assert!(!journal_path.exists());
        assert!(!recovered.changed);
    }

    #[test]
    fn account_return_promotes_relay_database_view_without_copying_jsonl() {
        let (home, _store_root, store, auth) = setup();
        let relay_plan =
            preflight_request_route_switch(&store, RELAY_RUNTIME_ID, home.path()).unwrap();
        switch_request_route_from_plan(
            &store,
            home.path(),
            relay_plan,
            RequestRouteFailurePoint::None,
            &mut || Ok(()),
            &mut |_| {},
        )
        .unwrap();

        let relay_config = fs::read_to_string(home.path().join("config.toml")).unwrap();
        let relay_doc = relay_config.parse::<toml_edit::DocumentMut>().unwrap();
        let relay_sqlite_home = relay_doc
            .get("sqlite_home")
            .and_then(toml_edit::Item::as_str)
            .unwrap();
        let relay_db = std::path::Path::new(relay_sqlite_home).join("state_5.sqlite");
        let connection = Connection::open(&relay_db).unwrap();
        for index in 0..33 {
            let id = format!("30000000-0000-4000-8000-{index:012}");
            let filename = format!("rollout-2026-07-29T00-00-{index:02}-{id}.jsonl");
            let relative = format!("sessions/{filename}");
            connection
                .execute(
                    "INSERT INTO threads (id, rollout_path, model_provider, archived)
                     VALUES (?1, ?2, 'openai_custom', 0)",
                    [&id, &relative],
                )
                .unwrap();
            fs::write(
                home.path().join("sessions").join(filename),
                format!(
                    "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"model_provider\":\"openai_custom\"}}}}\n"
                ),
            )
            .unwrap();
        }
        drop(connection);
        let jsonl_before = fs::read_dir(home.path().join("sessions"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry.path().extension().and_then(|value| value.to_str()) == Some("jsonl")
            })
            .count();

        let account_plan =
            preflight_request_route_switch(&store, PLUS_RUNTIME_ID, home.path()).unwrap();
        let account = switch_request_route_from_plan(
            &store,
            home.path(),
            account_plan,
            RequestRouteFailurePoint::None,
            &mut || Ok(()),
            &mut |_| {},
        )
        .unwrap();

        assert_eq!(account.incremental_session_sync.synced_threads, 34);
        assert_eq!(
            Connection::open(home.path().join("state_5.sqlite"))
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM threads WHERE model_provider = 'openai'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            34
        );
        let jsonl_after = fs::read_dir(home.path().join("sessions"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry.path().extension().and_then(|value| value.to_str()) == Some("jsonl")
            })
            .count();
        assert_eq!(jsonl_after, jsonl_before);
        assert!(fs::read_to_string(
            home.path()
                .join("sessions")
                .join("rollout-2026-07-29T00-00-00-30000000-0000-4000-8000-000000000000.jsonl",),
        )
        .unwrap()
        .contains("\"model_provider\":\"openai_custom\""));
        assert_eq!(fs::read(home.path().join("auth.json")).unwrap(), auth);
        assert!(!fs::read_to_string(home.path().join("config.toml"))
            .unwrap()
            .contains("sqlite_home"));
    }

    #[test]
    fn one_hundred_route_switches_do_not_create_or_rewrite_session_bodies() {
        let (home, store_root, store, auth) = setup();
        let session = home
            .path()
            .join("sessions")
            .join("rollout-2026-07-29T00-00-00-019fa68f-dd42-76b3-8299-84a865ab553c.jsonl");
        let bytes_before = fs::read(&session).unwrap();
        let modified_before = fs::metadata(&session).unwrap().modified().unwrap();

        for index in 0..100 {
            let target = if index % 2 == 0 {
                RELAY_RUNTIME_ID
            } else {
                PLUS_RUNTIME_ID
            };
            let plan = preflight_request_route_switch(&store, target, home.path()).unwrap();
            let receipt = switch_request_route_from_plan(
                &store,
                home.path(),
                plan,
                RequestRouteFailurePoint::None,
                &mut || Ok(()),
                &mut |_| {},
            )
            .unwrap();
            assert!(
                receipt.changed,
                "switch {index} should change the active route"
            );
        }

        assert_eq!(fs::read(&session).unwrap(), bytes_before);
        assert_eq!(
            fs::metadata(&session).unwrap().modified().unwrap(),
            modified_before
        );
        assert_eq!(count_jsonl_recursively(home.path()), 1);
        assert_eq!(count_jsonl_recursively(store_root.path()), 0);
        let (account_sqlite_home, relay_sqlite_home) =
            inspect_session_view_database_homes(&store.data_root().unwrap())
                .unwrap()
                .unwrap();
        assert_eq!(account_sqlite_home, home.path());
        for name in ["logs_2.sqlite", "goals_1.sqlite", "memories_1.sqlite"] {
            assert!(
                same_regular_file_identity(
                    &account_sqlite_home.join(name),
                    &relay_sqlite_home.join(name),
                )
                .unwrap(),
                "{name} should be a shared hard-linked database view"
            );
        }
        assert_eq!(fs::read(home.path().join("auth.json")).unwrap(), auth);
        assert!(!fs::read_to_string(home.path().join("config.toml"))
            .unwrap()
            .contains("sqlite_home"));
    }
}

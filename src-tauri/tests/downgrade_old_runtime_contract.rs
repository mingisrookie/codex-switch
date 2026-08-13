#![cfg(windows)]

use std::{
    env, fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use codex_switch_lib::runtime_store::{RuntimeStore, PLUS_RUNTIME_ID};
use codex_switch_lib::session_storage::codex_runtime_verifier::NativeCodexBackupVerifier;
use codex_switch_lib::session_storage::downgrade::{
    execute_downgrade_export, prepare_downgrade_export, verify_downgrade_package,
    verify_downgrade_package_with_runtime,
};
use codex_switch_lib::session_storage::migration::{
    persist_migration_preflight, run_migration_preflight,
};
use codex_switch_lib::session_storage::operation_ledger::{
    OperationLedgerStore, SessionStorageOperationKind, SessionStorageOperationPhase,
};
use codex_switch_lib::session_storage::storage_state::{
    finalize_canonical_storage_state, prepare_canonical_storage_state,
};
use rusqlite::Connection;
use serde_json::json;
use sha2::{Digest, Sha256};

const OUTPUT_ENV: &str = "CODEX_SWITCH_DOWNGRADE_FIXTURE_ROOT";
const THREAD_ID: &str = "11111111-1111-4111-8111-111111111111";

#[test]
#[ignore = "manual old-version runtime gate; requires native Codex and writes only to CODEX_SWITCH_DOWNGRADE_FIXTURE_ROOT"]
fn exports_every_supported_v02_runtime_fixture() {
    let output_root = env::var_os(OUTPUT_ENV)
        .map(std::path::PathBuf::from)
        .expect("CODEX_SWITCH_DOWNGRADE_FIXTURE_ROOT must be set");
    assert!(output_root.is_absolute(), "fixture root must be absolute");
    assert!(
        !output_root.exists(),
        "fixture root must not already exist; the test never deletes prior evidence"
    );

    let canonical_root = output_root.join("input/canonical");
    let data_root = output_root.join("input/data");
    let packages_root = output_root.join("packages");
    fs::create_dir_all(&canonical_root).unwrap();
    fs::create_dir_all(&data_root).unwrap();
    fs::create_dir_all(&packages_root).unwrap();
    create_profile(&canonical_root);
    create_malicious_saved_plus_slot(&canonical_root, &data_root);
    create_committed_downgrade_certificate(
        &canonical_root,
        &data_root,
        &output_root.join("input/backup"),
    );

    let mut packages = Vec::new();
    let requested_patch = env::var("CODEX_SWITCH_DOWNGRADE_FIXTURE_PATCH")
        .ok()
        .map(|value| value.parse::<u8>().expect("fixture patch must be 0..7"));
    let patches = requested_patch.map_or_else(
        || (0..=7).collect::<Vec<_>>(),
        |patch| {
            assert!(patch <= 7, "fixture patch must be 0..7");
            vec![patch]
        },
    );
    for patch in patches {
        let version = format!("v0.2.{patch}");
        let operation_id = format!("downgrade-runtime-v0-2-{patch}");
        let plan = prepare_downgrade_export(
            &canonical_root,
            &data_root,
            &packages_root,
            &operation_id,
            &version,
        )
        .unwrap();
        let receipt = execute_downgrade_export(&plan, || Ok(())).unwrap();
        let manifest = verify_downgrade_package(&receipt.package_dir).unwrap();
        assert_eq!(manifest.target.version, version);
        assert_eq!(manifest.logical_session_count, 1);
        assert_eq!(manifest.session_file_count, 1);
        assert_eq!(manifest.initial_runtime_slot_count, 1);
        let exported_slot_config = receipt
            .package_dir
            .join("appdata/codex-switch/runtimes/plus/config.toml");
        assert!(exported_slot_config.is_file());
        assert!(!fs::read_to_string(&exported_slot_config)
            .unwrap()
            .contains("sqlite_home"));
        let packaged_goals = receipt.package_dir.join("codex-home/goals_1.sqlite");
        assert!(packaged_goals.is_file());
        let goals = Connection::open(packaged_goals).unwrap();
        assert_eq!(
            goals
                .query_row("SELECT COUNT(*) FROM thread_goals", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            goals
                .query_row(
                    "SELECT COUNT(*) FROM thread_goal_continuation_deferrals",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        packages.push(json!({
            "version": version,
            "relativePackagePath": receipt
                .package_dir
                .strip_prefix(&output_root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/"),
            "logicalSessionCount": receipt.logical_session_count,
            "sessionFileCount": receipt.session_file_count,
            "packageBytes": receipt.package_bytes,
            "structurallyVerified": receipt.structurally_verified,
            "targetRuntimeVerificationRequired": receipt.target_runtime_verification_required,
            "savedRuntimeSlotSanitized": true,
        }));
    }

    fs::write(
        output_root.join("fixture-index.json"),
        serde_json::to_vec_pretty(&json!({
            "schemaVersion": 1,
            "threadId": THREAD_ID,
            "packages": packages,
        }))
        .unwrap(),
    )
    .unwrap();
}

fn create_committed_downgrade_certificate(
    canonical_root: &Path,
    data_root: &Path,
    backup_destination: &Path,
) {
    let operation_id = "migration-downgrade-runtime-fixture";
    fs::create_dir_all(backup_destination).unwrap();
    let store = OperationLedgerStore::new(data_root);
    store
        .create(
            operation_id,
            SessionStorageOperationKind::Migration,
            canonical_root,
        )
        .unwrap();
    let report =
        run_migration_preflight(canonical_root, data_root, operation_id, backup_destination)
            .unwrap();
    assert!(report.ready_for_backup, "{:?}", report.blockers);
    persist_migration_preflight(data_root, &report).unwrap();
    store
        .update(operation_id, |ledger| {
            ledger.backup_root = Some(report.backup_destination.join(operation_id));
            Ok(())
        })
        .unwrap();
    for phase in [
        SessionStorageOperationPhase::Preflight,
        SessionStorageOperationPhase::Backup,
        SessionStorageOperationPhase::BackupVerified,
        SessionStorageOperationPhase::PlanReady,
        SessionStorageOperationPhase::Applying,
        SessionStorageOperationPhase::Validating,
    ] {
        store.transition(operation_id, phase).unwrap();
    }
    prepare_canonical_storage_state(
        data_root,
        canonical_root,
        operation_id,
        &report.plan.inventory_fingerprint,
    )
    .unwrap();
    store
        .transition(operation_id, SessionStorageOperationPhase::Committed)
        .unwrap();
    finalize_canonical_storage_state(data_root, canonical_root, operation_id).unwrap();
}

#[test]
#[ignore = "manual native Codex gate; updates only CODEX_SWITCH_DOWNGRADE_FIXTURE_ROOT"]
fn verifies_every_supported_v02_fixture_with_native_codex() {
    let output_root = env::var_os(OUTPUT_ENV)
        .map(std::path::PathBuf::from)
        .expect("CODEX_SWITCH_DOWNGRADE_FIXTURE_ROOT must be set");
    assert!(output_root.is_absolute(), "fixture root must be absolute");
    assert!(
        output_root.join("fixture-index.json").is_file(),
        "the structural fixture generator must run first"
    );
    let verifier = NativeCodexBackupVerifier::discover().unwrap();
    let mut results = Vec::new();
    let requested_patch = env::var("CODEX_SWITCH_DOWNGRADE_FIXTURE_PATCH")
        .ok()
        .map(|value| value.parse::<u8>().expect("fixture patch must be 0..7"));
    let patches = requested_patch.map_or_else(
        || (0..=7).collect::<Vec<_>>(),
        |patch| {
            assert!(patch <= 7, "fixture patch must be 0..7");
            vec![patch]
        },
    );
    for patch in patches {
        let version = format!("v0.2.{patch}");
        let package_dir = output_root.join(format!(
            "packages/codex-switch-downgrade-0-2-{patch}-downgrade-runtime-v0-2-{patch}"
        ));
        let isolated_root = output_root.join(format!("native-runtime-v0-2-{patch}"));
        let manifest =
            verify_downgrade_package_with_runtime(&package_dir, &isolated_root, &verifier)
                .unwrap_or_else(|error| panic!("{version} native verification failed: {error}"));
        let runtime = manifest.native_runtime_verification.unwrap();
        assert_eq!(manifest.target.version, version);
        assert_eq!(runtime.expected_session_count, 1);
        assert_eq!(runtime.listed_session_count, 1);
        assert_eq!(runtime.resumed_session_count, 1);
        assert_eq!(runtime.continued_session_count, 4);
        assert_eq!(
            runtime.available_categories,
            vec!["ordinary", "long", "subagent", "conflictCanonical", "tool"]
        );
        assert_eq!(runtime.continued_categories, runtime.available_categories);
        assert_eq!(runtime.tool_session_count, 1);
        assert!(runtime.tool_round_trip_verified);
        assert_eq!(runtime.conflict_payload_count, 0);
        assert!(runtime.conflict_payloads_verified);
        assert!(runtime.conflict_proofs.is_empty());
        let capability_conflict_proof = runtime
            .capability_conflict_proof
            .as_ref()
            .expect("conflict capability proof");
        assert_eq!(capability_conflict_proof.relation, "divergent");
        assert_ne!(
            capability_conflict_proof.canonical_sha256,
            capability_conflict_proof.recycle_sha256
        );
        let runtime_binary_identity = runtime
            .runtime_binary_identity
            .as_ref()
            .expect("runtime binary identity");
        assert!(runtime_binary_identity.bytes > 0);
        assert!(!isolated_root.exists());
        results.push(json!({
            "version": version,
            "expectedSessionCount": runtime.expected_session_count,
            "listedSessionCount": runtime.listed_session_count,
            "resumedSessionCount": runtime.resumed_session_count,
            "continuedSessionCount": runtime.continued_session_count,
            "toolSessionCount": runtime.tool_session_count,
            "toolRoundTripVerified": runtime.tool_round_trip_verified,
            "availableCategories": runtime.available_categories,
            "continuedCategories": runtime.continued_categories,
            "conflictPayloadCount": runtime.conflict_payload_count,
            "conflictPayloadsVerified": runtime.conflict_payloads_verified,
            "conflictProofs": runtime.conflict_proofs,
            "capabilityConflictProof": capability_conflict_proof,
            "runtimeBinaryIdentity": runtime_binary_identity,
            "verifiedAtMs": runtime.verified_at_ms,
        }));
    }
    fs::write(
        output_root.join("native-runtime-verification.json"),
        serde_json::to_vec_pretty(&json!({
            "schemaVersion": 1,
            "runtime": "native-codex-app-server",
            "packages": results,
        }))
        .unwrap(),
    )
    .unwrap();
}

fn create_malicious_saved_plus_slot(canonical_root: &Path, data_root: &Path) {
    let live_config = fs::read_to_string(canonical_root.join("config.toml")).unwrap();
    fs::write(
        canonical_root.join("config.toml"),
        format!(
            "model = \"gpt-test\"\nsqlite_home = \"{}\"\n",
            canonical_root.to_string_lossy().replace('\\', "\\\\")
        ),
    )
    .unwrap();
    let store = RuntimeStore::new(data_root.join("runtimes"));
    store.import_plus_from_home(canonical_root, false).unwrap();
    fs::write(canonical_root.join("config.toml"), live_config).unwrap();

    let slot = store.runtime_dir(PLUS_RUNTIME_ID);
    let escaped = format!(
        "model = \"gpt-test\"\nsqlite_home = \"{}\"\n",
        canonical_root.to_string_lossy().replace('\\', "\\\\")
    );
    fs::write(slot.join("config.toml"), &escaped).unwrap();
    let mut bundle: serde_json::Value =
        serde_json::from_slice(&fs::read(slot.join("bundle.json")).unwrap()).unwrap();
    bundle["configSha256"] =
        serde_json::Value::String(format!("{:x}", Sha256::digest(escaped.as_bytes())));
    fs::write(
        slot.join("bundle.json"),
        serde_json::to_vec_pretty(&bundle).unwrap(),
    )
    .unwrap();
    assert!(store
        .load_runtime_files(PLUS_RUNTIME_ID)
        .unwrap()
        .config_toml
        .contains("sqlite_home"));
}

fn create_profile(root: &Path) {
    let session_dir = root.join("sessions/2026/08/12");
    fs::create_dir_all(&session_dir).unwrap();
    fs::write(
        root.join("auth.json"),
        br#"{"auth_mode":"chatgpt","tokens":{"access_token":"fixture-token"}}"#,
    )
    .unwrap();
    fs::write(
        root.join("config.toml"),
        format!(
            "model = \"codex-switch-downgrade-probe\"\nmodel_provider = \"openai_custom\"\nsqlite_home = \"{}\"\n\n[model_providers.openai_custom]\nname = \"Loopback fixture\"\nbase_url = \"http://127.0.0.1:9/v1\"\nwire_api = \"responses\"\nrequires_openai_auth = false\n",
            root.to_string_lossy().replace('\\', "\\\\")
        ),
    )
    .unwrap();
    fs::write(
        root.join("session_index.jsonl"),
        format!("{{\"id\":\"{THREAD_ID}\",\"thread_name\":\"downgrade fixture\"}}\n"),
    )
    .unwrap();
    let session = session_dir.join("rollout-downgrade-fixture.jsonl");
    write_session(&session);
    create_state_db(&root.join("state_5.sqlite"), &session);
    create_goals_db(&root.join("goals_1.sqlite"));
}

fn create_goals_db(path: &Path) {
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS thread_goals (
                thread_id TEXT PRIMARY KEY NOT NULL,
                goal_id TEXT NOT NULL,
                objective TEXT NOT NULL,
                status TEXT NOT NULL CHECK(status IN ('active','paused','blocked','usage_limited','budget_limited','complete')),
                token_budget INTEGER,
                tokens_used INTEGER NOT NULL DEFAULT 0,
                time_used_seconds INTEGER NOT NULL DEFAULT 0,
                created_at_ms INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS thread_goal_continuation_deferrals (
                thread_id TEXT PRIMARY KEY NOT NULL REFERENCES thread_goals(thread_id) ON DELETE CASCADE
             );
             INSERT OR REPLACE INTO thread_goals VALUES
                ('{THREAD_ID}','fixture-goal','preserve downgrade goal','active',NULL,11,12,13,14);
             INSERT OR REPLACE INTO thread_goal_continuation_deferrals VALUES ('{THREAD_ID}');"
        ))
        .unwrap();
    let columns = connection
        .prepare("PRAGMA table_xinfo(thread_goals)")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        columns,
        [
            "thread_id",
            "goal_id",
            "objective",
            "status",
            "token_budget",
            "tokens_used",
            "time_used_seconds",
            "created_at_ms",
            "updated_at_ms",
        ],
        "native Codex goals schema changed"
    );
}

fn write_session(path: &Path) {
    let turn_id = format!("turn-{THREAD_ID}-0");
    let lines = [
        json!({
            "type": "session_meta",
            "timestamp": "2026-08-12T00:00:00Z",
            "payload": {
                "session_id": THREAD_ID,
                "id": THREAD_ID,
                "timestamp": "2026-08-12T00:00:00Z",
                "cwd": ".",
                "originator": "codex-switch-downgrade-fixture",
                "cli_version": "0.147.0",
                "source": "cli",
                "thread_source": "user",
                "model_provider": "openai",
                "base_instructions": {"text": ""},
                "history_mode": "legacy",
                "context_window": {"window_id": THREAD_ID}
            }
        }),
        json!({"type":"event_msg","timestamp":"2026-08-12T00:00:01Z","payload":{"type":"task_started","turn_id":turn_id,"started_at":1786492800_u64,"model_context_window":258400,"collaboration_mode_kind":"default"}}),
        json!({"type":"turn_context","timestamp":"2026-08-12T00:00:01Z","payload":{"turn_id":turn_id}}),
        json!({"type":"response_item","timestamp":"2026-08-12T00:00:01Z","payload":{"type":"message","id":format!("msg-{THREAD_ID}-user"),"role":"user","content":[{"type":"input_text","text":"downgrade fixture"}],"internal_chat_message_metadata_passthrough":{"turn_id":turn_id}}}),
        json!({"type":"event_msg","timestamp":"2026-08-12T00:00:01Z","payload":{"type":"user_message","message":"downgrade fixture"}}),
        json!({"type":"response_item","timestamp":"2026-08-12T00:00:02Z","payload":{"type":"message","id":format!("msg-{THREAD_ID}-assistant"),"role":"assistant","content":[{"type":"output_text","text":"downgrade fixture reply"}],"internal_chat_message_metadata_passthrough":{"turn_id":turn_id}}}),
        json!({"type":"event_msg","timestamp":"2026-08-12T00:00:02Z","payload":{"type":"agent_message","message":"downgrade fixture reply"}}),
        json!({"type":"event_msg","timestamp":"2026-08-12T00:00:02Z","payload":{"type":"task_complete","turn_id":turn_id,"last_agent_message":"downgrade fixture reply","started_at":1786492800_u64,"completed_at":1786492801_u64,"duration_ms":1000,"time_to_first_token_ms":1}}),
    ];
    let body = lines
        .iter()
        .map(|line| serde_json::to_string(line).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fs::write(path, body).unwrap();
}

fn create_state_db(path: &Path, rollout: &Path) {
    initialize_native_state_db(path.parent().unwrap());
    let connection = Connection::open(path).unwrap();
    let changed = connection
        .execute(
            "UPDATE threads SET \
                rollout_path = ?2, model_provider = 'openai_custom', cwd = '.', \
                title = 'Downgrade fixture', preview = 'Synthetic runtime contract', \
                archived = 0, created_at = 1, updated_at = 2, created_at_ms = 1000, \
                updated_at_ms = 2000, recency_at = 2, recency_at_ms = 2000 \
             WHERE id = ?1",
            (THREAD_ID, rollout.to_string_lossy().to_string()),
        )
        .unwrap();
    if changed == 0 {
        connection
            .execute(
                "INSERT INTO threads (\
                id, rollout_path, created_at, updated_at, source, model_provider, cwd, title,\
                sandbox_policy, approval_mode, has_user_event, archived, first_user_message,\
                created_at_ms, updated_at_ms, preview, recency_at, recency_at_ms\
             ) VALUES (\
                ?1, ?2, 1, 2, 'cli', 'openai_custom', '.', 'Downgrade fixture',\
                '{\"type\":\"danger-full-access\"}', 'never', 1, 0, 'downgrade fixture',\
                1000, 2000, 'Synthetic runtime contract', 2, 2000\
             )",
                (THREAD_ID, rollout.to_string_lossy().to_string()),
            )
            .unwrap();
    }
}

fn initialize_native_state_db(codex_home: &Path) {
    let executable = discover_native_codex();
    let workspace = codex_home.join("runtime-schema-workspace");
    let user_home = codex_home.join("runtime-schema-user");
    let appdata = user_home.join("AppData/Roaming");
    let localappdata = user_home.join("AppData/Local");
    for directory in [&workspace, &appdata, &localappdata] {
        fs::create_dir_all(directory).unwrap();
    }

    let mut child = Command::new(executable)
        .args(["app-server", "--stdio", "--disable", "plugins"])
        .current_dir(&workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env("CODEX_HOME", codex_home)
        .env("CODEX_SQLITE_HOME", codex_home)
        .env("HOME", &user_home)
        .env("USERPROFILE", &user_home)
        .env("APPDATA", &appdata)
        .env("LOCALAPPDATA", &localappdata)
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("ALL_PROXY", "http://127.0.0.1:9")
        .env_remove("OPENAI_API_KEY")
        .env_remove("CODEX_API_KEY")
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (line_tx, line_rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });
    writeln!(
        stdin,
        "{}",
        json!({
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": {"name": "codex-switch-downgrade-fixture", "version": "0.3.0"},
                "capabilities": {"experimentalApi": false}
            }
        })
    )
    .unwrap();
    stdin.flush().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let acknowledged = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break false;
        }
        let Ok(line) = line_rx.recv_timeout(remaining) else {
            break false;
        };
        let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if message.get("id").and_then(serde_json::Value::as_u64) == Some(1) {
            break message.get("result").is_some();
        }
    };
    if acknowledged {
        writeln!(stdin, "{}", json!({"method": "initialized", "params": {}})).unwrap();
        stdin.flush().unwrap();
    }
    drop(stdin);
    if !acknowledged {
        let _ = child.kill();
    }
    let status = child.wait().unwrap();
    reader.join().unwrap();
    assert!(
        acknowledged && status.success(),
        "native Codex failed to initialize the synthetic state database: {}",
        status
    );
    assert!(codex_home.join("state_5.sqlite").is_file());
    fs::remove_dir_all(workspace).unwrap();
    fs::remove_dir_all(user_home).unwrap();
}

fn discover_native_codex() -> PathBuf {
    if let Some(path) = env::var_os("CODEX_SWITCH_CODEX_RUNTIME_EXE") {
        let path = PathBuf::from(path);
        assert!(path.is_absolute() && path.is_file());
        return path;
    }
    let output = Command::new("where.exe").arg("codex.exe").output().unwrap();
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .find(|path| path.is_absolute() && path.is_file())
        .expect("native codex.exe must be available")
}

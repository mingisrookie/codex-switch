use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, SystemTime},
};

#[cfg(feature = "runtime-evidence")]
use codex_switch_lib::codex_paths::resolve_user_codex_paths;
use codex_switch_lib::session_storage::{
    marker::inspect_provider_marker,
    migration::{run_migration_preflight, MigrationSessionAction},
    model::MarkerStatus,
    semantic::read_semantic_session,
    shadow_scan::{load_last_shadow_report, run_shadow_scan},
    StorageScanStatus,
};
#[cfg(feature = "runtime-evidence")]
use codex_switch_lib::{
    run_automatic_gc_safe_window_evidence_at,
    session_storage::{
        codex_runtime_verifier::NativeCodexBackupVerifier,
        migration::{migration_backup_sources_for_preflight, persist_migration_preflight},
        migration_apply::{
            apply_prepared_migration, cleanup_migration_staging, prepare_migration_apply_plan,
            validate_applied_migration, verify_applied_migration_with_runtime,
        },
        migration_backup::{create_migration_backup, verify_migration_backup_with_runtime},
        operation_ledger::{
            OperationLedgerStore, SessionStorageOperationKind, SessionStorageOperationPhase,
        },
        storage_state::{
            finalize_canonical_storage_state, prepare_canonical_storage_state,
            set_automatic_cleanup_enabled,
        },
    },
    AutomaticGcSafeWindowObservation,
};
use rusqlite::{params, Connection, OpenFlags, MAIN_DB};
use sha2::{Digest, Sha256};
use tempfile::{tempdir, tempdir_in};
use walkdir::WalkDir;

#[derive(Debug, PartialEq, Eq)]
struct SourceSnapshot {
    path: PathBuf,
    bytes: u64,
    sha256: [u8; 32],
    modified: SystemTime,
}

#[cfg(all(windows, feature = "runtime-evidence"))]
fn regular_file_identity(path: &Path) -> (u64, u64) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION},
    };

    let file = fs::File::open(path).unwrap();
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    let ok =
        unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut information) };
    assert_ne!(ok, 0, "failed to read test file identity");
    (
        u64::from(information.dwVolumeSerialNumber),
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow),
    )
}

#[cfg(all(unix, feature = "runtime-evidence"))]
fn regular_file_identity(path: &Path) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;

    let metadata = fs::metadata(path).unwrap();
    (metadata.dev(), metadata.ino())
}

#[test]
fn shadow_scan_builds_a_multidatabase_report_without_mutating_sources() {
    let root = tempdir().unwrap();
    let home = root.path().join("home");
    let data = root.path().join("data");
    let shared_home = data.join("shared-sessions");
    let relay_home = data.join("relay-sqlite");
    let backup_home = data.join("backups/legacy-full");
    for directory in [&home, &shared_home, &relay_home, &backup_home] {
        fs::create_dir_all(directory).unwrap();
    }
    fs::write(
        home.join("config.toml"),
        format!("sqlite_home = {:?}\n", home.to_string_lossy()),
    )
    .unwrap();

    let canonical_a = write_session(&home, "canonical-a.jsonl", "thread-a", "openai", &["one"]);
    let canonical_b = write_session(&home, "canonical-b.jsonl", "thread-b", "openai", &["left"]);
    let shared_a = write_session(
        &shared_home,
        "shared-a.jsonl",
        "thread-a",
        "openai_custom",
        &["one"],
    );
    let shared_b = write_session(
        &shared_home,
        "shared-b.jsonl",
        "thread-b",
        "openai_custom",
        &["right"],
    );

    let current_db = home.join("state_5.sqlite");
    let relay_db = relay_home.join("state_5.sqlite");
    let shared_db = shared_home.join("state_5.sqlite");
    let backup_db = backup_home.join("state_5.sqlite");
    for goals in [
        home.join("goals_1.sqlite"),
        relay_home.join("goals_1.sqlite"),
        shared_home.join("goals_1.sqlite"),
    ] {
        create_goals_database(&goals);
    }
    let current_connection = create_database(
        &current_db,
        &[
            ("thread-a", &canonical_a, "openai"),
            ("thread-b", &canonical_b, "openai"),
        ],
        true,
    );
    drop(create_database(
        &relay_db,
        &[
            ("thread-a", &shared_a, "openai_custom"),
            ("thread-b", &shared_b, "openai_custom"),
        ],
        false,
    ));
    drop(create_database(
        &shared_db,
        &[
            ("thread-a", &shared_a, "openai_custom"),
            ("thread-b", &shared_b, "openai_custom"),
        ],
        false,
    ));
    drop(create_database(
        &backup_db,
        &[("thread-a", &canonical_a, "openai")],
        false,
    ));

    let tracked = [
        canonical_a,
        canonical_b,
        shared_a,
        shared_b,
        current_db,
        relay_db,
        shared_db,
        backup_db,
        home.join("goals_1.sqlite"),
        relay_home.join("goals_1.sqlite"),
        shared_home.join("goals_1.sqlite"),
    ];
    let before = snapshot_sources(&tracked);

    let first = run_shadow_scan(&home, &data).unwrap();
    let second = run_shadow_scan(&home, &data).unwrap();
    let after = snapshot_sources(&tracked);

    assert_eq!(before, after);
    assert_eq!(first.status, StorageScanStatus::ReviewRequired);
    assert!(!first.deletion_enabled);
    assert!(first.summary.online_scan_only);
    assert!(first.summary.non_atomic_across_databases);
    assert_eq!(first.summary.runtime_database_count, 3);
    assert_eq!(first.summary.backup_database_count, 1);
    assert_eq!(first.summary.runtime_reference_count, 6);
    assert_eq!(first.summary.missing_runtime_reference_count, 0);
    assert_eq!(first.summary.mismatched_runtime_reference_count, 0);
    assert_eq!(first.summary.logical_session_count, 2);
    assert_eq!(first.summary.session_file_count, 4);
    assert_eq!(first.summary.high_confidence_copy_count, 1);
    assert_eq!(first.summary.conflict_session_count, 1);
    assert_eq!(first.summary.relation_counts.equal_except_provider, 1);
    assert_eq!(first.summary.relation_counts.divergent, 1);
    assert_eq!(second.summary.cache_hit_count, 4);
    assert_eq!(second.summary.cache_miss_count, 0);
    assert_eq!(
        load_last_shadow_report(&data).unwrap(),
        Some(second.clone())
    );

    let encoded = serde_json::to_string(&second).unwrap();
    assert!(!encoded.contains(&root.path().to_string_lossy().to_string()));
    assert!(!encoded.contains("left"));
    assert!(!encoded.contains("right"));
    assert!(!data.join("session-storage-v1/shadow").exists());

    drop(current_connection);
}

#[test]
fn concurrent_branch_writer_child() {
    if env::var_os("CODEX_SWITCH_BRANCH_WRITER_CHILD").is_none() {
        return;
    }

    let branch_path = required_env_path("CODEX_SWITCH_BRANCH_WRITER_PATH");
    let ready_path = required_env_path("CODEX_SWITCH_BRANCH_WRITER_READY");
    let release_path = required_env_path("CODEX_SWITCH_BRANCH_WRITER_RELEASE");
    let tail = env::var("CODEX_SWITCH_BRANCH_WRITER_TAIL")
        .expect("CODEX_SWITCH_BRANCH_WRITER_TAIL must be set for child writer");

    fs::write(&ready_path, b"ready").unwrap();
    let deadline = SystemTime::now() + Duration::from_secs(10);
    while !release_path.is_file() {
        assert!(
            SystemTime::now() < deadline,
            "branch writer release barrier timed out"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    let line = serde_json::json!({
        "type": "event_msg",
        "payload": {"type": "user_message", "message": tail}
    })
    .to_string();
    let mut branch = fs::OpenOptions::new()
        .append(true)
        .open(&branch_path)
        .unwrap();
    branch.write_all(line.as_bytes()).unwrap();
    branch.write_all(b"\n").unwrap();
    branch.sync_all().unwrap();
}

#[test]
fn concurrent_process_branches_remain_divergent_and_are_never_auto_merged() {
    let root = tempdir().unwrap();
    let home = root.path().join("home");
    let data = root.path().join("data");
    let shared_home = data.join("shared-sessions");
    let backup_destination = root.path().join("backup-destination");
    for directory in [&home, &shared_home, &backup_destination] {
        fs::create_dir_all(directory).unwrap();
    }
    fs::write(
        home.join("config.toml"),
        format!("sqlite_home = {:?}\n", home.to_string_lossy()),
    )
    .unwrap();

    let left = write_session(&home, "left.jsonl", "thread-a", "openai", &["base"]);
    let right = write_session(
        &shared_home,
        "right.jsonl",
        "thread-a",
        "openai_custom",
        &["base"],
    );
    drop(create_database(
        &home.join("state_5.sqlite"),
        &[("thread-a", &left, "openai")],
        false,
    ));
    drop(create_database(
        &shared_home.join("state_5.sqlite"),
        &[("thread-a", &right, "openai_custom")],
        false,
    ));
    create_goals_database(&home.join("goals_1.sqlite"));
    create_goals_database(&shared_home.join("goals_1.sqlite"));

    let release = root.path().join("release-writers");
    let left_ready = root.path().join("left-ready");
    let right_ready = root.path().join("right-ready");
    let test_binary = env::current_exe().unwrap();
    let spawn_writer = |branch_path: &Path, ready_path: &Path, tail: &str| {
        Command::new(&test_binary)
            .args(["--exact", "concurrent_branch_writer_child", "--nocapture"])
            .env("CODEX_SWITCH_BRANCH_WRITER_CHILD", "1")
            .env("CODEX_SWITCH_BRANCH_WRITER_PATH", branch_path)
            .env("CODEX_SWITCH_BRANCH_WRITER_READY", ready_path)
            .env("CODEX_SWITCH_BRANCH_WRITER_RELEASE", &release)
            .env("CODEX_SWITCH_BRANCH_WRITER_TAIL", tail)
            .spawn()
            .unwrap()
    };
    let mut left_writer = spawn_writer(&left, &left_ready, "left-tail");
    let mut right_writer = spawn_writer(&right, &right_ready, "right-tail");

    let deadline = SystemTime::now() + Duration::from_secs(10);
    while !left_ready.is_file() || !right_ready.is_file() {
        assert!(
            SystemTime::now() < deadline,
            "branch writers did not reach the process barrier"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    fs::write(&release, b"release").unwrap();
    assert!(left_writer.wait().unwrap().success());
    assert!(right_writer.wait().unwrap().success());

    let left_bytes = fs::read(&left).unwrap();
    let right_bytes = fs::read(&right).unwrap();
    assert!(String::from_utf8_lossy(&left_bytes).contains("left-tail"));
    assert!(String::from_utf8_lossy(&right_bytes).contains("right-tail"));
    assert_ne!(left_bytes, right_bytes);

    let preflight =
        run_migration_preflight(&home, &data, "multi-process-branches", &backup_destination)
            .unwrap();
    let session = preflight
        .plan
        .sessions
        .iter()
        .find(|session| session.thread_id == "thread-a")
        .expect("logical thread must remain in the migration plan");
    assert_eq!(session.action, MigrationSessionAction::Conflict);
    assert_eq!(preflight.conflict_count, 1);
    assert_eq!(preflight.plan.conflicts.len(), 1);
    let conflict = &preflight.plan.conflicts[0];
    assert!(!conflict.default_overwrite);
    let conflict_paths = BTreeSet::from([
        conflict.current_path.clone(),
        conflict.candidate_path.clone(),
    ]);
    assert_eq!(
        conflict_paths,
        BTreeSet::from([left.clone(), right.clone()])
    );
    assert_eq!(fs::read(&left).unwrap(), left_bytes);
    assert_eq!(fs::read(&right).unwrap(), right_bytes);
}

#[test]
#[ignore = "requires explicit local source roots and creates an isolated real-data fixture"]
fn shadow_scan_real_isolated_fixture_probe() {
    let source_home = required_env_path("CODEX_SWITCH_SHADOW_SOURCE_HOME");
    let source_data_root = required_env_path("CODEX_SWITCH_SHADOW_SOURCE_DATA_ROOT");
    let evidence_path = required_env_path("CODEX_SWITCH_SHADOW_EVIDENCE");
    let evidence_parent = evidence_path.parent().expect("evidence output parent");
    fs::create_dir_all(evidence_parent).unwrap();

    let fixture = tempdir_in(evidence_parent).unwrap();
    let isolated_home = fixture.path().join("home");
    let isolated_data_root = fixture.path().join("data");
    fs::create_dir_all(&isolated_home).unwrap();
    fs::create_dir_all(&isolated_data_root).unwrap();
    fs::write(
        isolated_home.join("config.toml"),
        format!(
            "sqlite_home = {:?}\n",
            isolated_home.to_string_lossy().to_string()
        ),
    )
    .unwrap();

    let copied_databases = snapshot_real_databases(
        &source_home,
        &source_data_root,
        &isolated_home,
        &isolated_data_root,
    );
    assert!(
        copied_databases.len() >= 3,
        "expected current, Relay and Shared DB snapshots"
    );

    let references = read_fixture_references(&copied_databases);
    let selected = select_stable_duplicate_groups(&references, 96, 512 * 1024 * 1024);
    assert!(
        !selected.is_empty(),
        "expected at least one stable real duplicate group"
    );
    let copied_sessions = copy_selected_sessions(
        &selected,
        &references,
        &source_home,
        &source_data_root,
        &isolated_home,
        &isolated_data_root,
    );
    let tracked_sources = copied_sessions.keys().cloned().collect::<Vec<_>>();
    let before = snapshot_sources(&tracked_sources);
    let _ = rewrite_fixture_references(&copied_databases, &copied_sessions);

    let first = run_shadow_scan(&isolated_home, &isolated_data_root).unwrap();
    let second = run_shadow_scan(&isolated_home, &isolated_data_root).unwrap();
    let after = snapshot_sources(&tracked_sources);

    assert_eq!(before, after);
    assert!(!first.deletion_enabled);
    assert!(first.summary.online_scan_only);
    assert!(first.summary.non_atomic_across_databases);
    assert!(first.summary.runtime_database_count >= 3);
    assert!(first.summary.logical_session_count > 0);
    assert!(first.summary.session_file_count >= first.summary.logical_session_count);
    assert_eq!(
        second.summary.cache_hit_count,
        second.summary.session_file_count
    );
    assert_eq!(second.summary.cache_miss_count, 0);
    assert_eq!(
        load_last_shadow_report(&isolated_data_root).unwrap(),
        Some(second.clone())
    );

    let sampled_bytes = tracked_sources.iter().fold(0_u64, |total, path| {
        total.saturating_add(fs::metadata(path).unwrap().len())
    });
    let proof = serde_json::json!({
        "schemaVersion": 1,
        "sourceKind": "isolated-real-session-sample",
        "sourceDatabaseSnapshotCount": copied_databases.len(),
        "sampleLogicalGroupCount": selected.len(),
        "sampleSessionFileCount": tracked_sources.len(),
        "sampleSessionBytes": sampled_bytes,
        "sourcesUnchanged": before == after,
        "first": first,
        "second": second,
    });
    let encoded = serde_json::to_vec_pretty(&proof).unwrap();
    let encoded_text = String::from_utf8_lossy(&encoded);
    assert!(!encoded_text.contains(&source_home.to_string_lossy().to_string()));
    assert!(!encoded_text.contains(&source_data_root.to_string_lossy().to_string()));
    fs::write(&evidence_path, encoded).unwrap();
    fixture.close().unwrap();
}

#[test]
#[cfg(feature = "runtime-evidence")]
#[ignore = "requires explicit local source roots; destructively tests only an isolated real-data fixture"]
fn canonical_migration_real_isolated_adversarial_probe() {
    let source_home = required_env_path("CODEX_SWITCH_SHADOW_SOURCE_HOME");
    let source_data_root = required_env_path("CODEX_SWITCH_SHADOW_SOURCE_DATA_ROOT");
    let evidence_path = required_env_path("CODEX_SWITCH_ADVERSARIAL_EVIDENCE");
    let evidence_parent = evidence_path.parent().expect("evidence output parent");
    fs::create_dir_all(evidence_parent).unwrap();

    let fixture = tempdir_in(evidence_parent).unwrap();
    let isolated_home = fixture.path().join("home");
    let isolated_data_root = fixture.path().join("data");
    let backup_destination = fixture.path().join("backups");
    fs::create_dir_all(&isolated_home).unwrap();
    fs::create_dir_all(&isolated_data_root).unwrap();
    fs::create_dir_all(&backup_destination).unwrap();
    fs::write(
        isolated_home.join("config.toml"),
        format!(
            "sqlite_home = {:?}\n",
            isolated_home.to_string_lossy().to_string()
        ),
    )
    .unwrap();

    let copied_databases = snapshot_core_real_databases(
        &source_home,
        &source_data_root,
        &isolated_home,
        &isolated_data_root,
    );
    assert!(copied_databases.len() >= 3);
    let references = read_fixture_references(&copied_databases);
    let selected = select_stable_duplicate_groups(&references, 3, 32 * 1024 * 1024);
    assert!(!selected.is_empty());
    let selected_valid_marker_count = references
        .iter()
        .filter(|reference| selected.contains(&reference.thread_id))
        .map(|reference| &reference.source_path)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|path| has_valid_provider_marker(path))
        .count();
    assert!(selected_valid_marker_count > 0);
    let available_tool_group_count = references
        .iter()
        .filter(|reference| reference.has_tool_pair)
        .map(|reference| &reference.thread_id)
        .collect::<BTreeSet<_>>()
        .len();
    let selected_tool_group_count = references
        .iter()
        .filter(|reference| selected.contains(&reference.thread_id) && reference.has_tool_pair)
        .map(|reference| &reference.thread_id)
        .collect::<BTreeSet<_>>()
        .len();
    if available_tool_group_count > 0 {
        assert!(selected_tool_group_count > 0);
    }
    let available_subagent_group_count = references
        .iter()
        .filter(|reference| reference.is_subagent)
        .map(|reference| &reference.thread_id)
        .collect::<BTreeSet<_>>()
        .len();
    let selected_subagent_group_count = references
        .iter()
        .filter(|reference| selected.contains(&reference.thread_id) && reference.is_subagent)
        .map(|reference| &reference.thread_id)
        .collect::<BTreeSet<_>>()
        .len();
    if available_subagent_group_count > 0 {
        assert!(selected_subagent_group_count > 0);
    }
    let copied_sessions = copy_selected_sessions(
        &selected,
        &references,
        &source_home,
        &source_data_root,
        &isolated_home,
        &isolated_data_root,
    );
    let tracked_sources = copied_sessions.keys().cloned().collect::<Vec<_>>();
    let source_before = snapshot_sources(&tracked_sources);
    let pruned_database_reference_count =
        rewrite_fixture_references(&copied_databases, &copied_sessions);
    assert_fixture_references_are_isolated(&copied_databases, fixture.path());
    normalize_fixture_database_journals(&copied_databases);

    let operation_id = "real-isolated-adversarial-migration";
    let migration_store = OperationLedgerStore::new(&isolated_data_root);
    migration_store
        .create(
            operation_id,
            SessionStorageOperationKind::Migration,
            &isolated_home,
        )
        .unwrap();
    migration_store
        .transition(operation_id, SessionStorageOperationPhase::Preflight)
        .unwrap();
    let preflight = run_migration_preflight(
        &isolated_home,
        &isolated_data_root,
        operation_id,
        &backup_destination,
    )
    .unwrap();
    assert!(preflight.ready_for_backup, "{:?}", preflight.blockers);
    assert!(preflight.provider_copy_count > 0);
    let planned_marked_provider_paths = preflight
        .plan
        .sessions
        .iter()
        .filter(|session| {
            session.action
                != codex_switch_lib::session_storage::migration::MigrationSessionAction::Conflict
        })
        .flat_map(|session| {
            let retained = (session.retained_path != session.canonical_path
                && has_valid_provider_marker(&session.retained_path))
            .then(|| session.retained_path.clone());
            retained.into_iter().chain(
                session
                    .duplicates
                    .iter()
                    .filter(|duplicate| {
                        duplicate.path != session.canonical_path
                            && duplicate.marker_status == MarkerStatus::Valid
                    })
                    .map(|duplicate| duplicate.path.clone()),
            )
        })
        .collect::<BTreeSet<_>>();
    let planned_marked_provider_copy_count = planned_marked_provider_paths.len();
    assert!(
        planned_marked_provider_copy_count > 0,
        "real sample has no marked non-canonical provider copy"
    );
    persist_migration_preflight(&isolated_data_root, &preflight).unwrap();
    let backup_sources =
        migration_backup_sources_for_preflight(&isolated_home, &isolated_data_root, &preflight)
            .unwrap();
    let backup =
        create_migration_backup(&backup_destination, operation_id, &backup_sources).unwrap();
    migration_store
        .transition(operation_id, SessionStorageOperationPhase::Backup)
        .unwrap();
    migration_store
        .update(operation_id, |ledger| {
            ledger.backup_root = Some(backup.backup_dir.clone());
            Ok(())
        })
        .unwrap();
    let verifier = NativeCodexBackupVerifier::discover().unwrap();
    let backup = verify_migration_backup_with_runtime(
        &backup.backup_dir,
        &fixture.path().join("backup-runtime-restore"),
        &verifier,
    )
    .unwrap();
    migration_store
        .transition(operation_id, SessionStorageOperationPhase::BackupVerified)
        .unwrap();
    let prepared =
        prepare_migration_apply_plan(&isolated_home, &isolated_data_root, &preflight, &backup)
            .unwrap();
    migration_store
        .update(operation_id, |ledger| {
            ledger.created_files = prepared.created_files.clone();
            ledger.rollback_steps = prepared.rollback_steps.clone();
            Ok(())
        })
        .unwrap();
    migration_store
        .transition(operation_id, SessionStorageOperationPhase::PlanReady)
        .unwrap();
    migration_store
        .transition(operation_id, SessionStorageOperationPhase::Applying)
        .unwrap();
    apply_prepared_migration(&prepared.plan, || Ok(())).unwrap();
    migration_store
        .transition(operation_id, SessionStorageOperationPhase::Validating)
        .unwrap();
    let applied = validate_applied_migration(&prepared.plan).unwrap();
    let goals_targets = prepared
        .plan
        .databases
        .iter()
        .filter(|database| {
            database
                .target_path
                .file_name()
                .is_some_and(|name| name == "goals_1.sqlite")
        })
        .map(|database| &database.target_path)
        .collect::<Vec<_>>();
    assert!(!goals_targets.is_empty());
    for target in goals_targets.iter().skip(1) {
        assert_ne!(
            regular_file_identity(goals_targets[0]),
            regular_file_identity(target),
            "each isolated runtime goals target must keep an independent publication identity"
        );
    }
    let goals = Connection::open(goals_targets[0]).unwrap();
    for table in ["thread_goals", "thread_goal_continuation_deferrals"] {
        let _: i64 = goals
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
    }
    drop(goals);
    let runtime = verify_applied_migration_with_runtime(&prepared.plan, &verifier).unwrap();
    assert_eq!(
        runtime.conflict_payload_count,
        runtime.conflict_proofs.len()
    );
    let runtime_binary_identity = runtime
        .runtime_binary_identity
        .as_ref()
        .expect("native runtime binary identity");
    let capability_conflict_proof = runtime
        .capability_conflict_proof
        .as_ref()
        .expect("native conflict capability proof");
    let conflict_proofs = runtime
        .conflict_proofs
        .iter()
        .map(|proof| {
            serde_json::json!({
                "threadIdSha256": proof.thread_id_sha256,
                "canonicalSha256": proof.canonical_sha256,
                "recycleSha256": proof.recycle_payload_sha256,
                "relation": proof.relation
            })
        })
        .collect::<Vec<_>>();
    prepare_canonical_storage_state(
        &isolated_data_root,
        &isolated_home,
        operation_id,
        &prepared.plan.inventory_fingerprint,
    )
    .unwrap();
    cleanup_migration_staging(&prepared.plan).unwrap();
    migration_store
        .transition(operation_id, SessionStorageOperationPhase::Committed)
        .unwrap();
    finalize_canonical_storage_state(&isolated_data_root, &isolated_home, operation_id).unwrap();

    assert!(planned_marked_provider_paths
        .iter()
        .all(|path| path.is_file() && has_valid_provider_marker(path)));
    set_automatic_cleanup_enabled(&isolated_data_root, false).unwrap();
    let disabled_gc = run_automatic_gc_safe_window_evidence_at(
        &isolated_home,
        &isolated_data_root,
        AutomaticGcSafeWindowObservation {
            baseline_scan_id: Some("scan-before-disabled"),
            observed_scan_id: Some("scan-after-disabled"),
            generation: 1,
            high_confidence_copy_count: planned_marked_provider_copy_count,
            shadow_scan_running: false,
            active_writer_count: 0,
        },
    )
    .unwrap();
    assert!(!disabled_gc.enabled);
    assert_eq!(disabled_gc.decision, "stop");
    assert!(disabled_gc.receipt.is_none());
    let candidates_present_while_disabled = planned_marked_provider_paths
        .iter()
        .all(|path| path.is_file() && has_valid_provider_marker(path));
    assert!(candidates_present_while_disabled);

    set_automatic_cleanup_enabled(&isolated_data_root, true).unwrap();
    let enabled_gc = run_automatic_gc_safe_window_evidence_at(
        &isolated_home,
        &isolated_data_root,
        AutomaticGcSafeWindowObservation {
            baseline_scan_id: Some("scan-before-enabled"),
            observed_scan_id: Some("scan-after-enabled"),
            generation: 2,
            high_confidence_copy_count: planned_marked_provider_copy_count,
            shadow_scan_running: false,
            active_writer_count: 0,
        },
    )
    .unwrap();
    assert!(enabled_gc.enabled);
    assert_eq!(enabled_gc.decision, "run");
    assert!(enabled_gc.writer_guard_observation_count > 0);
    let first_gc = enabled_gc.receipt.clone().expect("automatic GC receipt");
    assert!(first_gc.validated);
    assert!(first_gc.deleted_count > 0);
    assert_eq!(first_gc.candidate_count, planned_marked_provider_copy_count);
    assert_eq!(first_gc.deleted_count, planned_marked_provider_copy_count);
    let candidates_absent_before_repeat = planned_marked_provider_paths
        .iter()
        .all(|path| !path.exists() && !has_valid_provider_marker(path));
    assert!(candidates_absent_before_repeat);
    let repeated_gc = run_automatic_gc_safe_window_evidence_at(
        &isolated_home,
        &isolated_data_root,
        AutomaticGcSafeWindowObservation {
            baseline_scan_id: Some("scan-after-enabled"),
            observed_scan_id: Some("scan-after-repeat"),
            generation: 3,
            high_confidence_copy_count: 0,
            shadow_scan_running: false,
            active_writer_count: 0,
        },
    )
    .unwrap();
    assert_eq!(repeated_gc.decision, "stop");
    assert!(repeated_gc.receipt.is_none());
    let candidates_absent_after_repeat = planned_marked_provider_paths
        .iter()
        .all(|path| !path.exists() && !has_valid_provider_marker(path));
    assert!(candidates_absent_after_repeat);

    let source_after = snapshot_sources(&tracked_sources);
    assert_eq!(source_before, source_after);
    let sample_session_bytes = tracked_sources.iter().fold(0_u64, |total, path| {
        total.saturating_add(fs::metadata(path).unwrap().len())
    });
    let proof = serde_json::json!({
        "schemaVersion": 2,
        "sourceKind": "destructive-isolated-real-session-sample",
        "sourceDatabaseSnapshotCount": copied_databases.len(),
        "sampleLogicalGroupCount": selected.len(),
        "sampleSessionFileCount": tracked_sources.len(),
        "sampleSessionBytes": sample_session_bytes,
        "sampleValidMarkerCount": selected_valid_marker_count,
        "availableToolGroupCount": available_tool_group_count,
        "sampleToolGroupCount": selected_tool_group_count,
        "availableSubagentGroupCount": available_subagent_group_count,
        "sampleSubagentGroupCount": selected_subagent_group_count,
        "plannedMarkedProviderCopyCount": planned_marked_provider_copy_count,
        "prunedDatabaseReferenceCount": pruned_database_reference_count,
        "sourceFilesUnchanged": source_before == source_after,
        "preflight": {
            "canonicalSessionCount": preflight.canonical_session_count,
            "sessionFileCount": preflight.session_file_count,
            "providerCopyCount": preflight.provider_copy_count,
            "conflictCount": preflight.conflict_count,
            "anomalyCount": preflight.anomaly_count,
            "readyForBackup": preflight.ready_for_backup
        },
        "backup": {
            "entryCount": backup.entries.len(),
            "runtimeVerified": backup.runtime_verification.is_some()
        },
        "migration": {
            "canonicalCreatedCount": applied.canonical_created_count,
            "canonicalReplacedCount": applied.canonical_replaced_count,
            "databaseViewCount": applied.database_view_count,
            "conflictCount": applied.conflict_count,
            "validated": applied.validated,
            "runtimeExpectedSessionCount": runtime.expected_session_count,
            "runtimeListedSessionCount": runtime.listed_session_count,
            "runtimeResumedSessionCount": runtime.resumed_session_count,
            "runtimeContinuedSessionCount": runtime.continued_session_count,
            "runtimeAvailableCategories": &runtime.available_categories,
            "runtimeContinuedCategories": &runtime.continued_categories,
            "toolSessionCount": runtime.tool_session_count,
            "toolRoundTripVerified": runtime.tool_round_trip_verified,
            "conflictPayloadCount": runtime.conflict_payload_count,
            "conflictPayloadsVerified": runtime.conflict_payloads_verified,
            "conflictProofs": conflict_proofs,
            "capabilityConflictProof": capability_conflict_proof,
            "runtimeBinaryIdentity": runtime_binary_identity
        },
        "offlineGc": {
            "candidateCount": first_gc.candidate_count,
            "deletedCount": first_gc.deleted_count,
            "reclaimedBytes": first_gc.reclaimed_bytes,
            "validated": first_gc.validated,
            "disabledSafeWindow": disabled_gc,
            "enabledSafeWindow": enabled_gc,
            "repeatSafeWindow": repeated_gc,
            "disabledPreservedCandidateBodies": candidates_present_while_disabled,
            "repeatDecisionStop": true,
            "repeatAdditionalDeletedCount": 0,
            "repeatAdditionalReclaimedBytes": 0,
            "candidatesAbsentBeforeRepeat": candidates_absent_before_repeat,
            "candidatesAbsentAfterRepeat": candidates_absent_after_repeat,
            "repeatValidated": true
        }
    });
    let encoded = serde_json::to_vec_pretty(&proof).unwrap();
    let encoded_text = String::from_utf8_lossy(&encoded);
    assert!(!encoded_text.contains(&source_home.to_string_lossy().to_string()));
    assert!(!encoded_text.contains(&source_data_root.to_string_lossy().to_string()));
    for source in &tracked_sources {
        assert!(!encoded_text.contains(&source.to_string_lossy().to_string()));
    }
    fs::write(&evidence_path, encoded).unwrap();
    fixture.close().unwrap();
}

#[cfg(feature = "runtime-evidence")]
fn snapshot_core_real_databases(
    source_home: &Path,
    source_data_root: &Path,
    isolated_home: &Path,
    isolated_data_root: &Path,
) -> Vec<(PathBuf, PathBuf)> {
    let current = resolve_user_codex_paths(source_home).unwrap();
    let sources = [
        (current.state_db, isolated_home.join("state_5.sqlite")),
        (
            source_data_root.join("shared-sessions/state_5.sqlite"),
            isolated_data_root.join("shared-sessions/state_5.sqlite"),
        ),
        (
            source_data_root.join("relay-sqlite/state_5.sqlite"),
            isolated_data_root.join("relay-sqlite/state_5.sqlite"),
        ),
        (current.goals_db, isolated_home.join("goals_1.sqlite")),
        (
            source_data_root.join("shared-sessions/goals_1.sqlite"),
            isolated_data_root.join("shared-sessions/goals_1.sqlite"),
        ),
        (
            source_data_root.join("relay-sqlite/goals_1.sqlite"),
            isolated_data_root.join("relay-sqlite/goals_1.sqlite"),
        ),
    ];
    let mut copied = Vec::new();
    for (source, target) in sources {
        if !source.is_file() || source.symlink_metadata().unwrap().file_type().is_symlink() {
            continue;
        }
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        let connection = Connection::open_with_flags(
            &source,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .unwrap();
        connection.backup(MAIN_DB, &target, None).unwrap();
        drop(connection);
        copied.push((source, target));
    }
    copied
}

#[derive(Debug, Clone)]
struct FixtureReference {
    thread_id: String,
    source_path: PathBuf,
    bytes: u64,
    has_tool_pair: bool,
    is_subagent: bool,
}

#[derive(Debug)]
struct DuplicateGroupCandidate {
    thread_id: String,
    bytes: u64,
    has_valid_marker: bool,
    has_tool_pair: bool,
    is_subagent: bool,
}

fn required_env_path(name: &str) -> PathBuf {
    env::var_os(name)
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("{name} must be set explicitly"))
}

fn snapshot_real_databases(
    source_home: &Path,
    source_data_root: &Path,
    isolated_home: &Path,
    isolated_data_root: &Path,
) -> Vec<(PathBuf, PathBuf)> {
    let mut sources = vec![
        source_home.join("state_5.sqlite"),
        source_home.join("goals_1.sqlite"),
    ];
    if source_data_root.is_dir() {
        sources.extend(
            WalkDir::new(source_data_root)
                .max_depth(8)
                .follow_links(false)
                .into_iter()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().is_file())
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .eq_ignore_ascii_case("state_5.sqlite")
                        || entry
                            .file_name()
                            .to_string_lossy()
                            .eq_ignore_ascii_case("goals_1.sqlite")
                })
                .map(|entry| entry.into_path()),
        );
    }
    sources.sort();
    sources.dedup();

    let mut copied = Vec::new();
    for source in sources {
        if !source.is_file() || source.symlink_metadata().unwrap().file_type().is_symlink() {
            continue;
        }
        let target = if source.starts_with(source_home) {
            isolated_home.join(source.file_name().unwrap())
        } else {
            let relative = source
                .strip_prefix(source_data_root)
                .expect("discovered data DB must remain below source data root");
            isolated_data_root.join(relative)
        };
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        let connection = Connection::open_with_flags(
            &source,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .unwrap();
        connection.backup(MAIN_DB, &target, None).unwrap();
        drop(connection);
        copied.push((source, target));
    }
    copied
}

fn read_fixture_references(databases: &[(PathBuf, PathBuf)]) -> Vec<FixtureReference> {
    let mut raw_references = Vec::<(String, PathBuf, Option<String>)>::new();
    for (_, database) in databases {
        if database.file_name().and_then(|name| name.to_str()) != Some("state_5.sqlite") {
            continue;
        }
        let connection = Connection::open(database).unwrap();
        let has_source = connection
            .prepare("SELECT source FROM threads LIMIT 0")
            .is_ok();
        let select = if has_source {
            "SELECT id, rollout_path, CAST(source AS TEXT) FROM threads WHERE rollout_path IS NOT NULL"
        } else {
            "SELECT id, rollout_path, NULL FROM threads WHERE rollout_path IS NOT NULL"
        };
        let Ok(mut statement) = connection.prepare(select) else {
            continue;
        };
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })
            .unwrap();
        for row in rows.filter_map(Result::ok) {
            raw_references.push((row.0, PathBuf::from(row.1), row.2));
        }
    }

    // The real state databases can contain thousands of unrelated sessions.
    // This destructive probe only needs cross-view duplicate groups, so first
    // select duplicate identities from SQLite metadata and parse only their
    // JSONL bodies. This keeps the real source read-only probe bounded without
    // weakening any candidate's semantic/stability checks.
    let mut paths_by_thread = BTreeMap::<String, BTreeSet<PathBuf>>::new();
    for (thread_id, source_path, _) in &raw_references {
        paths_by_thread
            .entry(thread_id.clone())
            .or_default()
            .insert(source_path.clone());
    }
    let mut duplicate_hints = paths_by_thread
        .into_iter()
        .filter(|(_, paths)| paths.len() > 1)
        .filter_map(|(thread_id, paths)| {
            let mut bytes = 0_u64;
            for path in &paths {
                if !stable_real_session(path) {
                    return None;
                }
                bytes = bytes.checked_add(path.metadata().ok()?.len())?;
            }
            let marker_hint = paths.iter().any(|path| provider_marker_hint(path));
            let is_subagent = raw_references.iter().any(|(candidate, _, source)| {
                candidate == &thread_id
                    && source
                        .as_deref()
                        .is_some_and(|value| value.to_ascii_lowercase().contains("subagent"))
            });
            Some((thread_id, bytes, marker_hint, is_subagent))
        })
        .collect::<Vec<_>>();
    duplicate_hints.sort_by(|left, right| {
        (!left.2, !left.3, left.1, &left.0).cmp(&(!right.2, !right.3, right.1, &right.0))
    });
    let mut duplicate_threads = BTreeSet::new();
    let mut selected_bytes = 0_u64;
    for (thread_id, bytes, _, _) in duplicate_hints {
        if duplicate_threads.len() >= 12 || bytes > 32 * 1024 * 1024 {
            continue;
        }
        let Some(next_bytes) = selected_bytes.checked_add(bytes) else {
            continue;
        };
        if next_bytes > 48 * 1024 * 1024 {
            continue;
        }
        duplicate_threads.insert(thread_id);
        selected_bytes = next_bytes;
    }
    let mut references = Vec::new();
    let mut semantic_by_path = BTreeMap::<PathBuf, Option<(String, u64, bool)>>::new();
    for (thread_id, source_path, source_kind) in raw_references {
        if !duplicate_threads.contains(&thread_id) {
            continue;
        }
        let Some((semantic_thread_id, bytes, has_tool_pair)) = semantic_by_path
            .entry(source_path.clone())
            .or_insert_with(|| {
                if !stable_real_session(&source_path) {
                    return None;
                }
                let semantic = read_semantic_session(&source_path).ok()?;
                Some((
                    semantic.thread_id,
                    semantic.bytes,
                    semantic.tool_call_count > 0
                        && semantic.tool_call_count == semantic.tool_result_count,
                ))
            })
            .clone()
        else {
            continue;
        };
        if semantic_thread_id != thread_id {
            continue;
        }
        references.push(FixtureReference {
            thread_id,
            source_path,
            bytes,
            has_tool_pair,
            is_subagent: source_kind
                .as_deref()
                .is_some_and(|source| source.to_ascii_lowercase().contains("subagent")),
        });
    }
    references
}

fn stable_real_session(path: &Path) -> bool {
    let Ok(metadata) = path.symlink_metadata() else {
        return false;
    };
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() == 0
        || metadata.len() > 64 * 1024 * 1024
        || path.extension().and_then(|extension| extension.to_str()) != Some("jsonl")
    {
        return false;
    }
    metadata
        .modified()
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age >= Duration::from_secs(30 * 60))
}

fn provider_marker_hint(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| {
            path.with_file_name(format!(".{name}.codex-switch-slot-v1.json"))
                .is_file()
        })
        .unwrap_or(false)
}

fn select_stable_duplicate_groups(
    references: &[FixtureReference],
    max_groups: usize,
    max_bytes: u64,
) -> BTreeSet<String> {
    let mut by_thread = BTreeMap::<String, BTreeSet<PathBuf>>::new();
    for reference in references {
        by_thread
            .entry(reference.thread_id.clone())
            .or_default()
            .insert(reference.source_path.clone());
    }
    let mut candidates = by_thread
        .into_iter()
        .filter(|(_, paths)| paths.len() > 1)
        .map(|(thread_id, paths)| {
            let has_valid_marker = paths.iter().any(|path| has_valid_provider_marker(path));
            let bytes = paths.iter().fold(0_u64, |total, path| {
                total.saturating_add(
                    references
                        .iter()
                        .find(|reference| &reference.source_path == path)
                        .map(|reference| reference.bytes)
                        .unwrap_or(0),
                )
            });
            let has_tool_pair = references
                .iter()
                .any(|reference| reference.thread_id == thread_id && reference.has_tool_pair);
            let is_subagent = references
                .iter()
                .any(|reference| reference.thread_id == thread_id && reference.is_subagent);
            DuplicateGroupCandidate {
                thread_id,
                bytes,
                has_valid_marker,
                has_tool_pair,
                is_subagent,
            }
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        (!left.has_valid_marker, left.bytes, &left.thread_id).cmp(&(
            !right.has_valid_marker,
            right.bytes,
            &right.thread_id,
        ))
    });

    let mut selected = BTreeSet::new();
    let mut selected_bytes = 0_u64;
    if let Some(candidate) = candidates
        .iter()
        .filter(|candidate| candidate.has_valid_marker && candidate.has_tool_pair)
        .min_by_key(|candidate| (candidate.bytes, &candidate.thread_id))
    {
        select_duplicate_group(
            candidate,
            &mut selected,
            &mut selected_bytes,
            max_groups,
            max_bytes,
        );
    }
    if let Some(candidate) = candidates
        .iter()
        .filter(|candidate| candidate.has_valid_marker && candidate.is_subagent)
        .min_by_key(|candidate| (candidate.bytes, &candidate.thread_id))
    {
        select_duplicate_group(
            candidate,
            &mut selected,
            &mut selected_bytes,
            max_groups,
            max_bytes,
        );
    }
    if let Some(candidate) = candidates
        .iter()
        .filter(|candidate| candidate.has_valid_marker)
        .min_by_key(|candidate| (candidate.bytes, &candidate.thread_id))
    {
        select_duplicate_group(
            candidate,
            &mut selected,
            &mut selected_bytes,
            max_groups,
            max_bytes,
        );
    }
    if let Some(candidate) = candidates
        .iter()
        .filter(|candidate| candidate.has_tool_pair)
        .min_by_key(|candidate| {
            (
                !candidate.has_valid_marker,
                candidate.bytes,
                &candidate.thread_id,
            )
        })
    {
        select_duplicate_group(
            candidate,
            &mut selected,
            &mut selected_bytes,
            max_groups,
            max_bytes,
        );
    }
    if let Some(candidate) = candidates
        .iter()
        .filter(|candidate| candidate.is_subagent)
        .min_by_key(|candidate| {
            (
                !candidate.has_valid_marker,
                candidate.bytes,
                &candidate.thread_id,
            )
        })
    {
        select_duplicate_group(
            candidate,
            &mut selected,
            &mut selected_bytes,
            max_groups,
            max_bytes,
        );
    }
    if let Some(candidate) = candidates.iter().max_by_key(|candidate| candidate.bytes) {
        select_duplicate_group(
            candidate,
            &mut selected,
            &mut selected_bytes,
            max_groups,
            max_bytes,
        );
    }
    for candidate in &candidates {
        select_duplicate_group(
            candidate,
            &mut selected,
            &mut selected_bytes,
            max_groups,
            max_bytes,
        );
    }
    selected
}

fn select_duplicate_group(
    candidate: &DuplicateGroupCandidate,
    selected: &mut BTreeSet<String>,
    selected_bytes: &mut u64,
    max_groups: usize,
    max_bytes: u64,
) {
    if selected.len() >= max_groups
        || selected.contains(&candidate.thread_id)
        || selected_bytes.saturating_add(candidate.bytes) > max_bytes
    {
        return;
    }
    *selected_bytes = selected_bytes.saturating_add(candidate.bytes);
    selected.insert(candidate.thread_id.clone());
}

fn has_valid_provider_marker(path: &Path) -> bool {
    read_semantic_session(path).ok().is_some_and(|semantic| {
        inspect_provider_marker(path, Some(&semantic)) == MarkerStatus::Valid
    })
}

fn copy_selected_sessions(
    selected: &BTreeSet<String>,
    references: &[FixtureReference],
    source_home: &Path,
    source_data_root: &Path,
    isolated_home: &Path,
    isolated_data_root: &Path,
) -> BTreeMap<PathBuf, PathBuf> {
    let mut copied = BTreeMap::new();
    for reference in references {
        if !selected.contains(&reference.thread_id) || copied.contains_key(&reference.source_path) {
            continue;
        }
        let digest = format!(
            "{:x}",
            Sha256::digest(reference.source_path.to_string_lossy().as_bytes())
        );
        let file_name = reference
            .source_path
            .file_name()
            .expect("session file name");
        let target = if reference.source_path.starts_with(source_home) {
            isolated_home.join(
                reference
                    .source_path
                    .strip_prefix(source_home)
                    .expect("canonical sample path"),
            )
        } else if reference
            .source_path
            .starts_with(source_data_root.join("shared-sessions"))
        {
            isolated_data_root.join("shared-sessions").join(
                reference
                    .source_path
                    .strip_prefix(source_data_root.join("shared-sessions"))
                    .expect("shared sample path"),
            )
        } else {
            isolated_data_root
                .join("referenced-real-sample")
                .join(&digest[..16])
                .join(file_name)
        };
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::copy(&reference.source_path, &target).unwrap();
        copy_provider_marker_if_present(&reference.source_path, &target);
        copied.insert(reference.source_path.clone(), target);
    }
    copied
}

fn copy_provider_marker_if_present(source: &Path, target: &Path) {
    let Some(file_name) = source.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    let source_marker = source.with_file_name(format!(".{file_name}.codex-switch-slot-v1.json"));
    let target_marker = target.with_file_name(format!(".{file_name}.codex-switch-slot-v1.json"));
    let Ok(metadata) = source_marker.symlink_metadata() else {
        return;
    };
    if metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() <= 16 * 1024 {
        fs::copy(source_marker, target_marker).unwrap();
    }
}

fn rewrite_fixture_references(
    databases: &[(PathBuf, PathBuf)],
    copied_sessions: &BTreeMap<PathBuf, PathBuf>,
) -> usize {
    let mut deleted = 0_usize;
    for (_, database) in databases {
        let mut connection = Connection::open(database).unwrap();
        let rows = {
            let Ok(mut statement) = connection
                .prepare("SELECT id, rollout_path FROM threads WHERE rollout_path IS NOT NULL")
            else {
                continue;
            };
            statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .unwrap()
                .filter_map(Result::ok)
                .collect::<Vec<_>>()
        };
        let has_spawn_edges = connection
            .prepare("SELECT parent_thread_id, child_thread_id FROM thread_spawn_edges LIMIT 0")
            .is_ok();
        let transaction = connection.transaction().unwrap();
        for (thread_id, source_path) in rows {
            if let Some(target) = copied_sessions.get(&PathBuf::from(source_path)) {
                transaction
                    .execute(
                        "UPDATE threads SET rollout_path = ?1 WHERE id = ?2",
                        params![target.to_string_lossy(), thread_id],
                    )
                    .unwrap();
            } else {
                deleted = deleted.saturating_add(
                    transaction
                        .execute("DELETE FROM threads WHERE id = ?1", params![thread_id])
                        .unwrap(),
                );
            }
        }
        if has_spawn_edges {
            transaction
                .execute(
                    "DELETE FROM thread_spawn_edges
                     WHERE parent_thread_id NOT IN (SELECT id FROM threads)
                        OR child_thread_id NOT IN (SELECT id FROM threads)",
                    [],
                )
                .unwrap();
        }
        // The sampled real database can contain subagent rows whose parent
        // lives outside the bounded fixture. Once those orphan edges are
        // removed, keep the remaining rows as ordinary CLI records; the
        // native capability fixture below supplies the independent subagent
        // category without leaving an invalid parent reference in the DB.
        transaction
            .execute(
                "UPDATE threads SET source = 'cli'
                 WHERE lower(COALESCE(source, '')) LIKE '%subagent%'",
                [],
            )
            .unwrap();
        transaction.commit().unwrap();
        let quick_check: String = connection
            .query_row("PRAGMA quick_check", [], |row| row.get(0))
            .unwrap();
        assert_eq!(quick_check, "ok");
    }
    deleted
}

#[cfg(feature = "runtime-evidence")]
fn assert_fixture_references_are_isolated(databases: &[(PathBuf, PathBuf)], fixture_root: &Path) {
    for (_, database) in databases {
        let connection = Connection::open(database).unwrap();
        let Ok(mut statement) =
            connection.prepare("SELECT rollout_path FROM threads WHERE rollout_path IS NOT NULL")
        else {
            continue;
        };
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap();
        for rollout_path in rows.filter_map(Result::ok) {
            assert!(PathBuf::from(rollout_path).starts_with(fixture_root));
        }
    }
}

#[cfg(feature = "runtime-evidence")]
fn normalize_fixture_database_journals(databases: &[(PathBuf, PathBuf)]) {
    for (_, database) in databases {
        let connection = Connection::open(database).unwrap();
        connection
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .unwrap();
        let journal_mode: String = connection
            .query_row("PRAGMA journal_mode=DELETE", [], |row| row.get(0))
            .unwrap();
        assert!(journal_mode.eq_ignore_ascii_case("delete"));
        drop(connection);
        for suffix in ["-wal", "-shm", "-journal"] {
            assert!(
                !PathBuf::from(format!("{}{}", database.to_string_lossy(), suffix)).exists(),
                "closed isolated fixture must not retain SQLite sidecars"
            );
        }
    }
}

fn write_session(
    root: &Path,
    name: &str,
    thread_id: &str,
    provider: &str,
    messages: &[&str],
) -> PathBuf {
    let path = root.join("sessions/2026/08/11").join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut lines = vec![serde_json::json!({
        "type": "session_meta",
        "payload": {"id": thread_id, "model_provider": provider}
    })
    .to_string()];
    lines.extend(messages.iter().map(|message| {
        serde_json::json!({
            "type": "event_msg",
            "payload": {"type": "user_message", "message": message}
        })
        .to_string()
    }));
    fs::write(&path, lines.join("\n") + "\n").unwrap();
    path
}

fn create_database(
    path: &Path,
    references: &[(&str, &PathBuf, &str)],
    keep_wal_uncheckpointed: bool,
) -> Connection {
    let connection = Connection::open(path).unwrap();
    if keep_wal_uncheckpointed {
        connection
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
            .unwrap();
    }
    connection
        .execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                rollout_path TEXT,
                model_provider TEXT
            );",
        )
        .unwrap();
    for (thread_id, rollout_path, provider) in references {
        connection
            .execute(
                "INSERT INTO threads (id, rollout_path, model_provider) VALUES (?1, ?2, ?3)",
                params![thread_id, rollout_path.to_string_lossy(), provider],
            )
            .unwrap();
    }
    connection
}

fn create_goals_database(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    Connection::open(path)
        .unwrap()
        .execute_batch(
            "CREATE TABLE thread_goals (
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
             CREATE TABLE thread_goal_continuation_deferrals (
                thread_id TEXT PRIMARY KEY NOT NULL REFERENCES thread_goals(thread_id) ON DELETE CASCADE
             );",
        )
        .unwrap();
}

fn snapshot_sources(paths: &[PathBuf]) -> Vec<SourceSnapshot> {
    paths
        .iter()
        .map(|path| {
            let metadata = fs::metadata(path).unwrap();
            SourceSnapshot {
                path: path.clone(),
                bytes: metadata.len(),
                sha256: Sha256::digest(fs::read(path).unwrap()).into(),
                modified: metadata.modified().unwrap(),
            }
        })
        .collect()
}

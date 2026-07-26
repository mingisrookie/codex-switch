use std::{
    fs,
    path::{Path, PathBuf},
    time::{Instant, SystemTime},
};

use codex_switch_lib::{
    file_ops::walk_jsonl_files,
    session_sync::{sync_shared_to_user_home, SessionSyncResult},
};
use rusqlite::{params, Connection};
use serde_json::json;
use sha2::{Digest, Sha256};
use tempfile::{tempdir, TempDir};

const CI_SESSION_COUNT: usize = 64;
const CI_SESSION_BYTES: usize = 64 * 1024;
const CURRENT_PROVIDER: &str = "provider-a";
const SHARED_PROVIDER: &str = "provider-b";
const TARGET_PROVIDER: &str = "provider-c";

#[derive(Debug, PartialEq, Eq)]
struct FileSnapshot {
    relative_path: PathBuf,
    bytes: u64,
    sha256: [u8; 32],
    modified: SystemTime,
}

struct SyntheticHomes {
    current: TempDir,
    shared: TempDir,
    payload_bytes: u64,
}

#[test]
fn synthetic_large_home_provider_sync_preserves_existing_jsonl() {
    let homes = create_synthetic_homes(CI_SESSION_COUNT, CI_SESSION_BYTES);
    let before = snapshot_sessions(homes.current.path());

    assert_eq!(before.len(), CI_SESSION_COUNT);
    assert_eq!(
        total_bytes(&before),
        (CI_SESSION_COUNT * CI_SESSION_BYTES) as u64
    );

    let result =
        sync_shared_to_user_home(homes.shared.path(), homes.current.path(), TARGET_PROVIDER)
            .unwrap();
    let after = snapshot_sessions(homes.current.path());

    assert_quiescent_result(&result);
    assert_snapshots_unchanged(&before, &after);
    assert_eq!(imported_file_count(&after), 0);
    assert_all_providers(homes.current.path(), TARGET_PROVIDER, CI_SESSION_COUNT);
}

#[test]
#[ignore = "synthetic performance probe; run explicitly when measuring session sync"]
fn benchmark_synthetic_large_home_provider_sync() {
    let session_count = benchmark_value("CODEX_SWITCH_SYNTHETIC_SESSION_COUNT", 256);
    let session_kib = benchmark_value("CODEX_SWITCH_SYNTHETIC_SESSION_KIB", 64);
    let bytes_per_file = session_kib
        .checked_mul(1024)
        .expect("CODEX_SWITCH_SYNTHETIC_SESSION_KIB is too large");
    let homes = create_synthetic_homes(session_count, bytes_per_file);
    let before = snapshot_sessions(homes.current.path());

    let started = Instant::now();
    let result =
        sync_shared_to_user_home(homes.shared.path(), homes.current.path(), TARGET_PROVIDER)
            .unwrap();
    let elapsed = started.elapsed();
    let after = snapshot_sessions(homes.current.path());
    let mtime_changed = changed_mtime_count(&before, &after);

    println!(
        "synthetic-large-home elapsedMs={} bytes={} mtimeChanged={} copiedFiles={}",
        elapsed.as_millis(),
        homes.payload_bytes,
        mtime_changed,
        result.copied_session_files
    );

    assert_quiescent_result(&result);
    assert_snapshots_unchanged(&before, &after);
    assert_eq!(imported_file_count(&after), 0);
    assert_all_providers(homes.current.path(), TARGET_PROVIDER, session_count);
}

fn create_synthetic_homes(session_count: usize, bytes_per_file: usize) -> SyntheticHomes {
    assert!(session_count > 0, "session count must be greater than zero");
    let current = tempdir().unwrap();
    let shared = tempdir().unwrap();
    pin_sqlite_to_home(current.path());

    let mut current_rows = Vec::with_capacity(session_count);
    let mut shared_rows = Vec::with_capacity(session_count);
    for index in 0..session_count {
        let id = format!("synthetic-thread-{index:04}");
        let relative = PathBuf::from(format!(
            "sessions/2026/07/26/rollout-synthetic-{index:04}.jsonl"
        ));
        let current_path = current.path().join(&relative);
        let shared_path = shared.path().join(&relative);
        fs::create_dir_all(current_path.parent().unwrap()).unwrap();
        fs::create_dir_all(shared_path.parent().unwrap()).unwrap();
        fs::write(
            &current_path,
            session_payload(&id, CURRENT_PROVIDER, bytes_per_file),
        )
        .unwrap();
        fs::write(
            &shared_path,
            session_payload(&id, SHARED_PROVIDER, bytes_per_file),
        )
        .unwrap();
        current_rows.push((id.clone(), current_path));
        shared_rows.push((id, shared_path));
    }

    create_threads_db(current.path(), &current_rows, CURRENT_PROVIDER);
    create_threads_db(shared.path(), &shared_rows, SHARED_PROVIDER);

    let payload_bytes = u64::try_from(session_count)
        .unwrap()
        .checked_mul(u64::try_from(bytes_per_file).unwrap())
        .and_then(|bytes| bytes.checked_mul(2))
        .expect("synthetic payload size overflowed");
    SyntheticHomes {
        current,
        shared,
        payload_bytes,
    }
}

fn pin_sqlite_to_home(home: &Path) {
    let escaped = home
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    fs::write(
        home.join("config.toml"),
        format!("sqlite_home = \"{escaped}\"\n"),
    )
    .unwrap();
}

fn session_payload(id: &str, provider: &str, target_bytes: usize) -> Vec<u8> {
    let metadata = serde_json::to_vec(&json!({
        "type": "session_meta",
        "payload": {
            "id": id,
            "model_provider": provider,
            "source": "cli"
        }
    }))
    .unwrap();
    let response_prefix = br#"{"type":"response_item","payload":{"text":""#;
    let response_suffix = br#""}}"#;
    let fixed_bytes = metadata.len() + 1 + response_prefix.len() + response_suffix.len() + 1;
    assert!(
        target_bytes >= fixed_bytes,
        "synthetic JSONL target must be at least {fixed_bytes} bytes"
    );

    let mut payload = Vec::with_capacity(target_bytes);
    payload.extend_from_slice(&metadata);
    payload.push(b'\n');
    payload.extend_from_slice(response_prefix);
    payload.extend(std::iter::repeat_n(b'x', target_bytes - fixed_bytes));
    payload.extend_from_slice(response_suffix);
    payload.push(b'\n');
    assert_eq!(payload.len(), target_bytes);
    payload
}

fn create_threads_db(home: &Path, rows: &[(String, PathBuf)], provider: &str) {
    let mut conn = Connection::open(home.join("state_5.sqlite")).unwrap();
    conn.execute(
        "CREATE TABLE threads (
            id TEXT PRIMARY KEY,
            rollout_path TEXT NOT NULL,
            updated_at INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL,
            model_provider TEXT NOT NULL
        )",
        [],
    )
    .unwrap();
    let transaction = conn.transaction().unwrap();
    {
        let mut insert = transaction
            .prepare(
                "INSERT INTO threads (
                    id, rollout_path, updated_at, updated_at_ms, model_provider
                ) VALUES (?1, ?2, 1, 1000, ?3)",
            )
            .unwrap();
        for (id, rollout_path) in rows {
            insert
                .execute(params![id, rollout_path.to_string_lossy(), provider])
                .unwrap();
        }
    }
    transaction.commit().unwrap();
}

fn snapshot_sessions(home: &Path) -> Vec<FileSnapshot> {
    walk_jsonl_files(&home.join("sessions"))
        .unwrap()
        .into_iter()
        .map(|path| {
            let metadata = fs::metadata(&path).unwrap();
            let contents = fs::read(&path).unwrap();
            FileSnapshot {
                relative_path: path.strip_prefix(home).unwrap().to_path_buf(),
                bytes: metadata.len(),
                sha256: Sha256::digest(contents).into(),
                modified: metadata.modified().unwrap(),
            }
        })
        .collect()
}

fn assert_quiescent_result(result: &SessionSyncResult) {
    assert_eq!(result.inserted_threads, 0);
    assert_eq!(result.copied_session_files, 0);
}

fn assert_snapshots_unchanged(before: &[FileSnapshot], after: &[FileSnapshot]) {
    assert_eq!(
        after.len(),
        before.len(),
        "current JSONL file count changed"
    );
    assert_eq!(
        total_bytes(after),
        total_bytes(before),
        "current JSONL byte count changed"
    );
    for (before_file, after_file) in before.iter().zip(after) {
        assert_eq!(
            after_file.relative_path, before_file.relative_path,
            "current JSONL path set changed"
        );
        assert_eq!(
            after_file.bytes,
            before_file.bytes,
            "{} byte count changed",
            before_file.relative_path.display()
        );
        assert_eq!(
            after_file.sha256,
            before_file.sha256,
            "{} content hash changed",
            before_file.relative_path.display()
        );
        assert_eq!(
            after_file.modified,
            before_file.modified,
            "{} mtime changed",
            before_file.relative_path.display()
        );
    }
}

fn total_bytes(snapshots: &[FileSnapshot]) -> u64 {
    snapshots.iter().map(|snapshot| snapshot.bytes).sum()
}

fn imported_file_count(snapshots: &[FileSnapshot]) -> usize {
    snapshots
        .iter()
        .filter(|snapshot| {
            snapshot
                .relative_path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().contains("-imported-"))
        })
        .count()
}

fn changed_mtime_count(before: &[FileSnapshot], after: &[FileSnapshot]) -> usize {
    let compared = before
        .iter()
        .zip(after)
        .filter(|(before_file, after_file)| {
            before_file.relative_path != after_file.relative_path
                || before_file.modified != after_file.modified
        })
        .count();
    compared + before.len().abs_diff(after.len())
}

fn assert_all_providers(home: &Path, expected: &str, expected_count: usize) {
    let conn = Connection::open(home.join("state_5.sqlite")).unwrap();
    let total: usize = conn
        .query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0))
        .unwrap();
    let matching: usize = conn
        .query_row(
            "SELECT COUNT(*) FROM threads WHERE model_provider = ?1",
            [expected],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(total, expected_count);
    assert_eq!(matching, expected_count);
}

fn benchmark_value(name: &str, default: usize) -> usize {
    match std::env::var(name) {
        Ok(raw) => {
            let value = raw
                .parse::<usize>()
                .unwrap_or_else(|_| panic!("{name} must be a positive integer"));
            assert!(value > 0, "{name} must be greater than zero");
            value
        }
        Err(_) => default,
    }
}

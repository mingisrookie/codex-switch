use std::{
    collections::HashSet,
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, OpenFlags, MAIN_DB};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    codex_paths::{local_codex_paths, resolve_user_codex_paths, CodexPaths},
    crypto::{protect, unprotect},
    file_ops::{atomic_write, walk_jsonl_files},
};

static BACKUP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BackupManifest {
    pub version: u32,
    pub reason: String,
    pub created_at_ms: u128,
    pub source_root: PathBuf,
    pub root_existed: bool,
    #[serde(default)]
    pub state_db_is_local: bool,
    pub complete_sessions: bool,
    pub backup_dir: PathBuf,
    pub files: Vec<BackupFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BackupFile {
    pub source: PathBuf,
    pub relative_path: PathBuf,
    pub backup_path: PathBuf,
    pub bytes: u64,
    pub sha256: String,
    pub encrypted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RestoreResult {
    pub backup_dir: PathBuf,
    pub target_root: PathBuf,
    pub restored_files: usize,
    pub verified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BackupSummary {
    pub backup_dir: PathBuf,
    pub source_root: PathBuf,
    pub reason: String,
    pub created_at_ms: u128,
    pub file_count: usize,
    pub total_bytes: u64,
    pub verified: bool,
    pub complete_sessions: bool,
}

pub fn create_backup(
    home: &Path,
    destination_root: &Path,
    reason: &str,
) -> Result<BackupManifest, String> {
    let paths = resolve_user_codex_paths(home)?;
    create_backup_with_paths(home, destination_root, reason, paths)
}

pub fn create_local_backup(
    home: &Path,
    destination_root: &Path,
    reason: &str,
) -> Result<BackupManifest, String> {
    create_backup_with_paths(home, destination_root, reason, local_codex_paths(home))
}

fn create_backup_with_paths(
    home: &Path,
    destination_root: &Path,
    reason: &str,
    paths: CodexPaths,
) -> Result<BackupManifest, String> {
    let created_at_ms = timestamp_millis()?;
    let backup_dir = destination_root.join(format!(
        "{}-{}-{}-{}",
        created_at_ms,
        std::process::id(),
        BACKUP_COUNTER.fetch_add(1, Ordering::Relaxed),
        safe_reason(reason)
    ));
    fs::create_dir_all(&backup_dir)
        .map_err(|error| format!("failed to create backup dir: {error}"))?;

    let result = create_backup_in_dir(home, &backup_dir, reason, created_at_ms, paths);
    if result.is_err() {
        let _ = fs::remove_dir_all(&backup_dir);
    }
    result
}

fn create_backup_in_dir(
    home: &Path,
    backup_dir: &Path,
    reason: &str,
    created_at_ms: u128,
    paths: CodexPaths,
) -> Result<BackupManifest, String> {
    let root_existed = home.exists();
    let mut files = Vec::new();

    for (source, relative) in [
        (home.join("auth.json"), PathBuf::from("auth.json")),
        (home.join("config.toml"), PathBuf::from("config.toml")),
        (
            paths.session_index.clone(),
            PathBuf::from("session_index.jsonl"),
        ),
    ] {
        if source.is_file() {
            files.push(encrypt_payload(&source, backup_dir, &relative)?);
        }
    }

    if paths.state_db.is_file() {
        files.push(snapshot_sqlite(&paths.state_db, backup_dir)?);
    }

    if paths.sessions_dir.is_dir() {
        for path in walk_jsonl_files(&paths.sessions_dir)? {
            let relative = path
                .strip_prefix(home)
                .map_err(|error| format!("failed to map session backup path: {error}"))?
                .to_path_buf();
            files.push(encrypt_payload(&path, backup_dir, &relative)?);
        }
    }
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));

    let manifest = BackupManifest {
        version: 2,
        reason: reason.to_string(),
        created_at_ms,
        source_root: home.to_path_buf(),
        root_existed,
        state_db_is_local: paths.state_db == home.join("state_5.sqlite"),
        complete_sessions: true,
        backup_dir: backup_dir.to_path_buf(),
        files,
    };
    let encoded = serde_json::to_vec_pretty(&manifest)
        .map_err(|error| format!("failed to serialize backup manifest: {error}"))?;
    atomic_write(&backup_dir.join("manifest.json"), &encoded)?;
    verify_backup(backup_dir)
}

pub fn verify_backup(backup_dir: &Path) -> Result<BackupManifest, String> {
    let manifest = read_backup_manifest(backup_dir)?;
    let canonical_root = fs::canonicalize(backup_dir)
        .map_err(|error| format!("failed to resolve backup directory: {error}"))?;
    for file in &manifest.files {
        validate_relative_path(&file.relative_path)?;
        let canonical_payload = fs::canonicalize(&file.backup_path)
            .map_err(|error| format!("backup payload is missing: {error}"))?;
        if !canonical_payload.starts_with(&canonical_root) {
            return Err("backup payload escaped the backup directory".to_string());
        }
        let metadata = fs::metadata(&canonical_payload)
            .map_err(|error| format!("failed to inspect backup payload: {error}"))?;
        if metadata.len() != file.bytes {
            return Err(format!(
                "backup payload size mismatch: {}",
                file.relative_path.display()
            ));
        }
        if sha256_file(&canonical_payload)? != file.sha256 {
            return Err(format!(
                "backup payload checksum mismatch: {}",
                file.relative_path.display()
            ));
        }
        if !file.encrypted {
            return Err("unencrypted payloads are not restorable".to_string());
        }
    }
    Ok(manifest)
}

fn read_backup_manifest(backup_dir: &Path) -> Result<BackupManifest, String> {
    let manifest_path = backup_dir.join("manifest.json");
    let raw = fs::read(&manifest_path)
        .map_err(|error| format!("failed to read backup manifest: {error}"))?;
    let manifest: BackupManifest = serde_json::from_slice(&raw)
        .map_err(|error| format!("failed to parse backup manifest: {error}"))?;
    if manifest.version != 2 {
        return Err(format!(
            "unsupported backup manifest version: {}",
            manifest.version
        ));
    }
    Ok(manifest)
}

pub fn restore_backup(backup_dir: &Path, target_home: &Path) -> Result<RestoreResult, String> {
    let manifest = verify_backup(backup_dir)?;
    if !manifest.root_existed {
        clear_known_codex_state(target_home, manifest.state_db_is_local)?;
        return Ok(RestoreResult {
            backup_dir: backup_dir.to_path_buf(),
            target_root: target_home.to_path_buf(),
            restored_files: 0,
            verified: true,
        });
    }

    fs::create_dir_all(target_home)
        .map_err(|error| format!("failed to create restore target: {error}"))?;
    let old_paths = if manifest.state_db_is_local {
        local_codex_paths(target_home)
    } else {
        resolve_user_codex_paths(target_home)?
    };
    remove_absent_core_files(&manifest, target_home, &old_paths)?;
    if manifest.complete_sessions {
        remove_extra_session_files(&manifest, target_home)?;
    }

    let mut restored_files = 0;
    if let Some(config) = manifest
        .files
        .iter()
        .find(|file| file.relative_path == Path::new("config.toml"))
    {
        restore_file(config, &target_home.join("config.toml"))?;
        restored_files += 1;
    }

    let paths = if manifest.state_db_is_local {
        local_codex_paths(target_home)
    } else {
        resolve_user_codex_paths(target_home)?
    };
    if old_paths.state_db != paths.state_db {
        remove_sqlite_files(&old_paths.state_db)?;
    }
    for file in &manifest.files {
        if file.relative_path == Path::new("config.toml") {
            continue;
        }
        let target = restore_target(&paths, target_home, &file.relative_path)?;
        restore_file(file, &target)?;
        restored_files += 1;
    }
    remove_sqlite_sidecars(&paths.state_db)?;
    if paths.state_db.exists() {
        let conn = Connection::open_with_flags(&paths.state_db, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|error| format!("restored state_5.sqlite cannot be opened: {error}"))?;
        let result: String = conn
            .query_row("PRAGMA quick_check", [], |row| row.get(0))
            .map_err(|error| format!("failed to verify restored state_5.sqlite: {error}"))?;
        if result != "ok" {
            return Err(format!(
                "restored state_5.sqlite failed quick_check: {result}"
            ));
        }
    }

    Ok(RestoreResult {
        backup_dir: backup_dir.to_path_buf(),
        target_root: target_home.to_path_buf(),
        restored_files,
        verified: true,
    })
}

fn restore_file(file: &BackupFile, target: &Path) -> Result<(), String> {
    let encrypted = fs::read(&file.backup_path)
        .map_err(|error| format!("failed to read backup payload: {error}"))?;
    let plaintext = unprotect(&encrypted)?;
    atomic_write(target, &plaintext)
}

fn sqlite_sidecar(database: &Path, suffix: &str) -> PathBuf {
    let mut name = database.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    database.with_file_name(name)
}

fn remove_sqlite_sidecars(database: &Path) -> Result<(), String> {
    for suffix in ["-wal", "-shm"] {
        let sidecar = sqlite_sidecar(database, suffix);
        if sidecar.is_file() {
            fs::remove_file(&sidecar).map_err(|error| {
                format!(
                    "failed to remove stale SQLite sidecar {}: {error}",
                    sidecar.display()
                )
            })?;
        }
    }
    Ok(())
}

fn remove_sqlite_files(database: &Path) -> Result<(), String> {
    if database.is_file() {
        fs::remove_file(database).map_err(|error| {
            format!(
                "failed to remove previous SQLite database {}: {error}",
                database.display()
            )
        })?;
    }
    remove_sqlite_sidecars(database)
}

fn remove_absent_core_files(
    manifest: &BackupManifest,
    target_home: &Path,
    paths: &CodexPaths,
) -> Result<(), String> {
    let expected = manifest
        .files
        .iter()
        .map(|file| file.relative_path.as_path())
        .collect::<HashSet<_>>();
    for (relative, target) in [
        (Path::new("auth.json"), target_home.join("auth.json")),
        (Path::new("config.toml"), target_home.join("config.toml")),
        (Path::new("state_5.sqlite"), paths.state_db.clone()),
        (
            Path::new("session_index.jsonl"),
            target_home.join("session_index.jsonl"),
        ),
    ] {
        if !expected.contains(relative) && target.is_file() {
            fs::remove_file(&target).map_err(|error| {
                format!(
                    "failed to remove post-backup file {}: {error}",
                    target.display()
                )
            })?;
        }
    }
    Ok(())
}

pub fn list_recent_backups(
    destination_root: &Path,
    verification_limit: usize,
) -> Result<Vec<BackupSummary>, String> {
    if !destination_root.exists() || verification_limit == 0 {
        return Ok(Vec::new());
    }
    let mut candidates = Vec::new();
    for entry in fs::read_dir(destination_root)
        .map_err(|error| format!("failed to list backup directory: {error}"))?
    {
        let entry = entry.map_err(|error| format!("failed to read backup entry: {error}"))?;
        if !entry
            .file_type()
            .map_err(|error| format!("failed to inspect backup entry: {error}"))?
            .is_dir()
        {
            continue;
        }
        let backup_dir = entry.path();
        let Ok(manifest) = read_backup_manifest(&backup_dir) else {
            continue;
        };
        candidates.push((backup_dir, manifest.created_at_ms));
    }
    candidates.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| right.0.cmp(&left.0)));

    let mut summaries = Vec::new();
    for (backup_dir, _) in candidates.into_iter().take(verification_limit) {
        let Ok(manifest) = verify_backup(&backup_dir) else {
            continue;
        };
        summaries.push(BackupSummary {
            backup_dir,
            source_root: manifest.source_root,
            reason: manifest.reason,
            created_at_ms: manifest.created_at_ms,
            file_count: manifest.files.len(),
            total_bytes: manifest.files.iter().map(|file| file.bytes).sum(),
            verified: true,
            complete_sessions: manifest.complete_sessions,
        });
    }
    Ok(summaries)
}

fn snapshot_sqlite(source: &Path, backup_dir: &Path) -> Result<BackupFile, String> {
    let snapshot = backup_dir.join(".state_5.sqlite.snapshot");
    let source_conn = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| format!("failed to open state_5.sqlite for backup: {error}"))?;
    source_conn
        .backup(MAIN_DB, &snapshot, None)
        .map_err(|error| format!("failed to create SQLite backup snapshot: {error}"))?;
    let result = encrypt_payload(&snapshot, backup_dir, Path::new("state_5.sqlite"));
    let _ = fs::remove_file(snapshot);
    result.map(|mut file| {
        file.source = source.to_path_buf();
        file
    })
}

fn encrypt_payload(
    source: &Path,
    backup_dir: &Path,
    relative_path: &Path,
) -> Result<BackupFile, String> {
    validate_relative_path(relative_path)?;
    let plaintext =
        fs::read(source).map_err(|error| format!("failed to read backup source file: {error}"))?;
    let encrypted = protect(&plaintext)?;
    let backup_path = encrypted_payload_path(backup_dir, relative_path)?;
    atomic_write(&backup_path, &encrypted)?;
    let bytes = fs::metadata(&backup_path)
        .map_err(|error| format!("failed to inspect encrypted backup payload: {error}"))?
        .len();
    Ok(BackupFile {
        source: source.to_path_buf(),
        relative_path: relative_path.to_path_buf(),
        backup_path: backup_path.clone(),
        bytes,
        sha256: sha256_file(&backup_path)?,
        encrypted: true,
    })
}

fn encrypted_payload_path(backup_dir: &Path, relative_path: &Path) -> Result<PathBuf, String> {
    let file_name = relative_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "backup relative path must include a UTF-8 file name".to_string())?;
    let parent = relative_path.parent().unwrap_or_else(|| Path::new(""));
    Ok(backup_dir
        .join("payload")
        .join(parent)
        .join(format!("{file_name}.enc")))
}

fn restore_target(
    paths: &crate::codex_paths::CodexPaths,
    target_home: &Path,
    relative_path: &Path,
) -> Result<PathBuf, String> {
    validate_relative_path(relative_path)?;
    if relative_path == Path::new("state_5.sqlite") {
        return Ok(paths.state_db.clone());
    }
    Ok(target_home.join(relative_path))
}

fn remove_extra_session_files(manifest: &BackupManifest, target_home: &Path) -> Result<(), String> {
    let expected = manifest
        .files
        .iter()
        .filter(|file| file.relative_path.starts_with("sessions"))
        .map(|file| target_home.join(&file.relative_path))
        .collect::<HashSet<_>>();
    let sessions = target_home.join("sessions");
    if !sessions.exists() {
        return Ok(());
    }
    for path in walk_jsonl_files(&sessions)? {
        if !expected.contains(&path) {
            fs::remove_file(&path)
                .map_err(|error| format!("failed to remove post-backup session file: {error}"))?;
        }
    }
    Ok(())
}

fn clear_known_codex_state(target_home: &Path, state_db_is_local: bool) -> Result<(), String> {
    let paths = if state_db_is_local {
        local_codex_paths(target_home)
    } else {
        resolve_user_codex_paths(target_home)?
    };
    for path in [
        target_home.join("auth.json"),
        target_home.join("config.toml"),
        paths.state_db.clone(),
        sqlite_sidecar(&paths.state_db, "-wal"),
        sqlite_sidecar(&paths.state_db, "-shm"),
        paths.session_index,
    ] {
        if path.is_file() {
            fs::remove_file(&path).map_err(|error| {
                format!("failed to clear restored file {}: {error}", path.display())
            })?;
        }
    }
    let sessions = target_home.join("sessions");
    if sessions.is_dir() {
        fs::remove_dir_all(&sessions)
            .map_err(|error| format!("failed to clear restored sessions directory: {error}"))?;
    }
    Ok(())
}

pub(crate) fn migrate_legacy_plaintext_auth(destination_root: &Path) -> Result<(), String> {
    if !destination_root.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(destination_root)
        .map_err(|error| format!("failed to inspect legacy backups: {error}"))?
    {
        let entry =
            entry.map_err(|error| format!("failed to read legacy backup entry: {error}"))?;
        if !entry
            .file_type()
            .map_err(|error| format!("failed to inspect legacy backup entry: {error}"))?
            .is_dir()
        {
            continue;
        }
        let plaintext = entry.path().join("auth.json");
        if !plaintext.is_file() {
            continue;
        }
        let encrypted = protect(
            &fs::read(&plaintext)
                .map_err(|error| format!("failed to read legacy auth backup: {error}"))?,
        )?;
        atomic_write(&entry.path().join("auth.json.enc"), &encrypted)?;
        fs::remove_file(&plaintext)
            .map_err(|error| format!("failed to remove legacy plaintext auth backup: {error}"))?;
    }
    Ok(())
}

fn validate_relative_path(path: &Path) -> Result<(), String> {
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err("backup relative path is unsafe".to_string());
    }
    Ok(())
}

fn safe_reason(reason: &str) -> String {
    let value = reason
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    if value.trim_matches('-').is_empty() {
        "backup".to_string()
    } else {
        value
    }
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file =
        fs::File::open(path).map_err(|error| format!("failed to hash backup file: {error}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("failed to read backup file for hashing: {error}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
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

    use rusqlite::Connection;
    use tempfile::tempdir;

    use super::{
        create_backup, create_local_backup, list_recent_backups, migrate_legacy_plaintext_auth,
        restore_backup, verify_backup,
    };

    fn seed_home(home: &std::path::Path) -> std::path::PathBuf {
        fs::write(
            home.join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"fake-secret-token"}}"#,
        )
        .unwrap();
        fs::write(
            home.join("config.toml"),
            "model = \"gpt-5.5\"\nmodel_instructions_file = \"global\"\n",
        )
        .unwrap();
        let conn = Connection::open(home.join("state_5.sqlite")).unwrap();
        conn.execute(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT)",
            [],
        )
        .unwrap();
        let rollout = home.join("sessions/2026/07/13/rollout-thread-a.jsonl");
        fs::create_dir_all(rollout.parent().unwrap()).unwrap();
        fs::write(
            &rollout,
            concat!(
                r#"{"type":"session_meta","payload":{"id":"thread-a"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"text":"private session body"}}"#,
                "\n",
            ),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO threads (id, rollout_path) VALUES ('thread-a', ?1)",
            [rollout.to_string_lossy().to_string()],
        )
        .unwrap();
        drop(conn);
        fs::write(
            home.join("session_index.jsonl"),
            "{\"id\":\"thread-a\",\"thread_name\":\"Private\"}\n",
        )
        .unwrap();
        rollout
    }

    #[test]
    fn creates_verified_encrypted_snapshot_with_session_payloads() {
        let home = tempdir().unwrap();
        let rollout = seed_home(home.path());
        let backup_root = tempdir().unwrap();

        let manifest = create_backup(home.path(), backup_root.path(), "switch").unwrap();
        let verified = verify_backup(&manifest.backup_dir).unwrap();

        assert_eq!(manifest.reason, "switch");
        assert_eq!(verified.files, manifest.files);
        assert!(manifest.backup_dir.join("manifest.json").exists());
        assert!(!manifest.backup_dir.join("auth.json").exists());
        assert!(!manifest.backup_dir.join("state_5.sqlite").exists());
        assert!(!manifest
            .backup_dir
            .join("sessions/2026/07/13/rollout-thread-a.jsonl")
            .exists());
        assert!(manifest.files.iter().any(|file| file.source == rollout));

        for file in &manifest.files {
            let bytes = fs::read(&file.backup_path).unwrap();
            let visible = String::from_utf8_lossy(&bytes);
            assert!(!visible.contains("fake-secret-token"));
            assert!(!visible.contains("private session body"));
        }
    }

    #[test]
    fn recent_backup_listing_returns_only_the_five_newest_candidates() {
        let home = tempdir().unwrap();
        seed_home(home.path());
        let backup_root = tempdir().unwrap();
        let mut manifests = Vec::new();
        for index in 0..6_u128 {
            let mut manifest = create_backup(
                home.path(),
                backup_root.path(),
                &format!("candidate-{index}"),
            )
            .unwrap();
            manifest.created_at_ms = index;
            fs::write(
                manifest.backup_dir.join("manifest.json"),
                serde_json::to_vec_pretty(&manifest).unwrap(),
            )
            .unwrap();
            manifests.push(manifest);
        }
        let oldest_payload = &manifests[0].files[0].backup_path;
        let mut tampered = fs::read(oldest_payload).unwrap();
        tampered[0] ^= 0xff;
        fs::write(oldest_payload, tampered).unwrap();

        let summaries = list_recent_backups(backup_root.path(), 5).unwrap();

        assert_eq!(summaries.len(), 5);
        assert!(summaries.iter().all(|summary| summary.verified));
        assert!(summaries
            .iter()
            .all(|summary| summary.backup_dir != manifests[0].backup_dir));
        assert_eq!(summaries[0].backup_dir, manifests[5].backup_dir);
    }

    #[test]
    fn legacy_plaintext_auth_is_encrypted_before_the_original_is_removed() {
        let backup_root = tempdir().unwrap();
        let legacy = backup_root.path().join("legacy-backup");
        fs::create_dir_all(&legacy).unwrap();
        let plaintext = br#"{"auth_mode":"chatgpt","token":"fake-legacy-token"}"#;
        fs::write(legacy.join("auth.json"), plaintext).unwrap();

        migrate_legacy_plaintext_auth(backup_root.path()).unwrap();

        assert!(!legacy.join("auth.json").exists());
        let encrypted = fs::read(legacy.join("auth.json.enc")).unwrap();
        assert!(!encrypted
            .windows(plaintext.len())
            .any(|window| window == plaintext));
        assert_eq!(crate::crypto::unprotect(&encrypted).unwrap(), plaintext);
    }

    #[test]
    fn restores_all_payloads_and_rejects_tampering() {
        let home = tempdir().unwrap();
        let rollout = seed_home(home.path());
        let original_auth = fs::read(home.path().join("auth.json")).unwrap();
        let original_config = fs::read(home.path().join("config.toml")).unwrap();
        let original_index = fs::read(home.path().join("session_index.jsonl")).unwrap();
        let original_rollout = fs::read(&rollout).unwrap();
        let backup_root = tempdir().unwrap();
        let manifest = create_backup(home.path(), backup_root.path(), "delete").unwrap();

        fs::write(home.path().join("auth.json"), "{}\n").unwrap();
        fs::write(home.path().join("config.toml"), "model = \"broken\"\n").unwrap();
        fs::remove_file(home.path().join("state_5.sqlite")).unwrap();
        fs::remove_file(home.path().join("session_index.jsonl")).unwrap();
        fs::remove_file(&rollout).unwrap();

        let restored = restore_backup(&manifest.backup_dir, home.path()).unwrap();

        assert_eq!(restored.restored_files, manifest.files.len());
        assert_eq!(
            fs::read(home.path().join("auth.json")).unwrap(),
            original_auth
        );
        assert_eq!(
            fs::read(home.path().join("config.toml")).unwrap(),
            original_config
        );
        assert_eq!(
            fs::read(home.path().join("session_index.jsonl")).unwrap(),
            original_index
        );
        assert_eq!(fs::read(&rollout).unwrap(), original_rollout);
        let conn = Connection::open(home.path().join("state_5.sqlite")).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);

        let payload = &manifest.files[0].backup_path;
        let mut bytes = fs::read(payload).unwrap();
        bytes[0] ^= 0xff;
        fs::write(payload, bytes).unwrap();
        assert!(verify_backup(&manifest.backup_dir).is_err());
    }

    #[test]
    fn restore_routes_state_db_using_the_backed_up_config_before_writing_sqlite() {
        let home = tempdir().unwrap();
        let original_sqlite = tempdir().unwrap();
        let later_sqlite = tempdir().unwrap();
        let config = |path: &std::path::Path| {
            format!("sqlite_home = \"{}\"\n", path.display()).replace('\\', "\\\\")
        };
        fs::write(
            home.path().join("config.toml"),
            config(original_sqlite.path()),
        )
        .unwrap();
        let conn = Connection::open(original_sqlite.path().join("state_5.sqlite")).unwrap();
        conn.execute("CREATE TABLE marker (value TEXT)", [])
            .unwrap();
        conn.execute("INSERT INTO marker VALUES ('original')", [])
            .unwrap();
        drop(conn);
        let backup_root = tempdir().unwrap();
        let manifest = create_backup(home.path(), backup_root.path(), "external-sqlite").unwrap();
        assert!(!manifest.state_db_is_local);

        fs::write(home.path().join("config.toml"), config(later_sqlite.path())).unwrap();
        let conn = Connection::open(later_sqlite.path().join("state_5.sqlite")).unwrap();
        conn.execute("CREATE TABLE marker (value TEXT)", [])
            .unwrap();
        conn.execute("INSERT INTO marker VALUES ('later')", [])
            .unwrap();
        drop(conn);
        fs::remove_file(original_sqlite.path().join("state_5.sqlite")).unwrap();

        restore_backup(&manifest.backup_dir, home.path()).unwrap();

        let conn = Connection::open(original_sqlite.path().join("state_5.sqlite")).unwrap();
        let value: String = conn
            .query_row("SELECT value FROM marker", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, "original");
        assert!(!later_sqlite.path().join("state_5.sqlite").exists());
    }

    #[test]
    fn local_backup_ignores_a_user_sqlite_home_binding_for_shared_roots() {
        let shared = tempdir().unwrap();
        let external = tempdir().unwrap();
        fs::write(
            shared.path().join("config.toml"),
            format!("sqlite_home = \"{}\"\n", external.path().display()).replace('\\', "\\\\"),
        )
        .unwrap();
        Connection::open(shared.path().join("state_5.sqlite"))
            .unwrap()
            .execute("CREATE TABLE local_marker (value TEXT)", [])
            .unwrap();
        Connection::open(external.path().join("state_5.sqlite"))
            .unwrap()
            .execute("CREATE TABLE external_marker (value TEXT)", [])
            .unwrap();
        let backup_root = tempdir().unwrap();

        let manifest =
            create_local_backup(shared.path(), backup_root.path(), "shared-local").unwrap();

        assert!(manifest.state_db_is_local);
        let state = manifest
            .files
            .iter()
            .find(|file| file.relative_path == std::path::Path::new("state_5.sqlite"))
            .unwrap();
        assert_eq!(state.source, shared.path().join("state_5.sqlite"));
    }
}

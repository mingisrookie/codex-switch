use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BackupManifest {
    pub reason: String,
    pub backup_dir: PathBuf,
    pub files: Vec<BackupFile>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BackupFile {
    pub source: PathBuf,
    pub backup_path: PathBuf,
    pub bytes: u64,
    pub sha256: String,
}

pub fn create_backup(home: &Path, destination_root: &Path, reason: &str) -> Result<BackupManifest, String> {
    let backup_dir = destination_root.join(format!("{}-{reason}", timestamp_millis()?));
    fs::create_dir_all(&backup_dir).map_err(|error| format!("failed to create backup dir: {error}"))?;

    let mut files = Vec::new();
    for relative in [
        "auth.json",
        "config.toml",
        "state_5.sqlite",
        "state_5.sqlite-wal",
        "state_5.sqlite-shm",
    ] {
        let source = home.join(relative);
        if !source.exists() {
            continue;
        }
        let target = backup_dir.join(relative);
        files.push(copy_with_hash(&source, &target)?);
    }

    write_sessions_manifest(home, &backup_dir)?;

    Ok(BackupManifest {
        reason: reason.to_string(),
        backup_dir,
        files,
    })
}

fn copy_with_hash(source: &Path, target: &Path) -> Result<BackupFile, String> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("failed to create backup parent: {error}"))?;
    }
    fs::copy(source, target).map_err(|error| format!("failed to copy backup file: {error}"))?;
    let bytes = fs::metadata(target)
        .map_err(|error| format!("failed to stat backup file: {error}"))?
        .len();
    Ok(BackupFile {
        source: source.to_path_buf(),
        backup_path: target.to_path_buf(),
        bytes,
        sha256: sha256_file(target)?,
    })
}

fn write_sessions_manifest(home: &Path, backup_dir: &Path) -> Result<(), String> {
    let sessions = home.join("sessions");
    let mut files = Vec::new();
    if sessions.exists() {
        for entry in WalkDir::new(&sessions).into_iter().filter_map(Result::ok) {
            if entry.file_type().is_file()
                && entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
            {
                files.push(
                    entry
                        .path()
                        .strip_prefix(home)
                        .unwrap_or(entry.path())
                        .to_string_lossy()
                        .replace('/', "\\"),
                );
            }
        }
    }
    files.sort();
    let json = serde_json::to_string_pretty(&files)
        .map_err(|error| format!("failed to serialize sessions manifest: {error}"))?;
    fs::write(backup_dir.join("sessions-manifest.json"), json)
        .map_err(|error| format!("failed to write sessions manifest: {error}"))
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(|error| format!("failed to hash backup file: {error}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 8192];
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

    use tempfile::tempdir;

    use super::create_backup;

    #[test]
    fn creates_snapshot_with_core_files_and_session_manifest() {
        let home = tempdir().unwrap();
        fs::write(home.path().join("auth.json"), "{}").unwrap();
        fs::write(home.path().join("config.toml"), "model = \"gpt-5.5\"").unwrap();
        fs::write(home.path().join("state_5.sqlite"), "sqlite").unwrap();
        fs::write(home.path().join("state_5.sqlite-wal"), "wal").unwrap();
        fs::create_dir_all(home.path().join("sessions/2026/06/23")).unwrap();
        fs::write(home.path().join("sessions/2026/06/23/rollout.jsonl"), "{}\n").unwrap();
        let backup_root = tempdir().unwrap();

        let manifest = create_backup(home.path(), backup_root.path(), "switch").unwrap();

        assert_eq!(manifest.reason, "switch");
        assert!(manifest.backup_dir.join("auth.json").exists());
        assert!(manifest.backup_dir.join("config.toml").exists());
        assert!(manifest.backup_dir.join("state_5.sqlite").exists());
        assert!(manifest.backup_dir.join("state_5.sqlite-wal").exists());
        assert!(manifest.backup_dir.join("sessions-manifest.json").exists());
        assert!(!manifest.backup_dir.join("state_5.sqlite-shm").exists());
        assert_eq!(manifest.files.len(), 4);
    }
}

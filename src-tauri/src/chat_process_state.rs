use std::{
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Read},
    path::{Path, PathBuf},
};

#[cfg(windows)]
use std::os::windows::{
    fs::{MetadataExt, OpenOptionsExt},
    io::AsRawHandle,
};
#[cfg(windows)]
use windows_sys::Win32::Foundation::HANDLE;
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{
    GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
};

use crate::file_ops::atomic_write;

pub(crate) const CHAT_PROCESS_STATE_RELATIVE_PATH: &str = "process_manager/chat_processes.json";
const MAX_CHAT_PROCESS_STATE_BYTES: u64 = 16 * 1024 * 1024;

pub(crate) struct ChatProcessStateSnapshot {
    pub(crate) path: PathBuf,
    pub(crate) bytes: Vec<u8>,
    identity: Option<FileIdentity>,
    _parent_lock: Option<File>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity {
    volume_serial: u32,
    file_index: u64,
}

pub(crate) fn validate_snapshot_bytes(bytes: u64) -> Result<(), String> {
    if bytes > MAX_CHAT_PROCESS_STATE_BYTES {
        return Err(format!(
            "ChatGPT process state exceeds the {} byte safety limit",
            MAX_CHAT_PROCESS_STATE_BYTES
        ));
    }
    Ok(())
}

pub(crate) fn backup_source(codex_home: &Path) -> Result<Option<(PathBuf, u64)>, String> {
    let path = codex_home.join(CHAT_PROCESS_STATE_RELATIVE_PATH);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "failed to inspect ChatGPT process state {}: {error}",
                path.display()
            ));
        }
    };
    if is_reparse_point(&metadata) || metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("ChatGPT process state must be a regular file".to_string());
    }
    validate_snapshot_bytes(metadata.len())?;
    ensure_managed_parent(codex_home, &path)?;
    let path = managed_file_path(codex_home, &path)?;
    Ok(Some((path, metadata.len())))
}

pub(crate) fn read_snapshot(codex_home: &Path) -> Result<Option<ChatProcessStateSnapshot>, String> {
    let Some((path, expected_bytes)) = backup_source(codex_home)? else {
        return Ok(None);
    };
    let parent_lock = open_parent_lock(&path)?;
    let mut source = open_exclusive(&path)?;
    let identity = file_identity(&source)?;
    let metadata = source.metadata().map_err(|error| {
        format!(
            "failed to inspect locked ChatGPT process state {}: {error}",
            path.display()
        )
    })?;
    if is_reparse_point(&metadata) || metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("ChatGPT process state must be a regular file".to_string());
    }
    validate_snapshot_bytes(metadata.len())?;
    if metadata.len() != expected_bytes {
        return Err("ChatGPT process state changed during validation".to_string());
    }

    let read_limit = MAX_CHAT_PROCESS_STATE_BYTES
        .checked_add(1)
        .ok_or_else(|| "ChatGPT process state size overflow".to_string())?;
    let mut bytes = Vec::new();
    (&mut source)
        .take(read_limit)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            format!(
                "failed to read ChatGPT process state {}: {error}",
                path.display()
            )
        })?;
    let observed_bytes = u64::try_from(bytes.len())
        .map_err(|_| "ChatGPT process state size overflow".to_string())?;
    validate_snapshot_bytes(observed_bytes)?;
    if observed_bytes != expected_bytes
        || source
            .metadata()
            .map_err(|error| {
                format!(
                    "failed to revalidate locked ChatGPT process state {}: {error}",
                    path.display()
                )
            })?
            .len()
            != expected_bytes
    {
        return Err("ChatGPT process state changed during validation".to_string());
    }
    Ok(Some(ChatProcessStateSnapshot {
        path,
        bytes,
        identity,
        _parent_lock: parent_lock,
    }))
}

pub(crate) fn repair_after_shutdown(
    codex_home: &Path,
    checkpoint_bytes: Option<&[u8]>,
) -> Result<bool, String> {
    let source = read_snapshot(codex_home)?;
    let Some(ChatProcessStateSnapshot {
        path,
        bytes,
        identity,
        _parent_lock,
    }) = source
    else {
        return if checkpoint_bytes.is_none() {
            Ok(false)
        } else {
            Err("ChatGPT process state changed after the runtime checkpoint".to_string())
        };
    };
    let Some(checkpoint_bytes) = checkpoint_bytes else {
        return Err("ChatGPT process state changed after the runtime checkpoint".to_string());
    };
    if bytes != checkpoint_bytes {
        return Err("ChatGPT process state changed after the runtime checkpoint".to_string());
    }
    if serde_json::from_slice::<serde_json::Value>(&bytes).is_ok() {
        return Ok(false);
    }
    if !is_recognized_corruption(&bytes) {
        return Err(
            "ChatGPT process state has an unknown format; the original file was preserved"
                .to_string(),
        );
    }

    let Some(current) = read_snapshot(codex_home)? else {
        return Err("ChatGPT process state changed before repair".to_string());
    };
    if current.path != path || current.bytes != bytes || current.identity != identity {
        return Err("ChatGPT process state changed before repair".to_string());
    }
    atomic_write(&path, b"[]").map_err(|error| {
        format!(
            "failed to repair ChatGPT process state {}: {error}",
            path.display()
        )
    })?;
    let Some(verified) = read_snapshot(codex_home)? else {
        return Err("ChatGPT process state repair verification failed".to_string());
    };
    if verified.path == path && verified.bytes == b"[]" {
        Ok(true)
    } else {
        Err("ChatGPT process state repair verification failed".to_string())
    }
}

pub(crate) fn restore_target(codex_home: &Path) -> Result<PathBuf, String> {
    if !codex_home.exists() {
        fs::create_dir_all(codex_home)
            .map_err(|error| format!("failed to create Codex restore root: {error}"))?;
    }
    let path = codex_home.join(CHAT_PROCESS_STATE_RELATIVE_PATH);
    let parent = path
        .parent()
        .ok_or_else(|| "ChatGPT process state path has no parent".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("failed to create ChatGPT process state directory: {error}"))?;
    ensure_managed_parent(codex_home, &path)?;
    let parent = fs::canonicalize(parent)
        .map_err(|error| format!("failed to resolve ChatGPT process state directory: {error}"))?;
    Ok(parent.join("chat_processes.json"))
}

pub(crate) fn existing_restore_target(codex_home: &Path) -> Result<Option<PathBuf>, String> {
    if !codex_home.exists() {
        return Ok(None);
    }
    let path = codex_home.join(CHAT_PROCESS_STATE_RELATIVE_PATH);
    let Some(parent) = path.parent() else {
        return Err("ChatGPT process state path has no parent".to_string());
    };
    if !parent.exists() {
        return Ok(None);
    }
    ensure_managed_parent(codex_home, &path)?;
    let parent = fs::canonicalize(parent)
        .map_err(|error| format!("failed to resolve ChatGPT process state directory: {error}"))?;
    Ok(Some(parent.join("chat_processes.json")))
}

fn open_exclusive(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    options
        .share_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    options.open(path).map_err(|error| {
        format!(
            "failed to lock ChatGPT process state {}: {error}",
            path.display()
        )
    })
}

#[cfg(windows)]
fn open_parent_lock(path: &Path) -> Result<Option<File>, String> {
    let parent = path
        .parent()
        .ok_or_else(|| "ChatGPT process state path has no parent".to_string())?;
    let mut options = OpenOptions::new();
    options
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    options.open(parent).map(Some).map_err(|error| {
        format!(
            "failed to lock ChatGPT process state directory {}: {error}",
            parent.display()
        )
    })
}

#[cfg(not(windows))]
fn open_parent_lock(_path: &Path) -> Result<Option<File>, String> {
    Ok(None)
}

fn is_recognized_corruption(bytes: &[u8]) -> bool {
    bytes.is_empty()
        || bytes
            .iter()
            .all(|byte| *byte == 0 || byte.is_ascii_whitespace())
}

fn ensure_managed_parent(codex_home: &Path, path: &Path) -> Result<(), String> {
    let home = fs::canonicalize(codex_home)
        .map_err(|error| format!("failed to resolve Codex home: {error}"))?;
    let parent = path
        .parent()
        .ok_or_else(|| "ChatGPT process state path has no parent".to_string())?;
    if let Ok(metadata) = fs::symlink_metadata(parent) {
        if is_reparse_point(&metadata) || metadata.file_type().is_symlink() {
            return Err("ChatGPT process state directory must not be a link".to_string());
        }
    }
    let parent = match fs::canonicalize(parent) {
        Ok(parent) => parent,
        Err(error) if error.kind() == ErrorKind::NotFound => home.clone(),
        Err(error) => {
            return Err(format!(
                "failed to resolve ChatGPT process state directory: {error}"
            ));
        }
    };
    if !parent.starts_with(&home) {
        return Err("ChatGPT process state escaped the managed Codex home".to_string());
    }
    Ok(())
}

fn managed_file_path(codex_home: &Path, path: &Path) -> Result<PathBuf, String> {
    let home = fs::canonicalize(codex_home)
        .map_err(|error| format!("failed to resolve Codex home: {error}"))?;
    let file = fs::canonicalize(path)
        .map_err(|error| format!("failed to resolve ChatGPT process state file: {error}"))?;
    if !file.starts_with(&home) {
        return Err("ChatGPT process state escaped the managed Codex home".to_string());
    }
    Ok(file)
}

#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(windows)]
fn file_identity(file: &File) -> Result<Option<FileIdentity>, String> {
    let mut information = unsafe { std::mem::zeroed::<BY_HANDLE_FILE_INFORMATION>() };
    let ok = unsafe {
        GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut information as *mut _)
    };
    if ok == 0 {
        return Err(format!(
            "failed to identify locked ChatGPT process state: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(Some(FileIdentity {
        volume_serial: information.dwVolumeSerialNumber,
        file_index: (u64::from(information.nFileIndexHigh) << 32)
            | u64::from(information.nFileIndexLow),
    }))
}

#[cfg(not(windows))]
fn file_identity(_file: &File) -> Result<Option<FileIdentity>, String> {
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{
        backup_source, read_snapshot, repair_after_shutdown, CHAT_PROCESS_STATE_RELATIVE_PATH,
        MAX_CHAT_PROCESS_STATE_BYTES,
    };

    #[test]
    fn missing_process_state_is_left_for_chatgpt_to_create() {
        let home = tempdir().unwrap();
        let path = home.path().join(CHAT_PROCESS_STATE_RELATIVE_PATH);

        assert!(!repair_after_shutdown(home.path(), None).unwrap());
        assert!(!path.exists());
    }

    #[test]
    fn valid_process_state_is_not_rewritten() {
        let home = tempdir().unwrap();
        let path = home.path().join(CHAT_PROCESS_STATE_RELATIVE_PATH);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = br#"[{"id":"process-1"}]"#;
        fs::write(&path, original).unwrap();

        assert!(!repair_after_shutdown(home.path(), Some(original)).unwrap());
        assert_eq!(fs::read(path).unwrap(), original);
    }

    #[test]
    fn nul_filled_process_state_is_replaced_with_an_empty_array() {
        let home = tempdir().unwrap();
        let path = home.path().join(CHAT_PROCESS_STATE_RELATIVE_PATH);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = vec![0_u8; 103_405];
        fs::write(&path, &original).unwrap();

        assert!(repair_after_shutdown(home.path(), Some(&original)).unwrap());
        assert_eq!(fs::read(path).unwrap(), b"[]");
    }

    #[test]
    fn valid_json_with_an_unknown_shape_is_preserved_without_blocking() {
        let home = tempdir().unwrap();
        let path = home.path().join(CHAT_PROCESS_STATE_RELATIVE_PATH);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = br#"{"records":[]}"#;
        fs::write(&path, original).unwrap();

        assert!(!repair_after_shutdown(home.path(), Some(original)).unwrap());
        assert_eq!(fs::read(path).unwrap(), original);
    }

    #[test]
    fn array_with_unknown_records_is_preserved_without_blocking() {
        let home = tempdir().unwrap();
        let path = home.path().join(CHAT_PROCESS_STATE_RELATIVE_PATH);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = br#"[null]"#;
        fs::write(&path, original).unwrap();

        assert!(!repair_after_shutdown(home.path(), Some(original)).unwrap());
        assert_eq!(fs::read(path).unwrap(), original);
    }

    #[test]
    fn non_file_process_state_fails_closed() {
        let home = tempdir().unwrap();
        let path = home.path().join(CHAT_PROCESS_STATE_RELATIVE_PATH);
        fs::create_dir_all(&path).unwrap();

        let error = repair_after_shutdown(home.path(), Some(b"[]")).unwrap_err();
        assert!(
            error.contains("must be a regular file"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn checkpoint_drift_is_rejected_without_repairing() {
        let home = tempdir().unwrap();
        let path = home.path().join(CHAT_PROCESS_STATE_RELATIVE_PATH);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"bad-json").unwrap();

        let error = repair_after_shutdown(home.path(), Some(b"not-json")).unwrap_err();

        assert!(error.contains("changed after"), "unexpected error: {error}");
        assert_eq!(fs::read(path).unwrap(), b"bad-json");
    }

    #[test]
    fn oversized_process_state_is_rejected_before_reading() {
        let home = tempdir().unwrap();
        let path = home.path().join(CHAT_PROCESS_STATE_RELATIVE_PATH);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let file = fs::File::create(&path).unwrap();
        file.set_len(MAX_CHAT_PROCESS_STATE_BYTES + 1).unwrap();

        let error = backup_source(home.path()).unwrap_err();
        assert!(error.contains("safety limit"), "unexpected error: {error}");
    }

    #[test]
    fn empty_process_state_is_repaired_as_known_corruption() {
        let home = tempdir().unwrap();
        let path = home.path().join(CHAT_PROCESS_STATE_RELATIVE_PATH);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"").unwrap();

        assert!(repair_after_shutdown(home.path(), Some(b"")).unwrap());
        assert_eq!(read_snapshot(home.path()).unwrap().unwrap().bytes, b"[]");
    }
}

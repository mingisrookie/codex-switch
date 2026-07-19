use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
use walkdir::WalkDir;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    ensure_parent(path)?;
    let temp_path = unique_temp_path(path)?;
    let result = (|| {
        let mut file = create_new(&temp_path)?;
        file.write_all(bytes)
            .map_err(|error| format!("failed to write temporary file: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("failed to flush temporary file: {error}"))?;
        replace_path(&temp_path, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

pub fn atomic_copy(source: &Path, target: &Path) -> Result<u64, String> {
    ensure_parent(target)?;
    let temp_path = unique_temp_path(target)?;
    let result = (|| {
        let mut source_file = File::open(source)
            .map_err(|error| format!("failed to open source file for copy: {error}"))?;
        let mut target_file = create_new(&temp_path)?;
        let copied = io::copy(&mut source_file, &mut target_file)
            .map_err(|error| format!("failed to copy file: {error}"))?;
        target_file
            .sync_all()
            .map_err(|error| format!("failed to flush copied file: {error}"))?;
        replace_path(&temp_path, target)?;
        Ok(copied)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

pub fn atomic_rewrite<F>(path: &Path, writer: F) -> Result<(), String>
where
    F: FnOnce(&mut File) -> Result<(), String>,
{
    ensure_parent(path)?;
    let temp_path = unique_temp_path(path)?;
    let result = (|| {
        let mut file = create_new(&temp_path)?;
        writer(&mut file)?;
        file.sync_all()
            .map_err(|error| format!("failed to flush rewritten file: {error}"))?;
        replace_path(&temp_path, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

pub fn walk_jsonl_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    for entry in WalkDir::new(root) {
        let entry = entry.map_err(|error| {
            format!("failed to walk JSONL directory {}: {error}", root.display())
        })?;
        if entry.file_type().is_file()
            && entry
                .path()
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
        {
            files.push(entry.into_path());
        }
    }
    files.sort();
    Ok(files)
}

fn create_new(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("failed to create temporary file: {error}"))
}

fn ensure_parent(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "target file must have a parent directory".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("failed to create target parent directory: {error}"))
}

fn unique_temp_path(path: &Path) -> Result<PathBuf, String> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "target file path must include a UTF-8 file name".to_string())?;
    let sequence = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    Ok(path.with_file_name(format!(
        ".{file_name}.codex-switch.{}.{}.tmp",
        std::process::id(),
        sequence
    )))
}

#[cfg(windows)]
fn windows_api_path(path: &Path) -> Result<PathBuf, String> {
    let parent = path
        .parent()
        .ok_or_else(|| "atomic replace path must have a parent directory".to_string())?;
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let file_name = path
        .file_name()
        .ok_or_else(|| "atomic replace path must have a file name".to_string())?;
    let canonical_parent = fs::canonicalize(parent)
        .map_err(|error| format!("failed to resolve atomic replace parent directory: {error}"))?;
    Ok(canonical_parent.join(file_name))
}

#[cfg(windows)]
fn replace_path(source: &Path, target: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    // Rust's file APIs accept long Windows paths, but MoveFileExW receives the
    // raw path passed here. Canonicalizing the existing parent produces an
    // absolute verbatim path and also resolves directory junctions before the
    // atomic replacement.
    let source = windows_api_path(source)?;
    let target = windows_api_path(target)?;
    let source_wide = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let target_wide = target
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let ok = unsafe {
        MoveFileExW(
            source_wide.as_ptr(),
            target_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        return Err(format!(
            "failed to atomically replace file: {}",
            io::Error::last_os_error()
        ));
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_path(source: &Path, target: &Path) -> Result<(), String> {
    fs::rename(source, target)
        .map_err(|error| format!("failed to atomically replace file: {error}"))
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::{atomic_write, walk_jsonl_files};

    #[test]
    fn jsonl_walk_propagates_directory_errors() {
        let temp = tempdir().unwrap();

        let error = walk_jsonl_files(&temp.path().join("missing")).unwrap_err();

        assert!(error.contains("failed to walk JSONL directory"), "{error}");
    }

    #[cfg(windows)]
    #[test]
    fn atomic_write_supports_backup_paths_over_legacy_max_path() {
        use std::os::windows::ffi::OsStrExt;

        let temp = tempdir().unwrap();
        let parent = temp
            .path()
            .join("backups")
            .join("1784447000000-99999-0-switch-runtime-current")
            .join("payload")
            .join("sessions")
            .join("2026")
            .join("07")
            .join("19");
        let target = parent.join(format!("rollout-{}.jsonl.enc", "x".repeat(115)));
        let generated_temp = super::unique_temp_path(&target).unwrap();

        assert!(
            generated_temp.as_os_str().encode_wide().count() > 260,
            "the regression fixture must cross the legacy MAX_PATH boundary"
        );

        atomic_write(&target, b"long-path-backup").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"long-path-backup");
    }
}

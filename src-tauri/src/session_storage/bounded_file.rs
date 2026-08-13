use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::Read,
    path::Path,
    time::SystemTime,
};

#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileStamp {
    bytes: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
}

pub(crate) fn read_regular_file_bounded(path: &Path, max_bytes: u64) -> Result<Vec<u8>, ()> {
    if max_bytes == 0 {
        return Err(());
    }
    let before_path = fs::symlink_metadata(path).map_err(|_| ())?;
    validate_metadata(&before_path, max_bytes)?;

    let mut file = open_without_following_reparse(path).map_err(|_| ())?;
    let before_handle = file.metadata().map_err(|_| ())?;
    validate_metadata(&before_handle, max_bytes)?;
    if file_stamp(&before_handle) != file_stamp(&before_path) {
        return Err(());
    }
    let before_identity = file_identity(&file, &before_handle).ok_or(())?;

    let capacity = usize::try_from(before_handle.len()).unwrap_or(usize::MAX);
    let mut bytes = Vec::with_capacity(capacity.min(1024 * 1024));
    {
        let mut limited = (&mut file).take(max_bytes.saturating_add(1));
        limited.read_to_end(&mut bytes).map_err(|_| ())?;
    }
    if bytes.is_empty() || u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max_bytes {
        return Err(());
    }

    let after_handle = file.metadata().map_err(|_| ())?;
    validate_metadata(&after_handle, max_bytes)?;
    if file_stamp(&before_handle) != file_stamp(&after_handle)
        || file_identity(&file, &after_handle).as_ref() != Some(&before_identity)
    {
        return Err(());
    }

    let after_path = fs::symlink_metadata(path).map_err(|_| ())?;
    validate_metadata(&after_path, max_bytes)?;
    let after_file = open_without_following_reparse(path).map_err(|_| ())?;
    let reopened_metadata = after_file.metadata().map_err(|_| ())?;
    validate_metadata(&reopened_metadata, max_bytes)?;
    if file_stamp(&before_handle) != file_stamp(&after_path)
        || file_stamp(&before_handle) != file_stamp(&reopened_metadata)
        || file_identity(&after_file, &reopened_metadata).as_ref() != Some(&before_identity)
    {
        return Err(());
    }
    Ok(bytes)
}

pub(crate) fn same_regular_file_identity(left: &Path, right: &Path) -> Result<bool, ()> {
    let left_path = fs::symlink_metadata(left).map_err(|_| ())?;
    let right_path = fs::symlink_metadata(right).map_err(|_| ())?;
    validate_metadata(&left_path, u64::MAX)?;
    validate_metadata(&right_path, u64::MAX)?;
    let left_file = open_without_following_reparse(left).map_err(|_| ())?;
    let right_file = open_without_following_reparse(right).map_err(|_| ())?;
    let left_metadata = left_file.metadata().map_err(|_| ())?;
    let right_metadata = right_file.metadata().map_err(|_| ())?;
    validate_metadata(&left_metadata, u64::MAX)?;
    validate_metadata(&right_metadata, u64::MAX)?;
    if file_stamp(&left_path) != file_stamp(&left_metadata)
        || file_stamp(&right_path) != file_stamp(&right_metadata)
    {
        return Err(());
    }
    match (
        file_identity(&left_file, &left_metadata),
        file_identity(&right_file, &right_metadata),
    ) {
        (Some(left), Some(right)) => Ok(left == right),
        _ => Err(()),
    }
}

fn validate_metadata(metadata: &Metadata, max_bytes: u64) -> Result<(), ()> {
    if !metadata.is_file()
        || metadata_is_link_or_reparse(metadata)
        || metadata.len() == 0
        || metadata.len() > max_bytes
    {
        return Err(());
    }
    Ok(())
}

fn file_stamp(metadata: &Metadata) -> FileStamp {
    FileStamp {
        bytes: metadata.len(),
        modified: metadata.modified().ok(),
        created: metadata.created().ok(),
    }
}

#[cfg(windows)]
fn open_without_following_reparse(path: &Path) -> std::io::Result<File> {
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

#[cfg(not(windows))]
fn open_without_following_reparse(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().read(true).open(path)
}

#[cfg(windows)]
fn metadata_is_link_or_reparse(metadata: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_link_or_reparse(metadata: &Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn file_identity(file: &File, _metadata: &Metadata) -> Option<(u64, u64)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION},
    };

    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    let result =
        unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut information) };
    if result == 0 {
        return None;
    }
    let index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
    Some((u64::from(information.dwVolumeSerialNumber), index))
}

#[cfg(unix)]
fn file_identity(_file: &File, metadata: &Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;

    Some((metadata.dev(), metadata.ino()))
}

#[cfg(not(any(windows, unix)))]
fn file_identity(_file: &File, metadata: &Metadata) -> Option<(u64, u64)> {
    Some((metadata.len(), 0))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{read_regular_file_bounded, same_regular_file_identity};

    #[test]
    fn reads_only_nonempty_regular_files_within_the_limit() {
        let root = tempdir().unwrap();
        let path = root.path().join("state.json");
        fs::write(&path, b"abc").unwrap();

        assert_eq!(read_regular_file_bounded(&path, 3).unwrap(), b"abc");
        assert!(read_regular_file_bounded(&path, 2).is_err());

        fs::write(&path, b"").unwrap();
        assert!(read_regular_file_bounded(&path, 3).is_err());
        assert!(read_regular_file_bounded(root.path(), 3).is_err());
    }

    #[test]
    fn distinguishes_a_hard_link_from_an_independent_copy() {
        let root = tempdir().unwrap();
        let source = root.path().join("source.sqlite");
        let linked = root.path().join("linked.sqlite");
        let copied = root.path().join("copied.sqlite");
        fs::write(&source, b"sqlite").unwrap();
        fs::hard_link(&source, &linked).unwrap();
        fs::copy(&source, &copied).unwrap();

        assert!(same_regular_file_identity(&source, &linked).unwrap());
        assert!(!same_regular_file_identity(&source, &copied).unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn rejects_reparse_points() {
        use std::os::windows::fs::symlink_file;

        let root = tempdir().unwrap();
        let target = root.path().join("target.json");
        let link = root.path().join("link.json");
        fs::write(&target, b"abc").unwrap();
        if symlink_file(&target, &link).is_ok() {
            assert!(read_regular_file_bounded(&link, 3).is_err());
        }
    }
}

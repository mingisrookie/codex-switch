use std::{
    fs,
    path::{Component, Path, PathBuf},
};

pub const DIAGNOSTIC_ARCHIVE_PREFIX: &str = "ChatGPT-Switch-Diagnostics-";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalExportTime {
    pub filename_timestamp: String,
    pub rfc3339: String,
    pub timezone_offset_minutes: i32,
}

pub fn downloads_dir() -> Result<PathBuf, String> {
    downloads_dir_platform().and_then(validate_downloads_dir)
}

pub fn local_filename_timestamp() -> Result<String, String> {
    local_export_time().map(|value| value.filename_timestamp)
}

pub fn local_export_time() -> Result<LocalExportTime, String> {
    local_export_time_platform()
}

pub fn validate_downloads_dir(path: PathBuf) -> Result<PathBuf, String> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err("the Windows Downloads directory is unsafe".to_string());
    }
    let canonical = fs::canonicalize(path)
        .map_err(|_| "the Windows Downloads directory is unavailable".to_string())?;
    if !canonical.is_dir() {
        return Err("the Windows Downloads directory is unavailable".to_string());
    }
    Ok(canonical)
}

pub fn validate_export_file(downloads: &Path, exported: &Path) -> Result<PathBuf, String> {
    let downloads = validate_downloads_dir(downloads.to_path_buf())?;
    let metadata = fs::symlink_metadata(exported)
        .map_err(|_| "the diagnostic archive is unavailable".to_string())?;
    if !metadata.file_type().is_file() || is_reparse_point(&metadata) {
        return Err("the diagnostic archive is unsafe".to_string());
    }
    let exported = fs::canonicalize(exported)
        .map_err(|_| "the diagnostic archive is unavailable".to_string())?;
    if exported.parent() != Some(downloads.as_path())
        || !exported
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_diagnostic_archive_name)
    {
        return Err("the diagnostic archive is outside the Downloads directory".to_string());
    }
    Ok(exported)
}

pub fn open_export_location(exported: &Path) -> Result<(), String> {
    let downloads = downloads_dir()?;
    open_export_location_in(&downloads, exported)
}

pub fn open_export_location_in(directory: &Path, exported: &Path) -> Result<(), String> {
    open_export_location_with(directory, exported, open_in_explorer)
}

pub fn open_directory(path: &Path) -> Result<(), String> {
    open_directory_with(path, open_directory_in_explorer)
}

fn open_export_location_with<Open>(
    downloads: &Path,
    exported: &Path,
    open: Open,
) -> Result<(), String>
where
    Open: FnOnce(&Path) -> Result<(), String>,
{
    let exported = validate_export_file(downloads, exported)?;
    open(&exported)
}

fn open_directory_with<Open>(path: &Path, open: Open) -> Result<(), String>
where
    Open: FnOnce(&Path) -> Result<(), String>,
{
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| "the diagnostic directory is unavailable".to_string())?;
    if !metadata.file_type().is_dir() || is_reparse_point(&metadata) {
        return Err("the diagnostic directory is unsafe".to_string());
    }
    let path = fs::canonicalize(path)
        .map_err(|_| "the diagnostic directory is unavailable".to_string())?;
    if !path.is_dir() {
        return Err("the diagnostic directory is unavailable".to_string());
    }
    open(&path)
}

pub fn is_diagnostic_archive_name(name: &str) -> bool {
    let Some(body) = name
        .strip_prefix(DIAGNOSTIC_ARCHIVE_PREFIX)
        .and_then(|name| name.strip_suffix(".zip"))
    else {
        return false;
    };
    let body = body.as_bytes();
    if body.len() < 19 {
        return false;
    }
    if !body[..19].iter().copied().enumerate().all(|(index, byte)| {
        matches!(index, 8 | 15) && byte == b'-' || !matches!(index, 8 | 15) && byte.is_ascii_digit()
    }) {
        return false;
    }
    let suffix = &body[19..];
    suffix.is_empty()
        || suffix.first() == Some(&b'-')
            && suffix.len() > 1
            && suffix[1..].iter().all(u8::is_ascii_digit)
}

#[cfg(windows)]
fn downloads_dir_platform() -> Result<PathBuf, String> {
    use std::{ffi::OsString, os::windows::ffi::OsStringExt};
    use windows::{
        core::PWSTR,
        Win32::{
            System::Com::CoTaskMemFree,
            UI::Shell::{FOLDERID_Downloads, SHGetKnownFolderPath, KF_FLAG_DEFAULT},
        },
    };

    struct KnownFolderPath(PWSTR);
    impl Drop for KnownFolderPath {
        fn drop(&mut self) {
            unsafe {
                CoTaskMemFree(Some(self.0 .0.cast()));
            }
        }
    }

    let path = unsafe { SHGetKnownFolderPath(&FOLDERID_Downloads, KF_FLAG_DEFAULT, None) }
        .map(KnownFolderPath)
        .map_err(|_| "failed to resolve the Windows Downloads directory".to_string())?;
    if path.0 .0.is_null() {
        return Err("failed to resolve the Windows Downloads directory".to_string());
    }
    let mut length = 0usize;
    while length <= 32_768 && unsafe { *path.0 .0.add(length) } != 0 {
        length += 1;
    }
    if length == 0 || length > 32_768 {
        return Err("the Windows Downloads directory is invalid".to_string());
    }
    let wide = unsafe { std::slice::from_raw_parts(path.0 .0, length) };
    Ok(PathBuf::from(OsString::from_wide(wide)))
}

#[cfg(not(windows))]
fn downloads_dir_platform() -> Result<PathBuf, String> {
    Err("diagnostic export is only supported on Windows".to_string())
}

#[cfg(windows)]
fn local_export_time_platform() -> Result<LocalExportTime, String> {
    use windows_sys::Win32::{
        Foundation::SYSTEMTIME,
        System::{
            SystemInformation::GetLocalTime,
            Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION},
        },
    };

    let mut value = unsafe { std::mem::zeroed::<SYSTEMTIME>() };
    let mut time_zone = TIME_ZONE_INFORMATION::default();
    unsafe {
        GetLocalTime(&mut value);
    }
    let time_zone_id = unsafe { GetTimeZoneInformation(&mut time_zone) };
    if time_zone_id == u32::MAX {
        return Err("the local time zone is unavailable".to_string());
    }
    let seasonal_bias = match time_zone_id {
        2 => time_zone.DaylightBias,
        0 | 1 => time_zone.StandardBias,
        _ => return Err("the local time zone is invalid".to_string()),
    };
    let timezone_offset_minutes = time_zone
        .Bias
        .checked_add(seasonal_bias)
        .and_then(i32::checked_neg)
        .ok_or_else(|| "the local time zone is invalid".to_string())?;
    format_local_export_time(
        value.wYear,
        value.wMonth,
        value.wDay,
        value.wHour,
        value.wMinute,
        value.wSecond,
        value.wMilliseconds,
        timezone_offset_minutes,
    )
}

#[cfg(not(windows))]
fn local_export_time_platform() -> Result<LocalExportTime, String> {
    Err("diagnostic export is only supported on Windows".to_string())
}

#[allow(clippy::too_many_arguments)]
fn format_local_export_time(
    year: u16,
    month: u16,
    day: u16,
    hour: u16,
    minute: u16,
    second: u16,
    milliseconds: u16,
    timezone_offset_minutes: i32,
) -> Result<LocalExportTime, String> {
    if year == 0
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
        || milliseconds > 999
        || timezone_offset_minutes.unsigned_abs() >= 24 * 60
    {
        return Err("the local clock is unavailable".to_string());
    }
    let offset_sign = if timezone_offset_minutes < 0 {
        '-'
    } else {
        '+'
    };
    let offset = timezone_offset_minutes.unsigned_abs();
    Ok(LocalExportTime {
        filename_timestamp: format!(
            "{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}-{milliseconds:03}"
        ),
        rfc3339: format!(
            "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{milliseconds:03}{offset_sign}{:02}:{:02}",
            offset / 60,
            offset % 60
        ),
        timezone_offset_minutes,
    })
}

#[cfg(windows)]
fn open_in_explorer(exported: &Path) -> Result<(), String> {
    use std::{ffi::OsString, process::Command};

    let mut selection = OsString::from("/select,");
    selection.push(exported.as_os_str());
    Command::new(windows_explorer_path()?)
        .arg(selection)
        .spawn()
        .map(|_| ())
        .map_err(|_| "failed to open the diagnostic archive location".to_string())
}

#[cfg(windows)]
fn open_directory_in_explorer(directory: &Path) -> Result<(), String> {
    use std::process::Command;

    Command::new(windows_explorer_path()?)
        .arg(directory)
        .spawn()
        .map(|_| ())
        .map_err(|_| "failed to open the diagnostic directory".to_string())
}

#[cfg(windows)]
fn windows_explorer_path() -> Result<PathBuf, String> {
    use std::{
        ffi::{OsStr, OsString},
        os::windows::{ffi::OsStringExt, fs::MetadataExt},
    };
    use windows_sys::Win32::{
        Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT,
        System::SystemInformation::GetWindowsDirectoryW,
    };

    let mut buffer = vec![0u16; 260];
    let windows_dir = loop {
        let length = unsafe { GetWindowsDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
        if length == 0 {
            return Err("the Windows directory is unavailable".to_string());
        }
        let length = length as usize;
        if length < buffer.len() {
            break PathBuf::from(OsString::from_wide(&buffer[..length]));
        }
        buffer.resize(length.saturating_add(1), 0);
    };
    let windows_dir = fs::canonicalize(windows_dir)
        .map_err(|_| "the Windows directory is unavailable".to_string())?;
    let explorer = windows_dir.join("explorer.exe");
    let metadata = fs::symlink_metadata(&explorer)
        .map_err(|_| "Windows Explorer is unavailable".to_string())?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err("Windows Explorer is unavailable".to_string());
    }
    let explorer =
        fs::canonicalize(explorer).map_err(|_| "Windows Explorer is unavailable".to_string())?;
    if explorer
        .parent()
        .is_none_or(|parent| !paths_equal(parent, &windows_dir))
        || explorer.file_name() != Some(OsStr::new("explorer.exe"))
    {
        return Err("Windows Explorer is unavailable".to_string());
    }
    Ok(explorer)
}

#[cfg(not(windows))]
fn open_in_explorer(_exported: &Path) -> Result<(), String> {
    Err("opening diagnostic exports is only supported on Windows".to_string())
}

#[cfg(not(windows))]
fn open_directory_in_explorer(_directory: &Path) -> Result<(), String> {
    Err("opening diagnostic directories is only supported on Windows".to_string())
}

#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn paths_equal(left: &Path, right: &Path) -> bool {
    left.as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{
        format_local_export_time, is_diagnostic_archive_name, open_directory_with,
        open_export_location_with, validate_downloads_dir, validate_export_file,
    };

    #[test]
    fn validates_generated_archive_names() {
        assert!(is_diagnostic_archive_name(
            "ChatGPT-Switch-Diagnostics-20260809-153012-004.zip"
        ));
        assert!(is_diagnostic_archive_name(
            "ChatGPT-Switch-Diagnostics-20260809-153012-004-2.zip"
        ));
        for unsafe_name in [
            "diagnostics.zip",
            "ChatGPT-Switch-Diagnostics-2026-08-09.zip",
            "ChatGPT-Switch-Diagnostics-20260809-153012-004-copy.zip",
            "ChatGPT-Switch-Diagnostics-20260809-153012-004.zip.exe",
        ] {
            assert!(!is_diagnostic_archive_name(unsafe_name), "{unsafe_name}");
        }
    }

    #[test]
    fn export_file_must_be_a_direct_downloads_child() {
        let root = tempdir().unwrap();
        let downloads = root.path().join("downloads");
        let nested = downloads.join("nested");
        fs::create_dir_all(&nested).unwrap();
        let filename = "ChatGPT-Switch-Diagnostics-20260809-153012-004.zip";
        let direct = downloads.join(filename);
        let nested_file = nested.join(filename);
        fs::write(&direct, b"zip").unwrap();
        fs::write(&nested_file, b"zip").unwrap();

        assert_eq!(
            validate_export_file(&downloads, &direct).unwrap(),
            direct.canonicalize().unwrap()
        );
        assert!(validate_export_file(&downloads, &nested_file).is_err());
    }

    #[test]
    fn opening_location_revalidates_before_calling_platform_opener() {
        let root = tempdir().unwrap();
        let downloads = root.path().join("downloads");
        fs::create_dir(&downloads).unwrap();
        let exported = downloads.join("ChatGPT-Switch-Diagnostics-20260809-153012-004.zip");
        fs::write(&exported, b"zip").unwrap();
        let expected = exported.canonicalize().unwrap();

        open_export_location_with(&downloads, &exported, |path| {
            assert_eq!(path, expected);
            Ok(())
        })
        .unwrap();

        let unrelated = root
            .path()
            .join("ChatGPT-Switch-Diagnostics-20260809-153012-004.zip");
        fs::write(&unrelated, b"zip").unwrap();
        assert!(open_export_location_with(&downloads, &unrelated, |_| {
            panic!("unsafe path reached the opener")
        })
        .is_err());
    }

    #[test]
    fn opening_directory_revalidates_before_calling_platform_opener() {
        let root = tempdir().unwrap();
        let directory = root.path().join("diagnostics");
        fs::create_dir(&directory).unwrap();
        let expected = directory.canonicalize().unwrap();

        open_directory_with(&directory, |path| {
            assert_eq!(path, expected);
            Ok(())
        })
        .unwrap();

        let file = root.path().join("not-a-directory");
        fs::write(&file, b"file").unwrap();
        assert!(open_directory_with(&file, |_| panic!("unsafe path reached the opener")).is_err());
    }

    #[test]
    fn formats_local_export_time_with_real_offset_semantics() {
        let positive = format_local_export_time(2026, 8, 9, 15, 30, 12, 4, 480).unwrap();
        assert_eq!(positive.filename_timestamp, "20260809-153012-004");
        assert_eq!(positive.rfc3339, "2026-08-09T15:30:12.004+08:00");
        assert_eq!(positive.timezone_offset_minutes, 480);

        let negative = format_local_export_time(2026, 1, 2, 3, 4, 5, 6, -210).unwrap();
        assert_eq!(negative.rfc3339, "2026-01-02T03:04:05.006-03:30");
        assert!(format_local_export_time(2026, 13, 2, 3, 4, 5, 6, 0).is_err());
    }

    #[test]
    fn downloads_validation_rejects_missing_and_regular_files() {
        let root = tempdir().unwrap();
        assert!(validate_downloads_dir(root.path().join("missing")).is_err());
        let file = root.path().join("file");
        fs::write(&file, b"not a directory").unwrap();
        assert!(validate_downloads_dir(file).is_err());
    }
}

//! Fixed macOS client discovery. Bundle metadata identifies candidates; a
//! destructive action or launch additionally verifies the OpenAI signature.
use std::{
    fs,
    io::Cursor,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use plist::Value;

use super::ManagedClientPackage;
use crate::session_storage::bounded_file::read_regular_file_bounded;

// These identities are also published by openai/codex, desktop_app/mac.rs.
// The unified ChatGPT app retained the Codex bundle ID. com.openai.chat is
// ChatGPT Classic and does not identify the managed Codex desktop runtime.
pub(crate) const BUNDLE_ID: &str = "com.openai.codex";
const APP_NAMES: [&str; 2] = ["ChatGPT.app", "Codex.app"];
const SIGNING_REQUIREMENT: &str = "identifier \"com.openai.codex\" and anchor apple generic and certificate leaf[subject.OU] = \"2DC432GLL2\"";
const MAX_PLIST_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileStamp {
    device: u64,
    inode: u64,
    size: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl FileStamp {
    fn new(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManagedApp {
    pub(crate) path: PathBuf,
    pub(crate) executable: PathBuf,
    pub(crate) version: Option<String>,
    stamps: Vec<FileStamp>,
}

pub(crate) fn discover_apps() -> Result<Vec<ManagedApp>, String> {
    discover_apps_at(&application_directories())
}

pub(crate) fn inspect_packages() -> Vec<ManagedClientPackage> {
    discover_apps()
        .unwrap_or_default()
        .into_iter()
        .map(|app| ManagedClientPackage {
            aumid: BUNDLE_ID.to_string(),
            package_name: app
                .path
                .file_stem()
                .and_then(|name| name.to_str())
                .map(str::to_string),
            package_family_name: Some(BUNDLE_ID.to_string()),
            version: app.version,
        })
        .collect()
}

fn application_directories() -> Vec<PathBuf> {
    let mut result = vec![PathBuf::from("/Applications")];
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        if home.is_absolute()
            && home.file_name().is_some()
            && !home
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            result.push(home.join("Applications"));
        }
    }
    result
}

fn discover_apps_at(directories: &[PathBuf]) -> Result<Vec<ManagedApp>, String> {
    let mut apps = Vec::new();
    for directory in directories {
        let canonical_directory = match fs::canonicalize(directory) {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err("the macOS applications directory is unavailable".to_string()),
        };
        // A user-created symlink must not turn discovery into an arbitrary-path
        // launcher. macOS system firmlinks resolve without this symlink escape.
        if &canonical_directory != directory {
            return Err("the macOS applications directory must not be a symlink".to_string());
        }
        for name in APP_NAMES {
            if let Some(app) = read_app(&canonical_directory.join(name))? {
                if !apps.iter().any(|other: &ManagedApp| other.path == app.path) {
                    apps.push(app);
                }
            }
        }
    }
    Ok(apps)
}

fn read_app(path: &Path) -> Result<Option<ManagedApp>, String> {
    if !APP_NAMES
        .iter()
        .any(|name| path.file_name().is_some_and(|leaf| leaf == *name))
    {
        return Err("the macOS application is outside the fixed client allowlist".to_string());
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("the macOS client bundle is unreadable".to_string()),
    };
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || fs::canonicalize(path).ok().as_deref() != Some(path)
    {
        return Err("the macOS client bundle is not a canonical directory".to_string());
    }
    let contents = path.join("Contents");
    let macos = contents.join("MacOS");
    let plist = contents.join("Info.plist");
    let contents_stamp = directory_stamp(&contents)?;
    let macos_stamp = directory_stamp(&macos)?;
    let plist_stamp = regular_stamp(&plist)?;
    let bytes = read_regular_file_bounded(&plist, MAX_PLIST_BYTES)
        .map_err(|_| "the macOS client metadata is invalid or changed while reading".to_string())?;
    let value = Value::from_reader(Cursor::new(bytes))
        .map_err(|_| "the macOS client metadata is not a valid property list".to_string())?;
    let dictionary = value
        .as_dictionary()
        .ok_or_else(|| "the macOS client metadata is not a dictionary".to_string())?;
    if dictionary
        .get("CFBundleIdentifier")
        .and_then(Value::as_string)
        != Some(BUNDLE_ID)
    {
        // In particular, do not manage or launch ChatGPT Classic.
        return Ok(None);
    }
    let executable_name = dictionary
        .get("CFBundleExecutable")
        .and_then(Value::as_string)
        .filter(|name| matches!(*name, "Codex" | "ChatGPT"))
        .ok_or_else(|| "the macOS client executable is outside the allowlist".to_string())?;
    let executable = macos.join(executable_name);
    let executable_stamp = regular_stamp(&executable)?;
    if fs::symlink_metadata(&executable)
        .map_err(|_| "the macOS client executable is unreadable".to_string())?
        .mode()
        & 0o111
        == 0
    {
        return Err("the macOS client executable has no execute permission".to_string());
    }
    let version = dictionary
        .get("CFBundleShortVersionString")
        .and_then(Value::as_string)
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 96
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
        })
        .map(str::to_string);
    let stamps = vec![
        FileStamp::new(&metadata),
        contents_stamp,
        macos_stamp,
        plist_stamp,
        executable_stamp,
    ];
    let app = ManagedApp {
        path: path.to_path_buf(),
        executable,
        version,
        stamps,
    };
    if collect_stamps(&app)? != app.stamps {
        return Err("the macOS client bundle changed during inspection".to_string());
    }
    Ok(Some(app))
}

fn directory_stamp(path: &Path) -> Result<FileStamp, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| "the macOS client bundle directory is unreadable".to_string())?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || fs::canonicalize(path).ok().as_deref() != Some(path)
    {
        return Err("the macOS client bundle contains a redirected directory".to_string());
    }
    Ok(FileStamp::new(&metadata))
}

fn regular_stamp(path: &Path) -> Result<FileStamp, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| "the macOS client bundle file is unreadable".to_string())?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || fs::canonicalize(path).ok().as_deref() != Some(path)
    {
        return Err("the macOS client bundle contains a redirected file".to_string());
    }
    Ok(FileStamp::new(&metadata))
}

fn collect_stamps(app: &ManagedApp) -> Result<Vec<FileStamp>, String> {
    Ok(vec![
        directory_stamp(&app.path)?,
        directory_stamp(&app.path.join("Contents"))?,
        directory_stamp(&app.path.join("Contents/MacOS"))?,
        regular_stamp(&app.path.join("Contents/Info.plist"))?,
        regular_stamp(&app.executable)?,
    ])
}

pub(crate) fn revalidate_app(app: &ManagedApp) -> Result<(), String> {
    if read_app(&app.path)?.as_ref() != Some(app) {
        return Err(
            "the selected macOS client changed; open the intended client again".to_string(),
        );
    }
    Ok(())
}

pub(crate) fn verify_openai_signature(app: &ManagedApp) -> Result<(), String> {
    revalidate_app(app)?;
    let mut command = Command::new("/usr/bin/codesign");
    command
        .args(["--verify", "--deep", "--strict"])
        .arg(format!("-R={SIGNING_REQUIREMENT}"))
        .arg(&app.path);
    run_bounded_command(&mut command, Duration::from_secs(30))
        .map_err(|_| "the macOS client did not pass OpenAI signature verification".to_string())?;
    revalidate_app(app)
}

pub(crate) fn open_app(app: &ManagedApp) -> Result<(), String> {
    verify_openai_signature(app)?;
    let mut command = Command::new("/usr/bin/open");
    command.arg("-a").arg(&app.path);
    run_bounded_command(&mut command, Duration::from_secs(10))
        .map_err(|_| "macOS did not accept the managed client launch request".to_string())
}

// Only fixed system tools call this helper. Their output can contain local paths,
// so it is discarded rather than copied into errors, logs or command receipts.
fn run_bounded_command(command: &mut Command, timeout: Duration) -> Result<(), ()> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| ())?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success().then_some(()).ok_or(()),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            Ok(None) | Err(_) => {
                // This is exclusively the system-tool child created above.
                let _ = child.kill();
                let _ = child.wait();
                return Err(());
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn fixture_app(root: &Path, name: &str, bundle_id: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let app = root.join(name);
    fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
    let executable = if name == "Codex.app" {
        "Codex"
    } else {
        "ChatGPT"
    };
    let mut dictionary = plist::Dictionary::new();
    dictionary.insert(
        "CFBundleIdentifier".to_string(),
        Value::String(bundle_id.to_string()),
    );
    dictionary.insert(
        "CFBundleExecutable".to_string(),
        Value::String(executable.to_string()),
    );
    dictionary.insert(
        "CFBundleShortVersionString".to_string(),
        Value::String("26.810.1".to_string()),
    );
    Value::Dictionary(dictionary)
        .to_file_xml(app.join("Contents/Info.plist"))
        .unwrap();
    fs::write(
        app.join("Contents/MacOS").join(executable),
        b"fixture executable",
    )
    .unwrap();
    fs::set_permissions(
        app.join("Contents/MacOS").join(executable),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    app
}

#[cfg(test)]
pub(crate) fn fixture_metadata(root: &Path, name: &str) -> ManagedApp {
    let path = fixture_app(root, name, BUNDLE_ID);
    read_app(&path).unwrap().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tempfile::tempdir;

    #[test]
    fn discovers_both_current_and_legacy_filenames_with_the_codex_identity() {
        let temporary = tempdir().unwrap();
        let root = fs::canonicalize(temporary.path()).unwrap();
        let chatgpt = fixture_app(&root, "ChatGPT.app", BUNDLE_ID);
        let codex = fixture_app(&root, "Codex.app", BUNDLE_ID);
        let result = discover_apps_at(std::slice::from_ref(&root)).unwrap();
        assert_eq!(
            result.iter().map(|app| &app.path).collect::<Vec<_>>(),
            [&chatgpt, &codex]
        );
    }

    #[test]
    fn ignores_chatgpt_classic_instead_of_guessing_from_the_filename() {
        let temporary = tempdir().unwrap();
        let root = fs::canonicalize(temporary.path()).unwrap();
        fixture_app(&root, "ChatGPT.app", "com.openai.chat");
        assert!(discover_apps_at(&[root]).unwrap().is_empty());
    }

    #[test]
    fn rejects_symlinked_application_and_executable() {
        let temporary = tempdir().unwrap();
        let root = fs::canonicalize(temporary.path()).unwrap();
        let other = root.join("other");
        fs::create_dir(&other).unwrap();
        let app = fixture_app(&other, "Codex.app", BUNDLE_ID);
        symlink(&app, root.join("Codex.app")).unwrap();
        assert!(discover_apps_at(std::slice::from_ref(&root)).is_err());
        fs::remove_file(root.join("Codex.app")).unwrap();
        let app = fixture_app(&root, "Codex.app", BUNDLE_ID);
        let executable = app.join("Contents/MacOS/Codex");
        fs::remove_file(&executable).unwrap();
        symlink("/usr/bin/true", executable).unwrap();
        assert!(discover_apps_at(&[root]).is_err());
    }

    #[test]
    fn rejects_metadata_path_traversal() {
        let temporary = tempdir().unwrap();
        let root = fs::canonicalize(temporary.path()).unwrap();
        let app = fixture_app(&root, "Codex.app", BUNDLE_ID);
        let metadata = app.join("Contents/Info.plist");
        let text = fs::read_to_string(&metadata).unwrap();
        fs::write(
            &metadata,
            text.replace("<string>Codex</string>", "<string>../../bin/sh</string>"),
        )
        .unwrap();
        assert!(discover_apps_at(&[root]).is_err());
    }

    #[test]
    fn refuses_changed_and_unsigned_clients_before_launch() {
        let temporary = tempdir().unwrap();
        let root = fs::canonicalize(temporary.path()).unwrap();
        let app = fixture_metadata(&root, "Codex.app");
        assert!(verify_openai_signature(&app).is_err());
        fs::write(&app.executable, b"changed fixture executable").unwrap();
        assert!(revalidate_app(&app).is_err());
    }

    #[test]
    fn bounded_system_child_is_reaped_on_timeout() {
        let mut command = Command::new("/bin/sleep");
        command.arg("5");
        let started = Instant::now();
        assert!(run_bounded_command(&mut command, Duration::from_millis(40)).is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}

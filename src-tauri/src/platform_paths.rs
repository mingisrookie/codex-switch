//! User data locations shared by the desktop's platform services.
use std::{ffi::OsString, path::PathBuf};

/// The base containing `codex-switch/`. An explicit absolute override is useful
/// for isolated native validation; the normal macOS location is Application Support.
pub fn appdata_root() -> Result<PathBuf, String> {
    resolve_appdata_root(
        std::env::consts::OS,
        std::env::var_os("CODEX_SWITCH_DATA_HOME"),
        std::env::var_os("APPDATA"),
        std::env::var_os("HOME"),
    )
}

fn resolve_appdata_root(
    platform: &str,
    isolated: Option<OsString>,
    appdata: Option<OsString>,
    home: Option<OsString>,
) -> Result<PathBuf, String> {
    let root = if let Some(value) = isolated {
        PathBuf::from(value)
    } else if platform == "macos" {
        PathBuf::from(home.ok_or_else(|| "HOME is not set".to_string())?)
            .join("Library")
            .join("Application Support")
    } else {
        PathBuf::from(appdata.ok_or_else(|| "APPDATA is not set".to_string())?)
    };
    crate::codex_paths::validate_absolute_root(&root, "application data directory")
}

pub fn store_root() -> Result<PathBuf, String> {
    Ok(appdata_root()?.join("codex-switch"))
}

#[cfg(target_os = "macos")]
pub(crate) use macos_private::{
    ensure_private_directory, open_private_lock_file, try_acquire_private_lock_file,
};

#[cfg(target_os = "macos")]
mod macos_private {
    use std::{
        ffi::{CStr, CString, OsStr},
        fs::{self, File, OpenOptions},
        io,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::{
                ffi::OsStrExt,
                fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
            },
        },
        path::{Component, Path},
    };

    /// Create missing private directories with 0700 and secure only the leaf.
    /// Ancestors such as HOME and Library retain their existing permissions.
    pub(crate) fn ensure_private_directory(path: &Path) -> Result<File, String> {
        crate::codex_paths::validate_absolute_root(path, "private application directory")?;
        // Validate the complete existing chain before any mkdir or chmod.
        for ancestor in path.ancestors() {
            match fs::symlink_metadata(ancestor) {
                Ok(metadata) if !metadata.is_dir() || metadata.file_type().is_symlink() => {
                    return Err("private application directory ancestors must be directories without symlinks".into());
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(_) => return Err("failed to inspect private application directory".into()),
            }
        }

        let mut directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open("/")
            .map_err(|_| "failed to open the application directory root".to_string())?;
        // Walk with directory handles, so replacing a pathname with a symlink
        // after the validation above cannot redirect creation or fchmod.
        for component in path.components() {
            let Component::Normal(name) = component else {
                continue;
            };
            let name = CString::new(name.as_bytes()).map_err(|_| {
                "private application directory contains an invalid name".to_string()
            })?;
            directory = match open_directory_at(&directory, &name) {
                Ok(next) => next,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    // The parent handle and NUL-terminated component remain valid for this call.
                    let result =
                        unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) };
                    if result != 0
                        && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists
                    {
                        return Err("failed to create a private application directory".into());
                    }
                    open_directory_at(&directory, &name).map_err(|_| {
                        "failed to safely open a private application directory".to_string()
                    })?
                }
                Err(_) => return Err(
                    "private application directory ancestors must be directories without symlinks"
                        .into(),
                ),
            };
        }
        let metadata = directory
            .metadata()
            .map_err(|_| "failed to inspect the private application directory".to_string())?;
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err("private application directory must belong to the current user".into());
        }
        directory
            .set_permissions(fs::Permissions::from_mode(0o700))
            .map_err(|_| "failed to secure the private application directory".to_string())?;
        Ok(directory)
    }

    fn open_directory_at(parent: &File, name: &CStr) -> io::Result<File> {
        // openat returns an owned descriptor; O_NOFOLLOW applies to this single component.
        let descriptor = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if descriptor < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { File::from_raw_fd(descriptor) })
    }

    /// Open a regular, singly linked lock file relative to an already safe directory.
    /// Existing contents are preserved; only the owned file's permissions are secured.
    pub(crate) fn open_private_lock_file(directory: &File, name: &OsStr) -> Result<File, String> {
        open_private_lock_file_with_flags(directory, name, 0).map_err(|error| error.to_string())
    }

    /// Acquire a Darwin advisory lock as part of opening the file. A contender
    /// receives WouldBlock before inspecting or changing the lock file's mode.
    pub(crate) fn try_acquire_private_lock_file(
        directory: &File,
        name: &OsStr,
    ) -> io::Result<File> {
        open_private_lock_file_with_flags(directory, name, libc::O_EXLOCK)
    }

    fn open_private_lock_file_with_flags(
        directory: &File,
        name: &OsStr,
        lock_flags: libc::c_int,
    ) -> io::Result<File> {
        let mut components = Path::new(name).components();
        if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "private lock file must have a single filename",
            ));
        }
        let name = CString::new(name.as_bytes()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "private lock file has an invalid name",
            )
        })?;
        // O_NONBLOCK bounds both advisory-lock contention and substituted FIFOs.
        // O_EXLOCK, when requested, holds the lock throughout fstat and fchmod.
        let flags =
            libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK | lock_flags;
        // Give simultaneous first creators one exclusive winner. An existing
        // lock is opened separately, without permission to recreate it if its
        // directory entry disappears between these calls.
        let mut descriptor = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_CREAT | libc::O_EXCL,
                0o600,
            )
        };
        if descriptor < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::AlreadyExists {
                return Err(io::Error::new(
                    error.kind(),
                    format!("exclusive private lock creation failed: {error}"),
                ));
            }
            descriptor = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
            if descriptor < 0 {
                let error = io::Error::last_os_error();
                return Err(io::Error::new(
                    error.kind(),
                    format!("existing private lock open failed: {error}"),
                ));
            }
        }
        // The descriptor is owned by this call and transferred exactly once.
        let file = unsafe { File::from_raw_fd(descriptor) };
        let metadata = file.metadata().map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("failed to inspect private lock file: {error}"),
            )
        })?;
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.uid() != unsafe { libc::geteuid() }
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "private lock file must be a regular file owned only by the current user",
            ));
        }
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("failed to secure private lock file: {error}"),
                )
            })?;
        Ok(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_relative_data_override() {
        assert!(resolve_appdata_root("macos", Some("relative".into()), None, None).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn macos_uses_application_support_without_appdata() {
        assert_eq!(
            resolve_appdata_root(
                "macos",
                None,
                Some("/ignored".into()),
                Some("/Users/example".into())
            )
            .unwrap(),
            PathBuf::from("/Users/example/Library/Application Support")
        );
    }

    #[test]
    fn explicit_data_home_is_isolated() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve_appdata_root(
                "macos",
                Some(root.path().as_os_str().to_owned()),
                None,
                None
            )
            .unwrap(),
            root.path()
        );
    }
    #[test]
    fn data_override_rejects_filesystem_roots_and_parent_components() {
        let temp = tempfile::tempdir().unwrap();
        let filesystem_root = temp.path().ancestors().last().unwrap();
        for invalid in [
            filesystem_root.to_path_buf(),
            temp.path().join("child/../escape"),
        ] {
            assert!(
                resolve_appdata_root("macos", Some(invalid.into_os_string()), None, None).is_err()
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_private_directories_only_secure_owned_leaf_and_new_directories() {
        use std::{fs, os::unix::fs::PermissionsExt};
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().canonicalize().unwrap().join("home");
        fs::create_dir(&home).unwrap();
        fs::set_permissions(&home, fs::Permissions::from_mode(0o755)).unwrap();
        let leaf = home.join("codex-switch/logs");
        let directory = ensure_private_directory(&leaf).unwrap();
        assert_eq!(
            directory.metadata().unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(home.join("codex-switch"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&home).unwrap().permissions().mode() & 0o777,
            0o755
        );
        fs::set_permissions(&leaf, fs::Permissions::from_mode(0o755)).unwrap();
        ensure_private_directory(&leaf).unwrap();
        assert_eq!(
            fs::metadata(&leaf).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_private_directories_reject_symlink_ancestors_without_chmod_or_creation() {
        use std::{
            fs,
            os::unix::fs::{symlink, PermissionsExt},
        };
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let target = root.join("unrelated");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        let link = root.join("linked");
        symlink(&target, &link).unwrap();
        assert!(ensure_private_directory(&link).is_err());
        assert!(ensure_private_directory(&link.join("must-not-be-created")).is_err());
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_private_lock_files_keep_data_and_reject_symlinks_and_hardlinks() {
        use std::{
            ffi::OsStr,
            fs,
            io::Write,
            os::unix::fs::{symlink, PermissionsExt},
        };
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let directory = ensure_private_directory(&root.join("app")).unwrap();
        let mut lock = open_private_lock_file(&directory, OsStr::new("mutation.lock")).unwrap();
        lock.write_all(b"keep fixture contents").unwrap();
        assert_eq!(lock.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        drop(lock);
        let reopened = open_private_lock_file(&directory, OsStr::new("mutation.lock")).unwrap();
        drop(reopened);
        assert_eq!(
            fs::read(root.join("app/mutation.lock")).unwrap(),
            b"keep fixture contents"
        );
        let target = root.join("unrelated.txt");
        fs::write(&target, b"unrelated fixture").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        symlink(&target, root.join("app/symlink.lock")).unwrap();
        fs::hard_link(&target, root.join("app/hardlink.lock")).unwrap();
        assert!(open_private_lock_file(&directory, OsStr::new("symlink.lock")).is_err());
        assert!(open_private_lock_file(&directory, OsStr::new("hardlink.lock")).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"unrelated fixture");
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert!(open_private_lock_file(&directory, OsStr::new("../outside.lock")).is_err());
        assert!(!root.join("outside.lock").exists());
    }
}

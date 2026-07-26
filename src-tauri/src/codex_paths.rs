use std::{
    env,
    ffi::OsString,
    fs,
    path::{Component, Path, PathBuf},
    str::FromStr,
};

use toml_edit::DocumentMut;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexPaths {
    pub codex_home: PathBuf,
    pub sqlite_home: PathBuf,
    pub state_db: PathBuf,
    pub logs_db: PathBuf,
    pub goals_db: PathBuf,
    pub memories_db: PathBuf,
    pub sessions_dir: PathBuf,
    pub archived_sessions_dir: PathBuf,
    pub session_index: PathBuf,
}

pub fn resolve_user_codex_paths(codex_home: &Path) -> Result<CodexPaths, String> {
    let codex_home = validate_absolute_root(codex_home, "CODEX_HOME")?;
    let sqlite_home = resolve_sqlite_home(
        &codex_home,
        read_config_sqlite_home(&codex_home)?,
        env::var_os("CODEX_SQLITE_HOME"),
    )?;
    Ok(build_paths(&codex_home, &sqlite_home))
}

pub fn local_codex_paths(codex_home: &Path) -> CodexPaths {
    build_paths(codex_home, codex_home)
}

fn build_paths(codex_home: &Path, sqlite_home: &Path) -> CodexPaths {
    CodexPaths {
        codex_home: codex_home.to_path_buf(),
        sqlite_home: sqlite_home.to_path_buf(),
        state_db: sqlite_home.join("state_5.sqlite"),
        logs_db: sqlite_home.join("logs_2.sqlite"),
        goals_db: sqlite_home.join("goals_1.sqlite"),
        memories_db: sqlite_home.join("memories_1.sqlite"),
        sessions_dir: codex_home.join("sessions"),
        archived_sessions_dir: codex_home.join("archived_sessions"),
        session_index: codex_home.join("session_index.jsonl"),
    }
}

fn read_config_sqlite_home(codex_home: &Path) -> Result<Option<PathBuf>, String> {
    let raw = match fs::read_to_string(codex_home.join("config.toml")) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("failed to read config.toml: {error}")),
    };
    let doc = DocumentMut::from_str(&raw)
        .map_err(|error| format!("failed to parse config.toml: {error}"))?;
    let Some(item) = doc.get("sqlite_home") else {
        return Ok(None);
    };
    let value = item
        .as_str()
        .ok_or_else(|| "config.toml sqlite_home must be a string".to_string())?
        .trim();
    if value.is_empty() {
        Ok(None)
    } else {
        Ok(Some(PathBuf::from(value)))
    }
}

fn resolve_sqlite_home(
    codex_home: &Path,
    configured: Option<PathBuf>,
    env_value: Option<OsString>,
) -> Result<PathBuf, String> {
    if let Some(path) = configured {
        return validate_absolute_root(&path, "config.toml sqlite_home");
    }
    if let Some(raw) = env_value {
        let text = raw.to_string_lossy();
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            return validate_absolute_root(&PathBuf::from(trimmed), "CODEX_SQLITE_HOME");
        }
    }
    Ok(codex_home.to_path_buf())
}

pub fn validate_absolute_root(path: &Path, label: &str) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!("{label} must be an absolute path"));
    }
    if path.file_name().is_none() {
        return Err(format!("{label} must not be a filesystem root"));
    }
    if path
        .components()
        .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(format!("{label} must not contain relative path components"));
    }
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use std::path::Path;
    use tempfile::tempdir;

    use super::{
        local_codex_paths, resolve_sqlite_home, resolve_user_codex_paths, validate_absolute_root,
    };

    #[test]
    fn config_sqlite_home_overrides_codex_home() {
        let home = tempdir().unwrap();
        let sqlite_home = tempdir().unwrap();
        std::fs::write(
            home.path().join("config.toml"),
            format!("sqlite_home = \"{}\"\n", sqlite_home.path().display()).replace('\\', "\\\\"),
        )
        .unwrap();

        let paths = resolve_user_codex_paths(home.path()).unwrap();

        assert_eq!(paths.sqlite_home, sqlite_home.path());
        assert_eq!(paths.state_db, sqlite_home.path().join("state_5.sqlite"));
        assert_eq!(
            paths.archived_sessions_dir,
            home.path().join("archived_sessions")
        );
    }

    #[test]
    fn env_sqlite_home_relative_path_is_rejected() {
        let home = tempdir().unwrap();

        let error = resolve_sqlite_home(home.path(), None, Some(OsString::from("sqlite-state")))
            .unwrap_err();

        assert!(error.contains("absolute"));
    }

    #[test]
    fn local_paths_ignore_external_sqlite_home() {
        let home = tempdir().unwrap();

        let paths = local_codex_paths(home.path());

        assert_eq!(paths.sqlite_home, home.path());
        assert_eq!(paths.state_db, home.path().join("state_5.sqlite"));
        assert_eq!(paths.memories_db, home.path().join("memories_1.sqlite"));
        assert_eq!(
            paths.archived_sessions_dir,
            home.path().join("archived_sessions")
        );
    }

    #[test]
    fn invalid_config_is_rejected_instead_of_falling_back_to_the_wrong_database() {
        let home = tempdir().unwrap();
        std::fs::write(home.path().join("config.toml"), "sqlite_home = [broken\n").unwrap();

        let error = resolve_user_codex_paths(home.path()).unwrap_err();

        assert!(error.contains("config.toml"));
    }

    #[test]
    fn relative_codex_home_is_rejected() {
        let error = resolve_user_codex_paths(Path::new(".codex")).unwrap_err();

        assert!(error.contains("CODEX_HOME"));
        assert!(error.contains("absolute"));
    }

    #[test]
    fn parent_components_are_rejected() {
        let error =
            validate_absolute_root(Path::new(r"C:\Users\name\..\other"), "root").unwrap_err();

        assert!(error.contains("relative path components"));
    }

    #[test]
    fn filesystem_root_is_rejected() {
        let root = tempdir()
            .unwrap()
            .path()
            .ancestors()
            .last()
            .unwrap()
            .to_path_buf();

        let error = validate_absolute_root(&root, "root").unwrap_err();

        assert!(error.contains("filesystem root"));
    }
}

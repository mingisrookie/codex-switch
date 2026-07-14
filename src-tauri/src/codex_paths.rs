use std::{
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
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
    pub sessions_dir: PathBuf,
    pub session_index: PathBuf,
}

pub fn resolve_user_codex_paths(codex_home: &Path) -> Result<CodexPaths, String> {
    let cwd = env::current_dir().unwrap_or_else(|_| codex_home.to_path_buf());
    let sqlite_home = resolve_sqlite_home(
        codex_home,
        read_config_sqlite_home(codex_home)?,
        env::var_os("CODEX_SQLITE_HOME"),
        &cwd,
    );
    Ok(build_paths(codex_home, &sqlite_home))
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
        sessions_dir: codex_home.join("sessions"),
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
    cwd: &Path,
) -> PathBuf {
    if let Some(path) = configured {
        return absolutize(path, cwd);
    }
    if let Some(raw) = env_value {
        let text = raw.to_string_lossy();
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            return absolutize(PathBuf::from(trimmed), cwd);
        }
    }
    codex_home.to_path_buf()
}

fn absolutize(path: PathBuf, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use tempfile::tempdir;

    use super::{local_codex_paths, resolve_sqlite_home, resolve_user_codex_paths};

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
    }

    #[test]
    fn env_sqlite_home_relative_path_resolves_from_cwd() {
        let home = tempdir().unwrap();
        let cwd = tempdir().unwrap();

        let resolved = resolve_sqlite_home(
            home.path(),
            None,
            Some(OsString::from("sqlite-state")),
            cwd.path(),
        );

        assert_eq!(resolved, cwd.path().join("sqlite-state"));
    }

    #[test]
    fn local_paths_ignore_external_sqlite_home() {
        let home = tempdir().unwrap();

        let paths = local_codex_paths(home.path());

        assert_eq!(paths.sqlite_home, home.path());
        assert_eq!(paths.state_db, home.path().join("state_5.sqlite"));
    }

    #[test]
    fn invalid_config_is_rejected_instead_of_falling_back_to_the_wrong_database() {
        let home = tempdir().unwrap();
        std::fs::write(home.path().join("config.toml"), "sqlite_home = [broken\n").unwrap();

        let error = resolve_user_codex_paths(home.path()).unwrap_err();

        assert!(error.contains("config.toml"));
    }
}

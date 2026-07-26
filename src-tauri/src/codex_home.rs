use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::Serialize;
use serde_json::Value;

use crate::codex_paths::resolve_user_codex_paths;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FileStatus {
    pub path: PathBuf,
    pub exists: bool,
    pub bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AuthSummary {
    pub auth_mode: Option<String>,
    pub top_level_keys: Vec<String>,
    pub has_tokens_object: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexHomeStatus {
    pub root: PathBuf,
    pub sqlite_home: PathBuf,
    pub auth_json: FileStatus,
    pub config_toml: FileStatus,
    pub state_db: FileStatus,
    pub logs_db: FileStatus,
    pub codex_dev_db: FileStatus,
    pub sessions_dir: FileStatus,
    pub auth_summary: Option<AuthSummary>,
}

pub fn scan_codex_home(home: &Path) -> Result<CodexHomeStatus, String> {
    scan_codex_home_with_file_status(home, file_status)
}

fn scan_codex_home_with_file_status<F>(
    home: &Path,
    mut status_for: F,
) -> Result<CodexHomeStatus, String>
where
    F: FnMut(&Path) -> FileStatus,
{
    let paths = resolve_user_codex_paths(home)?;
    let auth_path = home.join("auth.json");
    let config_path = home.join("config.toml");
    let sessions_path = &paths.sessions_dir;
    let auth_json = status_for(&auth_path);

    let auth_summary = if auth_json.exists {
        Some(summarize_auth(&auth_path)?)
    } else {
        None
    };

    Ok(CodexHomeStatus {
        root: home.to_path_buf(),
        sqlite_home: paths.sqlite_home,
        auth_json,
        config_toml: status_for(&config_path),
        state_db: status_for(&paths.state_db),
        logs_db: status_for(&paths.logs_db),
        codex_dev_db: status_for(&home.join("sqlite").join("codex-dev.db")),
        sessions_dir: status_for(sessions_path),
        auth_summary,
    })
}

pub fn summarize_auth(path: &Path) -> Result<AuthSummary, String> {
    let raw =
        fs::read_to_string(path).map_err(|error| format!("failed to read auth.json: {error}"))?;
    let value: Value = serde_json::from_str(&raw)
        .map_err(|error| format!("failed to parse auth.json: {error}"))?;

    let Some(object) = value.as_object() else {
        return Err("auth.json must be a JSON object".to_string());
    };

    let mut top_level_keys = object.keys().cloned().collect::<Vec<_>>();
    top_level_keys.sort();

    Ok(AuthSummary {
        auth_mode: object
            .get("auth_mode")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        has_tokens_object: object.get("tokens").is_some_and(Value::is_object),
        top_level_keys,
    })
}

fn file_status(path: &Path) -> FileStatus {
    let metadata = fs::metadata(path).ok();
    FileStatus {
        path: path.to_path_buf(),
        exists: metadata.is_some(),
        bytes: metadata.map(|item| item.len()),
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, fs};

    use tempfile::tempdir;

    use super::{file_status, scan_codex_home, scan_codex_home_with_file_status, summarize_auth};

    #[test]
    fn scans_expected_codex_home_files_without_reading_secret_values() {
        let temp = tempdir().unwrap();
        let home = temp.path();
        fs::write(
            home.join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"fake-access"}}"#,
        )
        .unwrap();
        fs::write(
            home.join("config.toml"),
            r#"model = "gpt-5.5"
model_instructions_file = "C:\\Users\\alice\\.codex\\instruction.md"
"#,
        )
        .unwrap();
        fs::write(home.join("state_5.sqlite"), b"not a real sqlite").unwrap();
        fs::create_dir_all(home.join("sessions/2026/06/23")).unwrap();
        fs::write(home.join("sessions/2026/06/23/rollout.jsonl"), "{}\n").unwrap();

        let status = scan_codex_home(home).unwrap();

        assert!(status.auth_json.exists);
        assert!(status.config_toml.exists);
        assert!(status.state_db.exists);
        assert_eq!(status.sqlite_home, home);
        assert_eq!(
            status.auth_summary.unwrap().auth_mode.as_deref(),
            Some("chatgpt")
        );
    }

    #[test]
    fn scans_only_sessions_root_metadata_for_a_deep_tree() {
        let temp = tempdir().unwrap();
        let home = temp.path();
        let sessions = home.join("sessions");
        fs::create_dir(&sessions).unwrap();

        let mut level = sessions.clone();
        for depth in 0..24 {
            level.push(format!("d{depth:02}"));
            fs::create_dir(&level).unwrap();
            for index in 0..4 {
                // Any recursive text reader would fail on these rollout fixtures.
                fs::write(
                    level.join(format!("rollout-{index}.jsonl")),
                    [0xff, 0xfe, 0xfd],
                )
                .unwrap();
            }
        }

        let status = scan_codex_home(home).unwrap();
        let probed_paths = RefCell::new(Vec::new());
        let probed_status = scan_codex_home_with_file_status(home, |path| {
            probed_paths.borrow_mut().push(path.to_path_buf());
            file_status(path)
        })
        .unwrap();

        assert_eq!(probed_status, status);
        assert!(level.join("rollout-3.jsonl").is_file());
        assert_eq!(status.sessions_dir.path, sessions);
        assert!(status.sessions_dir.exists);
        assert_eq!(
            probed_paths
                .into_inner()
                .into_iter()
                .filter(|path| path.starts_with(&status.sessions_dir.path))
                .collect::<Vec<_>>(),
            vec![status.sessions_dir.path]
        );
    }

    #[test]
    fn auth_summary_reports_structure_only() {
        let temp = tempdir().unwrap();
        let auth = temp.path().join("auth.json");
        fs::write(
            &auth,
            r#"{"auth_mode":"apikey","OPENAI_API_KEY":"fake-key","tokens":{"access_token":"fake-access"}}"#,
        )
        .unwrap();

        let summary = summarize_auth(&auth).unwrap();

        assert_eq!(summary.auth_mode.as_deref(), Some("apikey"));
        assert!(summary
            .top_level_keys
            .contains(&"OPENAI_API_KEY".to_string()));
        assert!(summary.has_tokens_object);
    }
}

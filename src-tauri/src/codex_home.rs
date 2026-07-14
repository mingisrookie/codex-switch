use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::Serialize;
use serde_json::Value;

use crate::{codex_paths::resolve_user_codex_paths, file_ops::walk_jsonl_files};

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
    pub session_jsonl_count: usize,
    pub auth_summary: Option<AuthSummary>,
}

pub fn scan_codex_home(home: &Path) -> Result<CodexHomeStatus, String> {
    let paths = resolve_user_codex_paths(home)?;
    let auth_path = home.join("auth.json");
    let config_path = home.join("config.toml");
    let sessions_path = &paths.sessions_dir;

    let auth_summary = if auth_path.exists() {
        Some(summarize_auth(&auth_path)?)
    } else {
        None
    };

    Ok(CodexHomeStatus {
        root: home.to_path_buf(),
        sqlite_home: paths.sqlite_home,
        auth_json: file_status(&auth_path),
        config_toml: file_status(&config_path),
        state_db: file_status(&paths.state_db),
        logs_db: file_status(&paths.logs_db),
        codex_dev_db: file_status(&home.join("sqlite").join("codex-dev.db")),
        sessions_dir: file_status(sessions_path),
        session_jsonl_count: count_session_jsonl(sessions_path)?,
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

fn count_session_jsonl(sessions_path: &Path) -> Result<usize, String> {
    if !sessions_path.exists() {
        return Ok(0);
    }
    Ok(walk_jsonl_files(sessions_path)?.len())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{scan_codex_home, summarize_auth};

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
        assert_eq!(status.session_jsonl_count, 1);
        assert_eq!(
            status.auth_summary.unwrap().auth_mode.as_deref(),
            Some("chatgpt")
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

use reqwest::{
    blocking::{Client, Response},
    header::{ACCEPT, USER_AGENT},
    redirect::Policy,
    StatusCode,
};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::{
    io::Read,
    sync::{Mutex, MutexGuard, TryLockError},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tauri_plugin_opener::OpenerExt;

const LATEST_RELEASE_API: &str =
    "https://api.github.com/repos/mingisrookie/codex-switch/releases/latest";
const RELEASE_PAGE_URL: &str = "https://github.com/mingisrookie/codex-switch/releases/latest";
const MAX_RELEASE_BYTES: usize = 256 * 1024;
const MAX_NOTES_CHARS: usize = 800;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);

static UPDATE_CHECK_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCheckResult {
    pub current_version: String,
    pub latest_version: String,
    pub update_available: bool,
    pub release_notes: Option<String>,
    pub checked_at_ms: u64,
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    body: Option<String>,
}

pub fn check_latest_release() -> Result<UpdateCheckResult, String> {
    let _guard = acquire_update_check()?;
    let client = Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .redirect(Policy::none())
        .build()
        .map_err(|_| "failed to initialize update checker".to_string())?;
    let response = client
        .get(LATEST_RELEASE_API)
        .header(ACCEPT, "application/vnd.github+json")
        .header(
            USER_AGENT,
            format!("codex-switch/{}", env!("CARGO_PKG_VERSION")),
        )
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .map_err(|_| "failed to reach GitHub while checking for updates".to_string())?;
    let payload = read_release_response(response)?;
    evaluate_release_payload(&payload, env!("CARGO_PKG_VERSION"), timestamp_millis())
}

pub fn open_release_page(app: &tauri::AppHandle) -> Result<(), String> {
    app.opener()
        .open_url(RELEASE_PAGE_URL, None::<&str>)
        .map_err(|_| "failed to open the update download page".to_string())
}

fn acquire_update_check() -> Result<MutexGuard<'static, ()>, String> {
    match UPDATE_CHECK_LOCK.try_lock() {
        Ok(guard) => Ok(guard),
        Err(TryLockError::WouldBlock) => Err("an update check is already in progress".to_string()),
        Err(TryLockError::Poisoned(_)) => Err("update checker is unavailable".to_string()),
    }
}

fn read_release_response(mut response: Response) -> Result<Vec<u8>, String> {
    if response.status() == StatusCode::FORBIDDEN
        || response.status() == StatusCode::TOO_MANY_REQUESTS
    {
        return Err("GitHub update checks are temporarily rate limited".to_string());
    }
    if !response.status().is_success() {
        return Err(format!(
            "GitHub update check returned HTTP {}",
            response.status().as_u16()
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RELEASE_BYTES as u64)
    {
        return Err("GitHub release metadata is too large".to_string());
    }

    let mut payload = Vec::new();
    response
        .by_ref()
        .take((MAX_RELEASE_BYTES + 1) as u64)
        .read_to_end(&mut payload)
        .map_err(|_| "failed to read GitHub release metadata".to_string())?;
    if payload.len() > MAX_RELEASE_BYTES {
        return Err("GitHub release metadata is too large".to_string());
    }
    Ok(payload)
}

fn evaluate_release_payload(
    payload: &[u8],
    current_version: &str,
    checked_at_ms: u64,
) -> Result<UpdateCheckResult, String> {
    if payload.len() > MAX_RELEASE_BYTES {
        return Err("GitHub release metadata is too large".to_string());
    }
    let release: GithubRelease = serde_json::from_slice(payload)
        .map_err(|_| "GitHub release metadata is invalid".to_string())?;
    if release.draft || release.prerelease {
        return Err("GitHub latest release is not a stable release".to_string());
    }

    let current = Version::parse(current_version)
        .map_err(|_| "current application version is invalid".to_string())?;
    let tag = release
        .tag_name
        .strip_prefix('v')
        .or_else(|| release.tag_name.strip_prefix('V'))
        .unwrap_or(&release.tag_name);
    let latest =
        Version::parse(tag).map_err(|_| "GitHub latest release version is invalid".to_string())?;
    if !latest.pre.is_empty() {
        return Err("GitHub latest release is not a stable release".to_string());
    }

    Ok(UpdateCheckResult {
        current_version: current.to_string(),
        latest_version: latest.to_string(),
        update_available: latest > current,
        release_notes: summarize_notes(release.body.as_deref()),
        checked_at_ms,
    })
}

fn summarize_notes(body: Option<&str>) -> Option<String> {
    let body = body?.trim();
    if body.is_empty() {
        return None;
    }
    let mut notes = body.chars().take(MAX_NOTES_CHARS).collect::<String>();
    if body.chars().count() > MAX_NOTES_CHARS {
        notes.push('…');
    }
    Some(notes)
}

fn timestamp_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{
        acquire_update_check, evaluate_release_payload, MAX_NOTES_CHARS, MAX_RELEASE_BYTES,
        RELEASE_PAGE_URL, UPDATE_CHECK_LOCK,
    };

    fn release(tag: &str, draft: bool, prerelease: bool, body: &str) -> Vec<u8> {
        serde_json::json!({
            "tag_name": tag,
            "draft": draft,
            "prerelease": prerelease,
            "body": body,
            "html_url": "https://attacker.example.invalid/fake-download"
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn detects_a_newer_stable_semver_and_uses_the_fixed_release_page() {
        let result =
            evaluate_release_payload(&release("v0.1.6", false, false, "Fixes"), "0.1.5", 42)
                .unwrap();

        assert!(result.update_available);
        assert_eq!(result.current_version, "0.1.5");
        assert_eq!(result.latest_version, "0.1.6");
        assert_eq!(
            RELEASE_PAGE_URL,
            "https://github.com/mingisrookie/codex-switch/releases/latest"
        );
        assert_eq!(result.checked_at_ms, 42);
    }

    #[test]
    fn treats_equal_and_older_releases_as_current() {
        let equal =
            evaluate_release_payload(&release("0.1.5", false, false, ""), "0.1.5", 1).unwrap();
        let older =
            evaluate_release_payload(&release("v0.1.4", false, false, ""), "0.1.5", 1).unwrap();

        assert!(!equal.update_available);
        assert!(!older.update_available);
    }

    #[test]
    fn rejects_draft_prerelease_and_semver_prerelease_metadata() {
        for payload in [
            release("v0.1.6", true, false, ""),
            release("v0.1.6", false, true, ""),
            release("v0.1.6-rc.1", false, false, ""),
        ] {
            let error = evaluate_release_payload(&payload, "0.1.5", 1).unwrap_err();
            assert!(error.contains("not a stable release"), "{error}");
        }
    }

    #[test]
    fn rejects_invalid_or_oversized_metadata_without_echoing_the_payload() {
        let marker = "secret-response-marker";
        let malformed = format!("{{\"tag_name\":\"{marker}\"");
        let error = evaluate_release_payload(malformed.as_bytes(), "0.1.5", 1).unwrap_err();
        assert_eq!(error, "GitHub release metadata is invalid");
        assert!(!error.contains(marker));

        let oversized = vec![b'x'; MAX_RELEASE_BYTES + 1];
        assert_eq!(
            evaluate_release_payload(&oversized, "0.1.5", 1).unwrap_err(),
            "GitHub release metadata is too large"
        );

        let missing_required_fields = br#"{"tag_name":"v0.1.6"}"#;
        assert_eq!(
            evaluate_release_payload(missing_required_fields, "0.1.5", 1).unwrap_err(),
            "GitHub release metadata is invalid"
        );
    }

    #[test]
    fn rejects_invalid_versions_without_leaking_tag_values() {
        let payload = release("not-a-version-secret", false, false, "");
        assert_eq!(
            evaluate_release_payload(&payload, "0.1.5", 1).unwrap_err(),
            "GitHub latest release version is invalid"
        );
        assert_eq!(
            evaluate_release_payload(&release("v1.0.0", false, false, ""), "bad-current", 1)
                .unwrap_err(),
            "current application version is invalid"
        );
    }

    #[test]
    fn truncates_release_notes_on_a_character_boundary() {
        let body = "更".repeat(MAX_NOTES_CHARS + 20);
        let result =
            evaluate_release_payload(&release("v0.1.6", false, false, &body), "0.1.5", 1).unwrap();
        let notes = result.release_notes.unwrap();
        assert_eq!(notes.chars().count(), MAX_NOTES_CHARS + 1);
        assert!(notes.ends_with('…'));
    }

    #[test]
    fn rejects_overlapping_update_checks() {
        let guard = UPDATE_CHECK_LOCK.lock().unwrap();
        assert_eq!(
            acquire_update_check().unwrap_err(),
            "an update check is already in progress"
        );
        drop(guard);
        assert!(acquire_update_check().is_ok());
    }

    #[test]
    #[ignore = "requires live GitHub access"]
    fn live_github_release_contract_is_compatible() {
        let result = super::check_latest_release().unwrap();
        assert!(!result.current_version.is_empty());
        assert!(!result.latest_version.is_empty());
        assert_eq!(
            RELEASE_PAGE_URL,
            "https://github.com/mingisrookie/codex-switch/releases/latest"
        );
    }
}

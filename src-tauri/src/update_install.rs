use crate::update_check::{fetch_latest_release, GithubAsset, ReleaseCandidate};
use reqwest::{
    blocking::{Client, Response},
    header::USER_AGENT,
    redirect::{Attempt, Policy},
};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::{Mutex, OnceLock, TryLockError},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const UPDATE_ASSET_NAME: &str = "codex-switch.exe";
const UPDATE_URL_PREFIX: &str = "https://github.com/mingisrookie/codex-switch/releases/download/";
const MAX_UPDATE_BYTES: u64 = 64 * 1024 * 1024;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
const UPDATE_DIR_PREFIX: &str = "codex-switch-update-";
const UPDATE_PLAN_SCHEMA: u32 = 1;
const APPLY_UPDATE_ARG: &str = "--codex-switch-apply-update";
const UPDATE_COMPLETE_ARG: &str = "--codex-switch-update-complete";
const UPDATE_ROLLED_BACK_ARG: &str = "--codex-switch-update-rolled-back";
const STARTUP_ACK_NAME: &str = "startup-ack";
const STARTUP_ACK_ATTEMPTS: usize = 150;
const STARTUP_ACK_INTERVAL: Duration = Duration::from_millis(100);

static UPDATE_INSTALL_STARTED: Mutex<bool> = Mutex::new(false);
static STARTUP_NOTICE: OnceLock<Option<UpdateStartupNotice>> = OnceLock::new();
static STARTUP_CONTEXT: OnceLock<Option<StartupUpdateContext>> = OnceLock::new();

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInstallReceipt {
    pub from_version: String,
    pub to_version: String,
    pub downloaded_bytes: u64,
    pub sha256: String,
    pub restarting: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStartupNotice {
    pub status: UpdateStartupStatus,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum UpdateStartupStatus {
    Updated,
    RolledBack,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct UpdatePlan {
    schema_version: u32,
    parent_pid: u32,
    staging_dir: PathBuf,
    target_exe: PathBuf,
    helper_exe: PathBuf,
    staged_exe: PathBuf,
    expected_old_sha256: String,
    expected_new_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ValidatedAsset {
    size: u64,
    sha256: String,
    download_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StartupUpdateContext {
    status: UpdateStartupStatus,
    staging_dir: PathBuf,
    ack_path: PathBuf,
    ack_payload: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaunchStatus {
    Updated,
    RolledBack,
}

impl LaunchStatus {
    fn startup_status(self) -> UpdateStartupStatus {
        match self {
            Self::Updated => UpdateStartupStatus::Updated,
            Self::RolledBack => UpdateStartupStatus::RolledBack,
        }
    }
}

pub fn install_latest_update() -> Result<UpdateInstallReceipt, String> {
    if cfg!(debug_assertions) {
        return Err("self-update is disabled in debug builds".to_string());
    }
    #[cfg(not(windows))]
    {
        return Err("self-update is not available on this platform".to_string());
    }
    #[cfg(windows)]
    {
        mark_update_started()?;
        let result = prepare_update();
        if result.is_err() {
            reset_update_started();
        }
        result
    }
}

pub fn startup_update_notice() -> Option<UpdateStartupNotice> {
    STARTUP_NOTICE.get().cloned().flatten()
}

pub fn process_startup_update_args() -> Option<i32> {
    let args = env::args_os().collect::<Vec<_>>();
    if args.len() == 3 && args[1] == APPLY_UPDATE_ARG {
        let manifest = PathBuf::from(&args[2]);
        return Some(if run_update_helper(&manifest).is_ok() {
            0
        } else {
            1
        });
    }

    let context = startup_context_from_args(&args, &env::current_exe().ok()?);
    let notice = context.as_ref().map(|context| UpdateStartupNotice {
        status: context.status,
    });
    let _ = STARTUP_CONTEXT.set(context);
    let _ = STARTUP_NOTICE.set(notice);
    None
}

pub fn acknowledge_update_startup() -> Result<(), String> {
    let Some(context) = STARTUP_CONTEXT.get().cloned().flatten() else {
        return Ok(());
    };
    write_startup_ack(&context)?;
    schedule_staging_cleanup(context.staging_dir);
    Ok(())
}

fn startup_context_from_args(
    args: &[std::ffi::OsString],
    current_exe: &Path,
) -> Option<StartupUpdateContext> {
    if args.len() != 3 {
        return None;
    }
    let status = if args[1] == UPDATE_COMPLETE_ARG {
        UpdateStartupStatus::Updated
    } else if args[1] == UPDATE_ROLLED_BACK_ARG {
        UpdateStartupStatus::RolledBack
    } else {
        return None;
    };
    validate_startup_context(Path::new(&args[2]), status, current_exe).ok()
}

fn validate_startup_context(
    staging_dir: &Path,
    status: UpdateStartupStatus,
    current_exe: &Path,
) -> Result<StartupUpdateContext, String> {
    let staging_dir = canonical_staging_dir(staging_dir)?;
    let plan_path = staging_dir.join("update-plan.json");
    let plan: UpdatePlan = serde_json::from_slice(
        &fs::read(&plan_path).map_err(|_| "the update startup plan is missing".to_string())?,
    )
    .map_err(|_| "the update startup plan is invalid".to_string())?;
    if plan.schema_version != UPDATE_PLAN_SCHEMA {
        return Err("the update startup plan schema is unsupported".to_string());
    }
    let planned_staging = canonical_staging_dir(&plan.staging_dir)?;
    if !paths_equal(&planned_staging, &staging_dir) {
        return Err("the update startup staging directory does not match the plan".to_string());
    }
    let target = fs::canonicalize(&plan.target_exe)
        .map_err(|_| "the updated executable is missing".to_string())?;
    let running = fs::canonicalize(current_exe)
        .map_err(|_| "the restarted executable is invalid".to_string())?;
    if !paths_equal(&target, &running) {
        return Err("the restarted executable does not match the update plan".to_string());
    }
    let helper = fs::canonicalize(&plan.helper_exe)
        .map_err(|_| "the update helper is missing during startup".to_string())?;
    let staged = fs::canonicalize(&plan.staged_exe)
        .map_err(|_| "the staged update is missing during startup".to_string())?;
    if helper.parent() != Some(staging_dir.as_path())
        || staged.parent() != Some(staging_dir.as_path())
    {
        return Err("the update startup files are outside the staging directory".to_string());
    }
    validate_sha256(&plan.expected_old_sha256)?;
    validate_sha256(&plan.expected_new_sha256)?;
    if sha256_file(&helper)? != plan.expected_old_sha256
        || sha256_file(&staged)? != plan.expected_new_sha256
    {
        return Err("the update startup files do not match the plan".to_string());
    }
    let expected_sha256 = match status {
        UpdateStartupStatus::Updated => &plan.expected_new_sha256,
        UpdateStartupStatus::RolledBack => &plan.expected_old_sha256,
    };
    if sha256_file(&running)? != expected_sha256.as_str() {
        return Err("the restarted executable does not match the expected update".to_string());
    }
    Ok(StartupUpdateContext {
        status,
        ack_path: staging_dir.join(STARTUP_ACK_NAME),
        ack_payload: startup_ack_payload(status, expected_sha256),
        staging_dir,
    })
}

fn startup_ack_payload(status: UpdateStartupStatus, expected_sha256: &str) -> String {
    let status = match status {
        UpdateStartupStatus::Updated => "updated",
        UpdateStartupStatus::RolledBack => "rolledBack",
    };
    format!("codex-switch-update-ack-v1\n{status}\n{expected_sha256}\n")
}

fn write_startup_ack(context: &StartupUpdateContext) -> Result<(), String> {
    let staging_dir = canonical_staging_dir(&context.staging_dir)?;
    if !paths_equal(&context.ack_path, &staging_dir.join(STARTUP_ACK_NAME)) {
        return Err("the update startup acknowledgement path is unsafe".to_string());
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&context.ack_path)
        .map_err(|_| "failed to create the update startup acknowledgement".to_string())?;
    file.write_all(context.ack_payload.as_bytes())
        .and_then(|_| file.sync_all())
        .map_err(|_| "failed to persist the update startup acknowledgement".to_string())
}

#[cfg(windows)]
fn prepare_update() -> Result<UpdateInstallReceipt, String> {
    let current_version = Version::parse(env!("CARGO_PKG_VERSION"))
        .map_err(|_| "current application version is invalid".to_string())?;
    let release = fetch_latest_release()?;
    if release.version <= current_version {
        return Err("the application is already up to date".to_string());
    }
    let asset = select_update_asset(&release)?;
    let target_exe = env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|_| "failed to resolve the running executable".to_string())?;
    preflight_target(&target_exe)?;
    let old_sha256 = sha256_file(&target_exe)?;
    let staging_dir = create_staging_dir()?;
    let staged_exe = staging_dir.join("downloaded.exe");
    let helper_exe = staging_dir.join("updater-helper.exe");
    let manifest_path = staging_dir.join("update-plan.json");
    let ready_path = staging_dir.join("helper-ready");

    let result = (|| {
        download_asset(&asset, &staged_exe)?;
        fs::copy(&target_exe, &helper_exe)
            .map_err(|_| "failed to stage the update helper".to_string())?;
        if sha256_file(&helper_exe)? != old_sha256 {
            return Err("staged update helper verification failed".to_string());
        }

        let plan = UpdatePlan {
            schema_version: UPDATE_PLAN_SCHEMA,
            parent_pid: std::process::id(),
            staging_dir: staging_dir.clone(),
            target_exe: target_exe.clone(),
            helper_exe: helper_exe.clone(),
            staged_exe: staged_exe.clone(),
            expected_old_sha256: old_sha256,
            expected_new_sha256: asset.sha256.clone(),
        };
        write_update_plan(&manifest_path, &plan)?;
        let mut helper = Command::new(&helper_exe)
            .arg(APPLY_UPDATE_ARG)
            .arg(&manifest_path)
            .spawn()
            .map_err(|_| "failed to start the update helper".to_string())?;
        wait_for_helper_ready(&mut helper, &ready_path)?;

        Ok(UpdateInstallReceipt {
            from_version: current_version.to_string(),
            to_version: release.version.to_string(),
            downloaded_bytes: asset.size,
            sha256: asset.sha256,
            restarting: true,
        })
    })();

    if result.is_err() {
        let _ = remove_staging_dir(&staging_dir);
    }
    result
}

fn select_update_asset(release: &ReleaseCandidate) -> Result<ValidatedAsset, String> {
    if !release.version.pre.is_empty() || !release.version.build.is_empty() {
        return Err("GitHub latest release is not a stable release".to_string());
    }
    let expected_tag = release.version.to_string();
    if release.tag_name != expected_tag && release.tag_name != format!("v{expected_tag}") {
        return Err("GitHub latest release tag is invalid".to_string());
    }
    let matches = release
        .assets
        .iter()
        .filter(|asset| asset.name == UPDATE_ASSET_NAME)
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return Err("GitHub release must contain one codex-switch.exe asset".to_string());
    }
    validate_asset(matches[0], &release.tag_name)
}

fn validate_asset(asset: &GithubAsset, tag_name: &str) -> Result<ValidatedAsset, String> {
    if asset.size == 0 || asset.size > MAX_UPDATE_BYTES {
        return Err("GitHub update asset size is invalid".to_string());
    }
    let sha256 = asset
        .digest
        .as_deref()
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .filter(|digest| digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| "GitHub update asset SHA-256 digest is invalid".to_string())?;
    let download_url = format!("{UPDATE_URL_PREFIX}{tag_name}/{UPDATE_ASSET_NAME}");
    if asset.browser_download_url != download_url {
        return Err("GitHub update asset URL is invalid".to_string());
    }
    Ok(ValidatedAsset {
        size: asset.size,
        sha256,
        download_url,
    })
}

#[cfg(windows)]
fn preflight_target(target_exe: &Path) -> Result<(), String> {
    if !target_exe.is_file()
        || !target_exe
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
    {
        return Err("the running executable is not a replaceable Windows EXE".to_string());
    }
    let parent = target_exe
        .parent()
        .ok_or_else(|| "the running executable directory is invalid".to_string())?;
    let probe = parent.join(format!(
        ".codex-switch-update-probe-{}-{}",
        std::process::id(),
        timestamp_nanos()
    ));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .map_err(|_| "the application directory is not writable".to_string())?;
    drop(file);
    fs::remove_file(&probe).map_err(|_| "failed to remove the update preflight file".to_string())
}

#[cfg(windows)]
fn create_staging_dir() -> Result<PathBuf, String> {
    let path = env::temp_dir().join(format!(
        "{UPDATE_DIR_PREFIX}{}-{}",
        std::process::id(),
        timestamp_nanos()
    ));
    fs::create_dir(&path)
        .map_err(|_| "failed to create the update staging directory".to_string())?;
    canonical_staging_dir(&path)
}

#[cfg(windows)]
fn download_asset(asset: &ValidatedAsset, staged_exe: &Path) -> Result<(), String> {
    let client = Client::builder()
        .timeout(DOWNLOAD_TIMEOUT)
        .redirect(Policy::custom(github_asset_redirect))
        .build()
        .map_err(|_| "failed to initialize the update downloader".to_string())?;
    let response = client
        .get(&asset.download_url)
        .header(
            USER_AGENT,
            format!("codex-switch/{}", env!("CARGO_PKG_VERSION")),
        )
        .send()
        .map_err(|_| "failed to download the update asset".to_string())?;
    write_download(response, asset, staged_exe)
}

fn github_asset_redirect(attempt: Attempt<'_>) -> reqwest::redirect::Action {
    if attempt.previous().len() >= 5 {
        return attempt.error("too many update download redirects");
    }
    if allowed_update_redirect(attempt.url()) {
        attempt.follow()
    } else {
        attempt.stop()
    }
}

fn allowed_update_redirect(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && url.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("github.com")
                || host.eq_ignore_ascii_case("release-assets.githubusercontent.com")
                || host.eq_ignore_ascii_case("objects.githubusercontent.com")
        })
}

#[cfg(windows)]
fn write_download(
    mut response: Response,
    asset: &ValidatedAsset,
    staged_exe: &Path,
) -> Result<(), String> {
    if !response.status().is_success() {
        return Err(format!(
            "GitHub update download returned HTTP {}",
            response.status().as_u16()
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length != asset.size || length > MAX_UPDATE_BYTES)
    {
        return Err("GitHub update download size does not match metadata".to_string());
    }

    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(staged_exe)
        .map_err(|_| "failed to create the staged update file".to_string())?;
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = response
            .read(&mut buffer)
            .map_err(|_| "failed while reading the update asset".to_string())?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .ok_or_else(|| "GitHub update asset is too large".to_string())?;
        if total > asset.size || total > MAX_UPDATE_BYTES {
            return Err("GitHub update download exceeded the expected size".to_string());
        }
        output
            .write_all(&buffer[..read])
            .map_err(|_| "failed to write the staged update file".to_string())?;
        hasher.update(&buffer[..read]);
    }
    output
        .sync_all()
        .map_err(|_| "failed to flush the staged update file".to_string())?;
    if total != asset.size {
        return Err("GitHub update download is incomplete".to_string());
    }
    let actual = format!("{:x}", hasher.finalize());
    if actual != asset.sha256 {
        return Err("downloaded update SHA-256 verification failed".to_string());
    }
    Ok(())
}

#[cfg(windows)]
fn write_update_plan(path: &Path, plan: &UpdatePlan) -> Result<(), String> {
    let payload =
        serde_json::to_vec(plan).map_err(|_| "failed to encode the update plan".to_string())?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|_| "failed to create the update plan".to_string())?;
    file.write_all(&payload)
        .and_then(|_| file.sync_all())
        .map_err(|_| "failed to persist the update plan".to_string())
}

fn run_update_helper(manifest_path: &Path) -> Result<(), String> {
    #[cfg(not(windows))]
    {
        let _ = manifest_path;
        Err("self-update is not available on this platform".to_string())
    }
    #[cfg(windows)]
    {
        let manifest =
            fs::read(manifest_path).map_err(|_| "failed to read the update plan".to_string())?;
        let plan: UpdatePlan = serde_json::from_slice(&manifest)
            .map_err(|_| "the update plan is invalid".to_string())?;
        let helper_exe = env::current_exe()
            .and_then(fs::canonicalize)
            .map_err(|_| "failed to resolve the update helper".to_string())?;
        validate_update_plan(&plan, &helper_exe)?;
        let waiter = ParentProcessWaiter::open(plan.parent_pid)?;
        write_helper_ready(&plan.staging_dir.join("helper-ready"))?;
        apply_validated_update_plan_with(&plan, || waiter.wait(), launch_and_confirm)
    }
}

#[cfg(test)]
fn apply_update_plan_with<W, L>(
    plan: &UpdatePlan,
    running_helper: &Path,
    wait_for_parent: W,
    launch: L,
) -> Result<(), String>
where
    W: FnOnce() -> Result<(), String>,
    L: FnMut(&Path, LaunchStatus, &Path, &str) -> Result<(), String>,
{
    validate_update_plan(plan, running_helper)?;
    apply_validated_update_plan_with(plan, wait_for_parent, launch)
}

fn apply_validated_update_plan_with<W, L>(
    plan: &UpdatePlan,
    wait_for_parent: W,
    mut launch: L,
) -> Result<(), String>
where
    W: FnOnce() -> Result<(), String>,
    L: FnMut(&Path, LaunchStatus, &Path, &str) -> Result<(), String>,
{
    wait_for_parent()?;
    let target_parent = plan
        .target_exe
        .parent()
        .ok_or_else(|| "the update target directory is invalid".to_string())?;
    let target_name = plan
        .target_exe
        .file_name()
        .ok_or_else(|| "the update target filename is invalid".to_string())?
        .to_string_lossy();
    let suffix = format!("{}-{}", plan.parent_pid, timestamp_nanos());
    let replacement = target_parent.join(format!(".{target_name}.update-new-{suffix}"));
    let backup = target_parent.join(format!(".{target_name}.update-backup-{suffix}"));

    let mut fallback_launched = false;
    let result = (|| {
        if sha256_file(&plan.target_exe)? != plan.expected_old_sha256 {
            return Err("the installed executable changed before update".to_string());
        }
        fs::copy(&plan.staged_exe, &replacement)
            .map_err(|_| "failed to create the replacement executable".to_string())?;
        if sha256_file(&replacement)? != plan.expected_new_sha256 {
            return Err("replacement executable verification failed".to_string());
        }
        fs::rename(&plan.target_exe, &backup)
            .map_err(|_| "failed to back up the installed executable".to_string())?;
        if fs::rename(&replacement, &plan.target_exe).is_err() {
            let _ = fs::rename(&backup, &plan.target_exe);
            return Err("failed to activate the replacement executable".to_string());
        }
        if let Err(error) = launch(
            &plan.target_exe,
            LaunchStatus::Updated,
            &plan.staging_dir,
            &plan.expected_new_sha256,
        ) {
            rollback_executable(&plan.target_exe, &backup)?;
            launch(
                &plan.target_exe,
                LaunchStatus::RolledBack,
                &plan.staging_dir,
                &plan.expected_old_sha256,
            )
            .map_err(|_| "update failed and the restored version could not restart".to_string())?;
            fallback_launched = true;
            return Err(error);
        }
        fs::remove_file(&backup).map_err(|_| {
            "updated successfully but failed to remove the old executable".to_string()
        })?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&replacement);
        let target_is_expected_old = plan.target_exe.exists()
            && sha256_file(&plan.target_exe).is_ok_and(|hash| hash == plan.expected_old_sha256);
        if !fallback_launched && target_is_expected_old && !backup.exists() {
            let _ = launch(
                &plan.target_exe,
                LaunchStatus::RolledBack,
                &plan.staging_dir,
                &plan.expected_old_sha256,
            );
        }
    }
    result
}

fn validate_update_plan(plan: &UpdatePlan, running_helper: &Path) -> Result<(), String> {
    if plan.schema_version != UPDATE_PLAN_SCHEMA {
        return Err("the update plan schema is unsupported".to_string());
    }
    let staging_dir = canonical_staging_dir(&plan.staging_dir)?;
    let helper = fs::canonicalize(&plan.helper_exe)
        .map_err(|_| "the staged update helper is missing".to_string())?;
    let running = fs::canonicalize(running_helper)
        .map_err(|_| "the running update helper is invalid".to_string())?;
    if !paths_equal(&helper, &running) || helper.parent() != Some(staging_dir.as_path()) {
        return Err("the running update helper does not match the update plan".to_string());
    }
    let staged = fs::canonicalize(&plan.staged_exe)
        .map_err(|_| "the staged update executable is missing".to_string())?;
    if staged.parent() != Some(staging_dir.as_path()) {
        return Err("the staged update executable is outside the staging directory".to_string());
    }
    let target = fs::canonicalize(&plan.target_exe)
        .map_err(|_| "the installed executable is missing".to_string())?;
    if target == helper || target == staged {
        return Err("the update target is invalid".to_string());
    }
    validate_sha256(&plan.expected_old_sha256)?;
    validate_sha256(&plan.expected_new_sha256)?;
    if sha256_file(&helper)? != plan.expected_old_sha256 {
        return Err("the staged update helper is invalid".to_string());
    }
    if sha256_file(&staged)? != plan.expected_new_sha256 {
        return Err("the staged update executable is invalid".to_string());
    }
    Ok(())
}

fn rollback_executable(target: &Path, backup: &Path) -> Result<(), String> {
    fs::remove_file(target)
        .map_err(|_| "update failed and the replacement could not be removed".to_string())?;
    fs::rename(backup, target)
        .map_err(|_| "update failed and the previous executable could not be restored".to_string())
}

#[cfg(windows)]
fn launch_and_confirm(
    target: &Path,
    status: LaunchStatus,
    staging_dir: &Path,
    expected_sha256: &str,
) -> Result<(), String> {
    let notice_arg = match status {
        LaunchStatus::Updated => UPDATE_COMPLETE_ARG,
        LaunchStatus::RolledBack => UPDATE_ROLLED_BACK_ARG,
    };
    let ack_path = staging_dir.join(STARTUP_ACK_NAME);
    match fs::remove_file(&ack_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err("failed to reset the update startup acknowledgement".to_string()),
    }
    let mut child = Command::new(target)
        .arg(notice_arg)
        .arg(staging_dir)
        .spawn()
        .map_err(|_| "failed to restart the application after update".to_string())?;
    let payload = startup_ack_payload(status.startup_status(), expected_sha256);
    wait_for_startup_ack(&mut child, &ack_path, &payload)
}

#[cfg(windows)]
trait StartupChild {
    fn has_exited(&mut self) -> Result<bool, String>;
    fn abort(&mut self);
}

#[cfg(windows)]
impl StartupChild for std::process::Child {
    fn has_exited(&mut self) -> Result<bool, String> {
        self.try_wait()
            .map(|status| status.is_some())
            .map_err(|_| "failed to monitor the restarted application".to_string())
    }

    fn abort(&mut self) {
        let _ = self.kill();
        let _ = self.wait();
    }
}

#[cfg(windows)]
fn wait_for_startup_ack(
    child: &mut impl StartupChild,
    ack_path: &Path,
    expected_payload: &str,
) -> Result<(), String> {
    wait_for_startup_ack_with(
        child,
        ack_path,
        expected_payload,
        STARTUP_ACK_ATTEMPTS,
        STARTUP_ACK_INTERVAL,
    )
}

#[cfg(windows)]
fn wait_for_startup_ack_with(
    child: &mut impl StartupChild,
    ack_path: &Path,
    expected_payload: &str,
    attempts: usize,
    interval: Duration,
) -> Result<(), String> {
    for _ in 0..attempts {
        if fs::read(ack_path).is_ok_and(|payload| payload == expected_payload.as_bytes()) {
            return Ok(());
        }
        if child.has_exited()? {
            return Err("the application exited before completing startup".to_string());
        }
        thread::sleep(interval);
    }
    child.abort();
    Err("timed out waiting for the application startup acknowledgement".to_string())
}

#[cfg(windows)]
struct ParentProcessWaiter {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl ParentProcessWaiter {
    fn open(pid: u32) -> Result<Self, String> {
        use windows_sys::Win32::{
            Foundation::{GetLastError, ERROR_INVALID_PARAMETER},
            System::Threading::OpenProcess,
        };
        const PROCESS_SYNCHRONIZE_ACCESS: u32 = 0x0010_0000;
        unsafe {
            let handle = OpenProcess(PROCESS_SYNCHRONIZE_ACCESS, 0, pid);
            if handle.is_null() && GetLastError() != ERROR_INVALID_PARAMETER {
                return Err("failed to wait for the running application".to_string());
            }
            Ok(Self { handle })
        }
    }

    fn wait(self) -> Result<(), String> {
        use windows_sys::Win32::{
            Foundation::{WAIT_FAILED, WAIT_OBJECT_0},
            System::Threading::WaitForSingleObject,
        };
        if self.handle.is_null() {
            return Ok(());
        }
        let result = unsafe { WaitForSingleObject(self.handle, 60_000) };
        if result == WAIT_OBJECT_0 {
            Ok(())
        } else if result == WAIT_FAILED {
            Err("failed while waiting for the running application".to_string())
        } else {
            Err("timed out waiting for the running application to exit".to_string())
        }
    }
}

#[cfg(windows)]
impl Drop for ParentProcessWaiter {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        if !self.handle.is_null() {
            unsafe {
                CloseHandle(self.handle);
            }
        }
    }
}

#[cfg(windows)]
fn wait_for_helper_ready(child: &mut std::process::Child, ready_path: &Path) -> Result<(), String> {
    for _ in 0..50 {
        if ready_path.is_file() {
            return Ok(());
        }
        if child
            .try_wait()
            .map_err(|_| "failed to monitor the update helper".to_string())?
            .is_some()
        {
            return Err("the update helper failed its safety preflight".to_string());
        }
        thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    Err("timed out waiting for the update helper safety preflight".to_string())
}

#[cfg(windows)]
fn write_helper_ready(path: &Path) -> Result<(), String> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|_| "failed to signal update helper readiness".to_string())?;
    file.sync_all()
        .map_err(|_| "failed to persist update helper readiness".to_string())
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|_| "failed to read an update file".to_string())?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|_| "failed while hashing an update file".to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn validate_sha256(value: &str) -> Result<(), String> {
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err("the update plan contains an invalid SHA-256".to_string())
    }
}

fn canonical_staging_dir(path: &Path) -> Result<PathBuf, String> {
    let canonical = fs::canonicalize(path)
        .map_err(|_| "the update staging directory is missing".to_string())?;
    let temp = fs::canonicalize(env::temp_dir())
        .map_err(|_| "the system temporary directory is unavailable".to_string())?;
    let name_ok = canonical
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(UPDATE_DIR_PREFIX));
    if canonical.parent() != Some(temp.as_path()) || !name_ok {
        return Err("the update staging directory is unsafe".to_string());
    }
    Ok(canonical)
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    #[cfg(windows)]
    {
        left.as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

fn mark_update_started() -> Result<(), String> {
    match UPDATE_INSTALL_STARTED.try_lock() {
        Ok(mut started) if !*started => {
            *started = true;
            Ok(())
        }
        Ok(_) | Err(TryLockError::WouldBlock) => {
            Err("an update installation is already in progress".to_string())
        }
        Err(TryLockError::Poisoned(_)) => Err("the update installer is unavailable".to_string()),
    }
}

fn reset_update_started() {
    if let Ok(mut started) = UPDATE_INSTALL_STARTED.lock() {
        *started = false;
    }
}

fn schedule_staging_cleanup(path: PathBuf) {
    let Ok(staging_dir) = canonical_staging_dir(&path) else {
        return;
    };
    thread::spawn(move || {
        thread::sleep(Duration::from_secs(3));
        for _ in 0..10 {
            if remove_staging_dir(&staging_dir).is_ok() || !staging_dir.exists() {
                break;
            }
            thread::sleep(Duration::from_millis(500));
        }
    });
}

fn remove_staging_dir(path: &Path) -> Result<(), String> {
    let safe = canonical_staging_dir(path)?;
    fs::remove_dir_all(safe)
        .map_err(|_| "failed to remove the update staging directory".to_string())
}

fn timestamp_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::Builder;

    fn release(assets: Vec<GithubAsset>) -> ReleaseCandidate {
        ReleaseCandidate {
            tag_name: "v0.1.7".to_string(),
            version: Version::parse("0.1.7").unwrap(),
            release_notes: None,
            assets,
        }
    }

    fn asset(size: u64, digest: Option<String>, url: &str) -> GithubAsset {
        GithubAsset {
            name: UPDATE_ASSET_NAME.to_string(),
            size,
            digest,
            browser_download_url: url.to_string(),
        }
    }

    fn staged_plan() -> (tempfile::TempDir, UpdatePlan, PathBuf) {
        let temp = Builder::new()
            .prefix(UPDATE_DIR_PREFIX)
            .tempdir_in(env::temp_dir())
            .unwrap();
        let target_dir = tempfile::tempdir().unwrap();
        let target = target_dir.keep().join("codex-switch.exe");
        let helper = temp.path().join("updater-helper.exe");
        let staged = temp.path().join("downloaded.exe");
        fs::write(&target, b"old executable").unwrap();
        fs::write(&helper, b"old executable").unwrap();
        fs::write(&staged, b"new executable").unwrap();
        let plan = UpdatePlan {
            schema_version: UPDATE_PLAN_SCHEMA,
            parent_pid: 42,
            staging_dir: temp.path().to_path_buf(),
            target_exe: target,
            helper_exe: helper.clone(),
            staged_exe: staged,
            expected_old_sha256: sha256_file(&helper).unwrap(),
            expected_new_sha256: sha256_file(&temp.path().join("downloaded.exe")).unwrap(),
        };
        (temp, plan, helper)
    }

    fn persist_plan(plan: &UpdatePlan) {
        fs::write(
            plan.staging_dir.join("update-plan.json"),
            serde_json::to_vec(plan).unwrap(),
        )
        .unwrap();
    }

    #[cfg(windows)]
    #[derive(Default)]
    struct FakeStartupChild {
        exited: bool,
        aborted: bool,
    }

    #[cfg(windows)]
    impl StartupChild for FakeStartupChild {
        fn has_exited(&mut self) -> Result<bool, String> {
            Ok(self.exited)
        }

        fn abort(&mut self) {
            self.aborted = true;
        }
    }

    #[test]
    fn accepts_only_the_fixed_single_asset_with_github_digest() {
        let digest = "a".repeat(64);
        let validated = select_update_asset(&release(vec![asset(
            10,
            Some(format!("sha256:{digest}")),
            "https://github.com/mingisrookie/codex-switch/releases/download/v0.1.7/codex-switch.exe",
        )]))
        .unwrap();
        assert_eq!(validated.sha256, digest);
        assert_eq!(validated.size, 10);
    }

    #[test]
    fn rejects_missing_duplicate_oversized_or_unsigned_assets() {
        assert!(select_update_asset(&release(vec![])).is_err());
        let valid = asset(
            10,
            Some(format!("sha256:{}", "a".repeat(64))),
            "https://github.com/mingisrookie/codex-switch/releases/download/v0.1.7/codex-switch.exe",
        );
        assert!(select_update_asset(&release(vec![valid.clone(), valid.clone()])).is_err());
        assert!(select_update_asset(&release(vec![asset(
            MAX_UPDATE_BYTES + 1,
            valid.digest.clone(),
            &valid.browser_download_url,
        )]))
        .is_err());
        let unsigned = asset(10, None, &valid.browser_download_url);
        assert!(select_update_asset(&release(vec![unsigned])).is_err());
    }

    #[test]
    fn rejects_remote_control_of_the_download_url() {
        let result = select_update_asset(&release(vec![asset(
            10,
            Some(format!("sha256:{}", "a".repeat(64))),
            "https://attacker.example.invalid/codex-switch.exe",
        )]));
        assert_eq!(result.unwrap_err(), "GitHub update asset URL is invalid");
    }

    #[test]
    fn redirect_allowlist_rejects_http_and_lookalike_hosts() {
        for url in [
            "http://release-assets.githubusercontent.com/file",
            "https://release-assets.githubusercontent.com.evil.invalid/file",
            "https://raw.githubusercontent.com/owner/repo/file",
            "https://attacker.example.invalid/file",
        ] {
            assert!(!allowed_update_redirect(&reqwest::Url::parse(url).unwrap()));
        }
        assert!(allowed_update_redirect(
            &reqwest::Url::parse("https://release-assets.githubusercontent.com/file").unwrap()
        ));
    }

    #[test]
    fn helper_replaces_the_target_and_removes_the_backup() {
        let (_temp, plan, helper) = staged_plan();
        let mut launches = Vec::new();
        apply_update_plan_with(
            &plan,
            &helper,
            || Ok(()),
            |target, status, _, expected_sha256| {
                launches.push(status);
                assert_eq!(fs::read(target).unwrap(), b"new executable");
                assert_eq!(sha256_file(target).unwrap(), expected_sha256);
                assert!(target
                    .parent()
                    .unwrap()
                    .read_dir()
                    .unwrap()
                    .any(|entry| entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .contains("update-backup")));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(launches, vec![LaunchStatus::Updated]);
        assert_eq!(fs::read(&plan.target_exe).unwrap(), b"new executable");
        assert!(plan
            .target_exe
            .parent()
            .unwrap()
            .read_dir()
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("backup")));
    }

    #[test]
    fn startup_ack_requires_the_controlled_plan_target_status_and_hash() {
        let (_temp, plan, _helper) = staged_plan();
        persist_plan(&plan);
        let args = vec![
            std::ffi::OsString::from("codex-switch.exe"),
            std::ffi::OsString::from(UPDATE_COMPLETE_ARG),
            plan.staging_dir.as_os_str().to_owned(),
        ];

        assert!(startup_context_from_args(&args, &plan.target_exe).is_none());
        fs::write(&plan.target_exe, b"new executable").unwrap();
        let context = startup_context_from_args(&args, &plan.target_exe).unwrap();
        write_startup_ack(&context).unwrap();
        assert_eq!(
            fs::read(&context.ack_path).unwrap(),
            context.ack_payload.as_bytes()
        );

        let unrelated_target = tempfile::NamedTempFile::new().unwrap();
        assert!(startup_context_from_args(&args, unrelated_target.path()).is_none());
    }

    #[test]
    fn startup_ack_rejects_tampered_staging_files() {
        let (_temp, plan, _helper) = staged_plan();
        persist_plan(&plan);
        fs::write(&plan.target_exe, b"new executable").unwrap();
        fs::write(&plan.staged_exe, b"tampered staged executable").unwrap();
        let args = vec![
            std::ffi::OsString::from("codex-switch.exe"),
            std::ffi::OsString::from(UPDATE_COMPLETE_ARG),
            plan.staging_dir.as_os_str().to_owned(),
        ];

        assert!(startup_context_from_args(&args, &plan.target_exe).is_none());
        assert!(!plan.staging_dir.join(STARTUP_ACK_NAME).exists());
    }

    #[cfg(windows)]
    #[test]
    fn helper_waits_for_exact_startup_ack_and_aborts_on_timeout() {
        let temp = Builder::new()
            .prefix(UPDATE_DIR_PREFIX)
            .tempdir_in(env::temp_dir())
            .unwrap();
        let ack_path = temp.path().join(STARTUP_ACK_NAME);
        let payload = startup_ack_payload(UpdateStartupStatus::Updated, &"a".repeat(64));
        fs::write(&ack_path, &payload).unwrap();
        let mut acknowledged = FakeStartupChild::default();
        wait_for_startup_ack_with(&mut acknowledged, &ack_path, &payload, 1, Duration::ZERO)
            .unwrap();
        assert!(!acknowledged.aborted);

        fs::remove_file(&ack_path).unwrap();
        let mut timed_out = FakeStartupChild::default();
        let error =
            wait_for_startup_ack_with(&mut timed_out, &ack_path, &payload, 1, Duration::ZERO)
                .unwrap_err();
        assert!(error.contains("timed out"), "{error}");
        assert!(timed_out.aborted);
    }

    #[cfg(windows)]
    #[test]
    fn helper_rejects_an_early_exit_without_an_ack() {
        let temp = Builder::new()
            .prefix(UPDATE_DIR_PREFIX)
            .tempdir_in(env::temp_dir())
            .unwrap();
        let mut child = FakeStartupChild {
            exited: true,
            aborted: false,
        };
        let error = wait_for_startup_ack_with(
            &mut child,
            &temp.path().join(STARTUP_ACK_NAME),
            "expected",
            1,
            Duration::ZERO,
        )
        .unwrap_err();
        assert!(
            error.contains("exited before completing startup"),
            "{error}"
        );
        assert!(!child.aborted);
    }

    #[test]
    fn helper_rolls_back_when_the_new_executable_cannot_start() {
        let (_temp, plan, helper) = staged_plan();
        let mut launches = Vec::new();
        let error = apply_update_plan_with(
            &plan,
            &helper,
            || Ok(()),
            |target, status, _, expected_sha256| {
                launches.push(status);
                assert_eq!(sha256_file(target).unwrap(), expected_sha256);
                if status == LaunchStatus::Updated {
                    assert_eq!(fs::read(target).unwrap(), b"new executable");
                    Err("injected launch failure".to_string())
                } else {
                    assert_eq!(fs::read(target).unwrap(), b"old executable");
                    Ok(())
                }
            },
        )
        .unwrap_err();
        assert_eq!(error, "injected launch failure");
        assert_eq!(
            launches,
            vec![LaunchStatus::Updated, LaunchStatus::RolledBack]
        );
        assert_eq!(fs::read(&plan.target_exe).unwrap(), b"old executable");
    }

    #[test]
    fn invalid_staged_hash_never_changes_the_target() {
        let (_temp, mut plan, helper) = staged_plan();
        plan.expected_new_sha256 = "0".repeat(64);
        let error =
            apply_update_plan_with(&plan, &helper, || Ok(()), |_, _, _, _| Ok(())).unwrap_err();
        assert_eq!(error, "the staged update executable is invalid");
        assert_eq!(fs::read(&plan.target_exe).unwrap(), b"old executable");
    }

    #[test]
    fn target_changed_after_preflight_is_not_replaced_or_launched() {
        let (_temp, plan, helper) = staged_plan();
        let target = plan.target_exe.clone();
        let mut launch_count = 0;
        let error = apply_update_plan_with(
            &plan,
            &helper,
            || {
                fs::write(&target, b"externally changed").unwrap();
                Ok(())
            },
            |_, _, _, _| {
                launch_count += 1;
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error, "the installed executable changed before update");
        assert_eq!(launch_count, 0);
        assert_eq!(fs::read(&plan.target_exe).unwrap(), b"externally changed");
    }

    #[test]
    fn concurrent_install_attempt_is_rejected() {
        reset_update_started();
        mark_update_started().unwrap();
        assert_eq!(
            mark_update_started().unwrap_err(),
            "an update installation is already in progress"
        );
        reset_update_started();
    }

    #[test]
    fn cleanup_rejects_paths_outside_the_owned_temp_directory() {
        let unrelated = tempfile::tempdir().unwrap();
        assert_eq!(
            canonical_staging_dir(unrelated.path()).unwrap_err(),
            "the update staging directory is unsafe"
        );
        assert!(unrelated.path().exists());
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires live GitHub access and downloads the release EXE"]
    fn live_github_asset_download_contract_is_compatible() {
        let release = fetch_latest_release().unwrap();
        let asset = select_update_asset(&release).unwrap();
        let temp = Builder::new()
            .prefix(UPDATE_DIR_PREFIX)
            .tempdir_in(env::temp_dir())
            .unwrap();
        let staged = temp.path().join("downloaded.exe");
        download_asset(&asset, &staged).unwrap();
        assert_eq!(sha256_file(&staged).unwrap(), asset.sha256);
        assert_eq!(fs::metadata(staged).unwrap().len(), asset.size);
    }
}

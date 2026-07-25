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
    time::Duration,
};

const UPDATE_ASSET_NAME: &str = "codex-switch.exe";
const UPDATE_URL_PREFIX: &str = "https://github.com/mingisrookie/codex-switch/releases/download/";
const MAX_UPDATE_BYTES: u64 = 64 * 1024 * 1024;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
const UPDATE_DIR_PREFIX: &str = "codex-switch-update-";
const UPDATE_PLAN_SCHEMA: u32 = 1;
const APPLY_UPDATE_ARG: &str = "--codex-switch-apply-update";
const RECOVER_UPDATE_ARG: &str = "--codex-switch-recover-update";
const UPDATE_COMPLETE_ARG: &str = "--codex-switch-update-complete";
const UPDATE_ROLLED_BACK_ARG: &str = "--codex-switch-update-rolled-back";
const STARTUP_ACK_NAME: &str = "startup-ack";
const RECOVERY_READY_PREFIX: &str = "recovery-helper-ready-";
const UPDATE_JOURNAL_NAME: &str = "update-journal.json";
const UPDATE_JOURNAL_TEMP_NAME: &str = ".update-journal.tmp";
const UPDATE_JOURNAL_SCHEMA: u32 = 1;
const STARTUP_ACK_ATTEMPTS: usize = 150;
const STARTUP_ACK_INTERVAL: Duration = Duration::from_millis(100);
const HELPER_READY_ATTEMPTS: usize = 150;
const LEGACY_HELPER_ACK_GRACE: Duration = Duration::from_secs(16);
#[cfg(windows)]
const CLEANUP_LEASE_TIMEOUT_MS: u32 = 30_000;

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
    target_exe: PathBuf,
    plan: UpdatePlan,
    journal_present: bool,
    ack_path: PathBuf,
    ack_payload: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum UpdatePhase {
    Prepared,
    ParentStopped,
    ReplacementReady,
    BackupReady,
    Activated,
    Launching,
    Acked,
    RollingBack,
    RolledBack,
    Complete,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct UpdateJournal {
    schema_version: u32,
    phase: UpdatePhase,
    target_exe: PathBuf,
    replacement_exe: PathBuf,
    backup_exe: PathBuf,
    expected_old_sha256: String,
    expected_new_sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaunchStatus {
    Updated,
    RolledBack,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum StartupRecoveryAction {
    Resume(StartupUpdateContext),
    RestartHelper(UpdatePlan),
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
        return Some(if run_update_helper(&manifest, None).is_ok() {
            0
        } else {
            1
        });
    }
    if args.len() == 4 && args[1] == RECOVER_UPDATE_ARG {
        let manifest = PathBuf::from(&args[2]);
        let recovery_parent_pid = args[3].to_str()?.parse::<u32>().ok()?;
        return Some(
            if run_update_helper(&manifest, Some(recovery_parent_pid)).is_ok() {
                0
            } else {
                1
            },
        );
    }

    let current_exe = env::current_exe().ok()?;
    let mut context = startup_context_from_args(&args, &current_exe);
    if context.is_none() && args.len() == 1 {
        match discover_interrupted_update(&current_exe) {
            Some(StartupRecoveryAction::Resume(recovered)) => context = Some(recovered),
            Some(StartupRecoveryAction::RestartHelper(plan))
                if restart_interrupted_update(&plan).is_ok() =>
            {
                return Some(0);
            }
            Some(StartupRecoveryAction::RestartHelper(_)) | None => {}
        }
    }
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
    schedule_staging_cleanup(context);
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
    let journal_path = staging_dir.join(UPDATE_JOURNAL_NAME);
    let journal_present = journal_path.is_file();
    if journal_present {
        let journal: UpdateJournal = serde_json::from_slice(
            &fs::read(&journal_path)
                .map_err(|_| "failed to read the update startup journal".to_string())?,
        )
        .map_err(|_| "the update startup journal is invalid".to_string())?;
        validate_journal(&plan, &journal)?;
        validate_startup_journal_phase(status, journal.phase)?;
    }
    let ack_payload = startup_ack_payload(status, expected_sha256);
    Ok(StartupUpdateContext {
        status,
        target_exe: target,
        plan,
        journal_present,
        ack_path: staging_dir.join(STARTUP_ACK_NAME),
        ack_payload,
        staging_dir,
    })
}

#[cfg(windows)]
fn discover_interrupted_update(current_exe: &Path) -> Option<StartupRecoveryAction> {
    if !automatic_recovery_allowed(process_is_elevated()) {
        return None;
    }
    let temp = fs::canonicalize(env::temp_dir()).ok()?;
    let mut discovered = None;
    for entry in fs::read_dir(&temp).ok()? {
        let Ok(entry) = entry else {
            continue;
        };
        let name_matches = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(UPDATE_DIR_PREFIX));
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !name_matches || !file_type.is_dir() {
            continue;
        }
        let Ok(action) = recovery_action_from_staging(&entry.path(), current_exe) else {
            continue;
        };
        if discovered.is_some() {
            return None;
        }
        discovered = Some(action);
    }
    discovered
}

fn automatic_recovery_allowed(elevation: Result<bool, String>) -> bool {
    matches!(elevation, Ok(false))
}

#[cfg(not(windows))]
fn discover_interrupted_update(_current_exe: &Path) -> Option<StartupRecoveryAction> {
    None
}

fn recovery_action_from_staging(
    staging_dir: &Path,
    current_exe: &Path,
) -> Result<StartupRecoveryAction, String> {
    let staging_dir = canonical_staging_dir(staging_dir)?;
    let plan: UpdatePlan = serde_json::from_slice(
        &fs::read(staging_dir.join("update-plan.json"))
            .map_err(|_| "the interrupted update plan is missing".to_string())?,
    )
    .map_err(|_| "the interrupted update plan is invalid".to_string())?;
    let planned_staging = canonical_staging_dir(&plan.staging_dir)?;
    if !paths_equal(&planned_staging, &staging_dir) {
        return Err("the interrupted update staging directory does not match the plan".to_string());
    }
    validate_update_plan(&plan, &plan.helper_exe)?;
    let current = fs::canonicalize(current_exe)
        .map_err(|_| "failed to resolve the interrupted update target".to_string())?;
    let target = fs::canonicalize(&plan.target_exe)
        .map_err(|_| "the interrupted update target is missing".to_string())?;
    if !paths_equal(&current, &target) {
        return Err("the interrupted update belongs to another executable".to_string());
    }
    let journal: UpdateJournal = serde_json::from_slice(
        &fs::read(staging_dir.join(UPDATE_JOURNAL_NAME))
            .map_err(|_| "the interrupted update journal is missing".to_string())?,
    )
    .map_err(|_| "the interrupted update journal is invalid".to_string())?;
    validate_journal(&plan, &journal)?;

    let target_hash = sha256_file(&target)?;
    if target_hash == plan.expected_new_sha256 {
        if matches!(
            journal.phase,
            UpdatePhase::BackupReady
                | UpdatePhase::Activated
                | UpdatePhase::Launching
                | UpdatePhase::Acked
                | UpdatePhase::Complete
        ) {
            return validate_startup_context(&staging_dir, UpdateStartupStatus::Updated, &current)
                .map(StartupRecoveryAction::Resume);
        }
        if journal.phase == UpdatePhase::RollingBack {
            return Ok(StartupRecoveryAction::RestartHelper(plan));
        }
        return Err("the interrupted update phase does not match the installed executable".into());
    }
    if target_hash == plan.expected_old_sha256 {
        if journal.phase == UpdatePhase::RolledBack {
            return validate_startup_context(
                &staging_dir,
                UpdateStartupStatus::RolledBack,
                &current,
            )
            .map(StartupRecoveryAction::Resume);
        }
        if !matches!(journal.phase, UpdatePhase::Acked | UpdatePhase::Complete) {
            return Ok(StartupRecoveryAction::RestartHelper(plan));
        }
        return Err("the interrupted update phase does not match the installed executable".into());
    }
    Err("the interrupted update target hash is invalid".to_string())
}

#[cfg(windows)]
fn restart_interrupted_update(plan: &UpdatePlan) -> Result<(), String> {
    let recovery_parent_pid = std::process::id();
    let ready_path = helper_ready_path(&plan.staging_dir, Some(recovery_parent_pid));
    match fs::remove_file(&ready_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err("failed to reset the recovery helper readiness".to_string()),
    }
    let manifest_path = plan.staging_dir.join("update-plan.json");
    let mut helper = Command::new(&plan.helper_exe)
        .arg(RECOVER_UPDATE_ARG)
        .arg(&manifest_path)
        .arg(recovery_parent_pid.to_string())
        .spawn()
        .map_err(|_| "failed to restart the interrupted update helper".to_string())?;
    wait_for_helper_ready(&mut helper, &ready_path)
}

#[cfg(not(windows))]
fn restart_interrupted_update(_plan: &UpdatePlan) -> Result<(), String> {
    Err("self-update is not available on this platform".to_string())
}

fn startup_ack_payload(status: UpdateStartupStatus, expected_sha256: &str) -> String {
    let status = match status {
        UpdateStartupStatus::Updated => "updated",
        UpdateStartupStatus::RolledBack => "rolledBack",
    };
    format!("codex-switch-update-ack-v1\n{status}\n{expected_sha256}\n")
}

fn validate_startup_journal_phase(
    status: UpdateStartupStatus,
    phase: UpdatePhase,
) -> Result<(), String> {
    let matches_status = match status {
        UpdateStartupStatus::Updated => matches!(
            phase,
            UpdatePhase::BackupReady
                | UpdatePhase::Activated
                | UpdatePhase::Launching
                | UpdatePhase::Acked
                | UpdatePhase::Complete
        ),
        UpdateStartupStatus::RolledBack => {
            matches!(phase, UpdatePhase::RollingBack | UpdatePhase::RolledBack)
        }
    };
    if matches_status {
        Ok(())
    } else {
        Err("the update startup journal phase does not match the status".to_string())
    }
}

fn write_startup_ack(context: &StartupUpdateContext) -> Result<(), String> {
    let staging_dir = canonical_staging_dir(&context.staging_dir)?;
    if !paths_equal(&context.ack_path, &staging_dir.join(STARTUP_ACK_NAME)) {
        return Err("the update startup acknowledgement path is unsafe".to_string());
    }
    if context.ack_path.exists() {
        return if fs::read(&context.ack_path)
            .is_ok_and(|payload| payload == context.ack_payload.as_bytes())
        {
            Ok(())
        } else {
            Err("the update startup acknowledgement is invalid".to_string())
        };
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
    let (staging_dir, staging_guard) = create_staging_dir()?;
    let staged_exe = staging_dir.join("downloaded.exe");
    let helper_exe = staging_dir.join("updater-helper.exe");
    let manifest_path = staging_dir.join("update-plan.json");
    let ready_path = helper_ready_path(&staging_dir, None);

    let result = (|| {
        download_asset(&asset, &staged_exe)?;
        copy_file_synced(
            &target_exe,
            &helper_exe,
            "failed to stage the update helper",
        )?;
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

    drop(staging_guard);
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
        secure_random_hex()?
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
fn create_staging_dir() -> Result<(PathBuf, StagingGuard), String> {
    let elevated = process_is_elevated()?;
    let random = secure_random_hex()?;
    let path = env::temp_dir().join(format!(
        "{UPDATE_DIR_PREFIX}{}-{random}",
        std::process::id()
    ));
    create_staging_directory_with_restricted_dacl(&path, elevated)?;
    let canonical = canonical_staging_dir(&path)?;
    let guard = StagingGuard::open(&canonical)?;
    Ok((canonical, guard))
}

#[cfg(windows)]
fn process_is_elevated() -> Result<bool, String> {
    use std::{ffi::c_void, mem::size_of, ptr};
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY},
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    let mut token = ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err("failed to inspect the update process security context".to_string());
    }
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut returned = 0u32;
    let result = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast::<c_void>(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
    };
    unsafe {
        CloseHandle(token);
    }
    if result == 0 || returned != size_of::<TOKEN_ELEVATION>() as u32 {
        return Err("failed to inspect the update process elevation".to_string());
    }
    Ok(elevation.TokenIsElevated != 0)
}

#[cfg(windows)]
fn secure_random_hex() -> Result<String, String> {
    use std::ptr;
    use windows_sys::Win32::Security::Cryptography::{
        BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
    };

    let mut bytes = [0u8; 16];
    if unsafe {
        BCryptGenRandom(
            ptr::null_mut(),
            bytes.as_mut_ptr(),
            bytes.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    } < 0
    {
        return Err("failed to create a secure update staging name".to_string());
    }
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(windows)]
fn staging_sddl(elevated: bool) -> &'static str {
    if elevated {
        "O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"
    } else {
        "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;OW)"
    }
}

#[cfg(windows)]
fn create_staging_directory_with_restricted_dacl(
    path: &Path,
    elevated: bool,
) -> Result<(), String> {
    use std::{ffi::c_void, mem::size_of, os::windows::ffi::OsStrExt, ptr};
    use windows_sys::Win32::{
        Foundation::LocalFree, Security::SECURITY_ATTRIBUTES, Storage::FileSystem::CreateDirectoryW,
    };

    #[link(name = "advapi32")]
    extern "system" {
        fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
            string_security_descriptor: *const u16,
            string_sd_revision: u32,
            security_descriptor: *mut *mut c_void,
            security_descriptor_size: *mut u32,
        ) -> i32;
    }

    let sddl = staging_sddl(elevated)
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut descriptor = ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err("failed to create the update staging security descriptor".to_string());
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let path = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let created = unsafe { CreateDirectoryW(path.as_ptr(), &attributes) };
    unsafe {
        LocalFree(descriptor);
    }
    if created == 0 {
        return Err("failed to create the protected update staging directory".to_string());
    }
    Ok(())
}

#[cfg(windows)]
struct StagingGuard {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl StagingGuard {
    fn open(path: &Path) -> Result<Self, String> {
        use std::{os::windows::ffi::OsStrExt, ptr};
        use windows_sys::Win32::{
            Foundation::{GENERIC_READ, INVALID_HANDLE_VALUE},
            Storage::FileSystem::{
                CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_READ, FILE_SHARE_WRITE,
                OPEN_EXISTING,
            },
        };

        let path = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err("failed to lock the update staging directory".to_string());
        }
        Ok(Self { handle })
    }
}

#[cfg(windows)]
impl Drop for StagingGuard {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

#[cfg(windows)]
struct UpdateLease {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl UpdateLease {
    fn acquire(target: &Path, timeout_ms: u32) -> Result<Self, String> {
        use std::ptr;
        use windows_sys::Win32::{
            Foundation::{CloseHandle, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT},
            System::Threading::{CreateMutexW, WaitForSingleObject},
        };

        let name = lease_name_for_target(target)?
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let handle = unsafe { CreateMutexW(ptr::null(), 0, name.as_ptr()) };
        if handle.is_null() {
            return Err("failed to create the update target lease".to_string());
        }
        match unsafe { WaitForSingleObject(handle, timeout_ms) } {
            WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Self { handle }),
            WAIT_TIMEOUT => {
                unsafe {
                    CloseHandle(handle);
                }
                Err("another process is already updating this executable".to_string())
            }
            _ => {
                unsafe {
                    CloseHandle(handle);
                }
                Err("failed to acquire the update target lease".to_string())
            }
        }
    }
}

#[cfg(windows)]
impl Drop for UpdateLease {
    fn drop(&mut self) {
        use windows_sys::Win32::{Foundation::CloseHandle, System::Threading::ReleaseMutex};
        unsafe {
            ReleaseMutex(self.handle);
            CloseHandle(self.handle);
        }
    }
}

#[cfg(windows)]
fn lease_name_for_target(target: &Path) -> Result<String, String> {
    let canonical = fs::canonicalize(target)
        .map_err(|_| "failed to resolve the update lease target".to_string())?;
    let normalized = canonical
        .as_os_str()
        .to_string_lossy()
        .replace('/', "\\")
        .to_ascii_lowercase();
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    Ok(format!("Global\\CodexSwitchUpdate-{:x}", hasher.finalize()))
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

fn run_update_helper(manifest_path: &Path, recovery_parent_pid: Option<u32>) -> Result<(), String> {
    #[cfg(not(windows))]
    {
        let _ = (manifest_path, recovery_parent_pid);
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
        let _staging_guard = StagingGuard::open(&plan.staging_dir)?;
        let _lease = UpdateLease::acquire(&plan.target_exe, 0)?;
        let journal =
            load_helper_journal_with(&plan, recovery_parent_pid.is_none(), fetch_latest_release)?;
        let ready_path = helper_ready_path(&plan.staging_dir, recovery_parent_pid);

        if let Some(recovery_parent_pid) = recovery_parent_pid {
            let waiter = ParentProcessWaiter::open(recovery_parent_pid)?;
            write_helper_ready(&ready_path)?;
            waiter.wait()?;
            return apply_validated_update_plan_with(&plan, || Ok(()), launch_and_confirm);
        }

        let waiter = if journal.phase == UpdatePhase::Prepared {
            Some(ParentProcessWaiter::open(plan.parent_pid)?)
        } else {
            None
        };
        write_helper_ready(&ready_path)?;
        apply_validated_update_plan_with(
            &plan,
            || match waiter {
                Some(waiter) => waiter.wait(),
                None => Ok(()),
            },
            launch_and_confirm,
        )
    }
}

fn load_helper_journal_with<F>(
    plan: &UpdatePlan,
    create_if_missing: bool,
    fetch_release: F,
) -> Result<UpdateJournal, String>
where
    F: FnOnce() -> Result<ReleaseCandidate, String>,
{
    if plan.staging_dir.join(UPDATE_JOURNAL_NAME).exists() {
        return load_or_create_journal(plan);
    }
    if !create_if_missing {
        return Err("the interrupted update journal is missing".to_string());
    }
    validate_plan_release_binding(plan, &fetch_release()?)?;
    load_or_create_journal(plan)
}

fn validate_plan_release_binding(
    plan: &UpdatePlan,
    release: &ReleaseCandidate,
) -> Result<(), String> {
    let current = Version::parse(env!("CARGO_PKG_VERSION"))
        .map_err(|_| "current application version is invalid".to_string())?;
    if release.version <= current {
        return Err("the update plan is not bound to a newer GitHub release".to_string());
    }
    let asset = select_update_asset(release)?;
    if asset.sha256 != plan.expected_new_sha256
        || fs::metadata(&plan.staged_exe)
            .map(|metadata| metadata.len())
            .unwrap_or_default()
            != asset.size
    {
        return Err("the update plan does not match the current GitHub release".to_string());
    }
    Ok(())
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
    let mut journal = load_or_create_journal(plan)?;
    if journal.phase == UpdatePhase::Prepared {
        wait_for_parent()?;
        persist_phase(plan, &mut journal, UpdatePhase::ParentStopped)?;
    }

    let mut updated_launch_completed = false;
    let mut old_launch_completed = false;
    let result = (|| {
        let target_hash = sha256_file(&plan.target_exe)?;
        if target_hash != plan.expected_old_sha256 && target_hash != plan.expected_new_sha256 {
            return Err("the installed executable changed before update".to_string());
        }

        match journal.phase {
            UpdatePhase::Complete => {
                if target_hash == plan.expected_new_sha256 {
                    return Ok(());
                }
                return Err("the completed update target is invalid".to_string());
            }
            UpdatePhase::RolledBack => {
                if target_hash == plan.expected_old_sha256 {
                    return Err("the update was rolled back".to_string());
                }
                return Err("the rolled back update target is invalid".to_string());
            }
            UpdatePhase::RollingBack => {
                finish_rollback(plan, &mut journal, &mut launch)?;
                old_launch_completed = true;
                return Err("the interrupted update was rolled back".to_string());
            }
            UpdatePhase::Launching => {
                if target_hash == plan.expected_new_sha256
                    && startup_ack_matches(
                        plan,
                        UpdateStartupStatus::Updated,
                        &plan.expected_new_sha256,
                    )
                {
                    updated_launch_completed = true;
                    persist_phase(plan, &mut journal, UpdatePhase::Acked)?;
                    finalize_success(plan, &mut journal)?;
                    return Ok(());
                }
                finish_rollback(plan, &mut journal, &mut launch)?;
                old_launch_completed = true;
                return Err("the interrupted update was rolled back".to_string());
            }
            UpdatePhase::Acked => {
                if target_hash != plan.expected_new_sha256 {
                    return Err("the acknowledged update target is invalid".to_string());
                }
                updated_launch_completed = true;
                finalize_success(plan, &mut journal)?;
                return Ok(());
            }
            _ => {}
        }

        if target_hash == plan.expected_old_sha256 {
            prepare_replacement_and_backup(plan, &mut journal)?;
            atomic_replace_file(
                &journal.replacement_exe,
                &plan.target_exe,
                "failed to activate the replacement executable",
            )?;
            if sha256_file(&plan.target_exe)? != plan.expected_new_sha256 {
                return Err("activated replacement executable verification failed".to_string());
            }
            persist_phase(plan, &mut journal, UpdatePhase::Activated)?;
        } else if journal.phase != UpdatePhase::Activated {
            persist_phase(plan, &mut journal, UpdatePhase::Activated)?;
        }

        persist_phase(plan, &mut journal, UpdatePhase::Launching)?;
        match launch(
            &plan.target_exe,
            LaunchStatus::Updated,
            &plan.staging_dir,
            &plan.expected_new_sha256,
        ) {
            Ok(()) => {
                updated_launch_completed = true;
                persist_phase(plan, &mut journal, UpdatePhase::Acked)?;
                finalize_success(plan, &mut journal)
            }
            Err(error) => {
                finish_rollback(plan, &mut journal, &mut launch)?;
                old_launch_completed = true;
                Err(error)
            }
        }
    })();

    if result.is_err() && !updated_launch_completed && !old_launch_completed {
        let _ = emergency_restore_and_launch(plan, &mut journal, &mut launch);
    }
    result
}

fn prepare_replacement_and_backup(
    plan: &UpdatePlan,
    journal: &mut UpdateJournal,
) -> Result<(), String> {
    ensure_synced_copy(
        &plan.staged_exe,
        &journal.replacement_exe,
        &plan.expected_new_sha256,
        "failed to create the replacement executable",
    )?;
    persist_phase(plan, journal, UpdatePhase::ReplacementReady)?;
    ensure_synced_copy(
        &plan.target_exe,
        &journal.backup_exe,
        &plan.expected_old_sha256,
        "failed to persist the previous executable",
    )?;
    persist_phase(plan, journal, UpdatePhase::BackupReady)
}

fn finalize_success(plan: &UpdatePlan, journal: &mut UpdateJournal) -> Result<(), String> {
    if sha256_file(&plan.target_exe)? != plan.expected_new_sha256 {
        return Err("the updated executable changed before cleanup".to_string());
    }
    remove_verified_file(
        &journal.backup_exe,
        &plan.expected_old_sha256,
        "the update backup is invalid",
    )?;
    remove_verified_file(
        &journal.replacement_exe,
        &plan.expected_new_sha256,
        "the update replacement is invalid",
    )?;
    persist_phase(plan, journal, UpdatePhase::Complete)
}

fn finish_rollback<L>(
    plan: &UpdatePlan,
    journal: &mut UpdateJournal,
    launch: &mut L,
) -> Result<(), String>
where
    L: FnMut(&Path, LaunchStatus, &Path, &str) -> Result<(), String>,
{
    persist_phase(plan, journal, UpdatePhase::RollingBack)?;
    let target_hash = sha256_file(&plan.target_exe)?;
    if target_hash == plan.expected_new_sha256 {
        if sha256_file(&journal.backup_exe)? != plan.expected_old_sha256 {
            return Err("update failed and the previous executable backup is invalid".to_string());
        }
        atomic_replace_file(
            &journal.backup_exe,
            &plan.target_exe,
            "update failed and the previous executable could not be restored",
        )?;
    } else if target_hash != plan.expected_old_sha256 {
        return Err("update failed and the installed executable is invalid".to_string());
    }
    if sha256_file(&plan.target_exe)? != plan.expected_old_sha256 {
        return Err("update failed and the restored executable is invalid".to_string());
    }
    if !startup_ack_matches(
        plan,
        UpdateStartupStatus::RolledBack,
        &plan.expected_old_sha256,
    ) {
        launch(
            &plan.target_exe,
            LaunchStatus::RolledBack,
            &plan.staging_dir,
            &plan.expected_old_sha256,
        )
        .map_err(|_| "update failed and the restored version could not restart".to_string())?;
    }
    persist_phase(plan, journal, UpdatePhase::RolledBack)?;
    remove_verified_file(
        &journal.replacement_exe,
        &plan.expected_new_sha256,
        "the update replacement is invalid",
    )
}

fn emergency_restore_and_launch<L>(
    plan: &UpdatePlan,
    journal: &mut UpdateJournal,
    launch: &mut L,
) -> Result<(), String>
where
    L: FnMut(&Path, LaunchStatus, &Path, &str) -> Result<(), String>,
{
    let target_hash = sha256_file(&plan.target_exe)?;
    let _ = persist_phase(plan, journal, UpdatePhase::RollingBack);
    if target_hash == plan.expected_new_sha256 {
        if sha256_file(&journal.backup_exe)? != plan.expected_old_sha256 {
            return Err("the emergency update backup is invalid".to_string());
        }
        atomic_replace_file(
            &journal.backup_exe,
            &plan.target_exe,
            "failed to perform the emergency update rollback",
        )?;
    } else if target_hash != plan.expected_old_sha256 {
        return Err("the emergency update target is invalid".to_string());
    }
    if sha256_file(&plan.target_exe)? != plan.expected_old_sha256 {
        return Err("the emergency restored executable is invalid".to_string());
    }
    if !startup_ack_matches(
        plan,
        UpdateStartupStatus::RolledBack,
        &plan.expected_old_sha256,
    ) {
        launch(
            &plan.target_exe,
            LaunchStatus::RolledBack,
            &plan.staging_dir,
            &plan.expected_old_sha256,
        )?;
    }
    let _ = persist_phase(plan, journal, UpdatePhase::RolledBack);
    Ok(())
}

fn load_or_create_journal(plan: &UpdatePlan) -> Result<UpdateJournal, String> {
    let path = plan.staging_dir.join(UPDATE_JOURNAL_NAME);
    if path.exists() {
        let journal: UpdateJournal = serde_json::from_slice(
            &fs::read(&path).map_err(|_| "failed to read the update journal".to_string())?,
        )
        .map_err(|_| "the update journal is invalid".to_string())?;
        validate_journal(plan, &journal)?;
        return Ok(journal);
    }

    let (replacement_exe, backup_exe) = update_artifact_paths(plan)?;
    let journal = UpdateJournal {
        schema_version: UPDATE_JOURNAL_SCHEMA,
        phase: UpdatePhase::Prepared,
        target_exe: plan.target_exe.clone(),
        replacement_exe,
        backup_exe,
        expected_old_sha256: plan.expected_old_sha256.clone(),
        expected_new_sha256: plan.expected_new_sha256.clone(),
    };
    write_update_journal(plan, &journal)?;
    Ok(journal)
}

fn validate_journal(plan: &UpdatePlan, journal: &UpdateJournal) -> Result<(), String> {
    let (replacement_exe, backup_exe) = update_artifact_paths(plan)?;
    if journal.schema_version != UPDATE_JOURNAL_SCHEMA
        || !paths_equal(&journal.target_exe, &plan.target_exe)
        || !paths_equal(&journal.replacement_exe, &replacement_exe)
        || !paths_equal(&journal.backup_exe, &backup_exe)
        || journal.expected_old_sha256 != plan.expected_old_sha256
        || journal.expected_new_sha256 != plan.expected_new_sha256
    {
        return Err("the update journal does not match the update plan".to_string());
    }
    Ok(())
}

fn update_artifact_paths(plan: &UpdatePlan) -> Result<(PathBuf, PathBuf), String> {
    let target_parent = plan
        .target_exe
        .parent()
        .ok_or_else(|| "the update target directory is invalid".to_string())?;
    let target_name = plan
        .target_exe
        .file_name()
        .ok_or_else(|| "the update target filename is invalid".to_string())?
        .to_string_lossy();
    let mut hasher = Sha256::new();
    hasher.update(plan.staging_dir.as_os_str().to_string_lossy().as_bytes());
    let key = format!("{:x}", hasher.finalize());
    let suffix = &key[..16];
    Ok((
        target_parent.join(format!(".{target_name}.update-new-{suffix}")),
        target_parent.join(format!(".{target_name}.update-backup-{suffix}")),
    ))
}

fn persist_phase(
    plan: &UpdatePlan,
    journal: &mut UpdateJournal,
    phase: UpdatePhase,
) -> Result<(), String> {
    journal.phase = phase;
    write_update_journal(plan, journal)
}

fn write_update_journal(plan: &UpdatePlan, journal: &UpdateJournal) -> Result<(), String> {
    validate_journal(plan, journal)?;
    let payload = serde_json::to_vec(journal)
        .map_err(|_| "failed to encode the update journal".to_string())?;
    let temporary = plan.staging_dir.join(UPDATE_JOURNAL_TEMP_NAME);
    match fs::remove_file(&temporary) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err("failed to reset the update journal transaction".to_string()),
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|_| "failed to create the update journal transaction".to_string())?;
    file.write_all(&payload)
        .and_then(|_| file.sync_all())
        .map_err(|_| "failed to persist the update journal transaction".to_string())?;
    drop(file);
    atomic_replace_file(
        &temporary,
        &plan.staging_dir.join(UPDATE_JOURNAL_NAME),
        "failed to commit the update journal",
    )
}

fn ensure_synced_copy(
    source: &Path,
    destination: &Path,
    expected_sha256: &str,
    error: &str,
) -> Result<(), String> {
    if destination.exists() {
        if sha256_file(destination)? == expected_sha256 {
            return Ok(());
        }
        return Err(error.to_string());
    }
    copy_file_synced(source, destination, error)?;
    if sha256_file(destination)? != expected_sha256 {
        return Err(error.to_string());
    }
    Ok(())
}

fn copy_file_synced(source: &Path, destination: &Path, error: &str) -> Result<(), String> {
    let mut input = File::open(source).map_err(|_| error.to_string())?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(|_| error.to_string())?;
    std::io::copy(&mut input, &mut output).map_err(|_| error.to_string())?;
    output.sync_all().map_err(|_| error.to_string())
}

fn remove_verified_file(path: &Path, expected_sha256: &str, error: &str) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    if sha256_file(path)? != expected_sha256 {
        return Err(error.to_string());
    }
    fs::remove_file(path).map_err(|_| error.to_string())
}

fn startup_ack_matches(
    plan: &UpdatePlan,
    status: UpdateStartupStatus,
    expected_sha256: &str,
) -> bool {
    fs::read(plan.staging_dir.join(STARTUP_ACK_NAME))
        .is_ok_and(|payload| payload == startup_ack_payload(status, expected_sha256).as_bytes())
}

#[cfg(windows)]
fn atomic_replace_file(source: &Path, destination: &Path, error: &str) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(error.to_string());
    }
    Ok(())
}

#[cfg(not(windows))]
fn atomic_replace_file(source: &Path, destination: &Path, error: &str) -> Result<(), String> {
    if destination.exists() {
        fs::remove_file(destination).map_err(|_| error.to_string())?;
    }
    fs::rename(source, destination).map_err(|_| error.to_string())
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
    for _ in 0..HELPER_READY_ATTEMPTS {
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

fn helper_ready_path(staging_dir: &Path, recovery_parent_pid: Option<u32>) -> PathBuf {
    match recovery_parent_pid {
        Some(pid) => staging_dir.join(format!("{RECOVERY_READY_PREFIX}{pid}")),
        None => staging_dir.join("helper-ready"),
    }
}

#[cfg(windows)]
fn write_helper_ready(path: &Path) -> Result<(), String> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
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

fn schedule_staging_cleanup(context: StartupUpdateContext) {
    let Ok(staging_dir) = canonical_staging_dir(&context.staging_dir) else {
        return;
    };
    let cleanup_delay = startup_cleanup_delay(&staging_dir);
    thread::spawn(move || {
        thread::sleep(cleanup_delay);
        #[cfg(windows)]
        {
            let Ok(staging_guard) = StagingGuard::open(&staging_dir) else {
                return;
            };
            let Ok(_lease) = UpdateLease::acquire(&context.target_exe, CLEANUP_LEASE_TIMEOUT_MS)
            else {
                return;
            };
            if finalize_startup_cleanup(&context).is_err() {
                return;
            }
            drop(staging_guard);
            remove_staging_with_retries(&staging_dir);
        }
        #[cfg(not(windows))]
        {
            if finalize_startup_cleanup(&context).is_err() {
                return;
            }
            remove_staging_with_retries(&staging_dir);
        }
    });
}

fn startup_cleanup_delay(staging_dir: &Path) -> Duration {
    if staging_dir.join(UPDATE_JOURNAL_NAME).is_file() {
        Duration::ZERO
    } else {
        LEGACY_HELPER_ACK_GRACE
    }
}

fn remove_staging_with_retries(staging_dir: &Path) {
    for _ in 0..10 {
        if remove_staging_dir(staging_dir).is_ok() || !staging_dir.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(250));
    }
}

fn finalize_startup_cleanup(context: &StartupUpdateContext) -> Result<(), String> {
    let plan = &context.plan;
    let planned_staging = canonical_staging_dir(&plan.staging_dir)?;
    let planned_target = fs::canonicalize(&plan.target_exe)
        .map_err(|_| "the update cleanup target is missing".to_string())?;
    if !paths_equal(&planned_staging, &context.staging_dir)
        || !paths_equal(&planned_target, &context.target_exe)
    {
        return Err("the update cleanup context is invalid".to_string());
    }
    let expected = match context.status {
        UpdateStartupStatus::Updated => &plan.expected_new_sha256,
        UpdateStartupStatus::RolledBack => &plan.expected_old_sha256,
    };
    if sha256_file(&context.target_exe)? != expected.as_str() {
        return Err("the update cleanup target is invalid".to_string());
    }

    let journal_path = context.staging_dir.join(UPDATE_JOURNAL_NAME);
    if context.journal_present {
        let mut journal: UpdateJournal = serde_json::from_slice(
            &fs::read(&journal_path)
                .map_err(|_| "failed to read the update cleanup journal".to_string())?,
        )
        .map_err(|_| "the update cleanup journal is invalid".to_string())?;
        validate_journal(plan, &journal)?;
        remove_verified_file(
            &journal.backup_exe,
            &plan.expected_old_sha256,
            "the update cleanup backup is invalid",
        )?;
        remove_verified_file(
            &journal.replacement_exe,
            &plan.expected_new_sha256,
            "the update cleanup replacement is invalid",
        )?;
        let phase = match context.status {
            UpdateStartupStatus::Updated => UpdatePhase::Complete,
            UpdateStartupStatus::RolledBack => UpdatePhase::RolledBack,
        };
        persist_phase(plan, &mut journal, phase)?;
    }
    Ok(())
}

fn remove_staging_dir(path: &Path) -> Result<(), String> {
    let safe = canonical_staging_dir(path)?;
    let helper = safe.join("updater-helper.exe");
    if helper.exists() {
        fs::remove_file(&helper)
            .map_err(|_| "the update helper is still using the staging directory".to_string())?;
    }

    let mut files = fs::read_dir(&safe)
        .map_err(|_| "failed to inspect the update staging directory".to_string())?
        .map(|entry| {
            entry.map_err(|_| "failed to inspect the update staging directory".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    files.sort_by_key(|entry| entry.file_name() == STARTUP_ACK_NAME);
    for entry in files {
        let file_type = entry
            .file_type()
            .map_err(|_| "failed to inspect an update staging entry".to_string())?;
        if !file_type.is_file() {
            return Err("the update staging directory contains an unsafe entry".to_string());
        }
        fs::remove_file(entry.path())
            .map_err(|_| "failed to remove an update staging file".to_string())?;
    }
    fs::remove_dir(safe).map_err(|_| "failed to remove the update staging directory".to_string())
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

    #[cfg(windows)]
    fn file_dacl_evidence(path: &Path) -> Option<(bool, String, u32)> {
        use std::{ffi::c_void, mem::size_of, os::windows::ffi::OsStrExt, ptr, slice};
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::{
            AclSizeInformation, GetAce, GetAclInformation, GetFileSecurityW,
            GetSecurityDescriptorControl, GetSecurityDescriptorDacl, ACE_HEADER, ACL,
            ACL_SIZE_INFORMATION, DACL_SECURITY_INFORMATION, INHERITED_ACE, SE_DACL_PROTECTED,
        };

        #[link(name = "advapi32")]
        extern "system" {
            fn ConvertSecurityDescriptorToStringSecurityDescriptorW(
                security_descriptor: *mut c_void,
                requested_string_sd_revision: u32,
                security_information: u32,
                string_security_descriptor: *mut *mut u16,
                string_security_descriptor_len: *mut u32,
            ) -> i32;
        }

        let path = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let mut needed = 0u32;
        unsafe {
            GetFileSecurityW(
                path.as_ptr(),
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                0,
                &mut needed,
            );
        }
        if needed == 0 {
            return None;
        }
        let mut descriptor = vec![0u8; needed as usize];
        if unsafe {
            GetFileSecurityW(
                path.as_ptr(),
                DACL_SECURITY_INFORMATION,
                descriptor.as_mut_ptr().cast(),
                descriptor.len() as u32,
                &mut needed,
            )
        } == 0
        {
            return None;
        }

        let mut control = 0u16;
        let mut revision = 0u32;
        if unsafe {
            GetSecurityDescriptorControl(
                descriptor.as_mut_ptr().cast(),
                &mut control,
                &mut revision,
            )
        } == 0
        {
            return None;
        }
        let mut present = 0;
        let mut defaulted = 0;
        let mut dacl = ptr::null_mut::<ACL>();
        if unsafe {
            GetSecurityDescriptorDacl(
                descriptor.as_mut_ptr().cast(),
                &mut present,
                &mut dacl,
                &mut defaulted,
            )
        } == 0
            || present == 0
            || dacl.is_null()
        {
            return None;
        }
        let mut info = ACL_SIZE_INFORMATION::default();
        if unsafe {
            GetAclInformation(
                dacl,
                (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
                size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        } == 0
        {
            return None;
        }
        let inherited_aces = (0..info.AceCount)
            .filter(|index| {
                let mut ace = ptr::null_mut::<c_void>();
                (unsafe { GetAce(dacl, *index, &mut ace) }) != 0
                    && !ace.is_null()
                    && unsafe { (*(ace.cast::<ACE_HEADER>())).AceFlags as u32 & INHERITED_ACE != 0 }
            })
            .count() as u32;
        let mut sddl = ptr::null_mut::<u16>();
        let mut sddl_len = 0u32;
        if unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor.as_mut_ptr().cast(),
                1,
                DACL_SECURITY_INFORMATION,
                &mut sddl,
                &mut sddl_len,
            )
        } == 0
            || sddl.is_null()
        {
            return None;
        }
        let sddl_text =
            String::from_utf16_lossy(unsafe { slice::from_raw_parts(sddl, sddl_len as usize) });
        unsafe {
            LocalFree(sddl.cast());
        }
        Some((control & SE_DACL_PROTECTED != 0, sddl_text, inherited_aces))
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

    #[test]
    fn startup_context_accepts_only_a_matching_current_journal() {
        let (_temp, plan, _helper) = staged_plan();
        persist_plan(&plan);
        let mut journal = load_or_create_journal(&plan).unwrap();
        persist_phase(&plan, &mut journal, UpdatePhase::Launching).unwrap();
        fs::write(&plan.target_exe, b"new executable").unwrap();
        let args = vec![
            std::ffi::OsString::from("codex-switch.exe"),
            std::ffi::OsString::from(UPDATE_COMPLETE_ARG),
            plan.staging_dir.as_os_str().to_owned(),
        ];

        let context = startup_context_from_args(&args, &plan.target_exe).unwrap();
        assert!(context.journal_present);
        journal.backup_exe = plan.target_exe.with_extension("outside-journal");
        fs::write(
            plan.staging_dir.join(UPDATE_JOURNAL_NAME),
            serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();
        assert!(startup_context_from_args(&args, &plan.target_exe).is_none());
    }

    #[test]
    fn legacy_startup_without_a_journal_keeps_the_ack_grace_period() {
        let (_temp, plan, _helper) = staged_plan();
        let old_helper_ack_window = STARTUP_ACK_INTERVAL * STARTUP_ACK_ATTEMPTS as u32;
        assert!(
            startup_cleanup_delay(&plan.staging_dir)
                >= old_helper_ack_window + Duration::from_secs(1)
        );
        load_or_create_journal(&plan).unwrap();
        assert_eq!(startup_cleanup_delay(&plan.staging_dir), Duration::ZERO);
    }

    #[cfg(windows)]
    #[test]
    fn legacy_cleanup_preserves_ack_while_the_helper_is_locked() {
        use std::{os::windows::ffi::OsStrExt, ptr};
        use windows_sys::Win32::{
            Foundation::{CloseHandle, GENERIC_READ, INVALID_HANDLE_VALUE},
            Storage::FileSystem::{
                CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE,
                OPEN_EXISTING,
            },
        };

        let (_temp, plan, helper) = staged_plan();
        persist_plan(&plan);
        let ack = plan.staging_dir.join(STARTUP_ACK_NAME);
        fs::write(&ack, b"legacy helper still needs this ack").unwrap();
        let helper_wide = helper
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let helper_guard = unsafe {
            CreateFileW(
                helper_wide.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                ptr::null_mut(),
            )
        };
        assert_ne!(helper_guard, INVALID_HANDLE_VALUE);

        assert_eq!(
            remove_staging_dir(&plan.staging_dir).unwrap_err(),
            "the update helper is still using the staging directory"
        );
        assert!(ack.exists());
        assert!(plan.staged_exe.exists());
        assert!(plan.staging_dir.join("update-plan.json").exists());

        unsafe {
            CloseHandle(helper_guard);
        }
        remove_staging_dir(&plan.staging_dir).unwrap();
    }

    #[test]
    fn legacy_cleanup_ignores_a_journal_injected_after_startup_validation() {
        let (_temp, plan, _helper) = staged_plan();
        persist_plan(&plan);
        fs::write(&plan.target_exe, b"new executable").unwrap();
        let args = vec![
            std::ffi::OsString::from("codex-switch.exe"),
            std::ffi::OsString::from(UPDATE_COMPLETE_ARG),
            plan.staging_dir.as_os_str().to_owned(),
        ];
        let context = startup_context_from_args(&args, &plan.target_exe).unwrap();
        assert!(!context.journal_present);

        let victim = tempfile::NamedTempFile::new().unwrap();
        fs::write(victim.path(), b"old executable").unwrap();
        let mut injected_plan = plan.clone();
        injected_plan.target_exe = victim.path().to_path_buf();
        injected_plan.expected_old_sha256 = sha256_file(victim.path()).unwrap();
        let (replacement_exe, backup_exe) = update_artifact_paths(&injected_plan).unwrap();
        let injected_journal = UpdateJournal {
            schema_version: UPDATE_JOURNAL_SCHEMA,
            phase: UpdatePhase::Acked,
            target_exe: injected_plan.target_exe.clone(),
            replacement_exe,
            backup_exe,
            expected_old_sha256: injected_plan.expected_old_sha256.clone(),
            expected_new_sha256: injected_plan.expected_new_sha256.clone(),
        };
        fs::write(
            plan.staging_dir.join(UPDATE_JOURNAL_NAME),
            serde_json::to_vec(&injected_journal).unwrap(),
        )
        .unwrap();

        finalize_startup_cleanup(&context).unwrap();
        assert!(victim.path().exists());
        assert_eq!(fs::read(victim.path()).unwrap(), b"old executable");
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
    fn helper_recovers_an_activation_committed_before_the_journal_phase() {
        let (_temp, plan, helper) = staged_plan();
        let mut journal = load_or_create_journal(&plan).unwrap();
        persist_phase(&plan, &mut journal, UpdatePhase::ParentStopped).unwrap();
        prepare_replacement_and_backup(&plan, &mut journal).unwrap();
        atomic_replace_file(
            &journal.replacement_exe,
            &plan.target_exe,
            "injected activation failure",
        )
        .unwrap();
        assert_eq!(journal.phase, UpdatePhase::BackupReady);

        let mut launches = Vec::new();
        apply_update_plan_with(
            &plan,
            &helper,
            || panic!("a recovered helper must not wait for the original parent"),
            |target, status, _, expected_sha256| {
                launches.push(status);
                assert_eq!(sha256_file(target).unwrap(), expected_sha256);
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(launches, vec![LaunchStatus::Updated]);
        assert_eq!(fs::read(&plan.target_exe).unwrap(), b"new executable");
        assert_eq!(
            load_or_create_journal(&plan).unwrap().phase,
            UpdatePhase::Complete
        );
        assert!(!journal.backup_exe.exists());
    }

    #[test]
    fn helper_rolls_back_an_uncertain_launch_without_an_exact_ack() {
        let (_temp, plan, helper) = staged_plan();
        let mut journal = load_or_create_journal(&plan).unwrap();
        persist_phase(&plan, &mut journal, UpdatePhase::ParentStopped).unwrap();
        prepare_replacement_and_backup(&plan, &mut journal).unwrap();
        atomic_replace_file(
            &journal.replacement_exe,
            &plan.target_exe,
            "injected activation failure",
        )
        .unwrap();
        persist_phase(&plan, &mut journal, UpdatePhase::Launching).unwrap();

        let mut launches = Vec::new();
        let error = apply_update_plan_with(
            &plan,
            &helper,
            || panic!("a recovered helper must not wait for the original parent"),
            |target, status, _, expected_sha256| {
                launches.push(status);
                assert_eq!(status, LaunchStatus::RolledBack);
                assert_eq!(sha256_file(target).unwrap(), expected_sha256);
                Ok(())
            },
        )
        .unwrap_err();

        assert_eq!(error, "the interrupted update was rolled back");
        assert_eq!(launches, vec![LaunchStatus::RolledBack]);
        assert_eq!(fs::read(&plan.target_exe).unwrap(), b"old executable");
        assert_eq!(
            load_or_create_journal(&plan).unwrap().phase,
            UpdatePhase::RolledBack
        );
    }

    #[test]
    fn helper_accepts_a_durable_exact_ack_after_restart() {
        let (_temp, plan, helper) = staged_plan();
        let mut journal = load_or_create_journal(&plan).unwrap();
        persist_phase(&plan, &mut journal, UpdatePhase::ParentStopped).unwrap();
        prepare_replacement_and_backup(&plan, &mut journal).unwrap();
        atomic_replace_file(
            &journal.replacement_exe,
            &plan.target_exe,
            "injected activation failure",
        )
        .unwrap();
        persist_phase(&plan, &mut journal, UpdatePhase::Launching).unwrap();
        fs::write(
            plan.staging_dir.join(STARTUP_ACK_NAME),
            startup_ack_payload(UpdateStartupStatus::Updated, &plan.expected_new_sha256),
        )
        .unwrap();

        apply_update_plan_with(
            &plan,
            &helper,
            || panic!("a recovered helper must not wait for the original parent"),
            |_, _, _, _| panic!("an acknowledged update must not launch a duplicate process"),
        )
        .unwrap();

        assert_eq!(fs::read(&plan.target_exe).unwrap(), b"new executable");
        assert_eq!(
            load_or_create_journal(&plan).unwrap().phase,
            UpdatePhase::Complete
        );
    }

    #[test]
    fn journal_failure_after_activation_emergency_restores_and_launches_old_version() {
        let (_temp, plan, helper) = staged_plan();
        let mut journal = load_or_create_journal(&plan).unwrap();
        persist_phase(&plan, &mut journal, UpdatePhase::ParentStopped).unwrap();
        prepare_replacement_and_backup(&plan, &mut journal).unwrap();
        atomic_replace_file(
            &journal.replacement_exe,
            &plan.target_exe,
            "injected activation failure",
        )
        .unwrap();
        fs::create_dir(plan.staging_dir.join(UPDATE_JOURNAL_TEMP_NAME)).unwrap();

        let mut launches = Vec::new();
        let error = apply_update_plan_with(
            &plan,
            &helper,
            || panic!("a recovered helper must not wait for the original parent"),
            |target, status, _, expected_sha256| {
                launches.push(status);
                assert_eq!(status, LaunchStatus::RolledBack);
                assert_eq!(sha256_file(target).unwrap(), expected_sha256);
                Ok(())
            },
        )
        .unwrap_err();

        assert_eq!(error, "failed to reset the update journal transaction");
        assert_eq!(launches, vec![LaunchStatus::RolledBack]);
        assert_eq!(fs::read(&plan.target_exe).unwrap(), b"old executable");
    }

    #[test]
    fn tampered_journal_fails_closed_before_target_mutation() {
        let (_temp, plan, helper) = staged_plan();
        let mut journal = load_or_create_journal(&plan).unwrap();
        journal.backup_exe = plan.target_exe.with_extension("attacker-controlled");
        fs::write(
            plan.staging_dir.join(UPDATE_JOURNAL_NAME),
            serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();

        let error =
            apply_update_plan_with(&plan, &helper, || Ok(()), |_, _, _, _| Ok(())).unwrap_err();
        assert_eq!(error, "the update journal does not match the update plan");
        assert_eq!(fs::read(&plan.target_exe).unwrap(), b"old executable");
    }

    #[test]
    fn helper_rebinds_the_plan_to_the_current_fixed_github_digest() {
        let (_temp, plan, _helper) = staged_plan();
        let current = Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
        let next = Version::new(current.major, current.minor, current.patch + 1);
        let tag = format!("v{next}");
        let release = ReleaseCandidate {
            tag_name: tag.clone(),
            version: next,
            release_notes: None,
            assets: vec![asset(
                fs::metadata(&plan.staged_exe).unwrap().len(),
                Some(format!("sha256:{}", plan.expected_new_sha256)),
                &format!("{UPDATE_URL_PREFIX}{tag}/{UPDATE_ASSET_NAME}"),
            )],
        };

        validate_plan_release_binding(&plan, &release).unwrap();
        let mut tampered = plan.clone();
        tampered.expected_new_sha256 = "0".repeat(64);
        assert_eq!(
            validate_plan_release_binding(&tampered, &release).unwrap_err(),
            "the update plan does not match the current GitHub release"
        );
    }

    #[test]
    fn existing_journal_resumes_without_fetching_github() {
        let (_temp, plan, _helper) = staged_plan();
        let journal = load_or_create_journal(&plan).unwrap();
        let mut fetched = false;

        let loaded = load_helper_journal_with(&plan, true, || {
            fetched = true;
            Err("offline".to_string())
        })
        .unwrap();

        assert!(!fetched);
        assert_eq!(loaded, journal);
    }

    #[test]
    fn recovery_helper_requires_an_existing_journal_without_fetching_github() {
        let (_temp, plan, _helper) = staged_plan();
        let mut fetched = false;

        let error = load_helper_journal_with(&plan, false, || {
            fetched = true;
            Err("offline".to_string())
        })
        .unwrap_err();

        assert!(!fetched);
        assert_eq!(error, "the interrupted update journal is missing");
    }

    #[test]
    fn startup_recovery_accepts_new_target_phases_and_old_rolled_back_target() {
        for phase in [
            UpdatePhase::BackupReady,
            UpdatePhase::Activated,
            UpdatePhase::Launching,
            UpdatePhase::Acked,
            UpdatePhase::Complete,
        ] {
            let (_temp, plan, _helper) = staged_plan();
            persist_plan(&plan);
            let mut journal = load_or_create_journal(&plan).unwrap();
            persist_phase(&plan, &mut journal, phase).unwrap();
            fs::write(&plan.target_exe, b"new executable").unwrap();

            let action = recovery_action_from_staging(&plan.staging_dir, &plan.target_exe).unwrap();
            assert!(matches!(
                action,
                StartupRecoveryAction::Resume(StartupUpdateContext {
                    status: UpdateStartupStatus::Updated,
                    ..
                })
            ));
        }

        let (_temp, plan, _helper) = staged_plan();
        persist_plan(&plan);
        let mut journal = load_or_create_journal(&plan).unwrap();
        persist_phase(&plan, &mut journal, UpdatePhase::RolledBack).unwrap();
        let action = recovery_action_from_staging(&plan.staging_dir, &plan.target_exe).unwrap();
        assert!(matches!(
            action,
            StartupRecoveryAction::Resume(StartupUpdateContext {
                status: UpdateStartupStatus::RolledBack,
                ..
            })
        ));
    }

    #[test]
    fn startup_recovery_restarts_helper_for_old_nonterminal_target() {
        for phase in [
            UpdatePhase::Prepared,
            UpdatePhase::ReplacementReady,
            UpdatePhase::BackupReady,
            UpdatePhase::RollingBack,
        ] {
            let (_temp, plan, _helper) = staged_plan();
            persist_plan(&plan);
            let mut journal = load_or_create_journal(&plan).unwrap();
            persist_phase(&plan, &mut journal, phase).unwrap();

            let action = recovery_action_from_staging(&plan.staging_dir, &plan.target_exe).unwrap();
            assert!(matches!(action, StartupRecoveryAction::RestartHelper(_)));
        }
    }

    #[test]
    fn automatic_recovery_requires_a_confirmed_non_elevated_process() {
        assert!(automatic_recovery_allowed(Ok(false)));
        assert!(!automatic_recovery_allowed(Ok(true)));
        assert!(!automatic_recovery_allowed(Err(
            "security context unavailable".to_string()
        )));
    }

    #[test]
    fn startup_recovery_ignores_legacy_tampered_and_unrelated_staging() {
        let (_legacy_temp, legacy, _helper) = staged_plan();
        persist_plan(&legacy);
        assert!(recovery_action_from_staging(&legacy.staging_dir, &legacy.target_exe).is_err());

        let (_tampered_temp, tampered, _helper) = staged_plan();
        persist_plan(&tampered);
        load_or_create_journal(&tampered).unwrap();
        fs::write(tampered.staging_dir.join(UPDATE_JOURNAL_NAME), b"tampered").unwrap();
        assert!(recovery_action_from_staging(&tampered.staging_dir, &tampered.target_exe).is_err());

        let (_unrelated_temp, unrelated, _helper) = staged_plan();
        persist_plan(&unrelated);
        load_or_create_journal(&unrelated).unwrap();
        let other_target = tempfile::NamedTempFile::new().unwrap();
        assert!(recovery_action_from_staging(&unrelated.staging_dir, other_target.path()).is_err());
    }

    #[test]
    fn rollback_stays_in_progress_until_old_startup_is_acknowledged() {
        let (_temp, plan, _helper) = staged_plan();
        let mut journal = load_or_create_journal(&plan).unwrap();
        persist_phase(&plan, &mut journal, UpdatePhase::ParentStopped).unwrap();
        prepare_replacement_and_backup(&plan, &mut journal).unwrap();
        atomic_replace_file(
            &journal.replacement_exe,
            &plan.target_exe,
            "injected activation failure",
        )
        .unwrap();

        finish_rollback(&plan, &mut journal, &mut |_, status, _, _| {
            assert_eq!(status, LaunchStatus::RolledBack);
            assert_eq!(
                load_or_create_journal(&plan).unwrap().phase,
                UpdatePhase::RollingBack
            );
            Ok(())
        })
        .unwrap();

        assert_eq!(
            load_or_create_journal(&plan).unwrap().phase,
            UpdatePhase::RolledBack
        );
    }

    #[test]
    fn exact_rollback_ack_avoids_a_duplicate_old_process_launch() {
        let (_temp, plan, _helper) = staged_plan();
        let mut journal = load_or_create_journal(&plan).unwrap();
        persist_phase(&plan, &mut journal, UpdatePhase::ParentStopped).unwrap();
        prepare_replacement_and_backup(&plan, &mut journal).unwrap();
        persist_phase(&plan, &mut journal, UpdatePhase::RollingBack).unwrap();
        fs::write(
            plan.staging_dir.join(STARTUP_ACK_NAME),
            startup_ack_payload(UpdateStartupStatus::RolledBack, &plan.expected_old_sha256),
        )
        .unwrap();
        let mut launches = 0;

        finish_rollback(&plan, &mut journal, &mut |_, _, _, _| {
            launches += 1;
            Ok(())
        })
        .unwrap();

        assert_eq!(launches, 0);
        assert_eq!(
            load_or_create_journal(&plan).unwrap().phase,
            UpdatePhase::RolledBack
        );
    }

    #[test]
    fn v0_1_9_plan_args_and_ack_contract_remain_compatible() {
        let plan = UpdatePlan {
            schema_version: 1,
            parent_pid: 42,
            staging_dir: PathBuf::from("staging"),
            target_exe: PathBuf::from("target.exe"),
            helper_exe: PathBuf::from("helper.exe"),
            staged_exe: PathBuf::from("staged.exe"),
            expected_old_sha256: "a".repeat(64),
            expected_new_sha256: "b".repeat(64),
        };
        assert_eq!(
            serde_json::to_value(&plan).unwrap(),
            serde_json::json!({
                "schemaVersion": 1,
                "parentPid": 42,
                "stagingDir": "staging",
                "targetExe": "target.exe",
                "helperExe": "helper.exe",
                "stagedExe": "staged.exe",
                "expectedOldSha256": "a".repeat(64),
                "expectedNewSha256": "b".repeat(64),
            })
        );
        assert_eq!(APPLY_UPDATE_ARG, "--codex-switch-apply-update");
        assert_eq!(UPDATE_COMPLETE_ARG, "--codex-switch-update-complete");
        assert_eq!(UPDATE_ROLLED_BACK_ARG, "--codex-switch-update-rolled-back");
        assert_eq!(
            startup_ack_payload(UpdateStartupStatus::Updated, &"b".repeat(64)),
            format!("codex-switch-update-ack-v1\nupdated\n{}\n", "b".repeat(64))
        );
    }

    #[cfg(windows)]
    #[test]
    fn protected_staging_is_not_replaceable_while_guarded() {
        let (staging_dir, guard) = create_staging_dir().unwrap();
        let elevated = process_is_elevated().unwrap();
        let child = staging_dir.join("child-file");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&child)
            .unwrap();
        file.write_all(b"protected").unwrap();
        file.sync_all().unwrap();
        drop(file);
        assert_eq!(fs::read(&child).unwrap(), b"protected");
        OpenOptions::new()
            .append(true)
            .open(&child)
            .unwrap()
            .write_all(b"-writable")
            .unwrap();
        let (parent_protected, parent_sddl, _) = file_dacl_evidence(&staging_dir).unwrap();
        assert!(parent_protected);
        assert!(parent_sddl.starts_with("D:P"), "{parent_sddl}");
        let (_, child_sddl, _) = file_dacl_evidence(&child).unwrap();
        assert_eq!(child_sddl.matches("(A;").count(), 2, "{child_sddl}");
        assert!(child_sddl.contains("(A;;FA;;;SY)"), "{child_sddl}");
        let expected_principal = if elevated { "BA" } else { "OW" };
        assert!(
            child_sddl.contains(&format!("(A;;FA;;;{expected_principal})")),
            "{child_sddl}"
        );
        for forbidden_principal in ["WD", "BU", "AU", "AC"] {
            assert!(
                !child_sddl.contains(&format!(";;;{forbidden_principal})")),
                "{child_sddl}"
            );
        }
        assert!(fs::remove_dir(&staging_dir).is_err());
        assert!(!staging_sddl(true).contains(";;;OW"));
        assert!(staging_sddl(true).contains("(A;OICI;FA;;;SY)"));
        assert!(staging_sddl(true).contains("(A;OICI;FA;;;BA)"));
        assert!(staging_sddl(false).contains("(A;OICI;FA;;;OW)"));
        drop(guard);
        remove_staging_dir(&staging_dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn target_lease_probe_child() {
        let Some(target) = env::var_os("CODEX_SWITCH_UPDATE_LEASE_PROBE_TARGET") else {
            return;
        };
        let mode = env::var("CODEX_SWITCH_UPDATE_LEASE_PROBE_MODE").unwrap();
        let result = UpdateLease::acquire(Path::new(&target), 0);
        if mode == "blocked" {
            assert_eq!(
                result.map(|_lease| ()).unwrap_err(),
                "another process is already updating this executable"
            );
        } else {
            result.unwrap();
        }
    }

    #[cfg(windows)]
    #[test]
    fn target_lease_rejects_a_second_process_owner() {
        let target = tempfile::NamedTempFile::new().unwrap();
        let first = UpdateLease::acquire(target.path(), 0).unwrap();
        let blocked = Command::new(env::current_exe().unwrap())
            .arg("--exact")
            .arg("update_install::tests::target_lease_probe_child")
            .env("CODEX_SWITCH_UPDATE_LEASE_PROBE_TARGET", target.path())
            .env("CODEX_SWITCH_UPDATE_LEASE_PROBE_MODE", "blocked")
            .status()
            .unwrap();
        assert!(blocked.success());
        drop(first);

        let released = Command::new(env::current_exe().unwrap())
            .arg("--exact")
            .arg("update_install::tests::target_lease_probe_child")
            .env("CODEX_SWITCH_UPDATE_LEASE_PROBE_TARGET", target.path())
            .env("CODEX_SWITCH_UPDATE_LEASE_PROBE_MODE", "released")
            .status()
            .unwrap()
            .success();
        assert!(released);
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

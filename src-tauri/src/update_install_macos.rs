//! macOS preview updates are installed manually from a verified DMG.
//!
//! Keep the shared command/status interface without compiling the Windows
//! executable replacement and recovery protocol into the macOS application.

use serde::Serialize;

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

pub fn install_latest_update() -> Result<UpdateInstallReceipt, String> {
    Err("macOS 预览版请从 GitHub Release 下载新 DMG 手动更新".to_string())
}

pub fn startup_update_notice() -> Option<UpdateStartupNotice> {
    None
}

pub fn process_startup_update_args() -> Option<i32> {
    // Explicitly reject Windows helper invocations before the app starts;
    // they must never parse a manifest, replace files or acknowledge an update.
    std::env::args_os()
        .skip(1)
        .any(|argument| {
            matches!(
                argument.to_str(),
                Some(
                    "--codex-switch-apply-update"
                        | "--codex-switch-recover-update"
                        | "--codex-switch-update-complete"
                        | "--codex-switch-update-rolled-back"
                )
            )
        })
        .then_some(1)
}

pub fn acknowledge_update_startup() -> Result<(), String> {
    Ok(())
}

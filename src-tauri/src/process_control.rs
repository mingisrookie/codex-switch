use serde::Serialize;
use std::collections::{HashMap, HashSet};
#[cfg(any(windows, test))]
use std::sync::Mutex;
#[cfg(windows)]
use std::{
    ffi::OsString,
    fs,
    mem::size_of,
    os::windows::{ffi::OsStringExt, fs::MetadataExt, process::CommandExt},
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::Duration,
};
#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, GetLastError, ERROR_INSUFFICIENT_BUFFER, ERROR_NO_MORE_FILES, FILETIME,
        HANDLE, INVALID_HANDLE_VALUE,
    },
    Storage::{
        FileSystem::FILE_ATTRIBUTE_REPARSE_POINT, Packaging::Appx::GetApplicationUserModelId,
    },
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        },
        SystemInformation::GetSystemDirectoryW,
        Threading::{
            GetProcessTimes, OpenProcess, CREATE_NO_WINDOW, PROCESS_QUERY_LIMITED_INFORMATION,
        },
    },
};

#[cfg(windows)]
const GRACEFUL_CLOSE_POLL_ATTEMPTS: usize = 80;
#[cfg(windows)]
const GRACEFUL_CLOSE_POLL_INTERVAL: Duration = Duration::from_millis(100);
#[cfg(windows)]
const LAUNCH_VERIFY_POLL_ATTEMPTS: usize = 80;
#[cfg(windows)]
const LAUNCH_VERIFY_POLL_INTERVAL: Duration = Duration::from_millis(125);
#[cfg(windows)]
const MAX_APPLICATION_USER_MODEL_ID_LEN: u32 = 4_096;
#[cfg(any(windows, test))]
const TRUSTED_CHATGPT_AUMIDS: &[&str] = &[
    "OpenAI.ChatGPT-Desktop_2p2nqsd0c76g0!ChatGPT",
    "OpenAI.Codex_2p2nqsd0c76g0!App",
];

#[cfg(windows)]
static CHATGPT_LAUNCH_TARGET: Mutex<Option<String>> = Mutex::new(None);

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ChatGptLaunchStatus {
    Launched,
    AlreadyRunning,
    Failed,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChatGptLaunchResult {
    pub status: ChatGptLaunchStatus,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexProcess {
    pub image_name: String,
    pub pid: u32,
    pub parent_pid: u32,
    #[serde(skip)]
    pub(crate) creation_time_100ns: Option<u64>,
}

#[cfg(windows)]
pub fn list_codex_processes() -> Result<Vec<CodexProcess>, String> {
    list_codex_process_inventory().map(|(managed, _)| managed)
}

#[cfg(windows)]
pub fn list_standalone_codex_processes() -> Result<Vec<CodexProcess>, String> {
    list_codex_process_inventory().map(|(_, standalone)| standalone)
}

#[cfg(windows)]
pub fn list_codex_process_inventory() -> Result<(Vec<CodexProcess>, Vec<CodexProcess>), String> {
    let snapshot = snapshot_processes()?;
    Ok((
        managed_process_tree(&snapshot),
        standalone_codex_processes(&snapshot),
    ))
}

#[cfg(not(windows))]
pub fn list_codex_processes() -> Result<Vec<CodexProcess>, String> {
    Err("ChatGPT process control is not supported on this platform".to_string())
}

#[cfg(not(windows))]
pub fn list_standalone_codex_processes() -> Result<Vec<CodexProcess>, String> {
    Err("ChatGPT process control is not supported on this platform".to_string())
}

#[cfg(not(windows))]
pub fn list_codex_process_inventory() -> Result<(Vec<CodexProcess>, Vec<CodexProcess>), String> {
    Err("ChatGPT process control is not supported on this platform".to_string())
}

#[cfg(windows)]
pub fn cache_chatgpt_launch_target() -> Result<(), String> {
    let mut cached = CHATGPT_LAUNCH_TARGET
        .lock()
        .map_err(|_| "managed ChatGPT launch target is unavailable".to_string())?;
    *cached = None;
    let processes =
        snapshot_processes().map_err(|_| "managed ChatGPT launch target is unavailable")?;
    let target = resolve_launch_target_with(&processes, application_user_model_id)
        .map_err(LaunchTargetError::message)?;
    *cached = Some(target);
    Ok(())
}

#[cfg(not(windows))]
pub fn cache_chatgpt_launch_target() -> Result<(), String> {
    Err("ChatGPT launch is not supported on this platform".to_string())
}

#[cfg(windows)]
pub fn launch_cached_chatgpt() -> ChatGptLaunchResult {
    let target = match take_cached_launch_target_with(&CHATGPT_LAUNCH_TARGET, || {
        discover_registered_chatgpt_launch_target()
    }) {
        Ok(target) => target,
        Err(_) => {
            return ChatGptLaunchResult::failed(
                "The managed ChatGPT Windows app identity could not be discovered.",
            )
        }
    };
    launch_chatgpt_with(
        &target,
        snapshot_processes,
        application_user_model_id,
        || activate_chatgpt_application(&target),
        || thread::sleep(LAUNCH_VERIFY_POLL_INTERVAL),
        LAUNCH_VERIFY_POLL_ATTEMPTS,
    )
}

#[cfg(not(windows))]
pub fn launch_cached_chatgpt() -> ChatGptLaunchResult {
    ChatGptLaunchResult::failed("ChatGPT launch is not supported on this platform.")
}

impl ChatGptLaunchResult {
    #[cfg(any(windows, test))]
    fn launched() -> Self {
        Self {
            status: ChatGptLaunchStatus::Launched,
            message: None,
        }
    }

    #[cfg(any(windows, test))]
    fn already_running() -> Self {
        Self {
            status: ChatGptLaunchStatus::AlreadyRunning,
            message: None,
        }
    }

    fn failed(message: impl Into<String>) -> Self {
        Self {
            status: ChatGptLaunchStatus::Failed,
            message: Some(message.into()),
        }
    }
}

#[cfg(windows)]
pub fn close_codex_processes() -> Result<Vec<CodexProcess>, String> {
    let taskkill = system_tool_path("taskkill.exe")?;
    close_codex_processes_with(
        snapshot_processes,
        move |process, force| {
            let Ok(_identity_guard) = ProcessIdentityGuard::open(process) else {
                return TaskkillResult::Failed;
            };
            let pid = process.pid.to_string();
            let mut command = hidden_command(&taskkill);
            command.args(["/PID", &pid, "/T"]);
            if force {
                command.arg("/F");
            }
            match command.output() {
                Ok(output) if output.status.success() => TaskkillResult::Succeeded,
                Ok(_) | Err(_) => TaskkillResult::Failed,
            }
        },
        || thread::sleep(GRACEFUL_CLOSE_POLL_INTERVAL),
        GRACEFUL_CLOSE_POLL_ATTEMPTS,
    )
}

#[cfg(windows)]
fn hidden_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(program);
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

#[cfg(windows)]
struct SnapshotHandle(HANDLE);

#[cfg(windows)]
impl Drop for SnapshotHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
fn snapshot_processes() -> Result<Vec<CodexProcess>, String> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err("failed to create a Windows process snapshot".to_string());
    }
    let _snapshot = SnapshotHandle(snapshot);
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    if unsafe { Process32FirstW(snapshot, &mut entry) } == 0 {
        return if unsafe { GetLastError() } == ERROR_NO_MORE_FILES {
            Ok(Vec::new())
        } else {
            Err("failed to read the Windows process snapshot".to_string())
        };
    }

    let mut processes = Vec::new();
    loop {
        processes.push(CodexProcess {
            image_name: decode_process_name(&entry.szExeFile),
            pid: entry.th32ProcessID,
            parent_pid: entry.th32ParentProcessID,
            creation_time_100ns: None,
        });
        if unsafe { Process32NextW(snapshot, &mut entry) } != 0 {
            continue;
        }
        if unsafe { GetLastError() } == ERROR_NO_MORE_FILES {
            break;
        }
        return Err("failed to continue reading the Windows process snapshot".to_string());
    }
    let managed_pids = managed_process_tree(&processes)
        .into_iter()
        .map(|process| process.pid)
        .collect::<HashSet<_>>();
    for process in &mut processes {
        if managed_pids.contains(&process.pid) {
            process.creation_time_100ns = process_creation_time(process.pid);
        }
    }
    Ok(processes)
}

#[cfg(windows)]
struct ProcessIdentityGuard(HANDLE);

#[cfg(windows)]
impl ProcessIdentityGuard {
    fn open(process: &CodexProcess) -> Result<Self, ()> {
        let expected = process.creation_time_100ns.ok_or(())?;
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process.pid) };
        if handle.is_null() {
            return Err(());
        }
        let guard = Self(handle);
        if process_creation_time_from_handle(handle) != Some(expected) {
            return Err(());
        }
        Ok(guard)
    }
}

#[cfg(windows)]
impl Drop for ProcessIdentityGuard {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
fn process_creation_time(pid: u32) -> Option<u64> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }
    let guard = ProcessIdentityGuard(handle);
    let created = process_creation_time_from_handle(handle);
    drop(guard);
    created
}

#[cfg(windows)]
fn process_creation_time_from_handle(handle: HANDLE) -> Option<u64> {
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    if unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) } == 0 {
        return None;
    }
    Some((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

#[cfg(any(windows, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaunchTargetError {
    Missing,
    Conflict,
}

#[cfg(windows)]
impl LaunchTargetError {
    fn message(self) -> String {
        match self {
            Self::Missing => "managed ChatGPT launch target is unavailable".to_string(),
            Self::Conflict => "managed ChatGPT launch target is ambiguous".to_string(),
        }
    }
}

#[cfg(windows)]
fn application_user_model_id(process: &CodexProcess) -> Result<String, ()> {
    let guard = ProcessIdentityGuard::open(process)?;
    let mut length = 0_u32;
    let status = unsafe { GetApplicationUserModelId(guard.0, &mut length, std::ptr::null_mut()) };
    if status != ERROR_INSUFFICIENT_BUFFER
        || !(2..=MAX_APPLICATION_USER_MODEL_ID_LEN).contains(&length)
    {
        return Err(());
    }

    let mut buffer = vec![0_u16; length as usize];
    let status = unsafe { GetApplicationUserModelId(guard.0, &mut length, buffer.as_mut_ptr()) };
    if status != 0 || length < 2 || length as usize > buffer.len() {
        return Err(());
    }
    let used = length as usize;
    if buffer[used - 1] != 0 || buffer[..used - 1].contains(&0) {
        return Err(());
    }
    String::from_utf16(&buffer[..used - 1])
        .map_err(|_| ())
        .and_then(|value| validate_application_user_model_id(value).ok_or(()))
}

#[cfg(any(windows, test))]
fn validate_application_user_model_id(value: String) -> Option<String> {
    TRUSTED_CHATGPT_AUMIDS
        .iter()
        .any(|trusted| trusted.eq_ignore_ascii_case(&value))
        .then_some(value)
}

#[cfg(any(windows, test))]
fn resolve_registered_launch_target_with<Check>(
    mut is_registered: Check,
) -> Result<String, LaunchTargetError>
where
    Check: FnMut(&str) -> bool,
{
    let mut matches = TRUSTED_CHATGPT_AUMIDS
        .iter()
        .copied()
        .filter(|candidate| is_registered(candidate));
    let target = matches.next().ok_or(LaunchTargetError::Missing)?;
    if matches.next().is_some() {
        return Err(LaunchTargetError::Conflict);
    }
    Ok(target.to_string())
}

#[cfg(any(windows, test))]
fn take_cached_launch_target_with<Discover>(
    cache: &Mutex<Option<String>>,
    mut discover: Discover,
) -> Result<String, LaunchTargetError>
where
    Discover: FnMut() -> Result<String, LaunchTargetError>,
{
    let cached = cache.lock().map_err(|_| LaunchTargetError::Missing)?.take();
    match cached {
        Some(target) => {
            validate_application_user_model_id(target).ok_or(LaunchTargetError::Missing)
        }
        None => discover().and_then(|target| {
            validate_application_user_model_id(target).ok_or(LaunchTargetError::Missing)
        }),
    }
}

#[cfg(windows)]
fn registered_chatgpt_launch_targets() -> Result<Vec<String>, ()> {
    use windows::{core::HSTRING, ApplicationModel::AppInfo};

    Ok(TRUSTED_CHATGPT_AUMIDS
        .iter()
        .copied()
        .filter(|candidate| AppInfo::GetFromAppUserModelId(&HSTRING::from(*candidate)).is_ok())
        .map(str::to_string)
        .collect())
}

#[cfg(windows)]
fn discover_registered_chatgpt_launch_target() -> Result<String, LaunchTargetError> {
    let registered = registered_chatgpt_launch_targets().map_err(|_| LaunchTargetError::Missing)?;
    resolve_registered_launch_target_with(|candidate| {
        registered
            .iter()
            .any(|registered| registered.eq_ignore_ascii_case(candidate))
    })
}

#[cfg(any(windows, test))]
fn resolve_launch_target_with<Query>(
    processes: &[CodexProcess],
    mut query_aumid: Query,
) -> Result<String, LaunchTargetError>
where
    Query: FnMut(&CodexProcess) -> Result<String, ()>,
{
    let roots = managed_app_roots(processes);
    if roots.is_empty() {
        return Err(LaunchTargetError::Missing);
    }

    let mut resolved: Option<String> = None;
    for root in roots {
        let aumid = query_aumid(&root)
            .ok()
            .and_then(validate_application_user_model_id)
            .ok_or(LaunchTargetError::Missing)?;
        if let Some(existing) = resolved.as_ref() {
            if !existing.eq_ignore_ascii_case(&aumid) {
                return Err(LaunchTargetError::Conflict);
            }
        } else {
            resolved = Some(aumid);
        }
    }
    resolved.ok_or(LaunchTargetError::Missing)
}

#[cfg(any(windows, test))]
fn has_matching_managed_root_with<Query>(
    processes: &[CodexProcess],
    target: &str,
    query_aumid: &mut Query,
) -> Result<bool, ()>
where
    Query: FnMut(&CodexProcess) -> Result<String, ()>,
{
    let mut unverified_root = false;
    for root in managed_app_roots(processes) {
        match query_aumid(&root)
            .ok()
            .and_then(validate_application_user_model_id)
        {
            Some(aumid) if aumid.eq_ignore_ascii_case(target) => return Ok(true),
            Some(_) => {}
            None => unverified_root = true,
        }
    }
    if unverified_root {
        Err(())
    } else {
        Ok(false)
    }
}

#[cfg(any(windows, test))]
fn launch_chatgpt_with<List, Query, Activate, Wait>(
    target: &str,
    mut list_processes: List,
    mut query_aumid: Query,
    mut activate: Activate,
    mut wait: Wait,
    poll_attempts: usize,
) -> ChatGptLaunchResult
where
    List: FnMut() -> Result<Vec<CodexProcess>, String>,
    Query: FnMut(&CodexProcess) -> Result<String, ()>,
    Activate: FnMut() -> Result<(), ()>,
    Wait: FnMut(),
{
    let running = match list_processes() {
        Ok(processes) => has_matching_managed_root_with(&processes, target, &mut query_aumid),
        Err(_) => Err(()),
    };
    match running {
        Ok(true) => return ChatGptLaunchResult::already_running(),
        Ok(false) => {}
        Err(()) => {
            return ChatGptLaunchResult::failed(
                "The managed ChatGPT process inventory could not be verified.",
            )
        }
    }

    if activate().is_err() {
        return ChatGptLaunchResult::failed(
            "ChatGPT could not be activated using the captured Windows app identity.",
        );
    }

    for attempt in 0..poll_attempts {
        if let Ok(processes) = list_processes() {
            if has_matching_managed_root_with(&processes, target, &mut query_aumid) == Ok(true) {
                return ChatGptLaunchResult::launched();
            }
        }
        if attempt + 1 < poll_attempts {
            wait();
        }
    }
    ChatGptLaunchResult::failed(
        "ChatGPT activation did not produce a verified managed process before timeout.",
    )
}

#[cfg(windows)]
fn activate_chatgpt_application(aumid: &str) -> Result<(), ()> {
    use windows::{
        core::{IUnknown, PCWSTR},
        Win32::{
            Foundation::RPC_E_CHANGED_MODE,
            System::Com::{
                CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_LOCAL_SERVER,
                COINIT_APARTMENTTHREADED,
            },
            UI::Shell::{
                ApplicationActivationManager, IApplicationActivationManager, AO_NOERRORUI,
            },
        },
    };

    struct ComInitialization(bool);
    impl Drop for ComInitialization {
        fn drop(&mut self) {
            if self.0 {
                unsafe {
                    CoUninitialize();
                }
            }
        }
    }

    let initialized = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    if initialized.is_err() && initialized != RPC_E_CHANGED_MODE {
        return Err(());
    }
    let _initialization = ComInitialization(initialized.is_ok());
    let manager: IApplicationActivationManager = unsafe {
        CoCreateInstance(
            &ApplicationActivationManager,
            None::<&IUnknown>,
            CLSCTX_LOCAL_SERVER,
        )
    }
    .map_err(|_| ())?;
    let app_id = aumid
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let empty_arguments = [0_u16];
    unsafe {
        manager.ActivateApplication(
            PCWSTR(app_id.as_ptr()),
            PCWSTR(empty_arguments.as_ptr()),
            AO_NOERRORUI,
        )
    }
    .map(|_| ())
    .map_err(|_| ())
}

#[cfg(windows)]
fn system_tool_path(name: &str) -> Result<PathBuf, String> {
    if Path::new(name).file_name().and_then(|value| value.to_str()) != Some(name) {
        return Err("required Windows process-control tool is unavailable".to_string());
    }
    let mut buffer = vec![0_u16; 260];
    let system_dir = loop {
        let length =
            unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len().try_into().unwrap()) };
        if length == 0 {
            return Err("Windows system directory is unavailable".to_string());
        }
        let length = length as usize;
        if length < buffer.len() {
            break PathBuf::from(OsString::from_wide(&buffer[..length]));
        }
        buffer.resize(length.saturating_add(1), 0);
    };
    let system_dir = fs::canonicalize(system_dir)
        .map_err(|_| "Windows system directory is unavailable".to_string())?;
    let path = system_dir.join(name);
    let metadata = fs::symlink_metadata(&path)
        .map_err(|_| "required Windows process-control tool is unavailable".to_string())?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err("required Windows process-control tool is unavailable".to_string());
    }
    let path = fs::canonicalize(path)
        .map_err(|_| "required Windows process-control tool is unavailable".to_string())?;
    if path.parent() != Some(system_dir.as_path()) {
        return Err("required Windows process-control tool is unavailable".to_string());
    }
    Ok(path)
}

#[cfg(not(windows))]
pub fn close_codex_processes() -> Result<Vec<CodexProcess>, String> {
    Err("ChatGPT process control is not supported on this platform".to_string())
}

#[cfg(any(windows, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskkillResult {
    Succeeded,
    Failed,
}

#[cfg(any(windows, test))]
fn close_codex_processes_with<List, Kill, Wait>(
    mut list_processes: List,
    mut kill_process: Kill,
    mut wait: Wait,
    graceful_poll_attempts: usize,
) -> Result<Vec<CodexProcess>, String>
where
    List: FnMut() -> Result<Vec<CodexProcess>, String>,
    Kill: FnMut(&CodexProcess, bool) -> TaskkillResult,
    Wait: FnMut(),
{
    let processes = managed_process_tree(&list_processes()?);
    if processes.is_empty() {
        return Ok(processes);
    }
    for process in processes
        .iter()
        .filter(|process| is_managed_app_root(&process.image_name))
    {
        let _ = kill_process(process, false);
    }

    let mut managed_processes = processes
        .iter()
        .cloned()
        .map(|process| (process.pid, process))
        .collect::<HashMap<_, _>>();
    let mut survivors = managed_survivors(
        &list_processes()
            .map_err(|_| "failed to verify that ChatGPT processes exited".to_string())?,
        &mut managed_processes,
    );
    for _ in 0..graceful_poll_attempts {
        if survivors.is_empty() {
            return Ok(processes);
        }
        wait();
        survivors = managed_survivors(
            &list_processes()
                .map_err(|_| "failed to verify that ChatGPT processes exited".to_string())?,
            &mut managed_processes,
        );
    }

    if survivors.is_empty() {
        return Ok(processes);
    }
    let failed_pids = survivors
        .iter()
        .filter_map(|process| {
            if process.creation_time_100ns.is_none() {
                Some(process.pid)
            } else {
                (kill_process(process, true) == TaskkillResult::Failed).then_some(process.pid)
            }
        })
        .collect::<Vec<_>>();
    let survivors = managed_survivors(
        &list_processes()
            .map_err(|_| "failed to verify that ChatGPT processes exited".to_string())?,
        &mut managed_processes,
    );
    if survivors.is_empty() {
        return Ok(processes);
    }
    let survivor_pids = format_pids(survivors.iter().map(|process| process.pid));
    let failed_note = if failed_pids.is_empty() {
        String::new()
    } else {
        format!(
            "; taskkill failed for PID(s): {}",
            format_pids(failed_pids.into_iter())
        )
    };
    Err(format!(
        "failed to close all ChatGPT processes; still running PID(s): {survivor_pids}{failed_note}"
    ))
}

fn managed_survivors(
    snapshot: &[CodexProcess],
    managed_processes: &mut HashMap<u32, CodexProcess>,
) -> Vec<CodexProcess> {
    snapshot
        .iter()
        .filter_map(|process| {
            let managed = managed_processes.get(&process.pid)?;
            if managed.parent_pid != process.parent_pid
                || !managed.image_name.eq_ignore_ascii_case(&process.image_name)
            {
                return None;
            }
            match (managed.creation_time_100ns, process.creation_time_100ns) {
                (Some(expected), Some(observed)) if expected != observed => None,
                (Some(_), Some(_)) => Some(process.clone()),
                _ => {
                    let mut unverified = process.clone();
                    unverified.creation_time_100ns = None;
                    Some(unverified)
                }
            }
        })
        .collect()
}

#[cfg(any(windows, test))]
fn format_pids(pids: impl IntoIterator<Item = u32>) -> String {
    pids.into_iter()
        .map(|pid| pid.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn managed_process_tree(processes: &[CodexProcess]) -> Vec<CodexProcess> {
    let mut managed_pids = processes
        .iter()
        .filter(|process| is_managed_app_root(&process.image_name))
        .map(|process| process.pid)
        .collect::<HashSet<_>>();
    loop {
        let before = managed_pids.len();
        for process in processes {
            if managed_pids.contains(&process.parent_pid) {
                managed_pids.insert(process.pid);
            }
        }
        if managed_pids.len() == before {
            break;
        }
    }
    processes
        .iter()
        .filter(|process| managed_pids.contains(&process.pid))
        .cloned()
        .collect()
}

#[cfg(any(windows, test))]
fn managed_app_roots(processes: &[CodexProcess]) -> Vec<CodexProcess> {
    let by_pid = processes
        .iter()
        .map(|process| (process.pid, process))
        .collect::<HashMap<_, _>>();
    processes
        .iter()
        .filter(|process| is_managed_app_root(&process.image_name))
        .filter(|process| {
            let mut visited = HashSet::new();
            let mut parent_pid = process.parent_pid;
            while parent_pid != 0 && visited.insert(parent_pid) {
                let Some(parent) = by_pid.get(&parent_pid) else {
                    break;
                };
                if is_managed_app_root(&parent.image_name) {
                    return false;
                }
                parent_pid = parent.parent_pid;
            }
            true
        })
        .cloned()
        .collect()
}

fn standalone_codex_processes(processes: &[CodexProcess]) -> Vec<CodexProcess> {
    let managed_pids = managed_process_tree(processes)
        .into_iter()
        .map(|process| process.pid)
        .collect::<HashSet<_>>();
    processes
        .iter()
        .filter(|process| {
            process.image_name.eq_ignore_ascii_case("codex.exe")
                && !managed_pids.contains(&process.pid)
        })
        .cloned()
        .collect()
}

#[cfg(any(windows, test))]
fn decode_process_name(buffer: &[u16]) -> String {
    let end = buffer
        .iter()
        .position(|character| *character == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..end])
}

fn is_managed_app_root(image_name: &str) -> bool {
    let lower = image_name.to_ascii_lowercase();
    matches!(lower.as_str(), "chatgpt.exe" | "openai.codex.exe")
}

#[cfg(test)]
mod tests {
    use super::{
        close_codex_processes_with, decode_process_name, launch_chatgpt_with, managed_app_roots,
        managed_process_tree, resolve_launch_target_with, resolve_registered_launch_target_with,
        standalone_codex_processes, take_cached_launch_target_with, ChatGptLaunchStatus,
        CodexProcess, LaunchTargetError, TaskkillResult, TRUSTED_CHATGPT_AUMIDS,
    };
    #[cfg(windows)]
    use super::{
        discover_registered_chatgpt_launch_target, registered_chatgpt_launch_targets,
        system_tool_path, GRACEFUL_CLOSE_POLL_ATTEMPTS, GRACEFUL_CLOSE_POLL_INTERVAL,
    };
    use std::{
        cell::{Cell, RefCell},
        collections::VecDeque,
        fs,
        sync::Mutex,
    };

    fn process(image_name: &str, pid: u32) -> CodexProcess {
        child_process(image_name, pid, 0)
    }

    fn child_process(image_name: &str, pid: u32, parent_pid: u32) -> CodexProcess {
        CodexProcess {
            image_name: image_name.to_string(),
            pid,
            parent_pid,
            creation_time_100ns: Some(u64::from(pid) * 10),
        }
    }

    #[test]
    fn managed_tree_includes_roots_and_descendants_without_standalone_codex_or_switch() {
        let chatgpt = process("ChatGPT.exe", 1000);
        let codex_child = child_process("codex.exe", 2000, 1000);
        let host_grandchild = child_process("codex-code-mode-host.exe", 4000, 2000);
        let legacy_root = process("OpenAI.Codex.exe", 3000);
        let standalone_codex = child_process("codex.exe", 5000, 42);
        let codex_switch = child_process("codex-switch.exe", 6000, 42);
        let unrelated = child_process("other.exe", 7000, 42);

        assert_eq!(
            managed_process_tree(&[
                host_grandchild.clone(),
                standalone_codex,
                codex_switch,
                unrelated,
                codex_child.clone(),
                legacy_root.clone(),
                chatgpt.clone(),
            ]),
            vec![host_grandchild, codex_child, legacy_root, chatgpt,]
        );
    }

    #[test]
    fn standalone_codex_detection_excludes_managed_descendants_and_never_targets_them() {
        let chatgpt = process("ChatGPT.exe", 1000);
        let managed_codex = child_process("codex.exe", 2000, 1000);
        let standalone_codex = child_process("CoDeX.ExE", 3000, 42);
        let codex_switch = child_process("codex-switch.exe", 4000, 42);

        assert_eq!(
            standalone_codex_processes(&[
                managed_codex,
                standalone_codex.clone(),
                codex_switch,
                chatgpt,
            ]),
            vec![standalone_codex]
        );
    }

    #[test]
    fn launch_target_roots_exclude_managed_descendants() {
        let root = process("ChatGPT.exe", 1000);
        let child = child_process("ChatGPT.exe", 2000, 1000);
        let host = child_process("other.exe", 3000, 2000);
        let grandchild = child_process("OpenAI.Codex.exe", 4000, 3000);
        let independent = child_process("OpenAI.Codex.exe", 5000, 42);

        assert_eq!(
            managed_app_roots(&[grandchild, independent.clone(), host, child, root.clone(),]),
            vec![independent, root]
        );
    }

    #[test]
    fn launch_target_accepts_one_identity_across_multiple_roots() {
        let processes = vec![
            process("ChatGPT.exe", 1000),
            process("OpenAI.Codex.exe", 2000),
        ];

        let target =
            resolve_launch_target_with(&processes, |_| Ok(TRUSTED_CHATGPT_AUMIDS[1].to_string()))
                .unwrap();

        assert_eq!(target, TRUSTED_CHATGPT_AUMIDS[1]);
    }

    #[test]
    fn launch_target_fails_closed_for_missing_or_conflicting_identities() {
        assert_eq!(
            resolve_launch_target_with(&[], |_| Ok("unexpected".to_string())),
            Err(LaunchTargetError::Missing)
        );
        assert_eq!(
            resolve_launch_target_with(&[process("ChatGPT.exe", 1000)], |_| Err(())),
            Err(LaunchTargetError::Missing)
        );
        assert_eq!(
            resolve_launch_target_with(
                &[
                    process("ChatGPT.exe", 1000),
                    process("OpenAI.Codex.exe", 2000),
                ],
                |process| {
                    Ok(if process.pid == 1000 {
                        TRUSTED_CHATGPT_AUMIDS[0]
                    } else {
                        TRUSTED_CHATGPT_AUMIDS[1]
                    }
                    .to_string())
                },
            ),
            Err(LaunchTargetError::Conflict)
        );
    }

    #[test]
    fn registered_launch_discovery_accepts_one_trusted_identity_and_rejects_ambiguity() {
        assert_eq!(
            resolve_registered_launch_target_with(|candidate| {
                candidate.eq_ignore_ascii_case(TRUSTED_CHATGPT_AUMIDS[1])
            })
            .unwrap(),
            TRUSTED_CHATGPT_AUMIDS[1]
        );
        assert_eq!(
            resolve_registered_launch_target_with(|_| false),
            Err(LaunchTargetError::Missing)
        );
        assert_eq!(
            resolve_registered_launch_target_with(|_| true),
            Err(LaunchTargetError::Conflict)
        );
    }

    #[test]
    fn cached_launch_target_is_consumed_and_discovery_results_are_not_reused() {
        let cache = Mutex::new(Some(TRUSTED_CHATGPT_AUMIDS[1].to_string()));
        let discoveries = Cell::new(0);

        let captured = take_cached_launch_target_with(&cache, || {
            discoveries.set(discoveries.get() + 1);
            Ok(TRUSTED_CHATGPT_AUMIDS[0].to_string())
        })
        .unwrap();
        let first_discovery = take_cached_launch_target_with(&cache, || {
            discoveries.set(discoveries.get() + 1);
            Ok(TRUSTED_CHATGPT_AUMIDS[0].to_string())
        })
        .unwrap();
        let second_discovery = take_cached_launch_target_with(&cache, || {
            discoveries.set(discoveries.get() + 1);
            Ok(TRUSTED_CHATGPT_AUMIDS[1].to_string())
        })
        .unwrap();

        assert_eq!(captured, TRUSTED_CHATGPT_AUMIDS[1]);
        assert_eq!(first_discovery, TRUSTED_CHATGPT_AUMIDS[0]);
        assert_eq!(second_discovery, TRUSTED_CHATGPT_AUMIDS[1]);
        assert_eq!(discoveries.get(), 2);
        assert_eq!(*cache.lock().unwrap(), None);
    }

    #[test]
    fn cached_or_discovered_launch_target_must_remain_trusted() {
        let stale_cache = Mutex::new(Some("Untrusted.App_123!App".to_string()));
        assert_eq!(
            take_cached_launch_target_with(&stale_cache, || {
                Ok(TRUSTED_CHATGPT_AUMIDS[1].to_string())
            }),
            Err(LaunchTargetError::Missing)
        );

        let empty_cache = Mutex::new(None);
        assert_eq!(
            take_cached_launch_target_with(&empty_cache, || {
                Ok("Untrusted.App_123!App".to_string())
            }),
            Err(LaunchTargetError::Missing)
        );
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "queries current-user AppInfo registrations without launching or closing ChatGPT"]
    fn live_discovers_trusted_chatgpt_appinfo_without_process_mutation() {
        let registered = registered_chatgpt_launch_targets()
            .expect("current-user AppInfo registration discovery should complete");
        assert!(registered.iter().all(|candidate| {
            TRUSTED_CHATGPT_AUMIDS
                .iter()
                .any(|trusted| trusted.eq_ignore_ascii_case(candidate))
        }));

        let discovered = discover_registered_chatgpt_launch_target();
        match registered.as_slice() {
            [] => assert_eq!(discovered, Err(LaunchTargetError::Missing)),
            [target] => assert_eq!(discovered.as_deref(), Ok(target.as_str())),
            _ => assert_eq!(discovered, Err(LaunchTargetError::Conflict)),
        }
    }

    #[test]
    fn launch_returns_already_running_without_activation() {
        let activations = RefCell::new(0);
        let result = launch_chatgpt_with(
            TRUSTED_CHATGPT_AUMIDS[1],
            || Ok(vec![process("ChatGPT.exe", 1000)]),
            |_| Ok(TRUSTED_CHATGPT_AUMIDS[1].to_ascii_lowercase()),
            || {
                *activations.borrow_mut() += 1;
                Ok(())
            },
            || panic!("already-running detection must not wait"),
            2,
        );

        assert_eq!(result.status, ChatGptLaunchStatus::AlreadyRunning);
        assert_eq!(result.message, None);
        assert_eq!(*activations.borrow(), 0);
    }

    #[test]
    fn cached_noop_launch_returns_already_running_without_rediscovery_or_activation() {
        let cache = Mutex::new(Some(TRUSTED_CHATGPT_AUMIDS[1].to_string()));
        let target = take_cached_launch_target_with(&cache, || {
            panic!("a captured no-op launch target must be consumed before discovery")
        })
        .unwrap();
        let activations = Cell::new(0);

        let result = launch_chatgpt_with(
            &target,
            || Ok(vec![process("OpenAI.Codex.exe", 1000)]),
            |_| Ok(TRUSTED_CHATGPT_AUMIDS[1].to_string()),
            || {
                activations.set(activations.get() + 1);
                Ok(())
            },
            || panic!("already-running no-op launch must not wait"),
            2,
        );

        assert_eq!(result.status, ChatGptLaunchStatus::AlreadyRunning);
        assert_eq!(result.message, None);
        assert_eq!(activations.get(), 0);
        assert_eq!(*cache.lock().unwrap(), None);
    }

    #[test]
    fn launch_reports_activation_failure_without_exposing_details() {
        let result = launch_chatgpt_with(
            TRUSTED_CHATGPT_AUMIDS[1],
            || Ok(Vec::new()),
            |_| Err(()),
            || Err(()),
            || panic!("activation failure must not poll"),
            2,
        );

        assert_eq!(result.status, ChatGptLaunchStatus::Failed);
        assert_eq!(
            result.message.as_deref(),
            Some("ChatGPT could not be activated using the captured Windows app identity.")
        );
    }

    #[test]
    fn launch_waits_for_a_matching_managed_root() {
        let listings = RefCell::new(VecDeque::from([
            Ok(Vec::new()),
            Ok(vec![child_process("codex.exe", 2000, 42)]),
            Ok(vec![process("ChatGPT.exe", 1000)]),
        ]));
        let activations = RefCell::new(0);
        let waits = RefCell::new(0);

        let result = launch_chatgpt_with(
            TRUSTED_CHATGPT_AUMIDS[1],
            || {
                listings
                    .borrow_mut()
                    .pop_front()
                    .expect("unexpected listing")
            },
            |_| Ok(TRUSTED_CHATGPT_AUMIDS[1].to_string()),
            || {
                *activations.borrow_mut() += 1;
                Ok(())
            },
            || *waits.borrow_mut() += 1,
            2,
        );

        assert_eq!(result.status, ChatGptLaunchStatus::Launched);
        assert_eq!(result.message, None);
        assert_eq!(*activations.borrow(), 1);
        assert_eq!(*waits.borrow(), 1);
        assert!(listings.borrow().is_empty());
    }

    #[test]
    fn launch_timeout_is_typed_and_bounded() {
        let listings = RefCell::new(VecDeque::from([
            Ok(Vec::new()),
            Ok(Vec::new()),
            Ok(Vec::new()),
        ]));
        let waits = RefCell::new(0);

        let result = launch_chatgpt_with(
            TRUSTED_CHATGPT_AUMIDS[1],
            || {
                listings
                    .borrow_mut()
                    .pop_front()
                    .expect("unexpected listing")
            },
            |_| Err(()),
            || Ok(()),
            || *waits.borrow_mut() += 1,
            2,
        );

        assert_eq!(result.status, ChatGptLaunchStatus::Failed);
        assert_eq!(
            result.message.as_deref(),
            Some("ChatGPT activation did not produce a verified managed process before timeout.")
        );
        assert_eq!(*waits.borrow(), 1);
        assert!(listings.borrow().is_empty());
    }

    #[test]
    fn launch_result_serializes_status_in_camel_case_and_null_message() {
        let result = launch_chatgpt_with(
            TRUSTED_CHATGPT_AUMIDS[1],
            || Ok(vec![process("ChatGPT.exe", 1000)]),
            |_| Ok(TRUSTED_CHATGPT_AUMIDS[1].to_string()),
            || panic!("already-running detection must not activate"),
            || panic!("already-running detection must not wait"),
            1,
        );

        assert_eq!(
            serde_json::to_value(result).unwrap(),
            serde_json::json!({
                "status": "alreadyRunning",
                "message": null,
            })
        );
    }

    #[test]
    fn decodes_null_terminated_windows_process_names() {
        let mut buffer = [0_u16; 260];
        let encoded = "ChatGPT.exe".encode_utf16().collect::<Vec<_>>();
        buffer[..encoded.len()].copy_from_slice(&encoded);

        assert_eq!(decode_process_name(&buffer), "ChatGPT.exe");
    }

    #[test]
    #[cfg(windows)]
    fn system_tool_path_ignores_forged_system_root() {
        const CHILD_MARKER: &str = "CODEX_SWITCH_SYSTEM_TOOL_PATH_CHILD";
        if std::env::var_os(CHILD_MARKER).is_some() {
            let forged_root = fs::canonicalize(std::env::var_os("SystemRoot").unwrap()).unwrap();
            let tool = system_tool_path("taskkill.exe").unwrap();
            assert!(tool.is_absolute());
            assert!(!tool.starts_with(forged_root));
            return;
        }

        let forged_root = tempfile::tempdir().unwrap();
        let forged_system32 = forged_root.path().join("System32");
        fs::create_dir_all(&forged_system32).unwrap();
        fs::write(forged_system32.join("taskkill.exe"), b"not a system tool").unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("process_control::tests::system_tool_path_ignores_forged_system_root")
            .arg("--exact")
            .env("SystemRoot", forged_root.path())
            .env(CHILD_MARKER, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child test failed:\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    #[cfg(windows)]
    fn graceful_close_waits_eight_seconds_before_force_fallback() {
        assert_eq!(
            GRACEFUL_CLOSE_POLL_INTERVAL.as_millis() * GRACEFUL_CLOSE_POLL_ATTEMPTS as u128,
            8_000
        );
    }

    #[test]
    fn graceful_close_targets_roots_without_forcing_or_killing_standalone_codex() {
        let root = process("ChatGPT.exe", 1234);
        let child = child_process("codex.exe", 5678, 1234);
        let standalone = child_process("codex.exe", 9012, 42);
        let initial_snapshot = vec![standalone.clone(), child.clone(), root.clone()];
        let managed = vec![child, root];
        let listings = RefCell::new(VecDeque::from([Ok(initial_snapshot), Ok(vec![standalone])]));
        let killed = RefCell::new(Vec::new());
        let waits = RefCell::new(0);

        let result = close_codex_processes_with(
            || {
                listings
                    .borrow_mut()
                    .pop_front()
                    .expect("unexpected listing")
            },
            |process, force| {
                killed.borrow_mut().push((process.pid, force));
                TaskkillResult::Succeeded
            },
            || *waits.borrow_mut() += 1,
            2,
        )
        .expect("the managed process tree should be confirmed closed");

        assert_eq!(result, managed);
        assert_eq!(*killed.borrow(), vec![(1234, false)]);
        assert_eq!(*waits.borrow(), 0);
        assert!(listings.borrow().is_empty());
    }

    #[test]
    fn changed_image_or_parent_for_the_same_pid_is_not_forced() {
        let root = process("ChatGPT.exe", 1234);
        let codex_child = child_process("codex.exe", 5678, 1234);
        let host_child = child_process("codex-code-mode-host.exe", 9012, 5678);
        let initial_snapshot = vec![root, codex_child, host_child];
        let reused_and_reparented = vec![
            child_process("codex.exe", 5678, 42),
            child_process("other.exe", 9012, 5678),
        ];
        let listings = RefCell::new(VecDeque::from([
            Ok(initial_snapshot),
            Ok(reused_and_reparented),
        ]));
        let killed = RefCell::new(Vec::new());

        close_codex_processes_with(
            || {
                listings
                    .borrow_mut()
                    .pop_front()
                    .expect("unexpected listing")
            },
            |process, force| {
                killed.borrow_mut().push((process.pid, force));
                TaskkillResult::Succeeded
            },
            || panic!("unproven reused processes must not enter the grace poll"),
            2,
        )
        .unwrap();

        assert_eq!(*killed.borrow(), vec![(1234, false)]);
        assert!(listings.borrow().is_empty());
    }

    #[test]
    fn same_pid_parent_and_image_with_new_creation_time_is_not_forced() {
        let original = process("ChatGPT.exe", 1234);
        let mut reused = original.clone();
        reused.creation_time_100ns = Some(original.creation_time_100ns.unwrap() + 1);
        let listings = RefCell::new(VecDeque::from([Ok(vec![original]), Ok(vec![reused])]));
        let killed = RefCell::new(Vec::new());

        close_codex_processes_with(
            || {
                listings
                    .borrow_mut()
                    .pop_front()
                    .expect("unexpected listing")
            },
            |process, force| {
                killed.borrow_mut().push((process.pid, force));
                TaskkillResult::Succeeded
            },
            || panic!("a reused PID must not enter the grace poll"),
            2,
        )
        .unwrap();

        assert_eq!(*killed.borrow(), vec![(1234, false)]);
        assert!(listings.borrow().is_empty());
    }

    #[test]
    fn root_exit_with_an_unverifiable_descendant_fails_closed_without_forcing_it() {
        let root = process("ChatGPT.exe", 1234);
        let helper = child_process("renderer-helper.exe", 5678, 1234);
        let mut unverifiable_helper = helper.clone();
        unverifiable_helper.creation_time_100ns = None;
        let listings = RefCell::new(VecDeque::from([
            Ok(vec![root, helper]),
            Ok(vec![unverifiable_helper.clone()]),
            Ok(vec![unverifiable_helper]),
        ]));
        let killed = RefCell::new(Vec::new());

        let error = close_codex_processes_with(
            || {
                listings
                    .borrow_mut()
                    .pop_front()
                    .expect("unexpected listing")
            },
            |process, force| {
                killed.borrow_mut().push((process.pid, force));
                TaskkillResult::Succeeded
            },
            || panic!("zero poll attempts should not wait"),
            0,
        )
        .expect_err("an unverifiable managed descendant must block the mutation");

        assert_eq!(*killed.borrow(), vec![(1234, false)]);
        assert!(error.contains("still running PID(s): 5678"), "{error}");
        assert!(
            error.contains("taskkill failed for PID(s): 5678"),
            "{error}"
        );
        assert!(listings.borrow().is_empty());
    }

    #[test]
    fn graceful_close_does_not_force_when_multiple_roots_exit() {
        let initial = vec![
            process("ChatGPT.exe", 1234),
            process("openai.codex.exe", 5678),
        ];
        let listings = RefCell::new(VecDeque::from([Ok(initial.clone()), Ok(Vec::new())]));
        let killed = RefCell::new(Vec::new());
        let waits = RefCell::new(0);

        let result = close_codex_processes_with(
            || {
                listings
                    .borrow_mut()
                    .pop_front()
                    .expect("unexpected listing")
            },
            |process, force| {
                killed.borrow_mut().push((process.pid, force));
                TaskkillResult::Succeeded
            },
            || *waits.borrow_mut() += 1,
            2,
        )
        .expect("all processes should be confirmed closed");

        assert_eq!(result, initial);
        assert_eq!(*killed.borrow(), vec![(1234, false), (5678, false)]);
        assert_eq!(*waits.borrow(), 0);
        assert!(listings.borrow().is_empty());
    }

    #[test]
    fn graceful_timeout_forces_only_the_surviving_roots() {
        let initial = vec![process("ChatGPT.exe", 1234)];
        let listings = RefCell::new(VecDeque::from([
            Ok(initial.clone()),
            Ok(initial.clone()),
            Ok(initial.clone()),
            Ok(initial.clone()),
            Ok(Vec::new()),
        ]));
        let killed = RefCell::new(Vec::new());
        let waits = RefCell::new(0);

        let result = close_codex_processes_with(
            || {
                listings
                    .borrow_mut()
                    .pop_front()
                    .expect("unexpected listing")
            },
            |process, force| {
                killed.borrow_mut().push((process.pid, force));
                TaskkillResult::Succeeded
            },
            || *waits.borrow_mut() += 1,
            2,
        )
        .expect("the force fallback should close the surviving root");

        assert_eq!(result, initial);
        assert_eq!(*killed.borrow(), vec![(1234, false), (1234, true)]);
        assert_eq!(*waits.borrow(), 2);
        assert!(listings.borrow().is_empty());
    }

    #[test]
    fn accepts_failed_graceful_taskkill_when_reenumeration_confirms_exit() {
        let initial = vec![process("ChatGPT.exe", 1234)];
        let listings = RefCell::new(VecDeque::from([Ok(initial.clone()), Ok(Vec::new())]));

        let result = close_codex_processes_with(
            || {
                listings
                    .borrow_mut()
                    .pop_front()
                    .expect("unexpected listing")
            },
            |_, force| {
                assert!(!force);
                TaskkillResult::Failed
            },
            || panic!("no wait is needed after the root exits"),
            2,
        )
        .expect("the process may have exited before taskkill completed");

        assert_eq!(result, initial);
        assert!(listings.borrow().is_empty());
    }

    #[test]
    fn reports_survivors_and_failed_commands_without_command_output() {
        let initial = vec![
            process("ChatGPT.exe", 1234),
            process("openai.codex.exe", 5678),
        ];
        let listings = RefCell::new(VecDeque::from([
            Ok(initial),
            Ok(vec![process("ChatGPT.exe", 1234)]),
            Ok(vec![process("ChatGPT.exe", 1234)]),
        ]));

        let error = close_codex_processes_with(
            || {
                listings
                    .borrow_mut()
                    .pop_front()
                    .expect("unexpected listing")
            },
            |process, force| {
                if force && process.pid == 1234 {
                    TaskkillResult::Failed
                } else {
                    TaskkillResult::Succeeded
                }
            },
            || panic!("zero poll attempts should force immediately"),
            0,
        )
        .expect_err("a surviving Codex process must fail the close operation");

        assert!(error.contains("still running PID(s): 1234"));
        assert!(error.contains("taskkill failed for PID(s): 1234"));
        assert!(!error.contains("stderr"));
        assert!(!error.contains("secret command output"));
    }

    #[test]
    fn successful_taskkill_still_fails_when_a_process_survives() {
        let initial = vec![process("ChatGPT.exe", 1234)];
        let listings = RefCell::new(VecDeque::from([
            Ok(initial.clone()),
            Ok(initial.clone()),
            Ok(initial),
        ]));

        let error = close_codex_processes_with(
            || {
                listings
                    .borrow_mut()
                    .pop_front()
                    .expect("unexpected listing")
            },
            |_, _| TaskkillResult::Succeeded,
            || panic!("zero poll attempts should force immediately"),
            0,
        )
        .expect_err("exit status alone must not prove that the process exited");

        assert!(error.contains("still running PID(s): 1234"));
        assert!(!error.contains("taskkill failed"));
    }

    #[test]
    fn verification_failure_does_not_expose_enumeration_details() {
        let listings = RefCell::new(VecDeque::from([
            Ok(vec![process("ChatGPT.exe", 1234)]),
            Err("secret tasklist output".to_string()),
        ]));

        let error = close_codex_processes_with(
            || {
                listings
                    .borrow_mut()
                    .pop_front()
                    .expect("unexpected listing")
            },
            |_, _| TaskkillResult::Succeeded,
            || panic!("verification should fail before waiting"),
            0,
        )
        .expect_err("the close operation needs a successful verification scan");

        assert_eq!(error, "failed to verify that ChatGPT processes exited");
    }
}

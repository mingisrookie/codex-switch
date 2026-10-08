//! macOS process control without process-command-line collection or PID signals.
//! libproc supplies identities; AppKit sends normal Quit requests to app objects.
use std::{
    collections::{HashMap, HashSet},
    ffi::OsStr,
    fs,
    mem::{size_of, MaybeUninit},
    os::unix::ffi::OsStrExt,
    path::PathBuf,
    sync::Mutex,
    thread,
    time::Duration,
};

use objc2_app_kit::NSRunningApplication;
use objc2_foundation::NSURL;

use crate::{
    managed_client::macos::{
        discover_apps, open_app, revalidate_app, verify_openai_signature, ManagedApp, BUNDLE_ID,
    },
    process_control::{
        ChatGptLaunchFailureReason, ChatGptLaunchResult, ChatGptLaunchStatus, CodexProcess,
    },
};

// PROC_UID_ONLY from Apple's <sys/proc_info.h>; proc_listpids returns bytes.
const PROC_UID_ONLY: u32 = 4;
const MAX_USER_PROCESSES: usize = 32_768;
const CLOSE_POLLS: usize = 100;
const LAUNCH_POLLS: usize = 80;
const POLL_INTERVAL: Duration = Duration::from_millis(100);
static LAUNCH_TARGET: Mutex<Option<ManagedApp>> = Mutex::new(None);

#[derive(Debug, Clone)]
struct Process {
    public: CodexProcess,
    executable: PathBuf,
    uid: u32,
}

impl Process {
    fn same_incarnation(&self, other: &Self) -> bool {
        self.public.pid == other.public.pid
            && self.uid == other.uid
            && self.public.creation_time_100ns.is_some()
            && self.public.creation_time_100ns == other.public.creation_time_100ns
    }

    fn same_executable(&self, other: &Self) -> bool {
        self.same_incarnation(other) && self.executable == other.executable
    }
}

#[derive(Debug, Clone)]
struct Inventory {
    processes: Vec<Process>,
    apps: Vec<ManagedApp>,
    roots: Vec<(Process, ManagedApp)>,
    managed: Vec<Process>,
    standalone: Vec<Process>,
}

pub(crate) fn list_codex_process_inventory(
) -> Result<(Vec<CodexProcess>, Vec<CodexProcess>), String> {
    let inventory = inventory()?;
    Ok((
        inventory
            .managed
            .into_iter()
            .map(|process| process.public)
            .collect(),
        inventory
            .standalone
            .into_iter()
            .map(|process| process.public)
            .collect(),
    ))
}

pub(crate) fn cache_chatgpt_launch_target() -> Result<(), String> {
    let mut cache = LAUNCH_TARGET
        .lock()
        .map_err(|_| "the macOS launch target is unavailable".to_string())?;
    *cache = None;
    let inventory = inventory()?;
    let target = select_target(&inventory).map_err(|reason| match reason {
        ChatGptLaunchFailureReason::LaunchTargetAmbiguous => {
            "more than one managed macOS client is installed; open the intended app first"
                .to_string()
        }
        _ => "no trusted macOS Codex desktop client was found".to_string(),
    })?;
    verify_openai_signature(&target)?;
    *cache = Some(target);
    Ok(())
}

pub(crate) fn launch_cached_chatgpt() -> ChatGptLaunchResult {
    let selected = match LAUNCH_TARGET.lock() {
        Ok(cache) => cache.clone(),
        Err(_) => {
            return failed(
                ChatGptLaunchFailureReason::LaunchTargetMissing,
                "The selected macOS client is unavailable.",
            );
        }
    };
    let selected = match selected {
        Some(app) => app,
        None => {
            let inventory = match inventory() {
                Ok(value) => value,
                Err(_) => {
                    return failed(
                        ChatGptLaunchFailureReason::ProcessInventoryUnavailable,
                        "The macOS process inventory is unavailable.",
                    );
                }
            };
            match select_target(&inventory) {
                Ok(app) => app,
                Err(reason) => {
                    return failed(
                        reason,
                        "Open the intended Codex desktop app from Applications before trying again.",
                    );
                }
            }
        }
    };
    if verify_openai_signature(&selected).is_err() {
        return failed(
            ChatGptLaunchFailureReason::ActivationFailed,
            "The selected macOS client no longer has a verified OpenAI signature.",
        );
    }
    let result = launch_with(
        &selected,
        inventory,
        open_app,
        native_app_matches,
        || thread::sleep(POLL_INTERVAL),
        LAUNCH_POLLS,
    );
    if result.status != ChatGptLaunchStatus::Failed {
        if let Ok(mut cache) = LAUNCH_TARGET.lock() {
            *cache = Some(selected);
        }
    }
    result
}

pub(crate) fn close_codex_processes() -> Result<Vec<CodexProcess>, String> {
    close_with(
        inventory,
        verify_openai_signature,
        quit_verified_app,
        || thread::sleep(POLL_INTERVAL),
        CLOSE_POLLS,
    )
}

fn inventory() -> Result<Inventory, String> {
    let apps = discover_apps()?;
    let processes = snapshot()?;
    for app in &apps {
        revalidate_app(app)?;
    }
    Ok(classify(processes, apps))
}

fn snapshot() -> Result<Vec<Process>, String> {
    // Rapidly exiting children can invalidate one observation. A bounded retry
    // obtains another complete snapshot; an unreadable live process is never
    // silently treated as absent.
    for attempt in 0..3 {
        match snapshot_once() {
            Ok(value) => return Ok(value),
            Err(_) if attempt < 2 => thread::sleep(Duration::from_millis(20)),
            Err(error) => return Err(error),
        }
    }
    unreachable!()
}

fn snapshot_once() -> Result<Vec<Process>, String> {
    let uid = unsafe { libc::geteuid() };
    let mut pids = vec![0i32; MAX_USER_PROCESSES];
    let capacity_bytes = (pids.len() * size_of::<i32>()) as i32;
    // SAFETY: pids is a writable, aligned array of capacity_bytes bytes.
    let bytes = unsafe {
        libc::proc_listpids(PROC_UID_ONLY, uid, pids.as_mut_ptr().cast(), capacity_bytes)
    };
    if bytes <= 0 || bytes >= capacity_bytes || bytes as usize % size_of::<i32>() != 0 {
        return Err("the macOS process inventory is unavailable or exceeds its limit".to_string());
    }
    pids.truncate(bytes as usize / size_of::<i32>());
    pids.retain(|pid| *pid > 0);
    pids.sort_unstable();
    pids.dedup();
    let mut result = Vec::new();
    for pid in pids {
        if let Some(process) = read_process(pid)? {
            if process.uid != uid {
                return Err("the macOS process inventory changed while reading".to_string());
            }
            result.push(process);
        }
    }
    Ok(result)
}

fn read_bsd_info(pid: i32) -> Result<Option<libc::proc_bsdinfo>, String> {
    let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let expected_bytes = size_of::<libc::proc_bsdinfo>() as i32;
    // SAFETY: info is sized and aligned for the SDK proc_bsdinfo declaration.
    let bytes = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            expected_bytes,
        )
    };
    if bytes != expected_bytes {
        let error = std::io::Error::last_os_error();
        if bytes == 0 && matches!(error.raw_os_error(), Some(libc::ESRCH | libc::ENOENT)) {
            return Ok(None);
        }
        return Err("a live macOS process identity could not be inspected".to_string());
    }
    // SAFETY: libproc wrote exactly the full initialized SDK struct.
    let info = unsafe { info.assume_init() };
    if info.pbi_pid != pid as u32 {
        return Err("a macOS process identity changed during inspection".to_string());
    }
    if info.pbi_status == libc::SZOMB {
        return Ok(None);
    }
    Ok(Some(info))
}

fn read_process(pid: i32) -> Result<Option<Process>, String> {
    let Some(before) = read_bsd_info(pid)? else {
        return Ok(None);
    };
    let start = creation_stamp(&before)?;
    let mut bytes = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: bytes is writable and meets libproc's documented path buffer size.
    let length = unsafe { libc::proc_pidpath(pid, bytes.as_mut_ptr().cast(), bytes.len() as u32) };
    if length <= 0 || length as usize >= bytes.len() {
        return match read_bsd_info(pid)? {
            None => Ok(None),
            Some(_) => Err("a live macOS process executable could not be inspected".to_string()),
        };
    }
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| "a macOS process executable path is invalid".to_string())?;
    let observed_path = PathBuf::from(OsStr::from_bytes(&bytes[..end]));
    if !observed_path.is_absolute() {
        return Err("a macOS process executable path is not absolute".to_string());
    }
    let executable = fs::canonicalize(&observed_path).unwrap_or(observed_path);
    let Some(after) = read_bsd_info(pid)? else {
        return Ok(None);
    };
    if creation_stamp(&after)? != start
        || before.pbi_uid != after.pbi_uid
        || before.pbi_ppid != after.pbi_ppid
    {
        return Err("a macOS process identity changed during inspection".to_string());
    }
    let image_name = executable
        .file_name()
        .ok_or_else(|| "a macOS process executable has no file name".to_string())?
        .to_string_lossy()
        .chars()
        .filter(|value| !value.is_control())
        .take(128)
        .collect();
    Ok(Some(Process {
        public: CodexProcess {
            image_name,
            pid: after.pbi_pid,
            parent_pid: after.pbi_ppid,
            creation_time_100ns: Some(start),
        },
        executable,
        uid: after.pbi_uid,
    }))
}

fn creation_stamp(info: &libc::proc_bsdinfo) -> Result<u64, String> {
    if info.pbi_start_tvsec == 0 || info.pbi_start_tvusec >= 1_000_000 {
        return Err("a macOS process creation time is unavailable".to_string());
    }
    info.pbi_start_tvsec
        .checked_mul(10_000_000)
        .and_then(|seconds| seconds.checked_add(info.pbi_start_tvusec * 10))
        .ok_or_else(|| "a macOS process creation time exceeds its limit".to_string())
}

fn classify(processes: Vec<Process>, apps: Vec<ManagedApp>) -> Inventory {
    let roots = processes
        .iter()
        .filter_map(|process| {
            apps.iter()
                .find(|app| process.executable == app.executable)
                .map(|app| (process.clone(), app.clone()))
        })
        .collect::<Vec<_>>();
    let mut managed = roots
        .iter()
        .map(|(process, _)| (process.public.pid, process.clone()))
        .collect::<HashMap<_, _>>();
    extend_descendants(&processes, &mut managed);
    let standalone = processes
        .iter()
        .filter(|process| {
            !managed.contains_key(&process.public.pid)
                && is_possible_unmanaged_writer(&process.public.image_name)
        })
        .cloned()
        .collect();
    Inventory {
        managed: processes
            .iter()
            .filter(|process| managed.contains_key(&process.public.pid))
            .cloned()
            .collect(),
        processes,
        apps,
        roots,
        standalone,
    }
}

fn extend_descendants(processes: &[Process], tracked: &mut HashMap<u32, Process>) {
    let live = processes
        .iter()
        .map(|process| (process.public.pid, process))
        .collect::<HashMap<_, _>>();
    loop {
        let mut changed = false;
        for process in processes {
            if tracked
                .get(&process.public.pid)
                .is_some_and(|expected| expected.same_incarnation(process))
            {
                continue;
            }
            let Some(parent) = tracked.get(&process.public.parent_pid) else {
                continue;
            };
            let Some(live_parent) = live.get(&parent.public.pid) else {
                continue;
            };
            if parent.same_incarnation(live_parent)
                && parent.uid == process.uid
                && process.public.creation_time_100ns >= parent.public.creation_time_100ns
            {
                tracked.insert(process.public.pid, process.clone());
                changed = true;
            }
        }
        if !changed {
            return;
        }
    }
}

fn is_possible_unmanaged_writer(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        "codex" | "codex-aarch64-apple-darwin" | "codex-x86_64-apple-darwin" | "chatgpt"
    ) || name.starts_with("codex helper")
        || name.starts_with("chatgpt helper")
}

fn select_target(inventory: &Inventory) -> Result<ManagedApp, ChatGptLaunchFailureReason> {
    let running_paths = inventory
        .roots
        .iter()
        .map(|(_, app)| &app.path)
        .collect::<HashSet<_>>();
    if running_paths.len() > 1 {
        return Err(ChatGptLaunchFailureReason::LaunchTargetAmbiguous);
    }
    if let Some(path) = running_paths.into_iter().next() {
        return inventory
            .apps
            .iter()
            .find(|app| &app.path == path)
            .cloned()
            .ok_or(ChatGptLaunchFailureReason::LaunchTargetMissing);
    }
    match inventory.apps.as_slice() {
        [app] => Ok(app.clone()),
        [] => Err(ChatGptLaunchFailureReason::LaunchTargetMissing),
        _ => Err(ChatGptLaunchFailureReason::LaunchTargetAmbiguous),
    }
}

fn canonical_url_path(url: &NSURL) -> Option<PathBuf> {
    let path = url.path()?;
    fs::canonicalize(PathBuf::from(path.to_string())).ok()
}

fn native_app_matches(process: &Process, app: &ManagedApp) -> bool {
    let Some(application) =
        NSRunningApplication::runningApplicationWithProcessIdentifier(process.public.pid as i32)
    else {
        return false;
    };
    if application.processIdentifier() != process.public.pid as i32
        || application
            .bundleIdentifier()
            .map(|value| value.to_string())
            .as_deref()
            != Some(BUNDLE_ID)
        || application
            .bundleURL()
            .as_deref()
            .and_then(canonical_url_path)
            .as_ref()
            != Some(&app.path)
        || application
            .executableURL()
            .as_deref()
            .and_then(canonical_url_path)
            .as_ref()
            != Some(&app.executable)
    {
        return false;
    }
    read_process(process.public.pid as i32)
        .ok()
        .flatten()
        .is_some_and(|current| process.same_executable(&current))
}

fn quit_verified_app(process: &Process, app: &ManagedApp) -> Result<(), String> {
    revalidate_app(app)?;
    let Some(before) = read_process(process.public.pid as i32)? else {
        return Ok(());
    };
    if !process.same_executable(&before) {
        return Err("the managed macOS client identity changed before Quit".to_string());
    }
    let application =
        NSRunningApplication::runningApplicationWithProcessIdentifier(process.public.pid as i32)
            .ok_or_else(|| {
                "macOS could not resolve the managed application instance".to_string()
            })?;
    if application.processIdentifier() != process.public.pid as i32
        || application
            .bundleIdentifier()
            .map(|value| value.to_string())
            .as_deref()
            != Some(BUNDLE_ID)
        || application
            .bundleURL()
            .as_deref()
            .and_then(canonical_url_path)
            .as_ref()
            != Some(&app.path)
        || application
            .executableURL()
            .as_deref()
            .and_then(canonical_url_path)
            .as_ref()
            != Some(&app.executable)
    {
        return Err("the macOS application instance does not match the managed client".to_string());
    }
    let Some(after) = read_process(process.public.pid as i32)? else {
        return Ok(());
    };
    if !process.same_executable(&after) {
        return Err("the managed macOS client identity changed before Quit".to_string());
    }
    // AppKit keeps the instance distinct even if its PID is reused. Never issue
    // SIGTERM/SIGKILL or a process-name kill command. A refused Quit remains a
    // blocking survivor; the user can save work and close the app themselves.
    let _ = application.terminate();
    Ok(())
}

fn close_with<List, Verify, Quit, Wait>(
    mut list: List,
    mut verify: Verify,
    mut quit: Quit,
    mut wait: Wait,
    poll_attempts: usize,
) -> Result<Vec<CodexProcess>, String>
where
    List: FnMut() -> Result<Inventory, String>,
    Verify: FnMut(&ManagedApp) -> Result<(), String>,
    Quit: FnMut(&Process, &ManagedApp) -> Result<(), String>,
    Wait: FnMut(),
{
    let initial = list()?;
    if !initial.standalone.is_empty() {
        return Err(
            "a standalone or unverified Codex process is active; close it manually".to_string(),
        );
    }
    if initial.managed.is_empty() {
        return Ok(Vec::new());
    }
    // Validate every selected bundle before any application's Quit request.
    for (_, app) in &initial.roots {
        verify(app)?;
    }
    for (process, app) in &initial.roots {
        quit(process, app)?;
    }
    let closed = initial
        .managed
        .iter()
        .map(|process| process.public.clone())
        .collect();
    let mut tracked = initial
        .managed
        .into_iter()
        .map(|process| (process.public.pid, process))
        .collect::<HashMap<_, _>>();
    for attempt in 0..=poll_attempts {
        let observed = list()?;
        for process in &observed.managed {
            tracked.insert(process.public.pid, process.clone());
        }
        extend_descendants(&observed.processes, &mut tracked);
        let survivors = observed.processes.iter().any(|process| {
            tracked
                .get(&process.public.pid)
                .is_some_and(|expected| expected.same_incarnation(process))
        });
        // Include new roots and independently started CLI processes. Reparented
        // known descendants stay in tracked until their exact incarnation exits.
        if !survivors && observed.managed.is_empty() && observed.standalone.is_empty() {
            return Ok(closed);
        }
        if attempt < poll_attempts {
            wait();
        }
    }
    Err(
        "the macOS client or a Codex writer is still running; save work and close it manually"
            .to_string(),
    )
}

fn launch_with<List, Open, Matches, Wait>(
    app: &ManagedApp,
    mut list: List,
    mut open: Open,
    mut matches: Matches,
    mut wait: Wait,
    poll_attempts: usize,
) -> ChatGptLaunchResult
where
    List: FnMut() -> Result<Inventory, String>,
    Open: FnMut(&ManagedApp) -> Result<(), String>,
    Matches: FnMut(&Process, &ManagedApp) -> bool,
    Wait: FnMut(),
{
    let before = match list() {
        Ok(value) => value,
        Err(_) => {
            return failed(
                ChatGptLaunchFailureReason::ProcessInventoryUnavailable,
                "The macOS process inventory is unavailable.",
            )
        }
    };
    if !before.apps.iter().any(|current| current == app) {
        return failed(
            ChatGptLaunchFailureReason::LaunchTargetMissing,
            "The selected macOS client changed or is no longer installed.",
        );
    }
    if before
        .roots
        .iter()
        .any(|(process, current)| current == app && matches(process, app))
    {
        return success(ChatGptLaunchStatus::AlreadyRunning);
    }
    if open(app).is_err() {
        return failed(
            ChatGptLaunchFailureReason::ActivationFailed,
            "macOS could not launch the verified Codex desktop client.",
        );
    }
    for attempt in 0..=poll_attempts {
        let observed = match list() {
            Ok(value) => value,
            Err(_) => {
                return failed(
                    ChatGptLaunchFailureReason::ProcessInventoryUnavailable,
                    "The launched macOS client could not be inspected.",
                )
            }
        };
        if observed
            .roots
            .iter()
            .any(|(process, current)| current == app && matches(process, app))
        {
            return success(ChatGptLaunchStatus::Launched);
        }
        if attempt < poll_attempts {
            wait();
        }
    }
    failed(
        ChatGptLaunchFailureReason::VerificationFailed,
        "The expected macOS client could not be verified; open it from Applications.",
    )
}

fn failed(reason: ChatGptLaunchFailureReason, message: &str) -> ChatGptLaunchResult {
    ChatGptLaunchResult {
        status: ChatGptLaunchStatus::Failed,
        message: Some(message.to_string()),
        reason: Some(reason),
    }
}

fn success(status: ChatGptLaunchStatus) -> ChatGptLaunchResult {
    ChatGptLaunchResult {
        status,
        message: None,
        reason: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed_client::macos::fixture_metadata;
    use std::{
        cell::RefCell,
        collections::VecDeque,
        process::{Child, Command, Stdio},
    };
    use tempfile::tempdir;

    fn process(pid: u32, parent_pid: u32, start: u64, path: PathBuf) -> Process {
        Process {
            public: CodexProcess {
                image_name: path.file_name().unwrap().to_string_lossy().into_owned(),
                pid,
                parent_pid,
                creation_time_100ns: Some(start),
            },
            executable: path,
            uid: 501,
        }
    }

    fn snapshots(values: Vec<Inventory>) -> impl FnMut() -> Result<Inventory, String> {
        let mut values: VecDeque<_> = values.into();
        move || {
            values
                .pop_front()
                .ok_or_else(|| "fixture exhausted".to_string())
        }
    }

    #[test]
    fn native_snapshot_identifies_only_our_test_child_without_collecting_arguments() {
        struct OwnChild(Child);
        impl Drop for OwnChild {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let child = OwnChild(
            Command::new("/bin/sleep")
                .arg("10")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let observed = snapshot().unwrap();
        let child_process = observed
            .iter()
            .find(|process| process.public.pid == child.0.id())
            .unwrap();
        assert_eq!(child_process.public.parent_pid, std::process::id());
        assert!(child_process.public.creation_time_100ns.unwrap() > 0);
        assert_eq!(
            child_process.executable.file_name(),
            Some(OsStr::new("sleep"))
        );
        assert!(read_process(child.0.id() as i32)
            .unwrap()
            .unwrap()
            .same_executable(child_process));
    }

    #[test]
    fn distinguishes_managed_children_from_standalone_cli_and_spoofed_names() {
        let temporary = tempdir().unwrap();
        let root = fs::canonicalize(temporary.path()).unwrap();
        let app = fixture_metadata(&root, "ChatGPT.app");
        let processes = vec![
            process(10, 1, 100, app.executable.clone()),
            process(11, 10, 110, PathBuf::from("/synthetic/app/codex")),
            process(12, 1, 120, PathBuf::from("/synthetic/cli/codex")),
            process(13, 1, 130, PathBuf::from("/synthetic/impostor/ChatGPT")),
            process(14, 10, 90, PathBuf::from("/synthetic/old-child/codex")),
        ];
        let result = classify(processes, vec![app]);
        assert_eq!(
            result
                .managed
                .iter()
                .map(|p| p.public.pid)
                .collect::<Vec<_>>(),
            [10, 11]
        );
        assert_eq!(
            result
                .standalone
                .iter()
                .map(|p| p.public.pid)
                .collect::<Vec<_>>(),
            [12, 13, 14]
        );
    }

    #[test]
    fn standalone_cli_blocks_before_any_quit_or_signature_action() {
        let cli = process(12, 1, 120, PathBuf::from("/synthetic/cli/codex"));
        let result = close_with(
            snapshots(vec![classify(vec![cli], vec![])]),
            |_| panic!("must not verify or mutate"),
            |_, _| panic!("must never terminate CLI"),
            || {},
            0,
        );
        assert!(result.unwrap_err().contains("standalone"));
    }

    #[test]
    fn normal_quit_waits_for_reparented_descendants_without_forcing_them() {
        let temporary = tempdir().unwrap();
        let root = fs::canonicalize(temporary.path()).unwrap();
        let app = fixture_metadata(&root, "ChatGPT.app");
        let main = process(10, 1, 100, app.executable.clone());
        let child = process(11, 10, 110, PathBuf::from("/synthetic/helper"));
        let mut orphan = child.clone();
        orphan.public.parent_pid = 1;
        let quit_pids = RefCell::new(Vec::new());
        let result = close_with(
            snapshots(vec![
                classify(vec![main, child], vec![app.clone()]),
                classify(vec![orphan], vec![app.clone()]),
                classify(vec![], vec![app]),
            ]),
            |_| Ok(()),
            |process, _| {
                quit_pids.borrow_mut().push(process.public.pid);
                Ok(())
            },
            || {},
            1,
        )
        .unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(*quit_pids.borrow(), [10]);
    }

    #[test]
    fn child_exec_and_reparenting_remain_blocking_until_the_incarnation_exits() {
        let temporary = tempdir().unwrap();
        let root = fs::canonicalize(temporary.path()).unwrap();
        let app = fixture_metadata(&root, "Codex.app");
        let main = process(10, 1, 100, app.executable.clone());
        let child = process(11, 10, 110, PathBuf::from("/synthetic/codex"));
        let changed_child = process(11, 1, 110, PathBuf::from("/synthetic/replacement-writer"));
        let result = close_with(
            snapshots(vec![
                classify(vec![main, child], vec![app.clone()]),
                classify(vec![changed_child], vec![app]),
            ]),
            |_| Ok(()),
            |_, _| Ok(()),
            || {},
            0,
        );
        assert!(result.is_err());
    }

    #[test]
    fn reused_child_pid_stays_tracked_after_the_new_child_is_reparented() {
        let temporary = tempdir().unwrap();
        let root = fs::canonicalize(temporary.path()).unwrap();
        let app = fixture_metadata(&root, "Codex.app");
        let main = process(10, 1, 100, app.executable.clone());
        let old_child = process(11, 10, 110, PathBuf::from("/synthetic/helper"));
        let new_child = process(11, 10, 210, PathBuf::from("/synthetic/helper"));
        let mut orphan = new_child.clone();
        orphan.public.parent_pid = 1;
        let result = close_with(
            snapshots(vec![
                classify(vec![main.clone(), old_child], vec![app.clone()]),
                classify(vec![main, new_child], vec![app.clone()]),
                classify(vec![orphan], vec![app]),
            ]),
            |_| Ok(()),
            |_, _| Ok(()),
            || {},
            1,
        );
        assert!(result.is_err());
    }

    #[test]
    fn pid_reuse_does_not_keep_or_terminate_an_unrelated_process() {
        let temporary = tempdir().unwrap();
        let root = fs::canonicalize(temporary.path()).unwrap();
        let app = fixture_metadata(&root, "Codex.app");
        let main = process(10, 1, 100, app.executable.clone());
        let unrelated = process(10, 1, 200, PathBuf::from("/synthetic/editor"));
        let result = close_with(
            snapshots(vec![
                classify(vec![main], vec![app.clone()]),
                classify(vec![unrelated], vec![app]),
            ]),
            |_| Ok(()),
            |_, _| Ok(()),
            || {},
            0,
        )
        .unwrap();
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn refuses_all_quit_requests_if_any_bundle_verification_fails() {
        let temporary = tempdir().unwrap();
        let root = fs::canonicalize(temporary.path()).unwrap();
        let app = fixture_metadata(&root, "Codex.app");
        let main = process(10, 1, 100, app.executable.clone());
        assert!(close_with(
            snapshots(vec![classify(vec![main], vec![app])]),
            |_| Err("signature rejected".to_string()),
            |_, _| panic!("unverified client must not receive Quit"),
            || {},
            0,
        )
        .is_err());
    }

    #[test]
    fn second_install_is_ambiguous_until_a_unique_running_root_selects_it() {
        let temporary = tempdir().unwrap();
        let root = fs::canonicalize(temporary.path()).unwrap();
        let first = fixture_metadata(&root, "Codex.app");
        let second = fixture_metadata(&root, "ChatGPT.app");
        let inventory = classify(vec![], vec![first.clone(), second.clone()]);
        assert_eq!(
            select_target(&inventory).unwrap_err(),
            ChatGptLaunchFailureReason::LaunchTargetAmbiguous
        );
        let main = process(10, 1, 100, second.executable.clone());
        let inventory = classify(vec![main], vec![first, second.clone()]);
        assert_eq!(select_target(&inventory).unwrap(), second);
    }

    #[test]
    fn launch_requires_the_same_app_and_verified_native_instance() {
        let temporary = tempdir().unwrap();
        let root = fs::canonicalize(temporary.path()).unwrap();
        let app = fixture_metadata(&root, "ChatGPT.app");
        let main = process(10, 1, 100, app.executable.clone());
        let result = launch_with(
            &app,
            snapshots(vec![
                classify(vec![], vec![app.clone()]),
                classify(vec![main], vec![app.clone()]),
            ]),
            |_| Ok(()),
            |_, _| true,
            || {},
            0,
        );
        assert_eq!(result.status, ChatGptLaunchStatus::Launched);
        let result = launch_with(
            &app,
            snapshots(vec![classify(vec![], vec![])]),
            |_| panic!("absent target must never be opened"),
            |_, _| false,
            || {},
            0,
        );
        assert_eq!(
            result.reason,
            Some(ChatGptLaunchFailureReason::LaunchTargetMissing)
        );
    }

    #[test]
    fn matching_path_without_appkit_identity_does_not_prove_launch() {
        let temporary = tempdir().unwrap();
        let root = fs::canonicalize(temporary.path()).unwrap();
        let app = fixture_metadata(&root, "ChatGPT.app");
        let main = process(10, 1, 100, app.executable.clone());
        let result = launch_with(
            &app,
            snapshots(vec![
                classify(vec![], vec![app.clone()]),
                classify(vec![main], vec![app.clone()]),
            ]),
            |_| Ok(()),
            |_, _| false,
            || {},
            0,
        );
        assert_eq!(
            result.reason,
            Some(ChatGptLaunchFailureReason::VerificationFailed)
        );
    }

    #[test]
    fn close_refusal_and_inventory_errors_remain_fail_closed() {
        let temporary = tempdir().unwrap();
        let root = fs::canonicalize(temporary.path()).unwrap();
        let app = fixture_metadata(&root, "ChatGPT.app");
        let main = process(10, 1, 100, app.executable.clone());
        let state = classify(vec![main], vec![app]);
        assert!(close_with(
            snapshots(vec![state.clone(), state]),
            |_| Ok(()),
            |_, _| Ok(()),
            || {},
            0
        )
        .is_err());
        assert!(close_with(
            || Err("unavailable".to_string()),
            |_| Ok(()),
            |_, _| Ok(()),
            || {},
            0
        )
        .is_err());
    }
}

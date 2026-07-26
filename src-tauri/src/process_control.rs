use serde::Serialize;
use std::collections::{HashMap, HashSet};
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
        CloseHandle, GetLastError, ERROR_NO_MORE_FILES, FILETIME, HANDLE, INVALID_HANDLE_VALUE,
    },
    Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT,
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
    Ok(managed_process_tree(&snapshot_processes()?))
}

#[cfg(windows)]
pub fn list_standalone_codex_processes() -> Result<Vec<CodexProcess>, String> {
    Ok(standalone_codex_processes(&snapshot_processes()?))
}

#[cfg(not(windows))]
pub fn list_codex_processes() -> Result<Vec<CodexProcess>, String> {
    Err("ChatGPT process control is not supported on this platform".to_string())
}

#[cfg(not(windows))]
pub fn list_standalone_codex_processes() -> Result<Vec<CodexProcess>, String> {
    Err("ChatGPT process control is not supported on this platform".to_string())
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
        close_codex_processes_with, decode_process_name, managed_process_tree,
        standalone_codex_processes, CodexProcess, TaskkillResult,
    };
    #[cfg(windows)]
    use super::{system_tool_path, GRACEFUL_CLOSE_POLL_ATTEMPTS, GRACEFUL_CLOSE_POLL_INTERVAL};
    use std::{cell::RefCell, collections::VecDeque, fs};

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

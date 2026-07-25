use serde::Serialize;
#[cfg(windows)]
use std::{mem::size_of, os::windows::process::CommandExt, path::PathBuf, process::Command};
#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        },
        Threading::CREATE_NO_WINDOW,
    },
};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexProcess {
    pub image_name: String,
    pub pid: u32,
}

#[cfg(windows)]
pub fn list_codex_processes() -> Result<Vec<CodexProcess>, String> {
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
        return Err("failed to read the Windows process snapshot".to_string());
    }

    let mut processes = Vec::new();
    loop {
        let image_name = decode_process_name(&entry.szExeFile);
        if is_codex_process(&image_name) {
            processes.push(CodexProcess {
                image_name,
                pid: entry.th32ProcessID,
            });
        }
        if unsafe { Process32NextW(snapshot, &mut entry) } == 0 {
            break;
        }
    }
    Ok(processes)
}

#[cfg(not(windows))]
pub fn list_codex_processes() -> Result<Vec<CodexProcess>, String> {
    Err("Codex process control is not supported on this platform".to_string())
}

#[cfg(windows)]
pub fn close_codex_processes() -> Result<Vec<CodexProcess>, String> {
    let taskkill = system_tool_path("taskkill.exe")?;
    close_codex_processes_with(list_codex_processes, move |pid| {
        match Command::new(&taskkill)
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        {
            Ok(output) if output.status.success() => TaskkillResult::Succeeded,
            Ok(_) | Err(_) => TaskkillResult::Failed,
        }
    })
}

#[cfg(windows)]
struct SnapshotHandle(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Drop for SnapshotHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
fn system_tool_path(name: &str) -> Result<PathBuf, String> {
    let root = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| "Windows system directory is unavailable".to_string())?;
    let path = root.join("System32").join(name);
    if !path.is_file() {
        return Err("required Windows process-control tool is unavailable".to_string());
    }
    Ok(path)
}

#[cfg(not(windows))]
pub fn close_codex_processes() -> Result<Vec<CodexProcess>, String> {
    Err("Codex process control is not supported on this platform".to_string())
}

#[cfg(any(windows, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskkillResult {
    Succeeded,
    Failed,
}

#[cfg(any(windows, test))]
fn close_codex_processes_with<List, Kill>(
    mut list_processes: List,
    mut kill_process: Kill,
) -> Result<Vec<CodexProcess>, String>
where
    List: FnMut() -> Result<Vec<CodexProcess>, String>,
    Kill: FnMut(u32) -> TaskkillResult,
{
    let processes = list_processes()?;
    let failed_pids = processes
        .iter()
        .filter_map(|process| {
            (kill_process(process.pid) == TaskkillResult::Failed).then_some(process.pid)
        })
        .collect::<Vec<_>>();

    let survivors =
        list_processes().map_err(|_| "failed to verify that Codex processes exited".to_string())?;
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
        "failed to close all Codex processes; still running PID(s): {survivor_pids}{failed_note}"
    ))
}

#[cfg(any(windows, test))]
fn format_pids(pids: impl IntoIterator<Item = u32>) -> String {
    pids.into_iter()
        .map(|pid| pid.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(any(windows, test))]
fn decode_process_name(buffer: &[u16]) -> String {
    let end = buffer
        .iter()
        .position(|character| *character == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..end])
}

fn is_codex_process(image_name: &str) -> bool {
    let lower = image_name.to_ascii_lowercase();
    matches!(lower.as_str(), "codex.exe" | "openai.codex.exe" | "codex")
}

#[cfg(test)]
mod tests {
    use super::{
        close_codex_processes_with, decode_process_name, is_codex_process, CodexProcess,
        TaskkillResult,
    };
    use std::{cell::RefCell, collections::VecDeque};

    fn process(image_name: &str, pid: u32) -> CodexProcess {
        CodexProcess {
            image_name: image_name.to_string(),
            pid,
        }
    }

    #[test]
    fn recognizes_codex_process_names_without_matching_codex_switch() {
        assert!(is_codex_process("codex.exe"));
        assert!(is_codex_process("OpenAI.Codex.exe"));
        assert!(!is_codex_process("codex-switch.exe"));
    }

    #[test]
    fn decodes_null_terminated_windows_process_names() {
        let mut buffer = [0_u16; 260];
        let encoded = "codex.exe".encode_utf16().collect::<Vec<_>>();
        buffer[..encoded.len()].copy_from_slice(&encoded);

        assert_eq!(decode_process_name(&buffer), "codex.exe");
    }

    #[test]
    fn closes_each_initial_process_and_confirms_none_survive() {
        let initial = vec![
            process("codex.exe", 1234),
            process("openai.codex.exe", 5678),
        ];
        let listings = RefCell::new(VecDeque::from([Ok(initial.clone()), Ok(Vec::new())]));
        let killed = RefCell::new(Vec::new());

        let result = close_codex_processes_with(
            || {
                listings
                    .borrow_mut()
                    .pop_front()
                    .expect("unexpected listing")
            },
            |pid| {
                killed.borrow_mut().push(pid);
                TaskkillResult::Succeeded
            },
        )
        .expect("all processes should be confirmed closed");

        assert_eq!(result, initial);
        assert_eq!(*killed.borrow(), vec![1234, 5678]);
        assert!(listings.borrow().is_empty());
    }

    #[test]
    fn accepts_failed_taskkill_when_reenumeration_confirms_exit() {
        let initial = vec![process("codex.exe", 1234)];
        let listings = RefCell::new(VecDeque::from([Ok(initial.clone()), Ok(Vec::new())]));

        let result = close_codex_processes_with(
            || {
                listings
                    .borrow_mut()
                    .pop_front()
                    .expect("unexpected listing")
            },
            |_| TaskkillResult::Failed,
        )
        .expect("the process may have exited before taskkill completed");

        assert_eq!(result, initial);
        assert!(listings.borrow().is_empty());
    }

    #[test]
    fn reports_survivors_and_failed_commands_without_command_output() {
        let initial = vec![
            process("codex.exe", 1234),
            process("openai.codex.exe", 5678),
        ];
        let listings = RefCell::new(VecDeque::from([
            Ok(initial),
            Ok(vec![process("codex.exe", 1234)]),
        ]));

        let error = close_codex_processes_with(
            || {
                listings
                    .borrow_mut()
                    .pop_front()
                    .expect("unexpected listing")
            },
            |pid| {
                if pid == 1234 {
                    TaskkillResult::Failed
                } else {
                    TaskkillResult::Succeeded
                }
            },
        )
        .expect_err("a surviving Codex process must fail the close operation");

        assert!(error.contains("still running PID(s): 1234"));
        assert!(error.contains("taskkill failed for PID(s): 1234"));
        assert!(!error.contains("stderr"));
        assert!(!error.contains("secret command output"));
    }

    #[test]
    fn successful_taskkill_still_fails_when_a_process_survives() {
        let initial = vec![process("codex.exe", 1234)];
        let listings = RefCell::new(VecDeque::from([Ok(initial.clone()), Ok(initial)]));

        let error = close_codex_processes_with(
            || {
                listings
                    .borrow_mut()
                    .pop_front()
                    .expect("unexpected listing")
            },
            |_| TaskkillResult::Succeeded,
        )
        .expect_err("exit status alone must not prove that the process exited");

        assert!(error.contains("still running PID(s): 1234"));
        assert!(!error.contains("taskkill failed"));
    }

    #[test]
    fn verification_failure_does_not_expose_enumeration_details() {
        let listings = RefCell::new(VecDeque::from([
            Ok(vec![process("codex.exe", 1234)]),
            Err("secret tasklist output".to_string()),
        ]));

        let error = close_codex_processes_with(
            || {
                listings
                    .borrow_mut()
                    .pop_front()
                    .expect("unexpected listing")
            },
            |_| TaskkillResult::Succeeded,
        )
        .expect_err("the close operation needs a successful verification scan");

        assert_eq!(error, "failed to verify that Codex processes exited");
    }
}

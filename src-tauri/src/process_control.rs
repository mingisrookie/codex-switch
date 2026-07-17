use serde::Serialize;
#[cfg(windows)]
use std::process::Command;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexProcess {
    pub image_name: String,
    pub pid: u32,
}

#[cfg(windows)]
pub fn list_codex_processes() -> Result<Vec<CodexProcess>, String> {
    let output = Command::new("tasklist")
        .args(["/FO", "CSV", "/NH"])
        .output()
        .map_err(|error| format!("failed to run tasklist: {error}"))?;
    if !output.status.success() {
        return Err("tasklist failed".to_string());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_tasklist_csv(&stdout))
}

#[cfg(not(windows))]
pub fn list_codex_processes() -> Result<Vec<CodexProcess>, String> {
    Err("Codex process control is not supported on this platform".to_string())
}

#[cfg(windows)]
pub fn close_codex_processes() -> Result<Vec<CodexProcess>, String> {
    close_codex_processes_with(list_codex_processes, |pid| {
        match Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output()
        {
            Ok(output) if output.status.success() => TaskkillResult::Succeeded,
            Ok(_) | Err(_) => TaskkillResult::Failed,
        }
    })
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

pub fn parse_tasklist_csv(text: &str) -> Vec<CodexProcess> {
    text.lines()
        .filter_map(parse_tasklist_line)
        .filter(|process| is_codex_process(&process.image_name))
        .collect()
}

fn parse_tasklist_line(line: &str) -> Option<CodexProcess> {
    let fields = parse_csv_line(line);
    let image_name = fields.first()?.to_string();
    let pid = fields.get(1)?.parse().ok()?;
    Some(CodexProcess { image_name, pid })
}

fn parse_csv_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for ch in line.chars() {
        match ch {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                fields.push(current.clone());
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    fields.push(current);
    fields
}

fn is_codex_process(image_name: &str) -> bool {
    let lower = image_name.to_ascii_lowercase();
    matches!(lower.as_str(), "codex.exe" | "openai.codex.exe" | "codex")
}

#[cfg(test)]
mod tests {
    use super::{close_codex_processes_with, parse_tasklist_csv, CodexProcess, TaskkillResult};
    use std::{cell::RefCell, collections::VecDeque};

    fn process(image_name: &str, pid: u32) -> CodexProcess {
        CodexProcess {
            image_name: image_name.to_string(),
            pid,
        }
    }

    #[test]
    fn parses_codex_processes_without_matching_codex_switch() {
        let rows = "\"codex.exe\",\"1234\",\"Console\",\"1\",\"10,000 K\"\n\"codex-switch.exe\",\"5678\",\"Console\",\"1\",\"10,000 K\"\n";
        let processes = parse_tasklist_csv(rows);
        assert_eq!(processes.len(), 1);
        assert_eq!(processes[0].pid, 1234);
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

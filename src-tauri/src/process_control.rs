use serde::Serialize;
use std::process::Command;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexProcess {
    pub image_name: String,
    pub pid: u32,
}

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

pub fn close_codex_processes() -> Result<Vec<CodexProcess>, String> {
    let processes = list_codex_processes()?;
    for process in &processes {
        let _ = Command::new("taskkill")
            .args(["/PID", &process.pid.to_string(), "/T", "/F"])
            .output();
    }
    Ok(processes)
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
    use super::parse_tasklist_csv;

    #[test]
    fn parses_codex_processes_without_matching_codex_switch() {
        let rows = "\"codex.exe\",\"1234\",\"Console\",\"1\",\"10,000 K\"\n\"codex-switch.exe\",\"5678\",\"Console\",\"1\",\"10,000 K\"\n";
        let processes = parse_tasklist_csv(rows);
        assert_eq!(processes.len(), 1);
        assert_eq!(processes[0].pid, 1234);
    }
}

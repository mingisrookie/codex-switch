use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

use crate::file_ops::atomic_write;

const CHAT_PROCESS_STATE_PATH: &str = "process_manager/chat_processes.json";

pub(crate) fn repair_after_chatgpt_shutdown(codex_home: &Path) -> Result<bool, String> {
    let path = chat_process_state_path(codex_home);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(format!(
                "failed to read ChatGPT process state {}: {error}",
                path.display()
            ));
        }
    };
    if serde_json::from_slice::<Vec<serde_json::Value>>(&bytes).is_ok() {
        return Ok(false);
    }

    atomic_write(&path, b"[]").map_err(|error| {
        format!(
            "failed to repair ChatGPT process state {}: {error}",
            path.display()
        )
    })?;
    let repaired = fs::read(&path).map_err(|error| {
        format!(
            "failed to verify ChatGPT process state repair {}: {error}",
            path.display()
        )
    })?;
    let records = serde_json::from_slice::<Vec<serde_json::Value>>(&repaired).map_err(|error| {
        format!(
            "ChatGPT process state repair verification failed {}: {error}",
            path.display()
        )
    })?;
    if records.is_empty() {
        Ok(true)
    } else {
        Err(format!(
            "ChatGPT process state repair verification failed {}",
            path.display()
        ))
    }
}

fn chat_process_state_path(codex_home: &Path) -> PathBuf {
    codex_home.join(CHAT_PROCESS_STATE_PATH)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{chat_process_state_path, repair_after_chatgpt_shutdown};

    #[test]
    fn missing_process_state_is_left_for_chatgpt_to_create() {
        let home = tempdir().unwrap();
        let path = chat_process_state_path(home.path());

        assert!(!repair_after_chatgpt_shutdown(home.path()).unwrap());
        assert!(!path.exists());
    }

    #[test]
    fn valid_process_state_is_not_rewritten() {
        let home = tempdir().unwrap();
        let path = chat_process_state_path(home.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = br#"[{"id":"process-1"}]"#;
        fs::write(&path, original).unwrap();

        assert!(!repair_after_chatgpt_shutdown(home.path()).unwrap());
        assert_eq!(fs::read(path).unwrap(), original);
    }

    #[test]
    fn nul_filled_process_state_is_replaced_with_an_empty_array() {
        let home = tempdir().unwrap();
        let path = chat_process_state_path(home.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, vec![0_u8; 103_405]).unwrap();

        assert!(repair_after_chatgpt_shutdown(home.path()).unwrap());
        assert_eq!(fs::read(path).unwrap(), b"[]");
    }

    #[test]
    fn valid_json_with_the_wrong_shape_is_repaired() {
        let home = tempdir().unwrap();
        let path = chat_process_state_path(home.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, br#"{"records":[]}"#).unwrap();

        assert!(repair_after_chatgpt_shutdown(home.path()).unwrap());
        assert_eq!(fs::read(path).unwrap(), b"[]");
    }

    #[test]
    fn unreadable_process_state_fails_closed() {
        let home = tempdir().unwrap();
        let path = chat_process_state_path(home.path());
        fs::create_dir_all(&path).unwrap();

        let error = repair_after_chatgpt_shutdown(home.path()).unwrap_err();
        assert!(
            error.contains("failed to read ChatGPT process state"),
            "{error}"
        );
    }
}

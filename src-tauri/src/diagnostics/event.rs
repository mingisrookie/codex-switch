use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const DIAGNOSTIC_SCHEMA_VERSION: u32 = 1;

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DiagnosticLevel {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DiagnosticEventKind {
    SessionStarted,
    AppReady,
    ExitRequested,
    SessionEnded,
    PreviousSessionUnclean,
    Panic,
    OperationStarted,
    OperationBound,
    OperationPhase,
    OperationBranch,
    OperationTerminal,
    BackgroundFailure,
    StartupFailure,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DiagnosticTerminalStatus {
    Succeeded,
    Failed,
    Partial,
    Blocked,
    Cancelled,
    RolledBack,
    RollbackFailed,
    Unknown,
}

pub type SafeContext = BTreeMap<String, Value>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DiagnosticEvent {
    pub schema_version: u32,
    pub event_id: String,
    pub session_id: String,
    pub sequence: u64,
    pub timestamp: u128,
    pub level: DiagnosticLevel,
    pub component: String,
    pub event_kind: DiagnosticEventKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_status: Option<DiagnosticTerminalStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub safe_message: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub safe_context: SafeContext,
}

#[derive(Debug, Clone)]
pub struct DiagnosticEventInput {
    pub level: DiagnosticLevel,
    pub component: String,
    pub event_kind: DiagnosticEventKind,
    pub attempt_id: Option<String>,
    pub operation_id: Option<String>,
    pub action: Option<String>,
    pub phase: Option<String>,
    pub terminal_status: Option<DiagnosticTerminalStatus>,
    pub error_code: Option<String>,
    pub safe_message: Option<String>,
    pub safe_context: SafeContext,
}

impl DiagnosticEventInput {
    pub fn new(
        level: DiagnosticLevel,
        component: impl Into<String>,
        event_kind: DiagnosticEventKind,
    ) -> Self {
        Self {
            level,
            component: component.into(),
            event_kind,
            attempt_id: None,
            operation_id: None,
            action: None,
            phase: None,
            terminal_status: None,
            error_code: None,
            safe_message: None,
            safe_context: BTreeMap::new(),
        }
    }

    pub fn with_attempt_id(mut self, attempt_id: impl Into<String>) -> Self {
        self.attempt_id = Some(attempt_id.into());
        self
    }

    pub fn with_operation_id(mut self, operation_id: impl Into<String>) -> Self {
        self.operation_id = Some(operation_id.into());
        self
    }

    pub fn with_action(mut self, action: impl Into<String>) -> Self {
        self.action = Some(action.into());
        self
    }

    pub fn with_phase(mut self, phase: impl Into<String>) -> Self {
        self.phase = Some(phase.into());
        self
    }

    pub fn with_terminal_status(mut self, status: DiagnosticTerminalStatus) -> Self {
        self.terminal_status = Some(status);
        self
    }

    pub fn with_error(
        mut self,
        error_code: impl Into<String>,
        safe_message: impl Into<String>,
    ) -> Self {
        self.error_code = Some(error_code.into());
        self.safe_message = Some(safe_message.into());
        self
    }

    pub fn with_message(mut self, safe_message: impl Into<String>) -> Self {
        self.safe_message = Some(safe_message.into());
        self
    }

    pub fn with_context(mut self, safe_context: SafeContext) -> Self {
        self.safe_context = safe_context;
        self
    }
}

pub fn timestamp_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

pub fn new_diagnostic_id(prefix: &str) -> String {
    if let Some(random) = system_random_128() {
        let suffix = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        return format!("{prefix}-{suffix}");
    }

    // Non-Windows tests and the exceptional Windows CSPRNG failure path still
    // need a per-process, non-stable identifier so diagnostics remain fail-open.
    let counter = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let seed = format!(
        "{prefix}:{}:{}:{counter}:{:?}",
        timestamp_millis(),
        std::process::id(),
        std::thread::current().id()
    );
    let digest = Sha256::digest(seed.as_bytes());
    let suffix = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{prefix}-{suffix}")
}

#[cfg(windows)]
fn system_random_128() -> Option<[u8; 16]> {
    use windows_sys::Win32::Security::Cryptography::{
        BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
    };

    let mut bytes = [0_u8; 16];
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            bytes.as_mut_ptr(),
            bytes.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status >= 0 {
        return Some(bytes);
    }

    // Keep the exceptional BCrypt failure path on an OS-generated identifier
    // before considering the non-stable process-local fallback below.
    unsafe { windows::Win32::System::Com::CoCreateGuid() }
        .ok()
        .map(|guid| guid.to_u128().to_be_bytes())
}

#[cfg(not(windows))]
fn system_random_128() -> Option<[u8; 16]> {
    None
}

#[cfg(test)]
mod tests {
    use super::{new_diagnostic_id, DiagnosticEventInput, DiagnosticEventKind, DiagnosticLevel};

    #[test]
    fn generated_ids_are_session_scoped_and_unique() {
        let first = new_diagnostic_id("session");
        let second = new_diagnostic_id("session");

        assert_ne!(first, second);
        assert!(first.starts_with("session-"));
        assert_eq!(first.len(), "session-".len() + 32);
    }

    #[test]
    fn event_input_builder_keeps_only_explicit_optional_fields() {
        let input = DiagnosticEventInput::new(
            DiagnosticLevel::Error,
            "runtimeSwitch",
            DiagnosticEventKind::OperationTerminal,
        )
        .with_attempt_id("attempt-1")
        .with_error("runtime.preflight", "preflight failed");

        assert_eq!(input.attempt_id.as_deref(), Some("attempt-1"));
        assert_eq!(input.error_code.as_deref(), Some("runtime.preflight"));
        assert!(input.operation_id.is_none());
        assert!(input.safe_context.is_empty());
    }
}

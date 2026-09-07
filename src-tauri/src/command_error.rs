use serde::{Deserialize, Serialize};
use std::{error::Error, fmt};

pub const COMMAND_ERROR_ENVELOPE_PREFIX: &str = "__CHATGPT_SWITCH_COMMAND_ERROR_V1__";
const MAX_SAFE_MESSAGE_BYTES: usize = 16 * 1024;
const MAX_PHASE_BYTES: usize = 128;
const MAX_PAYLOAD_BYTES: usize = 32 * 1024;

fn bounded_utf8(mut value: String, limit: usize) -> String {
    if value.len() > limit {
        let mut boundary = limit;
        while !value.is_char_boundary(boundary) {
            boundary -= 1;
        }
        value.truncate(boundary);
    }
    value
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CommandErrorCode {
    RuntimeCompatibilityBlocked,
    RuntimeCompatibilityUnavailable,
    ProviderCapabilityInvalid,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CommandRecoverability {
    Retry,
    Reconfigure,
    CloseWritersAndRetry,
    ManualInvestigation,
    UnsupportedClient,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommandFailure {
    pub code: CommandErrorCode,
    pub safe_message: String,
    pub phase: String,
    pub recoverability: CommandRecoverability,
}

impl CommandFailure {
    pub fn new(
        code: CommandErrorCode,
        safe_message: impl Into<String>,
        phase: impl Into<String>,
        recoverability: CommandRecoverability,
    ) -> Self {
        Self {
            code,
            safe_message: bounded_utf8(safe_message.into(), MAX_SAFE_MESSAGE_BYTES),
            phase: bounded_utf8(phase.into(), MAX_PHASE_BYTES),
            recoverability,
        }
    }

    pub fn encoded(&self) -> String {
        // Also bound public fields that a caller may have changed after construction.
        let mut safe = Self::new(
            self.code,
            self.safe_message.clone(),
            self.phase.clone(),
            self.recoverability,
        );
        if safe.phase.is_empty() {
            safe.phase = "unknown".to_string();
        }
        loop {
            match serde_json::to_string(&safe) {
                Ok(payload) if payload.len() <= MAX_PAYLOAD_BYTES => {
                    return format!("{COMMAND_ERROR_ENVELOPE_PREFIX}{payload}");
                }
                Ok(_) if !safe.safe_message.is_empty() => {
                    // Escaped control characters can take six bytes each in JSON.
                    let limit = safe.safe_message.len() / 2;
                    safe.safe_message = bounded_utf8(safe.safe_message, limit);
                }
                _ => return "操作失败；请刷新后重试或导出诊断。".to_string(),
            }
        }
    }
}

impl fmt::Display for CommandFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.encoded())
    }
}

impl Error for CommandFailure {}

pub fn decode_command_failure(value: &str) -> Option<CommandFailure> {
    let payload = value.strip_prefix(COMMAND_ERROR_ENVELOPE_PREFIX)?;
    if payload.is_empty() || payload.len() > MAX_PAYLOAD_BYTES {
        return None;
    }
    serde_json::from_str(payload).ok()
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{
        decode_command_failure, CommandErrorCode, CommandFailure, CommandRecoverability,
        COMMAND_ERROR_ENVELOPE_PREFIX,
    };

    #[test]
    fn typed_command_failures_round_trip_without_parsing_user_messages() {
        let failure = CommandFailure::new(
            CommandErrorCode::RuntimeCompatibilityBlocked,
            "当前客户端数据库结构未经支持验证",
            "preflight",
            CommandRecoverability::UnsupportedClient,
        );
        let encoded = failure.encoded();

        assert!(encoded.starts_with(COMMAND_ERROR_ENVELOPE_PREFIX));
        assert_eq!(decode_command_failure(&encoded), Some(failure));
    }

    #[test]
    fn arbitrary_strings_are_not_treated_as_typed_failures() {
        assert!(decode_command_failure("runtime compatibility blocked").is_none());
        assert!(decode_command_failure(COMMAND_ERROR_ENVELOPE_PREFIX).is_none());
    }
    #[test]
    fn unicode_messages_and_phases_are_bounded_before_serialization() {
        let failure = CommandFailure::new(
            CommandErrorCode::ProviderCapabilityInvalid,
            "中".repeat(20_000),
            "阶".repeat(500),
            CommandRecoverability::Reconfigure,
        );
        assert!(failure.safe_message.len() <= super::MAX_SAFE_MESSAGE_BYTES);
        assert!(failure.phase.len() <= super::MAX_PHASE_BYTES);
        assert!(decode_command_failure(&failure.encoded()).is_some());
    }

    #[test]
    fn escaping_and_post_construction_changes_cannot_exceed_the_wire_limit() {
        let mut failure = CommandFailure::new(
            CommandErrorCode::RuntimeCompatibilityBlocked,
            "safe",
            "preflight",
            CommandRecoverability::ManualInvestigation,
        );
        failure.safe_message = "\u{0001}".repeat(100_000);
        failure.phase = String::new();
        let encoded = failure.encoded();
        assert!(encoded.len() <= COMMAND_ERROR_ENVELOPE_PREFIX.len() + super::MAX_PAYLOAD_BYTES);
        let decoded = decode_command_failure(&encoded).unwrap();
        assert!(decoded.safe_message.len() <= super::MAX_SAFE_MESSAGE_BYTES);
        assert_eq!(decoded.phase, "unknown");
    }

    proptest! {
        #[test]
        fn typed_command_error_round_trip_is_message_independent(
            message in "[A-Za-z0-9 _.-]{1,128}",
            phase in "[a-z][A-Za-z0-9]{0,31}",
            code_index in 0u8..3,
            recovery_index in 0u8..5,
        ) {
            let code = match code_index {
                0 => CommandErrorCode::RuntimeCompatibilityBlocked,
                1 => CommandErrorCode::RuntimeCompatibilityUnavailable,
                _ => CommandErrorCode::ProviderCapabilityInvalid,
            };
            let recoverability = match recovery_index {
                0 => CommandRecoverability::Retry,
                1 => CommandRecoverability::Reconfigure,
                2 => CommandRecoverability::CloseWritersAndRetry,
                3 => CommandRecoverability::ManualInvestigation,
                _ => CommandRecoverability::UnsupportedClient,
            };
            let failure = CommandFailure::new(code, message, phase, recoverability);
            prop_assert_eq!(decode_command_failure(&failure.encoded()), Some(failure));
        }
    }
}

use std::{collections::BTreeSet, fs, path::Path};

use serde::{Deserialize, Serialize};

use crate::file_ops::atomic_write;

use super::{
    bounded_file::read_regular_file_bounded,
    semantic::{SemanticSession, TurnContextIdentity},
};

const SCHEMA_VERSION: u32 = 1;
const MAX_LEDGER_BYTES: u64 = 16 * 1024 * 1024;
const MAX_OPERATION_ID_BYTES: usize = 160;
const MAX_ACCOUNT_SLOT_BYTES: usize = 96;
const MAX_MODEL_BYTES: usize = 256;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RouteProvenanceStatus {
    Pending,
    Recorded,
    Unchanged,
    Failed,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RouteProvenanceReceipt {
    pub status: RouteProvenanceStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum TurnSourceStatus {
    Resolved,
    HistoricalUnknown,
    Incomplete,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TurnProvenance {
    pub turn_id: String,
    pub timestamp: String,
    pub model: Option<String>,
    pub status: TurnSourceStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_slot: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configured_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route_operation_id: Option<String>,
}

impl RouteProvenanceReceipt {
    pub(crate) fn pending() -> Self {
        Self {
            status: RouteProvenanceStatus::Pending,
            message: None,
        }
    }

    pub(crate) fn failed(message: String) -> Self {
        Self {
            status: RouteProvenanceStatus::Failed,
            message: Some(message),
        }
    }

    pub(crate) fn is_ready(&self) -> bool {
        matches!(
            self.status,
            RouteProvenanceStatus::Recorded | RouteProvenanceStatus::Unchanged
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RouteEpoch {
    schema_version: u32,
    operation_id: String,
    effective_at_ms: u128,
    runtime_id: String,
    provider: String,
    account_slot: String,
    model: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RouteProvenanceLedger {
    entries: Vec<RouteEpoch>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RouteEpochInput<'a> {
    operation_id: &'a str,
    effective_at_ms: u128,
    runtime_id: &'a str,
    provider: &'a str,
    account_slot: &'a str,
    model: Option<&'a str>,
}

impl<'a> RouteEpochInput<'a> {
    pub(crate) fn new(
        operation_id: &'a str,
        effective_at_ms: u128,
        runtime_id: &'a str,
        provider: &'a str,
        account_slot: &'a str,
        model: Option<&'a str>,
    ) -> Self {
        Self {
            operation_id,
            effective_at_ms,
            runtime_id,
            provider,
            account_slot,
            model,
        }
    }
}

impl RouteProvenanceLedger {
    pub(crate) fn load(data_root: &Path) -> Result<Self, String> {
        Ok(Self {
            entries: load_entries(&route_epoch_path(data_root))?,
        })
    }

    pub(crate) fn resolve(&self, session: &SemanticSession) -> Result<Vec<TurnProvenance>, String> {
        resolve_turns(&self.entries, session)
    }
}

pub(crate) fn record_or_verify_route_epoch(
    data_root: &Path,
    input: RouteEpochInput<'_>,
    allow_append: bool,
) -> Result<RouteProvenanceReceipt, String> {
    let RouteEpochInput {
        operation_id,
        effective_at_ms,
        runtime_id,
        provider,
        account_slot,
        model,
    } = input;
    let expected = RouteEpoch {
        schema_version: SCHEMA_VERSION,
        operation_id: operation_id.to_string(),
        effective_at_ms,
        runtime_id: runtime_id.to_string(),
        provider: provider.to_string(),
        account_slot: account_slot.to_string(),
        model: model.map(str::to_string),
    };
    validate_epoch(&expected)?;
    let path = route_epoch_path(data_root);
    let mut entries = load_entries(&path)?;

    if let Some(existing) = entries
        .iter()
        .find(|entry| entry.operation_id == operation_id)
    {
        if existing == &expected {
            return Ok(RouteProvenanceReceipt {
                status: RouteProvenanceStatus::Unchanged,
                message: None,
            });
        }
        return Err(
            "route provenance operation already exists with different metadata".to_string(),
        );
    }

    let matches_latest = entries.last().is_some_and(|entry| {
        entry.runtime_id == expected.runtime_id
            && entry.provider == expected.provider
            && entry.account_slot == expected.account_slot
            && entry.model == expected.model
    });
    if matches_latest {
        return Ok(RouteProvenanceReceipt {
            status: RouteProvenanceStatus::Unchanged,
            message: None,
        });
    }

    if !allow_append {
        return Err("route provenance baseline is missing for the active runtime".to_string());
    }

    if entries
        .last()
        .is_some_and(|entry| entry.effective_at_ms > expected.effective_at_ms)
    {
        return Err("route provenance clock moved backwards".to_string());
    }

    entries.push(expected);
    let mut bytes = Vec::new();
    for entry in &entries {
        serde_json::to_writer(&mut bytes, entry)
            .map_err(|_| "failed to serialize route provenance".to_string())?;
        bytes.push(b'\n');
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_LEDGER_BYTES {
            return Err("route provenance ledger reached its size limit".to_string());
        }
    }
    atomic_write(&path, &bytes)?;
    let persisted = load_entries(&path)?;
    if persisted.last() != entries.last() || persisted.len() != entries.len() {
        return Err("route provenance verification failed".to_string());
    }
    Ok(RouteProvenanceReceipt {
        status: RouteProvenanceStatus::Recorded,
        message: None,
    })
}

#[cfg(test)]
fn resolve_turn_provenance(
    data_root: &Path,
    session: &SemanticSession,
) -> Result<Vec<TurnProvenance>, String> {
    RouteProvenanceLedger::load(data_root)?.resolve(session)
}

fn resolve_turns(
    entries: &[RouteEpoch],
    session: &SemanticSession,
) -> Result<Vec<TurnProvenance>, String> {
    let mut seen_turn_ids = BTreeSet::new();
    session
        .turn_contexts
        .iter()
        .map(|turn| {
            if !seen_turn_ids.insert(turn.turn_id.as_str()) {
                return Err("session contains duplicate turn provenance identifiers".to_string());
            }
            let timestamp_ms = parse_rfc3339_millis(&turn.timestamp)
                .ok_or_else(|| "turn provenance timestamp is unsupported".to_string())?;
            let epoch = entries
                .iter()
                .rev()
                .find(|entry| entry.effective_at_ms <= timestamp_ms);
            Ok(turn_provenance(turn, epoch))
        })
        .collect()
}

fn turn_provenance(turn: &TurnContextIdentity, epoch: Option<&RouteEpoch>) -> TurnProvenance {
    TurnProvenance {
        turn_id: turn.turn_id.clone(),
        timestamp: turn.timestamp.clone(),
        model: turn.model.clone(),
        status: match (epoch, turn.model.as_ref()) {
            (Some(_), Some(_)) => TurnSourceStatus::Resolved,
            (Some(_), None) => TurnSourceStatus::Incomplete,
            (None, _) => TurnSourceStatus::HistoricalUnknown,
        },
        provider: epoch.map(|entry| entry.provider.clone()),
        account_slot: epoch.map(|entry| entry.account_slot.clone()),
        configured_model: epoch.and_then(|entry| entry.model.clone()),
        route_operation_id: epoch.map(|entry| entry.operation_id.clone()),
    }
}

fn route_epoch_path(data_root: &Path) -> std::path::PathBuf {
    data_root.join("session-storage-v1/route-epochs.jsonl")
}

fn load_entries(path: &Path) -> Result<Vec<RouteEpoch>, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => return Err("route provenance ledger is not a regular file".to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err("failed to inspect route provenance ledger".to_string()),
    }
    let bytes = read_regular_file_bounded(path, MAX_LEDGER_BYTES)
        .map_err(|_| "failed to read route provenance ledger".to_string())?;
    if !bytes.ends_with(b"\n") {
        return Err("route provenance ledger has an incomplete tail".to_string());
    }
    let entries = bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            let entry = serde_json::from_slice::<RouteEpoch>(line)
                .map_err(|_| "route provenance ledger is invalid".to_string())?;
            validate_epoch(&entry)?;
            Ok(entry)
        })
        .collect::<Result<Vec<_>, String>>()?;
    validate_epoch_sequence(&entries)?;
    Ok(entries)
}

fn validate_epoch_sequence(entries: &[RouteEpoch]) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    let mut previous = None;
    for entry in entries {
        if !seen.insert(entry.operation_id.as_str()) {
            return Err("route provenance ledger contains a duplicate operation".to_string());
        }
        if previous.is_some_and(|timestamp| timestamp > entry.effective_at_ms) {
            return Err("route provenance ledger timestamps are not ordered".to_string());
        }
        previous = Some(entry.effective_at_ms);
    }
    Ok(())
}

fn validate_epoch(epoch: &RouteEpoch) -> Result<(), String> {
    if epoch.schema_version != SCHEMA_VERSION
        || epoch.operation_id.is_empty()
        || epoch.operation_id.len() > MAX_OPERATION_ID_BYTES
        || !epoch
            .operation_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        || !matches!(epoch.runtime_id.as_str(), "plus" | "relay")
        || !valid_account_slot(&epoch.account_slot, &epoch.runtime_id)
        || !matches!(epoch.provider.as_str(), "openai" | "openai_custom")
        || epoch.model.as_ref().is_some_and(|model| {
            model.is_empty() || model.len() > MAX_MODEL_BYTES || model.chars().any(char::is_control)
        })
    {
        return Err("route provenance metadata is invalid".to_string());
    }
    let expected_provider = if epoch.runtime_id == "plus" {
        "openai"
    } else {
        "openai_custom"
    };
    if epoch.provider != expected_provider {
        return Err("route provenance provider does not match the runtime".to_string());
    }
    Ok(())
}

fn valid_account_slot(account_slot: &str, runtime_id: &str) -> bool {
    if account_slot.len() > MAX_ACCOUNT_SLOT_BYTES {
        return false;
    }
    account_slot
        .strip_prefix(runtime_id)
        .and_then(|suffix| suffix.strip_prefix(':'))
        .is_some_and(|generation| {
            !generation.is_empty() && generation.bytes().all(|byte| byte.is_ascii_digit())
        })
}

pub(crate) fn parse_rfc3339_millis(value: &str) -> Option<u128> {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || bytes.get(10) != Some(&b'T')
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
    {
        return None;
    }
    let year = parse_digits(bytes.get(0..4)?)? as i64;
    let month = parse_digits(bytes.get(5..7)?)? as i64;
    let day = parse_digits(bytes.get(8..10)?)? as i64;
    let hour = parse_digits(bytes.get(11..13)?)? as i64;
    let minute = parse_digits(bytes.get(14..16)?)? as i64;
    let second = parse_digits(bytes.get(17..19)?)? as i64;
    if year < 1970
        || !(1..=12).contains(&month)
        || day < 1
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }

    let mut cursor = 19;
    let mut fractional_millis = 0_i64;
    if bytes.get(cursor) == Some(&b'.') {
        cursor += 1;
        let start = cursor;
        while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
            cursor += 1;
        }
        let fraction = bytes.get(start..cursor)?;
        if fraction.is_empty() || fraction.len() > 9 {
            return None;
        }
        for index in 0..3 {
            fractional_millis *= 10;
            if let Some(digit) = fraction.get(index) {
                fractional_millis += i64::from(digit - b'0');
            }
        }
    }

    let offset_seconds = match bytes.get(cursor) {
        Some(b'Z') if cursor + 1 == bytes.len() => 0_i64,
        Some(sign @ (b'+' | b'-')) if cursor + 6 == bytes.len() => {
            if bytes.get(cursor + 3) != Some(&b':') {
                return None;
            }
            let offset_hour = parse_digits(bytes.get(cursor + 1..cursor + 3)?)? as i64;
            let offset_minute = parse_digits(bytes.get(cursor + 4..cursor + 6)?)? as i64;
            if offset_hour > 23 || offset_minute > 59 {
                return None;
            }
            let seconds = offset_hour * 3_600 + offset_minute * 60;
            if *sign == b'+' {
                seconds
            } else {
                -seconds
            }
        }
        _ => return None,
    };

    let days = days_from_civil(year, month, day);
    let unix_seconds = days
        .checked_mul(86_400)?
        .checked_add(hour * 3_600 + minute * 60 + second)?
        .checked_sub(offset_seconds)?;
    if unix_seconds < 0 {
        return None;
    }
    u128::try_from(unix_seconds)
        .ok()?
        .checked_mul(1_000)?
        .checked_add(u128::try_from(fractional_millis).ok()?)
}

fn parse_digits(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    bytes.iter().try_fold(0_u32, |value, digit| {
        value.checked_mul(10)?.checked_add(u32::from(digit - b'0'))
    })
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 400 == 0 || (year % 4 == 0 && year % 100 != 0) => 29,
        2 => 28,
        _ => 0,
    }
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let adjusted_year = year - i64::from(month <= 2);
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let adjusted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{
        parse_rfc3339_millis, record_or_verify_route_epoch, resolve_turn_provenance,
        RouteEpochInput, RouteProvenanceStatus, TurnSourceStatus,
    };
    use crate::session_storage::semantic::read_semantic_session;

    fn epoch<'a>(
        operation_id: &'a str,
        effective_at_ms: u128,
        runtime_id: &'a str,
        provider: &'a str,
        account_slot: &'a str,
        model: Option<&'a str>,
    ) -> RouteEpochInput<'a> {
        RouteEpochInput::new(
            operation_id,
            effective_at_ms,
            runtime_id,
            provider,
            account_slot,
            model,
        )
    }

    #[test]
    fn records_idempotent_epochs_and_verifies_a_closed_noop() {
        let root = tempdir().unwrap();
        let first = record_or_verify_route_epoch(
            root.path(),
            epoch(
                "switch-runtime-1",
                10,
                "relay",
                "openai_custom",
                "relay:1",
                Some("relay-model"),
            ),
            true,
        )
        .unwrap();
        assert_eq!(first.status, RouteProvenanceStatus::Recorded);

        let replay = record_or_verify_route_epoch(
            root.path(),
            epoch(
                "switch-runtime-1",
                10,
                "relay",
                "openai_custom",
                "relay:1",
                Some("relay-model"),
            ),
            true,
        )
        .unwrap();
        assert_eq!(replay.status, RouteProvenanceStatus::Unchanged);

        let no_op = record_or_verify_route_epoch(
            root.path(),
            epoch(
                "switch-runtime-2",
                20,
                "relay",
                "openai_custom",
                "relay:1",
                Some("relay-model"),
            ),
            false,
        )
        .unwrap();
        assert_eq!(no_op.status, RouteProvenanceStatus::Unchanged);
    }

    #[test]
    fn rejects_a_missing_noop_baseline_and_mismatched_duplicate() {
        let root = tempdir().unwrap();
        let missing = record_or_verify_route_epoch(
            root.path(),
            epoch(
                "switch-runtime-1",
                10,
                "relay",
                "openai_custom",
                "relay:1",
                Some("relay-model"),
            ),
            false,
        )
        .unwrap_err();
        assert!(missing.contains("baseline"));

        record_or_verify_route_epoch(
            root.path(),
            epoch(
                "switch-runtime-1",
                10,
                "plus",
                "openai",
                "plus:1",
                Some("account-model"),
            ),
            true,
        )
        .unwrap();
        let mismatch = record_or_verify_route_epoch(
            root.path(),
            epoch(
                "switch-runtime-1",
                11,
                "plus",
                "openai",
                "plus:1",
                Some("account-model"),
            ),
            true,
        )
        .unwrap_err();
        assert!(mismatch.contains("different metadata"));
    }

    #[test]
    fn fails_closed_on_a_truncated_or_untrusted_ledger() {
        let root = tempdir().unwrap();
        let path = root.path().join("session-storage-v1/route-epochs.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{\"schemaVersion\":1}").unwrap();
        let error = record_or_verify_route_epoch(
            root.path(),
            epoch(
                "switch-runtime-1",
                10,
                "plus",
                "openai",
                "plus:1",
                Some("account-model"),
            ),
            true,
        )
        .unwrap_err();
        assert!(error.contains("incomplete tail"));
    }

    #[test]
    fn resolves_each_turn_against_the_latest_route_epoch() {
        let root = tempdir().unwrap();
        record_or_verify_route_epoch(
            root.path(),
            epoch(
                "switch-runtime-1",
                1_000,
                "plus",
                "openai",
                "plus:10",
                Some("configured-account-model"),
            ),
            true,
        )
        .unwrap();
        record_or_verify_route_epoch(
            root.path(),
            epoch(
                "switch-runtime-2",
                3_000,
                "relay",
                "openai_custom",
                "relay:20",
                Some("configured-relay-model"),
            ),
            true,
        )
        .unwrap();
        let session_path = root.path().join("session.jsonl");
        let lines = [
            serde_json::json!({"type":"session_meta","payload":{"id":"thread-a","model_provider":"openai"}}),
            serde_json::json!({"timestamp":"1970-01-01T00:00:00.500Z","type":"turn_context","payload":{"turn_id":"turn-0","model":"actual-0"}}),
            serde_json::json!({"timestamp":"1970-01-01T00:00:01.500Z","type":"turn_context","payload":{"turn_id":"turn-1","model":"actual-1"}}),
            serde_json::json!({"timestamp":"1970-01-01T00:00:03.250Z","type":"turn_context","payload":{"turn_id":"turn-2","model":"actual-2"}}),
        ];
        let body = lines
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .join("\n")
            + "\n";
        fs::write(&session_path, body).unwrap();
        let session = read_semantic_session(&session_path).unwrap();

        let turns = resolve_turn_provenance(root.path(), &session).unwrap();

        assert_eq!(turns.len(), 3);
        assert_eq!(turns[0].status, TurnSourceStatus::HistoricalUnknown);
        assert_eq!(turns[0].provider, None);
        assert_eq!(turns[1].provider.as_deref(), Some("openai"));
        assert_eq!(turns[1].account_slot.as_deref(), Some("plus:10"));
        assert_eq!(turns[1].model.as_deref(), Some("actual-1"));
        assert_eq!(
            turns[1].configured_model.as_deref(),
            Some("configured-account-model")
        );
        assert_eq!(turns[2].provider.as_deref(), Some("openai_custom"));
        assert_eq!(turns[2].account_slot.as_deref(), Some("relay:20"));
    }

    #[test]
    fn rejects_clock_regression_and_invalid_ledger_order() {
        let root = tempdir().unwrap();
        record_or_verify_route_epoch(
            root.path(),
            epoch("switch-runtime-1", 20, "plus", "openai", "plus:1", None),
            true,
        )
        .unwrap();
        let error = record_or_verify_route_epoch(
            root.path(),
            epoch(
                "switch-runtime-2",
                10,
                "relay",
                "openai_custom",
                "relay:1",
                None,
            ),
            true,
        )
        .unwrap_err();
        assert!(error.contains("clock moved backwards"));
    }

    #[test]
    fn parses_utc_and_offset_rfc3339_timestamps_without_rounding_up() {
        assert_eq!(
            parse_rfc3339_millis("1970-01-01T00:00:01.234567Z"),
            Some(1_234)
        );
        assert_eq!(
            parse_rfc3339_millis("1970-01-01T08:00:01.234+08:00"),
            Some(1_234)
        );
        assert_eq!(parse_rfc3339_millis("2024-02-30T00:00:00Z"), None);
    }
}

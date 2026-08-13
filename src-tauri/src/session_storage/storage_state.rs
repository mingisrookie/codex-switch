use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{file_ops::atomic_write, operation_log::timestamp_millis};

use super::{
    bounded_file::read_regular_file_bounded,
    metrics::load_reclaim_metrics,
    migration::{load_migration_preflight, MigrationPreflightReport, MigrationSessionAction},
    operation_ledger::{
        OperationLedgerStore, SessionStorageOperationKind, SessionStorageOperationLedger,
        SessionStorageOperationPhase,
    },
    reference_graph::path_key,
};

const CANONICAL_STATE_SCHEMA_VERSION: u32 = 2;
const LEGACY_CANONICAL_STATE_SCHEMA_VERSION: u32 = 1;
const SETTINGS_SCHEMA_VERSION: u32 = 1;
const CONTROL_STATE_SCHEMA_VERSION: u32 = 1;
const MAX_CANONICAL_STATE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ENCRYPTED_STATE_BYTES: u64 = MAX_CANONICAL_STATE_BYTES * 2 + 64 * 1024;
const MAX_SETTINGS_BYTES: u64 = 256 * 1024;
const CANONICAL_STATE_NAME: &str = "canonical-storage-state-v1.json";
const CANONICAL_STATE_CIPHERTEXT_MAGIC: &[u8] = b"CSCANONICAL2\0";
const SETTINGS_NAME: &str = "settings-v1.json";

/// Durable certificate that the canonical migration committed successfully.
///
/// Unlike the seven-day operation ledger and preflight, this certificate is
/// part of the long-lived storage control plane. On Windows its entire
/// envelope is protected with current-user DPAPI because it contains local
/// absolute paths.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CanonicalStorageState {
    pub schema_version: u32,
    pub migration_operation_id: String,
    pub canonical_root: PathBuf,
    pub inventory_fingerprint: String,
    pub prepared_at_ms: u128,
    pub committed_at_ms: u128,
    pub backup_destination: PathBuf,
    pub gc_discovery_paths: Vec<PathBuf>,
}

impl CanonicalStorageState {
    pub fn gc_discovery_paths(&self) -> &[PathBuf] {
        &self.gc_discovery_paths
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PreparedCanonicalStorageState {
    schema_version: u32,
    migration_operation_id: String,
    canonical_root: PathBuf,
    inventory_fingerprint: String,
    prepared_at_ms: u128,
    backup_destination: PathBuf,
    gc_discovery_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "phase",
    content = "certificate",
    rename_all = "camelCase",
    deny_unknown_fields
)]
enum CanonicalStorageStateRecord {
    Prepared(PreparedCanonicalStorageState),
    Committed(CanonicalStorageState),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CanonicalStorageStateEnvelope {
    record: CanonicalStorageStateRecord,
    integrity_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LegacyCanonicalStorageStateV1 {
    schema_version: u32,
    migration_operation_id: String,
    canonical_root: PathBuf,
    inventory_fingerprint: String,
    prepared_at_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LegacyCanonicalStorageStateEnvelopeV1 {
    state: LegacyCanonicalStorageStateV1,
    integrity_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LoadedCanonicalStorageState {
    V2(CanonicalStorageStateRecord),
    V1(LegacyCanonicalStorageStateV1),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionStorageSettings {
    pub schema_version: u32,
    pub automatic_cleanup_enabled: bool,
    pub updated_at_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SessionStorageSettingsEnvelope {
    settings: SessionStorageSettings,
    integrity_sha256: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionStorageControlState {
    pub schema_version: u32,
    pub canonical_ready: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migration_operation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migration_prepared_at_ms: Option<u128>,
    pub automatic_cleanup_enabled: bool,
    pub online_deletion_enabled: bool,
    pub reclaimed_bytes: u64,
}

/// Persists the pre-commit half of the durable canonical-state certificate.
/// The matching migration preflight must still exist and match exactly.
pub fn prepare_canonical_storage_state(
    data_root: &Path,
    canonical_root: &Path,
    migration_operation_id: &str,
    inventory_fingerprint: &str,
) -> Result<(), String> {
    validate_data_root(data_root)?;
    validate_operation_id(migration_operation_id)?;
    validate_sha256(inventory_fingerprint)?;
    let ledger = matching_ledger(data_root, migration_operation_id, canonical_root)?
        .ok_or_else(|| "canonical storage migration ledger proof is unavailable".to_string())?;
    if ledger.phase != SessionStorageOperationPhase::Validating {
        return Err(
            "canonical storage migration is not ready to prepare its certificate".to_string(),
        );
    }
    let report = load_matching_preflight(
        data_root,
        canonical_root,
        migration_operation_id,
        inventory_fingerprint,
    )?;
    let prepared = PreparedCanonicalStorageState {
        schema_version: CANONICAL_STATE_SCHEMA_VERSION,
        migration_operation_id: migration_operation_id.to_string(),
        canonical_root: canonical_root.to_path_buf(),
        inventory_fingerprint: inventory_fingerprint.to_string(),
        prepared_at_ms: timestamp_millis()?,
        backup_destination: report.backup_destination.clone(),
        gc_discovery_paths: migration_gc_discovery_paths(&report)?,
    };
    validate_prepared_state(&prepared, canonical_root)?;
    let record = CanonicalStorageStateRecord::Prepared(prepared);

    if let Some(existing) = read_canonical_state(data_root)? {
        return match (&existing, &record) {
            (
                LoadedCanonicalStorageState::V2(CanonicalStorageStateRecord::Prepared(existing)),
                CanonicalStorageStateRecord::Prepared(candidate),
            ) if same_preparation_identity(existing, candidate) => Ok(()),
            _ => Err("canonical storage state already belongs to another migration".to_string()),
        };
    }

    persist_canonical_record(data_root, &record)
}

/// Finalizes a prepared v2 state (or a proven v1 state) after the migration
/// ledger reaches `Committed`. This is idempotent after the v2 certificate has
/// been written, including after the seven-day ledger/preflight retention has
/// removed its original proof files.
pub fn finalize_canonical_storage_state(
    data_root: &Path,
    canonical_root: &Path,
    migration_operation_id: &str,
) -> Result<CanonicalStorageState, String> {
    validate_data_root(data_root)?;
    validate_operation_id(migration_operation_id)?;
    let loaded = read_canonical_state(data_root)?
        .ok_or_else(|| "canonical storage state is unavailable".to_string())?;
    match loaded {
        LoadedCanonicalStorageState::V2(CanonicalStorageStateRecord::Committed(state)) => {
            validate_committed_state(&state, canonical_root)?;
            if state.migration_operation_id != migration_operation_id {
                return Err("canonical storage state migration identity changed".to_string());
            }
            validate_optional_retained_ledger(data_root, &state)?;
            Ok(state)
        }
        LoadedCanonicalStorageState::V2(CanonicalStorageStateRecord::Prepared(prepared)) => {
            if prepared.migration_operation_id != migration_operation_id {
                return Err("canonical storage state migration identity changed".to_string());
            }
            finalize_prepared_state(data_root, canonical_root, prepared)
        }
        LoadedCanonicalStorageState::V1(legacy) => {
            if legacy.migration_operation_id != migration_operation_id {
                return Err("canonical storage state migration identity changed".to_string());
            }
            finalize_legacy_state(data_root, canonical_root, legacy)
        }
    }
}

pub fn load_committed_canonical_storage_state(
    data_root: &Path,
    canonical_root: &Path,
) -> Result<Option<CanonicalStorageState>, String> {
    validate_data_root(data_root)?;
    let Some(loaded) = read_canonical_state(data_root)? else {
        return Ok(None);
    };
    match loaded {
        LoadedCanonicalStorageState::V2(CanonicalStorageStateRecord::Committed(state)) => {
            validate_committed_state(&state, canonical_root)?;
            validate_optional_retained_ledger(data_root, &state)?;
            Ok(Some(state))
        }
        LoadedCanonicalStorageState::V2(CanonicalStorageStateRecord::Prepared(prepared)) => {
            validate_prepared_state(&prepared, canonical_root)?;
            let Some(ledger) =
                matching_ledger(data_root, &prepared.migration_operation_id, canonical_root)?
            else {
                return Err("prepared canonical storage state lost its migration proof".to_string());
            };
            if ledger.phase != SessionStorageOperationPhase::Committed {
                return Ok(None);
            }
            finalize_prepared_state(data_root, canonical_root, prepared).map(Some)
        }
        LoadedCanonicalStorageState::V1(legacy) => {
            validate_legacy_state(&legacy, canonical_root)?;
            let Some(ledger) =
                matching_ledger(data_root, &legacy.migration_operation_id, canonical_root)?
            else {
                return Err("legacy canonical storage state has no committed proof".to_string());
            };
            if ledger.phase != SessionStorageOperationPhase::Committed {
                return Ok(None);
            }
            finalize_legacy_state(data_root, canonical_root, legacy).map(Some)
        }
    }
}

pub fn clear_canonical_storage_state_for_operation(
    data_root: &Path,
    migration_operation_id: &str,
) -> Result<(), String> {
    validate_data_root(data_root)?;
    validate_operation_id(migration_operation_id)?;
    let path = canonical_state_path(data_root);
    let Some(loaded) = read_canonical_state(data_root)? else {
        return Ok(());
    };
    let persisted_operation_id = match loaded {
        LoadedCanonicalStorageState::V2(CanonicalStorageStateRecord::Prepared(state)) => {
            state.migration_operation_id
        }
        LoadedCanonicalStorageState::V2(CanonicalStorageStateRecord::Committed(state)) => {
            state.migration_operation_id
        }
        LoadedCanonicalStorageState::V1(state) => state.migration_operation_id,
    };
    if persisted_operation_id != migration_operation_id {
        return Ok(());
    }
    fs::remove_file(path).map_err(|_| "failed to remove canonical storage state".to_string())
}

pub fn load_session_storage_settings(data_root: &Path) -> Result<SessionStorageSettings, String> {
    let path = settings_path(data_root);
    let bytes = match read_regular_file_bounded(&path, MAX_SETTINGS_BYTES) {
        Ok(bytes) => bytes,
        Err(_) if !path.exists() => {
            return Ok(SessionStorageSettings {
                schema_version: SETTINGS_SCHEMA_VERSION,
                automatic_cleanup_enabled: true,
                updated_at_ms: 0,
            });
        }
        Err(_) => return Err("session storage settings are unreadable".to_string()),
    };
    let envelope = serde_json::from_slice::<SessionStorageSettingsEnvelope>(&bytes)
        .map_err(|_| "session storage settings are invalid".to_string())?;
    validate_settings(&envelope.settings)?;
    if digest(&envelope.settings)? != envelope.integrity_sha256 {
        return Err("session storage settings integrity check failed".to_string());
    }
    Ok(envelope.settings)
}

pub fn set_automatic_cleanup_enabled(
    data_root: &Path,
    enabled: bool,
) -> Result<SessionStorageSettings, String> {
    let settings = SessionStorageSettings {
        schema_version: SETTINGS_SCHEMA_VERSION,
        automatic_cleanup_enabled: enabled,
        updated_at_ms: timestamp_millis()?,
    };
    let envelope = SessionStorageSettingsEnvelope {
        integrity_sha256: digest(&settings)?,
        settings: settings.clone(),
    };
    persist_envelope(&settings_path(data_root), &envelope)?;
    let verified = load_session_storage_settings(data_root)?;
    if verified != settings {
        return Err("session storage settings verification failed".to_string());
    }
    Ok(settings)
}

pub fn load_session_storage_control_state(
    data_root: &Path,
    canonical_root: &Path,
) -> Result<SessionStorageControlState, String> {
    let canonical = load_committed_canonical_storage_state(data_root, canonical_root)?;
    let settings = load_session_storage_settings(data_root)?;
    let metrics = load_reclaim_metrics(data_root)?;
    Ok(SessionStorageControlState {
        schema_version: CONTROL_STATE_SCHEMA_VERSION,
        canonical_ready: canonical.is_some(),
        migration_operation_id: canonical
            .as_ref()
            .map(|state| state.migration_operation_id.clone()),
        migration_prepared_at_ms: canonical.as_ref().map(|state| state.prepared_at_ms),
        automatic_cleanup_enabled: settings.automatic_cleanup_enabled,
        online_deletion_enabled: false,
        reclaimed_bytes: metrics.reclaimed_bytes,
    })
}

fn finalize_prepared_state(
    data_root: &Path,
    canonical_root: &Path,
    prepared: PreparedCanonicalStorageState,
) -> Result<CanonicalStorageState, String> {
    validate_prepared_state(&prepared, canonical_root)?;
    let ledger =
        require_committed_ledger(data_root, &prepared.migration_operation_id, canonical_root)?;
    let report = load_matching_preflight(
        data_root,
        canonical_root,
        &prepared.migration_operation_id,
        &prepared.inventory_fingerprint,
    )?;
    validate_prepared_evidence(&prepared, &report, &ledger)?;
    let state = CanonicalStorageState {
        schema_version: CANONICAL_STATE_SCHEMA_VERSION,
        migration_operation_id: prepared.migration_operation_id,
        canonical_root: prepared.canonical_root,
        inventory_fingerprint: prepared.inventory_fingerprint,
        prepared_at_ms: prepared.prepared_at_ms,
        committed_at_ms: ledger.updated_at_ms,
        backup_destination: prepared.backup_destination,
        gc_discovery_paths: prepared.gc_discovery_paths,
    };
    validate_committed_state(&state, canonical_root)?;
    persist_canonical_record(
        data_root,
        &CanonicalStorageStateRecord::Committed(state.clone()),
    )?;
    Ok(state)
}

fn finalize_legacy_state(
    data_root: &Path,
    canonical_root: &Path,
    legacy: LegacyCanonicalStorageStateV1,
) -> Result<CanonicalStorageState, String> {
    validate_legacy_state(&legacy, canonical_root)?;
    let ledger =
        require_committed_ledger(data_root, &legacy.migration_operation_id, canonical_root)?;
    let report = load_matching_preflight(
        data_root,
        canonical_root,
        &legacy.migration_operation_id,
        &legacy.inventory_fingerprint,
    )?;
    validate_backup_ledger_identity(&report, &ledger)?;
    if legacy.prepared_at_ms < ledger.started_at_ms || ledger.updated_at_ms < legacy.prepared_at_ms
    {
        return Err("legacy canonical storage state timestamps are invalid".to_string());
    }
    let state = CanonicalStorageState {
        schema_version: CANONICAL_STATE_SCHEMA_VERSION,
        migration_operation_id: legacy.migration_operation_id,
        canonical_root: legacy.canonical_root,
        inventory_fingerprint: legacy.inventory_fingerprint,
        prepared_at_ms: legacy.prepared_at_ms,
        committed_at_ms: ledger.updated_at_ms,
        backup_destination: report.backup_destination.clone(),
        gc_discovery_paths: migration_gc_discovery_paths(&report)?,
    };
    validate_committed_state(&state, canonical_root)?;
    persist_canonical_record(
        data_root,
        &CanonicalStorageStateRecord::Committed(state.clone()),
    )?;
    Ok(state)
}

fn validate_prepared_evidence(
    prepared: &PreparedCanonicalStorageState,
    report: &MigrationPreflightReport,
    ledger: &SessionStorageOperationLedger,
) -> Result<(), String> {
    if path_key(&prepared.backup_destination) != path_key(&report.backup_destination)
        || prepared.gc_discovery_paths != migration_gc_discovery_paths(report)?
        || prepared.prepared_at_ms < ledger.started_at_ms
        || ledger.updated_at_ms < prepared.prepared_at_ms
    {
        return Err("canonical storage state commit proof changed".to_string());
    }
    validate_backup_ledger_identity(report, ledger)
}

fn validate_backup_ledger_identity(
    report: &MigrationPreflightReport,
    ledger: &SessionStorageOperationLedger,
) -> Result<(), String> {
    let expected_backup_root = report.backup_destination.join(&report.operation_id);
    if ledger
        .backup_root
        .as_ref()
        .is_none_or(|path| path_key(path) != path_key(&expected_backup_root))
    {
        return Err("canonical storage migration backup proof is invalid".to_string());
    }
    Ok(())
}

fn load_matching_preflight(
    data_root: &Path,
    canonical_root: &Path,
    migration_operation_id: &str,
    inventory_fingerprint: &str,
) -> Result<MigrationPreflightReport, String> {
    let report = load_migration_preflight(data_root, migration_operation_id)
        .map_err(|_| "canonical storage migration preflight proof is unavailable".to_string())?;
    if report.operation_id != migration_operation_id
        || !report.ready_for_backup
        || !report.blockers.is_empty()
        || path_key(&report.plan.canonical_root) != path_key(canonical_root)
        || report.plan.inventory_fingerprint != inventory_fingerprint
        || !report.backup_destination.is_absolute()
    {
        return Err("canonical storage migration preflight proof is invalid".to_string());
    }
    Ok(report)
}

fn migration_gc_discovery_paths(report: &MigrationPreflightReport) -> Result<Vec<PathBuf>, String> {
    let mut paths = Vec::new();
    for session in report
        .plan
        .sessions
        .iter()
        .filter(|session| session.action != MigrationSessionAction::Conflict)
    {
        paths.push(session.retained_path.clone());
        paths.extend(
            session
                .duplicates
                .iter()
                .map(|duplicate| duplicate.path.clone()),
        );
    }
    normalize_gc_discovery_paths(paths)
}

fn normalize_gc_discovery_paths(mut paths: Vec<PathBuf>) -> Result<Vec<PathBuf>, String> {
    if paths.iter().any(|path| !path.is_absolute()) {
        return Err("canonical storage GC discovery path is invalid".to_string());
    }
    paths.sort_by_key(|path| durable_path_key(path));
    paths.dedup_by(|left, right| durable_path_key(left) == durable_path_key(right));
    Ok(paths)
}

fn matching_ledger(
    data_root: &Path,
    migration_operation_id: &str,
    canonical_root: &Path,
) -> Result<Option<SessionStorageOperationLedger>, String> {
    let Some(ledger) = OperationLedgerStore::new(data_root).try_load(migration_operation_id)?
    else {
        return Ok(None);
    };
    if ledger.operation_id != migration_operation_id
        || ledger.kind != SessionStorageOperationKind::Migration
        || path_key(&ledger.canonical_root) != path_key(canonical_root)
    {
        return Err("canonical storage migration ledger identity is invalid".to_string());
    }
    Ok(Some(ledger))
}

fn require_committed_ledger(
    data_root: &Path,
    migration_operation_id: &str,
    canonical_root: &Path,
) -> Result<SessionStorageOperationLedger, String> {
    let ledger = matching_ledger(data_root, migration_operation_id, canonical_root)?
        .ok_or_else(|| "canonical storage migration ledger proof is unavailable".to_string())?;
    if ledger.phase != SessionStorageOperationPhase::Committed {
        return Err("canonical storage migration is not committed".to_string());
    }
    Ok(ledger)
}

fn validate_optional_retained_ledger(
    data_root: &Path,
    state: &CanonicalStorageState,
) -> Result<(), String> {
    let Some(ledger) = matching_ledger(
        data_root,
        &state.migration_operation_id,
        &state.canonical_root,
    )?
    else {
        return Ok(());
    };
    let expected_backup_root = state.backup_destination.join(&state.migration_operation_id);
    if ledger.phase != SessionStorageOperationPhase::Committed
        || ledger.updated_at_ms != state.committed_at_ms
        || ledger
            .backup_root
            .as_ref()
            .is_none_or(|path| path_key(path) != path_key(&expected_backup_root))
    {
        return Err(
            "canonical storage migration ledger no longer matches its certificate".to_string(),
        );
    }
    Ok(())
}

fn read_canonical_state(data_root: &Path) -> Result<Option<LoadedCanonicalStorageState>, String> {
    let path = canonical_state_path(data_root);
    let bytes = match read_regular_file_bounded(&path, MAX_ENCRYPTED_STATE_BYTES) {
        Ok(bytes) => bytes,
        Err(_) if !path.exists() => return Ok(None),
        Err(_) => return Err("canonical storage state is unreadable".to_string()),
    };

    if let Some(ciphertext) = bytes.strip_prefix(CANONICAL_STATE_CIPHERTEXT_MAGIC) {
        let plaintext = crate::crypto::unprotect(ciphertext)
            .map_err(|_| "canonical storage state is unreadable".to_string())?;
        if plaintext.len() as u64 > MAX_CANONICAL_STATE_BYTES {
            return Err("canonical storage state reached its size limit".to_string());
        }
        return parse_v2_envelope(&plaintext).map(Some);
    }
    if bytes.len() as u64 > MAX_CANONICAL_STATE_BYTES {
        return Err("canonical storage state reached its size limit".to_string());
    }

    #[cfg(windows)]
    {
        parse_v1_envelope(&bytes).map(Some)
    }
    #[cfg(not(windows))]
    {
        match parse_v2_envelope(&bytes) {
            Ok(record) => Ok(Some(record)),
            Err(_) => parse_v1_envelope(&bytes).map(Some),
        }
    }
}

fn parse_v2_envelope(bytes: &[u8]) -> Result<LoadedCanonicalStorageState, String> {
    let envelope = serde_json::from_slice::<CanonicalStorageStateEnvelope>(bytes)
        .map_err(|_| "canonical storage state is invalid".to_string())?;
    if digest(&envelope.record)? != envelope.integrity_sha256 {
        return Err("canonical storage state integrity check failed".to_string());
    }
    match &envelope.record {
        CanonicalStorageStateRecord::Prepared(state) => {
            validate_prepared_state_without_expected_root(state)?
        }
        CanonicalStorageStateRecord::Committed(state) => {
            validate_committed_state_without_expected_root(state)?
        }
    }
    Ok(LoadedCanonicalStorageState::V2(envelope.record))
}

fn parse_v1_envelope(bytes: &[u8]) -> Result<LoadedCanonicalStorageState, String> {
    let envelope = serde_json::from_slice::<LegacyCanonicalStorageStateEnvelopeV1>(bytes)
        .map_err(|_| "canonical storage state is invalid".to_string())?;
    if digest(&envelope.state)? != envelope.integrity_sha256 {
        return Err("canonical storage state integrity check failed".to_string());
    }
    validate_legacy_state_without_expected_root(&envelope.state)?;
    Ok(LoadedCanonicalStorageState::V1(envelope.state))
}

fn persist_canonical_record(
    data_root: &Path,
    record: &CanonicalStorageStateRecord,
) -> Result<(), String> {
    let envelope = CanonicalStorageStateEnvelope {
        record: record.clone(),
        integrity_sha256: digest(record)?,
    };
    let plaintext = serde_json::to_vec_pretty(&envelope)
        .map_err(|_| "failed to serialize canonical storage state".to_string())?;
    if plaintext.len() as u64 > MAX_CANONICAL_STATE_BYTES {
        return Err("canonical storage state reached its size limit".to_string());
    }
    #[cfg(windows)]
    let bytes = {
        let ciphertext = crate::crypto::protect(&plaintext)
            .map_err(|_| "failed to protect canonical storage state".to_string())?;
        let mut protected =
            Vec::with_capacity(CANONICAL_STATE_CIPHERTEXT_MAGIC.len() + ciphertext.len());
        protected.extend_from_slice(CANONICAL_STATE_CIPHERTEXT_MAGIC);
        protected.extend_from_slice(&ciphertext);
        protected
    };
    #[cfg(not(windows))]
    let bytes = plaintext;
    if bytes.len() as u64 > MAX_ENCRYPTED_STATE_BYTES {
        return Err("canonical storage state reached its size limit".to_string());
    }
    atomic_write(&canonical_state_path(data_root), &bytes)?;
    match read_canonical_state(data_root)? {
        Some(LoadedCanonicalStorageState::V2(persisted)) if &persisted == record => Ok(()),
        _ => Err("canonical storage state verification failed".to_string()),
    }
}

fn validate_prepared_state(
    state: &PreparedCanonicalStorageState,
    canonical_root: &Path,
) -> Result<(), String> {
    validate_prepared_state_without_expected_root(state)?;
    if path_key(&state.canonical_root) != path_key(canonical_root) {
        return Err("canonical storage state identity is invalid".to_string());
    }
    Ok(())
}

fn same_preparation_identity(
    left: &PreparedCanonicalStorageState,
    right: &PreparedCanonicalStorageState,
) -> bool {
    left.schema_version == right.schema_version
        && left.migration_operation_id == right.migration_operation_id
        && path_key(&left.canonical_root) == path_key(&right.canonical_root)
        && left.inventory_fingerprint == right.inventory_fingerprint
        && path_key(&left.backup_destination) == path_key(&right.backup_destination)
        && left.gc_discovery_paths == right.gc_discovery_paths
}

fn validate_prepared_state_without_expected_root(
    state: &PreparedCanonicalStorageState,
) -> Result<(), String> {
    if state.schema_version != CANONICAL_STATE_SCHEMA_VERSION
        || !state.canonical_root.is_absolute()
        || !state.backup_destination.is_absolute()
        || state.prepared_at_ms == 0
    {
        return Err("canonical storage state identity is invalid".to_string());
    }
    validate_operation_id(&state.migration_operation_id)?;
    validate_sha256(&state.inventory_fingerprint)?;
    validate_normalized_gc_paths(&state.gc_discovery_paths)
}

fn validate_committed_state(
    state: &CanonicalStorageState,
    canonical_root: &Path,
) -> Result<(), String> {
    validate_committed_state_without_expected_root(state)?;
    if path_key(&state.canonical_root) != path_key(canonical_root) {
        return Err("canonical storage state identity is invalid".to_string());
    }
    Ok(())
}

fn validate_committed_state_without_expected_root(
    state: &CanonicalStorageState,
) -> Result<(), String> {
    if state.schema_version != CANONICAL_STATE_SCHEMA_VERSION
        || !state.canonical_root.is_absolute()
        || !state.backup_destination.is_absolute()
        || state.prepared_at_ms == 0
        || state.committed_at_ms < state.prepared_at_ms
    {
        return Err("canonical storage state identity is invalid".to_string());
    }
    validate_operation_id(&state.migration_operation_id)?;
    validate_sha256(&state.inventory_fingerprint)?;
    validate_normalized_gc_paths(&state.gc_discovery_paths)
}

fn validate_normalized_gc_paths(paths: &[PathBuf]) -> Result<(), String> {
    if paths.iter().any(|path| !path.is_absolute()) {
        return Err("canonical storage GC discovery path is invalid".to_string());
    }
    let keys = paths
        .iter()
        .map(|path| durable_path_key(path))
        .collect::<Vec<_>>();
    let mut normalized = keys.clone();
    normalized.sort();
    normalized.dedup();
    if normalized != keys {
        return Err("canonical storage GC discovery paths are not normalized".to_string());
    }
    Ok(())
}

fn durable_path_key(path: &Path) -> String {
    let normalized = path.to_string_lossy().replace('/', "\\");
    if cfg!(windows) {
        normalized.to_ascii_lowercase()
    } else {
        normalized
    }
}

fn validate_legacy_state(
    state: &LegacyCanonicalStorageStateV1,
    canonical_root: &Path,
) -> Result<(), String> {
    validate_legacy_state_without_expected_root(state)?;
    if path_key(&state.canonical_root) != path_key(canonical_root) {
        return Err("legacy canonical storage state identity is invalid".to_string());
    }
    Ok(())
}

fn validate_legacy_state_without_expected_root(
    state: &LegacyCanonicalStorageStateV1,
) -> Result<(), String> {
    if state.schema_version != LEGACY_CANONICAL_STATE_SCHEMA_VERSION
        || !state.canonical_root.is_absolute()
        || state.prepared_at_ms == 0
    {
        return Err("legacy canonical storage state identity is invalid".to_string());
    }
    validate_operation_id(&state.migration_operation_id)?;
    validate_sha256(&state.inventory_fingerprint)
}

fn validate_settings(settings: &SessionStorageSettings) -> Result<(), String> {
    if settings.schema_version != SETTINGS_SCHEMA_VERSION {
        return Err("session storage settings version is unsupported".to_string());
    }
    Ok(())
}

fn validate_data_root(data_root: &Path) -> Result<(), String> {
    if !data_root.is_absolute() {
        return Err("canonical storage data root is invalid".to_string());
    }
    Ok(())
}

fn persist_envelope<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|_| "failed to serialize session storage state".to_string())?;
    if bytes.len() as u64 > MAX_SETTINGS_BYTES {
        return Err("session storage state reached its size limit".to_string());
    }
    atomic_write(path, &bytes)
}

fn canonical_state_path(data_root: &Path) -> PathBuf {
    data_root
        .join("session-storage-v1")
        .join(CANONICAL_STATE_NAME)
}

fn settings_path(data_root: &Path) -> PathBuf {
    data_root.join("session-storage-v1").join(SETTINGS_NAME)
}

fn digest<T: Serialize>(value: &T) -> Result<String, String> {
    let bytes = serde_json::to_vec(value)
        .map_err(|_| "failed to fingerprint session storage state".to_string())?;
    Ok(hex_sha256(Sha256::digest(bytes)))
}

fn validate_operation_id(operation_id: &str) -> Result<(), String> {
    if operation_id.is_empty()
        || operation_id.len() > 160
        || !operation_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("canonical storage migration operation id is invalid".to_string());
    }
    Ok(())
}

fn validate_sha256(value: &str) -> Result<(), String> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("session storage state checksum is invalid".to_string());
    }
    Ok(())
}

fn hex_sha256(value: impl AsRef<[u8]>) -> String {
    value
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use rusqlite::Connection;
    use tempfile::tempdir;

    #[cfg(windows)]
    use super::CANONICAL_STATE_CIPHERTEXT_MAGIC;
    use super::{
        canonical_state_path, digest, finalize_canonical_storage_state,
        load_committed_canonical_storage_state, load_session_storage_control_state,
        load_session_storage_settings, normalize_gc_discovery_paths,
        prepare_canonical_storage_state, set_automatic_cleanup_enabled,
        LegacyCanonicalStorageStateEnvelopeV1, LegacyCanonicalStorageStateV1,
        LEGACY_CANONICAL_STATE_SCHEMA_VERSION,
    };
    use crate::operation_log::timestamp_millis;
    use crate::session_storage::{
        metrics::record_reclaimed_bytes,
        migration::{persist_migration_preflight, run_migration_preflight},
        operation_ledger::{
            OperationLedgerStore, SessionStorageOperationKind, SessionStorageOperationPhase,
        },
    };

    fn persist_matching_preflight(
        root: &Path,
        data: &Path,
        home: &Path,
        operation_id: &str,
    ) -> (String, std::path::PathBuf) {
        fs::write(
            home.join("config.toml"),
            format!("sqlite_home = {:?}\n", home.to_string_lossy()),
        )
        .unwrap();
        Connection::open(home.join("state_5.sqlite"))
            .unwrap()
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS threads (\
                    id TEXT PRIMARY KEY, \
                    rollout_path TEXT, \
                    model_provider TEXT\
                );",
            )
            .unwrap();
        let backup_destination = root.join(format!("backup-{operation_id}"));
        fs::create_dir_all(&backup_destination).unwrap();
        let report =
            run_migration_preflight(home, data, operation_id, &backup_destination).unwrap();
        assert!(report.ready_for_backup, "{:?}", report.blockers);
        persist_migration_preflight(data, &report).unwrap();
        (report.plan.inventory_fingerprint, report.backup_destination)
    }

    fn bind_backup_and_advance_to_validating(
        store: &OperationLedgerStore,
        operation_id: &str,
        backup_destination: &Path,
    ) {
        store
            .update(operation_id, |ledger| {
                ledger.backup_root = Some(backup_destination.join(operation_id));
                Ok(())
            })
            .unwrap();
        for phase in [
            SessionStorageOperationPhase::Preflight,
            SessionStorageOperationPhase::Backup,
            SessionStorageOperationPhase::BackupVerified,
            SessionStorageOperationPhase::PlanReady,
            SessionStorageOperationPhase::Applying,
            SessionStorageOperationPhase::Validating,
        ] {
            store.transition(operation_id, phase).unwrap();
        }
    }

    fn commit(store: &OperationLedgerStore, operation_id: &str) {
        store
            .transition(operation_id, SessionStorageOperationPhase::Committed)
            .unwrap();
    }

    #[test]
    fn canonical_v2_survives_ledger_and_preflight_retention() {
        let root = tempdir().unwrap();
        let data = root.path().join("data");
        let home = root.path().join("home");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&home).unwrap();
        let operation_id = "migration-state-v2";
        let store = OperationLedgerStore::new(&data);
        store
            .create(operation_id, SessionStorageOperationKind::Migration, &home)
            .unwrap();
        let (fingerprint, backup_destination) =
            persist_matching_preflight(root.path(), &data, &home, operation_id);
        bind_backup_and_advance_to_validating(&store, operation_id, &backup_destination);
        prepare_canonical_storage_state(&data, &home, operation_id, &fingerprint).unwrap();
        commit(&store, operation_id);

        let state = finalize_canonical_storage_state(&data, &home, operation_id).unwrap();
        assert_eq!(state.schema_version, 2);
        assert!(state.committed_at_ms >= state.prepared_at_ms);
        assert_eq!(state.backup_destination, backup_destination);
        #[cfg(windows)]
        {
            let persisted = fs::read(canonical_state_path(&data)).unwrap();
            assert!(persisted.starts_with(CANONICAL_STATE_CIPHERTEXT_MAGIC));
            assert!(!persisted
                .windows(home.to_string_lossy().len())
                .any(|window| window == home.to_string_lossy().as_bytes()));
        }
        fs::remove_file(
            data.join("session-storage-v1/operations")
                .join(operation_id)
                .join("preflight.json"),
        )
        .unwrap();
        store.remove_terminal_operation(operation_id).unwrap();

        assert_eq!(
            load_committed_canonical_storage_state(&data, &home)
                .unwrap()
                .unwrap(),
            state
        );
    }

    #[test]
    fn canonical_v2_tamper_fails_closed() {
        let root = tempdir().unwrap();
        let data = root.path().join("data");
        let home = root.path().join("home");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&home).unwrap();
        let operation_id = "migration-state-tamper";
        let store = OperationLedgerStore::new(&data);
        store
            .create(operation_id, SessionStorageOperationKind::Migration, &home)
            .unwrap();
        let (fingerprint, backup_destination) =
            persist_matching_preflight(root.path(), &data, &home, operation_id);
        bind_backup_and_advance_to_validating(&store, operation_id, &backup_destination);
        prepare_canonical_storage_state(&data, &home, operation_id, &fingerprint).unwrap();
        commit(&store, operation_id);
        finalize_canonical_storage_state(&data, &home, operation_id).unwrap();

        let path = canonical_state_path(&data);
        let mut bytes = fs::read(&path).unwrap();
        let last = bytes.last_mut().unwrap();
        *last ^= 0x01;
        fs::write(path, bytes).unwrap();
        assert!(load_committed_canonical_storage_state(&data, &home).is_err());
    }

    #[test]
    fn legacy_v1_upgrades_only_with_committed_ledger_and_preflight_proof() {
        let root = tempdir().unwrap();
        let data = root.path().join("data");
        let home = root.path().join("home");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&home).unwrap();
        let operation_id = "migration-state-v1-upgrade";
        let store = OperationLedgerStore::new(&data);
        store
            .create(operation_id, SessionStorageOperationKind::Migration, &home)
            .unwrap();
        let (fingerprint, backup_destination) =
            persist_matching_preflight(root.path(), &data, &home, operation_id);
        let legacy = LegacyCanonicalStorageStateV1 {
            schema_version: LEGACY_CANONICAL_STATE_SCHEMA_VERSION,
            migration_operation_id: operation_id.to_string(),
            canonical_root: home.clone(),
            inventory_fingerprint: fingerprint,
            prepared_at_ms: timestamp_millis().unwrap(),
        };
        let envelope = LegacyCanonicalStorageStateEnvelopeV1 {
            integrity_sha256: digest(&legacy).unwrap(),
            state: legacy,
        };
        fs::create_dir_all(canonical_state_path(&data).parent().unwrap()).unwrap();
        fs::write(
            canonical_state_path(&data),
            serde_json::to_vec_pretty(&envelope).unwrap(),
        )
        .unwrap();
        bind_backup_and_advance_to_validating(&store, operation_id, &backup_destination);
        commit(&store, operation_id);

        let upgraded = load_committed_canonical_storage_state(&data, &home)
            .unwrap()
            .unwrap();
        assert_eq!(upgraded.schema_version, 2);
        assert_eq!(upgraded.migration_operation_id, operation_id);
        #[cfg(windows)]
        assert!(fs::read(canonical_state_path(&data))
            .unwrap()
            .starts_with(CANONICAL_STATE_CIPHERTEXT_MAGIC));
    }

    #[test]
    fn legacy_v1_without_retained_proof_fails_closed() {
        let root = tempdir().unwrap();
        let data = root.path().join("data");
        let home = root.path().join("home");
        fs::create_dir_all(canonical_state_path(&data).parent().unwrap()).unwrap();
        fs::create_dir_all(&home).unwrap();
        let legacy = LegacyCanonicalStorageStateV1 {
            schema_version: LEGACY_CANONICAL_STATE_SCHEMA_VERSION,
            migration_operation_id: "migration-state-v1-no-proof".to_string(),
            canonical_root: home.clone(),
            inventory_fingerprint: "a".repeat(64),
            prepared_at_ms: timestamp_millis().unwrap(),
        };
        let envelope = LegacyCanonicalStorageStateEnvelopeV1 {
            integrity_sha256: digest(&legacy).unwrap(),
            state: legacy,
        };
        fs::write(
            canonical_state_path(&data),
            serde_json::to_vec_pretty(&envelope).unwrap(),
        )
        .unwrap();

        assert!(load_committed_canonical_storage_state(&data, &home).is_err());
    }

    #[test]
    fn prepared_state_is_not_committed() {
        let root = tempdir().unwrap();
        let data = root.path().join("data");
        let home = root.path().join("home");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&home).unwrap();
        let operation_id = "migration-state-prepared";
        let store = OperationLedgerStore::new(&data);
        store
            .create(operation_id, SessionStorageOperationKind::Migration, &home)
            .unwrap();
        let (fingerprint, backup_destination) =
            persist_matching_preflight(root.path(), &data, &home, operation_id);
        bind_backup_and_advance_to_validating(&store, operation_id, &backup_destination);
        prepare_canonical_storage_state(&data, &home, operation_id, &fingerprint).unwrap();
        let first_bytes = fs::read(canonical_state_path(&data)).unwrap();
        prepare_canonical_storage_state(&data, &home, operation_id, &fingerprint).unwrap();
        assert_eq!(fs::read(canonical_state_path(&data)).unwrap(), first_bytes);

        assert!(load_committed_canonical_storage_state(&data, &home)
            .unwrap()
            .is_none());
    }

    #[test]
    fn gc_discovery_paths_are_sorted_and_deduplicated() {
        let root = tempdir().unwrap();
        let a = root.path().join("a.jsonl");
        let b = root.path().join("b.jsonl");
        let paths = normalize_gc_discovery_paths(vec![b.clone(), a.clone(), b]).unwrap();
        assert_eq!(paths, vec![a, root.path().join("b.jsonl")]);
    }

    #[test]
    fn automatic_cleanup_defaults_on_and_persists_explicit_off() {
        let root = tempdir().unwrap();
        let data = root.path().join("data");
        let home = root.path().join("home");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&home).unwrap();
        assert!(
            load_session_storage_settings(&data)
                .unwrap()
                .automatic_cleanup_enabled
        );
        set_automatic_cleanup_enabled(&data, false).unwrap();
        assert!(
            !load_session_storage_control_state(&data, &home)
                .unwrap()
                .automatic_cleanup_enabled
        );
        record_reclaimed_bytes(&data, "offline-gc-storage-state", 4096).unwrap();
        assert_eq!(
            load_session_storage_control_state(&data, &home)
                .unwrap()
                .reclaimed_bytes,
            4096
        );
    }
}

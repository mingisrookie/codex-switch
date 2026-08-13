use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
    str::FromStr,
    thread,
    time::Duration,
};

use rusqlite::{Connection, OpenFlags, MAIN_DB};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use toml_edit::DocumentMut;
use walkdir::WalkDir;

use crate::{
    codex_paths::{local_codex_paths, resolve_user_codex_paths},
    file_ops::{atomic_copy, atomic_write},
    mobile_continuity,
    operation_log::timestamp_millis,
    runtime_session_view::{
        inspect_legacy_session_view_database_homes, inspect_session_view_database_homes,
    },
    runtime_store::{RuntimeStore, PLUS_RUNTIME_ID, RELAY_RUNTIME_ID},
    session_incremental::save_session_sync_index,
};

use super::{
    bounded_file::read_regular_file_bounded,
    catalog::{discover_database_catalog, goals_database_digest},
    migration::{available_backup_bytes, collect_inventory, MigrationInventory},
    migration_apply::{
        merge_goals_database_views, stable_file_digest, MigrationDatabaseApplyEntry,
    },
    migration_backup::{
        cleanup_isolated_root, MigrationBackupEntry, MigrationBackupEntryKind,
        MigrationBackupManifest, MigrationBackupRuntimeVerifier, MigrationBackupStatus,
        MigrationRuntimeVerification,
    },
    model::{DatabaseInput, DatabaseRole, FileOrigin, SessionRelation},
    operation_ledger::{
        OperationLedgerStore, SessionStorageOperationKind, SessionStorageOperationPhase,
    },
    reference_graph::path_key,
    semantic::read_semantic_session,
    storage_state::{load_committed_canonical_storage_state, CanonicalStorageState},
};

const DOWNGRADE_SCHEMA_VERSION: u32 = 1;
const MAX_PLAN_BYTES: u64 = 64 * 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;
const OBSERVATION_DELAY: Duration = Duration::from_millis(250);
const CAPACITY_RESERVE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const CAPACITY_RESERVE_PERCENT: u64 = 15;
const PACKAGE_MARKER_NAME: &str = ".codex-switch-downgrade-v1";
const MANIFEST_NAME: &str = "manifest.json";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DowngradeCompatibilityBand {
    A,
    B,
    C,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DowngradeTargetContract {
    pub version: String,
    pub band: DowngradeCompatibilityBand,
    pub runtime_bundle_required: bool,
    pub incremental_index_required: bool,
    pub relay_session_view_supported: bool,
    pub mobile_continuity_required: bool,
}

pub fn downgrade_target_contract(raw: &str) -> Result<DowngradeTargetContract, String> {
    let normalized = raw.trim().strip_prefix('v').unwrap_or(raw.trim());
    let version = Version::parse(normalized)
        .map_err(|_| "downgrade target version is invalid".to_string())?;
    if version.major != 0
        || version.minor != 2
        || version.patch > 7
        || !version.pre.is_empty()
        || !version.build.is_empty()
    {
        return Err(
            "downgrade target must be an exact supported v0.2.0-v0.2.7 version".to_string(),
        );
    }
    let band = match version.patch {
        0 | 1 => DowngradeCompatibilityBand::A,
        2 | 3 => DowngradeCompatibilityBand::B,
        4..=7 => DowngradeCompatibilityBand::C,
        _ => unreachable!("supported patch range was checked"),
    };
    Ok(DowngradeTargetContract {
        version: format!("v{version}"),
        band,
        runtime_bundle_required: version.patch >= 1,
        incremental_index_required: version.patch >= 3,
        relay_session_view_supported: version.patch >= 4,
        mobile_continuity_required: version.patch >= 4,
    })
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub enum DowngradePackageFileKind {
    ActiveSession,
    ArchivedSession,
    ConflictBranch,
    RecoveryPayload,
    StateDatabase,
    AuxiliaryDatabase,
    SessionIndex,
    RuntimeBundle,
    SharedView,
    Bootstrap,
    Credential,
    Config,
    Launcher,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DowngradePackageEntry {
    pub relative_path: PathBuf,
    pub kind: DowngradePackageFileKind,
    pub bytes: u64,
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logical_thread_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DowngradeExportStatus {
    StructurallyVerified,
    TargetRuntimeVerified,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DowngradeRuntimeVerification {
    pub target_version: String,
    pub listed_session_count: usize,
    pub resumed_session_count: usize,
    pub continued_session_count: usize,
    pub verified_at_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DowngradeExportManifest {
    pub schema_version: u32,
    pub operation_id: String,
    pub created_at_ms: u128,
    pub created_with_version: String,
    pub target: DowngradeTargetContract,
    pub status: DowngradeExportStatus,
    pub source_inventory_fingerprint: String,
    pub codex_home_relative_path: PathBuf,
    pub appdata_relative_path: PathBuf,
    pub contains_credentials: bool,
    pub initial_runtime_slot_count: usize,
    pub logical_session_count: usize,
    pub session_file_count: usize,
    pub conflict_branch_count: usize,
    pub recovery_payload_count: usize,
    pub package_bytes: u64,
    pub entries: Vec<DowngradePackageEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_runtime_verification: Option<MigrationRuntimeVerification>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_verification: Option<DowngradeRuntimeVerification>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DowngradeManifestEnvelope {
    manifest: DowngradeExportManifest,
    integrity_sha256: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DowngradeExportReceipt {
    pub operation_id: String,
    pub target: DowngradeTargetContract,
    pub package_dir: PathBuf,
    pub logical_session_count: usize,
    pub session_file_count: usize,
    pub conflict_branch_count: usize,
    pub recovery_payload_count: usize,
    pub package_bytes: u64,
    pub contains_credentials: bool,
    pub structurally_verified: bool,
    pub native_runtime_verified: bool,
    pub target_runtime_verification_required: bool,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DowngradeRecoveryStatus {
    Committed,
    RolledBack,
    ResidualPreserved,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DowngradeRecoveryReceipt {
    pub operation_id: String,
    pub status: DowngradeRecoveryStatus,
    pub package_preserved: bool,
    pub staging_removed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DowngradeSessionExportPlan {
    pub source_path: PathBuf,
    pub output_relative_path: PathBuf,
    pub kind: DowngradePackageFileKind,
    pub bytes: u64,
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logical_thread_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DowngradeExportPlan {
    pub schema_version: u32,
    pub operation_id: String,
    pub generated_at_ms: u128,
    pub target: DowngradeTargetContract,
    pub canonical_root: PathBuf,
    pub data_root: PathBuf,
    pub account_sqlite_home: PathBuf,
    pub account_database_id: String,
    pub account_catalog_fingerprint: String,
    pub canonical_migration_operation_id: String,
    pub canonical_inventory_fingerprint: String,
    pub destination_root: PathBuf,
    pub staging_dir: PathBuf,
    pub package_dir: PathBuf,
    pub source_inventory_fingerprint: String,
    pub projected_bytes: u64,
    pub required_available_bytes: u64,
    pub available_bytes: u64,
    pub sessions: Vec<DowngradeSessionExportPlan>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DowngradePlanEnvelope {
    plan: DowngradeExportPlan,
    integrity_sha256: String,
}

pub fn prepare_downgrade_export(
    codex_home: &Path,
    data_root: &Path,
    destination_root: &Path,
    operation_id: &str,
    target_version: &str,
) -> Result<DowngradeExportPlan, String> {
    let canonical_state = load_committed_canonical_storage_state(data_root, codex_home)?
        .ok_or_else(|| {
            "downgrade export requires a committed canonical storage migration".to_string()
        })?;
    prepare_downgrade_export_with_state(
        codex_home,
        data_root,
        destination_root,
        operation_id,
        target_version,
        &canonical_state,
    )
}

fn prepare_downgrade_export_with_state(
    codex_home: &Path,
    data_root: &Path,
    destination_root: &Path,
    operation_id: &str,
    target_version: &str,
    canonical_state: &CanonicalStorageState,
) -> Result<DowngradeExportPlan, String> {
    validate_operation_id(operation_id)?;
    validate_absolute_directory(codex_home, "canonical root")?;
    validate_absolute_directory(data_root, "data root")?;
    validate_absolute_directory(destination_root, "downgrade destination")?;
    validate_canonical_state_binding(canonical_state, codex_home)?;
    let target = downgrade_target_contract(target_version)?;
    let safe_version = target.version.trim_start_matches('v').replace('.', "-");
    let package_dir = destination_root.join(format!(
        "codex-switch-downgrade-{safe_version}-{operation_id}"
    ));
    let staging_dir = destination_root.join(format!(
        ".codex-switch-downgrade-{safe_version}-{operation_id}.staging"
    ));
    if package_dir.exists() || staging_dir.exists() {
        return Err("downgrade export destination already contains this operation".to_string());
    }

    let first = collect_inventory(codex_home, data_root)?;
    thread::sleep(OBSERVATION_DELAY);
    let second = collect_inventory(codex_home, data_root)?;
    if first.fingerprint != second.fingerprint {
        return Err("downgrade source inventory changed during preflight".to_string());
    }
    if second.database_discovery_errors > 0
        || second.catalog.database_errors > 0
        || second.goals_catalog.errors > 0
        || second.session_discovery_errors > 0
    {
        return Err("downgrade source inventory is incomplete".to_string());
    }

    let selected = select_account_database(codex_home, data_root, canonical_state, &second)?;
    validate_non_overlapping_roots(
        &[codex_home, data_root, &selected.sqlite_home],
        destination_root,
    )?;
    validate_inventory_session_source_overlap(destination_root, &second)?;
    let account_state_db = selected.sqlite_home.join("state_5.sqlite");
    let sessions = plan_session_exports(codex_home, &second, &selected.database_id)?;
    validate_session_source_overlap(destination_root, &sessions)?;
    validate_account_database_references(&account_state_db, &sessions)?;

    let projected_bytes =
        projected_export_bytes(codex_home, data_root, &selected.sqlite_home, &sessions)?;
    let reserve = CAPACITY_RESERVE_BYTES.max(
        projected_bytes
            .checked_mul(CAPACITY_RESERVE_PERCENT)
            .ok_or_else(|| "downgrade export capacity calculation overflowed".to_string())?
            / 100,
    );
    let required_available_bytes = projected_bytes
        .checked_add(reserve)
        .ok_or_else(|| "downgrade export capacity calculation overflowed".to_string())?;
    let available_bytes = available_backup_bytes(destination_root)?;
    if available_bytes < required_available_bytes {
        return Err("downgrade export destination has insufficient free space".to_string());
    }

    let plan = DowngradeExportPlan {
        schema_version: DOWNGRADE_SCHEMA_VERSION,
        operation_id: operation_id.to_string(),
        generated_at_ms: timestamp_millis()?,
        target,
        canonical_root: codex_home.to_path_buf(),
        data_root: data_root.to_path_buf(),
        account_sqlite_home: selected.sqlite_home,
        account_database_id: selected.database_id,
        account_catalog_fingerprint: selected.catalog_fingerprint,
        canonical_migration_operation_id: canonical_state.migration_operation_id.clone(),
        canonical_inventory_fingerprint: canonical_state.inventory_fingerprint.clone(),
        destination_root: destination_root.to_path_buf(),
        staging_dir,
        package_dir,
        source_inventory_fingerprint: second.fingerprint,
        projected_bytes,
        required_available_bytes,
        available_bytes,
        sessions,
    };
    validate_plan(&plan)?;
    persist_downgrade_plan(data_root, &plan)?;
    load_downgrade_plan(data_root, operation_id)
}

pub fn persist_downgrade_plan(data_root: &Path, plan: &DowngradeExportPlan) -> Result<(), String> {
    validate_plan(plan)?;
    if plan.data_root != data_root {
        return Err("downgrade export plan data root changed".to_string());
    }
    let envelope = DowngradePlanEnvelope {
        plan: plan.clone(),
        integrity_sha256: plan_digest(plan)?,
    };
    let bytes = serde_json::to_vec_pretty(&envelope)
        .map_err(|_| "failed to serialize downgrade export plan".to_string())?;
    if bytes.len() as u64 > MAX_PLAN_BYTES {
        return Err("downgrade export plan reached its size limit".to_string());
    }
    atomic_write(&downgrade_plan_path(data_root, &plan.operation_id)?, &bytes)
}

pub fn load_downgrade_plan(
    data_root: &Path,
    operation_id: &str,
) -> Result<DowngradeExportPlan, String> {
    let path = downgrade_plan_path(data_root, operation_id)?;
    let bytes = read_regular_file_bounded(&path, MAX_PLAN_BYTES)
        .map_err(|_| "downgrade export plan is unreadable".to_string())?;
    let envelope = serde_json::from_slice::<DowngradePlanEnvelope>(&bytes)
        .map_err(|_| "downgrade export plan is invalid".to_string())?;
    validate_plan(&envelope.plan)?;
    if envelope.plan.operation_id != operation_id
        || envelope.plan.data_root != data_root
        || envelope.integrity_sha256 != plan_digest(&envelope.plan)?
    {
        return Err("downgrade export plan integrity check failed".to_string());
    }
    Ok(envelope.plan)
}

pub fn execute_downgrade_export<Guard>(
    plan: &DowngradeExportPlan,
    mut writer_guard: Guard,
) -> Result<DowngradeExportReceipt, String>
where
    Guard: FnMut() -> Result<(), String>,
{
    validate_plan(plan)?;
    validate_absolute_directory(&plan.destination_root, "downgrade destination")?;
    validate_non_overlapping_roots(
        &[
            &plan.canonical_root,
            &plan.data_root,
            &plan.account_sqlite_home,
        ],
        &plan.destination_root,
    )?;
    validate_session_source_overlap(&plan.destination_root, &plan.sessions)?;
    if plan.package_dir.exists() || plan.staging_dir.exists() {
        return Err("an interrupted downgrade export must be recovered first".to_string());
    }
    writer_guard()?;
    let inventory = collect_inventory(&plan.canonical_root, &plan.data_root)?;
    validate_inventory_session_source_overlap(&plan.destination_root, &inventory)?;
    if inventory.fingerprint != plan.source_inventory_fingerprint {
        return Err("downgrade source inventory changed after planning".to_string());
    }
    validate_live_plan_identity(plan, &inventory)?;

    create_safe_directory(&plan.staging_dir)?;
    let stage_result: Result<usize, String> = (|| {
        write_package_marker(&plan.staging_dir, plan)?;
        let staging_codex_home = plan.staging_dir.join("codex-home");
        let staging_data_root = plan.staging_dir.join("appdata/codex-switch");
        create_safe_directory(&staging_codex_home)?;
        create_safe_directory(&staging_data_root)?;
        create_safe_directory(&plan.staging_dir.join("localappdata"))?;
        create_safe_directory(&plan.staging_dir.join("workspace"))?;

        copy_session_payloads(plan)?;
        copy_live_profile(plan, &staging_codex_home)?;
        snapshot_profile_databases(plan, &staging_codex_home)?;
        rewrite_account_rollout_paths(
            &staging_codex_home.join("state_5.sqlite"),
            plan,
            &plan.staging_dir,
        )?;

        let current = local_codex_paths(&staging_codex_home);
        let shared = local_codex_paths(&staging_data_root.join("shared-sessions"));
        create_shared_session_view(&current, &shared)?;

        rewrite_database_root(
            &current.state_db,
            &staging_codex_home,
            &plan.package_dir.join("codex-home"),
        )?;
        rewrite_database_root(
            &shared.state_db,
            &staging_data_root.join("shared-sessions"),
            &plan
                .package_dir
                .join("appdata/codex-switch/shared-sessions"),
        )?;
        let runtime_slot_count = copy_runtime_slots(plan, &staging_codex_home, &staging_data_root)?;
        write_launchers_and_readme(&plan.staging_dir, plan)?;
        Ok(runtime_slot_count)
    })();
    let runtime_slot_count = stage_result?;

    writer_guard()?;
    let final_inventory = collect_inventory(&plan.canonical_root, &plan.data_root)?;
    if final_inventory.fingerprint != plan.source_inventory_fingerprint {
        return Err("downgrade source inventory changed during export".to_string());
    }
    validate_live_plan_identity(plan, &final_inventory)?;
    writer_guard()?;
    fs::rename(&plan.staging_dir, &plan.package_dir)
        .map_err(|_| "failed to publish downgrade package".to_string())?;

    let final_codex_home = plan.package_dir.join("codex-home");
    let final_data_root = plan.package_dir.join("appdata/codex-switch");
    if plan.target.incremental_index_required {
        save_session_sync_index(
            &final_data_root.join("session-sync-state-v1.json"),
            &local_codex_paths(&final_codex_home),
            &local_codex_paths(&final_data_root.join("shared-sessions")),
        )
        .map_err(|_| "failed to initialize the v0.2 incremental session index".to_string())?;
    }
    if plan.target.mobile_continuity_required {
        mobile_continuity::initialize_status(
            &final_data_root.join("mobile-continuity-v1.json"),
            &local_codex_paths(&final_codex_home),
        )
        .map_err(|_| "failed to initialize the v0.2 mobile continuity state".to_string())?;
    }
    let manifest = build_downgrade_manifest(plan, runtime_slot_count)?;
    write_downgrade_manifest(&plan.package_dir, &manifest)?;
    let verified = verify_downgrade_package(&plan.package_dir)?;
    if verified != manifest {
        return Err("downgrade package verification identity changed".to_string());
    }
    Ok(receipt_from_manifest(&plan.package_dir, &verified))
}

pub fn verify_downgrade_package(package_dir: &Path) -> Result<DowngradeExportManifest, String> {
    validate_absolute_directory(package_dir, "downgrade package")?;
    let envelope = read_downgrade_manifest(package_dir)?;
    let manifest = envelope.manifest;
    validate_manifest(&manifest)?;
    if envelope.integrity_sha256 != manifest_digest(&manifest)? {
        return Err("downgrade package manifest integrity check failed".to_string());
    }

    let mut expected_paths = BTreeSet::new();
    let mut package_bytes = 0_u64;
    let mut session_files = 0_usize;
    let mut conflict_branches = 0_usize;
    let mut recovery_payloads = 0_usize;
    let mut logical_threads = BTreeSet::new();
    for entry in &manifest.entries {
        validate_relative_path(&entry.relative_path)?;
        if !expected_paths.insert(relative_path_key(&entry.relative_path)) {
            return Err("downgrade package manifest has duplicate entries".to_string());
        }
        let path = package_dir.join(&entry.relative_path);
        let (bytes, sha256) = stable_file_digest(&path)
            .map_err(|_| "downgrade package payload is unavailable".to_string())?;
        if bytes != entry.bytes || sha256 != entry.sha256 {
            return Err("downgrade package payload integrity check failed".to_string());
        }
        package_bytes = package_bytes
            .checked_add(bytes)
            .ok_or_else(|| "downgrade package byte count overflowed".to_string())?;
        if matches!(
            entry.kind,
            DowngradePackageFileKind::ActiveSession
                | DowngradePackageFileKind::ArchivedSession
                | DowngradePackageFileKind::ConflictBranch
        ) {
            let semantic = read_semantic_session(&path)
                .map_err(|_| "downgrade package session payload is invalid".to_string())?;
            if entry.logical_thread_id.as_deref() != Some(semantic.thread_id.as_str()) {
                return Err("downgrade package session identity changed".to_string());
            }
            logical_threads.insert(semantic.thread_id);
            session_files = session_files.saturating_add(1);
        }
        if entry.kind == DowngradePackageFileKind::ConflictBranch {
            conflict_branches = conflict_branches.saturating_add(1);
        }
        if entry.kind == DowngradePackageFileKind::RecoveryPayload {
            recovery_payloads = recovery_payloads.saturating_add(1);
        }
        if matches!(
            entry.kind,
            DowngradePackageFileKind::StateDatabase
                | DowngradePackageFileKind::AuxiliaryDatabase
                | DowngradePackageFileKind::SharedView
        ) && path
            .extension()
            .is_some_and(|extension| extension == "sqlite")
        {
            quick_check_sqlite(&path)?;
        }
        if entry.relative_path == Path::new("codex-home/goals_1.sqlite") {
            if entry.kind != DowngradePackageFileKind::AuxiliaryDatabase
                || entry.logical_thread_id.is_some()
            {
                return Err("downgrade goals database manifest entry is invalid".to_string());
            }
            let connection = Connection::open_with_flags(
                &path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .map_err(|_| "failed to open exported downgrade goals database".to_string())?;
            goals_database_digest(&connection)?;
        }
    }
    let actual_paths = package_regular_files(package_dir)?;
    let manifest_key = relative_path_key(Path::new(MANIFEST_NAME));
    if actual_paths
        .iter()
        .filter(|path| **path != manifest_key)
        .cloned()
        .collect::<BTreeSet<_>>()
        != expected_paths
    {
        return Err("downgrade package contains an untracked payload".to_string());
    }
    if !actual_paths.contains(&manifest_key)
        || package_bytes != manifest.package_bytes
        || session_files != manifest.session_file_count
        || conflict_branches != manifest.conflict_branch_count
        || recovery_payloads != manifest.recovery_payload_count
        || logical_threads.len() != manifest.logical_session_count
    {
        return Err("downgrade package manifest counts are invalid".to_string());
    }
    verify_package_bootstrap(package_dir, &manifest)?;
    verify_exported_state_references(package_dir, &manifest)?;
    Ok(manifest)
}

pub fn verify_downgrade_package_with_runtime<V: MigrationBackupRuntimeVerifier>(
    package_dir: &Path,
    isolated_root: &Path,
    verifier: &V,
) -> Result<DowngradeExportManifest, String> {
    let mut manifest = verify_downgrade_package(package_dir)?;
    if manifest.native_runtime_verification.is_some() {
        return Ok(manifest);
    }
    validate_new_isolated_runtime_root(package_dir, isolated_root)?;
    create_safe_directory(isolated_root)?;
    let verification_result = (|| {
        let runtime_manifest =
            prepare_downgrade_runtime_manifest(package_dir, isolated_root, &manifest)?;
        verifier.verify(isolated_root, &runtime_manifest)
    })();
    let cleanup_result = cleanup_isolated_root(isolated_root)
        .map_err(|_| "failed to clean the isolated downgrade runtime root".to_string());
    let runtime = match (verification_result, cleanup_result) {
        (Ok(runtime), Ok(())) => runtime,
        (Err(error), Ok(())) => return Err(error),
        (Ok(_), Err(error)) => return Err(error),
        (Err(runtime_error), Err(cleanup_error)) => {
            return Err(format!("{runtime_error}; {cleanup_error}"))
        }
    };
    let unchanged = verify_downgrade_package(package_dir)?;
    if unchanged != manifest {
        return Err("downgrade package changed during native runtime verification".to_string());
    }
    if !native_runtime_verification_complete(&runtime, manifest.logical_session_count)
        || runtime.verified_at_ms < manifest.created_at_ms
    {
        return Err("native Codex did not verify the complete downgrade package".to_string());
    }
    manifest.native_runtime_verification = Some(runtime);
    write_downgrade_manifest(package_dir, &manifest)?;
    verify_downgrade_package(package_dir)
}

fn validate_new_isolated_runtime_root(
    package_dir: &Path,
    isolated_root: &Path,
) -> Result<(), String> {
    if !isolated_root.is_absolute()
        || isolated_root
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
        || isolated_root.exists()
        || isolated_root.starts_with(package_dir)
        || package_dir.starts_with(isolated_root)
    {
        return Err("isolated downgrade runtime root is invalid".to_string());
    }
    let parent = isolated_root
        .parent()
        .ok_or_else(|| "isolated downgrade runtime root has no parent".to_string())?;
    validate_absolute_directory(parent, "isolated downgrade runtime parent")
}

fn prepare_downgrade_runtime_manifest(
    package_dir: &Path,
    isolated_root: &Path,
    manifest: &DowngradeExportManifest,
) -> Result<MigrationBackupManifest, String> {
    let mut entries = Vec::new();
    let mut payload_paths = BTreeSet::new();
    let mut state_database_count = 0_usize;
    for (index, entry) in manifest.entries.iter().enumerate() {
        let (payload_relative_path, kind) = match entry.kind {
            DowngradePackageFileKind::StateDatabase => {
                state_database_count = state_database_count.saturating_add(1);
                (
                    PathBuf::from("canonical-state_5.sqlite"),
                    MigrationBackupEntryKind::Database,
                )
            }
            DowngradePackageFileKind::AuxiliaryDatabase => {
                let name = entry
                    .relative_path
                    .file_name()
                    .ok_or_else(|| "downgrade auxiliary database has no file name".to_string())?;
                (
                    PathBuf::from(format!("canonical-{}", name.to_string_lossy())),
                    MigrationBackupEntryKind::Database,
                )
            }
            DowngradePackageFileKind::ActiveSession
            | DowngradePackageFileKind::ArchivedSession
            | DowngradePackageFileKind::ConflictBranch => (
                PathBuf::from("canonical/sessions").join(format!("{index:06}.jsonl")),
                MigrationBackupEntryKind::Session,
            ),
            _ => continue,
        };
        if !payload_paths.insert(relative_path_key(&payload_relative_path)) {
            return Err("downgrade runtime verification payload collided".to_string());
        }
        let source = package_dir.join(&entry.relative_path);
        let target = isolated_root.join(&payload_relative_path);
        let parent = target
            .parent()
            .ok_or_else(|| "downgrade runtime payload has no parent".to_string())?;
        create_safe_directory(parent)?;
        atomic_copy(&source, &target)?;
        let (bytes, sha256) = stable_file_digest(&target)?;
        if bytes != entry.bytes || sha256 != entry.sha256 {
            return Err("downgrade runtime verification payload changed".to_string());
        }
        entries.push(MigrationBackupEntry {
            source_path: source,
            payload_relative_path,
            kind,
            bytes,
            sha256,
            logical_thread_id: entry.logical_thread_id.clone(),
        });
    }
    if state_database_count != 1 || entries.is_empty() {
        return Err("downgrade runtime verification fixture is incomplete".to_string());
    }
    let verified_at_ms = timestamp_millis()?;
    Ok(MigrationBackupManifest {
        schema_version: 1,
        operation_id: manifest.operation_id.clone(),
        created_at_ms: manifest.created_at_ms,
        expires_at_ms: verified_at_ms,
        backup_dir: isolated_root.to_path_buf(),
        status: MigrationBackupStatus::IsolatedRestoreVerified,
        entries,
        isolated_restore_verified_at_ms: Some(verified_at_ms),
        runtime_verification: None,
    })
}

pub(crate) fn load_downgrade_manifest_baseline(
    package_dir: &Path,
) -> Result<DowngradeExportManifest, String> {
    validate_absolute_directory(package_dir, "downgrade package")?;
    let envelope = read_downgrade_manifest(package_dir)?;
    if manifest_digest(&envelope.manifest)? != envelope.integrity_sha256 {
        return Err("downgrade package manifest integrity check failed".to_string());
    }
    validate_manifest(&envelope.manifest)?;
    validate_manifest_marker(package_dir, &envelope.manifest)?;
    Ok(envelope.manifest)
}

pub fn recover_interrupted_downgrade_export<Guard>(
    store: &OperationLedgerStore,
    data_root: &Path,
    operation_id: &str,
    mut writer_guard: Guard,
) -> Result<DowngradeRecoveryReceipt, String>
where
    Guard: FnMut() -> Result<(), String>,
{
    let ledger = store.load(operation_id)?;
    if ledger.kind != SessionStorageOperationKind::DowngradeExport
        || ledger.canonical_root.as_os_str().is_empty()
    {
        return Err("downgrade recovery ledger identity is invalid".to_string());
    }
    let plan = match load_downgrade_plan(data_root, operation_id) {
        Ok(plan) => plan,
        Err(_)
            if matches!(
                ledger.phase,
                SessionStorageOperationPhase::Available | SessionStorageOperationPhase::Preflight
            ) =>
        {
            if ledger.phase == SessionStorageOperationPhase::Available {
                store.transition(operation_id, SessionStorageOperationPhase::Preflight)?;
            }
            store.transition(operation_id, SessionStorageOperationPhase::RollingBack)?;
            store.transition(operation_id, SessionStorageOperationPhase::RolledBack)?;
            return Ok(DowngradeRecoveryReceipt {
                operation_id: operation_id.to_string(),
                status: DowngradeRecoveryStatus::RolledBack,
                package_preserved: false,
                staging_removed: false,
            });
        }
        Err(error) => return Err(error),
    };
    if plan.canonical_root != ledger.canonical_root {
        return Err("downgrade recovery plan identity changed".to_string());
    }
    let runtime_root = data_root
        .join("session-storage-v1/operations")
        .join(operation_id)
        .join("downgrade-runtime-verify");
    cleanup_isolated_root(&runtime_root)
        .map_err(|_| "failed to recover the isolated downgrade runtime root".to_string())?;
    if ledger.phase == SessionStorageOperationPhase::Committed {
        let manifest = verify_downgrade_package(&plan.package_dir)?;
        if manifest.operation_id != operation_id || manifest.native_runtime_verification.is_none() {
            return Err("committed downgrade package identity changed".to_string());
        }
        return Ok(DowngradeRecoveryReceipt {
            operation_id: operation_id.to_string(),
            status: DowngradeRecoveryStatus::Committed,
            package_preserved: true,
            staging_removed: false,
        });
    }
    if matches!(
        ledger.phase,
        SessionStorageOperationPhase::RolledBack | SessionStorageOperationPhase::Failed
    ) {
        return Ok(DowngradeRecoveryReceipt {
            operation_id: operation_id.to_string(),
            status: DowngradeRecoveryStatus::RolledBack,
            package_preserved: plan.package_dir.exists(),
            staging_removed: !plan.staging_dir.exists(),
        });
    }

    writer_guard()?;
    if plan.package_dir.exists() {
        if plan.staging_dir.exists() {
            return Ok(DowngradeRecoveryReceipt {
                operation_id: operation_id.to_string(),
                status: DowngradeRecoveryStatus::ResidualPreserved,
                package_preserved: true,
                staging_removed: false,
            });
        }
        validate_package_marker(&plan.package_dir, &plan)?;
        if !matches!(
            ledger.phase,
            SessionStorageOperationPhase::Applying | SessionStorageOperationPhase::Validating
        ) {
            return Ok(DowngradeRecoveryReceipt {
                operation_id: operation_id.to_string(),
                status: DowngradeRecoveryStatus::ResidualPreserved,
                package_preserved: true,
                staging_removed: false,
            });
        }
        let manifest = if plan.package_dir.join(MANIFEST_NAME).is_file() {
            verify_downgrade_package(&plan.package_dir)?
        } else {
            finish_published_downgrade_package(&plan)?
        };
        if manifest.operation_id != operation_id || manifest.target != plan.target {
            return Err("recovered downgrade package identity changed".to_string());
        }
        if manifest.native_runtime_verification.is_none() {
            remove_owned_downgrade_tree(&plan.package_dir, &plan.destination_root)?;
            let current = store.load(operation_id)?;
            if current.phase != SessionStorageOperationPhase::RollingBack {
                store.transition(operation_id, SessionStorageOperationPhase::RollingBack)?;
            }
            store.transition(operation_id, SessionStorageOperationPhase::RolledBack)?;
            return Ok(DowngradeRecoveryReceipt {
                operation_id: operation_id.to_string(),
                status: DowngradeRecoveryStatus::RolledBack,
                package_preserved: false,
                staging_removed: true,
            });
        }
        let current = store.load(operation_id)?;
        if current.phase == SessionStorageOperationPhase::Applying {
            store.transition(operation_id, SessionStorageOperationPhase::Validating)?;
        }
        store.transition(operation_id, SessionStorageOperationPhase::Committed)?;
        return Ok(DowngradeRecoveryReceipt {
            operation_id: operation_id.to_string(),
            status: DowngradeRecoveryStatus::Committed,
            package_preserved: true,
            staging_removed: false,
        });
    }

    let mut staging_removed = false;
    if plan.staging_dir.exists() {
        validate_package_marker(&plan.staging_dir, &plan)?;
        remove_owned_downgrade_tree(&plan.staging_dir, &plan.destination_root)?;
        staging_removed = true;
    }
    let current = store.load(operation_id)?;
    if current.phase != SessionStorageOperationPhase::RollingBack {
        store.transition(operation_id, SessionStorageOperationPhase::RollingBack)?;
    }
    store.transition(operation_id, SessionStorageOperationPhase::RolledBack)?;
    Ok(DowngradeRecoveryReceipt {
        operation_id: operation_id.to_string(),
        status: DowngradeRecoveryStatus::RolledBack,
        package_preserved: false,
        staging_removed,
    })
}

fn finish_published_downgrade_package(
    plan: &DowngradeExportPlan,
) -> Result<DowngradeExportManifest, String> {
    let final_codex_home = plan.package_dir.join("codex-home");
    let final_data_root = plan.package_dir.join("appdata/codex-switch");
    if plan.target.incremental_index_required {
        save_session_sync_index(
            &final_data_root.join("session-sync-state-v1.json"),
            &local_codex_paths(&final_codex_home),
            &local_codex_paths(&final_data_root.join("shared-sessions")),
        )
        .map_err(|_| "failed to recover the v0.2 incremental session index".to_string())?;
    }
    if plan.target.mobile_continuity_required {
        mobile_continuity::initialize_status(
            &final_data_root.join("mobile-continuity-v1.json"),
            &local_codex_paths(&final_codex_home),
        )
        .map_err(|_| "failed to recover the v0.2 mobile continuity state".to_string())?;
    }
    let slots = count_runtime_slots(&final_data_root, &plan.target)?;
    let manifest = build_downgrade_manifest(plan, slots)?;
    write_downgrade_manifest(&plan.package_dir, &manifest)?;
    verify_downgrade_package(&plan.package_dir)
}

fn count_runtime_slots(
    data_root: &Path,
    target: &DowngradeTargetContract,
) -> Result<usize, String> {
    let runtimes = RuntimeStore::new(data_root.join("runtimes"));
    if target.runtime_bundle_required {
        let listed = runtimes
            .list_runtimes()
            .map_err(|_| "downgrade runtime inventory is invalid".to_string())?;
        for runtime in &listed {
            runtimes
                .load_runtime_files(&runtime.id)
                .map_err(|_| "downgrade runtime bundle is invalid".to_string())?;
        }
        Ok(listed.len())
    } else {
        Ok([PLUS_RUNTIME_ID, RELAY_RUNTIME_ID]
            .iter()
            .filter(|runtime_id| {
                let root = runtimes.runtime_dir(runtime_id);
                ["auth.enc", "config.toml", "runtime.json"]
                    .iter()
                    .all(|name| root.join(name).is_file())
            })
            .count())
    }
}

fn validate_package_marker(root: &Path, plan: &DowngradeExportPlan) -> Result<(), String> {
    validate_marker_identity(root, &plan.operation_id, &plan.target.version)
}

fn validate_manifest_marker(root: &Path, manifest: &DowngradeExportManifest) -> Result<(), String> {
    validate_marker_identity(root, &manifest.operation_id, &manifest.target.version)
}

fn validate_marker_identity(
    root: &Path,
    operation_id: &str,
    target_version: &str,
) -> Result<(), String> {
    let bytes = read_regular_file_bounded(&root.join(PACKAGE_MARKER_NAME), 64 * 1024)
        .map_err(|_| "downgrade package ownership marker is unavailable".to_string())?;
    let marker = serde_json::from_slice::<serde_json::Value>(&bytes)
        .map_err(|_| "downgrade package ownership marker is invalid".to_string())?;
    if marker
        .get("schemaVersion")
        .and_then(serde_json::Value::as_u64)
        != Some(DOWNGRADE_SCHEMA_VERSION as u64)
        || marker
            .get("operationId")
            .and_then(serde_json::Value::as_str)
            != Some(operation_id)
        || marker
            .get("targetVersion")
            .and_then(serde_json::Value::as_str)
            != Some(target_version)
    {
        return Err("downgrade package ownership marker does not match the operation".to_string());
    }
    Ok(())
}

fn remove_owned_downgrade_tree(root: &Path, destination_root: &Path) -> Result<(), String> {
    if root.parent() != Some(destination_root) || !root.is_absolute() {
        return Err("downgrade cleanup root is invalid".to_string());
    }
    let mut entries = Vec::new();
    for entry in WalkDir::new(root).follow_links(false).contents_first(true) {
        let entry = entry.map_err(|_| "failed to inspect downgrade cleanup tree".to_string())?;
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|_| "failed to inspect downgrade cleanup entry".to_string())?;
        if metadata_is_link_or_reparse(&metadata) || (!metadata.is_file() && !metadata.is_dir()) {
            return Err("downgrade cleanup tree contains an unsafe entry".to_string());
        }
        entries.push((entry.path().to_path_buf(), metadata.is_dir()));
    }
    for (path, is_directory) in entries {
        if is_directory {
            fs::remove_dir(&path)
                .map_err(|_| "failed to remove downgrade cleanup directory".to_string())?;
        } else {
            fs::remove_file(&path)
                .map_err(|_| "failed to remove downgrade cleanup file".to_string())?;
        }
    }
    Ok(())
}

fn copy_session_payloads(plan: &DowngradeExportPlan) -> Result<(), String> {
    let mut published = BTreeMap::<String, String>::new();
    for session in &plan.sessions {
        let before = stable_file_digest(&session.source_path)?;
        if before.0 != session.bytes || before.1 != session.sha256 {
            return Err("downgrade source session changed before copy".to_string());
        }
        let output = plan.staging_dir.join(&session.output_relative_path);
        let output_key = relative_path_key(&session.output_relative_path);
        if let Some(existing) = published.get(&output_key) {
            if existing != &session.sha256 {
                return Err("downgrade session output collided".to_string());
            }
        } else {
            atomic_copy(&session.source_path, &output)?;
            let copied = stable_file_digest(&output)?;
            if copied != before {
                return Err("downgrade session copy verification failed".to_string());
            }
            published.insert(output_key, session.sha256.clone());
        }
        if stable_file_digest(&session.source_path)? != before {
            return Err("downgrade source session changed during copy".to_string());
        }
    }
    Ok(())
}

fn copy_live_profile(plan: &DowngradeExportPlan, target_home: &Path) -> Result<(), String> {
    let auth = plan.canonical_root.join("auth.json");
    regular_file_len(&auth)?;
    atomic_copy(&auth, &target_home.join("auth.json"))?;
    if stable_file_digest(&auth)? != stable_file_digest(&target_home.join("auth.json"))? {
        return Err("downgrade auth copy verification failed".to_string());
    }
    let config_path = plan.canonical_root.join("config.toml");
    let config_bytes = read_regular_file_bounded(&config_path, 16 * 1024 * 1024)
        .map_err(|_| "downgrade source config is unreadable".to_string())?;
    let config = std::str::from_utf8(&config_bytes)
        .map_err(|_| "downgrade source config is not UTF-8".to_string())?;
    let mut document = DocumentMut::from_str(config)
        .map_err(|_| "downgrade source config is invalid".to_string())?;
    document.remove("sqlite_home");
    atomic_write(
        &target_home.join("config.toml"),
        document.to_string().as_bytes(),
    )?;
    let source_index = plan.canonical_root.join("session_index.jsonl");
    let target_index = target_home.join("session_index.jsonl");
    if source_index.is_file() {
        atomic_copy(&source_index, &target_index)?;
    } else {
        atomic_write(&target_index, b"")?;
    }
    Ok(())
}

fn snapshot_profile_databases(
    plan: &DowngradeExportPlan,
    target_home: &Path,
) -> Result<(), String> {
    for (name, required) in [
        ("state_5.sqlite", true),
        ("logs_2.sqlite", false),
        ("memories_1.sqlite", false),
    ] {
        let source = plan.account_sqlite_home.join(name);
        if !source.is_file() {
            if required {
                return Err("canonical Account state database is unavailable".to_string());
            }
            continue;
        }
        snapshot_sqlite(&source, &target_home.join(name))?;
    }
    snapshot_global_goals_database(plan, target_home)?;
    Ok(())
}

fn snapshot_global_goals_database(
    plan: &DowngradeExportPlan,
    target_home: &Path,
) -> Result<(), String> {
    let discovery = discover_database_catalog(&plan.canonical_root, &plan.data_root);
    if discovery.errors != 0 || discovery.goals_errors != 0 {
        return Err("downgrade goals database inventory is incomplete".to_string());
    }
    if discovery.goals_descriptors.is_empty() {
        return Ok(());
    }

    let staging_root = target_home.join(".goals-union");
    if staging_root.exists() {
        return Err("downgrade goals staging already exists".to_string());
    }
    create_safe_directory(&staging_root)?;
    let result = (|| {
        let mut databases = Vec::new();
        for descriptor in &discovery.goals_descriptors {
            let stage = staging_root.join(format!("{}.sqlite", descriptor.id));
            snapshot_sqlite(&descriptor.source_path, &stage)?;
            let connection = Connection::open_with_flags(
                &stage,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .map_err(|_| "failed to open downgrade goals snapshot".to_string())?;
            goals_database_digest(&connection)?;
            drop(connection);
            let (bytes, sha256) = stable_file_digest(&stage)?;
            let role = descriptor
                .views
                .iter()
                .map(|view| view.role)
                .min()
                .unwrap_or(DatabaseRole::UnknownRuntime);
            databases.push(MigrationDatabaseApplyEntry {
                database_id: format!("{}-view-0000", descriptor.id),
                role,
                target_path: descriptor.source_path.clone(),
                staged_path: stage,
                original_backup_payload: descriptor.source_path.clone(),
                original_sha256: sha256.clone(),
                staged_sha256: sha256,
                staged_bytes: bytes,
            });
        }
        merge_goals_database_views(&mut databases)?;
        let merged = databases
            .iter()
            .min_by_key(|database| {
                (
                    database.role,
                    path_key(&database.target_path),
                    database.database_id.clone(),
                )
            })
            .ok_or_else(|| "downgrade merged goals database is missing".to_string())?;
        let target = target_home.join("goals_1.sqlite");
        atomic_copy(&merged.staged_path, &target)?;
        quick_check_sqlite(&target)?;
        let connection = Connection::open_with_flags(
            &target,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|_| "failed to open exported downgrade goals database".to_string())?;
        let exported_digest = goals_database_digest(&connection)?;
        drop(connection);
        let merged_connection = Connection::open_with_flags(
            &merged.staged_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|_| "failed to reopen merged downgrade goals database".to_string())?;
        if goals_database_digest(&merged_connection)? != exported_digest {
            return Err("downgrade goals export changed after union".to_string());
        }
        Ok(())
    })();
    let cleanup = remove_owned_goals_staging(&staging_root, target_home);
    match (result, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(cleanup_error)) => Err(format!("{error}; {cleanup_error}")),
    }
}

fn remove_owned_goals_staging(root: &Path, target_home: &Path) -> Result<(), String> {
    if root.parent() != Some(target_home)
        || root.file_name().is_none_or(|name| name != ".goals-union")
    {
        return Err("downgrade goals staging root is invalid".to_string());
    }
    let mut entries = Vec::new();
    for entry in WalkDir::new(root).follow_links(false).contents_first(true) {
        let entry = entry.map_err(|_| "failed to inspect downgrade goals staging".to_string())?;
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|_| "failed to inspect downgrade goals staging entry".to_string())?;
        if metadata_is_link_or_reparse(&metadata) || (!metadata.is_file() && !metadata.is_dir()) {
            return Err("downgrade goals staging contains an unsafe entry".to_string());
        }
        entries.push((entry.path().to_path_buf(), metadata.is_dir()));
    }
    for (path, is_directory) in entries {
        if is_directory {
            fs::remove_dir(&path)
                .map_err(|_| "failed to remove downgrade goals staging directory".to_string())?;
        } else {
            fs::remove_file(&path)
                .map_err(|_| "failed to remove downgrade goals staging file".to_string())?;
        }
    }
    Ok(())
}

fn rewrite_account_rollout_paths(
    database: &Path,
    plan: &DowngradeExportPlan,
    output_root: &Path,
) -> Result<(), String> {
    let mapping = plan
        .sessions
        .iter()
        .filter(|session| {
            matches!(
                session.kind,
                DowngradePackageFileKind::ActiveSession | DowngradePackageFileKind::ArchivedSession
            )
        })
        .map(|session| {
            (
                path_key(&session.source_path),
                (
                    session.logical_thread_id.clone(),
                    output_root.join(&session.output_relative_path),
                ),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut connection = Connection::open(database)
        .map_err(|_| "failed to open exported state database".to_string())?;
    let transaction = connection
        .transaction()
        .map_err(|_| "failed to start exported state database update".to_string())?;
    let rows = {
        let mut statement = transaction
            .prepare("SELECT id, rollout_path FROM threads WHERE rollout_path IS NOT NULL")
            .map_err(|_| "exported state database threads schema is unsupported".to_string())?;
        let mapped = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|_| "failed to read exported state references".to_string())?;
        mapped
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "failed to read exported state references".to_string())?
    };
    for (thread_id, source) in rows {
        let (expected_thread, target) = mapping
            .get(&path_key(Path::new(&source)))
            .ok_or_else(|| "exported state database references an unplanned session".to_string())?;
        if expected_thread.as_deref() != Some(thread_id.as_str()) {
            return Err("exported state database thread identity changed".to_string());
        }
        let changed = transaction
            .execute(
                "UPDATE threads SET rollout_path = ?1 WHERE id = ?2 AND rollout_path = ?3",
                (target.to_string_lossy().to_string(), &thread_id, &source),
            )
            .map_err(|_| "failed to rewrite exported state reference".to_string())?;
        if changed != 1 {
            return Err("exported state reference changed concurrently".to_string());
        }
    }
    transaction
        .commit()
        .map_err(|_| "failed to commit exported state references".to_string())?;
    drop(connection);
    finalize_downgrade_sqlite(database)
}

fn rewrite_database_root(database: &Path, old_root: &Path, new_root: &Path) -> Result<(), String> {
    let mut connection = Connection::open(database)
        .map_err(|_| "failed to open downgrade database for final path binding".to_string())?;
    let transaction = connection
        .transaction()
        .map_err(|_| "failed to start downgrade path binding".to_string())?;
    let rows = {
        let mut statement = transaction
            .prepare("SELECT id, rollout_path FROM threads WHERE rollout_path IS NOT NULL")
            .map_err(|_| "downgrade database threads schema is unsupported".to_string())?;
        let mapped = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|_| "failed to read downgrade database paths".to_string())?;
        mapped
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "failed to read downgrade database paths".to_string())?
    };
    for (thread_id, old_path) in rows {
        let old_path_buf = PathBuf::from(&old_path);
        let relative = old_path_buf
            .strip_prefix(old_root)
            .map_err(|_| "downgrade database path escaped its isolated root".to_string())?;
        let new_path = new_root.join(relative).to_string_lossy().to_string();
        let changed = transaction
            .execute(
                "UPDATE threads SET rollout_path = ?1 WHERE id = ?2 AND rollout_path = ?3",
                (&new_path, &thread_id, &old_path),
            )
            .map_err(|_| "failed to bind downgrade database path".to_string())?;
        if changed != 1 {
            return Err("downgrade database path changed concurrently".to_string());
        }
    }
    transaction
        .commit()
        .map_err(|_| "failed to commit downgrade path binding".to_string())?;
    drop(connection);
    finalize_downgrade_sqlite(database)
}

fn create_shared_session_view(
    current: &crate::codex_paths::CodexPaths,
    shared: &crate::codex_paths::CodexPaths,
) -> Result<(), String> {
    create_safe_directory(&shared.codex_home)?;
    snapshot_sqlite(&current.state_db, &shared.state_db)?;
    let connection = Connection::open_with_flags(
        &current.state_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| "failed to open downgrade Account view".to_string())?;
    let mut statement = connection
        .prepare("SELECT id, rollout_path FROM threads WHERE rollout_path IS NOT NULL")
        .map_err(|_| "downgrade Account threads schema is unsupported".to_string())?;
    let mut rows = statement
        .query([])
        .map_err(|_| "failed to read downgrade Account view".to_string())?;
    let mut mappings = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|_| "failed to read downgrade Account view".to_string())?
    {
        let thread_id: String = row
            .get(0)
            .map_err(|_| "downgrade Account thread identity is invalid".to_string())?;
        let raw_path: String = row
            .get(1)
            .map_err(|_| "downgrade Account rollout path is invalid".to_string())?;
        let source = PathBuf::from(&raw_path);
        let target = if let Ok(relative) = source.strip_prefix(&current.sessions_dir) {
            shared.sessions_dir.join(relative)
        } else if let Ok(relative) = source.strip_prefix(&current.archived_sessions_dir) {
            shared.archived_sessions_dir.join(relative)
        } else {
            return Err("downgrade Account rollout escaped its isolated root".to_string());
        };
        let semantic = read_semantic_session(&source)
            .map_err(|_| "downgrade Account rollout is invalid".to_string())?;
        if semantic.thread_id != thread_id {
            return Err("downgrade Account rollout identity changed".to_string());
        }
        atomic_copy(&source, &target)?;
        if stable_file_digest(&source)? != stable_file_digest(&target)? {
            return Err("downgrade shared session copy verification failed".to_string());
        }
        mappings.push((thread_id, raw_path, target));
    }
    drop(rows);
    drop(statement);
    drop(connection);
    let mut target_connection = Connection::open(&shared.state_db)
        .map_err(|_| "failed to open downgrade Shared view".to_string())?;
    let transaction = target_connection
        .transaction()
        .map_err(|_| "failed to start downgrade Shared view update".to_string())?;
    for (thread_id, source, target) in mappings {
        let changed = transaction
            .execute(
                "UPDATE threads SET rollout_path = ?1 WHERE id = ?2 AND rollout_path = ?3",
                (target.to_string_lossy().to_string(), thread_id, source),
            )
            .map_err(|_| "failed to write downgrade Shared view".to_string())?;
        if changed != 1 {
            return Err("downgrade Shared view changed concurrently".to_string());
        }
    }
    transaction
        .commit()
        .map_err(|_| "failed to commit downgrade Shared view".to_string())?;
    drop(target_connection);
    if current.session_index.is_file() {
        atomic_copy(&current.session_index, &shared.session_index)?;
    } else {
        atomic_write(&shared.session_index, b"")?;
    }
    finalize_downgrade_sqlite(&shared.state_db)
}

fn copy_runtime_slots(
    plan: &DowngradeExportPlan,
    staging_codex_home: &Path,
    staging_data_root: &Path,
) -> Result<usize, String> {
    let source_store = RuntimeStore::new(plan.data_root.join("runtimes"));
    let target_store = RuntimeStore::new(staging_data_root.join("runtimes"));
    let mut copied = 0_usize;
    for runtime_id in [PLUS_RUNTIME_ID, RELAY_RUNTIME_ID] {
        let source_dir = source_store.runtime_dir(runtime_id);
        if !source_dir.exists() {
            continue;
        }
        source_store
            .load_runtime_files(runtime_id)
            .map_err(|_| "stored runtime slot is invalid".to_string())?;
        source_store
            .load_metadata(runtime_id)
            .map_err(|_| "stored runtime metadata is invalid".to_string())?;
        let mut names = vec!["auth.enc", "config.toml", "runtime.json"];
        if plan.target.runtime_bundle_required {
            names.extend(["bundle.json", ".bundle-v1"]);
        }
        let target_dir = target_store.runtime_dir(runtime_id);
        for name in names {
            let source = source_dir.join(name);
            regular_file_len(&source)?;
            atomic_copy(&source, &target_dir.join(name))?;
        }
        sanitize_exported_runtime_slot(&target_dir, plan.target.runtime_bundle_required)?;
        if plan.target.runtime_bundle_required {
            target_store
                .load_runtime_files(runtime_id)
                .map_err(|_| "exported runtime slot is incompatible".to_string())?;
        }
        copied = copied.saturating_add(1);
    }
    if copied == 0
        && target_store
            .import_plus_from_home(staging_codex_home, false)
            .is_ok()
    {
        copied = 1;
        if !plan.target.runtime_bundle_required {
            for name in ["bundle.json", ".bundle-v1"] {
                let path = target_store.runtime_dir(PLUS_RUNTIME_ID).join(name);
                if path.exists() {
                    fs::remove_file(path)
                        .map_err(|_| "failed to adapt the v0.2.0 runtime slot".to_string())?;
                }
            }
        }
    }
    Ok(copied)
}

fn sanitize_exported_runtime_slot(runtime_dir: &Path, bundle_required: bool) -> Result<(), String> {
    let config_path = runtime_dir.join("config.toml");
    let config = read_regular_file_bounded(&config_path, 16 * 1024 * 1024)
        .map_err(|_| "exported runtime config is unreadable".to_string())?;
    let mut document = DocumentMut::from_str(
        std::str::from_utf8(&config)
            .map_err(|_| "exported runtime config is not UTF-8".to_string())?,
    )
    .map_err(|_| "exported runtime config is invalid".to_string())?;
    document.remove("sqlite_home");
    atomic_write(&config_path, document.to_string().as_bytes())?;
    verify_exported_runtime_slot_isolated(runtime_dir)?;

    if bundle_required {
        let auth_sha256 = stable_file_digest(&runtime_dir.join("auth.enc"))?.1;
        let config_sha256 = stable_file_digest(&config_path)?.1;
        let metadata_sha256 = stable_file_digest(&runtime_dir.join("runtime.json"))?.1;
        let bundle = serde_json::to_vec_pretty(&serde_json::json!({
            "version": 1,
            "authSha256": auth_sha256,
            "configSha256": config_sha256,
            "metadataSha256": metadata_sha256,
        }))
        .map_err(|_| "failed to encode exported runtime bundle".to_string())?;
        atomic_write(&runtime_dir.join("bundle.json"), &bundle)?;
    }
    Ok(())
}

fn verify_exported_runtime_slot_isolated(runtime_dir: &Path) -> Result<(), String> {
    let config = read_regular_file_bounded(&runtime_dir.join("config.toml"), 16 * 1024 * 1024)
        .map_err(|_| "downgrade runtime config is unreadable".to_string())?;
    let document = DocumentMut::from_str(
        std::str::from_utf8(&config)
            .map_err(|_| "downgrade runtime config is not UTF-8".to_string())?,
    )
    .map_err(|_| "downgrade runtime config is invalid".to_string())?;
    if document.get("sqlite_home").is_some() {
        return Err("downgrade runtime slot escaped its isolated SQLite root".to_string());
    }
    Ok(())
}

fn write_package_marker(root: &Path, plan: &DowngradeExportPlan) -> Result<(), String> {
    let marker = serde_json::json!({
        "schemaVersion": DOWNGRADE_SCHEMA_VERSION,
        "operationId": plan.operation_id,
        "targetVersion": plan.target.version,
    });
    let mut bytes = serde_json::to_vec_pretty(&marker)
        .map_err(|_| "failed to serialize downgrade package marker".to_string())?;
    bytes.push(b'\n');
    atomic_write(&root.join(PACKAGE_MARKER_NAME), &bytes)
}

fn write_launchers_and_readme(root: &Path, plan: &DowngradeExportPlan) -> Result<(), String> {
    let switch_script = format!(
        "@echo off\r\nsetlocal\r\nset \"CODEX_HOME=%~dp0codex-home\"\r\nset \"APPDATA=%~dp0appdata\"\r\nset \"LOCALAPPDATA=%~dp0localappdata\"\r\nset \"CODEX_SQLITE_HOME=\"\r\nif not exist \"%~dp0codex-switch.exe\" (\r\n  echo Place the exact {} codex-switch.exe beside this script.\r\n  exit /b 2\r\n)\r\nstart \"\" /wait \"%~dp0codex-switch.exe\"\r\nendlocal\r\n",
        plan.target.version
    );
    let cli_script = "@echo off\r\nsetlocal\r\nset \"CODEX_HOME=%~dp0codex-home\"\r\nset \"APPDATA=%~dp0appdata\"\r\nset \"LOCALAPPDATA=%~dp0localappdata\"\r\nset \"CODEX_SQLITE_HOME=\"\r\nif defined CODEX_EXE (\r\n  \"%CODEX_EXE%\" %*\r\n) else (\r\n  codex %*\r\n)\r\nendlocal\r\n";
    let readme = format!(
        "Codex Switch isolated downgrade package\r\nTarget: {}\r\n\r\nThis package contains complete session bodies and local credentials. Keep it private.\r\nDo not point {} at the v0.3 canonical directory.\r\nUse launch-codex-cli.cmd for an environment-isolated Codex CLI.\r\nPlace the exact target codex-switch.exe beside launch-switch.cmd before using the old Switch UI.\r\nThis package is bound to the directory where it was generated. Choose the final location before export and do not move it; generate a new package from canonical storage when the location must change.\r\n",
        plan.target.version, plan.target.version
    );
    atomic_write(&root.join("launch-switch.cmd"), switch_script.as_bytes())?;
    atomic_write(&root.join("launch-codex-cli.cmd"), cli_script.as_bytes())?;
    atomic_write(&root.join("README.txt"), readme.as_bytes())
}

fn build_downgrade_manifest(
    plan: &DowngradeExportPlan,
    runtime_slot_count: usize,
) -> Result<DowngradeExportManifest, String> {
    let mut planned_sessions = BTreeMap::new();
    for session in &plan.sessions {
        planned_sessions.insert(
            relative_path_key(&session.output_relative_path),
            (session.kind, session.logical_thread_id.clone()),
        );
    }
    let mut entries = Vec::new();
    let mut package_bytes = 0_u64;
    for entry in WalkDir::new(&plan.package_dir).follow_links(false) {
        let entry = entry.map_err(|_| "failed to inventory downgrade package".to_string())?;
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|_| "failed to inspect downgrade package entry".to_string())?;
        if metadata_is_link_or_reparse(&metadata) || (!metadata.is_file() && !metadata.is_dir()) {
            return Err("downgrade package contains an unsafe entry".to_string());
        }
        if !metadata.is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(&plan.package_dir)
            .map_err(|_| "downgrade package path escaped its root".to_string())?
            .to_path_buf();
        if relative == Path::new(MANIFEST_NAME) {
            continue;
        }
        let (bytes, sha256) = stable_file_digest(entry.path())?;
        package_bytes = package_bytes
            .checked_add(bytes)
            .ok_or_else(|| "downgrade package byte count overflowed".to_string())?;
        let (kind, logical_thread_id) =
            classify_package_entry(&relative, &planned_sessions, entry.path())?;
        entries.push(DowngradePackageEntry {
            relative_path: relative,
            kind,
            bytes,
            sha256,
            logical_thread_id,
        });
    }
    entries.sort_by_key(|entry| relative_path_key(&entry.relative_path));
    let logical_session_count = entries
        .iter()
        .filter_map(|entry| entry.logical_thread_id.clone())
        .collect::<BTreeSet<_>>()
        .len();
    let session_file_count = entries
        .iter()
        .filter(|entry| {
            matches!(
                entry.kind,
                DowngradePackageFileKind::ActiveSession
                    | DowngradePackageFileKind::ArchivedSession
                    | DowngradePackageFileKind::ConflictBranch
            )
        })
        .count();
    let conflict_branch_count = entries
        .iter()
        .filter(|entry| entry.kind == DowngradePackageFileKind::ConflictBranch)
        .count();
    let recovery_payload_count = entries
        .iter()
        .filter(|entry| entry.kind == DowngradePackageFileKind::RecoveryPayload)
        .count();
    let manifest = DowngradeExportManifest {
        schema_version: DOWNGRADE_SCHEMA_VERSION,
        operation_id: plan.operation_id.clone(),
        created_at_ms: timestamp_millis()?,
        created_with_version: format!("v{}", env!("CARGO_PKG_VERSION")),
        target: plan.target.clone(),
        status: DowngradeExportStatus::StructurallyVerified,
        source_inventory_fingerprint: plan.source_inventory_fingerprint.clone(),
        codex_home_relative_path: PathBuf::from("codex-home"),
        appdata_relative_path: PathBuf::from("appdata"),
        contains_credentials: true,
        initial_runtime_slot_count: runtime_slot_count,
        logical_session_count,
        session_file_count,
        conflict_branch_count,
        recovery_payload_count,
        package_bytes,
        entries,
        native_runtime_verification: None,
        runtime_verification: None,
    };
    validate_manifest(&manifest)?;
    Ok(manifest)
}

fn classify_package_entry(
    relative: &Path,
    planned_sessions: &BTreeMap<String, (DowngradePackageFileKind, Option<String>)>,
    full_path: &Path,
) -> Result<(DowngradePackageFileKind, Option<String>), String> {
    if let Some((kind, thread_id)) = planned_sessions.get(&relative_path_key(relative)) {
        return Ok((*kind, thread_id.clone()));
    }
    if relative.starts_with(Path::new("appdata/codex-switch/shared-sessions")) {
        let is_session = relative
            .starts_with(Path::new("appdata/codex-switch/shared-sessions/sessions"))
            || relative.starts_with(Path::new(
                "appdata/codex-switch/shared-sessions/archived_sessions",
            ));
        let thread_id = if is_session
            && relative
                .extension()
                .is_some_and(|extension| extension == "jsonl")
        {
            Some(
                read_semantic_session(full_path)
                    .map_err(|_| "shared downgrade session is invalid".to_string())?
                    .thread_id,
            )
        } else {
            None
        };
        return Ok((DowngradePackageFileKind::SharedView, thread_id));
    }
    let name = relative
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let kind =
        if relative == Path::new("codex-home/auth.json") || name.eq_ignore_ascii_case("auth.enc") {
            DowngradePackageFileKind::Credential
        } else if relative == Path::new("codex-home/config.toml") {
            DowngradePackageFileKind::Config
        } else if relative.starts_with(Path::new("appdata/codex-switch/runtimes")) {
            DowngradePackageFileKind::RuntimeBundle
        } else if relative == Path::new("codex-home/state_5.sqlite") {
            DowngradePackageFileKind::StateDatabase
        } else if relative.starts_with(Path::new("codex-home")) && name.ends_with(".sqlite") {
            DowngradePackageFileKind::AuxiliaryDatabase
        } else if name == "session_index.jsonl" || name == "session-sync-state-v1.json" {
            DowngradePackageFileKind::SessionIndex
        } else if name.ends_with(".cmd") {
            DowngradePackageFileKind::Launcher
        } else {
            DowngradePackageFileKind::Bootstrap
        };
    Ok((kind, None))
}

fn write_downgrade_manifest(
    package_dir: &Path,
    manifest: &DowngradeExportManifest,
) -> Result<(), String> {
    validate_manifest(manifest)?;
    let envelope = DowngradeManifestEnvelope {
        manifest: manifest.clone(),
        integrity_sha256: manifest_digest(manifest)?,
    };
    let bytes = serde_json::to_vec_pretty(&envelope)
        .map_err(|_| "failed to serialize downgrade package manifest".to_string())?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err("downgrade package manifest reached its size limit".to_string());
    }
    atomic_write(&package_dir.join(MANIFEST_NAME), &bytes)
}

fn read_downgrade_manifest(package_dir: &Path) -> Result<DowngradeManifestEnvelope, String> {
    let bytes = read_regular_file_bounded(&package_dir.join(MANIFEST_NAME), MAX_MANIFEST_BYTES)
        .map_err(|_| "downgrade package manifest is unreadable".to_string())?;
    serde_json::from_slice(&bytes).map_err(|_| "downgrade package manifest is invalid".to_string())
}

fn manifest_digest(manifest: &DowngradeExportManifest) -> Result<String, String> {
    let bytes = serde_json::to_vec(manifest)
        .map_err(|_| "failed to encode downgrade package manifest".to_string())?;
    Ok(hex_digest(Sha256::digest(bytes)))
}

fn validate_manifest(manifest: &DowngradeExportManifest) -> Result<(), String> {
    if manifest.schema_version != DOWNGRADE_SCHEMA_VERSION
        || manifest.operation_id.is_empty()
        || downgrade_target_contract(&manifest.target.version)? != manifest.target
        || manifest.codex_home_relative_path != Path::new("codex-home")
        || manifest.appdata_relative_path != Path::new("appdata")
        || !manifest.contains_credentials
        || manifest.entries.is_empty()
        || manifest.source_inventory_fingerprint.len() != 64
        || !manifest
            .source_inventory_fingerprint
            .bytes()
            .all(is_lower_hex)
    {
        return Err("downgrade package manifest is invalid".to_string());
    }
    if let Some(runtime) = &manifest.native_runtime_verification {
        if !native_runtime_verification_complete(runtime, manifest.logical_session_count)
            || runtime.verified_at_ms < manifest.created_at_ms
        {
            return Err("downgrade package native runtime verification is invalid".to_string());
        }
    }
    match (manifest.status, manifest.runtime_verification.as_ref()) {
        (DowngradeExportStatus::StructurallyVerified, None) => {}
        (DowngradeExportStatus::TargetRuntimeVerified, Some(runtime))
            if runtime.target_version == manifest.target.version
                && runtime.listed_session_count >= manifest.logical_session_count
                && (manifest.logical_session_count == 0 || runtime.resumed_session_count > 0)
                && (manifest.logical_session_count == 0 || runtime.continued_session_count > 0)
                && runtime.verified_at_ms >= manifest.created_at_ms => {}
        _ => return Err("downgrade package target runtime verification is invalid".to_string()),
    }
    validate_operation_id(&manifest.operation_id)?;
    for entry in &manifest.entries {
        validate_relative_path(&entry.relative_path)?;
        if entry
            .relative_path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".sqlite-wal") || name.ends_with(".sqlite-shm"))
        {
            return Err("downgrade package manifest contains a SQLite sidecar".to_string());
        }
        if entry.sha256.len() != 64 || !entry.sha256.bytes().all(is_lower_hex) {
            return Err("downgrade package entry checksum is invalid".to_string());
        }
    }
    Ok(())
}

fn native_runtime_verification_complete(
    runtime: &MigrationRuntimeVerification,
    expected_session_count: usize,
) -> bool {
    let mut available = runtime.available_categories.clone();
    available.sort();
    available.dedup();
    let mut continued = runtime.continued_categories.clone();
    continued.sort();
    continued.dedup();
    runtime.expected_session_count == expected_session_count
        && runtime.listed_session_count == expected_session_count
        && runtime.resumed_session_count == expected_session_count
        && (expected_session_count == 0 || runtime.continued_session_count > 0)
        && (expected_session_count == 0 || (!available.is_empty() && available == continued))
        && (runtime.tool_session_count == 0 || runtime.tool_round_trip_verified)
        && (runtime.conflict_payload_count == 0 || runtime.conflict_payloads_verified)
}

fn verify_package_bootstrap(
    package_dir: &Path,
    manifest: &DowngradeExportManifest,
) -> Result<(), String> {
    for relative in [
        Path::new(PACKAGE_MARKER_NAME),
        Path::new("README.txt"),
        Path::new("launch-switch.cmd"),
        Path::new("launch-codex-cli.cmd"),
        Path::new("codex-home/auth.json"),
        Path::new("codex-home/config.toml"),
        Path::new("codex-home/state_5.sqlite"),
    ] {
        if !package_dir.join(relative).is_file() {
            return Err("downgrade package bootstrap is incomplete".to_string());
        }
    }
    let config = read_regular_file_bounded(
        &package_dir.join("codex-home/config.toml"),
        16 * 1024 * 1024,
    )
    .map_err(|_| "downgrade package config is unreadable".to_string())?;
    let document = DocumentMut::from_str(
        std::str::from_utf8(&config)
            .map_err(|_| "downgrade package config is not UTF-8".to_string())?,
    )
    .map_err(|_| "downgrade package config is invalid".to_string())?;
    if document.get("sqlite_home").is_some() {
        return Err("downgrade package config escaped its isolated SQLite root".to_string());
    }
    let data_root = package_dir.join("appdata/codex-switch");
    let incremental = data_root.join("session-sync-state-v1.json").is_file();
    let mobile = data_root.join("mobile-continuity-v1.json").is_file();
    if incremental != manifest.target.incremental_index_required
        || mobile != manifest.target.mobile_continuity_required
        || data_root
            .join("request-route-session-view-v1.json")
            .exists()
        || data_root.join("relay-sqlite").exists()
    {
        return Err(
            "downgrade package bootstrap does not match its compatibility band".to_string(),
        );
    }
    let runtimes = RuntimeStore::new(data_root.join("runtimes"));
    let listed = if manifest.target.runtime_bundle_required {
        let listed = runtimes
            .list_runtimes()
            .map_err(|_| "downgrade runtime inventory is invalid".to_string())?;
        for runtime in &listed {
            runtimes
                .load_runtime_files(&runtime.id)
                .map_err(|_| "downgrade runtime bundle is invalid".to_string())?;
            verify_exported_runtime_slot_isolated(&runtimes.runtime_dir(&runtime.id))?;
        }
        listed.len()
    } else {
        let count = [PLUS_RUNTIME_ID, RELAY_RUNTIME_ID]
            .iter()
            .filter(|runtime_id| {
                let root = runtimes.runtime_dir(runtime_id);
                ["auth.enc", "config.toml", "runtime.json"]
                    .iter()
                    .all(|name| root.join(name).is_file())
            })
            .count();
        for runtime_id in [PLUS_RUNTIME_ID, RELAY_RUNTIME_ID] {
            let root = runtimes.runtime_dir(runtime_id);
            if root.is_dir() {
                verify_exported_runtime_slot_isolated(&root)?;
            }
        }
        count
    };
    if listed != manifest.initial_runtime_slot_count {
        return Err("downgrade runtime slot count changed".to_string());
    }
    Ok(())
}

fn verify_exported_state_references(
    package_dir: &Path,
    _manifest: &DowngradeExportManifest,
) -> Result<(), String> {
    let state_db = package_dir.join("codex-home/state_5.sqlite");
    let connection = Connection::open_with_flags(
        &state_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| "failed to open downgrade state database".to_string())?;
    quick_check_connection(&connection, "downgrade state database")?;
    let mut statement = connection
        .prepare("SELECT id, rollout_path FROM threads WHERE rollout_path IS NOT NULL")
        .map_err(|_| "downgrade state database threads schema is unsupported".to_string())?;
    let mut rows = statement
        .query([])
        .map_err(|_| "failed to read downgrade state references".to_string())?;
    let allowed_sessions = package_dir.join("codex-home/sessions");
    let allowed_archived = package_dir.join("codex-home/archived_sessions");
    while let Some(row) = rows
        .next()
        .map_err(|_| "failed to read downgrade state references".to_string())?
    {
        let thread_id: String = row
            .get(0)
            .map_err(|_| "downgrade state thread identity is invalid".to_string())?;
        let path: String = row
            .get(1)
            .map_err(|_| "downgrade state rollout path is invalid".to_string())?;
        let path = PathBuf::from(path);
        if !path.starts_with(&allowed_sessions) && !path.starts_with(&allowed_archived) {
            return Err("downgrade state reference escaped the package".to_string());
        }
        let semantic = read_semantic_session(&path)
            .map_err(|_| "downgrade state references an invalid session".to_string())?;
        if semantic.thread_id != thread_id {
            return Err("downgrade state reference thread identity changed".to_string());
        }
    }
    Ok(())
}

fn snapshot_sqlite(source: &Path, target: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(source)
        .map_err(|_| "downgrade database source is unavailable".to_string())?;
    if !metadata.is_file() || metadata_is_link_or_reparse(&metadata) {
        return Err("downgrade database source is unsafe".to_string());
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .map_err(|_| "failed to create downgrade database directory".to_string())?;
    }
    let source_connection = Connection::open_with_flags(
        source,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| "failed to open downgrade database source".to_string())?;
    source_connection
        .backup(MAIN_DB, target, None)
        .map_err(|_| "failed to snapshot downgrade database".to_string())?;
    drop(source_connection);
    finalize_downgrade_sqlite(target)
}

fn quick_check_sqlite(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| "failed to inspect downgrade SQLite payload".to_string())?;
    if !metadata.is_file() || metadata_is_link_or_reparse(&metadata) {
        return Err("downgrade SQLite payload path is unsafe".to_string());
    }
    ensure_self_contained_sqlite(path)?;
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| "failed to open downgrade SQLite payload".to_string())?;
    quick_check_connection(&connection, "downgrade SQLite payload")
}

fn finalize_downgrade_sqlite(path: &Path) -> Result<(), String> {
    let connection = Connection::open(path)
        .map_err(|_| "failed to open downgrade SQLite snapshot".to_string())?;
    let journal_mode = connection
        .query_row("PRAGMA journal_mode = DELETE", [], |row| {
            row.get::<_, String>(0)
        })
        .map_err(|_| "failed to make downgrade SQLite snapshot self-contained".to_string())?;
    if !journal_mode.eq_ignore_ascii_case("delete") {
        return Err("downgrade SQLite snapshot remained in WAL mode".to_string());
    }
    quick_check_connection(&connection, "downgrade SQLite snapshot")?;
    drop(connection);
    ensure_self_contained_sqlite(path)?;
    for suffix in ["-wal", "-shm"] {
        if PathBuf::from(format!("{}{suffix}", path.to_string_lossy())).exists() {
            return Err("downgrade SQLite snapshot retained a sidecar".to_string());
        }
    }
    Ok(())
}

fn ensure_self_contained_sqlite(path: &Path) -> Result<(), String> {
    let mut file =
        fs::File::open(path).map_err(|_| "failed to open downgrade SQLite payload".to_string())?;
    let mut header = [0_u8; 20];
    file.read_exact(&mut header)
        .map_err(|_| "downgrade SQLite payload header is invalid".to_string())?;
    if &header[..16] != b"SQLite format 3\0" || header[18] != 1 || header[19] != 1 {
        return Err("downgrade SQLite payload is not self-contained".to_string());
    }
    Ok(())
}

fn package_regular_files(package_dir: &Path) -> Result<BTreeSet<String>, String> {
    let mut paths = BTreeSet::new();
    for entry in WalkDir::new(package_dir).follow_links(false) {
        let entry = entry.map_err(|_| "failed to inspect downgrade package".to_string())?;
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|_| "failed to inspect downgrade package entry".to_string())?;
        if metadata_is_link_or_reparse(&metadata) || (!metadata.is_file() && !metadata.is_dir()) {
            return Err("downgrade package contains an unsafe entry".to_string());
        }
        if metadata.is_file() {
            let relative = entry
                .path()
                .strip_prefix(package_dir)
                .map_err(|_| "downgrade package path escaped its root".to_string())?;
            paths.insert(relative_path_key(relative));
        }
    }
    Ok(paths)
}

pub(crate) fn receipt_from_manifest(
    package_dir: &Path,
    manifest: &DowngradeExportManifest,
) -> DowngradeExportReceipt {
    DowngradeExportReceipt {
        operation_id: manifest.operation_id.clone(),
        target: manifest.target.clone(),
        package_dir: package_dir.to_path_buf(),
        logical_session_count: manifest.logical_session_count,
        session_file_count: manifest.session_file_count,
        conflict_branch_count: manifest.conflict_branch_count,
        recovery_payload_count: manifest.recovery_payload_count,
        package_bytes: manifest.package_bytes,
        contains_credentials: manifest.contains_credentials,
        structurally_verified: true,
        native_runtime_verified: manifest.native_runtime_verification.is_some(),
        target_runtime_verification_required: manifest.status
            != DowngradeExportStatus::TargetRuntimeVerified,
    }
}

fn create_safe_directory(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path)
        .map_err(|_| "failed to create downgrade package directory".to_string())?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| "failed to inspect downgrade package directory".to_string())?;
    if !metadata.is_dir() || metadata_is_link_or_reparse(&metadata) {
        return Err("downgrade package directory is unsafe".to_string());
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SelectedAccountDatabase {
    sqlite_home: PathBuf,
    database_id: String,
    catalog_fingerprint: String,
}

fn select_account_database(
    codex_home: &Path,
    data_root: &Path,
    canonical_state: &CanonicalStorageState,
    inventory: &MigrationInventory,
) -> Result<SelectedAccountDatabase, String> {
    validate_canonical_state_binding(canonical_state, codex_home)?;
    let current = resolve_user_codex_paths(codex_home)?;
    let current_home_key = path_key(&current.sqlite_home);
    let current_view = inspect_session_view_database_homes(data_root)?;
    let legacy_view = inspect_legacy_session_view_database_homes(data_root)?;

    if let (Some((current_account, _)), Some((legacy_account, _))) = (&current_view, &legacy_view) {
        if path_key(current_account) != path_key(legacy_account) {
            return Err("v1 and v2 session view Account database identities conflict".to_string());
        }
    }

    let legacy_relay = data_root.join("relay-sqlite");
    if current_home_key == path_key(&legacy_relay)
        || legacy_view
            .as_ref()
            .is_some_and(|(_, relay)| current_home_key == path_key(relay))
    {
        return Err(
            "the current runtime uses a legacy Relay database without a v2 Account identity"
                .to_string(),
        );
    }
    if current_view.is_none() && is_managed_v2_relay_home(&current.sqlite_home) {
        return Err("the current Relay view has no v2 Account identity".to_string());
    }

    let account_sqlite_home = match &current_view {
        Some((account, relay)) if current_home_key == path_key(relay) => account.clone(),
        Some((account, _)) if current_home_key == path_key(account) => current.sqlite_home.clone(),
        Some(_) => {
            return Err(
                "session view Account database conflicts with the current runtime catalog"
                    .to_string(),
            )
        }
        None => current.sqlite_home.clone(),
    };

    if let Some((legacy_account, _)) = &legacy_view {
        if path_key(legacy_account) != path_key(&account_sqlite_home) {
            return Err(
                "legacy session view Account database conflicts with the current runtime catalog"
                    .to_string(),
            );
        }
    }
    let account_state_db = account_sqlite_home.join("state_5.sqlite");
    if !account_state_db.is_file() {
        return Err("canonical Account database is unavailable".to_string());
    }
    let mut matches = inventory.catalog.databases.iter().filter(|database| {
        database
            .path
            .as_ref()
            .is_some_and(|path| path_key(path) == path_key(&account_state_db))
    });
    let database = matches.next().ok_or_else(|| {
        "canonical Account database is missing from the current runtime catalog".to_string()
    })?;
    if matches.next().is_some() || database.role != DatabaseRole::CanonicalAccount {
        return Err("canonical Account database catalog identity is ambiguous".to_string());
    }
    validate_retained_account_references(inventory, database)?;
    Ok(SelectedAccountDatabase {
        sqlite_home: account_sqlite_home,
        database_id: database.id.clone(),
        catalog_fingerprint: account_catalog_fingerprint(canonical_state, inventory, database)?,
    })
}

fn is_managed_v2_relay_home(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("relay-v2"))
        && path.parent().and_then(Path::file_name).is_some_and(|name| {
            name.to_string_lossy()
                .eq_ignore_ascii_case(".codex-switch-session-views")
        })
}

fn validate_canonical_state_binding(
    state: &CanonicalStorageState,
    canonical_root: &Path,
) -> Result<(), String> {
    if state.schema_version != 2
        || path_key(&state.canonical_root) != path_key(canonical_root)
        || !state.backup_destination.is_absolute()
        || state.prepared_at_ms == 0
        || state.committed_at_ms < state.prepared_at_ms
        || state.inventory_fingerprint.len() != 64
        || !state.inventory_fingerprint.bytes().all(is_lower_hex)
        || state
            .gc_discovery_paths()
            .iter()
            .any(|path| !path.is_absolute())
    {
        return Err("canonical storage certificate identity is invalid".to_string());
    }
    validate_operation_id(&state.migration_operation_id)
        .map_err(|_| "canonical storage certificate identity is invalid".to_string())
}

fn validate_retained_account_references(
    inventory: &MigrationInventory,
    database: &DatabaseInput,
) -> Result<(), String> {
    let account_paths = database
        .references
        .iter()
        .map(|reference| path_key(&reference.rollout_path))
        .collect::<BTreeSet<_>>();
    if inventory.graph.files.iter().any(|node| {
        node.retained_candidate
            && node.thread_id.is_some()
            && !account_paths.contains(&node.path_key)
    }) {
        return Err(
            "canonical Account database is missing a certificate-retained session reference"
                .to_string(),
        );
    }
    Ok(())
}

fn account_catalog_fingerprint(
    state: &CanonicalStorageState,
    inventory: &MigrationInventory,
    database: &DatabaseInput,
) -> Result<String, String> {
    let database_path = database
        .path
        .as_ref()
        .ok_or_else(|| "canonical Account database catalog identity is incomplete".to_string())?;
    let mut references = database
        .references
        .iter()
        .map(|reference| {
            (
                reference.thread_id.clone(),
                path_key(&reference.rollout_path),
                reference.model_provider.clone(),
            )
        })
        .collect::<Vec<_>>();
    references.sort();
    let gc_discovery_paths = state
        .gc_discovery_paths()
        .iter()
        .map(|path| path_key(path))
        .collect::<Vec<_>>();
    let identity = serde_json::json!({
        "canonicalRoot": path_key(&state.canonical_root),
        "canonicalMigrationOperationId": state.migration_operation_id,
        "canonicalInventoryFingerprint": state.inventory_fingerprint,
        "canonicalGcDiscoveryPaths": gc_discovery_paths,
        "currentInventoryFingerprint": inventory.fingerprint,
        "databaseId": database.id,
        "databasePath": path_key(database_path),
        "databaseRole": database.role,
        "references": references,
    });
    let bytes = serde_json::to_vec(&identity)
        .map_err(|_| "failed to fingerprint canonical Account catalog identity".to_string())?;
    Ok(hex_digest(Sha256::digest(bytes)))
}

fn validate_live_plan_identity(
    plan: &DowngradeExportPlan,
    inventory: &MigrationInventory,
) -> Result<(), String> {
    let state = load_committed_canonical_storage_state(&plan.data_root, &plan.canonical_root)?
        .ok_or_else(|| {
            "downgrade export lost its committed canonical storage certificate".to_string()
        })?;
    if state.migration_operation_id != plan.canonical_migration_operation_id
        || state.inventory_fingerprint != plan.canonical_inventory_fingerprint
    {
        return Err("downgrade canonical storage certificate identity changed".to_string());
    }
    let selected =
        select_account_database(&plan.canonical_root, &plan.data_root, &state, inventory)?;
    if path_key(&selected.sqlite_home) != path_key(&plan.account_sqlite_home)
        || selected.database_id != plan.account_database_id
        || selected.catalog_fingerprint != plan.account_catalog_fingerprint
    {
        return Err("downgrade canonical Account catalog identity changed".to_string());
    }
    Ok(())
}

fn plan_session_exports(
    codex_home: &Path,
    inventory: &MigrationInventory,
    account_database_id: &str,
) -> Result<Vec<DowngradeSessionExportPlan>, String> {
    let files_by_key = inventory
        .files
        .iter()
        .map(|file| (path_key(&file.path), file))
        .collect::<BTreeMap<_, _>>();
    let mut planned = Vec::new();
    let mut outputs = BTreeMap::<String, String>::new();
    for node in &inventory.graph.files {
        if matches!(
            node.origin,
            FileOrigin::BackupInventory
                | FileOrigin::RecoveryPackage
                | FileOrigin::DowngradeExport
                | FileOrigin::TemporaryAdapter
        ) {
            continue;
        }
        let referenced_by_account = node
            .runtime_database_ids
            .iter()
            .any(|id| id == account_database_id);
        if node.retained_candidate && node.thread_id.is_some() && !referenced_by_account {
            return Err(
                "canonical Account database does not reference every retained session".to_string(),
            );
        }
        let preserve_branch = matches!(
            node.relation_to_retained,
            Some(SessionRelation::Divergent | SessionRelation::Unknown)
        );
        if !referenced_by_account && !node.retained_candidate && !preserve_branch {
            continue;
        }
        let source = files_by_key
            .get(&node.path_key)
            .ok_or_else(|| "downgrade session inventory changed".to_string())?;
        let kind = session_kind(
            codex_home,
            &node.path,
            node.thread_id.as_deref(),
            referenced_by_account,
        );
        let output_relative_path = session_output_path(
            codex_home,
            &node.path,
            node.thread_id.as_deref(),
            &source.raw_sha256,
            kind,
        )?;
        validate_relative_path(&output_relative_path)?;
        match outputs.get(&relative_path_key(&output_relative_path)) {
            Some(existing) if existing != &source.raw_sha256 => {
                return Err("downgrade session output path collided".to_string())
            }
            Some(_) => {}
            None => {
                outputs.insert(
                    relative_path_key(&output_relative_path),
                    source.raw_sha256.clone(),
                );
            }
        }
        planned.push(DowngradeSessionExportPlan {
            source_path: source.path.clone(),
            output_relative_path,
            kind,
            bytes: source.bytes,
            sha256: source.raw_sha256.clone(),
            logical_thread_id: node.thread_id.clone(),
        });
    }
    planned.sort_by(|left, right| {
        relative_path_key(&left.output_relative_path)
            .cmp(&relative_path_key(&right.output_relative_path))
            .then_with(|| path_key(&left.source_path).cmp(&path_key(&right.source_path)))
    });
    if planned.is_empty() {
        return Err("downgrade export found no recoverable sessions".to_string());
    }
    Ok(planned)
}

fn session_kind(
    codex_home: &Path,
    source: &Path,
    thread_id: Option<&str>,
    referenced_by_account: bool,
) -> DowngradePackageFileKind {
    if thread_id.is_none() {
        DowngradePackageFileKind::RecoveryPayload
    } else if !referenced_by_account {
        DowngradePackageFileKind::ConflictBranch
    } else if source.starts_with(codex_home.join("archived_sessions")) {
        DowngradePackageFileKind::ArchivedSession
    } else {
        DowngradePackageFileKind::ActiveSession
    }
}

fn session_output_path(
    codex_home: &Path,
    source: &Path,
    thread_id: Option<&str>,
    sha256: &str,
    kind: DowngradePackageFileKind,
) -> Result<PathBuf, String> {
    if matches!(
        kind,
        DowngradePackageFileKind::ActiveSession | DowngradePackageFileKind::ArchivedSession
    ) && source.starts_with(codex_home)
    {
        let relative = source
            .strip_prefix(codex_home)
            .map_err(|_| "downgrade session path is invalid".to_string())?;
        if relative.starts_with("sessions") || relative.starts_with("archived_sessions") {
            return Ok(PathBuf::from("codex-home").join(relative));
        }
    }
    if sha256.len() != 64 || !sha256.bytes().all(is_lower_hex) {
        return Err("downgrade session checksum is invalid".to_string());
    }
    if kind == DowngradePackageFileKind::RecoveryPayload || thread_id.is_none() {
        return Ok(PathBuf::from("recovery/unclassified").join(format!("{}.jsonl", &sha256[..32])));
    }
    let thread = safe_path_component(thread_id.unwrap_or("unknown"));
    let root = match kind {
        DowngradePackageFileKind::ActiveSession => {
            PathBuf::from("codex-home/sessions/downgrade-imported")
        }
        DowngradePackageFileKind::ArchivedSession => {
            PathBuf::from("codex-home/archived_sessions/downgrade-imported")
        }
        DowngradePackageFileKind::ConflictBranch => PathBuf::from("recovery/conflicts"),
        _ => return Err("downgrade session package kind is invalid".to_string()),
    };
    Ok(root.join(thread).join(format!("{}.jsonl", &sha256[..32])))
}

fn validate_account_database_references(
    state_db: &Path,
    sessions: &[DowngradeSessionExportPlan],
) -> Result<(), String> {
    let mapping = sessions
        .iter()
        .map(|session| (path_key(&session.source_path), session))
        .collect::<BTreeMap<_, _>>();
    let connection = Connection::open_with_flags(
        state_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| "failed to open canonical Account database".to_string())?;
    quick_check_connection(&connection, "canonical Account database")?;
    let mut statement = connection
        .prepare("SELECT id, rollout_path FROM threads WHERE rollout_path IS NOT NULL")
        .map_err(|_| "canonical Account threads schema is unsupported".to_string())?;
    let mut rows = statement
        .query([])
        .map_err(|_| "failed to read canonical Account references".to_string())?;
    while let Some(row) = rows
        .next()
        .map_err(|_| "failed to read canonical Account references".to_string())?
    {
        let thread_id: String = row
            .get(0)
            .map_err(|_| "canonical Account thread identity is invalid".to_string())?;
        let rollout_path: String = row
            .get(1)
            .map_err(|_| "canonical Account rollout path is invalid".to_string())?;
        let source = PathBuf::from(rollout_path);
        let session = mapping
            .get(&path_key(&source))
            .ok_or_else(|| "canonical Account references a missing session body".to_string())?;
        if session.logical_thread_id.as_deref() != Some(thread_id.as_str()) {
            return Err("canonical Account reference thread identity is mismatched".to_string());
        }
    }
    Ok(())
}

fn projected_export_bytes(
    codex_home: &Path,
    data_root: &Path,
    sqlite_home: &Path,
    sessions: &[DowngradeSessionExportPlan],
) -> Result<u64, String> {
    let unique_session_bytes = sessions
        .iter()
        .fold(BTreeMap::<String, u64>::new(), |mut entries, session| {
            entries
                .entry(relative_path_key(&session.output_relative_path))
                .or_insert(session.bytes);
            entries
        })
        .values()
        .try_fold(0_u64, |sum, bytes| sum.checked_add(*bytes))
        .ok_or_else(|| "downgrade export capacity calculation overflowed".to_string())?;
    let mut total = unique_session_bytes
        .checked_mul(2)
        .ok_or_else(|| "downgrade export capacity calculation overflowed".to_string())?;
    for path in [codex_home.join("auth.json"), codex_home.join("config.toml")] {
        total = total
            .checked_add(regular_file_len(&path)?)
            .ok_or_else(|| "downgrade export capacity calculation overflowed".to_string())?;
    }
    for name in ["state_5.sqlite", "logs_2.sqlite", "memories_1.sqlite"] {
        let path = sqlite_home.join(name);
        if path.is_file() {
            let multiplier = if name == "state_5.sqlite" { 2 } else { 1 };
            total = total
                .checked_add(
                    regular_file_len(&path)?
                        .checked_mul(multiplier)
                        .ok_or_else(|| {
                            "downgrade export capacity calculation overflowed".to_string()
                        })?,
                )
                .ok_or_else(|| "downgrade export capacity calculation overflowed".to_string())?;
        }
    }
    let runtimes = data_root.join("runtimes");
    if runtimes.is_dir() {
        for entry in WalkDir::new(&runtimes).max_depth(3).follow_links(false) {
            let entry = entry.map_err(|_| "failed to inspect stored runtime slots".to_string())?;
            if entry.file_type().is_file() {
                total = total
                    .checked_add(
                        entry
                            .metadata()
                            .map_err(|_| "failed to inspect stored runtime slot".to_string())?
                            .len(),
                    )
                    .ok_or_else(|| {
                        "downgrade export capacity calculation overflowed".to_string()
                    })?;
            }
        }
    }
    let goals_discovery = discover_database_catalog(codex_home, data_root);
    if goals_discovery.errors != 0 || goals_discovery.goals_errors != 0 {
        return Err("downgrade goals database inventory is incomplete".to_string());
    }
    let goals_source_bytes = goals_discovery
        .goals_descriptors
        .iter()
        .map(|descriptor| regular_file_len(&descriptor.source_path))
        .try_fold(0_u64, |sum, bytes| {
            sum.checked_add(bytes?)
                .ok_or_else(|| "downgrade export capacity calculation overflowed".to_string())
        })?;
    total = total
        .checked_add(
            goals_source_bytes
                .checked_mul(3)
                .ok_or_else(|| "downgrade export capacity calculation overflowed".to_string())?,
        )
        .ok_or_else(|| "downgrade export capacity calculation overflowed".to_string())?;
    Ok(total)
}

fn downgrade_plan_path(data_root: &Path, operation_id: &str) -> Result<PathBuf, String> {
    validate_operation_id(operation_id)?;
    Ok(data_root
        .join("session-storage-v1/operations")
        .join(operation_id)
        .join("downgrade-export-plan.json"))
}

fn plan_digest(plan: &DowngradeExportPlan) -> Result<String, String> {
    let bytes = serde_json::to_vec(plan)
        .map_err(|_| "failed to encode downgrade export plan".to_string())?;
    Ok(hex_digest(Sha256::digest(bytes)))
}

fn validate_plan(plan: &DowngradeExportPlan) -> Result<(), String> {
    if plan.schema_version != DOWNGRADE_SCHEMA_VERSION {
        return Err("downgrade export plan version is unsupported".to_string());
    }
    validate_operation_id(&plan.operation_id)?;
    let expected_target = downgrade_target_contract(&plan.target.version)?;
    if plan.target != expected_target
        || !plan.canonical_root.is_absolute()
        || !plan.data_root.is_absolute()
        || !plan.account_sqlite_home.is_absolute()
        || !plan.destination_root.is_absolute()
        || !plan.staging_dir.is_absolute()
        || !plan.package_dir.is_absolute()
        || plan.staging_dir.parent() != Some(plan.destination_root.as_path())
        || plan.package_dir.parent() != Some(plan.destination_root.as_path())
        || plan.account_database_id.is_empty()
        || plan.account_database_id.len() > 160
        || !plan
            .account_database_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        || plan.account_catalog_fingerprint.len() != 64
        || !plan.account_catalog_fingerprint.bytes().all(is_lower_hex)
        || plan.canonical_inventory_fingerprint.len() != 64
        || !plan
            .canonical_inventory_fingerprint
            .bytes()
            .all(is_lower_hex)
        || plan.source_inventory_fingerprint.len() != 64
        || !plan.source_inventory_fingerprint.bytes().all(is_lower_hex)
        || plan.available_bytes < plan.required_available_bytes
        || plan.sessions.is_empty()
    {
        return Err("downgrade export plan is invalid".to_string());
    }
    validate_operation_id(&plan.canonical_migration_operation_id)
        .map_err(|_| "downgrade export plan is invalid".to_string())?;
    let mut sources = BTreeSet::new();
    let mut outputs = BTreeMap::<String, String>::new();
    for session in &plan.sessions {
        if !session.source_path.is_absolute()
            || session.sha256.len() != 64
            || !session.sha256.bytes().all(is_lower_hex)
        {
            return Err("downgrade session plan is invalid".to_string());
        }
        validate_relative_path(&session.output_relative_path)?;
        if !sources.insert(path_key(&session.source_path)) {
            return Err("downgrade session plan has duplicate sources".to_string());
        }
        match outputs.insert(
            relative_path_key(&session.output_relative_path),
            session.sha256.clone(),
        ) {
            Some(existing) if existing != session.sha256 => {
                return Err("downgrade session plan has conflicting outputs".to_string())
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_non_overlapping_roots(
    managed_roots: &[&Path],
    destination_root: &Path,
) -> Result<(), String> {
    let destination = fs::canonicalize(destination_root)
        .map_err(|_| "downgrade destination is unavailable".to_string())?;
    for root in managed_roots {
        let managed = fs::canonicalize(root)
            .map_err(|_| "managed downgrade source root is unavailable".to_string())?;
        if destination.starts_with(&managed) || managed.starts_with(&destination) {
            return Err("downgrade destination overlaps managed storage".to_string());
        }
    }
    Ok(())
}

fn validate_session_source_overlap(
    destination_root: &Path,
    sessions: &[DowngradeSessionExportPlan],
) -> Result<(), String> {
    validate_session_source_paths_overlap(
        destination_root,
        sessions.iter().map(|session| session.source_path.as_path()),
    )
}

fn validate_inventory_session_source_overlap(
    destination_root: &Path,
    inventory: &MigrationInventory,
) -> Result<(), String> {
    validate_session_source_paths_overlap(
        destination_root,
        inventory.files.iter().map(|file| file.path.as_path()),
    )
}

fn validate_session_source_paths_overlap<'a>(
    destination_root: &Path,
    sources: impl IntoIterator<Item = &'a Path>,
) -> Result<(), String> {
    let destination = fs::canonicalize(destination_root)
        .map_err(|_| "downgrade destination is unavailable".to_string())?;
    for source_path in sources {
        let source = fs::canonicalize(source_path)
            .map_err(|_| "downgrade session source is unavailable".to_string())?;
        if source.starts_with(&destination) {
            return Err("downgrade destination overlaps a session source".to_string());
        }
        if let Some(storage_root) = session_storage_root(&source) {
            if destination.starts_with(&storage_root) || storage_root.starts_with(&destination) {
                return Err("downgrade destination overlaps a session storage root".to_string());
            }
        }
    }
    Ok(())
}

fn session_storage_root(path: &Path) -> Option<PathBuf> {
    if let Some(root) = path.ancestors().find_map(|ancestor| {
        let name = ancestor.file_name()?.to_string_lossy();
        if name.eq_ignore_ascii_case("sessions") || name.eq_ignore_ascii_case("archived_sessions") {
            ancestor.parent().map(Path::to_path_buf)
        } else {
            None
        }
    }) {
        return Some(root);
    }

    // Relocated Codex rollouts may be referenced by an absolute database path
    // without a literal `sessions` component. Their stable layout still ends
    // in `<root>/<yyyy>/<mm>/<dd>/rollout-*.jsonl`; bind the export overlap
    // check to that root instead of treating only the individual file as
    // managed storage.
    let file_name = path.file_name()?.to_string_lossy();
    if !file_name.starts_with("rollout-") || !file_name.ends_with(".jsonl") {
        return None;
    }
    let day = path.parent()?;
    let month = day.parent()?;
    let year = month.parent()?;
    let storage_root = year.parent()?;
    let numeric_component = |component: &Path, min: u32, max: u32, width: usize| {
        let value = component.file_name()?.to_string_lossy();
        if value.len() != width || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        value
            .parse::<u32>()
            .ok()
            .filter(|number| (min..=max).contains(number))
    };
    numeric_component(day, 1, 31, 2)?;
    numeric_component(month, 1, 12, 2)?;
    numeric_component(year, 2000, 9999, 4)?;
    Some(storage_root.to_path_buf())
}

fn validate_absolute_directory(path: &Path, label: &str) -> Result<(), String> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(format!("{label} path is invalid"));
    }
    let metadata = fs::symlink_metadata(path).map_err(|_| format!("{label} is unavailable"))?;
    if !metadata.is_dir() || metadata_is_link_or_reparse(&metadata) {
        return Err(format!("{label} is unsafe"));
    }
    Ok(())
}

fn validate_relative_path(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::Prefix(_)
                    | Component::RootDir
                    | Component::CurDir
                    | Component::ParentDir
            )
        })
    {
        return Err("downgrade package path is invalid".to_string());
    }
    Ok(())
}

fn validate_operation_id(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 160
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("downgrade operation id is invalid".to_string());
    }
    Ok(())
}

fn regular_file_len(path: &Path) -> Result<u64, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| "downgrade source file is unavailable".to_string())?;
    if !metadata.is_file() || metadata_is_link_or_reparse(&metadata) || metadata.len() == 0 {
        return Err("downgrade source file is invalid".to_string());
    }
    Ok(metadata.len())
}

fn safe_path_component(value: &str) -> String {
    let value = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if value.is_empty() {
        "unknown".to_string()
    } else {
        value
    }
}

fn relative_path_key(path: &Path) -> String {
    let normalized = path.to_string_lossy().replace('/', "\\");
    if cfg!(windows) {
        normalized.to_ascii_lowercase()
    } else {
        normalized
    }
}

fn quick_check_connection(connection: &Connection, label: &str) -> Result<(), String> {
    let result: String = connection
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(|_| format!("failed to verify {label}"))?;
    if result == "ok" {
        Ok(())
    } else {
        Err(format!("{label} failed quick_check"))
    }
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn is_lower_hex(byte: u8) -> bool {
    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
}

#[cfg(windows)]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, fs, path::Path};

    use rusqlite::Connection;
    use sha2::{Digest, Sha256};
    use tempfile::tempdir;

    use super::{
        downgrade_target_contract, execute_downgrade_export, quick_check_sqlite,
        receipt_from_manifest, recover_interrupted_downgrade_export, snapshot_sqlite,
        validate_manifest, verify_downgrade_package, verify_downgrade_package_with_runtime,
        write_package_marker, DowngradeCompatibilityBand, DowngradeExportPlan,
        DowngradeRecoveryStatus, DowngradeTargetContract,
    };
    use crate::runtime_store::{RuntimeStore, PLUS_RUNTIME_ID};
    use crate::session_storage::migration::{persist_migration_preflight, run_migration_preflight};
    use crate::session_storage::migration_backup::{
        MigrationBackupManifest, MigrationBackupRuntimeVerifier, MigrationRuntimeVerification,
    };
    use crate::session_storage::operation_ledger::{
        OperationLedgerStore, SessionStorageOperationKind, SessionStorageOperationPhase,
    };
    use crate::session_storage::storage_state::{
        finalize_canonical_storage_state, load_committed_canonical_storage_state,
        prepare_canonical_storage_state, CanonicalStorageState,
    };

    const THREAD_ID: &str = "11111111-1111-4111-8111-111111111111";

    fn prepare_downgrade_export(
        codex_home: &Path,
        data_root: &Path,
        destination_root: &Path,
        operation_id: &str,
        target_version: &str,
    ) -> Result<DowngradeExportPlan, String> {
        ensure_committed_certificate(codex_home, data_root);
        super::prepare_downgrade_export(
            codex_home,
            data_root,
            destination_root,
            operation_id,
            target_version,
        )
    }

    fn ensure_committed_certificate(
        canonical_root: &Path,
        data_root: &Path,
    ) -> CanonicalStorageState {
        if let Some(state) =
            load_committed_canonical_storage_state(data_root, canonical_root).unwrap()
        {
            return state;
        }
        let operation_id = "migration-downgrade-fixture";
        let backup = tempdir().unwrap();
        let store = OperationLedgerStore::new(data_root);
        store
            .create(
                operation_id,
                SessionStorageOperationKind::Migration,
                canonical_root,
            )
            .unwrap();
        let report =
            run_migration_preflight(canonical_root, data_root, operation_id, backup.path())
                .unwrap();
        assert!(report.ready_for_backup, "{:?}", report.blockers);
        persist_migration_preflight(data_root, &report).unwrap();
        store
            .update(operation_id, |ledger| {
                ledger.backup_root = Some(report.backup_destination.join(operation_id));
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
        prepare_canonical_storage_state(
            data_root,
            canonical_root,
            operation_id,
            &report.plan.inventory_fingerprint,
        )
        .unwrap();
        store
            .transition(operation_id, SessionStorageOperationPhase::Committed)
            .unwrap();
        finalize_canonical_storage_state(data_root, canonical_root, operation_id).unwrap()
    }

    struct FixtureRuntimeVerifier;

    impl MigrationBackupRuntimeVerifier for FixtureRuntimeVerifier {
        fn verify(
            &self,
            isolated_root: &Path,
            manifest: &MigrationBackupManifest,
        ) -> Result<MigrationRuntimeVerification, String> {
            let expected = manifest
                .entries
                .iter()
                .filter_map(|entry| entry.logical_thread_id.clone())
                .collect::<BTreeSet<_>>()
                .len();
            assert!(isolated_root.join("canonical-state_5.sqlite").is_file());
            assert!(manifest
                .entries
                .iter()
                .filter(|entry| entry.logical_thread_id.is_some())
                .all(|entry| isolated_root.join(&entry.payload_relative_path).is_file()));
            Ok(MigrationRuntimeVerification {
                expected_session_count: expected,
                listed_session_count: expected,
                resumed_session_count: expected,
                continued_session_count: usize::from(expected > 0),
                tool_session_count: 0,
                tool_round_trip_verified: true,
                available_categories: vec!["ordinary".to_string()],
                continued_categories: vec!["ordinary".to_string()],
                conflict_payload_count: 0,
                conflict_payloads_verified: true,
                conflict_proofs: Vec::new(),
                capability_conflict_proof: None,
                runtime_binary_identity: None,
                verified_at_ms: u128::MAX,
            })
        }
    }

    fn expected(
        version: &str,
        band: DowngradeCompatibilityBand,
        bundle: bool,
        index: bool,
        view: bool,
    ) -> DowngradeTargetContract {
        DowngradeTargetContract {
            version: version.to_string(),
            band,
            runtime_bundle_required: bundle,
            incremental_index_required: index,
            relay_session_view_supported: view,
            mobile_continuity_required: view,
        }
    }

    #[test]
    fn target_contract_covers_every_supported_exact_version() {
        let cases = [
            ("v0.2.0", DowngradeCompatibilityBand::A, false, false, false),
            ("0.2.1", DowngradeCompatibilityBand::A, true, false, false),
            ("v0.2.2", DowngradeCompatibilityBand::B, true, false, false),
            ("v0.2.3", DowngradeCompatibilityBand::B, true, true, false),
            ("v0.2.4", DowngradeCompatibilityBand::C, true, true, true),
            ("v0.2.5", DowngradeCompatibilityBand::C, true, true, true),
            ("v0.2.6", DowngradeCompatibilityBand::C, true, true, true),
            ("v0.2.7", DowngradeCompatibilityBand::C, true, true, true),
        ];
        for (raw, band, bundle, index, view) in cases {
            let canonical = format!("v{}", raw.trim_start_matches('v'));
            assert_eq!(
                downgrade_target_contract(raw).unwrap(),
                expected(&canonical, band, bundle, index, view),
                "{raw}"
            );
        }
    }

    #[test]
    fn target_contract_rejects_ranges_prereleases_and_unsupported_versions() {
        for raw in ["0.1.9", "0.2.8", "0.3.0", "0.2.7-rc.1", "0.2", "latest", ""] {
            assert!(downgrade_target_contract(raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn downgrade_prepare_requires_the_committed_canonical_certificate() {
        let source = tempdir().unwrap();
        let data = tempdir().unwrap();
        let destination = tempdir().unwrap();
        create_profile(source.path(), "canonical");

        let error = super::prepare_downgrade_export(
            source.path(),
            data.path(),
            destination.path(),
            "downgrade-missing-certificate",
            "v0.2.7",
        )
        .unwrap_err();

        assert!(
            error.contains("committed canonical storage migration"),
            "{error}"
        );
        assert_eq!(fs::read_dir(destination.path()).unwrap().count(), 0);
    }

    #[test]
    fn v2_relay_runtime_uses_the_certificate_bound_account_catalog() {
        let root = tempdir().unwrap();
        let source = root.path().join("canonical");
        let data = root.path().join("data");
        let destination = root.path().join("destination");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&destination).unwrap();
        let session = create_profile(&source, "canonical");
        let certificate = ensure_committed_certificate(&source, &data);
        let relay = source.join(".codex-switch-session-views/relay-v2");
        fs::create_dir_all(&relay).unwrap();
        create_state_db(&relay.join("state_5.sqlite"), &session);
        write_v2_session_view(&data, &source);
        write_sqlite_home_config(&source, &relay);

        let plan = super::prepare_downgrade_export(
            &source,
            &data,
            &destination,
            "downgrade-v2-relay-account",
            "v0.2.7",
        )
        .unwrap();

        assert_eq!(plan.account_sqlite_home, source);
        assert_eq!(
            plan.canonical_migration_operation_id,
            certificate.migration_operation_id
        );
        assert_eq!(
            plan.canonical_inventory_fingerprint,
            certificate.inventory_fingerprint
        );
        assert_eq!(plan.account_catalog_fingerprint.len(), 64);
        assert!(plan.account_database_id.starts_with("db-"));
    }

    #[test]
    fn stale_view_account_cannot_override_the_newer_current_retained_session() {
        let root = tempdir().unwrap();
        let source = root.path().join("canonical");
        let data = root.path().join("data");
        let destination = root.path().join("destination");
        let stale = root.path().join("stale-account");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&destination).unwrap();
        let canonical_session = create_profile(&source, "base");
        ensure_committed_certificate(&source, &data);
        write_session_messages(&canonical_session, &["base", "newer-tail"]);

        let stale_session = stale.join("sessions/2026/08/12/rollout-stale.jsonl");
        fs::create_dir_all(stale_session.parent().unwrap()).unwrap();
        write_session_messages(&stale_session, &["base"]);
        create_state_db(&stale.join("state_5.sqlite"), &stale_session);
        let stale_relay = stale.join(".codex-switch-session-views/relay-v2");
        fs::create_dir_all(&stale_relay).unwrap();
        create_state_db(&stale_relay.join("state_5.sqlite"), &stale_session);
        write_v2_session_view(&data, &stale);
        write_sqlite_home_config(&source, &stale_relay);

        let error = super::prepare_downgrade_export(
            &source,
            &data,
            &destination,
            "downgrade-stale-account",
            "v0.2.7",
        )
        .unwrap_err();

        assert!(
            error.contains("missing a certificate-retained session reference"),
            "{error}"
        );
        assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
    }

    #[test]
    fn stale_v1_account_cannot_override_the_newer_current_retained_session() {
        let root = tempdir().unwrap();
        let source = root.path().join("canonical");
        let data = root.path().join("data");
        let destination = root.path().join("destination");
        let stale = root.path().join("stale-v1-account");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&destination).unwrap();
        let canonical_session = create_profile(&source, "base");
        ensure_committed_certificate(&source, &data);
        write_session_messages(&canonical_session, &["base", "newer-tail"]);

        let stale_session = stale.join("sessions/2026/08/12/rollout-stale.jsonl");
        fs::create_dir_all(stale_session.parent().unwrap()).unwrap();
        write_session_messages(&stale_session, &["base"]);
        create_state_db(&stale.join("state_5.sqlite"), &stale_session);
        let relay = data.join("relay-sqlite");
        fs::create_dir_all(&relay).unwrap();
        create_state_db(&relay.join("state_5.sqlite"), &stale_session);
        write_v1_session_view(&data, &stale);

        let error = super::prepare_downgrade_export(
            &source,
            &data,
            &destination,
            "downgrade-stale-v1-account",
            "v0.2.7",
        )
        .unwrap_err();

        assert!(
            error.contains("conflicts with the current runtime catalog"),
            "{error}"
        );
        assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
    }

    #[test]
    fn current_legacy_relay_without_v2_account_identity_fails_closed() {
        let root = tempdir().unwrap();
        let source = root.path().join("canonical");
        let data = root.path().join("data");
        let destination = root.path().join("destination");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&destination).unwrap();
        let session = create_profile(&source, "canonical");
        ensure_committed_certificate(&source, &data);
        let relay = data.join("relay-sqlite");
        fs::create_dir_all(&relay).unwrap();
        create_state_db(&relay.join("state_5.sqlite"), &session);
        write_v1_session_view(&data, &source);
        write_sqlite_home_config(&source, &relay);

        let error = super::prepare_downgrade_export(
            &source,
            &data,
            &destination,
            "downgrade-legacy-relay",
            "v0.2.7",
        )
        .unwrap_err();

        assert!(error.contains("legacy Relay"), "{error}");
        assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
    }

    #[test]
    fn conflicting_v1_and_v2_account_descriptors_fail_closed() {
        let root = tempdir().unwrap();
        let source = root.path().join("canonical");
        let data = root.path().join("data");
        let destination = root.path().join("destination");
        let other_account = root.path().join("other-account");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&destination).unwrap();
        let session = create_profile(&source, "canonical");
        ensure_committed_certificate(&source, &data);
        fs::create_dir_all(&other_account).unwrap();
        create_state_db(&other_account.join("state_5.sqlite"), &session);
        let v2_relay = source.join(".codex-switch-session-views/relay-v2");
        let v1_relay = data.join("relay-sqlite");
        fs::create_dir_all(&v2_relay).unwrap();
        fs::create_dir_all(&v1_relay).unwrap();
        create_state_db(&v2_relay.join("state_5.sqlite"), &session);
        create_state_db(&v1_relay.join("state_5.sqlite"), &session);
        write_v2_session_view(&data, &source);
        write_v1_session_view(&data, &other_account);

        let error = super::prepare_downgrade_export(
            &source,
            &data,
            &destination,
            "downgrade-view-conflict",
            "v0.2.7",
        )
        .unwrap_err();

        assert!(error.contains("v1 and v2"), "{error}");
        assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
    }

    #[test]
    fn wal_source_snapshot_is_self_contained_and_read_verification_creates_no_sidecars() {
        let root = tempdir().unwrap();
        let source = root.path().join("source.sqlite");
        let target = root.path().join("target.sqlite");
        let source_connection = Connection::open(&source).unwrap();
        let journal_mode: String = source_connection
            .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
            .unwrap();
        assert_eq!(journal_mode.to_ascii_lowercase(), "wal");
        source_connection
            .execute_batch("CREATE TABLE fixture (id INTEGER PRIMARY KEY); INSERT INTO fixture DEFAULT VALUES;")
            .unwrap();

        snapshot_sqlite(&source, &target).unwrap();
        assert!(!root.path().join("target.sqlite-wal").exists());
        assert!(!root.path().join("target.sqlite-shm").exists());
        quick_check_sqlite(&target).unwrap();
        assert!(!root.path().join("target.sqlite-wal").exists());
        assert!(!root.path().join("target.sqlite-shm").exists());

        let target_connection = Connection::open(&target).unwrap();
        let target_mode: String = target_connection
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(target_mode.to_ascii_lowercase(), "delete");
        assert_eq!(
            target_connection
                .query_row("SELECT COUNT(*) FROM fixture", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn downgrade_manifest_rejects_sqlite_sidecars() {
        let source = tempdir().unwrap();
        let data = tempdir().unwrap();
        let destination = tempdir().unwrap();
        create_profile(source.path(), "canonical");
        let plan = prepare_downgrade_export(
            source.path(),
            data.path(),
            destination.path(),
            "downgrade-sidecar-reject",
            "v0.2.7",
        )
        .unwrap();
        let receipt = execute_downgrade_export(&plan, || Ok(())).unwrap();
        let mut manifest = verify_downgrade_package(&receipt.package_dir).unwrap();
        let mut sidecar = manifest.entries[0].clone();
        sidecar.relative_path = "codex-home/state_5.sqlite-wal".into();
        manifest.entries.push(sidecar);

        assert!(validate_manifest(&manifest)
            .unwrap_err()
            .contains("SQLite sidecar"));
    }

    #[test]
    fn isolated_export_rewrites_references_preserves_conflict_and_keeps_source_unchanged() {
        let source = tempdir().unwrap();
        let data = tempdir().unwrap();
        let destination = tempdir().unwrap();
        let canonical = create_profile(source.path(), "canonical");
        create_shared_conflict(data.path(), "shared-tail");
        let source_session = source
            .path()
            .join("sessions/2026/08/12/rollout-source.jsonl");
        let source_bytes = fs::read(&source_session).unwrap();
        let source_config = fs::read(source.path().join("config.toml")).unwrap();

        let plan = prepare_downgrade_export(
            source.path(),
            data.path(),
            destination.path(),
            "downgrade-fixture-1",
            "v0.2.4",
        )
        .unwrap();
        let receipt = execute_downgrade_export(&plan, || Ok(())).unwrap();
        let manifest = verify_downgrade_package(&receipt.package_dir).unwrap();

        assert!(receipt.structurally_verified);
        assert!(receipt.target_runtime_verification_required);
        assert_eq!(receipt.logical_session_count, 1);
        assert_eq!(receipt.conflict_branch_count, 1);
        assert!(manifest.target.incremental_index_required);
        assert!(manifest.target.mobile_continuity_required);
        assert!(receipt
            .package_dir
            .join("appdata/codex-switch/session-sync-state-v1.json")
            .is_file());
        assert!(receipt
            .package_dir
            .join("appdata/codex-switch/mobile-continuity-v1.json")
            .is_file());
        assert!(!receipt
            .package_dir
            .join("appdata/codex-switch/request-route-session-view-v1.json")
            .exists());
        assert!(!receipt
            .package_dir
            .join("appdata/codex-switch/relay-sqlite")
            .exists());

        let exported =
            Connection::open(receipt.package_dir.join("codex-home/state_5.sqlite")).unwrap();
        let rollout: String = exported
            .query_row(
                "SELECT rollout_path FROM threads WHERE id = ?1",
                [THREAD_ID],
                |row| row.get(0),
            )
            .unwrap();
        assert!(Path::new(&rollout).starts_with(receipt.package_dir.join("codex-home")));
        assert_eq!(fs::read(&source_session).unwrap(), source_bytes);
        assert_eq!(
            fs::read(source.path().join("config.toml")).unwrap(),
            source_config
        );
        assert_eq!(canonical, source_session);
        assert!(
            !fs::read_to_string(receipt.package_dir.join("codex-home/config.toml"))
                .unwrap()
                .contains("sqlite_home")
        );
        let readme = fs::read_to_string(receipt.package_dir.join("README.txt")).unwrap();
        assert!(readme.contains("bound to the directory where it was generated"));
        assert!(readme.contains("do not move it"));
    }

    #[test]
    fn downgrade_export_unions_all_runtime_goals_and_deferrals_exactly() {
        let source = tempdir().unwrap();
        let data = tempdir().unwrap();
        let destination = tempdir().unwrap();
        let session = create_profile(source.path(), "canonical");
        let relay = data.path().join("relay-sqlite");
        let shared = data.path().join("shared-sessions");
        fs::create_dir_all(&relay).unwrap();
        fs::create_dir_all(&shared).unwrap();
        create_state_db(&relay.join("state_5.sqlite"), &session);
        create_state_db(&shared.join("state_5.sqlite"), &session);

        let account_goals = source.path().join("goals_1.sqlite");
        let relay_goals = relay.join("goals_1.sqlite");
        let shared_goals = shared.join("goals_1.sqlite");
        create_goals_db(&account_goals);
        create_goals_db(&relay_goals);
        create_goals_db(&shared_goals);
        insert_goal(&account_goals, "account-only", "account", true);
        let relay_connection = Connection::open(&relay_goals).unwrap();
        let relay_journal: String = relay_connection
            .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
            .unwrap();
        assert_eq!(relay_journal.to_ascii_lowercase(), "wal");
        relay_connection
            .execute(
                "INSERT INTO thread_goals
                 (thread_id, goal_id, objective, status, token_budget, tokens_used,
                  time_used_seconds, created_at_ms, updated_at_ms)
                 VALUES ('relay-only', 'goal-relay-only', 'relay', 'active', NULL, 0, 0, 1, 1)",
                [],
            )
            .unwrap();
        insert_goal(&shared_goals, "shared-only", "shared", true);
        let before = [&account_goals, &relay_goals, &shared_goals].map(|path| {
            let connection = Connection::open(path).unwrap();
            crate::session_storage::catalog::goals_database_digest(&connection).unwrap()
        });

        let plan = prepare_downgrade_export(
            source.path(),
            data.path(),
            destination.path(),
            "downgrade-global-goals-union",
            "v0.2.7",
        )
        .unwrap();
        let receipt = execute_downgrade_export(&plan, || Ok(())).unwrap();
        let manifest = verify_downgrade_package(&receipt.package_dir).unwrap();
        let exported_path = receipt.package_dir.join("codex-home/goals_1.sqlite");
        assert!(manifest.entries.iter().any(|entry| {
            entry.relative_path == Path::new("codex-home/goals_1.sqlite")
                && entry.kind == super::DowngradePackageFileKind::AuxiliaryDatabase
        }));
        let exported = Connection::open(exported_path).unwrap();
        let mut goals_statement = exported
            .prepare("SELECT thread_id, goal_id, objective FROM thread_goals")
            .unwrap();
        let goals = goals_statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .unwrap()
            .collect::<Result<BTreeSet<_>, _>>()
            .unwrap();
        assert_eq!(
            goals,
            BTreeSet::from([
                (
                    "account-only".to_string(),
                    "goal-account-only".to_string(),
                    "account".to_string(),
                ),
                (
                    "relay-only".to_string(),
                    "goal-relay-only".to_string(),
                    "relay".to_string(),
                ),
                (
                    "shared-only".to_string(),
                    "goal-shared-only".to_string(),
                    "shared".to_string(),
                ),
            ])
        );
        let mut deferrals_statement = exported
            .prepare("SELECT thread_id FROM thread_goal_continuation_deferrals")
            .unwrap();
        let deferrals = deferrals_statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<BTreeSet<_>, _>>()
            .unwrap();
        assert_eq!(
            deferrals,
            BTreeSet::from(["account-only".to_string(), "shared-only".to_string()])
        );
        drop(deferrals_statement);
        drop(goals_statement);
        drop(exported);
        drop(relay_connection);
        assert_eq!(
            [&account_goals, &relay_goals, &shared_goals].map(|path| {
                let connection = Connection::open(path).unwrap();
                crate::session_storage::catalog::goals_database_digest(&connection).unwrap()
            }),
            before
        );
    }

    #[test]
    fn downgrade_export_without_any_goals_history_keeps_goals_absent() {
        let source = tempdir().unwrap();
        let data = tempdir().unwrap();
        let destination = tempdir().unwrap();
        create_profile(source.path(), "canonical");
        let plan = prepare_downgrade_export(
            source.path(),
            data.path(),
            destination.path(),
            "downgrade-no-goals-history",
            "v0.2.7",
        )
        .unwrap();
        let receipt = execute_downgrade_export(&plan, || Ok(())).unwrap();
        let manifest = verify_downgrade_package(&receipt.package_dir).unwrap();
        assert!(!receipt
            .package_dir
            .join("codex-home/goals_1.sqlite")
            .exists());
        assert!(!manifest
            .entries
            .iter()
            .any(|entry| entry.relative_path == Path::new("codex-home/goals_1.sqlite")));

        create_goals_db(&receipt.package_dir.join("codex-home/goals_1.sqlite"));
        let error = verify_downgrade_package(&receipt.package_dir).unwrap_err();
        assert!(error.contains("untracked payload"), "{error}");
    }

    #[test]
    fn downgrade_prepare_rejects_goals_schema_drift_before_destination_write() {
        let source = tempdir().unwrap();
        let data = tempdir().unwrap();
        let destination = tempdir().unwrap();
        create_profile(source.path(), "canonical");
        let goals_path = source.path().join("goals_1.sqlite");
        create_goals_db(&goals_path);
        ensure_committed_certificate(source.path(), data.path());
        Connection::open(&goals_path)
            .unwrap()
            .execute_batch("DROP TABLE thread_goal_continuation_deferrals;")
            .unwrap();

        let error = super::prepare_downgrade_export(
            source.path(),
            data.path(),
            destination.path(),
            "downgrade-goals-schema-drift",
            "v0.2.7",
        )
        .unwrap_err();
        assert!(error.contains("inventory is incomplete"), "{error}");
        assert_eq!(fs::read_dir(destination.path()).unwrap().count(), 0);
    }

    #[test]
    fn downgrade_package_declared_goals_must_pass_exact_schema_validation() {
        let source = tempdir().unwrap();
        let data = tempdir().unwrap();
        let destination = tempdir().unwrap();
        create_profile(source.path(), "canonical");
        let source_goals = source.path().join("goals_1.sqlite");
        create_goals_db(&source_goals);
        insert_goal(&source_goals, "declared-goal", "declared", true);
        let plan = prepare_downgrade_export(
            source.path(),
            data.path(),
            destination.path(),
            "downgrade-declared-goals-schema",
            "v0.2.7",
        )
        .unwrap();
        let receipt = execute_downgrade_export(&plan, || Ok(())).unwrap();
        let mut manifest = verify_downgrade_package(&receipt.package_dir).unwrap();
        let goals_path = receipt.package_dir.join("codex-home/goals_1.sqlite");
        Connection::open(&goals_path)
            .unwrap()
            .execute_batch("DROP TABLE thread_goal_continuation_deferrals;")
            .unwrap();
        let (changed_bytes, changed_sha256) = super::stable_file_digest(&goals_path).unwrap();
        let entry_index = manifest
            .entries
            .iter()
            .position(|entry| entry.relative_path == Path::new("codex-home/goals_1.sqlite"))
            .unwrap();
        let previous_bytes = manifest.entries[entry_index].bytes;
        manifest.package_bytes = manifest
            .package_bytes
            .checked_sub(previous_bytes)
            .and_then(|bytes| bytes.checked_add(changed_bytes))
            .unwrap();
        let entry = &mut manifest.entries[entry_index];
        entry.bytes = changed_bytes;
        entry.sha256 = changed_sha256;
        super::write_downgrade_manifest(&receipt.package_dir, &manifest).unwrap();

        let error = verify_downgrade_package(&receipt.package_dir).unwrap_err();
        assert!(error.contains("missing required table"), "{error}");
    }

    #[test]
    fn downgrade_export_rejects_conflicting_goals_primary_key() {
        let source = tempdir().unwrap();
        let data = tempdir().unwrap();
        let destination = tempdir().unwrap();
        let session = create_profile(source.path(), "canonical");
        let relay = data.path().join("relay-sqlite");
        fs::create_dir_all(&relay).unwrap();
        create_state_db(&relay.join("state_5.sqlite"), &session);
        let account_goals = source.path().join("goals_1.sqlite");
        let relay_goals = relay.join("goals_1.sqlite");
        create_goals_db(&account_goals);
        create_goals_db(&relay_goals);
        insert_goal(&account_goals, "same-thread", "account-objective", false);
        insert_goal(&relay_goals, "same-thread", "relay-objective", false);

        let plan = prepare_downgrade_export(
            source.path(),
            data.path(),
            destination.path(),
            "downgrade-goals-conflict",
            "v0.2.7",
        )
        .unwrap();
        let error = execute_downgrade_export(&plan, || Ok(())).unwrap_err();
        assert!(error.contains("same primary key"), "{error}");
        assert!(!plan.package_dir.exists());
    }

    #[test]
    fn native_runtime_verification_is_manifest_bound_and_cleans_its_isolated_copy() {
        let source = tempdir().unwrap();
        let data = tempdir().unwrap();
        let destination = tempdir().unwrap();
        create_profile(source.path(), "body");
        let plan = prepare_downgrade_export(
            source.path(),
            data.path(),
            destination.path(),
            "downgrade-native-runtime",
            "v0.2.7",
        )
        .unwrap();
        let receipt = execute_downgrade_export(&plan, || Ok(())).unwrap();
        let isolated_root = data.path().join("runtime-verify");

        let verified = verify_downgrade_package_with_runtime(
            &receipt.package_dir,
            &isolated_root,
            &FixtureRuntimeVerifier,
        )
        .unwrap();
        let verified_receipt = receipt_from_manifest(&receipt.package_dir, &verified);

        assert!(verified.native_runtime_verification.is_some());
        assert!(verified_receipt.native_runtime_verified);
        assert!(verified_receipt.target_runtime_verification_required);
        assert!(!isolated_root.exists());
        assert_eq!(
            verify_downgrade_package(&receipt.package_dir).unwrap(),
            verified
        );
    }

    #[test]
    fn exported_saved_runtime_slot_cannot_reinject_the_canonical_sqlite_home() {
        let root = tempdir().unwrap();
        let source = root.path().join("canonical");
        let data = root.path().join("data");
        let destination = root.path().join("destination");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&destination).unwrap();
        create_profile(&source, "body");

        let source_store = RuntimeStore::new(data.join("runtimes"));
        source_store.import_plus_from_home(&source, false).unwrap();
        let slot = source_store.runtime_dir(PLUS_RUNTIME_ID);
        let escaped = format!(
            "model = \"gpt-test\"\nsqlite_home = \"{}\"\n",
            source.to_string_lossy().replace('\\', "\\\\")
        );
        fs::write(slot.join("config.toml"), &escaped).unwrap();
        let mut bundle: serde_json::Value =
            serde_json::from_slice(&fs::read(slot.join("bundle.json")).unwrap()).unwrap();
        bundle["configSha256"] =
            serde_json::Value::String(format!("{:x}", Sha256::digest(escaped.as_bytes())));
        fs::write(
            slot.join("bundle.json"),
            serde_json::to_vec_pretty(&bundle).unwrap(),
        )
        .unwrap();
        assert!(source_store
            .load_runtime_files(PLUS_RUNTIME_ID)
            .unwrap()
            .config_toml
            .contains("sqlite_home"));

        let plan = prepare_downgrade_export(
            &source,
            &data,
            &destination,
            "downgrade-saved-slot-isolation",
            "v0.2.7",
        )
        .unwrap();
        let receipt = execute_downgrade_export(&plan, || Ok(())).unwrap();
        verify_downgrade_package(&receipt.package_dir).unwrap();
        let exported_store =
            RuntimeStore::new(receipt.package_dir.join("appdata/codex-switch/runtimes"));
        let exported = exported_store.load_runtime_files(PLUS_RUNTIME_ID).unwrap();
        assert!(!exported.config_toml.contains("sqlite_home"));
        assert!(fs::read_to_string(slot.join("config.toml"))
            .unwrap()
            .contains("sqlite_home"));
    }

    #[test]
    fn destination_overlap_rejects_an_external_account_sqlite_home() {
        let root = tempdir().unwrap();
        let source = root.path().join("canonical");
        let data = root.path().join("data");
        let external_sqlite = root.path().join("external-sqlite");
        let destination = external_sqlite.join("exports");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&destination).unwrap();
        create_profile(&source, "body");
        fs::rename(
            source.join("state_5.sqlite"),
            external_sqlite.join("state_5.sqlite"),
        )
        .unwrap();
        fs::write(
            source.join("config.toml"),
            format!(
                "model = \"gpt-test\"\nsqlite_home = \"{}\"\n",
                external_sqlite.to_string_lossy().replace('\\', "\\\\")
            ),
        )
        .unwrap();

        let error = prepare_downgrade_export(
            &source,
            &data,
            &destination,
            "downgrade-external-sqlite-overlap",
            "v0.2.7",
        )
        .unwrap_err();

        assert!(error.contains("overlaps managed storage"), "{error}");
        assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
    }

    #[test]
    fn destination_overlap_rejects_an_external_session_storage_root() {
        let root = tempdir().unwrap();
        let source = root.path().join("canonical");
        let data = root.path().join("data");
        let external_sessions = root.path().join("external-sessions");
        let destination = external_sessions.join("exports");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&destination).unwrap();
        let original = create_profile(&source, "body");
        let external = external_sessions.join("2026/08/12/rollout-external.jsonl");
        fs::create_dir_all(external.parent().unwrap()).unwrap();
        fs::rename(&original, &external).unwrap();
        Connection::open(source.join("state_5.sqlite"))
            .unwrap()
            .execute(
                "UPDATE threads SET rollout_path = ?1 WHERE id = ?2",
                (external.to_string_lossy().to_string(), THREAD_ID),
            )
            .unwrap();

        let error = prepare_downgrade_export(
            &source,
            &data,
            &destination,
            "downgrade-external-session-overlap",
            "v0.2.7",
        )
        .unwrap_err();

        assert!(error.contains("overlaps a session storage root"), "{error}");
        assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
    }

    #[test]
    fn destination_overlap_rejects_every_unplanned_inventory_session_root_relation() {
        for (relation, destination_selector) in [("exact", 0_u8), ("parent", 1_u8), ("child", 2_u8)]
        {
            let root = tempdir().unwrap();
            let source = root.path().join("canonical");
            let data = root.path().join("data");
            let external_parent = root.path().join("external-parent");
            let external_storage = external_parent.join("relocated");
            let external =
                external_storage.join("sessions/2026/08/12/rollout-unplanned-equal.jsonl");
            fs::create_dir_all(external.parent().unwrap()).unwrap();
            fs::create_dir_all(&data).unwrap();
            let canonical = create_profile(&source, "body");
            fs::copy(&canonical, &external).unwrap();

            let shared = data.join("shared-sessions");
            fs::create_dir_all(&shared).unwrap();
            create_state_db(&shared.join("state_5.sqlite"), &external);
            fs::write(shared.join("session_index.jsonl"), b"").unwrap();

            let destination = match destination_selector {
                0 => external_storage.clone(),
                1 => external_parent.clone(),
                _ => {
                    let child = external_storage.join("exports");
                    fs::create_dir_all(&child).unwrap();
                    child
                }
            };

            let error = prepare_downgrade_export(
                &source,
                &data,
                &destination,
                &format!("downgrade-unplanned-overlap-{relation}"),
                "v0.2.7",
            )
            .unwrap_err();

            assert!(
                error.contains("overlaps a session source")
                    || error.contains("overlaps a session storage root"),
                "{relation}: {error}"
            );
            assert!(!destination
                .join(format!(
                    "codex-switch-downgrade-0-2-7-downgrade-unplanned-overlap-{relation}"
                ))
                .exists());
        }
    }

    #[test]
    fn compatibility_bands_emit_only_the_bootstrap_the_target_reads() {
        for (index, target, index_required, mobile_required) in [
            (0, "v0.2.0", false, false),
            (1, "v0.2.3", true, false),
            (2, "v0.2.7", true, true),
        ] {
            let source = tempdir().unwrap();
            let data = tempdir().unwrap();
            let destination = tempdir().unwrap();
            create_profile(source.path(), &format!("body-{index}"));
            let plan = prepare_downgrade_export(
                source.path(),
                data.path(),
                destination.path(),
                &format!("downgrade-band-{index}"),
                target,
            )
            .unwrap();
            let receipt = execute_downgrade_export(&plan, || Ok(())).unwrap();
            let root = receipt.package_dir.join("appdata/codex-switch");

            assert_eq!(
                root.join("session-sync-state-v1.json").is_file(),
                index_required
            );
            assert_eq!(
                root.join("mobile-continuity-v1.json").is_file(),
                mobile_required
            );
            assert!(!root.join("relay-sqlite").exists());
            assert!(!root.join("request-route-session-view-v1.json").exists());
            verify_downgrade_package(&receipt.package_dir).unwrap();
        }
    }

    #[test]
    fn missing_account_rollout_fails_before_destination_write() {
        let source = tempdir().unwrap();
        let data = tempdir().unwrap();
        let destination = tempdir().unwrap();
        create_profile(source.path(), "body");
        let connection = Connection::open(source.path().join("state_5.sqlite")).unwrap();
        connection
            .execute(
                "UPDATE threads SET rollout_path = ?1 WHERE id = ?2",
                (
                    source
                        .path()
                        .join("sessions/missing.jsonl")
                        .to_string_lossy(),
                    THREAD_ID,
                ),
            )
            .unwrap();

        let error = prepare_downgrade_export(
            source.path(),
            data.path(),
            destination.path(),
            "downgrade-missing",
            "v0.2.7",
        )
        .unwrap_err();

        assert!(
            error.contains("missing") || error.contains("no recoverable"),
            "{error}"
        );
        assert_eq!(fs::read_dir(destination.path()).unwrap().count(), 0);
    }

    #[test]
    fn package_verification_rejects_post_export_tamper() {
        let source = tempdir().unwrap();
        let data = tempdir().unwrap();
        let destination = tempdir().unwrap();
        create_profile(source.path(), "body");
        let plan = prepare_downgrade_export(
            source.path(),
            data.path(),
            destination.path(),
            "downgrade-tamper",
            "v0.2.7",
        )
        .unwrap();
        let receipt = execute_downgrade_export(&plan, || Ok(())).unwrap();
        fs::write(
            receipt.package_dir.join("launch-codex-cli.cmd"),
            b"tampered",
        )
        .unwrap();

        assert!(verify_downgrade_package(&receipt.package_dir).is_err());
    }

    #[test]
    fn startup_recovery_removes_owned_staging_and_rolls_back() {
        let source = tempdir().unwrap();
        let data = tempdir().unwrap();
        let destination = tempdir().unwrap();
        create_profile(source.path(), "body");
        let store = OperationLedgerStore::new(data.path());
        store
            .create(
                "downgrade-recover-stage",
                SessionStorageOperationKind::DowngradeExport,
                source.path(),
            )
            .unwrap();
        store
            .transition(
                "downgrade-recover-stage",
                SessionStorageOperationPhase::Preflight,
            )
            .unwrap();
        let plan = prepare_downgrade_export(
            source.path(),
            data.path(),
            destination.path(),
            "downgrade-recover-stage",
            "v0.2.7",
        )
        .unwrap();
        store
            .transition(
                "downgrade-recover-stage",
                SessionStorageOperationPhase::Backup,
            )
            .unwrap();
        fs::create_dir(&plan.staging_dir).unwrap();
        write_package_marker(&plan.staging_dir, &plan).unwrap();
        fs::write(plan.staging_dir.join("partial.bin"), b"partial").unwrap();

        let recovery = recover_interrupted_downgrade_export(
            &store,
            data.path(),
            "downgrade-recover-stage",
            || Ok(()),
        )
        .unwrap();

        assert_eq!(recovery.status, DowngradeRecoveryStatus::RolledBack);
        assert!(recovery.staging_removed);
        assert!(!plan.staging_dir.exists());
        assert_eq!(
            store.load("downgrade-recover-stage").unwrap().phase,
            SessionStorageOperationPhase::RolledBack
        );
    }

    #[test]
    fn startup_recovery_rolls_back_a_published_package_before_runtime_verification() {
        let source = tempdir().unwrap();
        let data = tempdir().unwrap();
        let destination = tempdir().unwrap();
        create_profile(source.path(), "body");
        let operation_id = "downgrade-recover-published";
        let store = OperationLedgerStore::new(data.path());
        store
            .create(
                operation_id,
                SessionStorageOperationKind::DowngradeExport,
                source.path(),
            )
            .unwrap();
        store
            .transition(operation_id, SessionStorageOperationPhase::Preflight)
            .unwrap();
        let plan = prepare_downgrade_export(
            source.path(),
            data.path(),
            destination.path(),
            operation_id,
            "v0.2.7",
        )
        .unwrap();
        for phase in [
            SessionStorageOperationPhase::Backup,
            SessionStorageOperationPhase::BackupVerified,
            SessionStorageOperationPhase::PlanReady,
            SessionStorageOperationPhase::Applying,
        ] {
            store.transition(operation_id, phase).unwrap();
        }
        let receipt = execute_downgrade_export(&plan, || Ok(())).unwrap();
        fs::remove_file(receipt.package_dir.join("manifest.json")).unwrap();

        let recovery =
            recover_interrupted_downgrade_export(&store, data.path(), operation_id, || Ok(()))
                .unwrap();

        assert_eq!(recovery.status, DowngradeRecoveryStatus::RolledBack);
        assert!(!receipt.package_dir.exists());
        assert_eq!(
            store.load(operation_id).unwrap().phase,
            SessionStorageOperationPhase::RolledBack
        );
    }

    #[test]
    fn startup_recovery_commits_a_native_runtime_verified_package() {
        let source = tempdir().unwrap();
        let data = tempdir().unwrap();
        let destination = tempdir().unwrap();
        create_profile(source.path(), "body");
        let operation_id = "downgrade-recover-runtime-verified";
        let store = OperationLedgerStore::new(data.path());
        store
            .create(
                operation_id,
                SessionStorageOperationKind::DowngradeExport,
                source.path(),
            )
            .unwrap();
        store
            .transition(operation_id, SessionStorageOperationPhase::Preflight)
            .unwrap();
        let plan = prepare_downgrade_export(
            source.path(),
            data.path(),
            destination.path(),
            operation_id,
            "v0.2.7",
        )
        .unwrap();
        for phase in [
            SessionStorageOperationPhase::Backup,
            SessionStorageOperationPhase::BackupVerified,
            SessionStorageOperationPhase::PlanReady,
            SessionStorageOperationPhase::Applying,
        ] {
            store.transition(operation_id, phase).unwrap();
        }
        let receipt = execute_downgrade_export(&plan, || Ok(())).unwrap();
        verify_downgrade_package_with_runtime(
            &receipt.package_dir,
            &data.path().join("runtime-verify"),
            &FixtureRuntimeVerifier,
        )
        .unwrap();

        let recovery =
            recover_interrupted_downgrade_export(&store, data.path(), operation_id, || Ok(()))
                .unwrap();

        assert_eq!(recovery.status, DowngradeRecoveryStatus::Committed);
        assert!(verify_downgrade_package(&receipt.package_dir)
            .unwrap()
            .native_runtime_verification
            .is_some());
        assert_eq!(
            store.load(operation_id).unwrap().phase,
            SessionStorageOperationPhase::Committed
        );
    }

    fn create_profile(root: &Path, message: &str) -> std::path::PathBuf {
        fs::create_dir_all(root.join("sessions/2026/08/12")).unwrap();
        fs::write(
            root.join("auth.json"),
            br#"{"auth_mode":"chatgpt","tokens":{"access_token":"fixture-token"}}"#,
        )
        .unwrap();
        fs::write(
            root.join("config.toml"),
            format!(
                "model = \"gpt-test\"\nsqlite_home = \"{}\"\n",
                root.to_string_lossy().replace('\\', "\\\\")
            ),
        )
        .unwrap();
        fs::write(
            root.join("session_index.jsonl"),
            format!("{{\"id\":\"{THREAD_ID}\",\"thread_name\":\"fixture\"}}\n"),
        )
        .unwrap();
        let session = root.join("sessions/2026/08/12/rollout-source.jsonl");
        write_session(&session, message);
        create_state_db(&root.join("state_5.sqlite"), &session);
        session
    }

    fn create_shared_conflict(data_root: &Path, message: &str) {
        let shared = data_root.join("shared-sessions");
        fs::create_dir_all(shared.join("sessions/2026/08/12")).unwrap();
        let session = shared.join("sessions/2026/08/12/rollout-conflict.jsonl");
        write_session(&session, message);
        create_state_db(&shared.join("state_5.sqlite"), &session);
        fs::write(shared.join("session_index.jsonl"), b"").unwrap();
    }

    fn write_session(path: &Path, message: &str) {
        write_session_messages(path, &[message]);
    }

    fn write_session_messages(path: &Path, messages: &[&str]) {
        let mut lines = vec![
            serde_json::json!({"type":"session_meta","timestamp":"2026-08-12T00:00:00Z","payload":{"id":THREAD_ID,"model_provider":"openai"}}),
        ];
        for (index, message) in messages.iter().enumerate() {
            let seconds = index.saturating_mul(2).saturating_add(1);
            lines.push(serde_json::json!({"type":"event_msg","timestamp":format!("2026-08-12T00:00:{seconds:02}Z"),"payload":{"type":"user_message","message":message}}));
            lines.push(serde_json::json!({"type":"response_item","timestamp":format!("2026-08-12T00:00:{:02}Z", seconds.saturating_add(1)),"payload":{"type":"message","role":"assistant","content":[]}}));
        }
        let body = lines
            .iter()
            .map(|line| serde_json::to_string(line).unwrap())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(path, body).unwrap();
    }

    fn write_sqlite_home_config(codex_home: &Path, sqlite_home: &Path) {
        fs::write(
            codex_home.join("config.toml"),
            format!(
                "model = \"gpt-test\"\nsqlite_home = \"{}\"\n",
                sqlite_home.to_string_lossy().replace('\\', "\\\\")
            ),
        )
        .unwrap();
    }

    fn write_v2_session_view(data_root: &Path, account_sqlite_home: &Path) {
        let relay = account_sqlite_home.join(".codex-switch-session-views/relay-v2");
        fs::write(
            data_root.join("request-route-session-view-v2.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "version": 2,
                "accountConfiguredSqliteHome": account_sqlite_home.to_string_lossy(),
                "accountEffectiveSqliteHome": account_sqlite_home,
                "relaySqliteHome": relay,
                "lastCommonStateSha256": null,
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn write_v1_session_view(data_root: &Path, account_sqlite_home: &Path) {
        fs::write(
            data_root.join("request-route-session-view-v1.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "version": 1,
                "accountConfiguredSqliteHome": account_sqlite_home.to_string_lossy(),
                "accountEffectiveSqliteHome": account_sqlite_home,
                "relaySqliteHome": data_root.join("relay-sqlite"),
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn create_state_db(path: &Path, rollout: &Path) {
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE threads (\
                    id TEXT PRIMARY KEY,\
                    rollout_path TEXT,\
                    model_provider TEXT,\
                    archived INTEGER NOT NULL DEFAULT 0,\
                    updated_at INTEGER,\
                    updated_at_ms INTEGER\
                );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO threads (id, rollout_path, model_provider, archived, updated_at, updated_at_ms) \
                 VALUES (?1, ?2, 'openai', 0, 1, 1000)",
                (THREAD_ID, rollout.to_string_lossy().to_string()),
            )
            .unwrap();
    }

    fn create_goals_db(path: &Path) {
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 CREATE TABLE thread_goals (
                    thread_id TEXT PRIMARY KEY NOT NULL,
                    goal_id TEXT NOT NULL,
                    objective TEXT NOT NULL,
                    status TEXT NOT NULL CHECK(status IN ('active','paused','blocked','usage_limited','budget_limited','complete')),
                    token_budget INTEGER,
                    tokens_used INTEGER NOT NULL DEFAULT 0,
                    time_used_seconds INTEGER NOT NULL DEFAULT 0,
                    created_at_ms INTEGER NOT NULL,
                    updated_at_ms INTEGER NOT NULL
                 );
                 CREATE TABLE thread_goal_continuation_deferrals (
                    thread_id TEXT PRIMARY KEY NOT NULL REFERENCES thread_goals(thread_id) ON DELETE CASCADE
                 );",
            )
            .unwrap();
    }

    fn insert_goal(path: &Path, thread_id: &str, objective: &str, deferred: bool) {
        let connection = Connection::open(path).unwrap();
        connection
            .execute(
                "INSERT INTO thread_goals
                 (thread_id, goal_id, objective, status, token_budget, tokens_used,
                  time_used_seconds, created_at_ms, updated_at_ms)
                 VALUES (?1, ?2, ?3, 'active', NULL, 0, 0, 1, 1)",
                (thread_id, format!("goal-{thread_id}"), objective),
            )
            .unwrap();
        if deferred {
            connection
                .execute(
                    "INSERT INTO thread_goal_continuation_deferrals (thread_id) VALUES (?1)",
                    [thread_id],
                )
                .unwrap();
        }
    }
}

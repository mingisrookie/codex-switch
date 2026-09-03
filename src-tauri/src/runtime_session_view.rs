use std::{
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    str::FromStr,
    time::Instant,
};

use rusqlite::{types::ValueRef, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use toml_edit::DocumentMut;

use crate::{
    codex_paths::{codex_paths_with_sqlite_home, resolve_user_codex_paths, CodexPaths},
    config_patch::SqliteHomePatch,
    file_ops::{atomic_create, atomic_write},
    session_incremental::{IncrementalSessionSyncReceipt, IncrementalSessionSyncStatus},
    session_storage::{
        bounded_file::{read_regular_file_bounded, same_regular_file_identity},
        write_barrier::{
            parent_directory_identity_at_path, recover_handle_create,
            recover_handle_hardlink_create, recover_handle_replace, regular_file_identity_at_path,
            same_persisted_regular_file_identity, stage_handle_hardlink_create,
            DestructiveFileGuard, HandleCreateIdentityBindings, HandleCreatePaths,
            HandleCreateRecoveryDecision, HandleReplaceIdentityBindings, HandleReplacePaths,
            HandleReplaceRecoveryDecision, HardlinkSourceGuard, PublishedHandleCreate,
            PublishedHandleReplace, RegularFileIdentity, ResolvedHandleCreate,
            ResolvedHandleReplace, WriteExclusionGuard,
        },
    },
};

const STATE_VERSION: u32 = 2;
const MAX_STATE_BYTES: u64 = 64 * 1024;
const RELAY_PROVIDER: &str = "openai_custom";
const STATE_DATABASE: &str = "state_5.sqlite";
const GLOBAL_DATABASES: [&str; 3] = ["logs_2.sqlite", "goals_1.sqlite", "memories_1.sqlite"];
const MANAGED_VIEW_DIRECTORY: &str = ".codex-switch-session-views";
const RELAY_VIEW_DIRECTORY: &str = "relay-v2";
const TRANSITION_JOURNAL_VERSION: u32 = 2;
const TRANSITION_JOURNAL_NAME: &str = "request-route-session-view-transition-v2.json";
const TRANSITION_JOURNAL_MAGIC: &[u8] = b"CSVIEWTRANSITION2\0";
const MAX_TRANSITION_JOURNAL_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistedFileIdentity {
    volume_serial_number: u64,
    file_index: u64,
}

impl From<RegularFileIdentity> for PersistedFileIdentity {
    fn from(identity: RegularFileIdentity) -> Self {
        Self {
            volume_serial_number: identity.volume_serial_number,
            file_index: identity.file_index,
        }
    }
}

impl From<PersistedFileIdentity> for RegularFileIdentity {
    fn from(identity: PersistedFileIdentity) -> Self {
        Self {
            volume_serial_number: identity.volume_serial_number,
            file_index: identity.file_index,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum ViewTransitionPhase {
    Planned,
    GlobalsReady,
    SnapshotReady,
    TargetPrepared,
    TargetPublished,
    Committing,
    RollingBack,
    Committed,
    RolledBack,
    CleanupComplete,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExistingTargetProof {
    file_sha256: String,
    logical_sha256: String,
    file_identity: PersistedFileIdentity,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", tag = "kind", deny_unknown_fields)]
enum TargetProof {
    Absent,
    Existing(ExistingTargetProof),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StateReplacementPlan {
    target_path: PathBuf,
    recovery_path: PathBuf,
    staging_path: PathBuf,
    rollback_tombstone_path: PathBuf,
    snapshot_build_path: PathBuf,
    snapshot_path: PathBuf,
    snapshot_witness_path: PathBuf,
    target_proof: TargetProof,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GlobalLinkPlan {
    source_path: PathBuf,
    target_path: PathBuf,
    staging_path: PathBuf,
    rollback_tombstone_path: PathBuf,
    target_preexisted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GlobalLinkProof {
    source_path: PathBuf,
    expected_sha256: String,
    expected_identity: PersistedFileIdentity,
    create_identity_bindings: Option<PersistedHandleCreateIdentityBindings>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ViewTransitionPlanRecord {
    transition_id: String,
    operation_id: String,
    source_state_path: PathBuf,
    target_provider: String,
    source_state_file_sha256: String,
    source_state_identity: PersistedFileIdentity,
    source_state_logical_sha256: String,
    state_replacement: StateReplacementPlan,
    global_links: Vec<GlobalLinkPlan>,
    next_state_path: PathBuf,
    next_state: SessionViewState,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ViewTransitionJournal {
    schema_version: u32,
    plan: ViewTransitionPlanRecord,
    plan_sha256: String,
    phase: ViewTransitionPhase,
    snapshot_sha256: Option<String>,
    snapshot_identity: Option<PersistedFileIdentity>,
    state_create_identity_bindings: Option<PersistedHandleCreateIdentityBindings>,
    replace_identity_bindings: Option<PersistedHandleReplaceIdentityBindings>,
    global_link_proofs: Vec<GlobalLinkProof>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistedHandleReplaceIdentityBindings {
    parent_identity: PersistedFileIdentity,
    original_identity: PersistedFileIdentity,
    replacement_identity: PersistedFileIdentity,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistedHandleCreateIdentityBindings {
    parent_identity: PersistedFileIdentity,
    created_identity: PersistedFileIdentity,
}

impl From<HandleCreateIdentityBindings> for PersistedHandleCreateIdentityBindings {
    fn from(bindings: HandleCreateIdentityBindings) -> Self {
        Self {
            parent_identity: bindings.parent_identity.into(),
            created_identity: bindings.created_identity.into(),
        }
    }
}

impl From<PersistedHandleCreateIdentityBindings> for HandleCreateIdentityBindings {
    fn from(bindings: PersistedHandleCreateIdentityBindings) -> Self {
        Self {
            parent_identity: bindings.parent_identity.into(),
            created_identity: bindings.created_identity.into(),
        }
    }
}

impl From<HandleReplaceIdentityBindings> for PersistedHandleReplaceIdentityBindings {
    fn from(bindings: HandleReplaceIdentityBindings) -> Self {
        Self {
            parent_identity: bindings.parent_identity.into(),
            original_identity: bindings.original_identity.into(),
            replacement_identity: bindings.replacement_identity.into(),
        }
    }
}

impl From<PersistedHandleReplaceIdentityBindings> for HandleReplaceIdentityBindings {
    fn from(bindings: PersistedHandleReplaceIdentityBindings) -> Self {
        Self {
            parent_identity: bindings.parent_identity.into(),
            original_identity: bindings.original_identity.into(),
            replacement_identity: bindings.replacement_identity.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ViewTransitionJournalEnvelope {
    journal: ViewTransitionJournal,
    integrity_sha256: String,
}

#[derive(Debug)]
enum HeldStateTransition {
    None,
    Created(PublishedHandleCreate),
    Replaced { replacement: PublishedHandleReplace },
}

#[derive(Debug)]
enum ResolvedStateTransition {
    None,
    Created(ResolvedHandleCreate),
    Replaced(ResolvedHandleReplace),
}

#[derive(Debug)]
pub(crate) struct PreparedViewTransition {
    data_root: PathBuf,
    journal: Option<ViewTransitionJournal>,
    bootstrap: Option<PreparedRelayBootstrap>,
    held_state: HeldStateTransition,
    held_global_creates: Vec<PublishedHandleCreate>,
    held_source_guards: Vec<WriteExclusionGuard>,
    receipt: IncrementalSessionSyncReceipt,
}

#[derive(Debug)]
struct PreparedRelayBootstrap {
    state_path: PathBuf,
    expected_state: Vec<u8>,
    state_created: bool,
    relay_root: PathBuf,
    relay_root_created: bool,
    managed_root: PathBuf,
    managed_root_created: bool,
}

impl PreparedRelayBootstrap {
    fn verify(&self) -> Result<(), String> {
        let observed = read_regular_file_bounded(&self.state_path, MAX_STATE_BYTES)
            .map_err(|_| "failed to verify empty Relay bootstrap state".to_string())?;
        if observed != self.expected_state {
            return Err("empty Relay bootstrap state changed during route switch".to_string());
        }
        if !self.relay_root.is_dir() {
            return Err("empty Relay bootstrap directory is missing".to_string());
        }
        Ok(())
    }

    fn rollback(self) -> Result<(), String> {
        if self.state_created {
            self.verify()?;
            fs::remove_file(&self.state_path).map_err(|error| {
                format!("failed to roll back empty Relay bootstrap state: {error}")
            })?;
        }
        if self.relay_root_created {
            match fs::remove_dir(&self.relay_root) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "failed to roll back empty Relay bootstrap directory: {error}"
                    ))
                }
            }
        }
        if self.managed_root_created {
            match fs::remove_dir(&self.managed_root) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "failed to roll back managed session view directory: {error}"
                    ))
                }
            }
        }
        Ok(())
    }
}

type PreparedSynchronization = (
    usize,
    ViewTransitionJournal,
    HeldStateTransition,
    Vec<PublishedHandleCreate>,
    Vec<WriteExclusionGuard>,
);

impl PreparedViewTransition {
    pub(crate) fn skipped(plan: &SessionViewPlan) -> Self {
        Self {
            data_root: plan.data_root.clone(),
            journal: None,
            bootstrap: None,
            held_state: HeldStateTransition::None,
            held_global_creates: Vec::new(),
            held_source_guards: Vec::new(),
            receipt: IncrementalSessionSyncReceipt::skipped(),
        }
    }

    pub(crate) fn receipt(&self) -> &IncrementalSessionSyncReceipt {
        &self.receipt
    }
}

fn path_is_missing(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(_) => Err("managed session view artifact is unavailable".to_string()),
    }
}

fn verify_persisted_identity(path: &Path, expected: PersistedFileIdentity) -> Result<(), String> {
    if same_persisted_regular_file_identity(path, expected.into())? {
        Ok(())
    } else {
        Err("managed session view artifact identity changed".to_string())
    }
}

fn delete_exact_if_present(
    path: &Path,
    expected_sha256: &str,
    expected_identity: PersistedFileIdentity,
) -> Result<(), String> {
    if path_is_missing(path)? {
        return Ok(());
    }
    verify_persisted_identity(path, expected_identity)
        .map_err(|error| format!("artifact identity precheck failed: {error}"))?;
    let mut guard = DestructiveFileGuard::acquire(path)
        .map_err(|error| format!("artifact exclusive cleanup guard failed: {error}"))?;
    guard.verify_current_path(Some(expected_sha256))?;
    if guard.identity()? != expected_identity.into() {
        return Err("managed session view artifact identity changed".to_string());
    }
    guard.delete()
}

fn global_link_proof<'a>(
    journal: &'a ViewTransitionJournal,
    link: &GlobalLinkPlan,
) -> Result<&'a GlobalLinkProof, String> {
    journal
        .global_link_proofs
        .iter()
        .find(|proof| proof.source_path == link.source_path)
        .ok_or_else(|| "shared database ownership proof is missing".to_string())
}

fn cleanup_journal_artifacts(
    journal: &ViewTransitionJournal,
    rollback: bool,
) -> Result<(), String> {
    let replacement = journal.plan.state_replacement.clone();
    if let (Some(snapshot_sha256), Some(snapshot_identity)) = (
        journal.snapshot_sha256.as_deref(),
        journal.snapshot_identity,
    ) {
        for (label, path) in [
            ("build", &replacement.snapshot_build_path),
            ("published", &replacement.snapshot_path),
            ("witness", &replacement.snapshot_witness_path),
        ] {
            delete_exact_if_present(path, snapshot_sha256, snapshot_identity)
                .map_err(|error| format!("state snapshot {label} cleanup failed: {error}"))?;
        }
    } else if [
        &replacement.snapshot_build_path,
        &replacement.snapshot_path,
        &replacement.snapshot_witness_path,
    ]
    .iter()
    .any(|path| path.exists())
    {
        return Err("session view snapshot exists without a persisted identity".to_string());
    }
    let _ = rollback;
    for link in &journal.plan.global_links {
        if !link.target_preexisted
            && (link.staging_path.exists() || link.rollback_tombstone_path.exists())
        {
            return Err("shared database typed cleanup left an artifact".to_string());
        }
    }
    if matches!(replacement.target_proof, TargetProof::Absent)
        && (replacement.staging_path.exists() || replacement.rollback_tombstone_path.exists())
    {
        return Err("new session view typed cleanup left an artifact".to_string());
    }
    Ok(())
}

pub(crate) fn commit_transition(mut prepared: PreparedViewTransition) -> Result<(), String> {
    if let Some(bootstrap) = prepared.bootstrap.take() {
        bootstrap.verify()?;
    }
    let Some(mut journal) = prepared.journal.take() else {
        return Ok(());
    };
    journal.phase = ViewTransitionPhase::Committing;
    persist_transition_journal(&prepared.data_root, &journal)?;
    let resolved = match prepared.held_state {
        HeldStateTransition::None => ResolvedStateTransition::None,
        HeldStateTransition::Created(created) => ResolvedStateTransition::Created(
            created
                .commit()
                .map_err(|(error, _)| format!("failed to commit new state view: {error}"))?,
        ),
        HeldStateTransition::Replaced { replacement } => {
            let resolved = replacement
                .commit()
                .map_err(|(error, _)| format!("failed to commit state replacement: {error}"))?;
            ResolvedStateTransition::Replaced(resolved)
        }
    };
    let mut resolved_globals = Vec::with_capacity(prepared.held_global_creates.len());
    for created in prepared.held_global_creates {
        resolved_globals.push(
            created
                .commit()
                .map_err(|(error, _)| format!("failed to commit shared database view: {error}"))?,
        );
    }
    save_state(&journal.plan.next_state_path, &journal.plan.next_state)?;
    journal.phase = ViewTransitionPhase::Committed;
    persist_transition_journal(&prepared.data_root, &journal)?;
    drop(prepared.held_source_guards);
    match resolved {
        ResolvedStateTransition::None => {}
        ResolvedStateTransition::Created(resolved) => {
            let guard = resolved
                .cleanup_after_durable_terminal()
                .map_err(|(error, _)| format!("failed to finalize new state view: {error}"))?;
            drop(guard);
        }
        ResolvedStateTransition::Replaced(resolved) => {
            let guard = resolved
                .cleanup_after_durable_terminal()
                .map_err(|(error, _)| format!("failed to finalize state replacement: {error}"))?;
            drop(guard);
        }
    }
    for resolved in resolved_globals {
        let guard = resolved
            .cleanup_after_durable_terminal()
            .map_err(|(error, _)| format!("failed to finalize shared database view: {error}"))?;
        drop(guard);
    }
    cleanup_journal_artifacts(&journal, false)
        .map_err(|error| format!("failed to clean committed session view artifacts: {error}"))?;
    journal.phase = ViewTransitionPhase::CleanupComplete;
    persist_transition_journal(&prepared.data_root, &journal)?;
    remove_transition_journal(&prepared.data_root)
}

pub(crate) fn rollback_transition(mut prepared: PreparedViewTransition) -> Result<(), String> {
    if let Some(bootstrap) = prepared.bootstrap.take() {
        bootstrap.rollback()?;
    }
    let Some(mut journal) = prepared.journal.take() else {
        return Ok(());
    };
    journal.phase = ViewTransitionPhase::RollingBack;
    persist_transition_journal(&prepared.data_root, &journal)?;
    let resolved = match prepared.held_state {
        HeldStateTransition::None => ResolvedStateTransition::None,
        HeldStateTransition::Created(created) => {
            ResolvedStateTransition::Created(created.restore().map_err(|(error, _)| error)?)
        }
        HeldStateTransition::Replaced { replacement } => {
            let resolved = replacement.restore().map_err(|(error, _)| error)?;
            ResolvedStateTransition::Replaced(resolved)
        }
    };
    let mut resolved_globals = Vec::with_capacity(prepared.held_global_creates.len());
    for created in prepared.held_global_creates {
        resolved_globals.push(created.restore().map_err(|(error, _)| error)?);
    }
    journal.phase = ViewTransitionPhase::RolledBack;
    persist_transition_journal(&prepared.data_root, &journal)?;
    drop(prepared.held_source_guards);
    match resolved {
        ResolvedStateTransition::None => {}
        ResolvedStateTransition::Created(resolved) => {
            let guard = resolved
                .cleanup_after_durable_terminal()
                .map_err(|(error, _)| error)?;
            drop(guard);
        }
        ResolvedStateTransition::Replaced(resolved) => {
            let guard = resolved
                .cleanup_after_durable_terminal()
                .map_err(|(error, _)| error)?;
            drop(guard);
        }
    }
    for resolved in resolved_globals {
        let guard = resolved
            .cleanup_after_durable_terminal()
            .map_err(|(error, _)| error)?;
        drop(guard);
    }
    cleanup_journal_artifacts(&journal, true)?;
    journal.phase = ViewTransitionPhase::CleanupComplete;
    persist_transition_journal(&prepared.data_root, &journal)?;
    remove_transition_journal(&prepared.data_root)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionViewTarget {
    Account,
    Relay,
}

#[derive(Debug, Clone)]
pub(crate) enum SessionViewTransition {
    None,
    BootstrapRelay {
        account: CodexPaths,
        relay: CodexPaths,
        state: SessionViewState,
        session_view_state_path: PathBuf,
    },
    PrepareRelay {
        account: CodexPaths,
        relay: CodexPaths,
        state: SessionViewState,
        session_view_state_path: PathBuf,
        view_established: bool,
    },
    PublishAccount {
        relay: CodexPaths,
        account: CodexPaths,
        state: SessionViewState,
        session_view_state_path: PathBuf,
    },
    PublishLegacyAccount {
        relay: CodexPaths,
        account: CodexPaths,
        state: SessionViewState,
        session_view_state_path: PathBuf,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct SessionViewPlan {
    pub(crate) sqlite_home_patch: SqliteHomePatch,
    pub(crate) transition: SessionViewTransition,
    data_root: PathBuf,
    pending_recovery: bool,
}

impl SessionViewPlan {
    pub(crate) fn requires_work(&self) -> bool {
        self.pending_recovery || !matches!(&self.transition, SessionViewTransition::None)
    }

    pub(crate) fn recover_pending(&self, codex_home: &Path) -> Result<bool, String> {
        if self.pending_recovery {
            recover_pending_transition(codex_home, &self.data_root)
        } else {
            Ok(false)
        }
    }
}

fn session_view_plan(
    sqlite_home_patch: SqliteHomePatch,
    transition: SessionViewTransition,
) -> SessionViewPlan {
    SessionViewPlan {
        sqlite_home_patch,
        transition,
        data_root: PathBuf::new(),
        pending_recovery: false,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SessionViewState {
    version: u32,
    account_configured_sqlite_home: Option<String>,
    account_effective_sqlite_home: PathBuf,
    relay_sqlite_home: PathBuf,
    last_common_state_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LegacySessionViewStateV1 {
    version: u32,
    account_configured_sqlite_home: Option<String>,
    account_effective_sqlite_home: PathBuf,
    relay_sqlite_home: PathBuf,
}

pub(crate) fn plan_transition(
    codex_home: &Path,
    live_config: &str,
    target: SessionViewTarget,
    data_root: &Path,
) -> Result<SessionViewPlan, String> {
    if load_transition_journal(data_root)?.is_some() {
        return Ok(SessionViewPlan {
            sqlite_home_patch: SqliteHomePatch::Keep,
            transition: SessionViewTransition::None,
            data_root: data_root.to_path_buf(),
            pending_recovery: true,
        });
    }
    let session_view_state_path = state_path(data_root);
    let current = resolve_user_codex_paths(codex_home)?;
    let configured = configured_sqlite_home(live_config)?;
    let saved_state = load_state(&session_view_state_path, data_root)?;
    let legacy_state = load_legacy_state(data_root)?;
    validate_state_pair(saved_state.as_ref(), legacy_state.as_ref())?;
    let using_relay_view = saved_state
        .as_ref()
        .is_some_and(|state| current.sqlite_home == state.relay_sqlite_home);
    let using_legacy_relay = legacy_state
        .as_ref()
        .is_some_and(|state| current.sqlite_home == state.relay_sqlite_home);
    let legacy_relay_root = data_root.join("relay-sqlite");
    if saved_state.is_none()
        && legacy_state.is_none()
        && (looks_like_managed_relay_view(&current.sqlite_home)
            || current.sqlite_home == legacy_relay_root)
    {
        return Err("Relay 会话视图状态缺失；为避免误用孤立数据库，已停止切换".to_string());
    }

    if using_legacy_relay {
        let legacy = legacy_state
            .as_ref()
            .expect("legacy Relay identity was checked above");
        let state = match saved_state {
            Some(state) if state.last_common_state_sha256.is_none() => state,
            Some(_) => {
                return Err(
                    "旧版与当前 Relay 会话视图状态冲突；为避免覆盖数据库，已停止切换".to_string(),
                )
            }
            None => state_from_legacy(legacy),
        };
        let mut plan = match target {
            SessionViewTarget::Relay => Ok::<SessionViewPlan, String>(session_view_plan(
                SqliteHomePatch::Keep,
                SessionViewTransition::None,
            )),
            SessionViewTarget::Account => {
                let account = codex_paths_with_sqlite_home(
                    codex_home,
                    &legacy.account_effective_sqlite_home,
                )?;
                Ok::<SessionViewPlan, String>(session_view_plan(
                    account_sqlite_home_patch(legacy),
                    SessionViewTransition::PublishLegacyAccount {
                        relay: current,
                        account,
                        state,
                        session_view_state_path,
                    },
                ))
            }
        }?;
        plan.data_root = data_root.to_path_buf();
        return Ok(plan);
    }

    if let Some(state) = saved_state {
        if current.sqlite_home != state.account_effective_sqlite_home && !using_relay_view {
            return Err("OpenAI 主目录已变化；请先执行独立 canonical 目录迁移".to_string());
        }
        validate_state(&state, data_root)?;
        let account =
            codex_paths_with_sqlite_home(codex_home, &state.account_effective_sqlite_home)?;
        let relay = codex_paths_with_sqlite_home(codex_home, &state.relay_sqlite_home)?;
        let bootstrap_pending = state.last_common_state_sha256.is_none();
        let mut plan = match target {
            SessionViewTarget::Relay => Ok::<SessionViewPlan, String>(session_view_plan(
                SqliteHomePatch::Set(state.relay_sqlite_home.to_string_lossy().to_string()),
                if using_relay_view {
                    SessionViewTransition::None
                } else if bootstrap_pending
                    && !account.state_db.exists()
                    && !relay.state_db.exists()
                {
                    SessionViewTransition::BootstrapRelay {
                        account,
                        relay,
                        state,
                        session_view_state_path,
                    }
                } else if bootstrap_pending
                    && relay.state_db.is_file()
                    && !account.state_db.exists()
                {
                    SessionViewTransition::None
                } else {
                    let view_established = !bootstrap_pending;
                    SessionViewTransition::PrepareRelay {
                        account,
                        relay,
                        state,
                        session_view_state_path,
                        view_established,
                    }
                },
            )),
            SessionViewTarget::Account if using_relay_view && relay.state_db.is_file() => {
                Ok::<SessionViewPlan, String>(session_view_plan(
                    account_sqlite_home_patch_from_v2(&state),
                    SessionViewTransition::PublishAccount {
                        relay: current,
                        account,
                        state,
                        session_view_state_path,
                    },
                ))
            }
            SessionViewTarget::Account => Ok::<SessionViewPlan, String>(session_view_plan(
                if using_relay_view {
                    account_sqlite_home_patch_from_v2(&state)
                } else {
                    SqliteHomePatch::Keep
                },
                SessionViewTransition::None,
            )),
        }?;
        plan.data_root = data_root.to_path_buf();
        return Ok(plan);
    }

    if let Some(legacy) = legacy_state {
        if current.sqlite_home != legacy.account_effective_sqlite_home {
            return Err("旧版会话视图与当前 SQLite 主目录不一致；已停止切换".to_string());
        }
        let mut plan = match target {
            SessionViewTarget::Relay => {
                let state = state_from_legacy(&legacy);
                let account =
                    codex_paths_with_sqlite_home(codex_home, &state.account_effective_sqlite_home)?;
                let relay = codex_paths_with_sqlite_home(codex_home, &state.relay_sqlite_home)?;
                Ok::<SessionViewPlan, String>(session_view_plan(
                    SqliteHomePatch::Set(state.relay_sqlite_home.to_string_lossy().to_string()),
                    SessionViewTransition::PrepareRelay {
                        account,
                        relay,
                        state,
                        session_view_state_path,
                        view_established: false,
                    },
                ))
            }
            SessionViewTarget::Account => Ok::<SessionViewPlan, String>(session_view_plan(
                SqliteHomePatch::Keep,
                SessionViewTransition::None,
            )),
        }?;
        plan.data_root = data_root.to_path_buf();
        return Ok(plan);
    }

    let mut plan = match target {
        SessionViewTarget::Relay => {
            let relay_sqlite_home = managed_relay_sqlite_home(&current.sqlite_home);
            let sqlite_home_patch =
                SqliteHomePatch::Set(relay_sqlite_home.to_string_lossy().to_string());
            let state = SessionViewState {
                version: STATE_VERSION,
                account_configured_sqlite_home: configured,
                account_effective_sqlite_home: current.sqlite_home.clone(),
                relay_sqlite_home,
                last_common_state_sha256: None,
            };
            validate_state(&state, data_root)?;
            let account =
                codex_paths_with_sqlite_home(codex_home, &state.account_effective_sqlite_home)?;
            let relay = codex_paths_with_sqlite_home(codex_home, &state.relay_sqlite_home)?;
            let transition = if account.state_db.is_file() {
                SessionViewTransition::PrepareRelay {
                    account,
                    relay,
                    state,
                    session_view_state_path,
                    view_established: false,
                }
            } else {
                SessionViewTransition::BootstrapRelay {
                    account,
                    relay,
                    state,
                    session_view_state_path,
                }
            };
            Ok::<SessionViewPlan, String>(session_view_plan(sqlite_home_patch, transition))
        }
        SessionViewTarget::Account => Ok::<SessionViewPlan, String>(session_view_plan(
            SqliteHomePatch::Keep,
            SessionViewTransition::None,
        )),
    }?;
    plan.data_root = data_root.to_path_buf();
    Ok(plan)
}

pub(crate) fn prepare_transition(
    plan: &SessionViewPlan,
    operation_id: &str,
) -> Result<PreparedViewTransition, String> {
    let data_root = &plan.data_root;
    let started = Instant::now();
    match &plan.transition {
        SessionViewTransition::None => Ok(PreparedViewTransition::skipped(plan)),
        SessionViewTransition::BootstrapRelay {
            account,
            relay,
            state,
            session_view_state_path,
        } => prepare_empty_relay_bootstrap(
            plan,
            account,
            relay,
            state,
            session_view_state_path,
            started,
        ),
        SessionViewTransition::PrepareRelay {
            account,
            relay,
            state,
            session_view_state_path,
            view_established,
        } => {
            let projected_bytes = projected_database_bytes(account)?;
            let mut next_state = state.clone();
            let prepared = prepare_synchronized_session_view(
                account,
                relay,
                operation_id,
                RELAY_PROVIDER,
                if *view_established {
                    state.last_common_state_sha256.as_deref()
                } else {
                    None
                },
                *view_established,
                session_view_state_path,
                &mut next_state,
                data_root,
                true,
                true,
            )?;
            let receipt = IncrementalSessionSyncReceipt {
                status: IncrementalSessionSyncStatus::Applied,
                detected_threads: prepared.0,
                synced_threads: prepared.0,
                projected_bytes,
                duration_ms: started.elapsed().as_millis(),
                requires_full_sync: false,
            };
            Ok(PreparedViewTransition {
                data_root: data_root.clone(),
                journal: Some(prepared.1),
                bootstrap: None,
                held_state: prepared.2,
                held_global_creates: prepared.3,
                held_source_guards: prepared.4,
                receipt,
            })
        }
        SessionViewTransition::PublishAccount {
            relay,
            account,
            state,
            session_view_state_path,
        } => {
            let projected_bytes = projected_database_bytes(relay)?;
            let mut next_state = state.clone();
            let prepared = prepare_synchronized_session_view(
                relay,
                account,
                operation_id,
                "openai",
                state.last_common_state_sha256.as_deref(),
                true,
                session_view_state_path,
                &mut next_state,
                data_root,
                true,
                true,
            )?;
            let receipt = IncrementalSessionSyncReceipt {
                status: if prepared.0 > 0 {
                    IncrementalSessionSyncStatus::Applied
                } else {
                    IncrementalSessionSyncStatus::Unchanged
                },
                detected_threads: prepared.0,
                synced_threads: prepared.0,
                projected_bytes,
                duration_ms: started.elapsed().as_millis(),
                requires_full_sync: false,
            };
            Ok(PreparedViewTransition {
                data_root: data_root.clone(),
                journal: Some(prepared.1),
                bootstrap: None,
                held_state: prepared.2,
                held_global_creates: prepared.3,
                held_source_guards: prepared.4,
                receipt,
            })
        }
        SessionViewTransition::PublishLegacyAccount {
            relay,
            account,
            state,
            session_view_state_path,
        } => {
            let projected_bytes = projected_database_bytes(relay)?;
            ensure_legacy_relay_preserves_account_threads(&relay.state_db, &account.state_db)?;
            let inactive_digest = state_database_digest(&account.state_db)?;
            let mut next_state = state.clone();
            next_state.last_common_state_sha256 = None;
            let prepared = prepare_synchronized_session_view(
                relay,
                account,
                operation_id,
                "openai",
                Some(&inactive_digest),
                true,
                session_view_state_path,
                &mut next_state,
                data_root,
                false,
                false,
            )?;
            let receipt = IncrementalSessionSyncReceipt {
                status: if prepared.0 > 0 {
                    IncrementalSessionSyncStatus::Applied
                } else {
                    IncrementalSessionSyncStatus::Unchanged
                },
                detected_threads: prepared.0,
                synced_threads: prepared.0,
                projected_bytes,
                duration_ms: started.elapsed().as_millis(),
                requires_full_sync: false,
            };
            Ok(PreparedViewTransition {
                data_root: data_root.clone(),
                journal: Some(prepared.1),
                bootstrap: None,
                held_state: prepared.2,
                held_global_creates: prepared.3,
                held_source_guards: prepared.4,
                receipt,
            })
        }
    }
}

fn prepare_empty_relay_bootstrap(
    plan: &SessionViewPlan,
    account: &CodexPaths,
    relay: &CodexPaths,
    state: &SessionViewState,
    session_view_state_path: &Path,
    started: Instant,
) -> Result<PreparedViewTransition, String> {
    if account.state_db.exists() || relay.state_db.exists() {
        return Err("empty Relay bootstrap found an unexpected session database".to_string());
    }
    for name in GLOBAL_DATABASES {
        if account.sqlite_home.join(name).exists() || relay.sqlite_home.join(name).exists() {
            return Err(format!(
                "empty Relay bootstrap found an unexpected {name}; no database was changed"
            ));
        }
    }
    validate_state(state, &plan.data_root)?;
    let managed_root = relay
        .sqlite_home
        .parent()
        .ok_or_else(|| "Relay session view has no managed parent".to_string())?
        .to_path_buf();
    let managed_root_created = !managed_root.exists();
    let relay_root_created = !relay.sqlite_home.exists();
    let expected_state = serialize_state(state)?;
    let state_preexisted = session_view_state_path.exists();
    if state_preexisted {
        let existing = read_regular_file_bounded(session_view_state_path, MAX_STATE_BYTES)
            .map_err(|_| "failed to read empty Relay bootstrap state".to_string())?;
        if existing != expected_state {
            return Err("empty Relay bootstrap state conflicts with the planned view".to_string());
        }
    }
    if let Err(error) = ensure_relay_root(&relay.sqlite_home, &account.sqlite_home, false) {
        let cleanup = PreparedRelayBootstrap {
            state_path: session_view_state_path.to_path_buf(),
            expected_state: expected_state.clone(),
            state_created: false,
            relay_root: relay.sqlite_home.clone(),
            relay_root_created,
            managed_root: managed_root.clone(),
            managed_root_created,
        }
        .rollback();
        return match cleanup {
            Ok(()) => Err(error),
            Err(cleanup_error) => Err(format!(
                "{error}; bootstrap cleanup failed: {cleanup_error}"
            )),
        };
    }
    let state_created = if state_preexisted {
        false
    } else {
        match atomic_create(session_view_state_path, |output| {
            output
                .write_all(&expected_state)
                .map_err(|error| format!("failed to write empty Relay bootstrap state: {error}"))
        }) {
            Ok(created) => created,
            Err(error) => {
                let cleanup = PreparedRelayBootstrap {
                    state_path: session_view_state_path.to_path_buf(),
                    expected_state: expected_state.clone(),
                    state_created: false,
                    relay_root: relay.sqlite_home.clone(),
                    relay_root_created,
                    managed_root: managed_root.clone(),
                    managed_root_created,
                }
                .rollback();
                return match cleanup {
                    Ok(()) => Err(error),
                    Err(cleanup_error) => Err(format!(
                        "{error}; bootstrap cleanup failed: {cleanup_error}"
                    )),
                };
            }
        }
    };
    let observed_state = read_regular_file_bounded(session_view_state_path, MAX_STATE_BYTES);
    if observed_state.as_deref() != Ok(expected_state.as_slice()) {
        let cleanup = PreparedRelayBootstrap {
            state_path: session_view_state_path.to_path_buf(),
            expected_state: expected_state.clone(),
            state_created,
            relay_root: relay.sqlite_home.clone(),
            relay_root_created,
            managed_root: managed_root.clone(),
            managed_root_created,
        }
        .rollback();
        let error = "empty Relay bootstrap state conflicts with the planned view";
        return match cleanup {
            Ok(()) => Err(error.to_string()),
            Err(cleanup_error) => Err(format!(
                "{error}; bootstrap cleanup failed: {cleanup_error}"
            )),
        };
    }
    Ok(PreparedViewTransition {
        data_root: plan.data_root.clone(),
        journal: None,
        bootstrap: Some(PreparedRelayBootstrap {
            state_path: session_view_state_path.to_path_buf(),
            expected_state,
            state_created,
            relay_root: relay.sqlite_home.clone(),
            relay_root_created,
            managed_root,
            managed_root_created,
        }),
        held_state: HeldStateTransition::None,
        held_global_creates: Vec::new(),
        held_source_guards: Vec::new(),
        receipt: IncrementalSessionSyncReceipt {
            status: IncrementalSessionSyncStatus::Unchanged,
            detected_threads: 0,
            synced_threads: 0,
            projected_bytes: 0,
            duration_ms: started.elapsed().as_millis(),
            requires_full_sync: false,
        },
    })
}

fn managed_relay_sqlite_home(account_sqlite_home: &Path) -> PathBuf {
    account_sqlite_home
        .join(MANAGED_VIEW_DIRECTORY)
        .join(RELAY_VIEW_DIRECTORY)
}

fn looks_like_managed_relay_view(path: &Path) -> bool {
    path.file_name().and_then(|name| name.to_str()) == Some(RELAY_VIEW_DIRECTORY)
        && path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            == Some(MANAGED_VIEW_DIRECTORY)
}

fn state_path(data_root: &Path) -> PathBuf {
    data_root.join("request-route-session-view-v2.json")
}

fn transition_journal_path(data_root: &Path) -> PathBuf {
    data_root.join(TRANSITION_JOURNAL_NAME)
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn serializable_digest(value: &impl Serialize) -> Result<String, String> {
    serde_json::to_vec(value)
        .map(|bytes| sha256_bytes(&bytes))
        .map_err(|_| "failed to hash session view transition journal".to_string())
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_operation_id(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 160
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err("session view operation id is invalid".to_string());
    }
    Ok(())
}

fn protect_transition_journal(plaintext: &[u8]) -> Result<Vec<u8>, String> {
    #[cfg(windows)]
    {
        let ciphertext = crate::crypto::protect(plaintext)
            .map_err(|_| "failed to protect session view transition journal".to_string())?;
        let mut protected = Vec::with_capacity(TRANSITION_JOURNAL_MAGIC.len() + ciphertext.len());
        protected.extend_from_slice(TRANSITION_JOURNAL_MAGIC);
        protected.extend_from_slice(&ciphertext);
        Ok(protected)
    }
    #[cfg(not(windows))]
    {
        Ok(plaintext.to_vec())
    }
}

fn unprotect_transition_journal(protected: &[u8]) -> Result<Vec<u8>, String> {
    #[cfg(windows)]
    {
        let ciphertext = protected
            .strip_prefix(TRANSITION_JOURNAL_MAGIC)
            .ok_or_else(|| "session view transition journal is invalid".to_string())?;
        crate::crypto::unprotect(ciphertext)
            .map_err(|_| "session view transition journal is unreadable".to_string())
    }
    #[cfg(not(windows))]
    {
        Ok(protected.to_vec())
    }
}

fn encode_transition_journal(journal: &ViewTransitionJournal) -> Result<Vec<u8>, String> {
    let envelope = ViewTransitionJournalEnvelope {
        journal: journal.clone(),
        integrity_sha256: serializable_digest(journal)?,
    };
    let plaintext = serde_json::to_vec_pretty(&envelope)
        .map_err(|_| "failed to serialize session view transition journal".to_string())?;
    if plaintext.len() as u64 > MAX_TRANSITION_JOURNAL_BYTES {
        return Err("session view transition journal exceeded its size limit".to_string());
    }
    protect_transition_journal(&plaintext)
}

fn validate_transition_journal(
    journal: &ViewTransitionJournal,
    data_root: &Path,
) -> Result<(), String> {
    if journal.schema_version != TRANSITION_JOURNAL_VERSION
        || !valid_sha256(&journal.plan_sha256)
        || serializable_digest(&journal.plan)? != journal.plan_sha256
        || !valid_sha256(&journal.plan.transition_id)
        || !valid_sha256(&journal.plan.source_state_file_sha256)
        || !valid_sha256(&journal.plan.source_state_logical_sha256)
        || journal
            .snapshot_sha256
            .as_deref()
            .is_some_and(|value| !valid_sha256(value))
        || journal.snapshot_sha256.is_some() != journal.snapshot_identity.is_some()
        || matches!(
            &journal.plan.state_replacement.target_proof,
            TargetProof::Absent
        ) && journal.replace_identity_bindings.is_some()
        || matches!(
            &journal.plan.state_replacement.target_proof,
            TargetProof::Existing(_)
        ) && journal.state_create_identity_bindings.is_some()
        || journal
            .global_link_proofs
            .iter()
            .any(|proof| !valid_sha256(&proof.expected_sha256))
    {
        return Err("session view transition journal integrity is invalid".to_string());
    }
    validate_operation_id(&journal.plan.operation_id)?;
    validate_state(&journal.plan.next_state, data_root)?;
    if journal.plan.next_state_path != state_path(data_root) {
        return Err("session view transition state path is invalid".to_string());
    }
    let replacement = journal.plan.state_replacement.clone();
    let target_parent = replacement
        .target_path
        .parent()
        .ok_or_else(|| "session view transition target has no parent".to_string())?;
    let source_parent = journal
        .plan
        .source_state_path
        .parent()
        .ok_or_else(|| "session view transition source has no parent".to_string())?;
    let account_home = &journal.plan.next_state.account_effective_sqlite_home;
    let relay_home = &journal.plan.next_state.relay_sqlite_home;
    let legacy_relay_home = data_root.join("relay-sqlite");
    if replacement
        .target_path
        .file_name()
        .and_then(|name| name.to_str())
        != Some(STATE_DATABASE)
        || journal
            .plan
            .source_state_path
            .file_name()
            .and_then(|name| name.to_str())
            != Some(STATE_DATABASE)
        || (target_parent != account_home && target_parent != relay_home)
        || (source_parent != account_home
            && source_parent != relay_home
            && source_parent != legacy_relay_home)
        || source_parent == target_parent
    {
        return Err("session view transition route paths are invalid".to_string());
    }
    let artifacts = [
        &replacement.recovery_path,
        &replacement.staging_path,
        &replacement.rollback_tombstone_path,
        &replacement.snapshot_build_path,
        &replacement.snapshot_path,
        &replacement.snapshot_witness_path,
    ];
    if artifacts
        .iter()
        .any(|path| path.parent().is_none_or(|parent| parent != target_parent))
    {
        return Err("session view transition artifacts escaped the target directory".to_string());
    }
    let mut distinct = vec![replacement.target_path.clone()];
    distinct.extend(artifacts.into_iter().cloned());
    distinct.sort();
    distinct.dedup();
    if distinct.len() != 7 {
        return Err("session view transition artifact paths are not distinct".to_string());
    }
    if let TargetProof::Existing(proof) = &replacement.target_proof {
        if !valid_sha256(&proof.file_sha256) || !valid_sha256(&proof.logical_sha256) {
            return Err("session view transition target proof is invalid".to_string());
        }
        if let Some(bindings) = journal.replace_identity_bindings {
            if bindings.original_identity != proof.file_identity
                || journal
                    .snapshot_identity
                    .is_some_and(|identity| bindings.replacement_identity != identity)
            {
                return Err(
                    "session view transition replacement identity proof is inconsistent"
                        .to_string(),
                );
            }
        }
    }
    for link in &journal.plan.global_links {
        let source_name = link.source_path.file_name();
        let target_name = link.target_path.file_name();
        if source_name != target_name
            || source_name
                .and_then(|name| name.to_str())
                .is_none_or(|name| !GLOBAL_DATABASES.contains(&name))
            || link.staging_path.parent() != link.target_path.parent()
            || link.rollback_tombstone_path.parent() != link.target_path.parent()
            || link.staging_path == link.target_path
            || link.rollback_tombstone_path == link.target_path
            || link.staging_path == link.rollback_tombstone_path
            || ![
                (account_home.as_path(), relay_home.as_path()),
                (relay_home.as_path(), account_home.as_path()),
            ]
            .contains(&(
                link.source_path.parent().unwrap_or(Path::new("")),
                link.target_path.parent().unwrap_or(Path::new("")),
            ))
        {
            return Err("session view transition global database plan is invalid".to_string());
        }
    }
    if journal.global_link_proofs.iter().any(|proof| {
        !journal
            .plan
            .global_links
            .iter()
            .any(|link| link.source_path == proof.source_path)
    }) {
        return Err("session view transition global proof is not in its plan".to_string());
    }
    let unique_proofs = journal
        .global_link_proofs
        .iter()
        .map(|proof| &proof.source_path)
        .collect::<std::collections::BTreeSet<_>>();
    if unique_proofs.len() != journal.global_link_proofs.len() {
        return Err("session view transition global proofs are duplicated".to_string());
    }
    let expected_global_proofs = journal.plan.global_links.len();
    if journal.global_link_proofs.len() != expected_global_proofs {
        return Err("session view transition global proof set is incomplete".to_string());
    }
    for link in &journal.plan.global_links {
        let proof = global_link_proof(journal, link)?;
        if link.target_preexisted && proof.create_identity_bindings.is_some() {
            return Err("preexisting shared database has a create proof".to_string());
        }
        if !link.target_preexisted
            && matches!(
                journal.phase,
                ViewTransitionPhase::GlobalsReady
                    | ViewTransitionPhase::SnapshotReady
                    | ViewTransitionPhase::TargetPrepared
                    | ViewTransitionPhase::TargetPublished
                    | ViewTransitionPhase::Committing
                    | ViewTransitionPhase::Committed
            )
            && proof.create_identity_bindings.is_none()
        {
            return Err("shared database create identity proof is incomplete".to_string());
        }
    }
    if matches!(
        journal.phase,
        ViewTransitionPhase::SnapshotReady
            | ViewTransitionPhase::TargetPrepared
            | ViewTransitionPhase::TargetPublished
            | ViewTransitionPhase::Committing
            | ViewTransitionPhase::Committed
    ) && journal.snapshot_sha256.is_none()
    {
        return Err("session view transition phase has no snapshot proof".to_string());
    }
    if matches!(
        journal.phase,
        ViewTransitionPhase::TargetPrepared
            | ViewTransitionPhase::TargetPublished
            | ViewTransitionPhase::Committing
            | ViewTransitionPhase::Committed
    ) && matches!(&replacement.target_proof, TargetProof::Existing(_))
        && journal.replace_identity_bindings.is_none()
    {
        return Err("session view transition phase has no replacement identity proof".to_string());
    }
    if matches!(
        journal.phase,
        ViewTransitionPhase::TargetPrepared
            | ViewTransitionPhase::TargetPublished
            | ViewTransitionPhase::Committing
            | ViewTransitionPhase::Committed
    ) && matches!(&replacement.target_proof, TargetProof::Absent)
        && journal.state_create_identity_bindings.is_none()
    {
        return Err("session view transition phase has no create identity proof".to_string());
    }
    Ok(())
}

fn decode_transition_journal(
    protected: &[u8],
    data_root: &Path,
) -> Result<ViewTransitionJournal, String> {
    let plaintext = unprotect_transition_journal(protected)?;
    if plaintext.is_empty() || plaintext.len() as u64 > MAX_TRANSITION_JOURNAL_BYTES {
        return Err("session view transition journal is invalid".to_string());
    }
    let envelope = serde_json::from_slice::<ViewTransitionJournalEnvelope>(&plaintext)
        .map_err(|_| "session view transition journal is invalid".to_string())?;
    if serializable_digest(&envelope.journal)? != envelope.integrity_sha256 {
        return Err("session view transition journal integrity check failed".to_string());
    }
    validate_transition_journal(&envelope.journal, data_root)?;
    Ok(envelope.journal)
}

fn load_transition_journal(data_root: &Path) -> Result<Option<ViewTransitionJournal>, String> {
    let path = transition_journal_path(data_root);
    let protected = match read_regular_file_bounded(&path, MAX_TRANSITION_JOURNAL_BYTES * 2) {
        Ok(bytes) => bytes,
        Err(_) if !path.exists() => return Ok(None),
        Err(_) => return Err("session view transition journal is unreadable".to_string()),
    };
    decode_transition_journal(&protected, data_root).map(Some)
}

fn create_transition_journal(
    data_root: &Path,
    journal: &ViewTransitionJournal,
) -> Result<(), String> {
    validate_transition_journal(journal, data_root)?;
    let bytes = encode_transition_journal(journal)?;
    let path = transition_journal_path(data_root);
    let created = atomic_create(&path, |output| {
        output
            .write_all(&bytes)
            .map_err(|_| "failed to write session view transition journal".to_string())
    })?;
    if !created {
        return Err("another session view transition requires recovery".to_string());
    }
    match load_transition_journal(data_root)? {
        Some(persisted) if persisted == *journal => Ok(()),
        _ => Err("session view transition journal verification failed".to_string()),
    }
}

fn persist_transition_journal(
    data_root: &Path,
    journal: &ViewTransitionJournal,
) -> Result<(), String> {
    validate_transition_journal(journal, data_root)?;
    atomic_write(
        &transition_journal_path(data_root),
        &encode_transition_journal(journal)?,
    )?;
    match load_transition_journal(data_root)? {
        Some(persisted) if persisted == *journal => Ok(()),
        _ => Err("session view transition journal verification failed".to_string()),
    }
}

fn remove_transition_journal(data_root: &Path) -> Result<(), String> {
    let path = transition_journal_path(data_root);
    if path_is_missing(&path)? {
        return Ok(());
    }
    let expected_sha256 = file_sha256(&path)?;
    let mut guard = DestructiveFileGuard::acquire(&path)?;
    guard.verify_current_path(Some(&expected_sha256))?;
    guard.delete()
}

fn replace_paths_from_journal(
    journal: &ViewTransitionJournal,
) -> Result<HandleReplacePaths, String> {
    let replacement = &journal.plan.state_replacement;
    HandleReplacePaths::from_persisted_plan(
        replacement.target_path.clone(),
        replacement.recovery_path.clone(),
        replacement.staging_path.clone(),
        replacement.rollback_tombstone_path.clone(),
    )
}

fn state_create_paths_from_journal(
    journal: &ViewTransitionJournal,
) -> Result<HandleCreatePaths, String> {
    let replacement = &journal.plan.state_replacement;
    HandleCreatePaths::from_persisted_plan(
        replacement.target_path.clone(),
        replacement.staging_path.clone(),
        replacement.rollback_tombstone_path.clone(),
    )
}

fn global_create_paths(link: &GlobalLinkPlan) -> Result<HandleCreatePaths, String> {
    HandleCreatePaths::from_persisted_plan(
        link.target_path.clone(),
        link.staging_path.clone(),
        link.rollback_tombstone_path.clone(),
    )
}

fn verify_exact_path(
    path: &Path,
    expected_sha256: &str,
    expected_identity: PersistedFileIdentity,
) -> Result<(), String> {
    if path_is_missing(path)? {
        return Err("managed session view artifact is missing".to_string());
    }
    verify_persisted_identity(path, expected_identity)?;
    let mut guard = WriteExclusionGuard::acquire(path)?;
    guard.verify_current_path(Some(expected_sha256))?;
    if guard.identity()? != expected_identity.into() {
        return Err("managed session view artifact identity changed".to_string());
    }
    Ok(())
}

fn recover_existing_target(
    journal: &ViewTransitionJournal,
    decision: HandleReplaceRecoveryDecision,
) -> Result<Option<ResolvedHandleReplace>, String> {
    let TargetProof::Existing(original) = &journal.plan.state_replacement.target_proof else {
        return Err("existing session view proof is missing".to_string());
    };
    let Some(persisted_bindings) = journal.replace_identity_bindings else {
        if decision != HandleReplaceRecoveryDecision::Restore {
            return Err(
                "session view cannot commit before replacement identity was persisted".to_string(),
            );
        }
        let replacement = &journal.plan.state_replacement;
        if [
            &replacement.recovery_path,
            &replacement.staging_path,
            &replacement.rollback_tombstone_path,
        ]
        .iter()
        .any(|path| !path_is_missing(path).unwrap_or(false))
        {
            return Err(
                "session view replacement artifacts exist without identity bindings".to_string(),
            );
        }
        verify_exact_path(
            &replacement.target_path,
            &original.file_sha256,
            original.file_identity,
        )?;
        return Ok(None);
    };
    let replacement_sha256 = journal
        .snapshot_sha256
        .as_deref()
        .ok_or_else(|| "session view snapshot digest is missing".to_string())?;
    let bindings: HandleReplaceIdentityBindings = persisted_bindings.into();
    recover_handle_replace(
        &replace_paths_from_journal(journal)?,
        bindings,
        &original.file_sha256,
        replacement_sha256,
        decision,
    )
    .map(Some)
}

fn recover_created_state(
    journal: &ViewTransitionJournal,
    decision: HandleCreateRecoveryDecision,
) -> Result<Option<ResolvedHandleCreate>, String> {
    if !matches!(
        &journal.plan.state_replacement.target_proof,
        TargetProof::Absent
    ) {
        return Err("new session view proof is missing".to_string());
    }
    let Some(persisted_bindings) = journal.state_create_identity_bindings else {
        if decision != HandleCreateRecoveryDecision::Restore {
            return Err(
                "session view cannot commit before create identity was persisted".to_string(),
            );
        }
        let replacement = &journal.plan.state_replacement;
        if [
            &replacement.target_path,
            &replacement.staging_path,
            &replacement.rollback_tombstone_path,
        ]
        .iter()
        .any(|path| !path_is_missing(path).unwrap_or(false))
        {
            return Err("new session view exists without create identity bindings".to_string());
        }
        return Ok(None);
    };
    let snapshot_sha256 = journal
        .snapshot_sha256
        .as_deref()
        .ok_or_else(|| "session view snapshot digest is missing".to_string())?;
    recover_handle_create(
        &state_create_paths_from_journal(journal)?,
        persisted_bindings.into(),
        snapshot_sha256,
        decision,
    )
    .map(Some)
}

fn recover_global_creates(
    journal: &ViewTransitionJournal,
    decision: HandleCreateRecoveryDecision,
    source_guards: &[GlobalRecoverySourceGuard],
) -> Result<Vec<ResolvedHandleCreate>, String> {
    let mut recovered = Vec::new();
    for (index, link) in journal.plan.global_links.iter().enumerate() {
        let proof = global_link_proof(journal, link)?;
        if link.target_preexisted {
            let Some(GlobalRecoverySourceGuard::Exclusive(guard)) = source_guards.get(index) else {
                return Err("preexisting shared database recovery guard is missing".to_string());
            };
            if guard.identity()? != proof.expected_identity.into() {
                return Err(
                    "preexisting shared database recovery guard identity changed".to_string(),
                );
            }
            continue;
        }
        let Some(persisted_bindings) = proof.create_identity_bindings else {
            if decision != HandleCreateRecoveryDecision::Restore {
                return Err(
                    "shared database cannot commit before create identity was persisted"
                        .to_string(),
                );
            }
            if [
                &link.target_path,
                &link.staging_path,
                &link.rollback_tombstone_path,
            ]
            .iter()
            .any(|path| !path_is_missing(path).unwrap_or(false))
            {
                return Err("shared database exists without create identity bindings".to_string());
            }
            continue;
        };
        let Some(GlobalRecoverySourceGuard::Hardlink(source_guard)) = source_guards.get(index)
        else {
            return Err("shared database hardlink recovery guard is missing".to_string());
        };
        recovered.push(recover_handle_hardlink_create(
            &global_create_paths(link)?,
            persisted_bindings.into(),
            &proof.expected_sha256,
            decision,
            source_guard,
        )?);
    }
    Ok(recovered)
}

enum GlobalRecoverySourceGuard {
    Exclusive(WriteExclusionGuard),
    Hardlink(HardlinkSourceGuard),
}

fn acquire_global_recovery_guards(
    journal: &ViewTransitionJournal,
) -> Result<Vec<GlobalRecoverySourceGuard>, String> {
    let mut guards = Vec::with_capacity(journal.plan.global_links.len());
    for link in &journal.plan.global_links {
        let proof = global_link_proof(journal, link)?;
        if link.target_preexisted {
            let mut guard = WriteExclusionGuard::acquire(&link.source_path)?;
            guard.verify_current_path(Some(&proof.expected_sha256))?;
            if guard.identity()? != proof.expected_identity.into() {
                return Err("shared database source identity changed before recovery".to_string());
            }
            if !same_regular_file_identity(&link.source_path, &link.target_path).unwrap_or(false)
                || file_sha256(&link.target_path)? != proof.expected_sha256
            {
                return Err("preexisting shared database view changed before recovery".to_string());
            }
            guards.push(GlobalRecoverySourceGuard::Exclusive(guard));
        } else {
            guards.push(GlobalRecoverySourceGuard::Hardlink(
                HardlinkSourceGuard::acquire(
                    &link.source_path,
                    &proof.expected_sha256,
                    proof.expected_identity.into(),
                )?,
            ));
        }
    }
    Ok(guards)
}

fn canonical_directory(path: &Path) -> Result<PathBuf, String> {
    fs::canonicalize(path).map_err(|_| "session view route directory is unavailable".to_string())
}

fn route_matches(current: &Path, expected: &Path) -> Result<bool, String> {
    Ok(canonical_directory(current)? == canonical_directory(expected)?)
}

/// Recovers a durable Account/Relay database-view transition after every
/// writer has been closed. Returns `true` only when a journal was present.
pub(crate) fn recover_pending_transition(
    codex_home: &Path,
    data_root: &Path,
) -> Result<bool, String> {
    let Some(mut journal) = load_transition_journal(data_root)? else {
        return Ok(false);
    };
    let replacement = journal.plan.state_replacement.clone();
    let target_home = replacement
        .target_path
        .parent()
        .ok_or_else(|| "session view transition target has no parent".to_string())?;
    let source_home = journal
        .plan
        .source_state_path
        .parent()
        .ok_or_else(|| "session view transition source has no parent".to_string())?;
    let current_home = resolve_user_codex_paths(codex_home)?.sqlite_home;
    let commit = if route_matches(&current_home, target_home)? {
        true
    } else if route_matches(&current_home, source_home)? {
        false
    } else {
        return Err("live sqlite_home does not match either durable transition route".to_string());
    };

    let mut source_guard = WriteExclusionGuard::acquire(&journal.plan.source_state_path)?;
    source_guard.verify_current_path(Some(&journal.plan.source_state_file_sha256))?;
    if source_guard.identity()? != journal.plan.source_state_identity.into() {
        return Err("source session view identity changed before recovery".to_string());
    }
    let global_source_guards = acquire_global_recovery_guards(&journal)?;

    let mut recovered_replace = None;
    let mut recovered_state_create = None;
    let mut recovered_global_creates;
    if commit {
        if !matches!(
            journal.phase,
            ViewTransitionPhase::TargetPublished
                | ViewTransitionPhase::Committing
                | ViewTransitionPhase::Committed
                | ViewTransitionPhase::CleanupComplete
        ) {
            return Err("session view journal phase cannot commit the live route".to_string());
        }
        journal.phase = ViewTransitionPhase::Committing;
        persist_transition_journal(data_root, &journal)?;
        match &replacement.target_proof {
            TargetProof::Existing(_) => {
                recovered_replace =
                    recover_existing_target(&journal, HandleReplaceRecoveryDecision::Commit)?;
            }
            TargetProof::Absent => {
                recovered_state_create =
                    recover_created_state(&journal, HandleCreateRecoveryDecision::Commit)?;
            }
        }
        recovered_global_creates = recover_global_creates(
            &journal,
            HandleCreateRecoveryDecision::Commit,
            &global_source_guards,
        )?;
        save_state(&journal.plan.next_state_path, &journal.plan.next_state)?;
        journal.phase = ViewTransitionPhase::Committed;
        persist_transition_journal(data_root, &journal)?;
        if let Some(resolved) = recovered_replace.take() {
            let guard = resolved
                .cleanup_after_durable_terminal()
                .map_err(|(error, _)| error)?;
            drop(guard);
        }
        if let Some(resolved) = recovered_state_create.take() {
            let guard = resolved
                .cleanup_after_durable_terminal()
                .map_err(|(error, _)| error)?;
            drop(guard);
        }
        for resolved in recovered_global_creates.drain(..) {
            let guard = resolved
                .cleanup_after_durable_terminal()
                .map_err(|(error, _)| error)?;
            drop(guard);
        }
        cleanup_journal_artifacts(&journal, false)?;
    } else {
        if journal.phase == ViewTransitionPhase::Committed {
            return Err(
                "committed session view journal conflicts with the live source route".to_string(),
            );
        }
        journal.phase = ViewTransitionPhase::RollingBack;
        persist_transition_journal(data_root, &journal)?;
        if matches!(&replacement.target_proof, TargetProof::Existing(_)) {
            recovered_replace =
                recover_existing_target(&journal, HandleReplaceRecoveryDecision::Restore)?;
        } else {
            recovered_state_create =
                recover_created_state(&journal, HandleCreateRecoveryDecision::Restore)?;
        }
        recovered_global_creates = recover_global_creates(
            &journal,
            HandleCreateRecoveryDecision::Restore,
            &global_source_guards,
        )?;
        journal.phase = ViewTransitionPhase::RolledBack;
        persist_transition_journal(data_root, &journal)?;
        if let Some(resolved) = recovered_replace.take() {
            let guard = resolved
                .cleanup_after_durable_terminal()
                .map_err(|(error, _)| error)?;
            drop(guard);
        }
        if let Some(resolved) = recovered_state_create.take() {
            let guard = resolved
                .cleanup_after_durable_terminal()
                .map_err(|(error, _)| error)?;
            drop(guard);
        }
        for resolved in recovered_global_creates.drain(..) {
            let guard = resolved
                .cleanup_after_durable_terminal()
                .map_err(|(error, _)| error)?;
            drop(guard);
        }
        cleanup_journal_artifacts(&journal, true)?;
    }
    journal.phase = ViewTransitionPhase::CleanupComplete;
    persist_transition_journal(data_root, &journal)?;
    drop(global_source_guards);
    drop(source_guard);
    remove_transition_journal(data_root)?;
    Ok(true)
}

pub(crate) fn inspect_session_view_database_homes(
    data_root: &Path,
) -> Result<Option<(PathBuf, PathBuf)>, String> {
    Ok(load_state(&state_path(data_root), data_root)?
        .map(|state| (state.account_effective_sqlite_home, state.relay_sqlite_home)))
}

pub(crate) fn inspect_legacy_session_view_database_homes(
    data_root: &Path,
) -> Result<Option<(PathBuf, PathBuf)>, String> {
    Ok(load_legacy_state(data_root)?
        .map(|state| (state.account_effective_sqlite_home, state.relay_sqlite_home)))
}

fn load_legacy_state(data_root: &Path) -> Result<Option<LegacySessionViewStateV1>, String> {
    let path = data_root.join("request-route-session-view-v1.json");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("legacy session view state is unreadable".to_string()),
    };
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_STATE_BYTES {
        return Err("legacy session view state is invalid".to_string());
    }
    let bytes = read_regular_file_bounded(&path, MAX_STATE_BYTES)
        .map_err(|_| "legacy session view state is unreadable".to_string())?;
    let state = serde_json::from_slice::<LegacySessionViewStateV1>(&bytes)
        .map_err(|_| "legacy session view state is invalid".to_string())?;
    let expected_relay = data_root.join("relay-sqlite");
    if state.version != 1
        || !state.account_effective_sqlite_home.is_absolute()
        || state.account_effective_sqlite_home == expected_relay
        || state.relay_sqlite_home != expected_relay
        || state
            .account_configured_sqlite_home
            .as_ref()
            .is_some_and(|path| !Path::new(path).is_absolute())
    {
        return Err("legacy session view state paths are invalid".to_string());
    }
    Ok(Some(state))
}

fn validate_state_pair(
    current: Option<&SessionViewState>,
    legacy: Option<&LegacySessionViewStateV1>,
) -> Result<(), String> {
    let (Some(current), Some(legacy)) = (current, legacy) else {
        return Ok(());
    };
    if current.account_effective_sqlite_home != legacy.account_effective_sqlite_home
        || current.account_configured_sqlite_home != legacy.account_configured_sqlite_home
    {
        return Err(
            "旧版与当前会话视图状态指向不同 Account 数据库；为避免覆盖已停止切换".to_string(),
        );
    }
    Ok(())
}

fn state_from_legacy(legacy: &LegacySessionViewStateV1) -> SessionViewState {
    SessionViewState {
        version: STATE_VERSION,
        account_configured_sqlite_home: legacy.account_configured_sqlite_home.clone(),
        account_effective_sqlite_home: legacy.account_effective_sqlite_home.clone(),
        relay_sqlite_home: managed_relay_sqlite_home(&legacy.account_effective_sqlite_home),
        last_common_state_sha256: None,
    }
}

fn account_sqlite_home_patch(legacy: &LegacySessionViewStateV1) -> SqliteHomePatch {
    match &legacy.account_configured_sqlite_home {
        Some(path) => SqliteHomePatch::Set(path.clone()),
        None => SqliteHomePatch::Remove,
    }
}

fn account_sqlite_home_patch_from_v2(state: &SessionViewState) -> SqliteHomePatch {
    match &state.account_configured_sqlite_home {
        Some(path) => SqliteHomePatch::Set(path.clone()),
        None => SqliteHomePatch::Remove,
    }
}

fn configured_sqlite_home(config: &str) -> Result<Option<String>, String> {
    let doc = DocumentMut::from_str(config)
        .map_err(|_| "failed to parse live config.toml".to_string())?;
    let Some(item) = doc.get("sqlite_home") else {
        return Ok(None);
    };
    let path = item
        .as_str()
        .ok_or_else(|| "config.toml sqlite_home must be a string".to_string())?
        .trim();
    Ok((!path.is_empty()).then(|| path.to_string()))
}

fn load_state(path: &Path, data_root: &Path) -> Result<Option<SessionViewState>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("failed to inspect session view state: {error}")),
    };
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_STATE_BYTES {
        return Err("session view state is invalid".to_string());
    }
    let bytes = read_regular_file_bounded(path, MAX_STATE_BYTES)
        .map_err(|_| "failed to read session view state".to_string())?;
    let state = serde_json::from_slice::<SessionViewState>(&bytes)
        .map_err(|_| "session view state is invalid".to_string())?;
    validate_state(&state, data_root)?;
    Ok(Some(state))
}

fn save_state(path: &Path, state: &SessionViewState) -> Result<(), String> {
    let data_root = path
        .parent()
        .ok_or_else(|| "session view state path has no parent".to_string())?;
    validate_state(state, data_root)?;
    let bytes = serialize_state(state)?;
    atomic_write(path, &bytes)
}

fn serialize_state(state: &SessionViewState) -> Result<Vec<u8>, String> {
    let bytes = serde_json::to_vec_pretty(state)
        .map_err(|_| "failed to serialize session view state".to_string())?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err("session view state exceeded the size limit".to_string());
    }
    Ok(bytes)
}

fn validate_state(state: &SessionViewState, data_root: &Path) -> Result<(), String> {
    if state.version != STATE_VERSION {
        return Err("session view state version is unsupported".to_string());
    }
    let _ = data_root;
    let relay = managed_relay_sqlite_home(&state.account_effective_sqlite_home);
    if state.relay_sqlite_home != relay
        || state.account_effective_sqlite_home == relay
        || !state.account_effective_sqlite_home.is_absolute()
    {
        return Err("session view state paths are invalid".to_string());
    }
    if state
        .account_configured_sqlite_home
        .as_ref()
        .is_some_and(|path| !Path::new(path).is_absolute())
    {
        return Err("session view state paths are invalid".to_string());
    }
    if state
        .last_common_state_sha256
        .as_ref()
        .is_some_and(|digest| {
            digest.len() != 64
                || !digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
    {
        return Err("session view state digest is invalid".to_string());
    }
    Ok(())
}

fn projected_database_bytes(paths: &CodexPaths) -> Result<u64, String> {
    if !paths.state_db.is_file() {
        return Ok(0);
    }
    fs::metadata(&paths.state_db)
        .map(|metadata| metadata.len())
        .map_err(|error| format!("failed to inspect session view database: {error}"))
}

fn sanitized_operation_suffix(operation_id: &str) -> Result<String, String> {
    validate_operation_id(operation_id)?;
    let digest = sha256_bytes(operation_id.as_bytes());
    Ok(digest[..24].to_string())
}

fn transition_artifact_path(target: &Path, suffix: &str, kind: &str) -> Result<PathBuf, String> {
    let name = target
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| "session view target file name is invalid".to_string())?;
    Ok(target.with_file_name(format!(".{name}.view-{suffix}.{kind}")))
}

fn file_sha256(path: &Path) -> Result<String, String> {
    let mut file = File::open(path)
        .map_err(|error| format!("failed to open session view artifact: {error}"))?;
    let before = file
        .metadata()
        .map_err(|error| format!("failed to inspect session view artifact: {error}"))?;
    if !before.is_file() {
        return Err("session view artifact is not a regular file".to_string());
    }
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = std::io::Read::read(&mut file, &mut buffer)
            .map_err(|error| format!("failed to hash session view artifact: {error}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let after = file
        .metadata()
        .map_err(|error| format!("failed to inspect session view artifact: {error}"))?;
    if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
        return Err("session view artifact changed while it was hashed".to_string());
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn plan_global_links(
    account: &CodexPaths,
    relay: &CodexPaths,
    relay_is_active: bool,
    view_established: bool,
    operation_id: &str,
) -> Result<Vec<GlobalLinkPlan>, String> {
    let suffix = sanitized_operation_suffix(operation_id)?;
    let mut plans = Vec::new();
    for name in GLOBAL_DATABASES {
        let account_path = account.sqlite_home.join(name);
        let relay_path = relay.sqlite_home.join(name);
        let (source_path, target_path, target_preexisted) =
            match (account_path.is_file(), relay_path.is_file()) {
                (true, true) => {
                    if !same_regular_file_identity(&account_path, &relay_path).unwrap_or(false) {
                        return Err(format!(
                            "{name} is not the verified shared database view; no file was replaced"
                        ));
                    }
                    if relay_is_active {
                        checkpoint_database(&relay_path, name)?;
                        checkpoint_database(&account_path, name)?;
                    } else {
                        checkpoint_database(&account_path, name)?;
                        checkpoint_database(&relay_path, name)?;
                    }
                    (account_path, relay_path, true)
                }
                (true, false) => {
                    if relay_is_active && view_established {
                        return Err(format!("active Relay database {name} is missing"));
                    }
                    checkpoint_database(&account_path, name)?;
                    (account_path, relay_path, false)
                }
                (false, true) if view_established && relay_is_active => {
                    checkpoint_database(&relay_path, name)?;
                    (relay_path, account_path, false)
                }
                (false, true) => {
                    return Err(format!(
                        "Relay view contains an unowned {name}; no file was replaced"
                    ))
                }
                (false, false) => continue,
            };
        let staging_path = transition_artifact_path(&target_path, &suffix, "global-staging")?;
        let rollback_tombstone_path =
            transition_artifact_path(&target_path, &suffix, "global-rollback-tombstone")?;
        if staging_path.exists() || rollback_tombstone_path.exists() {
            return Err(format!(
                "shared database create artifact already exists for {name}"
            ));
        }
        plans.push(GlobalLinkPlan {
            source_path,
            target_path,
            staging_path,
            rollback_tombstone_path,
            target_preexisted,
        });
    }
    Ok(plans)
}

// These arguments are the persisted transition contract. Grouping them into a
// mutable options bag would make omission/defaulting easier during recovery.
#[allow(clippy::too_many_arguments)]
fn prepare_synchronized_session_view(
    source_paths: &CodexPaths,
    target_paths: &CodexPaths,
    operation_id: &str,
    target_provider: &str,
    expected_inactive_digest: Option<&str>,
    view_established: bool,
    next_state_path: &Path,
    next_state: &mut SessionViewState,
    data_root: &Path,
    managed_view: bool,
    record_common_digest: bool,
) -> Result<PreparedSynchronization, String> {
    if load_transition_journal(data_root)?.is_some() {
        return Err("another session view transition requires recovery".to_string());
    }
    if source_paths.codex_home != target_paths.codex_home
        || source_paths.sessions_dir != target_paths.sessions_dir
    {
        return Err("session views must share the canonical Codex Home".to_string());
    }
    if !source_paths.state_db.is_file() {
        return Err("source session view state_5.sqlite is missing".to_string());
    }
    let managed_paths = managed_view.then(|| {
        if looks_like_managed_relay_view(&source_paths.sqlite_home) {
            (target_paths, source_paths)
        } else {
            (source_paths, target_paths)
        }
    });
    if let Some((account, relay)) = managed_paths {
        ensure_relay_root(&relay.sqlite_home, &account.sqlite_home, view_established)?;
    }
    verify_inactive_state_view(
        &target_paths.state_db,
        expected_inactive_digest,
        view_established,
    )?;

    let global_links = if let Some((account, relay)) = managed_paths {
        let relay_is_active = source_paths.sqlite_home == relay.sqlite_home;
        plan_global_links(
            account,
            relay,
            relay_is_active,
            view_established,
            operation_id,
        )?
    } else {
        Vec::new()
    };
    let suffix = sanitized_operation_suffix(operation_id)?;
    let source_state_logical_sha256 =
        checkpoint_state_database_for_transition(&source_paths.state_db)?;
    let source_state_file_sha256 = file_sha256(&source_paths.state_db)?;
    let mut source_state_guard = WriteExclusionGuard::acquire(&source_paths.state_db)?;
    source_state_guard.verify_current_path(Some(&source_state_file_sha256))?;
    ensure_state_database_sidecars_absent(&source_paths.state_db)?;
    let source_state_identity: PersistedFileIdentity = source_state_guard.identity()?.into();
    let mut existing_target_guard = None;
    let target_proof = if target_paths.state_db.is_file() {
        let logical_sha256 = checkpoint_state_database_for_transition(&target_paths.state_db)?;
        let expected_logical_sha256 = expected_inactive_digest
            .ok_or_else(|| "session view baseline digest is missing".to_string())?;
        if logical_sha256 != expected_logical_sha256 {
            return Err(
                "inactive session view changed while its writer barrier was acquired".to_string(),
            );
        }
        let file_sha256 = file_sha256(&target_paths.state_db)?;
        let mut guard = WriteExclusionGuard::acquire(&target_paths.state_db)?;
        guard.verify_current_path(Some(&file_sha256))?;
        ensure_state_database_sidecars_absent(&target_paths.state_db)?;
        let proof = TargetProof::Existing(ExistingTargetProof {
            file_sha256,
            logical_sha256,
            file_identity: guard.identity()?.into(),
        });
        existing_target_guard = Some(guard);
        proof
    } else {
        TargetProof::Absent
    };
    let replacement = StateReplacementPlan {
        target_path: target_paths.state_db.clone(),
        recovery_path: transition_artifact_path(&target_paths.state_db, &suffix, "recovery")?,
        staging_path: transition_artifact_path(&target_paths.state_db, &suffix, "staging")?,
        rollback_tombstone_path: transition_artifact_path(
            &target_paths.state_db,
            &suffix,
            "rollback-tombstone",
        )?,
        snapshot_build_path: transition_artifact_path(
            &target_paths.state_db,
            &suffix,
            "snapshot-building",
        )?,
        snapshot_path: transition_artifact_path(&target_paths.state_db, &suffix, "snapshot")?,
        snapshot_witness_path: transition_artifact_path(
            &target_paths.state_db,
            &suffix,
            "snapshot-owner.witness",
        )?,
        target_proof,
    };
    let mut held_source_guards = vec![source_state_guard];
    let mut global_source_guards = Vec::with_capacity(global_links.len());
    let mut global_link_proofs = Vec::with_capacity(global_links.len());
    for link in &global_links {
        let mut guard = WriteExclusionGuard::acquire(&link.source_path)?;
        let expected_sha256 = guard.verify_current_path(None)?.1;
        let expected_identity: PersistedFileIdentity = guard.identity()?.into();
        if link.target_preexisted
            && !same_regular_file_identity(&link.source_path, &link.target_path).unwrap_or(false)
        {
            return Err("shared global database identity changed".to_string());
        }
        global_link_proofs.push(GlobalLinkProof {
            source_path: link.source_path.clone(),
            expected_sha256,
            expected_identity,
            create_identity_bindings: None,
        });
        global_source_guards.push(Some(guard));
    }
    let transition_id = sha256_bytes(
        serde_json::to_string(&(
            operation_id,
            &source_paths.state_db,
            &target_paths.state_db,
            target_provider,
            &source_state_file_sha256,
            &source_state_logical_sha256,
        ))
        .map_err(|_| "failed to identify session view transition".to_string())?
        .as_bytes(),
    );
    let mut journal = ViewTransitionJournal {
        schema_version: TRANSITION_JOURNAL_VERSION,
        plan: ViewTransitionPlanRecord {
            transition_id,
            operation_id: operation_id.to_string(),
            source_state_path: source_paths.state_db.clone(),
            target_provider: target_provider.to_string(),
            source_state_file_sha256,
            source_state_identity,
            source_state_logical_sha256,
            state_replacement: replacement,
            global_links,
            next_state_path: next_state_path.to_path_buf(),
            next_state: next_state.clone(),
        },
        plan_sha256: String::new(),
        phase: ViewTransitionPhase::Planned,
        snapshot_sha256: None,
        snapshot_identity: None,
        state_create_identity_bindings: None,
        replace_identity_bindings: None,
        global_link_proofs,
    };
    journal.plan_sha256 = serializable_digest(&journal.plan)?;
    create_transition_journal(data_root, &journal)?;

    let prepare_result = (|| {
        // From this point on every mutation is preceded by a durable journal.
        // An error deliberately leaves operation-owned artifacts in place for
        // `recover_pending_transition`; no path is unlinked on an uncertain
        // or partially completed prepare.
        let mut held_global_creates = Vec::new();
        for link in journal.plan.global_links.clone() {
            let proof_index = journal
                .global_link_proofs
                .iter()
                .position(|proof| proof.source_path == link.source_path)
                .ok_or_else(|| "shared database ownership proof is missing".to_string())?;
            let proof = journal.global_link_proofs[proof_index].clone();
            let mut source_guard = global_source_guards
                .get_mut(proof_index)
                .and_then(Option::take)
                .ok_or_else(|| "shared database source guard is missing".to_string())?;
            source_guard.verify_current_path(Some(&proof.expected_sha256))?;
            if source_guard.identity()? != proof.expected_identity.into() {
                return Err("shared database source identity changed".to_string());
            }
            if link.target_preexisted {
                if !same_regular_file_identity(&link.source_path, &link.target_path)
                    .unwrap_or(false)
                {
                    return Err("shared global database identity changed".to_string());
                }
                held_source_guards.push(source_guard);
                continue;
            }
            // The typed hard-link create rebinds the source identity and digest
            // before and after publication. Releasing this source guard is
            // required because the created target is the same file object and
            // two DELETE-denying guards on one identity would self-conflict.
            drop(source_guard);
            let paths = HandleCreatePaths::from_persisted_plan(
                link.target_path.clone(),
                link.staging_path.clone(),
                link.rollback_tombstone_path.clone(),
            )?;
            let expected_bindings = HandleCreateIdentityBindings {
                parent_identity: parent_directory_identity_at_path(&link.target_path)?,
                created_identity: proof.expected_identity.into(),
            };
            journal.global_link_proofs[proof_index].create_identity_bindings =
                Some(expected_bindings.into());
            persist_transition_journal(data_root, &journal)?;
            let staged =
                stage_handle_hardlink_create(&link.source_path, &proof.expected_sha256, &paths)
                    .map_err(|error| format!("failed to stage shared global database: {error}"))?;
            if staged.identity_bindings()? != expected_bindings {
                return Err("shared database create identity binding changed".to_string());
            }
            let published = staged.publish().map_err(|(error, _)| error)?;
            held_global_creates.push(published);
        }
        journal.phase = ViewTransitionPhase::GlobalsReady;
        persist_transition_journal(data_root, &journal)?;

        let replacement = journal.plan.state_replacement.clone();
        if replacement.snapshot_path.exists() || replacement.snapshot_witness_path.exists() {
            return Err("session view snapshot artifact already exists".to_string());
        }
        let snapshot_build_path = replacement.snapshot_build_path.clone();
        if snapshot_build_path.exists() {
            return Err("session view snapshot build artifact already exists".to_string());
        }
        ensure_state_database_sidecars_absent(&journal.plan.source_state_path)?;
        held_source_guards
            .first_mut()
            .ok_or_else(|| "source session view writer barrier is missing".to_string())?
            .copy_current_to_new_file(
                &snapshot_build_path,
                Some(&journal.plan.source_state_file_sha256),
            )?;
        let snapshot_connection = Connection::open(&snapshot_build_path)
            .map_err(|error| format!("failed to open staged state database: {error}"))?;
        snapshot_connection
            .execute_batch("PRAGMA journal_mode=DELETE;")
            .map_err(|error| format!("failed to finalize staged state database: {error}"))?;
        normalize_thread_provider(&snapshot_connection, &journal.plan.target_provider)?;
        let thread_count = count_threads(&snapshot_connection)?;
        verify_database(&snapshot_connection, STATE_DATABASE)?;
        if logical_state_digest(&snapshot_connection)? != journal.plan.source_state_logical_sha256 {
            return Err("session view normalization changed logical state".to_string());
        }
        drop(snapshot_connection);
        let snapshot_sha256 = file_sha256(&snapshot_build_path)?;
        let snapshot_identity: PersistedFileIdentity =
            regular_file_identity_at_path(&snapshot_build_path)?.into();
        journal.snapshot_sha256 = Some(snapshot_sha256.clone());
        journal.snapshot_identity = Some(snapshot_identity);
        persist_transition_journal(data_root, &journal)?;
        fs::hard_link(&snapshot_build_path, &replacement.snapshot_witness_path)
            .map_err(|error| format!("failed to publish state snapshot witness: {error}"))?;
        fs::hard_link(&snapshot_build_path, &replacement.snapshot_path)
            .map_err(|error| format!("failed to publish state snapshot: {error}"))?;
        if !same_regular_file_identity(
            &replacement.snapshot_path,
            &replacement.snapshot_witness_path,
        )
        .unwrap_or(false)
        {
            return Err("state snapshot witness identity verification failed".to_string());
        }
        let mut build_guard = DestructiveFileGuard::acquire(&snapshot_build_path)
            .map_err(|error| format!("failed to retire state snapshot build artifact: {error}"))?;
        build_guard.verify_current_path(Some(&snapshot_sha256))?;
        if build_guard.identity()? != snapshot_identity.into() {
            return Err("state snapshot build identity changed".to_string());
        }
        build_guard.delete()?;
        journal.phase = ViewTransitionPhase::SnapshotReady;
        persist_transition_journal(data_root, &journal)?;

        let held_state = match &replacement.target_proof {
            TargetProof::Existing(proof) => {
                let paths = HandleReplacePaths::from_persisted_plan(
                    replacement.target_path.clone(),
                    replacement.recovery_path.clone(),
                    replacement.staging_path.clone(),
                    replacement.rollback_tombstone_path.clone(),
                )?;
                let mut guard = existing_target_guard
                    .take()
                    .ok_or_else(|| "session view original guard is missing".to_string())?;
                guard.verify_current_path(Some(&proof.file_sha256))?;
                let staged = guard
                    .stage_handle_hardlink_replace(
                        &replacement.snapshot_path,
                        &snapshot_sha256,
                        &paths,
                    )
                    .map_err(|error| {
                        format!("failed to stage session state replacement: {error}")
                    })?;
                let bindings = staged.identity_bindings()?;
                if bindings.original_identity != proof.file_identity.into()
                    || bindings.replacement_identity != snapshot_identity.into()
                    || bindings.parent_identity
                        != parent_directory_identity_at_path(&replacement.target_path)?
                {
                    return Err("session view replacement identity binding changed".to_string());
                }
                journal.replace_identity_bindings = Some(bindings.into());
                persist_transition_journal(data_root, &journal)?;
                let mut prepared = match staged.prepare() {
                    Ok(prepared) => prepared,
                    Err((error, returned)) => {
                        let _ = returned;
                        return Err(error);
                    }
                };
                journal.phase = ViewTransitionPhase::TargetPrepared;
                persist_transition_journal(data_root, &journal)?;
                let published = match prepared.publish() {
                    Ok(published) => published,
                    Err((error, returned)) => {
                        prepared = returned;
                        let _ = prepared;
                        return Err(error);
                    }
                };
                if proof.file_sha256.is_empty() {
                    return Err("session view target proof is missing".to_string());
                }
                journal.phase = ViewTransitionPhase::TargetPublished;
                persist_transition_journal(data_root, &journal)?;
                HeldStateTransition::Replaced {
                    replacement: published,
                }
            }
            TargetProof::Absent => {
                let paths = HandleCreatePaths::from_persisted_plan(
                    replacement.target_path.clone(),
                    replacement.staging_path.clone(),
                    replacement.rollback_tombstone_path.clone(),
                )?;
                let expected_bindings = HandleCreateIdentityBindings {
                    parent_identity: parent_directory_identity_at_path(&replacement.target_path)?,
                    created_identity: snapshot_identity.into(),
                };
                journal.state_create_identity_bindings = Some(expected_bindings.into());
                persist_transition_journal(data_root, &journal)?;
                let staged = stage_handle_hardlink_create(
                    &replacement.snapshot_path,
                    &snapshot_sha256,
                    &paths,
                )
                .map_err(|error| format!("failed to stage new session state view: {error}"))?;
                if staged.identity_bindings()? != expected_bindings {
                    return Err("new session view create identity binding changed".to_string());
                }
                journal.phase = ViewTransitionPhase::TargetPrepared;
                persist_transition_journal(data_root, &journal)?;
                let published = staged.publish().map_err(|(error, _)| error)?;
                journal.phase = ViewTransitionPhase::TargetPublished;
                persist_transition_journal(data_root, &journal)?;
                HeldStateTransition::Created(published)
            }
        };
        if record_common_digest {
            journal.plan.next_state.last_common_state_sha256 =
                Some(journal.plan.source_state_logical_sha256.clone());
        }
        journal.plan_sha256 = serializable_digest(&journal.plan)?;
        persist_transition_journal(data_root, &journal)?;
        Ok((
            thread_count,
            journal,
            held_state,
            held_global_creates,
            held_source_guards,
        ))
    })();
    prepare_result
}

fn checkpoint_state_database_for_transition(path: &Path) -> Result<String, String> {
    let connection = Connection::open(path)
        .map_err(|error| format!("failed to open state database for session view: {error}"))?;
    connection
        .busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|error| format!("failed to configure state database checkpoint: {error}"))?;
    let (busy, _, _): (i64, i64, i64) = connection
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .map_err(|error| format!("failed to checkpoint state database: {error}"))?;
    if busy != 0 {
        return Err("state database is still busy".to_string());
    }
    verify_database(&connection, STATE_DATABASE)?;
    let logical_sha256 = logical_state_digest(&connection)?;
    drop(connection);
    ensure_state_database_sidecars_absent(path)?;
    Ok(logical_sha256)
}

fn ensure_state_database_sidecars_absent(path: &Path) -> Result<(), String> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut candidate = path.as_os_str().to_os_string();
        candidate.push(suffix);
        match fs::symlink_metadata(PathBuf::from(candidate)) {
            Ok(_) => return Err("state database still has an active SQLite sidecar".to_string()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("state database sidecar status is unavailable".to_string()),
        }
    }
    Ok(())
}

fn state_database_digest(path: &Path) -> Result<String, String> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| format!("failed to inspect inactive Account session view: {error}"))?;
    verify_database(&connection, STATE_DATABASE)?;
    logical_state_digest(&connection)
}

fn ensure_legacy_relay_preserves_account_threads(
    relay: &Path,
    account: &Path,
) -> Result<(), String> {
    fn thread_ids(path: &Path) -> Result<std::collections::BTreeSet<String>, String> {
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|_| "failed to inspect a legacy session view database".to_string())?;
        verify_database(&connection, STATE_DATABASE)?;
        let mut statement = connection
            .prepare("SELECT id FROM threads")
            .map_err(|_| "legacy session view threads schema is incompatible".to_string())?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|_| "failed to read legacy session view thread ids".to_string())?;
        rows.collect::<Result<std::collections::BTreeSet<_>, _>>()
            .map_err(|_| "legacy session view contains an invalid thread id".to_string())
    }

    let relay_threads = thread_ids(relay)?;
    let account_threads = thread_ids(account)?;
    if account_threads.is_subset(&relay_threads) {
        Ok(())
    } else {
        Err("旧版 Relay 会话视图缺少 Account 会话；为避免丢失数据库引用，已停止切换".to_string())
    }
}

fn verify_inactive_state_view(
    target: &Path,
    expected_digest: Option<&str>,
    view_established: bool,
) -> Result<(), String> {
    if !target.exists() {
        return if view_established && expected_digest.is_some() {
            Err("inactive session view state database is missing".to_string())
        } else {
            Ok(())
        };
    }
    if !view_established {
        return Err("new Relay session view directory is not empty".to_string());
    }
    let expected =
        expected_digest.ok_or_else(|| "session view baseline digest is missing".to_string())?;
    let connection = Connection::open_with_flags(
        target,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| format!("failed to inspect inactive session view: {error}"))?;
    verify_database(&connection, STATE_DATABASE)?;
    if logical_state_digest(&connection)? != expected {
        return Err(
            "inactive session view changed outside the last verified route transition".to_string(),
        );
    }
    Ok(())
}

fn checkpoint_database(path: &Path, name: &str) -> Result<(), String> {
    let connection = Connection::open(path)
        .map_err(|error| format!("failed to open shared database {name}: {error}"))?;
    connection
        .busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|error| format!("failed to configure shared database {name}: {error}"))?;
    let (busy, _, _): (i64, i64, i64) = connection
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .map_err(|error| format!("failed to checkpoint shared database {name}: {error}"))?;
    if busy != 0 {
        return Err(format!("shared database {name} is still busy"));
    }
    verify_database(&connection, name)
}

fn logical_state_digest(connection: &Connection) -> Result<String, String> {
    let mut table_statement = connection
        .prepare(
            "SELECT name, COALESCE(sql, '') FROM sqlite_master
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .map_err(|error| format!("failed to inspect session view schema: {error}"))?;
    let tables: Vec<(String, String)> = table_statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .and_then(|rows| rows.collect())
        .map_err(|error| format!("failed to inspect session view schema: {error}"))?;
    let mut database_hasher = Sha256::new();
    for (table, schema) in tables {
        hash_field(&mut database_hasher, table.as_bytes());
        hash_field(&mut database_hasher, schema.as_bytes());
        let quoted = quote_identifier(&table);
        let mut rows_statement = connection
            .prepare(&format!("SELECT * FROM {quoted}"))
            .map_err(|error| format!("failed to inspect session view table: {error}"))?;
        let column_names = rows_statement
            .column_names()
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        for column in &column_names {
            hash_field(&mut database_hasher, column.as_bytes());
        }
        let provider_column = (table == "threads")
            .then(|| {
                column_names
                    .iter()
                    .position(|column| column == "model_provider")
            })
            .flatten();
        let mut rows = rows_statement
            .query([])
            .map_err(|error| format!("failed to inspect session view rows: {error}"))?;
        let mut row_hashes = Vec::<[u8; 32]>::new();
        while let Some(row) = rows
            .next()
            .map_err(|error| format!("failed to inspect session view rows: {error}"))?
        {
            let mut row_hasher = Sha256::new();
            for index in 0..column_names.len() {
                if provider_column == Some(index) {
                    hash_typed_value(&mut row_hasher, ValueRef::Text(b"<active-provider>"));
                } else {
                    let value = row.get_ref(index).map_err(|error| {
                        format!("failed to inspect session view value: {error}")
                    })?;
                    hash_typed_value(&mut row_hasher, value);
                }
            }
            row_hashes.push(row_hasher.finalize().into());
        }
        row_hashes.sort_unstable();
        hash_field(
            &mut database_hasher,
            &u64::try_from(row_hashes.len())
                .unwrap_or(u64::MAX)
                .to_le_bytes(),
        );
        for row_hash in row_hashes {
            hash_field(&mut database_hasher, &row_hash);
        }
    }
    Ok(database_hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn hash_typed_value(hasher: &mut Sha256, value: ValueRef<'_>) {
    match value {
        ValueRef::Null => hash_field(hasher, b"n"),
        ValueRef::Integer(value) => {
            hash_field(hasher, b"i");
            hash_field(hasher, &value.to_le_bytes());
        }
        ValueRef::Real(value) => {
            hash_field(hasher, b"r");
            hash_field(hasher, &value.to_bits().to_le_bytes());
        }
        ValueRef::Text(value) => {
            hash_field(hasher, b"t");
            hash_field(hasher, value);
        }
        ValueRef::Blob(value) => {
            hash_field(hasher, b"b");
            hash_field(hasher, value);
        }
    }
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_le_bytes());
    hasher.update(value);
}

fn quote_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn normalize_thread_provider(connection: &Connection, provider: &str) -> Result<usize, String> {
    let columns = connection
        .prepare("PRAGMA table_info(threads)")
        .and_then(|mut statement| {
            statement
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<Result<Vec<_>, _>>()
        })
        .map_err(|error| format!("failed to inspect Relay session schema: {error}"))?;
    if columns.is_empty() {
        return Ok(0);
    }
    if !columns.iter().any(|column| column == "model_provider") {
        return Err("threads table has no model_provider column".to_string());
    }
    connection
        .execute(
            "UPDATE threads SET model_provider = ?1
             WHERE model_provider IS NULL OR model_provider != ?1",
            [provider],
        )
        .map_err(|error| format!("failed to prepare session view visibility: {error}"))
}

fn count_threads(connection: &Connection) -> Result<usize, String> {
    let count = connection
        .query_row("SELECT COUNT(*) FROM threads", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|error| format!("failed to count session view threads: {error}"))?;
    usize::try_from(count).map_err(|_| "session view thread count is invalid".to_string())
}

fn verify_database(connection: &Connection, name: &str) -> Result<(), String> {
    let result: String = connection
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(|error| format!("failed to verify {name}: {error}"))?;
    if result == "ok" {
        Ok(())
    } else {
        Err(format!("{name} failed quick_check"))
    }
}

fn is_expected_unestablished_relay_entry(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    GLOBAL_DATABASES.iter().any(|database| {
        name == *database
            || ["-wal", "-shm", "-journal"]
                .iter()
                .any(|suffix| name == format!("{database}{suffix}"))
    })
}

fn ensure_relay_root(
    path: &Path,
    account_sqlite_home: &Path,
    view_established: bool,
) -> Result<(), String> {
    if path != managed_relay_sqlite_home(account_sqlite_home) {
        return Err("Relay session view is outside the managed canonical root".to_string());
    }
    if !view_established && path.is_dir() {
        for entry in fs::read_dir(path)
            .map_err(|error| format!("failed to inspect Relay session view: {error}"))?
        {
            let entry =
                entry.map_err(|error| format!("failed to inspect Relay session view: {error}"))?;
            let file_type = entry
                .file_type()
                .map_err(|error| format!("failed to inspect Relay session view: {error}"))?;
            if !file_type.is_file() || !is_expected_unestablished_relay_entry(&entry.file_name()) {
                return Err("new Relay session view directory is not empty".to_string());
            }
        }
    }
    let store = path
        .parent()
        .ok_or_else(|| "Relay session view has no managed parent".to_string())?;
    fs::create_dir_all(store)
        .map_err(|error| format!("failed to create managed session view root: {error}"))?;
    fs::create_dir_all(path)
        .map_err(|error| format!("failed to create Relay session view: {error}"))?;
    let account = fs::canonicalize(account_sqlite_home)
        .map_err(|error| format!("failed to resolve canonical SQLite home: {error}"))?;
    let store = fs::canonicalize(store)
        .map_err(|error| format!("failed to resolve managed session view root: {error}"))?;
    let path = fs::canonicalize(path)
        .map_err(|error| format!("failed to resolve Relay session view: {error}"))?;
    if !store.starts_with(&account) || !path.starts_with(&store) {
        return Err("Relay session view escaped the canonical SQLite home".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    use rusqlite::Connection;
    use serde_json::json;
    use tempfile::tempdir;

    use super::{
        commit_transition, load_state, load_transition_journal, logical_state_digest,
        managed_relay_sqlite_home, normalize_thread_provider, persist_transition_journal,
        plan_global_links, plan_transition, prepare_transition, projected_database_bytes,
        recover_pending_transition, rollback_transition, save_state, state_path,
        transition_journal_path, PreparedViewTransition, SessionViewState, SessionViewTarget,
        SessionViewTransition, ViewTransitionPhase, GLOBAL_DATABASES, STATE_VERSION,
    };
    use crate::codex_paths::{codex_paths_with_sqlite_home, local_codex_paths};
    use crate::config_patch::SqliteHomePatch;
    use crate::session_storage::bounded_file::same_regular_file_identity;

    fn sqlite_config(sqlite_home: &Path) -> String {
        format!("sqlite_home = {:?}\n", sqlite_home.to_string_lossy())
    }

    fn write_state_database(path: &Path, provider: &str, thread_ids: &[&str]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, model_provider TEXT NOT NULL);",
            )
            .unwrap();
        for thread_id in thread_ids {
            connection
                .execute(
                    "INSERT INTO threads VALUES (?1, ?2)",
                    [*thread_id, provider],
                )
                .unwrap();
        }
    }

    fn write_legacy_state(data_root: &Path, account: &Path) {
        fs::create_dir_all(data_root).unwrap();
        fs::write(
            data_root.join("request-route-session-view-v1.json"),
            serde_json::to_vec_pretty(&json!({
                "version": 1,
                "accountConfiguredSqliteHome": account,
                "accountEffectiveSqliteHome": account,
                "relaySqliteHome": data_root.join("relay-sqlite"),
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn prepared_relay_crash_fixture(
        operation_id: &str,
    ) -> (
        tempfile::TempDir,
        PathBuf,
        PathBuf,
        PathBuf,
        PreparedViewTransition,
    ) {
        let root = tempdir().unwrap();
        let account_home = root.path().join("account");
        let data_root = root.path().join("data");
        let relay_home = managed_relay_sqlite_home(&account_home);
        fs::create_dir_all(account_home.join("sessions")).unwrap();
        write_state_database(
            &account_home.join("state_5.sqlite"),
            "openai",
            &["account-thread"],
        );
        for (name, table) in [
            ("logs_2.sqlite", "events"),
            ("goals_1.sqlite", "goals"),
            ("memories_1.sqlite", "memories"),
        ] {
            Connection::open(account_home.join(name))
                .unwrap()
                .execute_batch(&format!("CREATE TABLE {table} (id TEXT PRIMARY KEY);"))
                .unwrap();
        }
        let account_config = sqlite_config(&account_home);
        fs::write(account_home.join("config.toml"), &account_config).unwrap();
        let plan = plan_transition(
            &account_home,
            &account_config,
            SessionViewTarget::Relay,
            &data_root,
        )
        .unwrap();
        let prepared = prepare_transition(&plan, operation_id).unwrap();
        (root, account_home, data_root, relay_home, prepared)
    }

    #[test]
    fn crash_windows_before_config_commit_restore_the_source_route_idempotently() {
        for (index, phase) in [
            ViewTransitionPhase::Planned,
            ViewTransitionPhase::GlobalsReady,
            ViewTransitionPhase::SnapshotReady,
            ViewTransitionPhase::TargetPrepared,
            ViewTransitionPhase::TargetPublished,
            ViewTransitionPhase::RollingBack,
        ]
        .into_iter()
        .enumerate()
        {
            let (_root, account_home, data_root, relay_home, prepared) =
                prepared_relay_crash_fixture(&format!("crash-window-{index}"));
            let mut journal = load_transition_journal(&data_root).unwrap().unwrap();
            journal.phase = phase;
            persist_transition_journal(&data_root, &journal).unwrap();
            drop(prepared);

            assert!(recover_pending_transition(&account_home, &data_root).unwrap());
            assert!(!recover_pending_transition(&account_home, &data_root).unwrap());
            assert!(!transition_journal_path(&data_root).exists());
            assert!(!relay_home.join("state_5.sqlite").exists());
            for name in ["logs_2.sqlite", "goals_1.sqlite", "memories_1.sqlite"] {
                assert!(!relay_home.join(name).exists());
            }
        }
    }

    #[test]
    fn crash_after_config_write_commits_the_prepared_view_idempotently() {
        let (_root, account_home, data_root, relay_home, prepared) =
            prepared_relay_crash_fixture("crash-after-config");
        drop(prepared);
        fs::write(account_home.join("config.toml"), sqlite_config(&relay_home)).unwrap();

        assert!(recover_pending_transition(&account_home, &data_root).unwrap());
        assert!(!recover_pending_transition(&account_home, &data_root).unwrap());
        assert!(relay_home.join("state_5.sqlite").is_file());
        assert!(!transition_journal_path(&data_root).exists());
        for name in ["logs_2.sqlite", "goals_1.sqlite", "memories_1.sqlite"] {
            assert!(
                same_regular_file_identity(&account_home.join(name), &relay_home.join(name),)
                    .unwrap()
            );
        }
    }

    #[test]
    fn equal_bytes_replaced_at_the_target_name_fail_identity_bound_recovery() {
        let (_root, account_home, data_root, relay_home, prepared) =
            prepared_relay_crash_fixture("delete-contender");
        drop(prepared);
        let target = relay_home.join("state_5.sqlite");
        let contender = relay_home.join("contender.sqlite");
        fs::copy(&target, &contender).unwrap();
        fs::remove_file(&target).unwrap();
        fs::rename(&contender, &target).unwrap();
        fs::write(account_home.join("config.toml"), sqlite_config(&relay_home)).unwrap();

        let error = recover_pending_transition(&account_home, &data_root).unwrap_err();
        assert!(error.contains("crash state is unknown"));
        assert!(transition_journal_path(&data_root).is_file());
    }

    #[test]
    fn late_writer_on_published_state_fails_closed_without_overwrite() {
        let (_root, account_home, data_root, relay_home, prepared) =
            prepared_relay_crash_fixture("late-writer");
        drop(prepared);
        Connection::open(relay_home.join("state_5.sqlite"))
            .unwrap()
            .execute_batch("CREATE TABLE late_writer (id INTEGER);")
            .unwrap();
        fs::write(account_home.join("config.toml"), sqlite_config(&relay_home)).unwrap();

        let error = recover_pending_transition(&account_home, &data_root).unwrap_err();
        assert!(error.contains("crash state is unknown"));
        assert!(transition_journal_path(&data_root).is_file());
        assert!(Connection::open(relay_home.join("state_5.sqlite"))
            .unwrap()
            .prepare("SELECT id FROM late_writer")
            .is_ok());
    }

    #[test]
    fn legacy_active_relay_returns_to_account_and_seeds_uninitialized_v2_state() {
        let root = tempdir().unwrap();
        let codex_home = root.path().join("codex-home");
        let account_home = root.path().join("account-sqlite");
        let data_root = root.path().join("data");
        let legacy_relay_home = data_root.join("relay-sqlite");
        fs::create_dir_all(codex_home.join("sessions")).unwrap();
        write_state_database(
            &account_home.join("state_5.sqlite"),
            "openai",
            &["account-thread"],
        );
        write_state_database(
            &legacy_relay_home.join("state_5.sqlite"),
            "openai_custom",
            &["account-thread", "relay-thread"],
        );
        let session_path = codex_home.join("sessions/rollout.jsonl");
        fs::write(&session_path, b"canonical session bytes\n").unwrap();
        let session_before = fs::read(&session_path).unwrap();
        write_legacy_state(&data_root, &account_home);
        let live_config = sqlite_config(&legacy_relay_home);
        fs::write(codex_home.join("config.toml"), &live_config).unwrap();

        let relay_no_op = plan_transition(
            &codex_home,
            &live_config,
            SessionViewTarget::Relay,
            &data_root,
        )
        .unwrap();
        assert_eq!(relay_no_op.sqlite_home_patch, SqliteHomePatch::Keep);
        assert!(matches!(
            &relay_no_op.transition,
            SessionViewTransition::None
        ));
        assert!(!state_path(&data_root).exists());

        let account_plan = plan_transition(
            &codex_home,
            &live_config,
            SessionViewTarget::Account,
            &data_root,
        )
        .unwrap();
        assert_eq!(
            account_plan.sqlite_home_patch,
            SqliteHomePatch::Set(account_home.to_string_lossy().to_string())
        );
        assert!(matches!(
            &account_plan.transition,
            SessionViewTransition::PublishLegacyAccount { .. }
        ));

        let prepared = prepare_transition(&account_plan, "legacy-return-account").unwrap();
        let receipt = prepared.receipt().clone();
        commit_transition(prepared).unwrap();

        assert_eq!(receipt.synced_threads, 2);
        let account = Connection::open(account_home.join("state_5.sqlite")).unwrap();
        assert_eq!(
            account
                .query_row(
                    "SELECT COUNT(*) FROM threads WHERE model_provider = 'openai'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            2
        );
        let legacy_relay = Connection::open(legacy_relay_home.join("state_5.sqlite")).unwrap();
        assert_eq!(
            legacy_relay
                .query_row(
                    "SELECT COUNT(*) FROM threads WHERE model_provider = 'openai_custom'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            2
        );
        let state = load_state(&state_path(&data_root), &data_root)
            .unwrap()
            .unwrap();
        assert_eq!(state.account_effective_sqlite_home, account_home);
        assert_eq!(
            state.relay_sqlite_home,
            managed_relay_sqlite_home(&state.account_effective_sqlite_home)
        );
        assert!(state.last_common_state_sha256.is_none());
        assert!(!state.relay_sqlite_home.join("state_5.sqlite").exists());
        assert_eq!(fs::read(&session_path).unwrap(), session_before);

        // A crash after the database publication/state write but before the
        // config patch leaves the legacy Relay active. Replanning must remain
        // an idempotent publication, rather than treating the uninitialized
        // managed v2 view as an established conflicting view.
        drop(account);
        drop(legacy_relay);
        let retry_plan = plan_transition(
            &codex_home,
            &live_config,
            SessionViewTarget::Account,
            &data_root,
        )
        .unwrap();
        assert!(matches!(
            &retry_plan.transition,
            SessionViewTransition::PublishLegacyAccount { .. }
        ));
        let prepared = prepare_transition(&retry_plan, "legacy-return-account-retry").unwrap();
        commit_transition(prepared).unwrap();
        assert_eq!(fs::read(&session_path).unwrap(), session_before);

        let account_config = sqlite_config(&account_home);
        fs::write(codex_home.join("config.toml"), &account_config).unwrap();
        let relay_plan = plan_transition(
            &codex_home,
            &account_config,
            SessionViewTarget::Relay,
            &data_root,
        )
        .unwrap();
        assert!(matches!(
            &relay_plan.transition,
            SessionViewTransition::PrepareRelay {
                view_established: false,
                ..
            }
        ));
        let prepared = prepare_transition(&relay_plan, "legacy-return-then-relay").unwrap();
        commit_transition(prepared).unwrap();
        assert!(load_state(&state_path(&data_root), &data_root)
            .unwrap()
            .unwrap()
            .last_common_state_sha256
            .is_some());
        assert_eq!(fs::read(&session_path).unwrap(), session_before);
    }

    #[test]
    fn legacy_account_creates_the_managed_v2_relay_view_on_first_relay_switch() {
        let root = tempdir().unwrap();
        let codex_home = root.path().join("codex-home");
        let account_home = root.path().join("account-sqlite");
        let data_root = root.path().join("data");
        fs::create_dir_all(codex_home.join("sessions")).unwrap();
        write_state_database(
            &account_home.join("state_5.sqlite"),
            "openai",
            &["account-thread", "continued-thread"],
        );
        write_legacy_state(&data_root, &account_home);
        let live_config = sqlite_config(&account_home);
        fs::write(codex_home.join("config.toml"), &live_config).unwrap();

        let relay_plan = plan_transition(
            &codex_home,
            &live_config,
            SessionViewTarget::Relay,
            &data_root,
        )
        .unwrap();
        let managed_relay_home = managed_relay_sqlite_home(&account_home);
        assert_eq!(
            relay_plan.sqlite_home_patch,
            SqliteHomePatch::Set(managed_relay_home.to_string_lossy().to_string())
        );
        assert!(matches!(
            &relay_plan.transition,
            SessionViewTransition::PrepareRelay {
                view_established: false,
                ..
            }
        ));

        let prepared = prepare_transition(&relay_plan, "legacy-first-v2-relay").unwrap();
        let receipt = prepared.receipt().clone();
        commit_transition(prepared).unwrap();

        assert_eq!(receipt.synced_threads, 2);
        let account = Connection::open(account_home.join("state_5.sqlite")).unwrap();
        assert_eq!(
            account
                .query_row(
                    "SELECT COUNT(*) FROM threads WHERE model_provider = 'openai'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            2
        );
        let relay = Connection::open(managed_relay_home.join("state_5.sqlite")).unwrap();
        assert_eq!(
            relay
                .query_row(
                    "SELECT COUNT(*) FROM threads WHERE model_provider = 'openai_custom'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            2
        );
        let state = load_state(&state_path(&data_root), &data_root)
            .unwrap()
            .unwrap();
        assert!(state.last_common_state_sha256.is_some());
    }

    #[test]
    fn conflicting_v1_and_v2_account_identities_fail_closed() {
        let root = tempdir().unwrap();
        let codex_home = root.path().join("codex-home");
        let legacy_account = root.path().join("legacy-account");
        let current_account = root.path().join("current-account");
        let data_root = root.path().join("data");
        fs::create_dir_all(&codex_home).unwrap();
        fs::create_dir_all(&legacy_account).unwrap();
        fs::create_dir_all(&current_account).unwrap();
        write_legacy_state(&data_root, &legacy_account);
        let current_state = SessionViewState {
            version: STATE_VERSION,
            account_configured_sqlite_home: Some(current_account.to_string_lossy().to_string()),
            account_effective_sqlite_home: current_account.clone(),
            relay_sqlite_home: managed_relay_sqlite_home(&current_account),
            last_common_state_sha256: None,
        };
        save_state(&state_path(&data_root), &current_state).unwrap();
        let live_config = sqlite_config(&legacy_account);
        fs::write(codex_home.join("config.toml"), &live_config).unwrap();

        let error = plan_transition(
            &codex_home,
            &live_config,
            SessionViewTarget::Account,
            &data_root,
        )
        .unwrap_err();

        assert!(error.contains("不同 Account"));
        assert!(!managed_relay_sqlite_home(&current_account).exists());
    }

    #[test]
    fn initialized_v2_state_conflicts_with_an_active_legacy_relay() {
        let root = tempdir().unwrap();
        let codex_home = root.path().join("codex-home");
        let account_home = root.path().join("account");
        let data_root = root.path().join("data");
        let legacy_relay_home = data_root.join("relay-sqlite");
        fs::create_dir_all(&codex_home).unwrap();
        fs::create_dir_all(&account_home).unwrap();
        fs::create_dir_all(&legacy_relay_home).unwrap();
        write_legacy_state(&data_root, &account_home);
        let current_state = SessionViewState {
            version: STATE_VERSION,
            account_configured_sqlite_home: Some(account_home.to_string_lossy().to_string()),
            account_effective_sqlite_home: account_home.clone(),
            relay_sqlite_home: managed_relay_sqlite_home(&account_home),
            last_common_state_sha256: Some("0".repeat(64)),
        };
        save_state(&state_path(&data_root), &current_state).unwrap();
        let live_config = sqlite_config(&legacy_relay_home);
        fs::write(codex_home.join("config.toml"), &live_config).unwrap();

        let error = plan_transition(
            &codex_home,
            &live_config,
            SessionViewTarget::Account,
            &data_root,
        )
        .unwrap_err();

        assert!(error.contains("旧版与当前 Relay 会话视图状态冲突"));
        assert!(!managed_relay_sqlite_home(&account_home)
            .join("state_5.sqlite")
            .exists());
    }

    #[test]
    fn invalid_relative_v1_state_is_not_ignored_when_v2_state_exists() {
        let root = tempdir().unwrap();
        let codex_home = root.path().join("codex-home");
        let account_home = root.path().join("account");
        let data_root = root.path().join("data");
        fs::create_dir_all(&codex_home).unwrap();
        fs::create_dir_all(&account_home).unwrap();
        fs::create_dir_all(&data_root).unwrap();
        let current_state = SessionViewState {
            version: STATE_VERSION,
            account_configured_sqlite_home: Some(account_home.to_string_lossy().to_string()),
            account_effective_sqlite_home: account_home.clone(),
            relay_sqlite_home: managed_relay_sqlite_home(&account_home),
            last_common_state_sha256: None,
        };
        save_state(&state_path(&data_root), &current_state).unwrap();
        fs::write(
            data_root.join("request-route-session-view-v1.json"),
            serde_json::to_vec_pretty(&json!({
                "version": 1,
                "accountConfiguredSqliteHome": "relative-account",
                "accountEffectiveSqliteHome": "relative-account",
                "relaySqliteHome": data_root.join("relay-sqlite"),
            }))
            .unwrap(),
        )
        .unwrap();
        let live_config = sqlite_config(&account_home);
        fs::write(codex_home.join("config.toml"), &live_config).unwrap();

        let error = plan_transition(
            &codex_home,
            &live_config,
            SessionViewTarget::Account,
            &data_root,
        )
        .unwrap_err();

        assert!(error.contains("legacy session view state paths are invalid"));
    }

    #[test]
    fn legacy_relay_missing_an_account_thread_is_never_published() {
        let root = tempdir().unwrap();
        let codex_home = root.path().join("codex-home");
        let account_home = root.path().join("account");
        let data_root = root.path().join("data");
        let relay_home = data_root.join("relay-sqlite");
        fs::create_dir_all(&codex_home).unwrap();
        write_state_database(
            &account_home.join("state_5.sqlite"),
            "openai",
            &["account-only"],
        );
        write_state_database(
            &relay_home.join("state_5.sqlite"),
            "openai_custom",
            &["relay-only"],
        );
        let account_before = fs::read(account_home.join("state_5.sqlite")).unwrap();
        write_legacy_state(&data_root, &account_home);
        let live_config = sqlite_config(&relay_home);
        fs::write(codex_home.join("config.toml"), &live_config).unwrap();
        let plan = plan_transition(
            &codex_home,
            &live_config,
            SessionViewTarget::Account,
            &data_root,
        )
        .unwrap();

        let error = prepare_transition(&plan, "legacy-missing-thread").unwrap_err();

        assert!(error.contains("缺少 Account 会话"));
        assert_eq!(
            fs::read(account_home.join("state_5.sqlite")).unwrap(),
            account_before
        );
        assert!(!state_path(&data_root).exists());
    }

    #[test]
    fn provider_normalization_changes_only_the_copied_database() {
        let root = tempdir().unwrap();
        let connection = Connection::open(root.path().join("state_5.sqlite")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, model_provider TEXT);
                 INSERT INTO threads VALUES ('a', 'openai');
                 INSERT INTO threads VALUES ('b', 'openai_custom');",
            )
            .unwrap();

        assert_eq!(
            normalize_thread_provider(&connection, "openai_custom").unwrap(),
            1
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM threads WHERE model_provider = 'openai_custom'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            2
        );
    }

    #[test]
    fn projected_bytes_exclude_global_hard_linked_databases() {
        let root = tempdir().unwrap();
        let paths = local_codex_paths(root.path());
        fs::write(&paths.state_db, vec![0_u8; 11]).unwrap();
        fs::write(&paths.logs_db, vec![0_u8; 13]).unwrap();

        assert_eq!(projected_database_bytes(&paths).unwrap(), 11);
    }

    #[test]
    fn unestablished_relay_accepts_verified_shared_global_databases() {
        let root = tempdir().unwrap();
        let codex_home = root.path().join("codex-home");
        let account_home = root.path().join("account");
        let data_root = root.path().join("data");
        let relay_home = managed_relay_sqlite_home(&account_home);
        fs::create_dir_all(codex_home.join("sessions")).unwrap();
        fs::create_dir_all(&relay_home).unwrap();
        write_state_database(
            &account_home.join("state_5.sqlite"),
            "openai",
            &["account-thread"],
        );
        for name in GLOBAL_DATABASES {
            let account_path = account_home.join(name);
            Connection::open(&account_path)
                .unwrap()
                .execute_batch("CREATE TABLE shared_value (id INTEGER PRIMARY KEY);")
                .unwrap();
            fs::hard_link(&account_path, relay_home.join(name)).unwrap();
        }
        let state = SessionViewState {
            version: STATE_VERSION,
            account_configured_sqlite_home: Some(account_home.to_string_lossy().to_string()),
            account_effective_sqlite_home: account_home.clone(),
            relay_sqlite_home: relay_home.clone(),
            last_common_state_sha256: None,
        };
        save_state(&state_path(&data_root), &state).unwrap();
        let live_config = sqlite_config(&account_home);
        fs::write(codex_home.join("config.toml"), &live_config).unwrap();

        let relay_plan = plan_transition(
            &codex_home,
            &live_config,
            SessionViewTarget::Relay,
            &data_root,
        )
        .unwrap();
        assert!(matches!(
            relay_plan.transition,
            SessionViewTransition::PrepareRelay {
                view_established: false,
                ..
            }
        ));

        let prepared = prepare_transition(&relay_plan, "shared-global-bootstrap-retry").unwrap();
        commit_transition(prepared).unwrap();

        assert!(relay_home.join("state_5.sqlite").is_file());
        assert!(load_state(&state_path(&data_root), &data_root)
            .unwrap()
            .unwrap()
            .last_common_state_sha256
            .is_some());
        for name in GLOBAL_DATABASES {
            assert!(
                same_regular_file_identity(&account_home.join(name), &relay_home.join(name),)
                    .unwrap()
            );
        }
    }

    #[test]
    fn independent_global_database_is_never_replaced_or_deleted() {
        let root = tempdir().unwrap();
        let account_home = root.path().join("account");
        let relay_home = managed_relay_sqlite_home(&account_home);
        fs::create_dir_all(&account_home).unwrap();
        fs::create_dir_all(&relay_home).unwrap();
        let account = codex_paths_with_sqlite_home(&account_home, &account_home).unwrap();
        let relay = codex_paths_with_sqlite_home(&account_home, &relay_home).unwrap();
        Connection::open(&account.logs_db)
            .unwrap()
            .execute_batch("CREATE TABLE account_log (id INTEGER);")
            .unwrap();
        Connection::open(&relay.logs_db)
            .unwrap()
            .execute_batch("CREATE TABLE relay_log (id INTEGER);")
            .unwrap();

        let error =
            plan_global_links(&account, &relay, false, true, "independent-global").unwrap_err();

        assert!(error.contains("not the verified shared database view"));
        assert!(account.logs_db.is_file());
        assert!(relay.logs_db.is_file());
        assert!(!same_regular_file_identity(&account.logs_db, &relay.logs_db).unwrap());
    }

    #[test]
    fn relay_round_trip_copies_only_state_and_hard_links_global_databases() {
        let root = tempdir().unwrap();
        let account_home = root.path().join("account");
        let relay_home = managed_relay_sqlite_home(&account_home);
        fs::create_dir_all(account_home.join("sessions")).unwrap();
        let account = codex_paths_with_sqlite_home(&account_home, &account_home).unwrap();
        let relay = codex_paths_with_sqlite_home(&account_home, &relay_home).unwrap();

        Connection::open(&account.state_db)
            .unwrap()
            .execute_batch(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, model_provider TEXT);
                 INSERT INTO threads VALUES ('account-thread', 'openai');",
            )
            .unwrap();
        for (path, table) in [
            (&account.logs_db, "events"),
            (&account.goals_db, "goals"),
            (&account.memories_db, "memories"),
        ] {
            Connection::open(path)
                .unwrap()
                .execute_batch(&format!(
                    "CREATE TABLE {table} (id TEXT PRIMARY KEY); INSERT INTO {table} VALUES ('account');"
                ))
                .unwrap();
        }

        let data_root = root.path().join("data");
        let account_config = sqlite_config(&account_home);
        fs::write(account_home.join("config.toml"), &account_config).unwrap();
        let relay_plan = plan_transition(
            &account_home,
            &account_config,
            SessionViewTarget::Relay,
            &data_root,
        )
        .unwrap();
        let prepared = prepare_transition(&relay_plan, "test-all-databases").unwrap();
        assert_eq!(prepared.receipt().synced_threads, 1);
        commit_transition(prepared).unwrap();
        let baseline = load_state(&state_path(&data_root), &data_root)
            .unwrap()
            .unwrap()
            .last_common_state_sha256
            .unwrap();
        for name in ["logs_2.sqlite", "goals_1.sqlite", "memories_1.sqlite"] {
            assert!(same_regular_file_identity(
                &account.sqlite_home.join(name),
                &relay.sqlite_home.join(name),
            )
            .unwrap());
        }
        Connection::open(&relay.state_db)
            .unwrap()
            .execute(
                "INSERT INTO threads VALUES ('relay-thread', 'openai_custom')",
                [],
            )
            .unwrap();
        for (path, table) in [
            (&relay.logs_db, "events"),
            (&relay.goals_db, "goals"),
            (&relay.memories_db, "memories"),
        ] {
            Connection::open(path)
                .unwrap()
                .execute(&format!("INSERT INTO {table} VALUES ('relay')"), [])
                .unwrap();
        }

        let relay_config = sqlite_config(&relay_home);
        fs::write(account_home.join("config.toml"), &relay_config).unwrap();
        let account_plan = plan_transition(
            &account_home,
            &relay_config,
            SessionViewTarget::Account,
            &data_root,
        )
        .unwrap();
        let prepared = prepare_transition(&account_plan, "test-promote-all").unwrap();
        assert_eq!(prepared.receipt().synced_threads, 2);
        commit_transition(prepared).unwrap();
        let next_baseline = load_state(&state_path(&data_root), &data_root)
            .unwrap()
            .unwrap()
            .last_common_state_sha256
            .unwrap();
        assert_ne!(baseline, next_baseline);
        assert_eq!(
            Connection::open(&account.state_db)
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM threads WHERE model_provider = 'openai'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            2
        );
        for (path, table) in [
            (&account.logs_db, "events"),
            (&account.goals_db, "goals"),
            (&account.memories_db, "memories"),
        ] {
            assert_eq!(
                Connection::open(path)
                    .unwrap()
                    .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                        row.get::<_, i64>(0)
                    })
                    .unwrap(),
                2
            );
        }
        assert!(!relay_home.join("sessions").exists());
        assert!(account_home.join("sessions").is_dir());
    }

    #[test]
    fn changed_inactive_state_view_fails_closed_without_overwrite() {
        let root = tempdir().unwrap();
        let account_home = root.path().join("account");
        let relay_home = managed_relay_sqlite_home(&account_home);
        fs::create_dir_all(account_home.join("sessions")).unwrap();
        let account = codex_paths_with_sqlite_home(&account_home, &account_home).unwrap();
        let relay = codex_paths_with_sqlite_home(&account_home, &relay_home).unwrap();
        Connection::open(&account.state_db)
            .unwrap()
            .execute_batch(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, model_provider TEXT);
                 INSERT INTO threads VALUES ('account-thread', 'openai');",
            )
            .unwrap();
        let data_root = root.path().join("data");
        let account_config = sqlite_config(&account_home);
        fs::write(account_home.join("config.toml"), &account_config).unwrap();
        let relay_plan = plan_transition(
            &account_home,
            &account_config,
            SessionViewTarget::Relay,
            &data_root,
        )
        .unwrap();
        let prepared = prepare_transition(&relay_plan, "test-inactive-baseline").unwrap();
        commit_transition(prepared).unwrap();
        Connection::open(&account.state_db)
            .unwrap()
            .execute(
                "INSERT INTO threads VALUES ('unexpected-account-write', 'openai')",
                [],
            )
            .unwrap();
        Connection::open(&relay.state_db)
            .unwrap()
            .execute(
                "INSERT INTO threads VALUES ('relay-thread', 'openai_custom')",
                [],
            )
            .unwrap();

        let relay_config = sqlite_config(&relay_home);
        fs::write(account_home.join("config.toml"), &relay_config).unwrap();
        let account_plan = plan_transition(
            &account_home,
            &relay_config,
            SessionViewTarget::Account,
            &data_root,
        )
        .unwrap();
        let error = prepare_transition(&account_plan, "test-inactive-tamper").unwrap_err();

        assert!(error.contains("inactive session view changed"));
        let account_connection = Connection::open(&account.state_db).unwrap();
        assert_eq!(
            account_connection
                .query_row("SELECT COUNT(*) FROM threads", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            2
        );
        let relay_connection = Connection::open(&relay.state_db).unwrap();
        assert_eq!(
            relay_connection
                .query_row("SELECT COUNT(*) FROM threads", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert_ne!(
            logical_state_digest(&account_connection).unwrap(),
            logical_state_digest(&relay_connection).unwrap()
        );
    }

    #[test]
    fn empty_relay_bootstrap_creates_no_fake_database_and_materializes_account_later() {
        let root = tempdir().unwrap();
        let codex_home = root.path().join("codex-home");
        let data_root = root.path().join("data");
        fs::create_dir_all(&codex_home).unwrap();
        let account_config = sqlite_config(&codex_home);
        fs::write(codex_home.join("config.toml"), &account_config).unwrap();

        let relay_plan = plan_transition(
            &codex_home,
            &account_config,
            SessionViewTarget::Relay,
            &data_root,
        )
        .unwrap();
        assert!(matches!(
            relay_plan.transition,
            SessionViewTransition::BootstrapRelay { .. }
        ));
        let relay_home = match &relay_plan.sqlite_home_patch {
            SqliteHomePatch::Set(path) => PathBuf::from(path),
            other => panic!("unexpected Relay sqlite patch: {other:?}"),
        };
        let prepared = prepare_transition(&relay_plan, "test-empty-relay-bootstrap").unwrap();
        assert!(state_path(&data_root).is_file());
        assert!(relay_home.is_dir());
        assert!(!relay_home.join("state_5.sqlite").exists());
        commit_transition(prepared).unwrap();

        write_state_database(
            &relay_home.join("state_5.sqlite"),
            "openai_custom",
            &["relay-first-thread"],
        );
        let relay_config = sqlite_config(&relay_home);
        fs::write(codex_home.join("config.toml"), &relay_config).unwrap();
        let account_plan = plan_transition(
            &codex_home,
            &relay_config,
            SessionViewTarget::Account,
            &data_root,
        )
        .unwrap();
        assert!(matches!(
            account_plan.transition,
            SessionViewTransition::PublishAccount { .. }
        ));
        let prepared = prepare_transition(&account_plan, "test-bootstrap-to-account").unwrap();
        commit_transition(prepared).unwrap();

        assert_eq!(
            Connection::open(codex_home.join("state_5.sqlite"))
                .unwrap()
                .query_row(
                    "SELECT model_provider FROM threads WHERE id = 'relay-first-thread'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "openai"
        );
        assert!(load_state(&state_path(&data_root), &data_root)
            .unwrap()
            .unwrap()
            .last_common_state_sha256
            .is_some());
    }

    #[test]
    fn bootstrap_state_rejects_independent_account_and_relay_databases() {
        let root = tempdir().unwrap();
        let codex_home = root.path().join("codex-home");
        let data_root = root.path().join("data");
        fs::create_dir_all(&codex_home).unwrap();
        let account_config = sqlite_config(&codex_home);
        fs::write(codex_home.join("config.toml"), &account_config).unwrap();

        let bootstrap_plan = plan_transition(
            &codex_home,
            &account_config,
            SessionViewTarget::Relay,
            &data_root,
        )
        .unwrap();
        let relay_home = match &bootstrap_plan.sqlite_home_patch {
            SqliteHomePatch::Set(path) => PathBuf::from(path),
            other => panic!("unexpected Relay sqlite patch: {other:?}"),
        };
        commit_transition(
            prepare_transition(&bootstrap_plan, "test-independent-bootstrap").unwrap(),
        )
        .unwrap();

        write_state_database(
            &relay_home.join("state_5.sqlite"),
            "openai_custom",
            &["relay-thread"],
        );
        write_state_database(
            &codex_home.join("state_5.sqlite"),
            "openai",
            &["account-thread"],
        );
        let relay_before = fs::read(relay_home.join("state_5.sqlite")).unwrap();
        let account_before = fs::read(codex_home.join("state_5.sqlite")).unwrap();

        let relay_plan = plan_transition(
            &codex_home,
            &account_config,
            SessionViewTarget::Relay,
            &data_root,
        )
        .unwrap();
        assert!(matches!(
            relay_plan.transition,
            SessionViewTransition::PrepareRelay {
                view_established: false,
                ..
            }
        ));
        let error =
            prepare_transition(&relay_plan, "test-independent-bootstrap-conflict").unwrap_err();

        assert!(error.contains("new Relay session view directory is not empty"));
        assert_eq!(
            fs::read(relay_home.join("state_5.sqlite")).unwrap(),
            relay_before
        );
        assert_eq!(
            fs::read(codex_home.join("state_5.sqlite")).unwrap(),
            account_before
        );
    }

    #[test]
    fn empty_relay_bootstrap_rollback_restores_absent_state_and_directories() {
        let root = tempdir().unwrap();
        let codex_home = root.path().join("codex-home");
        let data_root = root.path().join("data");
        fs::create_dir_all(&codex_home).unwrap();
        let account_config = sqlite_config(&codex_home);
        fs::write(codex_home.join("config.toml"), &account_config).unwrap();
        let plan = plan_transition(
            &codex_home,
            &account_config,
            SessionViewTarget::Relay,
            &data_root,
        )
        .unwrap();
        let relay_home = match &plan.sqlite_home_patch {
            SqliteHomePatch::Set(path) => PathBuf::from(path),
            other => panic!("unexpected Relay sqlite patch: {other:?}"),
        };

        let prepared = prepare_transition(&plan, "test-empty-relay-rollback").unwrap();
        rollback_transition(prepared).unwrap();

        assert!(!state_path(&data_root).exists());
        assert!(!relay_home.exists());
        assert!(!relay_home.parent().unwrap().exists());
    }

    #[test]
    fn empty_relay_bootstrap_conflict_does_not_leave_managed_directories() {
        let root = tempdir().unwrap();
        let codex_home = root.path().join("codex-home");
        let data_root = root.path().join("data");
        fs::create_dir_all(&codex_home).unwrap();
        let account_config = sqlite_config(&codex_home);
        let plan = plan_transition(
            &codex_home,
            &account_config,
            SessionViewTarget::Relay,
            &data_root,
        )
        .unwrap();
        let relay_home = match &plan.sqlite_home_patch {
            SqliteHomePatch::Set(path) => PathBuf::from(path),
            other => panic!("unexpected Relay sqlite patch: {other:?}"),
        };
        fs::create_dir_all(&data_root).unwrap();
        fs::write(state_path(&data_root), b"{}").unwrap();

        let error = prepare_transition(&plan, "test-empty-relay-conflict").unwrap_err();

        assert!(error.contains("conflicts with the planned view"), "{error}");
        assert!(!relay_home.exists());
        assert!(!relay_home.parent().unwrap().exists());
        assert_eq!(fs::read(state_path(&data_root)).unwrap(), b"{}");
    }
}

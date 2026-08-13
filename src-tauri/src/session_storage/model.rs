use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const SESSION_STORAGE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub enum SessionRelation {
    Equal,
    EqualExceptProvider,
    LeftPrefix,
    RightPrefix,
    Divergent,
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub enum DatabaseRole {
    CanonicalAccount,
    AccountView,
    Relay,
    Shared,
    LegacyOrRelocated,
    Backup,
    RecoveryPackage,
    DowngradeExport,
    UnknownRuntime,
}

impl DatabaseRole {
    pub fn is_runtime(self) -> bool {
        !matches!(
            self,
            Self::Backup | Self::RecoveryPackage | Self::DowngradeExport
        )
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub enum FileOrigin {
    CanonicalHome,
    Shared,
    ReferencedExternal,
    BackupInventory,
    ConflictRecycle,
    RecoveryPackage,
    DowngradeExport,
    TemporaryAdapter,
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub enum MarkerStatus {
    Absent,
    Valid,
    Invalid,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum FileObservationState {
    Stable,
    ChangedDuringScan,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileObservation {
    pub state: FileObservationState,
    pub stable_observations: u32,
    pub observed_bytes: Option<u64>,
    pub last_verified_at_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RelationCounts {
    pub equal: usize,
    pub equal_except_provider: usize,
    pub prefix: usize,
    pub divergent: usize,
    pub unknown: usize,
}

impl RelationCounts {
    pub(crate) fn record(&mut self, relation: SessionRelation) {
        match relation {
            SessionRelation::Equal => self.equal = self.equal.saturating_add(1),
            SessionRelation::EqualExceptProvider => {
                self.equal_except_provider = self.equal_except_provider.saturating_add(1)
            }
            SessionRelation::LeftPrefix | SessionRelation::RightPrefix => {
                self.prefix = self.prefix.saturating_add(1)
            }
            SessionRelation::Divergent => self.divergent = self.divergent.saturating_add(1),
            SessionRelation::Unknown => self.unknown = self.unknown.saturating_add(1),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ShadowScanSummary {
    pub schema_version: u32,
    pub online_scan_only: bool,
    pub non_atomic_across_databases: bool,
    pub logical_session_count: usize,
    pub canonical_candidate_count: usize,
    pub duplicated_session_count: usize,
    pub conflict_session_count: usize,
    pub high_confidence_copy_count: usize,
    pub session_file_count: usize,
    pub session_bytes: u64,
    pub potential_reclaim_bytes: u64,
    pub marker_file_count: usize,
    pub runtime_database_count: usize,
    pub backup_database_count: usize,
    pub runtime_reference_count: usize,
    pub missing_runtime_reference_count: usize,
    pub mismatched_runtime_reference_count: usize,
    pub cache_hit_count: usize,
    pub cache_miss_count: usize,
    pub stable_file_count: usize,
    #[serde(default)]
    pub turn_context_count: usize,
    #[serde(default)]
    pub resolved_turn_provenance_count: usize,
    #[serde(default)]
    pub historical_unknown_turn_count: usize,
    #[serde(default)]
    pub incomplete_turn_provenance_count: usize,
    pub relation_counts: RelationCounts,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum StorageScanStatus {
    NoSessions,
    CanonicalReady,
    MigrationAvailable,
    ReviewRequired,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub enum ShadowScanIssueCode {
    DatabaseDiscoveryFailed,
    DatabaseSnapshotFailed,
    DatabaseRowMissingRolloutPath,
    SessionDiscoveryFailed,
    SessionParseFailed,
    InvalidProviderMarker,
    MissingRuntimeReference,
    MismatchedRuntimeReference,
    DivergentSession,
    OnlineSnapshotNotAtomic,
    ReportPersistenceFailed,
    HashCacheInvalid,
    HashCachePersistenceFailed,
    TurnProvenanceInvalid,
    StorageStateInvalid,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ShadowScanIssue {
    pub code: ShadowScanIssueCode,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ShadowScanReport {
    pub schema_version: u32,
    pub scan_id: String,
    pub generated_at_ms: u64,
    pub status: StorageScanStatus,
    pub migration_required: bool,
    pub deletion_enabled: bool,
    pub summary: ShadowScanSummary,
    pub issues: Vec<ShadowScanIssue>,
}

#[derive(Debug, Clone)]
pub struct SessionFileInput {
    pub path: PathBuf,
    pub origin: FileOrigin,
    pub marker_status: MarkerStatus,
    pub observation: FileObservation,
    pub semantic: Result<super::semantic::SemanticSession, super::semantic::SemanticError>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadReference {
    pub thread_id: String,
    pub rollout_path: PathBuf,
    pub model_provider: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseInput {
    pub id: String,
    pub path: Option<PathBuf>,
    pub role: DatabaseRole,
    pub references: Vec<ThreadReference>,
}

pub(crate) mod bounded_file;
pub mod catalog;
pub mod codex_runtime_verifier;
pub mod conflict;
pub mod conflict_resolution;
pub mod downgrade;
pub mod hash_cache;
pub mod investigation;
pub mod legacy_backup;
pub mod marker;
pub mod metrics;
pub mod migration;
pub mod migration_apply;
pub mod migration_backup;
pub mod model;
pub mod offline_gc;
pub mod operation_ledger;
pub mod provenance;
pub mod reference_graph;
pub mod relation;
pub mod restore_import;
pub mod retention;
pub mod semantic;
pub mod shadow_scan;
pub mod storage_state;
pub(crate) mod write_barrier;

pub use model::{
    DatabaseRole, FileObservation, FileObservationState, FileOrigin, MarkerStatus, RelationCounts,
    SessionRelation, ShadowScanSummary, StorageScanStatus,
};
pub use reference_graph::{
    analyze_reference_graph, build_reference_graph, ReferenceGraphInput, SessionFileNode,
    SessionReferenceGraph,
};

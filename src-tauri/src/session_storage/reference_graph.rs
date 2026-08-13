use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use super::{
    model::{
        DatabaseInput, FileObservationState, FileOrigin, MarkerStatus, RelationCounts,
        SessionFileInput, SessionRelation, ShadowScanSummary, SESSION_STORAGE_SCHEMA_VERSION,
    },
    relation::compare_sessions,
};

#[derive(Debug, Clone, Default)]
pub struct ReferenceGraphInput {
    pub files: Vec<SessionFileInput>,
    pub databases: Vec<DatabaseInput>,
}

#[derive(Debug, Clone)]
pub struct SessionReferenceGraph {
    pub files: Vec<SessionFileNode>,
    pub databases: Vec<DatabaseInput>,
    pub summary: ShadowScanSummary,
}

#[derive(Debug, Clone)]
pub struct SessionFileNode {
    pub path: PathBuf,
    pub path_key: String,
    pub origin: FileOrigin,
    pub thread_id: Option<String>,
    pub marker_status: MarkerStatus,
    pub observation_state: FileObservationState,
    pub stable_observations: u32,
    pub observed_bytes: Option<u64>,
    pub raw_sha256: Option<[u8; 32]>,
    pub last_verified_at_ms: u64,
    pub runtime_database_ids: Vec<String>,
    pub backup_database_ids: Vec<String>,
    pub retained_candidate: bool,
    pub is_canonical: bool,
    pub is_switch_provider_slot: bool,
    pub relation_to_retained: Option<SessionRelation>,
}

#[derive(Default)]
struct ReferenceSets {
    runtime: BTreeSet<String>,
    backup: BTreeSet<String>,
}

pub fn analyze_reference_graph(input: &ReferenceGraphInput) -> ShadowScanSummary {
    build_reference_graph(input).summary
}

pub fn build_reference_graph(input: &ReferenceGraphInput) -> SessionReferenceGraph {
    let mut references_by_path = BTreeMap::<String, ReferenceSets>::new();
    for database in &input.databases {
        for reference in &database.references {
            let references = references_by_path
                .entry(path_key(&reference.rollout_path))
                .or_default();
            if database.role.is_runtime() {
                references.runtime.insert(database.id.clone());
            } else {
                references.backup.insert(database.id.clone());
            }
        }
    }

    let mut by_thread = BTreeMap::<String, Vec<usize>>::new();
    let mut unknown_files = 0_usize;
    let mut session_bytes = 0_u64;
    let mut marker_file_count = 0_usize;
    let mut known_paths = BTreeMap::<String, Option<String>>::new();
    let mut nodes = Vec::with_capacity(input.files.len());
    for (index, file) in input.files.iter().enumerate() {
        if file.marker_status != MarkerStatus::Absent {
            marker_file_count = marker_file_count.saturating_add(1);
        }
        let key = path_key(&file.path);
        let references = references_by_path.remove(&key).unwrap_or_default();
        match &file.semantic {
            Ok(semantic) => {
                known_paths.insert(key.clone(), Some(semantic.thread_id.clone()));
                session_bytes = session_bytes.saturating_add(semantic.bytes);
                by_thread
                    .entry(semantic.thread_id.clone())
                    .or_default()
                    .push(index);
                nodes.push(SessionFileNode {
                    path: file.path.clone(),
                    path_key: key,
                    origin: file.origin,
                    thread_id: Some(semantic.thread_id.clone()),
                    marker_status: file.marker_status,
                    observation_state: file.observation.state,
                    stable_observations: file.observation.stable_observations,
                    observed_bytes: file.observation.observed_bytes.or(Some(semantic.bytes)),
                    raw_sha256: Some(semantic.raw_sha256),
                    last_verified_at_ms: file.observation.last_verified_at_ms,
                    runtime_database_ids: references.runtime.into_iter().collect(),
                    backup_database_ids: references.backup.into_iter().collect(),
                    retained_candidate: false,
                    is_canonical: false,
                    is_switch_provider_slot: file.marker_status == MarkerStatus::Valid,
                    relation_to_retained: None,
                });
            }
            Err(_) => {
                known_paths.insert(key.clone(), None);
                unknown_files = unknown_files.saturating_add(1);
                nodes.push(SessionFileNode {
                    path: file.path.clone(),
                    path_key: key,
                    origin: file.origin,
                    thread_id: None,
                    marker_status: file.marker_status,
                    observation_state: file.observation.state,
                    stable_observations: file.observation.stable_observations,
                    observed_bytes: file.observation.observed_bytes,
                    raw_sha256: None,
                    last_verified_at_ms: file.observation.last_verified_at_ms,
                    runtime_database_ids: references.runtime.into_iter().collect(),
                    backup_database_ids: references.backup.into_iter().collect(),
                    retained_candidate: false,
                    is_canonical: false,
                    is_switch_provider_slot: false,
                    relation_to_retained: Some(SessionRelation::Unknown),
                });
            }
        }
    }

    let mut canonical_candidate_count = 0_usize;
    let mut duplicated_session_count = 0_usize;
    let mut conflict_session_count = 0_usize;
    let mut high_confidence_copy_count = 0_usize;
    let mut potential_reclaim_bytes = 0_u64;
    let mut relation_counts = RelationCounts::default();

    for indexes in by_thread.values() {
        if indexes.len() > 1 {
            duplicated_session_count = duplicated_session_count.saturating_add(1);
        }
        let retained = select_retained_candidate(indexes, &input.files);
        canonical_candidate_count = canonical_candidate_count.saturating_add(1);
        let Some(retained) = retained else {
            conflict_session_count = conflict_session_count.saturating_add(1);
            relation_counts.unknown = relation_counts.unknown.saturating_add(indexes.len());
            continue;
        };
        nodes[retained].retained_candidate = true;
        nodes[retained].is_canonical = input.files[retained].origin == FileOrigin::CanonicalHome;
        nodes[retained].relation_to_retained = Some(SessionRelation::Equal);
        let retained_semantic = input.files[retained].semantic.as_ref().ok();
        let mut group_conflict = false;
        for index in indexes.iter().copied().filter(|index| *index != retained) {
            let relation = match (input.files[index].semantic.as_ref(), retained_semantic) {
                (Ok(candidate), Some(retained)) => compare_sessions(candidate, retained),
                _ => SessionRelation::Unknown,
            };
            nodes[index].relation_to_retained = Some(relation);
            relation_counts.record(relation);
            let safely_related = matches!(
                relation,
                SessionRelation::Equal
                    | SessionRelation::EqualExceptProvider
                    | SessionRelation::LeftPrefix
            );
            if !safely_related {
                group_conflict = true;
                continue;
            }
            if is_reclaim_estimate_origin(input.files[index].origin)
                && is_reclaim_estimate_origin(input.files[retained].origin)
            {
                high_confidence_copy_count = high_confidence_copy_count.saturating_add(1);
                if let Ok(candidate) = &input.files[index].semantic {
                    potential_reclaim_bytes =
                        potential_reclaim_bytes.saturating_add(candidate.bytes);
                }
            }
        }
        if group_conflict {
            conflict_session_count = conflict_session_count.saturating_add(1);
        }
    }
    if unknown_files > 0 {
        relation_counts.unknown = relation_counts.unknown.saturating_add(unknown_files);
        conflict_session_count = conflict_session_count.saturating_add(unknown_files);
    }

    let runtime_database_count = input
        .databases
        .iter()
        .filter(|database| database.role.is_runtime())
        .count();
    let backup_database_count = input.databases.len().saturating_sub(runtime_database_count);
    let mut runtime_reference_count = 0_usize;
    let mut missing_runtime_reference_count = 0_usize;
    let mut mismatched_runtime_reference_count = 0_usize;
    for database in input
        .databases
        .iter()
        .filter(|database| database.role.is_runtime())
    {
        for reference in &database.references {
            runtime_reference_count = runtime_reference_count.saturating_add(1);
            match known_paths.get(&path_key(&reference.rollout_path)) {
                None => {
                    missing_runtime_reference_count =
                        missing_runtime_reference_count.saturating_add(1)
                }
                Some(Some(thread_id)) if thread_id != &reference.thread_id => {
                    mismatched_runtime_reference_count =
                        mismatched_runtime_reference_count.saturating_add(1)
                }
                Some(None) => {
                    mismatched_runtime_reference_count =
                        mismatched_runtime_reference_count.saturating_add(1)
                }
                Some(Some(_)) => {}
            }
        }
    }

    let summary = ShadowScanSummary {
        schema_version: SESSION_STORAGE_SCHEMA_VERSION,
        online_scan_only: true,
        non_atomic_across_databases: true,
        logical_session_count: by_thread.len(),
        canonical_candidate_count,
        duplicated_session_count,
        conflict_session_count,
        high_confidence_copy_count,
        session_file_count: input.files.len(),
        session_bytes,
        potential_reclaim_bytes,
        marker_file_count,
        runtime_database_count,
        backup_database_count,
        runtime_reference_count,
        missing_runtime_reference_count,
        mismatched_runtime_reference_count,
        cache_hit_count: 0,
        cache_miss_count: 0,
        stable_file_count: 0,
        turn_context_count: 0,
        resolved_turn_provenance_count: 0,
        historical_unknown_turn_count: 0,
        incomplete_turn_provenance_count: 0,
        relation_counts,
    };
    SessionReferenceGraph {
        files: nodes,
        databases: input.databases.clone(),
        summary,
    }
}

fn select_retained_candidate(indexes: &[usize], files: &[SessionFileInput]) -> Option<usize> {
    let mut candidates = indexes
        .iter()
        .copied()
        .filter(|index| files[*index].semantic.is_ok())
        .collect::<Vec<_>>();
    candidates.sort_by_key(|index| {
        let semantic = files[*index].semantic.as_ref().expect("filtered semantic");
        (
            Reverse(semantic.normalized_line_sha256.len()),
            marker_rank(files[*index].marker_status),
            origin_rank(files[*index].origin),
            path_key(&files[*index].path),
        )
    });
    let complete = candidates.iter().copied().find(|candidate_index| {
        let candidate = files[*candidate_index]
            .semantic
            .as_ref()
            .expect("filtered semantic");
        indexes.iter().all(|other_index| {
            if other_index == candidate_index {
                return true;
            }
            match files[*other_index].semantic.as_ref() {
                Ok(other) => matches!(
                    compare_sessions(other, candidate),
                    SessionRelation::Equal
                        | SessionRelation::EqualExceptProvider
                        | SessionRelation::LeftPrefix
                ),
                Err(_) => false,
            }
        })
    });
    complete.or_else(|| {
        candidates.sort_by_key(|index| {
            let semantic = files[*index].semantic.as_ref().expect("filtered semantic");
            (
                origin_rank(files[*index].origin),
                marker_rank(files[*index].marker_status),
                Reverse(semantic.normalized_line_sha256.len()),
                path_key(&files[*index].path),
            )
        });
        candidates.into_iter().next()
    })
}

fn marker_rank(status: MarkerStatus) -> u8 {
    match status {
        MarkerStatus::Absent => 0,
        MarkerStatus::Invalid => 1,
        MarkerStatus::Valid => 2,
    }
}

fn origin_rank(origin: FileOrigin) -> u8 {
    match origin {
        FileOrigin::CanonicalHome => 0,
        FileOrigin::Shared => 1,
        FileOrigin::ReferencedExternal => 2,
        FileOrigin::BackupInventory => 3,
        FileOrigin::ConflictRecycle => 4,
        FileOrigin::RecoveryPackage => 5,
        FileOrigin::DowngradeExport => 6,
        FileOrigin::TemporaryAdapter => 7,
        FileOrigin::Unknown => 8,
    }
}

fn is_reclaim_estimate_origin(origin: FileOrigin) -> bool {
    matches!(
        origin,
        FileOrigin::CanonicalHome | FileOrigin::Shared | FileOrigin::ReferencedExternal
    )
}

pub(crate) fn path_key(path: &Path) -> String {
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path));
    let normalized = resolved.to_string_lossy().replace('/', "\\");
    if cfg!(windows) {
        normalized.to_ascii_lowercase()
    } else {
        normalized
    }
}

pub(crate) fn managed_relative_components(path: &Path, root: &Path) -> Option<Vec<String>> {
    let path = path_key(path);
    let root = path_key(root);
    let root = root.trim_end_matches('\\');
    if root.is_empty() {
        return None;
    }
    let suffix = path.strip_prefix(root)?;
    if suffix.is_empty() {
        return Some(Vec::new());
    }
    let relative = suffix.strip_prefix('\\')?;
    Some(
        relative
            .split('\\')
            .filter(|component| !component.is_empty())
            .map(str::to_ascii_lowercase)
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{
        analyze_reference_graph, build_reference_graph, managed_relative_components,
        ReferenceGraphInput,
    };
    use crate::session_storage::{
        model::{
            DatabaseInput, DatabaseRole, FileObservation, FileObservationState, FileOrigin,
            MarkerStatus, SessionFileInput, SessionRelation, ThreadReference,
        },
        semantic::read_semantic_session,
    };

    fn write_session(
        root: &std::path::Path,
        name: &str,
        id: &str,
        provider: &str,
        messages: &[&str],
    ) -> std::path::PathBuf {
        let path = root.join(name);
        let mut lines = vec![serde_json::json!({
            "type": "session_meta",
            "payload": {"id": id, "model_provider": provider}
        })
        .to_string()];
        lines.extend(messages.iter().map(|message| {
            serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "user_message", "message": message}
            })
            .to_string()
        }));
        fs::write(&path, lines.join("\n") + "\n").unwrap();
        path
    }

    fn file(path: std::path::PathBuf, origin: FileOrigin) -> SessionFileInput {
        let bytes = fs::metadata(&path).unwrap().len();
        SessionFileInput {
            semantic: read_semantic_session(&path),
            path,
            origin,
            marker_status: MarkerStatus::Valid,
            observation: FileObservation {
                state: FileObservationState::Stable,
                stable_observations: 2,
                observed_bytes: Some(bytes),
                last_verified_at_ms: 42,
            },
        }
    }

    #[test]
    fn chooses_content_superset_without_using_filename_time() {
        let root = tempdir().unwrap();
        let misleading_newer_name = write_session(
            root.path(),
            "rollout-2099-short.jsonl",
            "thread-a",
            "openai",
            &["one"],
        );
        let complete = write_session(
            root.path(),
            "rollout-2000-complete.jsonl",
            "thread-a",
            "openai_custom",
            &["one", "two"],
        );
        let input = ReferenceGraphInput {
            files: vec![
                file(misleading_newer_name, FileOrigin::Shared),
                file(complete.clone(), FileOrigin::CanonicalHome),
            ],
            databases: vec![DatabaseInput {
                id: "account".to_string(),
                path: None,
                role: DatabaseRole::CanonicalAccount,
                references: vec![ThreadReference {
                    thread_id: "thread-a".to_string(),
                    rollout_path: complete,
                    model_provider: Some("openai".to_string()),
                }],
            }],
        };

        let graph = build_reference_graph(&input);
        let summary = &graph.summary;

        assert_eq!(summary.logical_session_count, 1);
        assert_eq!(summary.duplicated_session_count, 1);
        assert_eq!(summary.conflict_session_count, 0);
        assert_eq!(summary.high_confidence_copy_count, 1);
        assert_eq!(summary.relation_counts.prefix, 1);
        assert_eq!(summary.missing_runtime_reference_count, 0);
        let retained = graph
            .files
            .iter()
            .find(|file| file.retained_candidate)
            .unwrap();
        assert!(retained.is_canonical);
        assert_eq!(retained.runtime_database_ids, ["account"]);
        assert_eq!(retained.observation_state, FileObservationState::Stable);
        assert_eq!(retained.stable_observations, 2);
        assert!(retained.raw_sha256.is_some());
        assert_eq!(
            graph.files[0].relation_to_retained,
            Some(SessionRelation::LeftPrefix)
        );
    }

    #[test]
    fn prefers_an_unmarked_canonical_file_over_an_equal_provider_slot() {
        let root = tempdir().unwrap();
        let provider_slot = write_session(
            root.path(),
            "a-provider-slot.jsonl",
            "thread-a",
            "openai_custom",
            &["one"],
        );
        let canonical = write_session(
            root.path(),
            "z-canonical.jsonl",
            "thread-a",
            "openai",
            &["one"],
        );
        let mut canonical_input = file(canonical.clone(), FileOrigin::CanonicalHome);
        canonical_input.marker_status = MarkerStatus::Absent;
        let input = ReferenceGraphInput {
            files: vec![
                file(provider_slot.clone(), FileOrigin::CanonicalHome),
                canonical_input,
            ],
            databases: Vec::new(),
        };

        let graph = build_reference_graph(&input);
        let retained = graph
            .files
            .iter()
            .find(|file| file.retained_candidate)
            .unwrap();
        let slot = graph
            .files
            .iter()
            .find(|file| file.path == provider_slot)
            .unwrap();

        assert_eq!(retained.path, canonical);
        assert_eq!(retained.marker_status, MarkerStatus::Absent);
        assert!(retained.is_canonical);
        assert_eq!(
            slot.relation_to_retained,
            Some(SessionRelation::EqualExceptProvider)
        );
        assert!(slot.is_switch_provider_slot);
    }

    #[test]
    fn a_marked_content_extension_still_wins_over_a_shorter_unmarked_file() {
        let root = tempdir().unwrap();
        let canonical = write_session(
            root.path(),
            "canonical.jsonl",
            "thread-a",
            "openai",
            &["one"],
        );
        let provider_extension = write_session(
            root.path(),
            "provider-extension.jsonl",
            "thread-a",
            "openai_custom",
            &["one", "two"],
        );
        let mut canonical_input = file(canonical.clone(), FileOrigin::CanonicalHome);
        canonical_input.marker_status = MarkerStatus::Absent;
        let input = ReferenceGraphInput {
            files: vec![
                canonical_input,
                file(provider_extension.clone(), FileOrigin::Shared),
            ],
            databases: Vec::new(),
        };

        let graph = build_reference_graph(&input);
        let retained = graph
            .files
            .iter()
            .find(|file| file.retained_candidate)
            .unwrap();

        assert_eq!(retained.path, provider_extension);
        assert_eq!(
            graph
                .files
                .iter()
                .find(|file| file.path == canonical)
                .unwrap()
                .relation_to_retained,
            Some(SessionRelation::LeftPrefix)
        );
    }

    #[test]
    fn divergent_and_missing_references_are_reported_not_reclaimed() {
        let root = tempdir().unwrap();
        let left = write_session(root.path(), "left.jsonl", "thread-a", "openai", &["left"]);
        let right = write_session(
            root.path(),
            "right.jsonl",
            "thread-a",
            "openai_custom",
            &["right"],
        );
        let input = ReferenceGraphInput {
            files: vec![
                file(left, FileOrigin::CanonicalHome),
                file(right, FileOrigin::Shared),
            ],
            databases: vec![DatabaseInput {
                id: "relay".to_string(),
                path: None,
                role: DatabaseRole::Relay,
                references: vec![ThreadReference {
                    thread_id: "thread-a".to_string(),
                    rollout_path: root.path().join("missing.jsonl"),
                    model_provider: Some("openai_custom".to_string()),
                }],
            }],
        };

        let summary = analyze_reference_graph(&input);

        assert_eq!(summary.conflict_session_count, 1);
        assert_eq!(summary.high_confidence_copy_count, 0);
        assert_eq!(summary.relation_counts.divergent, 1);
        assert_eq!(summary.missing_runtime_reference_count, 1);
        assert!(summary.online_scan_only);
        assert!(summary.non_atomic_across_databases);
    }

    #[test]
    fn three_or_more_copies_keep_the_complete_candidate_without_conflict() {
        let root = tempdir().unwrap();
        let canonical = write_session(
            root.path(),
            "rollout-2000-complete.jsonl",
            "thread-a",
            "openai",
            &["one", "two"],
        );
        let provider_copy = write_session(
            root.path(),
            "rollout-2099-provider.jsonl",
            "thread-a",
            "openai_custom",
            &["one", "two"],
        );
        let prefix = write_session(
            root.path(),
            "rollout-2100-short.jsonl",
            "thread-a",
            "openai",
            &["one"],
        );
        let input = ReferenceGraphInput {
            files: vec![
                file(prefix, FileOrigin::Shared),
                file(provider_copy, FileOrigin::ReferencedExternal),
                file(canonical.clone(), FileOrigin::CanonicalHome),
            ],
            databases: vec![DatabaseInput {
                id: "account".to_string(),
                path: None,
                role: DatabaseRole::CanonicalAccount,
                references: vec![ThreadReference {
                    thread_id: "thread-a".to_string(),
                    rollout_path: canonical,
                    model_provider: Some("openai".to_string()),
                }],
            }],
        };

        let summary = analyze_reference_graph(&input);

        assert_eq!(summary.logical_session_count, 1);
        assert_eq!(summary.session_file_count, 3);
        assert_eq!(summary.high_confidence_copy_count, 2);
        assert_eq!(summary.conflict_session_count, 0);
        assert_eq!(summary.relation_counts.equal_except_provider, 1);
        assert_eq!(summary.relation_counts.prefix, 1);
    }

    #[test]
    fn backup_references_are_inventory_only_not_runtime_retention_proof() {
        let root = tempdir().unwrap();
        let canonical = write_session(
            root.path(),
            "canonical.jsonl",
            "thread-a",
            "openai",
            &["one"],
        );
        let input = ReferenceGraphInput {
            files: vec![file(canonical.clone(), FileOrigin::CanonicalHome)],
            databases: vec![
                DatabaseInput {
                    id: "account".to_string(),
                    path: None,
                    role: DatabaseRole::CanonicalAccount,
                    references: vec![ThreadReference {
                        thread_id: "thread-a".to_string(),
                        rollout_path: canonical.clone(),
                        model_provider: Some("openai".to_string()),
                    }],
                },
                DatabaseInput {
                    id: "backup".to_string(),
                    path: None,
                    role: DatabaseRole::Backup,
                    references: vec![ThreadReference {
                        thread_id: "thread-b".to_string(),
                        rollout_path: canonical,
                        model_provider: Some("openai_custom".to_string()),
                    }],
                },
            ],
        };

        let graph = build_reference_graph(&input);
        let summary = &graph.summary;

        assert_eq!(summary.runtime_database_count, 1);
        assert_eq!(summary.backup_database_count, 1);
        assert_eq!(summary.runtime_reference_count, 1);
        assert_eq!(summary.missing_runtime_reference_count, 0);
        assert_eq!(summary.mismatched_runtime_reference_count, 0);
        assert_eq!(graph.files[0].runtime_database_ids, ["account"]);
        assert_eq!(graph.files[0].backup_database_ids, ["backup"]);
    }

    #[test]
    fn protected_package_copies_are_not_reported_as_potential_reclaim() {
        let root = tempdir().unwrap();
        let canonical = write_session(
            root.path(),
            "canonical.jsonl",
            "thread-a",
            "openai",
            &["one"],
        );
        let backup = write_session(
            root.path(),
            "backup.jsonl",
            "thread-a",
            "openai_custom",
            &["one"],
        );
        let input = ReferenceGraphInput {
            files: vec![
                file(canonical, FileOrigin::CanonicalHome),
                file(backup, FileOrigin::BackupInventory),
            ],
            databases: Vec::new(),
        };

        let summary = analyze_reference_graph(&input);

        assert_eq!(summary.relation_counts.equal_except_provider, 1);
        assert_eq!(summary.high_confidence_copy_count, 0);
        assert_eq!(summary.potential_reclaim_bytes, 0);
        assert_eq!(summary.conflict_session_count, 0);
    }

    #[test]
    fn protected_package_superset_does_not_make_canonical_reclaimable() {
        let root = tempdir().unwrap();
        let canonical = write_session(
            root.path(),
            "canonical.jsonl",
            "thread-a",
            "openai",
            &["one"],
        );
        let backup = write_session(
            root.path(),
            "backup.jsonl",
            "thread-a",
            "openai_custom",
            &["one", "two"],
        );
        let input = ReferenceGraphInput {
            files: vec![
                file(canonical, FileOrigin::CanonicalHome),
                file(backup, FileOrigin::BackupInventory),
            ],
            databases: Vec::new(),
        };

        let graph = build_reference_graph(&input);

        assert_eq!(graph.summary.relation_counts.prefix, 1);
        assert_eq!(graph.summary.high_confidence_copy_count, 0);
        assert_eq!(graph.summary.potential_reclaim_bytes, 0);
        assert_eq!(graph.summary.conflict_session_count, 0);
        assert_eq!(
            graph
                .files
                .iter()
                .filter(|file| file.retained_candidate)
                .count(),
            1
        );
        assert_eq!(
            graph
                .files
                .iter()
                .find(|file| file.retained_candidate)
                .map(|file| file.origin),
            Some(FileOrigin::BackupInventory)
        );
    }

    #[test]
    fn managed_relative_components_rejects_prefixes_and_unrelated_parents() {
        let root = std::path::Path::new("C:/user/recovery-name/codex-switch");
        assert_eq!(
            managed_relative_components(
                std::path::Path::new("C:/user/recovery-name/codex-switch/backups/item"),
                root,
            ),
            Some(vec!["backups".to_string(), "item".to_string()])
        );
        assert_eq!(
            managed_relative_components(
                std::path::Path::new("C:/user/recovery-name/codex-switch-other/backups"),
                root,
            ),
            None
        );
        assert_eq!(
            managed_relative_components(std::path::Path::new("C:/user/recovery-name"), root),
            None
        );
    }
}

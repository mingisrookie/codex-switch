use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    io::{BufRead, BufReader, BufWriter, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use toml_edit::{value, DocumentMut, Item, Table};

use crate::{file_ops::atomic_write, operation_log::timestamp_millis};

use super::{
    catalog::goals_database_digest,
    migration_backup::{
        MigrationBackupEntry, MigrationBackupEntryKind, MigrationBackupManifest,
        MigrationBackupRuntimeVerifier, MigrationRuntimeBinaryIdentity,
        MigrationRuntimeCapabilityConflictProof, MigrationRuntimeConflictProof,
        MigrationRuntimeVerification,
    },
    model::SessionRelation,
    reference_graph::path_key,
    relation::compare_sessions,
    semantic::{read_semantic_session, SemanticErrorKind, SemanticSession},
};

const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const PROCESS_EXIT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_RPC_LINE_BYTES: usize = 64 * 1024 * 1024;
const PROBE_PROVIDER: &str = "openai_custom";
const PROBE_MODEL: &str = "codex-switch-backup-probe";
const PROBE_COMPLETION_TEXT: &str = "isolated continuation verified";
const LONG_SESSION_MIN_BYTES: u64 = 256 * 1024;
const LONG_SESSION_MIN_MESSAGES: usize = 32;
const MAX_HTTP_REQUEST_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct NativeCodexBackupVerifier {
    runtime: RuntimeExecutableIdentity,
}

impl NativeCodexBackupVerifier {
    pub fn discover() -> Result<Self, String> {
        Ok(Self {
            runtime: discover_codex_executable()?,
        })
    }
}

impl MigrationBackupRuntimeVerifier for NativeCodexBackupVerifier {
    fn verify(
        &self,
        isolated_root: &Path,
        manifest: &MigrationBackupManifest,
    ) -> Result<MigrationRuntimeVerification, String> {
        let current = validate_codex_executable(self.runtime.executable.clone())?;
        if current.sha256 != self.runtime.sha256 || current.version != self.runtime.version {
            return Err("native Codex runtime identity changed before verification".to_string());
        }
        verify_with_native_codex(&current, isolated_root, manifest)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeExecutableIdentity {
    executable: PathBuf,
    bytes: u64,
    sha256: String,
    version: String,
}

#[derive(Debug)]
struct PreparedRuntime {
    isolated_root: PathBuf,
    codex_home: PathBuf,
    sqlite_home: PathBuf,
    workspace: PathBuf,
    expected_thread_ids: BTreeSet<String>,
    samples: Vec<RuntimeSample>,
    category_samples: Vec<RuntimeSample>,
    tool_session_count: usize,
    conflict_payload_count: usize,
    conflict_proofs: Vec<MigrationRuntimeConflictProof>,
    capability_conflict_proof: MigrationRuntimeCapabilityConflictProof,
}

#[derive(Debug, Clone)]
struct RuntimeSample {
    thread_id: String,
    restored_path: PathBuf,
    has_tool_pair: bool,
    message_count: usize,
    expected_turn_count: usize,
    expected_turn_ids: Vec<String>,
    expected_turn_content_sha256: Vec<String>,
    bytes: u64,
    categories: BTreeSet<RuntimeCategory>,
}

type PatchedRuntimeState = (
    BTreeSet<String>,
    Vec<RuntimeSample>,
    Vec<RuntimeSample>,
    usize,
    usize,
    Vec<MigrationRuntimeConflictProof>,
    MigrationRuntimeCapabilityConflictProof,
);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum RuntimeCategory {
    Ordinary,
    Long,
    Subagent,
    ConflictCanonical,
    Tool,
}

#[derive(Debug, Clone)]
struct ProbeDescriptor {
    prompt_token: String,
    call_id: String,
    output_token: String,
    command: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StartedTurn {
    thread_id: String,
    turn_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ThreadContentProof {
    runtime_turn_ids: Vec<String>,
}

impl RuntimeCategory {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ordinary => "ordinary",
            Self::Long => "long",
            Self::Subagent => "subagent",
            Self::ConflictCanonical => "conflictCanonical",
            Self::Tool => "tool",
        }
    }
}

fn verify_with_native_codex(
    runtime: &RuntimeExecutableIdentity,
    isolated_root: &Path,
    manifest: &MigrationBackupManifest,
) -> Result<MigrationRuntimeVerification, String> {
    let probe_server = LoopbackResponsesServer::start()?;
    let prepared = prepare_runtime(isolated_root, manifest, &probe_server.base_url())?;
    let mut client = JsonRpcClient::launch(&runtime.executable, &prepared)?;
    client.initialize()?;

    let listed = client.list_all_threads()?;
    let listed_expected = prepared.expected_thread_ids.intersection(&listed).count();
    if listed_expected != prepared.expected_thread_ids.len() {
        return Err(
            "isolated Codex runtime did not list every expected database thread".to_string(),
        );
    }

    let mut resumed = 0_usize;
    for sample in &prepared.samples {
        let read = client.request(
            "thread/read",
            json!({"threadId": sample.thread_id, "includeTurns": true}),
        )?;
        verify_thread_response(&read, &sample.thread_id)?;
        let content_proof = verify_thread_content(&read, sample).map_err(|error| {
            let categories = sample
                .categories
                .iter()
                .map(|category| category.as_str())
                .collect::<Vec<_>>()
                .join(",");
            format!(
                "native Codex thread/read validation failed for {} [{}]: {error}",
                sample.thread_id, categories
            )
        })?;
        let resumed_thread = client.request(
            "thread/resume",
            json!({
                "threadId": sample.thread_id,
                "model": PROBE_MODEL,
                "modelProvider": PROBE_PROVIDER,
                "cwd": prepared.workspace.to_string_lossy(),
                "approvalPolicy": "never"
            }),
        )?;
        verify_thread_response(&resumed_thread, &sample.thread_id)?;
        let resumed_read = client.request(
            "thread/read",
            json!({"threadId": sample.thread_id, "includeTurns": true}),
        )?;
        verify_thread_response(&resumed_read, &sample.thread_id)?;
        if verify_thread_content(&resumed_read, sample)? != content_proof {
            return Err("native Codex resume changed restored turn identity".to_string());
        }
        resumed = resumed.saturating_add(1);
    }
    if resumed != prepared.expected_thread_ids.len() {
        return Err(
            "isolated Codex runtime did not read and resume every expected session".to_string(),
        );
    }

    let representatives = category_representatives(&prepared.category_samples)?;
    let available_categories = representatives
        .values()
        .flat_map(|categories| categories.iter().copied())
        .collect::<BTreeSet<_>>();
    let mut continued_categories = BTreeSet::new();
    let mut continued_session_count = 0_usize;
    let mut verified_tool_sessions = 0_usize;
    for (thread_id, categories) in representatives {
        let sample = prepared
            .category_samples
            .iter()
            .find(|sample| sample.thread_id == thread_id)
            .ok_or_else(|| "runtime category representative disappeared".to_string())?;
        let read = client.request(
            "thread/read",
            json!({"threadId": sample.thread_id, "includeTurns": true}),
        )?;
        verify_thread_response(&read, &sample.thread_id)?;
        let content_proof = verify_thread_content(&read, sample)?;
        let resumed = client.request(
            "thread/resume",
            json!({
                "threadId": sample.thread_id,
                "model": PROBE_MODEL,
                "modelProvider": PROBE_PROVIDER,
                "cwd": prepared.workspace.to_string_lossy(),
                "approvalPolicy": "never"
            }),
        )?;
        verify_thread_response(&resumed, &sample.thread_id)?;
        let resumed_read = client.request(
            "thread/read",
            json!({"threadId": sample.thread_id, "includeTurns": true}),
        )?;
        verify_thread_response(&resumed_read, &sample.thread_id)?;
        if verify_thread_content(&resumed_read, sample)? != content_proof {
            return Err("native Codex category resume changed restored turn identity".to_string());
        }
        let tool_probe = categories.contains(&RuntimeCategory::Tool);
        continue_thread_with_probe(&mut client, &probe_server, &prepared, sample, tool_probe)?;
        continued_session_count = continued_session_count.saturating_add(1);
        continued_categories.extend(categories);
        if tool_probe {
            verified_tool_sessions = verified_tool_sessions.saturating_add(1);
        }
    }
    let tool_round_trip_verified = prepared.tool_session_count == 0 || verified_tool_sessions > 0;
    if continued_categories != available_categories || !tool_round_trip_verified {
        return Err("isolated Codex category continuation coverage is incomplete".to_string());
    }

    client.shutdown()?;
    Ok(MigrationRuntimeVerification {
        expected_session_count: prepared.expected_thread_ids.len(),
        listed_session_count: listed_expected,
        resumed_session_count: resumed,
        continued_session_count,
        tool_session_count: prepared.tool_session_count,
        tool_round_trip_verified,
        available_categories: available_categories
            .into_iter()
            .map(|category| category.as_str().to_string())
            .collect(),
        continued_categories: continued_categories
            .into_iter()
            .map(|category| category.as_str().to_string())
            .collect(),
        conflict_payload_count: prepared.conflict_payload_count,
        conflict_payloads_verified: prepared.conflict_payload_count
            == prepared.conflict_proofs.len(),
        conflict_proofs: prepared.conflict_proofs,
        capability_conflict_proof: Some(prepared.capability_conflict_proof),
        runtime_binary_identity: Some(MigrationRuntimeBinaryIdentity {
            version: runtime.version.clone(),
            bytes: runtime.bytes,
            sha256: runtime.sha256.clone(),
        }),
        verified_at_ms: timestamp_millis()?,
    })
}

fn prepare_runtime(
    isolated_root: &Path,
    manifest: &MigrationBackupManifest,
    probe_base_url: &str,
) -> Result<PreparedRuntime, String> {
    if !isolated_root.is_absolute() || !isolated_root.is_dir() {
        return Err("isolated Codex restore root is invalid".to_string());
    }
    let isolated_root = validate_isolated_directory(isolated_root, isolated_root)?;
    let codex_home = isolated_root.join("canonical");
    let sqlite_home = isolated_root.join("runtime-sqlite");
    let workspace = isolated_root.join("workspace");
    fs::create_dir_all(&codex_home)
        .map_err(|_| "failed to create isolated Codex home".to_string())?;
    fs::create_dir(&sqlite_home)
        .map_err(|_| "failed to create isolated Codex SQLite home".to_string())?;
    fs::create_dir(&workspace)
        .map_err(|_| "failed to create isolated Codex workspace".to_string())?;
    let codex_home = validate_isolated_directory(&isolated_root, &codex_home)?;
    let sqlite_home = validate_isolated_directory(&isolated_root, &sqlite_home)?;
    let workspace = validate_isolated_directory(&isolated_root, &workspace)?;

    copy_runtime_databases(&isolated_root, manifest, &sqlite_home)?;
    write_probe_config(&codex_home, &sqlite_home, probe_base_url)?;
    let (
        expected_thread_ids,
        samples,
        category_samples,
        tool_session_count,
        conflict_payload_count,
        conflict_proofs,
        capability_conflict_proof,
    ) = patch_runtime_state_db(
        &sqlite_home.join("state_5.sqlite"),
        &isolated_root,
        manifest,
    )?;
    if expected_thread_ids.is_empty() || samples.is_empty() {
        return Err("isolated Codex restore has no runtime-readable session sample".to_string());
    }
    Ok(PreparedRuntime {
        isolated_root,
        codex_home,
        sqlite_home,
        workspace,
        expected_thread_ids,
        samples,
        category_samples,
        tool_session_count,
        conflict_payload_count,
        conflict_proofs,
        capability_conflict_proof,
    })
}

fn copy_runtime_databases(
    isolated_root: &Path,
    manifest: &MigrationBackupManifest,
    sqlite_home: &Path,
) -> Result<(), String> {
    let has_goals_source = manifest.entries.iter().any(|entry| {
        entry.kind == MigrationBackupEntryKind::Database
            && entry
                .source_path
                .file_name()
                .is_some_and(|name| name == "goals_1.sqlite")
    });
    let has_canonical_goals = manifest.entries.iter().any(|entry| {
        entry.kind == MigrationBackupEntryKind::Database
            && entry
                .payload_relative_path
                .file_name()
                .is_some_and(|name| name == "canonical-goals_1.sqlite")
    });
    if has_goals_source && !has_canonical_goals {
        return Err("migration backup has goals data but no canonical goals database".to_string());
    }
    for (payload_name, target_name, required) in [
        ("canonical-state_5.sqlite", "state_5.sqlite", true),
        ("canonical-logs_2.sqlite", "logs_2.sqlite", false),
        ("canonical-goals_1.sqlite", "goals_1.sqlite", false),
        ("canonical-memories_1.sqlite", "memories_1.sqlite", false),
    ] {
        let entry = manifest.entries.iter().find(|entry| {
            entry.kind == MigrationBackupEntryKind::Database
                && entry
                    .payload_relative_path
                    .file_name()
                    .is_some_and(|name| name == payload_name)
        });
        let Some(entry) = entry else {
            if required {
                return Err("migration backup has no canonical state database".to_string());
            }
            continue;
        };
        let source = validate_isolated_file(
            isolated_root,
            &isolated_root.join(&entry.payload_relative_path),
        )?;
        quick_check_sqlite(&source)?;
        let goals_source_digest = if target_name == "goals_1.sqlite" {
            let connection = Connection::open_with_flags(
                &source,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .map_err(|_| "failed to open isolated goals database source".to_string())?;
            Some(goals_database_digest(&connection)?)
        } else {
            None
        };
        let target = sqlite_home.join(target_name);
        fs::copy(&source, &target)
            .map_err(|_| "failed to prepare isolated Codex database".to_string())?;
        if fs::metadata(&target)
            .map_err(|_| "isolated Codex database is unavailable".to_string())?
            .len()
            != entry.bytes
        {
            return Err("isolated Codex database size changed".to_string());
        }
        quick_check_sqlite(&target)?;
        if let Some(source_digest) = goals_source_digest {
            let connection = Connection::open_with_flags(
                &target,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .map_err(|_| "failed to open isolated goals database target".to_string())?;
            if goals_database_digest(&connection)? != source_digest {
                return Err("isolated goals database exact row digest changed".to_string());
            }
        }
    }
    quick_check_sqlite(&sqlite_home.join("state_5.sqlite"))
}

fn write_probe_config(
    codex_home: &Path,
    sqlite_home: &Path,
    probe_base_url: &str,
) -> Result<(), String> {
    let mut document = DocumentMut::new();
    document["model"] = value(PROBE_MODEL);
    document["model_provider"] = value(PROBE_PROVIDER);
    document["approval_policy"] = value("never");
    document["sandbox_mode"] = value("read-only");
    document["sqlite_home"] = value(sqlite_home.to_string_lossy().to_string());
    let mut providers = Table::new();
    let mut provider = Table::new();
    provider["name"] = value("Codex Switch Isolated Backup Probe");
    provider["base_url"] = value(probe_base_url);
    provider["wire_api"] = value("responses");
    provider["requires_openai_auth"] = value(false);
    provider["supports_websockets"] = value(false);
    providers[PROBE_PROVIDER] = Item::Table(provider);
    document["model_providers"] = Item::Table(providers);
    atomic_write(
        &codex_home.join("config.toml"),
        document.to_string().as_bytes(),
    )
}

fn patch_runtime_state_db(
    database: &Path,
    isolated_root: &Path,
    manifest: &MigrationBackupManifest,
) -> Result<PatchedRuntimeState, String> {
    let mut entries_by_thread = BTreeMap::<String, Vec<&MigrationBackupEntry>>::new();
    for entry in manifest
        .entries
        .iter()
        .filter(|entry| entry.kind == MigrationBackupEntryKind::Session)
    {
        let thread_id = entry
            .logical_thread_id
            .as_ref()
            .filter(|thread_id| !thread_id.is_empty())
            .ok_or_else(|| {
                "isolated Codex session payload has no logical thread identity".to_string()
            })?;
        entries_by_thread
            .entry(thread_id.clone())
            .or_default()
            .push(entry);
    }
    for entries in entries_by_thread.values_mut() {
        entries.sort_by_key(|entry| {
            (
                !entry
                    .payload_relative_path
                    .starts_with(Path::new("canonical")),
                std::cmp::Reverse(entry.bytes),
                entry.payload_relative_path.clone(),
            )
        });
    }

    let mut connection = Connection::open(database)
        .map_err(|_| "failed to open isolated Codex state database".to_string())?;
    let columns = table_columns(&connection, "threads")?;
    if !columns.iter().any(|column| column == "id")
        || !columns.iter().any(|column| column == "rollout_path")
        || !columns.iter().any(|column| column == "model_provider")
    {
        return Err("isolated Codex threads schema is incompatible".to_string());
    }
    let has_source = columns.iter().any(|column| column == "source");
    let rows = {
        let query = if has_source {
            "SELECT id, rollout_path, source FROM threads"
        } else {
            "SELECT id, rollout_path, NULL FROM threads"
        };
        let mut statement = connection
            .prepare(query)
            .map_err(|_| "failed to query isolated Codex threads".to_string())?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })
            .map_err(|_| "failed to query isolated Codex threads".to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "failed to read isolated Codex threads".to_string())?;
        rows
    };
    let spawn_columns = table_columns(&connection, "thread_spawn_edges")?;
    let mut subagent_threads = BTreeSet::new();
    if spawn_columns
        .iter()
        .any(|column| column == "child_thread_id")
    {
        let mut statement = connection
            .prepare("SELECT child_thread_id FROM thread_spawn_edges")
            .map_err(|_| "failed to query isolated Codex subagent edges".to_string())?;
        let children = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|_| "failed to query isolated Codex subagent edges".to_string())?;
        for child in children {
            subagent_threads.insert(
                child.map_err(|_| "failed to read isolated Codex subagent edge".to_string())?,
            );
        }
    }
    let fixture_template_thread_id = rows
        .first()
        .map(|(thread_id, _, _)| thread_id.clone())
        .ok_or_else(|| "isolated Codex runtime has no thread template".to_string())?;
    let transaction = connection
        .transaction()
        .map_err(|_| "failed to begin isolated Codex database patch".to_string())?;
    let mut expected_thread_ids = BTreeSet::new();
    let mut samples_by_thread = BTreeMap::<String, RuntimeSample>::new();
    let mut conflict_payload_count = 0_usize;
    let mut conflict_proofs = Vec::new();
    for (thread_id, original_path, source_kind) in rows {
        let entries = entries_by_thread.get(&thread_id).ok_or_else(|| {
            "isolated Codex database thread has no restored session payload".to_string()
        })?;
        let matching = entries
            .iter()
            .copied()
            .filter(|entry| path_key(&entry.source_path) == path_key(Path::new(&original_path)))
            .collect::<Vec<_>>();
        if matching.len() != 1 {
            return Err(
                "isolated Codex database primary is not bound to one backup payload".to_string(),
            );
        }
        let selected = matching[0];
        let restored_path = validate_isolated_file(
            isolated_root,
            &isolated_root.join(&selected.payload_relative_path),
        )?;
        let semantic = read_semantic_session(&restored_path).map_err(|error| {
            format!("isolated Codex session validation failed: {:?}", error.kind)
        })?;
        if semantic.thread_id != thread_id {
            return Err("isolated Codex session identity changed".to_string());
        }
        let mut conflict_canonical = false;
        for entry in entries.iter().copied() {
            let candidate_path = validate_isolated_file(
                isolated_root,
                &isolated_root.join(&entry.payload_relative_path),
            )?;
            let candidate = read_semantic_session(&candidate_path).map_err(|error| {
                format!(
                    "isolated conflict payload validation failed: {:?}",
                    error.kind
                )
            })?;
            if semantic_hex(&candidate) != entry.sha256 {
                return Err("isolated conflict payload checksum changed".to_string());
            }
            if path_key(&candidate_path) == path_key(&restored_path) {
                continue;
            }
            let relation = compare_sessions(&semantic, &candidate);
            if matches!(
                relation,
                SessionRelation::Divergent | SessionRelation::Unknown
            ) {
                conflict_canonical = true;
                conflict_payload_count = conflict_payload_count.saturating_add(1);
                conflict_proofs.push(MigrationRuntimeConflictProof {
                    thread_id_sha256: format!("{:x}", Sha256::digest(thread_id.as_bytes())),
                    canonical_payload_relative_path: selected.payload_relative_path.clone(),
                    canonical_sha256: selected.sha256.clone(),
                    recycle_payload_relative_path: entry.payload_relative_path.clone(),
                    recycle_payload_sha256: entry.sha256.clone(),
                    relation: match relation {
                        SessionRelation::Divergent => "divergent",
                        SessionRelation::Unknown => "unknown",
                        _ => unreachable!(),
                    }
                    .to_string(),
                });
            }
        }
        let updated = transaction
            .execute(
                "UPDATE threads SET rollout_path = ?1, model_provider = ?2 WHERE id = ?3",
                (
                    restored_path.to_string_lossy().to_string(),
                    PROBE_PROVIDER,
                    &thread_id,
                ),
            )
            .map_err(|_| "failed to patch isolated Codex thread view".to_string())?;
        if updated != 1 {
            return Err("isolated Codex thread view changed during patching".to_string());
        }
        expected_thread_ids.insert(thread_id.clone());
        let is_subagent = subagent_threads.contains(&thread_id)
            || source_kind
                .as_deref()
                .is_some_and(|source| source.to_ascii_lowercase().contains("subagent"));
        samples_by_thread.insert(
            thread_id.clone(),
            runtime_sample(&semantic, restored_path, is_subagent, conflict_canonical)?,
        );
    }
    let manifest_thread_ids = entries_by_thread.keys().cloned().collect::<BTreeSet<_>>();
    if manifest_thread_ids != expected_thread_ids {
        return Err(
            "isolated Codex session payload is not bound to the complete runtime view".to_string(),
        );
    }
    let (mut category_samples, capability_conflict_proof) = install_runtime_capability_fixture(
        &transaction,
        isolated_root,
        &expected_thread_ids,
        &columns,
        &fixture_template_thread_id,
    )?;
    transaction
        .commit()
        .map_err(|_| "failed to commit isolated Codex database patch".to_string())?;
    drop(connection);
    quick_check_sqlite(database)?;

    let mut samples = samples_by_thread.into_values().collect::<Vec<_>>();
    let tool_session_count = category_samples
        .iter()
        .filter(|sample| sample.has_tool_pair)
        .count();
    samples.sort_by(|left, right| left.thread_id.cmp(&right.thread_id));
    category_samples.sort_by(|left, right| left.thread_id.cmp(&right.thread_id));
    Ok((
        expected_thread_ids,
        samples,
        category_samples,
        tool_session_count,
        conflict_payload_count,
        conflict_proofs,
        capability_conflict_proof,
    ))
}

fn install_runtime_capability_fixture(
    connection: &Connection,
    isolated_root: &Path,
    existing_thread_ids: &BTreeSet<String>,
    thread_columns: &[String],
    template_thread_id: &str,
) -> Result<(Vec<RuntimeSample>, MigrationRuntimeCapabilityConflictProof), String> {
    let fixture_root = isolated_root.join("runtime-capability-fixture");
    fs::create_dir(&fixture_root)
        .map_err(|_| "failed to create isolated runtime capability fixture".to_string())?;
    let mut reserved = existing_thread_ids.clone();
    let definitions = [
        ("ordinary", 2, false, false, false),
        ("long", LONG_SESSION_MIN_MESSAGES, false, false, false),
        ("subagent-tool", 2, true, true, false),
        ("conflict-canonical", 2, false, false, true),
    ];
    let mut samples = Vec::new();
    let mut conflict_identity = None;
    for (index, (label, message_count, with_tool_pair, is_subagent, is_conflict)) in
        definitions.into_iter().enumerate()
    {
        let thread_id = next_capability_thread_id(&reserved, index as u64 + 1)?;
        reserved.insert(thread_id.clone());
        let path = fixture_root.join(format!("{label}.jsonl"));
        write_runtime_capability_session(&path, &thread_id, message_count, with_tool_pair, label)?;
        let inserted = insert_runtime_capability_thread(
            connection,
            thread_columns,
            template_thread_id,
            &thread_id,
            &path,
            if is_subagent { "subAgent" } else { "cli" },
            label,
        )
        .map_err(|_| "failed to insert isolated runtime capability fixture".to_string())?;
        if inserted != 1 {
            return Err("isolated runtime capability fixture was not inserted".to_string());
        }
        let semantic = read_semantic_session(&path)
            .map_err(|_| "isolated runtime capability fixture is invalid".to_string())?;
        if is_conflict {
            conflict_identity = Some((thread_id.clone(), path.clone()));
        }
        samples.push(runtime_sample(&semantic, path, is_subagent, is_conflict)?);
    }
    let available = samples
        .iter()
        .flat_map(|sample| sample.categories.iter().copied())
        .collect::<BTreeSet<_>>();
    if available
        != BTreeSet::from([
            RuntimeCategory::Ordinary,
            RuntimeCategory::Long,
            RuntimeCategory::Subagent,
            RuntimeCategory::ConflictCanonical,
            RuntimeCategory::Tool,
        ])
    {
        return Err("isolated runtime capability fixture categories are incomplete".to_string());
    }
    let (conflict_thread_id, canonical_path) = conflict_identity
        .ok_or_else(|| "isolated runtime conflict capability fixture is missing".to_string())?;
    let recycle_path = fixture_root.join("conflict-recycle.jsonl");
    write_runtime_capability_session(
        &recycle_path,
        &conflict_thread_id,
        2,
        false,
        "conflict-recycle",
    )?;
    let canonical = read_semantic_session(&canonical_path)
        .map_err(|_| "isolated runtime canonical capability fixture is invalid".to_string())?;
    let recycle = read_semantic_session(&recycle_path)
        .map_err(|_| "isolated runtime recycle capability fixture is invalid".to_string())?;
    if compare_sessions(&canonical, &recycle) != SessionRelation::Divergent {
        return Err("isolated runtime conflict capability relation is invalid".to_string());
    }
    Ok((
        samples,
        MigrationRuntimeCapabilityConflictProof {
            fixture_thread_id_sha256: format!(
                "{:x}",
                Sha256::digest(conflict_thread_id.as_bytes())
            ),
            canonical_bytes: canonical.bytes,
            canonical_sha256: semantic_hex(&canonical),
            recycle_bytes: recycle.bytes,
            recycle_sha256: semantic_hex(&recycle),
            relation: "divergent".to_string(),
        },
    ))
}

fn insert_runtime_capability_thread(
    connection: &Connection,
    columns: &[String],
    template_thread_id: &str,
    thread_id: &str,
    rollout_path: &Path,
    source: &str,
    label: &str,
) -> rusqlite::Result<usize> {
    let quote = |column: &str| format!("\"{}\"", column.replace('"', "\"\""));
    let column_list = columns
        .iter()
        .map(|column| quote(column))
        .collect::<Vec<_>>()
        .join(", ");
    let selections = columns
        .iter()
        .map(|column| match column.as_str() {
            "id" => "?1".to_string(),
            "rollout_path" => "?2".to_string(),
            "model_provider" => "?3".to_string(),
            "source" => "?4".to_string(),
            "title" | "first_user_message" | "preview" => "?5".to_string(),
            "archived" => "0".to_string(),
            "has_user_event" => "1".to_string(),
            _ => quote(column),
        })
        .collect::<Vec<_>>()
        .join(", ");
    connection.execute(
        &format!(
            "INSERT INTO threads ({column_list}) SELECT {selections} FROM threads WHERE id = ?6"
        ),
        rusqlite::params![
            thread_id,
            rollout_path.to_string_lossy().to_string(),
            PROBE_PROVIDER,
            source,
            label,
            template_thread_id,
        ],
    )
}

fn next_capability_thread_id(
    existing_thread_ids: &BTreeSet<String>,
    initial_serial: u64,
) -> Result<String, String> {
    for offset in 0..1_000_000_u64 {
        let serial = initial_serial
            .checked_add(offset)
            .ok_or_else(|| "isolated runtime capability identity overflowed".to_string())?;
        let candidate = format!("90000000-0000-4000-8000-{serial:012}");
        if !existing_thread_ids.contains(&candidate) {
            return Ok(candidate);
        }
    }
    Err("isolated runtime capability identity space is exhausted".to_string())
}

fn write_runtime_capability_session(
    path: &Path,
    thread_id: &str,
    message_count: usize,
    with_tool_pair: bool,
    label: &str,
) -> Result<(), String> {
    let mut lines = vec![json!({
        "type":"session_meta",
        "timestamp":"2026-08-13T00:00:00Z",
        "payload":{
            "session_id":thread_id,
            "id":thread_id,
            "timestamp":"2026-08-13T00:00:00Z",
            "cwd":".",
            "originator":"codex-switch-runtime-capability",
            "cli_version":"0.3.0",
            "source":"cli",
            "thread_source":"user",
            "model_provider":PROBE_PROVIDER,
            "base_instructions":{"text":""},
            "history_mode":"legacy",
            "context_window":{"window_id":thread_id}
        }
    })];
    for index in 0..message_count {
        let turn_id = format!("turn-{thread_id}-{index}");
        let user = format!("{label}-{index}");
        let assistant = format!("reply-{label}-{index}");
        let timestamp = format!("2026-08-13T00:00:{index:02}Z");
        lines.push(json!({
            "type":"event_msg",
            "timestamp":timestamp,
            "payload":{"type":"task_started","turn_id":turn_id,"started_at":1786579200_u64 + index as u64,"model_context_window":258400,"collaboration_mode_kind":"default"}
        }));
        lines.push(json!({
            "type":"turn_context",
            "timestamp":timestamp,
            "payload":{"turn_id":turn_id}
        }));
        lines.push(json!({"type":"response_item","timestamp":timestamp,"payload":{"type":"message","id":format!("msg-{thread_id}-{index}-user"),"role":"user","content":[{"type":"input_text","text":user}],"internal_chat_message_metadata_passthrough":{"turn_id":turn_id}}}));
        lines.push(json!({"type":"event_msg","timestamp":timestamp,"payload":{"type":"user_message","message":user}}));
        lines.push(json!({"type":"response_item","timestamp":timestamp,"payload":{"type":"message","id":format!("msg-{thread_id}-{index}-assistant"),"role":"assistant","content":[{"type":"output_text","text":assistant}],"internal_chat_message_metadata_passthrough":{"turn_id":turn_id}}}));
        lines.push(
            json!({"type":"event_msg","timestamp":timestamp,"payload":{"type":"agent_message","message":assistant}}),
        );
        if with_tool_pair && index + 1 == message_count {
            lines.push(json!({"type":"response_item","timestamp":timestamp,"payload":{"type":"function_call","id":format!("call-{thread_id}"),"call_id":"capability-tool-call","name":"shell_command","arguments":"{}","internal_chat_message_metadata_passthrough":{"turn_id":turn_id}}}));
            lines.push(json!({"type":"response_item","timestamp":timestamp,"payload":{"type":"function_call_output","id":format!("output-{thread_id}"),"call_id":"capability-tool-call","output":"ok","internal_chat_message_metadata_passthrough":{"turn_id":turn_id}}}));
        }
        lines.push(json!({
            "type":"event_msg",
            "timestamp":timestamp,
            "payload":{"type":"task_complete","turn_id":turn_id,"last_agent_message":assistant,"started_at":1786579200_u64 + index as u64,"completed_at":1786579201_u64 + index as u64,"duration_ms":1000,"time_to_first_token_ms":1}
        }));
    }
    let body = lines
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    atomic_write(path, body.as_bytes())
}

fn runtime_sample(
    semantic: &SemanticSession,
    restored_path: PathBuf,
    is_subagent: bool,
    conflict_canonical: bool,
) -> Result<RuntimeSample, String> {
    let has_tool_pair =
        semantic.tool_call_count > 0 && semantic.tool_call_count == semantic.tool_result_count;
    let mut categories = BTreeSet::new();
    if semantic.message_count >= LONG_SESSION_MIN_MESSAGES
        || semantic.bytes >= LONG_SESSION_MIN_BYTES
    {
        categories.insert(RuntimeCategory::Long);
    }
    if has_tool_pair {
        categories.insert(RuntimeCategory::Tool);
    }
    if is_subagent {
        categories.insert(RuntimeCategory::Subagent);
    }
    if conflict_canonical {
        categories.insert(RuntimeCategory::ConflictCanonical);
    }
    if categories.is_empty() {
        categories.insert(RuntimeCategory::Ordinary);
    }
    let expected_turn_ids = semantic
        .turn_contexts
        .iter()
        .map(|turn| turn.turn_id.clone())
        .collect::<Vec<_>>();
    let expected_turn_count = if expected_turn_ids.is_empty() {
        runtime_user_turn_count(&restored_path)?.max(usize::from(semantic.message_count > 0))
    } else {
        expected_turn_ids.len()
    };
    let expected_turn_content_sha256 = runtime_turn_content_sha256(&restored_path)?;
    if expected_turn_content_sha256.len() != expected_turn_count {
        return Err(format!(
            "isolated Codex session turn content could not be bound for {}: expected {} turns, bound {}",
            semantic.thread_id,
            expected_turn_count,
            expected_turn_content_sha256.len()
        ));
    }
    Ok(RuntimeSample {
        thread_id: semantic.thread_id.clone(),
        restored_path,
        has_tool_pair,
        message_count: semantic.message_count,
        expected_turn_count,
        expected_turn_ids,
        expected_turn_content_sha256,
        bytes: semantic.bytes,
        categories,
    })
}

fn semantic_hex(semantic: &SemanticSession) -> String {
    semantic
        .raw_sha256
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn category_representatives(
    samples: &[RuntimeSample],
) -> Result<BTreeMap<String, BTreeSet<RuntimeCategory>>, String> {
    let available = samples
        .iter()
        .flat_map(|sample| sample.categories.iter().copied())
        .collect::<BTreeSet<_>>();
    if available.is_empty() {
        return Err("isolated Codex runtime has no category-bound sample".to_string());
    }
    let mut representatives = BTreeMap::<String, BTreeSet<RuntimeCategory>>::new();
    for category in available {
        let selected = samples
            .iter()
            .filter(|sample| sample.categories.contains(&category))
            .max_by_key(|sample| (sample.message_count, sample.bytes, sample.thread_id.clone()))
            .ok_or_else(|| "isolated Codex runtime category sample disappeared".to_string())?;
        representatives
            .entry(selected.thread_id.clone())
            .or_default()
            .insert(category);
    }
    Ok(representatives)
}

fn continue_thread_with_probe(
    client: &mut JsonRpcClient,
    probe_server: &LoopbackResponsesServer,
    prepared: &PreparedRuntime,
    sample: &RuntimeSample,
    tool_probe: bool,
) -> Result<(), String> {
    let before = read_semantic_session(&sample.restored_path)
        .map_err(|_| "isolated continuation source is unreadable".to_string())?;
    let probe = probe_server.begin_probe(tool_probe)?;
    let started = client.request(
        "turn/start",
        json!({
            "threadId": sample.thread_id,
            "input": [{"type": "text", "text": probe.prompt_token}],
            "model": PROBE_MODEL,
            "effort": "low",
            "approvalPolicy": "never",
            "sandboxPolicy": {"type": "readOnly", "networkAccess": false},
            "cwd": prepared.workspace.to_string_lossy()
        }),
    )?;
    let started = parse_started_turn(&started, &sample.thread_id)?;
    client.wait_for_turn_completed(&started)?;
    probe_server.finish_probe()?;
    let after = read_semantic_session_after_runtime_flush(
        &sample.restored_path,
        &before,
        &started,
        &probe.prompt_token,
    )?;
    if after.thread_id != sample.thread_id
        || after.raw_sha256 == before.raw_sha256
        || after.message_count < before.message_count.saturating_add(2)
        || after
            .turn_contexts
            .iter()
            .all(|turn| turn.turn_id != started.turn_id)
    {
        return Err(
            "native Codex continuation did not append a complete isolated turn".to_string(),
        );
    }
    verify_persisted_probe_turn(&sample.restored_path, &started.turn_id, &probe.prompt_token)?;
    if tool_probe
        && (after.tool_call_count <= before.tool_call_count
            || after.tool_result_count <= before.tool_result_count
            || after.tool_call_count != after.tool_result_count)
    {
        return Err(
            "native Codex tool continuation did not preserve a call/output pair".to_string(),
        );
    }
    if tool_probe {
        verify_persisted_probe_tool(
            &sample.restored_path,
            &started.turn_id,
            &probe.call_id,
            &probe.command,
            &probe.output_token,
        )?;
    }
    Ok(())
}

fn read_semantic_session_after_runtime_flush(
    path: &Path,
    before: &SemanticSession,
    started: &StartedTurn,
    prompt_token: &str,
) -> Result<SemanticSession, String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last_observation = "no readable observation".to_string();
    loop {
        match read_semantic_session(path) {
            Ok(session)
                if session.thread_id == started.thread_id
                    && session.raw_sha256 != before.raw_sha256
                    && session.message_count >= before.message_count.saturating_add(2)
                    && session
                        .turn_contexts
                        .iter()
                        .any(|turn| turn.turn_id == started.turn_id)
                    && verify_persisted_probe_turn(path, &started.turn_id, prompt_token)
                        .is_ok() =>
            {
                return Ok(session);
            }
            Ok(session) if Instant::now() < deadline => {
                let persisted =
                    verify_persisted_probe_turn(path, &started.turn_id, prompt_token).is_ok();
                last_observation = format!(
                    "threadMatch={} rawChanged={} messageDelta={} turnPresent={} persisted={}",
                    session.thread_id == started.thread_id,
                    session.raw_sha256 != before.raw_sha256,
                    session.message_count.saturating_sub(before.message_count),
                    session
                        .turn_contexts
                        .iter()
                        .any(|turn| turn.turn_id == started.turn_id),
                    persisted
                );
                thread::sleep(Duration::from_millis(50));
            }
            Ok(_) => {
                return Err(format!(
                    "native Codex continuation did not finish persisting before the deadline: {last_observation}"
                ));
            }
            Err(error)
                if error.kind == SemanticErrorKind::ChangedDuringRead
                    && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => {
                return Err(format!(
                    "isolated continued session is unreadable: {:?}: {}",
                    error.kind, error.safe_detail
                ));
            }
        }
    }
}

fn verify_persisted_probe_turn(
    path: &Path,
    expected_turn_id: &str,
    prompt_token: &str,
) -> Result<(), String> {
    let mut turn_context_seen = false;
    let mut user_seen = false;
    let mut assistant_seen = false;
    visit_jsonl(path, |value| {
        if value.get("type").and_then(Value::as_str) == Some("turn_context")
            && value
                .get("payload")
                .and_then(|payload| payload.get("turn_id"))
                .and_then(Value::as_str)
                == Some(expected_turn_id)
        {
            turn_context_seen = true;
        }
        let Some(payload) = value.get("payload") else {
            return;
        };
        if value.get("type").and_then(Value::as_str) != Some("response_item")
            || payload.get("type").and_then(Value::as_str) != Some("message")
            || payload_turn_id(payload) != Some(expected_turn_id)
        {
            return;
        }
        match payload.get("role").and_then(Value::as_str) {
            Some("user") if value_contains_exact_string(payload, prompt_token) => user_seen = true,
            Some("assistant") if value_contains_exact_string(payload, PROBE_COMPLETION_TEXT) => {
                assistant_seen = true
            }
            _ => {}
        }
    })?;
    if turn_context_seen && user_seen && assistant_seen {
        Ok(())
    } else {
        Err("native Codex continuation was not fully persisted".to_string())
    }
}

fn verify_persisted_probe_tool(
    path: &Path,
    expected_turn_id: &str,
    expected_call_id: &str,
    expected_command: &str,
    expected_output_token: &str,
) -> Result<(), String> {
    let mut call_seen = false;
    let mut output_seen = false;
    visit_jsonl(path, |value| {
        let Some(payload) = value.get("payload") else {
            return;
        };
        if value.get("type").and_then(Value::as_str) != Some("response_item")
            || payload.get("call_id").and_then(Value::as_str) != Some(expected_call_id)
            || payload_turn_id(payload) != Some(expected_turn_id)
        {
            return;
        }
        match payload.get("type").and_then(Value::as_str) {
            Some("function_call") => {
                let command_matches = payload
                    .get("arguments")
                    .and_then(Value::as_str)
                    .and_then(|arguments| serde_json::from_str::<Value>(arguments).ok())
                    .and_then(|arguments| {
                        arguments
                            .get("command")
                            .and_then(Value::as_str)
                            .map(|command| command == expected_command)
                    })
                    .unwrap_or(false);
                if command_matches {
                    call_seen = true;
                }
            }
            Some("function_call_output")
                if call_seen
                    && payload.get("output").is_some_and(|output| {
                        value_contains_string_fragment(output, expected_output_token)
                    }) =>
            {
                output_seen = true;
            }
            _ => {}
        }
    })?;
    if call_seen && output_seen {
        Ok(())
    } else {
        Err("native Codex tool continuation did not persist the exact probe output".to_string())
    }
}

fn payload_turn_id(payload: &Value) -> Option<&str> {
    payload
        .get("internal_chat_message_metadata_passthrough")
        .and_then(|metadata| metadata.get("turn_id"))
        .and_then(Value::as_str)
}

fn runtime_user_turn_count(path: &Path) -> Result<usize, String> {
    let mut response_users = 0_usize;
    let mut event_users = 0_usize;
    visit_jsonl(path, |value| {
        let outer = value.get("type").and_then(Value::as_str);
        let payload = value.get("payload");
        if outer == Some("response_item")
            && payload
                .and_then(|payload| payload.get("type"))
                .and_then(Value::as_str)
                == Some("message")
            && payload
                .and_then(|payload| payload.get("role"))
                .and_then(Value::as_str)
                == Some("user")
        {
            response_users = response_users.saturating_add(1);
        }
        if outer == Some("event_msg")
            && payload
                .and_then(|payload| payload.get("type"))
                .and_then(Value::as_str)
                == Some("user_message")
        {
            event_users = event_users.saturating_add(1);
        }
    })?;
    Ok(response_users.max(event_users))
}

fn runtime_turn_content_sha256(path: &Path) -> Result<Vec<String>, String> {
    let mut turn_ids = Vec::new();
    let mut invalid_turn_context = false;
    visit_jsonl(path, |value| {
        if value.get("type").and_then(Value::as_str) != Some("turn_context") {
            return;
        }
        let turn_id = value
            .get("payload")
            .and_then(|payload| payload.get("turn_id"))
            .and_then(Value::as_str)
            .filter(|turn_id| !turn_id.is_empty());
        if let Some(turn_id) = turn_id {
            turn_ids.push(turn_id.to_string());
        } else {
            invalid_turn_context = true;
        }
    })?;
    if invalid_turn_context {
        return Err("isolated Codex session has an invalid turn context".to_string());
    }

    if !turn_ids.is_empty() {
        let indexes = turn_ids
            .iter()
            .enumerate()
            .map(|(index, turn_id)| (turn_id.clone(), index))
            .collect::<BTreeMap<_, _>>();
        if indexes.len() != turn_ids.len() {
            return Err("isolated Codex session has duplicate turn identities".to_string());
        }
        let mut response_messages = vec![Vec::new(); turn_ids.len()];
        let mut event_messages = vec![Vec::new(); turn_ids.len()];
        let mut seen_turn_context = vec![false; turn_ids.len()];
        let mut completed_turn = vec![false; turn_ids.len()];
        let mut active_turn = None::<usize>;
        let mut unbound_message = false;
        visit_jsonl(path, |value| {
            let outer = value.get("type").and_then(Value::as_str);
            let payload = value.get("payload");
            if outer == Some("turn_context") {
                active_turn = payload
                    .and_then(|payload| payload.get("turn_id"))
                    .and_then(Value::as_str)
                    .and_then(|turn_id| indexes.get(turn_id).copied());
                if let Some(index) = active_turn {
                    seen_turn_context[index] = true;
                    completed_turn[index] = false;
                }
                return;
            }
            if outer == Some("event_msg")
                && payload
                    .and_then(|payload| payload.get("type"))
                    .and_then(Value::as_str)
                    == Some("task_complete")
            {
                let completed = payload
                    .and_then(|payload| payload.get("turn_id"))
                    .and_then(Value::as_str)
                    .and_then(|turn_id| indexes.get(turn_id).copied())
                    .or(active_turn);
                if let Some(index) = completed {
                    completed_turn[index] = true;
                    if active_turn == Some(index) {
                        active_turn = None;
                    }
                }
                return;
            }
            let Some((role, text)) = runtime_message_text(outer, payload) else {
                return;
            };
            let explicit_turn_id = payload.and_then(payload_turn_id);
            let index = if let Some(turn_id) = explicit_turn_id {
                let Some(index) = indexes.get(turn_id).copied() else {
                    unbound_message = true;
                    return;
                };
                // Native Codex writes bootstrap instructions before the first
                // turn_context while already stamping the future turn id.
                // Those records are not conversation items in thread/read.
                if !seen_turn_context[index] || completed_turn[index] {
                    return;
                }
                index
            } else if let Some(index) = active_turn {
                index
            } else {
                // Task/bootstrap lifecycle mirrors before the first context
                // are outside the restored conversation projection.
                return;
            };
            match outer {
                Some("response_item") => response_messages[index].push((role, text)),
                Some("event_msg") => event_messages[index].push((role, text)),
                _ => {}
            }
        })?;
        if unbound_message {
            return Err("isolated Codex session has a message outside a turn".to_string());
        }
        return Ok(response_messages
            .into_iter()
            .zip(event_messages)
            .map(|(responses, events)| {
                if responses.is_empty() {
                    runtime_messages_digest(&events)
                } else {
                    runtime_messages_digest(&responses)
                }
            })
            .collect());
    }

    let mut response_messages = Vec::<Vec<(String, String)>>::new();
    let mut event_messages = Vec::<Vec<(String, String)>>::new();
    let mut turn_has_assistant = Vec::new();
    let mut active_turn = None::<usize>;
    visit_jsonl(path, |value| {
        let outer = value.get("type").and_then(Value::as_str);
        let payload = value.get("payload");
        let Some((role, text)) = runtime_message_text(outer, payload) else {
            return;
        };
        if role == "user" && active_turn.is_none_or(|index| turn_has_assistant[index]) {
            response_messages.push(Vec::new());
            event_messages.push(Vec::new());
            turn_has_assistant.push(false);
            active_turn = Some(response_messages.len().saturating_sub(1));
        }
        let Some(index) = active_turn else {
            return;
        };
        if role == "assistant" {
            turn_has_assistant[index] = true;
        }
        match outer {
            Some("response_item") => response_messages[index].push((role, text)),
            Some("event_msg") => event_messages[index].push((role, text)),
            _ => {}
        }
    })?;
    Ok(response_messages
        .into_iter()
        .zip(event_messages)
        .map(|(responses, events)| {
            if responses.is_empty() {
                runtime_messages_digest(&events)
            } else {
                runtime_messages_digest(&responses)
            }
        })
        .collect())
}

fn runtime_messages_digest(messages: &[(String, String)]) -> String {
    let mut digest = Sha256::new();
    for (role, text) in messages {
        update_runtime_turn_digest(&mut digest, role, text.as_bytes());
    }
    format!("{:x}", digest.finalize())
}

fn runtime_message_text(outer: Option<&str>, payload: Option<&Value>) -> Option<(String, String)> {
    let payload = payload?;
    match (outer, payload.get("type").and_then(Value::as_str)) {
        (Some("response_item"), Some("message")) => {
            let role = payload.get("role").and_then(Value::as_str)?;
            if !matches!(role, "user" | "assistant") {
                return None;
            }
            let text = response_message_text(payload);
            if text.is_empty() {
                return None;
            }
            Some((role.to_string(), text))
        }
        (Some("event_msg"), Some("user_message")) => Some((
            "user".to_string(),
            payload.get("message")?.as_str()?.to_string(),
        )),
        (Some("event_msg"), Some("agent_message")) => Some((
            "assistant".to_string(),
            payload.get("message")?.as_str()?.to_string(),
        )),
        _ => None,
    }
}

fn response_message_text(payload: &Value) -> String {
    let Some(content) = payload.get("content").and_then(Value::as_array) else {
        return String::new();
    };
    content
        .iter()
        .filter_map(|part| {
            part.get("text")
                .and_then(Value::as_str)
                .or_else(|| part.get("value").and_then(Value::as_str))
        })
        .collect::<Vec<_>>()
        .join("")
}

fn update_runtime_turn_digest(digest: &mut Sha256, role: &str, text: &[u8]) {
    digest.update((role.len() as u64).to_le_bytes());
    digest.update(role.as_bytes());
    digest.update((text.len() as u64).to_le_bytes());
    digest.update(text);
}

fn runtime_thread_turn_content_sha256(turns: &[Value]) -> Result<Vec<String>, String> {
    let mut content_sha256 = Vec::with_capacity(turns.len());
    for turn in turns {
        let items = turn
            .get("items")
            .and_then(Value::as_array)
            .ok_or_else(|| "native Codex thread response omitted turn items".to_string())?;
        let mut digest = Sha256::new();
        for item in items {
            match item.get("type").and_then(Value::as_str) {
                Some("userMessage") => {
                    let text = item
                        .get("content")
                        .and_then(Value::as_array)
                        .map(|parts| {
                            parts
                                .iter()
                                .filter_map(|part| part.get("text").and_then(Value::as_str))
                                .collect::<Vec<_>>()
                                .join("")
                        })
                        .unwrap_or_default();
                    update_runtime_turn_digest(&mut digest, "user", text.as_bytes());
                }
                Some("agentMessage") => {
                    let text = item.get("text").and_then(Value::as_str).unwrap_or_default();
                    update_runtime_turn_digest(&mut digest, "assistant", text.as_bytes());
                }
                _ => {}
            }
        }
        content_sha256.push(format!("{:x}", digest.finalize()));
    }
    Ok(content_sha256)
}

fn visit_jsonl(path: &Path, mut visitor: impl FnMut(&Value)) -> Result<(), String> {
    let file = fs::File::open(path)
        .map_err(|_| "isolated session payload could not be reopened".to_string())?;
    let mut reader = BufReader::new(file);
    loop {
        let mut line = Vec::new();
        let mut limited = reader
            .by_ref()
            .take((MAX_RPC_LINE_BYTES as u64).saturating_add(1));
        let read = limited
            .read_until(b'\n', &mut line)
            .map_err(|_| "isolated session payload could not be reread".to_string())?;
        if read == 0 {
            break;
        }
        if line.len() > MAX_RPC_LINE_BYTES {
            return Err("isolated session payload entry exceeded the limit".to_string());
        }
        while line
            .last()
            .is_some_and(|byte| matches!(byte, b'\r' | b'\n'))
        {
            line.pop();
        }
        let value = serde_json::from_slice::<Value>(&line)
            .map_err(|_| "isolated session payload became invalid".to_string())?;
        visitor(&value);
    }
    Ok(())
}

fn table_columns(connection: &Connection, table: &str) -> Result<Vec<String>, String> {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|_| "failed to inspect isolated Codex database schema".to_string())?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|_| "failed to inspect isolated Codex database schema".to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "failed to inspect isolated Codex database schema".to_string())?;
    Ok(columns)
}

fn validate_isolated_directory(root: &Path, candidate: &Path) -> Result<PathBuf, String> {
    if !root.is_absolute() || !candidate.is_absolute() {
        return Err("isolated Codex directory path is invalid".to_string());
    }
    let requested = candidate.to_path_buf();
    let original = fs::symlink_metadata(candidate)
        .map_err(|_| "isolated Codex directory metadata is unavailable".to_string())?;
    if !original.is_dir() || metadata_is_link_or_reparse(&original) {
        return Err("isolated Codex directory is unsafe".to_string());
    }
    let root = fs::canonicalize(root)
        .map_err(|_| "isolated Codex root could not be resolved".to_string())?;
    let candidate = fs::canonicalize(candidate)
        .map_err(|_| "isolated Codex directory could not be resolved".to_string())?;
    if !candidate.starts_with(&root) {
        return Err("isolated Codex directory escaped its restore root".to_string());
    }
    validate_isolated_ancestry(&root, &candidate)?;
    let metadata = fs::symlink_metadata(&candidate)
        .map_err(|_| "isolated Codex directory metadata is unavailable".to_string())?;
    if !metadata.is_dir() || metadata_is_link_or_reparse(&metadata) {
        return Err("isolated Codex directory is unsafe".to_string());
    }
    Ok(requested)
}

fn validate_isolated_file(root: &Path, candidate: &Path) -> Result<PathBuf, String> {
    if !root.is_absolute() || !candidate.is_absolute() {
        return Err("isolated Codex file path is invalid".to_string());
    }
    let requested = candidate.to_path_buf();
    let original = fs::symlink_metadata(candidate)
        .map_err(|_| "isolated Codex file metadata is unavailable".to_string())?;
    if !original.is_file() || metadata_is_link_or_reparse(&original) {
        return Err("isolated Codex file is unsafe".to_string());
    }
    let root = fs::canonicalize(root)
        .map_err(|_| "isolated Codex root could not be resolved".to_string())?;
    let candidate = fs::canonicalize(candidate)
        .map_err(|_| "isolated Codex file could not be resolved".to_string())?;
    if !candidate.starts_with(&root) {
        return Err("isolated Codex file escaped its restore root".to_string());
    }
    validate_isolated_ancestry(&root, &candidate)?;
    Ok(requested)
}

fn validate_isolated_ancestry(root: &Path, candidate: &Path) -> Result<(), String> {
    let relative = candidate
        .strip_prefix(root)
        .map_err(|_| "isolated Codex path escaped its restore root".to_string())?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component.as_os_str());
        let metadata = fs::symlink_metadata(&current)
            .map_err(|_| "isolated Codex path metadata is unavailable".to_string())?;
        if metadata_is_link_or_reparse(&metadata) {
            return Err("isolated Codex path ancestry is unsafe".to_string());
        }
    }
    Ok(())
}

fn quick_check_sqlite(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| "failed to inspect isolated Codex database".to_string())?;
    if !metadata.is_file() || metadata_is_link_or_reparse(&metadata) {
        return Err("isolated Codex database path is unsafe".to_string());
    }
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| "failed to open isolated Codex database".to_string())?;
    let result: String = connection
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(|_| "failed to verify isolated Codex database".to_string())?;
    if result == "ok" {
        Ok(())
    } else {
        Err("isolated Codex database failed quick_check".to_string())
    }
}

struct LoopbackResponsesServer {
    address: std::net::SocketAddr,
    state: Arc<Mutex<ProbeServerState>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

#[derive(Default)]
struct ProbeServerState {
    next_probe: u64,
    active: Option<ActiveProbe>,
    error: Option<&'static str>,
}

struct ActiveProbe {
    descriptor: ProbeDescriptor,
    tool_probe: bool,
    request_count: usize,
    tool_output_seen: bool,
}

impl LoopbackResponsesServer {
    fn start() -> Result<Self, String> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .map_err(|_| "failed to bind isolated Responses probe".to_string())?;
        let address = listener
            .local_addr()
            .map_err(|_| "isolated Responses probe address is unavailable".to_string())?;
        let state = Arc::new(Mutex::new(ProbeServerState::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_state = Arc::clone(&state);
        let worker_stop = Arc::clone(&stop);
        let worker = thread::Builder::new()
            .name("codex-backup-responses-probe".to_string())
            .spawn(move || {
                for incoming in listener.incoming() {
                    if worker_stop.load(Ordering::Acquire) {
                        break;
                    }
                    match incoming {
                        Ok(mut stream) => {
                            if let Err(code) = handle_probe_request(&mut stream, &worker_state) {
                                let mut state = worker_state
                                    .lock()
                                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                                state.error = Some(code);
                                let _ = write_http_response(
                                    &mut stream,
                                    500,
                                    b"isolated probe failed",
                                    "text/plain",
                                );
                            }
                        }
                        Err(_) => {
                            let mut state = worker_state
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                            state.error = Some("probeAcceptFailed");
                            break;
                        }
                    }
                }
            })
            .map_err(|_| "failed to start isolated Responses probe".to_string())?;
        Ok(Self {
            address,
            state,
            stop,
            worker: Some(worker),
        })
    }

    fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }

    fn begin_probe(&self, tool_probe: bool) -> Result<ProbeDescriptor, String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.error.is_some() || state.active.is_some() {
            return Err("isolated Responses probe is not idle".to_string());
        }
        state.next_probe = state
            .next_probe
            .checked_add(1)
            .ok_or_else(|| "isolated Responses probe counter overflowed".to_string())?;
        let prompt_token = format!("codex-switch-isolated-continuation-{}", state.next_probe);
        let call_id = format!("codex_switch_runtime_tool_call_{}", state.next_probe);
        let output_token = format!("CODEX_SWITCH_RUNTIME_TOOL_OUTPUT_{}", state.next_probe);
        let command = format!("Write-Output {output_token}");
        let descriptor = ProbeDescriptor {
            prompt_token,
            call_id,
            output_token,
            command,
        };
        state.active = Some(ActiveProbe {
            descriptor: descriptor.clone(),
            tool_probe,
            request_count: 0,
            tool_output_seen: false,
        });
        Ok(descriptor)
    }

    fn finish_probe(&self) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.error.take().is_some() {
            state.active.take();
            return Err("isolated Responses probe rejected a runtime request".to_string());
        }
        let active = state
            .active
            .take()
            .ok_or_else(|| "isolated Responses probe did not receive a turn".to_string())?;
        let expected_requests = if active.tool_probe { 2 } else { 1 };
        if active.request_count != expected_requests
            || (active.tool_probe && !active.tool_output_seen)
        {
            return Err("isolated Responses probe did not complete its expected flow".to_string());
        }
        Ok(())
    }
}

impl Drop for LoopbackResponsesServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Ok(stream) = TcpStream::connect(self.address) {
            let _ = stream.shutdown(Shutdown::Both);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn handle_probe_request(
    stream: &mut TcpStream,
    state: &Arc<Mutex<ProbeServerState>>,
) -> Result<(), &'static str> {
    let (path, body) = read_http_json(stream)?;
    if path != "/v1/responses" {
        return Err("probeUnexpectedPath");
    }
    let mut state = state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let active = state.active.as_mut().ok_or("probeWithoutActiveTurn")?;
    active.request_count = active.request_count.saturating_add(1);
    let response = if !active.tool_probe {
        if active.request_count != 1
            || !value_contains_exact_string(&body, &active.descriptor.prompt_token)
        {
            return Err("probeTextFlowMismatch");
        }
        text_probe_sse(active.request_count)
    } else if active.request_count == 1 {
        if !value_contains_exact_string(&body, &active.descriptor.prompt_token) {
            return Err("probeToolPromptMismatch");
        }
        tool_probe_sse(active.request_count, &active.descriptor)
    } else if active.request_count == 2 {
        if !request_contains_exact_tool_output(&body, &active.descriptor) {
            return Err("probeToolOutputMissing");
        }
        active.tool_output_seen = true;
        text_probe_sse(active.request_count)
    } else {
        return Err("probeTooManyRequests");
    };
    drop(state);
    write_http_response(stream, 200, response.as_bytes(), "text/event-stream")
        .map_err(|_| "probeResponseWriteFailed")
}

fn value_contains_exact_string(value: &Value, expected: &str) -> bool {
    match value {
        Value::String(value) => value == expected,
        Value::Array(values) => values
            .iter()
            .any(|value| value_contains_exact_string(value, expected)),
        Value::Object(values) => values
            .values()
            .any(|value| value_contains_exact_string(value, expected)),
        _ => false,
    }
}

fn value_contains_string_fragment(value: &Value, expected: &str) -> bool {
    match value {
        Value::String(value) => value.contains(expected),
        Value::Array(values) => values
            .iter()
            .any(|value| value_contains_string_fragment(value, expected)),
        Value::Object(values) => values
            .values()
            .any(|value| value_contains_string_fragment(value, expected)),
        _ => false,
    }
}

fn request_contains_exact_tool_output(body: &Value, descriptor: &ProbeDescriptor) -> bool {
    body.get("input")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.get("type").and_then(Value::as_str) == Some("function_call_output")
                    && item.get("call_id").and_then(Value::as_str)
                        == Some(descriptor.call_id.as_str())
                    && item.get("output").is_some_and(|output| {
                        value_contains_string_fragment(output, &descriptor.output_token)
                    })
            })
        })
}

fn read_http_json(stream: &mut TcpStream) -> Result<(String, Value), &'static str> {
    stream
        .set_read_timeout(Some(RPC_TIMEOUT))
        .map_err(|_| "probeReadTimeoutFailed")?;
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 16 * 1024];
    let header_end = loop {
        let read = stream.read(&mut buffer).map_err(|_| "probeReadFailed")?;
        if read == 0 {
            return Err("probeRequestTruncated");
        }
        bytes.extend_from_slice(&buffer[..read]);
        if bytes.len() > MAX_HTTP_REQUEST_BYTES {
            return Err("probeRequestOversized");
        }
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let header = std::str::from_utf8(&bytes[..header_end]).map_err(|_| "probeHeaderInvalid")?;
    let mut lines = header.split("\r\n");
    let request_line = lines.next().ok_or("probeRequestLineMissing")?;
    let mut request_parts = request_line.split_whitespace();
    if request_parts.next() != Some("POST") {
        return Err("probeMethodInvalid");
    }
    let path = request_parts.next().ok_or("probePathMissing")?.to_string();
    let content_length = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .ok_or("probeLengthMissing")?;
    if content_length > MAX_HTTP_REQUEST_BYTES.saturating_sub(header_end) {
        return Err("probeRequestOversized");
    }
    while bytes.len() < header_end.saturating_add(content_length) {
        let read = stream.read(&mut buffer).map_err(|_| "probeReadFailed")?;
        if read == 0 {
            return Err("probeRequestTruncated");
        }
        bytes.extend_from_slice(&buffer[..read]);
        if bytes.len() > MAX_HTTP_REQUEST_BYTES {
            return Err("probeRequestOversized");
        }
    }
    let body = serde_json::from_slice(&bytes[header_end..header_end + content_length])
        .map_err(|_| "probeInvalidJson")?;
    Ok((path, body))
}

fn write_http_response(
    stream: &mut TcpStream,
    status: u16,
    body: &[u8],
    content_type: &str,
) -> std::io::Result<()> {
    let reason = if status == 200 {
        "OK"
    } else {
        "Internal Server Error"
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nCache-Control: no-cache\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()
}

fn response_object(output: Vec<Value>, status: &str, request_id: usize) -> Value {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    json!({
        "id": format!("resp_codex_switch_{request_id}"),
        "object": "response",
        "created_at": now,
        "status": status,
        "background": false,
        "billing": {"payer": "developer"},
        "completed_at": if status == "completed" { Some(now) } else { None },
        "error": null,
        "incomplete_details": null,
        "instructions": null,
        "max_output_tokens": null,
        "max_tool_calls": null,
        "model": PROBE_MODEL,
        "output": output,
        "parallel_tool_calls": true,
        "previous_response_id": null,
        "prompt_cache_key": null,
        "prompt_cache_retention": null,
        "reasoning": {"effort": "low", "summary": null},
        "safety_identifier": null,
        "service_tier": "default",
        "store": false,
        "temperature": null,
        "text": {"format": {"type": "text"}, "verbosity": "medium"},
        "tool_choice": "auto",
        "tools": [],
        "top_logprobs": 0,
        "top_p": null,
        "truncation": "disabled",
        "usage": if status == "completed" {
            json!({
                "input_tokens": 1,
                "input_tokens_details": {"cached_tokens": 0},
                "output_tokens": 1,
                "output_tokens_details": {"reasoning_tokens": 0},
                "total_tokens": 2
            })
        } else { Value::Null },
        "user": null,
        "metadata": {}
    })
}

fn sse(events: Vec<Value>) -> String {
    let mut output = String::new();
    for event in events {
        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("response.completed");
        output.push_str("event: ");
        output.push_str(event_type);
        output.push_str("\ndata: ");
        output.push_str(&event.to_string());
        output.push_str("\n\n");
    }
    output.push_str("data: [DONE]\n\n");
    output
}

fn text_probe_sse(request_id: usize) -> String {
    let text = PROBE_COMPLETION_TEXT;
    let item_id = format!("msg_codex_switch_{request_id}");
    let part = json!({
        "type": "output_text",
        "text": text,
        "annotations": [],
        "logprobs": []
    });
    let item = json!({
        "id": item_id,
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [part.clone()]
    });
    let created = response_object(Vec::new(), "in_progress", request_id);
    let completed = response_object(vec![item.clone()], "completed", request_id);
    sse(vec![
        json!({"type":"response.created","response":created,"sequence_number":0}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":item_id,"type":"message","status":"in_progress","role":"assistant","content":[]},"sequence_number":1}),
        json!({"type":"response.content_part.added","item_id":item_id,"output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[],"logprobs":[]},"sequence_number":2}),
        json!({"type":"response.output_text.delta","item_id":item_id,"output_index":0,"content_index":0,"delta":text,"logprobs":[],"sequence_number":3}),
        json!({"type":"response.output_text.done","item_id":item_id,"output_index":0,"content_index":0,"text":text,"logprobs":[],"sequence_number":4}),
        json!({"type":"response.content_part.done","item_id":item_id,"output_index":0,"content_index":0,"part":part,"sequence_number":5}),
        json!({"type":"response.output_item.done","output_index":0,"item":item,"sequence_number":6}),
        json!({"type":"response.completed","response":completed,"sequence_number":7}),
    ])
}

fn tool_probe_sse(request_id: usize, descriptor: &ProbeDescriptor) -> String {
    let arguments = json!({"command": descriptor.command.as_str()}).to_string();
    let item_id = format!("fc_codex_switch_{request_id}");
    let item = json!({
        "id": item_id,
        "type": "function_call",
        "status": "completed",
        "call_id": descriptor.call_id.as_str(),
        "name": "shell_command",
        "arguments": arguments
    });
    let created = response_object(Vec::new(), "in_progress", request_id);
    let completed = response_object(vec![item.clone()], "completed", request_id);
    sse(vec![
        json!({"type":"response.created","response":created,"sequence_number":0}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":item_id,"type":"function_call","status":"in_progress","call_id":descriptor.call_id.as_str(),"name":"shell_command","arguments":""},"sequence_number":1}),
        json!({"type":"response.function_call_arguments.delta","item_id":item_id,"output_index":0,"delta":arguments,"sequence_number":2}),
        json!({"type":"response.function_call_arguments.done","item_id":item_id,"output_index":0,"arguments":arguments,"sequence_number":3}),
        json!({"type":"response.output_item.done","output_index":0,"item":item,"sequence_number":4}),
        json!({"type":"response.completed","response":completed,"sequence_number":5}),
    ])
}

struct JsonRpcClient {
    child: Child,
    stdin: Option<BufWriter<ChildStdin>>,
    messages: Receiver<Result<Value, String>>,
    pending: VecDeque<Value>,
    next_id: u64,
}

impl JsonRpcClient {
    fn launch(executable: &Path, prepared: &PreparedRuntime) -> Result<Self, String> {
        let user_home = prepared.isolated_root.join("user");
        let appdata = user_home.join("AppData/Roaming");
        let localappdata = user_home.join("AppData/Local");
        fs::create_dir_all(&appdata)
            .map_err(|_| "failed to create isolated Codex AppData".to_string())?;
        fs::create_dir_all(&localappdata)
            .map_err(|_| "failed to create isolated Codex LocalAppData".to_string())?;
        let user_home = validate_isolated_directory(&prepared.isolated_root, &user_home)?;
        let appdata = validate_isolated_directory(&prepared.isolated_root, &appdata)?;
        let localappdata = validate_isolated_directory(&prepared.isolated_root, &localappdata)?;
        let mut command = Command::new(executable);
        command
            .args(["app-server", "--stdio"])
            .current_dir(&prepared.workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("CODEX_HOME", &prepared.codex_home)
            .env("CODEX_SQLITE_HOME", &prepared.sqlite_home)
            .env("HOME", &user_home)
            .env("USERPROFILE", &user_home)
            .env("APPDATA", &appdata)
            .env("LOCALAPPDATA", &localappdata)
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("HTTP_PROXY", "http://127.0.0.1:9")
            .env("HTTPS_PROXY", "http://127.0.0.1:9")
            .env("ALL_PROXY", "http://127.0.0.1:9")
            .env_remove("OPENAI_API_KEY")
            .env_remove("CODEX_API_KEY");
        let mut child = command
            .spawn()
            .map_err(|_| "failed to launch native Codex app-server".to_string())?;
        let Some(stdin) = child.stdin.take() else {
            stop_child(&mut child);
            return Err("native Codex app-server stdin is unavailable".to_string());
        };
        let Some(stdout) = child.stdout.take() else {
            stop_child(&mut child);
            return Err("native Codex app-server stdout is unavailable".to_string());
        };
        let Some(stderr) = child.stderr.take() else {
            stop_child(&mut child);
            return Err("native Codex app-server stderr is unavailable".to_string());
        };
        let (sender, receiver) = mpsc::channel();
        if thread::Builder::new()
            .name("codex-backup-probe-stdout".to_string())
            .spawn(move || read_rpc_messages(stdout, sender))
            .is_err()
        {
            stop_child(&mut child);
            return Err("failed to start native Codex stdout reader".to_string());
        }
        if thread::Builder::new()
            .name("codex-backup-probe-stderr".to_string())
            .spawn(move || {
                let _ = std::io::copy(&mut BufReader::new(stderr), &mut std::io::sink());
            })
            .is_err()
        {
            stop_child(&mut child);
            return Err("failed to start native Codex stderr reader".to_string());
        }
        Ok(Self {
            child,
            stdin: Some(BufWriter::new(stdin)),
            messages: receiver,
            pending: VecDeque::new(),
            next_id: 1,
        })
    }

    fn initialize(&mut self) -> Result<(), String> {
        self.request(
            "initialize",
            json!({
                "clientInfo": {"name": "codex-switch-backup-verifier", "version": "0.3.0"},
                "capabilities": {"experimentalApi": false}
            }),
        )?;
        self.notify("initialized", json!({}))
    }

    fn list_all_threads(&mut self) -> Result<BTreeSet<String>, String> {
        let source_kinds = json!([
            "cli",
            "vscode",
            "exec",
            "appServer",
            "subAgent",
            "subAgentReview",
            "subAgentCompact",
            "subAgentThreadSpawn",
            "subAgentOther",
            "unknown"
        ]);
        let mut ids = BTreeSet::new();
        for archived in [false, true] {
            let mut cursor = Value::Null;
            loop {
                let result = self.request(
                    "thread/list",
                    json!({
                        "archived": archived,
                        "cursor": cursor,
                        "limit": 100,
                        "modelProviders": [],
                        "sourceKinds": source_kinds.clone(),
                        "useStateDbOnly": true
                    }),
                )?;
                let data = result
                    .get("data")
                    .and_then(Value::as_array)
                    .ok_or_else(|| "native Codex thread/list response is invalid".to_string())?;
                for thread in data {
                    let id = thread
                        .get("id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "native Codex thread/list item is invalid".to_string())?;
                    ids.insert(id.to_string());
                }
                cursor = result.get("nextCursor").cloned().unwrap_or(Value::Null);
                if cursor.is_null() {
                    break;
                }
            }
        }
        Ok(ids)
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| "native Codex RPC ID overflowed".to_string())?;
        self.write_message(&json!({"id": id, "method": method, "params": params}))?;
        let deadline = Instant::now() + RPC_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(format!("native Codex RPC timed out: {method}"));
            }
            let message = self
                .messages
                .recv_timeout(remaining)
                .map_err(|_| format!("native Codex RPC channel closed: {method}"))??;
            if message.get("id").and_then(Value::as_u64) != Some(id) {
                self.pending.push_back(message);
                continue;
            }
            if let Some(error) = message.get("error") {
                return Err(format!(
                    "native Codex RPC failed: {method}: {}",
                    safe_rpc_error(error)
                ));
            }
            return message
                .get("result")
                .cloned()
                .ok_or_else(|| format!("native Codex RPC result is missing: {method}"));
        }
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        self.write_message(&json!({"method": method, "params": params}))
    }

    fn wait_for_turn_completed(&mut self, expected: &StartedTurn) -> Result<(), String> {
        let deadline = Instant::now() + RPC_TIMEOUT;
        loop {
            let message = if let Some(message) = self.pending.pop_front() {
                message
            } else {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err("native Codex continuation timed out".to_string());
                }
                self.messages
                    .recv_timeout(remaining)
                    .map_err(|_| "native Codex continuation channel closed".to_string())??
            };
            if message.get("method").and_then(Value::as_str) == Some("turn/completed") {
                return verify_completed_turn(&message, expected);
            }
            if message.get("method").and_then(Value::as_str) == Some("error") {
                return Err("native Codex continuation reported a runtime error".to_string());
            }
            if message.get("id").is_some() && message.get("method").is_some() {
                return Err(
                    "native Codex continuation requested an unexpected approval".to_string()
                );
            }
        }
    }

    fn write_message(&mut self, message: &Value) -> Result<(), String> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| "native Codex app-server stdin is closed".to_string())?;
        serde_json::to_writer(&mut *stdin, message)
            .map_err(|_| "failed to encode native Codex RPC request".to_string())?;
        stdin
            .write_all(b"\n")
            .and_then(|_| stdin.flush())
            .map_err(|_| "failed to write native Codex RPC request".to_string())
    }

    fn shutdown(&mut self) -> Result<(), String> {
        self.stdin.take();
        let deadline = Instant::now() + PROCESS_EXIT_TIMEOUT;
        loop {
            if self
                .child
                .try_wait()
                .map_err(|_| "failed to inspect native Codex app-server".to_string())?
                .is_some()
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                self.child
                    .kill()
                    .map_err(|_| "failed to stop native Codex app-server".to_string())?;
                let _ = self.child.wait();
                return Ok(());
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for JsonRpcClient {
    fn drop(&mut self) {
        self.stdin.take();
        stop_child(&mut self.child);
    }
}

fn stop_child(child: &mut Child) {
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn read_rpc_messages(stdout: impl std::io::Read, sender: mpsc::Sender<Result<Value, String>>) {
    let mut reader = BufReader::new(stdout);
    loop {
        let mut line = Vec::new();
        let mut limited = reader
            .by_ref()
            .take((MAX_RPC_LINE_BYTES as u64).saturating_add(1));
        match limited.read_until(b'\n', &mut line) {
            Ok(0) => return,
            Ok(_) if line.len() > MAX_RPC_LINE_BYTES => {
                let _ = sender.send(Err("native Codex RPC line exceeded the limit".to_string()));
                return;
            }
            Ok(_) => {
                while line
                    .last()
                    .is_some_and(|byte| matches!(byte, b'\r' | b'\n'))
                {
                    line.pop();
                }
                if line.is_empty() {
                    continue;
                }
                let parsed = serde_json::from_slice(&line)
                    .map_err(|_| "native Codex RPC output is invalid".to_string());
                if sender.send(parsed).is_err() {
                    return;
                }
            }
            Err(_) => {
                let _ = sender.send(Err("native Codex RPC output is unreadable".to_string()));
                return;
            }
        }
    }
}

fn verify_thread_response(result: &Value, expected_id: &str) -> Result<(), String> {
    let thread = result
        .get("thread")
        .ok_or_else(|| "native Codex thread response is invalid".to_string())?;
    if thread.get("id").and_then(Value::as_str) == Some(expected_id) {
        Ok(())
    } else {
        Err("native Codex thread response identity changed".to_string())
    }
}

fn verify_thread_content(
    result: &Value,
    sample: &RuntimeSample,
) -> Result<ThreadContentProof, String> {
    if !sample.expected_turn_ids.is_empty()
        && sample.expected_turn_ids.len() != sample.expected_turn_count
    {
        return Err("isolated Codex sample has inconsistent source turn identity".to_string());
    }
    let thread = result
        .get("thread")
        .ok_or_else(|| "native Codex thread response is invalid".to_string())?;
    let runtime_path = thread
        .get("path")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .ok_or_else(|| "native Codex thread response omitted its storage identity".to_string())?;
    if path_key(Path::new(runtime_path)) != path_key(&sample.restored_path) {
        return Err("native Codex thread response used a different session payload".to_string());
    }
    let turns = thread
        .get("turns")
        .and_then(Value::as_array)
        .ok_or_else(|| "native Codex thread response omitted turns".to_string())?;
    if turns.iter().any(|turn| {
        turn.get("itemsView")
            .and_then(Value::as_str)
            .is_some_and(|view| view != "full")
    }) {
        return Err("native Codex thread response did not load complete turn items".to_string());
    }
    let actual_turn_ids = turns
        .iter()
        .map(|turn| {
            turn.get("id")
                .and_then(Value::as_str)
                .filter(|turn_id| !turn_id.is_empty())
                .map(ToOwned::to_owned)
                .ok_or_else(|| "native Codex thread response has an invalid turn".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    if actual_turn_ids.len() != sample.expected_turn_count {
        return Err(format!(
            "native Codex thread response changed restored turn count: expected {}, actual {}",
            sample.expected_turn_count,
            actual_turn_ids.len()
        ));
    }
    if actual_turn_ids.iter().collect::<BTreeSet<_>>().len() != actual_turn_ids.len() {
        return Err("native Codex thread response duplicated a restored turn identity".to_string());
    }
    let actual_content_sha256 = runtime_thread_turn_content_sha256(turns)?;
    if actual_content_sha256 != sample.expected_turn_content_sha256 {
        return Err(format!(
            "native Codex thread response changed restored turn content: expected {:?}, actual {:?}",
            sample.expected_turn_content_sha256, actual_content_sha256
        ));
    }
    Ok(ThreadContentProof {
        runtime_turn_ids: actual_turn_ids,
    })
}

fn parse_started_turn(result: &Value, expected_thread_id: &str) -> Result<StartedTurn, String> {
    let turn = result
        .get("turn")
        .ok_or_else(|| "native Codex turn/start response is invalid".to_string())?;
    let turn_id = turn
        .get("id")
        .and_then(Value::as_str)
        .filter(|turn_id| !turn_id.is_empty())
        .ok_or_else(|| "native Codex turn/start response omitted its turn identity".to_string())?;
    if turn.get("status").and_then(Value::as_str) != Some("inProgress") {
        return Err("native Codex turn/start did not start an active turn".to_string());
    }
    Ok(StartedTurn {
        thread_id: expected_thread_id.to_string(),
        turn_id: turn_id.to_string(),
    })
}

fn verify_completed_turn(message: &Value, expected: &StartedTurn) -> Result<(), String> {
    let params = message
        .get("params")
        .ok_or_else(|| "native Codex turn/completed notification is invalid".to_string())?;
    if params.get("threadId").and_then(Value::as_str) != Some(expected.thread_id.as_str()) {
        return Err("native Codex completed a different thread".to_string());
    }
    let turn = params
        .get("turn")
        .ok_or_else(|| "native Codex turn/completed notification omitted its turn".to_string())?;
    if turn.get("id").and_then(Value::as_str) != Some(expected.turn_id.as_str()) {
        return Err("native Codex completed a different turn".to_string());
    }
    if turn.get("status").and_then(Value::as_str) != Some("completed") {
        return Err("native Codex continuation did not complete successfully".to_string());
    }
    Ok(())
}

fn safe_rpc_error(error: &Value) -> String {
    error
        .get("code")
        .and_then(Value::as_i64)
        .map(|code| format!("code={code}"))
        .unwrap_or_else(|| "unclassified".to_string())
}

fn discover_codex_executable() -> Result<RuntimeExecutableIdentity, String> {
    if let Some(path) = std::env::var_os("CODEX_SWITCH_CODEX_RUNTIME_EXE") {
        return validate_codex_executable(PathBuf::from(path));
    }
    #[cfg(windows)]
    {
        let output = Command::new("where.exe")
            .arg("codex.exe")
            .output()
            .map_err(|_| "failed to locate native Codex runtime".to_string())?;
        if output.status.success() {
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                let path = PathBuf::from(line.trim());
                if let Ok(path) = validate_codex_executable(path) {
                    return Ok(path);
                }
            }
        }
    }
    Err("native Codex runtime is unavailable".to_string())
}

fn validate_codex_executable(path: PathBuf) -> Result<RuntimeExecutableIdentity, String> {
    if !path.is_absolute() {
        return Err("native Codex runtime path is invalid".to_string());
    }
    let before = fs::symlink_metadata(&path)
        .map_err(|_| "native Codex runtime is unavailable".to_string())?;
    if !before.is_file() || metadata_is_link_or_reparse(&before) {
        return Err("native Codex runtime path is unsafe".to_string());
    }
    let mut file = fs::File::open(&path)
        .map_err(|_| "native Codex runtime could not be opened".to_string())?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|_| "native Codex runtime could not be hashed".to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let sha256 = format!("{:x}", hasher.finalize());
    let output = Command::new(&path)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .map_err(|_| "native Codex runtime version is unavailable".to_string())?;
    if !output.status.success() || output.stdout.len() > 1024 {
        return Err("native Codex runtime version is invalid".to_string());
    }
    let version = String::from_utf8(output.stdout)
        .map_err(|_| "native Codex runtime version is invalid".to_string())?
        .trim()
        .to_string();
    if version.is_empty()
        || version.len() > 256
        || !version.to_ascii_lowercase().contains("codex")
        || version.chars().any(char::is_control)
    {
        return Err("native Codex runtime version is invalid".to_string());
    }
    let after = fs::symlink_metadata(&path)
        .map_err(|_| "native Codex runtime changed during validation".to_string())?;
    if !after.is_file()
        || metadata_is_link_or_reparse(&after)
        || before.len() != after.len()
        || before.modified().ok() != after.modified().ok()
        || before.created().ok() != after.created().ok()
    {
        return Err("native Codex runtime changed during validation".to_string());
    }
    Ok(RuntimeExecutableIdentity {
        executable: path,
        bytes: after.len(),
        sha256,
        version,
    })
}

#[cfg(windows)]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, fs, path::PathBuf};

    use rusqlite::Connection;
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use tempfile::tempdir;

    use super::{
        category_representatives, copy_runtime_databases, next_capability_thread_id,
        parse_started_turn, patch_runtime_state_db, request_contains_exact_tool_output,
        safe_rpc_error, validate_isolated_directory, validate_isolated_file, verify_completed_turn,
        verify_persisted_probe_tool, verify_persisted_probe_turn, verify_thread_content,
        verify_thread_response, NativeCodexBackupVerifier, ProbeDescriptor, RuntimeCategory,
        RuntimeExecutableIdentity, RuntimeSample, StartedTurn,
    };

    fn create_goals_database(path: &std::path::Path) {
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE thread_goals (
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
                 );
                 INSERT INTO thread_goals VALUES
                    ('thread-a','goal-a','objective','active',NULL,1,2,3,4);
                 INSERT INTO thread_goal_continuation_deferrals VALUES ('thread-a');",
            )
            .unwrap();
    }

    #[test]
    fn isolated_runtime_copies_and_exactly_verifies_split_goals_database() {
        let root = tempdir().unwrap();
        let isolated = root.path().join("isolated");
        let sqlite_home = isolated.join("sqlite");
        let payload = isolated.join("databases");
        fs::create_dir_all(&sqlite_home).unwrap();
        fs::create_dir_all(&payload).unwrap();
        let state = payload.join("canonical-state_5.sqlite");
        Connection::open(&state)
            .unwrap()
            .execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT);")
            .unwrap();
        let goals = payload.join("canonical-goals_1.sqlite");
        create_goals_database(&goals);
        let entry = |source: &std::path::Path, name: &str| {
            let bytes = fs::read(source).unwrap();
            MigrationBackupEntry {
                source_path: source.to_path_buf(),
                payload_relative_path: PathBuf::from("databases").join(name),
                kind: MigrationBackupEntryKind::Database,
                bytes: bytes.len() as u64,
                sha256: format!("{:x}", Sha256::digest(&bytes)),
                logical_thread_id: None,
            }
        };
        let manifest = MigrationBackupManifest {
            schema_version: 1,
            operation_id: "goals-verifier".to_string(),
            created_at_ms: 1,
            expires_at_ms: u128::MAX,
            backup_dir: isolated.clone(),
            status: MigrationBackupStatus::IntegrityVerified,
            entries: vec![
                entry(&state, "canonical-state_5.sqlite"),
                entry(&goals, "canonical-goals_1.sqlite"),
            ],
            isolated_restore_verified_at_ms: None,
            runtime_verification: None,
        };

        copy_runtime_databases(&isolated, &manifest, &sqlite_home).unwrap();
        let copied = Connection::open(sqlite_home.join("goals_1.sqlite")).unwrap();
        assert_eq!(
            copied
                .query_row("SELECT COUNT(*) FROM thread_goals", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            copied
                .query_row(
                    "SELECT COUNT(*) FROM thread_goal_continuation_deferrals",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
    }
    use crate::session_storage::{
        migration_backup::{
            create_migration_backup, verify_migration_backup_with_runtime, MigrationBackupEntry,
            MigrationBackupEntryKind, MigrationBackupManifest, MigrationBackupSource,
            MigrationBackupStatus,
        },
        semantic::read_semantic_session,
    };

    #[test]
    fn validates_thread_identity_without_exposing_rpc_messages() {
        assert!(verify_thread_response(&json!({"thread": {"id": "thread-a"}}), "thread-a").is_ok());
        assert!(
            verify_thread_response(&json!({"thread": {"id": "thread-b"}}), "thread-a").is_err()
        );
        assert_eq!(
            safe_rpc_error(&json!({"code": -32602, "message": "secret"})),
            "code=-32602"
        );
    }

    #[test]
    fn explicit_runtime_constructor_keeps_the_selected_binary() {
        let path = PathBuf::from("C:/isolated/codex.exe");
        let verifier = NativeCodexBackupVerifier {
            runtime: RuntimeExecutableIdentity {
                executable: path.clone(),
                bytes: 1,
                sha256: "11".repeat(32),
                version: "codex-cli test".to_string(),
            },
        };
        assert_eq!(verifier.runtime.executable, path);
    }

    #[test]
    fn isolated_runtime_directories_cannot_escape_the_restore_root() {
        let root = tempdir().unwrap();
        let inside = root.path().join("inside");
        fs::create_dir(&inside).unwrap();
        assert_eq!(
            validate_isolated_directory(root.path(), &inside).unwrap(),
            inside.clone()
        );
        let outside = tempdir().unwrap();
        assert!(validate_isolated_directory(root.path(), outside.path()).is_err());
        let inside_file = inside.join("session.jsonl");
        fs::write(&inside_file, b"{}\n").unwrap();
        assert_eq!(
            validate_isolated_file(root.path(), &inside_file).unwrap(),
            inside_file
        );
        let outside_file = outside.path().join("session.jsonl");
        fs::write(&outside_file, b"{}\n").unwrap();
        assert!(validate_isolated_file(root.path(), &outside_file).is_err());
    }

    #[test]
    fn restored_turn_validation_is_bound_to_path_order_and_full_items() {
        let sample = RuntimeSample {
            thread_id: "thread-a".to_string(),
            restored_path: PathBuf::from("C:/isolated/thread-a.jsonl"),
            has_tool_pair: false,
            message_count: 4,
            expected_turn_count: 2,
            expected_turn_ids: vec!["turn-1".to_string(), "turn-2".to_string()],
            expected_turn_content_sha256: runtime_turn_content_digests(&[
                ("first user", "first assistant"),
                ("second user", "second assistant"),
            ]),
            bytes: 1,
            categories: BTreeSet::from([RuntimeCategory::Ordinary]),
        };
        let path = sample.restored_path.to_string_lossy().to_string();
        assert!(verify_thread_content(
            &json!({"thread":{"path":path,"turns":[runtime_thread_turn(
                "turn-1", "first user", "first assistant"
            )]}}),
            &sample,
        )
        .is_err());
        assert!(verify_thread_content(
            &json!({"thread":{"path":path,"turns":[
                runtime_thread_turn("turn-1", "first user", "first assistant"),
                runtime_thread_turn("turn-2", "second user", "second assistant")
            ]}}),
            &sample,
        )
        .is_ok());
        assert!(verify_thread_content(
            &json!({"thread":{"path":path,"turns":[
                runtime_thread_turn("native-1", "first user", "first assistant"),
                runtime_thread_turn("native-2", "second user", "second assistant")
            ]}}),
            &sample,
        )
        .is_ok());
        assert!(verify_thread_content(
            &json!({"thread":{"path":path,"turns":[
                runtime_thread_turn("native-duplicate", "first user", "first assistant"),
                runtime_thread_turn("native-duplicate", "second user", "second assistant")
            ]}}),
            &sample,
        )
        .is_err());
        assert!(verify_thread_content(
            &json!({"thread":{"path":path,"turns":[
                runtime_thread_turn("turn-2", "second user", "second assistant"),
                runtime_thread_turn("turn-1", "first user", "first assistant")
            ]}}),
            &sample,
        )
        .is_err());
        assert!(verify_thread_content(
            &json!({"thread":{"path":path,"turns":[
                {"id":"turn-1","itemsView":"summary","items":[]},
                runtime_thread_turn("turn-2", "second user", "second assistant")
            ]}}),
            &sample,
        )
        .is_err());
        assert!(verify_thread_content(
            &json!({"thread":{"path":path,"turns":[
                runtime_thread_turn("turn-1", "first user", "tampered assistant"),
                runtime_thread_turn("turn-2", "second user", "second assistant")
            ]}}),
            &sample,
        )
        .is_err());

        let legacy = RuntimeSample {
            expected_turn_ids: Vec::new(),
            ..sample.clone()
        };
        assert!(verify_thread_content(
            &json!({"thread":{"path":path,"turns":[
                runtime_thread_turn("legacy-1", "first user", "first assistant"),
                runtime_thread_turn("legacy-2", "second user", "second assistant")
            ]}}),
            &legacy,
        )
        .is_ok());
        assert!(verify_thread_content(
            &json!({"thread":{"path":path,"turns":[runtime_thread_turn(
                "legacy-1", "first user", "first assistant"
            )]}}),
            &legacy,
        )
        .is_err());
    }

    fn runtime_thread_turn(id: &str, user: &str, assistant: &str) -> serde_json::Value {
        json!({
            "id": id,
            "itemsView": "full",
            "items": [
                {"type":"userMessage","id":format!("{id}-user"),"content":[{"type":"text","text":user}]},
                {"type":"agentMessage","id":format!("{id}-assistant"),"text":assistant}
            ]
        })
    }

    fn runtime_turn_content_digests(turns: &[(&str, &str)]) -> Vec<String> {
        turns
            .iter()
            .map(|(user, assistant)| {
                let mut digest = Sha256::new();
                super::update_runtime_turn_digest(&mut digest, "user", user.as_bytes());
                super::update_runtime_turn_digest(&mut digest, "assistant", assistant.as_bytes());
                format!("{:x}", digest.finalize())
            })
            .collect()
    }

    #[test]
    fn legacy_turn_content_binding_splits_turns_without_double_counting_mirrors() {
        let root = tempdir().unwrap();
        let path = root.path().join("legacy.jsonl");
        fs::write(
            &path,
            [
                json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"user-1"}]}}),
                json!({"type":"event_msg","payload":{"type":"user_message","message":"user-1"}}),
                json!({"type":"event_msg","payload":{"type":"agent_message","message":"assistant-1"}}),
                json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"assistant-1"}]}}),
                json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"user-2"}]}}),
                json!({"type":"event_msg","payload":{"type":"user_message","message":"user-2"}}),
                json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"assistant-2"}]}}),
                json!({"type":"event_msg","payload":{"type":"agent_message","message":"assistant-2"}}),
            ]
            .into_iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join("\n")
                + "\n",
        )
        .unwrap();
        assert_eq!(
            super::runtime_turn_content_sha256(&path).unwrap(),
            runtime_turn_content_digests(&[("user-1", "assistant-1"), ("user-2", "assistant-2"),])
        );
    }

    #[test]
    fn legacy_turn_content_binding_ignores_empty_response_placeholders() {
        let root = tempdir().unwrap();
        let path = root.path().join("legacy-empty-placeholder.jsonl");
        fs::write(
            &path,
            [
                json!({"type":"event_msg","payload":{"type":"user_message","message":"downgrade fixture"}}),
                json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[]}}),
            ]
            .into_iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join("\n")
                + "\n",
        )
        .unwrap();
        let expected = vec![super::runtime_messages_digest(&[(
            "user".to_string(),
            "downgrade fixture".to_string(),
        )])];
        assert_eq!(super::runtime_turn_content_sha256(&path).unwrap(), expected);
    }

    #[test]
    fn explicit_turn_binding_excludes_bootstrap_messages_before_the_first_context() {
        let root = tempdir().unwrap();
        let path = root.path().join("pre-context.jsonl");
        let turn_ids = ["turn-first", "turn-second", "turn-third"];
        fs::write(
            &path,
            [
                json!({"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"bootstrap developer"}],"internal_chat_message_metadata_passthrough":{"turn_id":turn_ids[0]}}}),
                json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"bootstrap user"}],"internal_chat_message_metadata_passthrough":{"turn_id":turn_ids[0]}}}),
                json!({"type":"turn_context","payload":{"turn_id":turn_ids[0]}}),
                json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"request one"}],"internal_chat_message_metadata_passthrough":{"turn_id":turn_ids[0]}}}),
                json!({"type":"event_msg","payload":{"type":"user_message","message":"request one"}}),
                json!({"type":"event_msg","payload":{"type":"agent_message","message":"answer one"}}),
                json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer one"}],"internal_chat_message_metadata_passthrough":{"turn_id":turn_ids[0]}}}),
                json!({"type":"turn_context","payload":{"turn_id":turn_ids[1]}}),
                json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"request two"}],"internal_chat_message_metadata_passthrough":{"turn_id":turn_ids[1]}}}),
                json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer two"}],"internal_chat_message_metadata_passthrough":{"turn_id":turn_ids[1]}}}),
                json!({"type":"turn_context","payload":{"turn_id":turn_ids[2]}}),
                json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"request three"}],"internal_chat_message_metadata_passthrough":{"turn_id":turn_ids[2]}}}),
                json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer three"}],"internal_chat_message_metadata_passthrough":{"turn_id":turn_ids[2]}}}),
                json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":turn_ids[2]}}),
                json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"post-completion padding"}]}}),
            ]
            .into_iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join("\n")
                + "\n",
        )
        .unwrap();
        let expected = runtime_turn_content_digests(&[
            ("request one", "answer one"),
            ("request two", "answer two"),
            ("request three", "answer three"),
        ]);
        assert_eq!(super::runtime_turn_content_sha256(&path).unwrap(), expected);
    }

    #[test]
    fn continuation_completion_is_bound_to_the_started_thread_and_turn() {
        let started = parse_started_turn(
            &json!({"turn":{"id":"turn-new","status":"inProgress","items":[]}}),
            "thread-a",
        )
        .unwrap();
        assert_eq!(
            started,
            StartedTurn {
                thread_id: "thread-a".to_string(),
                turn_id: "turn-new".to_string(),
            }
        );
        assert!(verify_completed_turn(
            &json!({"params":{"threadId":"thread-a","turn":{"id":"turn-new","status":"completed"}}}),
            &started,
        )
        .is_ok());
        assert!(verify_completed_turn(
            &json!({"params":{"threadId":"thread-b","turn":{"id":"turn-new","status":"completed"}}}),
            &started,
        )
        .is_err());
        assert!(verify_completed_turn(
            &json!({"params":{"threadId":"thread-a","turn":{"id":"turn-old","status":"completed"}}}),
            &started,
        )
        .is_err());
        assert!(verify_completed_turn(
            &json!({"params":{"threadId":"thread-a","turn":{"id":"turn-new","status":"failed"}}}),
            &started,
        )
        .is_err());
    }

    #[test]
    fn tool_probe_requires_the_exact_call_and_output_tokens() {
        let descriptor = ProbeDescriptor {
            prompt_token: "prompt-token".to_string(),
            call_id: "probe-call".to_string(),
            output_token: "PROBE_OUTPUT_TOKEN".to_string(),
            command: "Write-Output PROBE_OUTPUT_TOKEN".to_string(),
        };
        assert!(request_contains_exact_tool_output(
            &json!({"input":[{
                "type":"function_call_output",
                "call_id":"probe-call",
                "output":"Output:\nPROBE_OUTPUT_TOKEN\r\n"
            }]}),
            &descriptor,
        ));
        assert!(!request_contains_exact_tool_output(
            &json!({"input":[{
                "type":"function_call_output",
                "call_id":"other-call",
                "output":"Output:\nPROBE_OUTPUT_TOKEN\r\n"
            }]}),
            &descriptor,
        ));
        assert!(!request_contains_exact_tool_output(
            &json!({"input":[{
                "type":"function_call_output",
                "call_id":"probe-call",
                "output":"command failed"
            }]}),
            &descriptor,
        ));

        let root = tempdir().unwrap();
        let path = root.path().join("probe.jsonl");
        fs::write(
            &path,
            [
                json!({"type":"turn_context","payload":{"turn_id":"turn-new"}}),
                json!({"type":"response_item","payload":{
                    "type":"message",
                    "role":"user",
                    "content":[{"type":"input_text","text":"prompt-token"}],
                    "internal_chat_message_metadata_passthrough":{"turn_id":"turn-new"}
                }}),
                json!({"type":"response_item","payload":{
                    "type":"function_call",
                    "call_id":"probe-call",
                    "arguments":"{\"command\":\"Write-Output PROBE_OUTPUT_TOKEN\"}",
                    "internal_chat_message_metadata_passthrough":{"turn_id":"turn-new"}
                }}),
                json!({"type":"response_item","payload":{
                    "type":"message",
                    "role":"assistant",
                    "content":[{"type":"output_text","text":"isolated continuation verified"}],
                    "internal_chat_message_metadata_passthrough":{"turn_id":"turn-new"}
                }}),
                json!({"type":"response_item","payload":{
                    "type":"function_call_output",
                    "call_id":"probe-call",
                    "output":"Output:\nPROBE_OUTPUT_TOKEN\r\n",
                    "internal_chat_message_metadata_passthrough":{"turn_id":"turn-new"}
                }}),
            ]
            .into_iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join("\n")
                + "\n",
        )
        .unwrap();
        assert!(verify_persisted_probe_turn(&path, "turn-new", "prompt-token").is_ok());
        assert!(verify_persisted_probe_tool(
            &path,
            "turn-new",
            "probe-call",
            "Write-Output PROBE_OUTPUT_TOKEN",
            "PROBE_OUTPUT_TOKEN",
        )
        .is_ok());
        assert!(verify_persisted_probe_tool(
            &path,
            "turn-new",
            "probe-call",
            "Write-Output PROBE_OUTPUT_TOKEN",
            "WRONG_TOKEN",
        )
        .is_err());
    }

    #[test]
    fn runtime_inventory_binds_every_available_category_and_conflict_payload() {
        let root = tempdir().unwrap();
        let isolated = root.path().join("isolated");
        let payload_root = isolated.join("canonical/sessions");
        let sqlite_root = root.path().join("sqlite");
        fs::create_dir_all(&payload_root).unwrap();
        fs::create_dir_all(&sqlite_root).unwrap();
        let database = sqlite_root.join("state_5.sqlite");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE threads (
                    id TEXT PRIMARY KEY,
                    rollout_path TEXT NOT NULL,
                    model_provider TEXT NOT NULL,
                    source TEXT
                 );
                 CREATE TABLE thread_spawn_edges (
                    parent_thread_id TEXT NOT NULL,
                    child_thread_id TEXT NOT NULL,
                    PRIMARY KEY (parent_thread_id, child_thread_id)
                 );",
            )
            .unwrap();

        let ordinary = payload_root.join("ordinary.jsonl");
        write_runtime_session(&ordinary, "thread-ordinary", 2, false, "ordinary");
        let long = payload_root.join("long.jsonl");
        write_runtime_session(&long, "thread-long", 32, false, "long");
        let subagent = payload_root.join("subagent.jsonl");
        write_runtime_session(&subagent, "thread-subagent", 2, true, "subagent");
        let conflict = payload_root.join("conflict-canonical.jsonl");
        write_runtime_session(&conflict, "thread-conflict", 2, false, "canonical");
        let conflict_branch = isolated.join("recovery/conflict-branch.jsonl");
        write_runtime_session(&conflict_branch, "thread-conflict", 2, false, "branch");

        for (thread_id, path, source) in [
            ("thread-ordinary", &ordinary, "cli"),
            ("thread-long", &long, "cli"),
            ("thread-subagent", &subagent, "subAgent"),
            ("thread-conflict", &conflict, "cli"),
        ] {
            connection
                .execute(
                    "INSERT INTO threads VALUES (?1, ?2, 'openai', ?3)",
                    (thread_id, path.to_string_lossy().to_string(), source),
                )
                .unwrap();
        }
        connection
            .execute(
                "INSERT INTO thread_spawn_edges VALUES ('thread-ordinary', 'thread-subagent')",
                [],
            )
            .unwrap();
        drop(connection);

        let session_entry = |path: &std::path::Path, relative: &str, thread_id: &str| {
            let bytes = fs::read(path).unwrap();
            MigrationBackupEntry {
                source_path: path.to_path_buf(),
                payload_relative_path: PathBuf::from(relative),
                kind: MigrationBackupEntryKind::Session,
                bytes: bytes.len() as u64,
                sha256: format!("{:x}", Sha256::digest(&bytes)),
                logical_thread_id: Some(thread_id.to_string()),
            }
        };
        let manifest = MigrationBackupManifest {
            schema_version: 1,
            operation_id: "runtime-category-fixture".to_string(),
            created_at_ms: 1,
            expires_at_ms: u128::MAX,
            backup_dir: root.path().join("backup"),
            status: MigrationBackupStatus::IntegrityVerified,
            entries: vec![
                session_entry(
                    &ordinary,
                    "canonical/sessions/ordinary.jsonl",
                    "thread-ordinary",
                ),
                session_entry(&long, "canonical/sessions/long.jsonl", "thread-long"),
                session_entry(
                    &subagent,
                    "canonical/sessions/subagent.jsonl",
                    "thread-subagent",
                ),
                session_entry(
                    &conflict,
                    "canonical/sessions/conflict-canonical.jsonl",
                    "thread-conflict",
                ),
                session_entry(
                    &conflict_branch,
                    "recovery/conflict-branch.jsonl",
                    "thread-conflict",
                ),
            ],
            isolated_restore_verified_at_ms: None,
            runtime_verification: None,
        };

        let (
            expected,
            samples,
            category_samples,
            tool_count,
            conflict_count,
            conflict_proofs,
            capability_conflict_proof,
        ) = patch_runtime_state_db(&database, &isolated, &manifest).unwrap();
        assert_eq!(expected.len(), 4);
        assert_eq!(samples.len(), 4);
        assert_eq!(category_samples.len(), 4);
        assert_eq!(tool_count, 1);
        assert_eq!(conflict_count, 1);
        assert_eq!(conflict_proofs.len(), 1);
        assert_eq!(conflict_proofs[0].relation, "divergent");
        assert_eq!(capability_conflict_proof.relation, "divergent");
        assert_ne!(
            capability_conflict_proof.canonical_sha256,
            capability_conflict_proof.recycle_sha256
        );
        assert_eq!(
            conflict_proofs[0].recycle_payload_relative_path,
            PathBuf::from("recovery/conflict-branch.jsonl")
        );
        let available = category_samples
            .iter()
            .flat_map(|sample| sample.categories.iter().copied())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            available,
            BTreeSet::from([
                RuntimeCategory::Ordinary,
                RuntimeCategory::Long,
                RuntimeCategory::Subagent,
                RuntimeCategory::ConflictCanonical,
                RuntimeCategory::Tool,
            ])
        );
        let representatives = category_representatives(&category_samples).unwrap();
        assert_eq!(
            representatives
                .values()
                .flat_map(|categories| categories.iter().copied())
                .collect::<BTreeSet<_>>(),
            available
        );
        let orphan = isolated.join("recovery/orphan.jsonl");
        write_runtime_session(&orphan, "thread-orphan", 1, false, "orphan");
        let mut unbound_manifest = manifest.clone();
        unbound_manifest.entries.push(session_entry(
            &orphan,
            "recovery/orphan.jsonl",
            "thread-orphan",
        ));
        assert!(patch_runtime_state_db(&database, &isolated, &unbound_manifest).is_err());

        let connection = Connection::open(&database).unwrap();
        connection
            .execute(
                "UPDATE threads SET rollout_path = 'C:/different/session.jsonl' WHERE id = 'thread-ordinary'",
                [],
            )
            .unwrap();
        drop(connection);
        assert!(patch_runtime_state_db(&database, &isolated, &manifest).is_err());
        let connection = Connection::open(&database).unwrap();
        connection
            .execute(
                "UPDATE threads SET rollout_path = ?1 WHERE id = 'thread-ordinary'",
                [ordinary.to_string_lossy().to_string()],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO threads VALUES ('thread-missing', 'C:/missing/session.jsonl', 'openai', 'cli')",
                [],
            )
            .unwrap();
        drop(connection);
        assert!(patch_runtime_state_db(&database, &isolated, &manifest).is_err());
    }

    #[test]
    fn runtime_capability_fixture_ids_skip_real_backup_collisions() {
        let colliding = "90000000-0000-4000-8000-000000000001".to_string();
        let reserved = BTreeSet::from([colliding]);
        assert_eq!(
            next_capability_thread_id(&reserved, 1).unwrap(),
            "90000000-0000-4000-8000-000000000002"
        );
    }

    #[test]
    fn runtime_capability_fixture_supports_minimal_schema_and_real_id_collision() {
        let root = tempdir().unwrap();
        let isolated = root.path().join("isolated");
        let session = isolated.join("canonical/sessions/collision.jsonl");
        let sqlite_root = root.path().join("sqlite");
        fs::create_dir_all(&sqlite_root).unwrap();
        let collision = "90000000-0000-4000-8000-000000000001";
        write_runtime_session(&session, collision, 2, false, "real-backup");
        let database = sqlite_root.join("state_5.sqlite");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE threads (
                    id TEXT PRIMARY KEY,
                    rollout_path TEXT NOT NULL,
                    model_provider TEXT NOT NULL
                 );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO threads (id, rollout_path, model_provider) VALUES (?1, ?2, 'openai')",
                (collision, session.to_string_lossy().to_string()),
            )
            .unwrap();
        drop(connection);
        let bytes = fs::read(&session).unwrap();
        let manifest = MigrationBackupManifest {
            schema_version: 1,
            operation_id: "runtime-minimal-schema-collision".to_string(),
            created_at_ms: 1,
            expires_at_ms: u128::MAX,
            backup_dir: root.path().join("backup"),
            status: MigrationBackupStatus::IntegrityVerified,
            entries: vec![MigrationBackupEntry {
                source_path: session.clone(),
                payload_relative_path: PathBuf::from("canonical/sessions/collision.jsonl"),
                kind: MigrationBackupEntryKind::Session,
                bytes: bytes.len() as u64,
                sha256: format!("{:x}", Sha256::digest(&bytes)),
                logical_thread_id: Some(collision.to_string()),
            }],
            isolated_restore_verified_at_ms: None,
            runtime_verification: None,
        };

        let (expected, _, category_samples, tool_count, _, _, capability_proof) =
            patch_runtime_state_db(&database, &isolated, &manifest).unwrap();
        assert_eq!(expected, BTreeSet::from([collision.to_string()]));
        assert_eq!(category_samples.len(), 4);
        assert_eq!(tool_count, 1);
        assert!(category_samples
            .iter()
            .all(|sample| sample.thread_id != collision));
        assert_eq!(
            category_samples
                .iter()
                .map(|sample| sample.thread_id.as_str())
                .collect::<BTreeSet<_>>()
                .len(),
            4
        );
        assert_eq!(capability_proof.relation, "divergent");
        let connection = Connection::open(&database).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM threads", [], |row| row
                    .get::<_, usize>(0))
                .unwrap(),
            5
        );
    }

    fn write_runtime_session(
        path: &std::path::Path,
        thread_id: &str,
        message_count: usize,
        with_tool_pair: bool,
        branch: &str,
    ) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut lines = vec![json!({
            "type": "session_meta",
            "payload": {"id": thread_id, "model_provider": "openai"}
        })];
        for index in 0..message_count {
            let turn_id = format!("turn-{thread_id}-{index}");
            let user = format!("{branch}-{index}");
            let assistant = format!("reply-{branch}-{index}");
            let timestamp = format!("2026-08-11T00:00:{index:02}Z");
            lines.push(json!({
                "type": "event_msg",
                "timestamp": timestamp,
                "payload": {"type":"task_started","turn_id":turn_id,"started_at":1786406400_u64 + index as u64,"model_context_window":258400,"collaboration_mode_kind":"default"}
            }));
            lines.push(json!({
                "type": "turn_context",
                "timestamp": timestamp,
                "payload": {"turn_id": turn_id}
            }));
            lines.push(json!({
                "type": "response_item",
                "timestamp": timestamp,
                "payload": {"type":"message","id":format!("msg-{thread_id}-{index}-user"),"role":"user","content":[{"type":"input_text","text":user}],"internal_chat_message_metadata_passthrough":{"turn_id":turn_id}}
            }));
            lines.push(json!({
                "type": "event_msg",
                "timestamp": timestamp,
                "payload": {"type": "user_message", "message": user}
            }));
            lines.push(json!({
                "type": "response_item",
                "timestamp": timestamp,
                "payload": {"type":"message","id":format!("msg-{thread_id}-{index}-assistant"),"role":"assistant","content":[{"type":"output_text","text":assistant}],"internal_chat_message_metadata_passthrough":{"turn_id":turn_id}}
            }));
            lines.push(json!({
                "type": "event_msg",
                "timestamp": timestamp,
                "payload": {"type": "agent_message", "message": assistant}
            }));
            if with_tool_pair && index + 1 == message_count {
                lines.push(json!({"type":"response_item","timestamp":timestamp,"payload":{"type":"function_call","id":format!("call-{thread_id}"),"call_id":"runtime-tool-call","name":"shell_command","arguments":"{}","internal_chat_message_metadata_passthrough":{"turn_id":turn_id}}}));
                lines.push(json!({"type":"response_item","timestamp":timestamp,"payload":{"type":"function_call_output","id":format!("output-{thread_id}"),"call_id":"runtime-tool-call","output":"ok","internal_chat_message_metadata_passthrough":{"turn_id":turn_id}}}));
            }
            lines.push(json!({"type":"event_msg","timestamp":timestamp,"payload":{"type":"task_complete","turn_id":turn_id,"last_agent_message":assistant,"started_at":1786406400_u64 + index as u64,"completed_at":1786406401_u64 + index as u64,"duration_ms":1000,"time_to_first_token_ms":1}}));
        }
        fs::write(
            path,
            lines
                .into_iter()
                .map(|line| line.to_string())
                .collect::<Vec<_>>()
                .join("\n")
                + "\n",
        )
        .unwrap();
    }

    #[cfg(windows)]
    fn initialize_native_state_db(codex_home: &std::path::Path) {
        use std::{
            io::{BufRead, BufReader, Write},
            process::{Command, Stdio},
            sync::mpsc,
            thread,
            time::{Duration, Instant},
        };

        let executable = NativeCodexBackupVerifier::discover()
            .unwrap()
            .runtime
            .executable;
        let workspace = codex_home.join("runtime-schema-workspace");
        let user_home = codex_home.join("runtime-schema-user");
        let appdata = user_home.join("AppData/Roaming");
        let localappdata = user_home.join("AppData/Local");
        for directory in [&workspace, &appdata, &localappdata] {
            fs::create_dir_all(directory).unwrap();
        }
        let mut child = Command::new(executable)
            .args(["app-server", "--stdio", "--disable", "plugins"])
            .current_dir(&workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .env("CODEX_HOME", codex_home)
            .env("CODEX_SQLITE_HOME", codex_home)
            .env("HOME", &user_home)
            .env("USERPROFILE", &user_home)
            .env("APPDATA", &appdata)
            .env("LOCALAPPDATA", &localappdata)
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("HTTP_PROXY", "http://127.0.0.1:9")
            .env("HTTPS_PROXY", "http://127.0.0.1:9")
            .env("ALL_PROXY", "http://127.0.0.1:9")
            .env_remove("OPENAI_API_KEY")
            .env_remove("CODEX_API_KEY")
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (line_tx, line_rx) = mpsc::channel();
        let reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if line_tx.send(line).is_err() {
                    break;
                }
            }
        });
        writeln!(
            stdin,
            "{}",
            json!({
                "id": 1,
                "method": "initialize",
                "params": {
                    "clientInfo": {"name": "codex-switch-category-fixture", "version": "0.3.0"},
                    "capabilities": {"experimentalApi": false}
                }
            })
        )
        .unwrap();
        stdin.flush().unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        let acknowledged = loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break false;
            }
            let Ok(line) = line_rx.recv_timeout(remaining) else {
                break false;
            };
            let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if message.get("id").and_then(serde_json::Value::as_u64) == Some(1) {
                break message.get("result").is_some();
            }
        };
        if acknowledged {
            writeln!(stdin, "{}", json!({"method": "initialized", "params": {}})).unwrap();
            stdin.flush().unwrap();
        }
        drop(stdin);
        if !acknowledged {
            let _ = child.kill();
        }
        let status = child.wait().unwrap();
        reader.join().unwrap();
        assert!(
            acknowledged && status.success(),
            "native schema init failed: {status}"
        );
        assert!(codex_home.join("state_5.sqlite").is_file());
        fs::remove_dir_all(workspace).unwrap();
        fs::remove_dir_all(user_home).unwrap();
    }

    #[test]
    #[cfg(windows)]
    #[ignore = "manual native five-category gate; writes only to CODEX_SWITCH_CATEGORY_RUNTIME_ROOT"]
    fn verifies_every_required_category_with_native_codex_app_server() {
        let output_root = std::env::var_os("CODEX_SWITCH_CATEGORY_RUNTIME_ROOT")
            .map(PathBuf::from)
            .expect("CODEX_SWITCH_CATEGORY_RUNTIME_ROOT must be set");
        assert!(output_root.is_absolute());
        assert!(!output_root.exists(), "category evidence root must be new");
        let source = output_root.join("input");
        let sessions = source.join("sessions");
        let conflict_recovery = source.join("conflict-recovery");
        let backup_root = output_root.join("backup");
        fs::create_dir_all(&sessions).unwrap();
        fs::create_dir_all(&conflict_recovery).unwrap();
        fs::create_dir_all(&backup_root).unwrap();
        initialize_native_state_db(&source);

        let ordinary = sessions.join("ordinary.jsonl");
        let long = sessions.join("long.jsonl");
        let subagent = sessions.join("subagent.jsonl");
        let conflict = sessions.join("conflict-canonical.jsonl");
        let conflict_branch = conflict_recovery.join("conflict-branch.jsonl");
        write_runtime_session(&ordinary, "thread-ordinary", 2, false, "ordinary");
        write_runtime_session(&long, "thread-long", 32, false, "long");
        write_runtime_session(&subagent, "thread-subagent", 2, true, "subagent");
        write_runtime_session(&conflict, "thread-conflict", 2, false, "canonical");
        write_runtime_session(&conflict_branch, "thread-conflict", 2, false, "branch");

        let database = source.join("state_5.sqlite");
        let connection = Connection::open(&database).unwrap();
        connection.execute("DELETE FROM threads", []).unwrap();
        for (thread_id, path, source_kind, title) in [
            ("thread-ordinary", &ordinary, "cli", "Ordinary"),
            ("thread-long", &long, "cli", "Long"),
            ("thread-subagent", &subagent, "subAgent", "Subagent tool"),
            ("thread-conflict", &conflict, "cli", "Conflict canonical"),
        ] {
            connection.execute(
                "INSERT INTO threads (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, sandbox_policy, approval_mode, has_user_event, archived, first_user_message, created_at_ms, updated_at_ms, preview, recency_at, recency_at_ms) VALUES (?1, ?2, 1, 2, ?3, 'openai', '.', ?4, '{\"type\":\"danger-full-access\"}', 'never', 1, 0, ?4, 1000, 2000, ?4, 2, 2000)",
                (thread_id, path.to_string_lossy().to_string(), source_kind, title),
            ).unwrap();
        }
        drop(connection);

        let session_source =
            |path: &std::path::Path, relative: &str, thread_id: &str| MigrationBackupSource {
                source_path: path.to_path_buf(),
                payload_relative_path: PathBuf::from(relative),
                kind: MigrationBackupEntryKind::Session,
                expected_sha256: None,
                logical_thread_id: Some(thread_id.to_string()),
            };
        let sources = vec![
            session_source(
                &ordinary,
                "canonical/sessions/ordinary.jsonl",
                "thread-ordinary",
            ),
            session_source(&long, "canonical/sessions/long.jsonl", "thread-long"),
            session_source(
                &subagent,
                "canonical/sessions/subagent.jsonl",
                "thread-subagent",
            ),
            session_source(
                &conflict,
                "canonical/sessions/conflict.jsonl",
                "thread-conflict",
            ),
            session_source(
                &conflict_branch,
                "recovery/conflict-branch.jsonl",
                "thread-conflict",
            ),
            MigrationBackupSource {
                source_path: database,
                payload_relative_path: "databases/canonical-state_5.sqlite".into(),
                kind: MigrationBackupEntryKind::Database,
                expected_sha256: None,
                logical_thread_id: None,
            },
        ];
        let backup =
            create_migration_backup(&backup_root, "native-five-category", &sources).unwrap();
        let verifier = NativeCodexBackupVerifier::discover().unwrap();
        let verified = verify_migration_backup_with_runtime(
            &backup.backup_dir,
            &output_root.join("isolated"),
            &verifier,
        )
        .unwrap();
        let runtime = verified.runtime_verification.unwrap();
        let required = vec!["ordinary", "long", "subagent", "conflictCanonical", "tool"];
        for category in required {
            assert!(runtime
                .available_categories
                .iter()
                .any(|value| value == category));
            assert!(runtime
                .continued_categories
                .iter()
                .any(|value| value == category));
        }
        assert_eq!(runtime.expected_session_count, 4);
        assert_eq!(runtime.listed_session_count, 4);
        assert_eq!(runtime.resumed_session_count, 4);
        assert!(runtime.continued_session_count >= 3);
        assert_eq!(runtime.tool_session_count, 1);
        assert!(runtime.tool_round_trip_verified);
        assert_eq!(runtime.conflict_payload_count, 1);
        assert!(runtime.conflict_payloads_verified);
        fs::write(
            output_root.join("native-five-category-runtime.json"),
            serde_json::to_vec_pretty(&runtime).unwrap(),
        )
        .unwrap();
    }

    #[test]
    #[ignore = "requires CODEX_SWITCH_RUNTIME_FIXTURE_HOME and a native codex.exe"]
    fn verifies_a_backup_with_the_native_codex_app_server() {
        let fixture_home = PathBuf::from(
            std::env::var_os("CODEX_SWITCH_RUNTIME_FIXTURE_HOME")
                .expect("CODEX_SWITCH_RUNTIME_FIXTURE_HOME must be set"),
        );
        assert!(fixture_home.is_absolute());
        let root = tempdir().unwrap();
        let fixture_copy = root.path().join("fixture-source");
        fs::create_dir(&fixture_copy).unwrap();
        let source_database = fixture_home.join("state_5.sqlite");
        let database = fixture_copy.join("state_5.sqlite");
        copy_fixture_file_stable(&source_database, &database);
        let source_wal = fixture_home.join("state_5.sqlite-wal");
        if source_wal.is_file() {
            copy_fixture_file_stable(&source_wal, &fixture_copy.join("state_5.sqlite-wal"));
        }
        let connection = Connection::open(&database).unwrap();
        let mut statement = connection
            .prepare("SELECT id, rollout_path FROM threads ORDER BY id")
            .unwrap();
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        drop(statement);
        assert!(!rows.is_empty());

        let backup_root = root.path().join("backups");
        fs::create_dir(&backup_root).unwrap();
        let mut sources = Vec::new();
        for (index, (thread_id, rollout_path)) in rows.into_iter().enumerate() {
            let source_path = PathBuf::from(rollout_path);
            assert!(source_path.is_absolute() && source_path.starts_with(&fixture_home));
            let copied_path = fixture_copy
                .join("sessions")
                .join(format!("{index:06}.jsonl"));
            fs::create_dir_all(copied_path.parent().unwrap()).unwrap();
            copy_fixture_file_stable(&source_path, &copied_path);
            connection
                .execute(
                    "UPDATE threads SET rollout_path = ?1 WHERE id = ?2",
                    (copied_path.to_string_lossy().to_string(), &thread_id),
                )
                .unwrap();
            let source_path = copied_path;
            let semantic = read_semantic_session(&source_path).unwrap();
            assert_eq!(semantic.thread_id, thread_id);
            sources.push(MigrationBackupSource {
                source_path,
                payload_relative_path: PathBuf::from("canonical/sessions")
                    .join(format!("{index:06}"))
                    .join("session.jsonl"),
                kind: MigrationBackupEntryKind::Session,
                expected_sha256: None,
                logical_thread_id: Some(thread_id),
            });
        }
        let conflict_root = fixture_home.join("conflict-recovery");
        if conflict_root.is_dir() {
            let mut conflict_paths = fs::read_dir(&conflict_root)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("jsonl"))
                .collect::<Vec<_>>();
            conflict_paths.sort();
            for (index, source_path) in conflict_paths.into_iter().enumerate() {
                let copied_path = fixture_copy
                    .join("conflict-recovery")
                    .join(format!("{index:06}.jsonl"));
                fs::create_dir_all(copied_path.parent().unwrap()).unwrap();
                copy_fixture_file_stable(&source_path, &copied_path);
                let source_path = copied_path;
                let semantic = read_semantic_session(&source_path).unwrap();
                sources.push(MigrationBackupSource {
                    source_path,
                    payload_relative_path: PathBuf::from("recovery")
                        .join(format!("conflict-{index:06}.jsonl")),
                    kind: MigrationBackupEntryKind::Session,
                    expected_sha256: None,
                    logical_thread_id: Some(semantic.thread_id),
                });
            }
        }
        drop(connection);
        sources.push(MigrationBackupSource {
            source_path: database,
            payload_relative_path: "databases/canonical-state_5.sqlite".into(),
            kind: MigrationBackupEntryKind::Database,
            expected_sha256: None,
            logical_thread_id: None,
        });

        let backup =
            create_migration_backup(&backup_root, "native-runtime-test", &sources).unwrap();
        let verifier = NativeCodexBackupVerifier::discover().unwrap();
        let verified = verify_migration_backup_with_runtime(
            &backup.backup_dir,
            &root.path().join("isolated"),
            &verifier,
        )
        .unwrap();

        assert_eq!(verified.status, MigrationBackupStatus::RuntimeVerified);
        let runtime = verified.runtime_verification.unwrap();
        assert_eq!(runtime.listed_session_count, runtime.expected_session_count);
        assert_eq!(
            runtime.resumed_session_count,
            runtime.expected_session_count
        );
        assert!(runtime.continued_session_count > 0);
        assert_eq!(runtime.available_categories, runtime.continued_categories);
        if runtime.tool_session_count > 0 {
            assert!(runtime.tool_round_trip_verified);
        }
        if conflict_root.is_dir() {
            assert!(runtime.conflict_payload_count > 0);
            assert!(runtime.conflict_payloads_verified);
        }
        println!(
            "runtime-verification={}",
            serde_json::to_string(&runtime).unwrap()
        );
    }

    fn copy_fixture_file_stable(source: &std::path::Path, target: &std::path::Path) {
        let before = fs::read(source).unwrap();
        fs::write(target, &before).unwrap();
        assert_eq!(fs::read(source).unwrap(), before);
        assert_eq!(fs::read(target).unwrap(), before);
    }
}

use std::{
    fs,
    path::{Path, PathBuf},
    str::FromStr,
    time::{Duration, Instant},
};

use rusqlite::{Connection, OpenFlags, MAIN_DB};
use serde::{Deserialize, Serialize};
use toml_edit::DocumentMut;

use crate::{
    codex_paths::{codex_paths_with_sqlite_home, resolve_user_codex_paths, CodexPaths},
    config_patch::SqliteHomePatch,
    file_ops::{atomic_copy, atomic_write},
    mobile_continuity,
    session_incremental::{IncrementalSessionSyncReceipt, IncrementalSessionSyncStatus},
};

const STATE_VERSION: u32 = 1;
const MAX_STATE_BYTES: u64 = 64 * 1024;
const RELAY_PROVIDER: &str = "openai_custom";
const MAX_ACCOUNT_PUBLICATION_BATCHES: usize = 4;
const MAX_ACCOUNT_PUBLICATION_DURATION: Duration = Duration::from_secs(30);
const DATABASES: [&str; 4] = [
    "state_5.sqlite",
    "logs_2.sqlite",
    "goals_1.sqlite",
    "memories_1.sqlite",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionViewTarget {
    Account,
    Relay,
}

#[derive(Debug, Clone)]
pub(crate) enum SessionViewTransition {
    None,
    PrepareRelay {
        account: CodexPaths,
        relay: CodexPaths,
        state: SessionViewState,
        session_view_state_path: PathBuf,
        mobile_state_path: PathBuf,
    },
    PublishAccount {
        relay: CodexPaths,
        account: CodexPaths,
        mobile_state_path: PathBuf,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct SessionViewPlan {
    pub(crate) sqlite_home_patch: SqliteHomePatch,
    pub(crate) transition: SessionViewTransition,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SessionViewState {
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
    let relay_sqlite_home = default_relay_sqlite_home(data_root);
    let session_view_state_path = state_path(data_root);
    let mobile_state_path = data_root.join("mobile-continuity-v1.json");
    let current = resolve_user_codex_paths(codex_home)?;
    let configured = configured_sqlite_home(live_config)?;
    let using_relay_view = current.sqlite_home == relay_sqlite_home;

    match target {
        SessionViewTarget::Relay => {
            let state = if using_relay_view {
                load_state(&session_view_state_path, data_root)?.ok_or_else(|| {
                    "Relay 会话视图状态缺失；已停止切换，请先返回 Account".to_string()
                })?
            } else {
                SessionViewState {
                    version: STATE_VERSION,
                    account_configured_sqlite_home: configured,
                    account_effective_sqlite_home: current.sqlite_home.clone(),
                    relay_sqlite_home: relay_sqlite_home.clone(),
                }
            };
            validate_state(&state, data_root)?;
            let account =
                codex_paths_with_sqlite_home(codex_home, &state.account_effective_sqlite_home)?;
            let relay = codex_paths_with_sqlite_home(codex_home, &relay_sqlite_home)?;
            Ok(SessionViewPlan {
                sqlite_home_patch: SqliteHomePatch::Set(
                    relay_sqlite_home.to_string_lossy().to_string(),
                ),
                transition: if using_relay_view {
                    SessionViewTransition::None
                } else {
                    SessionViewTransition::PrepareRelay {
                        account,
                        relay,
                        state,
                        session_view_state_path,
                        mobile_state_path,
                    }
                },
            })
        }
        SessionViewTarget::Account if using_relay_view => {
            let state = load_state(&session_view_state_path, data_root)?.ok_or_else(|| {
                "Relay 会话视图状态缺失；为避免切错数据库，已停止返回 Account".to_string()
            })?;
            validate_state(&state, data_root)?;
            let account =
                codex_paths_with_sqlite_home(codex_home, &state.account_effective_sqlite_home)?;
            Ok(SessionViewPlan {
                sqlite_home_patch: match state.account_configured_sqlite_home {
                    Some(path) => SqliteHomePatch::Set(path),
                    None => SqliteHomePatch::Remove,
                },
                transition: SessionViewTransition::PublishAccount {
                    relay: current,
                    account,
                    mobile_state_path,
                },
            })
        }
        SessionViewTarget::Account => Ok(SessionViewPlan {
            sqlite_home_patch: SqliteHomePatch::Keep,
            transition: SessionViewTransition::None,
        }),
    }
}

pub(crate) fn prepare_transition(
    transition: &SessionViewTransition,
    operation_id: &str,
) -> Result<IncrementalSessionSyncReceipt, String> {
    let started = Instant::now();
    match transition {
        SessionViewTransition::None => Ok(IncrementalSessionSyncReceipt::skipped()),
        SessionViewTransition::PrepareRelay {
            account,
            relay,
            state,
            session_view_state_path,
            mobile_state_path,
        } => {
            mobile_continuity::initialize_status(mobile_state_path, account)?;
            let projected_bytes = projected_database_bytes(account)?;
            let normalized = refresh_relay_databases(account, relay, operation_id)?;
            save_state(session_view_state_path, state)?;
            Ok(IncrementalSessionSyncReceipt {
                status: IncrementalSessionSyncStatus::Applied,
                detected_threads: normalized,
                synced_threads: normalized,
                projected_bytes,
                duration_ms: started.elapsed().as_millis(),
                requires_full_sync: false,
            })
        }
        SessionViewTransition::PublishAccount {
            relay,
            account,
            mobile_state_path,
        } => {
            let mut detected_threads = 0_usize;
            let mut synced_threads = 0_usize;
            let mut deferred_threads = 0_usize;
            for _ in 0..MAX_ACCOUNT_PUBLICATION_BATCHES {
                let result = mobile_continuity::prepare_account_publication_between(
                    mobile_state_path,
                    relay,
                    account,
                )?;
                detected_threads = detected_threads.saturating_add(result.detected_threads);
                synced_threads = synced_threads
                    .saturating_add(result.published_threads)
                    .saturating_add(result.partial_threads);
                deferred_threads = result.deferred_threads;
                if deferred_threads == 0 {
                    break;
                }
                if started.elapsed() >= MAX_ACCOUNT_PUBLICATION_DURATION {
                    break;
                }
            }
            if deferred_threads > 0 {
                return Err(format!(
                    "仍有 {deferred_threads} 个 Relay 会话未完成增量同步；为避免切换后暂时不可见，已保留 Relay 请求端，请使用完全同步后重试"
                ));
            }
            Ok(IncrementalSessionSyncReceipt {
                status: if synced_threads > 0 {
                    IncrementalSessionSyncStatus::Applied
                } else {
                    IncrementalSessionSyncStatus::Unchanged
                },
                detected_threads,
                synced_threads,
                projected_bytes: 0,
                duration_ms: started.elapsed().as_millis(),
                requires_full_sync: false,
            })
        }
    }
}

fn default_relay_sqlite_home(data_root: &Path) -> PathBuf {
    data_root.join("relay-sqlite")
}

fn state_path(data_root: &Path) -> PathBuf {
    data_root.join("request-route-session-view-v1.json")
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
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("failed to inspect session view state: {error}")),
    };
    if !metadata.is_file() || metadata.len() > MAX_STATE_BYTES {
        return Err("session view state is invalid".to_string());
    }
    let bytes =
        fs::read(path).map_err(|error| format!("failed to read session view state: {error}"))?;
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
    let bytes = serde_json::to_vec_pretty(state)
        .map_err(|_| "failed to serialize session view state".to_string())?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err("session view state exceeded the size limit".to_string());
    }
    atomic_write(path, &bytes)
}

fn validate_state(state: &SessionViewState, data_root: &Path) -> Result<(), String> {
    if state.version != STATE_VERSION {
        return Err("session view state version is unsupported".to_string());
    }
    let relay = default_relay_sqlite_home(data_root);
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
    Ok(())
}

fn projected_database_bytes(paths: &CodexPaths) -> Result<u64, String> {
    database_paths(paths)
        .into_iter()
        .filter(|(_, path)| path.is_file())
        .try_fold(0_u64, |total, (_, path)| {
            let bytes = fs::metadata(path)
                .map_err(|error| format!("failed to inspect session view database: {error}"))?
                .len();
            total
                .checked_add(bytes)
                .ok_or_else(|| "session view database size overflowed".to_string())
        })
}

fn refresh_relay_databases(
    account: &CodexPaths,
    relay: &CodexPaths,
    operation_id: &str,
) -> Result<usize, String> {
    if !account.state_db.is_file() {
        return Err("Account state_5.sqlite is missing".to_string());
    }
    ensure_relay_root(&relay.sqlite_home)?;
    let suffix = operation_id
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || *character == '-')
        .collect::<String>();
    let mut normalized = 0;
    for (name, source) in database_paths(account) {
        let target = relay.sqlite_home.join(name);
        if !source.is_file() {
            remove_sqlite_auxiliary(&target)?;
            match fs::remove_file(&target) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!("failed to remove stale Relay {name}: {error}"));
                }
            }
            continue;
        }
        let stage = relay.sqlite_home.join(format!(".{name}.{suffix}.next"));
        let _ = fs::remove_file(&stage);
        let source_connection =
            Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(|error| format!("failed to open {name} for session view: {error}"))?;
        source_connection
            .backup(MAIN_DB, &stage, None)
            .map_err(|error| format!("failed to copy {name} for Relay session view: {error}"))?;
        drop(source_connection);
        let stage_connection = Connection::open(&stage)
            .map_err(|error| format!("failed to open copied {name}: {error}"))?;
        stage_connection
            .execute_batch("PRAGMA journal_mode=DELETE;")
            .map_err(|error| format!("failed to finalize copied {name}: {error}"))?;
        if name == "state_5.sqlite" {
            normalized = normalize_thread_provider(&stage_connection)?;
        }
        verify_database(&stage_connection, name)?;
        drop(stage_connection);
        remove_sqlite_auxiliary(&target)?;
        let copy_result = atomic_copy(&stage, &target);
        let _ = fs::remove_file(&stage);
        copy_result?;
        let target_connection = Connection::open_with_flags(
            &target,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|error| format!("failed to verify Relay {name}: {error}"))?;
        verify_database(&target_connection, name)?;
    }
    Ok(normalized)
}

fn normalize_thread_provider(connection: &Connection) -> Result<usize, String> {
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
            [RELAY_PROVIDER],
        )
        .map_err(|error| format!("failed to prepare Relay conversation visibility: {error}"))
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

fn database_paths(paths: &CodexPaths) -> Vec<(&'static str, &Path)> {
    vec![
        (DATABASES[0], paths.state_db.as_path()),
        (DATABASES[1], paths.logs_db.as_path()),
        (DATABASES[2], paths.goals_db.as_path()),
        (DATABASES[3], paths.memories_db.as_path()),
    ]
}

fn ensure_relay_root(path: &Path) -> Result<(), String> {
    let store = path
        .parent()
        .ok_or_else(|| "Relay session view has no runtime store".to_string())?;
    fs::create_dir_all(store)
        .map_err(|error| format!("failed to create runtime store: {error}"))?;
    fs::create_dir_all(path)
        .map_err(|error| format!("failed to create Relay session view: {error}"))?;
    let store = fs::canonicalize(store)
        .map_err(|error| format!("failed to resolve runtime store: {error}"))?;
    let path = fs::canonicalize(path)
        .map_err(|error| format!("failed to resolve Relay session view: {error}"))?;
    if !path.starts_with(&store) {
        return Err("Relay session view escaped the runtime store".to_string());
    }
    Ok(())
}

fn remove_sqlite_auxiliary(database: &Path) -> Result<(), String> {
    for suffix in ["-wal", "-shm"] {
        let path = PathBuf::from(format!("{}{suffix}", database.to_string_lossy()));
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "failed to clear stale Relay SQLite workspace: {error}"
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rusqlite::Connection;
    use tempfile::tempdir;

    use super::{normalize_thread_provider, projected_database_bytes, refresh_relay_databases};
    use crate::codex_paths::{codex_paths_with_sqlite_home, local_codex_paths};

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

        assert_eq!(normalize_thread_provider(&connection).unwrap(), 1);
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
    fn projected_bytes_include_only_existing_managed_databases() {
        let root = tempdir().unwrap();
        let paths = local_codex_paths(root.path());
        fs::write(&paths.state_db, vec![0_u8; 11]).unwrap();
        fs::write(&paths.logs_db, vec![0_u8; 13]).unwrap();

        assert_eq!(projected_database_bytes(&paths).unwrap(), 24);
    }

    #[test]
    fn relay_refresh_removes_an_optional_database_missing_from_account() {
        let root = tempdir().unwrap();
        let account_home = root.path().join("account");
        let relay_home = root.path().join("relay");
        fs::create_dir_all(&account_home).unwrap();
        fs::create_dir_all(&relay_home).unwrap();
        let account = codex_paths_with_sqlite_home(&account_home, &account_home).unwrap();
        let relay = codex_paths_with_sqlite_home(&account_home, &relay_home).unwrap();
        Connection::open(&account.state_db)
            .unwrap()
            .execute_batch("CREATE TABLE threads (id TEXT, model_provider TEXT);")
            .unwrap();
        Connection::open(&relay.logs_db)
            .unwrap()
            .execute_batch("CREATE TABLE stale (id INTEGER);")
            .unwrap();

        refresh_relay_databases(&account, &relay, "test-refresh").unwrap();

        assert!(!relay.logs_db.exists());
        assert!(relay.state_db.exists());
    }
}

//! macOS provider views use SQLite's own locks and atomic Online Backup writes.
//!
//! This deliberately does not implement the Windows destructive-file guard.
//! The app's process inventory and cross-process mutation lock remain required.
//! All SQLite connections below retain EXCLUSIVE locking mode through the config
//! transaction. Never open/close a second raw file descriptor for a locked DB:
//! on POSIX that can release this process's SQLite record locks.
//!
//! Existing destination databases are updated through SQLite, never renamed or
//! unlinked. A newly created, verified inactive view may be retained on rollback;
//! its v2 ownership/baseline state is recorded so the next attempt is safe.

use super::*;
use rusqlite::backup::{Backup, StepResult};
use std::{fs::OpenOptions, time::Duration};

const JOURNAL_NAME: &str = "macos-session-view-transition-v1.json";
const WORKSPACE_NAME: &str = "macos-session-view-transitions";
const JOURNAL_LIMIT: u64 = 256 * 1024;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FileId {
    volume: u64,
    file: u64,
}

#[derive(Debug)]
struct LockedDatabase {
    path: PathBuf,
    id: FileId,
    connection: Connection,
}

impl LockedDatabase {
    fn open(path: &Path) -> Result<Self, String> {
        let id = file_id(path, false)?;
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|_| "macOS session database could not be opened".to_string())?;
        connection
            .busy_timeout(Duration::from_millis(250))
            .map_err(|_| "macOS SQLite lock timeout could not be set".to_string())?;
        let mode: String = connection
            .query_row("PRAGMA locking_mode=EXCLUSIVE", [], |row| row.get(0))
            .map_err(|_| "macOS SQLite exclusive mode is unavailable".to_string())?;
        if !mode.eq_ignore_ascii_case("exclusive") {
            return Err("macOS SQLite exclusive mode was not granted".to_string());
        }
        connection
            .execute_batch("BEGIN EXCLUSIVE; COMMIT;")
            .map_err(|_| {
                "A SQLite reader or writer is active; close Codex and retry".to_string()
            })?;
        let result = Self {
            path: path.to_path_buf(),
            id,
            connection,
        };
        result.verify_identity()?;
        verify_database(&result.connection, "macOS session database")?;
        Ok(result)
    }

    fn verify_identity(&self) -> Result<(), String> {
        if file_id(&self.path, false)? != self.id {
            return Err("macOS session database file identity changed".to_string());
        }
        Ok(())
    }

    fn checkpoint(&self) -> Result<(), String> {
        self.verify_identity()?;
        let (busy, frames, checkpointed): (i64, i64, i64) = self
            .connection
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .map_err(|_| "macOS SQLite checkpoint failed".to_string())?;
        if busy != 0 || (frames >= 0 && frames != checkpointed) {
            return Err("macOS SQLite WAL is busy; no route was changed".to_string());
        }
        self.verify_identity()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Artifact {
    name: String,
    id: FileId,
    sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GlobalLink {
    source: PathBuf,
    target: PathBuf,
    id: FileId,
    existed: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum Phase {
    Prepared,
    Applied,
    Committed,
    RolledBack,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Journal {
    version: u32,
    operation_id: String,
    workspace: PathBuf,
    workspace_id: FileId,
    source: PathBuf,
    source_id: FileId,
    source_digest: String,
    target: PathBuf,
    target_id: FileId,
    target_existed: bool,
    before_digest: String,
    after_digest: String,
    before: Option<Artifact>,
    after: Artifact,
    new_target_witness: Option<String>,
    globals: Vec<GlobalLink>,
    previous_state: Option<SessionViewState>,
    next_state: SessionViewState,
    phase: Phase,
}

#[derive(Debug)]
pub(super) struct PreparedTransaction {
    data_root: PathBuf,
    journal: Journal,
    source: LockedDatabase,
    target: LockedDatabase,
    global_locks: Vec<LockedDatabase>,
}

impl PreparedTransaction {
    fn verify(&self, expected_target: &str) -> Result<(), String> {
        self.source.verify_identity()?;
        self.target.verify_identity()?;
        if exact_digest(&self.source.connection)? != self.journal.source_digest {
            return Err("macOS source view changed during the route transaction".to_string());
        }
        if exact_digest(&self.target.connection)? != expected_target {
            return Err("macOS target view changed during the route transaction".to_string());
        }
        verify_globals(&self.journal.globals, &self.global_locks)?;
        verify_workspace(&self.journal)
    }

    pub(super) fn commit(mut self) -> Result<(), String> {
        self.verify(&self.journal.after_digest)?;
        self.verify_state_before_write()?;
        save_state(&state_path(&self.data_root), &self.journal.next_state)?;
        self.journal.phase = Phase::Committed;
        write_journal(&self.data_root, &self.journal, false)?;
        self.finish()
    }

    pub(super) fn rollback(mut self) -> Result<(), String> {
        let current = exact_digest(&self.target.connection)?;
        if current != self.journal.before_digest && current != self.journal.after_digest {
            return Err("macOS target changed externally; rollback was not attempted".to_string());
        }
        self.source.verify_identity()?;
        self.target.verify_identity()?;
        verify_globals(&self.journal.globals, &self.global_locks)?;
        self.verify_state_before_write()?;
        if self.journal.phase == Phase::RolledBack {
            let expected = if self.journal.target_existed {
                &self.journal.before_digest
            } else {
                &self.journal.after_digest
            };
            self.verify(expected)?;
            self.verify_saved_state()?;
            return self.finish();
        }
        if exact_digest(&self.source.connection)? != self.journal.source_digest {
            return Err("macOS source changed; rollback was not attempted".to_string());
        }
        if let Some(before) = &self.journal.before {
            let snapshot = open_artifact(&self.journal, before)?;
            backup_into(&snapshot, &mut self.target.connection)?;
            if exact_digest(&self.target.connection)? != self.journal.before_digest {
                return Err("macOS previous view could not be restored".to_string());
            }
            if let Some(previous) = &self.journal.previous_state {
                save_state(&state_path(&self.data_root), previous)?;
            }
        } else {
            // No preexisting data may be deleted as part of a route rollback.
            // Keep the new valid inactive view and prove its baseline instead.
            if current != self.journal.after_digest {
                let snapshot = open_artifact(&self.journal, &self.journal.after)?;
                backup_into(&snapshot, &mut self.target.connection)?;
            }
            self.verify(&self.journal.after_digest)?;
            save_state(&state_path(&self.data_root), &self.journal.next_state)?;
        }
        self.journal.phase = Phase::RolledBack;
        write_journal(&self.data_root, &self.journal, false)?;
        self.finish()
    }

    fn verify_state_before_write(&self) -> Result<(), String> {
        let state = load_state(&state_path(&self.data_root), &self.data_root)?;
        if state != self.journal.previous_state && state.as_ref() != Some(&self.journal.next_state)
        {
            return Err("macOS view ownership state changed externally".to_string());
        }
        Ok(())
    }

    fn verify_saved_state(&self) -> Result<(), String> {
        let expected = if self.journal.phase == Phase::RolledBack && self.journal.target_existed {
            self.journal.previous_state.as_ref()
        } else {
            Some(&self.journal.next_state)
        };
        if load_state(&state_path(&self.data_root), &self.data_root)?.as_ref() != expected {
            return Err("macOS terminal view state changed before cleanup".to_string());
        }
        Ok(())
    }

    fn finish(self) -> Result<(), String> {
        self.source.verify_identity()?;
        self.target.verify_identity()?;
        verify_globals(&self.journal.globals, &self.global_locks)?;
        // Drop SQLite locks only after the durable terminal and view state.
        let Self {
            data_root,
            journal,
            source,
            target,
            global_locks,
        } = self;
        drop(global_locks);
        drop(target);
        drop(source);
        // Private witness cleanup removes an extra name, never a database view.
        if let Some(name) = &journal.new_target_witness {
            let witness = journal.workspace.join(name);
            if !path_is_missing(&witness)? {
                if file_id(&witness, false)? != journal.target_id {
                    return Err("macOS view witness changed; it was retained".to_string());
                }
                fs::remove_file(&witness)
                    .map_err(|_| "macOS view witness cleanup failed".to_string())?;
            }
        }
        if !path_is_missing(&journal.workspace)? {
            cleanup_artifact(&journal, &journal.after)?;
            if let Some(before) = &journal.before {
                cleanup_artifact(&journal, before)?;
            }
            verify_workspace(&journal)?;
            fs::remove_dir(&journal.workspace).map_err(|_| {
                "macOS view workspace contains unexpected files; it was retained".to_string()
            })?;
        }
        let path = data_root.join(JOURNAL_NAME);
        let persisted = load_journal(&data_root)?
            .ok_or_else(|| "macOS view journal disappeared before cleanup".to_string())?;
        if persisted != journal {
            return Err("macOS view journal changed before cleanup".to_string());
        }
        fs::remove_file(path).map_err(|_| "macOS view journal cleanup failed".to_string())?;
        sync_directory(&data_root)
    }
}

pub(crate) fn prepare_transition(
    plan: &SessionViewPlan,
    operation_id: &str,
) -> Result<PreparedViewTransition, String> {
    let started = Instant::now();
    match &plan.transition {
        SessionViewTransition::None => return Ok(PreparedViewTransition::skipped(plan)),
        SessionViewTransition::BootstrapRelay {
            account,
            relay,
            state,
            session_view_state_path,
        } => {
            return prepare_empty_relay_bootstrap(
                plan,
                account,
                relay,
                state,
                session_view_state_path,
                started,
            )
        }
        SessionViewTransition::PublishLegacyAccount { .. } => {
            return Err(
                "Legacy Windows session-view migration is unavailable in the macOS preview"
                    .to_string(),
            );
        }
        _ => {}
    }
    if has_pending(&plan.data_root)? || load_transition_journal(&plan.data_root)?.is_some() {
        return Err("A session-view transaction requires recovery before switching".to_string());
    }
    validate_operation_id(operation_id)?;
    let (source_paths, target_paths, state, established, relay_active, provider) =
        match &plan.transition {
            SessionViewTransition::PrepareRelay {
                account,
                relay,
                state,
                view_established,
                ..
            } => (
                account,
                relay,
                state,
                *view_established,
                false,
                RELAY_PROVIDER,
            ),
            SessionViewTransition::PublishAccount {
                relay,
                account,
                state,
                ..
            } => (relay, account, state, true, true, "openai"),
            _ => unreachable!("handled non-synchronizing view transition"),
        };
    validate_state(state, &plan.data_root)?;
    ensure_relay_root(
        &state.relay_sqlite_home,
        &state.account_effective_sqlite_home,
        established,
    )?;
    let source = LockedDatabase::open(&source_paths.state_db)?;
    source.checkpoint()?;
    let source_digest = exact_digest(&source.connection)?;
    let common_digest = logical_state_digest(&source.connection)?;
    let target_existed = !path_is_missing(&target_paths.state_db)?;
    let mut existing_target = if target_existed {
        ensure_state_database_sidecars_absent(&target_paths.state_db)?;
        Some(LockedDatabase::open(&target_paths.state_db)?)
    } else {
        None
    };
    if let Some(target) = &existing_target {
        if target.id == source.id {
            return Err("Account and Relay state databases must be separate views".to_string());
        }
        let baseline = state.last_common_state_sha256.as_deref().ok_or_else(|| {
            "An independent target view has no verified common baseline".to_string()
        })?;
        if logical_state_digest(&target.connection)? != baseline {
            return Err("inactive session view changed; no database was overwritten".to_string());
        }
    } else if established && state.last_common_state_sha256.is_some() {
        return Err("An established inactive session database is missing".to_string());
    }

    let (globals, global_locks) = lock_globals(state, relay_active, established)?;
    private_dir_all(&plan.data_root)?;
    let parent = workspace_root(&state.account_effective_sqlite_home);
    private_dir_all(&parent)?;
    let workspace = parent.join(sanitized_operation_suffix(operation_id)?);
    private_dir_new(&workspace)?;
    let workspace_id = file_id(&workspace, true)?;
    let prepared = (|| {
        let after = snapshot(
            &source.connection,
            &workspace,
            "after.sqlite",
            Some(provider),
        )?;
        let after_connection = open_snapshot(&workspace.join(&after.name))?;
        let after_digest = exact_digest(&after_connection)?;
        if logical_state_digest(&after_connection)? != common_digest {
            return Err("Provider normalization changed session content".to_string());
        }
        let detected_threads = count_threads(&after_connection)?;
        drop(after_connection);
        let before = existing_target
            .as_ref()
            .map(|target| snapshot(&target.connection, &workspace, "before.sqlite", None))
            .transpose()?;
        let before_digest = existing_target
            .as_ref()
            .map(|target| exact_digest(&target.connection))
            .transpose()?;
        let (target_id, empty_digest, witness_name) = if let Some(target) = &existing_target {
            (target.id, None, None)
        } else {
            let witness = workspace.join("new-target.sqlite");
            create_private_file(&witness)?;
            let empty = LockedDatabase::open(&witness)?;
            let id = empty.id;
            let digest = exact_digest(&empty.connection)?;
            drop(empty);
            (id, Some(digest), Some("new-target.sqlite".to_string()))
        };
        let mut next_state = state.clone();
        next_state.last_common_state_sha256 = Some(common_digest.clone());
        let journal = Journal {
            version: 1,
            operation_id: operation_id.to_string(),
            workspace: workspace.clone(),
            workspace_id,
            source: source_paths.state_db.clone(),
            source_id: source.id,
            source_digest,
            target: target_paths.state_db.clone(),
            target_id,
            target_existed,
            before_digest: before_digest
                .or(empty_digest)
                .expect("target digest is recorded"),
            after_digest,
            before,
            after,
            new_target_witness: witness_name,
            globals,
            previous_state: load_state(&state_path(&plan.data_root), &plan.data_root)?,
            next_state,
            phase: Phase::Prepared,
        };
        write_journal(&plan.data_root, &journal, true)?;
        let apply = (|| {
            publish_globals(&journal.globals, &global_locks)?;
            if !target_existed {
                let witness = workspace.join(journal.new_target_witness.as_ref().unwrap());
                fs::hard_link(&witness, &journal.target).map_err(|_| {
                    "macOS target view appeared or cannot be linked; it was not overwritten"
                        .to_string()
                })?;
                sync_directory(target_paths.sqlite_home.as_path())?;
                existing_target = Some(LockedDatabase::open(&journal.target)?);
            }
            let mut target = existing_target
                .take()
                .ok_or_else(|| "macOS target view could not be acquired".to_string())?;
            if target.id != journal.target_id {
                return Err("macOS target view identity changed before apply".to_string());
            }
            let snapshot = open_artifact(&journal, &journal.after)?;
            backup_into(&snapshot, &mut target.connection)?;
            let mut transaction = PreparedTransaction {
                data_root: plan.data_root.clone(),
                journal,
                source,
                target,
                global_locks,
            };
            transaction.verify(&transaction.journal.after_digest)?;
            transaction.journal.phase = Phase::Applied;
            write_journal(&plan.data_root, &transaction.journal, false)?;
            let mut prepared = PreparedViewTransition::skipped(plan);
            prepared.receipt = IncrementalSessionSyncReceipt {
                status: IncrementalSessionSyncStatus::Applied,
                detected_threads,
                synced_threads: detected_threads,
                projected_bytes: fs::metadata(&transaction.source.path)
                    .map_err(|_| "macOS source size is unavailable".to_string())?
                    .len(),
                duration_ms: started.elapsed().as_millis(),
                requires_full_sync: false,
            };
            prepared.macos = Some(transaction);
            Ok(prepared)
        })();
        apply
    })();
    if prepared.is_err() {
        if has_pending(&plan.data_root).unwrap_or(true) {
            return prepared.map_err(|error: String| {
                format!("{error}; the durable macOS view transaction was retained for recovery")
            });
        }
        // This directory was newly created by this attempt and no live write
        // occurs until its journal is durable. Do not touch unknown entries.
        for name in ["after.sqlite", "before.sqlite", "new-target.sqlite"] {
            let path = workspace.join(name);
            if let Ok(metadata) = fs::symlink_metadata(&path) {
                if metadata.is_file() && !metadata.file_type().is_symlink() {
                    let _ = fs::remove_file(path);
                }
            }
        }
        let _ = fs::remove_dir(&workspace);
    }
    prepared
}

fn lock_globals(
    state: &SessionViewState,
    relay_active: bool,
    established: bool,
) -> Result<(Vec<GlobalLink>, Vec<LockedDatabase>), String> {
    let mut links = Vec::new();
    let mut locks = Vec::new();
    for name in GLOBAL_DATABASES {
        let account = state.account_effective_sqlite_home.join(name);
        let relay = state.relay_sqlite_home.join(name);
        let account_present = !path_is_missing(&account)?;
        let relay_present = !path_is_missing(&relay)?;
        let (active, inactive, existed) = match (account_present, relay_present) {
            (false, false) => continue,
            (true, true) => {
                if file_id(&account, false)? != file_id(&relay, false)? {
                    return Err(format!(
                        "{name} is an independent database; no file was replaced"
                    ));
                }
                if relay_active {
                    (relay, account, true)
                } else {
                    (account, relay, true)
                }
            }
            (true, false) if !(relay_active && established) => (account, relay, false),
            (false, true) if relay_active && established => (relay, account, false),
            _ => {
                return Err(format!(
                    "{name} is missing from the active view or is unowned"
                ))
            }
        };
        ensure_sqlite_sidecars_absent(&inactive, name)?;
        let lock = LockedDatabase::open(&active)?;
        lock.checkpoint()?;
        ensure_sqlite_sidecars_absent(&inactive, name)?;
        links.push(GlobalLink {
            source: active,
            target: inactive,
            id: lock.id,
            existed,
        });
        locks.push(lock);
    }
    Ok((links, locks))
}

fn publish_globals(links: &[GlobalLink], locks: &[LockedDatabase]) -> Result<(), String> {
    for link in links {
        if !link.existed && path_is_missing(&link.target)? {
            fs::hard_link(&link.source, &link.target)
                .map_err(|_| "macOS shared database hard link could not be created".to_string())?;
            sync_directory(link.target.parent().ok_or("shared view has no parent")?)?;
        }
    }
    verify_globals(links, locks)
}

fn verify_globals(links: &[GlobalLink], locks: &[LockedDatabase]) -> Result<(), String> {
    if links.len() != locks.len() {
        return Err("macOS shared database lock inventory changed".to_string());
    }
    for (link, lock) in links.iter().zip(locks) {
        lock.verify_identity()?;
        if lock.id != link.id || file_id(&link.target, false)? != link.id {
            return Err("macOS shared database identity changed".to_string());
        }
        ensure_sqlite_sidecars_absent(&link.target, "inactive shared database")?;
    }
    Ok(())
}

pub(super) fn has_pending(data_root: &Path) -> Result<bool, String> {
    Ok(!path_is_missing(&data_root.join(JOURNAL_NAME))?)
}

pub(super) fn recover(codex_home: &Path, data_root: &Path) -> Result<bool, String> {
    let Some(journal) = load_journal(data_root)? else {
        return Ok(false);
    };
    validate_journal(data_root, &journal)?;
    let current = resolve_user_codex_paths(codex_home)?.sqlite_home;
    let source_home = journal
        .source
        .parent()
        .ok_or("macOS source has no parent")?;
    let target_home = journal
        .target
        .parent()
        .ok_or("macOS target has no parent")?;
    let commit = if route_matches(&current, target_home)? {
        true
    } else if route_matches(&current, source_home)? {
        false
    } else {
        return Err("Current SQLite Home matches neither macOS recovery route".to_string());
    };
    if !commit && journal.phase == Phase::Committed {
        return Err("Committed macOS view journal conflicts with the live route".to_string());
    }
    let source = LockedDatabase::open(&journal.source)?;
    if source.id != journal.source_id || exact_digest(&source.connection)? != journal.source_digest
    {
        return Err(
            "macOS source changed since interruption; recovery did not overwrite data".to_string(),
        );
    }
    let mut global_locks = Vec::new();
    for link in &journal.globals {
        let lock = LockedDatabase::open(&link.source)?;
        if lock.id != link.id {
            return Err("macOS shared source identity changed before recovery".to_string());
        }
        lock.checkpoint()?;
        global_locks.push(lock);
    }
    publish_globals(&journal.globals, &global_locks)?;
    if path_is_missing(&journal.target)? {
        if journal.target_existed {
            return Err(
                "macOS original target disappeared; recovery did not recreate it".to_string(),
            );
        }
        let witness = journal.workspace.join(
            journal
                .new_target_witness
                .as_ref()
                .ok_or("missing view witness")?,
        );
        if file_id(&witness, false)? != journal.target_id {
            return Err("macOS new-view witness identity changed".to_string());
        }
        fs::hard_link(witness, &journal.target)
            .map_err(|_| "macOS new view could not be published during recovery".to_string())?;
    }
    let target = LockedDatabase::open(&journal.target)?;
    if target.id != journal.target_id {
        return Err("macOS target identity changed; recovery did not overwrite it".to_string());
    }
    let transaction = PreparedTransaction {
        data_root: data_root.to_path_buf(),
        journal,
        source,
        target,
        global_locks,
    };
    if commit {
        transaction.commit()?;
    } else {
        transaction.rollback()?;
    }
    Ok(true)
}

fn snapshot(
    source: &Connection,
    workspace: &Path,
    name: &str,
    provider: Option<&str>,
) -> Result<Artifact, String> {
    let path = workspace.join(name);
    create_private_file(&path)?;
    let mut target = Connection::open(&path)
        .map_err(|_| "macOS session snapshot could not be created".to_string())?;
    backup_into(source, &mut target)?;
    if let Some(provider) = provider {
        normalize_thread_provider(&target, provider)?;
    }
    verify_database(&target, "macOS session snapshot")?;
    target
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;")
        .map_err(|_| "macOS session snapshot could not be finalized".to_string())?;
    drop(target);
    ensure_state_database_sidecars_absent(&path)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|_| "macOS snapshot is unavailable".to_string())?;
    file.sync_all()
        .map_err(|_| "macOS snapshot could not be flushed".to_string())?;
    drop(file);
    sync_directory(workspace)?;
    Ok(Artifact {
        name: name.to_string(),
        id: file_id(&path, false)?,
        sha256: file_sha256(&path)?,
    })
}

fn backup_into(source: &Connection, target: &mut Connection) -> Result<(), String> {
    let backup = Backup::new(source, target)
        .map_err(|_| "macOS SQLite Online Backup could not start".to_string())?;
    let started = Instant::now();
    loop {
        match backup
            .step(256)
            .map_err(|_| "macOS SQLite Online Backup failed".to_string())?
        {
            StepResult::Done => return Ok(()),
            StepResult::More if started.elapsed() < Duration::from_secs(60) => {}
            StepResult::Busy | StepResult::Locked => {
                return Err(
                    "macOS SQLite Online Backup found an active reader or writer".to_string(),
                )
            }
            _ => return Err("macOS SQLite Online Backup exceeded its bounded duration".to_string()),
        }
    }
}

fn exact_digest(connection: &Connection) -> Result<String, String> {
    let mut hasher = Sha256::new();
    hash_field(&mut hasher, logical_state_digest(connection)?.as_bytes());
    let mut statement = connection
        .prepare(
            "SELECT type, name, tbl_name, COALESCE(sql, '') FROM sqlite_master ORDER BY type,name",
        )
        .map_err(|_| "macOS database schema is unavailable".to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok([
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ])
        })
        .map_err(|_| "macOS schema digest failed".to_string())?;
    for row in rows {
        for value in row.map_err(|_| "macOS schema digest failed".to_string())? {
            hash_field(&mut hasher, value.as_bytes());
        }
    }
    let has_threads: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='threads'",
            [],
            |row| row.get(0),
        )
        .map_err(|_| "macOS thread schema is unavailable".to_string())?;
    if has_threads != 0 {
        let mut statement = connection
            .prepare("SELECT id, model_provider FROM threads ORDER BY id")
            .map_err(|_| "macOS thread provider schema is unavailable".to_string())?;
        let mut rows = statement
            .query([])
            .map_err(|_| "macOS thread provider digest failed".to_string())?;
        while let Some(row) = rows
            .next()
            .map_err(|_| "macOS thread provider digest failed".to_string())?
        {
            for column in 0..2 {
                hash_typed_value(
                    &mut hasher,
                    row.get_ref(column)
                        .map_err(|_| "macOS thread provider digest failed".to_string())?,
                );
            }
        }
    }
    // The reusable v2 baseline deliberately ignores SQLite's internal tables.
    // Recovery must also detect changes to sequence and optimizer metadata.
    let mut internal_tables = connection
        .prepare(
            "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'sqlite_%' ORDER BY name",
        )
        .map_err(|_| "macOS internal table inventory is unavailable".to_string())?;
    let tables: Vec<String> = internal_tables
        .query_map([], |row| row.get(0))
        .and_then(|rows| rows.collect())
        .map_err(|_| "macOS internal table inventory failed".to_string())?;
    for table in tables {
        hash_field(&mut hasher, table.as_bytes());
        let mut statement = connection
            .prepare(&format!("SELECT * FROM {}", quote_identifier(&table)))
            .map_err(|_| "macOS internal table is unavailable".to_string())?;
        let columns = statement.column_count();
        let mut rows = statement
            .query([])
            .map_err(|_| "macOS internal table digest failed".to_string())?;
        let mut row_hashes = Vec::<[u8; 32]>::new();
        while let Some(row) = rows
            .next()
            .map_err(|_| "macOS internal table digest failed".to_string())?
        {
            let mut row_hasher = Sha256::new();
            for column in 0..columns {
                hash_typed_value(
                    &mut row_hasher,
                    row.get_ref(column)
                        .map_err(|_| "macOS internal table value is unavailable".to_string())?,
                );
            }
            row_hashes.push(row_hasher.finalize().into());
        }
        row_hashes.sort_unstable();
        hash_field(&mut hasher, &(row_hashes.len() as u64).to_le_bytes());
        for digest in row_hashes {
            hash_field(&mut hasher, &digest);
        }
    }
    for pragma in ["PRAGMA user_version", "PRAGMA application_id"] {
        let value: i64 = connection
            .query_row(pragma, [], |row| row.get(0))
            .map_err(|_| "macOS database metadata is unavailable".to_string())?;
        hash_field(&mut hasher, &value.to_le_bytes());
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn open_snapshot(path: &Path) -> Result<Connection, String> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| "macOS session snapshot could not be opened".to_string())
}

fn open_artifact(journal: &Journal, artifact: &Artifact) -> Result<Connection, String> {
    verify_workspace(journal)?;
    let path = journal.workspace.join(&artifact.name);
    if file_id(&path, false)? != artifact.id || file_sha256(&path)? != artifact.sha256 {
        return Err("macOS session snapshot changed; no data was overwritten".to_string());
    }
    let connection = open_snapshot(&path)?;
    verify_database(&connection, "macOS session snapshot")?;
    Ok(connection)
}

fn cleanup_artifact(journal: &Journal, artifact: &Artifact) -> Result<(), String> {
    let path = journal.workspace.join(&artifact.name);
    if path_is_missing(&path)? {
        return Ok(());
    }
    if file_id(&path, false)? != artifact.id || file_sha256(&path)? != artifact.sha256 {
        return Err("macOS session snapshot changed; cleanup retained it".to_string());
    }
    fs::remove_file(path).map_err(|_| "macOS snapshot cleanup failed".to_string())
}

fn verify_workspace(journal: &Journal) -> Result<(), String> {
    if matches!(journal.phase, Phase::Committed | Phase::RolledBack)
        && path_is_missing(&journal.workspace)?
    {
        return Ok(());
    }
    if file_id(&journal.workspace, true)? != journal.workspace_id {
        return Err("macOS transaction workspace identity changed".to_string());
    }
    Ok(())
}

fn load_journal(data_root: &Path) -> Result<Option<Journal>, String> {
    let path = data_root.join(JOURNAL_NAME);
    if path_is_missing(&path)? {
        return Ok(None);
    }
    let bytes = read_regular_file_bounded(&path, JOURNAL_LIMIT)
        .map_err(|_| "macOS view journal is unreadable".to_string())?;
    let plaintext = crate::crypto::unprotect(&bytes)
        .map_err(|_| "macOS view journal could not be decrypted".to_string())?;
    let journal: Journal = serde_json::from_slice(&plaintext)
        .map_err(|_| "macOS view journal is invalid".to_string())?;
    validate_journal(data_root, &journal)?;
    Ok(Some(journal))
}

fn write_journal(data_root: &Path, journal: &Journal, create: bool) -> Result<(), String> {
    validate_journal(data_root, journal)?;
    let plaintext = serde_json::to_vec(journal)
        .map_err(|_| "macOS view journal could not be serialized".to_string())?;
    let bytes = crate::crypto::protect(&plaintext)
        .map_err(|_| "macOS view journal could not be encrypted".to_string())?;
    if bytes.len() as u64 > JOURNAL_LIMIT {
        return Err("macOS view journal exceeded its size bound".to_string());
    }
    let path = data_root.join(JOURNAL_NAME);
    if create {
        if !atomic_create(&path, |file| {
            file.write_all(&bytes)
                .map_err(|_| "macOS view journal could not be written".to_string())
        })? {
            return Err("A macOS view journal already exists".to_string());
        }
    } else {
        let mut previous =
            load_journal(data_root)?.ok_or_else(|| "macOS view journal disappeared".to_string())?;
        previous.phase = journal.phase;
        if previous != *journal {
            return Err("macOS view journal changed externally".to_string());
        }
        atomic_write(&path, &bytes)?;
    }
    sync_directory(data_root)
}

fn validate_journal(data_root: &Path, journal: &Journal) -> Result<(), String> {
    validate_operation_id(&journal.operation_id)?;
    validate_state(&journal.next_state, data_root)?;
    let expected_workspace = workspace_root(&journal.next_state.account_effective_sqlite_home)
        .join(sanitized_operation_suffix(&journal.operation_id)?);
    let account = journal
        .next_state
        .account_effective_sqlite_home
        .join(STATE_DATABASE);
    let relay = journal.next_state.relay_sqlite_home.join(STATE_DATABASE);
    if journal.version != 1
        || journal.workspace != expected_workspace
        || !((journal.source == account && journal.target == relay)
            || (journal.source == relay && journal.target == account))
        || journal.source_id == journal.target_id
        || !valid_sha256(&journal.source_digest)
        || !valid_sha256(&journal.before_digest)
        || !valid_sha256(&journal.after_digest)
        || journal.after.name != "after.sqlite"
        || journal
            .before
            .as_ref()
            .is_some_and(|a| a.name != "before.sqlite")
        || journal.target_existed != journal.before.is_some()
        || (journal.target_existed && journal.previous_state.is_none())
        || journal.new_target_witness.as_deref()
            != (!journal.target_existed).then_some("new-target.sqlite")
        || journal.globals.len() > GLOBAL_DATABASES.len()
    {
        return Err("macOS view journal failed its path and ownership contract".to_string());
    }
    for artifact in std::iter::once(&journal.after).chain(journal.before.iter()) {
        if !valid_sha256(&artifact.sha256) {
            return Err("macOS view snapshot digest is invalid".to_string());
        }
    }
    let mut names = std::collections::HashSet::new();
    for link in &journal.globals {
        let name = link
            .source
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or("invalid shared database name")?;
        if !GLOBAL_DATABASES.contains(&name)
            || !names.insert(name)
            || link.source.parent() != journal.source.parent()
            || link.target.parent() != journal.target.parent()
            || link.target.file_name() != link.source.file_name()
        {
            return Err("macOS shared-view journal path is invalid".to_string());
        }
    }
    Ok(())
}

#[cfg(unix)]
fn file_id(path: &Path, directory: bool) -> Result<FileId, String> {
    use std::os::unix::fs::MetadataExt;
    let metadata =
        fs::symlink_metadata(path).map_err(|_| "macOS view path is unavailable".to_string())?;
    if metadata.file_type().is_symlink()
        || (directory && !metadata.is_dir())
        || (!directory && !metadata.is_file())
    {
        return Err("macOS view path is not a safe regular file or directory".to_string());
    }
    Ok(FileId {
        volume: metadata.dev(),
        file: metadata.ino(),
    })
}

#[cfg(windows)]
fn file_id(path: &Path, directory: bool) -> Result<FileId, String> {
    // Test-only support for exercising the macOS SQLite transaction protocol on
    // Windows. Query identity without acquiring the production destructive guard.
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    let metadata =
        fs::symlink_metadata(path).map_err(|_| "view path is unavailable".to_string())?;
    if metadata.file_type().is_symlink()
        || (directory && !metadata.is_dir())
        || (!directory && !metadata.is_file())
    {
        return Err("view path is unsafe".to_string());
    }
    let file = OpenOptions::new()
        .access_mode(0)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|_| "view identity is unavailable".to_string())?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), &mut info) } == 0 {
        return Err("view identity is unavailable".to_string());
    }
    Ok(FileId {
        volume: u64::from(info.dwVolumeSerialNumber),
        file: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    })
}

fn create_private_file(path: &Path) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .map_err(|_| "macOS private snapshot path is already present or unavailable".to_string())?;
    file.sync_all()
        .map_err(|_| "macOS snapshot allocation could not be flushed".to_string())
}

fn workspace_root(account_sqlite_home: &Path) -> PathBuf {
    // The new-view hard-link witness must live on the Account/Relay volume,
    // including when CODEX_SQLITE_HOME points to an external disk.
    account_sqlite_home
        .join(MANAGED_VIEW_DIRECTORY)
        .join(WORKSPACE_NAME)
}

fn private_dir_all(path: &Path) -> Result<(), String> {
    if path_is_missing(path)? {
        private_dir_new(path)?;
    }
    file_id(path, true)?;
    Ok(())
}

fn private_dir_new(path: &Path) -> Result<(), String> {
    let builder = fs::DirBuilder::new();
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = builder;
        builder.mode(0o700);
        builder
    };
    builder
        .create(path)
        .map_err(|_| "macOS private view workspace already exists or is unavailable".to_string())
}

fn sync_directory(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        File::open(path)
            .and_then(|file| file.sync_all())
            .map_err(|_| "macOS view directory could not be flushed".to_string())?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    struct Fixture {
        _temp: TempDir,
        home: PathBuf,
        data: PathBuf,
        canonical: Vec<u8>,
    }

    impl Fixture {
        fn new(mode: &str) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = fs::canonicalize(temp.path()).unwrap();
            let home = root.join("codex");
            let data = root.join("switch");
            fs::create_dir_all(home.join("sessions")).unwrap();
            fs::create_dir(&data).unwrap();
            let canonical = b"{\"session\":\"canonical-body-must-not-change\"}\n".to_vec();
            fs::write(home.join("sessions/one.jsonl"), &canonical).unwrap();
            fs::write(home.join("config.toml"), "model_provider = \"openai\"\n").unwrap();
            let db = Connection::open(home.join(STATE_DATABASE)).unwrap();
            db.pragma_update(None, "journal_mode", mode).unwrap();
            db.execute_batch(
                "CREATE TABLE threads(id TEXT PRIMARY KEY, model_provider TEXT NOT NULL, title TEXT);
                 INSERT INTO threads VALUES('one','openai','first conversation');"
            ).unwrap();
            drop(db);
            for name in GLOBAL_DATABASES {
                let db = Connection::open(home.join(name)).unwrap();
                db.execute_batch(
                    "CREATE TABLE data(value TEXT); INSERT INTO data VALUES('shared');",
                )
                .unwrap();
            }
            Self {
                _temp: temp,
                home,
                data,
                canonical,
            }
        }

        fn plan(&self, target: SessionViewTarget) -> SessionViewPlan {
            let config = fs::read_to_string(self.home.join("config.toml")).unwrap();
            super::super::plan_transition(&self.home, &config, target, &self.data).unwrap()
        }

        fn select(&self, target: SessionViewTarget) {
            let config = if target == SessionViewTarget::Relay {
                let relay = managed_relay_sqlite_home(&self.home);
                format!(
                    "model_provider = \"openai_custom\"\nsqlite_home = '{}'\n",
                    relay.display()
                )
            } else {
                "model_provider = \"openai\"\n".to_string()
            };
            fs::write(self.home.join("config.toml"), config).unwrap();
        }

        fn relay(&self) -> PathBuf {
            managed_relay_sqlite_home(&self.home)
        }

        fn providers(&self, root: &Path) -> Vec<(String, String)> {
            let db = Connection::open(root.join(STATE_DATABASE)).unwrap();
            let result = db
                .prepare("SELECT id,model_provider FROM threads ORDER BY id")
                .unwrap()
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            result
        }

        fn assert_clean(&self) {
            assert!(!has_pending(&self.data).unwrap());
            let workspace = workspace_root(&self.home);
            if workspace.exists() {
                assert_eq!(fs::read_dir(workspace).unwrap().count(), 0);
            }
            assert_eq!(
                fs::read(self.home.join("sessions/one.jsonl")).unwrap(),
                self.canonical
            );
        }
    }

    #[test]
    fn mature_home_round_trip_preserves_threads_canonical_body_and_shared_file_ids() {
        for mode in ["delete", "wal"] {
            let fixture = Fixture::new(mode);
            let plan = fixture.plan(SessionViewTarget::Relay);
            let prepared = prepare_transition(&plan, &format!("mac-roundtrip-{mode}")).unwrap();
            fixture.select(SessionViewTarget::Relay);
            commit_transition(prepared).unwrap();
            assert_eq!(
                fixture.providers(&fixture.relay()),
                vec![("one".into(), RELAY_PROVIDER.into())]
            );
            for name in GLOBAL_DATABASES {
                assert_eq!(
                    file_id(&fixture.home.join(name), false).unwrap(),
                    file_id(&fixture.relay().join(name), false).unwrap()
                );
            }
            let db = Connection::open(fixture.relay().join(STATE_DATABASE)).unwrap();
            db.execute(
                "INSERT INTO threads VALUES('two',?1,'new relay conversation')",
                [RELAY_PROVIDER],
            )
            .unwrap();
            drop(db);
            let prepared = prepare_transition(
                &fixture.plan(SessionViewTarget::Account),
                &format!("mac-return-{mode}"),
            )
            .unwrap();
            fixture.select(SessionViewTarget::Account);
            commit_transition(prepared).unwrap();
            assert_eq!(
                fixture.providers(&fixture.home),
                vec![
                    ("one".into(), "openai".into()),
                    ("two".into(), "openai".into())
                ]
            );
            fixture.assert_clean();
        }
    }

    #[test]
    fn existing_target_rollback_restores_its_sqlite_contents() {
        let fixture = Fixture::new("wal");
        let prepared =
            prepare_transition(&fixture.plan(SessionViewTarget::Relay), "mac-rollback-seed")
                .unwrap();
        fixture.select(SessionViewTarget::Relay);
        commit_transition(prepared).unwrap();
        let original = fixture.providers(&fixture.home);
        let db = Connection::open(fixture.relay().join(STATE_DATABASE)).unwrap();
        db.execute(
            "INSERT INTO threads VALUES('two',?1,'new relay conversation')",
            [RELAY_PROVIDER],
        )
        .unwrap();
        drop(db);
        let prepared = prepare_transition(
            &fixture.plan(SessionViewTarget::Account),
            "mac-rollback-existing",
        )
        .unwrap();
        rollback_transition(prepared).unwrap();
        assert_eq!(fixture.providers(&fixture.home), original);
        assert_eq!(fixture.providers(&fixture.relay()).len(), 2);
        fixture.assert_clean();
    }

    #[test]
    fn new_inactive_view_is_retained_with_a_verified_baseline_after_rollback() {
        let fixture = Fixture::new("delete");
        let prepared =
            prepare_transition(&fixture.plan(SessionViewTarget::Relay), "mac-rollback-new")
                .unwrap();
        rollback_transition(prepared).unwrap();
        assert_eq!(
            fixture.providers(&fixture.home),
            vec![("one".into(), "openai".into())]
        );
        assert_eq!(
            fixture.providers(&fixture.relay()),
            vec![("one".into(), RELAY_PROVIDER.into())]
        );
        fixture.assert_clean();
        let prepared =
            prepare_transition(&fixture.plan(SessionViewTarget::Relay), "mac-retry-new").unwrap();
        fixture.select(SessionViewTarget::Relay);
        commit_transition(prepared).unwrap();
        fixture.assert_clean();
    }

    #[test]
    fn inactive_database_drift_is_rejected_without_overwriting_it() {
        let fixture = Fixture::new("delete");
        let prepared =
            prepare_transition(&fixture.plan(SessionViewTarget::Relay), "mac-drift-seed").unwrap();
        fixture.select(SessionViewTarget::Relay);
        commit_transition(prepared).unwrap();
        let db = Connection::open(fixture.home.join(STATE_DATABASE)).unwrap();
        db.execute(
            "INSERT INTO threads VALUES('external','openai','must survive')",
            [],
        )
        .unwrap();
        drop(db);
        let error = prepare_transition(
            &fixture.plan(SessionViewTarget::Account),
            "mac-reject-drift",
        )
        .unwrap_err();
        assert!(error.contains("inactive session view changed"), "{error}");
        assert!(fixture
            .providers(&fixture.home)
            .iter()
            .any(|row| row.0 == "external"));
        fixture.assert_clean();
    }

    #[test]
    fn interrupted_prepare_recovers_in_the_direction_of_the_persisted_route() {
        for commit in [false, true] {
            let fixture = Fixture::new("wal");
            let prepared = prepare_transition(
                &fixture.plan(SessionViewTarget::Relay),
                if commit {
                    "mac-recover-commit"
                } else {
                    "mac-recover-rollback"
                },
            )
            .unwrap();
            if commit {
                fixture.select(SessionViewTarget::Relay);
            }
            drop(prepared);
            assert!(has_pending(&fixture.data).unwrap());
            assert!(recover(&fixture.home, &fixture.data).unwrap());
            assert!(!recover(&fixture.home, &fixture.data).unwrap());
            assert_eq!(
                fixture.providers(&fixture.home),
                vec![("one".into(), "openai".into())]
            );
            assert_eq!(
                fixture.providers(&fixture.relay()),
                vec![("one".into(), RELAY_PROVIDER.into())]
            );
            fixture.assert_clean();
        }
    }

    #[test]
    fn recovery_refuses_an_external_database_change() {
        let fixture = Fixture::new("wal");
        let prepared =
            prepare_transition(&fixture.plan(SessionViewTarget::Relay), "mac-recover-drift")
                .unwrap();
        fixture.select(SessionViewTarget::Relay);
        drop(prepared);
        let db = Connection::open(fixture.relay().join(STATE_DATABASE)).unwrap();
        db.execute(
            "INSERT INTO threads VALUES('external',?1,'must survive')",
            [RELAY_PROVIDER],
        )
        .unwrap();
        drop(db);
        assert!(recover(&fixture.home, &fixture.data).is_err());
        assert!(fixture
            .providers(&fixture.relay())
            .iter()
            .any(|row| row.0 == "external"));
        assert!(has_pending(&fixture.data).unwrap());
    }

    #[test]
    fn equal_bytes_at_a_replaced_target_inode_are_not_recovered_or_deleted() {
        let fixture = Fixture::new("delete");
        let prepared = prepare_transition(
            &fixture.plan(SessionViewTarget::Relay),
            "mac-replaced-inode",
        )
        .unwrap();
        fixture.select(SessionViewTarget::Relay);
        drop(prepared);
        let target = fixture.relay().join(STATE_DATABASE);
        let replacement = fixture.relay().join("external-replacement.sqlite");
        fs::copy(&target, &replacement).unwrap();
        fs::remove_file(&target).unwrap();
        fs::rename(&replacement, &target).unwrap();
        let replacement_id = file_id(&target, false).unwrap();
        let bytes = fs::read(&target).unwrap();
        assert!(recover(&fixture.home, &fixture.data)
            .unwrap_err()
            .contains("identity changed"));
        assert_eq!(file_id(&target, false).unwrap(), replacement_id);
        assert_eq!(fs::read(&target).unwrap(), bytes);
        assert!(has_pending(&fixture.data).unwrap());
    }

    #[test]
    fn late_inactive_global_sidecars_block_commit_and_remain_untouched() {
        for suffix in ["-wal", "-shm", "-journal"] {
            let fixture = Fixture::new("delete");
            let prepared = prepare_transition(
                &fixture.plan(SessionViewTarget::Relay),
                &format!("mac-late-{suffix}"),
            )
            .unwrap();
            let sidecar = fixture.relay().join(format!("logs_2.sqlite{suffix}"));
            fs::write(&sidecar, b"external-sidecar-must-survive").unwrap();
            assert!(commit_transition(prepared).unwrap_err().contains("sidecar"));
            assert_eq!(
                fs::read(&sidecar).unwrap(),
                b"external-sidecar-must-survive"
            );
            assert!(has_pending(&fixture.data).unwrap());
        }
    }

    #[test]
    fn legacy_windows_view_migration_is_rejected_without_database_writes() {
        let fixture = Fixture::new("delete");
        let legacy = fixture.data.join("relay-sqlite");
        fs::create_dir(&legacy).unwrap();
        fs::copy(
            fixture.home.join(STATE_DATABASE),
            legacy.join(STATE_DATABASE),
        )
        .unwrap();
        let source = fs::read(fixture.home.join(STATE_DATABASE)).unwrap();
        let target = fs::read(legacy.join(STATE_DATABASE)).unwrap();
        fs::write(
            fixture.data.join("request-route-session-view-v1.json"),
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "accountConfiguredSqliteHome": fixture.home.to_string_lossy(),
                "accountEffectiveSqliteHome": fixture.home,
                "relaySqliteHome": legacy,
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            fixture.home.join("config.toml"),
            format!("sqlite_home = {:?}\n", legacy.to_string_lossy()),
        )
        .unwrap();
        let error = prepare_transition(
            &fixture.plan(SessionViewTarget::Account),
            "mac-legacy-rejected",
        )
        .unwrap_err();
        assert!(error.contains("Legacy Windows session-view migration"));
        assert_eq!(fs::read(fixture.home.join(STATE_DATABASE)).unwrap(), source);
        assert_eq!(fs::read(legacy.join(STATE_DATABASE)).unwrap(), target);
        fixture.assert_clean();
    }

    #[test]
    fn recovery_preserves_external_autoincrement_sequence_changes() {
        let fixture = Fixture::new("delete");
        let source = Connection::open(fixture.home.join(STATE_DATABASE)).unwrap();
        source
            .execute_batch(
                "CREATE TABLE counters(id INTEGER PRIMARY KEY AUTOINCREMENT, value TEXT);
             INSERT INTO counters(value) VALUES('baseline');",
            )
            .unwrap();
        drop(source);
        let prepared = prepare_transition(
            &fixture.plan(SessionViewTarget::Relay),
            "mac-recover-sequence-drift",
        )
        .unwrap();
        fixture.select(SessionViewTarget::Relay);
        drop(prepared);
        let external = Connection::open(fixture.relay().join(STATE_DATABASE)).unwrap();
        external
            .execute_batch(
                "INSERT INTO counters(value) VALUES('external');
             DELETE FROM counters WHERE value='external';",
            )
            .unwrap();
        drop(external);
        assert!(recover(&fixture.home, &fixture.data).is_err());
        let preserved = Connection::open(fixture.relay().join(STATE_DATABASE)).unwrap();
        let sequence: i64 = preserved
            .query_row(
                "SELECT seq FROM sqlite_sequence WHERE name='counters'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(sequence, 2);
        assert!(has_pending(&fixture.data).unwrap());
    }

    #[test]
    fn real_second_process_cannot_write_either_view_while_config_is_pending() {
        for mode in ["delete", "wal"] {
            let fixture = Fixture::new(mode);
            let prepared = prepare_transition(
                &fixture.plan(SessionViewTarget::Relay),
                &format!("mac-lock-{mode}"),
            )
            .unwrap();
            for path in [
                fixture.home.join(STATE_DATABASE),
                fixture.relay().join(STATE_DATABASE),
            ] {
                let output = Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "runtime_session_view::macos::tests::external_sqlite_writer_probe",
                        "--nocapture",
                    ])
                    .env("CODEX_SWITCH_MAC_LOCK_PROBE", path)
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert!(
                    String::from_utf8_lossy(&output.stdout).contains("MAC_SQLITE_WRITER_BLOCKED")
                );
            }
            rollback_transition(prepared).unwrap();
            fixture.assert_clean();
        }
    }

    #[test]
    fn production_metadata_observers_preserve_state_and_goals_locks() {
        use crate::{
            runtime_compatibility::{inspect_runtime_compatibility, StateDatabaseCompatibility},
            session_storage::catalog::discover_database_catalog,
        };

        for mode in ["delete", "wal"] {
            let fixture = Fixture::new(mode);
            for name in GLOBAL_DATABASES {
                let db = Connection::open(fixture.home.join(name)).unwrap();
                db.pragma_update(None, "journal_mode", mode).unwrap();
            }
            // Establish both persisted views so the real catalog must compare
            // the two goals aliases instead of accepting one file without a
            // physical-identity check.
            let seed = prepare_transition(
                &fixture.plan(SessionViewTarget::Relay),
                &format!("mac-observers-seed-{mode}"),
            )
            .unwrap();
            fixture.select(SessionViewTarget::Relay);
            commit_transition(seed).unwrap();

            let prepared = prepare_transition(
                &fixture.plan(SessionViewTarget::Account),
                &format!("mac-observers-held-{mode}"),
            )
            .unwrap();
            for selected in [SessionViewTarget::Relay, SessionViewTarget::Account] {
                fixture.select(selected);
                let report = inspect_runtime_compatibility(&fixture.home);
                assert_ne!(report.state_database, StateDatabaseCompatibility::Absent);
                let catalog = discover_database_catalog(&fixture.home, &fixture.data);
                assert_eq!(catalog.errors, 0);
                assert_eq!(catalog.goals_errors, 0);
                assert_eq!(catalog.goals_descriptors.len(), 1);
                let aliases = &catalog.goals_descriptors[0].views;
                assert_eq!(aliases.len(), 2);
                for root in [&fixture.home, &fixture.relay()] {
                    assert!(aliases
                        .iter()
                        .any(|view| view.path == root.join("goals_1.sqlite")));
                }
            }

            // POSIX record locks belong to the process. A raw close in either
            // observer used to cancel another thread's retained SQLite lock.
            // A real child must still be blocked after BOTH production paths.
            for root in [&fixture.home, &fixture.relay()] {
                assert_external_writer_blocked(&root.join(STATE_DATABASE), false);
                assert_external_writer_blocked(&root.join("goals_1.sqlite"), true);
            }
            commit_transition(prepared).unwrap();
            fixture.assert_clean();
        }
    }

    fn assert_external_writer_blocked(path: &Path, goals: bool) {
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "runtime_session_view::macos::tests::external_sqlite_writer_probe",
                "--nocapture",
            ])
            .env("CODEX_SWITCH_MAC_LOCK_PROBE", path)
            .env("CODEX_SWITCH_MAC_LOCK_GOALS", if goals { "1" } else { "0" })
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "writer was not blocked for {}: {} {}",
            path.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("MAC_SQLITE_WRITER_BLOCKED"));
    }

    #[test]
    fn external_sqlite_writer_probe() {
        let Some(path) = std::env::var_os("CODEX_SWITCH_MAC_LOCK_PROBE") else {
            return;
        };
        let connection = Connection::open(PathBuf::from(path)).unwrap();
        connection.busy_timeout(Duration::ZERO).unwrap();
        let sql = if std::env::var("CODEX_SWITCH_MAC_LOCK_GOALS").as_deref() == Ok("1") {
            "UPDATE data SET value='unexpected-writer'"
        } else {
            "UPDATE threads SET title='unexpected-writer'"
        };
        let error = connection.execute(sql, []).unwrap_err();
        let code = error.sqlite_error_code().unwrap();
        assert!(matches!(
            code,
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
        ));
        println!("MAC_SQLITE_WRITER_BLOCKED");
    }

    #[cfg(unix)]
    #[test]
    fn macos_snapshot_and_workspace_permissions_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new("delete");
        let prepared =
            prepare_transition(&fixture.plan(SessionViewTarget::Relay), "mac-permissions").unwrap();
        let transaction = prepared.macos.as_ref().unwrap();
        assert_eq!(
            fs::metadata(&transaction.journal.workspace)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(transaction.journal.workspace.join("after.sqlite"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        rollback_transition(prepared).unwrap();
    }
}

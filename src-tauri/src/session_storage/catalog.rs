//! Runtime database discovery and SQLite snapshotting live here.
//!
//! beta.1 intentionally keeps this module read/scan-only. It contains no
//! database update or file deletion entry point.

use std::{
    collections::BTreeMap,
    fs,
    path::{Component, Path, PathBuf},
};

use rusqlite::{types::Value as SqlValue, Connection, OpenFlags, MAIN_DB};
use sha2::{Digest, Sha256};
use toml_edit::DocumentMut;
use walkdir::WalkDir;

use super::{
    bounded_file::{read_regular_file_bounded, same_regular_file_identity},
    model::{DatabaseInput, DatabaseRole, ThreadReference},
    reference_graph::{managed_relative_components, path_key},
};

const MAX_STORED_ACCOUNT_CONFIG_BYTES: u64 = 4 * 1024 * 1024;
use crate::{
    codex_paths::{local_codex_paths, resolve_user_codex_paths},
    runtime_session_view::{
        inspect_legacy_session_view_database_homes, inspect_session_view_database_homes,
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseDescriptor {
    pub id: String,
    pub path: PathBuf,
    pub role: DatabaseRole,
    pub rollout_root: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct CatalogSnapshot {
    pub databases: Vec<DatabaseInput>,
    pub referenced_paths: Vec<PathBuf>,
    pub database_errors: usize,
    pub rows_missing_rollout_path: usize,
}

#[derive(Debug, Clone, Default)]
pub struct CatalogDiscovery {
    pub descriptors: Vec<DatabaseDescriptor>,
    pub goals_descriptors: Vec<GoalsDatabaseDescriptor>,
    pub errors: usize,
    pub goals_errors: usize,
}

/// One logical runtime view of a goals database. Multiple views may name the
/// same physical file and are deliberately retained so apply can converge all
/// runnable names after merging the physical inputs exactly once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalsDatabaseView {
    pub path: PathBuf,
    pub role: DatabaseRole,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalsDatabaseDescriptor {
    pub id: String,
    pub source_path: PathBuf,
    pub views: Vec<GoalsDatabaseView>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalsDatabaseSnapshot {
    pub descriptor: GoalsDatabaseDescriptor,
    pub schema_sha256: String,
    pub rows_sha256: String,
    pub row_count: usize,
}

#[derive(Debug, Clone, Default)]
pub struct GoalsCatalogSnapshot {
    pub databases: Vec<GoalsDatabaseSnapshot>,
    pub errors: usize,
}

pub fn discover_database_catalog(codex_home: &Path, data_root: &Path) -> CatalogDiscovery {
    let shared = local_codex_paths(&data_root.join("shared-sessions"));
    let shared_root = shared.codex_home.clone();
    let relay_path = data_root.join("relay-sqlite/state_5.sqlite");
    let mut discovered = BTreeMap::<String, (PathBuf, DatabaseRole, PathBuf, bool)>::new();
    let mut errors = 0_usize;

    match resolve_user_codex_paths(codex_home) {
        Ok(current) => {
            let current_role = if same_path(&current.state_db, &relay_path) {
                DatabaseRole::Relay
            } else {
                DatabaseRole::CanonicalAccount
            };
            let current_state_db = current.state_db;
            add_database(
                &mut discovered,
                current_state_db.clone(),
                current_role,
                codex_home.to_path_buf(),
                true,
            );
            let legacy_local_state_db = codex_home.join("state_5.sqlite");
            if !same_path(&current_state_db, &legacy_local_state_db) {
                add_database(
                    &mut discovered,
                    legacy_local_state_db,
                    DatabaseRole::LegacyOrRelocated,
                    codex_home.to_path_buf(),
                    false,
                );
            }
        }
        Err(_) => errors = errors.saturating_add(1),
    }
    add_database(
        &mut discovered,
        shared.state_db,
        DatabaseRole::Shared,
        shared_root.clone(),
        shared_root.exists(),
    );
    add_database(
        &mut discovered,
        relay_path,
        DatabaseRole::Relay,
        codex_home.to_path_buf(),
        data_root.join("relay-sqlite").exists(),
    );

    match inspect_session_view_database_homes(data_root) {
        Ok(Some((account_sqlite_home, relay_sqlite_home))) => {
            add_database(
                &mut discovered,
                account_sqlite_home.join("state_5.sqlite"),
                DatabaseRole::CanonicalAccount,
                codex_home.to_path_buf(),
                true,
            );
            add_database(
                &mut discovered,
                relay_sqlite_home.join("state_5.sqlite"),
                DatabaseRole::Relay,
                codex_home.to_path_buf(),
                true,
            );
        }
        Ok(None) => {}
        Err(_) => errors = errors.saturating_add(1),
    }
    match inspect_legacy_session_view_database_homes(data_root) {
        Ok(Some((account_sqlite_home, relay_sqlite_home))) => {
            add_database(
                &mut discovered,
                account_sqlite_home.join("state_5.sqlite"),
                DatabaseRole::CanonicalAccount,
                codex_home.to_path_buf(),
                true,
            );
            add_database(
                &mut discovered,
                relay_sqlite_home.join("state_5.sqlite"),
                DatabaseRole::Relay,
                codex_home.to_path_buf(),
                true,
            );
        }
        Ok(None) => {}
        Err(_) => errors = errors.saturating_add(1),
    }
    match read_stored_account_sqlite_home(data_root) {
        Ok(Some(sqlite_home)) => add_database(
            &mut discovered,
            sqlite_home.join("state_5.sqlite"),
            DatabaseRole::AccountView,
            codex_home.to_path_buf(),
            true,
        ),
        Ok(None) => {}
        Err(_) => errors = errors.saturating_add(1),
    }
    let scan_data_root = match fs::symlink_metadata(data_root) {
        Ok(metadata) if metadata.is_dir() && !metadata_is_link_or_reparse(&metadata) => true,
        Ok(_) => {
            errors = errors.saturating_add(1);
            false
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => {
            errors = errors.saturating_add(1);
            false
        }
    };
    if scan_data_root {
        for entry in WalkDir::new(data_root).max_depth(8).follow_links(false) {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    errors = errors.saturating_add(1);
                    continue;
                }
            };
            if !entry.file_type().is_file()
                || !entry
                    .file_name()
                    .to_string_lossy()
                    .eq_ignore_ascii_case("state_5.sqlite")
            {
                continue;
            }
            let path = entry.into_path();
            let role = classify_database(&path, data_root);
            let rollout_root = inferred_rollout_root(&path, role, codex_home, &shared_root);
            add_database(&mut discovered, path.clone(), role, rollout_root, false);
        }
    }

    let mut descriptors = Vec::new();
    for (path, role, rollout_root, required) in discovered.into_values() {
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() && !metadata_is_link_or_reparse(&metadata) => {
                descriptors.push((path, role, rollout_root));
            }
            Ok(_) => errors = errors.saturating_add(1),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !required => {}
            Err(_) => errors = errors.saturating_add(1),
        }
    }
    descriptors.sort_by_key(|(path, role, _)| (*role, path_key(path)));
    let descriptors = descriptors
        .into_iter()
        .enumerate()
        .map(|(index, (path, role, rollout_root))| DatabaseDescriptor {
            id: format!("db-{index:04}"),
            path,
            role,
            rollout_root,
        })
        .collect::<Vec<_>>();
    let (goals_descriptors, goals_errors) = discover_goals_databases(&descriptors);
    CatalogDiscovery {
        descriptors,
        goals_descriptors,
        errors,
        goals_errors,
    }
}

fn discover_goals_databases(
    state_descriptors: &[DatabaseDescriptor],
) -> (Vec<GoalsDatabaseDescriptor>, usize) {
    let mut logical = BTreeMap::<String, GoalsDatabaseView>::new();
    let mut missing_required = 0_usize;
    let mut errors = 0_usize;
    for descriptor in state_descriptors
        .iter()
        .filter(|descriptor| descriptor.role.is_runtime())
    {
        let Some(sqlite_home) = descriptor.path.parent() else {
            errors = errors.saturating_add(1);
            continue;
        };
        let path = sqlite_home.join("goals_1.sqlite");
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() && !metadata_is_link_or_reparse(&metadata) => {}
            Ok(_) => {
                // A state database that may still run makes its sibling goals
                // database required. Offline, reparse, directory, and unreadable
                // states are indistinguishable from an incomplete inventory.
                errors = errors.saturating_add(1);
                continue;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if matches!(
                    descriptor.role,
                    DatabaseRole::CanonicalAccount
                        | DatabaseRole::AccountView
                        | DatabaseRole::Relay
                        | DatabaseRole::UnknownRuntime
                ) {
                    missing_required = missing_required.saturating_add(1);
                }
                continue;
            }
            Err(_) => {
                errors = errors.saturating_add(1);
                continue;
            }
        }
        let key = path_key(&path);
        match logical.get_mut(&key) {
            Some(existing) if role_priority(descriptor.role) < role_priority(existing.role) => {
                existing.role = descriptor.role;
            }
            Some(_) => {}
            None => {
                logical.insert(
                    key,
                    GoalsDatabaseView {
                        path,
                        role: descriptor.role,
                    },
                );
            }
        }
    }

    // A genuinely fresh profile may have no goals database at all. Once any
    // switchable Account/Relay view has one, every other switchable SQLite
    // home must expose its sibling so union and alias convergence cannot
    // silently omit an offline view. Shared and legacy state databases remain
    // part of the reference graph, but older Switch releases legitimately
    // created them without the newer global goals database. Include such a
    // goals database when present; do not invent a missing one during scan.
    if !logical.is_empty() {
        errors = errors.saturating_add(missing_required);
    }

    let mut views = logical.into_values().collect::<Vec<_>>();
    views.sort_by_key(|view| (role_priority(view.role), path_key(&view.path)));
    let mut physical = Vec::<GoalsDatabaseDescriptor>::new();
    for view in views {
        let mut matching = None;
        let mut identity_failed = false;
        for (index, descriptor) in physical.iter().enumerate() {
            match same_regular_file_identity(&view.path, &descriptor.source_path) {
                Ok(true) => {
                    matching = Some(index);
                    break;
                }
                Ok(false) => {}
                Err(()) => {
                    errors = errors.saturating_add(1);
                    identity_failed = true;
                    break;
                }
            }
        }
        if identity_failed {
            continue;
        }
        if let Some(index) = matching {
            physical[index].views.push(view);
        } else {
            physical.push(GoalsDatabaseDescriptor {
                id: String::new(),
                source_path: view.path.clone(),
                views: vec![view],
            });
        }
    }
    physical.sort_by_key(|descriptor| path_key(&descriptor.source_path));
    for (index, descriptor) in physical.iter_mut().enumerate() {
        descriptor.id = format!("goals-db-{index:04}");
        descriptor
            .views
            .sort_by_key(|view| (role_priority(view.role), path_key(&view.path)));
        descriptor.source_path = descriptor.views[0].path.clone();
    }
    (physical, errors)
}

pub fn snapshot_database_catalog(
    descriptors: &[DatabaseDescriptor],
    staging_root: &Path,
) -> CatalogSnapshot {
    let mut snapshot = CatalogSnapshot::default();
    for (index, descriptor) in descriptors.iter().enumerate() {
        match snapshot_database_references(descriptor, staging_root, index) {
            Ok((database, missing_paths)) => {
                snapshot.rows_missing_rollout_path = snapshot
                    .rows_missing_rollout_path
                    .saturating_add(missing_paths);
                snapshot.referenced_paths.extend(
                    database
                        .references
                        .iter()
                        .map(|reference| reference.rollout_path.clone()),
                );
                snapshot.databases.push(database);
            }
            Err(_) => snapshot.database_errors = snapshot.database_errors.saturating_add(1),
        }
    }
    snapshot.referenced_paths.sort_by_key(|path| path_key(path));
    snapshot
        .referenced_paths
        .dedup_by(|left, right| path_key(left) == path_key(right));
    snapshot
}

/// Takes SQLite Online Backup snapshots of every distinct physical goals DB,
/// validates the two tables whose semantics we migrate, and fingerprints their
/// exact schemas and rows. Any per-database failure or cross-database schema
/// difference is fail-closed in `errors`.
pub fn snapshot_goals_database_catalog(
    descriptors: &[GoalsDatabaseDescriptor],
    staging_root: &Path,
) -> GoalsCatalogSnapshot {
    let mut result = GoalsCatalogSnapshot::default();
    let mut expected_schema = None::<String>;
    for (index, descriptor) in descriptors.iter().enumerate() {
        match snapshot_goals_database(descriptor, staging_root, index) {
            Ok(snapshot) => {
                if expected_schema
                    .as_ref()
                    .is_some_and(|expected| expected != &snapshot.schema_sha256)
                {
                    result.errors = result.errors.saturating_add(1);
                    continue;
                }
                expected_schema.get_or_insert_with(|| snapshot.schema_sha256.clone());
                result.databases.push(snapshot);
            }
            Err(_) => result.errors = result.errors.saturating_add(1),
        }
    }
    result
}

fn snapshot_goals_database(
    descriptor: &GoalsDatabaseDescriptor,
    staging_root: &Path,
    index: usize,
) -> Result<GoalsDatabaseSnapshot, String> {
    fs::create_dir_all(staging_root)
        .map_err(|_| "failed to create goals database staging directory".to_string())?;
    let staging_metadata = fs::symlink_metadata(staging_root)
        .map_err(|_| "failed to inspect goals database staging directory".to_string())?;
    if !staging_metadata.is_dir() || metadata_is_link_or_reparse(&staging_metadata) {
        return Err("goals database staging directory is unsafe".to_string());
    }
    let stage = staging_root.join(format!("goals-snapshot-{index:04}.sqlite"));
    remove_sqlite_family(&stage);
    let snapshot_result = (|| {
        let source_metadata = fs::symlink_metadata(&descriptor.source_path)
            .map_err(|_| "goals database is unavailable".to_string())?;
        if !source_metadata.is_file() || metadata_is_link_or_reparse(&source_metadata) {
            return Err("goals database path is unsafe".to_string());
        }
        let source = Connection::open_with_flags(
            &descriptor.source_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|_| "failed to open goals database read-only".to_string())?;
        source
            .backup(MAIN_DB, &stage, None)
            .map_err(|_| "failed to create goals database snapshot".to_string())?;
        drop(source);
        let connection = Connection::open_with_flags(
            &stage,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|_| "failed to open goals database snapshot".to_string())?;
        let quick_check: String = connection
            .query_row("PRAGMA quick_check", [], |row| row.get(0))
            .map_err(|_| "failed to verify goals database snapshot".to_string())?;
        if quick_check != "ok" {
            return Err("goals database snapshot failed quick_check".to_string());
        }
        let (schema_sha256, rows_sha256, row_count) = goals_database_digest(&connection)?;
        Ok(GoalsDatabaseSnapshot {
            descriptor: descriptor.clone(),
            schema_sha256,
            rows_sha256,
            row_count,
        })
    })();
    remove_sqlite_family(&stage);
    snapshot_result
}

#[derive(Debug, Clone)]
struct GoalsColumn {
    cid: i64,
    name: String,
    declared_type: String,
    not_null: bool,
    default_value: Option<String>,
    primary_key_order: i64,
    hidden: i64,
}

pub(crate) fn goals_database_digest(
    connection: &Connection,
) -> Result<(String, String, usize), String> {
    const TABLES: [&str; 2] = ["thread_goals", "thread_goal_continuation_deferrals"];
    let mut schema_hasher = Sha256::new();
    schema_hasher.update(b"codex-switch-goals-schema-v1\0");
    let mut rows_hasher = Sha256::new();
    rows_hasher.update(b"codex-switch-goals-rows-v1\0");
    let mut row_count = 0_usize;
    for table in TABLES {
        let table_sql = connection
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get::<_, String>(0),
            )
            .map_err(|_| format!("goals database is missing required table {table}"))?;
        let trigger_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_schema WHERE type = 'trigger' AND tbl_name = ?1",
                [table],
                |row| row.get(0),
            )
            .map_err(|_| "failed to inspect goals database triggers".to_string())?;
        if trigger_count != 0 {
            return Err(format!("goals table {table} has unsupported triggers"));
        }
        let columns = goals_table_columns(connection, table)?;
        if columns.is_empty()
            || columns.iter().any(|column| column.hidden != 0)
            || !columns.iter().any(|column| column.name == "thread_id")
        {
            return Err(format!("goals table {table} has an unsupported schema"));
        }
        validate_known_goals_schema(table, &columns)?;
        let mut primary_key = columns
            .iter()
            .enumerate()
            .filter(|(_, column)| column.primary_key_order > 0)
            .map(|(index, column)| (column.primary_key_order, index))
            .collect::<Vec<_>>();
        primary_key.sort_unstable();
        if primary_key.is_empty()
            || primary_key
                .iter()
                .enumerate()
                .any(|(index, (order, _))| *order != (index + 1) as i64)
        {
            return Err(format!("goals table {table} has no reliable primary key"));
        }

        digest_field(&mut schema_hasher, table.as_bytes());
        digest_field(&mut schema_hasher, table_sql.as_bytes());
        for column in &columns {
            schema_hasher.update(column.cid.to_le_bytes());
            digest_field(&mut schema_hasher, column.name.as_bytes());
            digest_field(&mut schema_hasher, column.declared_type.as_bytes());
            schema_hasher.update([u8::from(column.not_null)]);
            digest_field(
                &mut schema_hasher,
                column.default_value.as_deref().unwrap_or("").as_bytes(),
            );
            schema_hasher.update(column.primary_key_order.to_le_bytes());
            schema_hasher.update(column.hidden.to_le_bytes());
        }

        let mut rows = BTreeMap::<Vec<u8>, Vec<u8>>::new();
        let select = format!(
            "SELECT {} FROM {}",
            columns
                .iter()
                .map(|column| quote_identifier(&column.name))
                .collect::<Vec<_>>()
                .join(", "),
            quote_identifier(table)
        );
        let mut statement = connection
            .prepare(&select)
            .map_err(|_| format!("failed to prepare goals table {table}"))?;
        let mut query = statement
            .query([])
            .map_err(|_| format!("failed to query goals table {table}"))?;
        while let Some(row) = query
            .next()
            .map_err(|_| format!("failed to read goals table {table}"))?
        {
            let values = (0..columns.len())
                .map(|index| row.get::<_, SqlValue>(index))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| format!("failed to decode goals table {table}"))?;
            let key_values = primary_key
                .iter()
                .map(|(_, index)| values[*index].clone())
                .collect::<Vec<_>>();
            let key = encode_sql_values(&key_values)?;
            let encoded = encode_sql_values(&values)?;
            if rows.insert(key, encoded).is_some() {
                return Err(format!("goals table {table} has duplicate primary keys"));
            }
        }
        digest_field(&mut rows_hasher, table.as_bytes());
        for (key, row) in rows {
            digest_field(&mut rows_hasher, &key);
            digest_field(&mut rows_hasher, &row);
            row_count = row_count
                .checked_add(1)
                .ok_or_else(|| "goals row count overflowed".to_string())?;
        }
    }
    Ok((
        format!("{:x}", schema_hasher.finalize()),
        format!("{:x}", rows_hasher.finalize()),
        row_count,
    ))
}

fn validate_known_goals_schema(table: &str, columns: &[GoalsColumn]) -> Result<(), String> {
    let expected: &[(&str, &str, bool, i64)] = match table {
        "thread_goals" => &[
            ("thread_id", "TEXT", true, 1),
            ("goal_id", "TEXT", true, 0),
            ("objective", "TEXT", true, 0),
            ("status", "TEXT", true, 0),
            ("token_budget", "INTEGER", false, 0),
            ("tokens_used", "INTEGER", true, 0),
            ("time_used_seconds", "INTEGER", true, 0),
            ("created_at_ms", "INTEGER", true, 0),
            ("updated_at_ms", "INTEGER", true, 0),
        ],
        "thread_goal_continuation_deferrals" => &[("thread_id", "TEXT", true, 1)],
        _ => return Err("unknown goals table".to_string()),
    };
    if columns.len() != expected.len()
        || columns.iter().zip(expected).any(
            |(actual, (name, declared_type, not_null, primary_key_order))| {
                actual.name != *name
                    || !actual.declared_type.eq_ignore_ascii_case(declared_type)
                    || actual.not_null != *not_null
                    || actual.primary_key_order != *primary_key_order
            },
        )
    {
        return Err(format!("goals table {table} schema is incompatible"));
    }
    if table == "thread_goals"
        && (columns[5].default_value.as_deref() != Some("0")
            || columns[6].default_value.as_deref() != Some("0"))
    {
        return Err("goals table thread_goals defaults are incompatible".to_string());
    }
    Ok(())
}

fn goals_table_columns(connection: &Connection, table: &str) -> Result<Vec<GoalsColumn>, String> {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_xinfo({})", quote_identifier(table)))
        .map_err(|_| "failed to inspect goals database schema".to_string())?;
    let columns = statement
        .query_map([], |row| {
            Ok(GoalsColumn {
                cid: row.get(0)?,
                name: row.get(1)?,
                declared_type: row.get(2)?,
                not_null: row.get::<_, i64>(3)? != 0,
                default_value: row.get(4)?,
                primary_key_order: row.get(5)?,
                hidden: row.get(6)?,
            })
        })
        .map_err(|_| "failed to query goals database schema".to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "failed to read goals database schema".to_string())?;
    Ok(columns)
}

fn encode_sql_values(values: &[SqlValue]) -> Result<Vec<u8>, String> {
    let mut output = Vec::new();
    for value in values {
        match value {
            SqlValue::Null => output.push(0),
            SqlValue::Integer(value) => {
                output.push(1);
                output.extend_from_slice(&value.to_le_bytes());
            }
            SqlValue::Real(value) => {
                output.push(2);
                output.extend_from_slice(&value.to_bits().to_le_bytes());
            }
            SqlValue::Text(value) => {
                output.push(3);
                append_encoded_length(&mut output, value.len())?;
                output.extend_from_slice(value.as_bytes());
            }
            SqlValue::Blob(value) => {
                output.push(4);
                append_encoded_length(&mut output, value.len())?;
                output.extend_from_slice(value);
            }
        }
    }
    Ok(output)
}

fn append_encoded_length(output: &mut Vec<u8>, length: usize) -> Result<(), String> {
    let length = u64::try_from(length).map_err(|_| "goals value length overflowed".to_string())?;
    output.extend_from_slice(&length.to_le_bytes());
    Ok(())
}

fn digest_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value);
}

fn quote_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn snapshot_database_references(
    descriptor: &DatabaseDescriptor,
    staging_root: &Path,
    index: usize,
) -> Result<(DatabaseInput, usize), String> {
    fs::create_dir_all(staging_root)
        .map_err(|_| "failed to create shadow database staging directory".to_string())?;
    let staging_metadata = fs::symlink_metadata(staging_root)
        .map_err(|_| "failed to inspect shadow database staging directory".to_string())?;
    if !staging_metadata.is_dir() || metadata_is_link_or_reparse(&staging_metadata) {
        return Err("shadow database staging directory is unsafe".to_string());
    }
    let stage = staging_root.join(format!("snapshot-{index:04}.sqlite"));
    remove_sqlite_family(&stage);
    let result = (|| {
        let source_metadata = fs::symlink_metadata(&descriptor.path)
            .map_err(|_| "runtime database is unavailable".to_string())?;
        if !source_metadata.is_file() || metadata_is_link_or_reparse(&source_metadata) {
            return Err("runtime database path is unsafe".to_string());
        }
        let source = Connection::open_with_flags(
            &descriptor.path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|_| "failed to open a runtime database read-only".to_string())?;
        source
            .backup(MAIN_DB, &stage, None)
            .map_err(|_| "failed to create a runtime database snapshot".to_string())?;
        drop(source);
        let snapshot = Connection::open_with_flags(
            &stage,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|_| "failed to open a shadow database snapshot".to_string())?;
        let quick_check: String = snapshot
            .query_row("PRAGMA quick_check", [], |row| row.get(0))
            .map_err(|_| "failed to verify a shadow database snapshot".to_string())?;
        if quick_check != "ok" {
            return Err("shadow database snapshot failed quick_check".to_string());
        }
        let (references, missing_paths) = read_thread_references(
            &snapshot,
            &descriptor.rollout_root,
            !matches!(
                descriptor.role,
                DatabaseRole::LegacyOrRelocated | DatabaseRole::UnknownRuntime
            ),
        )?;
        Ok((
            DatabaseInput {
                id: descriptor.id.clone(),
                path: Some(descriptor.path.clone()),
                role: descriptor.role,
                references,
            },
            missing_paths,
        ))
    })();
    remove_sqlite_family(&stage);
    result
}

fn read_thread_references(
    connection: &Connection,
    rollout_root: &Path,
    relative_paths_authoritative: bool,
) -> Result<(Vec<ThreadReference>, usize), String> {
    let columns = table_columns(connection, "threads")?;
    if !columns.iter().any(|column| column == "id") {
        return Err("runtime database threads schema has no id".to_string());
    }
    let has_rollout_path = columns.iter().any(|column| column == "rollout_path");
    if !has_rollout_path {
        return Err("runtime database threads schema has no rollout_path".to_string());
    }
    let has_provider = columns.iter().any(|column| column == "model_provider");
    let query = if has_provider {
        "SELECT id, rollout_path, model_provider FROM threads"
    } else {
        "SELECT id, rollout_path, NULL FROM threads"
    };
    let mut statement = connection
        .prepare(query)
        .map_err(|_| "failed to prepare runtime thread reference query".to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, SqlValue>(0)?,
                row.get::<_, SqlValue>(1)?,
                row.get::<_, SqlValue>(2)?,
            ))
        })
        .map_err(|_| "failed to query runtime thread references".to_string())?;
    let mut references = Vec::new();
    let mut missing_paths = 0_usize;
    for row in rows {
        let (id, rollout_path, provider) =
            row.map_err(|_| "failed to read a runtime thread reference".to_string())?;
        let Some(id) = strict_text_value(id) else {
            return Err("runtime database thread id is invalid".to_string());
        };
        let Some(rollout_path) = strict_text_value(rollout_path) else {
            missing_paths = missing_paths.saturating_add(1);
            continue;
        };
        let rollout_path =
            resolve_rollout_path(rollout_root, &rollout_path, relative_paths_authoritative)?;
        references.push(ThreadReference {
            thread_id: id,
            rollout_path,
            model_provider: text_value(provider),
        });
    }
    Ok((references, missing_paths))
}

fn table_columns(connection: &Connection, table: &str) -> Result<Vec<String>, String> {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|_| "failed to inspect a runtime database schema".to_string())?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|_| "failed to query a runtime database schema".to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "failed to read a runtime database schema".to_string())?;
    Ok(columns)
}

fn text_value(value: SqlValue) -> Option<String> {
    match value {
        SqlValue::Text(value) if !value.trim().is_empty() => Some(value),
        SqlValue::Integer(value) => Some(value.to_string()),
        SqlValue::Real(value) => Some(value.to_string()),
        _ => None,
    }
}

fn strict_text_value(value: SqlValue) -> Option<String> {
    match value {
        SqlValue::Text(value) if !value.trim().is_empty() => Some(value),
        _ => None,
    }
}

fn resolve_rollout_path(
    root: &Path,
    value: &str,
    relative_paths_authoritative: bool,
) -> Result<PathBuf, String> {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        return Ok(path);
    }
    if !relative_paths_authoritative {
        return Err(
            "runtime database has a relative rollout_path without an authoritative root"
                .to_string(),
        );
    }
    if path.components().any(|component| {
        matches!(
            component,
            Component::CurDir | Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err("runtime database rollout_path is unsafe".to_string());
    }
    Ok(root.join(path))
}

fn read_stored_account_sqlite_home(data_root: &Path) -> Result<Option<PathBuf>, String> {
    let path = data_root.join("runtimes/plus/config.toml");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("stored Account config is unreadable".to_string()),
    };
    if !metadata.is_file()
        || metadata.len() == 0
        || metadata.len() > MAX_STORED_ACCOUNT_CONFIG_BYTES
    {
        return Err("stored Account config is invalid".to_string());
    }
    let encoded = read_regular_file_bounded(&path, MAX_STORED_ACCOUNT_CONFIG_BYTES)
        .map_err(|_| "stored Account config is unreadable".to_string())?;
    let raw =
        String::from_utf8(encoded).map_err(|_| "stored Account config is invalid".to_string())?;
    let document = raw
        .parse::<DocumentMut>()
        .map_err(|_| "stored Account config is invalid".to_string())?;
    let Some(value) = document.get("sqlite_home") else {
        return Ok(None);
    };
    let path = value
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "stored Account SQLite home is invalid".to_string())?;
    if !path.is_absolute() {
        return Err("stored Account SQLite home is invalid".to_string());
    }
    Ok(Some(path))
}

fn classify_database(path: &Path, data_root: &Path) -> DatabaseRole {
    let Some(components) = managed_relative_components(path, data_root) else {
        return DatabaseRole::LegacyOrRelocated;
    };
    if components
        .iter()
        .any(|component| managed_component(component, &["backup", "backups"]))
    {
        DatabaseRole::Backup
    } else if components
        .iter()
        .any(|component| managed_component(component, &["recovery", "restore"]))
    {
        DatabaseRole::RecoveryPackage
    } else if components
        .iter()
        .any(|component| managed_component(component, &["downgrade", "v0.2"]))
    {
        DatabaseRole::DowngradeExport
    } else if components
        .iter()
        .any(|component| component == "shared-sessions")
    {
        DatabaseRole::Shared
    } else if components
        .iter()
        .any(|component| component == "relay-sqlite")
    {
        DatabaseRole::Relay
    } else {
        DatabaseRole::LegacyOrRelocated
    }
}

fn managed_component(component: &str, names: &[&str]) -> bool {
    names.iter().any(|name| {
        component == *name
            || component
                .strip_prefix(name)
                .is_some_and(|suffix| suffix.starts_with(['-', '_']))
    })
}

fn add_database(
    discovered: &mut BTreeMap<String, (PathBuf, DatabaseRole, PathBuf, bool)>,
    path: PathBuf,
    role: DatabaseRole,
    rollout_root: PathBuf,
    required: bool,
) {
    let key = path_key(&path);
    match discovered.get_mut(&key) {
        Some((_, existing_role, existing_root, existing_required)) => {
            if role_priority(role) < role_priority(*existing_role) {
                *existing_role = role;
                *existing_root = rollout_root;
            }
            *existing_required |= required;
        }
        None => {
            discovered.insert(key, (path, role, rollout_root, required));
        }
    }
}

fn inferred_rollout_root(
    database: &Path,
    role: DatabaseRole,
    canonical_root: &Path,
    shared_root: &Path,
) -> PathBuf {
    match role {
        DatabaseRole::Shared => shared_root.to_path_buf(),
        DatabaseRole::CanonicalAccount
        | DatabaseRole::AccountView
        | DatabaseRole::Relay
        | DatabaseRole::LegacyOrRelocated
        | DatabaseRole::UnknownRuntime => canonical_root.to_path_buf(),
        DatabaseRole::Backup | DatabaseRole::RecoveryPackage | DatabaseRole::DowngradeExport => {
            database.parent().unwrap_or(canonical_root).to_path_buf()
        }
    }
}

fn role_priority(role: DatabaseRole) -> u8 {
    match role {
        DatabaseRole::CanonicalAccount => 0,
        DatabaseRole::AccountView => 1,
        DatabaseRole::Relay => 2,
        DatabaseRole::Shared => 3,
        DatabaseRole::LegacyOrRelocated => 4,
        DatabaseRole::UnknownRuntime => 5,
        DatabaseRole::Backup => 6,
        DatabaseRole::RecoveryPackage => 7,
        DatabaseRole::DowngradeExport => 8,
    }
}

fn same_path(left: &Path, right: &Path) -> bool {
    path_key(left) == path_key(right)
}

fn remove_sqlite_family(path: &Path) {
    for candidate in [
        path.to_path_buf(),
        PathBuf::from(format!("{}-wal", path.to_string_lossy())),
        PathBuf::from(format!("{}-shm", path.to_string_lossy())),
    ] {
        let _ = fs::remove_file(candidate);
    }
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
    use std::fs;

    use rusqlite::Connection;
    use tempfile::tempdir;

    use super::{
        classify_database, discover_database_catalog, snapshot_database_catalog,
        snapshot_goals_database_catalog,
    };
    use crate::session_storage::model::DatabaseRole;

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
                 );",
            )
            .unwrap();
    }

    fn create_required_goals_for_state_databases(root: &std::path::Path) {
        for entry in walkdir::WalkDir::new(root).follow_links(false) {
            let Ok(entry) = entry else { continue };
            if entry.file_type().is_file() && entry.file_name() == "state_5.sqlite" {
                let goals = entry.path().with_file_name("goals_1.sqlite");
                if !goals.exists() {
                    create_goals_database(&goals);
                }
            }
        }
    }

    #[test]
    fn snapshots_committed_wal_references_without_copying_wal_files() {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let data = root.path().join("data");
        let staging = root.path().join("staging");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&data).unwrap();
        fs::write(
            home.join("config.toml"),
            format!("sqlite_home = {:?}\n", home.to_string_lossy()),
        )
        .unwrap();
        let connection = Connection::open(home.join("state_5.sqlite")).unwrap();
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, model_provider TEXT);
                 INSERT INTO threads VALUES ('thread-a', 'C:/isolated/session.jsonl', 'openai');",
            )
            .unwrap();

        create_required_goals_for_state_databases(root.path());
        let discovery = discover_database_catalog(&home, &data);
        let snapshot = snapshot_database_catalog(&discovery.descriptors, &staging);

        assert_eq!(discovery.errors, 0);
        assert_eq!(snapshot.database_errors, 0);
        assert_eq!(snapshot.databases.len(), 1);
        assert_eq!(snapshot.databases[0].role, DatabaseRole::CanonicalAccount);
        assert_eq!(snapshot.databases[0].references.len(), 1);
        assert_eq!(snapshot.databases[0].references[0].thread_id, "thread-a");
        assert!(!staging.join("snapshot-0000.sqlite").exists());
    }

    #[test]
    fn malformed_optional_account_view_does_not_hide_other_databases() {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let data = root.path().join("data");
        fs::create_dir_all(data.join("runtimes/plus")).unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::write(
            home.join("config.toml"),
            format!("sqlite_home = {:?}\n", home.to_string_lossy()),
        )
        .unwrap();
        Connection::open(home.join("state_5.sqlite")).unwrap();
        fs::write(data.join("runtimes/plus/config.toml"), "not = [valid").unwrap();

        create_required_goals_for_state_databases(root.path());
        let discovery = discover_database_catalog(&home, &data);

        assert_eq!(discovery.errors, 1);
        assert!(discovery
            .descriptors
            .iter()
            .any(|descriptor| descriptor.role == DatabaseRole::CanonicalAccount));
    }

    #[test]
    fn invalid_session_view_paths_are_reported_instead_of_scanned() {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let data = root.path().join("data");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&data).unwrap();
        fs::write(
            home.join("config.toml"),
            format!("sqlite_home = {:?}\n", home.to_string_lossy()),
        )
        .unwrap();
        Connection::open(home.join("state_5.sqlite")).unwrap();
        fs::write(
            data.join("request-route-session-view-v2.json"),
            serde_json::to_vec(&serde_json::json!({
                "version": 2,
                "accountConfiguredSqliteHome": null,
                "accountEffectiveSqliteHome": home,
                "relaySqliteHome": root.path().join("outside-relay"),
                "lastCommonStateSha256": null,
            }))
            .unwrap(),
        )
        .unwrap();

        create_required_goals_for_state_databases(root.path());
        let discovery = discover_database_catalog(&root.path().join("home"), &data);

        assert_eq!(discovery.errors, 1);
        assert_eq!(discovery.descriptors.len(), 1);
        assert_eq!(
            discovery.descriptors[0].role,
            DatabaseRole::CanonicalAccount
        );
    }

    #[test]
    fn a_relocated_current_database_does_not_hide_the_legacy_local_database() {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let sqlite_home = root.path().join("relocated");
        let data = root.path().join("data");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&sqlite_home).unwrap();
        fs::write(
            home.join("config.toml"),
            format!("sqlite_home = {:?}\n", sqlite_home.to_string_lossy()),
        )
        .unwrap();
        Connection::open(home.join("state_5.sqlite")).unwrap();
        Connection::open(sqlite_home.join("state_5.sqlite")).unwrap();

        create_required_goals_for_state_databases(root.path());
        let discovery = discover_database_catalog(&home, &data);

        assert_eq!(discovery.errors, 0);
        assert_eq!(discovery.descriptors.len(), 2);
        assert!(discovery
            .descriptors
            .iter()
            .any(|database| database.role == DatabaseRole::CanonicalAccount));
        assert!(discovery
            .descriptors
            .iter()
            .any(|database| database.role == DatabaseRole::LegacyOrRelocated));
    }

    #[test]
    fn relative_rollout_paths_are_resolved_from_the_database_codex_home() {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let data = root.path().join("data");
        let staging = root.path().join("staging");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&data).unwrap();
        let connection = Connection::open(home.join("state_5.sqlite")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, model_provider TEXT);
                 INSERT INTO threads VALUES ('thread-relative', 'sessions/2026/thread.jsonl', 'openai');",
            )
            .unwrap();

        create_required_goals_for_state_databases(root.path());
        let discovery = discover_database_catalog(&home, &data);
        let snapshot = snapshot_database_catalog(&discovery.descriptors, &staging);

        assert_eq!(discovery.errors, 0);
        assert_eq!(snapshot.database_errors, 0);
        assert_eq!(
            snapshot.databases[0].references[0].rollout_path,
            home.join("sessions/2026/thread.jsonl")
        );
    }

    #[test]
    fn configured_account_database_that_is_offline_blocks_a_complete_catalog() {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let data = root.path().join("data");
        let offline = root.path().join("offline-account");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(data.join("runtimes/plus")).unwrap();
        let connection = Connection::open(home.join("state_5.sqlite")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, model_provider TEXT);",
            )
            .unwrap();
        fs::write(
            data.join("runtimes/plus/config.toml"),
            format!("sqlite_home = {:?}\n", offline.to_string_lossy()),
        )
        .unwrap();

        create_required_goals_for_state_databases(root.path());
        let discovery = discover_database_catalog(&home, &data);

        assert_eq!(discovery.errors, 1);
        assert_eq!(discovery.descriptors.len(), 1);
    }

    #[test]
    fn relative_stored_account_sqlite_home_blocks_a_complete_catalog() {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let data = root.path().join("data");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(data.join("runtimes/plus")).unwrap();
        Connection::open(home.join("state_5.sqlite")).unwrap();
        fs::write(
            data.join("runtimes/plus/config.toml"),
            "sqlite_home = 'relative-sqlite-home'\n",
        )
        .unwrap();

        create_required_goals_for_state_databases(root.path());
        let discovery = discover_database_catalog(&home, &data);

        assert_eq!(discovery.errors, 1);
        assert_eq!(discovery.descriptors.len(), 1);
    }

    #[test]
    fn an_unsafe_optional_runtime_database_path_is_not_treated_as_absent() {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let data = root.path().join("data");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(data.join("shared-sessions/state_5.sqlite")).unwrap();
        Connection::open(home.join("state_5.sqlite")).unwrap();

        create_required_goals_for_state_databases(root.path());
        let discovery = discover_database_catalog(&home, &data);

        assert_eq!(discovery.errors, 1);
        assert_eq!(discovery.descriptors.len(), 1);
    }

    #[test]
    fn incompatible_runtime_schema_is_a_catalog_error_not_zero_references() {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let data = root.path().join("data");
        let staging = root.path().join("staging");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&data).unwrap();
        let connection = Connection::open(home.join("state_5.sqlite")).unwrap();
        connection
            .execute_batch("CREATE TABLE threads (rollout_path TEXT);")
            .unwrap();

        create_required_goals_for_state_databases(root.path());
        let discovery = discover_database_catalog(&home, &data);
        let snapshot = snapshot_database_catalog(&discovery.descriptors, &staging);

        assert_eq!(discovery.errors, 0);
        assert_eq!(snapshot.database_errors, 1);
        assert!(snapshot.databases.is_empty());
    }

    #[test]
    fn profile_without_any_goals_history_is_legal() {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let data = root.path().join("data");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&data).unwrap();
        Connection::open(home.join("state_5.sqlite")).unwrap();

        let discovery = discover_database_catalog(&home, &data);

        assert_eq!(discovery.errors, 0);
        assert_eq!(discovery.goals_errors, 0);
        assert!(discovery.goals_descriptors.is_empty());
    }

    #[test]
    fn once_goals_exist_every_switchable_state_home_requires_its_sibling() {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let data = root.path().join("data");
        let relay = data.join("relay-sqlite");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&relay).unwrap();
        Connection::open(home.join("state_5.sqlite")).unwrap();
        Connection::open(relay.join("state_5.sqlite")).unwrap();
        create_goals_database(&home.join("goals_1.sqlite"));

        let discovery = discover_database_catalog(&home, &data);

        assert_eq!(discovery.goals_errors, 1);
        assert_eq!(discovery.goals_descriptors.len(), 1);
    }

    #[test]
    fn legacy_shared_state_without_goals_does_not_block_switchable_views() {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let data = root.path().join("data");
        let shared = data.join("shared-sessions");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&shared).unwrap();
        Connection::open(home.join("state_5.sqlite")).unwrap();
        Connection::open(shared.join("state_5.sqlite")).unwrap();
        create_goals_database(&home.join("goals_1.sqlite"));

        let discovery = discover_database_catalog(&home, &data);

        assert_eq!(discovery.goals_errors, 0);
        assert_eq!(discovery.goals_descriptors.len(), 1);
        assert_eq!(
            discovery.goals_descriptors[0].views[0].role,
            DatabaseRole::CanonicalAccount
        );
    }

    #[test]
    fn goals_directory_is_rejected_as_an_unsafe_runtime_database() {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let data = root.path().join("data");
        fs::create_dir_all(home.join("goals_1.sqlite")).unwrap();
        fs::create_dir_all(&data).unwrap();
        Connection::open(home.join("state_5.sqlite")).unwrap();

        let discovery = discover_database_catalog(&home, &data);

        assert_eq!(discovery.goals_errors, 1);
        assert!(discovery.goals_descriptors.is_empty());
    }

    #[test]
    #[cfg(windows)]
    fn goals_reparse_point_is_rejected() {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let data = root.path().join("data");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&data).unwrap();
        Connection::open(home.join("state_5.sqlite")).unwrap();
        let actual = root.path().join("actual-goals-directory");
        let link = home.join("goals_1.sqlite");
        fs::create_dir_all(&actual).unwrap();
        let status = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&link)
            .arg(&actual)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(
            status.success(),
            "failed to create the reparse-point fixture"
        );

        let discovery = discover_database_catalog(&home, &data);

        assert_eq!(discovery.goals_errors, 1);
        assert!(discovery.goals_descriptors.is_empty());
    }

    #[test]
    fn blob_thread_ids_fail_closed_instead_of_becoming_zero_references() {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let data = root.path().join("data");
        let staging = root.path().join("staging");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&data).unwrap();
        let connection = Connection::open(home.join("state_5.sqlite")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE threads (id BLOB PRIMARY KEY, rollout_path TEXT, model_provider TEXT);
                 INSERT INTO threads VALUES (x'0102', 'sessions/thread.jsonl', 'openai');",
            )
            .unwrap();

        create_required_goals_for_state_databases(root.path());
        let discovery = discover_database_catalog(&home, &data);
        let snapshot = snapshot_database_catalog(&discovery.descriptors, &staging);

        assert_eq!(discovery.errors, 0);
        assert_eq!(snapshot.database_errors, 1);
        assert!(snapshot.databases.is_empty());
    }

    #[test]
    fn a_present_relay_root_with_an_offline_database_blocks_discovery() {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let data = root.path().join("data");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(data.join("relay-sqlite")).unwrap();
        Connection::open(home.join("state_5.sqlite")).unwrap();

        create_required_goals_for_state_databases(root.path());
        let discovery = discover_database_catalog(&home, &data);

        assert_eq!(discovery.errors, 1);
        assert_eq!(discovery.descriptors.len(), 1);
    }

    #[test]
    fn a_recursive_legacy_database_with_a_relative_rollout_path_fails_closed() {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let data = root.path().join("data");
        let staging = root.path().join("staging");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(data.join("legacy-runtime")).unwrap();
        let canonical = Connection::open(home.join("state_5.sqlite")).unwrap();
        canonical
            .execute_batch(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, model_provider TEXT);",
            )
            .unwrap();
        let connection = Connection::open(data.join("legacy-runtime/state_5.sqlite")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, model_provider TEXT);
                 INSERT INTO threads VALUES ('legacy-relative', 'sessions/thread.jsonl', 'openai');",
            )
            .unwrap();

        create_required_goals_for_state_databases(root.path());
        let discovery = discover_database_catalog(&home, &data);
        let snapshot = snapshot_database_catalog(&discovery.descriptors, &staging);

        assert!(discovery
            .descriptors
            .iter()
            .any(|descriptor| descriptor.role == DatabaseRole::LegacyOrRelocated));
        assert_eq!(snapshot.database_errors, 1);
    }

    #[test]
    fn hardlinked_goals_views_are_snapshotted_once_but_both_names_are_retained() {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let relocated = root.path().join("relocated");
        let data = root.path().join("data");
        let staging = root.path().join("staging");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&relocated).unwrap();
        fs::create_dir_all(&data).unwrap();
        fs::write(
            home.join("config.toml"),
            format!("sqlite_home = {:?}\n", relocated.to_string_lossy()),
        )
        .unwrap();
        Connection::open(home.join("state_5.sqlite")).unwrap();
        Connection::open(relocated.join("state_5.sqlite")).unwrap();
        create_goals_database(&home.join("goals_1.sqlite"));
        fs::hard_link(
            home.join("goals_1.sqlite"),
            relocated.join("goals_1.sqlite"),
        )
        .unwrap();

        let discovery = discover_database_catalog(&home, &data);
        let snapshot = snapshot_goals_database_catalog(&discovery.goals_descriptors, &staging);

        assert_eq!(discovery.goals_errors, 0);
        assert_eq!(discovery.goals_descriptors.len(), 1);
        assert_eq!(discovery.goals_descriptors[0].views.len(), 2);
        assert_eq!(snapshot.errors, 0);
        assert_eq!(snapshot.databases.len(), 1);
    }

    #[test]
    fn goals_snapshot_uses_online_backup_and_captures_committed_wal_rows() {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let data = root.path().join("data");
        let staging = root.path().join("staging");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&data).unwrap();
        Connection::open(home.join("state_5.sqlite")).unwrap();
        create_goals_database(&home.join("goals_1.sqlite"));
        let connection = Connection::open(home.join("goals_1.sqlite")).unwrap();
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 INSERT INTO thread_goals
                 VALUES ('thread-a','goal-a','objective','active',NULL,1,2,3,4);
                 INSERT INTO thread_goal_continuation_deferrals VALUES ('thread-a');",
            )
            .unwrap();

        let discovery = discover_database_catalog(&home, &data);
        let snapshot = snapshot_goals_database_catalog(&discovery.goals_descriptors, &staging);

        assert_eq!(snapshot.errors, 0);
        assert_eq!(snapshot.databases[0].row_count, 2);
    }

    #[test]
    fn generated_or_missing_goals_schema_fails_closed() {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let data = root.path().join("data");
        let staging = root.path().join("staging");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&data).unwrap();
        Connection::open(home.join("state_5.sqlite")).unwrap();
        let goals = Connection::open(home.join("goals_1.sqlite")).unwrap();
        goals
            .execute_batch(
                "CREATE TABLE thread_goals (
                    thread_id TEXT PRIMARY KEY NOT NULL,
                    goal_id TEXT GENERATED ALWAYS AS (thread_id) STORED
                 );
                 CREATE TABLE thread_goal_continuation_deferrals (thread_id TEXT);",
            )
            .unwrap();

        let discovery = discover_database_catalog(&home, &data);
        let snapshot = snapshot_goals_database_catalog(&discovery.goals_descriptors, &staging);

        assert_eq!(snapshot.errors, 1);
        assert!(snapshot.databases.is_empty());
    }

    #[test]
    fn classification_uses_managed_relative_components_not_parent_names() {
        let root = tempdir().unwrap();
        let data = root.path().join("user-backup-name/codex-switch");

        assert_eq!(
            classify_database(&data.join("relay-sqlite/state_5.sqlite"), &data),
            DatabaseRole::Relay
        );
        assert_eq!(
            classify_database(&data.join("backups/item/state_5.sqlite"), &data),
            DatabaseRole::Backup
        );
        assert_eq!(
            classify_database(&data.join("legacy/state_5.sqlite"), &data),
            DatabaseRole::LegacyOrRelocated
        );
        assert_eq!(
            classify_database(
                &root
                    .path()
                    .join("backups-outside/codex-switch/state_5.sqlite"),
                &data,
            ),
            DatabaseRole::LegacyOrRelocated
        );
    }
}

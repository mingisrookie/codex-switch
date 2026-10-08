#![cfg(target_os = "macos")]
use std::{fs, os::unix::fs::PermissionsExt};

use codex_switch_lib::{
    file_ops::atomic_write,
    runtime_compatibility::{
        inspect_runtime_compatibility, require_advanced_storage_compatible, CapabilityAvailability,
    },
};
use rusqlite::Connection;

#[test]
fn macos_route_credentials_are_private_after_atomic_replacement() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config.toml");
    fs::write(&path, b"previous").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    atomic_write(&path, b"model_provider = 'fixture'").unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(fs::read(&path).unwrap(), b"model_provider = 'fixture'");
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
}

#[test]
fn macos_existing_database_allows_route_views_but_blocks_advanced_storage() {
    let root = tempfile::tempdir().unwrap();
    let conn = Connection::open(root.path().join("state_5.sqlite")).unwrap();
    conn.execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, model_provider TEXT NOT NULL, rollout_path TEXT NOT NULL);").unwrap();
    drop(conn);
    let report = inspect_runtime_compatibility(root.path());
    assert_eq!(report.session_view, CapabilityAvailability::Supported);
    assert_eq!(report.advanced_storage, CapabilityAvailability::Blocked);
    assert!(require_advanced_storage_compatible(root.path()).is_err());
}

#[test]
fn macos_fresh_home_inspection_does_not_create_user_state() {
    let root = tempfile::tempdir().unwrap();
    let report = inspect_runtime_compatibility(root.path());
    assert_eq!(report.route_config, CapabilityAvailability::Supported);
    assert_eq!(report.advanced_storage, CapabilityAvailability::Blocked);
    assert!(!root.path().join("auth.json").exists());
    assert!(!root.path().join("config.toml").exists());
    assert!(!root.path().join("state_5.sqlite").exists());
}

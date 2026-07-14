pub mod backup;
pub mod codex_home;
pub mod codex_paths;
mod commands;
pub mod config_patch;
pub mod crypto;
pub mod file_ops;
pub mod operation_log;
pub mod process_control;
pub mod profile_store;
pub mod redaction;
pub mod relay_verify;
pub mod runtime_store;
pub mod runtime_switcher;
pub mod session_manager;
pub mod session_scan;
pub mod session_sync;
pub mod skill_manager;
pub mod switcher;
pub mod update_check;

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            commands::get_app_status,
            commands::check_for_updates,
            commands::open_update_page,
            commands::scan_codex_home,
            commands::scan_sessions,
            commands::scan_managed_sessions,
            commands::dry_run_all_sessions,
            commands::list_runtimes,
            commands::scan_runtime_status,
            commands::import_plus_runtime,
            commands::upsert_relay_runtime,
            commands::test_relay_connection,
            commands::list_codex_processes,
            commands::close_codex_processes,
            commands::switch_runtime,
            commands::sync_all_sessions,
            commands::delete_managed_sessions,
            commands::restore_sessions_visible,
            commands::list_backups,
            commands::restore_backup,
            commands::list_operation_records,
            commands::list_skills,
            commands::install_skill,
            commands::save_skill_config
        ])
        .run(tauri::generate_context!())
        .expect("failed to run Codex Switch");
}

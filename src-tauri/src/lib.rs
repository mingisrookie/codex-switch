pub mod backup;
pub mod codex_home;
pub mod config_patch;
pub mod crypto;
mod commands;
pub mod process_control;
pub mod profile_store;
pub mod redaction;
pub mod runtime_store;
pub mod runtime_switcher;
pub mod session_scan;
pub mod session_sync;
pub mod switcher;

pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            commands::get_app_status,
            commands::scan_codex_home,
            commands::scan_sessions,
            commands::dry_run_sync,
            commands::create_backup,
            commands::list_profiles,
            commands::import_current_profile,
            commands::create_api_profile,
            commands::list_runtimes,
            commands::import_plus_runtime,
            commands::upsert_relay_runtime,
            commands::list_codex_processes,
            commands::close_codex_processes,
            commands::switch_profile,
            commands::switch_runtime,
            commands::sync_sessions_from_paths,
            commands::sync_all_sessions
        ])
        .run(tauri::generate_context!())
        .expect("failed to run Codex Switch");
}

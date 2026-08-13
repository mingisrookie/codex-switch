pub mod backup;
mod chat_process_state;
pub mod codex_home;
pub mod codex_paths;
mod commands;
#[cfg(feature = "runtime-evidence")]
pub use commands::{
    run_automatic_gc_safe_window_evidence_at, AutomaticGcSafeWindowEvidence,
    AutomaticGcSafeWindowObservation,
};
pub mod config_patch;
pub mod crypto;
mod diagnostic_commands;
pub mod diagnostics;
pub mod file_ops;
pub mod mobile_continuity;
pub mod operation_log;
pub mod process_control;
mod request_route_switcher;
mod runtime_session_view;
pub mod runtime_store;
pub mod runtime_switcher;
#[allow(dead_code)]
pub mod session_incremental;
pub mod session_manager;
pub mod session_scan;
pub mod session_storage;
pub mod session_sync;
pub mod skill_manager;
pub mod update_check;
pub mod update_install;

pub fn run() {
    let app = tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            commands::get_app_status,
            commands::request_app_exit,
            commands::check_for_updates,
            commands::install_update,
            commands::get_update_startup_notice,
            commands::scan_codex_home,
            commands::scan_sessions,
            commands::scan_managed_sessions,
            commands::get_session_storage_status,
            commands::get_session_storage_control_state,
            commands::create_session_storage_investigation_task,
            commands::open_session_storage_investigation_task,
            commands::set_session_storage_automatic_cleanup,
            commands::scan_session_storage,
            commands::list_session_storage_conflicts,
            commands::resolve_session_storage_conflict,
            commands::preflight_session_storage_migration,
            commands::create_session_storage_migration_backup,
            commands::verify_session_storage_migration_backup,
            commands::prepare_session_storage_migration,
            commands::cancel_session_storage_migration,
            commands::apply_session_storage_migration,
            commands::run_session_storage_offline_gc,
            commands::export_session_storage_downgrade,
            commands::import_session_storage_downgrade,
            commands::reconcile_session_storage_legacy_backups,
            commands::list_session_storage_pending_recovery,
            commands::defer_session_storage_pending_recovery,
            commands::restore_session_storage_pending_recovery,
            commands::list_runtimes,
            commands::get_mobile_continuity_status,
            commands::set_mobile_continuity_enabled,
            commands::acknowledge_mobile_continuity_notice,
            commands::publish_mobile_continuity_session,
            commands::scan_runtime_status,
            commands::import_plus_runtime,
            commands::upsert_relay_runtime,
            commands::list_codex_processes,
            commands::close_codex_processes,
            commands::launch_chatgpt,
            commands::switch_runtime,
            commands::merge_and_repair_sessions,
            commands::restore_sessions_visible,
            commands::list_backups,
            commands::inspect_checkpoint_storage,
            commands::cleanup_automatic_checkpoints,
            commands::create_full_backup,
            commands::delete_backup,
            commands::restore_backup,
            commands::list_operation_records,
            commands::list_skills,
            commands::install_skill,
            commands::save_skill_config,
            diagnostic_commands::get_diagnostic_status,
            diagnostic_commands::export_diagnostics,
            diagnostic_commands::export_diagnostics_to_diagnostic_directory,
            diagnostic_commands::open_diagnostic_export,
            diagnostic_commands::open_diagnostic_log_directory,
            diagnostic_commands::clear_diagnostic_logs,
            diagnostic_commands::record_frontend_diagnostic
        ])
        .build(tauri::generate_context!())
        .expect("failed to build ChatGPT Switch");
    app.run(|app_handle, event| {
        let lifecycle = diagnostics::global_runtime().map(|runtime| runtime.lifecycle());
        match event {
            tauri::RunEvent::Ready => {
                if let Some(lifecycle) = lifecycle {
                    let _ = lifecycle.mark_ready();
                }
                if update_install::acknowledge_update_startup().is_err() {
                    if let Some(lifecycle) = lifecycle {
                        let _ = lifecycle.record_startup_failure(
                            "update.startup_ack_failed",
                            "the updated application could not acknowledge startup",
                        );
                    }
                    app_handle.exit(1);
                } else {
                    commands::schedule_session_storage_startup_recovery();
                }
            }
            tauri::RunEvent::ExitRequested { code, api, .. } => {
                let prevented = commands::mutation_blocks_shutdown();
                if let Some(lifecycle) = lifecycle {
                    let reason = if code.is_some() {
                        "programmaticExit"
                    } else {
                        "userExit"
                    };
                    let _ = lifecycle.record_exit_requested(reason, prevented);
                }
                if prevented {
                    api.prevent_exit();
                }
            }
            tauri::RunEvent::WindowEvent {
                event: tauri::WindowEvent::CloseRequested { api, .. },
                ..
            } => {
                let prevented = commands::mutation_blocks_shutdown();
                if let Some(lifecycle) = lifecycle {
                    let _ = lifecycle.record_exit_requested("windowClose", prevented);
                }
                if prevented {
                    api.prevent_close();
                }
            }
            tauri::RunEvent::Exit => {
                if let Some(lifecycle) = lifecycle {
                    let _ = lifecycle.end_session("runEventExit", None);
                }
            }
            _ => {}
        }
    });
}

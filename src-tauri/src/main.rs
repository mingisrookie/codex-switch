#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

fn main() {
    let diagnostics = codex_switch_lib::diagnostics::initialize_global_from_environment();
    if let Some(runtime) = diagnostics {
        codex_switch_lib::diagnostics::install_panic_hook(runtime.recorder());
    }
    if let Some(exit_code) = codex_switch_lib::update_install::process_startup_update_args() {
        if let Some(runtime) = diagnostics {
            let _ = runtime
                .lifecycle()
                .end_session("updateStartupHelper", Some(exit_code));
        }
        std::process::exit(exit_code);
    }
    codex_switch_lib::run();
}

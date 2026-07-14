#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

fn main() {
    if let Some(exit_code) = codex_switch_lib::update_install::process_startup_update_args() {
        std::process::exit(exit_code);
    }
    codex_switch_lib::run();
}

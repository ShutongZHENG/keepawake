pub mod assertion;
pub mod cmd;
pub mod display;
pub mod power;
pub mod session;
pub mod setup;
pub mod state;
pub mod tray;
pub mod wifi;

pub use state::{AppStatus, AppState, tick_loop};

use cmd::SystemRunner;
use tauri::Manager;

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    tauri::Builder::default()
        .manage(AppState::new())
        .setup(|app| {
            // Tray menu + `status_update` subscription (tray.rs); also
            // registers the hidden `act_setup` warning as managed state.
            tray::build_tray(app)?;

            // Hold PreventUserIdleSystemSleep for the whole app lifetime:
            // the guard is managed state, dropped only when the App drops
            // at the end of `run()`. A failure degrades the spec — the
            // app keeps running (Task 8 Ruling 1).
            match assertion::assert_prevent_system_sleep() {
                Ok(guard) => {
                    app.manage(guard);
                }
                Err(err) => {
                    eprintln!("keepawake: prevent system sleep assertion: {err}");
                }
            }

            // KeepAlive LaunchAgent: best-effort install; a failure (e.g.
            // LaunchAgents dir missing/unwritable) must not stop the app.
            if let Err(err) = setup::install_launch_agent(&SystemRunner) {
                eprintln!("keepawake: install launch agent: {err}");
            }

            // First-run warning: reveal `act_setup` while system sleep is
            // still enabled (or `pmset -g` cannot be read) so the user can
            // run the admin pmset prompts from the tray.
            let needs_setup = match power::current_settings(&SystemRunner) {
                Ok(settings) => !settings.sleep_disabled,
                Err(err) => {
                    eprintln!("keepawake: pmset -g at startup: {err}");
                    true
                }
            };
            if needs_setup {
                if let Err(err) = app.state::<tray::SetupWarning>().show() {
                    eprintln!("keepawake: show setup warning: {err}");
                }
            }

            // 5 s scheduler: gathers power/wifi/session/display state and
            // emits `status_update` for the tray.
            tauri::async_runtime::spawn(tick_loop(app.handle().clone()));
            Ok(())
        })
        .run(tauri::generate_context!())?;
    Ok(())
}

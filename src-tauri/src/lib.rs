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

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    tauri::Builder::default()
        .manage(AppState::new())
        .setup(|app| {
            // Tray menu + `status_update` subscription (tray.rs).
            tray::build_tray(app)?;
            // 5 s scheduler: gathers power/wifi/session/display state and
            // emits `status_update` for the tray.
            tauri::async_runtime::spawn(tick_loop(app.handle().clone()));
            Ok(())
        })
        .run(tauri::generate_context!())?;
    Ok(())
}

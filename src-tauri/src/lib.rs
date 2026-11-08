use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;

pub mod assertion;
pub mod cmd;
pub mod display;
pub mod power;
pub mod session;
pub mod state;
pub mod wifi;

pub use state::{AppStatus, AppState, tick_loop};

const TRAY_ICON: tauri::image::Image<'static> = tauri::include_image!("icons/icon.png");

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    tauri::Builder::default()
        .manage(AppState::new())
        .setup(|app| {
            let status = MenuItem::with_id(app, "status", "KeepAwake (未配置)", false, None::<&str>)?;
            let menu = Menu::with_items(app, &[&status])?;
            TrayIconBuilder::with_id("main-tray")
                .icon(TRAY_ICON)
                .menu(&menu)
                .tooltip("KeepAwake")
                .build(app)?;
            // 5 s scheduler: gathers power/wifi/session/display state and
            // emits `status_update` for the tray.
            tauri::async_runtime::spawn(tick_loop(app.handle().clone()));
            Ok(())
        })
        .run(tauri::generate_context!())?;
    Ok(())
}

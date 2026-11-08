use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;

pub mod assertion;
pub mod cmd;
pub mod display;
pub mod power;

/// Shared application state (populated by later tasks).
pub struct AppState {}

const TRAY_ICON: tauri::image::Image<'static> = tauri::include_image!("icons/icon.png");

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    tauri::Builder::default()
        .manage(AppState {})
        .setup(|app| {
            let status = MenuItem::with_id(app, "status", "KeepAwake (未配置)", false, None::<&str>)?;
            let menu = Menu::with_items(app, &[&status])?;
            TrayIconBuilder::with_id("main-tray")
                .icon(TRAY_ICON)
                .menu(&menu)
                .tooltip("KeepAwake")
                .build(app)?;
            Ok(())
        })
        .run(tauri::generate_context!())?;
    Ok(())
}

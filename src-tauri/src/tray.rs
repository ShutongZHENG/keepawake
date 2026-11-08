//! Tray icon: live status menu, manual actions, first-run setup warning,
//! agent-safe quit.
//!
//! The four `st_*` items are disabled status lines seeded from the current
//! [`AppStatus`] at build time; [`build_tray`] re-sets their texts on every
//! `status_update` event. The `act_*` items mutate [`AppState`] only — the
//! tick loop consumes those mutations on its next pass.
//!
//! `act_setup` is created by [`build_tray`] but held out of the menu until
//! the startup checks say pmset attention is needed: muda menu items have
//! no visibility flag, so [`SetupWarning`] shows/hides it by inserting it
//! before the separator / removing it again.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use tauri::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tauri::tray::{TrayIcon, TrayIconBuilder};
use tauri::{App, AppHandle, Listener, Manager};

use crate::cmd::SystemRunner;
use crate::setup;
use crate::state::{AppStatus, AppState};

const TRAY_ICON: tauri::image::Image<'static> = tauri::include_image!("icons/icon.png");

/// Manual-wake window length set by `act_wake5` (5 minutes).
const MANUAL_WAKE: Duration = Duration::from_secs(5 * 60);

/// Label of the first-run `act_setup` warning row.
const SETUP_TEXT: &str = "⚠️ 需要设置 (点击运行 pmset)";

/// `防睡眠: ✅/❌` from `sleep_ok`.
pub(crate) fn sleep_text(status: &AppStatus) -> String {
    format!("防睡眠: {}", if status.sleep_ok { "✅" } else { "❌" })
}

/// `Wi-Fi: <ssid>` when connected (else `Wi-Fi: 未连接`), with `·重连中`
/// appended while the reconnect planner is working.
pub(crate) fn wifi_text(status: &AppStatus) -> String {
    let base = if status.wifi_connected {
        status.wifi_ssid.as_deref().unwrap_or("未连接")
    } else {
        "未连接"
    };
    let mut text = format!("Wi-Fi: {base}");
    if status.reconnecting {
        text.push_str("·重连中");
    }
    text
}

/// `远程会话: 在线/离线`, with `UU远程: 未检测到` appended only while the
/// UURemoteServer process is absent (`urnot_detected`).
pub(crate) fn session_text(status: &AppStatus) -> String {
    let mut text = format!(
        "远程会话: {}",
        if status.session_active { "在线" } else { "离线" }
    );
    if status.urnot_detected {
        text.push_str(" · UU远程: 未检测到");
    }
    text
}

/// `屏幕: 亮/黑` from `screen_awake`.
pub(crate) fn screen_text(status: &AppStatus) -> String {
    format!("屏幕: {}", if status.screen_awake { "亮" } else { "黑" })
}

/// Handle to the first-run `act_setup` warning row.
///
/// Tauri/muda menu items expose no visibility flag, so "visible" means
/// "inserted into the menu" and "hidden" means "removed from it". The
/// item is created by [`build_tray`] (initially **not** in the menu) and
/// registered as managed state so both `lib.rs` startup and the menu
/// click handler can flip it.
pub struct SetupWarning {
    item: MenuItem<tauri::Wry>,
    menu: Menu<tauri::Wry>,
}

impl SetupWarning {
    /// Insert `act_setup` immediately before the separator — i.e. right
    /// after `act_reconnect`, whose position [`build_tray`] fixes — and
    /// refresh its label. No-op when already visible.
    pub fn show(&self) -> tauri::Result<()> {
        if self.menu.get("act_setup").is_some() {
            return Ok(());
        }
        let items = self.menu.items()?;
        // `act_reconnect` + 1 == the slot before the separator; the
        // fallback (before the last row) only ever fires if the menu
        // layout changed under us.
        let position = items
            .iter()
            .position(|i| i.id().as_ref() == "act_reconnect")
            .map_or(items.len().saturating_sub(1), |idx| idx + 1);
        self.menu.insert(&self.item, position)?;
        self.item.set_text(SETUP_TEXT)?;
        Ok(())
    }

    /// Remove `act_setup` from the menu after a successful setup run.
    /// No-op when already hidden.
    pub fn hide(&self) -> tauri::Result<()> {
        if self.menu.get("act_setup").is_some() {
            self.menu.remove(&self.item)?;
        }
        Ok(())
    }
}

/// Menu clicks. Status items are disabled and never fire; actions only
/// mutate [`AppState`] (the tick loop consumes the mutations).
fn on_menu_event(app: &AppHandle, event: MenuEvent) {
    match event.id().as_ref() {
        "act_setup" => {
            // Two admin prompts (each `osascript … with administrator
            // privileges`); `setup::request_pmset`'s allow-list is the
            // only thing that can reach the shell. Short-circuit: a
            // cancelled first dialog must not open a second one.
            let ran = setup::request_pmset(&SystemRunner, &["-a", "disablesleep", "1"])
                .and_then(|_| setup::request_pmset(&SystemRunner, &["-a", "autorestart", "1"]));
            match ran {
                Ok(_) => {
                    // Optimistic update — the next tick re-reads `pmset -g`
                    // and emits the truth anyway.
                    {
                        let state = app.state::<AppState>();
                        let mut status = state
                            .status
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        status.sleep_ok = true;
                        status.autorestart_ok = true;
                    }
                    match app.try_state::<SetupWarning>() {
                        Some(warning) => {
                            if let Err(err) = warning.hide() {
                                eprintln!("keepawake: hide setup warning: {err}");
                            }
                        }
                        None => eprintln!("keepawake: setup warning state missing; left visible"),
                    }
                }
                Err(err) => {
                    // User cancelled the dialog or pmset failed: keep the
                    // warning visible so they can retry.
                    eprintln!("keepawake: pmset setup failed: {err}");
                }
            }
        }
        "act_wake5" => {
            let state = app.state::<AppState>();
            let mut deadline = state
                .manual_wake_until
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *deadline = Some(Instant::now() + MANUAL_WAKE);
        }
        "act_reconnect" => {
            let state = app.state::<AppState>();
            state.force_round.store(true, Ordering::SeqCst);
        }
        "act_quit" => {
            // Unload the LaunchAgent first — its KeepAlive would relaunch
            // the app forever. Any failure (agent not installed yet) is
            // ignored: quitting must always work (Ruling 2).
            if let Err(err) = setup::unload_launch_agent(&SystemRunner) {
                eprintln!("keepawake: unload launch agent on quit: {err}");
            }
            app.exit(0);
        }
        _ => {}
    }
}

/// Build the tray icon with its menu: four disabled status items (seeded
/// from the current [`AppState`] status), the two manual actions, a
/// separator, and quit. Also creates the hidden `act_setup` warning and
/// registers it as managed [`SetupWarning`] state (shown by `lib.rs`
/// startup when pmset needs attention). Subscribes to `status_update` to
/// re-set the four status texts on every event (live within the 5 s tick
/// cadence).
pub fn build_tray(app: &App) -> tauri::Result<TrayIcon> {
    let status = {
        let state = app.state::<AppState>();
        let current = state
            .status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        current.clone()
    };

    let st_sleep = MenuItem::with_id(app, "st_sleep", sleep_text(&status), false, None::<&str>)?;
    let st_wifi = MenuItem::with_id(app, "st_wifi", wifi_text(&status), false, None::<&str>)?;
    let st_session =
        MenuItem::with_id(app, "st_session", session_text(&status), false, None::<&str>)?;
    let st_screen = MenuItem::with_id(app, "st_screen", screen_text(&status), false, None::<&str>)?;
    let act_wake5 = MenuItem::with_id(app, "act_wake5", "保持清醒 5 分钟", true, None::<&str>)?;
    let act_reconnect = MenuItem::with_id(app, "act_reconnect", "立即重连", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let act_quit = MenuItem::with_id(app, "act_quit", "退出", true, None::<&str>)?;

    let menu = Menu::with_items(
        app,
        &[
            &st_sleep,
            &st_wifi,
            &st_session,
            &st_screen,
            &act_wake5,
            &act_reconnect,
            &separator,
            &act_quit,
        ],
    )?;

    // First-run warning: created hidden (not in the menu yet) and
    // managed as state; `lib.rs` shows it when pmset needs attention,
    // the `act_setup` click hides it after a successful run.
    let act_setup = MenuItem::with_id(app, "act_setup", SETUP_TEXT, true, None::<&str>)?;
    app.manage(SetupWarning {
        item: act_setup,
        menu: menu.clone(),
    });

    // Live status: re-set the four status item texts on each event.
    let live = [st_sleep, st_wifi, st_session, st_screen];
    app.listen("status_update", move |event| {
        let status: AppStatus = match serde_json::from_str(event.payload()) {
            Ok(status) => status,
            Err(err) => {
                eprintln!("keepawake: status_update payload is not AppStatus: {err}");
                return;
            }
        };
        let texts = [
            sleep_text(&status),
            wifi_text(&status),
            session_text(&status),
            screen_text(&status),
        ];
        for (item, text) in live.iter().zip(texts) {
            if let Err(err) = item.set_text(text) {
                eprintln!("keepawake: tray status set_text: {err}");
            }
        }
    });

    TrayIconBuilder::with_id("main-tray")
        .icon(TRAY_ICON)
        .menu(&menu)
        .tooltip("KeepAwake")
        .on_menu_event(on_menu_event)
        .build(app)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Status text formats are pinned by plan Task 7 (Ruling 4).
    #[test]
    fn sleep_text_shows_ok_cross() {
        let ok = AppStatus {
            sleep_ok: true,
            ..AppStatus::default()
        };
        let bad = AppStatus {
            sleep_ok: false,
            ..AppStatus::default()
        };
        assert_eq!(sleep_text(&ok), "防睡眠: ✅");
        assert_eq!(sleep_text(&bad), "防睡眠: ❌");
    }

    #[test]
    fn wifi_text_ssid_or_offline_plus_reconnect_suffix() {
        let idle = AppStatus::default();
        assert_eq!(wifi_text(&idle), "Wi-Fi: 未连接");

        let connected = AppStatus {
            wifi_connected: true,
            wifi_ssid: Some("HomeNet".to_string()),
            ..AppStatus::default()
        };
        assert_eq!(wifi_text(&connected), "Wi-Fi: HomeNet");

        let reconnecting = AppStatus {
            reconnecting: true,
            ..AppStatus::default()
        };
        assert_eq!(wifi_text(&reconnecting), "Wi-Fi: 未连接·重连中");

        let both = AppStatus {
            wifi_connected: true,
            wifi_ssid: Some("HomeNet".to_string()),
            reconnecting: true,
            ..AppStatus::default()
        };
        assert_eq!(wifi_text(&both), "Wi-Fi: HomeNet·重连中");
    }

    #[test]
    fn session_text_online_offline_and_urnot_note() {
        let idle = AppStatus::default();
        assert_eq!(session_text(&idle), "远程会话: 离线");

        let active = AppStatus {
            session_active: true,
            ..AppStatus::default()
        };
        assert_eq!(session_text(&active), "远程会话: 在线");

        // UU远程 note appears only while the UURemoteServer is absent.
        let urnot = AppStatus {
            urnot_detected: true,
            ..AppStatus::default()
        };
        assert_eq!(session_text(&urnot), "远程会话: 离线 · UU远程: 未检测到");
        assert!(!session_text(&active).contains("UU远程"));
    }

    #[test]
    fn screen_text_bright_or_black() {
        let dark = AppStatus::default();
        let bright = AppStatus {
            screen_awake: true,
            ..AppStatus::default()
        };
        assert_eq!(screen_text(&dark), "屏幕: 黑");
        assert_eq!(screen_text(&bright), "屏幕: 亮");
    }
}

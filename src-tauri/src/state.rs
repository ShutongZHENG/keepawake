//! Shared [`AppState`] and the pure per-tick merge, [`combine`].
//!
//! The 5 s scheduler ([`tick_loop`]) gathers real system inputs, ticks the
//! owned [`DisplayController`](crate::display::DisplayController) immediately
//! before calling [`combine`], merges everything into an [`AppStatus`],
//! executes the resulting [`ScreenCmd`]s, and emits `status_update` for the
//! tray. The merge itself is pure (Ruling 1).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager};

use crate::cmd::{CommandRunner, SystemRunner};
use crate::display::{DisplayController, ScreenCmd};
use crate::power::{current_settings, PowerSettings};
use crate::session::{sample_now, SessionDetector};
use crate::wifi::{
    current_ssid, parse_preferred_networks, parse_wifi_device, try_networks, ReconnectPlanner,
    RoundDecision,
};

/// Tick cadence of the scheduler (sleep after each pass).
const TICK: Duration = Duration::from_secs(5);
/// Consecutive `nettop` misses before the UURemoteServer pid is re-discovered
/// via `pgrep` (the process may have restarted).
const PID_MISS_LIMIT: u32 = 3;

/// Tray-facing snapshot of every subsystem, emitted as `status_update`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppStatus {
    /// `SleepDisabled` from `pmset -g`: system sleep is held off.
    pub sleep_ok: bool,
    /// `autorestart` from `pmset -g`: restart after a power failure.
    pub autorestart_ok: bool,
    /// Wi-Fi is associated with a network.
    pub wifi_connected: bool,
    /// SSID of the associated network (None while disconnected).
    pub wifi_ssid: Option<String>,
    /// Reconnect planner is working (Grace/Round/Wait, i.e. disconnected).
    pub reconnecting: bool,
    /// UURemoteServer traffic indicates an active remote session.
    pub session_active: bool,
    /// DisplayController currently holds the display awake.
    pub screen_awake: bool,
    /// `UURemoteServer` process is not (or no longer) running.
    pub urnot_detected: bool,
}

impl Default for AppStatus {
    fn default() -> Self {
        Self {
            sleep_ok: false,
            autorestart_ok: false,
            wifi_connected: false,
            wifi_ssid: None,
            reconnecting: false,
            session_active: false,
            screen_awake: false,
            urnot_detected: false,
        }
    }
}

/// Everything one tick produced, pre-computed outside [`combine`] so the
/// merge stays pure (Ruling 1): the controller is ticked by [`tick_loop`]
/// immediately before [`combine`] is called.
pub struct TickInputs {
    /// Fresh `pmset -g` reading; `None` on failure → keep last-known values.
    pub power: Option<PowerSettings>,
    /// Session detector's verdict for this tick.
    pub session_active: bool,
    /// Current SSID (`None` while disassociated or unreadable).
    pub ssid: Option<String>,
    /// Wi-Fi association as fed to the reconnect planner (`ssid.is_some()`).
    pub associated: bool,
    /// Reconnect planner's decision for this tick.
    pub decision: RoundDecision,
    /// Commands already returned by `DisplayController::tick`.
    pub screen_cmds: Vec<ScreenCmd>,
    /// `DisplayController::is_awake()` after that tick.
    pub screen_awake: bool,
    /// UURemoteServer pid was not discovered.
    pub urnot_detected: bool,
    /// Manual-wake deadline set by the tray (Task 7).
    pub manual_wake_until: Option<Instant>,
    /// Lid transitioned closed→open this tick.
    pub lid_opened_edge: bool,
    /// Timestamp of this tick.
    pub now: Instant,
}

/// Pure merge of one tick's module outputs into the next [`AppStatus`] and
/// the [`ScreenCmd`]s the tick loop must execute.
///
/// Failure policy: `power: None` keeps the previous power flags (last-known
/// values, Ruling 7). The lid closed→open edge forces a display black when
/// nothing wants the screen awake — macOS may light the display on lid-open
/// even after an earlier `ForceSleepDisplay`, and the controller's `awake`
/// flag is already false then, so its own transition would not fire
/// (Ruling 4). Only `ForceSleepDisplay` is executed as a system action; the
/// wake-hold start/stop variants are informational because the controller
/// owns the assertion internally (Ruling 3).
pub fn combine(prev: &AppStatus, inputs: TickInputs) -> (AppStatus, Vec<ScreenCmd>) {
    let mut cmds = inputs.screen_cmds;
    let manual_awake = inputs
        .manual_wake_until
        .is_some_and(|deadline| inputs.now < deadline);
    if inputs.lid_opened_edge
        && !inputs.session_active
        && !manual_awake
        && !cmds.contains(&ScreenCmd::ForceSleepDisplay)
    {
        cmds.push(ScreenCmd::ForceSleepDisplay);
    }
    let status = AppStatus {
        sleep_ok: inputs
            .power
            .as_ref()
            .map_or(prev.sleep_ok, |p| p.sleep_disabled),
        autorestart_ok: inputs
            .power
            .as_ref()
            .map_or(prev.autorestart_ok, |p| p.autorestart),
        wifi_connected: inputs.associated,
        wifi_ssid: inputs.ssid,
        reconnecting: matches!(
            inputs.decision,
            RoundDecision::Grace | RoundDecision::Round | RoundDecision::Wait
        ),
        session_active: inputs.session_active,
        screen_awake: inputs.screen_awake,
        urnot_detected: inputs.urnot_detected,
    };
    (status, cmds)
}

/// Shared application state managed by Tauri and driven by [`tick_loop`].
pub struct AppState {
    /// Latest tray-facing snapshot; updated every tick.
    pub status: Mutex<AppStatus>,
    /// Manual-wake deadline (`now + 5 min` from the tray); `tick_loop`
    /// passes it to `DisplayController::tick` each pass.
    pub manual_wake_until: Mutex<Option<Instant>>,
    /// One-shot "reconnect now" flag: when true, `tick_loop` runs one
    /// `try_networks` round bypassing planner timing, then clears it.
    pub force_round: AtomicBool,
    /// Remote-session detector (UURemoteServer throughput).
    session: Mutex<SessionDetector>,
    /// Display state machine owning the wake-hold assertion.
    display: Mutex<DisplayController>,
    /// Wi-Fi reconnect cadence.
    planner: Mutex<ReconnectPlanner>,
    /// System command runner used by the tick loop.
    runner: SystemRunner,
}

impl AppState {
    /// Fresh state: asleep, no manual window, no forced round, all modules
    /// in their initial (idle) states.
    pub fn new() -> Self {
        Self {
            status: Mutex::new(AppStatus::default()),
            manual_wake_until: Mutex::new(None),
            force_round: AtomicBool::new(false),
            session: Mutex::new(SessionDetector::new()),
            display: Mutex::new(DisplayController::production()),
            planner: Mutex::new(ReconnectPlanner::new()),
            runner: SystemRunner,
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

/// Lock a [`Mutex`], recovering the guarded value if a previous holder
/// panicked — the scheduler must never panic (Ruling 7).
fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// First pid printed by `pgrep -f UURemoteServer`; `None` when absent or
/// unreadable (a no-match exit is normal, not logged).
fn discover_pid(runner: &SystemRunner) -> Option<u32> {
    match runner.run("pgrep", &["-f", "UURemoteServer"]) {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .and_then(|line| line.trim().parse::<u32>().ok()),
        Ok(out) => {
            // pgrep exits 1 when nothing matches — the common
            // "UURemoteServer not running" case, not an error.
            if out.status.code() != Some(1) {
                eprintln!(
                    "keepawake: pgrep -f UURemoteServer failed with {}",
                    out.status
                );
            }
            None
        }
        Err(err) => {
            eprintln!("keepawake: pgrep -f UURemoteServer: {err}");
            None
        }
    }
}

/// Lid state from `ioreg -r -n IOPMrootDomain -d 1`.
///
/// Mapping observed on the live machine (lid open at the time):
/// `"AppleClamshellState" = No` — `No` ⇒ lid **open**, `Yes` ⇒ lid
/// **closed**. The `-r` flag is required: plain
/// `ioreg -n IOPMrootDomain -d 1` prints only the plane-root header and
/// none of the entry's properties on this macOS build.
fn lid_is_open(runner: &SystemRunner) -> Option<bool> {
    let output = match runner.run("ioreg", &["-r", "-n", "IOPMrootDomain", "-d", "1"]) {
        Ok(out) => out,
        Err(err) => {
            eprintln!("keepawake: ioreg ClamshellState: {err}");
            return None;
        }
    };
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        // Matches the registry key `AppleClamshellState` (the other
        // clamshell keys — `AppleClamshellCausesSleep`, `Clamshell Sleep`
        // — do not contain this substring).
        if line.contains("ClamshellState") && line.contains('=') {
            let value = line.split('=').nth(1)?.trim();
            return match value {
                "No" => Some(true),   // lid open (observed live)
                "Yes" => Some(false), // lid closed
                _ => None,
            };
        }
    }
    eprintln!("keepawake: ioreg returned no ClamshellState property");
    None
}

/// Run one `try_networks` round against the preferred networks of the
/// current Wi-Fi device; a missing device (or unreadable device list)
/// skips the round with a stderr note (Ruling 8).
fn run_reconnect_round(runner: &SystemRunner) {
    let ports = match runner.run("networksetup", &["-listallhardwareports"]) {
        Ok(out) => String::from_utf8_lossy(&out.stdout).into_owned(),
        Err(err) => {
            eprintln!("keepawake: networksetup -listallhardwareports: {err}");
            return;
        }
    };
    let Some(dev) = parse_wifi_device(&ports) else {
        eprintln!("keepawake: no Wi-Fi device found; skipping reconnect round");
        return;
    };
    let preferred_text =
        match runner.run("networksetup", &["-listpreferredwirelessnetworks", &dev]) {
            Ok(out) => String::from_utf8_lossy(&out.stdout).into_owned(),
            Err(err) => {
                eprintln!("keepawake: networksetup -listpreferredwirelessnetworks: {err}");
                return;
            }
        };
    let preferred = parse_preferred_networks(&preferred_text);
    for (ssid, ok) in try_networks(runner, &dev, &preferred) {
        if !ok {
            eprintln!("keepawake: failed to join network {ssid}");
        }
    }
}

/// Execute the cmds [`combine`] produced. Only `ForceSleepDisplay` runs a
/// system command (`pmset displaysleepnow`, idempotent, spec §7); the
/// wake-hold variants are informational — the `DisplayController` already
/// owns the assertion via its factory (Ruling 3).
fn execute_screen_cmd(runner: &SystemRunner, cmd: ScreenCmd) {
    match cmd {
        ScreenCmd::ForceSleepDisplay => match runner.run("pmset", &["displaysleepnow"]) {
            Ok(out) if out.status.success() => {}
            Ok(out) => eprintln!(
                "keepawake: pmset displaysleepnow failed with {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ),
            Err(err) => eprintln!("keepawake: pmset displaysleepnow: {err}"),
        },
        ScreenCmd::StartDisplayWakeHold | ScreenCmd::StopDisplayWakeHold => {
            // Informational: the controller manages the guard internally.
        }
    }
}

/// Whether the `pid == None` streak is due for a `pgrep` re-discovery:
/// at most once every 3 ticks (streaks 1, 4, 7, …).
fn should_rediscover(none_streak: u32) -> bool {
    none_streak % 3 == 1
}

/// Pure core of the tick loop's `pid == None` branch: advance the streak,
/// re-run discovery via `found` on cadence ([`should_rediscover`]), and
/// report this tick's `urnot_detected`. On success the pid is stored and
/// the streak resets — `urnot_detected` clears on that same tick, while
/// `sample_now` for the new pid resumes from the next tick. `None` is not a
/// terminal state: without this, a UURemoteServer that starts after
/// KeepAwake (login-item race) or restarts later would stay undetected
/// until app restart.
fn none_tick(
    pid: &mut Option<u32>,
    none_streak: &mut u32,
    found: impl FnOnce() -> Option<u32>,
) -> bool {
    *none_streak += 1;
    if should_rediscover(*none_streak) {
        if let Some(id) = found() {
            *pid = Some(id);
            *none_streak = 0;
        }
    }
    pid.is_none()
}

/// 5-second scheduler: gather real inputs, tick the display controller,
/// [`combine`] them into an [`AppStatus`], execute the display cmds, and
/// emit `status_update` for the tray. Never panics; system-call failures
/// log to stderr and the loop continues with last-known values.
pub async fn tick_loop(app: tauri::AppHandle) {
    // UURemoteServer pid: initial discovery at loop start (Ruling 5),
    // re-discovered after 3 nettop misses and, from the None state, at
    // most every 3 ticks via `none_tick`.
    let mut pid = {
        let state = app.state::<AppState>();
        discover_pid(&state.runner)
    };
    let mut pid_misses: u32 = 0;
    let mut none_streak: u32 = 0;
    let mut prev_lid_open: Option<bool> = None;

    loop {
        // All `AppState` borrows end at this block's close, before the
        // await below — the loop itself never holds state across a yield.
        {
            let state = app.state::<AppState>();
            let runner = &state.runner;
            let now = Instant::now();

            // --- Power settings (last-known kept on failure) ---
            let power = match current_settings(runner) {
                Ok(settings) => Some(settings),
                Err(err) => {
                    eprintln!("keepawake: pmset -g: {err}");
                    None
                }
            };

            // --- Remote session + UURemoteServer pid ---
            let (session_active, urnot_detected) = {
                let mut detector = lock_recover(&state.session);
                match pid {
                    Some(id) => match sample_now(runner, id) {
                        Some(total) => {
                            pid_misses = 0;
                            (detector.observe(total, now), false)
                        }
                        None => {
                            pid_misses += 1;
                            eprintln!(
                                "keepawake: nettop sample for pid {id} returned nothing \
                                 ({pid_misses}/{PID_MISS_LIMIT})"
                            );
                            if pid_misses >= PID_MISS_LIMIT {
                                // Process likely restarted: re-discover it.
                                pid = discover_pid(runner);
                                pid_misses = 0;
                            }
                            // Feed the detector as idle (a missing row means
                            // no observed traffic); urnot follows the pid.
                            (detector.observe(0, now), pid.is_none())
                        }
                    },
                    // No pid: detector fed as idle, sample_now never called
                    // (Ruling 5). Not terminal — `none_tick` re-runs pgrep
                    // at most every 3 ticks; on success the pid is stored
                    // and urnot_detected clears this same tick (session
                    // sampling resumes next tick).
                    None => (
                        detector.observe(0, now),
                        none_tick(&mut pid, &mut none_streak, || discover_pid(runner)),
                    ),
                }
            };

            // --- Wi-Fi association + reconnect planner ---
            let ssid = current_ssid(runner);
            let associated = ssid.is_some();
            let decision = {
                let mut planner = lock_recover(&state.planner);
                planner.on_tick(associated, now)
            };

            // --- Wi-Fi round: planner-timed or tray-forced (one-shot) ---
            let forced = state.force_round.swap(false, Ordering::SeqCst);
            if forced || matches!(decision, RoundDecision::Round) {
                run_reconnect_round(runner);
            }

            // --- Lid clamshell state + closed→open edge (Ruling 4) ---
            let (lid_open, lid_opened_edge) = match lid_is_open(runner) {
                Some(open) => {
                    let edge = prev_lid_open == Some(false) && open;
                    prev_lid_open = Some(open);
                    (open, edge)
                }
                None => {
                    // Keep last-known clamshell state; no edge without a
                    // fresh reading (failure policy: eprintln + last-known).
                    (prev_lid_open.unwrap_or(true), false)
                }
            };

            // --- Display controller tick (immediately before combine) ---
            let manual_wake_until = *lock_recover(&state.manual_wake_until);
            let (screen_cmds, screen_awake) = {
                let mut display = lock_recover(&state.display);
                let cmds = display.tick(session_active, lid_open, manual_wake_until, now);
                (cmds, display.is_awake())
            };

            // --- Pure merge (Ruling 1) ---
            let inputs = TickInputs {
                power,
                session_active,
                ssid,
                associated,
                decision,
                screen_cmds,
                screen_awake,
                urnot_detected,
                manual_wake_until,
                lid_opened_edge,
                now,
            };
            let (new_status, cmds) = {
                let prev = lock_recover(&state.status);
                combine(&prev, inputs)
            };

            // --- Execute display cmds, then publish the new status ---
            for cmd in cmds {
                execute_screen_cmd(runner, cmd);
            }
            *lock_recover(&state.status) = new_status.clone();
            if let Err(err) = app.emit("status_update", &new_status) {
                eprintln!("keepawake: emit status_update: {err}");
            }
        }

        tokio::time::sleep(TICK).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assertion::AssertionGuard;
    use crate::display::{DisplayController, ScreenCmd};
    use crate::power::PowerSettings;
    use crate::wifi::RoundDecision;
    use std::time::{Duration, Instant};

    /// Baseline tick inputs; individual tests override the fields they care
    /// about via struct-update syntax.
    fn inputs() -> TickInputs {
        TickInputs {
            power: Some(PowerSettings {
                sleep_disabled: true,
                autorestart: false,
            }),
            session_active: false,
            ssid: None,
            associated: false,
            decision: RoundDecision::Monitor,
            screen_cmds: Vec::new(),
            screen_awake: false,
            urnot_detected: false,
            manual_wake_until: None,
            lid_opened_edge: false,
            now: Instant::now(),
        }
    }

    /// Mandated test (brief Step 1 / Ruling 1): session inactive + manual
    /// wake expired + previously awake ⇒ `screen_awake=false` and
    /// `ForceSleepDisplay` present.
    #[test]
    fn combine_session_inactive_and_manual_expired_black_display() {
        let t0 = Instant::now();
        let manual = Some(t0 + Duration::from_secs(300));
        let mut display =
            DisplayController::with_factory(Box::new(|| Ok(AssertionGuard::fake())));
        display.tick(true, true, None, t0); // previously awake
        assert!(display.is_awake());

        let now = t0 + Duration::from_secs(600); // manual window expired
        let screen_cmds = display.tick(false, true, manual, now);
        let prev = AppStatus {
            screen_awake: true,
            session_active: true,
            ..AppStatus::default()
        };
        let (status, cmds) = combine(
            &prev,
            TickInputs {
                session_active: false,
                ssid: Some("HomeNet".to_string()),
                associated: true,
                screen_cmds,
                screen_awake: display.is_awake(),
                manual_wake_until: manual,
                now,
                ..inputs()
            },
        );
        assert!(!status.screen_awake);
        assert!(
            cmds.contains(&ScreenCmd::ForceSleepDisplay),
            "ForceSleepDisplay must be executed, got {cmds:?}"
        );
        assert!(!status.session_active);
        assert_eq!(status.wifi_ssid.as_deref(), Some("HomeNet"));
        assert!(status.wifi_connected);
        assert!(!status.reconnecting);
    }

    /// Lid closed→open edge while nothing wants the screen awake must force
    /// a display black even though the controller's own transition already
    /// happened (its `awake` flag is stale after macOS relights on
    /// lid-open).
    #[test]
    fn combine_lid_open_edge_while_idle_forces_display_sleep() {
        let now = Instant::now();
        let mut display =
            DisplayController::with_factory(Box::new(|| Ok(AssertionGuard::fake())));
        let screen_cmds = display.tick(false, true, None, now); // idle: no cmds
        assert!(screen_cmds.is_empty());

        let (status, cmds) = combine(
            &AppStatus::default(),
            TickInputs {
                screen_cmds,
                screen_awake: display.is_awake(),
                urnot_detected: true,
                decision: RoundDecision::Grace,
                lid_opened_edge: true,
                now,
                ..inputs()
            },
        );
        assert_eq!(cmds, vec![ScreenCmd::ForceSleepDisplay]);
        assert!(!status.screen_awake);
        assert!(status.urnot_detected);
        assert!(status.reconnecting, "Grace counts as reconnecting");
    }

    /// The lid-edge black must not fire while a manual wake window is still
    /// active (the tray's 5-minute keep-awake outranks the lid event).
    #[test]
    fn combine_lid_open_edge_during_manual_wake_stays_quiet() {
        let now = Instant::now();
        let manual = Some(now + Duration::from_secs(300));
        let mut display =
            DisplayController::with_factory(Box::new(|| Ok(AssertionGuard::fake())));
        display.tick(false, true, manual, now); // manual wake holds the display
        assert!(display.is_awake());

        let (_, cmds) = combine(
            &AppStatus::default(),
            TickInputs {
                manual_wake_until: manual,
                screen_awake: display.is_awake(),
                lid_opened_edge: true,
                now,
                ..inputs()
            },
        );
        assert!(
            cmds.is_empty(),
            "manual wake must suppress lid-edge black: {cmds:?}"
        );
    }

    /// A failed `pmset -g` (`power: None`) keeps the previous flags
    /// (last-known values, Ruling 7).
    #[test]
    fn combine_keeps_last_known_power_when_pmset_fails() {
        let prev = AppStatus {
            sleep_ok: true,
            autorestart_ok: true,
            ..AppStatus::default()
        };
        let (status, _) = combine(&prev, TickInputs { power: None, ..inputs() });
        assert!(status.sleep_ok);
        assert!(status.autorestart_ok);
    }

    /// `reconnecting` mirrors the planner: Grace/Round/Wait mean the link is
    /// down and being worked on; Monitor means healthy.
    #[test]
    fn combine_marks_reconnecting_unless_monitoring() {
        for (decision, expected) in [
            (RoundDecision::Monitor, false),
            (RoundDecision::Grace, true),
            (RoundDecision::Round, true),
            (RoundDecision::Wait, true),
        ] {
            let (status, _) = combine(
                &AppStatus::default(),
                TickInputs {
                    decision,
                    ..inputs()
                },
            );
            assert_eq!(status.reconnecting, expected, "{decision:?}");
        }
    }

    /// `AppStatus` must round-trip through serde — the tray receives it as
    /// the JSON payload of the `status_update` event.
    #[test]
    fn app_status_event_json_roundtrip() {
        let status = AppStatus {
            sleep_ok: true,
            autorestart_ok: false,
            wifi_connected: true,
            wifi_ssid: Some("HomeNet".to_string()),
            reconnecting: false,
            session_active: true,
            screen_awake: true,
            urnot_detected: false,
        };
        let json = serde_json::to_string(&status).expect("AppStatus must serialize");
        let back: AppStatus = serde_json::from_str(&json).expect("AppStatus must deserialize");
        assert_eq!(back, status);
    }

    /// Re-discovery from the `None` state must run at most once every 3
    /// ticks (cheap `pgrep -f UURemoteServer`): true at streaks 1, 4, 7, …
    #[test]
    fn should_rediscover_every_third_none_tick() {
        let expected = [
            (0, false), // streak is incremented before the check
            (1, true),
            (2, false),
            (3, false),
            (4, true),
            (5, false),
            (6, false),
            (7, true),
        ];
        for (streak, want) in expected {
            assert_eq!(
                should_rediscover(streak),
                want,
                "streak {streak} should_rediscover"
            );
        }
    }

    /// `pid == None` must not be terminal: a successful re-discovery stores
    /// the pid, resets the streak, and clears `urnot_detected` on that same
    /// tick (session sampling resumes the next tick).
    #[test]
    fn none_tick_rediscovery_clears_urnot_same_tick() {
        let mut pid: Option<u32> = None;
        let mut streak = 0u32;

        // Failed attempts at streaks 1 and 4; cadence ticks in between.
        assert!(none_tick(&mut pid, &mut streak, || None));
        assert!(none_tick(&mut pid, &mut streak, || None));
        assert!(none_tick(&mut pid, &mut streak, || None));
        assert!(none_tick(&mut pid, &mut streak, || None)); // streak 4: attempt
        assert_eq!(pid, None, "still absent after failed attempts");
        assert_eq!(streak, 4, "streak keeps advancing past failed attempts");

        // Streak 7 attempt succeeds → pid stored, urnot clears same tick.
        none_tick(&mut pid, &mut streak, || None); // 5
        none_tick(&mut pid, &mut streak, || None); // 6
        let urnot = none_tick(&mut pid, &mut streak, || Some(4242)); // 7: attempt
        assert_eq!(pid, Some(4242));
        assert_eq!(streak, 0, "streak resets on success");
        assert!(!urnot, "urnot_detected must clear on the rediscovery tick");
    }
}

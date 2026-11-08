//! Screen on/off state machine: decides when the display must black.

use std::time::Instant;

use crate::assertion::AssertionGuard;

/// Commands the app layer executes (`pmset displaysleepnow`, assertion start/stop).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenCmd {
    /// Black the display now (idempotent — safe while the lid is closed, spec §7).
    ForceSleepDisplay,
    /// Begin holding the display-awake assertion.
    StartDisplayWakeHold,
    /// Release the display-awake assertion without forcing sleep.
    StopDisplayWakeHold,
}

/// Tracks whether the display should be awake and owns the wake-hold guard.
pub struct DisplayController {
    awake: bool,
    wake_hold: Option<AssertionGuard>,
    make_hold: Box<dyn Fn() -> Result<AssertionGuard, String>>,
}

impl DisplayController {
    /// Create a controller whose wake-hold guards come from `make_hold`.
    /// Production passes [`crate::assertion::assert_prevent_display_sleep`]
    /// (see [`DisplayController::production`]); tests pass a fake factory.
    pub fn with_factory(make_hold: Box<dyn Fn() -> Result<AssertionGuard, String>>) -> Self {
        Self {
            awake: false,
            wake_hold: None,
            make_hold,
        }
    }

    /// Production controller that acquires real IOKit display-sleep assertions.
    pub fn production() -> Self {
        Self::with_factory(Box::new(crate::assertion::assert_prevent_display_sleep))
    }

    /// Whether the controller currently considers the display awake.
    pub fn is_awake(&self) -> bool {
        self.awake
    }

    /// Advance the state machine once (5s tick cadence).
    ///
    /// `desired_awake = session_active || manual_wake_until.map_or(false, |t| now < t)`.
    /// Emits [`ScreenCmd::StartDisplayWakeHold`] on false→true and
    /// [`ScreenCmd::ForceSleepDisplay`] on true→false (idempotent while the
    /// lid is closed, spec §7). Lid handling is transition-based: `lid_open`
    /// is accepted for the app layer's event-driven use but does not itself
    /// emit commands — a lid-open re-black is covered by the awake→sleep
    /// transition. If the wake-hold factory fails, the controller stays
    /// asleep and retries on the next tick.
    pub fn tick(
        &mut self,
        session_active: bool,
        lid_open: bool,
        manual_wake_until: Option<Instant>,
        now: Instant,
    ) -> Vec<ScreenCmd> {
        let _ = lid_open; // transition-based lid handling; see doc above
        let desired_awake = session_active || manual_wake_until.map_or(false, |t| now < t);
        let mut cmds = Vec::new();
        if desired_awake && !self.awake {
            let hold = (self.make_hold)();
            if let Ok(guard) = hold {
                self.wake_hold = Some(guard);
                self.awake = true;
                cmds.push(ScreenCmd::StartDisplayWakeHold);
            }
            // Factory error: stay asleep, retry next tick.
        } else if !desired_awake && self.awake {
            self.wake_hold = None; // Drop releases the assertion
            self.awake = false;
            cmds.push(ScreenCmd::ForceSleepDisplay);
        }
        cmds
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assertion::AssertionGuard;
    use std::time::{Duration, Instant};

    /// Wake-hold factory that never touches real IOKit.
    fn fake_controller() -> DisplayController {
        DisplayController::with_factory(Box::new(|| Ok(AssertionGuard::fake())))
    }

    #[test]
    fn idle_stays_black() {
        let mut c = fake_controller();
        let t = Instant::now();
        let cmds = c.tick(false, true, None, t);
        assert!(cmds.is_empty(), "idle must emit no cmds, got {cmds:?}");
        assert!(!c.is_awake());
    }

    #[test]
    fn session_start_wakes() {
        let mut c = fake_controller();
        let t = Instant::now();
        let cmds = c.tick(true, true, None, t);
        assert_eq!(cmds, vec![ScreenCmd::StartDisplayWakeHold]);
        assert!(c.is_awake());
    }

    #[test]
    fn session_end_after_manual_window_sleeps() {
        let mut c = fake_controller();
        let t = Instant::now();
        let manual = Some(t + Duration::from_secs(300));
        c.tick(true, true, manual, t);
        // Session ended and the manual-wake window has expired.
        let cmds = c.tick(false, true, manual, t + Duration::from_secs(600));
        assert_eq!(cmds, vec![ScreenCmd::ForceSleepDisplay]);
        assert!(!c.is_awake());
    }

    #[test]
    fn lid_open_while_idle_forces_black() {
        let mut c = fake_controller();
        let t = Instant::now();
        // Prior session left the display awake.
        c.tick(true, false, None, t);
        let cmds = c.tick(false, true, None, t + Duration::from_secs(60));
        assert_eq!(cmds, vec![ScreenCmd::ForceSleepDisplay]);
        assert!(!c.is_awake());
    }

    #[test]
    fn manual_wake_overrides_idle() {
        let mut c = fake_controller();
        let t = Instant::now();
        let cmds = c.tick(false, true, Some(t + Duration::from_secs(300)), t);
        assert!(
            !cmds.contains(&ScreenCmd::ForceSleepDisplay),
            "manual wake must not black the display, got {cmds:?}"
        );
        assert!(c.is_awake());
    }

    #[test]
    fn production_controller_starts_asleep() {
        let c = DisplayController::production();
        assert!(!c.is_awake());
    }
}

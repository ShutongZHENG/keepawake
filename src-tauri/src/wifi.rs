//! Wi-Fi device/SSID parsers and the 30 s reconnect planner.

use std::time::{Duration, Instant};

use crate::cmd::CommandRunner;

/// Grace period after a disconnect: gives the system's own auto-connect a
/// chance before we start forcing preferred networks.
const GRACE: Duration = Duration::from_secs(30);
/// Minimum spacing between reconnect rounds once grace has elapsed.
const ROUND_INTERVAL: Duration = Duration::from_secs(30);

/// What the reconnect planner wants the tick loop to do this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundDecision {
    /// Associated (or freshly reset) — observe only, nothing to reconnect.
    Monitor,
    /// Disconnected, still inside the auto-connect grace window.
    Grace,
    /// Grace elapsed and ≥30 s since the last round (or none yet) —
    /// try the preferred networks now.
    Round,
    /// Grace elapsed but <30 s since the last round — hold until due.
    Wait,
}

/// Pure time-state for the Wi-Fi reconnect cadence (no sleeping inside).
pub struct ReconnectPlanner {
    disconnected_since: Option<Instant>,
    last_round: Option<Instant>,
}

impl ReconnectPlanner {
    /// Fresh planner: no disconnect, no rounds.
    pub fn new() -> Self {
        Self {
            disconnected_since: None,
            last_round: None,
        }
    }

    /// Advance the planner once (5 s tick cadence from the scheduler).
    ///
    /// `associated=true` resets both timestamps and returns [`RoundDecision::Monitor`].
    /// On disconnect the first tick stamps `disconnected_since` and stays in
    /// [`RoundDecision::Grace`] for 30 s; afterwards [`RoundDecision::Round`] fires
    /// when `last_round` is absent or ≥30 s old (stamping `last_round`), with
    /// [`RoundDecision::Wait`] between rounds.
    pub fn on_tick(&mut self, associated: bool, now: Instant) -> RoundDecision {
        if associated {
            self.disconnected_since = None;
            self.last_round = None;
            return RoundDecision::Monitor;
        }
        let since = match self.disconnected_since {
            Some(since) => since,
            None => {
                self.disconnected_since = Some(now);
                now
            }
        };
        if now.duration_since(since) < GRACE {
            return RoundDecision::Grace;
        }
        match self.last_round {
            Some(last) if now.duration_since(last) < ROUND_INTERVAL => RoundDecision::Wait,
            Some(_) | None => {
                self.last_round = Some(now);
                RoundDecision::Round
            }
        }
    }
}

/// Parse `networksetup -listallhardwareports` output: the block containing
/// `Port: Wi-Fi` (or `Port: AirPort`) yields its `Device: <name>` line.
/// No Wi-Fi block (or one without a device line) → `None`.
pub fn parse_wifi_device(listallhardwareports: &str) -> Option<String> {
    let mut in_wifi = false;
    for line in listallhardwareports.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            in_wifi = false;
            continue;
        }
        if trimmed.contains("Port:") {
            // Any port line opens a new block; only Wi-Fi/AirPort counts.
            in_wifi = trimmed.contains("Port: Wi-Fi") || trimmed.contains("Port: AirPort");
            continue;
        }
        if in_wifi {
            if let Some(rest) = trimmed.strip_prefix("Device:") {
                let name = rest.trim();
                if !name.is_empty() {
                    return Some(name.to_string());
                }
            }
        }
    }
    None
}

/// Parse `networksetup -listpreferredwirelessnetworks <dev>` output: skip the
/// header line, then take every remaining non-empty line, trimmed.
pub fn parse_preferred_networks(text: &str) -> Vec<String> {
    text.lines()
        .skip(1) // header: "Preferred networks for <dev>:"
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// Read the associated SSID via `ipconfig getsummary <dev>`; `None` on spawn
/// error, parse miss, or a disassociated summary.
pub fn current_ssid(runner: &impl CommandRunner, dev: &str) -> Option<String> {
    let output = runner.run("ipconfig", &["getsummary", dev]).ok()?;
    parse_ssid_from_summary(&String::from_utf8_lossy(&output.stdout))
}

/// Pull the SSID out of `ipconfig getsummary` text: the first line containing
/// `SSID` with a non-empty value after `:`. Output mentioning
/// "not associated" (or no such line / an empty value) → `None`.
pub fn parse_ssid_from_summary(text: &str) -> Option<String> {
    if text.contains("not associated") {
        return None;
    }
    for line in text.lines() {
        if line.contains("SSID") {
            if let Some((_, value)) = line.split_once(':') {
                let value = value.trim();
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }
    None
}

/// Try each preferred SSID in list order via
/// `networksetup -setairportnetwork <dev> <ssid>`; a try succeeds when the
/// exit status is 0 and stdout+stderr do not contain `Failed`.
/// Returns `(ssid, ok)` per input, in order.
pub fn try_networks(
    runner: &impl CommandRunner,
    dev: &str,
    ssids: &[String],
) -> Vec<(String, bool)> {
    ssids
        .iter()
        .map(|ssid| {
            let ok = match runner.run("networksetup", &["-setairportnetwork", dev, ssid.as_str()]) {
                Ok(out) => {
                    let text = format!(
                        "{}{}",
                        String::from_utf8_lossy(&out.stdout),
                        String::from_utf8_lossy(&out.stderr)
                    );
                    out.status.success() && !text.contains("Failed")
                }
                Err(_) => false,
            };
            (ssid.clone(), ok)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::FakeRunner;
    use std::time::{Duration, Instant};

    #[test]
    fn parses_device_from_hardware_ports() {
        let ports = "\
Hardware Port: Wi-Fi
Device: en0
Ethernet Address: aa:bb:cc:dd:ee:ff

Hardware Port: Bluetooth PAN
Device: en5
Ethernet Address: <null>

Hardware Port: Thunderbolt Bridge
Device: en4
Ethernet Address: <null>
";
        assert_eq!(parse_wifi_device(ports), Some("en0".to_string()));
        // Older macOS names the port AirPort.
        let airport = "Hardware Port: AirPort\nDevice: en1\n\nHardware Port: Ethernet\nDevice: en2\n";
        assert_eq!(parse_wifi_device(airport), Some("en1".to_string()));
    }

    #[test]
    fn wifi_port_absent_returns_none() {
        let ports = "Hardware Port: Ethernet\nDevice: en0\n\nHardware Port: Bluetooth PAN\nDevice: en5\n";
        assert_eq!(parse_wifi_device(ports), None);
        assert_eq!(parse_wifi_device("Hardware Port: Wi-Fi\n"), None);
        assert_eq!(parse_wifi_device(""), None);
    }

    #[test]
    fn parses_preferred_networks_list() {
        let text = "Preferred networks for en0:\n    HomeNet\n\n  CafeWiFi  \n";
        assert_eq!(parse_preferred_networks(text), vec!["HomeNet", "CafeWiFi"]);
        assert_eq!(parse_preferred_networks(""), Vec::<String>::new());
    }

    #[test]
    fn parses_ssid_when_associated() {
        let summary = "\
ipconfig getsummary en0
[...]
SSID : HomeNet
Router : 192.168.1.1
";
        assert_eq!(parse_ssid_from_summary(summary), Some("HomeNet".to_string()));
    }

    #[test]
    fn no_ssid_when_disassociated() {
        // No SSID line at all.
        assert_eq!(parse_ssid_from_summary("Router : 192.168.1.1\n"), None);
        // Disassociated marker wins outright.
        assert_eq!(
            parse_ssid_from_summary("You are not associated with an AirPort network."),
            None
        );
        // SSID line present but empty.
        assert_eq!(parse_ssid_from_summary("SSID : \n"), None);
    }

    #[test]
    fn current_ssid_invokes_ipconfig_getsummary() {
        let fake = FakeRunner::with_stdout("SSID : HomeNet\n");
        assert_eq!(current_ssid(&fake, "en0"), Some("HomeNet".to_string()));
        let calls = fake.calls.lock().expect("calls lock");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "ipconfig");
        assert_eq!(calls[0].1, vec!["getsummary", "en0"]);
    }

    #[test]
    fn no_rounds_during_grace() {
        let mut p = ReconnectPlanner::new();
        let t0 = Instant::now();
        for secs in 0..=29u64 {
            let now = t0 + Duration::from_secs(secs);
            assert_eq!(
                p.on_tick(false, now),
                RoundDecision::Grace,
                "tick at {secs}s must stay in grace, never Round"
            );
        }
    }

    #[test]
    fn round_every_30s_after_grace() {
        let mut p = ReconnectPlanner::new();
        let t0 = Instant::now();
        assert_eq!(p.on_tick(false, t0), RoundDecision::Grace);
        // Grace ends at t+30s → first round.
        assert_eq!(
            p.on_tick(false, t0 + Duration::from_secs(30)),
            RoundDecision::Round
        );
        // 15 s after the round → wait, no second round.
        assert_eq!(
            p.on_tick(false, t0 + Duration::from_secs(45)),
            RoundDecision::Wait
        );
        // 30 s after the last round → round again.
        assert_eq!(
            p.on_tick(false, t0 + Duration::from_secs(60)),
            RoundDecision::Round
        );
    }

    #[test]
    fn reconnect_resets_planner() {
        let mut p = ReconnectPlanner::new();
        let t0 = Instant::now();
        assert_eq!(p.on_tick(false, t0), RoundDecision::Grace);
        // Association resets both timestamps.
        assert_eq!(p.on_tick(true, t0 + Duration::from_secs(60)), RoundDecision::Monitor);
        // Next disconnect starts grace anew (no stale last_round).
        assert_eq!(
            p.on_tick(false, t0 + Duration::from_secs(70)),
            RoundDecision::Grace
        );
        assert_eq!(
            p.on_tick(false, t0 + Duration::from_secs(95)),
            RoundDecision::Grace,
            "25 s into the new grace must not round"
        );
        assert_eq!(
            p.on_tick(false, t0 + Duration::from_secs(100)),
            RoundDecision::Round,
            "30 s after the new disconnect must round"
        );
        assert_eq!(
            p.on_tick(false, t0 + Duration::from_secs(115)),
            RoundDecision::Wait
        );
    }

    #[test]
    fn try_networks_tries_each_ssid_in_order() {
        let fake = FakeRunner::with_stdout("");
        let ssids = vec!["HomeNet".to_string(), "CafeWiFi".to_string()];
        assert_eq!(
            try_networks(&fake, "en0", &ssids),
            vec![("HomeNet".to_string(), true), ("CafeWiFi".to_string(), true)]
        );
        let calls = fake.calls.lock().expect("calls lock");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "networksetup");
        assert_eq!(calls[0].1, vec!["-setairportnetwork", "en0", "HomeNet"]);
        assert_eq!(calls[1].0, "networksetup");
        assert_eq!(calls[1].1, vec!["-setairportnetwork", "en0", "CafeWiFi"]);
    }

    #[test]
    fn try_networks_marks_failure_false() {
        let ssids = vec!["HomeNet".to_string()];
        // Exit 0 but the tool reported Failed → false.
        let fake = FakeRunner::with_stdout("Failed to join network\n");
        assert_eq!(
            try_networks(&fake, "en0", &ssids),
            vec![("HomeNet".to_string(), false)]
        );
        // Non-zero exit → false.
        let fake = FakeRunner::with_stdout("").failing_with(1, "");
        assert_eq!(
            try_networks(&fake, "en0", &ssids),
            vec![("HomeNet".to_string(), false)]
        );
    }
}

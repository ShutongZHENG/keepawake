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

/// Read the associated SSID via `system_profiler SPAirPortDataType`; `None`
/// on spawn error or parse miss (e.g. disassociated).
pub fn current_ssid(runner: &impl CommandRunner) -> Option<String> {
    let output = runner.run("system_profiler", &["SPAirPortDataType"]).ok()?;
    parse_current_network(&String::from_utf8_lossy(&output.stdout))
}

/// Extract the current network name from `system_profiler SPAirPortDataType`
/// output: scope to the top-level `Wi-Fi:` section; within it, after the first
/// `Current Network Information:` header, the name is the first subsequent
/// line that ends with `:` and has nothing after it (e.g. `ZTE-A7UPhK:`).
/// Lines with a value after the colon (keys like `PHY Mode: 802.11ac`) are
/// ignored, as are other interfaces' `Current Network Information:` blocks
/// (their first line is `Network Type: Infrastructure`, never a bare name).
/// No Wi-Fi section, no header, or no name line → `None`.
pub fn parse_current_network(profile: &str) -> Option<String> {
    let mut in_wifi = false;
    let mut after_header = false;
    for line in profile.lines() {
        let trimmed = line.trim();
        if !in_wifi {
            if trimmed == "Wi-Fi:" {
                in_wifi = true;
            }
            continue;
        }
        // A new top-level section (column 0) closes the Wi-Fi section.
        if !line.is_empty() && !line.starts_with(' ') && !line.starts_with('\t') {
            break;
        }
        if trimmed == "Current Network Information:" {
            after_header = true;
            continue;
        }
        if after_header {
            if let Some(name) = trimmed.strip_suffix(':') {
                if !name.is_empty() {
                    return Some(name.to_string());
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

    /// Realistic `system_profiler SPAirPortDataType` excerpt: Wi-Fi section,
    /// en0 connected to `ZTE-A7UPhK`.
    const WIFI_CONNECTED: &str = "\
Wi-Fi:

      Software Versions:
          CoreWLAN: 16.0 (1657)
          CoreWLANKit: 16.0 (1657)
      Interfaces:
        en0:
          Card Type: Wi-Fi  (0x14E4, 0x173)
          MAC Address: 8c:85:90:22:16:fc
          Status: Connected
          Current Network Information:
            ZTE-A7UPhK:
              PHY Mode: 802.11ac
              Channel: 48 (5GHz, 80MHz)
              Network Type: Infrastructure
              Security: WPA2 Personal
              Signal / Noise: -42 dBm / -93 dBm
";

    #[test]
    fn parses_ssid_when_associated() {
        assert_eq!(
            parse_current_network(WIFI_CONNECTED),
            Some("ZTE-A7UPhK".to_string())
        );
    }

    #[test]
    fn no_ssid_when_disassociated() {
        // No Wi-Fi section at all.
        assert_eq!(parse_current_network("Ethernet:\n  Link Speed: 1000\n"), None);
        // Wi-Fi section present but no Current Network Information header.
        assert_eq!(parse_current_network("Wi-Fi:\n      Interfaces:\n        en0:\n          Status: Off\n"), None);
        // Header present but no name line under it (awdl0-style block).
        assert_eq!(
            parse_current_network(
                "Wi-Fi:\n        awdl0:\n          Current Network Information:\n              Network Type: Infrastructure\n"
            ),
            None
        );
        // Empty output.
        assert_eq!(parse_current_network(""), None);
    }

    #[test]
    fn current_ssid_invokes_system_profiler() {
        let fake = FakeRunner::with_stdout(WIFI_CONNECTED);
        assert_eq!(current_ssid(&fake), Some("ZTE-A7UPhK".to_string()));
        let calls = fake.calls.lock().expect("calls lock");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "system_profiler");
        assert_eq!(calls[0].1, vec!["SPAirPortDataType"]);
    }

    #[test]
    fn second_current_network_block_does_not_override_wifi_name() {
        // Second `Current Network Information:` block (awdl0-style: first line
        // `Network Type: Infrastructure`) plus BSSID/MAC-address-style lines —
        // none of which may be parsed as a name or a MAC.
        let profile = "\
Wi-Fi:
      Interfaces:
        en0:
          MAC Address: 8c:85:90:22:16:fc
          BSSID: 8c:85:90:22:16:fc
          Current Network Information:
            ZTE-A7UPhK:
              PHY Mode: 802.11ac
              Network Type: Infrastructure
        awdl0:
          MAC Address: 5a:b6:b0:a4:4a:78
          Current Network Information:
              Network Type: Infrastructure
              Security: None
";
        assert_eq!(parse_current_network(profile), Some("ZTE-A7UPhK".to_string()));
        // A MAC-looking value after a colon is never taken as the name.
        let mac_only = "\
Wi-Fi:
        en0:
          BSSID: 8c:85:90:22:16:fc
          Current Network Information:
              BSSID: 8c:85:90:22:16:fc
";
        assert_eq!(parse_current_network(mac_only), None);
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

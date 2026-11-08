//! Remote-session detector: `UURemoteServer` throughput sampling with hysteresis.

use std::time::Instant;

use crate::cmd::CommandRunner;

/// Calibration (live measurement): active ≈296 KB/s, idle ≈2.7 KB/s —
/// threshold sits far above idle, far below active.
const THRESHOLD_BPS: f64 = 50_000.0;
/// Consecutive high samples required to enter the active state.
const ENTER_SAMPLES: u32 = 2;
/// Consecutive low samples required to leave the active state.
const EXIT_SAMPLES: u32 = 12;

/// Parse one `nettop -p <pid> -P -x -l 1` capture: return the process row's
/// `bytes_in + bytes_out`, or `None` if no row is present (header-only,
/// malformed, or empty output).
///
/// A process row is four whitespace-separated tokens: timestamp,
/// `Name.PID`, bytes-in, bytes-out (trailing columns are ignored). Header
/// lines and anything without that shape are skipped.
pub fn parse_nettop_process_total(stdout: &str) -> Option<u64> {
    for line in stdout.lines() {
        let mut tokens = line.split_whitespace();
        let (Some(_ts), Some(name_pid), Some(bytes_in), Some(bytes_out)) =
            (tokens.next(), tokens.next(), tokens.next(), tokens.next())
        else {
            continue;
        };
        if !name_pid.contains('.') {
            continue;
        }
        let (Ok(bin), Ok(bout)) = (bytes_in.parse::<u64>(), bytes_out.parse::<u64>()) else {
            continue;
        };
        return Some(bin.saturating_add(bout));
    }
    None
}

/// Decides whether someone is remote-controlling the Mac right now by
/// watching `UURemoteServer` network throughput between `nettop` samples.
pub struct SessionDetector {
    threshold_bps: f64,
    enter_left: u32,
    exit_left: u32,
    active: bool,
    last_bytes: Option<u64>,
    last_t: Option<Instant>,
}

impl SessionDetector {
    /// Threshold 50_000.0 B/s; enter after 2 consecutive high samples,
    /// exit after 12 consecutive low ones.
    pub fn new() -> Self {
        Self {
            threshold_bps: THRESHOLD_BPS,
            enter_left: ENTER_SAMPLES,
            exit_left: EXIT_SAMPLES,
            active: false,
            last_bytes: None,
            last_t: None,
        }
    }

    /// Feed one cumulative byte total taken at `now`; returns whether the
    /// session is active after this sample.
    ///
    /// The first sample only primes the baseline. Afterwards
    /// `bps = Δbytes / Δsecs`: at or above the threshold the enter counter
    /// decrements (flip active at 0) and the exit counter resets to 12;
    /// below the threshold the exit counter decrements (flip inactive at 0)
    /// and the enter counter resets to 2. Non-positive elapsed time is not
    /// a measurable interval and only refreshes the baseline.
    pub fn observe(&mut self, total_bytes: u64, now: Instant) -> bool {
        match (self.last_bytes, self.last_t) {
            (Some(prev_bytes), Some(prev_t)) if now > prev_t => {
                let secs = now.duration_since(prev_t).as_secs_f64();
                let bps = total_bytes.saturating_sub(prev_bytes) as f64 / secs;
                self.last_bytes = Some(total_bytes);
                self.last_t = Some(now);
                if bps >= self.threshold_bps {
                    self.enter_left = self.enter_left.saturating_sub(1);
                    if self.enter_left == 0 {
                        self.active = true;
                    }
                    self.exit_left = EXIT_SAMPLES;
                } else {
                    self.exit_left = self.exit_left.saturating_sub(1);
                    if self.exit_left == 0 {
                        self.active = false;
                    }
                    self.enter_left = ENTER_SAMPLES;
                }
                self.active
            }
            _ => {
                self.last_bytes = Some(total_bytes);
                self.last_t = Some(now);
                self.active
            }
        }
    }
}

/// Run `nettop -p <pid> -P -x -l 1` through `runner` and parse the process
/// row's total bytes; `None` on spawn error or parse miss.
pub fn sample_now(runner: &impl CommandRunner, pid: u32) -> Option<u64> {
    let pid = pid.to_string();
    let output = runner
        .run("nettop", &["-p", &pid, "-P", "-x", "-l", "1"])
        .ok()?;
    parse_nettop_process_total(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::FakeRunner;
    use std::time::{Duration, Instant};

    #[test]
    fn parses_process_row_summing_in_out() {
        let row = "11:17:41.6 UURemoteServer.91747 763716 31148838 6692 0 268";
        assert_eq!(parse_nettop_process_total(row), Some(31_912_554));
    }

    #[test]
    fn header_returns_none() {
        let header = "time                 bytes_in       bytes_out      packets_in     packets_out";
        assert_eq!(parse_nettop_process_total(header), None);
    }

    #[test]
    fn two_high_samples_activate() {
        // Calibrated active rate ≈ 296 KB/s → 1_480_000 B per 5 s sample.
        let mut d = SessionDetector::new();
        let t = Instant::now();
        assert!(!d.observe(0, t), "first sample primes only");
        assert!(
            !d.observe(1_480_000, t + Duration::from_secs(5)),
            "enter needs 2 high samples"
        );
        assert!(
            d.observe(2_960_000, t + Duration::from_secs(10)),
            "second high sample must activate"
        );
    }

    #[test]
    fn single_high_sample_stays_idle() {
        let mut d = SessionDetector::new();
        let t = Instant::now();
        assert!(!d.observe(0, t), "first sample primes only");
        assert!(
            !d.observe(1_480_000, t + Duration::from_secs(5)),
            "enter needs 2 high samples"
        );
    }

    #[test]
    fn static_screen_dip_keeps_active() {
        let mut d = SessionDetector::new();
        let t = Instant::now();
        // Activate: prime + two high samples at ≈ 296_000 B/s.
        d.observe(0, t);
        d.observe(1_480_000, t + Duration::from_secs(5));
        assert!(d.observe(2_960_000, t + Duration::from_secs(10)));

        // Static-screen dip: low samples at ≈ 5_000 B/s → 25_000 B each.
        let mut bytes = 2_960_000;
        let mut at = t + Duration::from_secs(10);
        for i in 1..=12 {
            bytes += 25_000;
            at += Duration::from_secs(5);
            let active = d.observe(bytes, at);
            if i < 12 {
                assert!(active, "still active after low sample {i} of 12 (exit needs 12)");
            } else {
                assert!(!active, "12th low sample must go idle");
            }
        }
    }

    #[test]
    fn idle_samples_never_activate() {
        // Calibrated idle rate ≈ 2.7 KB/s → 13_500 B per 5 s sample.
        let mut d = SessionDetector::new();
        let t = Instant::now();
        assert!(!d.observe(0, t), "first sample primes only");
        let mut bytes = 0;
        for i in 1..=20 {
            bytes += 13_500;
            let active = d.observe(bytes, t + Duration::from_secs(5 * i));
            assert!(!active, "idle traffic must never activate (sample {i})");
        }
    }

    #[test]
    fn sample_now_runs_nettop_and_parses_total() {
        let stdout = "time                 bytes_in       bytes_out      packets_in     packets_out\n\
                      11:17:41.6 UURemoteServer.91747 763716 31148838 6692 0 268\n";
        let fake = FakeRunner::with_stdout(stdout);
        let total = sample_now(&fake, 91_747).expect("header + process row should parse");
        assert_eq!(total, 31_912_554);

        let calls = fake.calls.lock().expect("calls lock");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "nettop");
        assert_eq!(calls[0].1, vec!["-p", "91747", "-P", "-x", "-l", "1"]);
    }

    #[test]
    fn sample_now_returns_none_on_parse_miss() {
        let fake = FakeRunner::with_stdout("no process row here\n");
        assert_eq!(sample_now(&fake, 91_747), None);
    }
}

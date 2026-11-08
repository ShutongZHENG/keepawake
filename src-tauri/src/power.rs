//! `pmset` power settings: parsing `pmset -g` output and live queries.

use crate::cmd::CommandRunner;

/// Subset of system power settings KeepAwake cares about.
pub struct PowerSettings {
    pub sleep_disabled: bool,
    pub autorestart: bool,
}

/// Parse `pmset -g` output into [`PowerSettings`].
///
/// `SleepDisabled 1` → `sleep_disabled: true`; `autorestart 1` → `autorestart: true`;
/// absent keys (or value `0`) stay `false`.
pub fn parse_pmset_g(text: &str) -> PowerSettings {
    let mut settings = PowerSettings {
        sleep_disabled: false,
        autorestart: false,
    };
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        match (parts.next(), parts.next()) {
            (Some("SleepDisabled"), Some(value)) => settings.sleep_disabled = value == "1",
            (Some("autorestart"), Some(value)) => settings.autorestart = value == "1",
            _ => {}
        }
    }
    settings
}

/// Run `pmset -g` through `runner` and parse the captured stdout.
pub fn current_settings(runner: &impl CommandRunner) -> std::io::Result<PowerSettings> {
    let output = runner.run("pmset", &["-g"])?;
    Ok(parse_pmset_g(&String::from_utf8_lossy(&output.stdout)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test] fn parses_sleep_disabled_and_autorestart() {
        let text = "System-wide power settings:\n SleepDisabled\t\t1\n autorestart\t\t1\n";
        let s = parse_pmset_g(text);
        assert!(s.sleep_disabled); assert!(s.autorestart);
    }
    #[test] fn defaults_false_when_absent() {
        let s = parse_pmset_g(" SleepDisabled\t\t0\n");
        assert!(!s.sleep_disabled); assert!(!s.autorestart);
    }
}

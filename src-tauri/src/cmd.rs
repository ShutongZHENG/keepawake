//! Command execution abstraction: [`CommandRunner`] plus system/test runners.

/// Runs an external program and returns its [`std::process::Output`].
pub trait CommandRunner {
    fn run(&self, program: &str, args: &[&str]) -> std::io::Result<std::process::Output>;
}

/// Production runner backed by [`std::process::Command`].
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&self, program: &str, args: &[&str]) -> std::io::Result<std::process::Output> {
        std::process::Command::new(program).args(args).output()
    }
}

/// Test double that records `(program, args)` calls and returns canned output.
#[cfg(test)]
pub struct FakeRunner {
    pub calls: std::sync::Mutex<Vec<(String, Vec<String>)>>,
    pub stdout: Vec<u8>,
}

#[cfg(test)]
impl FakeRunner {
    pub fn with_stdout(stdout: impl Into<Vec<u8>>) -> Self {
        Self {
            calls: std::sync::Mutex::new(Vec::new()),
            stdout: stdout.into(),
        }
    }
}

#[cfg(test)]
impl CommandRunner for FakeRunner {
    fn run(&self, program: &str, args: &[&str]) -> std::io::Result<std::process::Output> {
        self.calls.lock().expect("FakeRunner calls lock").push((
            program.to_string(),
            args.iter().map(|a| a.to_string()).collect(),
        ));
        use std::os::unix::process::ExitStatusExt;
        Ok(std::process::Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: self.stdout.clone(),
            stderr: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::power::current_settings;

    #[test]
    fn current_settings_invokes_pmset_g() {
        let fake = FakeRunner::with_stdout("SleepDisabled\t\t1\n");
        let s = current_settings(&fake).expect("fake runner should succeed");
        assert!(s.sleep_disabled);
        assert!(!s.autorestart);

        let calls = fake.calls.lock().expect("calls lock");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "pmset");
        assert_eq!(calls[0].1, vec!["-g".to_string()]);
    }
}

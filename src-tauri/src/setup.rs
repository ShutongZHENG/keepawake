//! First-run setup: LaunchAgent lifecycle (Task 7 stub; full installer in
//! Task 8). Quit must `launchctl bootout` the agent before exiting, or the
//! `KeepAlive` LaunchAgent relaunches the app forever (plan, Ruling 2).

use crate::cmd::CommandRunner;

/// Commands that unload the KeepAwake LaunchAgent for the given user, in
/// execution order: exactly one `launchctl bootout gui/<uid>/com.keepawake.app`.
///
/// The uid is a parameter so tests can pin the literal target string; the
/// real quit path passes `libc::getuid()` (no `users` crate).
pub fn quit_commands(uid: u32) -> Vec<(&'static str, Vec<String>)> {
    vec![(
        "launchctl",
        vec![
            "bootout".to_string(),
            format!("gui/{uid}/com.keepawake.app"),
        ],
    )]
}

/// Run [`quit_commands`] through `runner` to unload the LaunchAgent before
/// the app exits.
///
/// Returns the first error (spawn failure or non-zero exit). The quit menu
/// item ignores it: the agent may not exist yet (first run), and quitting
/// must always work (Ruling 2).
pub fn unload_launch_agent(runner: &impl CommandRunner) -> std::io::Result<()> {
    // SAFETY: `getuid(2)` has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    for (program, args) in quit_commands(uid) {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = runner.run(program, &refs)?;
        if !output.status.success() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("{program} {} exited with {}", refs.join(" "), output.status),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::FakeRunner;

    /// Mandated by plan (Task 7 / Ruling 2): the quit command list is
    /// exactly one `launchctl bootout gui/<uid>/com.keepawake.app` — uid is
    /// passed in for testability, so the assertion pins the literal string.
    #[test]
    fn quit_command_unloads_agent_first() {
        let cmds = quit_commands(501);
        assert_eq!(
            cmds,
            vec![(
                "launchctl",
                vec![
                    "bootout".to_string(),
                    "gui/501/com.keepawake.app".to_string()
                ]
            )]
        );
    }

    /// The real unload path must run the quit commands through the runner
    /// with the live uid.
    #[test]
    fn unload_launch_agent_runs_bootout_via_runner() {
        let fake = FakeRunner::with_stdout("");
        unload_launch_agent(&fake).expect("bootout should succeed");

        let calls = fake.calls.lock().expect("FakeRunner calls lock");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "launchctl");
        assert_eq!(
            calls[0].1,
            vec![
                "bootout".to_string(),
                format!("gui/{}/com.keepawake.app", unsafe { libc::getuid() })
            ]
        );
    }

    /// A failing bootout (e.g. agent not installed yet) surfaces as `Err` —
    /// the quit handler chooses to ignore it and exit anyway (Ruling 2).
    #[test]
    fn unload_launch_agent_propagates_bootout_failure() {
        let fake = FakeRunner::with_stdout("").failing_with(1, "Could not find job");
        let err = unload_launch_agent(&fake).expect_err("non-zero bootout must be an error");
        assert!(err.to_string().contains("bootout"));
    }
}

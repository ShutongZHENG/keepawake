//! First-run setup: LaunchAgent lifecycle and sudo `pmset` prompts.
//!
//! Quit must `launchctl bootout` the agent before exiting, or the
//! `KeepAlive` LaunchAgent relaunches the app forever (plan, Ruling 2).
//! At startup the app installs that LaunchAgent (best-effort) and, while
//! `pmset -g` still shows system sleep enabled, reveals the tray's
//! `act_setup` row so the user can approve the two admin `pmset` writes.

use std::path::{Path, PathBuf};

use crate::cmd::CommandRunner;

/// LaunchAgent label — also the plist file name base and the
/// `launchctl` target (`gui/<uid>/<label>`).
pub const AGENT_LABEL: &str = "com.keepawake.app";

/// Literal plist path with a home-directory placeholder, for reference
/// only: `~` is not a real directory for [`Path`], so the installer
/// expands the home dir at runtime (see `install_launch_agent`).
pub const PLIST_PATH: &str = "~/Library/LaunchAgents/com.keepawake.app.plist";

/// The only argv tails [`request_pmset`] may turn into a shell command.
/// Each becomes `pmset <args>` inside an AppleScript `do shell script`
/// run with administrator privileges; a fixed list is the entire
/// escaping strategy — arbitrary strings never reach the shell.
const ALLOWED_PMSET: [&[&str]; 2] = [
    &["-a", "disablesleep", "1"],
    &["-a", "autorestart", "1"],
];

/// Commands that unload the KeepAwake LaunchAgent for the given user, in
/// execution order: exactly one `launchctl bootout gui/<uid>/<label>`.
///
/// The uid is a parameter so tests can pin the literal target string; the
/// real quit path passes `libc::getuid()` (no `users` crate).
pub fn quit_commands(uid: u32) -> Vec<(&'static str, Vec<String>)> {
    vec![(
        "launchctl",
        vec![
            "bootout".to_string(),
            format!("gui/{uid}/{AGENT_LABEL}"),
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

/// XML plist for the KeepAwake LaunchAgent: the label, the executable as
/// the sole `ProgramArguments` entry, and `RunAtLoad`/`KeepAlive` both
/// true so launchd starts the app at login and relaunches it if it dies.
pub fn plist_content(exe_path: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{AGENT_LABEL}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{exe_path}</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
</dict>
</plist>
"#
    )
}

/// Runtime expansion of [`PLIST_PATH`] under `home`
/// (`<home>/Library/LaunchAgents/<label>.plist`).
fn plist_path(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents")
        .join(format!("{AGENT_LABEL}.plist"))
}

/// The user's home directory from `$HOME` (kept dependency-free; no
/// `dirs` crate just for one lookup).
fn home_dir() -> std::io::Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "HOME is not set"))
}

/// Write the LaunchAgent plist and `launchctl bootstrap` it with the real
/// home directory and uid — see [`install_launch_agent_at`] for the
/// testable core.
pub fn install_launch_agent(runner: &impl CommandRunner) -> std::io::Result<()> {
    let home = home_dir()?;
    // SAFETY: `getuid(2)` has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    install_launch_agent_at(runner, &home, uid)
}

/// Write `plist_content(current_exe)` to `<home>/Library/LaunchAgents/`
/// (creating the directory) and run
/// `launchctl bootstrap gui/<uid>/<label> <path>` through `runner`.
///
/// A non-zero bootstrap whose stderr mentions "already loaded" or the
/// error-37 phrasings ("error: 37", "failed: 37", …) is `Ok` — first run
/// may race an earlier install. Any other failure surfaces as `Err`
/// (the `lib.rs` call site is the best-effort policy, not this
/// function).
///
/// `home` and `uid` are parameters so tests never touch the real
/// `$HOME` or bootstrap anything on the host.
pub fn install_launch_agent_at(
    runner: &impl CommandRunner,
    home: &Path,
    uid: u32,
) -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    let path = plist_path(home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, plist_content(&exe.to_string_lossy()))?;

    let target = format!("gui/{uid}/{AGENT_LABEL}");
    let path_str = path.to_string_lossy().into_owned();
    let output = runner.run("launchctl", &["bootstrap", &target, &path_str])?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let lower = stderr.to_lowercase();
    // Already-loaded class: "service already loaded", "error: 37",
    // "Bootstrap failed: 37: …" — all mean the agent is registered.
    if lower.contains("already loaded")
        || lower.contains("error: 37")
        || lower.contains("error 37")
        || lower.contains("failed: 37")
    {
        return Ok(());
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::Other,
        format!(
            "launchctl bootstrap failed with {}: {}",
            output.status,
            stderr.trim()
        ),
    ))
}

/// Run `pmset <args>` through `osascript` with administrator privileges
/// (the macOS sudo dialog), but only for the exact vectors on
/// `ALLOWED_PMSET`: `["-a", "disablesleep", "1"]` and
/// `["-a", "autorestart", "1"]`. Anything else — arbitrary strings,
/// near-misses — is rejected with [`std::io::ErrorKind::InvalidInput`]
/// before the runner is touched.
///
/// The AppleScript is built by joining the already-validated args with
/// spaces, so no shell escaping is needed: the allow-list *is* the
/// escaping. A non-zero `osascript` exit (user cancelled the dialog,
/// `pmset` failed) becomes `Err` with the captured stderr.
///
/// The `ALLOWED_PMSET` const above is the complete allow-list.
pub fn request_pmset(
    runner: &impl CommandRunner,
    args: &[&str],
) -> std::io::Result<std::process::Output> {
    if !ALLOWED_PMSET.iter().any(|allowed| *allowed == args) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("pmset args not on the allow-list: {}", args.join(" ")),
        ));
    }
    let script = format!(
        "do shell script \"pmset {}\" with administrator privileges",
        args.join(" ")
    );
    let output = runner.run("osascript", &["-e", &script])?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        let message = if stderr.is_empty() {
            format!("osascript pmset failed with {}", output.status)
        } else {
            format!("osascript pmset failed with {}: {stderr}", output.status)
        };
        return Err(std::io::Error::new(std::io::ErrorKind::Other, message));
    }
    Ok(output)
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

    // ---- Task 8: first-run setup ----

    /// The public `PLIST_PATH` literal stays in step with `AGENT_LABEL`
    /// (both are the same file name the installer writes).
    #[test]
    fn plist_path_literal_is_label_based() {
        assert_eq!(
            PLIST_PATH,
            format!("~/Library/LaunchAgents/{AGENT_LABEL}.plist")
        );
    }

    /// (a) The generated plist carries `Label`, the program path inside
    /// `ProgramArguments`, and both `RunAtLoad`/`KeepAlive` true flags —
    /// launchd needs all of them to start and relaunch the app.
    #[test]
    fn plist_content_has_label_program_and_keepalive_flags() {
        let xml = plist_content("/path/app");
        assert!(xml.contains("<key>Label</key>"), "missing Label key:\n{xml}");
        assert!(xml.contains(AGENT_LABEL), "missing label value:\n{xml}");
        assert!(
            xml.contains("<key>ProgramArguments</key>"),
            "missing ProgramArguments:\n{xml}"
        );
        assert!(
            xml.contains("<string>/path/app</string>"),
            "missing exe path:\n{xml}"
        );
        assert!(xml.contains("<key>RunAtLoad</key>"), "missing RunAtLoad:\n{xml}");
        assert!(xml.contains("<key>KeepAlive</key>"), "missing KeepAlive:\n{xml}");
        assert_eq!(
            xml.matches("<true/>").count(),
            2,
            "both flags must be <true/>:\n{xml}"
        );
    }

    /// (b) Allow-list: the two exact pmset vectors become
    /// `osascript -e 'do shell script "pmset …" with administrator
    /// privileges'` — nothing else may reach the runner.
    #[test]
    fn request_pmset_allows_only_the_two_power_vectors() {
        let fake = FakeRunner::with_stdout("");
        request_pmset(&fake, &["-a", "disablesleep", "1"]).expect("disablesleep is allow-listed");
        request_pmset(&fake, &["-a", "autorestart", "1"]).expect("autorestart is allow-listed");

        let calls = fake.calls.lock().expect("FakeRunner calls lock");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "osascript");
        assert_eq!(
            calls[0].1,
            vec![
                "-e".to_string(),
                "do shell script \"pmset -a disablesleep 1\" with administrator privileges"
                    .to_string()
            ]
        );
        assert_eq!(calls[1].0, "osascript");
        assert_eq!(
            calls[1].1,
            vec![
                "-e".to_string(),
                "do shell script \"pmset -a autorestart 1\" with administrator privileges"
                    .to_string()
            ]
        );
    }

    /// Rejection: arbitrary strings (shell-injection shaped) and
    /// near-misses are refused before the runner is touched.
    #[test]
    fn request_pmset_rejects_anything_off_the_allow_list() {
        let fake = FakeRunner::with_stdout("");
        let err = request_pmset(&fake, &["-a", "rm", "-rf", "/"])
            .expect_err("arbitrary args must be rejected");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        // Near-misses: wrong value, read-only query, empty argv.
        assert!(request_pmset(&fake, &["-a", "disablesleep", "0"]).is_err());
        assert!(request_pmset(&fake, &["-g"]).is_err());
        assert!(request_pmset(&fake, &[]).is_err());
        assert!(
            fake.calls.lock().expect("FakeRunner calls lock").is_empty(),
            "rejected args must never reach the runner"
        );
    }

    /// Unique scratch home per test — never the real `$HOME`.
    fn temp_home(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("keepawake-task8-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp home");
        dir
    }

    /// (c) Install writes the expanded plist (creating the LaunchAgents
    /// dir) and bootstraps `gui/<uid>/<label>` pointing at that path.
    #[test]
    fn install_launch_agent_writes_plist_and_bootstraps() {
        let home = temp_home("install");
        let fake = FakeRunner::with_stdout("");

        install_launch_agent_at(&fake, &home, 501).expect("install should succeed");

        let plist = home
            .join("Library/LaunchAgents")
            .join(format!("{AGENT_LABEL}.plist"));
        let written = std::fs::read_to_string(&plist).expect("plist must be written");
        let exe = std::env::current_exe().expect("current exe");
        assert_eq!(written, plist_content(&exe.to_string_lossy()));

        let calls = fake.calls.lock().expect("FakeRunner calls lock");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "launchctl");
        assert_eq!(
            calls[0].1,
            vec![
                "bootstrap".to_string(),
                "gui/501/com.keepawake.app".to_string(),
                plist.to_string_lossy().to_string()
            ]
        );
        drop(calls);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// (d) A bootstrap reporting "already loaded" / error 37 exits Ok —
    /// first run may race an earlier install.
    #[test]
    fn install_launch_agent_treats_already_loaded_bootstrap_as_ok() {
        let home = temp_home("already");
        let fake =
            FakeRunner::with_stdout("").failing_with(37, "Bootstrap failed: service already loaded\n");
        install_launch_agent_at(&fake, &home, 501).expect("already-loaded stderr must be Ok");
        let _ = std::fs::remove_dir_all(&home);

        let home = temp_home("error37");
        let fake = FakeRunner::with_stdout("").failing_with(1, "Bootstrap failed: error: 37\n");
        install_launch_agent_at(&fake, &home, 501).expect("error 37 must be Ok");
        let _ = std::fs::remove_dir_all(&home);

        // launchctl's own wording for an already-bootstrapped job.
        let home = temp_home("failed37");
        let fake = FakeRunner::with_stdout("")
            .failing_with(1, "Bootstrap failed: 37: Operation now in progress\n");
        install_launch_agent_at(&fake, &home, 501).expect("failed: 37 must be Ok");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Other bootstrap failures still surface as `Err` — only the
    /// already-loaded class is swallowed here; `lib.rs` decides that
    /// install is best-effort.
    #[test]
    fn install_launch_agent_propagates_other_bootstrap_failures() {
        let home = temp_home("fail");
        let fake =
            FakeRunner::with_stdout("").failing_with(1, "Bootstrap failed: 5: Input/output error");
        let err = install_launch_agent_at(&fake, &home, 501)
            .expect_err("unrelated bootstrap failures must be Err");
        assert!(err.to_string().contains("Input/output error"), "got: {err}");
        let _ = std::fs::remove_dir_all(&home);
    }
}

//! Starting automatically on macOS: a launchd job written for this install.
//!
//! Run as root, `--install-service` writes a LaunchDaemon, which starts at boot before anyone logs
//! in and runs as the account that owns the install. Run as that account, it writes a LaunchAgent
//! instead, which starts when that account logs in -- and, living in the login session, can show
//! the menu bar icon, which a daemon never can. Either way it is started straight away, and
//! running it again after moving the install rewrites it for the new location.
//!
//! Compiled on every platform so the job's rendering is tested everywhere; only macOS calls it.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const LABEL: &str = "io.github.wagwan-piffting-blud.eas-listener";
const DAEMON_DIR: &str = "/Library/LaunchDaemons";
/// launchd starts jobs with only the system's own directories on PATH. Homebrew's are added so
/// the ffmpeg and espeak-ng it installs are found the same way they are from a terminal.
const JOB_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// /Library/LaunchDaemons: at boot, before login.
    Daemon,
    /// ~/Library/LaunchAgents: at the owner's login, in their session.
    Agent,
}

pub fn daemon_path() -> PathBuf {
    Path::new(DAEMON_DIR).join(format!("{LABEL}.plist"))
}

pub fn agent_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| {
            PathBuf::from(home)
                .join("Library/LaunchAgents")
                .join(format!("{LABEL}.plist"))
        })
}

/// Which job is installed, if either is. A root shell's HOME is root's, so only the daemon is
/// visible from there.
pub fn installed() -> Option<Kind> {
    if daemon_path().is_file() {
        Some(Kind::Daemon)
    } else if agent_path().is_some_and(|path| path.is_file()) {
        Some(Kind::Agent)
    } else {
        None
    }
}

/// What root's `--install-service` writes. As anyone else, it is a LaunchAgent.
pub fn kind_for_this_process() -> Kind {
    if crate::paths::is_root() {
        Kind::Daemon
    } else {
        Kind::Agent
    }
}

/// `launchctl managername` names the session this process belongs to: `Aqua` is a logged-in
/// desktop; a daemon's is `System`, an SSH login's `Background`.
pub fn in_desktop_session() -> bool {
    Command::new("launchctl")
        .arg("managername")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .is_some_and(|output| String::from_utf8_lossy(&output.stdout).trim() == "Aqua")
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn string(value: &str) -> String {
    format!("<string>{}</string>", xml_escape(value))
}

/// `user` is the account a daemon runs as; an agent always runs as whoever logs in.
pub fn render_plist(exe: &Path, app_root: &Path, kind: Kind, user: Option<&str>) -> String {
    let root = app_root.display().to_string();
    let log = format!("{}/launchd.log", root.trim_end_matches('/'));
    let user_entry = match (kind, user) {
        (Kind::Daemon, Some(name)) => {
            format!("    <key>UserName</key>\n    {}\n", string(name))
        }
        _ => String::new(),
    };
    let what = match kind {
        Kind::Daemon => "LaunchDaemon: starts at boot",
        Kind::Agent => "LaunchAgent: starts at login",
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <!-- Written by eas_listener's install-service option for the install in {root_comment}\n\
         \x20    ({what}). Run it again after moving the install; uninstall-service removes this job. -->\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \x20   <key>Label</key>\n\
         \x20   {label}\n\
         \x20   <key>ProgramArguments</key>\n\
         \x20   <array>\n\
         \x20       {exe}\n\
         \x20       <string>--service</string>\n\
         \x20       <string>--app-root</string>\n\
         \x20       {root}\n\
         \x20   </array>\n\
         \x20   <key>WorkingDirectory</key>\n\
         \x20   {root}\n\
         {user_entry}\
         \x20   <key>EnvironmentVariables</key>\n\
         \x20   <dict>\n\
         \x20       <key>PATH</key>\n\
         \x20       {path}\n\
         \x20   </dict>\n\
         \x20   <key>RunAtLoad</key>\n\
         \x20   <true/>\n\
         \x20   <!-- Restarted when it fails; a clean exit, such as Quit from the menu bar, is left alone. -->\n\
         \x20   <key>KeepAlive</key>\n\
         \x20   <dict>\n\
         \x20       <key>SuccessfulExit</key>\n\
         \x20       <false/>\n\
         \x20   </dict>\n\
         \x20   <key>ThrottleInterval</key>\n\
         \x20   <integer>10</integer>\n\
         \x20   <key>StandardOutPath</key>\n\
         \x20   {log}\n\
         \x20   <key>StandardErrorPath</key>\n\
         \x20   {log}\n\
         </dict>\n\
         </plist>\n",
        // A comment cannot contain "--", which a path can.
        root_comment = xml_escape(&root).replace("--", "- -"),
        label = string(LABEL),
        exe = string(&exe.display().to_string()),
        root = string(&root),
        path = string(JOB_PATH),
        log = string(&log),
    )
}

fn launchctl(args: &[&str]) -> Result<()> {
    let output = Command::new("launchctl")
        .args(args)
        .output()
        .context("Failed to run launchctl")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        bail!(
            "launchctl {} failed: {}",
            args.join(" "),
            if stderr.is_empty() { stdout } else { stderr }
        );
    }
    Ok(())
}

fn domain(kind: Kind) -> String {
    match kind {
        Kind::Daemon => "system".to_string(),
        // SAFETY: getuid has no preconditions and cannot fail.
        #[cfg(unix)]
        Kind::Agent => format!("gui/{}", unsafe { libc::getuid() }),
        #[cfg(not(unix))]
        Kind::Agent => "gui".to_string(),
    }
}

fn job_path(kind: Kind) -> Result<PathBuf> {
    match kind {
        Kind::Daemon => Ok(daemon_path()),
        Kind::Agent => agent_path().context("HOME is not set, so there is no LaunchAgents folder"),
    }
}

fn exe_display() -> String {
    std::env::current_exe()
        .map(|exe| exe.display().to_string())
        .unwrap_or_else(|_| "eas_listener".to_string())
}

/// Writes the job for this process's kind, loads it and starts it.
pub fn install() -> Result<()> {
    let kind = kind_for_this_process();
    let exe = std::env::current_exe()
        .and_then(|exe| exe.canonicalize())
        .context("Could not determine this executable's path")?;
    let app_root = crate::paths::app_root()
        .canonicalize()
        .context("Could not resolve the application root")?;

    let owner = crate::systemd::install_owner(&app_root);
    let user = match kind {
        Kind::Daemon => owner.and_then(crate::paths::user_name),
        Kind::Agent => None,
    };
    if let (Kind::Daemon, Some(uid)) = (kind, owner) {
        crate::systemd::return_to_owner(&app_root, uid).with_context(|| {
            format!(
                "Could not give the files root created in {} back to UID {uid}",
                app_root.display()
            )
        })?;
    }

    let path = job_path(kind)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Could not create {}", parent.display()))?;
    }
    std::fs::write(&path, render_plist(&exe, &app_root, kind, user.as_deref()))
        .with_context(|| format!("Could not write {}", path.display()))?;

    let domain = domain(kind);
    let target = format!("{domain}/{LABEL}");
    // A job already loaded from an earlier install has to go before the new one can load.
    let _ = launchctl(&["bootout", &target]);
    let path_text = path.display().to_string();
    let started = launchctl(&["enable", &target])
        .and_then(|()| launchctl(&["bootstrap", &domain, &path_text]));

    let (what, when) = match kind {
        Kind::Daemon => ("LaunchDaemon", "at every boot"),
        Kind::Agent => ("LaunchAgent", "every time you log in"),
    };
    match &started {
        Ok(()) => println!("Installed and started the {what}; it starts {when} from now on."),
        // An agent can only be started inside a logged-in desktop session; from SSH it waits
        // for the next login instead.
        Err(err) if kind == Kind::Agent => {
            println!("Installed the {what}; it starts {when}. It could not start now: {err:#}")
        }
        Err(_) => {}
    }
    started?;

    println!("  Job:              {}", path.display());
    println!("  Executable:       {}", exe.display());
    println!("  Application root: {}", app_root.display());
    if kind == Kind::Daemon {
        match &user {
            Some(name) => {
                println!("  Runs as:          {name} (the owner of the application root)")
            }
            None => println!("  Runs as:          root (the application root is owned by root)"),
        }
    }
    println!();
    println!(
        "Output:          {}",
        app_root.join("launchd.log").display()
    );
    let sudo = if kind == Kind::Daemon { "sudo " } else { "" };
    println!(
        "Remove it with:  {sudo}\"{}\" --uninstall-service",
        exe.display()
    );
    if kind == Kind::Agent {
        println!(
            "To start at boot instead, before anyone logs in: sudo \"{}\" --install-service",
            exe.display()
        );
    }
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let kind = match installed() {
        Some(kind) => kind,
        None if crate::paths::is_root() => bail!(
            "{} does not exist, so there is nothing to remove. A LaunchAgent is removed by \
             running this as the account that installed it, without sudo.",
            daemon_path().display()
        ),
        None => bail!(
            "No LaunchAgent is installed for this account. A LaunchDaemon is removed with: \
             sudo \"{}\" --uninstall-service",
            exe_display()
        ),
    };
    if kind == Kind::Daemon && !crate::paths::is_root() {
        bail!(
            "The LaunchDaemon needs root to remove. Run: sudo \"{}\" --uninstall-service",
            exe_display()
        );
    }
    // A job that is not loaded is not a reason to stop.
    if let Err(err) = launchctl(&["bootout", &format!("{}/{LABEL}", domain(kind))]) {
        eprintln!("Note: {err:#}");
    }
    let path = job_path(kind)?;
    std::fs::remove_file(&path).with_context(|| format!("Could not remove {}", path.display()))?;
    println!("Removed {}.", path.display());
    Ok(())
}

pub fn status() -> Result<()> {
    let Some(kind) = installed() else {
        println!("No launchd job is installed for EAS Listener.");
        return Ok(());
    };
    println!("{}", job_path(kind)?.display());
    // `print` exits non-zero for a job that is not loaded, which is an answer, not a failure.
    let output = Command::new("launchctl")
        .args(["print", &format!("{}/{LABEL}", domain(kind))])
        .output()
        .context("Failed to run launchctl")?;
    let text = String::from_utf8_lossy(&output.stdout);
    let wanted = ["state =", "pid =", "last exit code =", "runs ="];
    // Nested sections repeat some keys; the job's own come first.
    let mut seen = Vec::new();
    let summary: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(
            |line| match wanted.iter().find(|key| line.starts_with(**key)) {
                Some(key) if !seen.contains(key) => {
                    seen.push(*key);
                    true
                }
                _ => false,
            },
        )
        .collect();
    if summary.is_empty() {
        println!(
            "Not loaded: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    } else {
        for line in summary {
            println!("  {line}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_daemon_runs_this_install_as_its_owner() {
        let plist = render_plist(
            Path::new("/Users/wags/EAS Listener/eas_listener"),
            Path::new("/Users/wags/EAS Listener"),
            Kind::Daemon,
            Some("wags"),
        );
        assert!(plist.contains("<string>/Users/wags/EAS Listener/eas_listener</string>"));
        assert!(plist.contains(
            "<string>--service</string>\n        <string>--app-root</string>\n        <string>/Users/wags/EAS Listener</string>"
        ));
        assert!(plist.contains("<key>UserName</key>\n    <string>wags</string>"));
        assert!(plist.contains("<string>/opt/homebrew/bin:"));
        assert!(plist.contains("<string>/Users/wags/EAS Listener/launchd.log</string>"));
        assert!(plist.contains("<key>SuccessfulExit</key>\n        <false/>"));
    }

    #[test]
    fn an_agent_never_names_a_user() {
        let plist = render_plist(
            Path::new("/Applications/eas/eas_listener"),
            Path::new("/Applications/eas"),
            Kind::Agent,
            Some("wags"),
        );
        assert!(!plist.contains("UserName"));
        assert!(plist.contains("LaunchAgent: starts at login"));
    }

    #[test]
    fn paths_cannot_break_out_of_the_plist() {
        let plist = render_plist(
            Path::new("/srv/a&b <x> --odd/eas_listener"),
            Path::new("/srv/a&b <x> --odd"),
            Kind::Daemon,
            None,
        );
        assert!(plist.contains("<string>/srv/a&amp;b &lt;x&gt; --odd/eas_listener</string>"));
        assert!(!plist.contains("<x>"));
        for comment in plist.split("<!--").skip(1) {
            let body = comment.split("-->").next().unwrap_or_default();
            assert!(!body.contains("--"), "{body}");
        }
    }
}

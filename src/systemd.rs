//! Starting at boot on Linux: a systemd unit written for this install.
//!
//! The unit is generated rather than shipped because everything in it depends on where the
//! listener was unpacked -- the executable, the application root, and the account that owns it.
//! `--install-service` writes it, enables it and starts it; running it again after moving the
//! install rewrites it for the new location.
//!
//! Compiled on every platform so the unit's rendering is tested everywhere; only Linux calls it.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const UNIT_NAME: &str = "eas-listener.service";
const UNIT_DIR: &str = "/etc/systemd/system";

pub fn unit_path() -> PathBuf {
    Path::new(UNIT_DIR).join(UNIT_NAME)
}

/// systemd is the init system, which is what `sd_booted()` checks for too.
pub fn systemd_is_running() -> bool {
    Path::new("/run/systemd/system").is_dir()
}

pub fn is_installed() -> bool {
    unit_path().is_file()
}

/// Started by systemd rather than by hand: systemd sets this for every process a unit starts.
pub fn started_by_systemd() -> bool {
    std::env::var_os("INVOCATION_ID").is_some()
}

pub use crate::paths::is_root;

/// The account that owns the install, which is who the unit runs as: it has to be able to write
/// config.json and the state folder, and there is no reason to run as root just to listen.
#[cfg(unix)]
pub(crate) fn install_owner(app_root: &Path) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(app_root)
        .ok()
        .map(|meta| meta.uid())
        .filter(|&uid| uid != 0)
}

#[cfg(not(unix))]
pub(crate) fn install_owner(_app_root: &Path) -> Option<u32> {
    None
}

/// Gives what root created inside the install back to its owner: `--install-service` and a
/// first-run setup answered "yes" both run as root, and whatever they wrote -- config.json first
/// of all -- would otherwise be read-only to the unit that runs as the owner. Symlinks are
/// neither followed nor changed, so nothing outside the install is touched.
#[cfg(unix)]
pub(crate) fn return_to_owner(dir: &Path, uid: u32) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.uid() == 0 {
            std::os::unix::fs::lchown(&path, Some(uid), None)?;
        }
        if meta.is_dir() {
            return_to_owner(&path, uid)?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn return_to_owner(_dir: &Path, _uid: u32) -> std::io::Result<()> {
    Ok(())
}

/// Quotes one ExecStart argument. `%` starts a unit specifier, so it is doubled.
fn exec_arg(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    format!("\"{escaped}\"")
}

/// A path setting such as WorkingDirectory=, which is taken whole, so only specifiers need care.
fn path_setting(value: &str) -> String {
    value.replace('%', "%%")
}

pub fn render_unit(exe: &Path, app_root: &Path, user: Option<u32>) -> String {
    let exe = exe.display().to_string();
    let root = app_root.display().to_string();
    let user_line = user.map(|uid| format!("User={uid}\n")).unwrap_or_default();
    format!(
        "# Written by `eas_listener --install-service` for the install in {root}.\n\
         # Run that again after moving the install; `--uninstall-service` removes this unit.\n\
         [Unit]\n\
         Description=EAS Listener\n\
         Documentation=https://github.com/wagwan-piffting-blud/EAS_Listener\n\
         Wants=network-online.target\n\
         After=network-online.target\n\
         # A configuration that cannot load would otherwise restart every ten seconds forever.\n\
         StartLimitIntervalSec=600\n\
         StartLimitBurst=5\n\
         \n\
         [Service]\n\
         Type=simple\n\
         {user_line}\
         WorkingDirectory={working}\n\
         ExecStart={exe_arg} --service --app-root {root_arg}\n\
         Restart=on-failure\n\
         RestartSec=10\n\
         # ffmpeg and the TTS engines are the listener's children; stopping the unit stops them.\n\
         KillMode=control-group\n\
         TimeoutStopSec=20\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        working = path_setting(&root),
        exe_arg = exec_arg(&exe),
        root_arg = exec_arg(&root),
    )
}

fn systemctl(args: &[&str]) -> Result<String> {
    let output = Command::new("systemctl")
        .args(args)
        .output()
        .context("Failed to run systemctl")?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !output.status.success() {
        let detail = if stderr.is_empty() { &stdout } else { &stderr };
        bail!("systemctl {} failed: {}", args.join(" "), detail);
    }
    Ok(stdout)
}

fn require_root(action: &str) -> Result<()> {
    if is_root() {
        return Ok(());
    }
    let exe = std::env::current_exe()
        .map(|exe| exe.display().to_string())
        .unwrap_or_else(|_| "eas_listener".to_string());
    bail!("{action} needs root. Run it with sudo: sudo \"{exe}\" --{action}")
}

/// Writes the unit, enables it for boot and starts it.
pub fn install() -> Result<()> {
    require_root("install-service")?;
    if !systemd_is_running() {
        bail!(
            "systemd is not running on this machine, so there is nothing to install a unit into."
        );
    }

    let exe = std::env::current_exe()
        .and_then(|exe| exe.canonicalize())
        .context("Could not determine this executable's path")?;
    let app_root = crate::paths::app_root()
        .canonicalize()
        .context("Could not resolve the application root")?;
    let user = install_owner(&app_root);
    if let Some(uid) = user {
        return_to_owner(&app_root, uid).with_context(|| {
            format!(
                "Could not give the files root created in {} back to UID {uid}",
                app_root.display()
            )
        })?;
    }

    let path = unit_path();
    std::fs::write(&path, render_unit(&exe, &app_root, user))
        .with_context(|| format!("Could not write {}", path.display()))?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", UNIT_NAME])?;

    println!("Installed and started {UNIT_NAME}; it starts at every boot from now on.");
    println!("  Unit:             {}", path.display());
    println!("  Executable:       {}", exe.display());
    println!("  Application root: {}", app_root.display());
    match user {
        Some(uid) => println!("  Runs as UID:      {uid} (the owner of the application root)"),
        None => println!("  Runs as:          root (the application root is owned by root)"),
    }
    println!();
    println!("Logs:            journalctl -u {UNIT_NAME} -f");
    println!(
        "Remove it with:  sudo \"{}\" --uninstall-service",
        exe.display()
    );
    Ok(())
}

pub fn uninstall() -> Result<()> {
    require_root("uninstall-service")?;
    if !is_installed() {
        bail!(
            "{} does not exist, so there is nothing to remove.",
            unit_path().display()
        );
    }
    // A unit that is not running is not a reason to stop.
    if let Err(err) = systemctl(&["disable", "--now", UNIT_NAME]) {
        eprintln!("Note: {err}");
    }
    std::fs::remove_file(unit_path())
        .with_context(|| format!("Could not remove {}", unit_path().display()))?;
    systemctl(&["daemon-reload"])?;
    println!("Removed {UNIT_NAME}.");
    Ok(())
}

pub fn status() -> Result<()> {
    // `status` exits non-zero for a stopped unit, which is an answer rather than a failure.
    let output = Command::new("systemctl")
        .args(["status", "--no-pager", UNIT_NAME])
        .output()
        .context("Failed to run systemctl")?;
    print!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unit_starts_this_install_as_its_owner() {
        let unit = render_unit(
            Path::new("/opt/eas listener/eas_listener"),
            Path::new("/opt/eas listener"),
            Some(1000),
        );
        assert!(unit.contains("User=1000\n"));
        assert!(unit.contains("WorkingDirectory=/opt/eas listener\n"));
        assert!(unit.contains(
            "ExecStart=\"/opt/eas listener/eas_listener\" --service --app-root \"/opt/eas listener\"\n"
        ));
        assert!(unit.contains("WantedBy=multi-user.target"));
        assert!(unit.contains("After=network-online.target"));
    }

    #[test]
    fn a_root_owned_install_runs_as_root() {
        let unit = render_unit(
            Path::new("/opt/eas/eas_listener"),
            Path::new("/opt/eas"),
            None,
        );
        assert!(!unit.contains("User="));
    }

    #[test]
    fn specifiers_and_quotes_in_paths_cannot_escape_the_unit() {
        let unit = render_unit(
            Path::new("/srv/100%\"odd\"/eas_listener"),
            Path::new("/srv/100%\"odd\""),
            None,
        );
        assert!(unit.contains("WorkingDirectory=/srv/100%%\"odd\"\n"));
        assert!(unit.contains("ExecStart=\"/srv/100%%\\\"odd\\\"/eas_listener\""));
    }
}

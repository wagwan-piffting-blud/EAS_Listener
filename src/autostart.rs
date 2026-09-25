//! Whether first-run setup can offer to start the listener at boot, and doing it when asked.
//!
//! One question for every platform: a Windows service through `service`, a systemd unit through
//! `systemd`, a launchd job through `launchd`. Setup asks this before it offers anything, so the
//! page never shows a choice the machine cannot carry out.

use anyhow::Result;
use serde::Serialize;

/// A build that cannot install a service only ever says `Unavailable`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
#[cfg_attr(
    not(any(
        all(windows, feature = "service"),
        target_os = "linux",
        target_os = "macos"
    )),
    allow(dead_code)
)]
pub enum Availability {
    /// Nothing to offer, for `reason`.
    Unavailable { reason: String },
    /// Setup can install it itself. `prompt` is true when Windows asks for administrator rights
    /// first, on this machine's desktop.
    Automatic { kind: &'static str, prompt: bool },
    /// Needs rights this process cannot ask for from here; `command` is what to run instead.
    /// macOS never needs it: without root, a LaunchAgent needs none.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    Manual { kind: &'static str, command: String },
}

fn unavailable(reason: &str) -> Availability {
    Availability::Unavailable {
        reason: reason.to_string(),
    }
}

/// `client_is_local` is whether the setup page was loaded on this machine, which is the only
/// place a Windows consent prompt can be answered from.
pub fn availability(client_is_local: bool) -> Availability {
    if crate::paths::running_as_service() {
        return unavailable("It is already running as a service.");
    }
    // This project's image specifically, not any container: one that boots systemd can take a
    // unit like any other machine, and one that does not is ruled out by the systemd check.
    if crate::setup::restart_after_setup() || std::env::var_os("EAS_IMAGE_VARIANT").is_some() {
        return unavailable(
            "Docker starts it: set restart: unless-stopped for the container instead.",
        );
    }
    platform(client_is_local)
}

#[cfg(all(windows, feature = "service"))]
fn platform(client_is_local: bool) -> Availability {
    use crate::service;
    if service::is_installed() {
        return unavailable("The EASListener service is already installed.");
    }
    if service::is_elevated() {
        return Availability::Automatic {
            kind: "windows_service",
            prompt: false,
        };
    }
    if client_is_local {
        return Availability::Automatic {
            kind: "windows_service",
            prompt: true,
        };
    }
    Availability::Manual {
        kind: "windows_service",
        command: format!("\"{}\" --install-service", exe_display()),
    }
}

#[cfg(target_os = "linux")]
fn platform(_client_is_local: bool) -> Availability {
    use crate::systemd;
    if !systemd::systemd_is_running() {
        return unavailable("systemd is not running on this machine.");
    }
    if systemd::started_by_systemd() {
        return unavailable("It was already started by systemd.");
    }
    if systemd::is_installed() {
        return unavailable("eas-listener.service is already installed.");
    }
    if systemd::is_root() {
        return Availability::Automatic {
            kind: "systemd",
            prompt: false,
        };
    }
    Availability::Manual {
        kind: "systemd",
        command: format!("sudo \"{}\" --install-service", exe_display()),
    }
}

/// Never needs asking for rights: without root it installs a LaunchAgent, which starts at login.
#[cfg(target_os = "macos")]
fn platform(_client_is_local: bool) -> Availability {
    use crate::launchd::{self, Kind};
    match launchd::installed() {
        Some(Kind::Daemon) => unavailable("The EAS Listener LaunchDaemon is already installed."),
        Some(Kind::Agent) => unavailable("The EAS Listener LaunchAgent is already installed."),
        None => Availability::Automatic {
            kind: match launchd::kind_for_this_process() {
                Kind::Daemon => "launchd_daemon",
                Kind::Agent => "launchd_agent",
            },
            prompt: false,
        },
    }
}

#[cfg(not(any(
    all(windows, feature = "service"),
    target_os = "linux",
    target_os = "macos"
)))]
fn platform(_client_is_local: bool) -> Availability {
    unavailable("This build cannot install itself as a service.")
}

#[cfg(any(all(windows, feature = "service"), target_os = "linux"))]
fn exe_display() -> String {
    std::env::current_exe()
        .map(|exe| exe.display().to_string())
        .unwrap_or_else(|_| "eas_listener".to_string())
}

/// Installs and starts it. Blocking: on Windows this waits for the consent prompt to be answered.
#[cfg(all(windows, feature = "service"))]
pub fn install_and_start() -> Result<()> {
    if crate::service::is_elevated() {
        crate::service::install()
    } else {
        crate::service::install_elevated()
    }
}

#[cfg(target_os = "linux")]
pub fn install_and_start() -> Result<()> {
    crate::systemd::install()
}

#[cfg(target_os = "macos")]
pub fn install_and_start() -> Result<()> {
    crate::launchd::install()
}

#[cfg(not(any(
    all(windows, feature = "service"),
    target_os = "linux",
    target_os = "macos"
)))]
pub fn install_and_start() -> Result<()> {
    anyhow::bail!("This build cannot install itself as a service.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_is_told_how_to_offer_it() {
        assert_eq!(
            serde_json::to_value(Availability::Automatic {
                kind: "systemd",
                prompt: false
            })
            .unwrap(),
            serde_json::json!({ "mode": "automatic", "kind": "systemd", "prompt": false })
        );
        assert_eq!(
            serde_json::to_value(unavailable("no")).unwrap(),
            serde_json::json!({ "mode": "unavailable", "reason": "no" })
        );
    }
}

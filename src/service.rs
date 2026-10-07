//! Windows service registration.
//!
//! The tray shim only lives as long as a desktop session, so a machine that should keep listening
//! across logout and reboot wants a service instead. Registration shells out to `sc.exe` rather
//! than taking a service-framework dependency: the listener already runs fine as a plain process,
//! and this only has to create, delete and describe the entry.
//!
//! A real service also has to answer the Service Control Manager's start/stop handshake. That is
//! what `--service` is for -- see `install` for how the two fit together.

use anyhow::{bail, Context, Result};
use std::process::Command;

const SERVICE_NAME: &str = "EASListener";
const DISPLAY_NAME: &str = "EAS Listener";

/// `EASListener` for the default instance, which keeps the name existing installs registered;
/// `EASListener-<name>` for a named one, so several can be installed side by side.
pub fn service_name() -> String {
    match crate::paths::instance() {
        Some(name) => format!("{SERVICE_NAME}-{name}"),
        None => SERVICE_NAME.to_string(),
    }
}

fn display_name() -> String {
    match crate::paths::instance() {
        Some(name) => format!("{DISPLAY_NAME} ({name})"),
        None => DISPLAY_NAME.to_string(),
    }
}

/// What the service is started with, after `--service`: the instance, and its directory pinned
/// explicitly since a service starts with no working directory of its own.
fn instance_args(app_root: &std::path::Path) -> String {
    format!(
        "{} --app-root \"{}\"",
        crate::paths::instance_flag(),
        app_root.display()
    )
}

const DESCRIPTION: &str =
    "Listens to broadcast audio streams, decodes EAS/SAME messages and serves the monitoring dashboard.";

fn run_sc(args: &[&str]) -> Result<String> {
    let output = Command::new("sc.exe")
        .args(args)
        .output()
        .context("Failed to run sc.exe; is this a Windows host?")?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    if !output.status.success() {
        let detail = if stderr.is_empty() { &stdout } else { &stderr };
        bail!("sc.exe {} failed: {}", args.join(" "), detail);
    }

    Ok(stdout)
}

pub fn is_installed() -> bool {
    run_sc(&["query", &service_name()]).is_ok()
}

/// Installs from a process that is not elevated, by running this executable's own
/// `--install-service` elevated. Windows shows its consent prompt on the desktop and this blocks
/// until it is answered; declining it comes back as an error.
pub fn install_elevated() -> Result<()> {
    run_elevated("--install-service", "Installing")
}

/// Removes the service from a process that is not elevated, the same way.
pub fn uninstall_elevated() -> Result<()> {
    run_elevated("--uninstall-service", "Removing")
}

fn run_elevated(action: &str, doing: &str) -> Result<()> {
    let exe = std::env::current_exe().context("Could not determine this executable's path")?;
    // A trailing backslash would escape the closing quote wrapped around it below.
    let app_root = crate::paths::app_root()
        .display()
        .to_string()
        .trim_end_matches('\\')
        .to_string();

    // The paths travel in the environment rather than the command, so nothing in them can be
    // read as PowerShell.
    let script = "$ErrorActionPreference = 'Stop'; \
        $a = @($env:EAS_ELEVATE_ACTION, '--app-root', ('\"' + $env:EAS_ELEVATE_ROOT + '\"')); \
        if ($env:EAS_ELEVATE_INSTANCE) { $a += @('--instance', $env:EAS_ELEVATE_INSTANCE) }; \
        $p = Start-Process -FilePath $env:EAS_ELEVATE_EXE -Verb RunAs -Wait -PassThru -WindowStyle Hidden \
            -ArgumentList $a; \
        exit $p.ExitCode";
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .env("EAS_ELEVATE_EXE", &exe)
        .env("EAS_ELEVATE_ACTION", action)
        .env("EAS_ELEVATE_ROOT", app_root)
        .env(
            "EAS_ELEVATE_INSTANCE",
            crate::paths::instance().unwrap_or_default(),
        )
        .output()
        .context("Failed to run PowerShell to ask for administrator rights")?;

    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("canceled by the user") || stderr.contains("cancelled by the user") {
        bail!("The Windows administrator prompt was declined.");
    }
    bail!(
        "{doing} the service as administrator failed (exit code {:?}). {}",
        output.status.code(),
        stderr.trim()
    )
}

pub(crate) fn is_elevated() -> bool {
    // Querying the SCM's configuration requires administrator rights, so a successful query is a
    // reliable proxy without pulling in the Windows API crates.
    Command::new("net.exe")
        .args(["session"])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

pub fn install() -> Result<()> {
    if !is_elevated() {
        bail!(
            "Installing a service needs administrator rights. Re-run this from an elevated \
             terminal (right-click PowerShell, Run as administrator)."
        );
    }

    let exe = std::env::current_exe().context("Could not determine this executable's path")?;
    let app_root = crate::paths::app_root();

    let bin_path = format!("\"{}\" --service{}", exe.display(), instance_args(app_root));

    run_sc(&[
        "create",
        &service_name(),
        "binPath=",
        &bin_path,
        "start=",
        "auto",
        "DisplayName=",
        &display_name(),
    ])?;
    run_sc(&["description", &service_name(), DESCRIPTION])?;

    // Restart on failure: 60s, 60s, then every 5 minutes, with the counter resetting daily.
    run_sc(&[
        "failure",
        &service_name(),
        "reset=",
        "86400",
        "actions=",
        "restart/60000/restart/60000/restart/300000",
    ])?;

    run_sc(&["start", &service_name()])?;

    println!(
        "Installed and started the '{}' service; it starts at every boot from now on.",
        service_name()
    );
    println!("  Executable:       {}", exe.display());
    println!("  Application root: {}", app_root.display());
    println!();
    println!(
        "Remove it with:  \"{}\"{} --uninstall-service",
        exe.display(),
        crate::paths::instance_flag()
    );
    Ok(())
}

pub fn uninstall() -> Result<()> {
    if !is_elevated() {
        bail!(
            "Removing a service needs administrator rights. Re-run this from an elevated terminal."
        );
    }

    // Stopping a service that is not running is not an error worth failing over.
    if let Err(err) = run_sc(&["stop", &service_name()]) {
        eprintln!("Note: {err}");
    }

    run_sc(&["delete", &service_name()])?;
    println!("Removed the '{}' service.", service_name());
    Ok(())
}

pub fn status() -> Result<()> {
    println!("{}", run_sc(&["query", &service_name()])?);
    Ok(())
}

/// The service side: hands control to the Service Control Manager, which calls back into
/// `service_main` on its own thread.
pub mod host {
    use super::service_name;
    use std::ffi::OsString;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc};
    use std::time::Duration;
    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};

    windows_service::define_windows_service!(ffi_service_main, service_main);

    pub fn start() -> anyhow::Result<()> {
        windows_service::service_dispatcher::start(service_name(), ffi_service_main)
            .map_err(|err| anyhow::anyhow!("Failed to connect to the service dispatcher: {err}"))
    }

    fn service_main(_arguments: Vec<OsString>) {
        if let Err(err) = run() {
            eprintln!("Service exited with an error: {err:?}");
        }
    }

    /// Written beside config.json when the listener stops on its own. A service has no console,
    /// so this is the only place its reason can be read.
    const FAILURE_FILE: &str = "service-error.log";

    fn run() -> anyhow::Result<()> {
        crate::paths::mark_running_as_service();
        // Session 0 has no desktop, so a browser opened from here would never be seen.
        crate::setup::disable_browser_launch();

        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let stop_on_failure = shutdown_tx.clone();
        let failed = Arc::new(AtomicBool::new(false));
        let listener_failed = failed.clone();

        let handler = move |control| match control {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                let _ = shutdown_tx.send(());
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        };

        let status_handle = service_control_handler::register(service_name(), handler)
            .map_err(|err| anyhow::anyhow!("Failed to register the service handler: {err}"))?;

        let running = |state: ServiceState, controls: ServiceControlAccept| ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: controls,
            // A service-specific code is what makes the SCM report the stop as a failure.
            exit_code: if failed.load(Ordering::SeqCst) {
                ServiceExitCode::ServiceSpecific(1)
            } else {
                ServiceExitCode::Win32(0)
            },
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        };

        // The SCM gives a service about 30 seconds to report Running, so say so before starting
        // any of the listener's own work.
        status_handle.set_service_status(running(
            ServiceState::Running,
            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        ))?;

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        runtime.spawn(async move {
            if let Err(err) = crate::run_listener().await {
                let report = format!(
                    "{}: EAS Listener stopped: {err:?}\n",
                    chrono::Local::now().to_rfc3339()
                );
                let _ = std::fs::write(crate::paths::in_app_root(FAILURE_FILE), &report);
                eprint!("{report}");
                listener_failed.store(true, Ordering::SeqCst);
                let _ = stop_on_failure.send(());
            }
        });

        // Blocks until the SCM asks to stop or the listener gives up; dropping the runtime then
        // stops whatever is left of it.
        let _ = shutdown_rx.recv();

        status_handle.set_service_status(running(
            ServiceState::Stopped,
            ServiceControlAccept::empty(),
        ))?;
        Ok(())
    }
}

//! `--uninstall`: removes one instance -- its service and everything it wrote -- and with `--all`
//! every instance on the machine plus the tools and voices they fetched, leaving only the files
//! the release shipped.
//!
//! Only files this program writes are deleted, never a directory whole unless it is one the
//! program made for itself, so an `--app-root` or `SHARED_STATE_DIR` pointing somewhere shared
//! cannot take anything else with it.

use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

/// What an instance writes into its own directory.
const INSTANCE_FILES: &[&str] = &[
    "config.json",
    "config.json.bak",
    "config.json.migrated",
    "config.json.bak.migrated",
    "apprise.yml",
    "apprise.yml.bak",
    "apprise.yml.migrated",
    "apprise.yml.bak.migrated",
    "cap_tts_replacement_config.json",
    "cap_tts_replacement_config.json.migrated",
    "web_config.json",
    "setup-token.txt",
    "reload_signal",
    "test_alert_signal",
    "service-error.log",
    "launchd.log",
];

/// What a fetch leaves in a shared root, beside the files the release shipped there.
const FETCHED_DIRS: &[&str] = &["piper", "tts_voices/spfy/voices"];

/// The release's own files in `tools/` and `tts_voices/`, which stay.
const SHIPPED: &[&str] = &[
    "tools/components.json",
    "tools/fetch_components.sh",
    "tools/fetch_components.ps1",
    "tools/README.md",
    "tools/.gitignore",
    "tts_voices/cep6/fetch_voices.sh",
    "tts_voices/cep6/fetch_voices.ps1",
    "tts_voices/loq6/.gitkeep",
    "tts_voices/spfy/.gitkeep",
];

const IN_IMAGE: &str = "This is the Docker image: remove the container, and its volumes if you do \
                        not want to keep the alert archive (docker compose down -v).";

#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    pub all: bool,
    pub yes: bool,
    /// The alert archive and recordings go too; by default they are kept.
    pub delete_data: bool,
    /// After this instance, the fetched tools and voices and the program itself, when no other
    /// instance is left.
    pub with_program: bool,
    /// Started from the dashboard: give the listener time to stop before its files are touched.
    pub wait: bool,
}

pub fn run(options: Options) -> Result<()> {
    if std::env::var_os("EAS_IMAGE_VARIANT").is_some() {
        bail!(IN_IMAGE);
    }
    if options.all {
        uninstall_everything(options)
    } else {
        uninstall_this_instance(options)?;
        if options.with_program {
            remove_program_if_last();
        }
        Ok(())
    }
}

fn remove_program_if_last() {
    let left = crate::instances::all();
    if !left.is_empty() {
        let names: Vec<&str> = left.iter().map(|instance| instance.name.as_str()).collect();
        println!(
            "The program, and the tools and voices it fetched, were kept: {} still use them.",
            names.join(", ")
        );
        return;
    }
    remove_tools_and_program();
}

fn remove_tools_and_program() {
    let mut removed = Vec::new();
    for root in crate::paths::shared_roots() {
        remove_fetched(&root, &mut removed);
    }
    let _ = std::fs::remove_dir(crate::paths::instances_dir());
    println!("Removed the fetched tools and voices:");
    for path in &removed {
        println!("  {path}");
    }
    remove_program();
}

/// What a release archive holds beside the binary.
const PROGRAM_FILES: &[&str] = &[
    "README.md",
    "CHANGES.md",
    "LICENSE",
    "config.example.json",
    "cap_tts_replacement_config.example.json",
    "example.env",
];

/// Deletes the binary and the files the release shipped beside it, then their folders if that
/// leaves them empty -- never a folder whole, so a binary put in /usr/local/bin takes nothing
/// else with it. Not from a source checkout, where these are the repository's own files.
fn remove_program() {
    let install = crate::paths::install_root().to_path_buf();
    if install.join("Cargo.toml").exists() {
        println!(
            "{} is a source checkout, so the program's files were left in it.",
            install.display()
        );
        return;
    }
    let mut files: Vec<PathBuf> = PROGRAM_FILES
        .iter()
        .chain(SHIPPED)
        .map(|name| install.join(name))
        .filter(|path| path.is_file())
        .collect();
    if let Ok(exe) = std::env::current_exe().and_then(|exe| exe.canonicalize()) {
        let beside = install
            .canonicalize()
            .is_ok_and(|install| exe.parent() == Some(install.as_path()));
        if beside {
            files.push(exe);
        }
    }
    // Deepest first, so a folder is empty by the time its turn comes.
    let dirs: Vec<PathBuf> = [
        "tts_voices/cep6",
        "tts_voices/loq6",
        "tts_voices/spfy",
        "tts_voices",
        "tools",
        "",
    ]
    .iter()
    .map(|name| install.join(name))
    .collect();
    delete_program_files(&install, files, dirs);
}

#[cfg(unix)]
fn delete_program_files(install: &Path, files: Vec<PathBuf>, dirs: Vec<PathBuf>) {
    // A running binary can be unlinked here; it goes when the last process using it exits.
    let mut failed = Vec::new();
    for file in &files {
        if let Err(err) = std::fs::remove_file(file) {
            failed.push(format!("{} ({err})", file.display()));
        }
    }
    for dir in &dirs {
        let _ = std::fs::remove_dir(dir);
    }
    if failed.is_empty() {
        println!("Removed EAS Listener itself from {}.", install.display());
    } else {
        println!(
            "Could not remove these, probably for want of rights -- delete them with sudo:\n  {}",
            failed.join("\n  ")
        );
    }
}

/// A running .exe cannot be deleted, and this one is running: a detached cmd deletes it and the
/// rest a few seconds after this process exits.
#[cfg(windows)]
fn delete_program_files(install: &Path, files: Vec<PathBuf>, dirs: Vec<PathBuf>) {
    // cmd reads a / as the start of a switch, even inside a quoted path such as tools/README.md.
    let quote = |path: &Path| format!("\"{}\"", path.display().to_string().replace('/', "\\"));
    let mut script = String::from("ping -n 4 127.0.0.1 >nul");
    for file in &files {
        script.push_str(&format!(" & del /f /q {}", quote(file)));
    }
    for dir in &dirs {
        script.push_str(&format!(" & rmdir {} 2>nul", quote(dir)));
    }
    let cmd = std::env::var_os("ComSpec")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows\System32\cmd.exe"));
    let line = format!("\"{}\" /d /s /c \"{script}\"", cmd.display());
    match spawn_detached(&cmd, &line, &std::env::temp_dir()) {
        Ok(()) => println!(
            "EAS Listener itself is removed from {} a few seconds after this finishes.",
            install.display()
        ),
        Err(err) => println!(
            "Could not schedule removing the program ({err}); delete {} by hand.",
            install.display()
        ),
    }
}

// ----- from the dashboard -----

/// What the dashboard's uninstall section shows before anything is done.
#[derive(Debug, Serialize)]
pub struct Plan {
    pub instance: String,
    pub folder: String,
    /// A SHARED_STATE_DIR outside the folder, which is left alone.
    pub kept_state_dir: Option<String>,
    /// The alert archive and recordings, kept unless asked to go too.
    pub data_dir: String,
    /// Where the program is, and whether removing it is possible from here at all.
    pub program_dir: String,
    pub program_removable: bool,
    pub service: Option<String>,
    pub other_instances: Vec<String>,
    pub allowed: bool,
    /// Why not, when it is not, and the command that does it instead.
    pub reason: Option<String>,
    pub command: Option<String>,
    /// Windows will ask for administrator rights on this computer's desktop.
    pub prompts: bool,
}

pub fn plan() -> Plan {
    let root = crate::paths::app_root();
    let state_dir = configured_state_dir(root);
    let label = crate::paths::instance_label();
    let service = installed_service();
    let (allowed, reason, prompts) = if std::env::var_os("EAS_IMAGE_VARIANT").is_some() {
        (false, Some(IN_IMAGE.to_string()), false)
    } else {
        match service_rights() {
            Rights::Enough => (true, None, false),
            Rights::Prompt => (true, None, true),
            Rights::Lacking(why) => (false, Some(why), false),
        }
    };
    Plan {
        instance: label.to_string(),
        folder: root.display().to_string(),
        kept_state_dir: (!state_dir.starts_with(root)).then(|| state_dir.display().to_string()),
        data_dir: state_dir.display().to_string(),
        program_dir: crate::paths::install_root().display().to_string(),
        program_removable: !crate::paths::install_root().join("Cargo.toml").exists(),
        service,
        other_instances: crate::instances::all()
            .into_iter()
            .filter(|instance| instance.dir != root)
            .map(|instance| instance.name)
            .collect(),
        allowed,
        command: (!allowed && reason.as_deref() != Some(IN_IMAGE)).then(cli_command),
        reason,
        prompts,
    }
}

fn cli_command() -> String {
    if cfg!(windows) {
        let exe = std::env::current_exe()
            .map(|exe| exe.display().to_string())
            .unwrap_or_else(|_| "eas_listener".to_string());
        format!("\"{exe}\"{} --uninstall", crate::paths::instance_flag())
    } else {
        crate::systemd::sudo_command("--uninstall")
    }
}

/// Starts the uninstall in a process of its own, which outlives this one: a listener cannot
/// delete the files it has open. When this is the service, the helper removes the service, which
/// stops this process; otherwise `true` is returned and the caller stops this process itself.
pub fn start_from_dashboard(
    confirm: &str,
    delete_data: bool,
    with_program: bool,
) -> Result<(PathBuf, bool), String> {
    let plan = plan();
    if !plan.allowed {
        return Err(plan.reason.unwrap_or_else(|| "It cannot be done from here.".into()));
    }
    if confirm.trim() != plan.instance {
        return Err(format!(
            "Type the instance's name, {}, to confirm.",
            plan.instance
        ));
    }
    let exe = std::env::current_exe().map_err(|err| format!("Could not find this program: {err}"))?;
    let log = std::env::temp_dir().join(format!("eas-listener-uninstall-{}.log", plan.instance));

    let mut args: Vec<std::ffi::OsString> = vec!["--app-root".into(), crate::paths::app_root().into()];
    if let Some(name) = crate::paths::instance() {
        args.extend(["--instance".into(), name.into()]);
    }
    args.extend(["--uninstall".into(), "--yes".into(), "--wait".into()]);
    if delete_data {
        args.push("--delete-data".into());
    }
    if with_program {
        args.push("--with-program".into());
    }
    // The helper opens its own log: it is started with nothing of this process's.
    args.extend(["--log".into(), log.clone().into()]);

    spawn_helper(&exe, &args).map_err(|err| format!("Could not start the uninstall: {err}"))?;
    Ok((log, !service_stops_it()))
}

/// Points this process's stdout and stderr at `path`, for a helper started with no console.
pub fn log_to(path: &Path) -> Result<()> {
    let file = std::fs::File::create(path)
        .with_context(|| format!("Could not write {}", path.display()))?;
    redirect_output(file);
    Ok(())
}

#[cfg(unix)]
fn redirect_output(file: std::fs::File) {
    use std::os::fd::IntoRawFd;
    let fd = file.into_raw_fd();
    // SAFETY: dup2 onto the standard descriptors with a descriptor this process owns.
    unsafe {
        libc::dup2(fd, 1);
        libc::dup2(fd, 2);
    }
}

#[cfg(windows)]
fn redirect_output(file: std::fs::File) {
    use std::os::windows::io::IntoRawHandle;
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const STD_ERROR_HANDLE: u32 = -12i32 as u32;
    extern "system" {
        fn SetStdHandle(which: u32, handle: *mut std::ffi::c_void) -> i32;
    }
    let handle = file.into_raw_handle();
    // SAFETY: the handle is a file this process owns and never closes; Rust's stdout asks for
    // the standard handle on every write, so println! follows it.
    unsafe {
        SetStdHandle(STD_OUTPUT_HANDLE, handle);
        SetStdHandle(STD_ERROR_HANDLE, handle);
    }
}

#[cfg(unix)]
fn spawn_helper(exe: &Path, args: &[std::ffi::OsString]) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    let mut command = helper_command(exe, args);
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        // Out of this process's group, so launchd stopping the job does not take it too.
        .process_group(0);
    command.spawn().map(|_| ())
}

/// Rust always starts a Windows child inheriting every inheritable handle, and this process's
/// listening socket is one: the helper would hold the port open after the listener exits, then
/// wait for it forever. CreateProcessW with inheritance off starts it with nothing of ours.
#[cfg(windows)]
fn spawn_helper(exe: &Path, args: &[std::ffi::OsString]) -> std::io::Result<()> {
    let mut line = String::from_utf16_lossy(&quote_arg(exe.as_os_str()));
    for arg in args {
        line.push(' ');
        line.push_str(&String::from_utf16_lossy(&quote_arg(arg)));
    }
    let cwd = crate::paths::install_root()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir);
    spawn_detached(exe, &line, &cwd)
}

/// Starts `application` with the command line `line` in `cwd`, detached and inheriting nothing.
/// The working directory is given so the new process does not hold one being deleted.
#[cfg(windows)]
fn spawn_detached(application: &Path, line: &str, cwd: &Path) -> std::io::Result<()> {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;

    #[repr(C)]
    struct StartupInfoW {
        cb: u32,
        reserved: *mut u16,
        desktop: *mut u16,
        title: *mut u16,
        x: u32,
        y: u32,
        x_size: u32,
        y_size: u32,
        x_count_chars: u32,
        y_count_chars: u32,
        fill_attribute: u32,
        flags: u32,
        show_window: u16,
        reserved2_size: u16,
        reserved2: *mut u8,
        std_input: *mut c_void,
        std_output: *mut c_void,
        std_error: *mut c_void,
    }
    #[repr(C)]
    struct ProcessInformation {
        process: *mut c_void,
        thread: *mut c_void,
        process_id: u32,
        thread_id: u32,
    }
    extern "system" {
        #[allow(clippy::too_many_arguments)]
        fn CreateProcessW(
            application: *const u16,
            command_line: *mut u16,
            process_attributes: *const c_void,
            thread_attributes: *const c_void,
            inherit_handles: i32,
            creation_flags: u32,
            environment: *const c_void,
            current_directory: *const u16,
            startup_info: *const StartupInfoW,
            process_information: *mut ProcessInformation,
        ) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }
    // A hidden console rather than none: a console program it starts -- ping, from cmd --
    // would otherwise open a window of its own.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

    let mut line: Vec<u16> = std::ffi::OsStr::new(line).encode_wide().chain([0]).collect();
    let application: Vec<u16> = application.as_os_str().encode_wide().chain([0]).collect();
    let cwd: Vec<u16> = cwd.as_os_str().encode_wide().chain([0]).collect();
    let startup = StartupInfoW {
        cb: std::mem::size_of::<StartupInfoW>() as u32,
        reserved: std::ptr::null_mut(),
        desktop: std::ptr::null_mut(),
        title: std::ptr::null_mut(),
        x: 0,
        y: 0,
        x_size: 0,
        y_size: 0,
        x_count_chars: 0,
        y_count_chars: 0,
        fill_attribute: 0,
        flags: 0,
        show_window: 0,
        reserved2_size: 0,
        reserved2: std::ptr::null_mut(),
        std_input: std::ptr::null_mut(),
        std_output: std::ptr::null_mut(),
        std_error: std::ptr::null_mut(),
    };
    let mut info = ProcessInformation {
        process: std::ptr::null_mut(),
        thread: std::ptr::null_mut(),
        process_id: 0,
        thread_id: 0,
    };
    // SAFETY: every pointer is to a live, NUL-terminated buffer or a correctly sized struct, and
    // the returned handles are closed here.
    let ok = unsafe {
        CreateProcessW(
            application.as_ptr(),
            line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP,
            std::ptr::null(),
            cwd.as_ptr(),
            &startup,
            &mut info,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: both handles came from the successful CreateProcessW above.
    unsafe {
        CloseHandle(info.thread);
        CloseHandle(info.process);
    }
    Ok(())
}

/// One argument quoted the way the MSVC runtime splits a command line back apart.
#[cfg(windows)]
fn quote_arg(arg: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = arg.encode_wide().collect();
    let needs = wide.is_empty()
        || wide
            .iter()
            .any(|&c| c == ' ' as u16 || c == '\t' as u16 || c == '"' as u16);
    if !needs {
        return wide;
    }
    let mut quoted = vec!['"' as u16];
    let mut backslashes = 0;
    for &c in &wide {
        if c == '\\' as u16 {
            backslashes += 1;
            continue;
        }
        let run = if c == '"' as u16 { backslashes * 2 + 1 } else { backslashes };
        quoted.extend(std::iter::repeat_n('\\' as u16, run));
        backslashes = 0;
        quoted.push(c);
    }
    quoted.extend(std::iter::repeat_n('\\' as u16, backslashes * 2));
    quoted.push('"' as u16);
    quoted
}

/// Whether removing the service is what stops this process.
fn service_stops_it() -> bool {
    crate::paths::running_as_service() && installed_service().is_some()
}

/// A systemd unit stops every process in its cgroup, the helper included, so from a unit the
/// helper runs as a transient unit of its own.
#[cfg(unix)]
fn helper_command(exe: &Path, args: &[std::ffi::OsString]) -> Command {
    if cfg!(target_os = "linux") && crate::paths::running_as_service() {
        let mut command = Command::new("systemd-run");
        command
            .args(["--collect", "--quiet", "--unit"])
            .arg(format!(
                "eas-listener-uninstall-{}",
                crate::paths::instance_label()
            ))
            .arg(exe)
            .args(args);
        command
    } else {
        let mut command = Command::new(exe);
        command.args(args);
        command
    }
}

enum Rights {
    Enough,
    /// Windows asks on the desktop first.
    #[cfg_attr(not(all(windows, feature = "service")), allow(dead_code))]
    Prompt,
    #[cfg_attr(windows, allow(dead_code))]
    Lacking(String),
}

fn confirm(question: &str, answer: &str, yes: bool) -> Result<()> {
    if yes {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        bail!("Nothing is removed without confirmation. Run it again with --yes to go ahead.");
    }
    print!("{question}\nType '{answer}' to go ahead: ");
    std::io::stdout().flush().ok();
    let mut typed = String::new();
    std::io::stdin().lock().read_line(&mut typed)?;
    if typed.trim() != answer {
        bail!("Nothing was removed.");
    }
    Ok(())
}

fn uninstall_this_instance(options: Options) -> Result<()> {
    let yes = options.yes;
    let label = crate::paths::instance_label();
    let root = crate::paths::app_root().to_path_buf();
    let state_dir = configured_state_dir(&root);
    let delete_data = options.delete_data && state_dir.starts_with(&root);
    let mut question = format!(
        "This removes the EAS Listener instance '{label}': its service, if it has one, and its \
         configuration and notification list in {}.",
        root.display()
    );
    if delete_data {
        question.push_str(&format!(
            "\nIts alert archive and recordings in {} go too.",
            state_dir.display()
        ));
    } else {
        question.push_str(&format!(
            "\nIts alert archive and recordings in {} are kept.",
            state_dir.display()
        ));
    }
    if !root.join("config.json").exists() && !root.join("setup-token.txt").exists() {
        remove_service()?;
        bail!(
            "There is no instance '{label}' in {}. --list-instances shows the ones there are.",
            root.display()
        );
    }
    confirm(&question, label, yes)?;

    remove_service()?;
    // A service that was just stopped can take a moment to let go of its port, and a listener
    // that started this from its dashboard is on its way out.
    let port = crate::instances::read_instance(label, &root).dashboard_port;
    let tries = if options.wait { 120 } else { 20 };
    for _ in 0..tries {
        if !crate::instances::answers(port) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    if crate::instances::answers(port) {
        bail!(
            "Something still answers on port {port}, which is where this instance listens. If \
             that is it, running by hand or from the tray, stop it and run this again. Nothing \
             was removed."
        );
    }

    let mut removed = Vec::new();
    if delete_data {
        remove_dir(&state_dir, &mut removed)?;
    }
    for name in INSTANCE_FILES {
        remove_file(&root.join(name), &mut removed)?;
    }
    // Tools and voices an instance fetched for itself, when it could write nowhere shared. A
    // directory that is also the install root holds the shared ones, which stay.
    if root != crate::paths::install_root() {
        for name in ["tools", "tts_voices", "piper"] {
            remove_dir(&root.join(name), &mut removed)?;
        }
        if std::fs::remove_dir(&root).is_ok() {
            removed.push(root.display().to_string());
        }
    }

    println!("Removed the instance '{label}':");
    for path in &removed {
        println!("  {path}");
    }
    if state_dir.exists() {
        println!(
            "Kept its alert archive and recordings in {}. Delete that folder when you no longer \
             want them; an instance set up under the name '{label}' again picks them back up.",
            state_dir.display()
        );
    }
    let only_data_left = std::fs::read_dir(&root)
        .map(|entries| entries.flatten().all(|entry| entry.path() == state_dir))
        .unwrap_or(true);
    if root.exists() && !only_data_left && root != crate::paths::install_root() {
        println!(
            "{} still holds files the listener did not write, so it was left in place.",
            root.display()
        );
    }
    Ok(())
}

fn uninstall_everything(options: Options) -> Result<()> {
    let instances = crate::instances::all();
    let mut question = String::from(
        "This removes EAS Listener from this machine: every instance's service, configuration \
         and notification list, the tools and voices they fetched, and the program itself.\n",
    );
    question.push_str(if options.delete_data {
        "Their alert archives and recordings go too:\n"
    } else {
        "Their alert archives and recordings are kept:\n"
    });
    for instance in &instances {
        question.push_str(&format!(
            "  {:<16} {}\n",
            instance.name,
            instance.dir.display()
        ));
    }
    confirm(&question, "everything", options.yes)?;

    // Each in a process of its own, which names the right service for that instance.
    let exe = std::env::current_exe().context("Could not determine this executable's path")?;
    let mut failed = Vec::new();
    for instance in &instances {
        let mut command = Command::new(&exe);
        command.arg("--app-root").arg(&instance.dir);
        if instance.name != crate::paths::DEFAULT_INSTANCE {
            command.args(["--instance", &instance.name]);
        }
        command.args(["--uninstall", "--yes"]);
        if options.delete_data {
            command.arg("--delete-data");
        }
        let status = command
            .status()
            .with_context(|| format!("Could not run {}", exe.display()))?;
        if !status.success() {
            failed.push(instance.name.clone());
        }
    }
    if !failed.is_empty() {
        bail!(
            "These instances could not be removed, so the shared tools were left too: {}",
            failed.join(", ")
        );
    }

    remove_tools_and_program();
    Ok(())
}

/// The fetched tools and voices in one shared root, keeping what the release shipped. A root
/// this account cannot write -- root's, from a normal account -- is skipped.
fn remove_fetched(root: &Path, removed: &mut Vec<String>) {
    for sub in ["tools", "tts_voices"] {
        let dir = root.join(sub);
        let Ok(entries) = walk(&dir) else {
            continue;
        };
        for path in entries {
            let shipped = path
                .strip_prefix(root)
                .ok()
                .map(|relative| relative.to_string_lossy().replace('\\', "/"))
                .is_some_and(|relative| SHIPPED.contains(&relative.as_str()));
            if !shipped && std::fs::remove_file(&path).is_ok() {
                removed.push(path.display().to_string());
            }
        }
        prune_empty_dirs(&dir);
    }
    for name in FETCHED_DIRS {
        // A root this account cannot write is left as it is, like the files above.
        let _ = remove_dir(&root.join(name), removed);
    }
}

fn walk(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            files.extend(walk(&path)?);
        } else {
            files.push(path);
        }
    }
    Ok(files)
}

/// Removes the directories under `dir` that are empty, then `dir` if it is.
fn prune_empty_dirs(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                prune_empty_dirs(&entry.path());
            }
        }
    }
    let _ = std::fs::remove_dir(dir);
}

fn remove_file(path: &Path, removed: &mut Vec<String>) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => {
            removed.push(path.display().to_string());
            Ok(())
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("Could not remove {}", path.display())),
    }
}

fn remove_dir(path: &Path, removed: &mut Vec<String>) -> Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => {
            removed.push(format!("{}{}", path.display(), std::path::MAIN_SEPARATOR));
            Ok(())
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("Could not remove {}", path.display())),
    }
}

/// The state folder the instance's config.json names, or `data/` inside the instance.
fn configured_state_dir(root: &Path) -> PathBuf {
    std::fs::read_to_string(root.join("config.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|config| {
            config
                .get("SHARED_STATE_DIR")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|dir| !dir.is_empty())
                .map(PathBuf::from)
        })
        .map(|dir| if dir.is_absolute() { dir } else { root.join(dir) })
        .unwrap_or_else(|| root.join("data"))
}

#[cfg(all(windows, feature = "service"))]
fn installed_service() -> Option<String> {
    crate::service::is_installed().then(crate::service::service_name)
}

#[cfg(all(windows, feature = "service"))]
fn service_rights() -> Rights {
    if !crate::service::is_installed()
        || crate::paths::running_as_service()
        || crate::service::is_elevated()
    {
        Rights::Enough
    } else {
        Rights::Prompt
    }
}

#[cfg(target_os = "linux")]
fn installed_service() -> Option<String> {
    crate::systemd::is_installed().then(crate::systemd::unit_name)
}

#[cfg(target_os = "linux")]
fn service_rights() -> Rights {
    if !crate::systemd::is_installed() || crate::paths::is_root() {
        Rights::Enough
    } else {
        Rights::Lacking(format!(
            "{} is installed, and this listener does not run as root, so it cannot remove it.",
            crate::systemd::unit_name()
        ))
    }
}

#[cfg(target_os = "macos")]
fn installed_service() -> Option<String> {
    crate::launchd::installed().map(|_| crate::launchd::label())
}

#[cfg(target_os = "macos")]
fn service_rights() -> Rights {
    match crate::launchd::installed() {
        Some(crate::launchd::Kind::Daemon) if !crate::paths::is_root() => Rights::Lacking(
            "Its LaunchDaemon needs root to remove, which this listener does not run as.".into(),
        ),
        _ => Rights::Enough,
    }
}

#[cfg(not(any(
    all(windows, feature = "service"),
    target_os = "linux",
    target_os = "macos"
)))]
fn installed_service() -> Option<String> {
    None
}

#[cfg(not(any(
    all(windows, feature = "service"),
    target_os = "linux",
    target_os = "macos"
)))]
fn service_rights() -> Rights {
    Rights::Enough
}

#[cfg(all(windows, feature = "service"))]
fn remove_service() -> Result<()> {
    if !crate::service::is_installed() {
        return Ok(());
    }
    if crate::service::is_elevated() {
        crate::service::uninstall()
    } else {
        crate::service::uninstall_elevated()
    }
}

#[cfg(target_os = "linux")]
fn remove_service() -> Result<()> {
    if !crate::systemd::is_installed() {
        return Ok(());
    }
    if !crate::paths::is_root() {
        bail!(
            "{} is installed, and removing it needs root. Run: {}",
            crate::systemd::unit_name(),
            crate::systemd::sudo_command("--uninstall")
        );
    }
    crate::systemd::uninstall()
}

#[cfg(target_os = "macos")]
fn remove_service() -> Result<()> {
    use crate::launchd::Kind;
    match crate::launchd::installed() {
        None => Ok(()),
        Some(Kind::Daemon) if !crate::paths::is_root() => bail!(
            "Its LaunchDaemon is installed, and removing it needs root. Run: {}",
            crate::systemd::sudo_command("--uninstall")
        ),
        Some(_) => crate::launchd::uninstall(),
    }
}

#[cfg(not(any(
    all(windows, feature = "service"),
    target_os = "linux",
    target_os = "macos"
)))]
fn remove_service() -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn helper_arguments_are_quoted_the_way_windows_splits_them() {
        let quoted = |arg: &str| String::from_utf16(&quote_arg(std::ffi::OsStr::new(arg))).unwrap();
        assert_eq!(quoted("--yes"), "--yes");
        assert_eq!(
            quoted(r"C:\Program Files\eas\north"),
            r#""C:\Program Files\eas\north""#
        );
        assert_eq!(quoted(r"C:\a b\"), r#""C:\a b\\""#);
        assert_eq!(quoted(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quoted(""), r#""""#);
    }

    #[test]
    fn the_state_folder_is_the_instances_unless_config_json_names_another() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert_eq!(configured_state_dir(dir.path()), dir.path().join("data"));
        std::fs::write(
            dir.path().join("config.json"),
            r#"{"SHARED_STATE_DIR": "archive"}"#,
        )
        .expect("config");
        assert_eq!(configured_state_dir(dir.path()), dir.path().join("archive"));
    }

    #[test]
    fn fetched_tools_and_voices_go_and_the_shipped_files_stay() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path();
        for file in SHIPPED
            .iter()
            .copied()
            .chain(["tools/ffmpeg", "tools/.ffmpeg.sha256", "tools/piper/piper", "tools/SOURCES.txt"])
            .chain(["tts_voices/cep6/Allison/voice.idx", "tts_voices/spfy/voices/tom/tom.vin"])
            .chain(["piper/en_US-lessac-medium.onnx"])
        {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"x").unwrap();
        }

        let mut removed = Vec::new();
        remove_fetched(root, &mut removed);

        for shipped in SHIPPED {
            assert!(root.join(shipped).is_file(), "{shipped} was removed");
        }
        for gone in [
            "tools/ffmpeg",
            "tools/.ffmpeg.sha256",
            "tools/piper",
            "tools/SOURCES.txt",
            "tts_voices/cep6/Allison",
            "tts_voices/spfy/voices",
            "piper",
        ] {
            assert!(!root.join(gone).exists(), "{gone} is still there");
        }
    }
}

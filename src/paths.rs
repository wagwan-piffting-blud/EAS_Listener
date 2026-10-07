use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

static RUNNING_AS_SERVICE: AtomicBool = AtomicBool::new(false);

/// Called before configuration is read when a service manager started this process -- the
/// Windows Service Control Manager, or the systemd unit or launchd job `--install-service` writes.
#[cfg_attr(
    not(any(
        all(windows, feature = "service"),
        target_os = "linux",
        target_os = "macos"
    )),
    allow(dead_code)
)]
pub fn mark_running_as_service() {
    RUNNING_AS_SERVICE.store(true, Ordering::Relaxed);
}

pub fn running_as_service() -> bool {
    RUNNING_AS_SERVICE.load(Ordering::Relaxed)
}

#[cfg(unix)]
pub fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

#[cfg(not(unix))]
pub fn is_root() -> bool {
    false
}

/// The account name for a UID, which is what a launchd job's UserName takes.
#[cfg(unix)]
pub fn user_name(uid: u32) -> Option<String> {
    // SAFETY: getpwuid returns null or a pointer to static storage that stays valid until the
    // next getpwuid call; the name is copied out before returning.
    unsafe {
        let entry = libc::getpwuid(uid);
        if entry.is_null() || (*entry).pw_name.is_null() {
            return None;
        }
        std::ffi::CStr::from_ptr((*entry).pw_name)
            .to_str()
            .ok()
            .map(str::to_string)
    }
}

#[cfg(not(unix))]
pub fn user_name(_uid: u32) -> Option<String> {
    None
}

// ----- instances -----
//
// One binary runs any number of listeners. Each is an instance with a directory of its own --
// config.json, apprise.yml, the setup token, the signal files and its data -- under the OS's data
// directory. What every instance shares stays in the install root beside the binary: the fetched
// tools, the TTS engines' voices and scripts, and the dashboard's files.

/// Images published before EAS_APP_ROOT existed install the binary to /usr/local/bin while the
/// assets stay in /app, so the executable's own directory is the wrong answer inside them.
const LEGACY_DOCKER_ROOT: &str = "/app";
/// The file whose presence marks a directory as a populated install root.
const ROOT_MARKER: &str = "config.json";
const MAX_TARGET_WALK_UP: usize = 4;
pub const DEFAULT_INSTANCE: &str = "default";
const MAX_INSTANCE_NAME: usize = 40;
/// Folders beside the instances that hold what they share, so never an instance's name.
const RESERVED_NAMES: &[&str] = &["tools", "tts_voices", "piper", "instances"];

static INSTANCE: OnceLock<Option<String>> = OnceLock::new();
static INSTALL_ROOT: OnceLock<PathBuf> = OnceLock::new();
static APP_ROOT: OnceLock<PathBuf> = OnceLock::new();
static ASSETS_ROOT: OnceLock<PathBuf> = OnceLock::new();

/// An instance name as typed: letters, digits, `-` and `_`, since it becomes a directory and a
/// service name on every platform. `default` is the unnamed instance.
pub fn parse_instance_name(raw: &str) -> Result<Option<String>, String> {
    let name = raw.trim();
    if name.is_empty()
        || name.len() > MAX_INSTANCE_NAME
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        || name.starts_with('-')
    {
        return Err(format!(
            "'{raw}' is not an instance name: use up to {MAX_INSTANCE_NAME} letters, digits, - \
             and _, such as north or wxr-2."
        ));
    }
    let name = name.to_ascii_lowercase();
    if RESERVED_NAMES.contains(&name.as_str()) {
        return Err(format!(
            "'{raw}' is the name of a folder the instances share; choose another name."
        ));
    }
    Ok((name != DEFAULT_INSTANCE).then_some(name))
}

/// Chooses the instance for this process. Called by `main` before anything resolves a path;
/// without a call, `EAS_INSTANCE` decides, and without that it is the default instance.
pub fn set_instance(name: Option<String>) -> Result<(), String> {
    if INSTANCE.set(name.clone()).is_err() && instance() != name.as_deref() {
        return Err("The instance was chosen after paths were already resolved.".to_string());
    }
    Ok(())
}

/// The named instance, or `None` for the default one.
pub fn instance() -> Option<&'static str> {
    INSTANCE
        .get_or_init(|| {
            std::env::var("EAS_INSTANCE")
                .ok()
                .and_then(|raw| parse_instance_name(&raw).ok().flatten())
        })
        .as_deref()
}

pub fn instance_label() -> &'static str {
    instance().unwrap_or(DEFAULT_INSTANCE)
}

/// ` --instance <name>` for a named instance, to append to a command shown to someone; nothing
/// for the default one.
pub fn instance_flag() -> String {
    instance()
        .map(|name| format!(" --instance {name}"))
        .unwrap_or_default()
}

/// The directory holding what every instance shares: /app under Docker, the directory containing
/// the executable for a portable install, the crate root under `cargo run`/`cargo test`.
pub fn install_root() -> &'static Path {
    INSTALL_ROOT.get_or_init(detect_install_root).as_path()
}

pub fn in_install_root(relative: impl AsRef<Path>) -> PathBuf {
    install_root().join(relative)
}

/// This instance's own directory, holding config.json and everything written for it.
pub fn app_root() -> &'static Path {
    APP_ROOT
        .get_or_init(|| instance_root_candidate().0)
        .as_path()
}

pub fn in_app_root(relative: impl AsRef<Path>) -> PathBuf {
    app_root().join(relative)
}

/// Where instance directories live: `%ProgramData%\eas-listener` on Windows, which a service
/// running as LocalSystem shares with the desktop; on Linux `/var/lib/eas-listener` for root and
/// the XDG data directory for anyone else; on macOS the matching Application Support folder.
pub fn instances_dir() -> PathBuf {
    instances_dir_for(is_root(), |key| std::env::var_os(key).map(PathBuf::from))
        .unwrap_or_else(|| install_root().join("instances"))
}

fn instances_dir_for(root: bool, env: impl Fn(&str) -> Option<PathBuf>) -> Option<PathBuf> {
    let non_empty = |key: &str| env(key).filter(|path| !path.as_os_str().is_empty());
    let base = if cfg!(windows) {
        non_empty("ProgramData")
            .or_else(|| non_empty("PROGRAMDATA"))
            .or_else(|| Some(PathBuf::from(r"C:\ProgramData")))
    } else if cfg!(target_os = "macos") {
        if root {
            Some(PathBuf::from("/Library/Application Support"))
        } else {
            non_empty("HOME").map(|home| home.join("Library").join("Application Support"))
        }
    } else if root {
        Some(PathBuf::from("/var/lib"))
    } else {
        non_empty("XDG_DATA_HOME")
            .filter(|dir| dir.is_absolute())
            .or_else(|| non_empty("HOME").map(|home| home.join(".local").join("share")))
    };
    base.map(|base| base.join("eas-listener"))
}

pub fn instance_dir(name: Option<&str>) -> PathBuf {
    instances_dir().join(name.unwrap_or(DEFAULT_INSTANCE))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RootSource {
    /// EAS_APP_ROOT or --app-root: Docker, a service, or someone who asked.
    Explicit,
    /// `cargo run` from a checkout, which keeps config.json in the crate root.
    Development,
    Instance,
}

fn instance_root_candidate() -> (PathBuf, RootSource) {
    if let Some(dir) = env_dir_override("EAS_APP_ROOT") {
        return (dir, RootSource::Explicit);
    }
    if instance().is_none() {
        if let Some(crate_root) = exe_dir().as_deref().and_then(crate_root_above_target) {
            return (crate_root, RootSource::Development);
        }
    }
    (instance_dir(instance()), RootSource::Instance)
}

/// What `prepare_instance` did, for `main` to print before logging exists.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Prepared {
    pub notes: Vec<String>,
}

/// Settles this instance's directory before anything reads it. The default instance used to
/// keep its files beside the binary; the first run that finds them there moves them into its
/// instance directory -- unless a service still runs from beside the binary (`service_installed`),
/// in which case the old place stays in use until the service is installed again.
pub fn prepare_instance(service_installed: impl FnOnce() -> bool) -> Prepared {
    let mut prepared = Prepared::default();
    let (candidate, source) = instance_root_candidate();
    let legacy = install_root().to_path_buf();
    let legacy_config = legacy.join(ROOT_MARKER);

    let chosen = if source == RootSource::Instance
        && instance().is_none()
        && legacy_config.exists()
        && !candidate.join(ROOT_MARKER).exists()
        && legacy != candidate
    {
        if service_installed() {
            prepared.notes.push(format!(
                "Using the configuration beside the binary, in {}, because the installed \
                 service runs from there. Run --install-service again to move it to {}.",
                legacy.display(),
                candidate.display()
            ));
            legacy
        } else {
            match migrate_default_instance(&legacy, &candidate, &std::env::temp_dir()) {
                Ok(notes) => {
                    prepared.notes.extend(notes);
                    candidate
                }
                Err(err) => {
                    prepared.notes.push(format!(
                        "Could not move the configuration from {} to {} ({err}); using it \
                         where it is.",
                        legacy.display(),
                        candidate.display()
                    ));
                    legacy
                }
            }
        }
    } else {
        candidate
    };

    let _ = APP_ROOT.set(chosen);
    prepared
}

/// Made when the listener runs rather than when the instance is chosen, so asking about a
/// service or uninstalling one leaves no empty directory behind.
pub fn create_app_root() -> std::io::Result<()> {
    std::fs::create_dir_all(app_root())
}

/// The files an instance owns that the default instance kept beside the binary.
const MIGRATED_FILES: &[&str] = &[
    ROOT_MARKER,
    "apprise.yml",
    "apprise.yml.bak",
    "config.json.bak",
    "cap_tts_replacement_config.json",
];

/// Copies the default instance's files from `legacy` into `target`, then renames the originals to
/// `<name>.migrated` so nothing is read from two places. Its alert archive moves too when it was
/// in the default place -- `data/` beside the binary, which a service used, or the shared temp
/// folder otherwise -- and when it cannot be moved, the configuration is pointed at where it is.
pub(crate) fn migrate_default_instance(
    legacy: &Path,
    target: &Path,
    temp_dir: &Path,
) -> std::io::Result<Vec<String>> {
    std::fs::create_dir_all(target)?;
    let mut notes = Vec::new();
    let mut copied = Vec::new();
    for name in MIGRATED_FILES {
        let from = legacy.join(name);
        if from.is_file() {
            std::fs::copy(&from, target.join(name))?;
            copied.push(*name);
        }
    }

    let config_path = target.join(ROOT_MARKER);
    let mut config: serde_json::Value = std::fs::read_to_string(&config_path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    let names_state_dir = config
        .get("SHARED_STATE_DIR")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|dir| !dir.trim().is_empty());
    if !names_state_dir {
        let old_state = [legacy.join("data"), temp_dir.join("eas-listener")]
            .into_iter()
            .find(|dir| dir.is_dir());
        let new_state = target.join("data");
        if let Some(old_state) = old_state.filter(|_| !new_state.exists()) {
            if std::fs::rename(&old_state, &new_state).is_ok() {
                notes.push(format!(
                    "Moved the alert archive from {} to {}.",
                    old_state.display(),
                    new_state.display()
                ));
            } else if let Some(object) = config.as_object_mut() {
                object.insert(
                    "SHARED_STATE_DIR".to_string(),
                    serde_json::Value::String(old_state.display().to_string()),
                );
                let text = serde_json::to_string_pretty(&config).map_err(std::io::Error::other)?;
                std::fs::write(&config_path, format!("{text}\n"))?;
                notes.push(format!(
                    "The alert archive in {} could not be moved, so SHARED_STATE_DIR now points \
                     at it.",
                    old_state.display()
                ));
            }
        }
    }

    for name in &copied {
        let from = legacy.join(name);
        let mut moved_name = from.as_os_str().to_os_string();
        moved_name.push(".migrated");
        std::fs::rename(&from, PathBuf::from(moved_name))?;
    }
    notes.insert(
        0,
        format!(
            "Moved this listener's configuration ({}) from {} to its instance directory, {}. \
             The originals are kept as *.migrated.",
            copied.join(", "),
            legacy.display(),
            target.display()
        ),
    );
    Ok(notes)
}

/// Every place fetched tools and their data can be, for every instance whichever account runs
/// it: beside the binary; this account's instances directory; and root's (`/var/lib/eas-listener`
/// or `/Library/Application Support/eas-listener`), which a root-run listener fetches into when
/// the binary sits somewhere root cannot write and which everyone can read. On Windows the
/// instances directory is already the machine's. No directory every account can write is ever
/// used: a root-run listener executes what is in these, so that would hand root to anyone.
pub fn shared_roots() -> Vec<PathBuf> {
    let mut roots = vec![install_root().to_path_buf(), instances_dir()];
    if let Some(system) = instances_dir_for(true, |key| std::env::var_os(key).map(PathBuf::from)) {
        roots.push(system);
    }
    let mut unique = Vec::with_capacity(roots.len());
    for root in roots {
        if !unique.contains(&root) {
            unique.push(root);
        }
    }
    unique
}

/// Where this listener fetches tools and their data: the first shared root it can write, so an
/// instance that cannot write beside the binary still shares what it fetches with this account's
/// other instances. Only when none is writable does it keep them in its own directory.
pub fn assets_root() -> &'static Path {
    ASSETS_ROOT
        .get_or_init(|| {
            shared_roots()
                .into_iter()
                .find(|root| writable(&root.join("tools")))
                .unwrap_or_else(|| app_root().to_path_buf())
        })
        .as_path()
}

/// `relative` in the first shared root that has it -- whichever instance fetched it -- or where
/// this listener would fetch it to.
pub fn find_shared(relative: impl AsRef<Path>) -> PathBuf {
    let relative = relative.as_ref();
    shared_roots()
        .into_iter()
        .map(|root| root.join(relative))
        .find(|path| path.exists())
        .unwrap_or_else(|| assets_root().join(relative))
}

fn writable(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(format!(".write-test-{}", std::process::id()));
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    if ok {
        share_directory(dir);
    }
    ok
}

/// A directory this process made under a restrictive umask -- a service's is often 077 -- would
/// hide what is fetched into it from listeners running as anyone else.
#[cfg(unix)]
fn share_directory(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(dir) {
        let mode = meta.permissions().mode();
        if mode & 0o055 != 0o055 {
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode | 0o755));
        }
    }
}

#[cfg(not(unix))]
fn share_directory(_dir: &Path) {}

// ----- named paths -----

/// Where the configuration is read and written. Normally config.json itself; when that is a
/// directory -- what Docker creates for a bind-mounted file that does not exist on the host yet --
/// the configuration is kept inside it instead, which reaches the host through the same mount and
/// needs nobody to fix the mount first.
pub fn config_json() -> PathBuf {
    resolve_config_json(&in_app_root(ROOT_MARKER))
}

pub fn config_json_is_directory() -> bool {
    in_app_root(ROOT_MARKER).is_dir()
}

fn resolve_config_json(path: &Path) -> PathBuf {
    if path.is_dir() {
        path.join(ROOT_MARKER)
    } else {
        path.to_path_buf()
    }
}

pub fn web_runtime_config() -> PathBuf {
    in_app_root("web_config.json")
}

/// Written alongside the dashboard assets so a source checkout serves a current copy.
pub fn web_runtime_config_fallback() -> PathBuf {
    web_root().join("web_config.json")
}

pub fn apprise_config() -> PathBuf {
    in_app_root("apprise.yml")
}

/// Legacy trigger for a configuration reload: `touch` it and the listener reloads. Kept because
/// existing deployments script against it; the dashboard's POST /api/reload does the same thing.
pub fn reload_signal() -> PathBuf {
    in_app_root("reload_signal")
}

/// Legacy trigger for a manual test alert. See [`reload_signal`].
pub fn test_alert_signal() -> PathBuf {
    in_app_root("test_alert_signal")
}

/// The dashboard's static files. Docker serves them from /var/www/html for historical reasons;
/// everywhere else they sit next to the binary.
pub fn web_root() -> PathBuf {
    if let Some(dir) = env_dir_override("EAS_WEB_ROOT") {
        return dir;
    }
    in_install_root("web_server")
}

/// Where the fetch scripts are, and where a portable install keeps the third-party binaries it
/// was shipped or fetched with.
pub fn tools_dir() -> PathBuf {
    in_install_root("tools")
}

/// Every place a fetched tool can be, in `shared_roots` order, then this instance's own
/// directory when it could write to none of them.
pub fn tools_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = shared_roots()
        .into_iter()
        .map(|root| root.join("tools"))
        .collect();
    let own = assets_root().join("tools");
    if !dirs.contains(&own) {
        dirs.push(own);
    }
    dirs
}

pub fn cap_tts_replacement_dict() -> PathBuf {
    in_app_root("cap_tts_replacement_config.json")
}

pub fn piper_default_model() -> PathBuf {
    find_shared(Path::new("piper").join("en_US-lessac-medium.onnx"))
}

pub fn spfy_voice_dir() -> PathBuf {
    find_shared(
        Path::new("tts_voices")
            .join("spfy")
            .join("voices")
            .join("tom"),
    )
}

/// Where Cepstral voices are fetched to outside the image, beside the other engines' voices. In
/// the image they stay on the /data volume, which outlives the container.
pub fn cep6_voice_dir() -> PathBuf {
    assets_root().join("tts_voices").join("cep6")
}

/// Every folder of Cepstral voices another instance may already have filled.
pub fn cep6_voice_roots() -> Vec<PathBuf> {
    shared_roots()
        .into_iter()
        .map(|root| root.join("tts_voices").join("cep6"))
        .collect()
}

fn env_dir_override(key: &str) -> Option<PathBuf> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// cargo drops binaries in target/<profile>/ and test binaries in target/<profile>/deps/, neither
/// of which holds the assets; the crate root above the target directory does.
fn crate_root_above_target(exe_dir: &Path) -> Option<PathBuf> {
    let mut cursor = exe_dir;
    for _ in 0..MAX_TARGET_WALK_UP {
        if cursor.file_name() == Some(OsStr::new("target")) {
            return cursor.parent().map(Path::to_path_buf);
        }
        cursor = cursor.parent()?;
    }
    None
}

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
}

fn detect_install_root() -> PathBuf {
    if let Some(dir) = env_dir_override("EAS_INSTALL_ROOT") {
        return dir;
    }
    // The image keeps its assets in EAS_APP_ROOT, which is also its one instance's directory.
    if std::env::var_os("EAS_IMAGE_VARIANT").is_some() {
        if let Some(dir) = env_dir_override("EAS_APP_ROOT") {
            return dir;
        }
    }

    let exe_root = exe_dir().map(|dir| crate_root_above_target(&dir).unwrap_or(dir));

    if let Some(dir) = exe_root.as_ref() {
        if !dir.join(ROOT_MARKER).exists() && !dir.join("tools").is_dir() {
            let legacy = Path::new(LEGACY_DOCKER_ROOT);
            if legacy.join(ROOT_MARKER).exists() {
                return legacy.to_path_buf();
            }
        }
    }

    exe_root.unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_root_is_found_above_a_cargo_target_directory() {
        assert_eq!(
            crate_root_above_target(Path::new("/src/eas-listener/target/debug")),
            Some(PathBuf::from("/src/eas-listener"))
        );
        assert_eq!(
            crate_root_above_target(Path::new("/src/eas-listener/target/debug/deps")),
            Some(PathBuf::from("/src/eas-listener"))
        );
        assert_eq!(
            crate_root_above_target(Path::new(
                "/src/eas-listener/target/x86_64-unknown-linux-gnu/release"
            )),
            Some(PathBuf::from("/src/eas-listener"))
        );
    }

    #[test]
    fn a_plain_install_directory_is_not_mistaken_for_a_cargo_target() {
        assert_eq!(
            crate_root_above_target(Path::new("/opt/eas-listener")),
            None
        );
        assert_eq!(crate_root_above_target(Path::new("/usr/local/bin")), None);
    }

    #[test]
    fn a_config_json_directory_holds_the_configuration_inside_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = dir.path().join("config.json");

        assert_eq!(resolve_config_json(&file), file);

        std::fs::write(&file, "{}").expect("write");
        assert_eq!(resolve_config_json(&file), file);

        std::fs::remove_file(&file).expect("remove");
        std::fs::create_dir(&file).expect("mkdir");
        assert_eq!(resolve_config_json(&file), file.join("config.json"));
    }

    #[test]
    fn instance_files_hang_off_the_app_root_and_shared_ones_off_the_install_root() {
        let root = app_root();
        assert_eq!(config_json(), root.join("config.json"));
        assert_eq!(web_runtime_config(), root.join("web_config.json"));
        assert_eq!(apprise_config(), root.join("apprise.yml"));
        assert_eq!(
            cap_tts_replacement_dict(),
            root.join("cap_tts_replacement_config.json")
        );
        assert_eq!(tools_dir(), install_root().join("tools"));
        assert_eq!(tools_dirs()[0], tools_dir());
    }

    #[test]
    fn instance_names_are_safe_as_directories_and_service_names() {
        assert_eq!(parse_instance_name("north"), Ok(Some("north".to_string())));
        assert_eq!(
            parse_instance_name(" WXR_2 "),
            Ok(Some("wxr_2".to_string()))
        );
        assert_eq!(parse_instance_name("Default"), Ok(None));
        for bad in [
            "",
            "a/b",
            "..",
            "-flag",
            "two words",
            "é",
            &"x".repeat(41),
            "tools",
            "TTS_Voices",
            "piper",
        ] {
            assert!(parse_instance_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn instances_live_in_the_platforms_data_directory() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |key: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| PathBuf::from(v))
            }
        };
        if cfg!(windows) {
            assert_eq!(
                instances_dir_for(false, env(&[("ProgramData", r"D:\PD")])),
                Some(PathBuf::from(r"D:\PD").join("eas-listener"))
            );
            assert_eq!(
                instances_dir_for(false, env(&[])),
                Some(PathBuf::from(r"C:\ProgramData").join("eas-listener"))
            );
        } else if cfg!(target_os = "macos") {
            assert_eq!(
                instances_dir_for(false, env(&[("HOME", "/Users/w")])),
                Some(PathBuf::from(
                    "/Users/w/Library/Application Support/eas-listener"
                ))
            );
            assert_eq!(
                instances_dir_for(true, env(&[])),
                Some(PathBuf::from("/Library/Application Support/eas-listener"))
            );
        } else {
            assert_eq!(
                instances_dir_for(true, env(&[("HOME", "/root")])),
                Some(PathBuf::from("/var/lib/eas-listener"))
            );
            assert_eq!(
                instances_dir_for(false, env(&[("HOME", "/home/w")])),
                Some(PathBuf::from("/home/w/.local/share/eas-listener"))
            );
            assert_eq!(
                instances_dir_for(
                    false,
                    env(&[("HOME", "/home/w"), ("XDG_DATA_HOME", "/data/xdg")])
                ),
                Some(PathBuf::from("/data/xdg/eas-listener"))
            );
            // A relative XDG_DATA_HOME is invalid by the spec and ignored.
            assert_eq!(
                instances_dir_for(false, env(&[("HOME", "/home/w"), ("XDG_DATA_HOME", "rel")])),
                Some(PathBuf::from("/home/w/.local/share/eas-listener"))
            );
        }
    }

    #[test]
    fn the_default_instance_moves_out_from_beside_the_binary() {
        let dir = tempfile::tempdir().expect("temp dir");
        let legacy = dir.path().join("install");
        let target = dir.path().join("instances").join("default");
        let temp = dir.path().join("tmp");
        std::fs::create_dir_all(legacy.join("data")).expect("legacy data");
        std::fs::create_dir_all(temp.join("eas-listener")).expect("temp state");
        std::fs::write(legacy.join("config.json"), r#"{"EAS_RELAY_NAME": "X"}"#).expect("cfg");
        std::fs::write(legacy.join("apprise.yml"), "- json://h/\n").expect("apprise");
        std::fs::write(legacy.join("data").join("alerts.db"), b"db").expect("db");

        let notes = migrate_default_instance(&legacy, &target, &temp).expect("migrate");
        assert!(notes[0].contains("config.json, apprise.yml"), "{notes:?}");
        assert_eq!(
            std::fs::read_to_string(target.join("config.json")).unwrap(),
            r#"{"EAS_RELAY_NAME": "X"}"#
        );
        assert!(target.join("apprise.yml").is_file());
        // data/ beside the binary wins over the temp folder, and is moved rather than copied.
        assert!(target.join("data").join("alerts.db").is_file());
        assert!(!legacy.join("data").exists());
        assert!(temp.join("eas-listener").is_dir());
        assert!(!legacy.join("config.json").exists());
        assert!(legacy.join("config.json.migrated").is_file());
        assert!(legacy.join("apprise.yml.migrated").is_file());
    }

    #[test]
    fn a_state_dir_the_configuration_names_is_left_where_it_is() {
        let dir = tempfile::tempdir().expect("temp dir");
        let legacy = dir.path().join("install");
        let target = dir.path().join("instance");
        let temp = dir.path().join("tmp");
        std::fs::create_dir_all(temp.join("eas-listener")).expect("temp state");
        std::fs::create_dir_all(&legacy).expect("legacy");
        std::fs::write(
            legacy.join("config.json"),
            r#"{"SHARED_STATE_DIR": "/srv/eas"}"#,
        )
        .expect("cfg");

        migrate_default_instance(&legacy, &target, &temp).expect("migrate");
        assert!(temp.join("eas-listener").is_dir());
        assert!(!target.join("data").exists());
    }
}

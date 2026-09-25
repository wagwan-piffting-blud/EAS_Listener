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

/// Images published before EAS_APP_ROOT existed install the binary to /usr/local/bin while the
/// assets stay in /app, so the executable's own directory is the wrong answer inside them.
const LEGACY_DOCKER_ROOT: &str = "/app";
/// The file whose presence marks a directory as a populated install root.
const ROOT_MARKER: &str = "config.json";
const MAX_TARGET_WALK_UP: usize = 4;

static APP_ROOT: OnceLock<PathBuf> = OnceLock::new();

/// The directory holding config.json and the bundled assets: /app under Docker, the directory
/// containing the executable for a portable install, the crate root under `cargo run`/`cargo test`.
pub fn app_root() -> &'static Path {
    APP_ROOT.get_or_init(detect_app_root).as_path()
}

pub fn in_app_root(relative: impl AsRef<Path>) -> PathBuf {
    app_root().join(relative)
}

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
    in_app_root("web_server").join("web_config.json")
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
    in_app_root("web_server")
}

/// Where a portable install keeps the third-party binaries it was shipped or fetched with.
pub fn tools_dir() -> PathBuf {
    in_app_root("tools")
}

pub fn cap_tts_replacement_dict() -> PathBuf {
    in_app_root("cap_tts_replacement_config.json")
}

pub fn piper_default_model() -> PathBuf {
    in_app_root("piper").join("en_US-lessac-medium.onnx")
}

pub fn spfy_voice_dir() -> PathBuf {
    in_app_root("tts_voices")
        .join("spfy")
        .join("voices")
        .join("tom")
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

fn detect_app_root() -> PathBuf {
    if let Some(dir) = env_dir_override("EAS_APP_ROOT") {
        return dir;
    }

    let exe_root = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .map(|dir| crate_root_above_target(&dir).unwrap_or(dir));

    if let Some(dir) = exe_root.as_ref() {
        if !dir.join(ROOT_MARKER).exists() {
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
    fn named_paths_hang_off_the_resolved_app_root() {
        let root = app_root();
        assert_eq!(config_json(), root.join("config.json"));
        assert_eq!(web_runtime_config(), root.join("web_config.json"));
        assert_eq!(
            web_runtime_config_fallback(),
            root.join("web_server").join("web_config.json")
        );
        assert_eq!(apprise_config(), root.join("apprise.yml"));
        assert_eq!(
            cap_tts_replacement_dict(),
            root.join("cap_tts_replacement_config.json")
        );
    }
}

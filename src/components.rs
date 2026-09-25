//! Discovery of the third-party binaries the listener shells out to.
//!
//! Every one of these runs as a separate process over argv and stdio; none is linked in, so their
//! licences stay theirs. A future change tempted to link one of these libraries instead should
//! check that licence first: it is not a free swap.

use crate::config::Config;
use once_cell::sync::Lazy;
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::RwLock;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tracing::{info, warn};

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Requirement {
    /// The listener cannot do useful work without it.
    Required,
    /// One feature degrades if it is absent; everything else keeps working.
    Optional,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolvedFrom {
    /// An explicit path in config.json.
    Configured,
    /// Shipped or fetched into the install's own tools directory.
    Bundled,
    /// Left to the OS to find, which is how every Docker install resolves them.
    SystemPath,
}

#[derive(Debug, Clone, Copy)]
pub struct ComponentSpec {
    pub key: &'static str,
    /// Its name in tools/components.json, which is what the fetch scripts take.
    pub manifest: &'static str,
    pub config_key: &'static str,
    pub binary: &'static str,
    pub requirement: Requirement,
    pub purpose: &'static str,
    /// Arguments that make the binary print its version and exit. A wrong guess here is harmless:
    /// presence is decided by whether the process spawns at all, not by what it prints.
    pub version_args: &'static [&'static str],
}

pub const FFMPEG: ComponentSpec = ComponentSpec {
    key: "ffmpeg",
    manifest: "ffmpeg",
    config_key: "FFMPEG_PATH",
    binary: "ffmpeg",
    requirement: Requirement::Required,
    purpose: "Recording, relay encoding, CAP alert audio and encoding the alert stream",
    version_args: &["-version"],
};

pub const APPRISE: ComponentSpec = ComponentSpec {
    key: "apprise",
    manifest: "apprise",
    config_key: "APPRISE_PATH",
    binary: "apprise",
    requirement: Requirement::Optional,
    purpose: "Notifications to non-Discord targets; Discord is sent natively without it",
    version_args: &["--version"],
};

pub const PIPER: ComponentSpec = ComponentSpec {
    key: "piper",
    manifest: "piper",
    config_key: "PIPER_PATH",
    binary: "piper",
    requirement: Requirement::Optional,
    purpose: "CAP alert text-to-speech (piper engine)",
    version_args: &["--version"],
};

pub const ESPEAK_NG: ComponentSpec = ComponentSpec {
    key: "espeak-ng",
    manifest: "espeak-ng",
    config_key: "ESPEAK_NG_PATH",
    binary: "espeak-ng",
    requirement: Requirement::Optional,
    purpose: "CAP alert text-to-speech (espeak-ng engine)",
    version_args: &["--version"],
};

pub const SPFY_SYNTH: ComponentSpec = ComponentSpec {
    key: "speechify",
    manifest: "speechify",
    config_key: "SPFY_SYNTH_PATH",
    binary: "spfy_synth",
    requirement: Requirement::Optional,
    purpose: "CAP alert text-to-speech (Speechify Tom engine)",
    version_args: &["--version"],
};

pub const CEP6: ComponentSpec = ComponentSpec {
    key: "cepstral",
    manifest: "cep6",
    config_key: "CEP6_PATH",
    binary: "cep6",
    requirement: Requirement::Optional,
    purpose: "CAP alert text-to-speech (Cepstral engine)",
    version_args: &["--version"],
};

pub const LOQDAVE: ComponentSpec = ComponentSpec {
    key: "loquendo",
    manifest: "loqdave",
    config_key: "LOQDAVE_PATH",
    binary: "loqdave",
    requirement: Requirement::Optional,
    purpose: "CAP alert text-to-speech (Loquendo engine)",
    version_args: &["--version"],
};

pub const ALL: &[ComponentSpec] = &[FFMPEG, APPRISE, PIPER, ESPEAK_NG, SPFY_SYNTH, CEP6, LOQDAVE];

#[derive(Debug, Clone, Serialize)]
pub struct ComponentStatus {
    pub key: &'static str,
    pub config_key: &'static str,
    pub requirement: Requirement,
    pub purpose: &'static str,
    pub path: String,
    pub resolved_from: ResolvedFrom,
    pub present: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

static RESOLVED: RwLock<Option<HashMap<&'static str, (PathBuf, ResolvedFrom)>>> = RwLock::new(None);
static LAST_PROBE: RwLock<Option<Vec<ComponentStatus>>> = RwLock::new(None);

/// Resolution order: an explicit path in config.json, then the install's own tools directory,
/// then whatever the OS finds on PATH. PATH comes last so a Docker or distro install keeps using
/// its packaged binaries and nothing about those deployments changes.
fn resolve_in(
    spec: &ComponentSpec,
    configured: Option<&str>,
    tools_dir: &Path,
) -> (PathBuf, ResolvedFrom) {
    if let Some(value) = configured {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return (PathBuf::from(trimmed), ResolvedFrom::Configured);
        }
    }

    let file = format!("{}{}", spec.binary, std::env::consts::EXE_SUFFIX);
    // A program installed with its own folder -- Piper, with its libraries -- is one level down.
    for bundled in [
        tools_dir.join(&file),
        tools_dir.join(spec.manifest).join(&file),
    ] {
        if bundled.is_file() {
            return (bundled, ResolvedFrom::Bundled);
        }
    }

    (PathBuf::from(spec.binary), ResolvedFrom::SystemPath)
}

/// Whether the resolved path names something that exists, without running it: a path is checked
/// as a file, a bare name is looked for on PATH.
fn exists(path: &Path) -> bool {
    if path.components().count() > 1 {
        return path.is_file();
    }
    let with_suffix = format!(
        "{}{}",
        path.display(),
        if path.extension().is_none() {
            std::env::consts::EXE_SUFFIX
        } else {
            ""
        }
    );
    std::env::var_os("PATH").is_some_and(|dirs| {
        std::env::split_paths(&dirs).any(|dir| dir.join(&with_suffix).is_file())
    })
}

fn resolve(spec: &ComponentSpec, configured: Option<&str>) -> (PathBuf, ResolvedFrom) {
    resolve_in(spec, configured, &crate::paths::tools_dir())
}

pub fn apply_runtime_config(config: &Config) {
    let mut resolved = HashMap::new();
    for spec in ALL {
        let configured = config
            .component_paths
            .get(spec.config_key)
            .map(String::as_str);
        resolved.insert(spec.key, resolve(spec, configured));
    }

    match RESOLVED.write() {
        Ok(mut guard) => *guard = Some(resolved),
        Err(err) => warn!("Component path registry lock poisoned: {}", err),
    }
}

/// The resolved path for a component. Before any configuration is loaded -- first-run setup --
/// there is no configured path, so the tools directory and then PATH decide.
pub fn binary(spec: &ComponentSpec) -> PathBuf {
    RESOLVED
        .read()
        .ok()
        .and_then(|guard| {
            guard
                .as_ref()
                .and_then(|map| map.get(spec.key).map(|(path, _)| path.clone()))
        })
        .unwrap_or_else(|| resolve(spec, None).0)
}

pub fn ffmpeg() -> PathBuf {
    binary(&FFMPEG)
}

pub fn apprise() -> PathBuf {
    binary(&APPRISE)
}

/// Runs the binary's version command. Anything that spawns counts as present, so an unlucky guess
/// at `version_args` costs a version string and never a false absence.
async fn probe(spec: &ComponentSpec) -> ComponentStatus {
    let (path, resolved_from) = RESOLVED
        .read()
        .ok()
        .and_then(|guard| guard.as_ref().and_then(|map| map.get(spec.key).cloned()))
        .unwrap_or_else(|| (PathBuf::from(spec.binary), ResolvedFrom::SystemPath));

    let mut command = Command::new(&path);
    command
        .args(spec.version_args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let (present, version) = match command.spawn() {
        Ok(child) => match tokio::time::timeout(PROBE_TIMEOUT, child.wait_with_output()).await {
            Ok(Ok(output)) => {
                let mut text = String::from_utf8_lossy(&output.stdout).to_string();
                if text.trim().is_empty() {
                    text = String::from_utf8_lossy(&output.stderr).to_string();
                }
                let first_line = text
                    .lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .map(|line| line.chars().take(120).collect::<String>());
                (true, first_line)
            }
            // It started, so it exists; it just did not answer the way we asked.
            Ok(Err(_)) | Err(_) => (true, None),
        },
        Err(_) => (false, None),
    };

    ComponentStatus {
        key: spec.key,
        config_key: spec.config_key,
        requirement: spec.requirement,
        purpose: spec.purpose,
        path: path.to_string_lossy().into_owned(),
        resolved_from,
        present,
        version,
    }
}

pub async fn probe_all() -> Vec<ComponentStatus> {
    let mut statuses = Vec::with_capacity(ALL.len());
    for spec in ALL {
        statuses.push(probe(spec).await);
    }

    match LAST_PROBE.write() {
        Ok(mut guard) => *guard = Some(statuses.clone()),
        Err(err) => warn!("Component probe cache lock poisoned: {}", err),
    }

    statuses
}

/// The most recent probe. Served to the dashboard so a page load does not spawn every binary
/// again; the startup probe and each config reload refresh it.
pub fn last_probe() -> Vec<ComponentStatus> {
    LAST_PROBE
        .read()
        .ok()
        .and_then(|guard| guard.clone())
        .unwrap_or_default()
}

/// A download of ffmpeg or a Cepstral voice on a slow line takes minutes; this only stops a fetch
/// that hangs.
const FETCH_TIMEOUT: Duration = Duration::from_secs(20 * 60);
/// How long a failed fetch is remembered, so a missing engine is not downloaded again for every
/// passage of every alert while the network is down.
const FETCH_RETRY_AFTER: Duration = Duration::from_secs(10 * 60);

/// One fetch at a time, and when each target last failed, with why.
static FETCHES: Lazy<tokio::sync::Mutex<HashMap<String, (Instant, String)>>> =
    Lazy::new(|| tokio::sync::Mutex::new(HashMap::new()));

/// A fetch script shipped with the install: the PowerShell one on Windows, the shell one elsewhere.
fn script(dir: &Path, stem: &str) -> PathBuf {
    dir.join(format!(
        "{stem}.{}",
        if cfg!(windows) { "ps1" } else { "sh" }
    ))
}

/// Runs a fetch script with nothing to answer prompts -- a service has nobody at a terminal -- and
/// copies what it prints into the log. On failure, the last lines it printed are the reason.
async fn run_script(script: &Path, args: &[&str]) -> Result<String, String> {
    if !script.is_file() {
        return Err(format!("{} is not there", script.display()));
    }
    let mut command = if cfg!(windows) {
        let mut command = Command::new("powershell.exe");
        // Inherited from a PowerShell 7 session, this points Windows PowerShell at modules it
        // cannot load -- Expand-Archive among them. Without it, it builds its own.
        command.env_remove("PSModulePath").args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ]);
        command
    } else {
        Command::new("bash")
    };
    command
        .arg(script)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let child = command
        .spawn()
        .map_err(|err| format!("{} could not be run: {err}", script.display()))?;
    let output = tokio::time::timeout(FETCH_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| {
            format!(
                "it took longer than {} minutes",
                FETCH_TIMEOUT.as_secs() / 60
            )
        })?
        .map_err(|err| err.to_string())?;

    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    for line in &lines {
        info!("  {line}");
    }
    // The reason is in what the script said about the component, not its closing summary.
    let said: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|line| {
            ![
                "components: installed:",
                "components: needs manual install",
                "components: provenance",
                "Without it:",
            ]
            .iter()
            .any(|noise| line.starts_with(noise))
        })
        .collect();
    let tail = said[said.len().saturating_sub(2)..].join(" ");
    if output.status.success() {
        Ok(tail)
    } else {
        Err(tail)
    }
}

/// Runs `fetch` for `target` unless it failed recently, or `present` says a fetch that held the
/// lock first -- the startup prefetch, when an alert arrives mid-download -- already installed it.
/// The fetch runs as a task of its own, so a caller that gives up waiting -- an alert with a
/// deadline -- does not cut a download short.
async fn fetch_once(
    target: String,
    announce: String,
    present: impl Fn() -> bool + Send + 'static,
    fetch: impl std::future::Future<Output = Result<String, String>> + Send + 'static,
) -> Result<(), String> {
    let task = tokio::spawn(async move {
        let mut failures = FETCHES.lock().await;
        if present() {
            return Ok(());
        }
        if let Some((when, why)) = failures.get(&target) {
            if when.elapsed() < FETCH_RETRY_AFTER {
                return Err(format!(
                    "{why} (not retried until 10 minutes after that attempt)"
                ));
            }
        }
        info!("{announce}");
        let result = fetch.await;
        match &result {
            Ok(_) => {
                failures.remove(&target);
            }
            Err(why) => {
                failures.insert(target, (Instant::now(), why.clone()));
            }
        }
        result.map(|_| ())
    });
    task.await
        .map_err(|err| format!("the fetch stopped: {err}"))?
}

/// Makes sure a component can be run, fetching it with the install's own script when it is
/// missing: ffmpeg at startup, a TTS engine when it is first needed. A path set in config.json is
/// never fetched over, since fetching could not change what it points at.
pub async fn ensure(spec: &ComponentSpec) -> Result<PathBuf, String> {
    let (path, from) = resolved(spec);
    if exists(&path) {
        return Ok(path);
    }
    if from == ResolvedFrom::Configured {
        return Err(format!(
            "{} is set to {}, which does not exist",
            spec.config_key,
            path.display()
        ));
    }

    let script = script(&crate::paths::tools_dir(), "fetch_components");
    let announce = format!(
        "'{}' is not installed; fetching it with {} (this can take a few minutes).",
        spec.key,
        script.display()
    );
    let manifest = spec.manifest;
    let args: &[&str] = if cfg!(windows) {
        &["-Component", manifest]
    } else {
        &[manifest]
    };
    let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
    let wanted = *spec;
    let present = move || exists(&resolve(&wanted, None).0);
    fetch_once(format!("component:{manifest}"), announce, present, async move {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let said = run_script(&script, &args).await?;
        // The script succeeds without installing anything where there is no build for this
        // platform; that counts as a failure, so it is not tried again on every alert.
        if exists(&resolve(&wanted, None).0) {
            Ok(said)
        } else {
            Err(format!(
                "there is no build for this platform ({said}). Install it yourself, or set {} \
                 in config.json",
                wanted.config_key
            ))
        }
    })
    .await
    .map_err(|why| format!("'{}' could not be fetched: {why}", spec.key))?;

    // What the script installed is found under tools/, whatever PATH said before.
    let found = resolve(spec, None);
    if let Ok(mut guard) = RESOLVED.write() {
        guard
            .get_or_insert_with(HashMap::new)
            .insert(spec.key, found.clone());
    }
    info!("'{}' is installed at {}", spec.key, found.0.display());
    Ok(found.0)
}

/// Fetches the component again even though its program is present: for the data that comes with
/// it, such as Speechify's voice or Piper's model, when that is what is missing. `present` says
/// whether that data is there now.
pub async fn refetch(
    spec: &ComponentSpec,
    present: impl Fn() -> bool + Send + 'static,
) -> Result<(), String> {
    let script = script(&crate::paths::tools_dir(), "fetch_components");
    let manifest = spec.manifest;
    let announce = format!(
        "The data '{}' needs is missing; fetching it with {}.",
        spec.key,
        script.display()
    );
    fetch_once(format!("component:{manifest}"), announce, present, async move {
        let args: &[&str] = if cfg!(windows) {
            &["-Component", manifest]
        } else {
            &[manifest]
        };
        run_script(&script, args).await
    })
    .await
}

/// Installs one Cepstral voice into `voice_root` with the voice script shipped beside the
/// engine's voices, when it is not there yet. `present` says whether it is there now.
pub async fn ensure_cepstral_voice(
    voice_root: &Path,
    voice: &str,
    present: impl Fn() -> bool + Send + 'static,
) -> Result<(), String> {
    let script = script(
        &crate::paths::in_app_root("tts_voices").join("cep6"),
        "fetch_voices",
    );
    let announce = format!(
        "The Cepstral voice '{}' is not installed; fetching it into {} (a 337 MB download).",
        voice,
        voice_root.display()
    );
    let root = voice_root.display().to_string();
    let voice = voice.to_string();
    fetch_once(format!("cep6-voice:{voice}"), announce, present, async move {
        let flag = if cfg!(windows) { "-Directory" } else { "-d" };
        run_script(&script, &[flag, &root, &voice]).await
    })
    .await
}

/// Fetches each missing required component, so an install unpacked from a release archive gets
/// its ffmpeg without anyone running the script by hand. Returns whether anything was fetched,
/// after which the caller resolves and probes again.
pub async fn fetch_missing_required(statuses: &[ComponentStatus]) -> bool {
    let mut fetched = false;
    for status in missing_required(statuses) {
        let Some(spec) = ALL.iter().find(|spec| spec.key == status.key) else {
            continue;
        };
        match ensure(spec).await {
            Ok(_) => fetched = true,
            Err(why) => warn!("{why}"),
        }
    }
    fetched
}

fn resolved(spec: &ComponentSpec) -> (PathBuf, ResolvedFrom) {
    RESOLVED
        .read()
        .ok()
        .and_then(|guard| guard.as_ref().and_then(|map| map.get(spec.key).cloned()))
        .unwrap_or_else(|| resolve(spec, None))
}

pub fn missing_required(statuses: &[ComponentStatus]) -> Vec<&ComponentStatus> {
    statuses
        .iter()
        .filter(|status| status.requirement == Requirement::Required && !status.present)
        .collect()
}

/// Logs one line per component so a support log shows what the install actually found.
pub fn report(statuses: &[ComponentStatus]) {
    for status in statuses {
        if status.present {
            info!(
                "Component '{}' found at {} ({:?}){}",
                status.key,
                status.path,
                status.resolved_from,
                status
                    .version
                    .as_deref()
                    .map(|version| format!(": {version}"))
                    .unwrap_or_default()
            );
        } else if status.requirement == Requirement::Required {
            warn!(
                "Required component '{}' was not found (looked for '{}'). {}. Run {} to install it, or set {} in config.json.",
                status.key,
                status.path,
                status.purpose,
                crate::paths::tools_dir()
                    .join(if cfg!(windows) { "fetch_components.ps1" } else { "fetch_components.sh" })
                    .display(),
                status.config_key
            );
        } else {
            info!(
                "Optional component '{}' was not found (looked for '{}'); {} is unavailable.",
                status.key, status.path, status.purpose
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tools directory holding a stand-in for `spec`, so resolution tests do not depend on
    /// whatever the developer happens to have fetched into the real one.
    fn tools_dir_containing(spec: &ComponentSpec) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        let name = format!("{}{}", spec.binary, std::env::consts::EXE_SUFFIX);
        std::fs::write(dir.path().join(name), b"stand-in").expect("write stand-in");
        dir
    }

    #[test]
    fn a_configured_path_wins_over_everything_else() {
        let tools = tools_dir_containing(&FFMPEG);
        let (path, source) = resolve_in(&FFMPEG, Some("  C:/tools/ffmpeg.exe  "), tools.path());
        assert_eq!(path, PathBuf::from("C:/tools/ffmpeg.exe"));
        assert_eq!(source, ResolvedFrom::Configured);
    }

    #[test]
    fn a_bundled_binary_is_preferred_over_the_system_path() {
        let tools = tools_dir_containing(&FFMPEG);
        let (path, source) = resolve_in(&FFMPEG, None, tools.path());
        assert_eq!(
            path,
            tools
                .path()
                .join(format!("ffmpeg{}", std::env::consts::EXE_SUFFIX))
        );
        assert_eq!(source, ResolvedFrom::Bundled);
    }

    #[test]
    fn a_blank_configured_path_is_ignored() {
        let tools = tools_dir_containing(&FFMPEG);
        let (path, source) = resolve_in(&FFMPEG, Some("   "), tools.path());
        assert_eq!(
            path,
            tools
                .path()
                .join(format!("ffmpeg{}", std::env::consts::EXE_SUFFIX))
        );
        assert_eq!(source, ResolvedFrom::Bundled);
    }

    #[test]
    fn an_unbundled_component_falls_through_to_the_system_path() {
        let empty = tempfile::tempdir().expect("temp dir");
        let (path, source) = resolve_in(&APPRISE, None, empty.path());
        assert_eq!(path, PathBuf::from("apprise"));
        assert_eq!(source, ResolvedFrom::SystemPath);
    }

    #[test]
    fn every_component_has_a_distinct_key_and_config_key() {
        let mut keys: Vec<_> = ALL.iter().map(|spec| spec.key).collect();
        keys.sort_unstable();
        let count = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), count, "component keys must be unique");

        let mut config_keys: Vec<_> = ALL.iter().map(|spec| spec.config_key).collect();
        config_keys.sort_unstable();
        let count = config_keys.len();
        config_keys.dedup();
        assert_eq!(
            config_keys.len(),
            count,
            "component config keys must be unique"
        );
    }

    #[test]
    fn only_ffmpeg_is_required() {
        let required: Vec<_> = ALL
            .iter()
            .filter(|spec| spec.requirement == Requirement::Required)
            .map(|spec| spec.key)
            .collect();
        // ffprobe was required too, for one probe of the relay destination; symphonia reads
        // that now, in `relay::probe_stream_head`.
        assert_eq!(required, vec!["ffmpeg"]);
    }

    #[test]
    fn every_component_can_be_fetched_by_its_manifest_name() {
        let manifest: serde_json::Value =
            serde_json::from_str(include_str!("../tools/components.json")).expect("manifest");
        for spec in ALL {
            assert!(
                manifest["components"][spec.manifest].is_object(),
                "{} is not in tools/components.json",
                spec.manifest
            );
            assert_eq!(
                manifest["components"][spec.manifest]["provides"][0], spec.binary,
                "{}",
                spec.manifest
            );
        }
    }

    #[test]
    fn a_program_installed_with_its_folder_is_found_inside_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let folder = dir.path().join(PIPER.manifest);
        std::fs::create_dir(&folder).expect("mkdir");
        let exe = folder.join(format!("piper{}", std::env::consts::EXE_SUFFIX));
        std::fs::write(&exe, b"").expect("write");
        assert_eq!(
            resolve_in(&PIPER, None, dir.path()),
            (exe, ResolvedFrom::Bundled)
        );
    }

    #[test]
    fn a_bare_name_exists_only_when_path_has_it() {
        assert!(!exists(Path::new("eas_listener_nonexistent_probe_target")));
        let dir = tempfile::tempdir().expect("temp dir");
        let file = dir.path().join("present");
        std::fs::write(&file, b"").expect("write");
        assert!(exists(&file));
        assert!(!exists(&dir.path().join("absent")));
    }

    #[tokio::test]
    async fn probing_a_binary_that_does_not_exist_reports_absent() {
        let spec = ComponentSpec {
            key: "definitely-not-installed",
            manifest: "definitely-not-installed",
            config_key: "DEFINITELY_NOT_INSTALLED_PATH",
            binary: "eas_listener_nonexistent_probe_target",
            requirement: Requirement::Optional,
            purpose: "test",
            version_args: &["--version"],
        };
        let status = probe(&spec).await;
        assert!(!status.present);
        assert!(status.version.is_none());
    }
}

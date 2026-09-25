//! The dashboard's static files, from disk when a web root holds them and from the binary
//! otherwise.
//!
//! A source checkout and the Docker image both have `web_server/` next to the binary, so they
//! serve the files on disk and an edit shows up on the next refresh. A released binary has
//! nothing beside it and serves the copy `build.rs` baked in. `EAS_WEB_ROOT` points the live
//! source somewhere else; if what it points at has no `index.html` it is reported and the
//! built-in copy is used, rather than serving a half-empty dashboard.

use axum::body::Body;
use axum::http::{header, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;
use tracing::warn;

include!(concat!(env!("OUT_DIR"), "/web_assets.rs"));

/// A build that could not see `web_server/` -- a Dockerfile that forgot to copy it, say -- would
/// otherwise produce a binary whose dashboard is a 404.
const _: () = assert!(
    !EMBEDDED.is_empty(),
    "web_server/ held no files when this was built"
);

/// The file a request for a directory, or for a path the dashboard routes itself, falls back to.
const INDEX: &str = "index.html";

static LIVE_ROOT: OnceLock<Option<PathBuf>> = OnceLock::new();

/// The directory being served from disk, if there is one. Resolved once: a web root that appears
/// or disappears while the listener runs would otherwise change where pages come from mid-session.
pub fn live_root() -> Option<&'static Path> {
    LIVE_ROOT.get_or_init(detect_live_root).as_deref()
}

fn detect_live_root() -> Option<PathBuf> {
    let root = crate::paths::web_root();
    if root.join(INDEX).is_file() {
        return Some(root);
    }

    if std::env::var("EAS_WEB_ROOT").is_ok_and(|value| !value.trim().is_empty()) {
        warn!(
            "EAS_WEB_ROOT points at {:?}, which has no {}; serving the dashboard built into the \
             binary instead",
            root, INDEX
        );
    }
    None
}

/// What the startup log says about where pages are coming from.
pub fn describe() -> String {
    match live_root() {
        Some(root) => format!("{} (on disk)", root.display()),
        None => format!("built into the binary ({} files)", EMBEDDED.len()),
    }
}

/// Serves one known file by name. Missing means the build or the checkout is broken, so it is a
/// 404 rather than the index page.
pub async fn named(name: &'static str) -> Response {
    match read(name).await {
        Some(response) => response,
        None => not_found(),
    }
}

/// Serves whatever the request asks for, falling back to the index page the way the dashboard's
/// own routing expects.
pub async fn fallback(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() || path.ends_with('/') {
        format!("{path}{INDEX}")
    } else {
        path.to_string()
    };

    if let Some(response) = read(&path).await {
        return response;
    }
    read(INDEX).await.unwrap_or_else(not_found)
}

async fn read(path: &str) -> Option<Response> {
    let relative = safe_relative_path(path)?;
    if let Some(root) = live_root() {
        let full = root.join(&relative);
        let bytes = tokio::fs::read(&full).await.ok()?;
        return Some(response(content_type(&relative), Body::from(bytes)));
    }

    let (content_type, bytes) = embedded(&relative)?;
    Some(response(content_type, Body::from(bytes)))
}

/// The file's bytes as the binary carries them, or `None` when it carries no such file.
pub fn embedded(relative: &str) -> Option<(&'static str, &'static [u8])> {
    EMBEDDED
        .binary_search_by(|(route, _, _)| (*route).cmp(relative))
        .ok()
        .map(|index| {
            let (_, content_type, bytes) = EMBEDDED[index];
            (content_type, bytes)
        })
}

/// Rejects anything that is not a plain relative path under the web root: an absolute path, a
/// drive letter, a `..`, or a percent-encoded name, since none of the dashboard's files need one.
fn safe_relative_path(path: &str) -> Option<String> {
    if path.is_empty() || path.contains('%') || path.contains('\\') {
        return None;
    }
    if Path::new(path)
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return None;
    }
    Some(path.to_string())
}

fn content_type(relative: &str) -> &'static str {
    embedded(relative)
        .map(|(content_type, _)| content_type)
        .unwrap_or("application/octet-stream")
}

fn response(content_type: &'static str, body: Body) -> Response {
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(content_type)),
            // The dashboard's files change with the binary and are small; a cached copy from
            // before an upgrade is the only thing caching would buy.
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
        ],
        body,
    )
        .into_response()
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "Not found").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_binary_carries_every_page_the_dashboard_and_setup_serve() {
        for name in [
            "index.html",
            "archive.html",
            "config.html",
            "login.html",
            "login.js",
            "style.css",
            "favicon.ico",
            "site.webmanifest",
            "config-form.js",
            "config-form.css",
            "assets/favicon.svg",
        ] {
            assert!(embedded(name).is_some(), "{name} is not embedded");
        }

        for name in crate::setup::PAGE_FILES {
            assert!(embedded(name).is_some(), "{name} is not embedded");
        }
    }

    #[test]
    fn the_embedded_table_is_sorted_and_typed() {
        let mut sorted = EMBEDDED
            .iter()
            .map(|(route, _, _)| *route)
            .collect::<Vec<_>>();
        let original = sorted.clone();
        sorted.sort_unstable();
        assert_eq!(original, sorted, "the table has to stay binary-searchable");

        let (content_type, bytes) = embedded("index.html").expect("index.html");
        assert_eq!(content_type, "text/html; charset=utf-8");
        assert!(bytes.starts_with(b"<!DOCTYPE"), "index.html looks wrong");
    }

    #[test]
    fn the_local_configuration_is_never_baked_in() {
        // web_config.json is written at runtime from the user's own config.json.
        assert!(embedded("web_config.json").is_none());
    }

    #[test]
    fn a_path_that_climbs_out_of_the_web_root_is_refused() {
        for path in [
            "../config.json",
            "assets/../../config.json",
            "/etc/passwd",
            "assets%2f..%2fconfig.json",
            "assets\\..\\config.json",
            "",
        ] {
            assert!(safe_relative_path(path).is_none(), "{path} was allowed");
        }

        assert_eq!(
            safe_relative_path("assets/favicon.svg").as_deref(),
            Some("assets/favicon.svg")
        );
    }
}

//! nixcache-proxy — one local HTTP server bridging Nix's binary-cache protocol
//! to any number of GHCR-hosted nixcache-oci caches.
//!
//! The cache is chosen by the URL: the substituter for `pplanel/neovix` is
//! `http://127.0.0.1:37515/pplanel/neovix`, and Nix appends `/nix-cache-info`,
//! `/<hash>.narinfo` and `/nar/<file>` to it. If `NIXCACHE_REPO` is set, that
//! repo is also served at the root, which matches the old one-proxy-per-repo
//! layout. Repos in `NIXCACHE_PRELOAD` (space- or comma-separated) are loaded
//! at startup instead of on first request.

mod index;
mod registry;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use serde_json::json;
use tracing::{debug, info, warn};

use index::{Caches, Shared};
use registry::{FetchError, Registry};

const NIX_CACHE_INFO: &str = "StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 40\n";

struct App {
    caches: Caches,
    default_repo: Option<String>,
}

type AppState = State<Arc<App>>;

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// `owner/repo`, lowercased (GHCR names are lowercase). `None` if either part
/// has characters GitHub doesn't allow.
fn normalize_repo(owner: &str, repo: &str) -> Option<String> {
    let valid = |s: &str| {
        !s.is_empty()
            && !s.starts_with('.')
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    };
    (valid(owner) && valid(repo)).then(|| format!("{owner}/{repo}").to_ascii_lowercase())
}

fn default_index_dir() -> Option<PathBuf> {
    if let Some(dir) = env("NIXCACHE_INDEX_DIR").or_else(|| env("CACHE_DIRECTORY")) {
        return Some(dir.into());
    }
    env("HOME").map(|home| PathBuf::from(home).join(".cache/nixcache-proxy"))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nixcache_proxy=info".into()),
        )
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .init();
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| "could not install the rustls crypto provider")?;

    let ttl = Duration::from_secs(
        env("NIXCACHE_INDEX_TTL")
            .and_then(|v| v.parse().ok())
            .unwrap_or(300),
    );
    let registry = Registry::new(
        env("NIXCACHE_REGISTRY").unwrap_or_else(|| "ghcr.io".into()),
        env("GITHUB_TOKEN").or_else(|| env("GH_TOKEN")),
    )?;
    let default_repo = match env("NIXCACHE_REPO") {
        Some(r) => {
            let (owner, repo) = r
                .split_once('/')
                .ok_or("NIXCACHE_REPO must be owner/repo")?;
            Some(normalize_repo(owner, repo).ok_or("NIXCACHE_REPO is not a valid owner/repo")?)
        }
        None => None,
    };
    let index_dir = default_index_dir();

    info!(
        registry = registry.host(),
        default_repo = default_repo.as_deref().unwrap_or("-"),
        index_dir = %index_dir.as_deref().map_or("-".into(), |d| d.display().to_string()),
        ttl_secs = ttl.as_secs(),
        "nixcache-proxy starting"
    );

    let app = Arc::new(App {
        caches: Caches::new(Shared {
            registry,
            ttl,
            index_dir,
        }),
        default_repo,
    });
    // Warm these so the first Nix query doesn't wait on the registry.
    let preload = env("NIXCACHE_PRELOAD").unwrap_or_default();
    let preload = preload
        .split([' ', ',', '\n'])
        .filter(|r| !r.is_empty())
        .map(|r| {
            r.split_once('/')
                .and_then(|(owner, repo)| normalize_repo(owner, repo))
                .ok_or_else(|| format!("NIXCACHE_PRELOAD: {r:?} is not a valid owner/repo"))
        });
    let preload: Vec<String> = app
        .default_repo
        .iter()
        .cloned()
        .map(Ok)
        .chain(preload)
        .collect::<Result<_, _>>()?;
    for repo in preload {
        let cache = app.caches.get(&repo);
        tokio::spawn(async move { cache.index().await });
    }

    let router = Router::new()
        .route("/_status", get(global_status))
        .route("/{file}", get(root_file))
        .route("/_refresh", post(root_refresh))
        .route("/nar/{file}", get(root_nar))
        .route("/{owner}/{repo}/{file}", get(repo_file))
        .route("/{owner}/{repo}/_refresh", post(repo_refresh))
        .route("/{owner}/{repo}/nar/{file}", get(repo_nar))
        .with_state(app);

    let mut servers = tokio::task::JoinSet::new();
    for listener in listeners()? {
        info!(addr = %listener.local_addr()?, "listening");
        servers.spawn(axum::serve(listener, router.clone()).into_future());
    }

    tokio::select! {
        Some(res) = servers.join_next() => { res??; }
        _ = shutdown_signal() => info!("shutting down"),
    }
    Ok(())
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

/// Sockets from launchd when `NIXCACHE_LAUNCHD_SOCKET` names one of the job's
/// `Sockets` entries. launchd owns that socket from the moment the job loads,
/// so connections made while this process is starting or restarting wait in
/// the backlog instead of being refused. Otherwise bind
/// `NIXCACHE_LISTEN:NIXCACHE_PORT` ourselves.
fn listeners() -> std::io::Result<Vec<tokio::net::TcpListener>> {
    if let Some(name) = env("NIXCACHE_LAUNCHD_SOCKET") {
        return launchd::activate(&name)?
            .into_iter()
            .map(|l| {
                l.set_nonblocking(true)?;
                tokio::net::TcpListener::from_std(l)
            })
            .collect();
    }
    let addr: SocketAddr = format!(
        "{}:{}",
        env("NIXCACHE_LISTEN").unwrap_or_else(|| "127.0.0.1".into()),
        env("NIXCACHE_PORT").unwrap_or_else(|| "37515".into())
    )
    .parse()
    .map_err(std::io::Error::other)?;
    let listener = std::net::TcpListener::bind(addr)?;
    listener.set_nonblocking(true)?;
    Ok(vec![tokio::net::TcpListener::from_std(listener)?])
}

#[cfg(target_os = "macos")]
mod launchd {
    use std::ffi::CString;
    use std::os::fd::FromRawFd;
    use std::os::raw::{c_char, c_int};

    unsafe extern "C" {
        fn launch_activate_socket(
            name: *const c_char,
            fds: *mut *mut c_int,
            cnt: *mut usize,
        ) -> c_int;
    }

    pub fn activate(name: &str) -> std::io::Result<Vec<std::net::TcpListener>> {
        let name = CString::new(name).map_err(std::io::Error::other)?;
        let mut fds: *mut c_int = std::ptr::null_mut();
        let mut count = 0usize;
        let err = unsafe { launch_activate_socket(name.as_ptr(), &mut fds, &mut count) };
        if err != 0 {
            return Err(std::io::Error::from_raw_os_error(err));
        }
        let listeners = (0..count)
            .map(|i| unsafe { std::net::TcpListener::from_raw_fd(*fds.add(i)) })
            .collect();
        unsafe { libc::free(fds.cast()) };
        Ok(listeners)
    }
}

#[cfg(not(target_os = "macos"))]
mod launchd {
    pub fn activate(_name: &str) -> std::io::Result<Vec<std::net::TcpListener>> {
        Err(std::io::Error::other(
            "launchd socket activation is macOS-only",
        ))
    }
}

// ── Handlers ──────────────────────────────────────────────────────────

async fn global_status(State(app): AppState) -> Json<serde_json::Value> {
    let caches: Vec<_> = app.caches.all().iter().map(|c| c.status()).collect();
    Json(json!({
        "registry": app.caches.shared().registry.host(),
        "default_repo": app.default_repo,
        "index_ttl": app.caches.shared().ttl.as_secs(),
        "caches": caches,
    }))
}

async fn root_file(State(app): AppState, Path(file): Path<String>) -> Response {
    match app.default_repo.clone() {
        Some(repo) => serve_file(&app, repo, &file).await,
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn root_nar(State(app): AppState, Path(file): Path<String>) -> Response {
    match app.default_repo.clone() {
        Some(repo) => serve_nar(&app, repo, &file).await,
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn root_refresh(State(app): AppState) -> Response {
    match app.default_repo.clone() {
        Some(repo) => refresh(&app, repo).await,
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn repo_file(
    State(app): AppState,
    Path((owner, repo, file)): Path<(String, String, String)>,
) -> Response {
    match normalize_repo(&owner, &repo) {
        Some(repo) => serve_file(&app, repo, &file).await,
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn repo_nar(
    State(app): AppState,
    Path((owner, repo, file)): Path<(String, String, String)>,
) -> Response {
    match normalize_repo(&owner, &repo) {
        Some(repo) => serve_nar(&app, repo, &file).await,
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn repo_refresh(
    State(app): AppState,
    Path((owner, repo)): Path<(String, String)>,
) -> Response {
    match normalize_repo(&owner, &repo) {
        Some(repo) => refresh(&app, repo).await,
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn serve_file(app: &App, repo: String, file: &str) -> Response {
    if file == "nix-cache-info" {
        // Answer without touching the index, so Nix's startup probe is
        // instant; this also warms the index for the queries that follow.
        let cache = app.caches.get(&repo);
        tokio::spawn(async move { cache.index().await });
        return (
            [(header::CONTENT_TYPE, "text/x-nix-cache-info")],
            NIX_CACHE_INFO,
        )
            .into_response();
    }

    let cache = app.caches.get(&repo);
    if file == "_status" {
        cache.index().await;
        return Json(cache.status()).into_response();
    }
    let index = cache.index().await;
    if !index.available {
        return unavailable(&repo);
    }
    if file == "public-key" {
        return match &index.public_key {
            Some(key) => {
                ([(header::CONTENT_TYPE, "text/plain")], format!("{key}\n")).into_response()
            }
            None => (StatusCode::NOT_FOUND, "No public key configured\n").into_response(),
        };
    }
    let Some(hash) = file.strip_suffix(".narinfo") else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match index.narinfo(hash) {
        Some(narinfo) => {
            debug!(%repo, hash, "narinfo hit");
            ([(header::CONTENT_TYPE, "text/x-nix-narinfo")], narinfo).into_response()
        }
        None => {
            debug!(%repo, hash, "narinfo miss");
            StatusCode::NOT_FOUND.into_response()
        }
    }
}

async fn serve_nar(app: &App, repo: String, file: &str) -> Response {
    let cache = app.caches.get(&repo);
    let index = cache.index().await;
    if !index.available {
        return unavailable(&repo);
    }
    let Some(digest) = index.nar_digest(file).map(str::to_owned) else {
        debug!(%repo, file, "nar miss");
        return StatusCode::NOT_FOUND.into_response();
    };

    let resp = match cache
        .registry()
        .get(&repo, &format!("blobs/{digest}"), None, None)
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            warn!(%repo, file, %digest, error = %e, "nar fetch failed");
            return match e {
                FetchError::Missing(_) => StatusCode::NOT_FOUND,
                _ => StatusCode::BAD_GATEWAY,
            }
            .into_response();
        }
    };

    info!(%repo, file, bytes = resp.content_length(), "serving nar");
    let content_type = if file.ends_with(".xz") {
        "application/x-xz"
    } else {
        "application/x-nix-nar"
    };
    let mut builder = Response::builder().header(header::CONTENT_TYPE, content_type);
    if let Some(len) = resp.content_length() {
        builder = builder.header(header::CONTENT_LENGTH, len);
    }
    builder
        .body(Body::from_stream(resp.bytes_stream()))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// 503, not 404, while the index can't be loaded: Nix caches a 404 narinfo
/// for an hour, but treats a 5xx as transient.
fn unavailable(repo: &str) -> Response {
    debug!(repo, "index unavailable");
    (StatusCode::SERVICE_UNAVAILABLE, "cache index unavailable\n").into_response()
}

async fn refresh(app: &App, repo: String) -> Response {
    let entries = app.caches.get(&repo).force_refresh().await;
    Json(json!({ "refreshed": true, "repo": repo, "entries": entries })).into_response()
}

#[cfg(test)]
mod tests {
    use super::normalize_repo;

    #[test]
    fn repo_names() {
        assert_eq!(
            normalize_repo("pplanel", "pam_watchid").as_deref(),
            Some("pplanel/pam_watchid")
        );
        assert_eq!(
            normalize_repo("PPlanel", "Neovix").as_deref(),
            Some("pplanel/neovix")
        );
        assert_eq!(normalize_repo("pplanel", ".."), None);
        assert_eq!(normalize_repo("", "x"), None);
        assert_eq!(normalize_repo("a b", "x"), None);
    }
}

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
use axum::extract::{MatchedPath, Path, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use serde_json::json;
use tracing::field::Empty;
use tracing::{Instrument, Span, debug, info, info_span, warn};

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
    // Before telemetry: its exporter builds an HTTP client, and reqwest is
    // compiled here without a crypto provider of its own.
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| "could not install the rustls crypto provider")?;
    // Keep the guard to the end of `main`: dropping it flushes the exporters.
    let _telemetry = telemetry::init(telemetry::service!("nixcache-proxy"))?;

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
        .layer(middleware::from_fn(request_span))
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

/// One span per request, with OpenTelemetry's HTTP attribute names. The status
/// is what matters most here: Nix keeps a 404 narinfo for an hour and treats a
/// 5xx as transient. The span ends when the response headers are ready, so a
/// streamed NAR's body is not part of its duration.
async fn request_span(req: Request, next: Next) -> Response {
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(|path| path.as_str().to_owned());
    let span = info_span!(
        "request",
        http.request.method = %req.method(),
        url.path = %req.uri().path(),
        http.route = route.as_deref(),
        http.response.status_code = Empty,
        error.type = Empty,
        nixcache.repo = Empty,
    );
    let response = next.run(req).instrument(span.clone()).await;
    let status = response.status();
    span.record("http.response.status_code", status.as_u16());
    if status.is_server_error() {
        span.record("error.type", status.as_str());
    }
    response
}

/// Tag the current request span with the cache it is about.
fn note_repo(repo: &str) {
    Span::current().record("nixcache.repo", repo);
}

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
    note_repo(&repo);
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
    note_repo(&repo);
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
    note_repo(&repo);
    let entries = app.caches.get(&repo).force_refresh().await;
    Json(json!({ "refreshed": true, "repo": repo, "entries": entries })).into_response()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

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

    /// Collects what a `tracing` fmt subscriber writes.
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The only test that installs a subscriber: it is global, because the
    /// server runs on other threads, and a callsite's cached interest makes
    /// scoped subscribers unreliable next to parallel tests.
    #[tokio::test]
    async fn request_spans_carry_route_status_and_repo() {
        // reqwest is built without a crypto provider here; `main` installs this one.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let capture = Capture::default();
        tracing::subscriber::set_global_default(
            tracing_subscriber::fmt()
                .with_ansi(false)
                .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
                .with_writer({
                    let capture = capture.clone();
                    move || capture.clone()
                })
                .finish(),
        )
        .unwrap();

        let router = Router::new()
            .route(
                "/{owner}/{repo}/{file}",
                get(
                    |Path((owner, repo, _)): Path<(String, String, String)>| async move {
                        note_repo(&format!("{owner}/{repo}"));
                        StatusCode::SERVICE_UNAVAILABLE
                    },
                ),
            )
            .route("/ok", get(|| async { "fine" }))
            .layer(middleware::from_fn(request_span));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(axum::serve(listener, router).into_future());

        let unavailable = reqwest::get(format!("http://{addr}/pplanel/neovix/abc.narinfo"))
            .await
            .unwrap();
        assert_eq!(unavailable.status(), 503);
        let fine = reqwest::get(format!("http://{addr}/ok")).await.unwrap();
        assert_eq!(fine.status(), 200);

        let out = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        let lines: Vec<_> = out.lines().filter(|l| l.contains("request{")).collect();
        assert_eq!(lines.len(), 2, "one span per request: {out}");
        let (e503, e200) = (lines[0], lines[1]);
        for want in [
            "http.request.method=GET",
            "url.path=/pplanel/neovix/abc.narinfo",
            r#"http.route="/{owner}/{repo}/{file}""#,
            "http.response.status_code=503",
            r#"error.type="503""#,
            r#"nixcache.repo="pplanel/neovix""#,
        ] {
            assert!(e503.contains(want), "{want} missing: {e503}");
        }
        assert!(e200.contains("http.response.status_code=200"), "{e200}");
        assert!(
            !e200.contains("error.type"),
            "a 200 is not an error: {e200}"
        );
        assert!(!e200.contains("nixcache.repo"), "no repo on /ok: {e200}");
    }
}

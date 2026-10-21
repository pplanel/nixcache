//! The other Macs' Nix stores, as caches. Each Mac serves its own store with
//! harmonia (signed on the fly with that Mac's key) on loopback, and airshell
//! forwards it to the other Macs on a local port. Nix doesn't talk to those
//! forwards directly: an airshell forward accepts at once and then keeps
//! dialing for up to 30 seconds while the peer is out of reach, and Nix asks
//! the substituters before building anything. So it asks here, at
//! `/_peer/<host>`, and this answers within `timeout` either way.
//!
//! A peer that times out or errors is skipped for `backoff`: its narinfo
//! queries get a 404 at once, without dialing. A 404 is cached by Nix for an
//! hour (`narinfo-cache-negative-ttl`), so a path asked for while its peer was
//! away is built rather than copied until then; never worse than no peer.
//! `nix-cache-info` is answered here, so Nix never sees the cache fail to open.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;
use tracing::{debug, info, warn};

/// After cache.nixos.org and the GHCR caches (40): a peer is only asked for
/// what no public cache has, such as a private flake's outputs.
const NIX_CACHE_INFO: &str = "StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 50\n";

pub(crate) struct Peers {
    client: reqwest::Client,
    peers: BTreeMap<String, Peer>,
    timeout: Duration,
    backoff: Duration,
}

struct Peer {
    base: String,
    down_until: Mutex<Option<Instant>>,
}

impl Peer {
    fn down_for(&self) -> Option<Duration> {
        let until = (*self.down_until.lock().unwrap())?;
        until.checked_duration_since(Instant::now())
    }

    /// Returns whether it was up, so a peer going away is logged once.
    fn mark_down(&self, backoff: Duration) -> bool {
        let mut until = self.down_until.lock().unwrap();
        let was_up = until.is_none_or(|t| t <= Instant::now());
        *until = Some(Instant::now() + backoff);
        was_up
    }

    fn mark_up(&self) {
        *self.down_until.lock().unwrap() = None;
    }
}

/// A peer's name: lowercase letters, digits and `-`, as in a host name.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// A file name Nix asks for: `<hash>.narinfo`, `<hash>.nar[.xz|.zst]`. No
/// slashes or `..`, so it can't reach anything else on the peer.
fn valid_file(file: &str) -> bool {
    !file.is_empty()
        && !file.starts_with('.')
        && file
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

impl Peers {
    /// `spec` is `name=host:port` pairs, separated by spaces, commas or
    /// newlines; `host:port` is the airshell forward to that peer's harmonia.
    pub(crate) fn parse(spec: &str, timeout: Duration, backoff: Duration) -> Result<Self, String> {
        let mut peers = BTreeMap::new();
        for entry in spec.split([' ', ',', '\n']).filter(|e| !e.is_empty()) {
            let (name, addr) = entry
                .split_once('=')
                .filter(|(name, addr)| {
                    valid_name(name) && addr.parse::<std::net::SocketAddr>().is_ok()
                })
                .ok_or_else(|| format!("NIXCACHE_PEERS: {entry:?} is not name=ip:port"))?;
            let peer = Peer {
                base: format!("http://{addr}"),
                down_until: Mutex::new(None),
            };
            if peers.insert(name.to_owned(), peer).is_some() {
                return Err(format!("NIXCACHE_PEERS: {name:?} is listed twice"));
            }
        }
        let client = reqwest::Client::builder()
            .connect_timeout(timeout)
            .build()
            .map_err(|e| format!("peer client: {e}"))?;
        Ok(Self {
            client,
            peers,
            timeout,
            backoff,
        })
    }

    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.peers.keys().map(String::as_str)
    }

    pub(crate) fn status(&self) -> serde_json::Value {
        let peers: Vec<_> = self
            .peers
            .iter()
            .map(|(name, peer)| {
                json!({
                    "name": name,
                    "upstream": peer.base,
                    "skipped_for_secs": peer.down_for().map(|d| d.as_secs()),
                })
            })
            .collect();
        json!({
            "timeout_ms": self.timeout.as_millis(),
            "backoff_secs": self.backoff.as_secs(),
            "peers": peers,
        })
    }

    fn mark_down(&self, name: &str, peer: &Peer, error: &str) {
        if peer.mark_down(self.backoff) {
            warn!(
                peer = name,
                error,
                backoff_secs = self.backoff.as_secs(),
                "peer unreachable, skipping it"
            );
        }
    }

    /// `nix-cache-info` or `<hash>.narinfo`. Anything that isn't a narinfo
    /// from a reachable peer is a 404, within `timeout`.
    pub(crate) async fn file(&self, name: &str, file: &str) -> Response {
        let Some(peer) = self.peers.get(name) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        if file == "nix-cache-info" {
            return (
                [(header::CONTENT_TYPE, "text/x-nix-cache-info")],
                NIX_CACHE_INFO,
            )
                .into_response();
        }
        if !file.ends_with(".narinfo") || !valid_file(file) {
            return StatusCode::NOT_FOUND.into_response();
        }
        if let Some(left) = peer.down_for() {
            debug!(
                peer = name,
                file,
                skipped_for_secs = left.as_secs(),
                "peer skipped"
            );
            return StatusCode::NOT_FOUND.into_response();
        }

        let url = format!("{}/{file}", peer.base);
        let resp = self.client.get(&url).timeout(self.timeout).send().await;
        let error = match resp {
            Ok(resp) if resp.status() == StatusCode::OK => match resp.bytes().await {
                Ok(body) => {
                    peer.mark_up();
                    debug!(peer = name, file, "narinfo hit");
                    return ([(header::CONTENT_TYPE, "text/x-nix-narinfo")], body).into_response();
                }
                Err(e) => e.to_string(),
            },
            Ok(resp) if resp.status() == StatusCode::NOT_FOUND => {
                peer.mark_up();
                debug!(peer = name, file, "narinfo miss");
                return StatusCode::NOT_FOUND.into_response();
            }
            Ok(resp) => format!("status {}", resp.status()),
            Err(e) => e.to_string(),
        };
        self.mark_down(name, peer, &error);
        StatusCode::NOT_FOUND.into_response()
    }

    /// A NAR, streamed as the peer sends it. Only asked for after a narinfo
    /// from that peer, so no overall timeout: a NAR can be large. A failure is
    /// a 502, which Nix treats as transient.
    pub(crate) async fn nar(
        &self,
        name: &str,
        file: &str,
        query: Option<&str>,
        headers: &HeaderMap,
    ) -> Response {
        let Some(peer) = self.peers.get(name) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        if !valid_file(file) {
            return StatusCode::NOT_FOUND.into_response();
        }
        let mut url = format!("{}/nar/{file}", peer.base);
        if let Some(query) = query {
            url.push('?');
            url.push_str(query);
        }
        let mut req = self.client.get(&url);
        // The peer compresses when asked; the bytes are passed on untouched.
        if let Some(accept) = headers.get(header::ACCEPT_ENCODING) {
            req = req.header(header::ACCEPT_ENCODING, accept);
        }
        let resp = match req.send().await {
            Ok(resp) if resp.status() == StatusCode::OK => resp,
            Ok(resp) if resp.status() == StatusCode::NOT_FOUND => {
                return StatusCode::NOT_FOUND.into_response();
            }
            Ok(resp) => {
                warn!(peer = name, file, status = %resp.status(), "nar fetch failed");
                return StatusCode::BAD_GATEWAY.into_response();
            }
            Err(e) => {
                self.mark_down(name, peer, &e.to_string());
                return StatusCode::BAD_GATEWAY.into_response();
            }
        };

        info!(
            peer = name,
            file,
            bytes = resp.content_length(),
            "serving nar"
        );
        let mut builder = Response::builder();
        for name in [
            header::CONTENT_TYPE,
            header::CONTENT_ENCODING,
            header::CONTENT_LENGTH,
        ] {
            if let Some(value) = resp.headers().get(&name) {
                builder = builder.header(name, value);
            }
        }
        builder
            .body(Body::from_stream(resp.bytes_stream()))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Router;
    use axum::extract::{Path, RawQuery};
    use axum::routing::get;

    /// `Peers::parse` after the crypto provider `main` installs first: reqwest
    /// is built here without one of its own, and building a client without it
    /// panics. Tests run in any order, so each one installs it.
    fn parse(spec: &str, timeout: Duration, backoff: Duration) -> Result<Peers, String> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        Peers::parse(spec, timeout, backoff)
    }

    use super::*;

    #[test]
    fn spec() {
        let d = Duration::from_secs(1);
        let peers = parse("caladan=127.0.0.1:37522, imperium=127.0.0.1:37523\n", d, d).unwrap();
        assert_eq!(peers.names().collect::<Vec<_>>(), ["caladan", "imperium"]);
        assert_eq!(peers.peers["caladan"].base, "http://127.0.0.1:37522");
        assert!(parse("", d, d).unwrap().peers.is_empty());
        for bad in [
            "caladan",
            "caladan=nowhere",
            "Caladan=127.0.0.1:1",
            "a=127.0.0.1:1 a=127.0.0.1:2",
        ] {
            assert!(parse(bad, d, d).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn files() {
        assert!(valid_file("rsxvqxccq052y081pypbk0k95fbixjpq.narinfo"));
        assert!(valid_file("1abc.nar.zst"));
        for bad in [
            "",
            "..",
            ".narinfo",
            "../x.narinfo",
            "a/b.narinfo",
            "a b.narinfo",
        ] {
            assert!(!valid_file(bad), "{bad:?}");
        }
    }

    /// A stand-in for a peer's harmonia. `hang` makes every request stall
    /// like an airshell forward to a peer that isn't there.
    async fn fake_peer(hang: bool) -> (std::net::SocketAddr, Arc<AtomicUsize>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let counted = hits.clone();
        let router =
            Router::new()
                .route(
                    "/{file}",
                    get(move |Path(file): Path<String>| {
                        let hits = counted.clone();
                        async move {
                            hits.fetch_add(1, Ordering::SeqCst);
                            if hang {
                                tokio::time::sleep(Duration::from_secs(30)).await;
                            }
                            match file.as_str() {
                                "have.narinfo" => {
                                    (StatusCode::OK, "StorePath: /nix/store/have-x\n")
                                }
                                _ => (StatusCode::NOT_FOUND, ""),
                            }
                        }
                    }),
                )
                .route(
                    "/nar/{file}",
                    get(
                        |Path(file): Path<String>,
                         RawQuery(query): RawQuery,
                         headers: HeaderMap| async move {
                            let enc = headers
                                .get(header::ACCEPT_ENCODING)
                                .and_then(|v| v.to_str().ok())
                                .unwrap_or("-")
                                .to_owned();
                            (
                                [(header::CONTENT_ENCODING, "zstd")],
                                format!("{file} {} {enc}", query.unwrap_or_default()),
                            )
                        },
                    ),
                );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(axum::serve(listener, router).into_future());
        (addr, hits)
    }

    async fn body(resp: Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn passes_hits_misses_and_nars_through() {
        let (addr, _) = fake_peer(false).await;
        let d = Duration::from_secs(2);
        let peers = parse(&format!("caladan={addr}"), d, d).unwrap();

        let info = peers.file("caladan", "nix-cache-info").await;
        assert_eq!(info.status(), 200);
        assert!(body(info).await.contains("Priority: 50"));

        let hit = peers.file("caladan", "have.narinfo").await;
        assert_eq!(hit.status(), 200);
        assert_eq!(body(hit).await, "StorePath: /nix/store/have-x\n");
        assert_eq!(peers.file("caladan", "missing.narinfo").await.status(), 404);
        assert_eq!(peers.file("nobody", "have.narinfo").await.status(), 404);
        assert_eq!(peers.file("caladan", "other").await.status(), 404);

        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT_ENCODING, "zstd".parse().unwrap());
        let nar = peers
            .nar("caladan", "abc.nar", Some("hash=xyz"), &headers)
            .await;
        assert_eq!(nar.status(), 200);
        assert_eq!(nar.headers()[header::CONTENT_ENCODING], "zstd");
        assert_eq!(body(nar).await, "abc.nar hash=xyz zstd");
        // A miss doesn't mean the peer is gone.
        assert!(peers.peers["caladan"].down_for().is_none());
    }

    #[tokio::test]
    async fn a_hanging_peer_costs_one_timeout_then_is_skipped() {
        let (addr, hits) = fake_peer(true).await;
        let peers = parse(
            &format!("imperium={addr}"),
            Duration::from_millis(200),
            Duration::from_secs(60),
        )
        .unwrap();

        let started = Instant::now();
        assert_eq!(peers.file("imperium", "have.narinfo").await.status(), 404);
        let first = started.elapsed();
        assert!(first < Duration::from_secs(2), "waited {first:?}");
        assert!(peers.peers["imperium"].down_for().is_some());

        let started = Instant::now();
        for _ in 0..20 {
            assert_eq!(peers.file("imperium", "have.narinfo").await.status(), 404);
        }
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "skipped peers answer at once"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "only the first query dialed"
        );
        // Still answers nix-cache-info, so Nix keeps the cache.
        assert_eq!(peers.file("imperium", "nix-cache-info").await.status(), 200);
    }

    #[tokio::test]
    async fn a_dead_port_is_skipped_too() {
        // Bound then dropped: nothing listens there.
        let addr = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let d = Duration::from_millis(200);
        let peers = parse(&format!("caladan={addr}"), d, Duration::from_secs(60)).unwrap();
        assert_eq!(peers.file("caladan", "have.narinfo").await.status(), 404);
        assert!(peers.peers["caladan"].down_for().is_some());
        let nar = peers
            .nar("caladan", "abc.nar", None, &HeaderMap::new())
            .await;
        assert_eq!(nar.status(), 502);
    }
}

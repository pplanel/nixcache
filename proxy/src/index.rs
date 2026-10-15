//! Per-repo cache index: the `cache-index` blob that `lib/cache-builder.sh`
//! publishes, parsed once into lookup tables and swapped atomically on refresh.
//!
//! Requests never wait on the network once an index is loaded: a stale index
//! keeps being served while a single background task refreshes it. Only the
//! very first request for a repo with nothing on disk waits for the fetch.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwapOption;
use bytes::Bytes;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::registry::{FetchError, MANIFEST_ACCEPT, Registry};

/// How long to wait before retrying after a failed refresh.
const ERROR_BACKOFF: Duration = Duration::from_secs(30);

pub struct Index {
    narinfos: HashMap<Box<str>, Bytes>,
    /// NAR file name (`<hash>.nar.xz`, from the narinfo's `URL:`) → blob digest.
    nars: HashMap<Box<str>, Box<str>>,
    pub public_key: Option<String>,
    pub generated: Option<String>,
    /// Digest of the index blob this was parsed from. A refresh whose manifest
    /// still points at it skips the download.
    pub digest: Option<String>,
    /// False when the registry couldn't be reached and nothing was on disk.
    /// Lookups then answer 503, not 404: Nix caches a 404 narinfo for an hour
    /// (narinfo-cache-negative-ttl), so a boot without network would make it
    /// skip the cache and build everything locally.
    pub available: bool,
}

#[derive(Deserialize)]
struct RawIndex {
    #[serde(default)]
    entries: HashMap<String, RawEntry>,
    public_key: Option<String>,
    generated: Option<String>,
}

#[derive(Deserialize)]
struct RawEntry {
    narinfo: Option<String>,
    nar_digest: Option<String>,
}

#[derive(Deserialize)]
struct Manifest {
    layers: Vec<Layer>,
}

#[derive(Deserialize)]
struct Layer {
    digest: String,
}

impl Index {
    /// The registry has no cache-index for this repo: every lookup is a miss.
    fn missing() -> Self {
        Self {
            narinfos: HashMap::new(),
            nars: HashMap::new(),
            public_key: None,
            generated: None,
            digest: None,
            available: true,
        }
    }

    /// Nothing could be loaded yet: lookups can't tell a hit from a miss.
    fn unavailable() -> Self {
        Self {
            available: false,
            ..Self::missing()
        }
    }

    pub fn parse(blob: &[u8], digest: Option<String>) -> serde_json::Result<Self> {
        let raw: RawIndex = serde_json::from_slice(blob)?;
        let mut narinfos = HashMap::with_capacity(raw.entries.len());
        let mut nars = HashMap::with_capacity(raw.entries.len());
        for (hash, entry) in raw.entries {
            let Some(narinfo) = entry.narinfo else {
                continue;
            };
            if let Some(nar_digest) = entry.nar_digest {
                let url = narinfo.lines().find_map(|l| l.strip_prefix("URL: "));
                if let Some(file) = url.and_then(|u| u.trim().rsplit('/').next()) {
                    nars.insert(file.into(), nar_digest.into());
                }
            }
            narinfos.insert(hash.into(), Bytes::from(narinfo));
        }
        Ok(Self {
            narinfos,
            nars,
            public_key: raw.public_key.filter(|k| !k.is_empty()),
            generated: raw.generated,
            digest,
            available: true,
        })
    }

    pub fn narinfo(&self, store_hash: &str) -> Option<Bytes> {
        self.narinfos.get(store_hash).cloned()
    }

    pub fn nar_digest(&self, file: &str) -> Option<&str> {
        self.nars.get(file).map(|d| &**d)
    }

    pub fn len(&self) -> usize {
        self.narinfos.len()
    }
}

pub struct Shared {
    pub registry: Registry,
    pub ttl: Duration,
    /// Where indexes are persisted (`<dir>/<owner>--<repo>/cache-index.json`),
    /// so a restart serves the last index immediately, even offline.
    pub index_dir: Option<PathBuf>,
}

#[derive(Default)]
struct RefreshState {
    next: Option<Instant>,
    last_ok: Option<SystemTime>,
    last_error: Option<String>,
    missing: bool,
}

pub struct RepoCache {
    pub repo: String,
    shared: Arc<Shared>,
    index: ArcSwapOption<Index>,
    state: Mutex<RefreshState>,
    /// Serializes fetches, so a burst of cold requests makes one.
    fetch_lock: tokio::sync::Mutex<()>,
    refreshing: AtomicBool,
}

enum Fetched {
    Unchanged,
    New(Box<Index>, Bytes),
    Missing,
}

impl RepoCache {
    fn new(repo: String, shared: Arc<Shared>) -> Self {
        Self {
            repo,
            shared,
            index: ArcSwapOption::empty(),
            state: Mutex::new(RefreshState::default()),
            fetch_lock: tokio::sync::Mutex::new(()),
            refreshing: AtomicBool::new(false),
        }
    }

    pub fn registry(&self) -> &Registry {
        &self.shared.registry
    }

    /// The current index. Never blocks on the network unless nothing has been
    /// loaded yet, either from disk or the registry.
    pub async fn index(self: &Arc<Self>) -> Arc<Index> {
        if let Some(index) = self.index.load_full() {
            if self.is_stale() {
                self.spawn_refresh();
            }
            return index;
        }

        let _guard = self.fetch_lock.lock().await;
        if let Some(index) = self.index.load_full() {
            return index;
        }
        if let Some(index) = self.load_from_disk().await {
            let index = Arc::new(index);
            self.index.store(Some(index.clone()));
            // Disk may be behind the registry; check in the background.
            self.spawn_refresh();
            return index;
        }
        self.fetch_and_store().await;
        self.index
            .load_full()
            .unwrap_or_else(|| Arc::new(Index::unavailable()))
    }

    /// Fetch now, waiting for the result. Returns the entry count.
    pub async fn force_refresh(self: &Arc<Self>) -> usize {
        let _guard = self.fetch_lock.lock().await;
        self.fetch_and_store().await;
        self.index.load().as_ref().map_or(0, |i| i.len())
    }

    fn is_stale(&self) -> bool {
        self.state
            .lock()
            .unwrap()
            .next
            .is_none_or(|next| Instant::now() >= next)
    }

    fn spawn_refresh(self: &Arc<Self>) {
        if self.refreshing.swap(true, Ordering::AcqRel) {
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            {
                let _guard = this.fetch_lock.lock().await;
                if this.is_stale() {
                    this.fetch_and_store().await;
                }
            }
            this.refreshing.store(false, Ordering::Release);
        });
    }

    /// Fetch from the registry and update the index and refresh state.
    /// Caller holds `fetch_lock`.
    async fn fetch_and_store(&self) {
        let current = self.index.load_full();
        let known = current.as_ref().and_then(|i| i.digest.clone());
        let mut result = self.fetch(known.as_deref()).await;
        // A repo that had entries doesn't vanish; a 404/401 then is far more
        // likely a registry hiccup. Keep the last index rather than turn every
        // lookup into a miss.
        if matches!(result, Ok(Fetched::Missing)) && current.is_some_and(|i| i.len() > 0) {
            result = Err(FetchError::Status(reqwest::StatusCode::NOT_FOUND));
        }

        let mut state = self.state.lock().unwrap();
        state.next = Some(Instant::now() + self.shared.ttl);
        match result {
            Ok(Fetched::Unchanged) => {
                state.last_ok = Some(SystemTime::now());
                state.last_error = None;
            }
            Ok(Fetched::New(index, blob)) => {
                info!(repo = %self.repo, entries = index.len(), "index refreshed");
                let digest = index.digest.clone();
                self.index.store(Some(Arc::from(index)));
                state.last_ok = Some(SystemTime::now());
                state.last_error = None;
                state.missing = false;
                drop(state);
                self.save_to_disk(blob, digest);
            }
            Ok(Fetched::Missing) => {
                if !state.missing {
                    warn!(repo = %self.repo, "no cache-index in the registry; serving nothing");
                }
                self.index.store(Some(Arc::new(Index::missing())));
                state.last_ok = Some(SystemTime::now());
                state.last_error = None;
                state.missing = true;
            }
            Err(e) => {
                warn!(repo = %self.repo, error = %e, "index refresh failed");
                state.next = Some(Instant::now() + ERROR_BACKOFF);
                state.last_error = Some(e.to_string());
                // Keep serving whatever we had. With nothing, mark the repo
                // unavailable rather than make every request wait on the
                // registry.
                if self.index.load().is_none() {
                    self.index.store(Some(Arc::new(Index::unavailable())));
                }
            }
        }
    }

    async fn fetch(&self, known: Option<&str>) -> Result<Fetched, FetchError> {
        let registry = &self.shared.registry;
        let manifest = match registry
            .get(
                &self.repo,
                "manifests/cache-index",
                Some(MANIFEST_ACCEPT),
                Some(Duration::from_secs(30)),
            )
            .await
        {
            Ok(resp) => resp.bytes().await.map_err(FetchError::Http)?,
            Err(FetchError::Missing(_)) => return Ok(Fetched::Missing),
            Err(e) => return Err(e),
        };
        let Some(layer) = serde_json::from_slice::<Manifest>(&manifest)
            .ok()
            .and_then(|m| m.layers.into_iter().next())
        else {
            return Ok(Fetched::Missing);
        };
        if known == Some(layer.digest.as_str()) {
            return Ok(Fetched::Unchanged);
        }

        let blob = registry
            .get(
                &self.repo,
                &format!("blobs/{}", layer.digest),
                None,
                Some(Duration::from_secs(120)),
            )
            .await?
            .bytes()
            .await
            .map_err(FetchError::Http)?;
        match Index::parse(&blob, Some(layer.digest)) {
            Ok(index) => Ok(Fetched::New(Box::new(index), blob)),
            Err(e) => {
                warn!(repo = %self.repo, error = %e, "cache-index blob is not valid JSON");
                Ok(Fetched::Missing)
            }
        }
    }

    fn disk_dir(&self) -> Option<PathBuf> {
        let dir = self.shared.index_dir.as_ref()?;
        Some(dir.join(self.repo.replace('/', "--")))
    }

    async fn load_from_disk(&self) -> Option<Index> {
        let dir = self.disk_dir()?;
        let blob = tokio::fs::read(dir.join("cache-index.json")).await.ok()?;
        let digest = tokio::fs::read_to_string(dir.join("cache-index.digest"))
            .await
            .ok()
            .map(|d| d.trim().to_owned());
        let index = Index::parse(&blob, digest).ok()?;
        info!(repo = %self.repo, entries = index.len(), "index loaded from disk");
        Some(index)
    }

    fn save_to_disk(&self, blob: Bytes, digest: Option<String>) {
        let Some(dir) = self.disk_dir() else { return };
        let repo = self.repo.clone();
        tokio::spawn(async move {
            let write = async {
                tokio::fs::create_dir_all(&dir).await?;
                write_atomic(dir.join("cache-index.json"), &blob).await?;
                match digest {
                    Some(d) => write_atomic(dir.join("cache-index.digest"), d.as_bytes()).await,
                    None => Ok(()),
                }
            };
            if let Err(e) = write.await {
                warn!(%repo, error = %e, "could not persist index");
            }
        });
    }

    pub fn status(&self) -> Value {
        let index = self.index.load();
        let state = self.state.lock().unwrap();
        let unix = |t: SystemTime| t.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs());
        json!({
            "repo": self.repo,
            "loaded": index.is_some(),
            "available": index.as_ref().is_some_and(|i| i.available),
            "index_entries": index.as_ref().map_or(0, |i| i.len()),
            "index_generated": index.as_ref().and_then(|i| i.generated.clone()),
            "index_digest": index.as_ref().and_then(|i| i.digest.clone()),
            "missing": state.missing,
            "last_ok": state.last_ok.and_then(unix),
            "next_refresh_in": state.next.map(|n| n.saturating_duration_since(Instant::now()).as_secs()),
            "last_error": state.last_error,
        })
    }
}

async fn write_atomic(path: PathBuf, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    tokio::fs::write(&tmp, data).await?;
    tokio::fs::rename(&tmp, &path).await
}

/// All repos seen so far, created on first request.
pub struct Caches {
    shared: Arc<Shared>,
    repos: RwLock<HashMap<String, Arc<RepoCache>>>,
}

impl Caches {
    pub fn new(shared: Shared) -> Self {
        Self {
            shared: Arc::new(shared),
            repos: RwLock::new(HashMap::new()),
        }
    }

    pub fn shared(&self) -> &Shared {
        &self.shared
    }

    /// `repo` must already be validated and lowercased.
    pub fn get(&self, repo: &str) -> Arc<RepoCache> {
        if let Some(cache) = self.repos.read().unwrap().get(repo) {
            return cache.clone();
        }
        self.repos
            .write()
            .unwrap()
            .entry(repo.to_owned())
            .or_insert_with(|| Arc::new(RepoCache::new(repo.to_owned(), self.shared.clone())))
            .clone()
    }

    pub fn all(&self) -> Vec<Arc<RepoCache>> {
        let mut all: Vec<_> = self.repos.read().unwrap().values().cloned().collect();
        all.sort_by(|a, b| a.repo.cmp(&b.repo));
        all
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_builds_lookup_tables() {
        let blob = br#"{
            "version": 1,
            "public_key": "x-cache-1:abc=",
            "generated": "2026-10-15T00:00:00Z",
            "entries": {
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa": {
                    "name": "hello",
                    "narinfo": "StorePath: /nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello\nURL: nar/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.nar.xz\nCompression: xz\n",
                    "nar_digest": "sha256:1111",
                    "nar_size": 10
                },
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb": {"name": "no-narinfo"}
            },
            "gc_roots": []
        }"#;
        let index = Index::parse(blob, Some("sha256:idx".into())).unwrap();
        assert_eq!(index.len(), 1);
        assert!(index.narinfo("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").is_some());
        assert!(index.narinfo("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").is_none());
        assert_eq!(
            index.nar_digest("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.nar.xz"),
            Some("sha256:1111")
        );
        assert_eq!(index.public_key.as_deref(), Some("x-cache-1:abc="));
    }
}

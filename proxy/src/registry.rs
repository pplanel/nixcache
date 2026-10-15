//! Minimal OCI registry client for `<registry>/v2/<owner>/<repo>/nix-cache`.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use reqwest::{Client, Response, StatusCode, header};
use serde::Deserialize;

pub const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.manifest.v1+json";

#[derive(Debug)]
pub enum FetchError {
    /// 404, or 401/403 after a fresh token: the package doesn't exist or isn't public.
    Missing(StatusCode),
    Status(StatusCode),
    Http(reqwest::Error),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Missing(s) => write!(f, "not found ({s})"),
            FetchError::Status(s) => write!(f, "registry returned {s}"),
            FetchError::Http(e) => write!(f, "{e}"),
        }
    }
}

struct Token {
    value: String,
    expires: Instant,
}

pub struct Registry {
    http: Client,
    host: String,
    github_token: Option<String>,
    tokens: Mutex<HashMap<String, Token>>,
}

impl Registry {
    pub fn new(host: String, github_token: Option<String>) -> reqwest::Result<Self> {
        let http = Client::builder()
            .user_agent(concat!("nixcache-proxy/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            // Per-read, so a stalled NAR stream fails instead of hanging Nix,
            // while large NARs can still take as long as they need overall.
            .read_timeout(Duration::from_secs(60))
            .pool_idle_timeout(Duration::from_secs(90))
            .build()?;
        Ok(Self {
            http,
            host,
            github_token,
            tokens: Mutex::new(HashMap::new()),
        })
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    /// GET `/v2/<repo>/nix-cache/<path>`. Retries once with a fresh token on 401.
    /// `timeout` bounds the whole response, body included; leave it `None` for
    /// streamed NARs.
    pub async fn get(
        &self,
        repo: &str,
        path: &str,
        accept: Option<&str>,
        timeout: Option<Duration>,
    ) -> Result<Response, FetchError> {
        let url = format!("https://{}/v2/{}/nix-cache/{}", self.host, repo, path);
        let mut status = StatusCode::UNAUTHORIZED;
        for attempt in 0..2 {
            let mut req = self.http.get(&url);
            if let Some(accept) = accept {
                req = req.header(header::ACCEPT, accept);
            }
            if let Some(timeout) = timeout {
                req = req.timeout(timeout);
            }
            if let Some(token) = self.token(repo, attempt > 0).await {
                req = req.bearer_auth(token);
            }
            let resp = req.send().await.map_err(FetchError::Http)?;
            status = resp.status();
            if status.is_success() {
                return Ok(resp);
            }
            if status != StatusCode::UNAUTHORIZED {
                break;
            }
        }
        Err(match status {
            StatusCode::NOT_FOUND | StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                FetchError::Missing(status)
            }
            s => FetchError::Status(s),
        })
    }

    /// A pull token for `repo`, cached until shortly before it expires.
    async fn token(&self, repo: &str, force: bool) -> Option<String> {
        if !force {
            let tokens = self.tokens.lock().unwrap();
            if let Some(t) = tokens.get(repo).filter(|t| t.expires > Instant::now()) {
                return Some(t.value.clone());
            }
        }

        #[derive(Deserialize)]
        struct TokenResponse {
            token: Option<String>,
            access_token: Option<String>,
            expires_in: Option<u64>,
        }

        let url = format!(
            "https://{host}/token?scope=repository:{repo}/nix-cache:pull&service={host}",
            host = self.host
        );
        let mut req = self.http.get(url).timeout(Duration::from_secs(10));
        if let Some(gh) = &self.github_token {
            req = req.basic_auth("token", Some(gh));
        }
        let exchanged = match req.send().await.and_then(Response::error_for_status) {
            Ok(resp) => resp.json::<TokenResponse>().await.ok(),
            Err(_) => None,
        };
        let (value, ttl) = match exchanged {
            Some(r) => (r.token.or(r.access_token)?, r.expires_in.unwrap_or(300)),
            // Some registries accept the GitHub token itself as a bearer.
            None => (self.github_token.clone()?, 60),
        };

        let expires = Instant::now() + Duration::from_secs(ttl.saturating_sub(30).max(30));
        self.tokens.lock().unwrap().insert(
            repo.to_owned(),
            Token {
                value: value.clone(),
                expires,
            },
        );
        Some(value)
    }
}

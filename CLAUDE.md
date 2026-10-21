# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Shared tooling that turns any of pplanel's GitHub repos (pam_watchid, neovix, airshell, …) into a GHCR-backed Nix binary cache, in the [nixcache-oci](https://github.com/cmspam/nixcache-oci) format. Project repos don't vendor the scripts. They call the reusable workflow, and clients run the proxy from this flake. See README.md for the producer and consumer setup.

## Commands

```bash
cd proxy && cargo test && cargo clippy --all-targets   # proxy unit tests and lints
NIXCACHE_PORT=37600 RUST_LOG=nixcache_proxy=debug cargo run   # run the proxy locally
curl localhost:37600/_status                           # every cache's index state
curl localhost:37600/pplanel/neovix/_status            # one cache (loads it on first request)
curl -X POST localhost:37600/pplanel/neovix/_refresh   # force an index refetch
nix build .#nixcache-proxy                             # from the repo root
nix flake check                                        # evaluate flake outputs, including darwinModules.default
bash -n lib/cache-builder.sh                           # syntax-check the builder
```

The flake only sees files git knows about, so `git add -N` new files before `nix build`.

`lib/cache-builder.sh` is a library: it's sourced, and its `full_pipeline` is called (see `.github/workflows/publish-cache.yml`). It needs GNU coreutils and bash 5 (`stat -c`, `sed -i`, `mapfile`), so on macOS run it inside the same `nix shell nixpkgs#bash nixpkgs#coreutils nixpkgs#gnugrep nixpkgs#gnused nixpkgs#jq nixpkgs#curl nixpkgs#xz nixpkgs#python3` that the workflow uses. `start_self_substituter` writes to `~/.config/nix/nix.conf` when `/etc/nix/nix.conf` isn't writable, so try it locally with a throwaway `HOME`.

## Architecture

Three parts share one on-registry format, and changing one side means checking the other two:

- **Producer**: `lib/cache-builder.sh`, run by the reusable workflow `.github/workflows/publish-cache.yml` (`workflow_call`) in the *caller's* repo. Callers pin it with `@main`, and the workflow checks out this repo's `main` into `.nixcache`, so anything merged to `main` goes live for every caller on their next push. Each built output is rooted with `--out-link` under `~/.local/state/nixcache/gcroots/<repo>/` (`NIXCACHE_GCROOTS_DIR` overrides), one link per output overwritten by the next build: a persistent runner is also a peer store, and the path must survive a GC until the other Macs have pulled it.
- **Consumer**: `proxy/` (Rust, `packages.nixcache-proxy`, also exposed as `cache-proxy`, its old name). One process serves every cache, and the repo comes from the URL path: the substituter is `http://127.0.0.1:<port>/<owner>/<repo>`, and Nix appends `/nix-cache-info`, `/<hash>.narinfo` and `/nar/<file>`. `/<owner>/<repo>/{public-key,_status}`, `POST /<owner>/<repo>/_refresh` and a global `/_status` also exist, and `/_peer/<name>` for peers (below). `NIXCACHE_REPO` is served at the root too, which the CI self-substituter relies on.
- **Deployment**: `flake.nix` also has a generic `darwinModules.default` (a launchd daemon plus `nix.settings` substituters). The Macs don't use it, because they're Determinate-managed (`nix.enable = false`, so `nix.settings` is inert). Their single socket-activated daemon, on 127.0.0.1:37510, comes from `modules/darwin/nixcache.nix` in the nix-darwin repo.

### Registry layout (the contract)

Everything lives under the image `ghcr.io/<owner>/<repo>/nix-cache`:
- Each NAR is pushed as an xz-compressed blob, addressed by sha256 digest.
- The tag `cache-index` is a manifest whose `layers[0]` is a JSON blob: `{version, repo, registry, image, generated, public_key, entries: {<32-char store hash>: {name, narinfo, nar_digest, nar_size, added}}, gc_roots}`.
- The proxy serves a NAR by matching the file name in its narinfo's `URL: nar/<hash>.nar.xz` line and streaming that entry's `nar_digest` blob.
- Updates read, merge and rewrite the whole index. Entries are never removed.

### Proxy behaviour that's easy to break

- `src/index.rs` parses each repo's index once into `hash → narinfo` and `nar file → digest` maps and swaps it atomically (`ArcSwap`). A stale index keeps being served while one background task refreshes it (TTL `NIXCACHE_INDEX_TTL`, default 300s). A refresh skips the blob download when the manifest's layer digest hasn't changed.
- Indexes are persisted to `<dir>/<owner>--<repo>/cache-index.{json,digest}`, where `<dir>` is `NIXCACHE_INDEX_DIR`, then `$CACHE_DIRECTORY`, then `~/.cache/nixcache-proxy`. Only a repo with nothing on disk and not in `NIXCACHE_PRELOAD` makes a request wait on GHCR.
- **404 vs 503 matters.** Nix caches a 404 narinfo for an hour (`narinfo-cache-negative-ttl`). So a 404 means "not in this cache": the registry has no `cache-index`, or the hash isn't in it. When the index couldn't be loaded at all, the answer is 503, which Nix treats as transient. A 404/401 from the registry for a repo that already had entries keeps the last index instead of wiping it.
- There is no upstream fallback: a miss is a 404, because Nix already queries cache.nixos.org itself.
- **Peers** (`src/peer.rs`, `NIXCACHE_PEERS`, routes `/_peer/<name>/…`): other Macs' stores, through a local port each (an airshell forward to that Mac's harmonia). An airshell forward accepts at once and then dials for up to 30 s when the peer is away, so a narinfo gets a short timeout (`NIXCACHE_PEER_TIMEOUT_MS`, 2000), and a peer that fails is skipped for `NIXCACHE_PEER_BACKOFF` (60 s) with instant 404s. A 404, not a 503, on purpose: a 503 makes Nix retry with backoff on every query, the wait this exists to avoid; the cost is the hour-long negative cache for paths asked for while the peer was away. `nix-cache-info` is answered locally (priority 50). NARs stream with no overall timeout, passing the query string (harmonia's `?hash=`) and `Accept-Encoding`/`Content-Encoding` through. Tests that build a `Peers` must install the rustls provider first (the `parse` helper in its tests): reqwest has none of its own here, and building a client without one panics.
- `src/registry.rs` handles GHCR: one pull token per repo, refetched on 401. NARs are streamed, and GHCR's blob redirect is followed without forwarding the token.
- With `NIXCACHE_LAUNCHD_SOCKET=<Sockets key>`, the listening socket comes from launchd (`launch_activate_socket`, macOS only). Connections then queue while the daemon starts or restarts instead of being refused. A daemon that can't start leaves them hanging, so health checks must be HTTP requests with a timeout, not `nc -z`.

### Builder pipeline (`full_pipeline`)

1. **Self-substituter**: builds `#nixcache-proxy` from the `.nixcache` checkout (`NIXCACHE_PROXY_BIN` skips the build), starts it on a free port with `NIXCACHE_REPO` set, and appends a fenced block to `nix.conf` so the build can reuse earlier runs' outputs. A `trap ... EXIT` removes the block and kills the proxy. This matters on self-hosted runners, whose `nix.conf` and ports outlive the job. If the proxy can't be built or started, the pipeline goes on without it.
2. **Discover**: finds `packages.<system>`, `nixosConfigurations.*.toplevel` and `devShells.<system>` for the runner's system. The flake is evaluated as `git+file://` (adding `shallow=1` for shallow checkouts and `dir=` for subdirectories), not `path:`, so store paths match what consumers get from `github:owner/repo`.
3. **Build**: plain `nix build --accept-flake-config`.
4. **Filter**: uploads only paths in the closure that have **no signatures** (built locally) and aren't already in the index. Anything substituted from a cache carries that cache's signature.
5. **Export**: signs each path, then `nix-store --dump | xz -1`, and generates the narinfo itself instead of using `nix copy`.
6. **Upload**: pushes the blobs, merges the index, and pushes the `cache-index` manifest.

Large state (the existing index, upload receipts) is passed through files and environment variables, not argv, to stay under ARG_MAX.

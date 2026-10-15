# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Shared tooling that turns any of pplanel's GitHub repos (pam_watchid, neovix, airshell, …) into a GHCR-backed Nix binary cache, in the [nixcache-oci](https://github.com/cmspam/nixcache-oci) format. Project repos don't vendor the scripts. They call the reusable workflow, and clients run the proxy from this flake. See README.md for the producer and consumer setup.

## Commands

There is no test suite or linter. Check changes like this:

```bash
nix build .#cache-proxy                      # package the proxy (copies proxy/main.py to bin/nixcache-proxy)
nix flake check                              # evaluate flake outputs, including darwinModules.default
NIXCACHE_REPO=pplanel/pam_watchid python3 proxy/main.py   # run the proxy locally (default port 37515)
curl localhost:37515/_status                 # index size, TTL, repo, upstreams
curl -X POST localhost:37515/_refresh        # force an index refetch
bash -n lib/cache-builder.sh                 # syntax-check the builder
```

`lib/cache-builder.sh` is a library: it's sourced, and its `full_pipeline` is called (see `.github/workflows/publish-cache.yml`). It needs GNU coreutils and bash 5 (`stat -c`, `sed -i`, `mapfile`), so on macOS run it inside the same `nix shell nixpkgs#bash nixpkgs#coreutils nixpkgs#gnugrep nixpkgs#gnused nixpkgs#jq nixpkgs#curl nixpkgs#xz nixpkgs#python3` that the workflow uses.

## Architecture

Three parts share one on-registry format, and changing one side means checking the other two:

- **Producer**: `lib/cache-builder.sh`, run by the reusable workflow `.github/workflows/publish-cache.yml` (`workflow_call`) in the *caller's* repo. Callers pin it with `@main`, and the workflow checks out this repo's `main` into `.nixcache`, so anything merged to `main` goes live for every caller on their next push.
- **Consumer**: `proxy/main.py`, a stdlib-only Python HTTP server that speaks Nix's binary-cache protocol (`/nix-cache-info`, `/<hash>.narinfo`, `/nar/<file>`) and translates it to GHCR OCI calls. One process serves exactly one cache repo (`NIXCACHE_REPO`), so clients run one proxy per cache, each on its own port.
- **Deployment**: `flake.nix` exposes `packages.cache-proxy`, `apps.cache-proxy` and a generic `darwinModules.default` (a launchd daemon plus `nix.settings` substituters). The Macs themselves don't use this module, because they're Determinate-managed (`nix.enable = false`, so `nix.settings` is inert). The real daemons (`nixcache-proxy-{pam-watchid,neovix,airshell}` on ports 37515–37517) come from `modules/darwin/nixcache.nix` in the nix-darwin repo.

### Registry layout (the contract)

Everything lives under the image `ghcr.io/<owner>/<repo>/nix-cache`:
- Each NAR is pushed as an xz-compressed blob, addressed by sha256 digest.
- The tag `cache-index` is a manifest whose `layers[0]` is a JSON blob: `{version, repo, registry, image, generated, public_key, entries: {<32-char store hash>: {name, narinfo, nar_digest, nar_size, added}}, gc_roots}`.
- The proxy answers narinfo requests straight from `entries[hash].narinfo`, with no per-request network call. It finds a NAR by scanning narinfos for a `URL: nar/<hash>.nar.xz` line and streams the matching `nar_digest` blob. On a miss it falls back to `NIXCACHE_UPSTREAM` (default `https://cache.nixos.org`).
- The index is cached in memory for `NIXCACHE_INDEX_TTL` seconds (default 300) and written to disk (`NIXCACHE_INDEX_DIR`, then `$CACHE_DIRECTORY`, then `~/.cache/nixcache-proxy/<owner>--<repo>`) so it survives offline restarts.
- Updates read, merge and rewrite the whole index. Entries are never removed.

### Builder pipeline (`full_pipeline`)

1. **Self-substituter**: starts `proxy/main.py` on a free port, with `NIXCACHE_UPSTREAM=""`, and appends a fenced block to `nix.conf` so the build can reuse earlier runs' outputs. A `trap ... EXIT` removes the block and kills the proxy. This matters on self-hosted runners, whose `nix.conf` and ports outlive the job.
2. **Discover**: finds `packages.<system>`, `nixosConfigurations.*.toplevel` and `devShells.<system>` for the runner's system. The flake is evaluated as `git+file://` (adding `shallow=1` for shallow checkouts and `dir=` for subdirectories), not `path:`, so store paths match what consumers get from `github:owner/repo`.
3. **Build**: plain `nix build --accept-flake-config`.
4. **Filter**: uploads only paths in the closure that have **no signatures** (built locally) and aren't already in the index. Anything substituted from a cache carries that cache's signature.
5. **Export**: signs each path, then `nix-store --dump | xz -1`, and generates the narinfo itself instead of using `nix copy`.
6. **Upload**: pushes the blobs, merges the index, and pushes the `cache-index` manifest.

Large state (the existing index, upload receipts) is passed through files and environment variables, not argv, to stay under ARG_MAX.

## Rust proxy (`feat/unified_proxy`)

`proxy/src/` is a Rust rewrite (`packages.nixcache-proxy`) that replaces the one-daemon-per-repo setup. A single process serves every cache, and the repo comes from the URL path: the substituter is `http://127.0.0.1:37515/<owner>/<repo>`, and Nix appends `/nix-cache-info`, `/<hash>.narinfo` and `/nar/<file>`. `/<owner>/<repo>/{public-key,_status}`, `POST /<owner>/<repo>/_refresh` and a global `/_status` also exist. If `NIXCACHE_REPO` is set, that repo is served at the root too, as the Python proxy did.

```bash
cd proxy && cargo test && cargo clippy --all-targets
NIXCACHE_PORT=37600 RUST_LOG=nixcache_proxy=debug cargo run
nix build .#nixcache-proxy      # from the repo root; untracked files are invisible to the flake, so `git add -N` them first
```

- `src/index.rs`: each repo's index is parsed once into `hash → narinfo` and `nar file → digest` maps and swapped atomically (`ArcSwap`). A stale index keeps being served while a single background task refreshes it. A refresh skips the blob download when the manifest's layer digest hasn't changed. Indexes are persisted to `<index dir>/<owner>--<repo>/cache-index.{json,digest}`, the same path the Python proxy used under `~/.cache/nixcache-proxy`. Only a repo that has never been seen, with nothing on disk, makes a request wait on GHCR.
- `src/registry.rs` handles GHCR: one pull token per repo, refetched on 401. NARs are streamed, and GHCR's blob redirect is followed without forwarding the token.
- Unlike `main.py`, there is no upstream fallback: a miss is a 404, because Nix already queries cache.nixos.org itself.
- `NIXCACHE_LAUNCHD_SOCKET=<Sockets key>` makes it take its listening socket from launchd (`launch_activate_socket`). Then connections queue while the daemon is booting or restarting instead of being refused, which is what caused the bursts of failed connections with the per-repo daemons.

`flake.nix` still packages `proxy/main.py` as `cache-proxy`, and the builder's self-substituter still runs `proxy/main.py` by path. Keep both working until those callers move.

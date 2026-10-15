# nixcache

Shared tooling for turning any of my GitHub repos into a [nixcache-oci](https://github.com/cmspam/nixcache-oci)
GHCR-backed Nix binary cache. Single source of truth so individual project repos
(pam_watchid, neovix, …) don't each vendor copies of the build + proxy scripts.

## What's here

- `lib/cache-builder.sh` — builds a flake's outputs and pushes signed NARs to
  `ghcr.io/<owner>/<repo>/nix-cache` as OCI blobs.
- `proxy/` — `nixcache-proxy`, a local HTTP proxy (Rust) bridging Nix's binary-cache
  protocol to GHCR. One instance serves every cache, chosen by URL path.
- `.github/workflows/publish-cache.yml` — **reusable** workflow (`workflow_call`).
- `flake.nix` — `nixcache-proxy` package (alias `cache-proxy`), `apps.cache-proxy`,
  and a generic `darwinModules.default`.

## Publish a repo's builds (producer side)

1. Generate a signing key and set it as the repo's `NIX_SIGNING_KEY` secret:
   ```bash
   nix-store --generate-binary-cache-key <repo>-cache-1 secret.key public.key
   gh secret set NIX_SIGNING_KEY -R <owner>/<repo> < secret.key
   ```
   Commit `public.key`'s contents as `public-key.txt` in that repo.
2. Add a thin caller workflow `.github/workflows/publish-cache.yml`:
   ```yaml
   name: Publish Binary Cache
   on:
     push:
       branches: [main]
       tags: ['v*']
     workflow_dispatch:
   permissions:
     contents: read
     packages: write
   jobs:
     publish:
       uses: pplanel/nixcache/.github/workflows/publish-cache.yml@main
       secrets: inherit
       # with:
       #   runs-on: ubuntu-latest   # for Linux-only flakes
   ```
3. Push. After the first run, make the repo's `nix-cache` GHCR package **public**
   so clients pull anonymously.

By default the workflow discovers and builds `packages`, `devShells`, and
`nixosConfigurations` for the runner's system from the repo root (`config-dir: "."`).

## Consume a cache (client side)

Run one proxy and point a substituter per cache at it:
`http://127.0.0.1:<port>/<owner>/<repo>`. If `NIXCACHE_REPO` is set, that repo is
also served at `http://127.0.0.1:<port>`. List repos in `NIXCACHE_PRELOAD` to load
their indexes at startup. On standard nix-darwin use `darwinModules.default`
(one cache, served at the root):

```nix
{
  inputs.nixcache.url = "github:pplanel/nixcache";
  # modules:
  nixcache.darwinModules.default
  { services.nixcache-proxy = { enable = true; repo = "pplanel/pam_watchid"; publicKey = "pam-watchid-cache-1:…"; }; }
}
```

On Determinate-managed systems (`nix.enable = false`), `nix.settings` are inert;
register `http://127.0.0.1:<port>/<owner>/<repo>` in `/etc/nix/nix.custom.conf` and
run the proxy as a launchd daemon directly (see pplanel's nix-darwin
`modules/darwin/nixcache.nix`, which also hands it a launchd socket through
`NIXCACHE_LAUNCHD_SOCKET`). The nix daemon only reads that file at startup, so
restart it after changing the list.

Run manually: `nix run github:pplanel/nixcache` (port `NIXCACHE_PORT`, default 37515).

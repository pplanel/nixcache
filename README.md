# nixcache

Shared tooling for turning any of my GitHub repos into a [nixcache-oci](https://github.com/cmspam/nixcache-oci)
GHCR-backed Nix binary cache. Single source of truth so individual project repos
(pam_watchid, neovix, …) don't each vendor copies of the build + proxy scripts.

## What's here

- `lib/cache-builder.sh` — builds a flake's outputs and pushes signed NARs to
  `ghcr.io/<owner>/<repo>/nix-cache` as OCI blobs.
- `proxy/main.py` — local HTTP proxy bridging Nix's binary-cache protocol to GHCR.
- `.github/workflows/publish-cache.yml` — **reusable** workflow (`workflow_call`).
- `flake.nix` — `cache-proxy` package, `apps.cache-proxy`, and a generic
  `darwinModules.default`.

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

Run the proxy (one instance per cache, each on its own port, `NIXCACHE_REPO` set
to the cache's repo) and point a substituter at it. On standard nix-darwin use
`darwinModules.default`:

```nix
{
  inputs.nixcache.url = "github:pplanel/nixcache";
  # modules:
  nixcache.darwinModules.default
  { services.nixcache-proxy = { enable = true; repo = "pplanel/pam_watchid"; publicKey = "pam-watchid-cache-1:…"; }; }
}
```

On Determinate-managed systems (`nix.enable = false`), `nix.settings` are inert;
register `http://127.0.0.1:<port>` in `/etc/nix/nix.custom.conf` and run the proxy
as a launchd daemon directly (see pplanel's nix-darwin `modules/darwin/nixcache.nix`).

Run manually: `nix run github:pplanel/nixcache#cache-proxy` with
`NIXCACHE_REPO=<owner>/<repo>` set.

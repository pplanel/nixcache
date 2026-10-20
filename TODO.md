# TODO

## Make "Build and publish cache" faster

Measured on airshell's cache run 39340749288 (2026-10-20, runner `imperium-airshell`): the job
took ~12 min, all of it in the "Build and publish cache" step. From the `>>>` markers:

| phase | time |
|---|---|
| `nix shell nixpkgs#…` tools (workflow, before the script) | ~2m50s: unpacks `nixpkgs-unstable` (35 s), fetches 74 paths / 293 MiB |
| build `airshell-connect` | 2m53s |
| build `airshell-proxy` | 3m00s |
| build `airshell-tui` | 1m13s |
| build devShell | 1s |
| signatures + export 3 NARs | ~7s |
| upload 3 NARs (~4 MB) | 1m41s |
| cache index | 4s |

1. **Build the outputs in one `nix build`.** `build_outputs` (`lib/cache-builder.sh`, the
   `for ref in "${refs[@]}"` loop) builds one ref at a time; connect and proxy take ~3 min each,
   as if each compiles the caller's workspace on its own. Passing all refs to a single
   `nix build` lets Nix run them in parallel and share common derivations. Check the caller's
   flake too: if each binary is its own derivation recompiling everything, a shared deps step
   (crane's `buildDepsOnly`) helps more. Estimate: ~7 min of builds down to ~3.
2. **Pin the tools of `nix shell`.** `.github/workflows/publish-cache.yml` runs
   `nix shell nixpkgs#bash nixpkgs#coreutils … nixpkgs#python3`; `nixpkgs` resolves through the
   global registry to whatever `nixpkgs-unstable` is today, so ~300 MB of tools come down again
   whenever unstable moves. Take them from this flake's locked nixpkgs (`--inputs-from` this
   checkout, or a `tools` package in `flake.nix`) so they stay in the runner's store. ~2–3 min.
3. **Measure the upload again before touching it.** 1m41s for ~4 MB (27–60 KB/s per NAR) is slow,
   but that run fell in a window when imperium's network was being taken down on purpose
   (airshell AWDL tests). Re-time `oci_push_blob` on a quiet network first.

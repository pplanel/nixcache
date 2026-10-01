{
  description = "nixcache — shared tooling for GHCR-backed Nix binary caches (nixcache-oci)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  };

  outputs = {
    self,
    nixpkgs,
  }: let
    systems = ["aarch64-darwin" "x86_64-darwin" "aarch64-linux" "x86_64-linux"];
    forAllSystems = nixpkgs.lib.genAttrs systems;
  in {
    # The nixcache-oci proxy: bridges Nix's binary-cache protocol to a
    # GHCR-hosted cache. The repo it serves is chosen at runtime via
    # NIXCACHE_REPO, so one build of this package serves any number of caches.
    packages = forAllSystems (system: let
      pkgs = nixpkgs.legacyPackages.${system};
    in {
      cache-proxy = pkgs.stdenv.mkDerivation {
        pname = "nixcache-proxy";
        version = "0.1.0";
        src = ./proxy;
        nativeBuildInputs = [pkgs.python3];
        installPhase = ''
          mkdir -p $out/bin
          cp main.py $out/bin/nixcache-proxy
          chmod +x $out/bin/nixcache-proxy
          patchShebangs $out/bin/nixcache-proxy
        '';
      };
      default = self.packages.${system}.cache-proxy;
    });

    apps = forAllSystems (system: {
      cache-proxy = {
        type = "app";
        program = "${self.packages.${system}.cache-proxy}/bin/nixcache-proxy";
      };
      default = self.apps.${system}.cache-proxy;
    });

    # Generic nix-darwin module (uses nix.settings). Works on standard
    # nix-darwin systems. NOTE: on Determinate-managed systems that set
    # `nix.enable = false`, nix.settings are inert — register the substituter
    # in /etc/nix/nix.custom.conf and run the proxy as a launchd daemon
    # directly (see the nixcache.nix module in pplanel's nix-darwin config).
    darwinModules.default = {
      config,
      pkgs,
      lib,
      ...
    }: let
      cfg = config.services.nixcache-proxy;
      proxyPkg = self.packages.${pkgs.stdenv.hostPlatform.system}.cache-proxy;
    in {
      options.services.nixcache-proxy = {
        enable = lib.mkEnableOption "nixcache-oci proxy substituter (GHCR)";
        repo = lib.mkOption {
          type = lib.types.str;
          description = "GitHub owner/repo hosting the OCI cache.";
        };
        port = lib.mkOption {
          type = lib.types.port;
          default = 37515;
          description = "Port the proxy listens on (localhost only).";
        };
        publicKey = lib.mkOption {
          type = lib.types.str;
          description = "Trusted public key for verifying cache signatures.";
        };
      };

      config = lib.mkIf cfg.enable {
        launchd.daemons.nixcache-proxy = {
          serviceConfig = {
            ProgramArguments = ["${proxyPkg}/bin/nixcache-proxy"];
            RunAtLoad = true;
            KeepAlive = true;
            StandardOutPath = "/var/log/nixcache-proxy.log";
            StandardErrorPath = "/var/log/nixcache-proxy.log";
            EnvironmentVariables = {
              NIXCACHE_REPO = cfg.repo;
              NIXCACHE_PORT = toString cfg.port;
              NIXCACHE_LISTEN = "127.0.0.1";
            };
          };
        };
        nix.settings = {
          extra-substituters = ["http://127.0.0.1:${toString cfg.port}"];
          extra-trusted-substituters = ["http://127.0.0.1:${toString cfg.port}"];
          extra-trusted-public-keys = [cfg.publicKey];
        };
      };
    };
  };
}

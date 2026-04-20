{
  description = "Benchmark common Nix binary cache servers";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    # Servers under test, pulled from their own flakes so the benchmark
    # tracks upstream HEAD rather than whatever nixpkgs happens to ship.
    harmonia.url = "github:nix-community/harmonia";
    nix-serve.url = "github:edolstra/nix-serve";
    nix-serve-ng.url = "github:aristanetworks/nix-serve-ng";
    ncps.url = "github:kalbasit/ncps";
    attic.url = "github:zhaofengli/attic";
  };

  outputs =
    {
      self,
      nixpkgs,
      harmonia,
      nix-serve,
      nix-serve-ng,
      ncps,
      attic,
    }:
    let
      forAllSystems = nixpkgs.lib.genAttrs [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          # Workload closures the bench streams through every server.
          # Pick via BENCH_CLOSURES (comma-separated names).
          closure-firefox = pkgs.firefox;
        }
        // nixpkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          # A bootable system exercises many small paths plus a few large ones
          # (kernel, systemd), which is closer to a real `nixos-rebuild` pull.
          closure-nixos-minimal =
            (nixpkgs.lib.nixosSystem {
              inherit system;
              modules = [
                (
                  { modulesPath, lib, ... }:
                  {
                    imports = [ "${modulesPath}/profiles/minimal.nix" ];
                    # Dummy rootfs so toplevel evaluates while still pulling in
                    # kernel + initrd; we want those in the benchmark closure.
                    fileSystems."/" = {
                      device = "/dev/disk/by-label/nixos";
                      fsType = "ext4";
                    };
                    boot.loader.grub.enable = false;
                    system.stateVersion = lib.trivial.release;
                    nixpkgs.hostPlatform = system;
                  }
                )
              ];
            }).config.system.build.toplevel;
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};

          harmoniaPkg = harmonia.packages.${system}.harmonia;
          nixServePkg = nix-serve.packages.${system}.nix-serve;
          nixServeNgPkg = nix-serve-ng.packages.${system}.nix-serve-ng;
          # Upstream ncps runs postgres/minio/redis integration tests in
          # checkPhase which are flaky in our sandbox; we only need the binary.
          ncpsPkg = ncps.packages.${system}.ncps.overrideAttrs (_: {
            doCheck = false;
            # checkPhase normally writes $coverage; drop the output entirely
            # since we skip tests.
            outputs = [ "out" ];
            preInstall = "";
          });
          ncpsDbmatePkg = ncps.packages.${system}.dbmate-wrapper;
          atticServerPkg = attic.packages.${system}.attic-server;
          atticClientPkg = attic.packages.${system}.attic-client;
          # nginx with the third-party zstd filter so it can transfer-encode
          # proxied NAR streams; the stock build only ships gzip.
          nginxZstd = pkgs.nginx.override {
            modules = pkgs.nginx.modules ++ [ pkgs.nginxModules.zstd ];
          };
          plotPython = pkgs.python3.withPackages (
            ps: with ps; [
              seaborn
              pandas
              matplotlib
            ]
          );
        in
        {
          default = pkgs.mkShell {
            packages = [
              pkgs.cargo
              pkgs.rustc
              pkgs.pkg-config
              pkgs.openssl
              pkgs.nix

              harmoniaPkg
              nixServePkg
              nixServeNgPkg
              ncpsPkg
              ncpsDbmatePkg
              atticServerPkg
              atticClientPkg
              nginxZstd
              plotPython
            ];

            # Pin exact binaries so the bench harness is immune to PATH
            # ordering (nix-serve and nix-serve-ng both install `bin/nix-serve`).
            HARMONIA_BIN = "${harmoniaPkg}/bin/harmonia-cache";
            NIX_SERVE_BIN = "${nixServePkg}/bin/nix-serve";
            NIX_SERVE_NG_BIN = "${nixServeNgPkg}/bin/nix-serve";
            NCPS_BIN = "${ncpsPkg}/bin/ncps";
            NCPS_DBMATE_BIN = "${ncpsDbmatePkg}/bin/dbmate-wrapper";
            NCPS_DB_MIGRATIONS_DIR = "${ncps}/db/migrations";
            NCPS_DB_SCHEMA_DIR = "${ncps}/db/schema";
            ATTICD_BIN = "${atticServerPkg}/bin/atticd";
            ATTICADM_BIN = "${atticServerPkg}/bin/atticadm";
            ATTIC_BIN = "${atticClientPkg}/bin/attic";
            NGINX_BIN = "${nginxZstd}/bin/nginx";

            # Default workload set; override on the command line if needed.
            BENCH_CLOSURES =
              if pkgs.stdenv.hostPlatform.isLinux then "firefox,nixos-minimal" else "firefox";
          };
        }
      );

      formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.nixfmt-rfc-style);
    };
}

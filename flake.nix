{
  description = "pharos — Rust media server (Jellyfin/Plex-compatible)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      nixpkgs,
      flake-utils,
      rust-overlay,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };

        # Pinned toolchain from rust-toolchain.toml.
        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

        rustPlatform = pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        };

        commonNativeBuildInputs = with pkgs; [ pkg-config ];

        pharos = rustPlatform.buildRustPackage {
          pname = "pharos";
          version = "0.0.0";
          src = pkgs.lib.cleanSource ./.;
          cargoLock = {
            lockFile = ./Cargo.lock;
          };
          nativeBuildInputs = commonNativeBuildInputs;
          buildInputs = [ ];
          # ffmpeg is a runtime dep, not build.
          doCheck = true;
          meta = with pkgs.lib; {
            description = "Rust media server, Jellyfin/Plex-compatible";
            license = licenses.agpl3Plus;
            platforms = platforms.unix;
            mainProgram = "pharos";
          };
        };

        # OCI image. Linux-only; build on Linux host or via remote builder.
        ociImage = pkgs.dockerTools.buildLayeredImage {
          name = "pharos";
          tag = "latest";
          contents = [
            pharos
            pkgs.ffmpeg-headless
            pkgs.cacert
            pkgs.tzdata
          ];
          config = {
            Entrypoint = [ "${pharos}/bin/pharos" ];
            Cmd = [ "serve" ];
            ExposedPorts = {
              "8096/tcp" = { };
            };
            Env = [
              "PHAROS_CONFIG=/etc/pharos/config.toml"
              "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
            ];
          };
        };
      in
      {
        packages = {
          default = pharos;
          pharos = pharos;
          oci = ociImage;
        };

        apps.default = {
          type = "app";
          program = "${pharos}/bin/pharos";
        };

        devShells.default = pkgs.mkShell {
          packages = [
            rustToolchain
            pkgs.cargo-nextest
            pkgs.cargo-watch
            pkgs.cargo-deny
            pkgs.cargo-audit
            pkgs.ffmpeg-headless
            pkgs.pkg-config
            pkgs.git
            pkgs.just
          ];
          shellHook = ''
            echo "pharos devShell — rust $(rustc --version)"
          '';
        };

        checks.workspace-build = pharos;

        formatter = pkgs.nixfmt-rfc-style;
      }
    );
}

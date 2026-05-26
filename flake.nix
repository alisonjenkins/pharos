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
            pkgs.dioxus-cli
            pkgs.wasm-bindgen-cli
            pkgs.ffmpeg-headless
            pkgs.pkg-config
            pkgs.git
            pkgs.just
            pkgs.curl
            # Node + Playwright drive T29 phase 3 (jellyfin-web headless).
            # Browser binaries come from playwright-driver.browsers and
            # are wired in via PLAYWRIGHT_BROWSERS_PATH below.
            pkgs.nodejs_22
            pkgs.playwright-driver.browsers
            # schemathesis (Layer A of T29) — install separately via:
            #   pipx install schemathesis
            # Not pinned in the flake because nixpkgs lacks a stable
            # top-level attr today. Layer B (`tests/client_compat.rs`)
            # is the hard CI gate; Layer A is best-effort, manual.
          ];
          shellHook = ''
            echo "pharos devShell — rust $(rustc --version)"
            export PLAYWRIGHT_BROWSERS_PATH=${pkgs.playwright-driver.browsers}
            export PLAYWRIGHT_SKIP_VALIDATE_HOST_REQUIREMENTS=true
          '';
        };

        checks.workspace-build = pharos;

        formatter = pkgs.nixfmt-rfc-style;
      }
    );
}

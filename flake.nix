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

        # Pinned toolchain from rust-toolchain.toml — host-arch build.
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

        # Cross-compile target for the OCI image. dockerTools builds a
        # linux container, so the binary inside must be a linux ELF —
        # not a darwin Mach-O when the build host is macOS.
        #
        # Architecture follows the build host: aarch64-darwin → aarch64-linux,
        # x86_64-darwin → x86_64-linux. The image is the same shape on
        # native linux too; the cross-build degenerates to a native one.
        linuxArch =
          if pkgs.stdenv.hostPlatform.isAarch64 then "aarch64" else "x86_64";

        # Cross-pkgs supplying the linux C linker + sysroot. `pkgsCross`
        # is nixpkgs imported with `crossSystem` set, so its rustPlatform
        # knows the host = linux/arch and lays out the build accordingly
        # (target dir, default linker, output suffix). Binary-cache hits
        # are common for `aarch64-multiplatform` + `gnu64` aliases.
        crossPkgs =
          if linuxArch == "aarch64" then
            pkgs.pkgsCross.aarch64-multiplatform
          else
            pkgs.pkgsCross.gnu64;

        # Rust target triple matching the cross host.
        rustLinuxTarget = "${linuxArch}-unknown-linux-gnu";

        # Build-host rustc + cargo with the linux target installed. We
        # state the channel explicitly because `fromRustupToolchainFile`
        # doesn't expose the `targets` override knob.
        rustToolchainCross = pkgs.rust-bin.stable.latest.default.override {
          targets = [ rustLinuxTarget ];
        };

        # `makeRustPlatform` taken from `crossPkgs` so its embedded
        # `hostPlatform` is linux. That's what flips
        # `buildRustPackage`'s default cargo `--target` from the build
        # host (darwin) to the cross host (linux).
        rustPlatformCross = crossPkgs.makeRustPlatform {
          cargo = rustToolchainCross;
          rustc = rustToolchainCross;
        };

        # Pharos built for linux. Same source, cross-compiled. Skip
        # `doCheck` — tests need to run on the build host's arch and
        # the cross sysroot can't execute them on darwin.
        pharosLinux = rustPlatformCross.buildRustPackage {
          pname = "pharos-linux";
          version = "0.0.0";
          src = pkgs.lib.cleanSource ./.;
          cargoLock = {
            lockFile = ./Cargo.lock;
          };
          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = [ ];
          doCheck = false;
        };

        # Playwright fixture WebM. Built once with the build-host's
        # ffmpeg (output is platform-neutral bytes) and baked into the
        # OCI image so the seed step doesn't need ffmpeg at runtime.
        # Note this is also the manual-testing fixture for dev-stack —
        # the seeded library points at copies of this file.
        playwrightFixture = pkgs.runCommand "pharos-playwright-fixture.webm"
          {
            nativeBuildInputs = [ pkgs.ffmpeg-headless ];
          }
          ''
            ffmpeg -y -hide_banner -loglevel error \
              -f lavfi -i testsrc=duration=5:size=320x240:rate=15 \
              -f lavfi -i sine=frequency=440:duration=5 \
              -c:v libvpx-vp9 -deadline realtime -cpu-used 8 \
              -row-mt 1 -pix_fmt yuv420p \
              -c:a libopus -shortest "$out"
          '';

        # Skeleton rootfs: passwd/group, writable /tmp + var paths.
        rootfsSkel = pkgs.runCommand "rootfs-skel" { } ''
          mkdir -p $out/etc $out/var/lib/pharos/db $out/var/lib/pharos/media $out/var/lib/pharos/cache $out/tmp $out/usr/share/pharos
          printf 'root:x:0:0::/root:/sbin/nologin\npharos:x:1000:1000::/var/lib/pharos:/sbin/nologin\n' > $out/etc/passwd
          printf 'root:x:0:\npharos:x:1000:\n' > $out/etc/group
          chmod 1777 $out/tmp
          cp ${playwrightFixture} $out/usr/share/pharos/playwright-fixture.webm
        '';

        # OCI image — distroless layered image straight from nix store
        # paths. Contents are linux store paths from `crossPkgs`, so
        # the resulting image runs in docker on any host.
        #
        # ffmpeg is NOT in the image. Cross-compiling ffmpeg's full
        # dep tree from darwin → linux blows up on a darwin-only
        # linker flag in libwebp / giflib. The seed step uses the
        # pre-baked playwright fixture (no runtime ffmpeg). Transcode
        # + image-cache endpoints need ffmpeg and so don't function
        # in this image — manual testing is direct-play only. A
        # production image with full ffmpeg lands once we either ship
        # a static-ffmpeg fetch or document the linux-builder
        # prerequisite (T48-adjacent follow-up).
        ociImage = pkgs.dockerTools.buildLayeredImage {
          name = "pharos";
          tag = "latest";
          architecture = if linuxArch == "aarch64" then "arm64" else "amd64";
          contents = [
            pharosLinux
            crossPkgs.cacert
            crossPkgs.tzdata
            rootfsSkel
          ];
          config = {
            Entrypoint = [ "${pharosLinux}/bin/pharos" ];
            Cmd = [
              "--config"
              "/etc/pharos/config.toml"
              "serve"
            ];
            ExposedPorts = {
              "8096/tcp" = { };
            };
            Env = [
              "SSL_CERT_FILE=${crossPkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
              "PATH=${pharosLinux}/bin"
              "PHAROS_PLAYWRIGHT_FIXTURE=/usr/share/pharos/playwright-fixture.webm"
            ];
            WorkingDir = "/var/lib/pharos";
          };
        };
      in
      {
        packages = {
          default = pharos;
          pharos = pharos;
          pharosLinux = pharosLinux;
          oci = ociImage;
        };

        apps.default = {
          type = "app";
          program = "${pharos}/bin/pharos";
        };

        # `nix run .#dev-stack` boots pharos + jellyfin-web as
        # distroless OCI containers, both built reproducibly from the
        # flake (pharos via `.#oci` — cross-compiled to linux when the
        # build host is darwin; jellyfin-web bundle bind-mounted from
        # the pinned nixpkgs derivation). Manual testing entry point —
        # see scripts/dev-stack.sh for the full pipeline.
        apps.dev-stack = {
          type = "app";
          program =
            let
              script = pkgs.writeShellApplication {
                name = "pharos-dev-stack";
                runtimeInputs = [ pkgs.bash pkgs.coreutils pkgs.nix ];
                text = builtins.readFile ./scripts/dev-stack.sh;
              };
            in
            "${script}/bin/pharos-dev-stack";
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
            # Node + Playwright drive T29 phase 3. jellyfin-web is the
            # upstream prebuilt static bundle, referenced via
            # JELLYFIN_WEB_DIR at runtime. Playwright manages its own
            # chromium under ~/.cache/ms-playwright; nix's
            # playwright-driver.browsers had a directory-layout mismatch
            # with the npm package on darwin, so we let Playwright drive
            # the download (`npx playwright install chromium` once).
            pkgs.nodejs_22
            pkgs.jellyfin-web
            # schemathesis (Layer A of T29) — install separately via:
            #   pipx install schemathesis
            # Not pinned in the flake because nixpkgs lacks a stable
            # top-level attr today. Layer B (`tests/client_compat.rs`)
            # is the hard CI gate; Layer A is best-effort, manual.
          ];
          shellHook = ''
            echo "pharos devShell — rust $(rustc --version)"
            export JELLYFIN_WEB_DIR=${pkgs.jellyfin-web}/share/jellyfin-web
          '';
        };

        checks.workspace-build = pharos;

        formatter = pkgs.nixfmt-rfc-style;
      }
    );
}

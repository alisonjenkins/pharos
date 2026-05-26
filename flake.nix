{
  description = "pharos — Rust media server (Jellyfin/Plex-compatible)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    # Per-crate-derivation rust build. cargo2nix (the obvious pick)
    # fails to bootstrap on darwin — its own globwalk dep trips
    # macOS linker semantics. crate2nix offers the same per-crate
    # semantics (each Cargo.lock entry → its own derivation, dedup'd
    # via /nix/store across projects), runs darwin-native (it's in
    # nixpkgs as `pkgs.crate2nix`), and is what we actually use.
    # Generates `Cargo.nix` from `Cargo.lock`; the flake imports it
    # to build each crate.
    #
    # No new flake input: crate2nix comes from the pinned nixpkgs.
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

        # Pinned toolchain (rust + clippy + rustfmt + wasm target).
        # Used by the devShell + injected into crate2nix's generated
        # workspace below so dep builds use the same compiler that
        # `cargo nextest` does.
        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

        # crate2nix's per-crate package set. `Cargo.nix` is generated
        # by running `crate2nix generate` at the workspace root (see
        # the `just regen-cargo-nix` recipe). The generated file
        # exposes a `buildRustCrateForPkgs` builder per workspace
        # member + per dep in Cargo.lock — each dep is its own nix
        # derivation, cached in /nix/store and shared across any
        # project that resolves the same crate+version.
        cargoNix = import ./Cargo.nix {
          inherit pkgs;
          buildRustCrateForPkgs = pkgs: pkgs.buildRustCrate.override {
            rustc = rustToolchain;
            cargo = rustToolchain;
          };
        };

        pharos = cargoNix.workspaceMembers."pharos-server".build;

        # ─── Creative-Commons test media ──────────────────────────
        # Pinned URLs + sha256 so the corpus is bit-identical across
        # hosts. Licenses recorded inline so the audit trail lives
        # next to the fetcher.
        bbb360 = pkgs.fetchurl {
          url = "https://test-videos.co.uk/vids/bigbuckbunny/webm/vp9/360/Big_Buck_Bunny_360_10s_1MB.webm";
          hash = "sha256:0n4wjcy95idw59biq105j1k6hcirkj86s7cybpzzshd0kzfzwakb";
          # Big Buck Bunny © Blender Foundation, CC-BY 3.0
          # https://peach.blender.org/about/
          meta.license = pkgs.lib.licenses.cc-by-30;
        };
        bbb720 = pkgs.fetchurl {
          url = "https://test-videos.co.uk/vids/bigbuckbunny/webm/vp9/720/Big_Buck_Bunny_720_10s_2MB.webm";
          hash = "sha256:0dbjndi62f570hl1m29sirxsikaa5q93g08wmb77fcnraa6kfng0";
          meta.license = pkgs.lib.licenses.cc-by-30;
        };
        bbb1080 = pkgs.fetchurl {
          url = "https://test-videos.co.uk/vids/bigbuckbunny/webm/vp9/1080/Big_Buck_Bunny_1080_10s_5MB.webm";
          hash = "sha256:0ljxfk30n57zhq9z3mb8ww7vj2bb7qkshjqr6w7nqbblvkbs649h";
          meta.license = pkgs.lib.licenses.cc-by-30;
        };
        wikimediaExampleOgg = pkgs.fetchurl {
          url = "https://upload.wikimedia.org/wikipedia/commons/c/c8/Example.ogg";
          hash = "sha256:0ahq339irazfnlrdjhxczlf803b1jd9b8kr2077lgj74mbc5cyzm";
          # Public domain (Wikimedia Commons).
          meta.license = pkgs.lib.licenses.publicDomain;
        };
        kevinMacLeodCarefree = pkgs.fetchurl {
          url = "https://incompetech.com/music/royalty-free/mp3-royaltyfree/Carefree.mp3";
          hash = "sha256:04g36gblzryj9jalkld4k7lfq1vrcwn4916l9xcv3n9hlrqbfcw4";
          # Kevin MacLeod "Carefree", CC-BY 4.0.
          # https://incompetech.com/wordpress/2013/06/carefree/
          meta.license = pkgs.lib.licenses.cc-by-40;
        };

        # Test media tree assembled into one directory.
        #
        # Upstream test-videos.co.uk BBB samples are video-only. We mux a
        # silent Opus track in at build time so the corpus exercises the
        # full audio+video pipeline through jellyfin-web's htmlVideoPlayer
        # — direct-play of a video-only WebM works but a user who clicks
        # play sees no soundbar, which looks broken even though it isn't.
        # The Opus track is generated via ffmpeg's `anullsrc` lavfi
        # source; output is bit-identical across builds because nix sets
        # `SOURCE_DATE_EPOCH` and ffmpeg uses it for the WebM creation_time.
        pharosTestMedia = pkgs.runCommand "pharos-test-media"
          { nativeBuildInputs = [ pkgs.ffmpeg-headless ]; } ''
          mkdir -p $out
          add_silence() {
            in=$1
            out=$2
            dur=$(ffprobe -v error -show_entries format=duration \
                          -of default=nw=1:nk=1 "$in")
            ffmpeg -hide_banner -loglevel error -nostdin \
                   -i "$in" \
                   -f lavfi -t "$dur" -i "anullsrc=channel_layout=stereo:sample_rate=48000" \
                   -c:v copy -c:a libopus -b:a 64k \
                   -map 0:v:0 -map 1:a:0 \
                   -shortest "$out"
          }
          add_silence ${bbb360}   $out/01-big-buck-bunny-360p.webm
          add_silence ${bbb720}   $out/02-big-buck-bunny-720p.webm
          add_silence ${bbb1080}  $out/03-big-buck-bunny-1080p.webm
          cp ${wikimediaExampleOgg}  $out/04-wikimedia-example.ogg
          cp ${kevinMacLeodCarefree} $out/05-carefree.mp3
          # Synthetic AV-confirm fixture: 10 s of ffmpeg `testsrc`
          # (colour bars + frame counter) muxed with a 440 Hz sine
          # tone. The point is to confirm jellyfin-web's audio path
          # end-to-end — a user clicks play and *hears something*.
          # The BBB clips above are silent (their upstream encoding
          # was video-only) so they wouldn't surface a broken audio
          # decoder.
          ffmpeg -hide_banner -loglevel error -nostdin \
                 -f lavfi -i "testsrc=duration=10:size=640x480:rate=30" \
                 -f lavfi -i "sine=frequency=440:duration=10:sample_rate=48000" \
                 -c:v libvpx-vp9 -deadline realtime -cpu-used 8 -row-mt 1 \
                 -pix_fmt yuv420p \
                 -c:a libopus -b:a 64k \
                 -shortest \
                 $out/06-test-av-confirm.webm
          cat > $out/LICENSES.txt <<EOF
        01-big-buck-bunny-360p.webm   CC-BY 3.0       https://peach.blender.org/about/  (silent Opus track muxed at build)
        02-big-buck-bunny-720p.webm   CC-BY 3.0       https://peach.blender.org/about/  (silent Opus track muxed at build)
        03-big-buck-bunny-1080p.webm  CC-BY 3.0       https://peach.blender.org/about/  (silent Opus track muxed at build)
        04-wikimedia-example.ogg      Public Domain   https://commons.wikimedia.org/wiki/File:Example.ogg
        05-carefree.mp3               CC-BY 4.0       https://incompetech.com/wordpress/2013/06/carefree/
        06-test-av-confirm.webm       Synthetic       ffmpeg lavfi testsrc + 440 Hz sine, generated at build time
        EOF
        '';

        # Sibling OCI image whose only job is to copy
        # `pharosTestMedia` into the pharos_media docker volume on
        # demand. dev-stack runs it as a one-shot during bring-up.
        testMediaImage = pkgs.dockerTools.buildLayeredImage {
          name = "pharos-test-media";
          tag = "latest";
          architecture = if pkgs.stdenv.hostPlatform.isAarch64 then "arm64" else "amd64";
          contents = [
            pkgs.busybox
            pharosTestMedia
          ];
          config = {
            Entrypoint = [
              "/bin/sh"
              "-c"
              "cp -rL ${pharosTestMedia}/. /media/ && chmod -R a+r /media && echo '>>> test media populated:' && ls /media"
            ];
          };
        };

        # Skeleton rootfs: passwd / group / writable /tmp + state
        # directories. Distroless containers usually skip this, but
        # ffmpeg + tokio's getrandom path are happier with a real
        # /tmp + a passwd entry for the non-root user.
        rootfsSkel = pkgs.runCommand "rootfs-skel" { } ''
          mkdir -p $out/etc $out/var/lib/pharos/db $out/var/lib/pharos/media $out/var/lib/pharos/cache $out/tmp
          printf 'root:x:0:0::/root:/sbin/nologin\npharos:x:1000:1000::/var/lib/pharos:/sbin/nologin\n' > $out/etc/passwd
          printf 'root:x:0:\npharos:x:1000:\n' > $out/etc/group
          chmod 1777 $out/tmp
        '';

        # OCI image — distroless layered image. Only defined for linux
        # systems; on darwin nothing useful runs in a container. The
        # `nix build` invocation from darwin targets the linux variant
        # explicitly (via `--system aarch64-linux` or via the
        # `packages.<arch>-linux.oci` attribute path), dispatching to
        # the configured linux-builder.
        ociImage = pkgs.dockerTools.buildLayeredImage {
          name = "pharos";
          tag = "latest";
          architecture = if pkgs.stdenv.hostPlatform.isAarch64 then "arm64" else "amd64";
          contents = [
            pharos
            pkgs.ffmpeg-headless
            pkgs.cacert
            pkgs.tzdata
            rootfsSkel
          ];
          config = {
            Entrypoint = [ "${pharos}/bin/pharos" ];
            Cmd = [
              "--config"
              "/etc/pharos/config.toml"
              "serve"
            ];
            ExposedPorts = {
              "8096/tcp" = { };
            };
            Env = [
              "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
              "PATH=${pharos}/bin:${pkgs.ffmpeg-headless}/bin"
            ];
            WorkingDir = "/var/lib/pharos";
          };
        };

        # Sibling OCI image for jellyfin-web. Static-only consumer of
        # the pinned nixpkgs bundle. `darkhttpd` is a tiny single-binary
        # static server — no nginx config sprawl, no upstream Docker
        # Hub image, all from nix.
        jellyfinWebImage = pkgs.dockerTools.buildLayeredImage {
          name = "pharos-jellyfin-web";
          tag = "latest";
          architecture = if pkgs.stdenv.hostPlatform.isAarch64 then "arm64" else "amd64";
          contents = [
            pkgs.darkhttpd
            pkgs.jellyfin-web
            pkgs.cacert
          ];
          config = {
            Entrypoint = [
              "${pkgs.darkhttpd}/bin/darkhttpd"
              "${pkgs.jellyfin-web}/share/jellyfin-web"
              "--addr"
              "0.0.0.0"
              "--port"
              "8097"
            ];
            ExposedPorts = {
              "8097/tcp" = { };
            };
          };
        };

        # docker-compose manifest. Built as a nix store artefact so
        # the same flake commit pins both images + the orchestration
        # config. `dev-stack.sh` materialises this into state dir +
        # invokes `docker compose up`.
        composeFile = pkgs.writeText "pharos-dev-stack.yaml" ''
          services:
            pharos:
              image: pharos:latest
              container_name: pharos-dev-stack
              restart: unless-stopped
              ports:
                - "127.0.0.1:8096:8096"
              volumes:
                - pharos_db:/var/lib/pharos/db
                - pharos_media:/var/lib/pharos/media
                - pharos_cache:/var/lib/pharos/cache
                - ''${PHAROS_CONFIG_HOST}:/etc/pharos/config.toml:ro
            jellyfin-web:
              image: pharos-jellyfin-web:latest
              container_name: pharos-jellyfin-web
              restart: unless-stopped
              ports:
                - "127.0.0.1:8097:8097"

          volumes:
            pharos_db:
            pharos_media:
            pharos_cache:
        '';
      in
      {
        packages = {
          default = pharos;
          pharos = pharos;
        } // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          # `oci` + `jellyfinWebOci` + compose only meaningful on
          # linux. On darwin the same attrs resolve under
          # packages.<arch>-linux.* and the linux-builder picks up.
          oci = ociImage;
          jellyfinWebOci = jellyfinWebImage;
          testMediaOci = testMediaImage;
          testMediaTree = pharosTestMedia;
          composeFile = composeFile;
        };

        apps.default = {
          type = "app";
          program = "${pharos}/bin/pharos";
        };

        # `nix run .#dev-stack` boots pharos + jellyfin-web as
        # distroless OCI containers built reproducibly via nix
        # dockerTools. On darwin the pharos image build dispatches to
        # the configured linux-builder so the binary inside is a real
        # linux ELF; jellyfin-web bundle bind-mounted from pinned
        # nixpkgs. See scripts/dev-stack.sh for the full pipeline.
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
            # crate2nix regenerates Cargo.nix when Cargo.lock changes.
            # Run `just regen-cargo-nix` after touching dependencies.
            pkgs.crate2nix
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

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

        # `dx build` requires wasm-bindgen-cli to EXACTLY match the
        # wasm-bindgen crate the project locks (dioxus 0.7.9 → 0.2.122),
        # but the pinned nixpkgs ships 0.2.121. Build the matching version
        # from crates.io. (Bump version + both hashes together when dioxus
        # bumps its wasm-bindgen pin.)
        wasmBindgenCli = pkgs.rustPlatform.buildRustPackage rec {
          pname = "wasm-bindgen-cli";
          version = "0.2.122";
          src = pkgs.fetchCrate {
            inherit pname version;
            hash = "sha256-vO4RSxi/sMWxmsEs3GuljdMfIRSu75A+Q+c5wgYToRU=";
          };
          cargoHash = "sha256-Inup6vvJSG5ghNyeDPyZbfZo4d0LsMG2OJfStoaeDBs=";
          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = [ pkgs.openssl ]
            ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isDarwin [ pkgs.curl ];
          doCheck = false;
        };


        # Source for the cargo-based binary build (exclude build outputs).
        repoSrc = pkgs.lib.cleanSourceWith {
          src = ./.;
          filter = path: _type:
            let rel = pkgs.lib.removePrefix (toString ./. + "/") (toString path);
            in !(pkgs.lib.hasPrefix "target" rel
                  || pkgs.lib.hasPrefix "result" rel
                  || pkgs.lib.hasPrefix ".git" rel);
        };

        # The server + the out-of-process transcode worker (crash-isolated
        # HLS/segment encoder + libav tiny-op pool), built together with the
        # default ffmpeg backend (now libav/ffmpeg-lib — the hybrid).
        #
        # Built via cargo (buildRustPackage) rather than crate2nix: the libav
        # FFI crate (ffmpeg-the-third) emits its API-version cfgs through a
        # build script in the modern `cargo::` form + a large cfg set that the
        # pinned nixpkgs `buildRustCrate` mis-handles, so crate2nix compiled
        # the pre-5.1 libswresample API and failed against ffmpeg 8.1. Real
        # cargo applies the build-script cfgs correctly. pkg-config + the
        # ffmpeg dev libs + bindgenHook mirror the devShell's backend-lib env.
        pharosBins =
          (pkgs.makeRustPlatform {
            cargo = rustToolchain;
            rustc = rustToolchain;
          }).buildRustPackage {
            pname = "pharos";
            version = "0.0.0";
            src = repoSrc;
            cargoLock = {
              lockFile = ./Cargo.lock;
            };
            # Only the two server-side binaries; the wasm UI crate is built
            # separately by `dx` (pharosUiBundle).
            cargoBuildFlags = [ "-p" "pharos-server" "-p" "pharos-transcode" ];
            doCheck = false;
            nativeBuildInputs = [ pkgs.pkg-config pkgs.rustPlatform.bindgenHook ];
            buildInputs = [ pkgs.ffmpeg-headless.dev ];
          };
        pharos = pharosBins;
        # Bundled into the OCI image + pointed at via PHAROS_TRANSCODE_WORKER
        # (the two binaries share one store path here, but the env var is the
        # explicit first-choice lookup in worker/proc.rs regardless).
        transcodeWorker = pharosBins;

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

        # ─── Test media corpus ────────────────────────────────────
        #
        # Split per fixture so a change to ONE doesn't invalidate the
        # whole tree. Each sub-derivation lands in /nix/store keyed by
        # its own inputs; finer granularity = more cache reuse across
        # nixpkgs bumps (a ffmpeg-headless rev change only invalidates
        # the encodes that actually depend on it, not the cp-only
        # fixtures).
        #
        # Upstream test-videos.co.uk BBB samples are video-only. We mux
        # a silent Opus track in at build time so the corpus exercises
        # the full audio+video pipeline through jellyfin-web's
        # htmlVideoPlayer. The Opus track is generated via ffmpeg's
        # `anullsrc` lavfi source; output is bit-identical across
        # builds because nix sets SOURCE_DATE_EPOCH.

        # Per-resolution silent-Opus mux for the BBB fixtures. `-c:v
        # copy` keeps it fast — only the new audio track is encoded.
        addSilence = name: src:
          pkgs.runCommand name
            { nativeBuildInputs = [ pkgs.ffmpeg-headless ]; } ''
            dur=$(ffprobe -v error -show_entries format=duration \
                          -of default=nw=1:nk=1 ${src})
            ffmpeg -hide_banner -loglevel error -nostdin \
                   -i ${src} \
                   -f lavfi -t "$dur" -i "anullsrc=channel_layout=stereo:sample_rate=48000" \
                   -c:v copy -c:a libopus -b:a 64k \
                   -map 0:v:0 -map 1:a:0 \
                   -shortest $out
          '';
        bbb360Webm = addSilence "bbb-360p.webm" bbb360;
        bbb720Webm = addSilence "bbb-720p.webm" bbb720;
        bbb1080Webm = addSilence "bbb-1080p.webm" bbb1080;

        # Synthetic AV-confirm fixture: 10 s of ffmpeg `testsrc`
        # muxed with a 440 Hz sine tone. Confirms jellyfin-web's audio
        # path end-to-end (BBB clips above are silent).
        avConfirmWebm =
          pkgs.runCommand "test-av-confirm.webm"
            { nativeBuildInputs = [ pkgs.ffmpeg-headless ]; } ''
            ffmpeg -hide_banner -loglevel error -nostdin \
                   -f lavfi -i "testsrc=duration=10:size=640x480:rate=30" \
                   -f lavfi -i "sine=frequency=440:duration=10:sample_rate=48000" \
                   -c:v libvpx-vp9 -deadline realtime -cpu-used 8 -row-mt 1 \
                   -pix_fmt yuv420p \
                   -c:a libopus -b:a 64k \
                   -shortest \
                   $out
          '';

        # Subtitle-confirm fixture: testsrc + tone + embedded WebVTT.
        # Sidecar .vtt is built separately so a tweak to the cue text
        # doesn't reshell the slow VP9 encode.
        subtitlesVtt = pkgs.writeText "test-subtitles.vtt" ''
          WEBVTT

          00:00:00.500 --> 00:00:03.000
          Pharos subtitle smoke test

          00:00:03.500 --> 00:00:06.000
          If you see this, external VTT works

          00:00:06.500 --> 00:00:09.500
          End of test
        '';
        subtitlesWebm =
          pkgs.runCommand "test-subtitles.webm"
            { nativeBuildInputs = [ pkgs.ffmpeg-headless ]; } ''
            ffmpeg -hide_banner -loglevel error -nostdin \
                   -f lavfi -i "testsrc=duration=10:size=640x480:rate=30" \
                   -f lavfi -i "sine=frequency=523.25:duration=10:sample_rate=48000" \
                   -i ${subtitlesVtt} \
                   -c:v libvpx-vp9 -deadline realtime -cpu-used 8 -row-mt 1 \
                   -pix_fmt yuv420p \
                   -c:a libopus -b:a 64k \
                   -c:s webvtt \
                   -map 0:v:0 -map 1:a:0 -map 2:s:0 \
                   -metadata:s:s:0 language=eng \
                   -metadata:s:s:0 title="English" \
                   -shortest \
                   $out
          '';

        # Assembly-only derivation. Pure cp + cat — no ffmpeg here.
        # Each cp is from an already-cached store path; the assembly
        # itself is cheap and re-runs trivially when any one sub-fixture
        # changes (without re-encoding the others).
        pharosTestMedia = pkgs.runCommand "pharos-test-media" { } ''
          mkdir -p $out
          cp ${bbb360Webm}  $out/01-big-buck-bunny-360p.webm
          cp ${bbb720Webm}  $out/02-big-buck-bunny-720p.webm
          cp ${bbb1080Webm} $out/03-big-buck-bunny-1080p.webm
          cp ${wikimediaExampleOgg}  $out/04-wikimedia-example.ogg
          cp ${kevinMacLeodCarefree} $out/05-carefree.mp3
          cp ${avConfirmWebm} $out/06-test-av-confirm.webm
          cp ${subtitlesVtt}  $out/07-test-subtitles.vtt
          cp ${subtitlesWebm} $out/07-test-subtitles.webm
          cat > $out/LICENSES.txt <<EOF
        01-big-buck-bunny-360p.webm   CC-BY 3.0       https://peach.blender.org/about/  (silent Opus track muxed at build)
        02-big-buck-bunny-720p.webm   CC-BY 3.0       https://peach.blender.org/about/  (silent Opus track muxed at build)
        03-big-buck-bunny-1080p.webm  CC-BY 3.0       https://peach.blender.org/about/  (silent Opus track muxed at build)
        04-wikimedia-example.ogg      Public Domain   https://commons.wikimedia.org/wiki/File:Example.ogg
        05-carefree.mp3               CC-BY 4.0       https://incompetech.com/wordpress/2013/06/carefree/
        06-test-av-confirm.webm       Synthetic       ffmpeg lavfi testsrc + 440 Hz sine, generated at build time
        07-test-subtitles.webm        Synthetic       testsrc + 523 Hz tone + embedded WebVTT track (English)
        07-test-subtitles.vtt         Synthetic       sidecar VTT for 07-test-subtitles.webm
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

        # Per-test fixture corpus for `tests/ffmpeg_integration.rs`.
        # Built once, cached in /nix/store, consumed via the
        # `PHAROS_TEST_FIXTURES` env var the devShell exports. Keeps
        # the slow VP9 encodes out of `cargo nextest`.
        pharosIntegrationFixtures =
          pkgs.runCommand "pharos-integration-fixtures"
            { nativeBuildInputs = [ pkgs.ffmpeg-headless ]; } ''
              mkdir -p $out
              # 1. Base VP9 + Opus (3s, 320x240) — make_video_fixture.
              ffmpeg -hide_banner -loglevel error -nostdin \
                     -f lavfi -i "testsrc=duration=3:size=320x240:rate=10" \
                     -f lavfi -i "sine=frequency=440:duration=3" \
                     -c:v libvpx-vp9 -deadline realtime -cpu-used 8 -row-mt 1 \
                     -b:v 200k \
                     -c:a libopus \
                     -shortest \
                     $out/video.webm

              # 2. Opus-only (2s) — make_audio_fixture.
              ffmpeg -hide_banner -loglevel error -nostdin \
                     -f lavfi -i "sine=frequency=440:duration=2" \
                     -c:a libopus \
                     $out/audio.webm

              # 3. VP9 + Opus + embedded WebVTT — make_subtitled_video_fixture.
              cat > $out/subs.vtt <<VTT
              WEBVTT

              00:00:00.500 --> 00:00:02.000
              Hello pharos
              VTT
              ffmpeg -hide_banner -loglevel error -nostdin \
                     -f lavfi -i "testsrc=duration=3:size=320x240:rate=10" \
                     -f lavfi -i "sine=frequency=440:duration=3" \
                     -i $out/subs.vtt \
                     -c:v libvpx-vp9 -deadline realtime -cpu-used 8 -row-mt 1 \
                     -b:v 200k \
                     -c:a libopus \
                     -c:s webvtt \
                     -map 0:v:0 -map 1:a:0 -map 2:s:0 \
                     -metadata:s:s:0 language=eng \
                     -shortest \
                     $out/subbed.webm

              # 4. VP9 + two distinct audio tracks (440 Hz vs 880 Hz)
              # — drives W1 audio-track-switching integration tests.
              # Track 0 (eng) = 440 Hz tone, track 1 (jpn) = 880 Hz tone.
              ffmpeg -hide_banner -loglevel error -nostdin \
                     -f lavfi -i "testsrc=duration=3:size=320x240:rate=10" \
                     -f lavfi -i "sine=frequency=440:duration=3" \
                     -f lavfi -i "sine=frequency=880:duration=3" \
                     -c:v libvpx-vp9 -deadline realtime -cpu-used 8 -row-mt 1 \
                     -b:v 200k \
                     -c:a libopus \
                     -map 0:v:0 -map 1:a:0 -map 2:a:0 \
                     -metadata:s:a:0 language=eng \
                     -metadata:s:a:1 language=jpn \
                     -shortest \
                     $out/dualaudio.mkv

              # 5. VP9 + opus + two embedded subtitle tracks (eng + jpn)
              # — drives W2 burn-in integration tests.
              cat > $out/subs_eng.vtt <<VTT
              WEBVTT

              00:00:00.500 --> 00:00:02.000
              English subtitle
              VTT
              cat > $out/subs_jpn.vtt <<VTT
              WEBVTT

              00:00:00.500 --> 00:00:02.000
              日本語字幕
              VTT
              ffmpeg -hide_banner -loglevel error -nostdin \
                     -f lavfi -i "testsrc=duration=3:size=320x240:rate=10" \
                     -f lavfi -i "sine=frequency=440:duration=3" \
                     -i $out/subs_eng.vtt \
                     -i $out/subs_jpn.vtt \
                     -c:v libvpx-vp9 -deadline realtime -cpu-used 8 -row-mt 1 \
                     -b:v 200k \
                     -c:a libopus \
                     -c:s webvtt \
                     -map 0:v:0 -map 1:a:0 -map 2:s:0 -map 3:s:0 \
                     -metadata:s:s:0 language=eng \
                     -metadata:s:s:1 language=jpn \
                     -shortest \
                     $out/dualsubs.mkv

              # 6. MP3 with embedded ID3v2 attached_pic JPEG cover —
              # make_audio_fixture_with_cover. Two ffmpeg passes: one
              # for the 64x64 magenta JPEG, one for the mux.
              ffmpeg -hide_banner -loglevel error -nostdin \
                     -f lavfi -i "color=c=magenta:s=64x64:d=1" \
                     -frames:v 1 -f image2 \
                     $out/cover.jpg
              ffmpeg -hide_banner -loglevel error -nostdin \
                     -f lavfi -i "sine=frequency=440:duration=1" \
                     -i $out/cover.jpg \
                     -map 0:a:0 -map 1:v:0 \
                     -c:a libmp3lame -b:a 64k \
                     -c:v mjpeg \
                     -disposition:v:0 attached_pic \
                     -id3v2_version 3 \
                     -shortest \
                     $out/withcover.mp3
            '';

        # Skeleton rootfs (passwd / group / writable /tmp + state dirs) is
        # written as REAL files into the image layer via `extraCommands`
        # below — NOT a separate store path added to `contents`. A
        # store-path skeleton makes /etc/passwd a symlink into /nix/store,
        # which kind's containerd snapshotter rejects when run under
        # (rootless) podman: "openat etc/passwd: path escapes from parent".
        # Real in-rootfs files load identically under docker + podman + kind.

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
            transcodeWorker
            pkgs.ffmpeg-headless
            pkgs.cacert
            pkgs.tzdata
          ];
          # Real /etc + state dirs in the rootfs (see note above). chmod 1777
          # /tmp for ffmpeg + tokio getrandom; a passwd/group entry for the
          # non-root pharos user.
          extraCommands = ''
            mkdir -p etc var/lib/pharos/db var/lib/pharos/media var/lib/pharos/cache tmp
            printf 'root:x:0:0::/root:/sbin/nologin\npharos:x:1000:1000::/var/lib/pharos:/sbin/nologin\n' > etc/passwd
            printf 'root:x:0:\npharos:x:1000:\n' > etc/group
            chmod 1777 tmp
          '';
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
              "PATH=${pharos}/bin:${transcodeWorker}/bin:${pkgs.ffmpeg-headless}/bin"
              # Explicit worker path so the transcode scheduler uses the
              # crash-isolated worker pool, not the inline-ffmpeg fallback.
              "PHAROS_TRANSCODE_WORKER=${transcodeWorker}/bin/transcode-worker"
            ];
            WorkingDir = "/var/lib/pharos";
          };
        };

        # The pharos Dioxus UI compiled to a static WASM bundle via `dx`.
        # Built reproducibly in the nix sandbox: cargo deps are vendored from
        # Cargo.lock (offline), the wasm-bindgen-cli is the pinned 0.2.122,
        # and binaryen supplies wasm-opt. Output is the `dx` web `public/`
        # dir (index.html + /ui/assets/{wasm,js}); pharos.css is copied in
        # (dx 0.7's legacy [web.resource].style no longer copies it). The
        # app is built with base_path = "ui" so every asset URL is /ui/-
        # rooted and the angie pod can serve it under /ui/ without colliding
        # with the proxied REST API.
        uiSrc = pkgs.lib.cleanSourceWith {
          src = ./.;
          filter = path: _type:
            let rel = pkgs.lib.removePrefix (toString ./. + "/") (toString path);
            in !(pkgs.lib.hasPrefix "target" rel
                  || pkgs.lib.hasPrefix "result" rel
                  || pkgs.lib.hasPrefix ".git" rel);
        };
        pharosUiBundle = pkgs.stdenv.mkDerivation {
          pname = "pharos-ui-bundle";
          version = "0.0.0";
          src = uiSrc;
          nativeBuildInputs = [
            rustToolchain
            pkgs.dioxus-cli
            wasmBindgenCli
            pkgs.binaryen
            pkgs.rustPlatform.cargoSetupHook
          ];
          cargoDeps = pkgs.rustPlatform.importCargoLock {
            lockFile = ./Cargo.lock;
          };
          buildPhase = ''
            runHook preBuild
            export HOME=$TMPDIR
            dx build --package pharos-ui --release
            runHook postBuild
          '';
          installPhase = ''
            runHook preInstall
            mkdir -p $out
            cp -r target/dx/pharos-ui-web/release/web/public/. $out/
            install -Dm644 crates/pharos-ui/assets/pharos.css $out/assets/pharos.css
            runHook postInstall
          '';
          dontFixup = true;
        };

        # Both UIs are the same shape: an **angie** (nginx fork) pod that
        # serves a static SPA bundle under a URL prefix and reverse-proxies
        # every other request (the REST API + websocket) to pharos
        # (`$PHAROS_URL`, default the in-cluster service). So the browser
        # sees ONE same-origin server — the boot `/System/Info/Public` probe
        # and all traffic resolve through angie — mirroring the compat
        # suite's `http-server --proxy PHAROS?` fixture (cross-origin fails:
        # the connect probe must be same-origin). `__PHAROS_URL__` is the
        # only runtime knob; the entrypoint seds it into the config.
        mkAngieUi = { pname, port, prefix, bundle }:
          let
            conf = pkgs.writeText "${pname}.angie.conf" ''
              daemon off;
              worker_processes 1;
              pid /tmp/angie/angie.pid;
              error_log /dev/stderr warn;
              events { worker_connections 1024; }
              http {
                include ${pkgs.angie}/conf/mime.types;
                default_type application/octet-stream;
                access_log /dev/stdout;
                sendfile on;
                client_body_temp_path /tmp/angie/client_body;
                proxy_temp_path        /tmp/angie/proxy;
                fastcgi_temp_path      /tmp/angie/fastcgi;
                uwsgi_temp_path        /tmp/angie/uwsgi;
                scgi_temp_path         /tmp/angie/scgi;

                # websocket upgrade plumbing (jellyfin /socket, dioxus ws).
                map $http_upgrade $connection_upgrade {
                  default upgrade;
                  ""      close;
                }

                server {
                  listen ${toString port};

                  # SPA served under /${prefix}/; its files live at the
                  # bundle dir root, aliased in. try_files falls back to the
                  # SPA index for client-side routes.
                  location = /         { return 302 /${prefix}/; }
                  location = /${prefix} { return 302 /${prefix}/; }
                  location /${prefix}/ {
                    alias ${bundle}/;
                    try_files $uri $uri/ /${prefix}/index.html;
                  }

                  # Everything else → pharos (same-origin REST + websocket).
                  location / {
                    proxy_pass __PHAROS_URL__;
                    proxy_http_version 1.1;
                    proxy_set_header Host              $host;
                    proxy_set_header X-Real-IP         $remote_addr;
                    proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;
                    proxy_set_header X-Forwarded-Proto $scheme;
                    proxy_set_header Upgrade           $http_upgrade;
                    proxy_set_header Connection        $connection_upgrade;
                  }
                }
              }
            '';
            entrypoint = pkgs.writeShellApplication {
              name = "${pname}-entrypoint";
              runtimeInputs = [ pkgs.coreutils pkgs.gnused pkgs.angie ];
              text = ''
                : "''${PHAROS_URL:=http://pharos:8096}"
                mkdir -p /tmp/angie
                sed "s|__PHAROS_URL__|''${PHAROS_URL}|g" \
                  ${conf} > /tmp/angie/angie.conf
                # -e overrides the compiled-in default error-log path
                # (/var/log/angie) which the non-root image cannot create.
                exec angie -e /dev/stderr -c /tmp/angie/angie.conf
              '';
            };
          in
          pkgs.dockerTools.buildLayeredImage {
            name = pname;
            tag = "latest";
            architecture = if pkgs.stdenv.hostPlatform.isAarch64 then "arm64" else "amd64";
            contents = [ entrypoint pkgs.cacert ];
            # angie writes its pid + temp dirs under /tmp (writable /tmp);
            # and when the master runs as root it drops workers to `nobody`,
            # so /etc/passwd must carry that entry (+ a pharos:1000 user for
            # the k8s non-root securityContext).
            extraCommands = ''
              mkdir -p etc tmp
              printf 'root:x:0:0::/root:/sbin/nologin\nnobody:x:65534:65534::/:/sbin/nologin\npharos:x:1000:1000::/var/lib/pharos:/sbin/nologin\n' > etc/passwd
              printf 'root:x:0:\nnogroup:x:65534:\npharos:x:1000:\n' > etc/group
              chmod 1777 tmp
            '';
            config = {
              Entrypoint = [ "${entrypoint}/bin/${pname}-entrypoint" ];
              Env = [ "PHAROS_URL=http://pharos:8096" ];
              ExposedPorts = { "${toString port}/tcp" = { }; };
            };
          };

        jellyfinWebImage = mkAngieUi {
          pname = "pharos-jellyfin-web";
          port = 8097;
          prefix = "web";
          bundle = "${pkgs.jellyfin-web}/share/jellyfin-web";
        };
        pharosUiImage = mkAngieUi {
          pname = "pharos-ui";
          port = 8098;
          prefix = "ui";
          bundle = pharosUiBundle;
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
              # nginx serves the bundle + proxies the REST API to pharos
              # over the compose network, so the browser is same-origin.
              environment:
                - PHAROS_URL=http://pharos:8096
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
          integrationFixtures = pharosIntegrationFixtures;
        } // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          # `oci` + `jellyfinWebOci` + compose only meaningful on
          # linux. On darwin the same attrs resolve under
          # packages.<arch>-linux.* and the linux-builder picks up.
          oci = ociImage;
          jellyfinWebOci = jellyfinWebImage;
          pharosUiOci = pharosUiImage;
          pharosUiBundle = pharosUiBundle;
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
            # P52 — workspace dependency-graph + selective test tooling.
            # cargo-guppy enumerates packages affected by a git range
            # (consumed by `just test-changed`); cargo-hakari manages
            # the workspace-hack crate that dedupes feature unification
            # so cargo only compiles a dep once per feature-set
            # (1.7× cumulative speedup on workspace builds).
            pkgs.cargo-guppy
            pkgs.cargo-hakari
            pkgs.cargo-audit
            # crate2nix regenerates Cargo.nix when Cargo.lock changes.
            # Run `just regen-cargo-nix` after touching dependencies.
            pkgs.crate2nix
            pkgs.dioxus-cli
            wasmBindgenCli
            pkgs.ffmpeg-headless
            pkgs.pkg-config
            pkgs.git
            pkgs.just
            pkgs.curl
            # k8s deploy + Tilt inner-loop (charts/pharos + Tiltfile).
            # `just tilt-up` uses ctlptl to stand up a kind cluster + a local
            # OCI registry (wired together); nix builds the images, Tilt
            # pushes them to the registry, the kind node pulls from it, and
            # the Helm chart deploys. helm renders/lint; kubectl for ops.
            pkgs.kubernetes-helm
            pkgs.kind
            pkgs.kubectl
            pkgs.tilt
            pkgs.ctlptl
            pkgs.docker-client
            # `dx build --release` runs wasm-opt (binaryen) over the wasm.
            pkgs.binaryen
            # Node + Playwright drive T29 phase 3. jellyfin-web is the
            # upstream prebuilt static bundle, referenced via
            # JELLYFIN_WEB_DIR at runtime. Browser binaries come from the
            # nix-pinned `playwright-driver.browsers` (exported as
            # PLAYWRIGHT_BROWSERS_PATH below) so the suite runs offline and
            # identically on every machine — no `npx playwright install`.
            # The npm `@playwright/test` version (compat-playwright/
            # package.json) is pinned to match `playwright-driver.version`
            # exactly, so the browser revision the npm package expects is
            # the one present in the store path.
            pkgs.nodejs_22
            pkgs.jellyfin-web
            # schemathesis (Layer A of T29) — install separately via:
            #   pipx install schemathesis
            # Not pinned in the flake because nixpkgs lacks a stable
            # top-level attr today. Layer B (`tests/client_compat.rs`)
            # is the hard CI gate; Layer A is best-effort, manual.
          ];
          # ffmpeg-the-third's *-sys crate runs bindgen over the libav
          # headers (only when building `--features backend-lib`); bindgen
          # needs libclang + the libc / clang-builtin include paths (nix
          # doesn't put them on the default search path). ffmpeg-headless
          # (8.1) already exposes the dev libs via pkg-config, and
          # ffmpeg-the-third v5 supports ffmpeg 8.1 — so the FFI build
          # links the same ffmpeg the runtime uses (no version pin needed).
          LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
          BINDGEN_EXTRA_CLANG_ARGS =
            "-isystem ${pkgs.llvmPackages.libclang.lib}/lib/clang/${
              pkgs.lib.versions.major pkgs.llvmPackages.libclang.version
            }/include "
            + "-isystem ${pkgs.glibc.dev}/include";
          shellHook = ''
            echo "pharos devShell — rust $(rustc --version)"
            export JELLYFIN_WEB_DIR=${pkgs.jellyfin-web}/share/jellyfin-web
            # Pin Playwright's browsers to the nix store path so the compat
            # suite needs no `npx playwright install` and behaves the same
            # on every machine. The version matches package.json's
            # @playwright/test, so chromium-<rev> is present here.
            export PLAYWRIGHT_BROWSERS_PATH=${pkgs.playwright-driver.browsers}
            # nix-store browsers are prebuilt + immutable; skip Playwright's
            # apt-style host-dependency probe (false-negatives on non-Debian
            # distros) and any implicit download attempt.
            export PLAYWRIGHT_SKIP_VALIDATE_HOST_REQUIREMENTS=true
            export PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1
            # Test fixtures for `cargo nextest run -- --ignored
            # ffmpeg_integration`. Built once in /nix/store, cached
            # across CI + dev. Tests skip when env unset.
            export PHAROS_TEST_FIXTURES=${pharosIntegrationFixtures}
          '';
        };

        checks.workspace-build = pharos;

        formatter = pkgs.nixfmt-rfc-style;
      }
    );
}

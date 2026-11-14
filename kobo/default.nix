# The Kobo device package (KoboRoot.tgz): Cobalt at the pinned commit, with
# cobalt.patch applied and the Pharos app added as examples/pharos (ADR-0136).
# `nix build .#kobo`; Linux only.
{
  lib,
  stdenv,
  fetchFromGitHub,
  makeRustPlatform,
  rust-bin,
  writeShellScript,
  zig,
  pkg-config,
  # The server the app signs in to. The device build bakes it in.
  server ? "https://prototype.aliflix.alison-jenkins.com",
}:
let
  # Keep in step with cobalt.env.
  rev = "5ac30a46efce23d7fa0d1cb12a499c9b57b816b0";
  cobalt = fetchFromGitHub {
    owner = "BandarLabs";
    repo = "Cobalt";
    inherit rev;
    hash = "sha256-wR0ItVpdRgst+81QqIE0YbjFSze3GLAcR9jST5Cq4BY=";
  };
  target = "armv7-unknown-linux-musleabihf";
  # Cobalt's rust-toolchain.toml.
  toolchain = rust-bin.stable."1.85.1".default.override { targets = [ target ]; };
  rustPlatform = makeRustPlatform {
    cargo = toolchain;
    rustc = toolchain;
  };
  # Cobalt's CI compiles the device's C (bundled SQLite, ring) with zig, which
  # carries musl headers; cc-rs's --target is dropped because zig spells the
  # triple arm-linux-musleabihf.
  armCc = writeShellScript "armv7-musl-cc" ''
    count=$#
    index=0
    while [ "$index" -lt "$count" ]; do
      argument="$1"
      shift
      case "$argument" in
        --target=*) ;;
        *) set -- "$@" "$argument" ;;
      esac
      index=$((index + 1))
    done
    exec ${zig}/bin/zig cc -target arm-linux-musleabihf -fno-sanitize=undefined "$@"
  '';
  armAr = writeShellScript "armv7-musl-ar" ''exec ${zig}/bin/zig ar "$@"'';
in
stdenv.mkDerivation {
  pname = "pharos-kobo";
  version = "0.1.0";
  src = cobalt;

  # The app adds no crates.io packages of its own, so Cobalt's lock vendors
  # everything; the patch only adds the path package kobo-pharos to it.
  cargoDeps = rustPlatform.importCargoLock { lockFile = "${cobalt}/Cargo.lock"; };

  patches = [ ./cobalt.patch ];
  postPatch = ''
    mkdir -p examples/pharos
    cp -r ${./app}/. examples/pharos/
    chmod -R u+w examples/pharos
  '';

  nativeBuildInputs = [
    toolchain
    rustPlatform.cargoSetupHook
    pkg-config
  ];

  env = {
    PHAROS_KOBO_SERVER = server;
    "CC_${lib.replaceStrings [ "-" ] [ "_" ] target}" = "${armCc}";
    "AR_${lib.replaceStrings [ "-" ] [ "_" ] target}" = "${armAr}";
  };

  buildPhase = ''
    runHook preBuild
    export HOME="$TMPDIR"
    export ZIG_GLOBAL_CACHE_DIR="$TMPDIR/zig-cache"
    export ZIG_LOCAL_CACHE_DIR="$TMPDIR/zig-cache"
    cargo test --offline --locked -p kobo-pharos
    cargo build --offline --locked --release -p kobo-cli
    ./target/release/kobo package --out KoboRoot.tgz
    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall
    install -Dm644 KoboRoot.tgz "$out/KoboRoot.tgz"
    runHook postInstall
  '';

  meta = {
    description = "Cobalt for Kobo e-readers with the Pharos app built in";
    license = lib.licenses.agpl3Only;
    platforms = lib.platforms.linux;
  };
}

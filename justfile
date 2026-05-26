set shell := ["bash", "-cu"]

# Run the full nextest suite via the nix devShell.
test:
    nix develop --command cargo nextest run --workspace

# Clippy under workspace lints (denies warnings).
lint:
    nix develop --command cargo clippy --workspace --all-targets -- -D warnings

# Boot pharos with a known config, run schemathesis against the live
# port, then shut down. Layer A of T29. Requires `pkgs.schemathesis` from
# the devShell.
compat-openapi addr="127.0.0.1:18096":
    @echo "Fetching Jellyfin OpenAPI spec to target/jellyfin-openapi.json"
    mkdir -p target
    curl -fsSL https://api.jellyfin.org/openapi/jellyfin-openapi-stable.json \
        -o target/jellyfin-openapi.json
    @echo "Run pharos under PHAROS_BIND={{addr}} in another shell, then:"
    @echo "  nix develop --command schemathesis run \\"
    @echo "      --base-url http://{{addr}} \\"
    @echo "      --checks all \\"
    @echo "      --hypothesis-max-examples 50 \\"
    @echo "      target/jellyfin-openapi.json"

# Run the in-process Jellyfin client roundtrip test (Layer B).
compat-client:
    nix develop --command cargo nextest run --workspace --test client_compat

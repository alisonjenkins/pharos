//! V350: quire-web must leave this repository unchanged with quire, so its
//! manifest may name no `pharos-*` crate and inherit nothing from the
//! workspace; `quire` is its one path dependency.

#[test]
fn the_manifest_depends_on_nothing_from_the_workspace_but_quire() {
    let manifest = include_str!("../../Cargo.toml");
    for line in manifest
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
    {
        assert!(
            !line.contains("pharos"),
            "pharos named in quire-web's manifest: {line}"
        );
        assert!(
            !line.contains("workspace-hack"),
            "workspace-hack in quire-web's manifest: {line}"
        );
        assert!(
            !line.contains("workspace = true") && !line.contains(".workspace"),
            "inherits from the workspace: {line}"
        );
        assert!(
            !line.contains("path =")
                || line.starts_with("quire = { path = \"../quire\" }")
                || line.starts_with("path = \"examples/"),
            "path dependency other than quire: {line}"
        );
    }
}

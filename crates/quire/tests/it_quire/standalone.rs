//! V350: quire must be liftable out of this repository unchanged, so its
//! manifest may name no `pharos-*` crate, no `workspace-hack` and inherit
//! nothing from a workspace.

#[test]
fn the_manifest_depends_on_nothing_from_the_workspace() {
    let manifest = include_str!("../../Cargo.toml");
    for line in manifest
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
    {
        assert!(
            !line.contains("pharos"),
            "pharos named in quire's manifest: {line}"
        );
        assert!(
            !line.contains("workspace-hack"),
            "workspace-hack in quire's manifest: {line}"
        );
        assert!(
            !line.contains("workspace = true") && !line.contains(".workspace"),
            "inherits from the workspace: {line}"
        );
        assert!(
            !line.contains("path ="),
            "path dependency in quire's manifest: {line}"
        );
    }
}

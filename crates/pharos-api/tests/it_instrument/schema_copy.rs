//! The contract under `specs/017-native-api/contracts/` is what reviewers
//! read; the copy under `crates/pharos-api/schema/` is what builds. They must
//! be the same file, or the spec documents a wire format nobody serves.

#[test]
fn the_spec_contract_and_the_built_schema_are_identical() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let built = std::fs::read_to_string(format!("{root}/crates/pharos-api/schema/pharos.capnp"))
        .expect("built schema");
    let spec = std::fs::read_to_string(format!(
        "{root}/specs/017-native-api/contracts/pharos.capnp"
    ))
    .expect("spec contract");
    assert!(
        built == spec,
        "crates/pharos-api/schema/pharos.capnp and specs/017-native-api/contracts/pharos.capnp differ; copy one over the other"
    );
}

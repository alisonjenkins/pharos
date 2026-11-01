fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=schema/pharos.capnp");
    capnpc::CompilerCommand::new()
        .src_prefix("schema")
        .file("schema/pharos.capnp")
        .run()?;
    Ok(())
}

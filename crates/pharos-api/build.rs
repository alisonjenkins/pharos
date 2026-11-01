#[path = "build/gen.rs"]
mod gen;

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=schema/pharos.capnp");
    println!("cargo:rerun-if-changed=build/gen.rs");
    let out = PathBuf::from(std::env::var("OUT_DIR")?);
    let request = out.join("pharos.request.bin");
    capnpc::CompilerCommand::new()
        .src_prefix("schema")
        .file("schema/pharos.capnp")
        .raw_code_generator_request_path(&request)
        .run()?;
    let generated = gen::generate(&std::fs::read(&request)?)?;
    std::fs::write(out.join("instrumented.rs"), generated)?;
    Ok(())
}

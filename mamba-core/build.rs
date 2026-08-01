fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::compile_protos("proto/control.proto")?;
    println!("cargo:rerun-if-changed=proto/control.proto");
    Ok(())
}

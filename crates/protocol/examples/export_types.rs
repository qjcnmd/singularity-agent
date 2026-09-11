fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../cli/web/src/protocol.generated.ts");
    std::fs::write(path, singularity_protocol::typescript::client_types())?;
    Ok(())
}

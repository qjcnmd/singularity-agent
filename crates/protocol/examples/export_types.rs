fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/desktop/src/protocol.generated.ts");
    std::fs::write(path, protocol::typescript::client_types())?;
    Ok(())
}

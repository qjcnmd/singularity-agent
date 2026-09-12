fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../cli/web/src/protocol.generated.ts");
    // ts_rs wraps long type lines with trailing spaces; strip them so the
    // generated file stays stable under whitespace checks.
    let types = singularity_protocol::typescript::client_types();
    let mut cleaned = types
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    cleaned.push('\n');
    std::fs::write(path, cleaned)?;
    Ok(())
}

use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let check_only = match (args.next().as_deref(), args.next()) {
        (None, None) => false,
        (Some("--check"), None) => true,
        _ => return Err("usage: export_types [--check]".to_string()),
    };

    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/desktop/src/protocol.generated.ts");
    let expected = protocol::typescript::client_types();

    if check_only {
        let actual = std::fs::read_to_string(&path)
            .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
        if actual.replace("\r\n", "\n") != expected.replace("\r\n", "\n") {
            return Err(
                "apps/desktop/src/protocol.generated.ts is out of date with crates/protocol; run `cargo run -p protocol --features typescript --example export_types --locked` to regenerate."
                    .to_string(),
            );
        }
        return Ok(());
    }

    std::fs::write(&path, expected)
        .map_err(|error| format!("failed to write {}: {error}", path.display()))?;
    Ok(())
}

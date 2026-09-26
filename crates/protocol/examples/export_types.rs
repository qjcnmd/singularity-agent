//! 把 `client_types()` 生成的客户端声明写进前端源码目录。

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../apps/desktop/src/protocol.generated.ts");
    std::fs::write(path, singularity_protocol::typescript::client_types())?;
    Ok(())
}

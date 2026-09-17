use std::path::PathBuf;

fn main() {
    let manifest = match std::env::var_os("CARGO_MANIFEST_DIR") {
        Some(path) => PathBuf::from(path),
        None => panic!("CARGO_MANIFEST_DIR is required to embed the WebUI"),
    };
    let web = manifest.join("web");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        let dpi_manifest = manifest.join("windows.manifest");
        println!("cargo:rerun-if-changed={}", dpi_manifest.display());
        println!("cargo:rustc-link-arg-bin=singularity=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-bin=singularity=/MANIFESTINPUT:{}",
            dpi_manifest.display()
        );
    }
    // 只监听真正被消费的已构建 dist：源文件到 dist 由独立的 `npm run build`
    // 负责，Cargo 只把 dist 嵌入可执行文件，两者职责与失效条件因此一致。
    let dist = web.join("dist");
    let index = dist.join("index.html");

    println!("cargo:rerun-if-changed={}", dist.display());

    if !index.is_file() {
        panic!(
            "Singularity WebUI assets are missing. Run `npm --prefix crates/cli/web ci` and `npm --prefix crates/cli/web run build` before Cargo."
        );
    }
}

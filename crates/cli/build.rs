use std::path::PathBuf;

fn main() {
    let manifest = match std::env::var_os("CARGO_MANIFEST_DIR") {
        Some(path) => PathBuf::from(path),
        None => panic!("CARGO_MANIFEST_DIR is required to embed the WebUI"),
    };
    let web = manifest.join("web");
    let dist = web.join("dist");
    let index = dist.join("index.html");

    println!("cargo:rerun-if-changed={}", web.join("src").display());
    println!(
        "cargo:rerun-if-changed={}",
        web.join("index.html").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        web.join("package.json").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        web.join("package-lock.json").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        web.join("tsconfig.json").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        web.join("vite.config.ts").display()
    );
    println!("cargo:rerun-if-changed={}", dist.display());

    if !index.is_file() {
        panic!(
            "Singularity WebUI assets are missing. Run `npm --prefix crates/cli/web ci` and `npm --prefix crates/cli/web run build` before Cargo."
        );
    }
}

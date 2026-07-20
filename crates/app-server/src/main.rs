//! `singularity_app_server` 的 stdio 二进制入口。

mod transport;

fn main() {
    if let Err(error) = transport::run() {
        eprintln!("app-server error: {error}");
        std::process::exit(1);
    }
}

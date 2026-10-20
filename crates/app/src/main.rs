//! Electron 的 stdio AppServer 入口。

mod desktop;
mod session_options;

pub(crate) const PROGRAM_NAME: &str = env!("CARGO_BIN_NAME");

fn main() {
    if let Err(message) = run() {
        eprintln!("{PROGRAM_NAME}: {message}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let (home, _data_lock) = session_options::lock_data_directory()?;
    let runtime = tokio::runtime::Runtime::new().map_err(|error| error.to_string())?;
    let app = session_options::prepare_desktop(&home)?;
    runtime.block_on(desktop::run(app))
}

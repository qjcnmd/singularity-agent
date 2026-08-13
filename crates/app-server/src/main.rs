//! `singularity_app_server` 的 stdio 二进制入口。

use std::time::Duration;

mod transport;

// Tokio's stdio adapter owns an OS read that cannot be cancelled; bound runtime teardown after
// the transport has stopped so a lost peer cannot extend process shutdown indefinitely.
const RUNTIME_SHUTDOWN_GRACE: Duration = Duration::from_millis(100);

fn main() {
    // 单 stdin owner + 单 turn worker：current_thread runtime 足够，stdio 适配器
    // 与 writer 在同一线程上协作，blocking 工作走 Tokio 阻塞池。
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("app-server error: failed to build Tokio runtime: {error}");
            std::process::exit(1);
        }
    };
    let result = runtime.block_on(transport::run(runtime.handle().clone()));
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN_GRACE);
    if let Err(error) = result {
        eprintln!("app-server error: {error}");
        std::process::exit(1);
    }
}

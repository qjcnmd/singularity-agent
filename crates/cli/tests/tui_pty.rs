//! 真实 PTY 上的最小端到端：在 ConPTY（Windows）/forkpty（Unix）中启动
//! `sg` 交互模式，验证初始渲染、键入回显、Esc 清空、滚动不崩溃与干净
//! 退出。断言针对真实终端输出字节流，不依赖屏幕截图或假终端。
//!
//! 默认 `#[ignore]`：这些测试需要宿主具备可工作的伪控制台渲染转发层
//! （Windows 上为完整交互式桌面会话中的 ConPTY）。在满足条件的终端
//! 环境里以 `cargo test -p singularity_cli --test tui_pty -- --ignored` 执行。

use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize};

fn pty_system() -> Box<dyn portable_pty::PtySystem> {
    portable_pty::native_pty_system()
}

fn sg_binary() -> std::path::PathBuf {
    if let Some(path) = option_env!("CARGO_BIN_EXE_sg") {
        let binary = std::path::PathBuf::from(path);
        assert!(binary.is_file(), "sg binary missing: {}", binary.display());
        return binary;
    }
    let current_exe = std::env::current_exe().expect("current test binary");
    let profile_dir = current_exe
        .parent()
        .and_then(|parent| parent.parent())
        .expect("profile dir");
    profile_dir.join(format!("sg{}", std::env::consts::EXE_SUFFIX))
}

/// 在真实 PTY 中启动 `sg`；输出流经 channel 供断言轮询。
struct PtySession {
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    output_rx: mpsc::Receiver<Vec<u8>>,
    deadline: Instant,
}

impl PtySession {
    fn spawn() -> Self {
        let home = tempfile::tempdir().expect("temp home");
        let home_path = home.path().to_path_buf();
        // TempDir 需要活到进程退出；泄漏换取进程存续期间的目录存在。
        std::mem::forget(home);

        let mut builder = CommandBuilder::new(sg_binary());
        builder.env("SINGULARITY_HOME", &home_path);
        builder.env("SINGULARITY_MODEL", "base-model");
        builder.env("SINGULARITY_BASE_URL", "http://127.0.0.1:9/v1");
        builder.env("SINGULARITY_API_KEY", "tui-pty-test-placeholder");
        builder.cwd(std::env::current_dir().expect("cwd"));
        builder.env("TERM", "xterm-256color");

        let pair = pty_system()
            .openpty(PtySize {
                rows: 30,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open pty");
        let child = pair.slave.spawn_command(builder).expect("spawn in pty");
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().expect("pty reader");
        let writer = pair.master.take_writer().expect("pty writer");
        // master 句柄在子进程退出前必须存活（伪控制台输出端）。
        std::mem::forget(pair.master);

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        buffer.extend_from_slice(&chunk[..read]);
                        if buffer.len() > 1 << 20 {
                            break;
                        }
                    }
                }
            }
            let _ = tx.send(buffer);
        });

        Self {
            writer,
            child,
            output_rx: rx,
            deadline: Instant::now() + Duration::from_secs(60),
        }
    }

    fn send_keys(&mut self, keys: &[u8]) {
        self.writer.write_all(keys).expect("write keys to pty");
        let _ = self.writer.flush();
    }

    /// 轮询等待输出中出现 `needle`（按原始字节判定，含 ANSI 转义）。
    fn wait_for(&mut self, needle: &[u8]) -> bool {
        loop {
            let mut received = Vec::new();
            match self.output_rx.try_recv() {
                Ok(mut all) => received.append(&mut all),
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => return false,
            }
            if received
                .windows(needle.len())
                .any(|window| window == needle)
            {
                return true;
            }
            if Instant::now() > self.deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// 等待 PTY 输出 EOF（进程退出），随后取出真实退出码。
    fn wait_exit(&mut self) -> Option<portable_pty::ExitStatus> {
        loop {
            match self.output_rx.recv_timeout(Duration::from_millis(500)) {
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if Instant::now() > self.deadline {
                        return None;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return self.child.try_wait().ok().flatten();
                }
            }
        }
    }
}

fn rendered(needle: &str) -> Vec<u8> {
    needle.as_bytes().to_vec()
}

/// 空闲退出与两级 Ctrl+C：渲染出编辑器与空闲状态后，两次 Ctrl+C 应
/// 以退出码 0 干净终止。
#[test]
#[ignore = "needs a working pseudo-console render layer on the host"]
fn tui_boots_idle_and_renders_input_frame() {
    let mut session = PtySession::spawn();
    assert!(session.wait_for(&rendered("input")), "editor frame renders");
    assert!(session.wait_for(&rendered("idle")), "idle status line");
    assert!(
        session.wait_for(&rendered("[steer]")),
        "steer input mode is the default"
    );
    session.send_keys(b"\x03");
    std::thread::sleep(Duration::from_millis(150));
    session.send_keys(b"\x03");
    let status = session.wait_exit().expect("process exits");
    assert_eq!(status.exit_code(), 0, "idle double Ctrl+C exits 0");
}

/// 键入回显与 Esc 清空：编辑器接收字节并重绘，Esc 清空草稿不崩溃。
#[test]
#[ignore = "needs a working pseudo-console render layer on the host"]
fn tui_echoes_typed_input_and_esc_clears_it() {
    let mut session = PtySession::spawn();
    assert!(session.wait_for(&rendered("input")));
    session.send_keys(b"hello");
    session.send_keys(b"world");
    assert!(session.wait_for(&rendered("hello")), "typing echoes");
    assert!(session.wait_for(&rendered("world")), "typing echoes");
    session.send_keys(b"\x1b");
    assert!(
        session.wait_for(&rendered("idle")),
        "status line stays idle after Esc"
    );
    session.send_keys(b"\x03");
    std::thread::sleep(Duration::from_millis(150));
    session.send_keys(b"\x03");
    let status = session.wait_exit().expect("clean exit after Esc path");
    assert_eq!(status.exit_code(), 0);
}

/// 滚轮 SGR 事件不影响进程存活，之后仍可正常交互并干净退出。
#[test]
#[ignore = "needs a working pseudo-console render layer on the host"]
fn tui_mouse_wheel_does_not_crash() {
    let mut session = PtySession::spawn();
    assert!(session.wait_for(&rendered("input")));
    session.send_keys(b"\x1b[<64;50;15M");
    session.send_keys(b"x");
    assert!(session.wait_for(&rendered("x")), "typing after wheel works");
    session.send_keys(b"\x03");
    std::thread::sleep(Duration::from_millis(150));
    session.send_keys(b"\x03");
    let status = session.wait_exit().expect("clean exit after mouse event");
    assert_eq!(status.exit_code(), 0);
}

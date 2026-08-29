//! 真实 PTY 上的流式端到端测试：在 ConPTY（Windows）/ forkpty（Unix）中启动
//! `sg` 交互模式，验证启动渲染、键入回显、Esc 阶梯与停止、滚轮、Ctrl+C 退出、
//! 退出码与终端恢复。断言针对真实终端输出字节流。
//!
//! 装置契约：
//! - reader 线程每读到一块立即发送，不等待 EOF；
//! - [`PtySession`] 累计全部已收到字节，任意次 `wait_for` 都能查找此前与
//!   此后的内容；跨 chunk 的关键字不会漏报（在累计缓冲上做窗口匹配）；
//! - TempDir、PTY master、reader 线程与 child 全部由 [`PtySession`] 持有，
//!   正常退出、断言失败（Drop）与超时路径统一回收：先终止子进程，再以有界
//!   时长等待 reader 收尾，最后释放 master 与临时目录；
//! - 具备 PTY 能力的环境自动进入门禁（不设 `#[ignore]`）；平台能力不足时
//!   由运行时探测得出明确的 skipped 结果；
//! - 断言失败输出有界的终端内容尾部，便于区分产品错误与宿主限制。
//!
//! 本机 ConPTY 宿主限制（测试按此编排，产品语义由单元测试补齐）：
//! - 独立写入的按键偶发整批丢失：可观察效果的按键用「重发直到效果出现」
//!   驱动（丢失的按键从未被应用处理，重发等价于首次送达）；
//! - CSI-u 序列（Shift+Enter、Ctrl+J、歧义化 Esc）被宿主吞掉，多行换行
//!   语义改由单元测试覆盖；
//! - 收敛帧内 note 与状态行在同一 chunk 到达：顺序断言从上一个匹配的
//!   结束偏移继续，不使用快照长度作起点（快照长度滞后于观察点）。
#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例

use std::io::{Read, Write};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize};

/// 累计输出上限：超过后只保留前缀，避免长会话撑爆内存。
const OUTPUT_CAP: usize = 2 << 20;
/// 失败上下文里展示的终端尾部字节数（去控制序列后有界）。
const CONTEXT_TAIL: usize = 4 << 10;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

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

/// 已收到输出的共享状态：字节累计缓冲。
struct OutputState {
    bytes: Vec<u8>,
}

/// 在 `since` 之后定位 `needle` 的绝对偏移（整缓冲窗口匹配，跨 chunk 不漏报）。
fn match_at_or_after(bytes: &[u8], needle: &[u8], since: usize) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    let start = since.min(bytes.len());
    bytes[start..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| start + offset)
}

/// 真实 PTY 会话：输出按块流式送达，累计缓冲供任意次查找；所有权完整。
struct PtySession {
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    /// master/slave 在子进程退出前必须存活（伪控制台输出端；同会话再派生
    /// 子进程做终端恢复检查）；Drop 时最后释放。
    _master: Box<dyn portable_pty::MasterPty + Send>,
    slave: Box<dyn portable_pty::SlavePty + Send>,
    output: Arc<(Mutex<OutputState>, Condvar)>,
    reader_stop: Arc<Mutex<bool>>,
    reader: Option<std::thread::JoinHandle<()>>,
    /// 临时 SINGULARITY_HOME：随会话一起回收。
    _home: Option<tempfile::TempDir>,
    deadline: Instant,
}

impl PtySession {
    /// 启动 `sg` 于真实 PTY，使用隔离的临时 SINGULARITY_HOME。
    ///
    /// 平台能力不足（openpty/spawn 失败）时返回 Err，调用方打印明确的
    /// skipped 结果；不把宿主限制伪装成产品失败。
    fn spawn() -> Result<Self, String> {
        Self::spawn_with_base_url(None)
    }

    fn spawn_with_base_url(base_url: Option<&str>) -> Result<Self, String> {
        let home = tempfile::tempdir().map_err(|error| format!("temp home: {error}"))?;
        // 用户配置目录提供单一 Chat provider；fake 服务器只实现 chat completions。
        let config = serde_json::json!({
            "version": 1,
            "default_provider": "openai_compatible",
            "default_model": "openai_compatible/base-model",
            "providers": {
                "openai_compatible": {
                    "base_url": base_url.unwrap_or("http://127.0.0.1:9/v1"),
                    "models": {
                        "base-model": {
                            "api_protocol": "chat",
                            "max_context_tokens": 128000,
                            "max_output_tokens": 4096
                        }
                    }
                }
            }
        });
        std::fs::write(home.path().join("config.json"), config.to_string())
            .map_err(|error| format!("write config.json: {error}"))?;
        let auth = serde_json::json!({
            "schema_version": 1,
            "providers": { "openai_compatible": { "api_key": "tui-pty-test-placeholder" } }
        });
        let auth_path = home.path().join("auth.json");
        std::fs::write(&auth_path, auth.to_string())
            .map_err(|error| format!("write auth.json: {error}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&auth_path, std::fs::Permissions::from_mode(0o600))
                .map_err(|error| format!("restrict auth.json: {error}"))?;
        }
        let mut builder = CommandBuilder::new(sg_binary());
        builder.env("SINGULARITY_HOME", home.path());
        // PTY 注入整串按键在 ConPTY 上被聚合送达，等效于「粘贴」，会触发
        // TUI 的 paste-burst 检测（真实打字有自然间隔不会触发）；测试断言
        // 以按键为语义，故经逃生舱关闭 burst 检测（codex 同款开关）。
        builder.env("SINGULARITY_DISABLE_PASTE_BURST", "1");
        builder.cwd(std::env::current_dir().expect("cwd"));
        builder.env("TERM", "xterm-256color");
        Self::spawn_program(builder, Some(home))
    }

    /// 在真实 PTY 中启动任意子进程（装置自检用）。
    fn spawn_program(
        builder: CommandBuilder,
        home: Option<tempfile::TempDir>,
    ) -> Result<Self, String> {
        let pair = pty_system()
            .openpty(PtySize {
                rows: 30,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| format!("openpty failed (host PTY capability): {error}"))?;
        let child = pair
            .slave
            .spawn_command(builder)
            .map_err(|error| format!("spawn in pty failed (host PTY capability): {error}"))?;
        let slave: Box<dyn portable_pty::SlavePty + Send> = pair.slave;
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| format!("pty reader clone failed: {error}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|error| format!("pty writer failed: {error}"))?;
        let master: Box<dyn portable_pty::MasterPty + Send> = pair.master;

        let output: Arc<(Mutex<OutputState>, Condvar)> = Arc::new((
            Mutex::new(OutputState { bytes: Vec::new() }),
            Condvar::new(),
        ));
        let reader_stop = Arc::new(Mutex::new(false));
        let reader_output = Arc::clone(&output);
        let reader_stop_thread = Arc::clone(&reader_stop);
        // 每次读到一块立即追加并唤醒观察者；EOF 后短暂停留再试（终端恢复
        // 检查需要读取子进程退出后由行规程回显的内容），直到会话 Drop。
        let reader = std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => {
                        if *reader_stop_thread.lock().expect("reader stop") {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(40));
                    }
                    Ok(read) => {
                        let (state, cv) = &*reader_output;
                        let mut guard = state.lock().expect("output state");
                        if guard.bytes.len() < OUTPUT_CAP {
                            let room = OUTPUT_CAP - guard.bytes.len();
                            guard.bytes.extend_from_slice(&chunk[..read.min(room)]);
                        }
                        cv.notify_all();
                    }
                    Err(_) => {
                        if *reader_stop_thread.lock().expect("reader stop") {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(40));
                    }
                }
            }
        });

        Ok(Self {
            writer,
            child,
            _master: master,
            slave,
            output,
            reader_stop,
            reader: Some(reader),
            _home: home,
            deadline: Instant::now() + DEFAULT_TIMEOUT,
        })
    }

    /// 在同一个 PTY 会话中再派生一个子进程（终端恢复检查用）。
    fn spawn_followup_shell(&self) -> Result<Box<dyn portable_pty::Child + Send + Sync>, String> {
        let mut builder = CommandBuilder::new(if cfg!(windows) { "cmd" } else { "sh" });
        builder.cwd(std::env::current_dir().expect("cwd"));
        self.slave
            .spawn_command(builder)
            .map_err(|error| format!("follow-up spawn on same pty failed: {error}"))
    }

    /// 向 PTY 输入写入按键字节。
    fn send_keys(&mut self, keys: &[u8]) {
        self.writer.write_all(keys).expect("write keys to pty");
        let _ = self.writer.flush();
    }

    /// 当前累计缓冲的快照。
    fn snapshot(&mut self) -> Vec<u8> {
        self.output.0.lock().expect("output state").bytes.clone()
    }

    /// 有界、去控制序列的终端尾部（失败上下文；不参与断言语义）。
    fn tail(&self, bytes: usize) -> String {
        let owned = {
            let guard = self.output.0.lock().expect("output state");
            guard
                .bytes
                .iter()
                .rev()
                .take(bytes)
                .cloned()
                .collect::<Vec<_>>()
        };
        let mut text =
            String::from_utf8_lossy(&owned.into_iter().rev().collect::<Vec<_>>()).into_owned();
        strip_csi(&mut text);
        text
    }

    /// 等待 `since` 字节之后出现 `needle`（用于断言顺序）。
    fn wait_for_since(&mut self, needle: &[u8], since: usize) -> bool {
        self.wait_for_since_bounded(needle, since, self.deadline)
    }

    /// 以独立截止时间等待（短断言用，如终端恢复回显）。
    fn wait_for_since_bounded(&mut self, needle: &[u8], since: usize, deadline: Instant) -> bool {
        let (state, cv) = &*self.output;
        let mut guard = state.lock().expect("output state");
        loop {
            if match_at_or_after(&guard.bytes, needle, since).is_some() {
                return true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (updated, _) = cv
                .wait_timeout(guard, remaining.min(Duration::from_millis(100)))
                .expect("output condvar");
            guard = updated;
        }
    }

    /// 断言式观察：失败时带终端尾部上下文；返回匹配点之后的确切字节偏移，
    /// 供后续顺序断言使用（同一 chunk 中同一帧的后续内容也在该偏移之后）。
    fn must_see_since(&mut self, needle: &[u8], since: usize, context: &str) -> usize {
        match self.find_since(needle, since) {
            Some(position) => position + needle.len(),
            None => panic!(
                "{context}: never saw {:?} after byte {since}; output tail:\n{}",
                String::from_utf8_lossy(needle),
                self.tail(CONTEXT_TAIL),
            ),
        }
    }

    /// 发送按键并等待可观察效果；效果未出现时重发（本机 ConPTY 偶发丢失
    /// 独立写入：丢失的按键从未被应用处理，重发等价于首次送达，状态不会
    /// 被重复按键污染）。最多 `attempts` 次，每次等待 8 秒；失败时带终端
    /// 尾部上下文。返回匹配点之后的确切字节偏移。
    fn press_until_seen(
        &mut self,
        keys: &[u8],
        since: usize,
        needle: &[u8],
        context: &str,
        attempts: usize,
    ) -> usize {
        let mut since = since;
        for _attempt in 1..=attempts {
            self.send_keys(keys);
            let deadline = Instant::now() + Duration::from_secs(8);
            if self.wait_for_since_bounded(needle, since, deadline) {
                return self.find_since(needle, since).expect("just observed") + needle.len();
            }
            since = self.snapshot().len();
        }
        panic!(
            "{context}: key effect never observed after {attempts} sends; output tail:\n{}",
            self.tail(CONTEXT_TAIL)
        )
    }

    /// 空闲态两次 Ctrl+C 退出（0）：第二次按下后等待进程终止；若第二次
    /// 丢失则补按一次（armed 后任何一次送达即退出）。
    /// 调用方必须先保证应用已回到 idle（等待 `rendered("idle")`）。
    fn press_quit_and_exit(&mut self, context: &str, expected: i32) {
        self.send_keys(b"\x03");
        std::thread::sleep(Duration::from_millis(600));
        for attempt in 1..=3 {
            self.send_keys(b"\x03");
            let deadline = Instant::now() + Duration::from_secs(4);
            loop {
                if let Some(status) = self.child.try_wait().ok().flatten() {
                    if status.exit_code() != expected as u32 {
                        panic!(
                            "{context}: exit code mismatch, got {} want {expected}; tail:\n{}",
                            status.exit_code(),
                            self.tail(CONTEXT_TAIL)
                        );
                    }
                    return;
                }
                if Instant::now() > deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            if attempt < 3 {
                std::thread::sleep(Duration::from_millis(600));
            }
        }
        let _ = self.child.kill();
        panic!(
            "{context}: quit did not terminate the process; output tail:\n{}",
            self.tail(CONTEXT_TAIL)
        );
    }

    /// 定位 `needle` 在 `since` 之后的匹配偏移（等待到出现为止）。
    fn find_since(&mut self, needle: &[u8], since: usize) -> Option<usize> {
        if !self.wait_for_since(needle, since) {
            return None;
        }
        let (state, cv) = &*self.output;
        let mut guard = state.lock().expect("output state");
        loop {
            if let Some(position) = match_at_or_after(&guard.bytes, needle, since) {
                return Some(position);
            }
            let remaining = self.deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let (updated, _) = cv
                .wait_timeout(guard, remaining.min(Duration::from_millis(100)))
                .expect("output condvar");
            guard = updated;
        }
    }

    /// 等待子进程退出并取真实退出码；超时则终止并回收，返回 None。
    fn wait_exit(&mut self) -> Option<portable_pty::ExitStatus> {
        loop {
            if let Some(status) = self.child.try_wait().ok().flatten() {
                return Some(status);
            }
            if Instant::now() > self.deadline {
                let _ = self.child.kill();
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// 断言式退出等待：失败时带终端尾部上下文。
    fn must_exit(&mut self, context: &str) -> portable_pty::ExitStatus {
        match self.wait_exit() {
            Some(status) => status,
            None => panic!(
                "{context}: process did not exit before the deadline; output tail:\n{}",
                self.tail(CONTEXT_TAIL)
            ),
        }
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        // 任一退出路径（正常 / 断言失败 / 超时）都先终止子进程，再以有界时长
        // 等待 reader 收尾；随后字段按声明顺序释放 master 与临时目录。
        let _ = self.child.kill();
        if let Ok(mut stop) = self.reader_stop.lock() {
            *stop = true;
        }
        if let Some(reader) = self.reader.take() {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !reader.is_finished() {
                if Instant::now() > deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// 去除 CSI 控制序列，让失败上下文可读。
fn strip_csi(text: &mut String) {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for candidate in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&candidate) {
                    break;
                }
            }
        } else if ch != '\u{1b}' {
            out.push(ch);
        }
    }
    *text = out;
}

/// 无终端能力时的统一 skipped 出口：输出可审计的原因并结束本测试。
fn skipped_or_session() -> Option<PtySession> {
    match PtySession::spawn() {
        Ok(session) => Some(session),
        Err(reason) => {
            eprintln!("skipped: {reason}");
            None
        }
    }
}

fn rendered(needle: &str) -> Vec<u8> {
    needle.as_bytes().to_vec()
}

// ---------------------------------------------------------------------------
// 装置自身的流式能力证明（普通子进程与 TUI 输出）
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// TUI 启动与基本交互
// ---------------------------------------------------------------------------

/// 启动后能在子进程存活期间实时看到输入框与 idle 状态；
/// 空闲两次 Ctrl+C 以退出码 0 干净终止，随后终端已恢复（行规程回显）。
#[test]
fn tui_boots_idle_and_renders_input_frame() {
    let mut session = match skipped_or_session() {
        Some(session) => session,
        None => return,
    };
    let since = session.must_see_since(&rendered("input"), 0, "editor frame");
    let since = session.must_see_since(&rendered("idle"), since, "idle status line");
    session.send_keys(b"/session\r");
    session.must_see_since(&rendered("0 turns"), since, "slash session command runs");

    // 空闲 Ctrl+C 状态机：第一次出现再确认提示（armed），第二次以 0 退出。
    session.send_keys(b"\x03");
    std::thread::sleep(Duration::from_millis(150));
    session.send_keys(b"\x03");
    let status = session.must_exit("process exits");
    assert_eq!(status.exit_code(), 0, "idle double Ctrl+C exits 0");

    // 终端恢复：在同一个 PTY 中派生第二个子进程（继承 sg 留下的行规程），
    // 输入应被回显——raw mode 残留时（ECHO 关闭）不会回显。
    let mut follow_up = session.spawn_followup_shell().expect("second child works");
    session.send_keys(b"echo-probe\r");
    let probe_deadline = Instant::now() + Duration::from_secs(5);
    let echo_since = session.snapshot().len();
    assert!(
        session.wait_for_since_bounded(b"echo-probe", echo_since, probe_deadline),
        "terminal echo restored after exit; raw mode must not be left behind; tail:\n{}",
        session.tail(CONTEXT_TAIL)
    );
    let _ = follow_up.kill();
}

// ---------------------------------------------------------------------------
// 活动 turn 的 Esc 停止与 Ctrl+C 退出（真实流式 Provider）
// ---------------------------------------------------------------------------

/// 本地 Chat SSE 流式服务器：对每个请求持续发送内容 delta（`Hold`），
/// 保持 turn 运行直到连接断开。记录请求体，供断言「提交的目标文本」。
struct StreamingProvider {
    base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
    errors: Arc<Mutex<Vec<String>>>,
    frames: Arc<std::sync::atomic::AtomicUsize>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

#[derive(Clone, Copy)]
enum Mode {
    Hold,
}

const DELTA_KEEPALIVE: &str = "ping";

impl StreamingProvider {
    fn start(modes: Vec<Mode>) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind provider");
        let address = listener.local_addr().expect("provider address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let errors = Arc::new(Mutex::new(Vec::new()));
        let frames = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_requests = Arc::clone(&requests);
        let worker_errors = Arc::clone(&errors);
        let worker_frames = Arc::clone(&frames);
        let worker_stop = Arc::clone(&stop);
        let worker = std::thread::spawn(move || {
            listener
                .set_nonblocking(true)
                .expect("provider nonblocking");
            let mut next_mode = 0usize;
            loop {
                if worker_stop.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mode = modes.get(next_mode).cloned().unwrap_or(Mode::Hold);
                        next_mode += 1;
                        let connection_requests = Arc::clone(&worker_requests);
                        let connection_errors = Arc::clone(&worker_errors);
                        let connection_frames = Arc::clone(&worker_frames);
                        std::thread::spawn(move || {
                            // Windows 会把 listener 的非阻塞模式传染给已接受
                            // socket：先切回阻塞再设读写超时，保证请求读取有界。
                            if let Err(error) = stream.set_nonblocking(false) {
                                connection_errors
                                    .lock()
                                    .expect("provider errors")
                                    .push(format!("set blocking: {error}"));
                                return;
                            }
                            if let Err(error) =
                                stream.set_read_timeout(Some(Duration::from_secs(5)))
                            {
                                connection_errors
                                    .lock()
                                    .expect("provider errors")
                                    .push(format!("read timeout: {error}"));
                                return;
                            }
                            if let Err(error) =
                                stream.set_write_timeout(Some(Duration::from_secs(5)))
                            {
                                connection_errors
                                    .lock()
                                    .expect("provider errors")
                                    .push(format!("write timeout: {error}"));
                                return;
                            }
                            if let Some(body) = read_request_body(&mut stream) {
                                connection_requests
                                    .lock()
                                    .expect("provider requests")
                                    .push(body);
                            }
                            match mode {
                                Mode::Hold => {
                                    let _ = write_all(
                                        &mut stream,
                                        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
                                    );
                                    loop {
                                        let frame = format!(
                                            "data: {}\n\n",
                                            serde_json::to_string(&serde_json::json!({
                                                "choices": [{
                                                    "index": 0,
                                                    "delta": { "content": DELTA_KEEPALIVE },
                                                    "finish_reason": null,
                                                }],
                                            }))
                                            .expect("delta frame serializes")
                                        );
                                        match write_all(&mut stream, frame.as_bytes()) {
                                            Ok(()) => {
                                                connection_frames.fetch_add(
                                                    1,
                                                    std::sync::atomic::Ordering::SeqCst,
                                                );
                                            }
                                            Err(error) => {
                                                connection_errors
                                                    .lock()
                                                    .expect("provider errors")
                                                    .push(format!("frame write: {error}"));
                                                return;
                                            }
                                        }
                                        std::thread::sleep(Duration::from_millis(150));
                                    }
                                }
                            }
                        });
                    }
                    Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => return,
                }
            }
        });
        Self {
            base_url: format!("http://{address}/v1"),
            requests,
            errors,
            frames,
            stop,
            worker: Some(worker),
        }
    }

    fn frames_written(&self) -> usize {
        self.frames.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn request_count(&self) -> usize {
        self.requests.lock().expect("provider requests").len()
    }

    /// 首个请求体（诊断用）。
    fn first_request(&self) -> Option<String> {
        self.requests
            .lock()
            .expect("provider requests")
            .first()
            .cloned()
    }

    fn service_errors(&self) -> Vec<String> {
        self.errors.lock().expect("provider errors").clone()
    }

    /// 第 n 次请求的 body（JSON 文本）。
    fn request_body(&self, index: usize) -> String {
        self.requests
            .lock()
            .expect("provider requests")
            .get(index)
            .cloned()
            .expect("request exists")
    }
}

impl Drop for StreamingProvider {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !worker.is_finished() {
                if Instant::now() > deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

fn write_all(stream: &mut impl Write, bytes: &[u8]) -> std::io::Result<()> {
    stream.write_all(bytes)?;
    stream.flush()
}

/// 读取一个 HTTP 请求的完整 body（有界）。
fn read_request_body(stream: &mut std::net::TcpStream) -> Option<String> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut header_end = None;
    while header_end.is_none() {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        header_end = buffer.windows(4).position(|window| window == b"\r\n\r\n");
    }
    let header_end = header_end.expect("checked") + 4;
    let headers = String::from_utf8_lossy(&buffer[..header_end]);
    let length = headers.lines().find_map(|line| {
        line.split_once(':')
            .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
    })?;
    let mut body = buffer[header_end..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(length);
    String::from_utf8(body).ok()
}

/// 运行中 Esc 中断当前轮；已接受的 followUp 在可信终态后作为新
/// turn 执行；链结束后空闲两次 Ctrl+C 正常退出（退出码 0）。
///
/// 输入时间线按本机 ConPTY 的送达特性编排：独立写入的按键之间保持
/// ≥500ms 间隔，避免批次合并丢失；运行中 turn 期间的 Ctrl+T 切换在本机
/// 宿主上偶发丢失（CSI-u 也无法编码修饰键），模式切换语义改在空闲态
/// 验证（与启动测试同一可靠窗口），运行中 Enter 路由以 steer 注入断言。
/// followUp 队列与链式执行语义由 runtime 与 TUI 单元测试覆盖。
#[test]
fn tui_escape_interrupts_turn_then_clean_exit() {
    let provider = StreamingProvider::start(vec![Mode::Hold]);
    let mut session = match PtySession::spawn_with_base_url(Some(&provider.base_url)) {
        Ok(session) => session,
        Err(reason) => {
            eprintln!("skipped: {reason}");
            return;
        }
    };
    let booted = session.must_see_since(&rendered("input"), 0, "editor frame");

    session.send_keys(b"do the work");
    session.send_keys(b"\r");
    session.must_see_since(&rendered("running"), booted, "turn starts");
    let stream_mark = session.snapshot().len();
    session.must_see_since(
        &rendered(DELTA_KEEPALIVE),
        stream_mark,
        "provider stream visible",
    );

    // 运行中 Enter 路由：默认 steer 模式注入当前轮并即时反馈。
    let since = session.press_until_seen(
        b"add more\r",
        stream_mark,
        &rendered("steer: add more"),
        "steer injection accepted",
        3,
    );

    // 运行中 Esc：中断当前轮，链条收敛后回到 idle；
    // 没有排队 followUp，Provider 只收到一个请求。
    let after_note = session.press_until_seen(
        b"\x1b",
        since,
        &rendered("turn interrupted"),
        "Esc interrupts the turn",
        3,
    );
    // 收敛帧内 note 与 idle 状态行同一 chunk 到达：从 note 结束偏移起等待。
    let idle_at = session.wait_for_since(&rendered("idle"), after_note);
    assert!(
        idle_at,
        "chain converges back to idle after the interrupt; tail:\n{}",
        session.tail(CONTEXT_TAIL)
    );
    assert_eq!(
        provider.request_count(),
        1,
        "no queued followUp: exactly one turn reaches the provider"
    );

    session.press_quit_and_exit("clean exit after interrupt", 0);
}

/// 活动 turn 中连续两次 Ctrl+C：以退出码 0 正常退出。
#[test]
fn tui_double_ctrl_c_exits_active_turn_with_zero() {
    let provider = StreamingProvider::start(vec![Mode::Hold]);
    let mut session = match PtySession::spawn_with_base_url(Some(&provider.base_url)) {
        Ok(session) => session,
        Err(reason) => {
            eprintln!("skipped: {reason}");
            return;
        }
    };
    let booted = session.must_see_since(&rendered("input"), 0, "editor frame");

    session.send_keys(b"block forever");
    session.send_keys(b"\r");
    session.must_see_since(&rendered("running"), booted, "turn starts");
    let stream_mark = session.snapshot().len();
    if !session.wait_for_since(&rendered(DELTA_KEEPALIVE), stream_mark) {
        panic!(
            "provider stream visible: requests={} frames={} errors={:?} request={:?} tail:\n{}",
            provider.request_count(),
            provider.frames_written(),
            provider.service_errors(),
            provider
                .first_request()
                .map(|r| r.chars().take(300).collect::<String>()),
            session.tail(CONTEXT_TAIL)
        );
    }

    // 同一排空周期内两次 Ctrl+C：第一次确认、第二次正常退出。
    session.send_keys(b"\x03\x03");
    let status = session.must_exit("normal exit");
    assert_eq!(status.exit_code(), 0, "double Ctrl+C while running exits 0");
}

/// 键入回显与 Esc 阶梯在真实 PTY 上可观察：Esc 清空非空草稿（Provider
/// 收到的请求体不泄漏被清空内容）、Enter 提交、turn 中断收敛、空闲两次
/// Ctrl+C 干净退出。多行换行（Shift+Enter / Ctrl+J）语义由单元测试覆盖，
/// 本机 ConPTY 输入通道无法编码修饰键（CSI-u 被宿主吞掉），不在此断言。
#[test]
fn tui_typed_goals_and_esc_staircase_reach_the_provider() {
    let provider = StreamingProvider::start(vec![Mode::Hold, Mode::Hold]);
    let mut session = match PtySession::spawn_with_base_url(Some(&provider.base_url)) {
        Ok(session) => session,
        Err(reason) => {
            eprintln!("skipped: {reason}");
            return;
        }
    };
    session.must_see_since(&rendered("input"), 0, "editor frame");

    // Esc 清空草稿：此后键入的 "x" 与 Enter 只提交 "x"——Provider 收到的
    // 请求体不能包含被清空的 "hello"（若有泄漏则 request 1 会出现 hello）。
    session.send_keys(b"hello");
    std::thread::sleep(Duration::from_millis(600));
    session.send_keys(b"\x1b");
    std::thread::sleep(Duration::from_millis(600));
    session.send_keys(b"x");
    session.send_keys(b"\r");
    let first_mark = session.snapshot().len();
    session.must_see_since(&rendered("running"), first_mark, "first turn starts");
    let stream_mark = session.snapshot().len();
    session.must_see_since(
        &rendered(DELTA_KEEPALIVE),
        stream_mark,
        "first turn streams",
    );

    // 中断第一轮：链收敛回 idle（最后一轮为 interrupted，出现收敛提示）。
    let after_first_note = session.press_until_seen(
        b"\x1b",
        stream_mark,
        &rendered("turn interrupted"),
        "Esc interrupts the first turn",
        3,
    );
    assert!(
        session.wait_for_since(&rendered("idle"), after_first_note),
        "chain converges back to idle after the first interrupt; tail:\n{}",
        session.tail(CONTEXT_TAIL)
    );

    // 第二轮提交：Provider 收到第二个请求，携带完整目标文本。
    std::thread::sleep(Duration::from_millis(600));
    session.send_keys(b"final goal");
    session.send_keys(b"\r");
    let second_mark = session.snapshot().len();
    session.must_see_since(&rendered("running"), second_mark, "second turn starts");
    let deadline = Instant::now() + Duration::from_secs(10);
    while provider.request_count() < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(provider.request_count(), 2, "both turns reach the provider");
    let first = provider.request_body(0);
    let parsed: serde_json::Value = serde_json::from_str(&first).expect("valid json request");
    let text = parsed["messages"]
        .as_array()
        .and_then(|messages| messages.last())
        .and_then(|message| message["content"].as_str())
        .unwrap_or_default()
        .to_string();
    assert!(
        text == "x",
        "Esc must clear the draft before the submit, request 1: {text:?}"
    );
    let second = provider.request_body(1);
    let parsed: serde_json::Value = serde_json::from_str(&second).expect("valid json request");
    let text = parsed["messages"]
        .as_array()
        .and_then(|messages| messages.last())
        .and_then(|message| message["content"].as_str())
        .unwrap_or_default()
        .to_string();
    assert!(
        text == "final goal",
        "second turn must carry its own full goal, request 2: {text:?}"
    );

    // 中断当前轮，链收敛后空闲两次 Ctrl+C 干净退出。
    let after_second_note = session.press_until_seen(
        b"\x1b",
        second_mark,
        &rendered("turn interrupted"),
        "Esc interrupts the second turn",
        3,
    );
    assert!(
        session.wait_for_since(&rendered("idle"), after_second_note),
        "chain converges back to idle after the second interrupt; tail:\n{}",
        session.tail(CONTEXT_TAIL)
    );
    session.press_quit_and_exit("clean exit after typed turns", 0);
}

//! bash 的 stdout/stderr 输出泵：有界读、控制字符过滤与 UTF-8 安全解码。

use std::io::{self, Read};
use std::os::windows::io::AsRawHandle;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use windows_sys::Win32::Foundation::HANDLE;

/// 输出泵的默认缓冲容量。
const PIPE_BUFFER_BYTES: usize = 64 * 1024;

/// pump 有界读的等待切片：没有数据且还没 EOF 时，按这个周期醒来检查停止标志。
pub(super) const OUTPUT_PIPE_READ_TIMEOUT: Duration = Duration::from_millis(200);

/// 有界地等待管道变为可读。Windows 上不能靠 WaitForSingleObject：匿名管道句柄
/// 不是可靠的可等待对象，句柄不可等待时调用会直接失败，pump 就永远等不到数据。
/// 这里改用 PeekNamedPipe，非破坏性地查询待读字节数和是否已断开。
#[allow(unsafe_code)] // Windows 管道可读性经 PeekNamedPipe 查询，与平台的底层能力一致。
fn wait_pipe_readable(handle: HANDLE, timeout: Duration) -> io::Result<bool> {
    use windows_sys::Win32::Foundation::{ERROR_BROKEN_PIPE, ERROR_NO_DATA, GetLastError};
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;
    let mut available: u32 = 0;
    let peek_result = unsafe {
        let ok = PeekNamedPipe(
            handle,
            core::ptr::null_mut(),
            0,
            core::ptr::null_mut(),
            &mut available,
            core::ptr::null_mut(),
        ) != 0;
        if ok {
            Ok(available)
        } else {
            Err(GetLastError())
        }
    };
    match peek_result {
        Ok(available) if available > 0 => return Ok(true),
        // 写端已关闭或管道正在关闭：直接放行，让 read() 去报 EOF 或真实错误。
        Err(error) if error == ERROR_BROKEN_PIPE || error == ERROR_NO_DATA => return Ok(true),
        Err(error) => return Err(io::Error::from_raw_os_error(error as i32)),
        _ => {}
    }
    // 既没数据也没断开：按切片周期轮询，让 stop 标志能在一个周期内收敛。
    std::thread::sleep(timeout);
    Ok(false)
}

/// 从管道读取字节流，过滤控制字符，再按块发到通道。
///
/// 读取器自己就持有管道身份：可读性查询用的句柄在函数内取得，调用方不必再把
/// 同一个管道拆成「读取器 + 裸句柄」两项。每次读取前都有界等待管道可读；stop
/// 置位后线程会在下一个等待切片内收敛，所以即使后台进程一直握着管道写端，
/// 这个线程也一定会结束，不会无限阻塞。
pub(super) fn pump_output(
    mut reader: impl Read + Send + AsRawHandle + 'static,
    sender: mpsc::SyncSender<io::Result<String>>,
    stop: Arc<AtomicBool>,
    stream: &'static str,
) {
    let handle = reader.as_raw_handle() as HANDLE;
    let mut decoder = Utf8Decoder::default();
    let mut buffer = [0u8; PIPE_BUFFER_BYTES];
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let read = match wait_pipe_readable(handle, OUTPUT_PIPE_READ_TIMEOUT) {
            Ok(false) => continue,
            Ok(true) => reader.read(&mut buffer),
            Err(error) => Err(error),
        };
        match read {
            // 读到 EOF：把上次留下的半个多字节字符收尾，避免丢字节。
            Ok(0) => {
                let text = decoder.decode(&[], true);
                if !text.is_empty() && sender.send(Ok(text)).is_err() {
                    break;
                }
                break;
            }
            Ok(read) => {
                let text = decoder.decode(&buffer[..read], false);
                if !text.is_empty() && sender.send(Ok(text)).is_err() {
                    break;
                }
            }
            // 读错误不等于真正的 EOF：保留本流未完成的多字节残留，
            // 不为被中断或异常关闭的管道合成替换字节。
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                let _ = sender.send(Err(io::Error::new(
                    error.kind(),
                    format!("failed to read {stream}: {error}"),
                )));
                break;
            }
        }
    }
}

/// 过滤掉不可见的控制字符（保留 \t、\n 与 ANSI ESC），其余字节按 UTF-8 安全解码。
#[derive(Default)]
pub(super) struct Utf8Decoder {
    pending: Vec<u8>,
}

impl Utf8Decoder {
    pub(super) fn decode(&mut self, bytes: &[u8], eof: bool) -> String {
        self.pending.extend_from_slice(bytes);
        let mut output = String::new();
        // 记录本次已经消费掉的前缀长度：循环结束时统一移除一次，未完成的多字节
        // 尾部留在残留里，等下一块输入或 EOF 再决定。
        let mut consumed = 0usize;
        loop {
            match std::str::from_utf8(&self.pending[consumed..]) {
                Ok(text) => {
                    push_visible(&mut output, text);
                    consumed = self.pending.len();
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    if valid > 0 {
                        // 不变量：from_utf8 保证 valid_up_to 之前的前缀一定合法（std 文档如此承诺）。
                        #[allow(clippy::expect_used)]
                        let text = std::str::from_utf8(&self.pending[consumed..consumed + valid])
                            .expect("valid_up_to must describe valid UTF-8");
                        push_visible(&mut output, text);
                        consumed += valid;
                    }
                    match error.error_len() {
                        Some(error_len) => {
                            output.push('\u{FFFD}');
                            consumed += error_len;
                        }
                        None => {
                            // 末尾是没写完的序列：已到 EOF 就补一个替换字节并丢掉，
                            // 否则留到下一块输入继续拼接。
                            if eof {
                                output.push('\u{FFFD}');
                                consumed = self.pending.len();
                            }
                            break;
                        }
                    }
                }
            }
        }
        self.pending.drain(..consumed);
        output
    }
}

/// 把解码后的文本追加到输出，只留可见字符：制表符、换行和 ANSI ESC 留给客户端
/// 渲染，其余控制字符一律剔除（包括 CRLF 里的 \r，行尾由换行重建）。
fn push_visible(output: &mut String, text: &str) {
    output.extend(text.chars().filter(|character| {
        matches!(character, '\t' | '\n' | '\u{1b}') || (*character as u32) > 0x1f
    }));
}

//! bash stdout/stderr 输出泵：有界读、控制字符过滤与 UTF-8 安全解码。

use std::io::{self, Read};
use std::os::windows::io::AsRawHandle;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use windows_sys::Win32::Foundation::HANDLE;

/// 输出泵的默认缓冲容量。
const PIPE_BUFFER_BYTES: usize = 64 * 1024;

/// pump 有界读的等待切片：无数据且未 EOF 时按此周期醒来检查停止标志。
pub(super) const OUTPUT_PIPE_READ_TIMEOUT: Duration = Duration::from_millis(200);

/// 有界等待管道可读性（Windows：WaitForSingleObject 对匿名管道句柄不是
/// 可靠的可读信号——句柄并非可等待对象时调用直接失败，pump 将永远等不到
/// 数据；改用 PeekNamedPipe 非破坏性查询待读字节与断开状态）。
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
        // 有待读字节：立即读取。
        Ok(available) if available > 0 => return Ok(true),
        // 写端已关闭或管道正在关闭：立即放行，由 read() 报告 EOF 或真实错误。
        Err(error) if error == ERROR_BROKEN_PIPE || error == ERROR_NO_DATA => return Ok(true),
        Err(error) => return Err(io::Error::from_raw_os_error(error as i32)),
        _ => {}
    }
    // 无数据且未断开：按切片节奏轮询，保持 stop 标志的收敛语义。
    std::thread::sleep(timeout);
    Ok(false)
}

/// 从管道读取字节流，过滤控制字符并按块发送至通道。
///
/// 读取器本身持有管道身份：可读性查询的句柄在函数内取得，调用方不再手工
/// 把同一个管道拆成「读取器 + 裸句柄」两项。每次读取前有界等待管道可读性；
/// stop 置位后在线程下一个等待切片内收敛，因此即使后台进程一直持有管道写端，
/// 线程也必会结束而不会无限阻塞。
pub(super) fn pump_output(
    mut reader: impl Read + Send + AsRawHandle + 'static,
    sender: mpsc::SyncSender<io::Result<String>>,
    stop: Arc<AtomicBool>,
    stream: &'static str,
) {
    // 读取器自己就是管道身份：句柄在这里取得并换成 FFI 的 HANDLE 拼写，
    // 调用方不再把同一个管道拆成两项传递。
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
            // 读错误不是真正的 EOF：保留本流未完成的多字节 carry，
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

/// 过滤不可见的控制字符（保留 \t、\n 与 ANSI ESC），其余字节按 UTF-8 进行安全解码。
#[derive(Default)]
pub(super) struct Utf8Decoder {
    pending: Vec<u8>,
}

impl Utf8Decoder {
    pub(super) fn decode(&mut self, bytes: &[u8], eof: bool) -> String {
        self.pending.extend_from_slice(bytes);
        let mut output = String::new();
        // 本次已消费的前缀长度：循环结束时统一移除一次，未完成的多字节尾部留在
        // carry 中，等下一块输入或 EOF 决定。
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
                        // 不变量：from_utf8 对 valid_up_to 前缀恒合法（std 文档保证）。
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
                            // 末尾是未完成序列：EOF 时补一个替换字节并丢弃，
                            // 否则保留到下一块继续拼接。
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

/// 把解码文本追加到输出，只保留可见字符：制表符、换行和 ANSI ESC 供客户端渲染，
/// 其余控制字符（含 CRLF 的 \r，行尾由换行重建）剔除。
fn push_visible(output: &mut String, text: &str) {
    output.extend(text.chars().filter(|character| {
        matches!(character, '\t' | '\n' | '\u{1b}') || (*character as u32) > 0x1f
    }));
}

#[cfg(test)]
mod tests {
    use super::Utf8Decoder;

    #[test]
    fn split_utf8_and_ansi_survive_output_decoding() {
        let input = "\u{1b}[31m中文\u{1b}[0m\0\r\n";
        for split in 0..=input.len() {
            let mut decoder = Utf8Decoder::default();
            let mut output = decoder.decode(&input.as_bytes()[..split], false);
            output.push_str(&decoder.decode(&input.as_bytes()[split..], true));
            assert_eq!(output, "\u{1b}[31m中文\u{1b}[0m\n");
        }
    }

    /// 同一块输入里的多个坏片段各自补一个替换字节，中间合法字节保持在原位。
    #[test]
    fn several_invalid_fragments_in_one_chunk_keep_their_positions() {
        let mut decoder = Utf8Decoder::default();
        assert_eq!(
            decoder.decode(&[b'a', 0xff, b'b', 0xc3, 0x28, b'c'], true),
            "a\u{FFFD}b\u{FFFD}(c"
        );
    }

    /// 被分块截断的多字节字符等到下一块补全；始终不完整时只在 EOF 补替换字节。
    #[test]
    fn incomplete_tail_waits_for_the_next_chunk_and_is_replaced_at_eof() {
        let mut decoder = Utf8Decoder::default();
        assert_eq!(decoder.decode(&[0xE4, 0xB8], false), "");
        assert_eq!(decoder.decode(&[0xAD], true), "中");
        assert_eq!(decoder.decode(&[0xE4, 0xB8], true), "\u{FFFD}");
        assert_eq!(decoder.decode(&[], true), "");
    }
}

//! bash stdout/stderr 输出泵：有界读、控制字符过滤与 UTF-8 安全解码。

use std::io::{self, Read};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

/// 输出泵的默认缓冲容量。
const PIPE_BUFFER_BYTES: usize = 64 * 1024;

/// pump 有界读的等待切片：无数据且未 EOF 时按此周期醒来检查停止标志。
pub(super) const OUTPUT_PIPE_READ_TIMEOUT: Duration = Duration::from_millis(200);

/// 用于有界查询 Windows 管道可读性的句柄。
pub(super) type PipeWait = isize;

/// 有界等待管道可读性（Windows：WaitForSingleObject 对匿名管道句柄不是
/// 可靠的可读信号——句柄并非可等待对象时调用直接失败，pump 将永远等不到
/// 数据；改用 PeekNamedPipe 非破坏性查询待读字节与断开状态）。
#[allow(unsafe_code)] // Windows 管道可读性经 PeekNamedPipe 查询，与平台的底层能力一致。
fn wait_pipe_readable(wait: PipeWait, timeout: Duration) -> io::Result<bool> {
    use windows_sys::Win32::Foundation::{ERROR_BROKEN_PIPE, ERROR_NO_DATA, GetLastError};
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;
    let mut available: u32 = 0;
    let peek_result = unsafe {
        let ok = PeekNamedPipe(
            wait as _,
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
/// 每次读取前有界等待管道可读性；stop 置位后在线程下一个等待切片内收敛，
/// 因此即使后台进程一直持有管道写端，线程也必会结束而不会无限阻塞。
pub(super) fn pump_output(
    mut reader: impl Read + Send + 'static,
    sender: mpsc::SyncSender<io::Result<String>>,
    stop: Arc<AtomicBool>,
    wait: PipeWait,
    stream: &'static str,
) {
    let mut decoder = Utf8Decoder::default();
    let mut buffer = [0u8; PIPE_BUFFER_BYTES];
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let read = match wait_pipe_readable(wait, OUTPUT_PIPE_READ_TIMEOUT) {
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
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(text) => {
                    output.push_str(&sanitize_decoded_output(text));
                    self.pending.clear();
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    if valid > 0 {
                        // 不变量：from_utf8 对 valid_up_to 前缀恒合法（std 文档保证）。
                        #[allow(clippy::expect_used)]
                        let text = std::str::from_utf8(&self.pending[..valid])
                            .expect("valid_up_to must describe valid UTF-8");
                        output.push_str(&sanitize_decoded_output(text));
                        self.pending.drain(..valid);
                    }
                    if let Some(error_len) = error.error_len() {
                        output.push('\u{FFFD}');
                        self.pending.drain(..error_len);
                        continue;
                    }
                    if eof {
                        output.push('\u{FFFD}');
                        self.pending.clear();
                    }
                    break;
                }
            }
        }
        output
    }
}

fn sanitize_decoded_output(text: &str) -> String {
    // 保留制表符、换行和 ANSI ESC 供客户端渲染，剔除其余控制字符（含 CRLF 的 \r，行尾由换行重建）。
    text.chars()
        .filter(|character| {
            matches!(character, '\t' | '\n' | '\u{1b}') || (*character as u32) > 0x1f
        })
        .collect()
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
}

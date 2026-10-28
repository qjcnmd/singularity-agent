//! Bash 两条输出流的异步读取与 UTF-8 解码。

use std::io;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{ChildStderr, ChildStdout};

const PIPE_BUFFER_BYTES: usize = 64 * 1024;

pub(super) struct OutputChunk {
    pub(super) display: String,
    pub(super) model: String,
}

#[derive(Default)]
struct PlainText(String);

impl vte::Perform for PlainText {
    fn print(&mut self, character: char) {
        self.0.push(character);
    }

    fn execute(&mut self, byte: u8) {
        if matches!(byte, b'\t' | b'\n') {
            self.0.push(char::from(byte));
        }
    }
}

pub(super) struct OutputStreams {
    stdout: OutputReader<ChildStdout>,
    stderr: OutputReader<ChildStderr>,
}

impl OutputStreams {
    pub(super) fn new(stdout: ChildStdout, stderr: ChildStderr) -> Self {
        Self {
            stdout: OutputReader::new(stdout, "stdout"),
            stderr: OutputReader::new(stderr, "stderr"),
        }
    }

    pub(super) fn is_open(&self) -> bool {
        !self.stdout.closed || !self.stderr.closed
    }

    /// 两条流都结束才返回 None；一条流关闭后继续读取另一条。
    pub(super) async fn next(&mut self) -> Option<io::Result<OutputChunk>> {
        loop {
            let chunk = tokio::select! {
                chunk = self.stdout.next(), if !self.stdout.closed => chunk,
                chunk = self.stderr.next(), if !self.stderr.closed => chunk,
                else => return None,
            };
            if chunk.is_some() {
                return chunk;
            }
        }
    }
}

struct OutputReader<R> {
    reader: R,
    stream: &'static str,
    decoder: Utf8Decoder,
    // stdout / stderr 各自保留解析状态，控制序列跨块或两条流交错时也不会泄漏到模型。
    terminal: vte::Parser,
    buffer: Vec<u8>,
    closed: bool,
}

impl<R: AsyncRead + Unpin> OutputReader<R> {
    fn new(reader: R, stream: &'static str) -> Self {
        Self {
            reader,
            stream,
            decoder: Utf8Decoder::default(),
            terminal: vte::Parser::new(),
            buffer: vec![0; PIPE_BUFFER_BYTES],
            closed: false,
        }
    }

    async fn next(&mut self) -> Option<io::Result<OutputChunk>> {
        while !self.closed {
            match self.reader.read(&mut self.buffer).await {
                Ok(read) => {
                    self.closed = read == 0;
                    // 只有 EOF 才补全末尾残缺字符；取消读取不会伪造输出。
                    let text = self.decoder.decode(&self.buffer[..read], self.closed);
                    if !text.is_empty() {
                        let mut model = PlainText::default();
                        self.terminal.advance(&mut model, text.as_bytes());
                        let mut display = String::new();
                        push_visible(&mut display, &text);
                        return Some(Ok(OutputChunk { display, model: model.0 }));
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    self.closed = true;
                    return Some(Err(io::Error::new(
                        error.kind(),
                        format!("failed to read {}: {error}", self.stream),
                    )));
                }
            }
        }
        None
    }
}

/// 按 UTF-8 安全解码；控制字符留给后续的模型解析与界面投影。
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
                    output.push_str(text);
                    consumed = self.pending.len();
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    if valid > 0 {
                        // valid_up_to 之前的前缀是合法 UTF-8。
                        let text = std::str::from_utf8(&self.pending[consumed..consumed + valid])
                            .expect("valid_up_to must describe valid UTF-8");
                        output.push_str(text);
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
    output.extend(
        text.chars()
            .filter(|character| matches!(character, '\t' | '\n' | '\u{1b}') || (*character as u32) > 0x1f),
    );
}

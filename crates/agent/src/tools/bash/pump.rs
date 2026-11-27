use std::io;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{ChildStderr, ChildStdout};

const PIPE_BUFFER_BYTES: usize = 64 * 1024;

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

    pub(super) async fn next(&mut self) -> Option<io::Result<String>> {
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

    async fn next(&mut self) -> Option<io::Result<String>> {
        while !self.closed {
            match self.reader.read(&mut self.buffer).await {
                Ok(read) => {
                    self.closed = read == 0;
                    let text = self.decoder.decode(&self.buffer[..read], self.closed);
                    if !text.is_empty() {
                        let mut plain = PlainText::default();
                        self.terminal.advance(&mut plain, text.as_bytes());
                        if !plain.0.is_empty() {
                            return Some(Ok(plain.0));
                        }
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
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

#[derive(Default)]
pub(super) struct Utf8Decoder {
    pending: Vec<u8>,
}

impl Utf8Decoder {
    pub(super) fn decode(&mut self, bytes: &[u8], eof: bool) -> String {
        self.pending.extend_from_slice(bytes);
        let mut output = String::new();
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
                        let text = std::str::from_utf8(&self.pending[consumed..consumed + valid])
                            .expect("valid_up_to must describe valid UTF-8");
                        output.push_str(text);
                        consumed += valid;
                    }
                    if let Some(error_len) = error.error_len() {
                        output.push('\u{FFFD}');
                        consumed += error_len;
                    } else {
                        if eof {
                            output.push('\u{FFFD}');
                            consumed = self.pending.len();
                        }
                        break;
                    }
                }
            }
        }
        self.pending.drain(..consumed);
        output
    }
}

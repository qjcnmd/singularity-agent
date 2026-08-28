//! 无括号粘贴终端的粘贴 burst 检测。
//!
//! 部分终端（典型如 Windows 控制台）不提供 bracketed paste，粘贴以一段
//! 快速到达的 `Char`/`Enter` 按键流呈现。本状态机把这种流识别为一次粘贴：
//! 高频字符先缓冲成单一 paste，burst 中的 Enter 当作换行而非提交，静默
//! 间隙后整体 flush。它不直接改动编辑器，只输出决策，由调用方落地。

use std::time::{Duration, Instant};

/// 连续无修饰字符达到该数量即判定为粘贴型 burst。
const PASTE_BURST_MIN_CHARS: u16 = 3;
/// Enter 抑制窗口：burst 活动后短暂时间内 Enter 仍按换行处理，让多行
/// 粘贴末尾略微延迟的 Enter 保持分组。
const PASTE_ENTER_SUPPRESS_WINDOW: Duration = Duration::from_millis(120);
/// 相邻两字符视为同一 burst 的最大间隔。
const PASTE_BURST_CHAR_INTERVAL: Duration = Duration::from_millis(8);
/// burst 缓冲后的静默超时，超时即整体 flush 为一次粘贴。Windows 上的
/// 粘贴流更慢，给更长窗口避免过早拆散。
#[cfg(not(windows))]
const PASTE_BURST_ACTIVE_IDLE_TIMEOUT: Duration = Duration::from_millis(8);
#[cfg(windows)]
const PASTE_BURST_ACTIVE_IDLE_TIMEOUT: Duration = Duration::from_millis(60);

#[derive(Default)]
pub(crate) struct PasteBurst {
    last_plain_char_time: Option<Instant>,
    consecutive_plain_char_burst: u16,
    burst_window_until: Option<Instant>,
    buffer: String,
    active: bool,
    /// 暂存的首个快速 ASCII 字符：等待下一个字符以决定是单次输入还是
    /// burst，避免单个字符先渲染造成的闪烁。
    pending_first_char: Option<(char, Instant)>,
}

pub(crate) enum CharDecision {
    /// 开始缓冲，并把光标前已渲染的 `retro_chars` 个字符回抓进缓冲。
    BeginBuffer { retro_chars: u16 },
    /// 缓冲已激活，追加当前字符。
    BufferAppend,
    /// 暂存首个快速字符，不渲染。
    RetainFirstChar,
    /// 首个暂存字符随本次一起开始缓冲，无需回抓。
    BeginBufferFromPending,
}

pub(crate) enum FlushResult {
    Paste(String),
    Typed(char),
    None,
}

impl PasteBurst {
    /// 判定单个普通字符（ASCII）的处理方式。
    pub fn on_plain_char(&mut self, ch: char, now: Instant) -> CharDecision {
        self.note_plain_char(now);

        if self.active {
            self.burst_window_until = Some(now + PASTE_ENTER_SUPPRESS_WINDOW);
            return CharDecision::BufferAppend;
        }
        if let Some((held, held_at)) = self.pending_first_char
            && now.duration_since(held_at) <= PASTE_BURST_CHAR_INTERVAL
        {
            // 首个暂存字符尚未渲染，直接与当前字符一起开始缓冲。
            self.active = true;
            self.pending_first_char = None;
            self.buffer.push(held);
            self.burst_window_until = Some(now + PASTE_ENTER_SUPPRESS_WINDOW);
            return CharDecision::BeginBufferFromPending;
        }
        if self.consecutive_plain_char_burst >= PASTE_BURST_MIN_CHARS {
            return CharDecision::BeginBuffer {
                retro_chars: self.consecutive_plain_char_burst.saturating_sub(1),
            };
        }
        self.pending_first_char = Some((ch, now));
        CharDecision::RetainFirstChar
    }

    /// 判定非 ASCII（IME 等）字符：不暂存，避免输入感延迟；burst 判定
    /// 仍保留，供粘贴中文等场景使用。
    pub fn on_plain_char_no_hold(&mut self, now: Instant) -> Option<CharDecision> {
        self.note_plain_char(now);

        if self.active {
            self.burst_window_until = Some(now + PASTE_ENTER_SUPPRESS_WINDOW);
            return Some(CharDecision::BufferAppend);
        }
        if self.consecutive_plain_char_burst >= PASTE_BURST_MIN_CHARS {
            return Some(CharDecision::BeginBuffer {
                retro_chars: self.consecutive_plain_char_burst.saturating_sub(1),
            });
        }
        None
    }

    fn note_plain_char(&mut self, now: Instant) {
        match self.last_plain_char_time {
            Some(prev) if now.duration_since(prev) <= PASTE_BURST_CHAR_INTERVAL => {
                self.consecutive_plain_char_burst =
                    self.consecutive_plain_char_burst.saturating_add(1)
            }
            _ => self.consecutive_plain_char_burst = 1,
        }
        self.last_plain_char_time = Some(now);
    }

    /// 静默超时后 flush：缓冲内容作为一次粘贴，暂存字符作为普通输入。
    pub fn flush_if_due(&mut self, now: Instant) -> FlushResult {
        let timeout = if self.is_active_internal() {
            PASTE_BURST_ACTIVE_IDLE_TIMEOUT
        } else {
            PASTE_BURST_CHAR_INTERVAL
        };
        let timed_out = self
            .last_plain_char_time
            .is_some_and(|t| now.duration_since(t) > timeout);
        if timed_out && self.is_active_internal() {
            self.active = false;
            return FlushResult::Paste(std::mem::take(&mut self.buffer));
        }
        if timed_out && let Some((ch, _)) = self.pending_first_char.take() {
            return FlushResult::Typed(ch);
        }
        FlushResult::None
    }

    /// burst 上下文中的 Enter：追加换行到缓冲，不触发提交。
    pub fn append_newline_if_active(&mut self, now: Instant) -> bool {
        if self.is_active() {
            self.buffer.push('\n');
            self.burst_window_until = Some(now + PASTE_ENTER_SUPPRESS_WINDOW);
            true
        } else {
            false
        }
    }

    /// Enter 是否应插入换行而非提交：burst 激活或抑制窗口内。
    pub fn newline_should_insert_instead_of_submit(&self, now: Instant) -> bool {
        let in_burst_window = self.burst_window_until.is_some_and(|until| now <= until);
        self.is_active() || in_burst_window
    }

    /// 应用非文本输入前立即冲刷缓冲（不等待超时），避免残留文本卡住。
    pub fn flush_before_modified_input(&mut self) -> Option<String> {
        if !self.is_active_internal() {
            return None;
        }
        self.active = false;
        let mut out = std::mem::take(&mut self.buffer);
        if let Some((ch, _)) = self.pending_first_char.take() {
            out.push(ch);
        }
        Some(out)
    }

    /// 追加字符到已激活的缓冲。
    pub fn append_char_to_buffer(&mut self, ch: char, now: Instant) {
        self.buffer.push(ch);
        self.burst_window_until = Some(now + PASTE_ENTER_SUPPRESS_WINDOW);
    }

    /// 回抓判定：光标前 `retro_chars` 个字符是否像粘贴（含空白或足够长）。
    /// 返回 `(起始字节, 被回抓文本)`；不像粘贴时返回 `None`，调用方按普通
    /// 输入插入即可。
    pub fn decide_begin_buffer(
        &mut self,
        now: Instant,
        before: &str,
        retro_chars: usize,
    ) -> Option<(usize, String)> {
        let start_byte = retro_start_index(before, retro_chars);
        let grabbed = before[start_byte..].to_string();
        let looks_pastey =
            grabbed.chars().any(char::is_whitespace) || grabbed.chars().count() >= 16;
        if looks_pastey {
            self.active = true;
            self.buffer.push_str(&grabbed);
            self.burst_window_until = Some(now + PASTE_ENTER_SUPPRESS_WINDOW);
            Some((start_byte, grabbed))
        } else {
            None
        }
    }

    /// 显式 paste（bracketed paste 事件）后清空全部 transient 状态，防止
    /// 影响后续普通输入。
    pub fn clear_after_explicit_paste(&mut self) {
        self.last_plain_char_time = None;
        self.consecutive_plain_char_burst = 0;
        self.burst_window_until = None;
        self.active = false;
        self.buffer.clear();
        self.pending_first_char = None;
    }

    /// 清除计时窗口而不 emit 缓冲：供非文本输入后调用，避免下一按键被
    /// 并入上一 burst。调用方须先 flush 缓冲。
    pub fn clear_window_after_non_char(&mut self) {
        self.consecutive_plain_char_burst = 0;
        self.last_plain_char_time = None;
        self.burst_window_until = None;
        self.active = false;
        self.pending_first_char = None;
    }

    /// 是否处于任何 burst 相关 transient 状态（缓冲中、缓冲非空或暂存
    /// 首字符）。
    pub fn is_active(&self) -> bool {
        self.is_active_internal() || self.pending_first_char.is_some()
    }

    fn is_active_internal(&self) -> bool {
        self.active || !self.buffer.is_empty()
    }
}

/// 取 `before` 末尾 `retro_chars` 个字符的起始字节（UTF-8 边界安全）。
fn retro_start_index(before: &str, retro_chars: usize) -> usize {
    if retro_chars == 0 {
        return before.len();
    }
    before
        .char_indices()
        .rev()
        .nth(retro_chars.saturating_sub(1))
        .map(|(idx, _)| idx)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
    use super::*;
    use std::time::Instant;

    /// 单个快速 ASCII 字符被暂存，超时后作为普通输入 flush。
    #[test]
    fn ascii_first_char_held_then_flushes_as_typed() {
        let mut burst = PasteBurst::default();
        let t0 = Instant::now();
        assert!(matches!(
            burst.on_plain_char('a', t0),
            CharDecision::RetainFirstChar
        ));
        let t1 = t0 + PASTE_BURST_CHAR_INTERVAL + Duration::from_millis(1);
        assert!(matches!(burst.flush_if_due(t1), FlushResult::Typed('a')));
        assert!(!burst.is_active());
    }

    /// 两个快速 ASCII 字符开始缓冲，静默后整体 flush 为一次粘贴。
    #[test]
    fn two_fast_ascii_chars_flush_as_paste() {
        let mut burst = PasteBurst::default();
        let t0 = Instant::now();
        assert!(matches!(
            burst.on_plain_char('a', t0),
            CharDecision::RetainFirstChar
        ));
        let t1 = t0 + Duration::from_millis(1);
        assert!(matches!(
            burst.on_plain_char('b', t1),
            CharDecision::BeginBufferFromPending
        ));
        burst.append_char_to_buffer('b', t1);

        let t2 = t1 + PASTE_BURST_ACTIVE_IDLE_TIMEOUT + Duration::from_millis(1);
        assert!(matches!(
            burst.flush_if_due(t2),
            FlushResult::Paste(ref s) if s == "ab"
        ));
    }

    /// burst 中的 Enter 追加换行，不触发提交；抑制窗口过后恢复提交语义。
    #[test]
    fn enter_inside_burst_appends_newline_with_suppression_window() {
        let mut burst = PasteBurst::default();
        let t0 = Instant::now();
        assert!(matches!(
            burst.on_plain_char('a', t0),
            CharDecision::RetainFirstChar
        ));
        let t1 = t0 + Duration::from_millis(1);
        assert!(matches!(
            burst.on_plain_char('b', t1),
            CharDecision::BeginBufferFromPending
        ));
        burst.append_char_to_buffer('b', t1);

        // 缓冲激活：Enter 是换行。
        assert!(burst.append_newline_if_active(t1));
        assert!(burst.newline_should_insert_instead_of_submit(t1));

        // 静默 flush 后抑制窗口仍在：Enter 仍按换行处理。
        let t2 = t1 + PASTE_BURST_ACTIVE_IDLE_TIMEOUT + Duration::from_millis(1);
        assert!(matches!(burst.flush_if_due(t2), FlushResult::Paste(ref s) if s == "ab\n"));
        assert!(burst.newline_should_insert_instead_of_submit(t2));

        // 抑制窗口过期后恢复提交语义。
        let t3 = t1 + PASTE_ENTER_SUPPRESS_WINDOW + Duration::from_millis(1);
        assert!(!burst.newline_should_insert_instead_of_submit(t3));
    }

    /// 非文本输入前立即冲刷缓冲。
    #[test]
    fn flush_before_modified_input_emits_buffer_immediately() {
        let mut burst = PasteBurst::default();
        let t0 = Instant::now();
        assert!(matches!(
            burst.on_plain_char('a', t0),
            CharDecision::RetainFirstChar
        ));
        let t1 = t0 + Duration::from_millis(1);
        assert!(matches!(
            burst.on_plain_char('b', t1),
            CharDecision::BeginBufferFromPending
        ));
        burst.append_char_to_buffer('b', t1);
        assert_eq!(burst.flush_before_modified_input(), Some("ab".to_string()));
        assert!(!burst.is_active());
    }

    /// 回抓判定：仅当光标前文本像粘贴（含空白或足够长）才回抓。
    #[test]
    fn decide_begin_buffer_only_triggers_for_pastey_prefixes() {
        let mut burst = PasteBurst::default();
        let now = Instant::now();
        assert!(burst.decide_begin_buffer(now, "ab", 2).is_none());
        assert!(!burst.is_active());

        let (start, grabbed) = burst
            .decide_begin_buffer(now, "a b", 2)
            .expect("whitespace prefix should be pastey");
        assert_eq!(start, 1);
        assert_eq!(grabbed, " b");
        assert!(burst.is_active());
    }

    /// 非 ASCII 路径不暂存首字符，burst 判定仍可用。
    #[test]
    fn non_ascii_no_hold_allows_burst_detection() {
        let mut burst = PasteBurst::default();
        let t0 = Instant::now();
        assert!(burst.on_plain_char_no_hold(t0).is_none());
        let t1 = t0 + Duration::from_millis(1);
        assert!(burst.on_plain_char_no_hold(t1).is_none());
        let t2 = t1 + Duration::from_millis(1);
        assert!(matches!(
            burst.on_plain_char_no_hold(t2),
            Some(CharDecision::BeginBuffer { retro_chars: 2 })
        ));
    }
}

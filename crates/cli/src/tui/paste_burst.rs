//! 非括号粘贴的按键突发检测：部分终端（尤其是 Windows）把粘贴送达为高速
//! `Char`/`Enter` 按键流而非单个粘贴事件。本机把突发拼回单个粘贴文本，
//! 突发内的 Enter 视为换行而非提交；慢速输入原样直通。
//!
//! 与 Codex `paste_burst` 同形、阈值同源（连续快字符成组、字符间隔 8ms、
//! Enter 压制窗 120ms、Windows 冲刷 60ms）。相对 Codex 省掉了 retro-grab：
//! 本编辑器以行为单位，首字 hold 已足以让「双字符前缀 + Enter」的小粘贴
//! 正确成组，无需回抓已渲染文本。状态机是纯的（时间由调用方传入），
//! 不直接改编辑器，调用方按决策执行。

use std::time::{Duration, Instant};

/// 连续字符判定为同一突发的最大间隔：人类不可能以该间隔持续击键，
/// 按键重复频率（约 30Hz）也远慢于此。
const CHAR_INTERVAL: Duration = Duration::from_millis(8);

/// 突发结束后 Enter 仍视为换行的时间窗（压制提交）。
const ENTER_SUPPRESS_WINDOW: Duration = Duration::from_millis(120);

/// 突发停顿多久后落定为粘贴。Windows 下曾观测到较慢的突发，按 Codex
/// 同源取值区分平台。
#[cfg(windows)]
const BURST_IDLE_TIMEOUT: Duration = Duration::from_millis(60);
#[cfg(not(windows))]
const BURST_IDLE_TIMEOUT: Duration = Duration::from_millis(8);

/// 纯文本字符的归类。调用方按决策执行：`Held` 暂不渲染（等后随或到期
/// 冲刷）；`Buffered` 已入突发串；`Typed` 当前字按打字立即插入（冷字直通：
/// 距上一字符超过间隔的按键是人手，立即落屏不等待）。突发只在连续快字符
/// 的第二字起成组，首字若被单独消费则按打字落屏、后续字成串，内容顺序
/// 不变。状态机是纯的（时间由调用方传入），不直接改编辑器，调用方按决策
/// 执行。
#[derive(Debug, PartialEq, Eq)]
pub(super) enum CharDecision {
    Held,
    Buffered,
    Typed(char),
}

/// 到期/强制冲刷的结果：`Paste` 整串走粘贴路径，`Typed` 单字当打字插入。
#[derive(Debug, PartialEq, Eq)]
pub(super) enum FlushOutcome {
    Paste(String),
    Typed(char),
    None,
}

/// Enter 的归类：`Buffered` 记入突发换行；`LocalNewline` 本地换行；
/// `Submit` 走原提交路径。
#[derive(Debug, PartialEq, Eq)]
pub(super) enum EnterDecision {
    Buffered,
    LocalNewline,
    Submit,
}

#[derive(Debug, Default)]
pub(super) struct PasteBurst {
    /// 暂扣的首个快字符：等 8ms 看后随，超时按打字吐出。
    held: Option<(char, Instant)>,
    /// 已成组的突发累积（含突发内 Enter 转成的 `\n`）。
    buffer: String,
    active: bool,
    last_char_at: Option<Instant>,
    /// 压制窗deadline：突发后 120ms 内的 Enter 仍视为换行。
    suppress_until: Option<Instant>,
}

impl PasteBurst {
    /// 为及时落定建议的 poll 等待上限；无待定内容时为 `None`。
    pub(super) fn poll_grace(&self) -> Option<Duration> {
        if self.held.is_some() {
            Some(CHAR_INTERVAL + Duration::from_millis(1))
        } else if self.active {
            Some(BURST_IDLE_TIMEOUT + Duration::from_millis(1))
        } else {
            None
        }
    }

    pub(super) fn on_char(&mut self, ch: char, now: Instant) -> CharDecision {
        let fast = self
            .last_char_at
            .is_some_and(|at| now.duration_since(at) <= CHAR_INTERVAL);
        self.last_char_at = Some(now);
        if self.active {
            self.buffer.push(ch);
            self.suppress_until = Some(now + ENTER_SUPPRESS_WINDOW);
            return CharDecision::Buffered;
        }
        if let Some((held, _)) = self.held.take() {
            if fast {
                // 第二个快字符：成组（首字从 hold 直接入串，免回抓）。
                self.active = true;
                self.buffer.push(held);
                self.buffer.push(ch);
                self.suppress_until = Some(now + ENTER_SUPPRESS_WINDOW);
                return CharDecision::Buffered;
            }
            // 扣住的字姗姗来迟地落定：按打字吐出，当前字冷字直通。
            return CharDecision::Typed(held);
        }
        if fast {
            // 有快前件但前件已落屏（直通字）：重新起 hold 观察成组。
            self.held = Some((ch, now));
            return CharDecision::Held;
        }
        // 冷字直通：距上一字符超过间隔，人手打字，立即落屏不等待。
        CharDecision::Typed(ch)
    }

    pub(super) fn on_enter(&mut self, now: Instant) -> EnterDecision {
        if self.active {
            self.push_newline(now);
            return EnterDecision::Buffered;
        }
        if let Some((held, _)) = self.held.take() {
            // 单快字符前缀 + Enter（如 `a\n…` 小粘贴）：成组后记入换行，
            // 否则首字会滞留、Enter 会误提交。
            self.active = true;
            self.buffer.push(held);
            self.push_newline(now);
            return EnterDecision::Buffered;
        }
        if self.suppress_until.is_some_and(|until| now <= until) {
            return EnterDecision::LocalNewline;
        }
        EnterDecision::Submit
    }

    /// 到期冲刷（事件循环每轮调用）：hold 超时吐单字，突发停顿吐整串。
    pub(super) fn flush_if_due(&mut self, now: Instant) -> FlushOutcome {
        if let Some((held, at)) = self.held
            && now.duration_since(at) > CHAR_INTERVAL
        {
            self.held = None;
            return FlushOutcome::Typed(held);
        }
        if self.active
            && self
                .last_char_at
                .is_some_and(|at| now.duration_since(at) > BURST_IDLE_TIMEOUT)
        {
            self.active = false;
            let out = std::mem::take(&mut self.buffer);
            if out.is_empty() {
                return FlushOutcome::None;
            }
            return FlushOutcome::Paste(out);
        }
        FlushOutcome::None
    }

    /// 非字符/非 Enter 按键前的强制落定：先应用再处理该键，内容不许滞留
    /// （否则后续光标移动会错位、Ctrl+C 清空会漏字）。
    pub(super) fn flush_forced(&mut self) -> FlushOutcome {
        if let Some((held, _)) = self.held.take() {
            return FlushOutcome::Typed(held);
        }
        if self.active {
            self.active = false;
            let out = std::mem::take(&mut self.buffer);
            if !out.is_empty() {
                return FlushOutcome::Paste(out);
            }
        }
        FlushOutcome::None
    }

    fn push_newline(&mut self, now: Instant) {
        self.buffer.push('\n');
        self.last_char_at = Some(now);
        self.suppress_until = Some(now + ENTER_SUPPRESS_WINDOW);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
    use super::*;
    use std::time::Duration;

    fn t0() -> Instant {
        Instant::now()
    }

    /// 主路径：冷字直通 → 快字符成组 → Enter 记换行 → 停顿后整串落定。
    #[test]
    fn burst_groups_fast_chars_and_newlines_into_one_paste() {
        let start = t0();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut burst = PasteBurst::default();
        assert_eq!(burst.on_char('a', at(0)), CharDecision::Typed('a'));
        assert_eq!(burst.on_char('b', at(1)), CharDecision::Held);
        assert_eq!(burst.on_char('c', at(2)), CharDecision::Buffered);
        assert_eq!(burst.on_enter(at(3)), EnterDecision::Buffered);
        assert_eq!(burst.on_char('d', at(4)), CharDecision::Buffered);
        // 未到期不落定。
        assert_eq!(burst.flush_if_due(at(5)), FlushOutcome::None);
        match burst.flush_if_due(at(1000)) {
            FlushOutcome::Paste(text) => assert_eq!(text, "bc\nd"),
            other => panic!("expected one paste, got {other:?}"),
        }
        assert_eq!(burst.on_enter(at(5000)), EnterDecision::Submit);
    }

    /// 压制窗：突发超时落定后 120ms 内的 Enter 仍本地换行，不提交。
    #[test]
    fn enter_in_suppress_window_inserts_newline() {
        let start = t0();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut burst = PasteBurst::default();
        assert_eq!(burst.on_char('a', at(0)), CharDecision::Typed('a'));
        assert_eq!(burst.on_char('b', at(1)), CharDecision::Held);
        assert_eq!(burst.on_char('c', at(2)), CharDecision::Buffered);
        // 成组后慢字符仍入串（落定只看停顿超时），超时后整串落定。
        assert_eq!(burst.on_char('d', at(100)), CharDecision::Buffered);
        match burst.flush_if_due(at(200)) {
            FlushOutcome::Paste(text) => assert_eq!(text, "bcd"),
            other => panic!("expected one paste, got {other:?}"),
        }
        assert_eq!(burst.on_enter(at(210)), EnterDecision::LocalNewline);
        assert_eq!(burst.on_enter(at(5000)), EnterDecision::Submit);
    }

    /// 关键失败路径：慢速打字永不进突发（冷字直接落屏），孤立快字符
    /// 超时后按打字吐出。
    #[test]
    fn slow_typing_never_buffers() {
        let start = t0();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut burst = PasteBurst::default();
        // 冷字直通：人手按键立即落屏，不暂扣。
        assert_eq!(burst.on_char('h', at(0)), CharDecision::Typed('h'));
        assert_eq!(burst.on_char('i', at(100)), CharDecision::Typed('i'));
        assert_eq!(burst.on_enter(at(200)), EnterDecision::Submit);
        // 孤立快字符超时后按打字吐出，不成串。
        assert_eq!(burst.on_char('x', at(300)), CharDecision::Typed('x'));
        assert_eq!(burst.on_char('y', at(301)), CharDecision::Held);
        assert_eq!(burst.flush_if_due(at(306)), FlushOutcome::None);
        assert_eq!(burst.flush_if_due(at(311)), FlushOutcome::Typed('y'));
    }
}

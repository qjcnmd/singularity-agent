//! 会话流滚动状态机：底部跟随与历史浏览两个状态，无中间态。
//!
//! 合同：新内容到达时，跟随态钉住最新输出；用户上滚即进入浏览态并统计
//! 底部新增行数；滚动回底（过冲手势）或显式跳转后重新跟随。提交新消息
//! 后进入 page-flip：新内容首行钉在视口顶，填满一屏后自动回底跟随；
//! 任何用户手势立即解除钉住。resize 只做位置钳制，不改变跟随语义。

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScrollState {
    /// 跟随态：视口钉在内容底部。
    follow: bool,
    /// 浏览态下视口顶部的可视行号。
    top_row: usize,
    /// 浏览态期间底部新增的可视行数（供「N 行新内容」提示）。
    new_below: usize,
    /// page-flip：提交时刻的流总行数——新内容首行从该行开始显示，
    /// 视口钉在该行直到新内容填满一屏（参照 Grok 的 reserve-pad）。
    /// 任何用户滚动手势立即解除。
    pin_at_total: Option<usize>,
}

impl Default for ScrollState {
    fn default() -> Self {
        Self {
            follow: true,
            top_row: 0,
            new_below: 0,
            pin_at_total: None,
        }
    }
}

impl ScrollState {
    pub fn is_following(&self) -> bool {
        self.follow
    }

    pub fn pending_below(&self) -> usize {
        self.new_below
    }

    pub fn top_row(&self) -> usize {
        self.top_row
    }

    /// 提交新消息后进入 page-flip：视口钉在 `total_rows`（新内容首行），
    /// 保持跟随语义（状态行不显示浏览提示）。
    pub fn pin_new_content_at(&mut self, total_rows: usize) {
        self.follow = true;
        self.new_below = 0;
        self.pin_at_total = Some(total_rows);
        self.top_row = total_rows;
    }

    /// 内容增长后的收敛：跟随态保持钉底；浏览态累计底部新增并钳制位置；
    /// page-flip 保持钉顶直到新内容填满一屏。零增长帧不改变任何状态——
    /// 钉住期尤其不能被位置钳制解除。
    pub fn on_content_grow(&mut self, grown_rows: usize, total_rows: usize, viewport: usize) {
        if grown_rows == 0 {
            if self.pin_at_total.is_some() {
                return;
            }
            self.clamp(total_rows, viewport);
            return;
        }
        if let Some(pin) = self.pin_at_total {
            self.top_row = pin;
            if total_rows.saturating_sub(pin) >= viewport {
                self.pin_at_total = None;
                self.top_row = bottom_top(total_rows, viewport);
            }
            return;
        }
        if self.follow {
            self.top_row = bottom_top(total_rows, viewport);
        } else {
            self.new_below = self.new_below.saturating_add(grown_rows);
            self.clamp(total_rows, viewport);
        }
    }

    /// 当前帧渲染应取的可视顶行：page-flip 返回钉点（新内容首行），跟随态
    /// 返回底部，浏览态返回 top_row。draw 一律经此处取值，不自行判断状态。
    pub fn visible_top(&self, total_rows: usize, viewport: usize) -> usize {
        if let Some(pin) = self.pin_at_total {
            return pin.min(total_rows);
        }
        if self.follow {
            bottom_top(total_rows, viewport)
        } else {
            self.top_row
        }
    }

    /// 上滚 n 行：进入浏览态并解除 page-flip；已在顶部则停留。
    pub fn scroll_up(&mut self, rows: usize, total_rows: usize, viewport: usize) {
        if rows == 0 {
            return;
        }
        self.pin_at_total = None;
        if self.follow {
            // 从底部脱离：以当前底为锚向上滚。
            self.follow = false;
            self.new_below = 0;
            self.top_row = bottom_top(total_rows, viewport).saturating_sub(rows);
        } else {
            self.top_row = self.top_row.saturating_sub(rows);
        }
        self.clamp(total_rows, viewport);
    }

    /// 下滚 n 行：触及底部时到位但不立即回归——只有下一次滚动（过冲手势）
    /// 才恢复跟随，防止快速下滚意外进入跟随（参照 Grok 的 overscroll 语义）。
    /// page-flip 期间下滚：解除钉住，从钉点位置继续滚动。
    pub fn scroll_down(&mut self, rows: usize, total_rows: usize, viewport: usize) {
        if rows == 0 {
            return;
        }
        if let Some(pin) = self.pin_at_total.take() {
            self.top_row = pin;
            self.follow = false;
        } else if self.follow {
            return;
        }
        let bottom = bottom_top(total_rows, viewport);
        if self.top_row >= bottom {
            // 已到底再滚：过冲 → 回归跟随。
            self.reattach(total_rows, viewport);
            return;
        }
        let candidate = self.top_row.saturating_add(rows);
        if candidate >= bottom {
            // 恰好落到底部：到位，不回归。
            self.top_row = bottom;
            self.new_below = 0;
        } else {
            self.top_row = candidate;
            self.new_below = self.new_below.saturating_sub(rows);
        }
    }

    /// 显式跳转到底部（快捷键 / 发送输入）。
    pub fn jump_to_bottom(&mut self, total_rows: usize, viewport: usize) {
        self.pin_at_total = None;
        self.follow = true;
        self.new_below = 0;
        self.top_row = bottom_top(total_rows, viewport);
    }

    /// 跳转到内容顶部并进入浏览态；底部新增计数保持（内容确实在下方）。
    pub fn jump_to_top(&mut self) {
        self.pin_at_total = None;
        self.follow = false;
        self.top_row = 0;
    }

    /// resize 后的位置钳制：不改变跟随语义；page-flip 在钉点被视口吞没时
    /// 解除并回底。
    pub fn clamp(&mut self, total_rows: usize, viewport: usize) {
        let max_top = bottom_top(total_rows, viewport);
        if let Some(pin) = self.pin_at_total {
            if pin <= max_top {
                self.top_row = pin;
            } else {
                self.pin_at_total = None;
                self.top_row = max_top;
            }
            return;
        }
        if self.follow {
            self.top_row = max_top;
        } else {
            self.top_row = self.top_row.min(max_top);
        }
    }

    fn reattach(&mut self, total_rows: usize, viewport: usize) {
        self.pin_at_total = None;
        self.follow = true;
        self.new_below = 0;
        self.top_row = bottom_top(total_rows, viewport);
    }
}

fn bottom_top(total_rows: usize, viewport: usize) -> usize {
    total_rows.saturating_sub(viewport)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 钉住期的首个零增长帧不得丢失钉点：提交时刻 total 即 pin，
    /// 钳制会立即把它判为「被视口吞没」而清钉。
    #[test]
    fn zero_growth_frame_keeps_the_pin() {
        let mut state = ScrollState::default();
        state.pin_new_content_at(10);
        state.on_content_grow(0, 10, 5);
        assert_eq!(
            state.visible_top(10, 5),
            10,
            "pin survives a zero-growth frame"
        );
        assert!(state.is_following(), "pin keeps follow semantics");
    }

    /// 钉住期 visible_top 返回钉行；内容长到填满一屏时回底并解除。
    #[test]
    fn visible_top_returns_the_pinned_row_until_full_screen() {
        let mut state = ScrollState::default();
        state.pin_new_content_at(10);
        state.on_content_grow(2, 12, 5);
        assert_eq!(state.visible_top(12, 5), 10);
        state.on_content_grow(3, 15, 5);
        assert_eq!(
            state.visible_top(15, 5),
            15 - 5,
            "total - pin >= viewport releases the pin back to bottom"
        );
    }

    /// 钉住期内上滚立即解除并进入浏览态（以当前底为锚向上滚）。
    #[test]
    fn scrolling_up_releases_the_pin() {
        let mut state = ScrollState::default();
        state.pin_new_content_at(10);
        state.on_content_grow(2, 12, 5);
        state.scroll_up(1, 12, 5);
        assert!(!state.is_following());
        assert_eq!(state.visible_top(12, 5), 6);
    }
}

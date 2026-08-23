//! 会话流滚动状态机：底部跟随与历史浏览两个状态，无中间态。
//!
//! 合同：新内容到达时，跟随态钉住最新输出；用户上滚即进入浏览态并统计
//! 底部新增行数；滚动回底或显式跳转后重新跟随。resize 只做位置钳制，
//! 不改变跟随语义。

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScrollState {
    /// 跟随态：视口钉在内容底部。
    follow: bool,
    /// 浏览态下视口顶部的可视行号。
    top_row: usize,
    /// 浏览态期间底部新增的可视行数（供「N 行新内容」提示）。
    new_below: usize,
}

impl Default for ScrollState {
    fn default() -> Self {
        Self {
            follow: true,
            top_row: 0,
            new_below: 0,
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

    /// 内容增长后的收敛：跟随态保持钉底；浏览态累计底部新增并钳制位置。
    pub fn on_content_grow(&mut self, grown_rows: usize, total_rows: usize, viewport: usize) {
        if grown_rows == 0 {
            self.clamp(total_rows, viewport);
            return;
        }
        if self.follow {
            self.top_row = bottom_top(total_rows, viewport);
        } else {
            self.new_below = self.new_below.saturating_add(grown_rows);
            self.clamp(total_rows, viewport);
        }
    }

    /// 上滚 n 行：进入浏览态；已在顶部则停留。
    pub fn scroll_up(&mut self, rows: usize, total_rows: usize, viewport: usize) {
        if rows == 0 {
            return;
        }
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

    /// 下滚 n 行：触及底部时恢复跟随并清零新增计数。
    pub fn scroll_down(&mut self, rows: usize, total_rows: usize, viewport: usize) {
        if rows == 0 || self.follow {
            return;
        }
        let bottom = bottom_top(total_rows, viewport);
        let candidate = self.top_row.saturating_add(rows);
        if candidate >= bottom {
            self.reattach(total_rows, viewport);
        } else {
            self.top_row = candidate;
            self.new_below = self.new_below.saturating_sub(rows);
        }
    }

    /// 显式跳转到底部（快捷键 / 发送输入）。
    pub fn jump_to_bottom(&mut self, total_rows: usize, viewport: usize) {
        self.follow = true;
        self.new_below = 0;
        self.top_row = bottom_top(total_rows, viewport);
    }

    /// 跳转到内容顶部并进入浏览态；底部新增计数保持（内容确实在下方）。
    pub fn jump_to_top(&mut self) {
        self.follow = false;
        self.top_row = 0;
    }

    /// resize 后的位置钳制：不改变跟随语义。
    pub fn clamp(&mut self, total_rows: usize, viewport: usize) {
        let max_top = bottom_top(total_rows, viewport);
        if self.follow {
            self.top_row = max_top;
        } else {
            self.top_row = self.top_row.min(max_top);
        }
    }

    fn reattach(&mut self, total_rows: usize, viewport: usize) {
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

    const VIEW: usize = 10;

    /// 从 `previous_total` 增长到 `new_total` 行。
    fn grow_to(state: &mut ScrollState, previous_total: usize, new_total: usize) {
        state.on_content_grow(new_total.saturating_sub(previous_total), new_total, VIEW);
    }

    #[test]
    fn following_state_pins_to_bottom_on_growth() {
        let mut state = ScrollState::default();
        assert!(state.is_following());
        grow_to(&mut state, 0, 100);
        assert_eq!(state.top_row(), 100 - VIEW);
        assert!(state.is_following());
        assert_eq!(state.pending_below(), 0);
    }

    #[test]
    fn scrolling_up_detaches_and_counts_new_content() {
        let mut state = ScrollState::default();
        grow_to(&mut state, 0, 100);
        state.scroll_up(5, 100, VIEW);
        assert!(!state.is_following());
        assert_eq!(state.top_row(), 90 - 5);
        // 浏览态下内容增长：位置不动，新增累计。
        grow_to(&mut state, 100, 110);
        assert_eq!(state.top_row(), 85);
        assert_eq!(state.pending_below(), 10);
    }

    #[test]
    fn scrolling_down_to_bottom_reattaches_and_clears_counter() {
        let mut state = ScrollState::default();
        grow_to(&mut state, 0, 100);
        state.scroll_up(30, 100, VIEW);
        assert!(!state.is_following());
        state.scroll_down(usize::MAX / 2, 100, VIEW);
        assert!(state.is_following());
        assert_eq!(state.pending_below(), 0);
        assert_eq!(state.top_row(), 90);
    }

    #[test]
    fn partial_scroll_down_decrements_pending_without_reattach() {
        let mut state = ScrollState::default();
        grow_to(&mut state, 0, 100);
        state.scroll_up(20, 100, VIEW); // top=70
        grow_to(&mut state, 100, 120); // pending=20
        state.scroll_down(8, 120, VIEW);
        assert!(!state.is_following());
        assert_eq!(state.pending_below(), 12);
    }

    #[test]
    fn jump_shortcuts_set_explicit_states() {
        let mut state = ScrollState::default();
        grow_to(&mut state, 0, 100);
        state.jump_to_top();
        assert!(!state.is_following());
        assert_eq!(state.top_row(), 0);
        state.jump_to_bottom(100, VIEW);
        assert!(state.is_following());
        assert_eq!(state.top_row(), 90);
    }

    #[test]
    fn resize_clamps_position_without_changing_semantics() {
        let mut state = ScrollState::default();
        grow_to(&mut state, 0, 100);
        state.scroll_up(50, 100, VIEW);
        // 视口变小、总行数变多：位置钳制到新的底部上限内。
        state.clamp(200, 60);
        assert!(!state.is_following());
        assert!(state.top_row() <= 140);
        // 跟随态 resize 保持钉底。
        state.jump_to_bottom(200, 60);
        state.clamp(150, 40);
        assert!(state.is_following());
        assert_eq!(state.top_row(), 110);
    }
}

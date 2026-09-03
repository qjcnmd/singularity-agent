//! 会话流滚动状态机：底部跟随与历史浏览两个状态，无中间态。
//!
//! 合同：新内容到达时，跟随态钉住最新输出；用户上滚即进入浏览态并统计
//! 底部新增行数；下滚触底即恢复跟随；显式跳转（End/发送输入）同样回到底。
//! resize 只做位置钳制，不改变跟随语义。

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

    /// 当前帧渲染应取的可视顶行：跟随态返回底部，浏览态返回 top_row。
    /// draw 一律经此处取值，不自行判断状态。
    pub fn visible_top(&self, total_rows: usize, viewport: usize) -> usize {
        if self.follow {
            bottom_top(total_rows, viewport)
        } else {
            self.top_row
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

    /// 下滚 n 行：触及底部即恢复跟随。
    pub fn scroll_down(&mut self, rows: usize, total_rows: usize, viewport: usize) {
        if rows == 0 {
            return;
        } else if self.follow {
            return;
        }
        let bottom = bottom_top(total_rows, viewport);
        let candidate = self.top_row.saturating_add(rows);
        if candidate >= bottom {
            // 触底：到位并恢复跟随。
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
    fn clamp(&mut self, total_rows: usize, viewport: usize) {
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
    #![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
    use super::ScrollState;

    /// 下滚触底即恢复跟随：从浏览态下滚恰好落底即回到跟随，不需过冲。
    #[test]
    fn scroll_down_reattaches_at_bottom() {
        let mut scroll = ScrollState::default();
        // 从底部上滚进入浏览态。
        scroll.scroll_up(5, 100, 10);
        assert!(!scroll.is_following());
        assert_eq!(scroll.top_row(), 85);
        // 下滚恰好落底：到位并恢复跟随。
        scroll.scroll_down(5, 100, 10);
        assert_eq!(scroll.top_row(), 90);
        assert!(scroll.is_following(), "触底即恢复跟随");
    }

    /// 提交开新回合即回底跟随：增长后仍钉底，可视顶行即底部。
    #[test]
    fn submit_returns_to_bottom_follow() {
        let mut scroll = ScrollState::default();
        scroll.jump_to_bottom(50, 10);
        assert!(scroll.is_following());
        assert_eq!(scroll.visible_top(50, 10), 40);
        // 内容增长：跟随态保持钉底。
        scroll.on_content_grow(5, 55, 10);
        assert!(scroll.is_following());
        assert_eq!(scroll.visible_top(55, 10), 45);
    }
}

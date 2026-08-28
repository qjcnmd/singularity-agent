//! TUI 鼠标路由：滚轮归一化与点击命中（帧缓存点击矩形表）。
//!
//! 渲染帧在 [`TuiApp::draw`] 中登记 `(Rect, ClickTarget)` 对；鼠标事件对
//! 缓存做矩形包含测试，取代原先对状态行文本的反查。

use std::time::Instant;

use ratatui::layout::Rect;

use super::app::TuiApp;

/// 鼠标滚轮一格对应的三行滚动。
pub(super) const WHEEL_ROWS: usize = 3;

/// 可点击目标：运行中状态行末段的 [stop] 中断按钮，或编辑器内容区。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ClickTarget {
    Stop,
    Editor,
}

/// 滚轮归一化：按事件间隔区分滚轮/触控板并区间加速（参照 Grok 的
/// `mouse.rs` 简化版——<8ms ×2.5、<20ms ×1.6，其余 ×1.0），小数部分
/// 累计到下一事件，单次事件有上下限防失控。
#[derive(Default)]
pub(super) struct WheelNormalizer {
    last: Option<Instant>,
    pending: f64,
}

impl WheelNormalizer {
    fn rows_for(&mut self, now: Instant) -> usize {
        let multiplier = match self.last {
            Some(last) => {
                let gap_ms = now.duration_since(last).as_millis();
                if gap_ms <= 8 {
                    2.5
                } else if gap_ms <= 20 {
                    1.6
                } else {
                    1.0
                }
            }
            None => 1.0,
        };
        self.last = Some(now);
        self.pending += WHEEL_ROWS as f64 * multiplier;
        let rows = self.pending.floor() as usize;
        self.pending -= rows as f64;
        rows.clamp(1, 8)
    }
}

/// 矩形包含测试（ratatui `Rect` 的半开区间语义：含左/上，不含右/下）。
fn rect_contains(rect: Rect, column: u16, row: u16) -> bool {
    column >= rect.x
        && column < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

impl TuiApp {
    /// 鼠标滚轮：指针在输入框内时滚动编辑器视口（光标一动即回跟随），
    /// 其余滚动会话流；事件间隔触发滚轮/触控板加速（参照 Grok 的滚轮路由）。
    pub fn handle_wheel(&mut self, up: bool, column: u16, row: u16) {
        let rows = self.wheel.rows_for(Instant::now());
        let editor_rect = self
            .frame
            .click_targets
            .iter()
            .find(|(_, target)| *target == ClickTarget::Editor)
            .map(|(rect, _)| *rect);
        if let Some(rect) = editor_rect
            && rect_contains(rect, column, row)
        {
            self.editor
                .scroll_by(if up { -(rows as i32) } else { rows as i32 });
            return;
        }
        let (total, viewport) = self.flow_metrics();
        if up {
            self.scroll.scroll_up(rows, total, viewport);
        } else {
            self.scroll.scroll_down(rows, total, viewport);
        }
    }

    /// 点击路由：遍历帧缓存点击矩形表，命中则按目标执行（[stop]=中断、
    /// 编辑器=光标定位）。运行中点击 [stop] 与 Esc 同一中断路径。
    pub fn handle_click(&mut self, column: u16, row: u16) {
        let hit = self
            .frame
            .click_targets
            .iter()
            .find(|(rect, _)| rect_contains(*rect, column, row))
            .copied();
        let Some((rect, target)) = hit else {
            return;
        };
        match target {
            // 运行中点击 [stop] 与 Esc 同一中断路径。
            ClickTarget::Stop => self.request_interrupt(),
            ClickTarget::Editor => {
                let visual_row = self
                    .frame
                    .last_editor_scroll_top
                    .saturating_add((row - rect.y) as usize);
                let visual_col = (column - rect.x) as usize;
                self.editor
                    .set_cursor_visual(visual_row, visual_col, rect.width);
            }
        }
    }
}

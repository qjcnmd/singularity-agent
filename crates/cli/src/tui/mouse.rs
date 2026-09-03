//! TUI 鼠标路由：固定步长滚轮与帧缓存矩形命中。
//!
//! 渲染帧在 [`TuiApp::draw`] 中记录停止按钮和编辑器矩形，鼠标事件使用
//! ratatui 的半开区间包含语义命中对应区域。

use ratatui::layout::Position;

use super::app::TuiApp;

/// 鼠标滚轮一格对应的三行滚动（固定步长）。
pub(super) const WHEEL_ROWS: usize = 3;

impl TuiApp {
    /// 鼠标滚轮：指针在输入框内时滚动编辑器视口（光标一动即回跟随），
    /// 其余滚动会话流。
    pub fn handle_wheel(&mut self, up: bool, column: u16, row: u16) {
        let rows = WHEEL_ROWS;
        if let Some(rect) = self.frame.editor_rect
            && rect.contains(Position::new(column, row))
        {
            // 锚定上一帧实际视口顶行：跟随态滚动不跳到内容头。
            let base = self.frame.last_editor_scroll_top;
            self.editor
                .scroll_by(if up { -(rows as i32) } else { rows as i32 }, base);
            return;
        }
        let (total, viewport) = self.flow_metrics();
        if up {
            self.scroll.scroll_up(rows, total, viewport);
        } else {
            self.scroll.scroll_down(rows, total, viewport);
        }
    }

    /// 点击路由：[stop] 优先命中并中断，编辑器命中时定位光标。
    pub fn handle_click(&mut self, column: u16, row: u16) {
        let position = Position::new(column, row);
        if self
            .frame
            .stop_rect
            .is_some_and(|rect| rect.contains(position))
        {
            self.request_interrupt();
            return;
        }
        let Some(rect) = self.frame.editor_rect else {
            return;
        };
        if !rect.contains(position) {
            return;
        }
        let visual_row = self
            .frame
            .last_editor_scroll_top
            .saturating_add((row - rect.y) as usize);
        let visual_col = (column - rect.x) as usize;
        self.editor
            .set_cursor_visual(visual_row, visual_col, rect.width);
    }
}

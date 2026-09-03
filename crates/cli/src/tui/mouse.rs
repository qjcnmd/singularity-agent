//! TUI 鼠标路由：固定步长滚轮与帧缓存矩形命中。
//!
//! 渲染帧在 [`TuiApp::draw`] 中记录停止按钮和编辑器矩形，鼠标事件使用
//! ratatui 的半开区间包含语义命中对应区域。

use ratatui::layout::Position;

use super::app::TuiApp;
use super::flow_select::{FlowPoint, FlowSelection, selected_text};
use super::transcript::NoteStyle;

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

    /// 按下路由：[stop] 优先命中并中断；编辑器命中时定位光标并起选
    /// （松开无拖拽即普通点击，选区自动散掉）；会话流命中时起流选区。
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
        if let Some((visual_row, visual_col, width)) = self.editor_point(column, row) {
            self.flow_selection = None;
            self.frame.flow_plain_rows.clear();
            self.editor.set_cursor_visual(visual_row, visual_col, width);
            self.editor.begin_selection();
            return;
        }
        let Some(rect) = self.frame.flow_rect.filter(|rect| rect.contains(position)) else {
            return;
        };
        self.editor.cancel_selection();
        let point = FlowPoint {
            row: (row - rect.y) as usize,
            col: (column - rect.x) as usize,
        };
        self.flow_selection = Some(FlowSelection {
            anchor: point,
            focus: point,
        });
    }

    /// 拖拽扩展选中：编辑器只动光标（锚点已在按下时记下），会话流动焦点。
    pub fn handle_drag(&mut self, column: u16, row: u16) {
        if self.flow_selection.is_some() {
            let Some(rect) = self.frame.flow_rect else {
                return;
            };
            let (row, column) = (
                row.clamp(rect.y, rect.bottom().saturating_sub(1)),
                column.clamp(rect.x, rect.right().saturating_sub(1)),
            );
            if let Some(selection) = self.flow_selection.as_mut() {
                selection.focus = FlowPoint {
                    row: (row - rect.y) as usize,
                    col: (column - rect.x) as usize,
                };
            }
            return;
        }
        let Some((visual_row, visual_col, width)) = self.editor_point(column, row) else {
            return;
        };
        self.editor.set_cursor_visual(visual_row, visual_col, width);
    }

    /// 松开：会话流有拖选即复制并收起；编辑器纯点击散选，有宽度保留高亮。
    pub fn handle_release(&mut self) {
        if self.flow_selection.is_some() {
            self.copy_flow_selection();
            return;
        }
        self.editor.end_selection();
    }

    /// 复制选中的可见行到系统剪贴板并留一行灰字回执（Pi 的 copyOnSelect
    /// 同形：不占用按键）。零宽、空文本静默；剪贴板不可用留警告。
    fn copy_flow_selection(&mut self) {
        let Some(selection) = self.flow_selection.take() else {
            return;
        };
        if selection.is_empty() {
            self.frame.flow_plain_rows.clear();
            return;
        }
        let text = selected_text(&self.frame.flow_plain_rows, selection);
        self.frame.flow_plain_rows.clear();
        if text.trim().is_empty() {
            return;
        }
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        let line_count = text.lines().count();
        // 建句柄失败与写内容失败同一回执形状；前者不再静默。
        let Some(clipboard) = self.clipboard.as_mut() else {
            self.transcript
                .push_note("clipboard unavailable", NoteStyle::Warning);
            return;
        };
        match clipboard.set_text(text) {
            Ok(()) => self.transcript.push_note(
                format!("copied {line_count} lines to clipboard"),
                NoteStyle::Dim,
            ),
            Err(error) => self.transcript.push_note(
                format!("clipboard unavailable: {error}"),
                NoteStyle::Warning,
            ),
        }
    }

    /// 终端坐标换算编辑器可视坐标（含滚动偏移）；命中编辑器矩形才返回。
    fn editor_point(&self, column: u16, row: u16) -> Option<(usize, usize, u16)> {
        let rect = self.frame.editor_rect?;
        let position = Position::new(column, row);
        if !rect.contains(position) {
            return None;
        }
        Some((
            self.frame
                .last_editor_scroll_top
                .saturating_add((row - rect.y) as usize),
            (column - rect.x) as usize,
            rect.width,
        ))
    }
}

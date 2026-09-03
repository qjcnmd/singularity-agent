//! 会话流鼠标选中：屏幕坐标快照与选中纯文本提取。
//!
//! 选区以会话流视口内的 (可视行, 显示列) 记录，列按终端显示格计（宽字符
//! 占 2 格），与鼠标事件坐标同一坐标系。视口顶行或流宽在帧间变化时选区
//! 失效（快照语义，见 `TuiApp::draw`），因此不需要文档坐标换算与自动滚屏。
//! 复制在鼠标松开时发生，与 Pi 的 `copyOnSelect` 同形。

use unicode_width::UnicodeWidthChar;

/// 选区端点：会话流视口内的可视行与显示列（均自 0 起）。
/// 序按行优先、同行按列（字段顺序即比较顺序）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct FlowPoint {
    pub(super) row: usize,
    pub(super) col: usize,
}

/// 一次拖选：按下处为锚点，拖到处为焦点，两者可任意先后。
#[derive(Debug, Clone, Copy)]
pub(super) struct FlowSelection {
    pub(super) anchor: FlowPoint,
    pub(super) focus: FlowPoint,
}

impl FlowSelection {
    /// 规范化为 (起点, 终点)：行优先，同行按列。
    pub(super) fn normalized(self) -> (FlowPoint, FlowPoint) {
        if self.anchor <= self.focus {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        }
    }

    /// 零宽选区（纯点击）不产生复制。
    pub(super) fn is_empty(self) -> bool {
        let (start, end) = self.normalized();
        start == end
    }

    /// 该行被选中的显示列区间 `[from, to)`；行不在选区内返回 `None`。
    /// 整行选中的中间行以 `usize::MAX` 收尾，由调用方按行宽钳制。
    pub(super) fn cols_on(self, row: usize) -> Option<(usize, usize)> {
        let (start, end) = self.normalized();
        if row < start.row || row > end.row {
            return None;
        }
        let from = if row == start.row { start.col } else { 0 };
        let to = if row == end.row { end.col } else { usize::MAX };
        if from < to { Some((from, to)) } else { None }
    }
}

/// 按显示格切出 `[from, to)` 的文本：越界钳制，不在宽字符中间断开。
fn slice_cells(text: &str, from: usize, to: usize) -> String {
    let mut cells = 0usize;
    let mut out = String::new();
    for ch in text.chars() {
        if cells >= to {
            break;
        }
        if cells >= from {
            out.push(ch);
        }
        cells += ch.width().unwrap_or(0);
    }
    out
}

/// 从视口纯文本行提取选中内容：行间以 `\n` 连接，末尾空行裁掉。
/// 行文本是渲染后的可见内容，折行处因此带进换行（与 Pi 一致）。
pub(super) fn selected_text(rows: &[String], selection: FlowSelection) -> String {
    let (start, end) = selection.normalized();
    let mut lines = Vec::new();
    for row in start.row..=end.row {
        let Some(text) = rows.get(row) else {
            continue;
        };
        let Some((from, to)) = selection.cols_on(row) else {
            continue;
        };
        lines.push(slice_cells(text, from, to));
    }
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
    use super::*;

    fn point(row: usize, col: usize) -> FlowPoint {
        FlowPoint { row, col }
    }

    /// 反向拖选与跨行区间：规范化后行/列语义一致。
    #[test]
    fn selection_normalizes_and_slices_rows() {
        let rows = vec![
            "abcdefgh".to_string(),
            "second line".to_string(),
            "xyz".to_string(),
        ];
        // 从 (2,3) 拖到 (0,1)：反向选区，末行取到列 3。
        let upward = FlowSelection {
            anchor: point(2, 3),
            focus: point(0, 1),
        };
        assert_eq!(selected_text(&rows, upward), "bcdefgh\nsecond line\nxyz");
        // 同区间正向拖选：末行取到列 2。
        let downward = FlowSelection {
            anchor: point(0, 1),
            focus: point(2, 2),
        };
        assert_eq!(selected_text(&rows, downward), "bcdefgh\nsecond line\nxy");
        assert_eq!(downward.cols_on(1), Some((0, usize::MAX)));
        assert_eq!(downward.cols_on(3), None);
    }

    /// 宽字符按显示格切分：不在双格字符中间断开，零宽尾行裁掉。
    #[test]
    fn wide_chars_are_not_split_and_trailing_blanks_drop() {
        let rows = vec!["你好世界".to_string(), "  ".to_string(), "".to_string()];
        let selection = FlowSelection {
            anchor: point(0, 1),
            focus: point(0, 5),
        };
        // 列 1 落在「你」的右半格内：该字不进选区，从左起第 2 格的「好」起取。
        assert_eq!(selected_text(&rows, selection), "好世");
        let selection = FlowSelection {
            anchor: point(0, 0),
            focus: point(2, 1),
        };
        assert_eq!(selected_text(&rows, selection), "你好世界");
    }
}

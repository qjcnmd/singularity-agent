//! 底部多行输入编辑器：光标、编辑键、按内容折行的高度增长。
//!
//! 编辑器始终持有键盘焦点；`col` 以字符（char）为单位，可视列由渲染层
//! 按 unicode 宽度换算。高度为内容折行数钳制在 `[1, max_rows]`。鼠标滚轮
//! 可把视口暂时移离光标（`scroll_override`），任何编辑/移动光标操作立即
//! 清除覆盖、回到跟随光标。

use unicode_width::UnicodeWidthStr;

use super::wrapped_lines;

#[derive(Debug, Default, Clone)]
pub(crate) struct Editor {
    lines: Vec<String>,
    row: usize,
    col: usize,
    /// 鼠标滚轮造成的视口行偏移；`None` 表示跟随光标。
    scroll_override: Option<usize>,
}

impl Editor {
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            row: 0,
            col: 0,
            scroll_override: None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// 当前光标行（0 起）。
    pub fn row(&self) -> usize {
        self.row
    }

    /// 当前光标列（0 起，字符单位）。
    pub fn col(&self) -> usize {
        self.col
    }

    /// 整体替换编辑器内容，光标置于末尾。
    pub fn set_text(&mut self, text: &str) {
        self.clear();
        if !text.is_empty() {
            self.insert_str(text);
        }
    }

    /// 取走全部输入并复位。
    pub fn take(&mut self) -> String {
        let text = self.text();
        self.clear();
        text
    }

    pub fn clear(&mut self) {
        self.lines = vec![String::new()];
        self.row = 0;
        self.col = 0;
        self.scroll_override = None;
    }

    /// 鼠标滚轮滚动视口：`delta` 为行偏移（负=向上），`base` 为上一帧实际
    /// 视口顶行（跟随态与覆盖态统一以此为锚，滚动不跳变）。滚动后任何光标
    /// 移动都会清除覆盖；顶到 0 用 `Some(0)` 钉住（与 `None`=跟随区分）。
    pub fn scroll_by(&mut self, delta: i32, base: usize) {
        let next = base as i32 + delta;
        self.scroll_override = Some(next.max(0) as usize);
    }

    /// 实际视口顶行：覆盖偏移优先，否则跟随光标所在可视行。
    pub fn effective_scroll_top(&self, cursor_visual_row: usize, inner_height: usize) -> usize {
        match self.scroll_override {
            Some(offset) => offset,
            None => cursor_visual_row.saturating_sub(inner_height.saturating_sub(1)),
        }
    }

    pub fn insert_char(&mut self, ch: char) {
        self.scroll_override = None;
        let line = &mut self.lines[self.row];
        let byte = char_to_byte(line, self.col);
        line.insert(byte, ch);
        self.col += 1;
    }

    pub fn insert_newline(&mut self) {
        self.scroll_override = None;
        let line = &mut self.lines[self.row];
        let byte = char_to_byte(line, self.col);
        let tail = line.split_off(byte);
        self.lines.insert(self.row + 1, tail);
        self.row += 1;
        self.col = 0;
    }

    /// 在光标处插入整段文本；内部 `\n` 拆成多行。
    pub fn insert_str(&mut self, text: &str) {
        self.scroll_override = None;
        let mut parts = text.split('\n').peekable();
        // 首段并入当前行光标处。
        if let Some(first) = parts.next()
            && !first.is_empty()
        {
            let line = &mut self.lines[self.row];
            let byte = char_to_byte(line, self.col);
            line.insert_str(byte, first);
            self.col += first.chars().count();
        }
        // 后续每段前先拆行，再写入新行。
        for part in parts {
            self.insert_newline();
            if !part.is_empty() {
                let line = &mut self.lines[self.row];
                line.insert_str(0, part);
                self.col = part.chars().count();
            }
        }
    }

    pub fn backspace(&mut self) {
        self.scroll_override = None;
        if self.col > 0 {
            let line = &mut self.lines[self.row];
            let byte = char_to_byte(line, self.col - 1);
            line.remove(byte);
            self.col -= 1;
        } else if self.row > 0 {
            let tail = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.lines[self.row].chars().count();
            self.lines[self.row].push_str(&tail);
        }
    }

    pub fn delete(&mut self) {
        self.scroll_override = None;
        let line_len = self.lines[self.row].chars().count();
        if self.col < line_len {
            let line = &mut self.lines[self.row];
            let byte = char_to_byte(line, self.col);
            line.remove(byte);
        } else if self.row + 1 < self.lines.len() {
            let tail = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&tail);
        }
    }

    pub fn move_left(&mut self) {
        self.scroll_override = None;
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.lines[self.row].chars().count();
        }
    }

    pub fn move_right(&mut self) {
        self.scroll_override = None;
        let line_len = self.lines[self.row].chars().count();
        if self.col < line_len {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    pub fn move_up(&mut self) {
        self.scroll_override = None;
        if self.row == 0 {
            return;
        }
        self.row -= 1;
        self.col = self.col.min(self.lines[self.row].chars().count());
    }

    pub fn move_down(&mut self) {
        self.scroll_override = None;
        if self.row + 1 >= self.lines.len() {
            return;
        }
        self.row += 1;
        self.col = self.col.min(self.lines[self.row].chars().count());
    }

    pub fn move_home(&mut self) {
        self.scroll_override = None;
        self.col = 0;
    }

    pub fn move_end(&mut self) {
        self.scroll_override = None;
        self.col = self.lines[self.row].chars().count();
    }

    /// 内容按宽度逐逻辑行折行后的全部可视片段；高度计算与渲染共用这一
    /// 单次折行的产物。
    pub fn wrapped_pieces(&self, width: u16) -> Vec<String> {
        let width = width.max(1) as usize;
        self.lines
            .iter()
            .flat_map(|line| wrapped_lines(line, width))
            .collect()
    }

    /// 光标在折行后的可视坐标：返回 (可视行, 可视列)，供终端光标定位。
    pub fn cursor_visual(&self, width: u16) -> (usize, usize) {
        let width = width.max(1) as usize;
        let mut visual_row = 0usize;
        for (index, line) in self.lines.iter().enumerate() {
            let offsets = super::wrap_offsets(line, width);
            if index == self.row {
                let target_char = self.col;
                let mut consumed = 0usize;
                for (row_index, &char_start) in offsets.iter().enumerate() {
                    let row_chars = offsets
                        .get(row_index + 1)
                        .copied()
                        .unwrap_or(line.chars().count())
                        - char_start;
                    if target_char < consumed + row_chars || row_index + 1 == offsets.len() {
                        let within = target_char.saturating_sub(consumed);
                        let prefix: String = line.chars().skip(char_start).take(within).collect();
                        return (
                            visual_row + row_index,
                            UnicodeWidthStr::width(prefix.as_str()),
                        );
                    }
                    consumed += row_chars;
                }
                return (visual_row, 0);
            }
            visual_row += offsets.len();
        }
        (visual_row, 0)
    }

    /// 把折行后的可视坐标映射回字符光标；列落在宽字符中间时定位到该字符前。
    /// 点击定位即光标移动：清除滚轮滚动覆盖。
    pub fn set_cursor_visual(&mut self, target_row: usize, target_col: usize, width: u16) {
        self.scroll_override = None;
        let width = width.max(1) as usize;
        let mut visual_row = 0usize;
        for (logical_row, line) in self.lines.iter().enumerate() {
            for char_start in super::wrap_offsets(line, width) {
                if visual_row == target_row {
                    let mut used = 0usize;
                    let mut chars = 0usize;
                    for ch in line.chars().skip(char_start) {
                        let ch_width = super::char_display_width(ch);
                        if used + ch_width > target_col || used + ch_width > width {
                            break;
                        }
                        used += ch_width;
                        chars += 1;
                    }
                    self.row = logical_row;
                    self.col = char_start + chars;
                    return;
                }
                visual_row += 1;
            }
        }
        self.row = self.lines.len().saturating_sub(1);
        self.col = self.lines[self.row].chars().count();
    }
}

fn char_to_byte(line: &str, char_index: usize) -> usize {
    line.char_indices()
        .nth(char_index)
        .map(|(byte, _)| byte)
        .unwrap_or(line.len())
}

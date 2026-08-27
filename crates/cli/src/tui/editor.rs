//! 底部多行输入编辑器：光标、编辑键、按内容折行的高度增长。
//!
//! 编辑器始终持有键盘焦点；`col` 以字符（char）为单位，可视列由渲染层
//! 按 unicode 宽度换算。高度为内容折行数钳制在 `[1, max_rows]`。鼠标滚轮
//! 可把视口暂时移离光标（`scroll_override`），任何编辑/移动光标操作立即
//! 清除覆盖、回到跟随光标（参照 Grok textarea 的 scroll_override 语义）。

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

    /// 逐行只读访问（渲染用）。
    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.lines.iter().map(String::as_str)
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

    /// 鼠标滚轮滚动视口：`delta` 为行偏移（负=向上）。滚动后任何光标
    /// 移动都会清除覆盖；顶到 0 用 `Some(0)` 钉住（与 `None`=跟随区分）。
    pub fn scroll_by(&mut self, delta: i32) {
        let base = self.scroll_override.unwrap_or(0) as i32;
        let next = base + delta;
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

    /// 光标的字符位置（行、列）。
    #[cfg(test)]
    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    /// 内容在给定宽度下的折行总行数；用于编辑器高度计算。
    pub fn wrapped_height(&self, width: u16) -> usize {
        let width = width.max(1) as usize;
        self.lines
            .iter()
            .map(|line| wrapped_lines(line, width).len())
            .sum::<usize>()
            .max(1)
    }

    /// 编辑器显示高度：内容折行数钳制到 `max_rows`。
    pub fn display_height(&self, width: u16, max_rows: u16) -> u16 {
        self.wrapped_height(width).min(max_rows.max(1) as usize) as u16
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
                        let prefix: String =
                            line.chars().skip(char_start).take(within).collect();
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
                        let ch_width = UnicodeWidthStr::width(ch.to_string().as_str());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_newline_and_backspace_merge_across_lines() {
        let mut editor = Editor::new();
        for ch in "abc".chars() {
            editor.insert_char(ch);
        }
        editor.insert_newline();
        for ch in "de".chars() {
            editor.insert_char(ch);
        }
        assert_eq!(editor.text(), "abc\nde");
        editor.backspace(); // 删除 e
        editor.backspace(); // 删除 d
        editor.backspace(); // 合并回上一行
        assert_eq!(editor.text(), "abc");
        editor.backspace();
        editor.backspace();
        editor.backspace();
        editor.backspace(); // 空行上继续退格是 no-op
        assert!(editor.is_empty());
    }

    #[test]
    fn cursor_moves_across_rows_and_clamps_columns() {
        let mut editor = Editor::new();
        for ch in "abcdef".chars() {
            editor.insert_char(ch);
        }
        editor.move_home();
        assert_eq!(editor.cursor(), (0, 0));
        editor.move_end();
        assert_eq!(editor.cursor(), (0, 6));
        editor.move_left();
        editor.insert_newline(); // 在 e|f 处断行 → "abcde" / "f"
        assert_eq!(editor.text(), "abcde\nf");
        assert_eq!(editor.cursor(), (1, 0));
        editor.move_up();
        // 光标列保持 0：按当前列落到目标行。
        assert_eq!(editor.cursor(), (0, 0));
        editor.move_end();
        editor.move_down();
        // 下移后列号钳制到目标行长度（1）。
        assert_eq!(editor.cursor(), (1, 1));
        editor.move_down(); // 底部下移是 no-op
        assert_eq!(editor.cursor(), (1, 1));
    }

    #[test]
    fn take_resets_state() {
        let mut editor = Editor::new();
        editor.insert_char('x');
        editor.insert_newline();
        editor.insert_char('y');
        assert_eq!(editor.take(), "x\ny");
        assert!(editor.is_empty());
        assert_eq!(editor.cursor(), (0, 0));
    }

    #[test]
    fn wheel_override_scrolls_viewport_until_any_cursor_move() {
        let mut editor = Editor::new();
        for ch in "line one\nline two\nline three".chars() {
            if ch == '\n' {
                editor.insert_newline();
            } else {
                editor.insert_char(ch);
            }
        }
        // 光标在第 3 行（可视行 3），高度 2 → 跟随顶行 = 2。
        assert_eq!(editor.effective_scroll_top(3, 2), 2);
        // 滚轮向上偏移 2 行：视口顶 = 0 并钉住（与跟随区分）。
        editor.scroll_by(-2);
        assert_eq!(editor.effective_scroll_top(3, 2), 0);
        // 继续向上滚：不越界为负。
        editor.scroll_by(-5);
        assert_eq!(editor.effective_scroll_top(3, 2), 0);
        // 向下滚回：覆盖偏移生效。
        editor.scroll_by(2);
        assert_eq!(editor.effective_scroll_top(3, 2), 2);
        // 任何光标移动清除覆盖：回到跟随光标。
        editor.move_up();
        assert_eq!(editor.effective_scroll_top(2, 2), 1);
        // 点击定位也清除。
        editor.scroll_by(-3);
        editor.set_cursor_visual(0, 0, 80);
        assert_eq!(editor.effective_scroll_top(0, 2), 0);
        // 输入与清空同样清除。
        editor.scroll_by(-3);
        editor.insert_char('x');
        assert_eq!(editor.effective_scroll_top(0, 2), 0);
    }
}

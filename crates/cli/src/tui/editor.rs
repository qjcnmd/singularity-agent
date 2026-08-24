//! 底部多行输入编辑器：光标、编辑键、按内容折行的高度增长。
//!
//! 编辑器始终持有键盘焦点；`col` 以字符（char）为单位，可视列由渲染层
//! 按 unicode 宽度换算。高度为内容折行数钳制在 `[1, max_rows]`。

use unicode_width::UnicodeWidthStr;

#[derive(Debug, Default, Clone)]
pub(crate) struct Editor {
    lines: Vec<String>,
    row: usize,
    col: usize,
}

impl Editor {
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            row: 0,
            col: 0,
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
    }

    pub fn insert_char(&mut self, ch: char) {
        let line = &mut self.lines[self.row];
        let byte = char_to_byte(line, self.col);
        line.insert(byte, ch);
        self.col += 1;
    }

    pub fn insert_newline(&mut self) {
        let line = &mut self.lines[self.row];
        let byte = char_to_byte(line, self.col);
        let tail = line.split_off(byte);
        self.lines.insert(self.row + 1, tail);
        self.row += 1;
        self.col = 0;
    }

    pub fn backspace(&mut self) {
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
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.lines[self.row].chars().count();
        }
    }

    pub fn move_right(&mut self) {
        let line_len = self.lines[self.row].chars().count();
        if self.col < line_len {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    pub fn move_up(&mut self) {
        if self.row == 0 {
            return;
        }
        self.row -= 1;
        self.col = self.col.min(self.lines[self.row].chars().count());
    }

    pub fn move_down(&mut self) {
        if self.row + 1 >= self.lines.len() {
            return;
        }
        self.row += 1;
        self.col = self.col.min(self.lines[self.row].chars().count());
    }

    pub fn move_home(&mut self) {
        self.col = 0;
    }

    pub fn move_end(&mut self) {
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
            .map(|line| {
                let mut rows = 1usize;
                let mut current = 0usize;
                for ch in line.chars() {
                    let w = UnicodeWidthStr::width(ch.to_string().as_str());
                    if current + w > width && current > 0 {
                        rows += 1;
                        current = 0;
                    }
                    current += w;
                }
                rows
            })
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
            let rows = wrap_with_positions(line, width);
            if index == self.row {
                let target_char = self.col;
                // 找到目标字符所在的折行与列。
                let mut consumed = 0usize;
                for (row_index, (_, char_start)) in rows.iter().enumerate() {
                    let row_chars = chars_in_row(line, *char_start, width);
                    if target_char < consumed + row_chars || row_index + 1 == rows.len() {
                        let within = target_char.saturating_sub(consumed);
                        let prefix: String = line.chars().skip(*char_start).take(within).collect();
                        return (
                            visual_row + row_index,
                            UnicodeWidthStr::width(prefix.as_str()),
                        );
                    }
                    consumed += row_chars;
                }
                return (visual_row, 0);
            }
            visual_row += rows.len();
        }
        (visual_row, 0)
    }

    /// 把折行后的可视坐标映射回字符光标；列落在宽字符中间时定位到该字符前。
    pub fn set_cursor_visual(&mut self, target_row: usize, target_col: usize, width: u16) {
        let width = width.max(1) as usize;
        let mut visual_row = 0usize;
        for (logical_row, line) in self.lines.iter().enumerate() {
            for (_, char_start) in wrap_with_positions(line, width) {
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

/// 折一行并记录每折行的起始字符偏移。
fn wrap_with_positions(line: &str, width: usize) -> Vec<(String, usize)> {
    let mut rows: Vec<(String, usize)> = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    let mut start_char = 0usize;
    let mut consumed = 0usize;
    for (index, ch) in line.chars().enumerate() {
        let w = UnicodeWidthStr::width(ch.to_string().as_str());
        if current_width + w > width && !current.is_empty() {
            rows.push((std::mem::take(&mut current), start_char));
            start_char = index;
            current_width = 0;
            consumed = index;
        }
        current.push(ch);
        current_width += w;
    }
    rows.push((current, start_char));
    let _ = consumed;
    rows
}

/// 从 `start_char` 起最多填满一行的字符数。
fn chars_in_row(line: &str, start_char: usize, width: usize) -> usize {
    let mut count = 0usize;
    let mut used = 0usize;
    for ch in line.chars().skip(start_char) {
        let w = UnicodeWidthStr::width(ch.to_string().as_str());
        if used + w > width {
            break;
        }
        used += w;
        count += 1;
    }
    count
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
    fn delete_merges_following_line_at_row_end() {
        let mut editor = Editor::new();
        for ch in "ab".chars() {
            editor.insert_char(ch);
        }
        editor.insert_newline();
        editor.move_home();
        editor.delete(); // 行首 delete 是 no-op（右侧无内容合并语义按列）
        assert_eq!(editor.text(), "ab\n");
        editor.move_up();
        editor.move_end();
        editor.delete(); // 行尾删除合并下一行
        assert_eq!(editor.text(), "ab");
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
    fn wrapped_height_counts_wide_chars_and_grows_with_content() {
        let mut editor = Editor::new();
        assert_eq!(editor.wrapped_height(10), 1, "empty input stays one row");
        for _ in 0..12 {
            editor.insert_char('中'); // 每个宽 2
        }
        // 宽度 10 → 每行 5 个字 → 3 行。
        assert_eq!(editor.wrapped_height(10), 3);
        assert_eq!(editor.display_height(10, 2), 2, "clamped by max rows");
        assert_eq!(editor.display_height(10, 8), 3);
    }

    #[test]
    fn cursor_visual_maps_ascii_and_cjk_positions() {
        let mut editor = Editor::new();
        for ch in "中文a".chars() {
            editor.insert_char(ch);
        }
        // 宽度 20：光标在第 0 行，可视列 = 2+2+1 = 5。
        assert_eq!(editor.cursor_visual(20), (0, 5));
        editor.move_home();
        assert_eq!(editor.cursor_visual(20), (0, 0));
    }

    #[test]
    fn visual_click_positions_cursor_across_wrapped_and_wide_text() {
        let mut editor = Editor::new();
        for ch in "ab中文cd".chars() {
            editor.insert_char(ch);
        }
        editor.set_cursor_visual(1, 2, 5);
        assert_eq!(editor.cursor(), (0, 4));
        editor.set_cursor_visual(0, 1, 5);
        assert_eq!(editor.cursor(), (0, 1));
    }
}

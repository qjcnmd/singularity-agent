//! 底部多行输入编辑器：光标、编辑键、按内容折行的高度增长。
//!
//! 编辑器始终持有键盘焦点；`col` 以字符（char）为单位，可视列由渲染层
//! 按 unicode 宽度换算。高度为内容折行数钳制在 `[1, max_rows]`。鼠标滚轮
//! 可把视口暂时移离光标（`scroll_override`），任何编辑/移动光标操作立即
//! 清除覆盖、回到跟随光标。

use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

/// 超过该字符数的大粘贴不再逐字塞进编辑器：插入一行原子占位块，全文
/// 暂存、提交时展开（Codex `LARGE_PASTE_CHAR_THRESHOLD` 同形，阈值同为
/// 1000）。占位块不可停留光标、整体删除，逐帧折行只算一行标签。
pub(super) const LARGE_PASTE_CHAR_THRESHOLD: usize = 1000;

/// 设置/命名菜单单行字段的粘贴上限：provider/model/reasoning 均为短词，
/// 超限部分不可能合法，截断以防大体积文本进入逐帧弹窗渲染。
pub(super) const SETTINGS_PASTE_CHAR_LIMIT: usize = 512;

/// 同一逻辑粘贴被终端拆成多次投递时，把会话窗内的连续投递并入同一块
/// （Windows 控制台无帧标记，Pi 式的结束符重组不可用，时间窗口是等价实现）。
/// 分块是同一数据流的连续切分，直接拼接、不加分隔符；窗口外或有过其它
/// 编辑/移动的投递起新块。窗口取 1 秒：分块间隔是毫秒级，故意重粘的间隔
/// 是秒级；窗口内误并的代价是少一个块间换行（内容都在、顺序不错），窗口
/// 外漏并的代价只是多一个块——前者更重，所以窗口宁小。
const PASTE_SESSION_WINDOW: Duration = Duration::from_secs(1);

/// 粘贴会话：首次插入前的光标与整行快照、插入末端行号、已累积全文。
/// 区间内只会有本次会话插入的内容——其它任何编辑/移动/清空都终结会话
/// （各调用点置空），合并时只做行号边界防御。行快照让复原与插入形状
/// 无关（空头行丢弃后区间起点可能是块自身，坐标考古不可靠）。
#[derive(Debug, Clone)]
struct PasteSession {
    row: usize,
    col: usize,
    line: String,
    end_row: usize,
    text: String,
    since: Instant,
}
/// 编辑器的一行：普通文本行，或大粘贴的原子占位块（携带全文与显示标签）。
#[derive(Debug, Clone)]
enum EditorLine {
    Text(String),
    Paste(PasteBlock),
}

/// 大粘贴块：`label` 为单行显示（折行备忘直接采用），`text` 为提交时
/// 展开的全文。块随行入列、随行删除，无第二份索引需要同步。
#[derive(Debug, Clone)]
struct PasteBlock {
    label: String,
    text: String,
}

impl EditorLine {
    fn display(&self) -> &str {
        match self {
            EditorLine::Text(text) => text.as_str(),
            EditorLine::Paste(block) => block.label.as_str(),
        }
    }
}

/// 鼠标选中区间：锚点（按下处）+ 光标（拖到处）定范围；打字/换行/退格/
/// 删除/粘贴动手前先吃掉区间。锚点与光标恒落在文本行（定位时吸附），
/// 区间跨过的占位块整体删除。
#[derive(Debug, Clone, Copy)]
struct Selection {
    anchor_row: usize,
    anchor_col: usize,
}

/// 内容折行的帧间备忘：宽度或内容任一变化即失效。`wrapped_pieces` 与
/// `cursor_visual` 同帧共享同一次折行，宽字符规则与原来逐处调用一致。
#[derive(Debug, Default, Clone)]
struct EditorLayout {
    width: u16,
    pieces: Vec<String>,
    offsets: Vec<Vec<usize>>,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct Editor {
    lines: Vec<EditorLine>,
    row: usize,
    col: usize,
    /// 鼠标滚轮造成的视口行偏移；`None` 表示跟随光标。
    scroll_override: Option<usize>,
    layout_cache: std::cell::RefCell<EditorLayout>,
    /// 进行中的粘贴会话（见 [`PASTE_SESSION_WINDOW`]）；其它编辑/移动/
    /// 清空将其置空，合并路径开头取走。
    paste_session: Option<PasteSession>,
    /// 鼠标选中（锚点）；活动端即光标。键盘移动/编辑复位，纯点击松开即散。
    selection: Option<Selection>,
}

impl Editor {
    pub fn new() -> Self {
        Self {
            lines: vec![EditorLine::Text(String::new())],
            row: 0,
            col: 0,
            scroll_override: None,
            layout_cache: std::cell::RefCell::default(),
            paste_session: None,
            selection: None,
        }
    }

    /// 内容变化即废弃折行备忘（光标移动与滚轮覆盖不影响折行，不断备忘）。
    /// `width == 0` 就是"没有备忘"：真实宽度经 `max(1)` 恒 ≥ 1，0 是安全的空标记。
    fn invalidate_layout(&mut self) {
        *self.layout_cache.borrow_mut() = EditorLayout::default();
    }

    /// 内容折行与行内偏移的共享备忘：同宽同内容只算一次，渲染与光标
    /// 换算共用。
    fn layout(&self, width: u16) -> std::cell::Ref<'_, EditorLayout> {
        let width = width.max(1);
        let cached_width = self.layout_cache.borrow().width;
        if cached_width != width {
            let w = width as usize;
            let mut rebuilt = EditorLayout {
                width,
                pieces: Vec::new(),
                offsets: Vec::with_capacity(self.lines.len()),
            };
            for line in &self.lines {
                match line {
                    EditorLine::Text(text) => {
                        rebuilt.offsets.push(super::wrap_offsets(text, w));
                        rebuilt.pieces.extend(super::wrapped_lines(text, w));
                    }
                    // 占位块恒为一行标签：宽度无关，不参与折行。
                    EditorLine::Paste(block) => {
                        rebuilt.offsets.push(vec![0]);
                        rebuilt.pieces.push(block.label.clone());
                    }
                }
            }
            *self.layout_cache.borrow_mut() = rebuilt;
        }
        self.layout_cache.borrow()
    }

    pub fn is_empty(&self) -> bool {
        self.lines
            .iter()
            .all(|line| matches!(line, EditorLine::Text(text) if text.is_empty()))
    }

    /// 展示文本：粘贴块只露出一行标签（命令菜单触发、草稿检查等展示用途）。
    pub fn text(&self) -> String {
        self.lines
            .iter()
            .map(EditorLine::display)
            .collect::<Vec<_>>()
            .join("\n")
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

    /// 取走提交文本并复位：粘贴块展开为全文。发送/转向/排队与出队、
    /// 退还合并统一走此处，内容不丢。
    pub fn take_expanded(&mut self) -> String {
        let text = self.expanded_text();
        self.clear();
        text
    }

    /// 提交形态的全文（非破坏性）：历史回溯的草稿暂存用，恢复后内容不丢
    /// （占位形态不保留，与中断退还一致）。
    pub fn expanded_text(&self) -> String {
        self.lines
            .iter()
            .map(|line| match line {
                EditorLine::Text(text) => text.as_str(),
                EditorLine::Paste(block) => block.text.as_str(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn clear(&mut self) {
        self.lines = vec![EditorLine::Text(String::new())];
        self.row = 0;
        self.col = 0;
        self.scroll_override = None;
        self.paste_session = None;
        self.selection = None;
        self.invalidate_layout();
    }

    /// 动手前的共同瞬态收尾：粘贴会话结束，滚轮视口回到跟随光标。
    fn end_transient(&mut self) {
        self.paste_session = None;
        self.scroll_override = None;
    }

    /// 键盘移动光标：选中散掉（不删内容），粘贴会话与滚轮覆盖一并结束。
    fn cancel_selection_for_move(&mut self) {
        self.selection = None;
        self.end_transient();
    }

    /// 鼠标按下起选：锚点=当前光标（调用方已先定位）。拖拽只动光标，
    /// 锚点不动，区间自然扩展。
    pub fn begin_selection(&mut self) {
        self.selection = Some(Selection {
            anchor_row: self.row,
            anchor_col: self.col,
        });
    }

    /// 鼠标松开：零宽（纯点击）即散选，有宽度则保留高亮。
    pub fn end_selection(&mut self) {
        if let Some(selection) = self.selection
            && selection.anchor_row == self.row
            && selection.anchor_col == self.col
        {
            self.selection = None;
        }
    }

    /// 取消编辑器选中：与会话流选区互斥（会话流起选时调用），不动光标。
    pub fn cancel_selection(&mut self) {
        self.selection = None;
    }

    /// 规范化选中区间（起止有序）；无选择或零宽返回 `None`。
    fn selection_range(&self) -> Option<((usize, usize), (usize, usize))> {
        let selection = self.selection?;
        let (anchor, cursor) = (
            (selection.anchor_row, selection.anchor_col),
            (self.row, self.col),
        );
        if anchor == cursor {
            return None;
        }
        Some(if anchor < cursor {
            (anchor, cursor)
        } else {
            (cursor, anchor)
        })
    }

    /// 吃掉选中区：删区间内容（含跨过的占位块整体），光标落起点，有选区
    /// 返回 true。调用方在内容变化前先调；粘贴会话一并终结（区间外的投递
    /// 续传视为新会话）。
    fn delete_selection(&mut self) -> bool {
        // 动手就结束粘贴会话与滚轮覆盖，无论是否真有选中。
        self.end_transient();
        let Some(((start_row, start_col), (end_row, end_col))) = self.selection_range() else {
            return false;
        };
        self.selection = None;
        self.invalidate_layout();
        if start_row == end_row {
            if let Some(EditorLine::Text(line)) = self.lines.get_mut(start_row) {
                let len = line.chars().count();
                let (from, to) = (start_col.min(len), end_col.min(len));
                line.replace_range(char_to_byte(line, from)..char_to_byte(line, to), "");
            }
        } else {
            let tail = match self.lines.get(end_row) {
                Some(EditorLine::Text(text)) => {
                    text[char_to_byte(text, end_col.min(text.chars().count()))..].to_string()
                }
                _ => String::new(),
            };
            if let Some(EditorLine::Text(head)) = self.lines.get_mut(start_row) {
                head.truncate(char_to_byte(head, start_col));
                head.push_str(&tail);
            }
            if end_row > start_row {
                self.lines.drain(start_row + 1..=end_row);
            }
        }
        self.row = start_row;
        self.col = start_col.min(self.row_len(start_row));
        true
    }

    /// 指定行的字符数（粘贴块按 0 计）：行宽钳制的单一口径。
    fn row_len(&self, row: usize) -> usize {
        match self.lines.get(row) {
            Some(EditorLine::Text(text)) => text.chars().count(),
            _ => 0,
        }
    }

    /// 就近吸附到文本行并给出落列（光标恒不停在粘贴块上）：`from` 起向下找
    /// 到则取行首，否则向上取最近文本行的行尾。鼠标点中块、或点在末行之外
    /// 时都归到这里。
    fn nearest_text_caret(&self, from: usize) -> Option<(usize, usize)> {
        let is_text = |row: usize| matches!(self.lines.get(row), Some(EditorLine::Text(_)));
        if let Some(row) = (from..self.lines.len()).find(|&row| is_text(row)) {
            return Some((row, 0));
        }
        (0..from)
            .rev()
            .find(|&row| is_text(row))
            .map(|row| (row, self.row_len(row)))
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
        self.delete_selection();
        let Some(EditorLine::Text(line)) = self.lines.get_mut(self.row) else {
            return;
        };
        let byte = char_to_byte(line, self.col);
        line.insert(byte, ch);
        self.col += 1;
        self.invalidate_layout();
    }

    pub fn insert_newline(&mut self) {
        self.delete_selection();
        let Some(EditorLine::Text(line)) = self.lines.get_mut(self.row) else {
            return;
        };
        let byte = char_to_byte(line, self.col);
        let tail = line.split_off(byte);
        self.lines.insert(self.row + 1, EditorLine::Text(tail));
        self.row += 1;
        self.col = 0;
        self.invalidate_layout();
    }

    /// 在光标处插入整段文本；内部 `\n` 拆成多行。
    pub fn insert_str(&mut self, text: &str) {
        self.end_transient();
        self.invalidate_layout();
        let mut parts = text.split('\n').peekable();
        // 首段并入当前行光标处。
        if let Some(first) = parts.next()
            && !first.is_empty()
            && let Some(EditorLine::Text(line)) = self.lines.get_mut(self.row)
        {
            let byte = char_to_byte(line, self.col);
            line.insert_str(byte, first);
            self.col += first.chars().count();
        }
        // 后续每段前先拆行，再写入新行。
        for part in parts {
            self.insert_newline();
            if !part.is_empty()
                && let Some(EditorLine::Text(line)) = self.lines.get_mut(self.row)
            {
                line.insert_str(0, part);
                self.col = part.chars().count();
            }
        }
    }

    /// 粘贴投递的统一入口（括号粘贴/同批突发/突发落定三路汇合）：会话窗内
    /// 的连续投递并入同一块——回到会话起始复原，全文重走一次插入（长短
    /// 路由与标签字数自动更新）。空投递直接忽略（Pi 同形的前置守卫）。
    pub fn insert_paste(&mut self, text: String, now: Instant) {
        if text.is_empty() {
            return;
        }
        if let Some(session) = self.paste_session.take() {
            let live = now.duration_since(session.since) <= PASTE_SESSION_WINDOW
                && self.session_span_usable(&session);
            if live {
                let PasteSession {
                    row,
                    col,
                    line,
                    end_row,
                    text: mut full,
                    ..
                } = session;
                full.push_str(&text);
                self.restore_session_span(row, col, line, end_row);
                self.insert_paste_fresh(full, now);
                return;
            }
        }
        self.insert_paste_fresh(text, now);
    }

    /// 单次粘贴的实际插入并开启会话：超限进原子占位块（空的首尾行不留，
    /// 块前不再有空行），否则以内联文本落子（短分块先以内联形态暂存，
    /// 后续投递到达时再吸收合并）。
    fn insert_paste_fresh(&mut self, text: String, now: Instant) {
        // 粘贴替换选中：先吃选区再落子（会话取走在外层，内部置空无影响）。
        self.delete_selection();
        self.invalidate_layout();
        let (row, col) = (self.row, self.col);
        let line = match self.lines.get(row) {
            Some(EditorLine::Text(current)) => current.clone(),
            _ => return,
        };
        if text.chars().count() > LARGE_PASTE_CHAR_THRESHOLD {
            let Some(EditorLine::Text(head)) = self.lines.get_mut(row) else {
                return;
            };
            let byte = char_to_byte(head, self.col);
            let tail = head.split_off(byte);
            let head_empty = head.is_empty();
            let chars = text.chars().count();
            let label = format!("[pasted text · {chars} chars · expands on submit]");
            let block = EditorLine::Paste(PasteBlock {
                label,
                text: text.clone(),
            });
            if head_empty {
                // 空行处粘贴：块顶替该行，不留空头行；尾段（可能为空）随后，
                // 光标永远落在文本行。
                self.lines[row] = block;
                self.lines.insert(row + 1, EditorLine::Text(tail));
                self.row = row + 1;
            } else {
                self.lines.insert(row + 1, block);
                self.lines.insert(row + 2, EditorLine::Text(tail));
                self.row = row + 2;
            }
            self.col = 0;
        } else {
            // 会话为空（刚取走/过期），内联插入不污染会话。
            self.insert_str(&text);
        }
        let end_row = self.row;
        self.paste_session = Some(PasteSession {
            row,
            col,
            line,
            end_row,
            text,
            since: now,
        });
    }

    /// 会话区间是否可用：起止行越界即放弃合并（正常路径下不可达——其它
    /// 编辑/移动都会先终结会话；防御不断言、不抛错）。
    fn session_span_usable(&self, session: &PasteSession) -> bool {
        session.row <= session.end_row
            && session.end_row < self.lines.len()
            && session.row < self.lines.len()
    }

    /// 回到会话起始：区间内会话插入的行整体删掉，起始行恢复快照，光标归位。
    /// 之后调用方重走一次插入，等价于“整包一次到达”。调用前已确认区间
    /// 可用，参数取快照值而非会话引用（调用方已解构会话取走累积文本）。
    fn restore_session_span(&mut self, row: usize, col: usize, line: String, end_row: usize) {
        self.scroll_override = None;
        self.invalidate_layout();
        self.lines.splice(row..=end_row, [EditorLine::Text(line)]);
        self.row = row;
        self.col = col;
    }

    pub fn backspace(&mut self) {
        // 选中吃掉本次按键：只删选区，不再多删一字。
        if self.delete_selection() {
            return;
        }
        if self.col > 0 {
            let Some(EditorLine::Text(line)) = self.lines.get_mut(self.row) else {
                return;
            };
            let byte = char_to_byte(line, self.col - 1);
            line.remove(byte);
            self.col -= 1;
        } else if self.row > 0 {
            match self.lines.get(self.row - 1) {
                // 块整体删除：光标留在原文本行首（行号前移一位）。
                Some(EditorLine::Paste(_)) => {
                    self.lines.remove(self.row - 1);
                    self.row -= 1;
                    self.col = 0;
                }
                Some(EditorLine::Text(_)) => {
                    let tail = match self.lines.remove(self.row) {
                        EditorLine::Text(tail) => tail,
                        EditorLine::Paste(_) => String::new(),
                    };
                    self.row -= 1;
                    let Some(EditorLine::Text(line)) = self.lines.get_mut(self.row) else {
                        return;
                    };
                    self.col = line.chars().count();
                    line.push_str(&tail);
                }
                None => {}
            }
        }
        self.invalidate_layout();
    }

    pub fn delete(&mut self) {
        // 选中吃掉本次按键：只删选区，不再多删一字。
        if self.delete_selection() {
            return;
        }
        let line_len = self.row_len(self.row);
        if self.col < line_len {
            let Some(EditorLine::Text(line)) = self.lines.get_mut(self.row) else {
                return;
            };
            let byte = char_to_byte(line, self.col);
            line.remove(byte);
        } else if self.row + 1 < self.lines.len() {
            match self.lines.get(self.row + 1) {
                // 块整体删除。
                Some(EditorLine::Paste(_)) => {
                    self.lines.remove(self.row + 1);
                }
                Some(EditorLine::Text(_)) => {
                    let tail = match self.lines.remove(self.row + 1) {
                        EditorLine::Text(tail) => tail,
                        EditorLine::Paste(_) => String::new(),
                    };
                    let Some(EditorLine::Text(line)) = self.lines.get_mut(self.row) else {
                        return;
                    };
                    line.push_str(&tail);
                }
                None => {}
            }
        }
        self.invalidate_layout();
    }

    pub fn move_left(&mut self) {
        self.cancel_selection_for_move();
        if self.col > 0 {
            self.col -= 1;
            return;
        }
        // 行首左移：越过粘贴块，落到上一个文本行尾。
        let mut row = self.row;
        while row > 0 {
            row -= 1;
            if let Some(EditorLine::Text(line)) = self.lines.get(row) {
                self.row = row;
                self.col = line.chars().count();
                return;
            }
        }
    }

    pub fn move_right(&mut self) {
        self.cancel_selection_for_move();
        let line_len = self.row_len(self.row);
        if self.col < line_len {
            self.col += 1;
            return;
        }
        // 行尾右移：越过粘贴块，落到下一个文本行首。
        let mut row = self.row;
        while row + 1 < self.lines.len() {
            row += 1;
            if matches!(self.lines.get(row), Some(EditorLine::Text(_))) {
                self.row = row;
                self.col = 0;
                return;
            }
        }
    }

    pub fn move_up(&mut self) {
        self.cancel_selection_for_move();
        // 越过粘贴块，列钳制到目标文本行宽。
        let mut row = self.row;
        while row > 0 {
            row -= 1;
            if matches!(self.lines.get(row), Some(EditorLine::Text(_))) {
                self.row = row;
                self.col = self.col.min(self.row_len(row));
                return;
            }
        }
    }

    pub fn move_down(&mut self) {
        self.cancel_selection_for_move();
        let mut row = self.row;
        while row + 1 < self.lines.len() {
            row += 1;
            if matches!(self.lines.get(row), Some(EditorLine::Text(_))) {
                self.row = row;
                self.col = self.col.min(self.row_len(row));
                return;
            }
        }
    }

    pub fn move_home(&mut self) {
        self.cancel_selection_for_move();
        self.col = 0;
    }

    pub fn move_end(&mut self) {
        self.cancel_selection_for_move();
        self.col = self.row_len(self.row);
    }

    /// 内容按宽度逐逻辑行折行后的全部可视片段；高度计算与渲染共用这一
    /// 备忘的产物（同帧内光标换算不再重复折行）。
    pub fn wrapped_pieces(&self, width: u16) -> Vec<String> {
        self.layout(width).pieces.clone()
    }

    /// 选中区在折行后各可视片段上的字符区间：(全局可视行, 片段内起, 止)，
    /// 按全局可视行升序。无选择/零宽返回空。块行整行高亮（标签全文）。
    pub(crate) fn selection_spans(&self, width: u16) -> Vec<(usize, usize, usize)> {
        let Some(((start_row, start_col), (end_row, end_col))) = self.selection_range() else {
            return Vec::new();
        };
        let layout = self.layout(width);
        let mut spans = Vec::new();
        let mut base = 0usize;
        for (index, line) in self.lines.iter().enumerate() {
            let piece_count = layout.offsets.get(index).map_or(0, Vec::len);
            if index >= start_row && index <= end_row {
                let text_len = line.display().chars().count();
                let (from, to) = if start_row == end_row {
                    (start_col, end_col)
                } else if index == start_row {
                    (start_col, text_len)
                } else if index == end_row {
                    (0, end_col)
                } else {
                    (0, text_len)
                };
                let (from, to) = (from.min(text_len), to.min(text_len));
                if from < to
                    && let Some(offsets) = layout.offsets.get(index)
                {
                    for (piece, &piece_start) in offsets.iter().enumerate() {
                        let piece_end = offsets.get(piece + 1).copied().unwrap_or(text_len);
                        let (sel_from, sel_to) = (from.max(piece_start), to.min(piece_end));
                        if sel_from < sel_to {
                            spans.push((
                                base + piece,
                                sel_from - piece_start,
                                sel_to - piece_start,
                            ));
                        }
                    }
                }
            }
            base += piece_count;
        }
        spans
    }
    /// 光标在折行后的可视坐标：返回 (可视行, 可视列)，供终端光标定位。
    /// 偏移表取自与 `wrapped_pieces` 同一份备忘。
    pub fn cursor_visual(&self, width: u16) -> (usize, usize) {
        let layout = self.layout(width);
        let mut visual_row = 0usize;
        for (index, line) in self.lines.iter().enumerate() {
            let empty: &[usize] = &[];
            let offsets = layout.offsets.get(index).map_or(empty, Vec::as_slice);
            if index == self.row {
                let text = line.display();
                let target_char = self.col;
                let mut consumed = 0usize;
                for (row_index, &char_start) in offsets.iter().enumerate() {
                    let row_chars = offsets
                        .get(row_index + 1)
                        .copied()
                        .unwrap_or_else(|| text.chars().count())
                        - char_start;
                    if target_char < consumed + row_chars || row_index + 1 == offsets.len() {
                        let within = target_char.saturating_sub(consumed);
                        let prefix: String = text.chars().skip(char_start).take(within).collect();
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
    /// 粘贴块不可停留：点中时就近吸附到相邻文本行（优先下方行首）。
    /// 点击定位即光标移动：清除滚轮滚动覆盖。
    pub fn set_cursor_visual(&mut self, target_row: usize, target_col: usize, width: u16) {
        self.end_transient();
        let width = width.max(1);
        let w = width as usize;
        // 命中行与该可视片段的起始字符：折行取自与渲染同一份备忘（`layout`），
        // 不在这里重算一遍。
        let mut hit: Option<(usize, usize)> = None;
        {
            let layout = self.layout(width);
            let mut visual_row = 0usize;
            for (logical_row, offsets) in layout.offsets.iter().enumerate() {
                let span = offsets.len().max(1);
                if target_row < visual_row + span {
                    let piece = target_row - visual_row;
                    hit = Some((logical_row, offsets.get(piece).copied().unwrap_or(0)));
                    break;
                }
                visual_row += span;
            }
        }
        let caret = match hit {
            Some((logical_row, char_start)) => match self.lines.get(logical_row) {
                Some(EditorLine::Text(text)) => {
                    let mut used = 0usize;
                    let mut chars = 0usize;
                    for ch in text.chars().skip(char_start) {
                        let ch_width = super::char_display_width(ch);
                        if used + ch_width > target_col || used + ch_width > w {
                            break;
                        }
                        used += ch_width;
                        chars += 1;
                    }
                    Some((logical_row, char_start + chars))
                }
                // 点中粘贴块：块不可停留光标，吸附到相邻文本行（优先下方行首）。
                Some(EditorLine::Paste(_)) => self.nearest_text_caret(logical_row + 1),
                None => None,
            },
            // 点在末行之外：落到最后一个文本行尾。
            None => self.nearest_text_caret(self.lines.len()),
        };
        if let Some((row, col)) = caret {
            self.row = row;
            self.col = col;
        }
    }
}

fn char_to_byte(line: &str, char_index: usize) -> usize {
    line.char_indices()
        .nth(char_index)
        .map(|(byte, _)| byte)
        .unwrap_or(line.len())
}

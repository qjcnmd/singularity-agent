//! 会话流投影：typed 事件合并为可读条目，并按宽度计算可视行。
//!
//! [`Transcript`] 是纯状态对象：assistant 增量累积成段落；工具调用以
//! [`ToolItem`] 为单位就地刷新（运行中更新预览，结束后固化为稳定记录），
//! 不向会话流追加重复行。可视行计算覆盖显式换行、CJK 宽字符与长行折行。

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

const TOOL_RESULT_PREVIEW_LINES: usize = 3;
/// 展开态下的结果行上限：防超长输出撑爆视口。
const TOOL_RESULT_EXPANDED_LINES: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoteStyle {
    Dim,
    Info,
    Warning,
    Error,
    Accent,
}

impl NoteStyle {
    fn style(self) -> Style {
        match self {
            Self::Dim => Style::new().fg(Color::DarkGray),
            Self::Info => Style::new(),
            Self::Warning => Style::new().fg(Color::Yellow),
            Self::Error => Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            Self::Accent => Style::new().fg(Color::Cyan),
        }
    }
}

/// 工具调用的会话流记录：开始后就地更新，结束一次定型。
#[derive(Debug)]
pub(crate) struct ToolItem {
    pub call_id: String,
    pub name: String,
    args_head: String,
    state: ToolState,
}

#[derive(Debug)]
enum ToolState {
    /// 运行中：携带最近一次增量输出的有界预览。
    Running { last_output: String },
    /// 已结束：稳定结果、错误标记与展开态（展开显示全量预览）。
    Done {
        output: String,
        is_error: bool,
        expanded: bool,
    },
}

impl ToolItem {
    fn header(&self) -> String {
        if self.args_head.is_empty() {
            self.name.clone()
        } else {
            format!("{} {}", self.name, self.args_head)
        }
    }
}

/// 会话流中的一个条目。
#[derive(Debug)]
pub(crate) enum FlowItem {
    Text { style: NoteStyle, text: String },
    Tool(ToolItem),
}

/// 主会话流投影状态。
#[derive(Default)]
pub(crate) struct Transcript {
    items: Vec<FlowItem>,
    assistant_buffer: String,
    assistant_active: bool,
}

impl Transcript {
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加一条非流式文本（先落定进行中的 assistant 段落）。
    pub fn push_note(&mut self, text: impl Into<String>, style: NoteStyle) {
        self.flush_assistant();
        self.items.push(FlowItem::Text {
            style,
            text: text.into(),
        });
    }

    /// 累积 assistant 增量：同一段落只产生一个会话流条目。
    pub fn assistant_delta(&mut self, delta: &str) {
        if !self.assistant_active {
            self.assistant_buffer.clear();
            self.assistant_active = true;
        }
        self.assistant_buffer.push_str(delta);
    }

    /// 落定当前 assistant 段落（若有）。
    pub fn flush_assistant(&mut self) {
        if self.assistant_active {
            let text = std::mem::take(&mut self.assistant_buffer);
            self.items.push(FlowItem::Text {
                style: NoteStyle::Info,
                text,
            });
            self.assistant_active = false;
        }
    }

    /// 工具开始：建立运行中记录；未知重复 id 保持原记录（幂等）。
    pub fn tool_start(&mut self, call_id: &str, name: &str, args: &serde_json::Value) {
        self.flush_assistant();
        if self.tool_item(call_id).is_some() {
            return;
        }
        let serialized = serde_json::to_string(args).unwrap_or_default();
        self.items.push(FlowItem::Tool(ToolItem {
            call_id: call_id.to_string(),
            name: name.to_string(),
            args_head: truncate_chars(&serialized, 120),
            state: ToolState::Running {
                last_output: String::new(),
            },
        }));
    }

    /// 工具增量：仅刷新对应运行中记录的预览，不新增条目。
    pub fn tool_update(&mut self, call_id: &str, partial_output: &str) {
        if let Some(item) = self.tool_item_mut(call_id)
            && let ToolState::Running { last_output } = &mut item.state
        {
            *last_output = truncate_chars(partial_output, 200);
        }
    }

    /// 工具结束：就地定型为稳定记录；首个终态生效，重复终态保持首见结果。
    pub fn tool_end(&mut self, call_id: &str, result: &str, is_error: bool) {
        if let Some(item) = self.tool_item_mut(call_id) {
            if matches!(item.state, ToolState::Running { .. }) {
                item.state = ToolState::Done {
                    output: result.to_string(),
                    is_error,
                    expanded: false,
                };
            }
        } else {
            // 终态补偿路径可能先于 start 到达（理论上不发生）：保真为完整记录。
            self.items.push(FlowItem::Tool(ToolItem {
                call_id: call_id.to_string(),
                name: call_id.to_string(),
                args_head: String::new(),
                state: ToolState::Done {
                    output: result.to_string(),
                    is_error,
                    expanded: false,
                },
            }));
        }
    }

    /// 切换最近一个已完成工具块的展开态；返回是否发生了切换。
    /// 运行中或没有已完成工具时为 false（提示行据此不再承诺按键行为）。
    pub fn toggle_latest_tool_expansion(&mut self) -> bool {
        for item in self.items.iter_mut().rev() {
            if let FlowItem::Tool(tool) = item
                && let ToolState::Done { expanded, .. } = &mut tool.state
            {
                *expanded = !*expanded;
                return true;
            }
        }
        false
    }

    fn tool_item(&self, call_id: &str) -> Option<&ToolItem> {
        self.items.iter().find_map(|item| match item {
            FlowItem::Tool(tool) if tool.call_id == call_id => Some(tool),
            _ => None,
        })
    }

    fn tool_item_mut(&mut self, call_id: &str) -> Option<&mut ToolItem> {
        self.items.iter_mut().find_map(|item| match item {
            FlowItem::Tool(tool) if tool.call_id == call_id => Some(tool),
            _ => None,
        })
    }

    /// 条目总数。
    pub fn item_count(&self) -> usize {
        self.items.len()
    }

    /// 进行中 assistant 段落的可视行数：未落定内容随帧实时可见，不再
    /// 等到段落关闭才一次性出现。
    pub fn live_row_count(&self, width: u16) -> usize {
        if !self.assistant_active {
            return 0;
        }
        wrapped_lines(&self.assistant_buffer, width.max(1) as usize)
            .len()
            .max(1)
    }

    /// 进行中 assistant 段落的第 `row_in_item` 可视行。
    pub fn render_live_row(&self, row_in_item: usize, width: u16) -> Option<Line<'static>> {
        if !self.assistant_active {
            return None;
        }
        wrapped_lines(&self.assistant_buffer, width.max(1) as usize)
            .into_iter()
            .nth(row_in_item)
            .map(|line| Line::from(Span::styled(line, NoteStyle::Info.style())))
    }

    /// 在给定宽度下每个条目占用的可视行数。
    pub fn row_counts(&self, width: u16) -> Vec<usize> {
        let width = width.max(1) as usize;
        self.items
            .iter()
            .map(|item| item_row_count(item, width))
            .collect()
    }

    /// 物化某条目的第 `row_in_item` 可视行为一行渲染输出。
    /// `spinner` 为运行中工具的状态字符，由调用方按节拍提供。
    pub fn render_item_row(
        &self,
        item_index: usize,
        row_in_item: usize,
        width: u16,
        spinner: char,
    ) -> Option<Line<'static>> {
        let width = width.max(1) as usize;
        let item = self.items.get(item_index)?;
        match item {
            FlowItem::Text { style, text } => wrapped_lines(text, width)
                .into_iter()
                .nth(row_in_item)
                .map(|line| Line::from(Span::styled(line, style.style()))),
            FlowItem::Tool(tool) => {
                let header_rows = wrapped_lines(&tool.header(), width.saturating_sub(2));
                let header_len = header_rows.len().max(1);
                if row_in_item < header_len {
                    let marker = match &tool.state {
                        ToolState::Running { .. } => spinner,
                        ToolState::Done { is_error: true, .. } => '✖',
                        ToolState::Done {
                            is_error: false, ..
                        } => '·',
                    };
                    let style = match &tool.state {
                        ToolState::Running { .. } => NoteStyle::Accent.style(),
                        ToolState::Done { is_error: true, .. } => NoteStyle::Error.style(),
                        ToolState::Done {
                            is_error: false, ..
                        } => NoteStyle::Dim.style(),
                    };
                    return Some(Line::from(Span::styled(
                        format!("{marker} {}", header_rows[row_in_item]),
                        style,
                    )));
                }
                let row = row_in_item - header_len;
                match &tool.state {
                    ToolState::Running { last_output } => {
                        if last_output.trim().is_empty() {
                            return None;
                        }
                        let preview = bounded_preview(last_output);
                        preview.into_iter().nth(row).map(|line| {
                            Line::from(Span::styled(format!("│ {line}"), NoteStyle::Dim.style()))
                        })
                    }
                    ToolState::Done {
                        output,
                        is_error,
                        expanded,
                    } => {
                        let total_nonempty =
                            output.lines().filter(|l| !l.trim().is_empty()).count();
                        let visible: Vec<&str> = if *expanded {
                            output
                                .lines()
                                .filter(|line| !line.trim().is_empty())
                                .take(TOOL_RESULT_EXPANDED_LINES)
                                .collect()
                        } else {
                            bounded_preview(output)
                        };
                        if row < visible.len() {
                            let style = if *is_error {
                                NoteStyle::Error.style()
                            } else {
                                NoteStyle::Dim.style()
                            };
                            Some(Line::from(Span::styled(
                                format!("│ {}", visible[row]),
                                style,
                            )))
                        } else if row == visible.len() {
                            if *expanded && total_nonempty > visible.len() {
                                Some(Line::from(Span::styled(
                                    format!(
                                        "│ … {} more lines (Alt+O collapse)",
                                        total_nonempty - visible.len()
                                    ),
                                    NoteStyle::Dim.style(),
                                )))
                            } else if !*expanded && total_nonempty > visible.len() {
                                Some(Line::from(Span::styled(
                                    format!(
                                        "│ … {} more lines (Alt+O expand)",
                                        total_nonempty - visible.len()
                                    ),
                                    NoteStyle::Dim.style(),
                                )))
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    }
                }
            }
        }
    }
}

/// 有界结果预览：前若干非空行。
fn bounded_preview(output: &str) -> Vec<&str> {
    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .take(TOOL_RESULT_PREVIEW_LINES)
        .collect()
}

fn item_row_count(item: &FlowItem, width: usize) -> usize {
    match item {
        FlowItem::Text { text, .. } => wrapped_lines(text, width).len().max(1),
        FlowItem::Tool(tool) => {
            let mut rows = wrapped_lines(&tool.header(), width.saturating_sub(2))
                .len()
                .max(1);
            match &tool.state {
                ToolState::Running { last_output } => {
                    if !last_output.trim().is_empty() {
                        rows += TOOL_RESULT_PREVIEW_LINES;
                    }
                }
                ToolState::Done {
                    output, expanded, ..
                } => {
                    let total_nonempty = output.lines().filter(|l| !l.trim().is_empty()).count();
                    if *expanded {
                        let visible = total_nonempty.min(TOOL_RESULT_EXPANDED_LINES);
                        rows += visible;
                        if total_nonempty > visible {
                            rows += 1;
                        }
                    } else {
                        let preview = total_nonempty.min(TOOL_RESULT_PREVIEW_LINES);
                        rows += preview;
                        if total_nonempty > TOOL_RESULT_PREVIEW_LINES {
                            rows += 1;
                        }
                    }
                }
            }
            rows
        }
    }
}

/// 贪心折行：按显示宽度断行，显式 `\n` 强制换行；空文本产出一空行。
fn wrapped_lines(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for logical in text.split('\n') {
        let mut current = String::new();
        let mut current_width = 0usize;
        for ch in logical.chars() {
            let ch_width = UnicodeWidthStr::width(ch.to_string().as_str());
            if current_width + ch_width > width && !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                current_width = 0;
            }
            current.push(ch);
            current_width += ch_width;
        }
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let cut: String = text.chars().take(max_chars.saturating_sub(3)).collect();
    format!("{cut}...")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assistant_deltas_merge_into_one_paragraph() {
        let mut transcript = Transcript::new();
        transcript.assistant_delta("Hel");
        transcript.assistant_delta("lo");
        // 其他事实到达时段落落定，且只有一个文本条目。
        transcript.push_note("marker", NoteStyle::Dim);
        assert_eq!(transcript.item_count(), 2);
        let rows = transcript.row_counts(80);
        assert_eq!(rows, vec![1, 1]);
        let first = transcript.render_item_row(0, 0, 80, ' ').unwrap();
        let text: String = first
            .spans
            .iter()
            .map(|span| span.content.clone())
            .collect();
        assert_eq!(text, "Hello");
    }

    #[test]
    fn explicit_newlines_and_wide_chars_count_as_visual_rows() {
        let mut transcript = Transcript::new();
        // 「中文」宽度为 4；宽度 5 时一行放不下两个词，折成两行。
        transcript.push_note("中文中文中文", NoteStyle::Info);
        let rows = transcript.row_counts(5);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0], 3, "6 wide chars at width 5 wrap into 3 rows");
        // 显式换行强制分行。
        let mut multi = Transcript::new();
        multi.push_note("a\nb\nc", NoteStyle::Info);
        assert_eq!(multi.row_counts(80), vec![3]);
    }

    #[test]
    fn tool_lifecycle_updates_in_place_and_converges_once() {
        let mut transcript = Transcript::new();
        transcript.tool_start("call-1", "bash", &serde_json::json!({"command":"echo hi"}));
        assert_eq!(
            transcript.item_count(),
            1,
            "start creates exactly one record"
        );
        transcript.tool_update("call-1", "partial output");
        transcript.tool_update("call-1", "partial output two");
        assert_eq!(transcript.item_count(), 1, "updates refresh in place");
        transcript.tool_end("call-1", "line one\nline two\nline three\nline four", false);
        assert_eq!(transcript.item_count(), 1, "end finalizes in place");
        // 幂等终态：重复 end 不新增条目、不覆盖首见结果。
        transcript.tool_end("call-1", "different", true);
        assert_eq!(transcript.item_count(), 1);

        // 渲染：头部 + 三行预览 + 溢出行。
        let counts = transcript.row_counts(80);
        assert_eq!(counts[0], 5, "header + 3 preview lines + more-lines hint");
        let header = transcript.render_item_row(0, 0, 80, '|').unwrap();
        let text: String = header.spans.iter().map(|s| s.content.clone()).collect();
        assert!(text.contains("bash"), "header names the tool");
        let more = transcript.render_item_row(0, 4, 80, '|').unwrap();
        let text: String = more.spans.iter().map(|s| s.content.clone()).collect();
        assert!(
            text.contains("more lines"),
            "overflow is advertised instead of silently dropped"
        );
    }

    #[test]
    fn running_tool_renders_spinner_without_result_rows() {
        let mut transcript = Transcript::new();
        transcript.tool_start("c", "grep", &serde_json::json!({}));
        assert_eq!(transcript.row_counts(40)[0], 1);
        transcript.tool_update("c", "streaming…");
        assert_eq!(transcript.row_counts(40)[0], 4, "running preview bounded");
    }

    #[test]
    fn completed_tool_toggles_between_preview_and_expanded() {
        let mut transcript = Transcript::new();
        transcript.tool_start("c", "bash", &serde_json::json!({}));
        let output: String = (0..40).map(|i| format!("line-{i}\n")).collect();
        transcript.tool_end("c", &output, false);
        // 折叠态：头 + 3 行预览 + 溢出提示。
        assert_eq!(transcript.row_counts(80)[0], 5);
        // 展开切换：全量（上限内）可见，无折叠提示之外的行。
        assert!(transcript.toggle_latest_tool_expansion());
        let counts = transcript.row_counts(80)[0];
        assert!(counts > 40, "expanded shows the full result: {counts}");
        let expanded_row = transcript.render_item_row(0, 10, 80, ' ').unwrap();
        let text: String = expanded_row
            .spans
            .iter()
            .map(|s| s.content.clone())
            .collect();
        assert!(text.contains("line-9"), "deep rows reachable: {text}");
        assert!(transcript.toggle_latest_tool_expansion());
        assert_eq!(transcript.row_counts(80)[0], 5, "toggle back to preview");
        // 无已完成工具时切换不承诺：返回 false。
        let mut empty = Transcript::new();
        assert!(!empty.toggle_latest_tool_expansion());
        empty.tool_start("c", "bash", &serde_json::json!({}));
        assert!(
            !empty.toggle_latest_tool_expansion(),
            "running tool cannot be expanded"
        );
    }
}

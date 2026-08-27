//! 会话流投影：typed 事件合并为可读条目，并按宽度计算可视行。
//!
//! [`Transcript`] 是纯状态对象：assistant 增量累积成段落；工具调用以
//! [`ToolItem`] 为单位就地刷新（运行中更新预览，结束后固化为稳定记录），
//! 不向会话流追加重复行。可视行计算覆盖显式换行、CJK 宽字符与长行折行。

use super::wrapped_lines;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

const TOOL_RESULT_PREVIEW_LINES: usize = 3;
/// 展开态下的结果行上限：防超长输出撑爆视口。
const TOOL_RESULT_EXPANDED_LINES: usize = 100;
const TOOL_COMPLETION_FLASH: std::time::Duration = std::time::Duration::from_millis(400);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolDisplay {
    Collapsed,
    Truncated,
    Full,
}

impl ToolDisplay {
    fn next(self) -> Self {
        match self {
            Self::Collapsed => Self::Truncated,
            Self::Truncated => Self::Full,
            Self::Full => Self::Collapsed,
        }
    }
}

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
        display: ToolDisplay,
        completed_at: std::time::Instant,
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
    Thinking(String),
    Tool(ToolItem),
}

/// 主会话流投影状态。
#[derive(Default)]
pub(crate) struct Transcript {
    items: Vec<FlowItem>,
    assistant_buffer: String,
    assistant_active: bool,
    thinking_collapsed: bool,
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

    pub fn push_thinking(&mut self, text: impl Into<String>) {
        self.flush_assistant();
        self.items.push(FlowItem::Thinking(text.into()));
    }

    pub fn toggle_thinking(&mut self) {
        self.thinking_collapsed = !self.thinking_collapsed;
    }

    pub fn thinking_collapsed(&self) -> bool {
        self.thinking_collapsed
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
        if let Some(item) = self.tool_item_mut(call_id)
            && matches!(item.state, ToolState::Running { .. })
        {
            item.state = ToolState::Done {
                output: result.to_string(),
                is_error,
                display: ToolDisplay::Truncated,
                completed_at: std::time::Instant::now(),
            };
        }
    }

    /// 条目终态：在未收到 ToolExecutionEnd 时把运行中工具定型为稳定记录。
    /// 取消/异常中断时 `ItemCompleted`/`ItemFailed` 是唯一收尾信号，工具块
    /// 不能停留在 Running；输出回退到最后一次增量预览。
    pub fn tool_terminal(&mut self, call_id: &str, is_error: bool) {
        if let Some(item) = self.tool_item_mut(call_id) {
            let ToolState::Running { last_output } = &mut item.state else {
                return;
            };
            let output = std::mem::take(last_output);
            item.state = ToolState::Done {
                output,
                is_error,
                display: ToolDisplay::Truncated,
                completed_at: std::time::Instant::now(),
            };
        }
    }

    /// 切换最近一个已完成工具块的展开态；返回是否发生了切换。
    /// 运行中或没有已完成工具时为 false（提示行据此不再承诺按键行为）。
    pub fn toggle_latest_tool_expansion(&mut self) -> bool {
        for item in self.items.iter_mut().rev() {
            if let FlowItem::Tool(tool) = item
                && let ToolState::Done { display, .. } = &mut tool.state
            {
                *display = display.next();
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

    /// 该 item_id 是否为工具条目（runtime 中工具条目的 item id 即
    /// tool call id）。事件投影据此区分工具相关事件与 assistant 文本。
    pub fn is_tool_item(&self, call_id: &str) -> bool {
        self.tool_item(call_id).is_some()
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
            .map(|item| item_rows(item, width, self.thinking_collapsed, ' ').len())
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
        item_rows(item, width, self.thinking_collapsed, spinner)
            .get(row_in_item)
            .cloned()
    }
}

/// 物化一条会话流条目为全部可视行；行数是渲染与计数共享的单一来源，
/// 避免行数计算与逐行渲染各自推导而失同步。
fn item_rows(
    item: &FlowItem,
    width: usize,
    thinking_collapsed: bool,
    spinner: char,
) -> Vec<Line<'static>> {
    match item {
        FlowItem::Text { style, text } => wrapped_lines(text, width)
            .into_iter()
            .map(|line| Line::from(Span::styled(line, style.style())))
            .collect(),
        FlowItem::Thinking(text) => {
            if thinking_collapsed {
                return vec![Line::from(Span::styled(
                    "▸ thinking",
                    NoteStyle::Dim.style(),
                ))];
            }
            let mut rows = vec![Line::from(Span::styled(
                "▾ thinking",
                NoteStyle::Accent.style(),
            ))];
            rows.extend(
                wrapped_lines(text, width.saturating_sub(2))
                    .into_iter()
                    .map(|line| {
                        Line::from(Span::styled(format!("│ {line}"), NoteStyle::Dim.style()))
                    }),
            );
            rows
        }
        FlowItem::Tool(tool) => {
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
                    completed_at,
                    is_error: false,
                    ..
                } if completed_at.elapsed() < TOOL_COMPLETION_FLASH => {
                    NoteStyle::Accent.style().add_modifier(Modifier::BOLD)
                }
                ToolState::Done {
                    is_error: false, ..
                } => NoteStyle::Dim.style(),
            };
            let mut rows: Vec<Line<'static>> = wrapped_lines(&tool.header(), width.saturating_sub(2))
                .into_iter()
                .map(|line| {
                    Line::from(Span::styled(format!("{marker} {line}"), style))
                })
                .collect();
            match &tool.state {
                ToolState::Running { last_output } => {
                    if !last_output.trim().is_empty() {
                        rows.extend(bounded_preview(last_output).into_iter().map(|line| {
                            Line::from(Span::styled(
                                format!("│ {line}"),
                                NoteStyle::Dim.style(),
                            ))
                        }));
                    }
                }
                ToolState::Done {
                    output,
                    is_error,
                    display,
                    ..
                } => {
                    if *display == ToolDisplay::Collapsed {
                        return rows;
                    }
                    let total_nonempty =
                        output.lines().filter(|l| !l.trim().is_empty()).count();
                    let visible: Vec<&str> = if *display == ToolDisplay::Full {
                        output
                            .lines()
                            .filter(|line| !line.trim().is_empty())
                            .take(TOOL_RESULT_EXPANDED_LINES)
                            .collect()
                    } else {
                        bounded_preview(output)
                    };
                    let result_style = if *is_error {
                        NoteStyle::Error.style()
                    } else {
                        NoteStyle::Dim.style()
                    };
                    rows.extend(visible.iter().copied().map(|line| {
                        Line::from(Span::styled(format!("│ {line}"), result_style))
                    }));
                    if total_nonempty > visible.len() {
                        let action = if *display == ToolDisplay::Full {
                            "collapse"
                        } else {
                            "expand"
                        };
                        rows.push(Line::from(Span::styled(
                            format!(
                                "│ … {} more lines (Ctrl+O {action})",
                                total_nonempty - visible.len()
                            ),
                            NoteStyle::Dim.style(),
                        )));
                    }
                }
            }
            rows
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

/// 贪心折行：按显示宽度断行，显式 `\n` 强制换行；空文本产出一空行。
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
    fn thinking_blocks_toggle_between_content_and_one_line_header() {
        let mut transcript = Transcript::new();
        transcript.push_thinking("first line\nsecond line");
        assert_eq!(transcript.row_counts(80), vec![3]);
        transcript.toggle_thinking();
        assert!(transcript.thinking_collapsed());
        assert_eq!(transcript.row_counts(80), vec![1]);
        let row = transcript.render_item_row(0, 0, 80, ' ').unwrap();
        let text: String = row.spans.iter().map(|span| span.content.clone()).collect();
        assert_eq!(text, "▸ thinking");
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
}
